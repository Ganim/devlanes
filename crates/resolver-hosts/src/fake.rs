//! Recording [`Executor`] and in-memory [`FsProbe`] for tests. Enabled by the `fake`
//! feature and in this crate's own tests.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;

use crate::exec::{CommandOutput, CommandSpec, Executor, FileInfo, FileKind, FsProbe};

type Handler = Box<dyn Fn(&CommandSpec) -> io::Result<CommandOutput> + Send + Sync>;

/// Records every command and answers through a handler instead of running anything.
pub struct FakeExecutor {
    handler: Handler,
    calls: Mutex<Vec<CommandSpec>>,
}

impl FakeExecutor {
    /// Answers every command with `handler`.
    pub fn new(
        handler: impl Fn(&CommandSpec) -> io::Result<CommandOutput> + Send + Sync + 'static,
    ) -> Self {
        Self {
            handler: Box::new(handler),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Commands run so far, in order.
    pub fn calls(&self) -> Vec<CommandSpec> {
        self.lock().clone()
    }

    /// Argv of every command run so far.
    pub fn argvs(&self) -> Vec<Vec<String>> {
        self.lock()
            .iter()
            .map(|command| command.argv().into_iter().map(str::to_owned).collect())
            .collect()
    }

    /// Locks the call log; a panicking handler must not hide later assertions.
    fn lock(&self) -> MutexGuard<'_, Vec<CommandSpec>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    /// Records the command and returns the handler's answer.
    async fn run(&self, command: &CommandSpec) -> io::Result<CommandOutput> {
        self.lock().push(command.clone());
        (self.handler)(command)
    }
}

/// Paths with fixed `lstat` answers; everything else is missing.
#[derive(Debug, Default)]
pub struct FakeFs {
    entries: Mutex<BTreeMap<PathBuf, Result<FileInfo, io::ErrorKind>>>,
}

impl FakeFs {
    /// The standard root-owned layout: `/`, `/usr`, `/usr/local` and `/etc/sudoers.d`.
    pub fn standard() -> Self {
        let fs = Self::default();
        for dir in ["/", "/usr", "/usr/local", "/etc/sudoers.d"] {
            fs.set(dir, FileKind::Dir, 0, 0o755);
        }
        fs
    }

    /// Sets the answer for `path`.
    pub fn set(&self, path: impl AsRef<Path>, kind: FileKind, uid: u32, mode: u32) {
        self.lock()
            .insert(path.as_ref().to_owned(), Ok(FileInfo { kind, uid, mode }));
    }

    /// Makes `lstat(path)` fail with `kind`.
    pub fn fail(&self, path: impl AsRef<Path>, kind: io::ErrorKind) {
        self.lock().insert(path.as_ref().to_owned(), Err(kind));
    }

    /// Removes `path`.
    pub fn remove(&self, path: impl AsRef<Path>) {
        self.lock().remove(path.as_ref());
    }

    /// Locks the table.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<PathBuf, Result<FileInfo, io::ErrorKind>>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl FsProbe for FakeFs {
    /// The configured answer, or `None`.
    fn lstat(&self, path: &Path) -> io::Result<Option<FileInfo>> {
        match self.lock().get(path) {
            None => Ok(None),
            Some(Ok(info)) => Ok(Some(*info)),
            Some(Err(kind)) => Err(io::Error::from(*kind)),
        }
    }
}
