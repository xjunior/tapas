//! The YAML config: which tables to sample, how their rows depend on each other, and what to write
//! in place of sensitive values.

use std::collections::{BTreeMap, HashSet};
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
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
}

/// What one file holds. A file can hold only samplers, only `sanitize:`, or both.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    samplers: Vec<Sampler>,
    #[serde(default)]
    sanitize: SanitizeFile,
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
    /// The sampler extracts at most this many rows.
    pub limit: Option<u64>,
    /// `my_column: parent.column`: only rows that belong to the parent's extracted rows.
    #[serde(default)]
    pub of: BTreeMap<String, ColumnRef>,
    /// `my_column: parent.column`: the parent rows this table's rows point at get extracted too.
    /// Applies to every row of the table, whichever sampler brought it in.
    #[serde(default)]
    pub deps: BTreeMap<String, ColumnRef>,
    /// Columns `--since` filters on. Rows match when any of them is recent enough.
    #[serde(default, deserialize_with = "one_or_many")]
    pub time_column: Vec<String>,
    /// Where the sampler was defined, for messages: `users.yml sampler #2 (users)`.
    #[serde(skip)]
    pub origin: String,
}

impl Sampler {
    /// Returns whether the sampler has no `where`, `limit` or `of`. `--since` can still narrow
    /// one with a `time_column`; the plan decides whether it keeps every row.
    #[must_use]
    pub fn is_unfiltered(&self) -> bool {
        self.filter.is_none() && self.limit.is_none() && self.of.is_empty()
    }
}

fn one_or_many<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    let columns = match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(column) => vec![column],
        OneOrMany::Many(columns) => columns,
    };
    if columns.is_empty() || columns.iter().any(|column| column.trim().is_empty()) {
        return Err(de::Error::custom(
            "time_column needs at least one column name",
        ));
    }
    Ok(columns)
}

/// A `table.column` reference to a parent's extracted rows.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "String")]
pub struct ColumnRef {
    /// The parent table, or the name of a sampler of it.
    pub table: String,
    /// The parent's column.
    pub column: String,
}

impl TryFrom<String> for ColumnRef {
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

impl Display for ColumnRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.table, self.column)
    }
}

impl Config {
    /// Loads and merges every file the paths or glob patterns name. A pattern must match at least
    /// one file; a file named twice is loaded once.
    pub fn load(patterns: &[String]) -> Result<Self> {
        let mut merged = Self::default();
        for path in expand(patterns)? {
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let config = Self::parse_named(&text, &path.display().to_string())
                .with_context(|| format!("parsing {}", path.display()))?;
            merged.samplers.extend(config.samplers);
            merged.sanitize.merge(config.sanitize)?;
        }
        if merged.samplers.is_empty() {
            bail!("no samplers defined");
        }
        Ok(merged)
    }

    fn parse_named(text: &str, name: &str) -> Result<Self> {
        let file: File = serde_yaml_ng::from_str(text)?;
        let mut samplers = file.samplers;
        for (index, sampler) in samplers.iter_mut().enumerate() {
            sampler.origin = format!("{name} sampler #{} ({})", index + 1, sampler.table);
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
        Ok(Self { samplers, sanitize })
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

    #[test]
    fn parses_samplers() -> Result<()> {
        let config: Config = r#"
samplers:
  - table: countries
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
            "countries has no filter, limit or deps: {:?}",
            config.samplers[0]
        );

        let users = &config.samplers[1];
        assert_eq!(users.filter.as_deref(), Some("activated_at IS NOT NULL"));
        assert_eq!(users.order_by.as_deref(), Some("id DESC"));
        assert_eq!(users.limit, Some(100));
        assert_eq!(
            users.of["country_id"],
            ColumnRef {
                table: "countries".to_string(),
                column: "id".to_string(),
            }
        );
        assert!(!users.is_unfiltered(), "users has a filter: {users:?}");
        Ok(())
    }

    #[test]
    fn parses_time_columns() -> Result<()> {
        let config: Config = r#"
samplers:
  - table: users
  - table: orders
    time_column: created_at
  - table: events
    time_column: [created_at, updated_at]
"#
        .parse()?;

        assert!(
            config.samplers[0].time_column.is_empty(),
            "users has no time_column, yet got {:?}",
            config.samplers[0].time_column
        );
        assert_eq!(config.samplers[1].time_column, ["created_at"]);
        assert!(
            config.samplers[1].is_unfiltered(),
            "a time_column only filters once --since says how far back"
        );
        assert_eq!(config.samplers[2].time_column, ["created_at", "updated_at"]);
        Ok(())
    }

    #[test]
    fn rejects_empty_time_columns() {
        for bad in ["[]", "''", "[created_at, '']"] {
            let yaml = format!("samplers:\n  - table: users\n    time_column: {bad}\n");
            let error = yaml
                .parse::<Config>()
                .expect_err("an empty time_column is invalid");
            assert!(error.to_string().contains("time_column"), "{bad}: {error}");
        }
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
            "samplers:\n  - table: users\n  - table: orders\n",
        )?;
        dir.write("billing/tapas.yml", "samplers:\n  - table: invoices\n")?;
        dir.write("empty/tapas.yml", "samplers: []\n")?;
        dir.write("other.yml", "samplers:\n  - table: ignored\n")?;

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
        dir.write("good.yml", "samplers:\n  - table: users\n")?;
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
                "samplers:\n  - table: users\nsanitize:\n  tables:\n    users:\n      {rules}\n"
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
            "samplers:\n  - table: payroll_runs\nsanitize:\n  tables:\n    employees:\n      \
             ssn: { fake: ssn }\n",
        )?;
        let people = dir.write(
            "people/tapas.yml",
            "samplers:\n  - table: employees\nsanitize:\n  tables:\n    employees:\n      \
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
            "samplers:\n  - table: users\nsanitize:\n  columns:\n    ssn: { fake: ssn }\n  \
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
