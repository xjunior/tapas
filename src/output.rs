//! Where the sample goes: a SQL file, or straight into a target database.

use std::fmt::{self, Display, Formatter};
use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use tokio::sync::mpsc;
use tokio::task::JoinHandle as TaskHandle;
use tokio_postgres::Client;

use crate::postgres;
use crate::sql::{self, Dialect};

/// Where the sample goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// A SQL file of INSERT statements, gzipped when its path ends in `.gz`.
    File(PathBuf),
    /// A database of the same kind as the source, as mysql://user:password@host:port/database
    /// or postgres://user:password@host:port/database.
    Database(String),
}

impl Display for Output {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Output::File(path) => write!(f, "{}", path.display()),
            Output::Database(url) => {
                let location = match Dialect::from_url(url) {
                    Some(Dialect::MySql) => Opts::from_url(url).ok().map(|connection_options| {
                        format!(
                            "{}:{}/{}",
                            connection_options.ip_or_hostname(),
                            connection_options.tcp_port(),
                            connection_options.db_name().unwrap_or_default()
                        )
                    }),
                    Some(Dialect::Postgres) => postgres::config(url)
                        .ok()
                        .map(|config| postgres::location(&config)),
                    None => None,
                };
                f.write_str(location.as_deref().unwrap_or("the target database"))
            }
        }
    }
}

/// What `finish` does with the statements written so far.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    /// The whole sample worked: keep what was written.
    Keep,
    /// Something failed: throw away what was written.
    Discard,
}

/// Receives INSERT statements and writes them to an `Output`.
#[must_use = "call `finish` to wait for the statements and keep or discard them"]
pub enum Sink {
    /// Writes to a SQL file.
    File(SqlFile),
    /// Writes to a MySQL target database.
    MySql(MySqlTarget),
    /// Writes to a Postgres target database.
    Postgres(PostgresTarget),
}

impl Sink {
    /// Opens `output` for the INSERTs into `tables`, written in `dialect`. With `debug`, a
    /// statement the target refuses shows in full in the error. Returns the sink and the sender
    /// to send statements through.
    pub async fn open(
        output: &Output,
        dialect: Dialect,
        tables: &[&str],
        debug: bool,
    ) -> Result<(Self, mpsc::Sender<Vec<u8>>)> {
        Ok(match (output, dialect) {
            (Output::File(path), Dialect::MySql | Dialect::Postgres) => {
                let (file, sender) = SqlFile::create(path, dialect, tables)?;
                (Sink::File(file), sender)
            }
            (Output::Database(url), Dialect::MySql) => {
                let (target, sender) = MySqlTarget::connect(url, debug).await?;
                (Sink::MySql(target), sender)
            }
            (Output::Database(url), Dialect::Postgres) => {
                let (target, sender) = PostgresTarget::connect(url, tables, debug).await?;
                (Sink::Postgres(target), sender)
            }
        })
    }

    /// Waits for every statement to be written, then keeps or discards them as `outcome` says.
    /// Drop all senders first.
    ///
    /// # Panics
    ///
    /// Panics if the thread or task that writes the statements panicked.
    pub async fn finish(self, outcome: Outcome) -> Result<()> {
        match self {
            Sink::File(file) => file.finish(outcome),
            Sink::MySql(target) => target.finish(outcome).await,
            Sink::Postgres(target) => target.finish(outcome).await,
        }
    }
}

/// Writes INSERT statements to `<path>.partial` on its own thread, gzipped when `path` ends in
/// `.gz`. The file is only moved to `path` by `finish`, so a failed run never leaves something
/// that looks like a full sample.
#[must_use = "call `finish` to wait for the statements and keep or discard them"]
pub struct SqlFile {
    path: PathBuf,
    partial: PathBuf,
    thread: JoinHandle<Result<()>>,
}

impl SqlFile {
    /// Creates `<path>.partial` and starts the thread that writes to it the INSERTs into
    /// `tables`, between the settings `dialect` needs to load them. Returns the file and the
    /// sender to send statements through.
    pub fn create(
        path: &Path,
        dialect: Dialect,
        tables: &[&str],
    ) -> Result<(Self, mpsc::Sender<Vec<u8>>)> {
        use std::io::Write;

        let encoding = Encoding::for_path(path);
        let partial = PathBuf::from(format!("{}.partial", path.display()));
        let file =
            File::create(&partial).with_context(|| format!("creating {}", partial.display()))?;
        let (sender, receiver) = mpsc::channel::<Vec<u8>>(256);
        let header = sql::file_header(dialect);
        let footer = sql::file_footer(dialect, tables);

        let thread = thread::spawn(move || -> Result<()> {
            let out = BufWriter::with_capacity(1 << 20, file);
            match encoding {
                Encoding::Plain => write_statements(out, header, &footer, receiver)?.flush()?,
                Encoding::Gzip => {
                    let gzip = GzEncoder::new(out, Compression::fast());
                    write_statements(gzip, header, &footer, receiver)?
                        .finish()?
                        .flush()?
                }
            }
            Ok(())
        });

        Ok((
            Self {
                path: path.to_path_buf(),
                partial,
                thread,
            },
            sender,
        ))
    }

    /// Waits for every statement to be written, then moves the file to its path, or removes it
    /// unless the `outcome` is `Keep`. Drop all senders first.
    ///
    /// # Panics
    ///
    /// Panics if the thread that writes the file panicked.
    pub fn finish(self, outcome: Outcome) -> Result<()> {
        let written = self
            .thread
            .join()
            .expect("the SQL writer thread only returns errors, it doesn't panic");
        let keep = match outcome {
            Outcome::Keep => written.is_ok(),
            Outcome::Discard => false,
        };
        if keep {
            fs::rename(&self.partial, &self.path)
                .with_context(|| format!("moving output to {}", self.path.display()))?;
        } else {
            let _ = fs::remove_file(&self.partial);
        }
        written.context("writing the SQL file")
    }
}

/// How a `SqlFile` encodes what it writes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Encoding {
    /// Plain SQL text.
    Plain,
    /// Gzipped SQL text, for a path that ends in `.gz`.
    Gzip,
}

impl Encoding {
    fn for_path(path: &Path) -> Self {
        match path.extension() {
            Some(extension) if extension.eq_ignore_ascii_case("gz") => Encoding::Gzip,
            _ => Encoding::Plain,
        }
    }
}

/// Writes `header`, every statement `receiver` gets, and `footer` to `out`, and returns `out` for
/// the caller to flush or finish.
fn write_statements<W: io::Write>(
    mut out: W,
    header: &[u8],
    footer: &[u8],
    mut receiver: mpsc::Receiver<Vec<u8>>,
) -> io::Result<W> {
    out.write_all(header)?;
    while let Some(statement) = receiver.blocking_recv() {
        out.write_all(&statement)?;
    }
    out.write_all(footer)?;
    Ok(out)
}

/// Runs INSERT statements against a MySQL target database over one connection, inside one
/// transaction. `finish` commits it only once the whole sample worked, so a failed run leaves
/// the target as it was.
#[must_use = "call `finish` to wait for the statements and commit or roll them back"]
pub struct MySqlTarget {
    task: TaskHandle<Result<Conn>>,
}

impl MySqlTarget {
    /// Connects to the database at `url`, starts a transaction, and starts the task that runs
    /// the statements. With `debug`, a refused statement shows in full in the error. Returns the
    /// target and the sender to send statements through.
    pub async fn connect(url: &str, debug: bool) -> Result<(Self, mpsc::Sender<Vec<u8>>)> {
        let connection_options = Opts::from_url(url).context("parsing the target URL")?;
        let mut connection = Conn::new(connection_options)
            .await
            .context("connecting to the target")?;
        connection
            .query_drop("SET NAMES utf8mb4")
            .await
            .context("setting up the target")?;
        connection
            .query_drop("SET SESSION FOREIGN_KEY_CHECKS = 0")
            .await
            .context("setting up the target")?;
        connection
            .query_drop("START TRANSACTION")
            .await
            .context("starting a transaction on the target")?;

        let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(256);
        let task = tokio::spawn(async move {
            while let Some(statement) = receiver.recv().await {
                connection
                    .query_drop(statement.as_slice())
                    .await
                    .with_context(|| format!("running {}", refused(&statement, debug)))?;
            }
            Ok(connection)
        });
        Ok((Self { task }, sender))
    }

    /// Waits for every statement to run, then commits, or rolls back unless the `outcome` is
    /// `Keep`. Drop all senders first.
    ///
    /// # Panics
    ///
    /// Panics if the task that runs the statements panicked.
    pub async fn finish(self, outcome: Outcome) -> Result<()> {
        let mut connection = self
            .task
            .await
            .expect("the target writer task only returns errors, it doesn't panic")
            .context("writing to the target")?;
        match outcome {
            Outcome::Keep => {
                connection
                    .query_drop("COMMIT")
                    .await
                    .context("committing to the target")?;
            }
            Outcome::Discard => {
                connection.query_drop("ROLLBACK").await.ok();
            }
        }
        connection.disconnect().await.ok();
        Ok(())
    }
}

/// Runs INSERT statements against a Postgres target database over one connection, inside one
/// transaction, like `MySqlTarget`. Foreign keys go unchecked while the statements run, and the
/// tables' sequences move past the copied rows before the commit.
#[must_use = "call `finish` to wait for the statements and commit or roll them back"]
pub struct PostgresTarget {
    task: TaskHandle<Result<Client>>,
    footer: String,
}

impl PostgresTarget {
    /// Connects to the database at `url`, starts a transaction, and starts the task that runs
    /// the INSERTs into `tables`. With `debug`, a refused statement shows in full in the error.
    /// Returns the target and the sender to send statements through.
    pub async fn connect(
        url: &str,
        tables: &[&str],
        debug: bool,
    ) -> Result<(Self, mpsc::Sender<Vec<u8>>)> {
        let config = postgres::config(url).context("parsing the target URL")?;
        let client = postgres::connect(&config)
            .await
            .context("connecting to the target")?;
        let header = str::from_utf8(sql::file_header(Dialect::Postgres))
            .expect("the Postgres header is ASCII");
        client.batch_execute(header).await.context(
            "setting up the target. Skipping its foreign key checks takes a superuser, or on \
             Postgres 15 and later, GRANT SET ON PARAMETER session_replication_role TO the user",
        )?;
        client
            .batch_execute("BEGIN")
            .await
            .context("starting a transaction on the target")?;
        let footer = String::from_utf8(sql::file_footer(Dialect::Postgres, tables))
            .expect("the footer holds table names, which are UTF-8");

        let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(256);
        let task = tokio::spawn(async move {
            while let Some(statement) = receiver.recv().await {
                // Postgres sends text, so its values, and the statements made of them, are UTF-8.
                let statement = str::from_utf8(&statement).context("a statement isn't UTF-8")?;
                client
                    .batch_execute(statement)
                    .await
                    .with_context(|| format!("running {}", refused(statement.as_bytes(), debug)))?;
            }
            Ok(client)
        });
        Ok((Self { task, footer }, sender))
    }

    /// Waits for every statement to run, then moves the sequences and commits, or rolls back
    /// unless the `outcome` is `Keep`. Drop all senders first.
    ///
    /// # Panics
    ///
    /// Panics if the task that runs the statements panicked.
    pub async fn finish(self, outcome: Outcome) -> Result<()> {
        let client = self
            .task
            .await
            .expect("the target writer task only returns errors, it doesn't panic")
            .context("writing to the target")?;
        match outcome {
            Outcome::Keep => {
                client
                    .batch_execute(&self.footer)
                    .await
                    .context("moving the target's sequences past the copied rows")?;
                client
                    .batch_execute("COMMIT")
                    .await
                    .context("committing to the target")?;
            }
            Outcome::Discard => {
                client.batch_execute("ROLLBACK").await.ok();
            }
        }
        Ok(())
    }
}

/// A refused INSERT statement, as its error shows it: in full with `debug`, or else its first
/// line, which names its table and columns, followed by `…` for the rows, so the error holds no
/// values.
fn refused(statement: &[u8], debug: bool) -> String {
    if debug {
        return String::from_utf8_lossy(statement).trim_end().to_owned();
    }
    let end = statement
        .iter()
        .position(|&byte| byte == b'\n')
        .unwrap_or(statement.len());
    format!("{} …", String::from_utf8_lossy(&statement[..end]))
}

#[cfg(test)]
mod tests {
    use std::{env, process};

    use flate2::read::GzDecoder;

    use super::*;

    /// Writes `statement` through a new `SqlFile` at `path` and finishes it with `outcome`.
    fn write_one(path: &Path, statement: &[u8], outcome: Outcome) -> Result<()> {
        let (file, sender) = SqlFile::create(path, Dialect::MySql, &[])?;
        sender.blocking_send(statement.to_vec())?;
        drop(sender);
        file.finish(outcome)
    }

    /// Returns what a MySQL SQL file holding `statements` looks like.
    fn mysql_file(statements: &[u8]) -> Vec<u8> {
        let footer = sql::file_footer(Dialect::MySql, &[]);
        [sql::file_header(Dialect::MySql), statements, &footer].concat()
    }

    fn temp_path(name: &str) -> PathBuf {
        env::temp_dir().join(format!("tapas-output-{name}-{}.sql", process::id()))
    }

    #[test]
    fn sql_file_is_moved_into_place_when_kept() -> Result<()> {
        let path = temp_path("keep");
        write_one(&path, b"INSERT 1;\n", Outcome::Keep)?;

        let written = fs::read(&path)?;
        fs::remove_file(&path)?;
        assert_eq!(written, mysql_file(b"INSERT 1;\n"));
        let partial = PathBuf::from(format!("{}.partial", path.display()));
        assert!(
            !partial.exists(),
            "{} should have been renamed",
            partial.display()
        );
        Ok(())
    }

    #[test]
    fn sql_file_is_gzipped_when_its_path_ends_in_gz() -> Result<()> {
        use std::io::Read;

        let path = temp_path("gzip").with_extension("sql.gz");
        write_one(&path, b"INSERT 1;\n", Outcome::Keep)?;

        let mut written = Vec::new();
        let decoded = GzDecoder::new(File::open(&path)?).read_to_end(&mut written);
        fs::remove_file(&path)?;
        decoded.context("reading the file back as gzip")?;
        assert_eq!(written, mysql_file(b"INSERT 1;\n"));
        Ok(())
    }

    #[test]
    fn sql_file_leaves_nothing_behind_when_discarded() -> Result<()> {
        let path = temp_path("discard");
        write_one(&path, b"INSERT 1;\n", Outcome::Discard)?;

        let partial = PathBuf::from(format!("{}.partial", path.display()));
        assert!(
            !path.exists() && !partial.exists(),
            "a failed run must not leave {} or its .partial",
            path.display()
        );
        Ok(())
    }

    #[test]
    fn sql_file_is_framed_for_its_dialect() -> Result<()> {
        let path = temp_path("postgres");
        let (file, sender) = SqlFile::create(&path, Dialect::Postgres, &["users"])?;
        sender.blocking_send(b"INSERT 1;\n".to_vec())?;
        drop(sender);
        file.finish(Outcome::Keep)?;

        let written = fs::read(&path)?;
        fs::remove_file(&path)?;
        let expected = [
            sql::file_header(Dialect::Postgres),
            b"INSERT 1;\n",
            &sql::file_footer(Dialect::Postgres, &["users"]),
        ]
        .concat();
        assert_eq!(written, expected);
        Ok(())
    }

    #[test]
    fn displays_targets_without_their_passwords() {
        let shown = |url: &str| Output::Database(url.to_string()).to_string();
        assert_eq!(shown("mysql://root:secret@db:3307/shop"), "db:3307/shop");
        assert_eq!(shown("postgres://root:secret@db:5433/shop"), "db:5433/shop");
        assert_eq!(
            shown("postgresql://root:secret@db/shop"),
            "db:5432/shop",
            "5432 is Postgres's default port"
        );
        assert_eq!(shown("sqlite://secret.db"), "the target database");
    }
}
