//! Runs a whole extraction: connects to the source, extracts, and writes the output.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use mysql_async::{Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts};

use crate::extract::{self, Options};
use crate::output::{Outcome, Output, Sink};
use crate::plan::Plan;

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
    use anyhow::Context;

    plan.check_since()?;
    if let Output::Database(target_url) = output
        && same_database(source_url, target_url)
    {
        bail!("the target is the source database; tapas won't write into what it reads");
    }

    let connection_options = Opts::from_url(source_url).context("parsing the source URL")?;
    let constraints = PoolConstraints::new(1, connections.max(1)).context("pool size")?;
    let connection_options = OptsBuilder::from_opts(connection_options)
        .pool_opts(PoolOpts::default().with_constraints(constraints))
        // tapas only reads; make sure of it.
        .setup(vec!["SET SESSION TRANSACTION READ ONLY"]);
    let pool = Pool::new(connection_options);

    let (sink, sender) = Sink::open(output).await?;
    let extracted = extract::extract(plan, pool.clone(), options, sender).await;
    let outcome = if extracted.is_ok() {
        Outcome::Keep
    } else {
        Outcome::Discard
    };
    let written = sink.finish(outcome).await;
    pool.disconnect().await.ok();

    let rows = extracted?;
    written?;
    Ok(rows)
}

/// Returns whether two URLs point at the same database on the same server.
#[must_use]
fn same_database(first: &str, second: &str) -> bool {
    let (Ok(first), Ok(second)) = (Opts::from_url(first), Opts::from_url(second)) else {
        return false;
    };
    first.ip_or_hostname() == second.ip_or_hostname()
        && first.tcp_port() == second.tcp_port()
        && first.db_name() == second.db_name()
}

#[cfg(test)]
mod tests {
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
}
