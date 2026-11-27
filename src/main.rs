use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use tapas::{Config, Options, Plan, Since};

/// Extracts a sample of a MySQL database that keeps its references intact.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// YAML files listing the samplers. Glob patterns work too: quote them, as in
    /// 'apps/**/tapas.yml', and tapas expands them itself.
    #[arg(required = true)]
    configs: Vec<String>,

    /// How far back samplers with a time_column reach, such as '2 weeks' or '3 months'.
    /// Required when any sampler has a time_column; `ever` keeps every row.
    #[arg(long)]
    since: Option<Since>,

    /// Print the tables and queries tapas would run, without connecting.
    #[arg(long)]
    plan: bool,

    /// Source database, as mysql://user:password@host:port/database.
    #[arg(
        long,
        env = "TAPAS_SOURCE_URL",
        hide_env_values = true,
        required_unless_present = "plan"
    )]
    source: Option<String>,

    /// SQL file to write the sample to.
    #[arg(long, short, required_unless_present = "plan")]
    output: Option<PathBuf>,

    /// Most connections open to the source at once.
    #[arg(long, default_value_t = 8)]
    connections: usize,

    /// Values per IN (...) list.
    #[arg(long, default_value_t = 5_000)]
    chunk_size: usize,

    /// Rows per INSERT statement.
    #[arg(long, default_value_t = 500)]
    batch_rows: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let started = Instant::now();

    let plan = Plan::build(Config::load(&args.configs)?, args.since)?;
    if args.plan {
        print!("{}", plan.describe());
        return Ok(());
    }

    let (Some(source), Some(output)) = (args.source, args.output) else {
        unreachable!("clap requires --source and --output without --plan");
    };
    let options = Options {
        chunk_size: args.chunk_size.max(1),
        batch_rows: args.batch_rows.max(1),
    };
    let rows = tapas::run(plan, &source, args.connections, &output, options).await?;

    let total: u64 = rows.values().sum();
    eprintln!();
    for (table, count) in &rows {
        eprintln!("{count:>12}  {table}");
    }
    eprintln!(
        "{total:>12}  rows in {:.2}s -> {}",
        started.elapsed().as_secs_f64(),
        output.display()
    );
    Ok(())
}
