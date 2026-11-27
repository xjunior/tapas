pub mod config;
pub mod extract;
pub mod output;
pub mod plan;
pub mod since;
pub mod sql;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use mysql_async::{Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts};

pub use config::Config;
pub use extract::Options;
pub use plan::Plan;
pub use since::Since;

/// Extracts the sample `plan` describes from `source_url` into a SQL file at `output`.
/// Returns the number of rows written per table.
pub async fn run(
    plan: Plan,
    source_url: &str,
    connections: usize,
    output: &Path,
    options: Options,
) -> Result<BTreeMap<String, u64>> {
    plan.check_since()?;

    let opts = Opts::from_url(source_url).context("parsing the source URL")?;
    let constraints = PoolConstraints::new(1, connections.max(1)).context("pool size")?;
    let opts = OptsBuilder::from_opts(opts)
        .pool_opts(PoolOpts::default().with_constraints(constraints))
        // tapas only reads; make sure of it.
        .setup(vec!["SET SESSION TRANSACTION READ ONLY"]);
    let pool = Pool::new(opts);

    let (file, sender) = output::SqlFile::create(output)?;
    let extracted = extract::extract(plan, pool.clone(), options, sender).await;
    let written = file.finish(extracted.is_ok());
    pool.disconnect().await.ok();

    let rows = extracted?;
    written?;
    Ok(rows)
}
