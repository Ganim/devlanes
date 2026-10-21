//! iTerm2 launcher: a new window that types the command into its shell.

#[cfg(target_os = "macos")]
use devlanes_term::applescript_string;
use devlanes_term::{Invocation, LaunchError, LaunchRequest, TerminalLauncher};

/// Selected by `terminal: iterm2` in the user config.
#[derive(Debug, Clone, Copy, Default)]
pub struct Iterm2;

impl TerminalLauncher for Iterm2 {
    /// Returns the user config value.
    fn name(&self) -> &'static str {
        "iterm2"
    }

    /// Plans an `osascript` call; iTerm2 has no stable CLI for new windows.
    #[cfg(target_os = "macos")]
    fn invocation(&self, request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        let line = request.shell_line()?;
        let mut script = String::from(
            "tell application \"iTerm\"\nactivate\nset w to (create window with default profile)\ntell current session of w\n",
        );
        if let Some(title) = &request.title {
            script.push_str(&format!("set name to {}\n", applescript_string(title)));
        }
        // write text goes through the user's login shell, so their PATH and rc files apply.
        script.push_str(&format!(
            "write text {}\nend tell\nend tell",
            applescript_string(&line)
        ));
        Ok(Invocation::new(
            "osascript",
            vec!["-e".into(), script],
            true,
        ))
    }

    /// iTerm2 exists only on macOS.
    #[cfg(not(target_os = "macos"))]
    fn invocation(&self, _request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        Err(LaunchError::Unsupported { terminal: "iTerm2" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LaunchRequest {
        LaunchRequest {
            cwd: "/work/acme".into(),
            command: vec!["devlanes".into(), "attach".into(), "acme@2:api".into()],
            title: Some("acme@2:api".into()),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plans_osascript_with_cd_and_title() -> Result<(), LaunchError> {
        let invocation = Iterm2.invocation(&request())?;
        assert_eq!(invocation.program, "osascript");
        assert!(invocation.wait);
        let script = &invocation.args[1];
        assert!(script.contains("create window with default profile"));
        assert!(script.contains("set name to \"acme@2:api\""));
        assert!(script.contains("write text \"cd /work/acme && devlanes attach acme@2:api\""));
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_off_macos() {
        assert!(matches!(
            Iterm2.invocation(&request()),
            Err(LaunchError::Unsupported { .. })
        ));
    }
}
