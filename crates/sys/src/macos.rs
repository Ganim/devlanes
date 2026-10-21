//! macOS backend over libproc, sysctl and `lsof`.

use std::{
    ffi::c_int,
    io,
    mem::{MaybeUninit, size_of},
    process::{Command, Stdio},
    ptr,
};

use crate::{Error, ListeningSocket, ProcessInfo, lsof, raw_pid};

/// `sizeof(struct proc_bsdinfo)`, the exact length libproc must return.
const BSDINFO_SIZE: c_int = size_of::<libc::proc_bsdinfo>() as c_int;

/// Lists all PIDs, then reads BSD info for each; processes exiting mid-scan are skipped.
pub(crate) fn processes() -> Result<Vec<ProcessInfo>, Error> {
    Ok(all_pids()?.into_iter().filter_map(bsd_info).collect())
}

/// Reads one process, falling back to sysctl for unreaped zombies.
pub(crate) fn process(pid: u32) -> Result<Option<ProcessInfo>, Error> {
    let Ok(pid) = i32::try_from(pid) else {
        return Ok(None);
    };
    Ok(bsd_info(pid).or_else(|| zombie_info(pid)))
}

/// `sizeof(struct kinfo_proc)` on 64-bit macOS; the libc crate does not define it.
const KINFO_PROC_SIZE: usize = 648;
/// Offsets inside `kinfo_proc.kp_proc` (`struct extern_proc`), stable since 10.0:
/// `p_un.__p_starttime` (timeval) at 0, `p_stat` at 36 and `p_pid` at 40.
const STARTTIME_SEC: usize = 0;
const STARTTIME_USEC: usize = 8;
const P_STAT: usize = 36;
const P_PID: usize = 40;

/// Reads identity fields of a zombie through `KERN_PROC_PID`, which libproc refuses
/// once a process exited. Needed so a leader that exits instantly still gets an
/// identity before it is reaped. Parent, group and name are not decoded.
fn zombie_info(pid: libc::pid_t) -> Option<ProcessInfo> {
    let (stat, start_time) = kinfo(pid)?;
    if u32::from(stat) != libc::SZOMB {
        return None;
    }
    Some(ProcessInfo {
        pid: u32::try_from(pid).ok()?,
        ppid: 0,
        pgid: 0,
        sid: None,
        uid: 0,
        start_time,
        zombie: true,
        name: String::new(),
    })
}

/// Returns `(p_stat, start time in microseconds)` from `KERN_PROC_PID`.
fn kinfo(pid: libc::pid_t) -> Option<(u8, u64)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut buffer = [0_u8; KINFO_PROC_SIZE];
    let mut len = buffer.len();
    // SAFETY: `mib` holds four valid MIB entries, `buffer` and `len` describe
    // KINFO_PROC_SIZE writable bytes, and no new value is supplied.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buffer.as_mut_ptr().cast(),
            &raw mut len,
            ptr::null_mut(),
            0,
        )
    };
    // A missing PID succeeds with zero length.
    if rc != 0 || len != KINFO_PROC_SIZE {
        return None;
    }
    let read_i64 = |at: usize| {
        buffer
            .get(at..at + 8)?
            .try_into()
            .ok()
            .map(i64::from_ne_bytes)
    };
    let read_i32 = |at: usize| {
        buffer
            .get(at..at + 4)?
            .try_into()
            .ok()
            .map(i32::from_ne_bytes)
    };
    // A PID mismatch means the assumed layout is wrong; refuse rather than misread.
    if read_i32(P_PID)? != pid {
        return None;
    }
    let seconds = u64::try_from(read_i64(STARTTIME_SEC)?).ok()?;
    let micros = u64::try_from(read_i32(STARTTIME_USEC)?).ok()?;
    Some((
        *buffer.get(P_STAT)?,
        seconds.saturating_mul(1_000_000).saturating_add(micros),
    ))
}

/// Returns every PID, retrying when the table grew past the buffer between calls.
pub(crate) fn all_pids() -> io::Result<Vec<libc::pid_t>> {
    loop {
        // SAFETY: a null buffer with zero size only asks libproc for the current count.
        let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
        let count = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
        // Headroom for processes forked between the two calls.
        let capacity = count + 64;
        let mut pids: Vec<libc::pid_t> = vec![0; capacity];
        let bytes = c_int::try_from(capacity * size_of::<libc::pid_t>())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        // SAFETY: `pids` owns `bytes` writable bytes of initialized pid_t storage, and
        // libproc writes at most `bytes` bytes.
        let written = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        let written = usize::try_from(written).map_err(|_| io::Error::last_os_error())?;
        if written < capacity {
            pids.truncate(written);
            return Ok(pids);
        }
    }
}

/// Reads `PROC_PIDTBSDINFO`; `None` when the process exited or is not inspectable.
fn bsd_info(pid: libc::pid_t) -> Option<ProcessInfo> {
    if pid <= 0 {
        return None;
    }
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // SAFETY: `info` is a writable, aligned proc_bsdinfo and BSDINFO_SIZE is its exact
    // length, so libproc cannot write past it.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            BSDINFO_SIZE,
        )
    };
    if written != BSDINFO_SIZE {
        return None;
    }
    // SAFETY: the buffer started zeroed and proc_bsdinfo holds only integers and byte
    // arrays, so every bit pattern libproc leaves is a valid value.
    let info = unsafe { info.assume_init() };
    // proc_bsdinfo has no session field; getsid works for any visible PID.
    let sid = raw_pid(info.pbi_pid)
        .and_then(|p| rustix::process::getsid(Some(p)).ok())
        .and_then(|sid| u32::try_from(sid.as_raw_nonzero().get()).ok());
    let name = c_name(&info.pbi_name);
    Some(ProcessInfo {
        pid: info.pbi_pid,
        ppid: info.pbi_ppid,
        pgid: info.pbi_pgid,
        sid,
        uid: info.pbi_uid,
        start_time: info
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec),
        zombie: info.pbi_status == libc::SZOMB,
        name: if name.is_empty() {
            c_name(&info.pbi_comm)
        } else {
            name
        },
    })
}

/// Decodes a NUL-padded kernel name.
fn c_name(raw: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Runs lsof restricted to the pid set. libproc socket structures are not exposed by
/// the libc crate, and this path is diagnostics-only.
pub(crate) fn listening_tcp(pids: &[u32]) -> Result<Vec<ListeningSocket>, Error> {
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let output = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-p", &list, "-iTCP", "-sTCP:LISTEN", "-Fptn"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::Tool {
            tool: "lsof",
            detail: error.to_string(),
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // lsof exits 1 when nothing matched, which is a normal empty result.
    if !output.status.success() && !stdout.trim().is_empty() {
        return Err(Error::Tool {
            tool: "lsof",
            detail: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(lsof::parse(&stdout))
}

/// `PROC_PIDFDVNODEINFO` and `PROC_PIDFDPIPEINFO` from `<sys/proc_info.h>`.
const PROC_PIDFDVNODEINFO: c_int = 1;
const PROC_PIDFDPIPEINFO: c_int = 6;

/// `struct proc_fileinfo`; libc does not define it. Unread fields keep the layout.
#[repr(C)]
struct ProcFileInfo {
    _fi_openflags: u32,
    _fi_status: u32,
    _fi_offset: i64,
    _fi_type: i32,
    _fi_guardflags: u32,
}

/// `struct vnode_fdinfo`.
#[repr(C)]
struct VnodeFdInfo {
    _pfi: ProcFileInfo,
    pvi: libc::vnode_info,
}

/// `struct pipe_info`.
#[repr(C)]
struct PipeInfo {
    _pipe_stat: libc::vinfo_stat,
    pipe_handle: u64,
    pipe_peerhandle: u64,
    _pipe_status: i32,
    _rfu_1: i32,
}

/// `struct pipe_fdinfo`.
#[repr(C)]
struct PipeFdInfo {
    _pfi: ProcFileInfo,
    pipeinfo: PipeInfo,
}

/// Kernel structs made only of integers, valid for every bit pattern.
trait PlainData {}
impl PlainData for VnodeFdInfo {}
impl PlainData for PipeFdInfo {}

/// Device number for character devices; for pipes, this end's kernel handle.
pub(crate) fn open_file(fd: std::os::fd::BorrowedFd<'_>) -> Result<crate::OpenFile, Error> {
    use std::os::{fd::AsRawFd, unix::fs::FileTypeExt, unix::fs::MetadataExt};
    let metadata = std::fs::File::from(fd.try_clone_to_owned()?).metadata()?;
    if metadata.file_type().is_char_device() {
        return Ok(crate::OpenFile::Device(metadata.rdev()));
    }
    let own = i32::try_from(std::process::id()).map_err(|_| io::Error::other("pid range"))?;
    pipe_info(own, fd.as_raw_fd())
        .map(|info| crate::OpenFile::Pipe(info.pipeinfo.pipe_handle))
        .ok_or_else(|| Error::Malformed {
            source_name: "open file",
            detail: "not a device or pipe".into(),
        })
}

/// Walks the descriptor table of `pid` through libproc.
pub(crate) fn holds(pid: u32, target: crate::OpenFile) -> Result<bool, Error> {
    let Ok(pid) = i32::try_from(pid) else {
        return Ok(false);
    };
    for fd in descriptors(pid) {
        let matched = match target {
            crate::OpenFile::Device(rdev) if fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32 => {
                vnode_info(pid, fd.proc_fd)
                    .is_some_and(|info| u64::from(info.pvi.vi_stat.vst_rdev) == rdev)
            }
            crate::OpenFile::Pipe(handle) if fd.proc_fdtype == libc::PROX_FDTYPE_PIPE as u32 => {
                pipe_info(pid, fd.proc_fd).is_some_and(|info| {
                    info.pipeinfo.pipe_handle == handle || info.pipeinfo.pipe_peerhandle == handle
                })
            }
            _ => false,
        };
        if matched {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Lists the open descriptors of `pid`; empty when it exited or is not inspectable.
fn descriptors(pid: c_int) -> Vec<libc::proc_fdinfo> {
    // SAFETY: a null buffer with size 0 only asks libproc for the needed byte count.
    let needed = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, ptr::null_mut(), 0) };
    let Ok(needed) = usize::try_from(needed) else {
        return Vec::new();
    };
    // Room for descriptors opened between the two calls.
    let capacity = needed / size_of::<libc::proc_fdinfo>() + 16;
    let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(capacity);
    let Ok(bytes) = c_int::try_from(capacity * size_of::<libc::proc_fdinfo>()) else {
        return Vec::new();
    };
    // SAFETY: `fds` has capacity for `bytes` bytes of proc_fdinfo, and libproc writes
    // at most `bytes`, returning how many it wrote.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            bytes,
        )
    };
    let Ok(written) = usize::try_from(written) else {
        return Vec::new();
    };
    let count = (written / size_of::<libc::proc_fdinfo>()).min(capacity);
    // SAFETY: libproc initialized `count` whole entries, all plain integers, and
    // `count` never exceeds the allocated capacity.
    unsafe { fds.set_len(count) };
    fds
}

/// Reads one descriptor's flavor-specific info; `None` unless libproc filled it all.
fn fd_info<T: PlainData>(pid: c_int, fd: i32, flavor: c_int) -> Option<T> {
    let size = c_int::try_from(size_of::<T>()).ok()?;
    let mut info = MaybeUninit::<T>::zeroed();
    // SAFETY: `info` is `size` writable bytes for one `T`; libproc writes at most that.
    let written = unsafe { libc::proc_pidfdinfo(pid, fd, flavor, info.as_mut_ptr().cast(), size) };
    if written != size {
        return None;
    }
    // SAFETY: `T: PlainData` is valid for any bit pattern, and libproc filled the
    // whole zero-initialized buffer.
    Some(unsafe { info.assume_init() })
}

/// `PROC_PIDFDVNODEINFO` for one descriptor.
fn vnode_info(pid: c_int, fd: i32) -> Option<VnodeFdInfo> {
    fd_info(pid, fd, PROC_PIDFDVNODEINFO)
}

/// `PROC_PIDFDPIPEINFO` for one descriptor.
fn pipe_info(pid: c_int, fd: i32) -> Option<PipeFdInfo> {
    fd_info(pid, fd, PROC_PIDFDPIPEINFO)
}

/// Reads `kern.bootsessionuuid`, a UUID the kernel generates once per boot.
/// `kern.boottime` is not used: XNU shifts it whenever the wall clock is stepped,
/// so it can change while the machine keeps running.
pub(crate) fn boot_id() -> Result<String, Error> {
    let mut buffer = [0u8; 128];
    let mut len = buffer.len();
    // SAFETY: the name is a NUL-terminated literal, `buffer`/`len` describe a
    // writable byte array of that length, and no new value is supplied.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &raw mut len,
            ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let bytes = &buffer[..len.min(buffer.len())];
    let bytes = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
    let id = std::str::from_utf8(bytes)
        .map_err(|error| Error::Malformed {
            source_name: "kern.bootsessionuuid",
            detail: error.to_string(),
        })?
        .trim();
    if id.is_empty() {
        return Err(Error::Malformed {
            source_name: "kern.bootsessionuuid",
            detail: "empty".into(),
        });
    }
    Ok(id.to_owned())
}

/// Caps the soft limit at `kern.maxfilesperproc`; macOS rejects larger values.
pub(crate) fn max_nofile(hard: Option<u64>) -> Result<u64, Error> {
    let mut value: c_int = 0;
    let mut len = size_of::<c_int>();
    // SAFETY: the name is a NUL-terminated literal and `value`/`len` describe a
    // writable c_int; no new value is supplied.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&raw mut value).cast(),
            &raw mut len,
            ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let per_process = u64::try_from(value).unwrap_or(0);
    Ok(hard.map_or(per_process, |hard| hard.min(per_process)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-written kinfo_proc offsets agree with libproc for a live process.
    #[test]
    fn kinfo_layout_matches_libproc() {
        let pid = i32::try_from(std::process::id()).expect("pid fits pid_t");
        let live = bsd_info(pid).expect("own bsd info");
        let (stat, start_time) = kinfo(pid).expect("own kinfo");
        assert_eq!(start_time, live.start_time);
        assert_ne!(u32::from(stat), libc::SZOMB);
    }
}
