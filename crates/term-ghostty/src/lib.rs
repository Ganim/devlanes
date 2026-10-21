//! Ghostty launcher: a new window with `--working-directory` and `-e`.

use devlanes_term::{Invocation, LaunchError, LaunchRequest, TerminalLauncher};

/// Selected by `terminal: ghostty` in the user config.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ghostty;

impl TerminalLauncher for Ghostty {
    /// Returns the user config value.
    fn name(&self) -> &'static str {
        "ghostty"
    }

    /// Plans the Ghostty command line; `-e` must come last because it takes the rest.
    fn invocation(&self, request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        let command = request.require_command()?;
        let mut args = vec![format!("--working-directory={}", request.cwd_arg())];
        if let Some(title) = &request.title {
            args.push(format!("--title={title}"));
        }
        args.push("-e".into());
        args.extend(command.iter().cloned());
        Ok(platform(args))
    }
}

/// Ghostty's macOS app does not support launching windows from its CLI binary,
/// so the app bundle is started through `open`.
#[cfg(target_os = "macos")]
fn platform(args: Vec<String>) -> Invocation {
    let mut open = vec!["-na".into(), "Ghostty.app".into(), "--args".into()];
    open.extend(args);
    Invocation::new("open", open, true)
}

/// On Linux the `ghostty` binary is the window; it is reaped in the background.
#[cfg(not(target_os = "macos"))]
fn platform(args: Vec<String>) -> Invocation {
    Invocation::new("ghostty", args, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_flag_is_last_before_command() -> Result<(), LaunchError> {
        let invocation = Ghostty.invocation(&LaunchRequest {
            cwd: "/work/acme".into(),
            command: vec!["devlanes".into(), "attach".into(), "acme@2:api".into()],
            title: None,
        })?;
        let tail: Vec<&str> = invocation
            .args
            .iter()
            .rev()
            .take(4)
            .rev()
            .map(String::as_str)
            .collect();
        assert_eq!(tail, ["-e", "devlanes", "attach", "acme@2:api"]);
        assert!(
            invocation
                .args
                .contains(&"--working-directory=/work/acme".to_owned())
        );
        Ok(())
    }
}
