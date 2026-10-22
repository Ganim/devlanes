//! Plain text on the Windows clipboard, for the TUI's copies: classic console
//! windows (conhost) drop the OSC 52 sequence terminals use for it.

use std::io;
use std::ptr;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

/// `CF_UNICODETEXT`: NUL-terminated UTF-16 with CRLF line ends.
const CF_UNICODETEXT: u32 = 13;
/// Tries to open the clipboard while another program holds it open.
const OPEN_ATTEMPTS: u32 = 10;
/// Pause between tries; holders (clipboard managers, Office) keep it for
/// milliseconds, so the whole wait stays under a quarter second.
const OPEN_PAUSE: Duration = Duration::from_millis(20);

/// Replaces the clipboard's contents with `text`. Blocks for up to about
/// 200 ms while another program has the clipboard open, so call it off async
/// executor threads.
pub fn set_text(text: &str) -> io::Result<()> {
    let units = encode(text);
    let memory = Global::copy_of(&units)?;
    let _open = retrying(OPEN_ATTEMPTS, OPEN_PAUSE, Open::new)?;
    // SAFETY: the clipboard is open by this thread (`_open`).
    if unsafe { EmptyClipboard() } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the clipboard is open and emptied by this thread; `memory` is
    // an unlocked movable allocation holding NUL-terminated UTF-16.
    if unsafe { SetClipboardData(CF_UNICODETEXT, memory.0 as HANDLE) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    // The clipboard owns the allocation now and frees it on the next change.
    std::mem::forget(memory);
    Ok(())
}

/// `text` as `CF_UNICODETEXT` wants it: UTF-16, every line end CRLF (Windows
/// programs show a bare LF as nothing or a box), and a terminating NUL.
fn encode(text: &str) -> Vec<u16> {
    let mut units = Vec::with_capacity(text.len() + 1);
    let mut previous = '\0';
    for c in text.chars() {
        if c == '\n' && previous != '\r' {
            units.push(u16::from(b'\r'));
        }
        let mut buffer = [0; 2];
        units.extend_from_slice(c.encode_utf16(&mut buffer));
        previous = c;
    }
    units.push(0);
    units
}

/// Calls `attempt` up to `attempts` times, sleeping `pause` between failures,
/// and returns the first success or the last error.
fn retrying<T>(
    attempts: u32,
    pause: Duration,
    mut attempt: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut tries = 1;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) if tries >= attempts => return Err(error),
            Err(_) => {
                tries += 1;
                thread::sleep(pause);
            }
        }
    }
}

/// The open clipboard, closed on drop.
struct Open;

impl Open {
    /// Opens the clipboard for this thread; fails while another window has
    /// it open.
    fn new() -> io::Result<Self> {
        // SAFETY: a null owner is allowed; the clipboard is then owned by no
        // window, which only rules out delayed rendering (not used here).
        if unsafe { OpenClipboard(ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard in `Open::new`.
        unsafe { CloseClipboard() };
    }
}

/// A movable global allocation, freed on drop unless handed to the clipboard.
struct Global(HGLOBAL);

impl Global {
    /// A new allocation holding `units`.
    fn copy_of(units: &[u16]) -> io::Result<Self> {
        let bytes = size_of_val(units);
        // SAFETY: plain allocation call; the result is checked below.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let memory = Self(handle);
        // SAFETY: `handle` is a live movable allocation owned by `memory`.
        let target = unsafe { GlobalLock(handle) }.cast::<u16>();
        if target.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the lock gives at least `bytes` writable bytes, which hold
        // `units.len()` u16s; source and target cannot overlap. The unlock
        // pairs with the lock above; its result only reports the lock count.
        unsafe {
            ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
            GlobalUnlock(handle);
        }
        Ok(memory)
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        // SAFETY: the allocation is unlocked and still owned here; `set_text`
        // forgets it once the clipboard owns it.
        unsafe { GlobalFree(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(units: &[u16]) -> String {
        String::from_utf16(units).expect("valid UTF-16")
    }

    #[test]
    fn text_is_nul_terminated_utf16_with_crlf_line_ends() {
        let units = encode("a\nb\r\nc\n");
        assert_eq!(units.last(), Some(&0));
        assert_eq!(text(&units[..units.len() - 1]), "a\r\nb\r\nc\r\n");
        // Characters outside the BMP become surrogate pairs.
        let units = encode("é😀");
        assert_eq!(units, [0xe9, 0xd83d, 0xde00, 0]);
        assert_eq!(encode(""), [0]);
        // A lone CR stays as it is.
        assert_eq!(text(&encode("a\rb")[..3]), "a\rb");
    }

    #[test]
    fn opening_retries_while_the_clipboard_is_busy() {
        let busy = || io::Error::from_raw_os_error(5);
        let mut calls = 0;
        let opened = retrying(3, Duration::ZERO, || {
            calls += 1;
            if calls < 3 { Err(busy()) } else { Ok(calls) }
        });
        assert_eq!(opened.expect("third try"), 3);
        let mut calls = 0;
        let failed = retrying(3, Duration::ZERO, || -> io::Result<()> {
            calls += 1;
            Err(busy())
        });
        assert_eq!(failed.expect_err("busy").raw_os_error(), Some(5));
        assert_eq!(calls, 3, "gives up after the last try");
    }

    #[test]
    fn allocations_hold_the_encoded_text() {
        let units = encode("acme\nweb");
        let memory = Global::copy_of(&units).expect("allocation");
        // SAFETY: the allocation is live and owned by `memory`; the lock is
        // released before it is freed.
        let copied = unsafe {
            let source = GlobalLock(memory.0).cast::<u16>();
            assert!(!source.is_null());
            let copied = std::slice::from_raw_parts(source, units.len()).to_vec();
            GlobalUnlock(memory.0);
            copied
        };
        assert_eq!(copied, units);
    }
}
