//! Flushing the DNS Client cache after a change.
//!
//! The DNS Client service watches the hosts file and reloads it on its own, so
//! this only shortens the moment a stale negative answer is served. A failure is
//! returned as a warning and never fails the update.

use std::os::windows::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use devlanes_sys::windows::process::CREATE_NO_WINDOW;

/// Longest `ipconfig /flushdns` may take before it is killed.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `%SystemRoot%\System32\ipconfig.exe /flushdns` without a window. The
/// System32 path comes from the API, not the environment. Returns a warning
/// when it could not run or failed.
pub fn flush() -> Option<String> {
    let ipconfig = match devlanes_sys::windows::dirs::system_directory() {
        Ok(directory) => directory.join("ipconfig.exe"),
        Err(error) => return Some(format!("finding System32: {error}")),
    };
    let mut child = match Command::new(&ipconfig)
        .arg("/flushdns")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return Some(format!("running {}: {error}", ipconfig.display())),
    };
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return None,
            Ok(Some(status)) => return Some(format!("ipconfig /flushdns exited with {status}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Some(format!(
                    "ipconfig /flushdns did not finish within {TIMEOUT:?}"
                ));
            }
            Err(error) => return Some(format!("waiting for ipconfig: {error}")),
        }
    }
}
