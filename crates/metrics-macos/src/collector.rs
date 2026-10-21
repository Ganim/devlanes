use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, PoisonError},
    time::{Instant, SystemTime},
};

use async_trait::async_trait;
use devlanes_core::ProcessIdentity;
use devlanes_metrics::{Error, MetricsSource, Sample, SampleRequest, Subject, UnitTarget, unix_ms};

use crate::ffi::{self, ShortInfo, Timebase};

/// Previous CPU reading of one process, keyed by (pid, start token).
#[derive(Debug, Clone, Copy)]
struct CpuReading {
    cpu_ns: u64,
    at: Instant,
}

#[derive(Debug)]
struct State {
    timebase: Timebase,
    rusage_flavor: ffi::RusageFlavor,
    previous: HashMap<(u32, u64), CpuReading>,
}

/// Samples unit process trees with libproc.
///
/// Membership per unit is: members of the target's sessions, tracked processes
/// whose start time still matches, and live descendants (by parent PID) of either,
/// which catches children forked since the daemon's last descendant snapshot.
/// Only processes owned by the daemon's uid are considered. A process claimed by
/// several targets counts for the first one.
#[derive(Debug, Clone)]
pub struct MacosSource {
    state: Arc<Mutex<State>>,
}

impl MacosSource {
    /// Creates a collector, reading the mach timebase once.
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                timebase: Timebase::read()?,
                rusage_flavor: ffi::RusageFlavor::V6,
                previous: HashMap::new(),
            })),
        })
    }

    /// Samples synchronously. Blocks for the process-table scan (about a
    /// millisecond for a few hundred PIDs); async callers use
    /// [`MetricsSource::sample`], which runs this on the blocking pool.
    pub fn sample_blocking(&self, units: &[UnitTarget]) -> Result<Vec<Sample>, Error> {
        // A poisoned lock only means an earlier sample panicked; the CPU cache is
        // still usable and is rebuilt from scratch on every call anyway.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        collect(&mut state, units)
    }
}

#[async_trait]
impl MetricsSource for MacosSource {
    /// Returns `macos`.
    fn name(&self) -> &'static str {
        "macos"
    }

    /// Samples every unit target on the blocking pool; containers are ignored.
    async fn sample(&self, request: &SampleRequest) -> Result<Vec<Sample>, Error> {
        let source = self.clone();
        let units = request.units.clone();
        tokio::task::spawn_blocking(move || source.sample_blocking(&units))
            .await
            .map_err(|error| Error::Unavailable {
                source_name: "macos",
                detail: format!("sampling task failed: {error}"),
            })?
    }
}

/// Captures the identity `devlanes-sys` would record for `pid`, using the same
/// start-time token. Returns `None` once the process is gone.
pub fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let start_time = ffi::start_token(pid)?;
    Some(ProcessIdentity {
        pid,
        sid: ffi::session(pid).unwrap_or(0),
        start_time,
    })
}

/// Resolves membership, reads rusage for members and converts CPU ticks to
/// percentages against the previous call.
fn collect(state: &mut State, units: &[UnitTarget]) -> Result<Vec<Sample>, Error> {
    if units.is_empty() {
        state.previous.clear();
        return Ok(Vec::new());
    }
    let owners = membership(units)?;
    let ts_ms = unix_ms(SystemTime::now());
    let now = Instant::now();
    let mach_now = ffi::mach_now();
    let mut previous = HashMap::with_capacity(owners.len());
    let mut samples = Vec::with_capacity(owners.len());
    for (pid, index) in owners {
        let Some(start_time) = ffi::start_token(pid) else {
            continue;
        };
        let usage = match ffi::rusage(pid, state.rusage_flavor) {
            Ok(usage) => usage,
            Err(()) => {
                // macOS 13 and older reject v6; v4 carries every field read here.
                state.rusage_flavor = ffi::RusageFlavor::V4;
                ffi::rusage(pid, state.rusage_flavor).ok().flatten()
            }
        };
        let Some(usage) = usage else {
            continue;
        };
        let cpu_ns = state
            .timebase
            .nanos(usage.user_time.saturating_add(usage.system_time));
        let key = (pid, start_time);
        let cpu_percent = match state.previous.get(&key) {
            Some(last) => percent(
                cpu_ns.saturating_sub(last.cpu_ns),
                now.duration_since(last.at).as_nanos(),
            ),
            // First sight: lifetime average, so a busy process is not reported idle
            // for a tick. Both clocks are mach ticks since boot.
            None => percent(
                cpu_ns,
                u128::from(
                    state
                        .timebase
                        .nanos(mach_now.saturating_sub(usage.proc_start_abstime)),
                ),
            ),
        };
        previous.insert(key, CpuReading { cpu_ns, at: now });
        samples.push(Sample {
            subject: Subject::Process {
                unit: units[index].unit.clone(),
                pid,
                start_time,
            },
            cpu_percent,
            footprint_bytes: usage.phys_footprint,
            rss_bytes: usage.resident_size,
            ts_ms,
        });
    }
    // Replacing the cache drops exited processes, so it never grows past the members.
    state.previous = previous;
    samples.sort_by(|a, b| a.subject.cmp(&b.subject));
    Ok(samples)
}

/// CPU time over wall time as a percentage of one core; 0 when no time elapsed.
fn percent(cpu_ns: u64, wall_ns: u128) -> f64 {
    if wall_ns == 0 {
        return 0.0;
    }
    cpu_ns as f64 / wall_ns as f64 * 100.0
}

/// Maps each member PID to the index of the target that owns it.
fn membership(units: &[UnitTarget]) -> Result<HashMap<u32, usize>, Error> {
    let uid = ffi::current_uid();
    let table: HashMap<u32, ShortInfo> = ffi::list_pids()?
        .into_iter()
        .filter_map(|pid| ffi::short_info(pid).map(|info| (pid, info)))
        .filter(|(_, info)| info.uid == uid && !info.zombie)
        .collect();

    let mut sessions: HashMap<u32, usize> = HashMap::new();
    for (index, target) in units.iter().enumerate() {
        for &sid in &target.sessions {
            // Session 0/1 belong to the kernel and launchd; never sweep those in.
            if sid > 1 {
                sessions.entry(sid).or_insert(index);
            }
        }
    }

    let mut owners: HashMap<u32, usize> = HashMap::new();
    if !sessions.is_empty() {
        for &pid in table.keys() {
            if let Some(&index) = ffi::session(pid).and_then(|sid| sessions.get(&sid)) {
                owners.insert(pid, index);
            }
        }
    }
    for (index, target) in units.iter().enumerate() {
        for identity in &target.processes {
            if owners.contains_key(&identity.pid) || !table.contains_key(&identity.pid) {
                continue;
            }
            // A different start time means the PID was recycled by an unrelated process.
            if ffi::start_token(identity.pid) == Some(identity.start_time) {
                owners.insert(identity.pid, index);
            }
        }
    }

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, info) in &table {
        children.entry(info.ppid).or_default().push(pid);
    }
    let mut queue: VecDeque<(u32, usize)> = owners.iter().map(|(&pid, &i)| (pid, i)).collect();
    while let Some((pid, index)) = queue.pop_front() {
        for &child in children.get(&pid).into_iter().flatten() {
            if let std::collections::hash_map::Entry::Vacant(entry) = owners.entry(child) {
                entry.insert(index);
                queue.push_back((child, index));
            }
        }
    }
    Ok(owners)
}
