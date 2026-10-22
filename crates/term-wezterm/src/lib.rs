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
        let mut args = vec!["start".to_owned()];
        args.extend(START_FLAGS.iter().map(|flag| (*flag).to_owned()));
        args.extend(["--cwd".into(), request.cwd_arg(), "--".into()]);
        args.extend(command.iter().cloned());
        Ok(platform(args, &request.env))
    }

    /// WezTerm.app on macOS, the `wezterm` binary elsewhere.
    fn available(&self) -> bool {
        #[cfg(target_os = "macos")]
        return devlanes_term::app_installed("WezTerm.app");
        #[cfg(not(target_os = "macos"))]
        return devlanes_term::on_path("wezterm");
    }
}

/// Extra `wezterm start` flags: none outside Windows.
#[cfg(not(windows))]
const START_FLAGS: &[&str] = &[];

/// Extra `wezterm start` flags. On Windows a running WezTerm GUI would open the
/// window itself, with its own environment, so the request's `env`
/// (`DEVLANES_HOME`) would not reach the command; a new process carries it.
#[cfg(windows)]
const START_FLAGS: &[&str] = &["--always-new-process"];

/// The macOS app bundle does not put `wezterm` on PATH by default. `open`
/// starts the app with launchd's environment, so `env` cannot reach it.
#[cfg(target_os = "macos")]
fn platform(args: Vec<String>, _env: &[(String, String)]) -> Invocation {
    let mut open = vec!["-na".into(), "WezTerm.app".into(), "--args".into()];
    open.extend(args);
    Invocation::new("open", open, true)
}

/// On Linux and Windows `wezterm start` may stay in the foreground; it is
/// reaped in the background. A new WezTerm process hands its environment to
/// the command, which is how `env` reaches it on Windows.
#[cfg(not(target_os = "macos"))]
fn platform(args: Vec<String>, env: &[(String, String)]) -> Invocation {
    Invocation::new("wezterm", args, false).with_env(env)
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
            env: Vec::new(),
        })?;
        let args = invocation.args.join(" ");
        assert!(args.ends_with("--cwd /work/acme -- devlanes attach acme@2:api"));
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn reuses_a_running_gui_outside_windows() -> Result<(), LaunchError> {
        let invocation = Wezterm.invocation(&LaunchRequest {
            cwd: "/work/acme".into(),
            command: vec!["devlanes".into()],
            title: None,
            env: Vec::new(),
        })?;
        assert!(
            invocation
                .args
                .join(" ")
                .ends_with("start --cwd /work/acme -- devlanes")
        );
        Ok(())
    }

    /// The new window must come from a new process, which has `env`.
    #[cfg(windows)]
    #[test]
    fn starts_a_new_process_on_windows() -> Result<(), LaunchError> {
        let invocation = Wezterm.invocation(&LaunchRequest {
            cwd: r"C:\work\acme".into(),
            command: vec!["devlanes.exe".into()],
            title: None,
            env: Vec::new(),
        })?;
        assert_eq!(invocation.program, "wezterm");
        assert_eq!(
            invocation.args,
            [
                "start",
                "--always-new-process",
                "--cwd",
                r"C:\work\acme",
                "--",
                "devlanes.exe"
            ]
        );
        Ok(())
    }

    /// Outside macOS the helper itself carries the variables to the new window.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn passes_env_to_the_wezterm_process() -> Result<(), LaunchError> {
        let env = vec![(
            "DEVLANES_HOME".to_owned(),
            r"C:\Users\acme\.devlanes".to_owned(),
        )];
        let invocation = Wezterm.invocation(&LaunchRequest {
            cwd: "/work/acme".into(),
            command: vec!["devlanes".into(), "attach".into(), "acme@2:api".into()],
            title: None,
            env: env.clone(),
        })?;
        assert_eq!(invocation.env, env);
        Ok(())
    }
}
