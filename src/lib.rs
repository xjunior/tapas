pub mod config;
pub mod extract;
pub mod output;
pub mod plan;
pub mod since;
pub mod sql;

use anyhow::{Context, Result, bail};
use mysql_async::{Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts};
use std::collections::BTreeMap;

pub use config::Config;
pub use extract::Options;
pub use output::Output;
pub use plan::Plan;
pub use since::Since;

/// Extracts the sample `plan` describes from `source_url` into `output`, a SQL file or a target
/// database. Returns the number of rows written per table.
pub async fn run(
    plan: Plan,
    source_url: &str,
    connections: usize,
    output: &Output,
    options: Options,
) -> Result<BTreeMap<String, u64>> {
    plan.check_since()?;
    if let Output::Database(target_url) = output
        && same_database(source_url, target_url)
    {
        bail!("the target is the source database; tapas won't write into what it reads");
    }

    let opts = Opts::from_url(source_url).context("parsing the source URL")?;
    let constraints = PoolConstraints::new(1, connections.max(1)).context("pool size")?;
    let opts = OptsBuilder::from_opts(opts)
        .pool_opts(PoolOpts::default().with_constraints(constraints))
        // tapas only reads; make sure of it.
        .setup(vec!["SET SESSION TRANSACTION READ ONLY"]);
    let pool = Pool::new(opts);

    let (sink, sender) = output::Sink::open(output).await?;
    let extracted = extract::extract(plan, pool.clone(), options, sender).await;
    let written = sink.finish(extracted.is_ok()).await;
    pool.disconnect().await.ok();

    let rows = extracted?;
    written?;
    Ok(rows)
}

/// Whether two URLs point at the same database on the same server.
fn same_database(a: &str, b: &str) -> bool {
    match (Opts::from_url(a), Opts::from_url(b)) {
        (Ok(a), Ok(b)) => {
            a.ip_or_hostname() == b.ip_or_hostname()
                && a.tcp_port() == b.tcp_port()
                && a.db_name() == b.db_name()
        }
        _ => false,
    }
}
