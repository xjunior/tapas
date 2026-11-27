//! Extracts tables that depend on each other. Needs TAPAS_TEST_DATABASE_URL, like end_to_end.rs.

use std::{env, fs, process};

use anyhow::Result;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use tapas::{Config, Options, Output, Plan, Since};

const SCHEMA: &str = r#"
CREATE TABLE employees (id BIGINT PRIMARY KEY, manager_id BIGINT NULL);
CREATE TABLE projects (id BIGINT PRIMARY KEY, updated_at DATETIME NOT NULL);
CREATE TABLE project_tasks (id BIGINT PRIMARY KEY, project_id BIGINT NOT NULL, updated_at DATETIME NOT NULL);
CREATE TABLE write_offs (id BIGINT PRIMARY KEY, project_id BIGINT NOT NULL, updated_at DATETIME NOT NULL);
"#;

const SEED: &str = r#"
-- Employee n reports to n / 2, so 40 -> 20 -> 10 -> 5 -> 2 -> 1.
INSERT INTO employees
  WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 50)
  SELECT i, IF(i = 1, NULL, i DIV 2) FROM n;

-- 20 old projects with 3 old tasks each; project 12 was updated today, and one task of projects
-- 3 and 7 was.
INSERT INTO projects
  WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20)
  SELECT i, IF(i = 12, NOW(), NOW() - INTERVAL 1 YEAR) FROM n;
INSERT INTO project_tasks
  SELECT p.id * 10 + k, p.id, IF(k = 1 AND p.id IN (3, 7), NOW(), NOW() - INTERVAL 1 YEAR)
  FROM projects p, (SELECT 1 k UNION SELECT 2 UNION SELECT 3) ks;

-- Project 15 was written off today, project 16 a year ago.
INSERT INTO write_offs VALUES (1, 15, NOW()), (2, 16, NOW() - INTERVAL 1 YEAR);
"#;

const SAMPLERS: &str = r#"
samplers:
  # Employee 40, and the manager each employee points at, all the way up.
  - table: employees
    where: id = 40
    deps:
      manager_id: employees.id

  # Recently touched projects and tasks, the projects those tasks point at, and every task of
  # the projects in the sample.
  - table: projects
    time_column: updated_at
  - table: project_tasks
    time_column: updated_at
    deps:
      project_id: projects.id
  - table: project_tasks
    of:
      project_id: projects.id

  # Every write-off, but only the recent ones bring their project along.
  - table: write_offs
  - table: write_offs
    name: recent_write_offs
    time_column: updated_at
  - table: projects
    of:
      id: recent_write_offs.project_id
"#;

async fn ids(connection: &mut Conn, table: &str) -> Result<Vec<u64>> {
    let ids: Vec<u64> = connection
        .query(format!("SELECT id FROM {table} ORDER BY id"))
        .await?;
    Ok(ids)
}

#[tokio::test(flavor = "multi_thread")]
async fn extracts_cycles_until_they_stop_growing() -> Result<()> {
    let Ok(base) = env::var("TAPAS_TEST_DATABASE_URL") else {
        eprintln!("TAPAS_TEST_DATABASE_URL not set, skipping");
        return Ok(());
    };

    let mut connection = Conn::new(Opts::from_url(&base)?).await?;
    for database in ["tapas_cycles_source", "tapas_cycles_target"] {
        connection
            .query_drop(format!(
                "DROP DATABASE IF EXISTS {database}; CREATE DATABASE {database}"
            ))
            .await?;
    }
    connection
        .query_drop(format!("USE tapas_cycles_source; {SCHEMA} {SEED}"))
        .await?;
    connection
        .query_drop(format!("USE tapas_cycles_target; {SCHEMA}"))
        .await?;

    let config: Config = SAMPLERS.parse()?;
    let since: Since = "2 weeks".parse().map_err(anyhow::Error::msg)?;
    let plan = Plan::try_new(config, Some(since))?;
    assert_eq!(plan.cycles.len(), 2);

    let output = env::temp_dir().join(format!("tapas-cycles-{}.sql", process::id()));
    let rows = tapas::run(
        plan,
        &format!("{base}/tapas_cycles_source"),
        4,
        &Output::File(output.clone()),
        // Tiny chunks, so each round's new values span several queries.
        Options {
            chunk_size: 2,
            batch_rows: 3,
        },
    )
    .await?;

    let sql = fs::read_to_string(&output)?;
    fs::remove_file(&output)?;
    connection
        .query_drop(format!("USE tapas_cycles_target; {sql}"))
        .await?;

    assert_eq!(
        ids(&mut connection, "employees").await?,
        [1, 2, 5, 10, 20, 40]
    );
    assert_eq!(rows["employees"], 6);

    assert_eq!(ids(&mut connection, "projects").await?, [3, 7, 12, 15]);
    assert_eq!(
        ids(&mut connection, "project_tasks").await?,
        [31, 32, 33, 71, 72, 73, 121, 122, 123, 151, 152, 153]
    );
    assert_eq!(rows["project_tasks"], 12, "each task written once");
    assert_eq!(ids(&mut connection, "write_offs").await?, [1, 2]);

    connection
        .query_drop("DROP DATABASE tapas_cycles_source; DROP DATABASE tapas_cycles_target")
        .await?;
    connection.disconnect().await?;
    Ok(())
}
