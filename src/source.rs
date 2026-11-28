//! The database a sample is read from.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use mysql_async::{Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts};
use tokio_postgres::SimpleQueryMessage;

use crate::config::{ColumnRef, Columns};
use crate::plan::ForeignKey;
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

    /// Returns the foreign keys between tables of the current database, which a sample follows
    /// like `deps`. A key over several columns is one entry.
    pub async fn foreign_keys(&self) -> Result<Vec<ForeignKey>> {
        use mysql_async::prelude::Queryable;

        let query = sql::foreign_keys_query(self.dialect());
        let rows: Vec<(String, String, String, String, String)> = match self {
            Source::MySql(pool) => pool
                .get_conn()
                .await?
                .query(query)
                .await
                .context("reading the source's foreign keys")?,
            Source::Postgres(pool) => {
                let messages = pool
                    .get()
                    .await?
                    .simple_query(query)
                    .await
                    .context("reading the source's foreign keys")?;
                let mut rows = Vec::new();
                for message in messages {
                    // Every value comes as text.
                    let SimpleQueryMessage::Row(row) = message else {
                        continue;
                    };
                    let text = |index: usize| row.get(index).unwrap_or_default().to_string();
                    rows.push((text(0), text(1), text(2), text(3), text(4)));
                }
                rows
            }
        };
        Ok(group_foreign_keys(rows))
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

/// Joins the rows of a foreign keys query, one per column of a key, into one entry per key. Rows
/// of one key come together and in the key's order.
#[must_use]
fn group_foreign_keys(rows: Vec<(String, String, String, String, String)>) -> Vec<ForeignKey> {
    let mut keys: Vec<(ForeignKey, Vec<String>, Vec<String>)> = Vec::new();
    for (name, table, column, parent, parent_column) in rows {
        match keys.last_mut() {
            Some((key, columns, parent_columns)) if key.name == name && key.table == table => {
                columns.push(column);
                parent_columns.push(parent_column);
            }
            _ => keys.push((
                ForeignKey {
                    name,
                    table,
                    columns: Columns::from(column.as_str()),
                    parent: ColumnRef {
                        table: parent,
                        columns: Columns::from(parent_column.as_str()),
                    },
                },
                vec![column],
                vec![parent_column],
            )),
        }
    }
    keys.into_iter()
        .map(|(mut key, columns, parent_columns)| {
            key.columns = Columns::new(columns);
            key.parent.columns = Columns::new(parent_columns);
            key
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        name: &str,
        table: &str,
        column: &str,
        parent: &str,
        to: &str,
    ) -> (String, String, String, String, String) {
        let text = |value: &str| value.to_string();
        (
            text(name),
            text(table),
            text(column),
            text(parent),
            text(to),
        )
    }

    #[test]
    fn joins_the_columns_of_a_key() {
        let keys = group_foreign_keys(vec![
            row(
                "memberships_user_fk",
                "memberships",
                "tenant_id",
                "users",
                "tenant_id",
            ),
            row(
                "memberships_user_fk",
                "memberships",
                "user_id",
                "users",
                "id",
            ),
            row("orders_user_fk", "orders", "user_id", "users", "id"),
        ]);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].name, "memberships_user_fk");
        assert_eq!(keys[0].columns.to_string(), "(tenant_id, user_id)");
        assert_eq!(keys[0].parent.to_string(), "users.(tenant_id, id)");
        assert_eq!(keys[1].columns.to_string(), "user_id");
        assert_eq!(keys[1].parent.to_string(), "users.id");
    }
}
