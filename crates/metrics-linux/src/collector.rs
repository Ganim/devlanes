use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    fs, io,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant, SystemTime},
};

use async_trait::async_trait;
use devlanes_core::ProcessIdentity;
use devlanes_metrics::{Error, MetricsSource, Sample, SampleRequest, Subject, UnitTarget, unix_ms};

use crate::procfs::{self, ProcStat};

/// How often PSS is re-read per process; `smaps_rollup` costs a page-table walk.
pub const DEFAULT_PSS_INTERVAL: Duration = Duration::from_secs(10);

/// Previous CPU reading of one process, keyed by (pid, starttime).
#[derive(Debug, Clone, Copy)]
struct CpuReading {
    ticks: u64,
    at: Instant,
}

#[derive(Debug)]
struct State {
    ticks_per_second: u64,
    page_size: u64,
    pss_interval: Duration,
    previous: HashMap<(u32, u64), CpuReading>,
    pss: HashMap<(u32, u64), (u64, Instant)>,
}

/// Samples unit process trees from procfs.
///
/// Membership per unit is: members of the target's sessions, tracked processes
/// whose `starttime` still matches, and live descendants (by parent PID) of
/// either. A process claimed by several targets counts for the first one. The
/// request's daemon process is read by PID alone, without its descendants. It
/// never counts for a unit, and the descendant walk does not pass through it.
#[derive(Debug, Clone)]
pub struct LinuxSource {
    state: Arc<Mutex<State>>,
}

impl LinuxSource {
    /// Creates a collector with [`DEFAULT_PSS_INTERVAL`].
    pub fn new() -> Self {
        Self::with_pss_interval(DEFAULT_PSS_INTERVAL)
    }

    /// Creates a collector that re-reads PSS at most every `pss_interval`;
    /// `Duration::ZERO` reads it on every sample.
    pub fn with_pss_interval(pss_interval: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                ticks_per_second: rustix::param::clock_ticks_per_second(),
                page_size: rustix::param::page_size() as u64,
                pss_interval,
                previous: HashMap::new(),
                pss: HashMap::new(),
            })),
        }
    }

    /// Samples synchronously: every unit's members as [`Subject::Process`] and,
    /// when given, the `daemon` process alone as [`Subject::Daemon`]. Reads
    /// `/proc` for every visible process when there are units. Async callers use
    /// [`MetricsSource::sample`], which runs this on the blocking pool.
    pub fn sample_blocking(
        &self,
        units: &[UnitTarget],
        daemon: Option<ProcessIdentity>,
    ) -> Result<Vec<Sample>, Error> {
        // A poisoned lock only means an earlier sample panicked; the caches are
        // advisory and rebuilt on every call.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        collect(&mut state, units, daemon)
    }
}

impl Default for LinuxSource {
    /// Same as [`LinuxSource::new`].
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MetricsSource for LinuxSource {
    /// Returns `linux`.
    fn name(&self) -> &'static str {
        "linux"
    }

    /// Samples every unit target and the daemon on the blocking pool; containers
    /// are ignored.
    async fn sample(&self, request: &SampleRequest) -> Result<Vec<Sample>, Error> {
        let source = self.clone();
        let units = request.units.clone();
        let daemon = request.daemon;
        tokio::task::spawn_blocking(move || source.sample_blocking(&units, daemon))
            .await
            .map_err(|error| Error::Unavailable {
                source_name: "linux",
                detail: format!("sampling task failed: {error}"),
            })?
    }
}

/// Captures the identity `devlanes-sys` would record for `pid` (start token =
/// `starttime` ticks after boot). Returns `None` once the process is gone.
pub fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let stat = read_stat(pid).ok().flatten()?;
    Some(ProcessIdentity {
        pid,
        sid: stat.session,
        start_time: stat.starttime,
    })
}

/// Reads a procfs file, mapping "process gone" to `None`.
fn read_proc(pid: u32, file: &str) -> io::Result<Option<String>> {
    match fs::read_to_string(format!("/proc/{pid}/{file}")) {
        Ok(text) => Ok(Some(text)),
        // ESRCH shows up when the task exits between open and read.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(3) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Reads and parses one stat file; vanished processes are `None`.
fn read_stat(pid: u32) -> Result<Option<ProcStat>, Error> {
    match read_proc(pid, "stat")? {
        Some(text) => procfs::parse_stat(&text).map(Some),
        None => Ok(None),
    }
}

/// Lists live (non-zombie) processes. Unreadable or vanished entries are skipped.
fn process_table() -> Result<HashMap<u32, ProcStat>, Error> {
    let mut table = HashMap::new();
    for entry in fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(Some(stat)) = read_stat(pid)
            && !stat.is_exited()
        {
            table.insert(pid, stat);
        }
    }
    Ok(table)
}

/// Maps each member PID to the index of the target that owns it. The `daemon`
/// PID is never a member, and its children join a unit only through their own
/// session or tracking, not as its descendants.
fn membership(
    units: &[UnitTarget],
    table: &HashMap<u32, ProcStat>,
    daemon: Option<u32>,
) -> HashMap<u32, usize> {
    let mut sessions: HashMap<u32, usize> = HashMap::new();
    for (index, target) in units.iter().enumerate() {
        for &sid in &target.sessions {
            // Sessions 0/1 belong to the kernel and init; never sweep those in.
            if sid > 1 {
                sessions.entry(sid).or_insert(index);
            }
        }
    }
    let mut owners: HashMap<u32, usize> = table
        .iter()
        .filter(|&(&pid, _)| Some(pid) != daemon)
        .filter_map(|(&pid, stat)| sessions.get(&stat.session).map(|&index| (pid, index)))
        .collect();
    for (index, target) in units.iter().enumerate() {
        for identity in &target.processes {
            // A different starttime means the PID now names an unrelated process.
            if Some(identity.pid) != daemon
                && table
                    .get(&identity.pid)
                    .is_some_and(|stat| stat.starttime == identity.start_time)
            {
                owners.entry(identity.pid).or_insert(index);
            }
        }
    }
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    // Leaving the daemon out of the tree keeps the walk from reaching it, and
    // so from reaching its other children (git, docker) through it.
    for (&pid, stat) in table.iter().filter(|&(&pid, _)| Some(pid) != daemon) {
        children.entry(stat.ppid).or_default().push(pid);
    }
    let mut queue: VecDeque<(u32, usize)> = owners.iter().map(|(&pid, &i)| (pid, i)).collect();
    while let Some((pid, index)) = queue.pop_front() {
        for &child in children.get(&pid).into_iter().flatten() {
            if let Entry::Vacant(entry) = owners.entry(child) {
                entry.insert(index);
                queue.push_back((child, index));
            }
        }
    }
    owners
}

/// Resolves membership and measures every member and the daemon.
fn collect(
    state: &mut State,
    units: &[UnitTarget],
    daemon: Option<ProcessIdentity>,
) -> Result<Vec<Sample>, Error> {
    // Without units only the daemon is read, so `/proc` is not scanned.
    let (table, owners) = if units.is_empty() {
        (HashMap::new(), HashMap::new())
    } else {
        let table = process_table()?;
        let owners = membership(units, &table, daemon.map(|daemon| daemon.pid));
        (table, owners)
    };
    // A different starttime means the daemon's PID now names an unrelated process.
    let daemon = daemon.and_then(|daemon| {
        read_stat(daemon.pid)
            .ok()
            .flatten()
            .filter(|stat| stat.starttime == daemon.start_time && !stat.is_exited())
            .map(|stat| (daemon.pid, stat))
    });
    if owners.is_empty() && daemon.is_none() {
        state.previous.clear();
        state.pss.clear();
        return Ok(Vec::new());
    }
    let mut cycle = Cycle {
        uptime: procfs::parse_uptime(&fs::read_to_string("/proc/uptime")?)?,
        now: Instant::now(),
        previous: HashMap::with_capacity(owners.len() + 1),
        pss: HashMap::with_capacity(owners.len() + 1),
    };
    let ts_ms = unix_ms(SystemTime::now());
    let mut samples = Vec::with_capacity(owners.len() + 1);
    for (pid, index) in owners {
        let Some(stat) = table.get(&pid) else {
            continue;
        };
        let Some((cpu_percent, footprint_bytes, rss_bytes)) = measure(state, &mut cycle, pid, stat)
        else {
            continue;
        };
        samples.push(Sample {
            subject: Subject::Process {
                unit: units[index].unit.clone(),
                pid,
                start_time: stat.starttime,
            },
            cpu_percent,
            footprint_bytes,
            rss_bytes,
            ts_ms,
        });
    }
    if let Some((pid, stat)) = daemon
        && let Some((cpu_percent, footprint_bytes, rss_bytes)) =
            measure(state, &mut cycle, pid, &stat)
    {
        samples.push(Sample {
            subject: Subject::Daemon,
            cpu_percent,
            footprint_bytes,
            rss_bytes,
            ts_ms,
        });
    }
    // Replacing the caches drops exited processes.
    state.previous = cycle.previous;
    state.pss = cycle.pss;
    samples.sort_by(|a, b| a.subject.cmp(&b.subject));
    Ok(samples)
}

/// One sampling pass: its clocks and the caches that replace [`State`]'s at
/// the end, so processes not read in this pass drop out.
struct Cycle {
    uptime: f64,
    now: Instant,
    previous: HashMap<(u32, u64), CpuReading>,
    pss: HashMap<(u32, u64), (u64, Instant)>,
}

/// Measures one process as `(cpu_percent, footprint_bytes, rss_bytes)`, with
/// CPU against the previous pass and PSS from the cache while it is fresh.
/// Returns `None` once the process is gone.
fn measure(state: &State, cycle: &mut Cycle, pid: u32, stat: &ProcStat) -> Option<(f64, u64, u64)> {
    let statm = read_proc(pid, "statm").ok().flatten()?;
    let statm = procfs::parse_statm(&statm).ok()?;
    let now = cycle.now;
    let key = (pid, stat.starttime);
    let ticks = stat.utime.saturating_add(stat.stime);
    let cpu_percent = match state.previous.get(&key) {
        Some(last) => procfs::cpu_percent(
            ticks.saturating_sub(last.ticks),
            state.ticks_per_second,
            now.duration_since(last.at).as_secs_f64(),
        ),
        // First sight: lifetime average so a busy process is not idle for a tick.
        None => procfs::lifetime_cpu_percent(stat, state.ticks_per_second, cycle.uptime),
    };
    cycle.previous.insert(key, CpuReading { ticks, at: now });

    let rss_bytes = statm.resident.saturating_mul(state.page_size);
    let pss = match state.pss.get(&key) {
        Some(&(bytes, at)) if now.duration_since(at) < state.pss_interval => Some((bytes, at)),
        // Unreadable smaps_rollup (kernel < 4.14, ptrace policy) falls back to RSS.
        _ => read_proc(pid, "smaps_rollup")
            .ok()
            .flatten()
            .and_then(|text| procfs::parse_smaps_rollup_pss(&text).ok())
            .map(|bytes| (bytes, now)),
    };
    if let Some(entry) = pss {
        cycle.pss.insert(key, entry);
    }
    Some((
        cpu_percent,
        pss.map_or(rss_bytes, |(bytes, _)| bytes),
        rss_bytes,
    ))
}
