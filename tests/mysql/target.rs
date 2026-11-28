//! Writes a sample straight into a target database.

use anyhow::Result;
use mysql_async::prelude::Queryable;
use tapas::{Config, Options, Output, Plan};

use super::support::{connect, scalar, url};

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
    limit: all
    where: user_id >= 2
    deps:
      user_id: users.id
"#;

#[tokio::test(flavor = "multi_thread")]
async fn writes_straight_into_a_target_database() -> Result<()> {
    let Some(base) = url() else {
        return Ok(());
    };

    let mut connection = connect(&base).await?;
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

    for (table, expected) in [("orders", 3), ("users", 2)] {
        let found = scalar(
            &mut connection,
            &format!("SELECT COUNT(*) FROM tapas_target_target.{table}"),
        )
        .await?;
        assert_eq!(found, expected, "{table}");
    }

    connection
        .query_drop("DROP DATABASE tapas_target_source; DROP DATABASE tapas_target_target")
        .await?;
    connection.disconnect().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn says_why_the_target_refused_a_statement() -> Result<()> {
    let Some(base) = url() else {
        return Ok(());
    };

    // The target's users table lacks a column the source has, as after a migration the target
    // hasn't run. More statements than the output's channel holds make extraction still be
    // sending when the target refuses the first one.
    let mut connection = connect(&base).await?;
    connection
        .query_drop(
            "DROP DATABASE IF EXISTS tapas_refused_source; CREATE DATABASE tapas_refused_source;
             DROP DATABASE IF EXISTS tapas_refused_target; CREATE DATABASE tapas_refused_target;
             CREATE TABLE tapas_refused_source.users (id BIGINT PRIMARY KEY, name VARCHAR(100));
             CREATE TABLE tapas_refused_target.users (id BIGINT PRIMARY KEY);
             SET SESSION cte_max_recursion_depth = 1000;
             INSERT INTO tapas_refused_source.users
               WITH RECURSIVE n (id) AS (SELECT 1 UNION ALL SELECT id + 1 FROM n WHERE id < 1000)
               SELECT id, 'ana' FROM n;",
        )
        .await?;

    let config: Config = "samplers:\n  - table: users\n    limit: all\n".parse()?;
    let plan = Plan::try_new(config, None)?;
    let source_url = format!("{base}/tapas_refused_source");
    let target = Output::Database(format!("{base}/tapas_refused_target"));
    let options = Options {
        batch_rows: 1,
        ..Options::default()
    };

    let error = tapas::run(plan.clone(), &source_url, 2, &target, options)
        .await
        .expect_err("the target has no name column");
    let message = format!("{error:#}");
    assert!(
        message.contains("INSERT INTO `users` (`id`, `name`) VALUES"),
        "names the statement: {message}"
    );
    assert!(
        message.contains("Unknown column 'name'"),
        "gives the target's reason: {message}"
    );
    assert!(
        !message.contains("'ana'"),
        "keeps the values out without debug: {message}"
    );

    let debug = Options {
        debug: true,
        ..options
    };
    let error = tapas::run(plan, &source_url, 2, &target, debug)
        .await
        .expect_err("the target has no name column");
    let message = format!("{error:#}");
    assert!(
        message.contains(", 'ana');"),
        "shows the refused statement's values with debug: {message}"
    );

    connection
        .query_drop("DROP DATABASE tapas_refused_source; DROP DATABASE tapas_refused_target")
        .await?;
    connection.disconnect().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn debug_shows_the_row_the_target_refused() -> Result<()> {
    let Some(base) = url() else {
        return Ok(());
    };

    // One name is too long for the target, and all four rows go in one statement.
    let mut connection = connect(&base).await?;
    connection
        .query_drop(
            "DROP DATABASE IF EXISTS tapas_row_source; CREATE DATABASE tapas_row_source;
             DROP DATABASE IF EXISTS tapas_row_target; CREATE DATABASE tapas_row_target;
             CREATE TABLE tapas_row_source.users (id BIGINT PRIMARY KEY, name VARCHAR(100));
             CREATE TABLE tapas_row_target.users (id BIGINT PRIMARY KEY, name VARCHAR(5));
             INSERT INTO tapas_row_source.users
               VALUES (1, 'ana'), (2, 'bo'), (3, 'Maximiliana'), (4, 'di');",
        )
        .await?;

    let config: Config = "samplers:\n  - table: users\n    limit: all\n".parse()?;
    let error = tapas::run(
        Plan::try_new(config, None)?,
        &format!("{base}/tapas_row_source"),
        2,
        &Output::Database(format!("{base}/tapas_row_target")),
        Options {
            debug: true,
            ..Options::default()
        },
    )
    .await
    .expect_err("the target's names are too short");
    let message = format!("{error:#}");
    assert!(
        message.contains("running INSERT INTO `users` (`id`, `name`) VALUES\n(3, 'Maximiliana');"),
        "shows the refused row: {message}"
    );
    assert!(
        !message.contains("'ana'"),
        "leaves out the rows the target took: {message}"
    );
    assert!(
        message.contains("Data too long for column 'name'"),
        "gives the target's reason: {message}"
    );

    connection
        .query_drop("DROP DATABASE tapas_row_source; DROP DATABASE tapas_row_target")
        .await?;
    connection.disconnect().await?;
    Ok(())
}
