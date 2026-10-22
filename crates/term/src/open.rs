//! Opening a URL in the user's browser, shared by the CLI (`devlanes open`) and
//! the TUI. The browser is always started by the client, never the daemon, which
//! may run on a remote or headless machine.

#[cfg(any(target_os = "linux", test))]
use crate::Invocation;
use crate::LaunchError;

/// Opens `url` in the default browser with `open` and waits for it to hand the
/// URL over. Blocking, so async callers use `spawn_blocking`.
#[cfg(target_os = "macos")]
pub fn open_url(url: &str) -> Result<(), LaunchError> {
    crate::Invocation::new("open", vec![url.to_owned()], true).run()
}

/// Opens `url` in the default browser; blocking. Outside WSL this runs
/// `xdg-open`. Inside WSL with interop the Windows browser is the one to use,
/// so it tries `wslview`, then Windows' `rundll32.exe url.dll,FileProtocolHandler`,
/// then `xdg-open`, until one succeeds. No opener runs through a shell.
#[cfg(target_os = "linux")]
pub fn open_url(url: &str) -> Result<(), LaunchError> {
    let openers = match devlanes_sys::wsl() {
        Some(info) if info.interop => {
            wsl_openers(url, info.system32.as_deref(), crate::on_path(WSLVIEW))
        }
        _ => vec![xdg_open(url)],
    };
    let mut failure = LaunchError::EmptyCommand;
    for opener in openers {
        match opener.run() {
            Ok(()) => return Ok(()),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

/// Opens `url` in the default browser through the shell (`ShellExecuteExW`),
/// which takes the URL as one string, so `&` is never a command separator.
/// Blocking.
#[cfg(windows)]
pub fn open_url(url: &str) -> Result<(), LaunchError> {
    devlanes_sys::windows::shell::open_url(url).map_err(|source| LaunchError::Spawn {
        program: "the default browser".into(),
        source,
    })
}

/// Opens `url` with the freedesktop opener, as Linux does outside WSL. Blocking.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub fn open_url(url: &str) -> Result<(), LaunchError> {
    crate::Invocation::new("xdg-open", vec![url.to_owned()], true).run()
}

/// wslu's opener, which hands the URL to the Windows default browser.
#[cfg(any(target_os = "linux", test))]
const WSLVIEW: &str = "wslview";

/// The freedesktop opener.
#[cfg(any(target_os = "linux", test))]
fn xdg_open(url: &str) -> Invocation {
    Invocation::new("xdg-open", vec![url.to_owned()], true)
}

/// The openers to try inside WSL, in order: `wslview` when on `PATH`, then
/// `rundll32.exe url.dll,FileProtocolHandler <url>` from `system32`, then
/// `xdg-open`, which a distro may route to Windows itself.
///
/// wslu is archived and its older releases fail where WSL names its binfmt
/// entry `WSLInterop-late`, so `rundll32` stays as the fallback. WSL quotes each
/// argument for the Windows command line only when it has spaces or quotes
/// (checked against `GetCommandLineW`), and `rundll32` hands everything after
/// the entry point to `FileProtocolHandler` without a shell, so `&` and `?` in
/// a URL arrive unchanged.
#[cfg(any(target_os = "linux", test))]
fn wsl_openers(url: &str, system32: Option<&std::path::Path>, wslview: bool) -> Vec<Invocation> {
    let mut openers = Vec::new();
    if wslview {
        openers.push(Invocation::new(WSLVIEW, vec![url.to_owned()], true));
    }
    if let Some(system32) = system32 {
        openers.push(Invocation::new(
            system32.join("rundll32.exe").to_string_lossy(),
            vec!["url.dll,FileProtocolHandler".to_owned(), url.to_owned()],
            true,
        ));
    }
    openers.push(xdg_open(url));
    openers
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const URL: &str = "https://web.acme-2.localhost:8443/search?q=a&page=2";

    /// The argv of each planned opener.
    fn argvs(openers: &[Invocation]) -> Vec<Vec<String>> {
        openers
            .iter()
            .map(|opener| {
                std::iter::once(opener.program.clone())
                    .chain(opener.args.iter().cloned())
                    .collect()
            })
            .collect()
    }

    /// wslview first when installed, then rundll32 from System32, then xdg-open;
    /// the URL is one argument, `&` included, and every opener is waited for.
    #[test]
    fn wsl_openers_prefer_wslview_then_rundll32() {
        let system32 = Path::new("/mnt/c/Windows/System32");
        let rundll32 = system32.join("rundll32.exe").to_string_lossy().into_owned();
        let openers = wsl_openers(URL, Some(system32), true);
        assert_eq!(
            argvs(&openers),
            [
                vec!["wslview", URL],
                vec![rundll32.as_str(), "url.dll,FileProtocolHandler", URL],
                vec!["xdg-open", URL],
            ]
        );
        assert!(openers.iter().all(|opener| opener.wait));
    }

    /// Without wslview or a System32 mount, only what exists is planned.
    #[test]
    fn wsl_openers_skip_what_is_missing() {
        let rundll32 = Path::new("/mnt/c/Windows/System32").join("rundll32.exe");
        assert_eq!(
            argvs(&wsl_openers(
                URL,
                Some(Path::new("/mnt/c/Windows/System32")),
                false
            )),
            [
                vec![
                    rundll32.to_string_lossy().into_owned(),
                    "url.dll,FileProtocolHandler".into(),
                    URL.into()
                ],
                vec!["xdg-open".into(), URL.into()],
            ]
        );
        assert_eq!(
            argvs(&wsl_openers(URL, None, false)),
            [vec!["xdg-open", URL]]
        );
    }
}
