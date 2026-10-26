//! Finding devlanes on the box. The box installs devlanes itself; a box
//! without it is reported, never changed.

use crate::sh_quote;

/// Where the probe looks for the binary after `PATH`, relative to the remote `$HOME`.
pub const INSTALL_DIR: &str = ".devlanes/bin";

/// Where the error for a box without devlanes points the user.
pub(crate) const SETUP_GUIDE: &str =
    "https://github.com/devlanes-run/devlanes/blob/main/docs/guides/remote-setup.md";

/// Normalized OS and architecture reported by `uname`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlatform {
    /// `linux`, `macos`, or the raw lowercase `uname -s` value.
    pub os: String,
    /// `x86_64`, `aarch64`, or the raw `uname -m` value.
    pub arch: String,
}

impl RemotePlatform {
    /// Maps `uname -s` / `uname -m` output onto short names.
    pub fn from_uname(sysname: &str, machine: &str) -> Self {
        let os = match sysname.trim() {
            "Linux" => "linux".to_owned(),
            "Darwin" => "macos".to_owned(),
            other => other.to_ascii_lowercase(),
        };
        let arch = match machine.trim() {
            "x86_64" | "amd64" => "x86_64".to_owned(),
            "aarch64" | "arm64" => "aarch64".to_owned(),
            other => other.to_owned(),
        };
        Self { os, arch }
    }
}

/// What the box reported about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProbe {
    /// Box platform.
    pub platform: RemotePlatform,
    /// Path or name to run, or `None` when devlanes is not installed.
    pub program: Option<String>,
}

/// Builds the probe script; prints `uname -s`, `uname -m`, then the program or an empty line.
pub(crate) fn probe_script(program: &str) -> String {
    let program = sh_quote(program);
    format!(
        "uname -s; uname -m; \
         if command -v {program} >/dev/null 2>&1; then command -v {program}; \
         elif [ -x \"$HOME/{INSTALL_DIR}/devlanes\" ]; then printf '%s\\n' \"$HOME/{INSTALL_DIR}/devlanes\"; \
         else echo; fi"
    )
}

/// Parses probe output; `None` when it does not have the expected shape.
pub(crate) fn parse_probe(output: &str) -> Option<RemoteProbe> {
    let mut lines = output.lines();
    let sysname = lines.next()?;
    let machine = lines.next()?;
    let program = lines
        .next()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned);
    Some(RemoteProbe {
        platform: RemotePlatform::from_uname(sysname, machine),
        program,
    })
}

/// Builds the script a Windows session runs: a `start` line, what
/// [`probe_script`] prints, an `end` line, then `exec <program> tunnel
/// --stdio` when devlanes was found. Everything after `end` belongs to the
/// tunnel, so probe and tunnel share one ssh connection and one
/// authentication. The newline before `start` puts it on a line of its own
/// even after login output without a final newline.
#[cfg(windows)]
pub(crate) fn session_script(program: &str, start: &str, end: &str) -> String {
    let program = sh_quote(program);
    let start = sh_quote(start);
    let end = sh_quote(end);
    format!(
        "printf '\\n%s\\n' {start}; uname -s; uname -m; \
         if command -v {program} >/dev/null 2>&1; then p=$(command -v {program}); \
         elif [ -x \"$HOME/{INSTALL_DIR}/devlanes\" ]; then p=\"$HOME/{INSTALL_DIR}/devlanes\"; \
         else p=; fi; \
         printf '%s\\n' \"$p\" {end}; \
         if [ -n \"$p\" ]; then exec \"$p\" tunnel --stdio; fi"
    )
}

/// Parses the lines a [`session_script`] printed before its end line: exactly
/// the three after the last `start` line, so whatever a login shell's startup
/// files print first is ignored.
#[cfg(windows)]
pub(crate) fn parse_session_header(lines: &[String], start: &str) -> Option<RemoteProbe> {
    let at = lines.iter().rposition(|line| line == start)?;
    let probe = &lines[at + 1..];
    if probe.len() != 3 {
        return None;
    }
    parse_probe(&probe.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_parses_missing_and_present_binaries() {
        let missing = parse_probe("Linux\nx86_64\n\n");
        assert_eq!(
            missing,
            Some(RemoteProbe {
                platform: RemotePlatform {
                    os: "linux".into(),
                    arch: "x86_64".into()
                },
                program: None,
            })
        );
        let present = parse_probe("Darwin\narm64\n/usr/local/bin/devlanes\n");
        assert_eq!(
            present.as_ref().map(|p| p.platform.arch.as_str()),
            Some("aarch64")
        );
        assert_eq!(
            present.and_then(|p| p.program).as_deref(),
            Some("/usr/local/bin/devlanes")
        );
        assert_eq!(parse_probe("Linux\n"), None);
    }

    /// Windows sessions read the probe from the three lines between the start
    /// and end lines, whatever a login shell printed first.
    #[cfg(windows)]
    #[test]
    fn session_headers_use_the_lines_after_the_start() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let present = parse_session_header(
            &lines(
                "welcome from .bashrc\nno newline here\nstart-1\nLinux\nx86_64\n/home/acme/.devlanes/bin/devlanes",
            ),
            "start-1",
        );
        assert_eq!(
            present.and_then(|probe| probe.program).as_deref(),
            Some("/home/acme/.devlanes/bin/devlanes")
        );
        let missing = parse_session_header(&lines("start-1\nLinux\naarch64\n"), "start-1");
        assert_eq!(missing, None, "a missing program is still a line");
        let missing = parse_session_header(
            &[
                "start-1".into(),
                "Linux".into(),
                "aarch64".into(),
                String::new(),
            ],
            "start-1",
        );
        assert_eq!(
            missing,
            Some(RemoteProbe {
                platform: RemotePlatform {
                    os: "linux".into(),
                    arch: "aarch64".into()
                },
                program: None,
            })
        );
        // Login output glued to the start line is not the start line.
        assert_eq!(
            parse_session_header(
                &lines("motdstart-1\nLinux\nx86_64\n/bin/devlanes"),
                "start-1"
            ),
            None
        );
        assert_eq!(
            parse_session_header(&lines("start-1\nextra\nLinux\nx86_64\n/bin/d"), "start-1"),
            None
        );
        let script = session_script("devlanes", "start-1", "ready-1");
        assert!(
            script.starts_with("printf '\\n%s\\n' 'start-1'; uname -s;"),
            "{script}"
        );
        assert!(
            script.contains("printf '%s\\n' \"$p\" 'ready-1'"),
            "{script}"
        );
        assert!(
            script.ends_with("exec \"$p\" tunnel --stdio; fi"),
            "{script}"
        );
    }
}
