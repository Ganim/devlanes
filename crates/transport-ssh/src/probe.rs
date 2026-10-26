//! Finding devlanes on the box. The box installs devlanes itself; a box
//! without it is reported, never changed.

use crate::sh_quote;

/// Where the probe looks for the binary after `PATH`, relative to the remote `$HOME`.
pub const INSTALL_DIR: &str = ".devlanes/bin";

/// Where the error for a box without devlanes points the user.
pub(crate) const SETUP_GUIDE: &str =
    "https://github.com/devlanes-run/devlanes/blob/main/docs/guides/remote-setup.md#on-the-box";

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

/// What every probe line starts with, so output a login shell or profile file
/// prints around it is told apart.
const PROBE_TAG: &str = "devlanes-probe:";

/// Builds the probe script: it prints tagged `os=`, `arch=` and `program=`
/// lines, the last one empty when devlanes is not found, and leaves the program
/// found in `p`. The newline first puts the tags on lines of their own even
/// after login output without a final newline.
pub(crate) fn probe_script(program: &str) -> String {
    let program = sh_quote(program);
    format!(
        "if command -v {program} >/dev/null 2>&1; then p=$(command -v {program}); \
         elif [ -x \"$HOME/{INSTALL_DIR}/devlanes\" ]; then p=\"$HOME/{INSTALL_DIR}/devlanes\"; \
         else p=; fi; \
         printf '\\n{PROBE_TAG}os=%s\\n{PROBE_TAG}arch=%s\\n{PROBE_TAG}program=%s\\n' \
         \"$(uname -s)\" \"$(uname -m)\" \"$p\""
    )
}

/// Parses probe output, ignoring lines that are not the probe's own; `None`
/// when the platform lines are missing.
pub(crate) fn parse_probe(output: &str) -> Option<RemoteProbe> {
    let mut sysname = None;
    let mut machine = None;
    let mut program = None;
    for line in output.lines() {
        let Some((key, value)) = line
            .trim_end_matches('\r')
            .strip_prefix(PROBE_TAG)
            .and_then(|rest| rest.split_once('='))
        else {
            continue;
        };
        // The last of each wins: the probe prints after any login output.
        match key {
            "os" => sysname = Some(value),
            "arch" => machine = Some(value),
            "program" => program = Some(value.trim()),
            _ => {}
        }
    }
    Some(RemoteProbe {
        platform: RemotePlatform::from_uname(sysname?, machine?),
        program: program.filter(|p| !p.is_empty()).map(str::to_owned),
    })
}

/// Builds the script a Windows session runs: a `start` line, what
/// [`probe_script`] prints, an `end` line, then `exec <program> tunnel
/// --stdio` when devlanes was found. Everything after `end` belongs to the
/// tunnel, which writes its preamble first, so probe and tunnel share one ssh
/// connection and one authentication. The newline before `start` puts it on a
/// line of its own even after login output without a final newline.
#[cfg(windows)]
pub(crate) fn session_script(program: &str, start: &str, end: &str) -> String {
    let probe = probe_script(program);
    let start = sh_quote(start);
    let end = sh_quote(end);
    format!(
        "printf '\\n%s\\n' {start}; {probe}; printf '%s\\n' {end}; \
         if [ -n \"$p\" ]; then exec \"$p\" tunnel --stdio; fi"
    )
}

/// Parses the probe lines a [`session_script`] printed after its last `start`
/// line, so whatever a login shell's startup files print first is ignored.
#[cfg(windows)]
pub(crate) fn parse_session_header(lines: &[String], start: &str) -> Option<RemoteProbe> {
    let at = lines.iter().rposition(|line| line == start)?;
    parse_probe(&lines[at + 1..].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_parses_missing_and_present_binaries() {
        let missing = parse_probe(
            "\ndevlanes-probe:os=Linux\ndevlanes-probe:arch=x86_64\ndevlanes-probe:program=\n",
        );
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
        let present = parse_probe(
            "devlanes-probe:os=Darwin\r\ndevlanes-probe:arch=arm64\r\n\
             devlanes-probe:program=/usr/local/bin/devlanes\r\n",
        );
        assert_eq!(
            present.as_ref().map(|p| p.platform.arch.as_str()),
            Some("aarch64")
        );
        assert_eq!(
            present.and_then(|p| p.program).as_deref(),
            Some("/usr/local/bin/devlanes")
        );
        assert_eq!(parse_probe("devlanes-probe:os=Linux\n"), None);
    }

    /// Output a login shell or profile file prints around the probe (Tailscale
    /// SSH runs `su -l`) is not the probe's.
    #[test]
    fn probe_output_ignores_other_lines() {
        let probe = parse_probe(
            "Last login: Mon Oct 26\nLinux\nwelcome to acme\n\
             devlanes-probe:os=Linux\ndevlanes-probe:arch=aarch64\n\
             devlanes-probe:program=/home/acme/.devlanes/bin/devlanes\nlogout\n",
        );
        assert_eq!(
            probe,
            Some(RemoteProbe {
                platform: RemotePlatform {
                    os: "linux".into(),
                    arch: "aarch64".into()
                },
                program: Some("/home/acme/.devlanes/bin/devlanes".into()),
            })
        );
        assert_eq!(parse_probe("Linux\nx86_64\n/usr/bin/devlanes\n"), None);
        let script = probe_script("devlanes");
        assert!(
            script.contains("printf '\\ndevlanes-probe:os=%s\\n"),
            "{script}"
        );
    }

    /// Windows sessions read the probe from the lines after the start line,
    /// whatever a login shell printed first.
    #[cfg(windows)]
    #[test]
    fn session_headers_use_the_lines_after_the_start() {
        let lines = |text: &str| text.lines().map(str::to_owned).collect::<Vec<_>>();
        let present = parse_session_header(
            &lines(
                "welcome from .bashrc\nno newline here\nstart-1\n\ndevlanes-probe:os=Linux\n\
                 devlanes-probe:arch=x86_64\ndevlanes-probe:program=/home/acme/.devlanes/bin/devlanes",
            ),
            "start-1",
        );
        assert_eq!(
            present.and_then(|probe| probe.program).as_deref(),
            Some("/home/acme/.devlanes/bin/devlanes")
        );
        let missing = parse_session_header(
            &lines(
                "start-1\ndevlanes-probe:os=Linux\ndevlanes-probe:arch=aarch64\ndevlanes-probe:program=",
            ),
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
                &lines("motdstart-1\ndevlanes-probe:os=Linux\ndevlanes-probe:arch=x86_64"),
                "start-1"
            ),
            None
        );
        let script = session_script("devlanes", "start-1", "ready-1");
        assert!(
            script.starts_with("printf '\\n%s\\n' 'start-1'; if command -v"),
            "{script}"
        );
        assert!(script.contains("printf '%s\\n' 'ready-1'"), "{script}");
        assert!(
            script.ends_with("exec \"$p\" tunnel --stdio; fi"),
            "{script}"
        );
    }
}
