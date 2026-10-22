//! Process-table snapshots shared by every caller in this process.
//!
//! Scans are coalesced: a caller that needs a fresh table waits for a scan that
//! started after its request, and every caller waiting at that point shares it. So
//! forty units polling during a stop cost one scan per round, not forty.

use std::{
    io,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use crate::{Error, ProcessInfo};

/// A completed scan, or its error message (shared by every waiter).
type ScanResult = Result<Arc<Vec<ProcessInfo>>, Arc<str>>;

/// Scan bookkeeping. `started` and `finished` count scans; a scan's number is the
/// value of `started` once it began.
struct State {
    started: u64,
    finished: u64,
    scanning: bool,
    /// Result of scan number `finished`, with the time it completed.
    last: Option<(Instant, ScanResult)>,
}

static STATE: Mutex<State> = Mutex::new(State {
    started: 0,
    finished: 0,
    scanning: false,
    last: None,
});
static DONE: Condvar = Condvar::new();

/// Locks the state, ignoring poisoning: a panicking scanner leaves counters intact.
fn lock() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Returns a snapshot no older than `max_age`. `Duration::ZERO` asks for a scan that
/// started after this call; concurrent callers share it.
///
/// Blocking: call from `spawn_blocking`.
pub fn shared_processes(max_age: Duration) -> Result<Arc<Vec<ProcessInfo>>, Error> {
    let mut state = lock();
    if !max_age.is_zero()
        && let Some((taken, Ok(snapshot))) = &state.last
        && taken.elapsed() <= max_age
    {
        return Ok(Arc::clone(snapshot));
    }
    // A scan already running began before this request and may miss what the
    // caller is looking for, so only scan number `wanted` or later qualifies.
    let wanted = state.started + 1;
    loop {
        if state.finished >= wanted
            && let Some((_, result)) = &state.last
        {
            return result
                .clone()
                .map_err(|message| Error::Io(io::Error::other(message.to_string())));
        }
        if !state.scanning {
            state.started += 1;
            state.scanning = true;
            let number = state.started;
            drop(state);
            let guard = ScanGuard;
            let result = crate::processes().map(Arc::new);
            std::mem::forget(guard);
            state = lock();
            state.scanning = false;
            state.finished = number;
            state.last = Some((
                Instant::now(),
                result
                    .as_ref()
                    .map(Arc::clone)
                    .map_err(|error| Arc::from(error.to_string())),
            ));
            DONE.notify_all();
            return result;
        }
        state = DONE.wait(state).unwrap_or_else(PoisonError::into_inner);
    }
}

/// Number of scans started so far, for tests that count scans.
pub fn scans_started() -> u64 {
    lock().started
}

/// Releases waiters if a scan unwinds, so they start their own instead of hanging.
struct ScanGuard;

impl Drop for ScanGuard {
    /// Only reached on unwind; the normal path forgets the guard.
    fn drop(&mut self) {
        lock().scanning = false;
        DONE.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concurrent fresh requests share scans instead of running one each.
    #[test]
    fn concurrent_fresh_requests_coalesce() {
        let before = scans_started();
        let threads: Vec<_> = (0..16)
            .map(|_| std::thread::spawn(|| shared_processes(Duration::ZERO).map(|s| s.len())))
            .collect();
        for thread in threads {
            assert!(thread.join().expect("no panic").expect("scan") > 0);
        }
        let scans = scans_started() - before;
        assert!((1..16).contains(&scans), "{scans} scans for 16 callers");
    }
}
