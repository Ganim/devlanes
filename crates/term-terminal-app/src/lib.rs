//! Terminal.app launcher: `do script` opens a new window running the command.

#[cfg(target_os = "macos")]
use devlanes_term::applescript_string;
use devlanes_term::{Invocation, LaunchError, LaunchRequest, TerminalLauncher};

/// Selected by `terminal: terminal` in the user config.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalApp;

impl TerminalLauncher for TerminalApp {
    /// Returns the user config value.
    fn name(&self) -> &'static str {
        "terminal"
    }

    /// Plans an `osascript` call; Terminal.app is scriptable only through AppleScript.
    #[cfg(target_os = "macos")]
    fn invocation(&self, request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        let line = request.shell_line()?;
        let mut script = format!(
            "tell application \"Terminal\"\nactivate\nset t to do script {}\n",
            applescript_string(&line)
        );
        if let Some(title) = &request.title {
            script.push_str(&format!(
                "set custom title of t to {}\n",
                applescript_string(title)
            ));
        }
        script.push_str("end tell");
        Ok(Invocation::new(
            "osascript",
            vec!["-e".into(), script],
            true,
        ))
    }

    /// Terminal.app exists only on macOS.
    #[cfg(not(target_os = "macos"))]
    fn invocation(&self, _request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        Err(LaunchError::Unsupported {
            terminal: "Terminal.app",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LaunchRequest {
        LaunchRequest {
            cwd: "/work/acme".into(),
            command: vec!["devlanes".into(), "attach".into(), "acme@2:api".into()],
            title: None,
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plans_do_script() -> Result<(), LaunchError> {
        let invocation = TerminalApp.invocation(&request())?;
        assert_eq!(invocation.program, "osascript");
        assert!(
            invocation.args[1]
                .contains("do script \"cd /work/acme && devlanes attach acme@2:api\"")
        );
        assert!(!invocation.args[1].contains("custom title"));
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_off_macos() {
        assert!(matches!(
            TerminalApp.invocation(&request()),
            Err(LaunchError::Unsupported { .. })
        ));
    }
}
