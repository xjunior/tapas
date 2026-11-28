//! References that follow several columns.

use anyhow::Result;
use mysql_async::Conn;
use mysql_async::prelude::Queryable;
use tapas::Options;

use super::support::{self, Sample, ids, url};

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

/// Samples the seeded database with `samplers`, one tuple per chunk so chunking counts tuples.
async fn sample(name: &str, samplers: &str) -> Result<Sample> {
    let base = url().expect("callers checked the URL");
    support::sample(
        &base,
        name,
        (SCHEMA, SEED),
        samplers,
        None,
        Options {
            chunk_size: 1,
            ..Options::default()
        },
    )
    .await
}

async fn pairs(connection: &mut Conn, query: &str) -> Result<Vec<(u64, u64)>> {
    Ok(connection.query(query).await?)
}

#[tokio::test(flavor = "multi_thread")]
async fn deps_pull_in_the_rows_a_composite_key_points_at() -> Result<()> {
    if url().is_none() {
        return Ok(());
    }
    let mut sample = sample(
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
    let connection = &mut sample.connection;

    assert_eq!(
        pairs(connection, "SELECT tenant_id, id FROM users ORDER BY 1, 2").await?,
        [(1, 2), (2, 1)],
        "only the users the memberships point at, not every user whose tenant or id matches"
    );
    assert_eq!(
        ids(connection, "memberships").await?,
        [1, 2, 3],
        "the membership whose user is NULL is copied, and pulls nothing"
    );

    sample.finish().await
}

#[tokio::test(flavor = "multi_thread")]
async fn of_follows_the_rows_a_composite_key_matches() -> Result<()> {
    if url().is_none() {
        return Ok(());
    }
    let mut sample = sample(
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
        pairs(
            &mut sample.connection,
            "SELECT id, user_id FROM notes ORDER BY 1"
        )
        .await?,
        [(1, 1)],
        "tenant 2's user 1 has the same id, but is another user"
    );

    sample.finish().await
}
