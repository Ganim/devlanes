//! macOS backend over libproc, sysctl and `lsof`.

use std::{
    ffi::{CStr, c_int},
    io,
    mem::{MaybeUninit, size_of},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    process::{Command, Stdio},
    ptr,
};

use crate::{
    Error, ListeningSocket, ProcessIdentity, ProcessInfo, SignalOutcome, TcpConnection, lsof,
    raw_pid,
};

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

/// Sets or clears `PRIO_DARWIN_BG` with `setpriority(PRIO_DARWIN_PROCESS)` after
/// checking the PID still has the recorded start time. The kernel allows it for
/// processes of the same user, in both directions.
pub(crate) fn set_background(
    identity: &ProcessIdentity,
    background: bool,
) -> Result<SignalOutcome, Error> {
    let Some(info) = process(identity.pid)? else {
        return Ok(SignalOutcome::Gone);
    };
    if info.start_time != identity.start_time {
        return Ok(SignalOutcome::Mismatch);
    }
    if info.zombie {
        return Ok(SignalOutcome::Gone);
    }
    let value = if background { libc::PRIO_DARWIN_BG } else { 0 };
    // SAFETY: setpriority takes only integers and reads or writes no memory of
    // this process.
    let rc = unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, identity.pid, value) };
    if rc == 0 {
        return Ok(SignalOutcome::Sent);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(SignalOutcome::Gone);
    }
    Err(error.into())
}

/// Reads `net.inet.tcp.pcblist_n`, the TCP table `netstat` prints, and keeps
/// established connections.
pub(crate) fn tcp_connections() -> Result<Vec<TcpConnection>, Error> {
    parse_pcblist(&sysctl_bytes(c"net.inet.tcp.pcblist_n")?)
}

/// Reads a variable-length sysctl, retrying when it grew between the size query
/// and the read.
fn sysctl_bytes(name: &CStr) -> io::Result<Vec<u8>> {
    for _ in 0..8 {
        let mut len = 0_usize;
        // SAFETY: the name is NUL-terminated and a null buffer with a writable
        // length only asks the kernel for the size it needs.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                ptr::null_mut(),
                &raw mut len,
                ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // The kernel's estimate already has headroom; a little more saves a retry.
        let mut buffer = vec![0_u8; len + len / 8 + 4096];
        let mut written = buffer.len();
        // SAFETY: `buffer` and `written` describe writable bytes the kernel fills
        // at most up to `written`; no new value is supplied.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                &raw mut written,
                ptr::null_mut(),
                0,
            )
        };
        if rc == 0 {
            buffer.truncate(written.min(buffer.len()));
            return Ok(buffer);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOMEM) {
            return Err(error);
        }
    }
    Err(io::Error::other(format!(
        "sysctl {} kept growing",
        name.to_string_lossy()
    )))
}

/// `sizeof(struct xinpgen)`, the record that opens and closes `pcblist_n`.
/// Items are never this short, which is how `netstat` finds the end.
const XINPGEN_SIZE: usize = 24;
/// `xgn_kind` of `struct xinpcb_n` and `struct xtcpcb_n` (`<sys/socketvar.h>`).
const XSO_INPCB: u32 = 0x10;
const XSO_TCPCB: u32 = 0x20;
/// Offsets in `struct xinpcb_n`, laid out under `#pragma pack(4)` in
/// `<netinet/in_pcb.h>`. Ports are in network byte order.
const INP_FPORT: usize = 16;
const INP_LPORT: usize = 18;
const INP_VFLAG: usize = 44;
const INP_FADDR: usize = 48;
const INP_LADDR: usize = 64;
/// End of the local address, the last field read.
const XINPCB_READ: usize = 80;
/// `inp_vflag` bits.
const INP_IPV4: u8 = 0x1;
const INP_IPV6: u8 = 0x2;
/// Offset of `t_state` in `struct xtcpcb_n` (`<netinet/tcp_var.h>`, pack(4)).
const T_STATE: usize = 36;
/// `TCPS_ESTABLISHED` from `<netinet/tcp_fsm.h>`.
const TCPS_ESTABLISHED: i32 = 4;

/// Builds the error for a `pcblist_n` buffer that does not have the expected shape.
fn pcblist_malformed(detail: impl Into<String>) -> Error {
    Error::Malformed {
        source_name: "net.inet.tcp.pcblist_n",
        detail: detail.into(),
    }
}

/// Reads a native-endian `u32` at `at`.
fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_ne_bytes(bytes.get(at..end)?.try_into().ok()?))
}

/// Parses the output of `net.inet.tcp.pcblist_n` and returns its established
/// connections.
///
/// The buffer is an `xinpgen` header, then one group of items per PCB
/// (`xinpcb_n`, `xsocket_n`, two `xsockbuf_n`, `xsockstat_n`, `xtcpcb_n`), then a
/// closing `xinpgen`. Every item starts with its length and kind and is padded to
/// 8 bytes. A group's addresses come from its `xinpcb_n` and its state from the
/// `xtcpcb_n` that ends it; other kinds are skipped by length, as `netstat` does.
pub(crate) fn parse_pcblist(bytes: &[u8]) -> Result<Vec<TcpConnection>, Error> {
    let header = read_u32(bytes, 0).ok_or_else(|| pcblist_malformed("no header"))? as usize;
    if header < XINPGEN_SIZE || header > bytes.len() {
        return Err(pcblist_malformed(format!("header length {header}")));
    }
    let mut at = header.next_multiple_of(8);
    // The kernel writes no closing record when the table is empty.
    if at >= bytes.len() {
        return Ok(Vec::new());
    }
    // Addresses of the PCB whose `xtcpcb_n` has not arrived yet; the inner
    // `None` is a PCB of neither family.
    let mut pending: Option<Option<TcpConnection>> = None;
    let mut connections = Vec::new();
    loop {
        let len = read_u32(bytes, at)
            .ok_or_else(|| pcblist_malformed(format!("truncated at {at}")))?
            as usize;
        if len <= XINPGEN_SIZE {
            if pending.is_some() {
                return Err(pcblist_malformed("PCB without TCP state at the end"));
            }
            return Ok(connections);
        }
        let item = at
            .checked_add(len)
            .and_then(|end| bytes.get(at..end))
            .ok_or_else(|| pcblist_malformed(format!("item of {len} bytes at {at} overruns")))?;
        match read_u32(item, 4) {
            Some(XSO_INPCB) => {
                if pending.is_some() {
                    return Err(pcblist_malformed(format!("PCB at {at} follows a PCB")));
                }
                pending = Some(inpcb_addresses(item)?);
            }
            Some(XSO_TCPCB) => {
                let addresses = pending
                    .take()
                    .ok_or_else(|| pcblist_malformed(format!("TCP state at {at} without PCB")))?;
                let state = read_u32(item, T_STATE)
                    .ok_or_else(|| pcblist_malformed(format!("short TCP state at {at}")))?;
                if state.cast_signed() == TCPS_ESTABLISHED
                    && let Some(connection) = addresses
                {
                    connections.push(connection);
                }
            }
            _ => {}
        }
        at = at
            .checked_add(len.next_multiple_of(8))
            .ok_or_else(|| pcblist_malformed("offset overflow"))?;
    }
}

/// Decodes the local and foreign address of one `xinpcb_n`. An IPv4 address sits
/// in the last four bytes of the `in6_addr` union.
fn inpcb_addresses(item: &[u8]) -> Result<Option<TcpConnection>, Error> {
    let field = |at: usize, len: usize| {
        item.get(at..at + len)
            .ok_or_else(|| pcblist_malformed(format!("PCB of {} bytes", item.len())))
    };
    field(0, XINPCB_READ)?;
    let port = |at: usize| field(at, 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let address = |at: usize, vflag: u8| -> Result<Option<IpAddr>, Error> {
        let raw: [u8; 16] = field(at, 16)?
            .try_into()
            .map_err(|_| pcblist_malformed("address"))?;
        Ok(if vflag & INP_IPV4 != 0 {
            Some(IpAddr::V4(Ipv4Addr::new(
                raw[12], raw[13], raw[14], raw[15],
            )))
        } else if vflag & INP_IPV6 != 0 {
            Some(IpAddr::V6(Ipv6Addr::from(raw)))
        } else {
            None
        })
    };
    let vflag = field(INP_VFLAG, 1)?[0];
    let (Some(local), Some(remote)) = (address(INP_LADDR, vflag)?, address(INP_FADDR, vflag)?)
    else {
        return Ok(None);
    };
    Ok(Some(TcpConnection {
        local: SocketAddr::new(local, port(INP_LPORT)?),
        remote: SocketAddr::new(remote, port(INP_FPORT)?),
    }))
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

    /// Scheduling priority `ps` reports for `pid`; 4 (`MAXPRI_THROTTLE`) in the
    /// background band. `getpriority(PRIO_DARWIN_PROCESS)` cannot read it back:
    /// XNU answers with the caller's own state whatever PID is passed.
    fn priority_of(pid: u32) -> u32 {
        let output = Command::new("/bin/ps")
            .args(["-o", "pri=", "-p", &pid.to_string()])
            .output()
            .expect("run ps");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("priority")
    }

    /// A same-user child can be put in the background band and taken out again,
    /// and a recycled identity is refused.
    #[test]
    fn background_round_trip() {
        /// Owns the test child.
        struct Reaped(std::process::Child);
        impl Drop for Reaped {
            /// Kills and reaps the child even when an assertion failed.
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Reaped(
            Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep"),
        );
        let identity = crate::identity(child.0.id())
            .expect("lookup")
            .expect("child identity");
        let normal = priority_of(identity.pid);
        assert_ne!(normal, 4);
        assert_eq!(
            set_background(&identity, true).expect("set"),
            SignalOutcome::Sent
        );
        assert_eq!(priority_of(identity.pid), 4);
        assert_eq!(
            set_background(&identity, false).expect("clear"),
            SignalOutcome::Sent
        );
        assert_eq!(priority_of(identity.pid), normal);
        let forged = ProcessIdentity {
            start_time: identity.start_time + 1,
            ..identity
        };
        assert_eq!(
            set_background(&forged, true).expect("mismatch"),
            SignalOutcome::Mismatch
        );
        assert_eq!(priority_of(identity.pid), normal);
    }

    /// Appends one item padded to 8 bytes, with its length and kind filled in.
    fn item(buffer: &mut Vec<u8>, kind: u32, mut body: Vec<u8>) {
        let len = u32::try_from(body.len()).expect("small item");
        body[0..4].copy_from_slice(&len.to_ne_bytes());
        body[4..8].copy_from_slice(&kind.to_ne_bytes());
        buffer.extend_from_slice(&body);
        buffer.resize(buffer.len().next_multiple_of(8), 0);
    }

    /// An `xinpgen` record.
    fn xinpgen(buffer: &mut Vec<u8>) {
        let mut body = vec![0_u8; XINPGEN_SIZE];
        body[0..4].copy_from_slice(&(XINPGEN_SIZE as u32).to_ne_bytes());
        buffer.extend_from_slice(&body);
    }

    /// One PCB group in kernel order: `xinpcb_n` (104 bytes), `xsocket_n`, two
    /// `xsockbuf_n`, `xsockstat_n` and `xtcpcb_n`. The items the parser skips
    /// are filler of odd sizes, so padding to 8 bytes is exercised.
    fn pcb(buffer: &mut Vec<u8>, local: SocketAddr, remote: SocketAddr, state: i32) {
        let mut inpcb = vec![0_u8; 104];
        inpcb[INP_LPORT..INP_LPORT + 2].copy_from_slice(&local.port().to_be_bytes());
        inpcb[INP_FPORT..INP_FPORT + 2].copy_from_slice(&remote.port().to_be_bytes());
        for (at, addr) in [(INP_LADDR, local.ip()), (INP_FADDR, remote.ip())] {
            match addr {
                IpAddr::V4(v4) => {
                    inpcb[INP_VFLAG] = INP_IPV4;
                    inpcb[at + 12..at + 16].copy_from_slice(&v4.octets());
                }
                IpAddr::V6(v6) => {
                    inpcb[INP_VFLAG] = INP_IPV6;
                    inpcb[at..at + 16].copy_from_slice(&v6.octets());
                }
            }
        }
        item(buffer, XSO_INPCB, inpcb);
        item(buffer, 0x1, vec![0xAA; 117]);
        item(buffer, 0x2, vec![0xAA; 36]);
        item(buffer, 0x4, vec![0xAA; 36]);
        item(buffer, 0x8, vec![0xAA; 165]);
        let mut tcpcb = vec![0_u8; 244];
        tcpcb[T_STATE..T_STATE + 4].copy_from_slice(&state.to_ne_bytes());
        item(buffer, XSO_TCPCB, tcpcb);
    }

    /// Socket address shorthand for fixtures.
    fn addr(text: &str) -> SocketAddr {
        text.parse().expect("socket address")
    }

    /// Established IPv4 and IPv6 connections are decoded; listeners and other
    /// states are dropped.
    #[test]
    fn pcblist_fixture() {
        let mut buffer = Vec::new();
        xinpgen(&mut buffer);
        pcb(&mut buffer, addr("127.0.0.1:4000"), addr("0.0.0.0:0"), 1);
        pcb(
            &mut buffer,
            addr("127.0.0.1:4000"),
            addr("127.0.0.1:51000"),
            TCPS_ESTABLISHED,
        );
        pcb(
            &mut buffer,
            addr("[::1]:5432"),
            addr("[::1]:52000"),
            TCPS_ESTABLISHED,
        );
        pcb(
            &mut buffer,
            addr("10.0.0.2:443"),
            addr("10.0.0.9:60000"),
            10,
        );
        xinpgen(&mut buffer);
        assert_eq!(
            parse_pcblist(&buffer).expect("parse"),
            [
                TcpConnection {
                    local: addr("127.0.0.1:4000"),
                    remote: addr("127.0.0.1:51000"),
                },
                TcpConnection {
                    local: addr("[::1]:5432"),
                    remote: addr("[::1]:52000"),
                },
            ]
        );

        // The kernel skips the closing record when the table is empty.
        let mut empty = Vec::new();
        xinpgen(&mut empty);
        assert_eq!(parse_pcblist(&empty).expect("empty"), []);
    }

    /// Truncated or inconsistent buffers are errors, never a partial table.
    #[test]
    fn pcblist_rejects_malformed_input() {
        let mut full = Vec::new();
        xinpgen(&mut full);
        pcb(
            &mut full,
            addr("127.0.0.1:4000"),
            addr("127.0.0.1:51000"),
            TCPS_ESTABLISHED,
        );
        xinpgen(&mut full);
        assert!(parse_pcblist(&full).is_ok());
        let trailer = full.len() - XINPGEN_SIZE;
        for cut in [0, 3, 30, 100, trailer - 50, trailer] {
            assert!(
                matches!(parse_pcblist(&full[..cut]), Err(Error::Malformed { .. })),
                "cut at {cut}"
            );
        }

        let mut overrun = full.clone();
        overrun[XINPGEN_SIZE..XINPGEN_SIZE + 4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(matches!(
            parse_pcblist(&overrun),
            Err(Error::Malformed { .. })
        ));

        let mut short_pcb = Vec::new();
        xinpgen(&mut short_pcb);
        item(&mut short_pcb, XSO_INPCB, vec![0; 40]);
        xinpgen(&mut short_pcb);
        assert!(matches!(
            parse_pcblist(&short_pcb),
            Err(Error::Malformed { .. })
        ));

        let mut orphan_state = Vec::new();
        xinpgen(&mut orphan_state);
        item(&mut orphan_state, XSO_TCPCB, vec![0; 244]);
        xinpgen(&mut orphan_state);
        assert!(matches!(
            parse_pcblist(&orphan_state),
            Err(Error::Malformed { .. })
        ));
    }

    /// The live table parses and lists both ends of a loopback connection.
    #[test]
    fn pcblist_live() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let client =
            std::net::TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let local = client.local_addr().expect("client addr");
        let server = listener.local_addr().expect("server addr");
        let connections = tcp_connections().expect("pcblist_n");
        assert!(connections.contains(&TcpConnection {
            local,
            remote: server
        }));
        assert!(connections.contains(&TcpConnection {
            local: server,
            remote: local
        }));
    }
}
