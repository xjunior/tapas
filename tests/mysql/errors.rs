//! What errors say about the source's queries, with and without `debug`.

use std::{env, process};

use anyhow::Result;
use mysql_async::Conn;
use mysql_async::prelude::Queryable;
use tapas::{Config, Options, Output, Plan};

use super::support::{connect, url};

const SCHEMA: &str = "
CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR(100));
CREATE TABLE phones (id BIGINT PRIMARY KEY, user_id BIGINT NOT NULL);
INSERT INTO users VALUES (1, 'ana'), (2, 'bo'), (3, 'cy');
INSERT INTO phones VALUES (10, 1), (20, 2), (30, 3);
";

/// Creates the database `name` with `SCHEMA`.
async fn create_database(base: &str, name: &str) -> Result<Conn> {
    let mut connection = connect(base).await?;
    connection
        .query_drop(format!(
            "DROP DATABASE IF EXISTS {name}; CREATE DATABASE {name}; USE {name}; {SCHEMA}"
        ))
        .await?;
    Ok(connection)
}

/// Runs `samplers` against the database `name`, and returns the error they fail with, as users
/// see it.
async fn error_of(base: &str, name: &str, samplers: &str, debug: bool) -> Result<String> {
    let config: Config = samplers.parse()?;
    let output = env::temp_dir().join(format!("tapas-{name}-{}.sql", process::id()));
    let error = tapas::run(
        Plan::try_new(config, None)?,
        &format!("{base}/{name}"),
        2,
        &Output::File(output),
        Options {
            debug,
            ..Options::default()
        },
    )
    .await
    .expect_err("the samplers fail");
    Ok(format!("{error:#}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn shows_the_values_of_a_failed_query_only_with_debug() -> Result<()> {
    let Some(base) = url() else {
        return Ok(());
    };
    let name = "tapas_errors_values";
    let mut connection = create_database(&base, name).await?;
    let samplers = "
samplers:
  - table: users
    limit: all
    where: id < 3
  - table: phones
    where: nope = 1
    of:
      user_id: users.id
";

    let message = error_of(&base, name, samplers, false).await?;
    assert!(
        message.contains(
            "running SELECT * FROM `phones` WHERE (nope = 1) AND `user_id` IN (<users.id>)"
        ),
        "outlines the query as --plan does: {message}"
    );
    assert!(
        message.contains("Unknown column 'nope'"),
        "gives the source's reason: {message}"
    );

    let message = error_of(&base, name, samplers, true).await?;
    assert!(
        message.contains("running SELECT * FROM `phones` WHERE (nope = 1) AND `user_id` IN (1, 2)"),
        "shows the query's values with debug: {message}"
    );

    connection
        .query_drop(format!("DROP DATABASE {name}"))
        .await?;
    connection.disconnect().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn names_the_query_that_failed_partway_through() -> Result<()> {
    let Some(base) = url() else {
        return Ok(());
    };
    let name = "tapas_errors_stream";
    let mut connection = create_database(&base, name).await?;
    // MySQL sends the columns before it evaluates any row, so the second row fails mid-stream.
    let samplers = "
samplers:
  - table: users
    limit: all
    where: EXP(IF(id = 2, 1000, 0)) > 0
";

    let message = error_of(&base, name, samplers, false).await?;
    assert!(
        message.contains("running SELECT * FROM `users` WHERE (EXP(IF(id = 2, 1000, 0)) > 0)"),
        "names the query: {message}"
    );
    assert!(
        message.contains("out of range"),
        "gives the source's reason: {message}"
    );

    connection
        .query_drop(format!("DROP DATABASE {name}"))
        .await?;
    connection.disconnect().await?;
    Ok(())
}
