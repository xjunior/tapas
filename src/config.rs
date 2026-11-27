//! The YAML config: which tables to sample, and how their rows depend on each other.

use std::collections::{BTreeMap, HashSet};
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, de};

/// Every sampler of one or more config files.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The samplers, in the order the files list them.
    pub samplers: Vec<Sampler>,
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
        let mut samplers = Vec::new();
        for path in expand(patterns)? {
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let config = Self::parse_named(&text, &path.display().to_string())
                .with_context(|| format!("parsing {}", path.display()))?;
            samplers.extend(config.samplers);
        }
        if samplers.is_empty() {
            bail!("no samplers defined");
        }
        Ok(Self { samplers })
    }

    fn parse_named(text: &str, name: &str) -> Result<Self> {
        let mut config: Config = serde_yaml_ng::from_str(text)?;
        for (index, sampler) in config.samplers.iter_mut().enumerate() {
            sampler.origin = format!("{name} sampler #{} ({})", index + 1, sampler.table);
        }
        Ok(config)
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
    fn rejects_when_no_file_has_samplers() -> Result<()> {
        let dir = TempDir::new("all-empty")?;
        dir.write("a.yml", "samplers: []\n")?;
        let error = Config::load(&[dir.pattern("*.yml")]).expect_err("a.yml has no samplers");
        assert_eq!(error.to_string(), "no samplers defined");
        Ok(())
    }
}
