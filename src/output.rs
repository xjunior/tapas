use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

const HEADER: &[u8] = b"SET NAMES utf8mb4;\nSET FOREIGN_KEY_CHECKS=0;\n\n";
const FOOTER: &[u8] = b"\nSET FOREIGN_KEY_CHECKS=1;\n";

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
