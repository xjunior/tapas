//! Sanitizes a sample on its way out.

use std::path::{Path, PathBuf};
use std::{env, fs, process};

use anyhow::Result;
use mysql_async::prelude::Queryable;
use tapas::sanitize::Seed;
use tapas::{Config, Options, Output, Plan};

use super::support::{connect, scalar, url};

const SCHEMA: &str = r#"
CREATE TABLE users (
  id BIGINT PRIMARY KEY,
  email VARCHAR(255),
  first_name VARCHAR(40),
  ssn INT UNSIGNED,
  login VARCHAR(40),
  notes TEXT,
  card VARBINARY(32),
  nickname VARCHAR(40)
);
CREATE TABLE orders (
  id BIGINT PRIMARY KEY,
  user_id BIGINT NOT NULL,
  customer_email VARCHAR(255),
  notes TEXT
);
"#;

// Every fifth user has no email and every seventh an empty one. Orders repeat their user's email
// in capitals.
const SEED: &str = r#"
INSERT INTO users
  WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20)
  SELECT i, CASE WHEN i % 5 = 0 THEN NULL WHEN i % 7 = 0 THEN '' ELSE CONCAT('person', i, '@corp.com') END,
         CONCAT('First', i), 123456789 + i, CONCAT('login', i), CONCAT('secret ', i),
         '4111111111111111', CONCAT('nick', i)
  FROM n;
INSERT INTO orders
  SELECT id * 10 + k, id, UPPER(email), 'private' FROM users, (SELECT 1 k UNION SELECT 2) ks;
"#;

const SAMPLERS: &str = r#"
samplers:
  - table: orders
    limit: all
    deps:
      user_id: users.id
sanitize:
  columns:
    "*email*": { fake: email }
    "*notes*": { value: Redacted }
  tables:
    users:
      first_name: { fake: first_name }
      ssn: { fake: ssn }
      card: { fake: credit_card }
      login: { template: "user_{id}" }
      nickname: keep
"#;

fn temp_file(name: &str) -> PathBuf {
    env::temp_dir().join(format!("tapas-sanitize-{name}-{}.sql", process::id()))
}

async fn sample(base: &str, yaml: &str, seed: &str, output: &Path) -> Result<()> {
    let config: Config = yaml.parse()?;
    let plan = Plan::try_new(config, None)?;
    let options = Options {
        sanitize_seed: Some(seed.parse::<Seed>().map_err(anyhow::Error::msg)?),
        ..Options::default()
    };
    tapas::run(
        plan,
        &format!("{base}/tapas_sanitize_source"),
        2,
        &Output::File(output.to_path_buf()),
        options,
    )
    .await?;
    Ok(())
}

/// Returns the statements of a SQL file, sorted, since tables can be written in any order.
fn statements(path: &Path) -> Result<Vec<String>> {
    let mut lines: Vec<String> = fs::read_to_string(path)?
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    Ok(lines)
}

#[tokio::test(flavor = "multi_thread")]
async fn sanitizes_values_consistently() -> Result<()> {
    let Some(base) = url() else {
        return Ok(());
    };

    let mut connection = connect(&base).await?;
    for database in ["tapas_sanitize_source", "tapas_sanitize_target"] {
        connection
            .query_drop(format!(
                "DROP DATABASE IF EXISTS {database}; CREATE DATABASE {database}; \
                 USE {database}; {SCHEMA}"
            ))
            .await?;
    }
    connection
        .query_drop(format!("USE tapas_sanitize_source; {SEED}"))
        .await?;

    let output = temp_file("first");
    sample(&base, SAMPLERS, "a secret", &output).await?;
    let sql = fs::read_to_string(&output)?;
    connection
        .query_drop(format!("USE tapas_sanitize_target; {sql}"))
        .await?;

    let count = |condition: &str| {
        format!(
            "SELECT COUNT(*) FROM tapas_sanitize_target.users t \
             JOIN tapas_sanitize_source.users s ON s.id = t.id WHERE {condition}"
        )
    };
    assert_eq!(scalar(&mut connection, &count("TRUE")).await?, 20);

    // No original email survives; NULL and empty ones stay as they were.
    assert_eq!(
        scalar(
            &mut connection,
            &count("t.email = s.email AND s.email <> ''")
        )
        .await?,
        0,
        "an email made it across"
    );
    assert_eq!(
        scalar(
            &mut connection,
            &count("NOT (t.email <=> s.email) AND (s.email IS NULL OR s.email = '')")
        )
        .await?,
        0,
        "NULL and empty emails stay as they are"
    );
    assert_eq!(
        scalar(
            &mut connection,
            &count("s.email <> '' AND t.email NOT LIKE '%@example.___'")
        )
        .await?,
        0,
        "fake emails use reserved domains"
    );

    // The same email gets the same fake in another table and column, whatever its case.
    assert_eq!(
        scalar(
            &mut connection,
            "SELECT COUNT(*) FROM tapas_sanitize_target.orders o \
             JOIN tapas_sanitize_target.users u ON u.id = o.user_id \
             WHERE NOT (o.customer_email <=> u.email)"
        )
        .await?,
        0,
        "orders.customer_email and users.email should match, as they did in the source"
    );
    assert_eq!(
        scalar(
            &mut connection,
            "SELECT COUNT(DISTINCT email) FROM tapas_sanitize_target.users WHERE email <> ''"
        )
        .await?,
        14,
        "different emails get different fakes"
    );

    // Every other replacement.
    for (condition, why) in [
        ("t.first_name = s.first_name", "first names are faked"),
        (
            "t.ssn NOT BETWEEN 900000000 AND 999999999",
            "fake SSNs use areas never issued",
        ),
        (
            "t.card NOT LIKE '400000%' OR LENGTH(t.card) <> 16",
            "fake cards are in the test range",
        ),
        (
            "t.login <> CONCAT('user_', t.id)",
            "the template fills in the id",
        ),
        ("t.notes <> 'Redacted'", "notes get the fixed value"),
        ("t.nickname <> s.nickname", "keep leaves nicknames alone"),
    ] {
        assert_eq!(
            scalar(&mut connection, &count(condition)).await?,
            0,
            "{why}"
        );
    }
    assert_eq!(
        scalar(
            &mut connection,
            "SELECT COUNT(*) FROM tapas_sanitize_target.orders WHERE notes <> 'Redacted'"
        )
        .await?,
        0,
        "global rules apply to every table"
    );

    // The same seed makes the same sample; another seed makes other fakes.
    let again = temp_file("again");
    sample(&base, SAMPLERS, "a secret", &again).await?;
    assert_eq!(statements(&output)?, statements(&again)?);
    sample(&base, SAMPLERS, "another secret", &again).await?;
    assert_ne!(statements(&output)?, statements(&again)?);

    // A rule that would break a reference stops the run before it writes anything.
    let broken = temp_file("broken");
    let error = sample(
        &base,
        &SAMPLERS.replace(
            "      user_id: users.id",
            "      user_id: users.id\n      customer_email: users.email",
        ),
        "a secret",
        &broken,
    )
    .await
    .expect_err("users.email is a reference now");
    assert_eq!(
        error.to_string(),
        // Both ends of the reference are protected; tables are checked by name, so orders first.
        "config sanitize.columns.*email*: orders.customer_email can't be sanitized, because a \
         reference follows it. Add `customer_email: keep` under sanitize.tables.orders"
    );
    assert!(!broken.exists(), "nothing is written");

    for path in [output, again] {
        fs::remove_file(path)?;
    }
    connection
        .query_drop("DROP DATABASE tapas_sanitize_source; DROP DATABASE tapas_sanitize_target")
        .await?;
    Ok(())
}
