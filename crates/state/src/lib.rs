//! Daemon-only SQLite store. All SQLite work runs on one dedicated OS thread.

mod allocation;
mod lifecycle;
mod queries;
mod records;

pub use lifecycle::{
    LogSegmentInput, Pruned, RemoteHost, RollupRow, RunSummaryRow, SetupRecord, TaskRun, TaskRunId,
    TaskRunInput,
};
pub use records::*;

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};
use std::{path::Path, time::Duration};
use tokio::sync::{mpsc, oneshot};

/// Highest supported forward-only migration. Newer databases require an upgrade.
pub const SCHEMA_VERSION: u32 = 5;

/// Store failures preserve database and allocation errors for structured reporting.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("database schema {found} is newer than supported schema {supported}; upgrade devlanes")]
    NewerSchema { found: u32, supported: u32 },
    #[error("store actor has stopped")]
    Closed,
    #[error("unable to start store thread: {0}")]
    Thread(#[from] std::io::Error),
    #[error("record not found: {0}")]
    NotFound(&'static str),
    #[error("invalid state input: {0}")]
    Invalid(String),
    #[error(transparent)]
    Allocation(#[from] devlanes_core::ports::AllocationError),
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// Cloneable async handle; the last drop closes the actor after queued work drains.
/// Cancelling a submitted future does not roll back an operation already queued.
#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
}

impl Store {
    /// Opens and migrates a database on its dedicated thread. Parent directories
    /// must exist. Only daemon composition code should call this API.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        let (sender, mut receiver) = mpsc::channel::<Job>(128);
        let (ready, result) = oneshot::channel();
        std::thread::Builder::new()
            .name("devlanes-state".into())
            .spawn(move || {
                let connection =
                    Connection::open(path)
                        .map_err(Error::from)
                        .and_then(|mut connection| {
                            configure(&mut connection)?;
                            Ok(connection)
                        });
                match connection {
                    Ok(mut connection) => {
                        if ready.send(Ok(())).is_err() {
                            return;
                        }
                        while let Some(job) = receiver.blocking_recv() {
                            job(&mut connection);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })?;
        result.await.map_err(|_| Error::Closed)??;
        Ok(Self { sender })
    }

    /// Queues typed work without exposing a connection or blocking executor threads.
    async fn call<T, F>(&self, operation: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, Error> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(Box::new(move |connection| {
                let _ = sender.send(operation(connection));
            }))
            .await
            .map_err(|_| Error::Closed)?;
        receiver.await.map_err(|_| Error::Closed)?
    }

    /// Reports the applied migration for daemon doctor/status responses.
    pub async fn schema_version(&self) -> Result<u32, Error> {
        self.call(|connection| {
            Ok(connection.pragma_query_value(None, "user_version", |row| row.get(0))?)
        })
        .await
    }
}

/// Configures durability and refuses unknown future schemas before applying SQL.
fn configure(connection: &mut Connection) -> Result<(), Error> {
    connection.busy_timeout(Duration::from_secs(5))?;
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(Error::NewerSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", true)?;
    migrations().to_latest(connection)?;
    Ok(())
}

/// Keeps migration numbering local and append-only.
fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("../migrations/001_registry.sql")),
        M::up(include_str!("../migrations/002_unit_runs.sql")),
        M::up(include_str!("../migrations/003_lifecycle.sql")),
        M::up(include_str!("../migrations/004_history.sql")),
        M::up(include_str!("../migrations/005_remote_hosts.sql")),
    ])
}
