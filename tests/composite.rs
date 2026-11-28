//! References that follow several columns. Needs TAPAS_TEST_DATABASE_URL, like end_to_end.rs.

use std::{env, fs, process};

use anyhow::Result;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use tapas::{Config, Options, Output, Plan};

// Every tenant has users 1 and 2, so a user's id alone doesn't say which user it is.
const SCHEMA: &str = r#"
CREATE TABLE users (tenant_id INT NOT NULL, id INT NOT NULL, PRIMARY KEY (tenant_id, id));
CREATE TABLE memberships (id INT PRIMARY KEY, tenant_id INT NOT NULL, user_id INT NULL);
CREATE TABLE notes (id INT PRIMARY KEY, tenant_id INT NOT NULL, user_id INT NOT NULL);
"#;

const SEED: &str = r#"
INSERT INTO users VALUES (1, 1), (1, 2), (2, 1), (2, 2);
-- Tenant 1's user 2, tenant 2's user 1, and one that points at no user at all.
INSERT INTO memberships VALUES (1, 1, 2), (2, 2, 1), (3, 1, NULL);
INSERT INTO notes VALUES (1, 1, 1), (2, 2, 1), (3, 1, 2), (4, 2, 2);
"#;

/// Extracts a sample of the seeded database with `samplers`, loads it into an empty copy, and
/// returns a connection to it.
async fn sample(name: &str, samplers: &str) -> Result<Conn> {
    let base = env::var("TAPAS_TEST_DATABASE_URL")?;
    let (source, target) = (
        format!("tapas_{name}_source"),
        format!("tapas_{name}_target"),
    );
    let mut connection = Conn::new(Opts::from_url(&base)?).await?;
    for database in [&source, &target] {
        connection
            .query_drop(format!(
                "DROP DATABASE IF EXISTS {database}; CREATE DATABASE {database}"
            ))
            .await?;
    }
    connection
        .query_drop(format!("USE {source}; {SCHEMA} {SEED}"))
        .await?;
    connection
        .query_drop(format!("USE {target}; {SCHEMA}"))
        .await?;

    let config: Config = samplers.parse()?;
    let output = env::temp_dir().join(format!("tapas-{name}-{}.sql", process::id()));
    tapas::run(
        Plan::try_new(config, None)?,
        &format!("{base}/{source}"),
        4,
        &Output::File(output.clone()),
        // One tuple per chunk, so chunking counts tuples.
        Options {
            chunk_size: 1,
            ..Options::default()
        },
    )
    .await?;
    let sql = fs::read_to_string(&output)?;
    fs::remove_file(&output)?;
    connection
        .query_drop(format!("USE {target}; {sql}"))
        .await?;
    Ok(connection)
}

async fn pairs(connection: &mut Conn, query: &str) -> Result<Vec<(u64, u64)>> {
    Ok(connection.query(query).await?)
}

#[tokio::test(flavor = "multi_thread")]
async fn deps_pull_in_the_rows_a_composite_key_points_at() -> Result<()> {
    if env::var("TAPAS_TEST_DATABASE_URL").is_err() {
        eprintln!("TAPAS_TEST_DATABASE_URL not set, skipping");
        return Ok(());
    }
    let mut connection = sample(
        "composite_deps",
        r#"
samplers:
  - table: memberships
    where: id <= 3
    limit: all
    deps:
      - { tenant_id: users.tenant_id, user_id: users.id }
"#,
    )
    .await?;

    assert_eq!(
        pairs(
            &mut connection,
            "SELECT tenant_id, id FROM users ORDER BY 1, 2"
        )
        .await?,
        [(1, 2), (2, 1)],
        "only the users the memberships point at, not every user whose tenant or id matches"
    );
    let memberships: Vec<u64> = connection
        .query("SELECT id FROM memberships ORDER BY 1")
        .await?;
    assert_eq!(
        memberships,
        [1, 2, 3],
        "the membership whose user is NULL is copied, and pulls nothing"
    );

    connection
        .query_drop(
            "DROP DATABASE tapas_composite_deps_source; DROP DATABASE tapas_composite_deps_target",
        )
        .await?;
    connection.disconnect().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn of_follows_the_rows_a_composite_key_matches() -> Result<()> {
    if env::var("TAPAS_TEST_DATABASE_URL").is_err() {
        eprintln!("TAPAS_TEST_DATABASE_URL not set, skipping");
        return Ok(());
    }
    let mut connection = sample(
        "composite_of",
        r#"
samplers:
  - table: users
    where: tenant_id = 1 AND id = 1
    limit: all
  - table: notes
    of:
      - { tenant_id: users.tenant_id, user_id: users.id }
"#,
    )
    .await?;

    assert_eq!(
        pairs(&mut connection, "SELECT id, user_id FROM notes ORDER BY 1").await?,
        [(1, 1)],
        "tenant 2's user 1 has the same id, but is another user"
    );

    connection
        .query_drop(
            "DROP DATABASE tapas_composite_of_source; DROP DATABASE tapas_composite_of_target",
        )
        .await?;
    connection.disconnect().await?;
    Ok(())
}
