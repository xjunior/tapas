//! The `tapas` command line.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context;
use anyhow::{Result, bail};
use clap::Parser;
use tapas::config::DepsSource;
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

    /// Where references between tables come from: `declared` takes only the config's `deps` and
    /// `of`; `foreign-keys` also follows the source's foreign keys, which needs --source even with
    /// --plan. Overrides the config's `deps`.
    #[arg(long)]
    deps: Option<DepsSource>,

    /// Show the values in errors, such as every row of an INSERT the target refused. Those rows
    /// are as tapas writes them, so sanitized columns hold their replacements.
    #[arg(long)]
    debug: bool,
}

/// Returns where references come from: the command line's choice, else the config's, else the
/// declared `deps` and `of` alone.
#[must_use]
fn deps_source(command_line: Option<DepsSource>, config: Option<DepsSource>) -> DepsSource {
    command_line.or(config).unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let started = Instant::now();

    let config = Config::load(&args.configs)?;
    let foreign_keys = match deps_source(args.deps, config.deps) {
        DepsSource::Declared => Vec::new(),
        DepsSource::ForeignKeys => {
            let Some(source) = args.source.as_deref() else {
                bail!("--deps foreign-keys reads the foreign keys from --source; pass it");
            };
            tapas::foreign_keys(source).await?
        }
    };
    let plan = Plan::try_new_with_foreign_keys(config, args.since, &foreign_keys)?;
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
    fn deps_defaults_to_what_the_config_says() -> Result<()> {
        let args = Args::try_parse_from(["tapas", "tapas.yml", "--plan"])?;
        assert_eq!(args.deps, None, "the config decides unless --deps is given");
        let args =
            Args::try_parse_from(["tapas", "tapas.yml", "--plan", "--deps", "foreign-keys"])?;
        assert_eq!(args.deps, Some(DepsSource::ForeignKeys));
        let error = Args::try_parse_from(["tapas", "tapas.yml", "--plan", "--deps", "keys"])
            .expect_err("`keys` isn't a source of references");
        assert!(
            error
                .to_string()
                .contains("use `declared` or `foreign-keys`"),
            "{error}"
        );
        Ok(())
    }

    #[test]
    fn the_command_line_wins_over_the_config() {
        use DepsSource::{Declared, ForeignKeys};
        assert_eq!(deps_source(None, None), Declared);
        assert_eq!(deps_source(None, Some(ForeignKeys)), ForeignKeys);
        assert_eq!(deps_source(Some(Declared), Some(ForeignKeys)), Declared);
        assert_eq!(deps_source(Some(ForeignKeys), Some(Declared)), ForeignKeys);
    }

    #[test]
    fn debug_is_off_unless_asked_for() -> Result<()> {
        let args = Args::try_parse_from(["tapas", "tapas.yml", "--plan"])?;
        assert!(!args.debug, "errors keep values out by default");
        let args = Args::try_parse_from(["tapas", "tapas.yml", "--plan", "--debug"])?;
        assert!(args.debug, "--debug turns it on");
        Ok(())
    }
}
