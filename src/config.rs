use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub samplers: Vec<Sampler>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sampler {
    pub table: String,
    /// Lets other samplers depend on just this sampler's rows: `deps: { id: <name>.column }`.
    pub name: Option<String>,
    #[serde(rename = "where")]
    pub filter: Option<String>,
    pub order_by: Option<String>,
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
    /// The `--since` condition, filled in by the plan.
    #[serde(skip)]
    pub time_filter: Option<String>,
}

impl Sampler {
    /// A sampler that keeps every row of its table.
    pub fn is_full(&self) -> bool {
        self.filter.is_none()
            && self.time_filter.is_none()
            && self.limit.is_none()
            && self.of.is_empty()
    }
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
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
        return Err(serde::de::Error::custom(
            "time_column needs at least one column name",
        ));
    }
    Ok(columns)
}

/// A `table.column` reference to a parent's extracted rows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct ColumnRef {
    pub table: String,
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

impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.table, self.column)
    }
}

impl Config {
    /// Loads and merges every file the paths or glob patterns name. A pattern must match at least
    /// one file; a file named twice is loaded once.
    pub fn load(patterns: &[String]) -> Result<Self> {
        let mut samplers = Vec::new();
        for path in expand(patterns)? {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let config = Self::parse_named(&text, &path.display().to_string())
                .with_context(|| format!("parsing {}", path.display()))?;
            samplers.extend(config.samplers);
        }
        if samplers.is_empty() {
            bail!("no samplers defined");
        }
        Ok(Self { samplers })
    }

    /// Parses one config on its own.
    pub fn parse(text: &str) -> Result<Self> {
        let config = Self::parse_named(text, "config")?;
        if config.samplers.is_empty() {
            bail!("no samplers defined");
        }
        Ok(config)
    }

    fn parse_named(text: &str, name: &str) -> Result<Self> {
        let mut config: Config = serde_yaml_ng::from_str(text)?;
        for (index, sampler) in config.samplers.iter_mut().enumerate() {
            sampler.origin = format!("{name} sampler #{} ({})", index + 1, sampler.table);
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
            .collect::<Result<Vec<_>, _>>()?;
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
    use super::*;

    #[test]
    fn parses_samplers() {
        let config = Config::parse(
            r#"
samplers:
  - table: countries
  - table: users
    where: activated_at IS NOT NULL
    order_by: id DESC
    limit: 100
    of:
      country_id: countries.id
"#,
        )
        .unwrap();

        assert_eq!(config.samplers.len(), 2);
        assert!(config.samplers[0].is_full());

        let users = &config.samplers[1];
        assert_eq!(users.filter.as_deref(), Some("activated_at IS NOT NULL"));
        assert_eq!(users.order_by.as_deref(), Some("id DESC"));
        assert_eq!(users.limit, Some(100));
        assert_eq!(
            users.of["country_id"],
            ColumnRef {
                table: "countries".into(),
                column: "id".into()
            }
        );
        assert!(!users.is_full());
    }

    #[test]
    fn parses_time_columns() {
        let config = Config::parse(
            r#"
samplers:
  - table: users
  - table: orders
    time_column: created_at
  - table: events
    time_column: [created_at, updated_at]
"#,
        )
        .unwrap();

        assert!(config.samplers[0].time_column.is_empty());
        assert_eq!(config.samplers[1].time_column, ["created_at"]);
        assert_eq!(config.samplers[2].time_column, ["created_at", "updated_at"]);
    }

    #[test]
    fn rejects_empty_time_columns() {
        for bad in ["[]", "''", "[created_at, '']"] {
            let yaml = format!("samplers:\n  - table: users\n    time_column: {bad}\n");
            let error = Config::parse(&yaml).unwrap_err();
            assert!(error.to_string().contains("time_column"), "{bad}: {error}");
        }
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = Config::parse("samplers:\n  - table: users\n    limt: 100\n").unwrap_err();
        assert!(error.to_string().contains("limt"), "{error}");
    }

    #[test]
    fn rejects_bad_column_refs() {
        for bad in ["users", "users.", ".id", "db.users.id"] {
            let yaml = format!("samplers:\n  - table: phones\n    of:\n      user_id: {bad}\n");
            let error = Config::parse(&yaml).unwrap_err();
            assert!(error.to_string().contains("table.column"), "{bad}: {error}");
        }
    }

    #[test]
    fn rejects_empty_config() {
        assert!(Config::parse("samplers: []\n").is_err());
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("tapas-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, text: &str) -> String {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path.display().to_string()
        }

        fn pattern(&self, pattern: &str) -> String {
            self.0.join(pattern).display().to_string()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn merges_files_and_globs() {
        let dir = TempDir::new("globs");
        let shop = dir.write(
            "shop/tapas.yml",
            "samplers:\n  - table: users\n  - table: orders\n",
        );
        dir.write("billing/tapas.yml", "samplers:\n  - table: invoices\n");
        dir.write("empty/tapas.yml", "samplers: []\n");
        dir.write("other.yml", "samplers:\n  - table: ignored\n");

        // The glob also matches the file named explicitly; it's only loaded once.
        let config = Config::load(&[shop.clone(), dir.pattern("**/tapas.yml")]).unwrap();
        let tables: Vec<&str> = config.samplers.iter().map(|s| s.table.as_str()).collect();
        assert_eq!(tables, ["users", "orders", "invoices"]);
        assert_eq!(
            config.samplers[1].origin,
            format!("{shop} sampler #2 (orders)")
        );
    }

    #[test]
    fn rejects_globs_that_match_nothing() {
        let dir = TempDir::new("no-match");
        let pattern = dir.pattern("*.yml");
        let error = Config::load(std::slice::from_ref(&pattern)).unwrap_err();
        assert_eq!(error.to_string(), format!("{pattern} matches no files"));
    }

    #[test]
    fn names_the_file_that_fails() {
        let dir = TempDir::new("bad-file");
        dir.write("good.yml", "samplers:\n  - table: users\n");
        let bad = dir.write("bad.yml", "samplers:\n  - table: users\n    limt: 1\n");
        let error = Config::load(&[dir.pattern("*.yml")]).unwrap_err();
        assert_eq!(error.to_string(), format!("parsing {bad}"));
    }

    #[test]
    fn rejects_when_no_file_has_samplers() {
        let dir = TempDir::new("all-empty");
        dir.write("a.yml", "samplers: []\n");
        let error = Config::load(&[dir.pattern("*.yml")]).unwrap_err();
        assert_eq!(error.to_string(), "no samplers defined");
    }
}
