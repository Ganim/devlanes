//! kitty launcher: `kitty --single-instance --directory <dir> <command>`.

use devlanes_term::{Invocation, LaunchError, LaunchRequest, TerminalLauncher};

/// Selected by `terminal: kitty` in the user config.
#[derive(Debug, Clone, Copy, Default)]
pub struct Kitty;

impl TerminalLauncher for Kitty {
    /// Returns the user config value.
    fn name(&self) -> &'static str {
        "kitty"
    }

    /// Plans a kitty window; `--single-instance` reuses a running kitty when present.
    fn invocation(&self, request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        let command = request.require_command()?;
        let mut args = vec![
            "--single-instance".into(),
            "--directory".into(),
            request.cwd_arg(),
        ];
        if let Some(title) = &request.title {
            args.extend(["--title".into(), title.clone()]);
        }
        args.extend(command.iter().cloned());
        Ok(platform(args))
    }
}

/// The macOS app bundle does not put `kitty` on PATH by default.
#[cfg(target_os = "macos")]
fn platform(args: Vec<String>) -> Invocation {
    let mut open = vec!["-na".into(), "kitty.app".into(), "--args".into()];
    open.extend(args);
    Invocation::new("open", open, true)
}

/// On Linux the first kitty instance stays in the foreground; it is reaped in the background.
#[cfg(not(target_os = "macos"))]
fn platform(args: Vec<String>) -> Invocation {
    Invocation::new("kitty", args, false)
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
        })?;
        let args = invocation.args.join(" ");
        assert!(args.ends_with(
            "--single-instance --directory /work/acme --title api devlanes attach acme@2:api"
        ));
        Ok(())
    }
}
