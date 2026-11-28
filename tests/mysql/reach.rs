//! How far a sampler reaches: `limit` with `rows` and `since`, and tables that only deps pull in.

use anyhow::Result;
use tapas::Options;

use super::support::{ids, sample, url};

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

#[tokio::test(flavor = "multi_thread")]
async fn takes_only_the_rows_a_limit_or_a_dep_asks_for() -> Result<()> {
    let Some(base) = url() else {
        return Ok(());
    };
    let mut sample = sample(
        &base,
        "reach",
        (SCHEMA, SEED),
        SAMPLERS,
        Some("2 weeks"),
        Options::default(),
    )
    .await?;
    let connection = &mut sample.connection;

    // The newest two of the three recent events.
    assert_eq!(ids(connection, "events").await?, [4, 5]);
    // Only the first order, its invoice and its customer.
    assert_eq!(ids(connection, "orders").await?, [100]);
    assert_eq!(ids(connection, "invoices").await?, [10]);
    assert_eq!(ids(connection, "customers").await?, [1]);
    // A table no sampler or dep names has no rows.
    assert_eq!(ids(connection, "audit_log").await?, Vec::<u64>::new());

    sample.finish().await
}
