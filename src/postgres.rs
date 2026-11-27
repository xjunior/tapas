//! Connections to Postgres: TLS when the server offers it, and a pool of reused connections.

use std::ops::Deref;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio_postgres::config::Host;
use tokio_postgres::{Client, Config};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Parses a `postgres://` URL.
pub fn config(url: &str) -> Result<Config> {
    Ok(Config::from_str(url)?)
}

/// Returns where `config` connects, as `host:port/database`, with Postgres's defaults filled
/// in: port 5432, and a database named after the user.
#[must_use]
pub fn location(config: &Config) -> String {
    let host = match config.get_hosts().first() {
        Some(Host::Tcp(name)) => name.clone(),
        #[cfg(unix)]
        Some(Host::Unix(path)) => path.display().to_string(),
        None => "localhost".to_string(),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let database = config
        .get_dbname()
        .or(config.get_user())
        .unwrap_or_default();
    format!("{host}:{port}/{database}")
}

/// Connects with `config`, over TLS if the server offers it, unless its `sslmode` says
/// otherwise. The connection closes once the client is dropped.
pub async fn connect(config: &Config) -> Result<Client> {
    let (client, connection) = config
        .connect(MakeRustlsConnect::with_webpki_roots())
        .await?;
    // Its errors also fail the client's next query, which reports them.
    tokio::spawn(connection);
    Ok(client)
}

/// Connections to one database, opened as they're needed, at most a set number at once, and
/// reused. Clones share the connections.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    idle: Mutex<Vec<Client>>,
    permits: Semaphore,
}

impl Pool {
    /// Prepares at most `size` connections with `config`. Nothing connects until `get`.
    #[must_use]
    pub fn new(config: Config, size: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                idle: Mutex::new(Vec::new()),
                permits: Semaphore::new(size.max(1)),
            }),
        }
    }

    /// Returns an idle connection, or a new one once fewer than the limit are in use.
    pub async fn get(&self) -> Result<Connection<'_>> {
        let permit = self
            .inner
            .permits
            .acquire()
            .await
            .expect("the pool never closes its semaphore");
        let idle = lock(&self.inner.idle).pop();
        let client = match idle {
            Some(client) if !client.is_closed() => client,
            Some(_) | None => connect(&self.inner.config)
                .await
                .context("connecting to the source")?,
        };
        Ok(Connection {
            pool: self,
            client: Some(client),
            _permit: permit,
        })
    }

    /// Closes the idle connections.
    pub fn close(&self) {
        lock(&self.inner.idle).clear();
    }
}

fn lock(idle: &Mutex<Vec<Client>>) -> MutexGuard<'_, Vec<Client>> {
    idle.lock()
        .expect("no thread panics while it holds the idle connections")
}

/// A connection from a `Pool`, which goes back to it when dropped.
pub struct Connection<'pool> {
    pool: &'pool Pool,
    // Only `None` while dropped.
    client: Option<Client>,
    _permit: SemaphorePermit<'pool>,
}

impl Deref for Connection<'_> {
    type Target = Client;

    fn deref(&self) -> &Client {
        self.client
            .as_ref()
            .expect("a connection has its client until it's dropped")
    }
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.client.take()
            && !client.is_closed()
        {
            lock(&self.pool.inner.idle).push(client);
        }
    }
}
