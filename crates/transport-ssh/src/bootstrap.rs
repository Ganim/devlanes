//! Detecting devlanes on the box and installing it when missing.
//!
//! Installation only happens when no binary exists; a present binary is never
//! replaced, so a running box daemon is never upgraded underneath its
//! processes. Version skew surfaces as a protocol error at handshake instead.

use serde::{Deserialize, Serialize};

use crate::sh_quote;

/// Where bootstrap installs the binary, relative to the remote `$HOME`.
pub const INSTALL_DIR: &str = ".devlanes/bin";

/// Exit status of the install script when a binary already exists.
pub(crate) const EXIT_ALREADY_INSTALLED: i32 = 10;

/// Release artifacts the client may install, normally embedded in the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// Release version these artifacts belong to.
    pub version: String,
    /// One entry per supported platform.
    pub artifacts: Vec<Artifact>,
}

/// One downloadable build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Normalized OS: `linux` or `macos`.
    pub os: String,
    /// Normalized architecture: `x86_64` or `aarch64`.
    pub arch: String,
    /// HTTPS download URL, fetched on the box.
    pub url: String,
    /// Lowercase hex SHA-256 of the downloaded file.
    pub sha256: String,
    /// Packaging of the download.
    pub format: ArtifactFormat,
}

/// How an artifact is packaged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ArtifactFormat {
    /// The file is the executable.
    Binary,
    /// A gzip tarball containing the executable at `member`.
    TarGz {
        /// Relative path of the executable inside the archive.
        member: String,
    },
}

/// Why a manifest or artifact cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid release manifest: {0}")]
pub struct ManifestError(pub String);

impl ReleaseManifest {
    /// Parses and validates a JSON manifest.
    pub fn from_json(json: &str) -> Result<Self, ManifestError> {
        let manifest: Self =
            serde_json::from_str(json).map_err(|error| ManifestError(error.to_string()))?;
        for artifact in &manifest.artifacts {
            artifact.validate()?;
        }
        Ok(manifest)
    }

    /// Finds the artifact for `platform`.
    pub fn select(&self, platform: &RemotePlatform) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|a| a.os == platform.os && a.arch == platform.arch)
    }
}

impl Artifact {
    /// Rejects values that would be unsafe to splice into the install script.
    ///
    /// Only HTTPS URLs are allowed so the checksum is not the only integrity check.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !self.url.starts_with("https://") {
            return Err(ManifestError(format!("{} is not an https URL", self.url)));
        }
        self.validate_script_inputs()
    }

    /// Checks the fields that reach the shell, independent of URL scheme.
    pub(crate) fn validate_script_inputs(&self) -> Result<(), ManifestError> {
        let hex = self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex {
            return Err(ManifestError(format!(
                "sha256 for {}/{} must be 64 lowercase hex digits",
                self.os, self.arch
            )));
        }
        if let ArtifactFormat::TarGz { member } = &self.format {
            let safe = !member.is_empty()
                && !member.starts_with('/')
                && !member.split('/').any(|part| part == "..")
                && member
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b));
            if !safe {
                return Err(ManifestError(format!("unsafe archive member {member:?}")));
            }
        }
        Ok(())
    }
}

/// Normalized OS and architecture reported by `uname`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePlatform {
    /// `linux`, `macos`, or the raw lowercase `uname -s` value.
    pub os: String,
    /// `x86_64`, `aarch64`, or the raw `uname -m` value.
    pub arch: String,
}

impl RemotePlatform {
    /// Maps `uname -s` / `uname -m` output onto manifest names.
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

/// What to do when devlanes is missing on a box.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BootstrapPolicy {
    /// Fail with [`crate::SshError::BootstrapRequired`] so the user can approve.
    #[default]
    Report,
    /// Install from the configured manifest, then continue.
    Install,
}

/// A pending installation, returned when devlanes is missing on the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapPlan {
    /// The ssh destination.
    pub destination: String,
    /// Box platform.
    pub platform: RemotePlatform,
    /// Matching artifact, or `None` when the manifest has none (or none was given).
    pub artifact: Option<Artifact>,
    /// Why the release manifest could not be fetched, when it could not.
    pub manifest_error: Option<String>,
}

impl BootstrapPlan {
    /// Remote install path, with `$HOME` expanded by the remote shell.
    pub fn install_path(&self) -> String {
        format!("$HOME/{INSTALL_DIR}/devlanes")
    }

    /// User-facing next steps.
    pub fn instructions(&self) -> String {
        match &self.artifact {
            Some(artifact) => format!(
                "devlanes is not installed on {}. Run `devlanes remote connect {} --install` \
                 to install {} ({}/{}) to {}, then run `devlanes daemon install` there and, \
                 on Linux, `loginctl enable-linger` so the box daemon survives logout",
                self.destination,
                self.destination,
                artifact.url,
                self.platform.os,
                self.platform.arch,
                self.install_path()
            ),
            None => {
                let why = match &self.manifest_error {
                    Some(error) => format!("the release list could not be read ({error})"),
                    None => format!(
                        "no release matches {}/{}",
                        self.platform.os, self.platform.arch
                    ),
                };
                format!(
                    "devlanes is not installed on {} and {why}. \
                     Install it there manually so `devlanes` is on PATH or at {}",
                    self.destination,
                    self.install_path()
                )
            }
        }
    }
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

/// Builds the install script run on the box with `sh -c`.
///
/// Downloads over HTTPS only (redirects included) with curl or wget, verifies
/// SHA-256 with `sha256sum` or `shasum -a 256`, and places the binary with a hard
/// link, which fails instead of replacing a binary that appeared meanwhile.
pub(crate) fn install_script(artifact: &Artifact) -> Result<String, ManifestError> {
    install_script_for(artifact, "=https")
}

/// [`install_script`] with curl's `--proto` value; tests allow `file` too.
fn install_script_for(artifact: &Artifact, protocols: &str) -> Result<String, ManifestError> {
    artifact.validate_script_inputs()?;
    let url = sh_quote(&artifact.url);
    let sha = &artifact.sha256;
    let protocols = sh_quote(protocols);
    let built = match &artifact.format {
        ArtifactFormat::Binary => "bin=\"$tmp\"".to_owned(),
        ArtifactFormat::TarGz { member } => {
            let member = sh_quote(member);
            format!(
                "mkdir \"$tmp.x\"; tar -xzf \"$tmp\" -C \"$tmp.x\" {member}; bin=\"$tmp.x\"/{member}"
            )
        }
    };
    Ok(format!(
        "set -eu\n\
         dir=\"$HOME/{INSTALL_DIR}\"; dest=\"$dir/devlanes\"\n\
         exists() {{ echo \"devlanes already exists at $dest; not replacing it\" >&2; exit {EXIT_ALREADY_INSTALLED}; }}\n\
         if [ -e \"$dest\" ]; then exists; fi\n\
         mkdir -p \"$dir\"\n\
         tmp=$(mktemp \"$dir/.download.XXXXXX\")\n\
         trap 'rm -rf \"$tmp\" \"$tmp.x\"' EXIT\n\
         if command -v curl >/dev/null 2>&1; then curl -fsSL --proto {protocols} --proto-redir {protocols} -o \"$tmp\" {url}\n\
         elif command -v wget >/dev/null 2>&1; then wget -q --https-only -O \"$tmp\" {url}\n\
         else echo 'curl or wget is required to install devlanes' >&2; exit 11; fi\n\
         sum=$( (sha256sum \"$tmp\" 2>/dev/null || shasum -a 256 \"$tmp\") | cut -d ' ' -f 1)\n\
         if [ \"$sum\" != {sha} ]; then echo \"sha256 mismatch: expected {sha}, got $sum\" >&2; exit 12; fi\n\
         {built}\n\
         chmod 755 \"$bin\"\n\
         if ! ln \"$bin\" \"$dest\" 2>/dev/null; then if [ -e \"$dest\" ]; then exists; fi; echo \"cannot place devlanes at $dest\" >&2; exit 13; fi\n"
    ))
}

/// Downloads and validates the release manifest at `url` with the local `curl`,
/// over HTTPS only, and checks it is for `version`.
pub async fn fetch_manifest(url: &str, version: &str) -> Result<ReleaseManifest, ManifestError> {
    if !url.starts_with("https://") {
        return Err(ManifestError(format!("{url} is not an https URL")));
    }
    let output = tokio::process::Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-time",
            "30",
            "--max-filesize",
            "1048576",
            "--",
            url,
        ])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| ManifestError(format!("running curl: {error}")))?;
    if !output.status.success() {
        return Err(ManifestError(format!(
            "downloading {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let manifest = ReleaseManifest::from_json(&String::from_utf8_lossy(&output.stdout))?;
    if manifest.version != version {
        return Err(ManifestError(format!(
            "{url} describes {} but this client is {version}",
            manifest.version
        )));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use sha2::{Digest, Sha256};
    #[cfg(unix)]
    use std::time::Duration;

    fn artifact(url: String, sha256: String) -> Artifact {
        Artifact {
            os: "linux".into(),
            arch: "x86_64".into(),
            url,
            sha256,
            format: ArtifactFormat::Binary,
        }
    }

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

    #[test]
    fn manifest_validation_rejects_unsafe_values() {
        let good = format!("{:064x}", 1);
        let json = |url: &str, sha: &str| {
            format!(
                r#"{{"version":"0.1.0","artifacts":[{{"os":"linux","arch":"x86_64","url":"{url}","sha256":"{sha}","format":{{"kind":"binary"}}}}]}}"#
            )
        };
        let manifest = ReleaseManifest::from_json(&json("https://example.test/d", &good));
        let platform = RemotePlatform::from_uname("Linux", "amd64");
        assert!(manifest.is_ok_and(|m| m.select(&platform).is_some()));
        assert!(ReleaseManifest::from_json(&json("http://example.test/d", &good)).is_err());
        assert!(ReleaseManifest::from_json(&json("https://example.test/d", "abc")).is_err());
        let traversal = Artifact {
            format: ArtifactFormat::TarGz {
                member: "../x".into(),
            },
            ..artifact("https://example.test/d".into(), good)
        };
        assert!(traversal.validate().is_err());
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

    /// Runs the install script locally against a `file://` URL; needs `sh` and
    /// `curl`. The script runs on a box, which is never Windows.
    #[cfg(unix)]
    #[tokio::test]
    async fn install_script_verifies_checksum_and_never_replaces()
    -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let home = dir.path().join("home");
            std::fs::create_dir(&home)?;
            let release = dir.path().join("devlanes-release");
            std::fs::write(&release, b"#!/bin/sh\necho fake\n")?;
            let sha = format!("{:x}", Sha256::digest(std::fs::read(&release)?));
            let url = format!("file://{}", release.display());
            let run = |script: String| {
                let home = home.clone();
                async move {
                    tokio::process::Command::new("sh")
                        .arg("-c")
                        .arg(script)
                        .env("HOME", home)
                        .kill_on_drop(true)
                        .output()
                        .await
                }
            };

            // Production scripts refuse anything but HTTPS, file:// included.
            let https_only = install_script(&artifact(url.clone(), sha.clone()))?;
            let output = run(https_only).await?;
            assert!(!output.status.success());
            let installed = home.join(INSTALL_DIR).join("devlanes");
            assert!(!installed.exists());

            let bad =
                install_script_for(&artifact(url.clone(), format!("{:064x}", 0)), "=https,file")?;
            let output = run(bad).await?;
            assert_eq!(output.status.code(), Some(12));
            assert!(!installed.exists());

            let good = install_script_for(&artifact(url, sha), "=https,file")?;
            let output = run(good.clone()).await?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read(&installed)?, std::fs::read(&release)?);

            let output = run(good).await?;
            assert_eq!(output.status.code(), Some(EXIT_ALREADY_INSTALLED));
            Ok(())
        })
        .await?
    }
}
