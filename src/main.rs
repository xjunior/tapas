//! The `tapas` command line.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use tapas::sanitize::Seed;
use tapas::sql::Dialect;
use tapas::{Config, Options, Output, Plan, Since};

/// Extracts a sample of a MySQL or Postgres database that keeps its references intact.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    /// YAML files listing the samplers. Glob patterns work too: quote them, as in
    /// 'apps/**/tapas.yml', and tapas expands them itself.
    #[arg(required = true)]
    configs: Vec<String>,

    /// How far back samplers whose limit has a `since` reach, such as '2 weeks' or '3 months'.
    /// Required when any limit has a `since`; `ever` keeps every row.
    #[arg(long)]
    since: Option<Since>,

    /// Print the tables and queries tapas would run, without connecting. The queries are written
    /// for the --source database, or for MySQL without one.
    #[arg(long)]
    plan: bool,

    /// Source database, as mysql://user:password@host:port/database or
    /// postgres://user:password@host:port/database.
    #[arg(
        long,
        env = "TAPAS_SOURCE_URL",
        hide_env_values = true,
        required_unless_present = "plan"
    )]
    source: Option<String>,

    /// SQL file to write the sample to. A path ending in .gz is gzipped as it's written.
    #[arg(long, short, required_unless_present_any = ["plan", "target"], conflicts_with = "target")]
    output: Option<PathBuf>,

    /// Database to write the sample straight into, of the same kind as the source, as
    /// mysql://user:password@host:port/database or postgres://user:password@host:port/database.
    /// Its tables must exist. Everything goes in one transaction, committed only if the whole
    /// sample worked.
    #[arg(long, env = "TAPAS_TARGET_URL", hide_env_values = true)]
    target: Option<String>,

    /// Most connections open to the source at once.
    #[arg(long, default_value_t = 8)]
    connections: usize,

    /// Values per IN (...) list.
    #[arg(long, default_value_t = 5_000)]
    chunk_size: usize,

    /// Rows per INSERT statement.
    #[arg(long, default_value_t = 500)]
    batch_rows: usize,

    /// Any text that fake values start from. With the same seed, the same original value gets
    /// the same fake one in every run; keep it secret. Without it, each run picks its own.
    #[arg(long, env = "TAPAS_SANITIZE_SEED", hide_env_values = true)]
    sanitize_seed: Option<Seed>,

    /// Show the values in errors, such as every row of an INSERT the target refused. Those rows
    /// are as tapas writes them, so sanitized columns hold their replacements.
    #[arg(long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let started = Instant::now();

    let plan = Plan::try_new(Config::load(&args.configs)?, args.since)?;
    if args.plan {
        let dialect = match args.source.as_deref() {
            Some(url) => Dialect::from_url(url)
                .context("--source must start with mysql:// or postgres://")?,
            None => Dialect::MySql,
        };
        print!("{}", plan.describe(dialect));
        return Ok(());
    }

    let Some(source) = args.source else {
        unreachable!("clap requires --source without --plan");
    };
    let output = match (args.output, args.target) {
        (Some(path), _) => Output::File(path),
        (None, Some(url)) => Output::Database(url),
        (None, None) => unreachable!("clap requires --output or --target without --plan"),
    };
    let options = Options {
        chunk_size: args.chunk_size.max(1),
        batch_rows: args.batch_rows.max(1),
        sanitize_seed: args.sanitize_seed,
        debug: args.debug,
    };
    let rows = tapas::run(plan, &source, args.connections, &output, options).await?;

    let total: u64 = rows.values().sum();
    eprintln!();
    for (table, count) in &rows {
        eprintln!("{count:>12}  {table}");
    }
    eprintln!(
        "{total:>12}  rows in {:.2}s -> {output}",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_off_unless_asked_for() -> Result<()> {
        let args = Args::try_parse_from(["tapas", "tapas.yml", "--plan"])?;
        assert!(!args.debug, "errors keep values out by default");
        let args = Args::try_parse_from(["tapas", "tapas.yml", "--plan", "--debug"])?;
        assert!(args.debug, "--debug turns it on");
        Ok(())
    }
}
