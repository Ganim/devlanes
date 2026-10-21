//! Open-file limit handling: the daemon raises its own, children get the original.

use std::sync::OnceLock;

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

use crate::{Error, os};

/// `RLIMIT_NOFILE` values; `None` means unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NofileLimit {
    pub soft: Option<u64>,
    pub hard: Option<u64>,
}

impl From<Rlimit> for NofileLimit {
    fn from(limit: Rlimit) -> Self {
        Self {
            soft: limit.current,
            hard: limit.maximum,
        }
    }
}

/// The limit observed before the first raise; set at most once per process.
static ORIGINAL: OnceLock<NofileLimit> = OnceLock::new();

/// Raises this process's soft `RLIMIT_NOFILE` to the platform maximum and returns it.
///
/// Call early in the daemon: each PTY costs descriptors and macOS defaults to 256.
/// The first call records the original limit for [`child_nofile_limit`]. On macOS
/// the soft limit is capped by `kern.maxfilesperproc`, since `setrlimit` rejects more.
pub fn raise_nofile_limit() -> Result<NofileLimit, Error> {
    let current = getrlimit(Resource::Nofile);
    ORIGINAL.get_or_init(|| current.into());
    let target = os::max_nofile(current.maximum)?;
    if !current.current.is_some_and(|soft| soft < target) {
        return Ok(current.into());
    }
    let raised = Rlimit {
        current: Some(target),
        maximum: current.maximum,
    };
    setrlimit(Resource::Nofile, raised).map_err(|e| Error::Io(e.into()))?;
    Ok(raised.into())
}

/// Returns the limit children should get, or `None` when the daemon never raised its own.
///
/// Many tools (select-based code, old JVMs) misbehave with huge limits, so managed
/// processes see the soft limit the daemon was started with.
pub fn child_nofile_limit() -> Option<NofileLimit> {
    ORIGINAL.get().copied()
}
