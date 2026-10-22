//! Which processes another process holds handles to.
//!
//! A console host keeps a handle to every client attached to it, and to the
//! console host it took the session over from, so this links an `OpenConsole`
//! (a Windows Terminal tab) to the programs it shows. The end-to-end tests use it
//! to tell a tab devlanes opened from the user's own.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::ptr;
use std::sync::OnceLock;

use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessHandleInformation};
use windows_sys::Win32::Foundation::{DuplicateHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessId, PROCESS_DUP_HANDLE, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::proc::{creation_time, open};
use super::token::{owned, raw};
use crate::ProcessIdentity;

// The snapshot layout below is the 64-bit one.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("devlanes supports 64-bit Windows (x64 and arm64) only");

/// `STATUS_INFO_LENGTH_MISMATCH`: the buffer is too small for the snapshot.
const INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32.cast_signed();

/// One entry of `PROCESS_HANDLE_SNAPSHOT_INFORMATION` (ntddk), which
/// windows-sys does not declare.
/// Only the handle value and the object type are read.
#[repr(C)]
#[derive(Clone, Copy)]
struct HandleEntry {
    handle_value: HANDLE,
    _handle_count: usize,
    _pointer_count: usize,
    _granted_access: u32,
    object_type_index: u32,
    _handle_attributes: u32,
    _reserved: u32,
}

/// The processes `pid` holds handles to, by identity, or `None` when `pid` is
/// gone. Handles it holds to other kinds of objects, and process handles that
/// cannot be duplicated, are left out. Needs `PROCESS_DUP_HANDLE` on `pid`, so
/// only processes of the same user (at no higher integrity) can be read.
pub fn held_processes(pid: u32) -> io::Result<Option<Vec<ProcessIdentity>>> {
    let Some(owner) = open(pid, PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE)? else {
        return Ok(None);
    };
    let process_type = process_type_index()?;
    let mut held = Vec::new();
    for entry in handle_entries(&owner)? {
        if entry.object_type_index != process_type {
            continue;
        }
        let mut duplicate: HANDLE = ptr::null_mut();
        // SAFETY: `owner` is open with PROCESS_DUP_HANDLE; the duplicate is a new
        // handle in this process, owned below.
        let ok = unsafe {
            DuplicateHandle(
                raw(&owner),
                entry.handle_value,
                GetCurrentProcess(),
                &raw mut duplicate,
                PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                0,
            )
        };
        // The handle may have been closed since the snapshot.
        if ok == 0 {
            continue;
        }
        let duplicate = owned(duplicate)?;
        // SAFETY: `duplicate` is an open process handle with query rights.
        let target = unsafe { GetProcessId(raw(&duplicate)) };
        if target == 0 {
            continue;
        }
        if let Ok(start_time) = creation_time(&duplicate) {
            held.push(ProcessIdentity {
                pid: target,
                sid: target,
                start_time,
            });
        }
    }
    Ok(Some(held))
}

/// The handle table of an open process.
fn handle_entries(process: &OwnedHandle) -> io::Result<Vec<HandleEntry>> {
    // The snapshot is two usize counts followed by the entries; usize elements
    // keep it aligned. It is regrown while handles are opened.
    let mut capacity = 256_usize;
    for _ in 0..8 {
        let words = 2 + capacity * size_of::<HandleEntry>() / size_of::<usize>();
        let mut buffer = vec![0_usize; words];
        let mut needed = 0_u32;
        let bytes = u32::try_from(words * size_of::<usize>())
            .map_err(|_| io::Error::other("handle table too large"))?;
        // SAFETY: `process` is open with PROCESS_QUERY_INFORMATION; `buffer`
        // holds `bytes` writable, aligned bytes.
        let status = unsafe {
            NtQueryInformationProcess(
                raw(process),
                ProcessHandleInformation,
                buffer.as_mut_ptr().cast(),
                bytes,
                &raw mut needed,
            )
        };
        if status == INFO_LENGTH_MISMATCH {
            let wanted =
                (needed as usize).saturating_sub(2 * size_of::<usize>()) / size_of::<HandleEntry>();
            capacity = wanted.max(capacity * 2) + 64;
            continue;
        }
        if status != 0 {
            return Err(io::Error::other(format!(
                "NtQueryInformationProcess(ProcessHandleInformation) failed: {status:#x}"
            )));
        }
        let count = buffer[0].min(capacity);
        let base = buffer[2..].as_ptr().cast::<HandleEntry>();
        return Ok((0..count)
            // SAFETY: the call filled `count` entries after the two counts, all
            // within `buffer`, which is aligned for usize like the entries.
            .map(|index| unsafe { base.add(index).read() })
            .collect());
    }
    Err(io::Error::other(
        "the handle table kept growing while being read",
    ))
}

/// The object type index process handles have on this boot, found from a
/// handle to this very process. Blocking; computed once.
fn process_type_index() -> io::Result<u32> {
    static INDEX: OnceLock<u32> = OnceLock::new();
    if let Some(index) = INDEX.get() {
        return Ok(*index);
    }
    let me = open(std::process::id(), PROCESS_QUERY_LIMITED_INFORMATION)?
        .ok_or_else(|| io::Error::other("this process has no handle to itself"))?;
    let self_handle = open(
        std::process::id(),
        PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE,
    )?
    .ok_or_else(|| io::Error::other("this process has no handle to itself"))?;
    let index = handle_entries(&self_handle)?
        .into_iter()
        .find(|entry| entry.handle_value == raw(&me))
        .map(|entry| entry.object_type_index)
        .ok_or_else(|| io::Error::other("own process handle not in the handle table"))?;
    Ok(*INDEX.get_or_init(|| index))
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    /// This process holds a handle to a child it started, which the child does
    /// not hold to it; a gone PID is `None`.
    #[test]
    fn finds_the_processes_a_process_holds() -> io::Result<()> {
        let mut child = Command::new("cmd.exe")
            .args(["/d", "/c", "ping -n 30 127.0.0.1 >NUL"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let found = (|| {
            let identity = crate::identity(child.id())
                .map_err(io::Error::other)?
                .ok_or_else(|| io::Error::other("the child vanished"))?;
            let mine = held_processes(std::process::id())?.unwrap_or_default();
            let its = held_processes(child.id())?.unwrap_or_default();
            Ok::<_, io::Error>((identity, mine, its))
        })();
        let _ = child.kill();
        let _ = child.wait();
        let (identity, mine, its) = found?;
        assert!(
            mine.iter()
                .any(|held| held.pid == identity.pid && held.start_time == identity.start_time),
            "{mine:?}"
        );
        assert!(
            !its.iter().any(|held| held.pid == std::process::id()),
            "{its:?}"
        );
        assert!(held_processes(0)?.is_none());
        Ok(())
    }
}
