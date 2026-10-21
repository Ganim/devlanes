//! Daemon-only SQLite store. All SQLite work runs on one dedicated OS thread.

mod allocation;
mod lifecycle;
mod models;
mod queries;
mod records;
mod retention;
mod schema;

pub use lifecycle::{
    LogSegmentInput, RemoteHost, RollupRow, RunSummaryRow, STARTUP_PEAKS_PER_SERIES, SetupRecord,
    StartupPeak, TaskRun, TaskRunId, TaskRunInput,
};
pub use records::*;
pub use retention::*;

use diesel::{
    connection::{AnsiTransactionManager, SimpleConnection, TransactionManager},
    prelude::*,
    result::DatabaseErrorKind,
};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use std::{
    os::unix::ffi::OsStrExt,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};
use tokio::sync::{mpsc, oneshot};

/// Highest supported forward-only migration. Newer databases require an upgrade.
pub const SCHEMA_VERSION: u32 = 8;
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// Store failures. Diesel, connection, migration and filesystem failures keep the
/// original error as their `source`; uniqueness conflicts, unfinished runs and
/// undecodable rows have their own variants so callers can react without
/// parsing messages.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A query failed for a reason without a more specific variant.
    #[error("database error: {0}")]
    Database(#[source] diesel::result::Error),
    /// The database file could not be opened.
    #[error("database connection error: {0}")]
    Connection(#[source] diesel::ConnectionError),
    /// Reading the migration ledger or applying a migration failed.
    #[error("migration error: {0}")]
    Migration(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// A pre-Diesel development database; only a reset recovers.
    #[error(
        "STATE_LEGACY_SCHEMA: user_version schema {version}; run `devlanes daemon reset-state` before opening this database"
    )]
    LegacySchema { version: i32 },
    /// A newer devlanes migrated this database.
    #[error("database schema {found} is newer than supported schema {supported}; upgrade devlanes")]
    NewerSchema { found: u32, supported: u32 },
    /// The store thread has stopped, after [`Store::close`] or a failed recovery.
    #[error("store actor has stopped")]
    Closed,
    /// The operation panicked on the store thread. Its transaction was rolled
    /// back and the store keeps serving other calls.
    #[error("store operation panicked")]
    Panicked,
    /// The store thread could not be started.
    #[error("unable to start store thread: {0}")]
    Thread(#[from] std::io::Error),
    /// Resolving the database path or measuring its files failed.
    #[error("database file metadata error: {0}")]
    Filesystem(#[source] std::io::Error),
    /// The addressed record does not exist (or is not active, as named).
    #[error("record not found: {0}")]
    NotFound(&'static str),
    /// The caller supplied a value the store refuses to write.
    #[error("invalid state input: {0}")]
    Invalid(String),
    /// A uniqueness constraint rejected the write, for example a second
    /// unfinished run of the same lane and unit.
    #[error("conflicts with an existing record ({constraint})")]
    Conflict { constraint: String },
    /// The operation requires every unit and task run in its scope to be finished.
    #[error("{units} unit run(s) and {tasks} task run(s) are still unfinished")]
    ActiveRuns { units: u64, tasks: u64 },
    /// A stored value could not be decoded into its record type.
    #[error("corrupt stored value: {0}")]
    Corrupt(String),
    /// Port allocation rules rejected the request.
    #[error(transparent)]
    Allocation(#[from] devlanes_core::ports::AllocationError),
}

impl From<diesel::result::Error> for Error {
    /// Turns uniqueness violations into [`Error::Conflict`] and keeps every other
    /// Diesel error as the source of [`Error::Database`].
    fn from(error: diesel::result::Error) -> Self {
        match error {
            diesel::result::Error::DatabaseError(DatabaseErrorKind::UniqueViolation, info) => {
                // SQLite reports no constraint name, only the columns or index in the message.
                let constraint = info.constraint_name().map_or_else(
                    || {
                        let message = info.message();
                        message
                            .strip_prefix("UNIQUE constraint failed: ")
                            .unwrap_or(message)
                            .to_owned()
                    },
                    str::to_owned,
                );
                Self::Conflict { constraint }
            }
            other => Self::Database(other),
        }
    }
}

type Job = Box<dyn FnOnce(&mut SqliteConnection) + Send>;

/// Work for the store thread. `Close` ends the thread after the jobs queued before it.
enum Message {
    Run(Job),
    Close,
}

/// Cloneable async handle; the last drop closes the actor after queued work drains.
/// Cancelling a submitted future does not roll back an operation already queued.
#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Message>,
    database_path: PathBuf,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Store {
    /// Opens and migrates a database on its dedicated thread. Parent directories
    /// must exist. Only daemon composition code should call this API.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        let (sender, receiver) = mpsc::channel::<Message>(128);
        let (ready, result) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("devlanes-state".into())
            .spawn(move || match open_connection(&path) {
                Ok((connection, path)) => {
                    if ready.send(Ok(path.clone())).is_ok() {
                        serve(connection, &path, receiver);
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })?;
        let database_path = result.await.map_err(|_| Error::Closed)??;
        Ok(Self {
            sender,
            database_path,
            thread: Arc::new(Mutex::new(Some(thread))),
        })
    }

    /// Runs the jobs already queued, closes the connection (checkpointing the WAL)
    /// and joins the store thread. Every clone fails with [`Error::Closed`]
    /// afterwards. Calling it again returns `Ok` immediately.
    pub async fn close(&self) -> Result<(), Error> {
        let thread = self.thread.lock().map_err(|_| Error::Panicked)?.take();
        let Some(thread) = thread else {
            return Ok(());
        };
        // A send failure means the thread already ended; joining still reaps it.
        let _ = self.sender.send(Message::Close).await;
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .map_err(|_| Error::Closed)?
            .map_err(|_| Error::Panicked)
    }

    /// Queues typed work without exposing a connection or blocking executor threads.
    async fn call<T, F>(&self, operation: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce(&mut SqliteConnection) -> Result<T, Error> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(Message::Run(Box::new(move |connection| {
                let _ = sender.send(operation(connection));
            })))
            .await
            .map_err(|_| Error::Closed)?;
        // The reply sender only drops unanswered when the job unwound, unless
        // the thread itself is gone.
        receiver.await.map_err(|_| {
            if self.sender.is_closed() {
                Error::Closed
            } else {
                Error::Panicked
            }
        })?
    }

    /// Reports the applied migration for daemon doctor/status responses.
    pub async fn schema_version(&self) -> Result<u32, Error> {
        self.call(applied_version).await
    }
}

/// The store thread's loop. A panicking job must not take the daemon's only
/// database connection with it: the panic is caught, an open transaction is
/// rolled back, and only a connection that cannot be recovered is reopened.
fn serve(mut connection: SqliteConnection, path: &Path, mut receiver: mpsc::Receiver<Message>) {
    while let Some(message) = receiver.blocking_recv() {
        let Message::Run(job) = message else {
            break;
        };
        if std::panic::catch_unwind(AssertUnwindSafe(|| job(&mut connection))).is_ok()
            || roll_back_open_transactions(&mut connection)
        {
            continue;
        }
        // An in-memory database cannot be reopened with its contents.
        if path == Path::new(":memory:") {
            break;
        }
        match open_connection(path) {
            Ok((fresh, _)) => connection = fresh,
            Err(_) => break,
        }
    }
}

/// Unwinds Diesel's transaction bookkeeping after a panic inside a transaction.
/// Returns false when the connection is left in an unknown state.
fn roll_back_open_transactions(connection: &mut SqliteConnection) -> bool {
    loop {
        match AnsiTransactionManager::transaction_manager_status_mut(connection).transaction_depth()
        {
            Ok(None) => return true,
            Ok(Some(_)) => {
                if AnsiTransactionManager::rollback_transaction(connection).is_err() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

/// Encodes Unix paths as SQLite file URIs to preserve non-UTF-8 names and prevent
/// filename punctuation from becoming URI options. Runs on the database thread.
fn open_connection(path: &Path) -> Result<(SqliteConnection, PathBuf), Error> {
    let (path, url) = if path == Path::new(":memory:") {
        (path.to_path_buf(), ":memory:".to_owned())
    } else {
        let path = std::path::absolute(path).map_err(Error::Filesystem)?;
        let bytes = path.as_os_str().as_bytes();
        if bytes.contains(&0) {
            return Err(Error::Invalid("database path contains NUL".into()));
        }
        let url = format!(
            "file:{}",
            percent_encoding::percent_encode(bytes, percent_encoding::NON_ALPHANUMERIC)
        );
        (path, url)
    };
    let mut connection = SqliteConnection::establish(&url).map_err(Error::Connection)?;
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
        .map_err(Error::Migration)?;
    Ok(())
}

/// Checks the Diesel migration ledger rather than SQLite's legacy user_version.
fn applied_version(connection: &mut SqliteConnection) -> Result<u32, Error> {
    let versions = connection.applied_migrations().map_err(Error::Migration)?;
    let mut latest = 0;
    for version in versions {
        let found: u32 = version.to_string().parse().map_err(|_| {
            Error::Migration(format!("unknown migration {version}; upgrade devlanes").into())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A job that panics inside a transaction is reported, rolled back, and the
    /// same in-memory database keeps serving later calls and transactions.
    #[tokio::test]
    async fn a_panicking_job_rolls_back_and_the_store_keeps_serving() {
        let store = Store::open(":memory:").await.expect("store");
        let panicked = store
            .call(|c| {
                c.immediate_transaction::<(), Error, _>(|c| {
                    c.batch_execute(
                        "INSERT INTO remote_hosts (name, transport, address) VALUES ('box', 'ssh', 'example.invalid')",
                    )?;
                    std::panic::panic_any("job failure under test")
                })
            })
            .await;
        assert!(matches!(panicked, Err(Error::Panicked)), "{panicked:?}");
        assert!(store.remote_hosts().await.expect("hosts").is_empty());
        store
            .call(|c| {
                c.immediate_transaction(|c| {
                    c.batch_execute(
                        "INSERT INTO remote_hosts (name, transport, address) VALUES ('box', 'ssh', 'example.invalid')",
                    )?;
                    Ok::<_, Error>(())
                })
            })
            .await
            .expect("transactions work after the panic");
        assert_eq!(store.remote_hosts().await.expect("hosts").len(), 1);
    }

    /// `close` drains queued work, joins the thread and fails later calls.
    #[tokio::test]
    async fn close_joins_the_thread_and_later_calls_fail() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path().join("state.db"))
            .await
            .expect("store");
        let clone = store.clone();
        store.close().await.expect("close");
        assert!(matches!(clone.projects().await, Err(Error::Closed)));
        store.close().await.expect("second close");
    }
}
