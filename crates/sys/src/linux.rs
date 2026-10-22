//! Linux backend over procfs.

use std::{collections::HashSet, fs, io, os::unix::fs::MetadataExt, path::Path};

use crate::{Error, ListeningSocket, ProcessInfo, procfs};

/// Builds a [`Error::Malformed`] for one procfs file.
fn malformed(source_name: &'static str, detail: impl Into<String>) -> Error {
    Error::Malformed {
        source_name,
        detail: detail.into(),
    }
}

/// Reads the kernel's random per-boot UUID.
pub(crate) fn boot_id() -> Result<String, Error> {
    let id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let id = id.trim();
    if id.is_empty() {
        return Err(malformed("boot_id", "empty"));
    }
    Ok(id.to_owned())
}

/// Scans `/proc` for numeric entries; processes exiting mid-scan are skipped.
pub(crate) fn processes() -> Result<Vec<ProcessInfo>, Error> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        if let Some(info) = read(pid)? {
            processes.push(info);
        }
    }
    Ok(processes)
}

/// Reads one process; zombies keep their stat file until reaped.
pub(crate) fn process(pid: u32) -> Result<Option<ProcessInfo>, Error> {
    read(pid)
}

/// Reads stat plus the owning uid of the procfs directory.
fn read(pid: u32) -> Result<Option<ProcessInfo>, Error> {
    let dir = Path::new("/proc").join(pid.to_string());
    let (stat, metadata) = match (fs::read_to_string(dir.join("stat")), fs::metadata(&dir)) {
        (Ok(stat), Ok(metadata)) => (stat, metadata),
        (Err(error), _) | (_, Err(error)) if gone(&error) => return Ok(None),
        (Err(error), _) | (_, Err(error)) => return Err(error.into()),
    };
    let stat = procfs::parse_stat(&stat)
        .ok_or_else(|| malformed("/proc/<pid>/stat", format!("pid {pid}")))?;
    Ok(Some(ProcessInfo {
        pid: stat.pid,
        ppid: stat.ppid,
        pgid: Some(stat.pgid),
        sid: Some(stat.sid),
        uid: Some(metadata.uid()),
        start_time: stat.start_time,
        // X (dead) is transient but equally unsignalable.
        zombie: matches!(stat.state, 'Z' | 'X'),
        name: stat.name,
    }))
}

/// Exited processes surface as missing files or ESRCH depending on timing.
fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error())
}

/// Joins socket inodes from each pid's fd table with the TCP tables of this namespace.
pub(crate) fn listening_tcp(pids: &[u32]) -> Result<Vec<ListeningSocket>, Error> {
    let mut owners = Vec::new();
    for &pid in pids {
        let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            // Exited, or another user's process; diagnostics are best effort.
            continue;
        };
        for entry in entries.flatten() {
            if let Some(inode) = fs::read_link(entry.path())
                .ok()
                .and_then(|target| procfs::socket_inode(&target.to_string_lossy()))
            {
                owners.push((inode, pid));
            }
        }
    }
    let inodes: HashSet<u64> = owners.iter().map(|(inode, _)| *inode).collect();
    let mut sockets = Vec::new();
    for (path, ipv6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            // tcp6 is absent when IPv6 is disabled.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for row in procfs::parse_tcp_table(&content, ipv6) {
            if !row.listening || !inodes.contains(&row.inode) {
                continue;
            }
            for (inode, pid) in &owners {
                if *inode == row.inode {
                    sockets.push(ListeningSocket {
                        pid: *pid,
                        addr: row.local,
                    });
                }
            }
        }
    }
    Ok(sockets)
}

/// Classifies a descriptor by `fstat`: character devices by rdev, pipes by inode.
pub(crate) fn open_file(fd: std::os::fd::BorrowedFd<'_>) -> Result<crate::OpenFile, Error> {
    let metadata = fs::File::from(fd.try_clone_to_owned()?).metadata()?;
    classify(&metadata).ok_or_else(|| malformed("open file", "not a device or pipe"))
}

/// Maps file metadata to the comparable identity used by [`holds`].
fn classify(metadata: &fs::Metadata) -> Option<crate::OpenFile> {
    use std::os::unix::fs::FileTypeExt;
    let kind = metadata.file_type();
    if kind.is_char_device() {
        Some(crate::OpenFile::Device(metadata.rdev()))
    } else if kind.is_fifo() {
        Some(crate::OpenFile::Pipe(metadata.ino()))
    } else {
        None
    }
}

/// Stats every `/proc/<pid>/fd` entry; the magic links resolve to the open file
/// itself, pipes included.
pub(crate) fn holds(pid: u32, target: crate::OpenFile) -> Result<bool, Error> {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Ok(false);
    };
    Ok(entries.flatten().any(|entry| {
        fs::metadata(entry.path())
            .ok()
            .and_then(|metadata| classify(&metadata))
            == Some(target)
    }))
}

/// Linux accepts any soft limit up to the hard limit; unlimited falls back to `nr_open`.
pub(crate) fn max_nofile(hard: Option<u64>) -> Result<u64, Error> {
    if let Some(hard) = hard {
        return Ok(hard);
    }
    let nr_open = fs::read_to_string("/proc/sys/fs/nr_open")?;
    nr_open
        .trim()
        .parse()
        .map_err(|_| malformed("/proc/sys/fs/nr_open", nr_open))
}
