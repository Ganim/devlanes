//! Parsers for the procfs files the collector reads. Pure functions over text.

use devlanes_metrics::Error;

/// Fields of `/proc/<pid>/stat` used for membership and CPU (see `proc_pid_stat(5)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: u32,
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    pub pgrp: u32,
    pub session: u32,
    /// User and system time in clock ticks (`sysconf(_SC_CLK_TCK)`).
    pub utime: u64,
    pub stime: u64,
    /// Clock ticks after boot; `devlanes-sys` uses this as the start-time token.
    pub starttime: u64,
    /// Resident pages.
    pub rss_pages: u64,
}

impl ProcStat {
    /// Zombies (`Z`) and dead tasks (`X`) hold no memory worth reporting.
    pub fn is_exited(&self) -> bool {
        matches!(self.state, 'Z' | 'X' | 'x')
    }
}

/// Builds a [`Error::Malformed`] for one procfs file.
fn malformed(what: &'static str, detail: impl Into<String>) -> Error {
    Error::Malformed {
        what,
        detail: detail.into(),
    }
}

/// Parses `/proc/<pid>/stat`. `comm` may contain spaces and parentheses, so the
/// fields are split after the last `)`.
pub fn parse_stat(text: &str) -> Result<ProcStat, Error> {
    let what = "/proc/<pid>/stat";
    let open = text
        .find('(')
        .ok_or_else(|| malformed(what, "missing `(`"))?;
    let close = text
        .rfind(')')
        .ok_or_else(|| malformed(what, "missing `)`"))?;
    if close < open {
        return Err(malformed(what, "unbalanced comm"));
    }
    let pid = text[..open]
        .trim()
        .parse()
        .map_err(|_| malformed(what, "bad pid"))?;
    let comm = text[open + 1..close].to_owned();
    // Index 0 is field 3 (state); field N of the man page is index N - 3.
    let fields: Vec<&str> = text[close + 1..].split_ascii_whitespace().collect();
    let number = |field: usize| -> Result<u64, Error> {
        fields
            .get(field - 3)
            .ok_or_else(|| malformed(what, format!("missing field {field}")))?
            .parse()
            .map_err(|_| malformed(what, format!("bad field {field}")))
    };
    let small = |field: usize| -> Result<u32, Error> {
        u32::try_from(number(field)?)
            .map_err(|_| malformed(what, format!("field {field} too large")))
    };
    let state = fields
        .first()
        .and_then(|state| state.chars().next())
        .ok_or_else(|| malformed(what, "missing state"))?;
    Ok(ProcStat {
        pid,
        comm,
        state,
        ppid: small(4)?,
        pgrp: small(5)?,
        session: small(6)?,
        utime: number(14)?,
        stime: number(15)?,
        starttime: number(22)?,
        rss_pages: number(24)?,
    })
}

/// `/proc/<pid>/statm` sizes, in pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Statm {
    pub size: u64,
    pub resident: u64,
    pub shared: u64,
}

/// Parses `/proc/<pid>/statm`.
pub fn parse_statm(text: &str) -> Result<Statm, Error> {
    let what = "/proc/<pid>/statm";
    let mut fields = text.split_ascii_whitespace().map(str::parse::<u64>);
    let mut next = |name: &str| -> Result<u64, Error> {
        fields
            .next()
            .ok_or_else(|| malformed(what, format!("missing {name}")))?
            .map_err(|_| malformed(what, format!("bad {name}")))
    };
    Ok(Statm {
        size: next("size")?,
        resident: next("resident")?,
        shared: next("shared")?,
    })
}

/// Returns the `Pss:` total of `/proc/<pid>/smaps_rollup` in bytes. Lines such as
/// `Pss_Anon:` are not the total and are skipped.
pub fn parse_smaps_rollup_pss(text: &str) -> Result<u64, Error> {
    let what = "/proc/<pid>/smaps_rollup";
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("Pss:"))
        .ok_or_else(|| malformed(what, "missing Pss"))?;
    let mut parts = line.split_ascii_whitespace();
    let value: u64 = parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| malformed(what, "bad Pss value"))?;
    match parts.next() {
        Some("kB") => Ok(value.saturating_mul(1024)),
        other => Err(malformed(what, format!("unexpected Pss unit {other:?}"))),
    }
}

/// Parses the first field of `/proc/uptime`: seconds since boot.
pub fn parse_uptime(text: &str) -> Result<f64, Error> {
    text.split_ascii_whitespace()
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| malformed("/proc/uptime", "bad uptime"))
}

/// CPU percent of one core for `cpu_ticks` over `wall_seconds`.
pub fn cpu_percent(cpu_ticks: u64, ticks_per_second: u64, wall_seconds: f64) -> f64 {
    if ticks_per_second == 0 || wall_seconds <= 0.0 {
        return 0.0;
    }
    cpu_ticks as f64 / ticks_per_second as f64 / wall_seconds * 100.0
}

/// Lifetime CPU percent for a process seen for the first time: total ticks over
/// the time since it started, both measured from boot.
pub fn lifetime_cpu_percent(stat: &ProcStat, ticks_per_second: u64, uptime_seconds: f64) -> f64 {
    if ticks_per_second == 0 {
        return 0.0;
    }
    let started = stat.starttime as f64 / ticks_per_second as f64;
    cpu_percent(
        stat.utime.saturating_add(stat.stime),
        ticks_per_second,
        uptime_seconds - started,
    )
}
