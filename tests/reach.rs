//! How far a sampler reaches: `limit` with `rows` and `since`, and tables that only deps pull in.
//! Needs TAPAS_TEST_DATABASE_URL, like end_to_end.rs.

use std::{env, fs, process};

use anyhow::Result;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use tapas::{Config, Options, Output, Plan, Since};

const SCHEMA: &str = r#"
CREATE TABLE customers (id BIGINT PRIMARY KEY);
CREATE TABLE invoices (id BIGINT PRIMARY KEY, customer_id BIGINT NOT NULL);
CREATE TABLE orders (id BIGINT PRIMARY KEY, invoice_id BIGINT NOT NULL);
CREATE TABLE events (id BIGINT PRIMARY KEY, created_at DATETIME NOT NULL);
CREATE TABLE audit_log (id BIGINT PRIMARY KEY);
"#;

const SEED: &str = r#"
INSERT INTO customers VALUES (1), (2), (3);
INSERT INTO invoices VALUES (10, 1), (11, 2), (12, 3);
INSERT INTO orders VALUES (100, 10), (101, 11);
-- Events 1 and 2 are a month old, 3 to 5 are recent.
INSERT INTO events VALUES
  (1, NOW() - INTERVAL 1 MONTH), (2, NOW() - INTERVAL 1 MONTH),
  (3, NOW() - INTERVAL 1 DAY), (4, NOW() - INTERVAL 2 DAY), (5, NOW() - INTERVAL 3 DAY);
INSERT INTO audit_log VALUES (1), (2);
"#;

const SAMPLERS: &str = r#"
samplers:
  # The two newest events inside --since.
  - table: events
    order_by: id DESC
    limit:
      rows: 2
      since: created_at

  # Declared only for its deps: no invoice comes in on its own.
  - table: invoices
    deps:
      customer_id: customers.id

  # The first order, whose invoice and customer come along.
  - table: orders
    order_by: id
    limit: 1
    deps:
      invoice_id: invoices.id
"#;

async fn ids(connection: &mut Conn, table: &str) -> Result<Vec<u64>> {
    let ids: Vec<u64> = connection
        .query(format!("SELECT id FROM {table} ORDER BY id"))
        .await?;
    Ok(ids)
}

#[tokio::test(flavor = "multi_thread")]
async fn takes_only_the_rows_a_limit_or_a_dep_asks_for() -> Result<()> {
    let Ok(base) = env::var("TAPAS_TEST_DATABASE_URL") else {
        eprintln!("TAPAS_TEST_DATABASE_URL not set, skipping");
        return Ok(());
    };

    let mut connection = Conn::new(Opts::from_url(&base)?).await?;
    for database in ["tapas_reach_source", "tapas_reach_target"] {
        connection
            .query_drop(format!(
                "DROP DATABASE IF EXISTS {database}; CREATE DATABASE {database}"
            ))
            .await?;
    }
    connection
        .query_drop(format!("USE tapas_reach_source; {SCHEMA} {SEED}"))
        .await?;
    connection
        .query_drop(format!("USE tapas_reach_target; {SCHEMA}"))
        .await?;

    let config: Config = SAMPLERS.parse()?;
    let since: Since = "2 weeks".parse().map_err(anyhow::Error::msg)?;
    let plan = Plan::try_new(config, Some(since))?;
    let output = env::temp_dir().join(format!("tapas-reach-{}.sql", process::id()));
    tapas::run(
        plan,
        &format!("{base}/tapas_reach_source"),
        4,
        &Output::File(output.clone()),
        Options::default(),
    )
    .await?;

    let sql = fs::read_to_string(&output)?;
    fs::remove_file(&output)?;
    connection
        .query_drop(format!("USE tapas_reach_target; {sql}"))
        .await?;

    // The newest two of the three recent events.
    assert_eq!(ids(&mut connection, "events").await?, [4, 5]);
    // Only the first order, its invoice and its customer.
    assert_eq!(ids(&mut connection, "orders").await?, [100]);
    assert_eq!(ids(&mut connection, "invoices").await?, [10]);
    assert_eq!(ids(&mut connection, "customers").await?, [1]);
    // A table no sampler or dep names has no rows.
    assert_eq!(ids(&mut connection, "audit_log").await?, Vec::<u64>::new());

    connection
        .query_drop("DROP DATABASE tapas_reach_source; DROP DATABASE tapas_reach_target")
        .await?;
    connection.disconnect().await?;
    Ok(())
}
