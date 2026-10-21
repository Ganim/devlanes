//! WezTerm launcher: `wezterm start --cwd <dir> -- <command>`.

use devlanes_term::{Invocation, LaunchError, LaunchRequest, TerminalLauncher};

/// Selected by `terminal: wezterm` in the user config.
#[derive(Debug, Clone, Copy, Default)]
pub struct Wezterm;

impl TerminalLauncher for Wezterm {
    /// Returns the user config value.
    fn name(&self) -> &'static str {
        "wezterm"
    }

    /// Plans `wezterm start`, which opens a window in a running GUI when there is one.
    /// The title is ignored: `wezterm start` has no title flag.
    fn invocation(&self, request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        let command = request.require_command()?;
        let mut args = vec!["start".into(), "--cwd".into(), request.cwd_arg()];
        args.push("--".into());
        args.extend(command.iter().cloned());
        Ok(platform(args))
    }

    /// WezTerm.app on macOS, the `wezterm` binary elsewhere.
    fn available(&self) -> bool {
        #[cfg(target_os = "macos")]
        return devlanes_term::app_installed("WezTerm.app");
        #[cfg(not(target_os = "macos"))]
        return devlanes_term::on_path("wezterm");
    }
}

/// The macOS app bundle does not put `wezterm` on PATH by default.
#[cfg(target_os = "macos")]
fn platform(args: Vec<String>) -> Invocation {
    let mut open = vec!["-na".into(), "WezTerm.app".into(), "--args".into()];
    open.extend(args);
    Invocation::new("open", open, true)
}

/// On Linux `wezterm start` may stay in the foreground; it is reaped in the background.
#[cfg(not(target_os = "macos"))]
fn platform(args: Vec<String>) -> Invocation {
    Invocation::new("wezterm", args, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separates_command_with_double_dash() -> Result<(), LaunchError> {
        let invocation = Wezterm.invocation(&LaunchRequest {
            cwd: "/work/acme".into(),
            command: vec!["devlanes".into(), "attach".into(), "acme@2:api".into()],
            title: None,
        })?;
        let args = invocation.args.join(" ");
        assert!(args.ends_with("start --cwd /work/acme -- devlanes attach acme@2:api"));
        Ok(())
    }
}
