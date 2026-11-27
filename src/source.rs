//! The database a sample is read from.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use mysql_async::{Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts};
use tokio_postgres::SimpleQueryMessage;

use crate::postgres;
use crate::sanitize::ColumnInfo;
use crate::sql::{self, Dialect};

/// Session settings for every Postgres source connection: it only reads, and writes values as
/// text that reads back exactly the same: ISO dates, floats with every digit, and backslashes as
/// plain characters.
const POSTGRES_SETTINGS: &str = "-c default_transaction_read_only=on \
                                 -c standard_conforming_strings=on \
                                 -c DateStyle=ISO -c IntervalStyle=postgres \
                                 -c extra_float_digits=1";

/// The source database, with at most a set number of connections open at once. Clones share
/// the connections.
#[derive(Clone)]
pub enum Source {
    /// A MySQL server.
    MySql(Pool),
    /// A Postgres server.
    Postgres(postgres::Pool),
}

impl Source {
    /// Prepares connections to the `dialect` database at `url`, at most `connections` at once,
    /// each only allowed to read. Nothing connects until the first query.
    pub fn try_new(dialect: Dialect, url: &str, connections: usize) -> Result<Self> {
        match dialect {
            Dialect::MySql => {
                let connection_options = Opts::from_url(url).context("parsing the source URL")?;
                let constraints =
                    PoolConstraints::new(1, connections.max(1)).context("pool size")?;
                let connection_options = OptsBuilder::from_opts(connection_options)
                    .pool_opts(PoolOpts::default().with_constraints(constraints))
                    // tapas only reads; make sure of it.
                    .setup(vec!["SET SESSION TRANSACTION READ ONLY"]);
                Ok(Source::MySql(Pool::new(connection_options)))
            }
            Dialect::Postgres => {
                let mut config = postgres::config(url).context("parsing the source URL")?;
                let settings = match config.get_options() {
                    Some(own) => format!("{own} {POSTGRES_SETTINGS}"),
                    None => POSTGRES_SETTINGS.to_string(),
                };
                config.options(settings);
                Ok(Source::Postgres(postgres::Pool::new(config, connections)))
            }
        }
    }

    /// Returns the SQL the source speaks.
    #[must_use]
    pub fn dialect(&self) -> Dialect {
        match self {
            Source::MySql(_) => Dialect::MySql,
            Source::Postgres(_) => Dialect::Postgres,
        }
    }

    /// Returns the columns of `tables` in the current database, per table, in their order.
    pub async fn columns(&self, tables: &[&str]) -> Result<BTreeMap<String, Vec<ColumnInfo>>> {
        use mysql_async::prelude::Queryable;

        let query = sql::columns_query(self.dialect(), tables);
        let mut columns: BTreeMap<String, Vec<ColumnInfo>> = BTreeMap::new();
        let mut add = |table: String, name: String, data_type: &str, max_chars: Option<u64>| {
            columns
                .entry(table)
                .or_default()
                .push(ColumnInfo::new(name, data_type, max_chars));
        };
        match self {
            Source::MySql(pool) => {
                let mut connection = pool.get_conn().await?;
                let rows: Vec<(String, String, String, Option<u64>)> = connection
                    .query(query.as_slice())
                    .await
                    .context("reading the source's columns")?;
                for (table, name, data_type, max_chars) in rows {
                    add(table, name, &data_type, max_chars);
                }
            }
            Source::Postgres(pool) => {
                let query = str::from_utf8(&query).context("table names are UTF-8")?;
                let messages = pool
                    .get()
                    .await?
                    .simple_query(query)
                    .await
                    .context("reading the source's columns")?;
                for message in messages {
                    // Every value comes as text.
                    let SimpleQueryMessage::Row(row) = message else {
                        continue;
                    };
                    let text = |index: usize| row.get(index).unwrap_or_default().to_string();
                    let max_chars: Option<u64> = row
                        .get(3)
                        .map(str::parse)
                        .transpose()
                        .context("reading a column's length")?;
                    add(text(0), text(1), row.get(2).unwrap_or_default(), max_chars);
                }
            }
        }
        Ok(columns)
    }

    /// Closes every connection.
    pub async fn disconnect(self) {
        match self {
            Source::MySql(pool) => {
                pool.disconnect().await.ok();
            }
            Source::Postgres(pool) => pool.close(),
        }
    }
}
