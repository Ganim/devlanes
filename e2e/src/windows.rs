//! Windows side of the harness: the process table, verified termination and the
//! TCP table through `devlanes-sys`, and port slots that avoid reserved ranges.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::Duration,
};

use devlanes_sys::{DescendantTracker, ProcessInfo};

/// What commands keep of the outer environment besides `PATH`: what Windows
/// programs need to start (Winsock fails without `SystemRoot`), to find programs
/// (`PATHEXT`) and temp files. `APPDATA` and `LOCALAPPDATA` stay out, so nothing
/// reads the developer's real profile.
pub(crate) const PASSED_ENV: [&str; 19] = [
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERNAME",
    "USERDOMAIN",
    "COMPUTERNAME",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "OS",
    "PROCESSOR_ARCHITECTURE",
];

/// Exit code of processes the harness terminates.
const KILLED: u32 = 1;

/// How long the reserved ranges saved in the ports directory are trusted.
/// nextest runs every test in its own process, and Windows changes the ranges
/// rarely (Hyper-V, WSL or Docker starting).
const RANGES_FRESH: Duration = Duration::from_secs(10 * 60);

/// `<System32>\<program>`, so a same-named program earlier on `PATH` (Git's
/// `curl`, for one) is never taken instead.
pub fn system32(program: &str) -> PathBuf {
    devlanes_sys_windows::file::system_directory()
        .unwrap_or_else(|_| PathBuf::from(r"C:\Windows\System32"))
        .join(program)
}

/// `%TEMP%\devlanes-e2e-ports`; the temp directory is already per user.
pub(crate) fn ports_dir() -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join("devlanes-e2e-ports");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// False when the `size` ports from `base` overlap a reserved TCP port range,
/// where binding fails with access denied, or when a program other than
/// `fixture-server` (a developer's daemon, another test suite) listens on one of
/// them. Leftover fixtures do not count: the slot's new owner reaps them.
pub(crate) fn ports_usable(dir: &Path, base: u16, size: u16) -> bool {
    let last = base.saturating_add(size - 1);
    if reserved_ranges(dir)
        .iter()
        .any(|&(start, end)| start <= last && base <= end)
    {
        return false;
    }
    let others: Vec<u32> = devlanes_sys::processes()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !p.name.eq_ignore_ascii_case("fixture-server.exe"))
        .map(|p| p.pid)
        .collect();
    !devlanes_sys::listening_tcp(&others)
        .unwrap_or_default()
        .iter()
        .any(|socket| (base..=last).contains(&socket.addr.port()))
}

/// IPv4 and IPv6 reserved TCP ranges, read once per test process from the copy in
/// `dir` while it is fresh, else from `netsh`, saving them. A failing `netsh`
/// leaves the list empty and unsaved, which only risks a bind error later.
fn reserved_ranges(dir: &Path) -> &'static [(u16, u16)] {
    static RANGES: OnceLock<Vec<(u16, u16)>> = OnceLock::new();
    RANGES.get_or_init(|| {
        let saved = dir.join("reserved-ranges.txt");
        if let Some(ranges) = read_saved(&saved) {
            return ranges;
        }
        let mut ranges = Vec::new();
        for family in ["ipv4", "ipv6"] {
            let mut command = Command::new(system32("netsh.exe"));
            command
                .args([
                    "interface",
                    family,
                    "show",
                    "excludedportrange",
                    "protocol=tcp",
                ])
                .stdin(Stdio::null());
            match crate::run_bounded(command, Duration::from_secs(20)) {
                Ok(output) if output.success() => {
                    ranges.extend(parse(&output.stdout));
                }
                _ => return ranges,
            }
        }
        save(&saved, &ranges);
        ranges
    })
}

/// The ranges saved at `path` when it was written less than [`RANGES_FRESH`]
/// ago; `None` when missing, stale or unreadable. The copy uses `netsh`'s row
/// format, so the same parser reads it.
fn read_saved(path: &Path) -> Option<Vec<(u16, u16)>> {
    let age = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?;
    if age >= RANGES_FRESH {
        return None;
    }
    Some(parse(&std::fs::read_to_string(path).ok()?))
}

/// The `(start, end)` rows of `netsh interface <family> show excludedportrange
/// protocol=tcp` output. Headers are translated on localized Windows, so only
/// lines starting with two port numbers count. The daemon's `reserved_ports`
/// has the same parser; the harness does not depend on the daemon.
fn parse(text: &str) -> Vec<(u16, u16)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let start: u16 = fields.next()?.parse().ok()?;
            let end: u16 = fields.next()?.parse().ok()?;
            (start <= end).then_some((start, end))
        })
        .collect()
}

/// Saves `ranges` at `path` through a file of this process renamed into place,
/// so a concurrent reader never sees half a list. Failures only cost the next
/// process another `netsh` run.
fn save(path: &Path, ranges: &[(u16, u16)]) {
    let text: String = ranges
        .iter()
        .map(|(start, end)| format!("{start} {end}\n"))
        .collect();
    let partial = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&partial, text).is_err()
        || devlanes_sys_windows::file::rename_retrying(&partial, path).is_err()
    {
        let _ = std::fs::remove_file(&partial);
    }
}

/// The process, when it is visible to this user.
fn info(pid: i32) -> Option<ProcessInfo> {
    devlanes_sys::process(u32::try_from(pid).ok()?)
        .ok()
        .flatten()
}

/// Creation time of `pid` from the process table, compared only for equality;
/// `None` once the process is gone.
pub fn start_time(pid: i32) -> Option<String> {
    info(pid).map(|info| info.start_time.to_string())
}

/// True while `pid` runs. A process that exited while someone (the daemon) still
/// holds its handle is not alive.
pub fn alive(pid: i32) -> bool {
    info(pid).is_some_and(|info| !info.zombie)
}

/// Terminates `pid` at once, only if the PID still names the process just looked
/// up; a missing process is not an error.
pub fn kill(pid: i32) {
    if let Some(info) = info(pid) {
        let _ = devlanes_sys::terminate_verified(&info.identity(), KILLED);
    }
}

/// Terminates `pid` and every descendant. Windows has no process groups, and a
/// unit's job cannot be reopened once its daemon is gone, so descendants come
/// from parent PIDs, each verified by its start time.
pub fn kill_tree(pid: i32) {
    let Some(root) = info(pid) else {
        return;
    };
    let mut tree = DescendantTracker::new(root.identity());
    tree.update(&devlanes_sys::processes().unwrap_or_default());
    for victim in tree.live() {
        let _ = devlanes_sys::terminate_verified(&victim.identity(), KILLED);
    }
}

/// `fixture-server.exe` PIDs listening on `first..=last`.
pub(crate) fn fixture_listeners(first: u16, last: u16) -> Vec<i32> {
    let fixtures: Vec<u32> = devlanes_sys::processes()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.name.eq_ignore_ascii_case("fixture-server.exe"))
        .map(|p| p.pid)
        .collect();
    let mut pids: Vec<i32> = devlanes_sys::listening_tcp(&fixtures)
        .unwrap_or_default()
        .into_iter()
        .filter(|socket| (first..=last).contains(&socket.addr.port()))
        .filter_map(|socket| i32::try_from(socket.pid).ok())
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The saved copy's `start end` rows read back as written.
    #[test]
    fn saved_ranges_round_trip() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("reserved-ranges.txt");
        save(&path, &[(27339, 27438), (50000, 50059)]);
        assert_eq!(
            read_saved(&path),
            Some(vec![(27339, 27438), (50000, 50059)])
        );
    }
}
