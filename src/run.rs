//! Runs a whole extraction: connects to the source, extracts, and writes the output.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use mysql_async::Opts;

use crate::extract::{self, Options};
use crate::output::{Outcome, Output, Sink};
use crate::plan::{ForeignKey, Plan};
use crate::postgres;
use crate::sanitize::{Sanitizer, Seed};
use crate::source::Source;
use crate::sql::Dialect;

/// Extracts the sample `plan` describes from `source_url` into `output`, a SQL file or a target
/// database. Returns the number of rows written per table.
///
/// # Panics
///
/// Panics if `options.chunk_size` is 0, or if a task that writes the output panics.
pub async fn run(
    plan: Plan,
    source_url: &str,
    connections: usize,
    output: &Output,
    options: Options,
) -> Result<BTreeMap<String, u64>> {
    plan.check_since()?;
    let dialect = dialect_of(source_url, "source")?;
    if let Output::Database(target_url) = output {
        let target = dialect_of(target_url, "target")?;
        if target != dialect {
            bail!(
                "the source is {dialect} and the target is {target}; tapas only writes into a \
                 target of the same kind"
            );
        }
        if same_database(source_url, target_url) {
            bail!("the target is the source database; tapas won't write into what it reads");
        }
    }

    let source = Source::try_new(dialect, source_url, connections)?;

    // Checked before the output opens, so a bad rule leaves nothing behind.
    let sanitizer = match sanitizer(&plan, &source, options.sanitize_seed).await {
        Ok(sanitizer) => sanitizer,
        Err(error) => {
            source.disconnect().await;
            return Err(error);
        }
    };
    let tables: Vec<&str> = plan.tables.keys().map(String::as_str).collect();
    let (sink, sender) = Sink::open(output, dialect, &tables, options.debug).await?;
    let extracted = extract::extract(plan, source.clone(), options, sanitizer, sender).await;
    let outcome = if extracted.is_ok() {
        Outcome::Keep
    } else {
        Outcome::Discard
    };
    let written = sink.finish(outcome).await;
    source.disconnect().await;

    // A writer that fails stops taking statements, which fails the extraction too. Its own error
    // says why, so it comes first.
    written?;
    extracted
}

/// Returns the foreign keys between tables of the database at `source_url`, which
/// [`Plan::try_new_with_foreign_keys`] turns into references.
pub async fn foreign_keys(source_url: &str) -> Result<Vec<ForeignKey>> {
    let dialect = dialect_of(source_url, "source")?;
    let source = Source::try_new(dialect, source_url, 1)?;
    let keys = source.foreign_keys().await;
    source.disconnect().await;
    keys
}

/// Works out what the plan's sanitize rules do to each table, from the source's columns. Reads
/// them in one query, and only when there are rules.
async fn sanitizer(plan: &Plan, source: &Source, seed: Option<Seed>) -> Result<Sanitizer> {
    if plan.sanitize.is_empty() {
        return Ok(Sanitizer::default());
    }
    let tables: Vec<&str> = if plan.sanitize.columns.is_empty() {
        plan.sanitize.tables.keys().map(String::as_str).collect()
    } else {
        plan.tables.keys().map(String::as_str).collect()
    };
    let columns = source.columns(&tables).await?;
    Sanitizer::try_new(
        plan,
        &columns,
        source.dialect(),
        seed.unwrap_or_else(Seed::random),
    )
}

/// Returns the dialect of the database at `url`, the `what` URL, or says which schemes work.
fn dialect_of(url: &str, what: &str) -> Result<Dialect> {
    match Dialect::from_url(url) {
        Some(dialect) => Ok(dialect),
        None => bail!("the {what} URL must start with mysql:// or postgres://"),
    }
}

/// Returns whether two URLs point at the same database on the same server.
#[must_use]
fn same_database(first: &str, second: &str) -> bool {
    match (Dialect::from_url(first), Dialect::from_url(second)) {
        (Some(Dialect::MySql), Some(Dialect::MySql)) => {
            let (Ok(first), Ok(second)) = (Opts::from_url(first), Opts::from_url(second)) else {
                return false;
            };
            first.ip_or_hostname() == second.ip_or_hostname()
                && first.tcp_port() == second.tcp_port()
                && first.db_name() == second.db_name()
        }
        (Some(Dialect::Postgres), Some(Dialect::Postgres)) => {
            let (Ok(first), Ok(second)) = (postgres::config(first), postgres::config(second))
            else {
                return false;
            };
            postgres::location(&first) == postgres::location(&second)
        }
        (Some(Dialect::MySql), Some(Dialect::Postgres))
        | (Some(Dialect::Postgres), Some(Dialect::MySql))
        | (None, _)
        | (_, None) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn same_database_compares_host_port_and_database() {
        assert!(
            same_database(
                "mysql://reader@db:3306/shop",
                "mysql://root:secret@db:3306/shop"
            ),
            "users and passwords don't make it another database"
        );
        assert!(
            !same_database("mysql://db:3306/shop", "mysql://db:3306/shop_dev"),
            "another database on the same server is fine"
        );
        assert!(
            !same_database("mysql://db:3306/shop", "mysql://db:3307/shop"),
            "the same name on another port is another server"
        );
        assert!(
            !same_database("not a url", "not a url"),
            "URLs that don't parse fail later, with their own error"
        );
    }

    #[test]
    fn same_database_compares_postgres_urls_too() {
        assert!(
            same_database(
                "postgres://reader@db/shop",
                "postgresql://root:secret@db:5432/shop"
            ),
            "5432 is Postgres's default port, and both schemes mean Postgres"
        );
        assert!(
            same_database("postgres://shop@db", "postgres://db/shop"),
            "without a database in the URL, Postgres uses the user's name"
        );
        assert!(!same_database(
            "postgres://db/shop",
            "postgres://db/shop_dev"
        ));
        assert!(
            !same_database("postgres://db:3306/shop", "mysql://db:3306/shop"),
            "a MySQL database and a Postgres one are never the same"
        );
    }

    #[tokio::test]
    async fn refuses_urls_it_cannot_read_or_write() -> anyhow::Result<()> {
        let plan = Plan::try_new(
            "samplers:\n  - table: users\n    limit: all\n".parse()?,
            None,
        )?;
        let refusal = |source: &'static str, output: Output| {
            let plan = plan.clone();
            async move {
                let error = run(plan, source, 1, &output, Options::default())
                    .await
                    .expect_err("tapas refuses before it connects");
                error.to_string()
            }
        };
        let file = || Output::File(PathBuf::from("never-written.sql"));
        let database = |url: &str| Output::Database(url.to_string());

        assert_eq!(
            refusal("sqlite://shop.db", file()).await,
            "the source URL must start with mysql:// or postgres://"
        );
        assert_eq!(
            refusal("mysql://db/shop", database("sqlite://shop.db")).await,
            "the target URL must start with mysql:// or postgres://"
        );
        assert_eq!(
            refusal("mysql://db/shop", database("postgres://db/shop")).await,
            "the source is MySQL and the target is Postgres; tapas only writes into a target \
             of the same kind"
        );
        assert_eq!(
            refusal("postgres://db/shop", database("mysql://db/shop")).await,
            "the source is Postgres and the target is MySQL; tapas only writes into a target \
             of the same kind"
        );
        assert!(
            !Path::new("never-written.sql.partial").exists(),
            "nothing is written"
        );
        Ok(())
    }
}
