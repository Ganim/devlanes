//! Daemon-only SQLite store. All SQLite work runs on one dedicated OS thread.

mod allocation;
mod lifecycle;
mod models;
mod queries;
mod records;
mod retention;
mod schema;

pub use lifecycle::{
    LogSegmentInput, Pruned, RemoteHost, RollupRow, RunSummaryRow, SetupRecord, TaskRun, TaskRunId,
    TaskRunInput,
};
pub use records::*;
pub use retention::*;

use diesel::{connection::SimpleConnection, prelude::*};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use std::{path::{Path, PathBuf}, os::unix::ffi::OsStrExt};
use tokio::sync::{mpsc, oneshot};

/// Highest supported forward-only migration. Newer databases require an upgrade.
pub const SCHEMA_VERSION: u32 = 5;
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// Store failures preserve database and allocation errors for structured reporting.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(String),
    #[error("database connection error: {0}")]
    Connection(String),
    #[error("migration error: {0}")]
    Migration(String),
    #[error(
        "STATE_LEGACY_SCHEMA: user_version schema {version}; run `devlanes daemon reset-state` before opening this database"
    )]
    LegacySchema { version: i32 },
    #[error("database schema {found} is newer than supported schema {supported}; upgrade devlanes")]
    NewerSchema { found: u32, supported: u32 },
    #[error("store actor has stopped")]
    Closed,
    #[error("unable to start store thread: {0}")]
    Thread(#[from] std::io::Error),
    #[error("database file metadata error: {0}")]
    Filesystem(String),
    #[error("record not found: {0}")]
    NotFound(&'static str),
    #[error("invalid state input: {0}")]
    Invalid(String),
    #[error(transparent)]
    Allocation(#[from] devlanes_core::ports::AllocationError),
}

impl From<diesel::result::Error> for Error {
    /// Keeps Diesel-specific error types behind the persistence boundary.
    fn from(error: diesel::result::Error) -> Self {
        Self::Database(error.to_string())
    }
}

type Job = Box<dyn FnOnce(&mut SqliteConnection) + Send>;

/// Cloneable async handle; the last drop closes the actor after queued work drains.
/// Cancelling a submitted future does not roll back an operation already queued.
#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
    database_path: PathBuf,
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
                let connection = open_connection(&path);
                match connection {
                    Ok((mut connection, path)) => {
                        if ready.send(Ok(path)).is_err() {
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
        let database_path = result.await.map_err(|_| Error::Closed)??;
        Ok(Self { sender, database_path })
    }

    /// Queues typed work without exposing a connection or blocking executor threads.
    async fn call<T, F>(&self, operation: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&mut SqliteConnection) -> Result<T, Error> + Send + 'static,
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
        self.call(applied_version).await
    }
}

/// Encodes Unix paths as SQLite file URIs to preserve non-UTF-8 names and prevent
/// filename punctuation from becoming URI options. Runs on the database thread.
fn open_connection(path: &Path) -> Result<(SqliteConnection, PathBuf), Error> {
    let (path, url) = if path == Path::new(":memory:") {
        (path.to_path_buf(), ":memory:".to_owned())
    } else {
        let path = std::path::absolute(path).map_err(|error| Error::Filesystem(error.to_string()))?;
        let bytes = path.as_os_str().as_bytes();
        if bytes.contains(&0) { return Err(Error::Invalid("database path contains NUL".into())); }
        let url = format!("file:{}", percent_encoding::percent_encode(bytes, percent_encoding::NON_ALPHANUMERIC));
        (path, url)
    };
    let mut connection = SqliteConnection::establish(&url).map_err(|error| Error::Connection(error.to_string()))?;
    configure(&mut connection)?;
    Ok((connection, path))
}

/// Configures durability and refuses unknown future schemas before applying SQL.
fn configure(connection: &mut SqliteConnection) -> Result<(), Error> {
    #[derive(diesel::QueryableByName)]
    struct LegacyVersion {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        user_version: i32,
    }
    connection.batch_execute("PRAGMA busy_timeout = 5000;")?;
    let version = diesel::sql_query("PRAGMA user_version")
        .get_result::<LegacyVersion>(connection)?
        .user_version;
    if version != 0 {
        return Err(Error::LegacySchema { version });
    }
    connection.batch_execute("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    applied_version(connection)?;
    connection
        .run_pending_migrations(MIGRATIONS)
        .map_err(|error| Error::Migration(error.to_string()))?;
    Ok(())
}

/// Checks the Diesel migration ledger rather than SQLite's legacy user_version.
fn applied_version(connection: &mut SqliteConnection) -> Result<u32, Error> {
    let versions = connection
        .applied_migrations()
        .map_err(|error| Error::Migration(error.to_string()))?;
    let mut latest = 0;
    for version in versions {
        let found: u32 = version.to_string().parse().map_err(|_| {
            Error::Migration(format!("unknown migration {version}; upgrade devlanes"))
        })?;
        if found > SCHEMA_VERSION {
            return Err(Error::NewerSchema {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        latest = latest.max(found);
    }
    Ok(latest)
}
