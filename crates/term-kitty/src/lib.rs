//! kitty launcher: `kitty --directory <dir> <command>`, through `open -na kitty.app` on macOS.

use devlanes_term::{Invocation, LaunchError, LaunchRequest, TerminalLauncher};

/// Selected by `terminal: kitty` in the user config.
#[derive(Debug, Clone, Copy, Default)]
pub struct Kitty;

impl TerminalLauncher for Kitty {
    /// Returns the user config value.
    fn name(&self) -> &'static str {
        "kitty"
    }

    /// Plans a kitty window in the request's directory.
    fn invocation(&self, request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        let command = request.require_command()?;
        let mut args = vec!["--directory".into(), request.cwd_arg()];
        if let Some(title) = &request.title {
            args.extend(["--title".into(), title.clone()]);
        }
        args.extend(command.iter().cloned());
        Ok(platform(args))
    }

    /// kitty.app on macOS, the `kitty` binary elsewhere.
    fn available(&self) -> bool {
        #[cfg(target_os = "macos")]
        return devlanes_term::app_installed("kitty.app");
        #[cfg(not(target_os = "macos"))]
        return devlanes_term::on_path("kitty");
    }
}

/// The macOS app bundle does not put `kitty` on PATH by default. `open -n`
/// starts a separate kitty process, so `--single-instance` would be ignored.
#[cfg(target_os = "macos")]
fn platform(args: Vec<String>) -> Invocation {
    let mut open = vec!["-na".into(), "kitty.app".into(), "--args".into()];
    open.extend(args);
    Invocation::new("open", open, true)
}

/// On Linux `--single-instance` opens the window in a running kitty when there
/// is one; the first instance stays in the foreground and is reaped in the background.
#[cfg(not(target_os = "macos"))]
fn platform(args: Vec<String>) -> Invocation {
    let mut single = vec!["--single-instance".to_owned()];
    single.extend(args);
    Invocation::new("kitty", single, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_directory_then_command() -> Result<(), LaunchError> {
        let invocation = Kitty.invocation(&LaunchRequest {
            cwd: "/work/acme".into(),
            command: vec!["devlanes".into(), "attach".into(), "acme@2:api".into()],
            title: Some("api".into()),
            env: Vec::new(),
        })?;
        let args = invocation.args.join(" ");
        assert!(args.ends_with("--directory /work/acme --title api devlanes attach acme@2:api"));
        // `open -na` and `--single-instance` contradict each other.
        let single = invocation.args.iter().any(|arg| arg == "--single-instance");
        assert_eq!(single, invocation.program == "kitty");
        Ok(())
    }
}
