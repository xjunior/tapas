//! What the MySQL tests share: the server's URL, and sampling a seeded database into an empty one.

use std::collections::BTreeMap;
use std::{env, fs, process};

use anyhow::{Context, Result};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use tapas::{Config, Options, Output, Plan, Since};

const URL: &str = "TAPAS_TEST_DATABASE_URL";

/// The URL of the test server, or nothing when the tests should skip. A missing URL fails under
/// CI, where skipping would hide a broken setup.
pub fn url() -> Option<String> {
    let url = env::var(URL).ok();
    if url.is_none() {
        assert!(env::var_os("CI").is_none(), "{URL} must be set in CI");
        eprintln!("{URL} not set, skipping");
    }
    url
}

pub async fn connect(base: &str) -> Result<Conn> {
    Ok(Conn::new(Opts::from_url(base)?).await?)
}

/// Drops the database `name` if it exists, and creates it empty.
pub async fn recreate(connection: &mut Conn, name: &str) -> Result<()> {
    connection
        .query_drop(format!(
            "DROP DATABASE IF EXISTS {name}; CREATE DATABASE {name}"
        ))
        .await?;
    Ok(())
}

/// The ids of `table`, in order.
pub async fn ids(connection: &mut Conn, table: &str) -> Result<Vec<u64>> {
    let ids: Vec<u64> = connection
        .query(format!("SELECT id FROM {table} ORDER BY id"))
        .await?;
    Ok(ids)
}

/// The one number `query` returns.
pub async fn scalar(connection: &mut Conn, query: &str) -> Result<u64> {
    let value: Option<u64> = connection.query_first(query).await?;
    value.with_context(|| format!("`{query}` returned no rows"))
}

/// A sample loaded into a target database.
pub struct Sample {
    /// A connection to the server, for looking at the source and the target.
    pub connection: Conn,
    /// The rows tapas wrote, per table.
    pub rows: BTreeMap<String, u64>,
    pub source: String,
    pub target: String,
}

/// Extracts a sample of a database seeded with `schema` and `seed` through `samplers`, and loads
/// it into an empty copy that has `schema`. The databases are called `tapas_{name}_source` and
/// `tapas_{name}_target`.
pub async fn sample(
    base: &str,
    name: &str,
    (schema, seed): (&str, &str),
    samplers: &str,
    since: Option<&str>,
    options: Options,
) -> Result<Sample> {
    let (source, target) = (
        format!("tapas_{name}_source"),
        format!("tapas_{name}_target"),
    );
    let mut connection = connect(base).await?;
    for database in [&source, &target] {
        recreate(&mut connection, database).await?;
    }
    connection
        .query_drop(format!("USE {source}; {schema} {seed}"))
        .await?;
    connection
        .query_drop(format!("USE {target}; {schema}"))
        .await?;

    let config: Config = samplers.parse()?;
    let since = since
        .map(str::parse::<Since>)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let output = env::temp_dir().join(format!("tapas-{name}-{}.sql", process::id()));
    let rows = tapas::run(
        Plan::try_new(config, since)?,
        &format!("{base}/{source}"),
        4,
        &Output::File(output.clone()),
        options,
    )
    .await?;
    let sql = fs::read_to_string(&output)?;
    fs::remove_file(&output)?;
    connection
        .query_drop(format!("USE {target}; {sql}"))
        .await?;
    Ok(Sample {
        connection,
        rows,
        source,
        target,
    })
}

impl Sample {
    /// Drops both databases. Tests call it last, so a failing one leaves them to look at.
    pub async fn finish(mut self) -> Result<()> {
        self.connection
            .query_drop(format!(
                "DROP DATABASE {}; DROP DATABASE {}",
                self.source, self.target
            ))
            .await?;
        self.connection.disconnect().await?;
        Ok(())
    }
}
