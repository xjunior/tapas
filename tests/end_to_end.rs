//! Runs tapas against a real MySQL server. Set TAPAS_TEST_DATABASE_URL to a URL whose user can
//! create databases (no database in the path), e.g. mysql://root:tapas@127.0.0.1:3399
//! Without it, the test is skipped.

use std::{env, fs, process};

use anyhow::{Context, Result};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use tapas::{Config, Options, Output, Plan, Since};

const SCHEMA: &str = r#"
CREATE TABLE countries (id INT PRIMARY KEY, code CHAR(2) NOT NULL);
CREATE TABLE users (
  id BIGINT PRIMARY KEY,
  country_id INT NULL,
  admin TINYINT(1) NOT NULL,
  name VARCHAR(100),
  bio TEXT,
  token VARBINARY(16),
  settings JSON,
  balance DECIMAL(10, 2),
  created_at DATETIME(6)
);
CREATE TABLE phones (id BIGINT PRIMARY KEY, user_id BIGINT NOT NULL, number VARCHAR(20));
CREATE TABLE notes (id BIGINT PRIMARY KEY, user_id BIGINT NOT NULL, body TEXT);
CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT NOT NULL, total DOUBLE);
CREATE TABLE line_items (id BIGINT PRIMARY KEY, order_id BIGINT NOT NULL, sku VARCHAR(20));
CREATE TABLE events (id BIGINT PRIMARY KEY, created_at DATETIME NOT NULL, updated_at DATETIME NOT NULL);
"#;

const SEED: &str = r#"
SET SESSION cte_max_recursion_depth = 10000;
INSERT INTO countries
  WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5)
  SELECT i, CONCAT('C', i) FROM n;
INSERT INTO users
  WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300)
  SELECT i, IF(i % 7 = 0, NULL, i % 5 + 1), i % 50 = 0,
         CONCAT('it''s \\ user ', i, '\n\0'), REPEAT('ção ', i % 3),
         UNHEX(MD5(i)), JSON_OBJECT('i', i, 'q', 'a"b'), i * 1.25,
         TIMESTAMP('2024-01-01 00:00:00.123456') + INTERVAL i MINUTE
  FROM n;
INSERT INTO phones SELECT id * 10 + k, id, CONCAT('555-', id, k) FROM users, (SELECT 1 k UNION SELECT 2) ks;
INSERT INTO notes SELECT id * 10 + k, id, 'note' FROM users, (SELECT 1 k UNION SELECT 2) ks;
INSERT INTO orders SELECT id * 10 + k, id, k * 9.5 FROM users, (SELECT 1 k UNION SELECT 2 UNION SELECT 3) ks;
INSERT INTO line_items SELECT id * 10 + k, id, CONCAT('SKU', k) FROM orders, (SELECT 1 k UNION SELECT 2) ks;
-- One event a day for 60 days; every 10th was also updated today.
INSERT INTO events
  WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 60)
  SELECT i, NOW() - INTERVAL i DAY, IF(i % 10 = 0, NOW(), NOW() - INTERVAL i DAY) FROM n;
"#;

const SAMPLERS: &str = r#"
samplers:
  - table: countries

  - table: users
    order_by: id DESC
    limit: 100
    of:
      country_id: countries.id

  - table: users
    where: admin = 1

  - table: phones
    of:
      user_id: users.id

  - table: notes
    order_by: id
    limit: 15
    of:
      user_id: users.id

  - table: orders
    of:
      user_id: users.id

  - table: line_items
    of:
      order_id: orders.id

  - table: events
    time_column: [created_at, updated_at]
"#;

async fn scalar(connection: &mut Conn, query: &str) -> Result<u64> {
    let value: Option<u64> = connection.query_first(query).await?;
    value.with_context(|| format!("`{query}` returned no rows"))
}

#[tokio::test(flavor = "multi_thread")]
async fn extracts_a_sample_that_keeps_references() -> Result<()> {
    let Ok(base) = env::var("TAPAS_TEST_DATABASE_URL") else {
        eprintln!("TAPAS_TEST_DATABASE_URL not set, skipping");
        return Ok(());
    };

    let mut connection = Conn::new(Opts::from_url(&base)?).await?;
    for database in ["tapas_source", "tapas_target"] {
        connection
            .query_drop(format!(
                "DROP DATABASE IF EXISTS {database}; CREATE DATABASE {database}"
            ))
            .await?;
    }
    connection
        .query_drop(format!("USE tapas_source; {SCHEMA} {SEED}"))
        .await?;
    connection
        .query_drop(format!("USE tapas_target; {SCHEMA}"))
        .await?;

    let output = env::temp_dir().join(format!("tapas-test-{}.sql", process::id()));
    let config: Config = SAMPLERS.parse()?;
    let since: Since = "2 weeks".parse().map_err(anyhow::Error::msg)?;
    let plan = Plan::try_new(config, Some(since))?;
    let rows = tapas::run(
        plan,
        &format!("{base}/tapas_source"),
        4,
        &Output::File(output.clone()),
        // Small chunks and batches so chunking and batching are exercised.
        Options {
            chunk_size: 7,
            batch_rows: 13,
            ..Options::default()
        },
    )
    .await?;

    let sql = fs::read_to_string(&output)?;
    fs::remove_file(&output)?;
    connection
        .query_drop(format!("USE tapas_target; {sql}"))
        .await?;

    // All countries.
    assert_eq!(
        scalar(&mut connection, "SELECT COUNT(*) FROM countries").await?,
        5
    );

    // The 100 newest users with a country (multiples of 7 have none), plus every admin
    // (multiples of 50), with users in both samples counted once.
    let mut expected: Vec<u64> = (1..=300).rev().filter(|id| id % 7 != 0).take(100).collect();
    expected.extend((1..=300).filter(|id| id % 50 == 0));
    expected.sort();
    expected.dedup();
    let expected_users = expected.len() as u64;
    assert!(
        expected_users > 100 && expected_users < 106,
        "the two samples should overlap"
    );
    assert_eq!(
        scalar(&mut connection, "SELECT COUNT(*) FROM users").await?,
        expected_users
    );
    assert_eq!(rows["users"], expected_users);
    assert_eq!(
        scalar(
            &mut connection,
            "SELECT COUNT(*) FROM users WHERE admin = 1"
        )
        .await?,
        6
    );

    // Every child row of an extracted parent came along, and nothing else.
    assert_eq!(
        scalar(&mut connection, "SELECT COUNT(*) FROM phones").await?,
        expected_users * 2
    );
    assert_eq!(
        scalar(&mut connection, "SELECT COUNT(*) FROM orders").await?,
        expected_users * 3
    );
    assert_eq!(
        scalar(&mut connection, "SELECT COUNT(*) FROM line_items").await?,
        expected_users * 6
    );

    // The limit holds across chunks.
    assert_eq!(
        scalar(&mut connection, "SELECT COUNT(*) FROM notes").await?,
        15
    );

    // --since 2 weeks: the 14 events created in that window, plus the 5 older ones updated today.
    assert_eq!(
        scalar(&mut connection, "SELECT COUNT(*) FROM events").await?,
        19
    );
    assert_eq!(rows["events"], 19);

    // No dangling references.
    for (child, column, parent) in [
        ("users", "country_id", "countries"),
        ("phones", "user_id", "users"),
        ("notes", "user_id", "users"),
        ("orders", "user_id", "users"),
        ("line_items", "order_id", "orders"),
    ] {
        let dangling = scalar(
            &mut connection,
            &format!(
                "SELECT COUNT(*) FROM {child} c LEFT JOIN {parent} p ON p.id = c.{column} \
                 WHERE c.{column} IS NOT NULL AND p.id IS NULL"
            ),
        )
        .await?;
        assert_eq!(dangling, 0, "{child}.{column} has rows without a {parent}");
    }

    // Every value made it across unchanged.
    let changed = scalar(
        &mut connection,
        "SELECT COUNT(*) FROM tapas_target.users t JOIN tapas_source.users s ON s.id = t.id \
         WHERE NOT (t.country_id <=> s.country_id AND t.admin <=> s.admin AND t.name <=> s.name \
           AND t.bio <=> s.bio AND t.token <=> s.token AND t.settings <=> s.settings \
           AND t.balance <=> s.balance AND t.created_at <=> s.created_at)",
    )
    .await?;
    assert_eq!(changed, 0, "values changed on the way across");

    connection
        .query_drop("DROP DATABASE tapas_source; DROP DATABASE tapas_target")
        .await?;
    connection.disconnect().await?;
    Ok(())
}
