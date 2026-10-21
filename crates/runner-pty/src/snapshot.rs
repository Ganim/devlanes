//! Process-table snapshots shared by all units in this process.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use devlanes_sys::ProcessInfo;

/// Last scan and when it finished. Scanning holds the lock so concurrent units reuse
/// one scan instead of each walking the whole table.
static CACHE: Mutex<Option<(Instant, Arc<Vec<ProcessInfo>>)>> = Mutex::new(None);

/// Returns a snapshot no older than `max_age`; `Duration::ZERO` forces a fresh scan.
///
/// Blocking: call from `spawn_blocking`.
pub(crate) fn processes(max_age: Duration) -> Result<Arc<Vec<ProcessInfo>>, devlanes_sys::Error> {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if !max_age.is_zero()
        && let Some((taken, snapshot)) = cache.as_ref()
        && taken.elapsed() <= max_age
    {
        return Ok(Arc::clone(snapshot));
    }
    let snapshot = Arc::new(devlanes_sys::processes()?);
    *cache = Some((Instant::now(), Arc::clone(&snapshot)));
    Ok(snapshot)
}
