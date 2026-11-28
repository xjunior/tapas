//! Following the source's foreign keys instead of declared `deps`. The database tests need
//! TAPAS_TEST_DATABASE_URL, like end_to_end.rs.

use std::process::Command;
use std::{env, fs, process};

use anyhow::Result;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use tapas::{Config, Options, Output, Plan};

// Every tenant has users 1 and 2, and a manager is a user of the same tenant.
const SCHEMA: &str = r#"
CREATE TABLE territories (id INT PRIMARY KEY);
CREATE TABLE users (
  tenant_id INT NOT NULL, id INT NOT NULL, territory_id INT NULL, manager_id INT NULL,
  PRIMARY KEY (tenant_id, id),
  CONSTRAINT users_territory_fk FOREIGN KEY (territory_id) REFERENCES territories (id),
  CONSTRAINT users_manager_fk FOREIGN KEY (tenant_id, manager_id) REFERENCES users (tenant_id, id)
);
CREATE TABLE orders (
  id INT PRIMARY KEY, tenant_id INT NOT NULL, user_id INT NOT NULL,
  CONSTRAINT orders_user_fk FOREIGN KEY (tenant_id, user_id) REFERENCES users (tenant_id, id)
);
"#;

const SEED: &str = r#"
INSERT INTO territories VALUES (1), (2), (3);
INSERT INTO users VALUES
  (1, 1, 1, NULL), (1, 2, 2, 1), (1, 3, NULL, 2),
  (2, 1, 1, NULL), (2, 2, 3, 1);
INSERT INTO orders VALUES (1, 1, 3), (2, 2, 1), (3, 2, 2);
"#;

const ORDERS: &str = r#"
samplers:
  - table: orders
    where: id <= 2
    limit: all
"#;

// What the foreign keys say, written as `deps`.
const DECLARED: &str = r#"
samplers:
  - table: orders
    where: id <= 2
    limit: all
    deps:
      - { tenant_id: users.tenant_id, user_id: users.id }
  - table: users
    deps:
      - { tenant_id: users.tenant_id, manager_id: users.id }
      - { territory_id: territories.id }
"#;

/// Which `deps` a run follows.
#[derive(Clone, Copy)]
enum Refs {
    ForeignKeys,
    Declared,
}

struct Rows {
    orders: Vec<u64>,
    users: Vec<(u64, u64)>,
    territories: Vec<u64>,
}

async fn recreate(connection: &mut Conn, database: &str) -> Result<()> {
    connection
        .query_drop(format!(
            "DROP DATABASE IF EXISTS {database}; CREATE DATABASE {database}"
        ))
        .await?;
    Ok(())
}

/// Extracts a sample of the seeded database with `samplers` into an empty copy, and returns what
/// landed there.
async fn sample(name: &str, samplers: &str, refs: Refs) -> Result<Rows> {
    let base = env::var("TAPAS_TEST_DATABASE_URL")?;
    let (source, target) = (
        format!("tapas_{name}_source"),
        format!("tapas_{name}_target"),
    );
    let mut connection = Conn::new(Opts::from_url(&base)?).await?;
    recreate(&mut connection, &source).await?;
    recreate(&mut connection, &target).await?;
    connection
        .query_drop(format!("USE {source}; {SCHEMA} {SEED}"))
        .await?;
    connection
        .query_drop(format!("USE {target}; {SCHEMA}"))
        .await?;

    let source_url = format!("{base}/{source}");
    let config: Config = samplers.parse()?;
    let plan = match refs {
        Refs::ForeignKeys => {
            let keys = tapas::foreign_keys(&source_url).await?;
            Plan::try_new_with_foreign_keys(config, None, &keys)?
        }
        Refs::Declared => Plan::try_new(config, None)?,
    };
    let output = env::temp_dir().join(format!("tapas-{name}-{}.sql", process::id()));
    tapas::run(
        plan,
        &source_url,
        4,
        &Output::File(output.clone()),
        // One value per chunk, so chunking counts tuples.
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

    let rows = Rows {
        orders: connection.query("SELECT id FROM orders ORDER BY 1").await?,
        users: connection
            .query("SELECT tenant_id, id FROM users ORDER BY 1, 2")
            .await?,
        territories: connection
            .query("SELECT id FROM territories ORDER BY 1")
            .await?,
    };
    connection
        .query_drop(format!("DROP DATABASE {source}; DROP DATABASE {target}"))
        .await?;
    connection.disconnect().await?;
    Ok(rows)
}

fn database_url() -> Option<String> {
    let url = env::var("TAPAS_TEST_DATABASE_URL").ok();
    if url.is_none() {
        eprintln!("TAPAS_TEST_DATABASE_URL not set, skipping");
    }
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_foreign_keys_like_the_deps_that_say_the_same() -> Result<()> {
    if database_url().is_none() {
        return Ok(());
    }
    let followed = sample("fk_followed", ORDERS, Refs::ForeignKeys).await?;

    assert_eq!(followed.orders, [1, 2]);
    assert_eq!(
        followed.users,
        [(1, 1), (1, 2), (1, 3), (2, 1)],
        "the users of the orders, and their managers up the chain, but not tenant 2's user 2"
    );
    assert_eq!(
        followed.territories,
        [1, 2],
        "the territories of those users; territory 3 belongs to a user left out"
    );

    let declared = sample("fk_declared", DECLARED, Refs::Declared).await?;
    assert_eq!(followed.orders, declared.orders);
    assert_eq!(followed.users, declared.users);
    assert_eq!(followed.territories, declared.territories);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn without_foreign_keys_nothing_is_followed() -> Result<()> {
    if database_url().is_none() {
        return Ok(());
    }
    let declared_only = sample("fk_off", ORDERS, Refs::Declared).await?;
    assert_eq!(declared_only.orders, [1, 2]);
    assert!(declared_only.users.is_empty(), "no deps, no users");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_every_foreign_key_of_the_database() -> Result<()> {
    let Some(base) = database_url() else {
        return Ok(());
    };
    let database = "tapas_fk_keys";
    let mut connection = Conn::new(Opts::from_url(&base)?).await?;
    recreate(&mut connection, database).await?;
    connection
        .query_drop(format!("USE {database}; {SCHEMA}"))
        .await?;

    let keys = tapas::foreign_keys(&format!("{base}/{database}")).await?;
    let described: Vec<String> = keys
        .iter()
        .map(|key| {
            format!(
                "{}: {}.{} -> {}",
                key.name, key.table, key.columns, key.parent
            )
        })
        .collect();
    assert_eq!(
        described,
        [
            "orders_user_fk: orders.(tenant_id, user_id) -> users.(tenant_id, id)",
            "users_manager_fk: users.(tenant_id, manager_id) -> users.(tenant_id, id)",
            "users_territory_fk: users.territory_id -> territories.id",
        ]
    );

    connection
        .query_drop(format!("DROP DATABASE {database}"))
        .await?;
    connection.disconnect().await?;
    Ok(())
}

/// Runs the tapas binary on `config` with `args`, and returns what it printed to stdout.
fn plan_of(name: &str, config: &str, args: &[&str]) -> Result<String> {
    let path = env::temp_dir().join(format!("tapas-{name}-{}.yml", process::id()));
    fs::write(&path, config)?;
    let output = Command::new(env!("CARGO_BIN_EXE_tapas"))
        .arg(&path)
        .arg("--plan")
        .args(args)
        .output()?;
    fs::remove_file(&path)?;
    assert!(
        output.status.success(),
        "tapas failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn a_plan_without_a_source_says_it_did_not_read_the_foreign_keys() -> Result<()> {
    let note = "foreign keys: not read, since --source wasn't given";

    let plan = plan_of("fk_default", ORDERS, &[])?;
    assert!(plan.starts_with(note), "following is the default: {plan}");

    let plan = plan_of("fk_flag", ORDERS, &["--disable-foreign-keys"])?;
    assert!(!plan.contains(note), "nothing to read: {plan}");

    let config = format!("foreign_keys: false\n{ORDERS}");
    let plan = plan_of("fk_yaml", &config, &[])?;
    assert!(!plan.contains(note), "the config turns it off too: {plan}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plan_follows_foreign_keys_unless_disabled() -> Result<()> {
    let Some(base) = database_url() else {
        return Ok(());
    };
    let database = "tapas_fk_plan";
    let mut connection = Conn::new(Opts::from_url(&base)?).await?;
    recreate(&mut connection, database).await?;
    connection
        .query_drop(format!("USE {database}; {SCHEMA}"))
        .await?;
    let source = format!("{base}/{database}");
    let pull = "pulled in by foreign key orders_user_fk";

    let plan = plan_of("fk_plan_on", ORDERS, &["--source", &source])?;
    assert!(plan.contains(pull), "on by default: {plan}");

    let plan = plan_of(
        "fk_plan_flag",
        ORDERS,
        &["--source", &source, "--disable-foreign-keys"],
    )?;
    assert!(!plan.contains(pull), "--disable-foreign-keys: {plan}");

    let config = format!("foreign_keys: false\n{ORDERS}");
    let plan = plan_of("fk_plan_yaml", &config, &["--source", &source])?;
    assert!(!plan.contains(pull), "foreign_keys: false: {plan}");

    connection
        .query_drop(format!("DROP DATABASE {database}"))
        .await?;
    connection.disconnect().await?;
    Ok(())
}
