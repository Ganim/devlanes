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
                "devlanes is not installed on {}. Approve installing {} ({}/{}) to {}, \
                 then run `devlanes daemon install` there and, on Linux, \
                 `loginctl enable-linger` so the box daemon survives logout",
                self.destination,
                artifact.url,
                self.platform.os,
                self.platform.arch,
                self.install_path()
            ),
            None => format!(
                "devlanes is not installed on {} and no release matches {}/{}. \
                 Install it there manually so `devlanes` is on PATH or at {}",
                self.destination,
                self.platform.os,
                self.platform.arch,
                self.install_path()
            ),
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

/// Builds the install script run on the box with `sh -c`.
///
/// Downloads with curl or wget, verifies SHA-256 with `sha256sum` or
/// `shasum -a 256`, and moves the binary into place only if none exists.
pub(crate) fn install_script(artifact: &Artifact) -> Result<String, ManifestError> {
    artifact.validate_script_inputs()?;
    let url = sh_quote(&artifact.url);
    let sha = &artifact.sha256;
    let place = match &artifact.format {
        ArtifactFormat::Binary => "chmod 755 \"$tmp\"; mv \"$tmp\" \"$dest\"".to_owned(),
        ArtifactFormat::TarGz { member } => {
            let member = sh_quote(member);
            format!(
                "mkdir \"$tmp.x\"; tar -xzf \"$tmp\" -C \"$tmp.x\" {member}; \
                 chmod 755 \"$tmp.x\"/{member}; mv \"$tmp.x\"/{member} \"$dest\""
            )
        }
    };
    Ok(format!(
        "set -eu\n\
         dir=\"$HOME/{INSTALL_DIR}\"; dest=\"$dir/devlanes\"\n\
         if [ -e \"$dest\" ]; then echo \"devlanes already exists at $dest; not replacing it\" >&2; exit {EXIT_ALREADY_INSTALLED}; fi\n\
         mkdir -p \"$dir\"\n\
         tmp=$(mktemp \"$dir/.download.XXXXXX\")\n\
         trap 'rm -rf \"$tmp\" \"$tmp.x\"' EXIT\n\
         if command -v curl >/dev/null 2>&1; then curl -fsSL -o \"$tmp\" {url}\n\
         elif command -v wget >/dev/null 2>&1; then wget -q -O \"$tmp\" {url}\n\
         else echo 'curl or wget is required to install devlanes' >&2; exit 11; fi\n\
         sum=$( (sha256sum \"$tmp\" 2>/dev/null || shasum -a 256 \"$tmp\") | cut -d ' ' -f 1)\n\
         if [ \"$sum\" != {sha} ]; then echo \"sha256 mismatch: expected {sha}, got $sum\" >&2; exit 12; fi\n\
         {place}\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
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

    /// Runs the install script locally against a `file://` URL; needs `sh` and `curl`.
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

            let bad = install_script(&artifact(url.clone(), format!("{:064x}", 0)))?;
            let output = run(bad).await?;
            assert_eq!(output.status.code(), Some(12));
            let installed = home.join(INSTALL_DIR).join("devlanes");
            assert!(!installed.exists());

            let good = install_script(&artifact(url, sha))?;
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
