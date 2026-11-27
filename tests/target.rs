//! Writes a sample straight into a target database. Needs TAPAS_TEST_DATABASE_URL, like
//! tests/end_to_end.rs; skipped without it.

use std::env;

use anyhow::{Context, Result};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use tapas::{Config, Options, Output, Plan};

const SCHEMA: &str = r#"
CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR(100));
CREATE TABLE orders (
  id BIGINT PRIMARY KEY,
  user_id BIGINT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users (id)
);
"#;

const SEED: &str = r#"
INSERT INTO users VALUES (1, 'ana'), (2, 'bo'), (3, 'cy');
INSERT INTO orders VALUES (10, 1), (20, 2), (30, 3), (31, 3);
"#;

// Orders come first and bring their users, so the target sees orders before users.
const SAMPLERS: &str = r#"
samplers:
  - table: orders
    where: user_id >= 2
    deps:
      user_id: users.id
"#;

#[tokio::test(flavor = "multi_thread")]
async fn writes_straight_into_a_target_database() -> Result<()> {
    let Ok(base) = env::var("TAPAS_TEST_DATABASE_URL") else {
        eprintln!("TAPAS_TEST_DATABASE_URL not set, skipping");
        return Ok(());
    };

    let mut connection = Conn::new(Opts::from_url(&base)?).await?;
    for database in ["tapas_target_source", "tapas_target_target"] {
        connection.query_drop(format!(
            "DROP DATABASE IF EXISTS {database}; CREATE DATABASE {database}; USE {database}; {SCHEMA}"
        ))
        .await?;
    }
    connection
        .query_drop(format!("USE tapas_target_source; {SEED}"))
        .await?;

    let config: Config = SAMPLERS.parse()?;
    let plan = Plan::try_new(config, None)?;
    let source = format!("{base}/tapas_target_source");

    // Writing into the source is refused.
    let error = tapas::run(
        plan.clone(),
        &source,
        2,
        &Output::Database(source.clone()),
        Options::default(),
    )
    .await
    .expect_err("tapas refuses to write into its source");
    assert!(error.to_string().contains("source"), "{error}");

    let rows = tapas::run(
        plan,
        &source,
        2,
        &Output::Database(format!("{base}/tapas_target_target")),
        Options::default(),
    )
    .await?;
    assert_eq!(rows["orders"], 3);
    assert_eq!(rows["users"], 2);

    let count = |table: &str| format!("SELECT COUNT(*) FROM tapas_target_target.{table}");
    for (table, expected) in [("orders", 3u64), ("users", 2)] {
        let found: Option<u64> = connection.query_first(count(table)).await?;
        let found = found.context("COUNT(*) returns a row")?;
        assert_eq!(found, expected, "{table}");
    }

    connection
        .query_drop("DROP DATABASE tapas_target_source; DROP DATABASE tapas_target_target")
        .await?;
    connection.disconnect().await?;
    Ok(())
}
