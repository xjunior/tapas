//! The example from the design discussion: deps pull in the rows they point at.

use anyhow::Result;
use tapas::Options;

use super::support::{ids, sample, url};

const SCHEMA: &str = r#"
CREATE TABLE territories (id BIGINT PRIMARY KEY);
CREATE TABLE users (id BIGINT PRIMARY KEY, territory_id BIGINT NOT NULL, age INT NOT NULL);
CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT NOT NULL, created_at DATETIME NOT NULL);
"#;

const SEED: &str = r#"
INSERT INTO territories VALUES (1), (2), (3);
-- Users 1-10 are 20 years old, users 11-20 are 30.
INSERT INTO users
  WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20)
  SELECT i, i % 3 + 1, IF(i <= 10, 20, 30) FROM n;
-- Recent orders from users 2 and 12; old orders from users 3 and 13.
INSERT INTO orders VALUES
  (1, 2, NOW() - INTERVAL 1 DAY), (2, 12, NOW() - INTERVAL 3 DAY),
  (3, 3, NOW() - INTERVAL 1 MONTH), (4, 13, NOW() - INTERVAL 1 MONTH);
"#;

const SAMPLERS: &str = r#"
samplers:
  - table: users
    limit: all
    deps:
      territory_id: territories.id
    where: age > 25

  - table: orders
    deps:
      user_id: users.id
    limit: { since: created_at }

  - table: territories
    limit: all
"#;

#[tokio::test(flavor = "multi_thread")]
async fn deps_pull_in_what_the_rows_point_at() -> Result<()> {
    let Some(base) = url() else {
        return Ok(());
    };
    let mut sample = sample(
        &base,
        "refs",
        (SCHEMA, SEED),
        SAMPLERS,
        Some("2 weeks"),
        Options::default(),
    )
    .await?;
    let connection = &mut sample.connection;

    // Orders from the last two weeks, whoever placed them.
    assert_eq!(ids(connection, "orders").await?, [1, 2]);
    // Users over 25, plus user 2, who is 20 but placed a recent order.
    let mut expected: Vec<u64> = (11..=20).collect();
    expected.insert(0, 2);
    assert_eq!(ids(connection, "users").await?, expected);
    // Every territory.
    assert_eq!(ids(connection, "territories").await?, [1, 2, 3]);

    sample.finish().await
}
