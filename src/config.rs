//! The YAML config: which tables to sample, how their rows depend on each other, and what to write
//! in place of sensitive values.

use std::collections::{BTreeMap, HashSet};
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::PathBuf;
use std::slice;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Deserializer, de};
use serde_yaml_ng::Value;

use crate::fake::Fake;

/// Every sampler and sanitize rule of one or more config files.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    /// The samplers, in the order the files list them.
    pub samplers: Vec<Sampler>,
    /// What to write in place of sensitive values.
    pub sanitize: Sanitize,
    /// Where references come from, when a file says.
    pub deps: Option<DepsSource>,
}

/// Where the references between tables come from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepsSource {
    /// Only the `deps` and `of` the config declares.
    #[default]
    Declared,
    /// The foreign keys of the source database, plus whatever the config declares.
    ForeignKeys,
}

impl Display for DepsSource {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Declared => f.write_str("declared"),
            Self::ForeignKeys => f.write_str("foreign_keys"),
        }
    }
}

impl FromStr for DepsSource {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "declared" => Ok(Self::Declared),
            "foreign-keys" | "foreign_keys" => Ok(Self::ForeignKeys),
            _ => Err(format!(
                "`{text}` isn't where references come from; use `declared` or `foreign-keys`"
            )),
        }
    }
}

/// What one file holds. A file can hold only samplers, only `sanitize:`, or both.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    samplers: Vec<Sampler>,
    #[serde(default)]
    sanitize: SanitizeFile,
    deps: Option<DepsSource>,
}

/// The `sanitize:` section of one file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SanitizeFile {
    #[serde(default)]
    columns: RuleList,
    #[serde(default)]
    tables: BTreeMap<String, RuleList>,
}

/// The `sanitize:` sections of every file, merged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sanitize {
    /// `sanitize.columns`: rules for every table. Only one file may have them.
    pub columns: Vec<Rule>,
    /// `sanitize.tables`: rules for one table each, which win over `columns`. Several files may
    /// have rules for the same table, as long as they don't set the same column.
    pub tables: BTreeMap<String, Vec<Rule>>,
}

impl Sanitize {
    /// Returns whether no rule sanitizes anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty() && self.tables.values().all(Vec::is_empty)
    }

    /// Adds `other`'s rules. Fails if both have `columns`, or set the same column of a table.
    fn merge(&mut self, other: Self) -> Result<()> {
        if let (Some(mine), Some(theirs)) = (self.columns.first(), other.columns.first()) {
            bail!(
                "sanitize.columns is in both {} and {}; keep the rules for every table in one file",
                mine.file,
                theirs.file
            );
        }
        if !other.columns.is_empty() {
            self.columns = other.columns;
        }
        for (table, rules) in other.tables {
            if rules.is_empty() {
                continue;
            }
            let merged = self.tables.entry(table.clone()).or_default();
            for rule in rules {
                if let Some(seen) = merged.iter().find(|seen| seen.pattern == rule.pattern) {
                    bail!(
                        "{}: sanitize.tables.{table} sets `{}`, which {} sets too",
                        rule.file,
                        rule.pattern,
                        seen.file
                    );
                }
                merged.push(rule);
            }
        }
        Ok(())
    }
}

/// One `column: replacement` entry of `sanitize:`.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// The columns the rule applies to.
    pub pattern: ColumnPattern,
    /// What their values become.
    pub replacement: Replacement,
    /// The file that has the rule, for messages.
    pub file: String,
}

/// The entries of a `column: replacement` map, in the order the file lists them, since the first
/// glob that matches a column wins.
#[derive(Debug, Default)]
struct RuleList(Vec<(ColumnPattern, Replacement)>);

impl RuleList {
    fn into_rules(self, file: &str) -> Vec<Rule> {
        self.0
            .into_iter()
            .map(|(pattern, replacement)| Rule {
                pattern,
                replacement,
                file: file.to_string(),
            })
            .collect()
    }
}

impl<'de> Deserialize<'de> for RuleList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = RuleList;

            fn expecting(&self, f: &mut Formatter<'_>) -> fmt::Result {
                f.write_str("a map of `column: replacement`")
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<RuleList, A::Error> {
                let mut rules: Vec<(ColumnPattern, Replacement)> = Vec::new();
                while let Some((pattern, replacement)) = map.next_entry::<ColumnPattern, _>()? {
                    if rules.iter().any(|(seen, _)| *seen == pattern) {
                        return Err(de::Error::custom(format!("`{pattern}` is listed twice")));
                    }
                    rules.push((pattern, replacement));
                }
                Ok(RuleList(rules))
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

/// A column name, or a glob such as `*email*`, matched against whole column names ignoring
/// case, as MySQL compares them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct ColumnPattern {
    // Lowercase, so `Email` and `email` are the same pattern.
    glob: Pattern,
}

impl ColumnPattern {
    /// Returns the pattern, in lowercase.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.glob.as_str()
    }

    /// Returns whether the pattern is a plain column name, without `*`, `?` or `[`.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        !self.as_str().contains(['*', '?', '['])
    }

    /// Returns whether `column` matches, ignoring case.
    #[must_use]
    pub fn matches(&self, column: &str) -> bool {
        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };
        self.glob.matches_with(column, options)
    }
}

impl TryFrom<String> for ColumnPattern {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.trim().is_empty() {
            return Err("a column pattern can't be empty".to_string());
        }
        let glob = Pattern::new(&value.to_lowercase())
            .map_err(|error| format!("`{value}` isn't a valid pattern: {error}"))?;
        Ok(Self { glob })
    }
}

impl Display for ColumnPattern {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a sanitized column's values become. NULL and empty values stay as they are, whatever
/// the replacement.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "Value")]
pub enum Replacement {
    /// `keep`: the values are copied as they are, even if another rule matches the column.
    Keep,
    /// `null`: NULL.
    Null,
    /// `{ value: … }`: this text or number.
    Value(String),
    /// `{ fake: … }`: a made-up value. The same original value always makes the same one.
    Fake(Fake),
    /// `{ template: … }`: text with the row's other values in it, such as `user_{id}`.
    Template(Template),
}

impl TryFrom<Value> for Replacement {
    type Error = String;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        const EXPECTED: &str = "a replacement is `keep`, `null`, `{ value: … }`, `{ fake: … }` or \
                                `{ template: … }`";
        let mapping = match value {
            Value::Null => return Ok(Self::Null),
            Value::String(word) if word == "keep" => return Ok(Self::Keep),
            Value::Mapping(mapping) if mapping.len() == 1 => mapping,
            Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Sequence(_)
            | Value::Mapping(_)
            | Value::Tagged(_) => return Err(EXPECTED.to_string()),
        };
        let Some((key, value)) = mapping.into_iter().next() else {
            unreachable!("the mapping has one entry");
        };
        match (key.as_str(), value) {
            (Some("value"), Value::String(text)) => Ok(Self::Value(text)),
            (Some("value"), Value::Number(number)) => Ok(Self::Value(number.to_string())),
            (Some("fake"), Value::String(name)) => name.parse().map(Self::Fake),
            (Some("template"), Value::String(text)) => text.parse().map(Self::Template),
            _ => Err(EXPECTED.to_string()),
        }
    }
}

impl Display for Replacement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => f.write_str("keep"),
            Self::Null => f.write_str("null"),
            Self::Value(_) => f.write_str("value"),
            Self::Fake(fake) => write!(f, "fake {fake}"),
            Self::Template(template) => write!(f, "template {template}"),
        }
    }
}

/// Text with the row's other values in it: `user_{id}@example.com`. Braces always mark a column.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    /// The text as configured.
    text: String,
    /// The pieces, in order.
    pub parts: Vec<TemplatePart>,
}

/// A piece of a template.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplatePart {
    /// Text written as it is.
    Text(String),
    /// The value of this column of the same row.
    Column(String),
}

impl FromStr for Template {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut parts = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let brace = rest.find(['{', '}']).unwrap_or(rest.len());
            if brace > 0 {
                parts.push(TemplatePart::Text(rest[..brace].to_string()));
            }
            rest = &rest[brace..];
            if rest.starts_with('}') {
                return Err(format!("`{text}` has a `}}` without an opening `{{`"));
            }
            if let Some(after) = rest.strip_prefix('{') {
                let Some(end) = after.find('}') else {
                    return Err(format!("`{text}` has a `{{` without a closing `}}`"));
                };
                let column = after[..end].trim();
                if column.is_empty() {
                    return Err(format!("`{text}` has an empty `{{}}`"));
                }
                if column.contains('{') {
                    return Err(format!("`{text}` has a `{{` without a closing `}}`"));
                }
                parts.push(TemplatePart::Column(column.to_string()));
                rest = &after[end + 1..];
            }
        }
        Ok(Self {
            text: text.to_string(),
            parts,
        })
    }
}

impl Display for Template {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// One entry of `samplers`: which rows of a table to extract.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sampler {
    /// The table the rows come from.
    pub table: String,
    /// Lets other samplers depend on just this sampler's rows: `deps: { id: <name>.column }`.
    pub name: Option<String>,
    /// A SQL condition the rows must meet, from the `where` key.
    #[serde(rename = "where")]
    pub filter: Option<String>,
    /// The SQL ordering applied before `limit`.
    pub order_by: Option<String>,
    /// How far the sampler reaches. Without one it takes no rows of its own, unless `of`
    /// narrows it to the rows that belong to its parents.
    pub limit: Option<Limit>,
    /// `my_column: parent.column`: only rows that belong to the parent's extracted rows. A list
    /// of such maps gives one reference over several columns for each item.
    #[serde(default, deserialize_with = "references")]
    pub of: BTreeMap<Columns, ColumnRef>,
    /// `my_column: parent.column`: the parent rows this table's rows point at get extracted too.
    /// Applies to every row of the table, whichever sampler brought it in. A list of such maps
    /// gives one reference over several columns for each item.
    #[serde(default, deserialize_with = "references")]
    pub deps: BTreeMap<Columns, ColumnRef>,
    /// Where the sampler was defined, for messages: `users.yml sampler #2 (users)`.
    #[serde(skip)]
    pub origin: String,
}

impl Sampler {
    /// Returns whether the sampler takes rows: it has a `limit`, or an `of` that picks the rows
    /// belonging to its parents. One that doesn't only declares `deps`.
    #[must_use]
    pub fn takes_rows(&self) -> bool {
        self.limit.is_some() || !self.of.is_empty()
    }

    /// Returns whether the sampler has `limit: all` and no `where` or `of`. `--since` can still
    /// narrow one with a `since`; the plan decides whether it keeps every row.
    #[must_use]
    pub fn is_unfiltered(&self) -> bool {
        self.filter.is_none()
            && self.of.is_empty()
            && self
                .limit
                .as_ref()
                .is_some_and(|limit| limit.rows == Rows::All)
    }

    /// Returns the most rows the sampler takes, if it has a cap.
    #[must_use]
    pub fn row_cap(&self) -> Option<u64> {
        match self.limit.as_ref()?.rows {
            Rows::All => None,
            Rows::Count(count) => Some(count),
        }
    }

    /// Returns the columns `--since` filters on, or none when the sampler has no time window.
    #[must_use]
    pub fn since_columns(&self) -> &[String] {
        self.limit.as_ref().map_or(&[], |limit| &limit.since)
    }

    /// Fails when the sampler can't take a row and has nothing to say about its `deps` either.
    fn check_reach(&self) -> Result<()> {
        if self.takes_rows() {
            return Ok(());
        }
        if self.filter.is_some() || self.order_by.is_some() {
            bail!(
                "{}: `where` and `order_by` need a `limit` to apply to; write `limit: all` to take \
                 every row that matches",
                self.origin
            );
        }
        if self.deps.is_empty() {
            bail!(
                "{}: takes no rows and declares no deps; write `limit: all` to copy the whole table",
                self.origin
            );
        }
        Ok(())
    }
}

/// How far a sampler reaches: how many rows it takes and how recent they must be.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Limit {
    /// The most rows to take.
    pub rows: Rows,
    /// Columns `--since` filters on. Rows match when any of them is recent enough. Empty means
    /// no time window.
    pub since: Vec<String>,
}

/// The row count of a [`Limit`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Rows {
    /// Every row.
    #[default]
    All,
    /// At most this many rows.
    Count(u64),
}

impl<'de> Deserialize<'de> for Limit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer)?
            .try_into()
            .map_err(de::Error::custom)
    }
}

impl TryFrom<Value> for Limit {
    type Error = String;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        const EXPECTED: &str = "a limit is a number, `all`, or a map of `rows` and `since`";
        let Value::Mapping(mapping) = value else {
            return Rows::try_from(value)
                .map(|rows| Self {
                    rows,
                    since: Vec::new(),
                })
                .map_err(|_| EXPECTED.to_string());
        };
        if mapping.is_empty() {
            return Err(
                "limit needs rows or since; write limit: all to take every row".to_string(),
            );
        }
        let mut limit = Self::default();
        for (key, value) in mapping {
            match key.as_str() {
                Some("rows") => limit.rows = Rows::try_from(value)?,
                Some("since") => limit.since = since_columns(value)?,
                _ => {
                    return Err(format!(
                        "unknown key in limit, expected `rows` or `since`: {key:?}"
                    ));
                }
            }
        }
        Ok(limit)
    }
}

impl TryFrom<Value> for Rows {
    type Error = String;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::String(word) if word == "all" => Ok(Self::All),
            Value::Number(number) => number
                .as_u64()
                .map(Self::Count)
                .ok_or_else(|| "rows is a number or `all`".to_string()),
            Value::Null
            | Value::Bool(_)
            | Value::String(_)
            | Value::Sequence(_)
            | Value::Mapping(_)
            | Value::Tagged(_) => Err("rows is a number or `all`".to_string()),
        }
    }
}

/// Reads `since`: one column name, or a list of them.
fn since_columns(value: Value) -> Result<Vec<String>, String> {
    const EXPECTED: &str = "since needs at least one column name";
    let names = match value {
        Value::String(name) => vec![name],
        Value::Sequence(names) => names
            .into_iter()
            .map(|name| match name {
                Value::String(name) => Ok(name),
                Value::Null
                | Value::Bool(_)
                | Value::Number(_)
                | Value::Sequence(_)
                | Value::Mapping(_)
                | Value::Tagged(_) => Err(EXPECTED.to_string()),
            })
            .collect::<Result<Vec<String>, String>>()?,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Mapping(_) | Value::Tagged(_) => {
            return Err(EXPECTED.to_string());
        }
    };
    if names.is_empty() || names.iter().any(|name| name.trim().is_empty()) {
        return Err(EXPECTED.to_string());
    }
    Ok(names)
}

/// The columns on one side of a reference: one, or several for a composite key. The columns on
/// the two sides pair up by position.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Columns(Vec<String>);

impl Columns {
    /// Wraps `columns`, which must not be empty.
    ///
    /// # Panics
    ///
    /// Panics if `columns` is empty.
    #[must_use]
    pub fn new(columns: Vec<String>) -> Self {
        assert!(
            !columns.is_empty(),
            "a reference follows at least one column"
        );
        Self(columns)
    }

    /// Returns the column names, in order.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Iterates over the column names, in order.
    pub fn iter(&self) -> slice::Iter<'_, String> {
        self.0.iter()
    }
}

impl From<&str> for Columns {
    fn from(column: &str) -> Self {
        Self(vec![column.to_string()])
    }
}

impl Display for Columns {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self.0.as_slice() {
            [column] => f.write_str(column),
            columns => write!(f, "({})", columns.join(", ")),
        }
    }
}

/// A parent's columns, and the table or named sampler whose extracted rows they read.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnRef {
    /// The parent table, or the name of a sampler of it.
    pub table: String,
    /// The parent's columns.
    pub columns: Columns,
}

impl Display for ColumnRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.table, self.columns)
    }
}

/// A `table.column` as written in YAML.
#[derive(Debug, Deserialize)]
#[serde(try_from = "String")]
struct Target {
    table: String,
    column: String,
}

impl TryFrom<String> for Target {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.split_once('.') {
            Some((table, column))
                if !table.is_empty() && !column.is_empty() && !column.contains('.') =>
            {
                Ok(Self {
                    table: table.to_string(),
                    column: column.to_string(),
                })
            }
            _ => Err(format!("`{value}` must look like `table.column`")),
        }
    }
}

/// One reference written as a list item: the `column: table.column` entries of a composite key.
struct Reference(Vec<(String, Target)>);

impl<'de> Deserialize<'de> for Reference {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Reference;

            fn expecting(&self, f: &mut Formatter<'_>) -> fmt::Result {
                f.write_str("a map of `column: table.column`")
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Reference, A::Error> {
                let mut entries: Vec<(String, Target)> = Vec::new();
                while let Some((column, target)) = map.next_entry::<String, Target>()? {
                    if entries.iter().any(|(seen, _)| *seen == column) {
                        return Err(de::Error::custom(format!("`{column}` is listed twice")));
                    }
                    entries.push((column, target));
                }
                Ok(Reference(entries))
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

impl Reference {
    /// Pairs the columns, sorted by the child's column so the order they're written in doesn't
    /// matter. Fails when they point at different tables.
    fn into_entry(self) -> Result<(Columns, ColumnRef), String> {
        let mut entries = self.0;
        entries.sort_by(|first, second| first.0.cmp(&second.0));
        let Some((first_column, first)) = entries.first() else {
            return Err("a reference needs at least one column".to_string());
        };
        if let Some((column, other)) = entries
            .iter()
            .find(|(_, target)| target.table != first.table)
        {
            return Err(format!(
                "one reference's columns must point at one table, but `{first_column}` points at \
                 {} and `{column}` at {}",
                first.table, other.table
            ));
        }
        let table = first.table.clone();
        let (children, parents): (Vec<String>, Vec<String>) = entries
            .into_iter()
            .map(|(column, target)| (column, target.column))
            .unzip();
        Ok((
            Columns::new(children),
            ColumnRef {
                table,
                columns: Columns::new(parents),
            },
        ))
    }
}

/// Reads `of` or `deps`: a map of `my_column: table.column`, one reference for each entry, or a
/// list of such maps, one reference for each item.
fn references<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<Columns, ColumnRef>, D::Error> {
    struct Visitor;

    impl Visitor {
        fn add<E: de::Error>(
            found: &mut BTreeMap<Columns, ColumnRef>,
            reference: Reference,
        ) -> Result<(), E> {
            let (columns, parent) = reference.into_entry().map_err(E::custom)?;
            if found.contains_key(&columns) {
                return Err(E::custom(format!("`{columns}` is listed twice")));
            }
            found.insert(columns, parent);
            Ok(())
        }
    }

    impl<'de> de::Visitor<'de> for Visitor {
        type Value = BTreeMap<Columns, ColumnRef>;

        fn expecting(&self, f: &mut Formatter<'_>) -> fmt::Result {
            f.write_str(
                "a map of `column: table.column`, or a list of such maps, one for each reference",
            )
        }

        fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut found: BTreeMap<Columns, ColumnRef> = BTreeMap::new();
            while let Some((column, target)) = map.next_entry::<String, Target>()? {
                Self::add(&mut found, Reference(vec![(column, target)]))?;
            }
            Ok(found)
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut found: BTreeMap<Columns, ColumnRef> = BTreeMap::new();
            while let Some(reference) = seq.next_element::<Reference>()? {
                Self::add(&mut found, reference)?;
            }
            Ok(found)
        }
    }

    deserializer.deserialize_any(Visitor)
}

impl Config {
    /// Loads and merges every file the paths or glob patterns name. A pattern must match at least
    /// one file; a file named twice is loaded once.
    pub fn load(patterns: &[String]) -> Result<Self> {
        let mut merged = Self::default();
        // The file that set `deps` first, so a file that disagrees can name it.
        let mut deps_file: Option<String> = None;
        for path in expand(patterns)? {
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let config = Self::parse_named(&text, &path.display().to_string())
                .with_context(|| format!("parsing {}", path.display()))?;
            merged.samplers.extend(config.samplers);
            merged.sanitize.merge(config.sanitize)?;
            if let Some(source) = config.deps {
                match (merged.deps, &deps_file) {
                    (Some(seen), Some(file)) if seen != source => bail!(
                        "deps is {seen} in {file} and {source} in {}",
                        path.display()
                    ),
                    (Some(_), Some(_)) => {}
                    (_, _) => {
                        merged.deps = Some(source);
                        deps_file = Some(path.display().to_string());
                    }
                }
            }
        }
        if merged.samplers.is_empty() {
            bail!("no samplers defined");
        }
        Ok(merged)
    }

    fn parse_named(text: &str, name: &str) -> Result<Self> {
        let file: File = serde_yaml_ng::from_str(text).map_err(|error| {
            if error.to_string().contains("unknown field `time_column`") {
                anyhow!("time_column moved into limit; write `limit: {{ since: created_at }}`")
            } else {
                error.into()
            }
        })?;
        let mut samplers = file.samplers;
        for (index, sampler) in samplers.iter_mut().enumerate() {
            sampler.origin = format!("{name} sampler #{} ({})", index + 1, sampler.table);
            sampler.check_reach()?;
        }
        let sanitize = Sanitize {
            columns: file.sanitize.columns.into_rules(name),
            tables: file
                .sanitize
                .tables
                .into_iter()
                .map(|(table, rules)| (table, rules.into_rules(name)))
                .collect(),
        };
        Ok(Self {
            samplers,
            sanitize,
            deps: file.deps,
        })
    }
}

// Parses one config on its own; its samplers' origins say `config`.
impl FromStr for Config {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let config = Self::parse_named(text, "config")?;
        if config.samplers.is_empty() {
            bail!("no samplers defined");
        }
        Ok(config)
    }
}

fn expand(patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for pattern in patterns {
        if !pattern.contains(['*', '?', '[']) {
            paths.push(PathBuf::from(pattern));
            continue;
        }
        let matches = glob::glob(pattern)
            .with_context(|| format!("bad glob pattern {pattern}"))?
            .collect::<Result<Vec<PathBuf>, _>>()?;
        if matches.is_empty() {
            bail!("{pattern} matches no files");
        }
        paths.extend(matches);
    }
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.canonicalize().unwrap_or_else(|_| path.clone())));
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use std::{env, process, slice};

    use super::*;

    fn count(rows: u64) -> Option<Limit> {
        Some(Limit {
            rows: Rows::Count(rows),
            since: Vec::new(),
        })
    }

    #[test]
    fn parses_samplers() -> Result<()> {
        let config: Config = r#"
samplers:
  - table: countries
    limit: all
  - table: users
    where: activated_at IS NOT NULL
    order_by: id DESC
    limit: 100
    of:
      country_id: countries.id
"#
        .parse()?;

        assert_eq!(config.samplers.len(), 2);
        assert!(
            config.samplers[0].is_unfiltered(),
            "countries has `limit: all` and no filter: {:?}",
            config.samplers[0]
        );

        let users = &config.samplers[1];
        assert_eq!(users.filter.as_deref(), Some("activated_at IS NOT NULL"));
        assert_eq!(users.order_by.as_deref(), Some("id DESC"));
        assert_eq!(users.limit, count(100));
        assert_eq!(
            users.of[&Columns::from("country_id")],
            ColumnRef {
                table: "countries".to_string(),
                columns: Columns::from("id"),
            }
        );
        assert!(!users.is_unfiltered(), "users has a filter: {users:?}");
        Ok(())
    }

    #[test]
    fn limit_shorthands_equal_their_maps() -> Result<()> {
        let config: Config = r#"
samplers:
  - table: a
    limit: 1000
  - table: b
    limit: { rows: 1000 }
  - table: c
    limit: all
  - table: d
    limit: { rows: all }
  - table: e
    limit: { since: created_at }
  - table: f
    limit: { rows: all, since: [created_at] }
"#
        .parse()?;

        let limits: Vec<&Option<Limit>> = config.samplers.iter().map(|s| &s.limit).collect();
        assert_eq!(limits[0], limits[1], "1000 is `{{ rows: 1000 }}`");
        assert_eq!(limits[2], limits[3], "all is `{{ rows: all }}`");
        assert_eq!(limits[4], limits[5], "rows defaults to all");
        assert_eq!(config.samplers[0].row_cap(), Some(1000));
        assert_eq!(config.samplers[2].row_cap(), None);
        Ok(())
    }

    #[test]
    fn parses_since_columns() -> Result<()> {
        let config: Config = r#"
samplers:
  - table: users
    limit: all
  - table: orders
    limit: { since: created_at }
  - table: events
    limit:
      rows: 50
      since: [created_at, updated_at]
"#
        .parse()?;

        assert!(
            config.samplers[0].since_columns().is_empty(),
            "users has no time window: {:?}",
            config.samplers[0].limit
        );
        assert_eq!(config.samplers[1].since_columns(), ["created_at"]);
        assert!(
            config.samplers[1].is_unfiltered(),
            "a since only filters once --since says how far back"
        );
        assert_eq!(
            config.samplers[2].since_columns(),
            ["created_at", "updated_at"]
        );
        assert_eq!(config.samplers[2].row_cap(), Some(50));
        Ok(())
    }

    #[test]
    fn rejects_bad_limits() {
        let cases = [
            (
                "{}",
                "limit needs rows or since; write limit: all to take every row",
            ),
            ("{ row: 100 }", "row"),
            ("{ since: [] }", "since needs at least one column name"),
            ("{ since: '' }", "since needs at least one column name"),
            (
                "{ since: [created_at, ''] }",
                "since needs at least one column name",
            ),
            ("{ rows: lots }", "rows is a number or `all`"),
            (
                "lots",
                "a limit is a number, `all`, or a map of `rows` and `since`",
            ),
            (
                "-1",
                "a limit is a number, `all`, or a map of `rows` and `since`",
            ),
            (
                "[100]",
                "a limit is a number, `all`, or a map of `rows` and `since`",
            ),
        ];
        for (bad, message) in cases {
            let yaml = format!("samplers:\n  - table: users\n    limit: {bad}\n");
            let error = yaml.parse::<Config>().expect_err("the limit is invalid");
            assert!(error.to_string().contains(message), "{bad}: {error}");
        }
    }

    #[test]
    fn says_where_time_column_went() {
        let error = "samplers:\n  - table: orders\n    time_column: created_at\n"
            .parse::<Config>()
            .expect_err("time_column moved into limit");
        assert!(
            error
                .to_string()
                .contains("time_column moved into limit; write `limit: { since: created_at }`"),
            "{error}"
        );
    }

    #[test]
    fn rejects_a_sampler_that_takes_nothing() {
        let error = "samplers:\n  - table: countries\n"
            .parse::<Config>()
            .expect_err("a bare table takes no rows");
        assert!(
            error.to_string().contains(
                "config sampler #1 (countries): takes no rows and declares no deps; write \
                 `limit: all` to copy the whole table"
            ),
            "{error}"
        );
    }

    #[test]
    fn rejects_where_and_order_by_without_a_limit() {
        for key in ["where: x = 1", "order_by: id DESC"] {
            let yaml = format!("samplers:\n  - table: users\n    {key}\n");
            let error = yaml
                .parse::<Config>()
                .expect_err("nothing to apply the key to");
            assert!(
                error.to_string().contains(
                    "config sampler #1 (users): `where` and `order_by` need a `limit` to apply \
                     to; write `limit: all` to take every row that matches"
                ),
                "{key}: {error}"
            );
        }
    }

    #[test]
    fn a_sampler_with_only_deps_or_of_is_valid_and_takes_nothing_on_its_own() -> Result<()> {
        let config: Config = r#"
samplers:
  - table: invoices
    deps:
      customer_id: customers.id
  - table: line_items
    of:
      order_id: orders.id
"#
        .parse()?;

        assert!(
            !config.samplers[0].takes_rows(),
            "deps alone take no rows: {:?}",
            config.samplers[0]
        );
        assert!(
            config.samplers[1].takes_rows(),
            "of picks the rows that belong to its parents"
        );
        Ok(())
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = "samplers:\n  - table: users\n    limt: 100\n"
            .parse::<Config>()
            .expect_err("`limt` isn't a sampler field");
        assert!(error.to_string().contains("limt"), "{error}");
    }

    #[test]
    fn rejects_bad_column_refs() {
        for bad in ["users", "users.", ".id", "db.users.id"] {
            let yaml = format!("samplers:\n  - table: phones\n    of:\n      user_id: {bad}\n");
            let error = yaml
                .parse::<Config>()
                .expect_err("a dep must be `table.column`");
            assert!(error.to_string().contains("table.column"), "{bad}: {error}");
        }
    }

    #[test]
    fn a_list_item_is_one_reference_over_several_columns() -> Result<()> {
        let config: Config = r#"
samplers:
  - table: memberships
    limit: all
    deps:
      - { user_id: users.id, tenant_id: users.tenant_id }
      - { created_by: users.id }
    of:
      - { shop_id: shops.shop_id, order_id: shops.id }
"#
        .parse()?;
        let sampler = &config.samplers[0];

        let composite = Columns::new(vec!["tenant_id".to_string(), "user_id".to_string()]);
        assert_eq!(
            sampler.deps[&composite],
            ColumnRef {
                table: "users".to_string(),
                columns: Columns::new(vec!["tenant_id".to_string(), "id".to_string()]),
            },
            "columns are paired, and sorted by the child's column so their order doesn't matter"
        );
        assert_eq!(
            sampler.deps[&Columns::from("created_by")].to_string(),
            "users.id",
            "another item is another reference"
        );
        assert_eq!(sampler.deps.len(), 2);
        assert_eq!(sampler.of.len(), 1, "`of` takes the list form too");
        assert_eq!(composite.to_string(), "(tenant_id, user_id)");
        Ok(())
    }

    #[test]
    fn rejects_a_reference_that_points_at_two_tables() {
        let error = r#"
samplers:
  - table: memberships
    limit: all
    deps:
      - { user_id: users.id, shop_id: shops.id }
"#
        .parse::<Config>()
        .expect_err("one reference points at one table");
        assert!(
            error.to_string().contains(
                "one reference's columns must point at one table, but `shop_id` points at shops \
                 and `user_id` at users"
            ),
            "{error}"
        );
    }

    #[test]
    fn rejects_the_same_columns_listed_twice() {
        for deps in [
            "- { user_id: users.id }\n      - { user_id: users.id }",
            "- { a: t.x, b: t.y }\n      - { b: t.y, a: t.x }",
        ] {
            let yaml = format!(
                "samplers:\n  - table: memberships\n    limit: all\n    deps:\n      {deps}\n"
            );
            let error = yaml
                .parse::<Config>()
                .expect_err("a reference can't be listed twice");
            assert!(error.to_string().contains("is listed twice"), "{error}");
        }
    }

    #[test]
    fn two_references_may_share_a_column() -> Result<()> {
        let config: Config = r#"
samplers:
  - table: tasks
    limit: all
    deps:
      - { tenant_id: users.tenant_id, user_id: users.id }
      - { tenant_id: projects.tenant_id, project_id: projects.id }
"#
        .parse()?;
        assert_eq!(config.samplers[0].deps.len(), 2, "both keys use tenant_id");
        Ok(())
    }

    #[test]
    fn a_file_says_where_references_come_from() -> Result<()> {
        let config: Config = "samplers:\n  - table: users\n    limit: all\n".parse()?;
        assert_eq!(
            config.deps, None,
            "a config that doesn't say leaves it open"
        );

        let config: Config =
            "deps: foreign_keys\nsamplers:\n  - table: users\n    limit: all\n".parse()?;
        assert_eq!(config.deps, Some(DepsSource::ForeignKeys));
        let config: Config =
            "deps: declared\nsamplers:\n  - table: users\n    limit: all\n".parse()?;
        assert_eq!(config.deps, Some(DepsSource::Declared));
        Ok(())
    }

    #[test]
    fn rejects_an_unknown_source_of_references() {
        let error = "deps: keys\nsamplers:\n  - table: users\n    limit: all\n"
            .parse::<Config>()
            .expect_err("`keys` isn't a source of references");
        assert!(
            error
                .to_string()
                .contains("unknown variant `keys`, expected `declared` or `foreign_keys`"),
            "{error}"
        );
    }

    #[test]
    fn parses_the_source_of_references_from_the_command_line() {
        assert_eq!("declared".parse(), Ok(DepsSource::Declared));
        assert_eq!("foreign-keys".parse(), Ok(DepsSource::ForeignKeys));
        assert_eq!("foreign_keys".parse(), Ok(DepsSource::ForeignKeys));
        assert_eq!(
            "keys".parse::<DepsSource>(),
            Err(
                "`keys` isn't where references come from; use `declared` or `foreign-keys`"
                    .to_string()
            )
        );
    }

    #[test]
    fn files_must_agree_on_where_references_come_from() -> Result<()> {
        let dir = TempDir::new("deps-source")?;
        let samplers = "samplers:\n  - table: users\n    limit: all\n";
        let keys = dir.write("keys.yml", &format!("deps: foreign_keys\n{samplers}"))?;
        let again = dir.write("again.yml", &format!("deps: foreign_keys\n{samplers}"))?;
        let silent = dir.write("silent.yml", samplers)?;
        let declared = dir.write("declared.yml", &format!("deps: declared\n{samplers}"))?;

        let config = Config::load(&[keys.clone(), silent.clone(), again])?;
        assert_eq!(
            config.deps,
            Some(DepsSource::ForeignKeys),
            "a file that doesn't say takes nothing back"
        );

        let error = Config::load(&[keys.clone(), silent, declared.clone()])
            .expect_err("two files disagree");
        assert_eq!(
            error.to_string(),
            format!("deps is foreign_keys in {keys} and declared in {declared}")
        );
        Ok(())
    }

    #[test]
    fn rejects_empty_config() {
        let parsed: Result<Config> = "samplers: []\n".parse();
        assert!(parsed.is_err(), "a config needs a sampler: {parsed:?}");
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Result<Self> {
            let dir = env::temp_dir().join(format!("tapas-{name}-{}", process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir)?;
            Ok(Self(dir))
        }

        fn write(&self, name: &str, text: &str) -> Result<String> {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, text)?;
            Ok(path.display().to_string())
        }

        fn pattern(&self, pattern: &str) -> String {
            self.0.join(pattern).display().to_string()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn merges_files_and_globs() -> Result<()> {
        let dir = TempDir::new("globs")?;
        let shop = dir.write(
            "shop/tapas.yml",
            "samplers:\n  - table: users\n    limit: all\n  - table: orders\n    limit: all\n",
        )?;
        dir.write(
            "billing/tapas.yml",
            "samplers:\n  - table: invoices\n    limit: all\n",
        )?;
        dir.write("empty/tapas.yml", "samplers: []\n")?;
        dir.write(
            "other.yml",
            "samplers:\n  - table: ignored\n    limit: all\n",
        )?;

        // The glob also matches the file named explicitly; it's only loaded once.
        let config = Config::load(&[shop.clone(), dir.pattern("**/tapas.yml")])?;
        let tables: Vec<&str> = config
            .samplers
            .iter()
            .map(|sampler| sampler.table.as_str())
            .collect();
        assert_eq!(tables, ["users", "orders", "invoices"]);
        assert_eq!(
            config.samplers[1].origin,
            format!("{shop} sampler #2 (orders)")
        );
        Ok(())
    }

    #[test]
    fn rejects_globs_that_match_nothing() -> Result<()> {
        let dir = TempDir::new("no-match")?;
        let pattern = dir.pattern("*.yml");
        let error =
            Config::load(slice::from_ref(&pattern)).expect_err("the directory has no YAML files");
        assert_eq!(error.to_string(), format!("{pattern} matches no files"));
        Ok(())
    }

    #[test]
    fn names_the_file_that_fails() -> Result<()> {
        let dir = TempDir::new("bad-file")?;
        dir.write("good.yml", "samplers:\n  - table: users\n    limit: all\n")?;
        let bad = dir.write("bad.yml", "samplers:\n  - table: users\n    limt: 1\n")?;
        let error = Config::load(&[dir.pattern("*.yml")]).expect_err("bad.yml has a typo");
        assert_eq!(error.to_string(), format!("parsing {bad}"));
        Ok(())
    }

    #[test]
    fn parses_sanitize_rules_in_order() -> Result<()> {
        let config: Config = r#"
samplers:
  - table: users
    limit: all
sanitize:
  columns:
    "*_id": keep
    Email: { fake: email }
    "*notes*": { value: Redacted for privacy }
    balance: { value: 0 }
    "*encrypted*": null
  tables:
    users:
      login: { template: "user_{id}@example.com" }
"#
        .parse()?;

        let global: Vec<(&str, &Replacement)> = config
            .sanitize
            .columns
            .iter()
            .map(|rule| (rule.pattern.as_str(), &rule.replacement))
            .collect();
        assert_eq!(
            global,
            [
                ("*_id", &Replacement::Keep),
                ("email", &Replacement::Fake(Fake::Email)),
                (
                    "*notes*",
                    &Replacement::Value("Redacted for privacy".to_string())
                ),
                ("balance", &Replacement::Value("0".to_string())),
                ("*encrypted*", &Replacement::Null),
            ]
        );
        assert_eq!(config.sanitize.columns[0].file, "config");

        let login = &config.sanitize.tables["users"][0];
        assert_eq!(login.pattern.as_str(), "login");
        let Replacement::Template(template) = &login.replacement else {
            panic!("login should be a template: {login:?}");
        };
        assert_eq!(
            template.parts,
            [
                TemplatePart::Text("user_".to_string()),
                TemplatePart::Column("id".to_string()),
                TemplatePart::Text("@example.com".to_string()),
            ]
        );
        Ok(())
    }

    #[test]
    fn matches_columns_ignoring_case() -> Result<()> {
        let exact: ColumnPattern = "Email".to_string().try_into().map_err(anyhow::Error::msg)?;
        assert!(exact.is_exact(), "no wildcards");
        assert!(exact.matches("EMAIL") && exact.matches("email"));
        assert!(
            !exact.matches("email_2"),
            "an exact name matches the whole name"
        );

        let glob: ColumnPattern = "*email*"
            .to_string()
            .try_into()
            .map_err(anyhow::Error::msg)?;
        assert!(!glob.is_exact(), "has a wildcard");
        for column in ["email", "recipient_email", "Account_Email_2"] {
            assert!(glob.matches(column), "{column}");
        }
        assert!(!glob.matches("e_mail"));
        Ok(())
    }

    #[test]
    fn rejects_bad_sanitize_rules() {
        for (rules, message) in [
            (
                "email: keepit",
                "a replacement is `keep`, `null`, `{ value: … }`, `{ fake: … }` or `{ template: … }`",
            ),
            (
                "email: { fake: e-mail }",
                "`e-mail` isn't a fake; use one of first_name,",
            ),
            (
                "email: { value: a, fake: email }",
                "a replacement is `keep`",
            ),
            (
                "login: { template: 'user_{id' }",
                "`user_{id` has a `{` without a closing `}`",
            ),
            (
                "login: { template: 'user_{}' }",
                "`user_{}` has an empty `{}`",
            ),
            (
                "login: { template: 'user_}' }",
                "`user_}` has a `}` without an opening `{`",
            ),
            ("email: keep\n      EMAIL: null", "`email` is listed twice"),
            ("'': keep", "a column pattern can't be empty"),
        ] {
            let yaml = format!(
                "samplers:\n  - table: users\n    limit: all\nsanitize:\n  tables:\n    users:\n      {rules}\n"
            );
            let error = yaml.parse::<Config>().expect_err("the rule is invalid");
            assert!(
                format!("{error:#}").contains(message),
                "{rules}: expected {message:?} in {error:#}"
            );
        }
    }

    #[test]
    fn merges_sanitize_rules_across_files() -> Result<()> {
        let dir = TempDir::new("sanitize-merge")?;
        let shared = dir.write(
            "shared.yml",
            "sanitize:\n  columns:\n    \"*email*\": { fake: email }\n",
        )?;
        let payroll = dir.write(
            "payroll/tapas.yml",
            "samplers:\n  - table: payroll_runs\n    limit: all\nsanitize:\n  tables:\n    employees:\n      \
             ssn: { fake: ssn }\n",
        )?;
        let people = dir.write(
            "people/tapas.yml",
            "samplers:\n  - table: employees\n    limit: all\nsanitize:\n  tables:\n    employees:\n      \
             first_name: { fake: first_name }\n",
        )?;

        let config = Config::load(&[shared.clone(), payroll.clone(), people.clone()])?;
        assert_eq!(config.samplers.len(), 2, "shared.yml holds no samplers");
        assert_eq!(config.sanitize.columns[0].file, shared);
        let employees: Vec<(&str, &str)> = config.sanitize.tables["employees"]
            .iter()
            .map(|rule| (rule.pattern.as_str(), rule.file.as_str()))
            .collect();
        assert_eq!(
            employees,
            [("ssn", payroll.as_str()), ("first_name", people.as_str())]
        );
        Ok(())
    }

    #[test]
    fn rejects_sanitize_rules_that_clash_across_files() -> Result<()> {
        let dir = TempDir::new("sanitize-clash")?;
        let first = dir.write(
            "a.yml",
            "samplers:\n  - table: users\n    limit: all\nsanitize:\n  columns:\n    ssn: { fake: ssn }\n  \
             tables:\n    users:\n      email: { fake: email }\n",
        )?;
        let second = dir.write("b.yml", "sanitize:\n  columns:\n    ssn: null\n")?;
        let error = Config::load(&[first.clone(), second.clone()])
            .expect_err("two files set rules for every table");
        assert_eq!(
            error.to_string(),
            format!(
                "sanitize.columns is in both {first} and {second}; keep the rules for every table \
                 in one file"
            )
        );

        let third = dir.write(
            "c.yml",
            "sanitize:\n  tables:\n    users:\n      EMAIL: null\n",
        )?;
        let error =
            Config::load(&[first.clone(), third.clone()]).expect_err("two files set users.email");
        assert_eq!(
            error.to_string(),
            format!("{third}: sanitize.tables.users sets `email`, which {first} sets too")
        );
        Ok(())
    }

    #[test]
    fn rejects_when_no_file_has_samplers() -> Result<()> {
        let dir = TempDir::new("all-empty")?;
        dir.write("a.yml", "samplers: []\n")?;
        let error = Config::load(&[dir.pattern("*.yml")]).expect_err("a.yml has no samplers");
        assert_eq!(error.to_string(), "no samplers defined");
        Ok(())
    }
}
