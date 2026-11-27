//! Where the sample goes: a SQL file, or straight into a target database.

use std::fmt::{self, Display, Formatter};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use tokio::sync::mpsc;
use tokio::task::JoinHandle as TaskHandle;

const HEADER: &[u8] = b"SET NAMES utf8mb4;\nSET FOREIGN_KEY_CHECKS=0;\n\n";
const FOOTER: &[u8] = b"\nSET FOREIGN_KEY_CHECKS=1;\n";

/// Where the sample goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// A SQL file of INSERT statements.
    File(PathBuf),
    /// A MySQL database, as mysql://user:password@host:port/database.
    Database(String),
}

impl Display for Output {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Output::File(path) => write!(f, "{}", path.display()),
            Output::Database(url) => match Opts::from_url(url) {
                Ok(connection_options) => write!(
                    f,
                    "{}:{}/{}",
                    connection_options.ip_or_hostname(),
                    connection_options.tcp_port(),
                    connection_options.db_name().unwrap_or_default()
                ),
                Err(_) => write!(f, "the target database"),
            },
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
    /// Writes to a target database.
    Database(Target),
}

impl Sink {
    /// Opens `output` for writing. Returns the sink and the sender to send statements through.
    pub async fn open(output: &Output) -> Result<(Self, mpsc::Sender<Vec<u8>>)> {
        Ok(match output {
            Output::File(path) => {
                let (file, sender) = SqlFile::create(path)?;
                (Sink::File(file), sender)
            }
            Output::Database(url) => {
                let (target, sender) = Target::connect(url).await?;
                (Sink::Database(target), sender)
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
            Sink::Database(target) => target.finish(outcome).await,
        }
    }
}

/// Writes INSERT statements to `<path>.partial` on its own thread. The file is only moved to
/// `path` by `finish`, so a failed run never leaves something that looks like a full sample.
#[must_use = "call `finish` to wait for the statements and keep or discard them"]
pub struct SqlFile {
    path: PathBuf,
    partial: PathBuf,
    thread: JoinHandle<Result<()>>,
}

impl SqlFile {
    /// Creates `<path>.partial` and starts the thread that writes to it. Returns the file and the
    /// sender to send statements through.
    pub fn create(path: &Path) -> Result<(Self, mpsc::Sender<Vec<u8>>)> {
        use std::io::Write;

        let partial = PathBuf::from(format!("{}.partial", path.display()));
        let file =
            File::create(&partial).with_context(|| format!("creating {}", partial.display()))?;
        let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(256);

        let thread = thread::spawn(move || -> Result<()> {
            let mut out = BufWriter::with_capacity(1 << 20, file);
            out.write_all(HEADER)?;
            while let Some(statement) = receiver.blocking_recv() {
                out.write_all(&statement)?;
            }
            out.write_all(FOOTER)?;
            out.flush()?;
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

/// Runs INSERT statements against a target database over one connection, inside one
/// transaction. `finish` commits it only once the whole sample worked, so a failed run leaves
/// the target as it was.
#[must_use = "call `finish` to wait for the statements and commit or roll them back"]
pub struct Target {
    task: TaskHandle<Result<Conn>>,
}

impl Target {
    /// Connects to the database at `url`, starts a transaction, and starts the task that runs
    /// the statements. Returns the target and the sender to send statements through.
    pub async fn connect(url: &str) -> Result<(Self, mpsc::Sender<Vec<u8>>)> {
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
                connection.query_drop(statement.as_slice()).await?;
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

#[cfg(test)]
mod tests {
    use std::{env, process};

    use super::*;

    /// Writes `statement` through a new `SqlFile` at `path` and finishes it with `outcome`.
    fn write_one(path: &Path, statement: &[u8], outcome: Outcome) -> Result<()> {
        let (file, sender) = SqlFile::create(path)?;
        sender.blocking_send(statement.to_vec())?;
        drop(sender);
        file.finish(outcome)
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
        assert_eq!(written, [HEADER, b"INSERT 1;\n", FOOTER].concat());
        let partial = PathBuf::from(format!("{}.partial", path.display()));
        assert!(
            !partial.exists(),
            "{} should have been renamed",
            partial.display()
        );
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
}
