use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use tokio::sync::mpsc;
use tokio::task::JoinHandle as TaskHandle;

const HEADER: &[u8] = b"SET NAMES utf8mb4;\nSET FOREIGN_KEY_CHECKS=0;\n\n";
const FOOTER: &[u8] = b"\nSET FOREIGN_KEY_CHECKS=1;\n";

/// Where the sample goes.
#[derive(Debug, Clone)]
pub enum Output {
    /// A SQL file of INSERT statements.
    File(PathBuf),
    /// A MySQL database, as mysql://user:password@host:port/database.
    Database(String),
}

impl std::fmt::Display for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Output::File(path) => write!(f, "{}", path.display()),
            Output::Database(url) => match Opts::from_url(url) {
                Ok(opts) => write!(
                    f,
                    "{}:{}/{}",
                    opts.ip_or_hostname(),
                    opts.tcp_port(),
                    opts.db_name().unwrap_or_default()
                ),
                Err(_) => write!(f, "the target database"),
            },
        }
    }
}

/// Receives INSERT statements and writes them to an `Output`.
pub enum Sink {
    File(SqlFile),
    Database(Target),
}

impl Sink {
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

    /// Waits for every statement to be written, then keeps them only if `succeeded`. Drop all
    /// senders first.
    pub async fn finish(self, succeeded: bool) -> Result<()> {
        match self {
            Sink::File(file) => file.finish(succeeded),
            Sink::Database(target) => target.finish(succeeded).await,
        }
    }
}

/// Writes INSERT statements to `<path>.partial` on its own thread. The file is only moved to
/// `path` by `finish`, so a failed run never leaves something that looks like a full sample.
pub struct SqlFile {
    path: PathBuf,
    partial: PathBuf,
    thread: JoinHandle<Result<()>>,
}

impl SqlFile {
    pub fn create(path: &Path) -> Result<(Self, mpsc::Sender<Vec<u8>>)> {
        let partial = PathBuf::from(format!("{}.partial", path.display()));
        let file =
            File::create(&partial).with_context(|| format!("creating {}", partial.display()))?;
        let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(256);

        let thread = std::thread::spawn(move || -> Result<()> {
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

    /// Waits for every statement to be written. Drop all senders first.
    pub fn finish(self, succeeded: bool) -> Result<()> {
        let written = self.thread.join().expect("SQL writer thread panicked");
        if succeeded && written.is_ok() {
            std::fs::rename(&self.partial, &self.path)
                .with_context(|| format!("moving output to {}", self.path.display()))?;
        } else {
            let _ = std::fs::remove_file(&self.partial);
        }
        written.context("writing the SQL file")
    }
}

/// Runs INSERT statements against a target database over one connection, inside one
/// transaction. `finish` commits it only once the whole sample worked, so a failed run leaves
/// the target as it was.
pub struct Target {
    task: TaskHandle<Result<Conn>>,
}

impl Target {
    pub async fn connect(url: &str) -> Result<(Self, mpsc::Sender<Vec<u8>>)> {
        let opts = Opts::from_url(url).context("parsing the target URL")?;
        let mut conn = Conn::new(opts).await.context("connecting to the target")?;
        conn.query_drop("SET NAMES utf8mb4")
            .await
            .context("setting up the target")?;
        conn.query_drop("SET SESSION FOREIGN_KEY_CHECKS = 0")
            .await
            .context("setting up the target")?;
        conn.query_drop("START TRANSACTION")
            .await
            .context("starting a transaction on the target")?;

        let (sender, mut receiver) = mpsc::channel::<Vec<u8>>(256);
        let task = tokio::spawn(async move {
            while let Some(statement) = receiver.recv().await {
                conn.query_drop(statement.as_slice()).await?;
            }
            Ok(conn)
        });
        Ok((Self { task }, sender))
    }

    /// Waits for every statement to run, then commits, or rolls back unless `succeeded`. Drop
    /// all senders first.
    pub async fn finish(self, succeeded: bool) -> Result<()> {
        let mut conn = self
            .task
            .await
            .expect("target writer task panicked")
            .context("writing to the target")?;
        if succeeded {
            conn.query_drop("COMMIT")
                .await
                .context("committing to the target")?;
        } else {
            conn.query_drop("ROLLBACK").await.ok();
        }
        conn.disconnect().await.ok();
        Ok(())
    }
}
