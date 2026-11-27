//! The database a sample is read from.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use mysql_async::{Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts};

use crate::sanitize::ColumnInfo;
use crate::sql;

/// The source database, with at most a set number of connections open at once. Clones share
/// the connections.
#[derive(Debug, Clone)]
pub enum Source {
    /// A MySQL server.
    MySql(Pool),
}

impl Source {
    /// Prepares connections to the database at `url`, at most `connections` at once, each only
    /// allowed to read. Nothing connects until the first query.
    pub fn try_new(url: &str, connections: usize) -> Result<Self> {
        let connection_options = Opts::from_url(url).context("parsing the source URL")?;
        let constraints = PoolConstraints::new(1, connections.max(1)).context("pool size")?;
        let connection_options = OptsBuilder::from_opts(connection_options)
            .pool_opts(PoolOpts::default().with_constraints(constraints))
            // tapas only reads; make sure of it.
            .setup(vec!["SET SESSION TRANSACTION READ ONLY"]);
        Ok(Source::MySql(Pool::new(connection_options)))
    }

    /// Returns the columns of `tables` in the current database, per table, in their order.
    pub async fn columns(&self, tables: &[&str]) -> Result<BTreeMap<String, Vec<ColumnInfo>>> {
        use mysql_async::prelude::Queryable;

        let query = sql::columns_query(tables);
        let rows: Vec<(String, String, String, Option<u64>)> = match self {
            Source::MySql(pool) => {
                let mut connection = pool.get_conn().await?;
                connection
                    .query(query.as_slice())
                    .await
                    .context("reading the source's columns")?
            }
        };
        let mut columns: BTreeMap<String, Vec<ColumnInfo>> = BTreeMap::new();
        for (table, name, data_type, max_chars) in rows {
            columns
                .entry(table)
                .or_default()
                .push(ColumnInfo::new(name, &data_type, max_chars));
        }
        Ok(columns)
    }

    /// Closes every connection.
    pub async fn disconnect(self) {
        match self {
            Source::MySql(pool) => {
                pool.disconnect().await.ok();
            }
        }
    }
}
