//! Writes a sample straight into a target database. Needs TAPAS_TEST_DATABASE_URL, like
//! tests/end_to_end.rs; skipped without it.

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};

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
async fn writes_straight_into_a_target_database() {
    let Ok(base) = std::env::var("TAPAS_TEST_DATABASE_URL") else {
        eprintln!("TAPAS_TEST_DATABASE_URL not set, skipping");
        return;
    };

    let mut conn = Conn::new(Opts::from_url(&base).unwrap()).await.unwrap();
    for database in ["tapas_target_source", "tapas_target_target"] {
        conn.query_drop(format!(
            "DROP DATABASE IF EXISTS {database}; CREATE DATABASE {database}; USE {database}; {SCHEMA}"
        ))
        .await
        .unwrap();
    }
    conn.query_drop(format!("USE tapas_target_source; {SEED}"))
        .await
        .unwrap();

    let plan = || tapas::Plan::build(tapas::Config::parse(SAMPLERS).unwrap(), None).unwrap();
    let source = format!("{base}/tapas_target_source");

    // Writing into the source is refused.
    let error = tapas::run(
        plan(),
        &source,
        2,
        &tapas::Output::Database(source.clone()),
        tapas::Options::default(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("source"), "{error}");

    let rows = tapas::run(
        plan(),
        &source,
        2,
        &tapas::Output::Database(format!("{base}/tapas_target_target")),
        tapas::Options::default(),
    )
    .await
    .unwrap();
    assert_eq!(rows["orders"], 3);
    assert_eq!(rows["users"], 2);

    let count = |table: &str| format!("SELECT COUNT(*) FROM tapas_target_target.{table}");
    for (table, expected) in [("orders", 3u64), ("users", 2)] {
        let found: u64 = conn.query_first(count(table)).await.unwrap().unwrap();
        assert_eq!(found, expected, "{table}");
    }

    conn.query_drop("DROP DATABASE tapas_target_source; DROP DATABASE tapas_target_target")
        .await
        .unwrap();
    conn.disconnect().await.unwrap();
}
