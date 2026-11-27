//! The example from the design discussion: deps pull in the rows they point at. Needs
//! TAPAS_TEST_DATABASE_URL, like end_to_end.rs.

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};

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
    deps:
      territory_id: territories.id
    where: age > 25

  - table: orders
    deps:
      user_id: users.id
    time_column: created_at

  - table: territories
"#;

async fn ids(conn: &mut Conn, table: &str) -> Vec<u64> {
    conn.query(format!("SELECT id FROM {table} ORDER BY id"))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn deps_pull_in_what_the_rows_point_at() {
    let Ok(base) = std::env::var("TAPAS_TEST_DATABASE_URL") else {
        eprintln!("TAPAS_TEST_DATABASE_URL not set, skipping");
        return;
    };

    let mut conn = Conn::new(Opts::from_url(&base).unwrap()).await.unwrap();
    for database in ["tapas_refs_source", "tapas_refs_target"] {
        conn.query_drop(format!(
            "DROP DATABASE IF EXISTS {database}; CREATE DATABASE {database}"
        ))
        .await
        .unwrap();
    }
    conn.query_drop(format!("USE tapas_refs_source; {SCHEMA} {SEED}"))
        .await
        .unwrap();
    conn.query_drop(format!("USE tapas_refs_target; {SCHEMA}"))
        .await
        .unwrap();

    let plan = tapas::Plan::build(
        tapas::Config::parse(SAMPLERS).unwrap(),
        Some("2 weeks".parse().unwrap()),
    )
    .unwrap();
    let output = std::env::temp_dir().join(format!("tapas-refs-{}.sql", std::process::id()));
    tapas::run(
        plan,
        &format!("{base}/tapas_refs_source"),
        4,
        &output,
        tapas::Options::default(),
    )
    .await
    .unwrap();

    let sql = std::fs::read_to_string(&output).unwrap();
    std::fs::remove_file(&output).unwrap();
    conn.query_drop(format!("USE tapas_refs_target; {sql}"))
        .await
        .unwrap();

    // Orders from the last two weeks, whoever placed them.
    assert_eq!(ids(&mut conn, "orders").await, [1, 2]);
    // Users over 25, plus user 2, who is 20 but placed a recent order.
    let mut expected: Vec<u64> = (11..=20).collect();
    expected.insert(0, 2);
    assert_eq!(ids(&mut conn, "users").await, expected);
    // Every territory.
    assert_eq!(ids(&mut conn, "territories").await, [1, 2, 3]);

    conn.query_drop("DROP DATABASE tapas_refs_source; DROP DATABASE tapas_refs_target")
        .await
        .unwrap();
    conn.disconnect().await.unwrap();
}
