//! Runs tapas against a real Postgres server. Set TAPAS_TEST_POSTGRES_URL to a URL whose user can
//! create databases (no database in the path), e.g. postgres://postgres:tapas@127.0.0.1:5499
//! Without it, the tests are skipped.

use std::collections::BTreeMap;
use std::{env, fs, process};

use anyhow::{Context, Result};
use tapas::{Config, Options, Output, Plan, Since};
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

const SCHEMA: &str = r#"
CREATE TYPE mood AS ENUM ('happy', 'sad');
CREATE TABLE countries (id integer PRIMARY KEY, code char(2) NOT NULL);
CREATE TABLE users (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  country_id integer REFERENCES countries (id),
  manager_id bigint REFERENCES users (id),
  "displayName" varchar(60),
  admin boolean NOT NULL,
  token bytea,
  settings jsonb,
  tags text[],
  balance numeric(10, 2),
  ratio double precision,
  mood mood,
  external_id uuid,
  created_at timestamptz
);
CREATE TABLE orders (
  id serial PRIMARY KEY,
  user_id bigint NOT NULL REFERENCES users (id),
  total numeric,
  placed_at timestamp NOT NULL
);
"#;

// Users above 100 report to the user 100 below them. Orders are placed `user_id % 30` days ago.
const SEED: &str = r#"
INSERT INTO countries SELECT i, 'C' || i FROM generate_series(1, 5) i;
INSERT INTO users (id, country_id, manager_id, "displayName", admin, token, settings, tags,
                   balance, ratio, mood, external_id, created_at)
  OVERRIDING SYSTEM VALUE
  SELECT i, NULLIF(i % 6, 0), CASE WHEN i > 100 THEN i - 100 END,
         E'it''s \\ user ' || i || E'\n\t''', i % 50 = 0, decode(md5(i::text), 'hex'),
         jsonb_build_object('i', i, 'q', 'a"b'), ARRAY['x', 'y z', NULL, E'q\'"'],
         i * 1.25, CASE i % 3 WHEN 0 THEN 'NaN'::float8 WHEN 1 THEN 1.0 / 3 ELSE -1e300 END,
         CASE WHEN i % 2 = 0 THEN 'happy'::mood ELSE 'sad'::mood END, md5(i::text)::uuid,
         timestamptz '2024-01-01 00:00:00.123456+00' + i * interval '1 minute'
  FROM generate_series(1, 300) i;
INSERT INTO orders (user_id, total, placed_at)
  SELECT u, k * 9.5, now() - (u % 30) * interval '1 day'
  FROM generate_series(1, 300) u, generate_series(1, 3) k;
"#;

// Users 291-300 bring their managers 191-200, who bring 91-100.
const SAMPLERS: &str = r#"
samplers:
  - table: countries
    limit: all

  - table: users
    limit: all
    where: id > 290
    deps:
      manager_id: users.id

  - table: orders
    limit: { since: placed_at }
    of:
      user_id: users.id
"#;

async fn connect(url: &str) -> Result<Client> {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    tokio::spawn(connection);
    Ok(client)
}

/// Returns the rows `query` returns, as text.
async fn rows(client: &Client, query: &str) -> Result<Vec<Vec<Option<String>>>> {
    let mut rows = Vec::new();
    for message in client.simple_query(query).await? {
        if let SimpleQueryMessage::Row(row) = message {
            let values: Vec<Option<String>> = (0..row.len())
                .map(|index| row.get(index).map(String::from))
                .collect();
            rows.push(values);
        }
    }
    Ok(rows)
}

async fn scalar(client: &Client, query: &str) -> Result<String> {
    rows(client, query)
        .await?
        .first()
        .and_then(|row| row.first().cloned().flatten())
        .with_context(|| format!("`{query}` returned no value"))
}

/// Drops and creates `databases`, each with the schema, and seeds the first one.
async fn create_databases(base: &str, databases: [&str; 2]) -> Result<Client> {
    let admin = connect(&format!("{base}/postgres")).await?;
    for database in databases {
        // One statement per call: Postgres won't drop or create a database inside the implicit
        // transaction of several.
        admin
            .batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
            .await?;
        admin
            .batch_execute(&format!("CREATE DATABASE {database}"))
            .await?;
        let client = connect(&format!("{base}/{database}")).await?;
        client.batch_execute(SCHEMA).await?;
        if database == databases[0] {
            client.batch_execute(SEED).await?;
        }
    }
    Ok(admin)
}

async fn drop_databases(admin: &Client, databases: [&str; 2]) -> Result<()> {
    for database in databases {
        admin
            .batch_execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
            .await?;
    }
    Ok(())
}

/// Checks that every row of `table` in `target` is the row with the same id in `source`, every
/// value the same.
async fn assert_copied(source: &Client, target: &Client, table: &str) -> Result<()> {
    let query = format!("SELECT id::text, t::text FROM {table} t");
    let originals: BTreeMap<Option<String>, Option<String>> = rows(source, &query)
        .await?
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    for row in rows(target, &query).await? {
        assert_eq!(
            Some(&row[1]),
            originals.get(&row[0]),
            "{table} row {:?} changed on the way",
            row[0]
        );
    }
    Ok(())
}

/// Checks that no row of the sample points at a row the sample lacks.
async fn assert_references_hold(target: &Client) -> Result<()> {
    for (child, column, parent) in [
        ("users", "country_id", "countries"),
        ("users", "manager_id", "users"),
        ("orders", "user_id", "users"),
    ] {
        let dangling = scalar(
            target,
            &format!(
                "SELECT count(*) FROM {child} c WHERE {column} IS NOT NULL \
                 AND NOT EXISTS (SELECT 1 FROM {parent} p WHERE p.id = c.{column})"
            ),
        )
        .await?;
        assert_eq!(dangling, "0", "{child}.{column} points at missing {parent}");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn extracts_a_sample_into_a_sql_file_that_loads_into_postgres() -> Result<()> {
    let Ok(base) = env::var("TAPAS_TEST_POSTGRES_URL") else {
        eprintln!("TAPAS_TEST_POSTGRES_URL not set, skipping");
        return Ok(());
    };
    let databases = ["tapas_pg_file_source", "tapas_pg_file_target"];
    let admin = create_databases(&base, databases).await?;

    let output = env::temp_dir().join(format!("tapas-pg-test-{}.sql", process::id()));
    let config: Config = SAMPLERS.parse()?;
    let since: Since = "2 weeks".parse().map_err(anyhow::Error::msg)?;
    let rows_written = tapas::run(
        Plan::try_new(config, Some(since))?,
        &format!("{base}/{}", databases[0]),
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
    assert_eq!(rows_written["countries"], 5);
    assert_eq!(
        rows_written["users"], 30,
        "10 users and two levels of managers"
    );
    // Users 91-100, 191-194 and 300 placed their orders 14 days ago or less.
    assert_eq!(rows_written["orders"], 45);

    let sample = fs::read_to_string(&output)?;
    fs::remove_file(&output)?;
    let source = connect(&format!("{base}/{}", databases[0])).await?;
    let target = connect(&format!("{base}/{}", databases[1])).await?;
    target
        .batch_execute(&sample)
        .await
        .context("loading the SQL file")?;

    for table in ["countries", "users", "orders"] {
        assert_copied(&source, &target, table).await?;
    }
    assert_references_hold(&target).await?;

    // The sequences moved past the copied ids, so new rows don't clash with them.
    let new_user = scalar(
        &target,
        "INSERT INTO users (admin) VALUES (false) RETURNING id",
    )
    .await?;
    assert_eq!(new_user, "301");
    let highest_order = scalar(&target, "SELECT max(id) FROM orders").await?;
    let new_order = scalar(
        &target,
        "INSERT INTO orders (user_id, placed_at) VALUES (300, now()) RETURNING id",
    )
    .await?;
    assert!(
        new_order.parse::<u64>()? > highest_order.parse::<u64>()?,
        "order {new_order} comes after {highest_order}"
    );

    drop_databases(&admin, databases).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_straight_into_a_postgres_target() -> Result<()> {
    let Ok(base) = env::var("TAPAS_TEST_POSTGRES_URL") else {
        eprintln!("TAPAS_TEST_POSTGRES_URL not set, skipping");
        return Ok(());
    };
    let databases = ["tapas_pg_target_source", "tapas_pg_target_target"];
    let admin = create_databases(&base, databases).await?;
    let source_url = format!("{base}/{}", databases[0]);
    let config: Config = format!(
        "{SAMPLERS}
sanitize:
  tables:
    users:
      displayName: {{ template: \"user_{{id}}\" }}
      token: null
"
    )
    .parse()?;
    let plan = Plan::try_new(config, Some(Since::Ever))?;

    let error = tapas::run(
        plan.clone(),
        &source_url,
        2,
        &Output::Database(source_url.clone()),
        Options::default(),
    )
    .await
    .expect_err("tapas refuses to write into its source");
    assert!(error.to_string().contains("source"), "{error}");

    let rows_written = tapas::run(
        plan,
        &source_url,
        2,
        &Output::Database(format!("{base}/{}", databases[1])),
        Options::default(),
    )
    .await?;
    assert_eq!(rows_written["users"], 30);
    assert_eq!(rows_written["orders"], 90, "--since ever keeps every order");

    let target = connect(&format!("{base}/{}", databases[1])).await?;
    for (table, expected) in [("countries", "5"), ("users", "30"), ("orders", "90")] {
        let found = scalar(&target, &format!("SELECT count(*) FROM {table}")).await?;
        assert_eq!(found, expected, "{table}");
    }
    assert_references_hold(&target).await?;
    let unsanitized = scalar(
        &target,
        r#"SELECT count(*) FROM users WHERE "displayName" <> 'user_' || id OR token IS NOT NULL"#,
    )
    .await?;
    assert_eq!(unsanitized, "0", "every user's name and token is replaced");
    let new_user = scalar(
        &target,
        "INSERT INTO users (admin) VALUES (false) RETURNING id",
    )
    .await?;
    assert_eq!(
        new_user, "301",
        "the target's sequences moved past the copied ids"
    );

    drop(target);
    drop_databases(&admin, databases).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn says_what_a_target_user_needs_to_skip_foreign_key_checks() -> Result<()> {
    let Ok(base) = env::var("TAPAS_TEST_POSTGRES_URL") else {
        eprintln!("TAPAS_TEST_POSTGRES_URL not set, skipping");
        return Ok(());
    };
    let databases = ["tapas_pg_role_source", "tapas_pg_role_target"];
    let admin = create_databases(&base, databases).await?;
    admin
        .batch_execute(
            "DROP ROLE IF EXISTS tapas_writer; \
             CREATE ROLE tapas_writer LOGIN PASSWORD 'writer'",
        )
        .await?;
    connect(&format!("{base}/{}", databases[1]))
        .await?
        .batch_execute(
            "GRANT ALL ON ALL TABLES IN SCHEMA public TO tapas_writer; \
             GRANT ALL ON ALL SEQUENCES IN SCHEMA public TO tapas_writer",
        )
        .await?;
    let (_, server) = base
        .rsplit_once('@')
        .context("the URL names a user, who can create databases")?;
    let target = Output::Database(format!(
        "postgres://tapas_writer:writer@{server}/{}",
        databases[1]
    ));
    let source_url = format!("{base}/{}", databases[0]);
    let plan = Plan::try_new(SAMPLERS.parse()?, Some(Since::Ever))?;

    let error = tapas::run(plan.clone(), &source_url, 2, &target, Options::default())
        .await
        .expect_err("an ordinary user can't skip foreign key checks");
    assert_eq!(
        error.to_string(),
        "setting up the target. Skipping its foreign key checks takes a superuser, or on \
         Postgres 15 and later, GRANT SET ON PARAMETER session_replication_role TO the user"
    );

    admin
        .batch_execute("GRANT SET ON PARAMETER session_replication_role TO tapas_writer")
        .await?;
    let rows_written = tapas::run(plan, &source_url, 2, &target, Options::default()).await?;
    assert_eq!(rows_written["users"], 30, "the grant is enough");

    drop_databases(&admin, databases).await?;
    admin
        .batch_execute(
            "REVOKE SET ON PARAMETER session_replication_role FROM tapas_writer; \
             DROP ROLE tapas_writer",
        )
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn says_why_the_target_refused_a_statement() -> Result<()> {
    let Ok(base) = env::var("TAPAS_TEST_POSTGRES_URL") else {
        eprintln!("TAPAS_TEST_POSTGRES_URL not set, skipping");
        return Ok(());
    };
    let databases = ["tapas_pg_refused_source", "tapas_pg_refused_target"];
    let admin = create_databases(&base, databases).await?;
    // As after a migration the target hasn't run. One row per statement makes extraction still
    // be sending when the target refuses the first one.
    connect(&format!("{base}/{}", databases[1]))
        .await?
        .batch_execute("ALTER TABLE orders DROP COLUMN total")
        .await?;
    let config: Config = "samplers:\n  - table: orders\n    limit: all\n".parse()?;
    let plan = Plan::try_new(config, None)?;
    let source_url = format!("{base}/{}", databases[0]);
    let target = Output::Database(format!("{base}/{}", databases[1]));
    let options = Options {
        batch_rows: 1,
        ..Options::default()
    };

    let error = tapas::run(plan.clone(), &source_url, 2, &target, options)
        .await
        .expect_err("the target has no total column");
    let message = format!("{error:#}");
    assert!(
        message.contains(r#"INSERT INTO "orders" ("id", "user_id", "total", "placed_at")"#),
        "names the statement: {message}"
    );
    assert!(
        message.contains(r#"column "total" of relation "orders" does not exist"#),
        "gives the target's reason: {message}"
    );
    assert!(
        message.contains("VALUES …") && !message.contains("VALUES\n('"),
        "keeps the values out without debug: {message}"
    );

    let debug = Options {
        debug: true,
        ..options
    };
    let error = tapas::run(plan, &source_url, 2, &target, debug)
        .await
        .expect_err("the target has no total column");
    let message = format!("{error:#}");
    assert!(
        message.contains("OVERRIDING SYSTEM VALUE VALUES\n('"),
        "shows the refused statement's values with debug: {message}"
    );

    drop_databases(&admin, databases).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn debug_shows_the_row_the_target_refused() -> Result<()> {
    let Ok(base) = env::var("TAPAS_TEST_POSTGRES_URL") else {
        eprintln!("TAPAS_TEST_POSTGRES_URL not set, skipping");
        return Ok(());
    };
    let databases = ["tapas_pg_row_source", "tapas_pg_row_target"];
    let admin = create_databases(&base, databases).await?;
    // Orders total 9.5, 19 or 28.5, and the target only takes the first two.
    connect(&format!("{base}/{}", databases[1]))
        .await?
        .batch_execute("ALTER TABLE orders ADD CONSTRAINT small CHECK (total < 28)")
        .await?;
    let config: Config = "samplers:\n  - table: orders\n    limit: all\n".parse()?;

    let error = tapas::run(
        Plan::try_new(config, None)?,
        &format!("{base}/{}", databases[0]),
        2,
        &Output::Database(format!("{base}/{}", databases[1])),
        Options {
            debug: true,
            ..Options::default()
        },
    )
    .await
    .expect_err("the target refuses totals of 28 and over");
    let message = format!("{error:#}");
    assert!(
        message.contains("OVERRIDING SYSTEM VALUE VALUES\n('") && message.contains("'28.5'"),
        "shows the refused row: {message}"
    );
    assert!(
        !message.contains("),\n("),
        "leaves out the rows the target took: {message}"
    );
    assert!(
        message.contains(r#"violates check constraint "small""#),
        "gives the target's reason: {message}"
    );

    drop_databases(&admin, databases).await?;
    Ok(())
}
