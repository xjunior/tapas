//! Decides what each sanitized column's values become, and rewrites rows' values to match.
//!
//! Rules are checked against the source's columns once, before anything is extracted, so a typo
//! or a rule that would break a reference stops the run before it writes a row.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, RandomState};
use std::io::Write;
use std::mem;
use std::str::FromStr;
use std::time::SystemTime;

use anyhow::{Result, anyhow, bail};
use xxhash_rust::xxh3;

use crate::config::{Replacement, Rule, TemplatePart};
use crate::fake::Fake;
use crate::plan::Plan;
use crate::sql::{self, Dialect, Kind};

/// Where fake values start from. The same seed turns the same original value into the same fake
/// one, run after run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seed(u64);

impl Seed {
    /// Returns a seed no other run shares.
    #[must_use]
    pub fn random() -> Self {
        Self(RandomState::new().hash_one(SystemTime::now()))
    }
}

/// Any text, such as a passphrase from a secret store.
impl FromStr for Seed {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() {
            return Err("the seed can't be empty".to_string());
        }
        Ok(Self(xxh3::xxh3_64(text.as_bytes())))
    }
}

/// What the source says about one column of a table.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    /// The column's name.
    pub name: String,
    /// The column holds numbers, so it can't take text.
    pub numeric: bool,
    /// The most characters a value holds, for text and binary columns.
    pub max_chars: Option<usize>,
}

impl ColumnInfo {
    /// Describes a column from `information_schema.COLUMNS`: its `DATA_TYPE`, such as `varchar`
    /// or Postgres's `double precision`, and its `CHARACTER_MAXIMUM_LENGTH`.
    #[must_use]
    pub fn new(name: String, data_type: &str, max_chars: Option<u64>) -> Self {
        let numeric = matches!(
            data_type.to_ascii_lowercase().as_str(),
            "tinyint"
                | "smallint"
                | "mediumint"
                | "int"
                | "integer"
                | "bigint"
                | "decimal"
                | "numeric"
                | "float"
                | "double"
                | "year"
                | "bit"
                // Postgres's names that MySQL doesn't share.
                | "real"
                | "double precision"
        );
        Self {
            name,
            numeric,
            max_chars: max_chars.map(|max| usize::try_from(max).unwrap_or(usize::MAX)),
        }
    }
}

/// What a sanitized column's values become.
#[derive(Debug, Clone, PartialEq)]
enum Action {
    /// This literal: NULL, or a configured value.
    Literal(Vec<u8>),
    /// A made-up value, from the original one.
    Fake(Fake),
    /// `CONCAT` of these pieces.
    Template(Vec<Piece>),
}

/// A piece of a template.
#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Literal(Vec<u8>),
    Column(String),
}

/// One sanitized column of a table.
#[derive(Debug, Clone, PartialEq)]
struct Sanitized {
    column: String,
    action: Action,
    max_chars: Option<usize>,
}

/// Every table's sanitized columns, and the seed fakes start from.
#[derive(Debug, Clone, PartialEq)]
pub struct Sanitizer {
    seed: Seed,
    dialect: Dialect,
    tables: BTreeMap<String, Vec<Sanitized>>,
}

impl Default for Sanitizer {
    /// Sanitizes nothing.
    fn default() -> Self {
        Self {
            seed: Seed(0),
            dialect: Dialect::MySql,
            tables: BTreeMap::new(),
        }
    }
}

impl Sanitizer {
    /// Works out what the plan's rules do to each of its tables, given the source's `columns`
    /// per table and the `dialect` it speaks. Fails on a rule that names a column the table lacks, would break a reference,
    /// or writes something the column can't hold.
    pub fn try_new(
        plan: &Plan,
        columns: &BTreeMap<String, Vec<ColumnInfo>>,
        dialect: Dialect,
        seed: Seed,
    ) -> Result<Self> {
        let mut tables = BTreeMap::new();
        for table in plan.tables.keys() {
            let own: &[Rule] = plan.sanitize.tables.get(table).map_or(&[], Vec::as_slice);
            if own.is_empty() && plan.sanitize.columns.is_empty() {
                continue;
            }
            let Some(table_columns) = columns.get(table) else {
                if let Some(rule) = own.first() {
                    bail!(
                        "{}: sanitize.tables.{table}: the source has no table {table}",
                        rule.file
                    );
                }
                continue;
            };
            let sanitized = resolve(
                dialect,
                table,
                own,
                &plan.sanitize.columns,
                &plan.references(table),
                table_columns,
            )?;
            if !sanitized.is_empty() {
                tables.insert(table.clone(), sanitized);
            }
        }
        Ok(Self {
            seed,
            dialect,
            tables,
        })
    }

    /// Finds `table`'s sanitized columns among a query's column `names`, whose values are
    /// written as `kinds`.
    pub fn place(&self, table: &str, names: &[String], kinds: &[Kind]) -> Result<Placed<'_>> {
        let position = |column: &str| {
            names
                .iter()
                .position(|name| name.eq_ignore_ascii_case(column))
                .ok_or_else(|| {
                    anyhow!("{table} has no column {column} any more, which a sanitize rule needs")
                })
        };
        let mut columns = Vec::new();
        for sanitized in self.tables.get(table).map_or(&[][..], Vec::as_slice) {
            let at = position(&sanitized.column)?;
            let action = match &sanitized.action {
                Action::Literal(literal) => PlacedAction::Literal(literal),
                Action::Fake(fake) => PlacedAction::Fake {
                    fake: *fake,
                    // Mixing in the fake's name, not the column's, keeps a value's fake the same
                    // in every column that uses that fake.
                    seed: self.seed.0 ^ xxh3::xxh3_64(fake.as_str().as_bytes()),
                },
                Action::Template(pieces) => {
                    let mut placed = Vec::with_capacity(pieces.len());
                    for piece in pieces {
                        placed.push(match piece {
                            Piece::Literal(literal) => PlacedPiece::Literal(literal),
                            Piece::Column(column) => PlacedPiece::Column(position(column)?),
                        });
                    }
                    PlacedAction::Template(placed)
                }
            };
            columns.push(PlacedColumn {
                position: at,
                kind: kinds[at],
                action,
                max_chars: sanitized.max_chars,
            });
        }
        Ok(Placed {
            dialect: self.dialect,
            columns,
        })
    }
}

/// Works out the sanitized columns of `table`, from its `own` rules and the `global` ones.
fn resolve(
    dialect: Dialect,
    table: &str,
    own: &[Rule],
    global: &[Rule],
    references: &BTreeSet<&str>,
    columns: &[ColumnInfo],
) -> Result<Vec<Sanitized>> {
    for rule in own.iter().filter(|rule| rule.pattern.is_exact()) {
        if !columns
            .iter()
            .any(|column| rule.pattern.matches(&column.name))
        {
            bail!(
                "{}: {table} has no column {}",
                origin(rule, Some(table)),
                rule.pattern
            );
        }
    }

    let mut chosen: Vec<(&ColumnInfo, &Rule, String)> = Vec::new();
    for column in columns {
        let Some((rule, origin)) = pick(table, &column.name, own, global)? else {
            continue;
        };
        match rule.replacement {
            Replacement::Keep => continue,
            Replacement::Null
            | Replacement::Value(_)
            | Replacement::Fake(_)
            | Replacement::Template(_) => {}
        }
        if references
            .iter()
            .any(|reference| reference.eq_ignore_ascii_case(&column.name))
        {
            bail!(
                "{origin}: {table}.{} can't be sanitized, because a reference follows it. Add \
                 `{}: keep` under sanitize.tables.{table}",
                column.name,
                column.name
            );
        }
        chosen.push((column, rule, origin));
    }

    let mut sanitized = Vec::with_capacity(chosen.len());
    for (column, rule, origin) in &chosen {
        let name = &column.name;
        let action = match &rule.replacement {
            Replacement::Keep => continue,
            Replacement::Null => Action::Literal(sql::NULL.to_vec()),
            Replacement::Value(text) => {
                if column.numeric && text.parse::<f64>().is_err() {
                    bail!("{origin}: {table}.{name} holds numbers, but `{text}` isn't one");
                }
                let length = text.chars().count();
                if let Some(max) = column.max_chars
                    && length > max
                {
                    bail!(
                        "{origin}: `{text}` has {length} characters, but {table}.{name} holds \
                         {max}"
                    );
                }
                Action::Literal(sql::literal(dialect, Kind::Text, Some(text.as_bytes())))
            }
            Replacement::Fake(fake) => {
                if column.numeric && !fake.is_digits() {
                    bail!("{origin}: {table}.{name} holds numbers, but fake {fake} makes text");
                }
                Action::Fake(*fake)
            }
            Replacement::Template(template) => {
                if column.numeric {
                    bail!("{origin}: {table}.{name} holds numbers, but a template makes text");
                }
                let mut pieces = Vec::with_capacity(template.parts.len());
                for part in &template.parts {
                    pieces.push(match part {
                        TemplatePart::Text(text) => {
                            Piece::Literal(sql::literal(dialect, Kind::Text, Some(text.as_bytes())))
                        }
                        TemplatePart::Column(wanted) => {
                            let Some(found) = columns
                                .iter()
                                .find(|column| column.name.eq_ignore_ascii_case(wanted))
                            else {
                                bail!("{origin}: the template uses {wanted}, which {table} lacks");
                            };
                            if chosen
                                .iter()
                                .any(|(column, _, _)| column.name == found.name)
                            {
                                bail!(
                                    "{origin}: the template uses {wanted}, which is sanitized \
                                     too, so its original value would leak"
                                );
                            }
                            Piece::Column(found.name.clone())
                        }
                    });
                }
                Action::Template(pieces)
            }
        };
        sanitized.push(Sanitized {
            column: name.clone(),
            action,
            max_chars: column.max_chars,
        });
    }
    Ok(sanitized)
}

/// Picks the rule for `column`, and says where it's defined. The table's own rules win over
/// global ones, and a rule naming the column wins over a glob. Among globs, the first listed
/// wins; globs from two files that match the same column are an error, since nothing orders them.
fn pick<'rules>(
    table: &str,
    column: &str,
    own: &'rules [Rule],
    global: &'rules [Rule],
) -> Result<Option<(&'rules Rule, String)>> {
    let exact = |rule: &&Rule| rule.pattern.is_exact() && rule.pattern.matches(column);
    let glob = |rule: &&Rule| !rule.pattern.is_exact() && rule.pattern.matches(column);

    if let Some(rule) = own.iter().find(exact) {
        return Ok(Some((rule, origin(rule, Some(table)))));
    }
    let mut globs = own.iter().filter(glob);
    if let Some(first) = globs.next() {
        if let Some(other) = globs.find(|other| other.file != first.file) {
            bail!(
                "{table}.{column} matches `{}` in {} and `{}` in {}; name the column in one of \
                 them",
                first.pattern,
                first.file,
                other.pattern,
                other.file
            );
        }
        return Ok(Some((first, origin(first, Some(table)))));
    }
    let rule = global
        .iter()
        .find(exact)
        .or_else(|| global.iter().find(glob));
    Ok(rule.map(|rule| (rule, origin(rule, None))))
}

/// Says where a rule is defined: `shop.yml sanitize.tables.users.email`.
#[must_use]
fn origin(rule: &Rule, table: Option<&str>) -> String {
    match table {
        Some(table) => format!("{} sanitize.tables.{table}.{}", rule.file, rule.pattern),
        None => format!("{} sanitize.columns.{}", rule.file, rule.pattern),
    }
}

/// A table's sanitized columns, found among one query's columns.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed<'a> {
    dialect: Dialect,
    columns: Vec<PlacedColumn<'a>>,
}

#[derive(Debug, Clone, PartialEq)]
struct PlacedColumn<'a> {
    position: usize,
    kind: Kind,
    action: PlacedAction<'a>,
    max_chars: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
enum PlacedAction<'a> {
    Literal(&'a [u8]),
    Fake { fake: Fake, seed: u64 },
    Template(Vec<PlacedPiece<'a>>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PlacedPiece<'a> {
    Literal(&'a [u8]),
    Column(usize),
}

/// Buffers `Placed::apply` reuses from one value to the next.
#[derive(Debug, Clone, Default)]
pub struct Scratch {
    raw: Vec<u8>,
    text: String,
}

impl Placed<'_> {
    /// Returns whether no column of the query is sanitized.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Replaces the sanitized values of `row`, a row of literals, in place. NULL and empty
    /// values stay as they are.
    pub fn apply(&self, row: &mut [Vec<u8>], scratch: &mut Scratch) {
        for column in &self.columns {
            // Taken out, so a template can read the row's other values while this one is built.
            let mut value = mem::take(&mut row[column.position]);
            if !is_blank(&value) {
                write_replacement(self.dialect, column, &mut value, row, scratch);
            }
            row[column.position] = value;
        }
    }
}

/// Returns whether a literal is NULL or empty.
#[must_use]
fn is_blank(literal: &[u8]) -> bool {
    literal == sql::NULL || literal == b"''" || literal == b"X''"
}

/// Replaces `value`, the literal of `column` in `row`, with its sanitized literal.
fn write_replacement(
    dialect: Dialect,
    column: &PlacedColumn<'_>,
    value: &mut Vec<u8>,
    row: &[Vec<u8>],
    scratch: &mut Scratch,
) {
    match &column.action {
        PlacedAction::Literal(literal) => {
            value.clear();
            value.extend_from_slice(literal);
        }
        PlacedAction::Fake { fake, seed } => {
            scratch.raw.clear();
            sql::push_raw(&mut scratch.raw, dialect, column.kind, value);
            // MySQL's usual collations ignore case, so `Ana@x.com` and `ana@x.com` are the same
            // value to it, and get the same fake. Postgres values are treated alike, so a value's
            // fake doesn't depend on which database it came from.
            scratch.raw.make_ascii_lowercase();
            scratch.text.clear();
            fake.write(
                xxh3::xxh3_128_with_seed(&scratch.raw, *seed),
                &mut scratch.text,
            );
            if let Some(max) = column.max_chars {
                // Fakes are ASCII, so this cuts between characters.
                scratch.text.truncate(max);
            }
            value.clear();
            sql::push_literal(value, dialect, Kind::Text, scratch.text.as_bytes());
        }
        PlacedAction::Template(pieces) => {
            value.clear();
            if column.max_chars.is_some() {
                value.extend_from_slice(b"LEFT(");
            }
            let (open, separator, close) = dialect.concat();
            value.extend_from_slice(open);
            for (index, piece) in pieces.iter().enumerate() {
                if index > 0 {
                    value.extend_from_slice(separator);
                }
                match piece {
                    PlacedPiece::Literal(literal) => value.extend_from_slice(literal),
                    PlacedPiece::Column(position) => value.extend_from_slice(&row[*position]),
                }
            }
            value.extend_from_slice(close);
            if let Some(max) = column.max_chars {
                // Writing to a `Vec` can't fail.
                let _ = write!(value, ", {max})");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn plan(yaml: &str) -> Result<Plan> {
        let config: Config = yaml.parse()?;
        Plan::try_new(config, None)
    }

    fn text(name: &str, max_chars: u64) -> ColumnInfo {
        ColumnInfo::new(name.to_string(), "varchar", Some(max_chars))
    }

    fn number(name: &str) -> ColumnInfo {
        ColumnInfo::new(name.to_string(), "bigint", None)
    }

    fn seed() -> Seed {
        "test".parse().expect("a seed")
    }

    /// Sanitizes `rows` of `table` (literals as strings), whose columns are all text but `id`.
    fn sanitize(
        sanitizer: &Sanitizer,
        table: &str,
        names: &[&str],
        rows: &[&[&str]],
    ) -> Result<Vec<Vec<String>>> {
        let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        let kinds: Vec<Kind> = names
            .iter()
            .map(|name| {
                if name == "id" {
                    Kind::Number
                } else {
                    Kind::Text
                }
            })
            .collect();
        let placed = sanitizer.place(table, &names, &kinds)?;
        let mut scratch = Scratch::default();
        let mut out = Vec::new();
        for row in rows {
            let mut row: Vec<Vec<u8>> = row.iter().map(|value| value.as_bytes().to_vec()).collect();
            placed.apply(&mut row, &mut scratch);
            out.push(
                row.into_iter()
                    .map(|value| String::from_utf8(value).expect("UTF-8"))
                    .collect(),
            );
        }
        Ok(out)
    }

    const PEOPLE: &str = r#"
samplers:
  - table: users
    limit: all
  - table: contacts
    limit: all
sanitize:
  columns:
    "*_id": keep
    "*email*": { fake: email }
    notes: { value: "Redacted" }
    "*notes*": null
  tables:
    users:
      notes: keep
      "*name*": { fake: first_name }
      login: { template: "user_{id}" }
"#;

    fn people() -> Result<Sanitizer> {
        people_seeded(seed())
    }

    fn people_seeded(seed: Seed) -> Result<Sanitizer> {
        let columns = BTreeMap::from([
            (
                "users".to_string(),
                vec![
                    number("id"),
                    text("email", 255),
                    text("first_name", 40),
                    text("login", 20),
                    text("notes", 255),
                    text("legacy_notes", 255),
                    text("external_id", 40),
                ],
            ),
            (
                "contacts".to_string(),
                vec![number("id"), text("email_address", 255), text("notes", 255)],
            ),
        ]);
        Sanitizer::try_new(&plan(PEOPLE)?, &columns, Dialect::MySql, seed)
    }

    #[test]
    fn reads_column_types() {
        let id = ColumnInfo::new("id".to_string(), "BIGINT", None);
        assert!(id.numeric, "{id:?}");
        let email = ColumnInfo::new("email".to_string(), "varchar", Some(255));
        assert!(!email.numeric, "{email:?}");
        assert_eq!(email.max_chars, Some(255));
    }

    #[test]
    fn picks_the_most_specific_rule() -> Result<()> {
        let sanitizer = people()?;
        let rows = sanitize(
            &sanitizer,
            "users",
            &USERS,
            &[&["7", "'ana@x.com'", "'Ana'", "'ana'", "'n'", "'old'", "'e7'"]],
        )?;
        let row = &rows[0];
        assert_eq!(row[0], "7", "id has no rule");
        assert!(
            row[1].contains("@example."),
            "the global glob fakes email: {row:?}"
        );
        assert_ne!(row[2], "'Ana'", "the table's glob fakes first_name");
        assert_eq!(row[3], "LEFT(CONCAT('user_', 7), 20)", "the template");
        assert_eq!(row[4], "'n'", "the table's keep wins over the global value");
        assert_eq!(row[5], "NULL", "the global glob");
        assert_eq!(row[6], "'e7'", "the global keep comes first");

        let rows = sanitize(
            &sanitizer,
            "contacts",
            &["id", "email_address", "notes"],
            &[&["1", "'x'", "'secret'"]],
        )?;
        assert_eq!(
            rows[0][2], "'Redacted'",
            "the global exact name wins over the global glob listed after it"
        );
        Ok(())
    }

    const USERS: [&str; 7] = [
        "id",
        "email",
        "first_name",
        "login",
        "notes",
        "legacy_notes",
        "external_id",
    ];

    #[test]
    fn leaves_null_and_empty_values_alone() -> Result<()> {
        let blank = ["1", "NULL", "''", "NULL", "''", "NULL", "''"];
        let rows = sanitize(&people()?, "users", &USERS, &[&blank])?;
        assert_eq!(rows[0], blank);
        Ok(())
    }

    #[test]
    fn the_same_value_gets_the_same_fake_everywhere() -> Result<()> {
        let sanitizer = people()?;
        let users_input = [
            ["1", "'ana@x.com'", "NULL", "NULL", "NULL", "NULL", "NULL"],
            ["2", "'bo@x.com'", "NULL", "NULL", "NULL", "NULL", "NULL"],
            ["3", "'ANA@x.com'", "NULL", "NULL", "NULL", "NULL", "NULL"],
        ];
        let rows: Vec<&[&str]> = users_input.iter().map(|row| &row[..]).collect();
        let users = sanitize(&sanitizer, "users", &USERS, &rows)?;
        let contacts = sanitize(
            &sanitizer,
            "contacts",
            &["id", "email_address", "notes"],
            &[&["9", "'ana@x.com'", "NULL"]],
        )?;
        assert_eq!(
            users[0][1], contacts[0][1],
            "the same email in another table and column"
        );
        assert_eq!(users[0][1], users[2][1], "case doesn't matter");
        assert_ne!(users[0][1], users[1][1], "another email gets another fake");

        let reseeded = people_seeded("another".parse().map_err(anyhow::Error::msg)?)?;
        let again = sanitize(&reseeded, "users", &USERS, &[&users_input[0]])?;
        assert_ne!(again[0][1], users[0][1], "another seed makes other fakes");
        Ok(())
    }

    #[test]
    fn reads_binary_and_numeric_values_by_what_they_hold() -> Result<()> {
        let config = plan(
            "samplers:\n  - table: t\n    limit: all\nsanitize:\n  columns:\n    \"*\": { fake: ssn }\n",
        )?;
        let columns = BTreeMap::from([(
            "t".to_string(),
            vec![number("a"), text("b", 20), text("c", 20)],
        )]);
        let sanitizer = Sanitizer::try_new(&config, &columns, Dialect::MySql, seed())?;
        let names = ["a".to_string(), "b".to_string(), "c".to_string()];
        let placed = sanitizer.place("t", &names, &[Kind::Number, Kind::Text, Kind::Binary])?;
        // 123456789 as a number, as text, and as binary.
        let mut row = vec![
            b"123456789".to_vec(),
            b"'123456789'".to_vec(),
            b"X'313233343536373839'".to_vec(),
        ];
        placed.apply(&mut row, &mut Scratch::default());
        assert!(row[0] == row[1] && row[1] == row[2], "{row:?}");
        Ok(())
    }

    #[test]
    fn cuts_fakes_to_fit() -> Result<()> {
        let config = plan(
            "samplers:\n  - table: t\n    limit: all\nsanitize:\n  columns:\n    email: { fake: email }\n",
        )?;
        let columns = BTreeMap::from([("t".to_string(), vec![text("email", 6)])]);
        let sanitizer = Sanitizer::try_new(&config, &columns, Dialect::MySql, seed())?;
        let rows = sanitize(&sanitizer, "t", &["email"], &[&["'someone@x.com'"]])?;
        assert_eq!(
            rows[0][0].len(),
            8,
            "six characters and two quotes: {rows:?}"
        );
        Ok(())
    }

    fn error(yaml: &str, columns: Vec<ColumnInfo>) -> String {
        let columns = BTreeMap::from([("users".to_string(), columns)]);
        let result =
            plan(yaml).and_then(|plan| Sanitizer::try_new(&plan, &columns, Dialect::MySql, seed()));
        match result {
            Ok(sanitizer) => panic!("expected an error, got {sanitizer:?}"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn rejects_rules_that_break_references() {
        let message = error(
            r#"
samplers:
  - table: users
    limit: all
  - table: orders
    limit: all
    deps:
      user_email: users.email
sanitize:
  columns:
    "*email*": { fake: email }
"#,
            vec![number("id"), text("email", 255)],
        );
        assert_eq!(
            message,
            "config sanitize.columns.*email*: users.email can't be sanitized, because a \
             reference follows it. Add `email: keep` under sanitize.tables.users"
        );
    }

    #[test]
    fn rejects_rules_the_columns_cant_take() {
        let users =
            "samplers:\n  - table: users\n    limit: all\nsanitize:\n  tables:\n    users:\n";
        for (rule, message) in [
            (
                "emial: null",
                "config sanitize.tables.users.emial: users has no column emial",
            ),
            (
                "id: { fake: email }",
                "config sanitize.tables.users.id: users.id holds numbers, but fake email makes \
                 text",
            ),
            (
                "id: { value: none }",
                "config sanitize.tables.users.id: users.id holds numbers, but `none` isn't one",
            ),
            (
                "id: { template: 'x{email}' }",
                "config sanitize.tables.users.id: users.id holds numbers, but a template makes \
                 text",
            ),
            (
                "login: { value: much too long }",
                "config sanitize.tables.users.login: `much too long` has 13 characters, but \
                 users.login holds 5",
            ),
            (
                "login: { template: 'u{uid}' }",
                "config sanitize.tables.users.login: the template uses uid, which users lacks",
            ),
            (
                "login: { template: 'u{email}' }\n      email: null",
                "config sanitize.tables.users.login: the template uses email, which is \
                 sanitized too, so its original value would leak",
            ),
        ] {
            let found = error(
                &format!("{users}      {rule}\n"),
                vec![number("id"), text("email", 255), text("login", 5)],
            );
            assert_eq!(found, message, "{rule}");
        }
    }

    #[test]
    fn rejects_globs_from_two_files_on_one_column() -> Result<()> {
        let mut config: Config = "samplers:\n  - table: users\n    limit: all\n".parse()?;
        for (file, pattern) in [("a.yml", "*mail*"), ("b.yml", "e*")] {
            config
                .sanitize
                .tables
                .entry("users".to_string())
                .or_default()
                .push(Rule {
                    pattern: pattern.to_string().try_into().map_err(anyhow::Error::msg)?,
                    replacement: Replacement::Null,
                    file: file.to_string(),
                });
        }
        let plan = Plan::try_new(config, None)?;
        let columns = BTreeMap::from([("users".to_string(), vec![text("email", 255)])]);
        let error = Sanitizer::try_new(&plan, &columns, Dialect::MySql, seed())
            .expect_err("an unordered clash");
        assert_eq!(
            error.to_string(),
            "users.email matches `*mail*` in a.yml and `e*` in b.yml; name the column in one of \
             them"
        );
        Ok(())
    }

    #[test]
    fn rejects_tables_missing_from_the_source() -> Result<()> {
        let plan = plan(
            "samplers:\n  - table: users\n    limit: all\nsanitize:\n  tables:\n    users:\n      email: null\n",
        )?;
        let error = Sanitizer::try_new(&plan, &BTreeMap::new(), Dialect::MySql, seed())
            .expect_err("the source has no users");
        assert_eq!(
            error.to_string(),
            "config: sanitize.tables.users: the source has no table users"
        );
        Ok(())
    }

    #[test]
    fn parses_seeds() {
        assert_eq!("a".parse::<Seed>(), "a".parse::<Seed>());
        assert_ne!("a".parse::<Seed>(), "b".parse::<Seed>());
        assert_eq!(
            "".parse::<Seed>(),
            Err("the seed can't be empty".to_string())
        );
        assert_ne!(Seed::random(), Seed::random());
    }

    #[test]
    fn writes_postgres_replacements() -> Result<()> {
        let config = plan(
            "samplers:\n  - table: users\n    limit: all\nsanitize:\n  columns:\n    \
             login: { template: \"it's_{id}\" }\n    bio: { value: \"it's\" }\n    \
             email: { fake: email }\n",
        )?;
        let columns = BTreeMap::from([(
            "users".to_string(),
            vec![
                number("id"),
                text("login", 40),
                text("bio", 100),
                text("email", 100),
            ],
        )]);
        let names = ["id", "login", "bio", "email"];
        let postgres = Sanitizer::try_new(&config, &columns, Dialect::Postgres, seed())?;
        let rows = sanitize(
            &postgres,
            "users",
            &names,
            &[
                &["'7'", "'x'", "'x'", "'it''s@x.com'"],
                &["'8'", "'x'", "'x'", "''"],
            ],
        )?;
        assert_eq!(
            rows[0][1], "LEFT(CAST('it''s_' || '7' AS text), 40)",
            "|| keeps a NULL piece's NULL, as MySQL's CONCAT does"
        );
        assert_eq!(rows[0][2], "'it''s'");
        assert_eq!(rows[1][3], "''", "empty values stay empty");

        let mysql = Sanitizer::try_new(&config, &columns, Dialect::MySql, seed())?;
        let mysql_rows = sanitize(
            &mysql,
            "users",
            &names,
            &[&["7", "'x'", "'x'", r"'it\'s@x.com'"]],
        )?;
        assert_eq!(
            rows[0][3], mysql_rows[0][3],
            "a value gets the same fake whichever database it comes from"
        );
        Ok(())
    }

    #[test]
    fn knows_postgres_numeric_types() {
        for data_type in ["integer", "numeric", "real", "double precision"] {
            let column = ColumnInfo::new("n".to_string(), data_type, None);
            assert!(column.numeric, "{data_type} holds numbers");
        }
        let column = ColumnInfo::new("s".to_string(), "character varying", Some(20));
        assert!(!column.numeric, "character varying holds text");
    }
}
