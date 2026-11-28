//! Extracts tables that depend on each other.

use anyhow::Result;
use tapas::{Config, Options, Plan, Since};

use super::support::{ids, sample, url};

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
    limit: all
    where: id = 40
    deps:
      manager_id: employees.id

  # Recently touched projects and tasks, the projects those tasks point at, and every task of
  # the projects in the sample.
  - table: projects
    limit: { since: updated_at }
  - table: project_tasks
    limit: { since: updated_at }
    deps:
      project_id: projects.id
  - table: project_tasks
    of:
      project_id: projects.id

  # Every write-off, but only the recent ones bring their project along.
  - table: write_offs
    limit: all
  - table: write_offs
    name: recent_write_offs
    limit: { since: updated_at }
  - table: projects
    of:
      id: recent_write_offs.project_id
"#;

#[tokio::test(flavor = "multi_thread")]
async fn extracts_cycles_until_they_stop_growing() -> Result<()> {
    let Some(base) = url() else {
        return Ok(());
    };
    let config: Config = SAMPLERS.parse()?;
    let since: Since = "2 weeks".parse().map_err(anyhow::Error::msg)?;
    assert_eq!(Plan::try_new(config, Some(since))?.cycles.len(), 2);

    let mut sample = sample(
        &base,
        "cycles",
        (SCHEMA, SEED),
        SAMPLERS,
        Some("2 weeks"),
        // Tiny chunks, so each round's new values span several queries.
        Options {
            chunk_size: 2,
            batch_rows: 3,
            ..Options::default()
        },
    )
    .await?;
    let connection = &mut sample.connection;

    assert_eq!(ids(connection, "employees").await?, [1, 2, 5, 10, 20, 40]);
    assert_eq!(sample.rows["employees"], 6);

    assert_eq!(ids(connection, "projects").await?, [3, 7, 12, 15]);
    assert_eq!(
        ids(connection, "project_tasks").await?,
        [31, 32, 33, 71, 72, 73, 121, 122, 123, 151, 152, 153]
    );
    assert_eq!(sample.rows["project_tasks"], 12, "each task written once");
    assert_eq!(ids(connection, "write_offs").await?, [1, 2]);

    sample.finish().await
}
