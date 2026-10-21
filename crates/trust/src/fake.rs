//! Recording [`CommandExecutor`] for argv tests. Enabled by the `fake` feature.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;

use crate::{CommandExecutor, CommandLine, CommandOutput};

type Handler = Box<dyn Fn(&CommandLine) -> io::Result<CommandOutput> + Send + Sync>;

/// Records every command and answers through a handler instead of running anything.
pub struct FakeExecutor {
    handler: Handler,
    programs: BTreeSet<String>,
    calls: Mutex<Vec<CommandLine>>,
}

impl FakeExecutor {
    /// Answers every command with `handler`.
    ///
    /// The handler runs while the command's temp files still exist, so it can read them.
    pub fn new(
        handler: impl Fn(&CommandLine) -> io::Result<CommandOutput> + Send + Sync + 'static,
    ) -> Self {
        Self {
            handler: Box::new(handler),
            programs: BTreeSet::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Makes [`CommandExecutor::find_program`] report these names as installed.
    ///
    /// Found programs resolve to their bare name so argv assertions stay readable.
    #[must_use]
    pub fn with_programs(mut self, names: &[&str]) -> Self {
        self.programs
            .extend(names.iter().map(|name| (*name).to_owned()));
        self
    }

    /// Commands run so far, in order.
    pub fn calls(&self) -> Vec<CommandLine> {
        self.lock().clone()
    }

    /// Argv of every command run so far, for compact assertions.
    pub fn argvs(&self) -> Vec<Vec<String>> {
        self.lock()
            .iter()
            .map(|command| command.argv().into_iter().map(str::to_owned).collect())
            .collect()
    }

    /// Locks the call log; a panicking handler must not hide later assertions.
    fn lock(&self) -> MutexGuard<'_, Vec<CommandLine>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl CommandExecutor for FakeExecutor {
    /// Records the command and returns the handler's answer.
    async fn run(&self, command: &CommandLine) -> io::Result<CommandOutput> {
        self.lock().push(command.clone());
        (self.handler)(command)
    }

    /// Reports only programs registered with [`FakeExecutor::with_programs`].
    fn find_program(&self, name: &str, _extra_dirs: &[PathBuf]) -> Option<PathBuf> {
        self.programs.contains(name).then(|| PathBuf::from(name))
    }
}
