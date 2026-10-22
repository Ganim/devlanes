//! `doctor`'s `wsl` facts: the WSL distro this CLI runs in and the settings that
//! change how devlanes behaves there. The CLI and its daemon share the distro,
//! so the CLI reads them for both doctor paths. WSL is a runtime fact of a Linux
//! build, so this module checks [`devlanes_sys::wsl`] instead of `cfg`; on macOS
//! and Windows it adds nothing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use devlanes_sys::WslInfo;
use serde_json::{Value, json};

/// How long one interop or `wslinfo` call may take before doctor gives up on it.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Adds `wsl: {distro, version, interop, systemd, networking_mode,
/// networking_source, generate_hosts, unprivileged_port_start,
/// privileged_proxy_ports}` when this process runs inside WSL. `networking_mode`
/// is what `wslinfo` reports, else `networkingMode` from the Windows user's
/// `.wslconfig` (read only), else `null`. `privileged_proxy_ports` are the
/// configured proxy ports below `net.ipv4.ip_unprivileged_port_start`, which the
/// proxy cannot bind as a normal user.
pub async fn annotate(paths: &devlanes_client::Paths, data: &mut Value) {
    if !data.is_object() {
        return;
    }
    let config_path = paths.home.join("config.yaml");
    // The first WSL detection stats the drive mounts through the 9P file
    // server, so it runs off the async threads with the file reads.
    let local = tokio::task::spawn_blocking(move || {
        devlanes_sys::wsl().map(|info| (info, local_facts(&config_path)))
    })
    .await;
    let Ok(Some((info, local))) = local else {
        return;
    };
    let (mode, source) = networking_mode(info).await;
    data["wsl"] = json!({
        "distro": info.distro,
        "version": info.version,
        "interop": info.interop,
        "systemd": local.systemd,
        "networking_mode": mode,
        "networking_source": source,
        "generate_hosts": local.generate_hosts,
        "unprivileged_port_start": local.port_start,
        "privileged_proxy_ports": local.privileged_proxy_ports,
    });
}

/// Facts read from this distro's files.
struct LocalFacts {
    systemd: bool,
    generate_hosts: bool,
    port_start: Option<u16>,
    privileged_proxy_ports: Vec<u16>,
}

/// Reads systemd, `/etc/wsl.conf`, the unprivileged port start and the proxy
/// ports from the user config (its defaults when it does not load). Blocking.
fn local_facts(config: &Path) -> LocalFacts {
    let wsl_conf = std::fs::read_to_string("/etc/wsl.conf").unwrap_or_default();
    let port_start = std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
        .ok()
        .and_then(|text| text.trim().parse().ok());
    let proxy = devlanes_config::load_user(config).unwrap_or_default().proxy;
    LocalFacts {
        // systemd's own test for "booted with systemd" (sd_booted(3)).
        systemd: Path::new("/run/systemd/system").is_dir(),
        // WSL writes /etc/hosts at every start unless this is false.
        generate_hosts: ini_value(&wsl_conf, "network", "generateHosts")
            .is_none_or(|value| !value.eq_ignore_ascii_case("false")),
        port_start,
        privileged_proxy_ports: privileged(&[proxy.http_port, proxy.https_port], port_start),
    }
}

/// The ports among `ports` below `start`, which need root (or a lower
/// `ip_unprivileged_port_start`) to bind.
fn privileged(ports: &[u16], start: Option<u16>) -> Vec<u16> {
    let Some(start) = start else {
        return Vec::new();
    };
    ports.iter().copied().filter(|port| *port < start).collect()
}

/// The networking mode and where it came from: `wslinfo --networking-mode`
/// (WSL 2.0 and later), else `.wslconfig` through interop, else unknown.
async fn networking_mode(info: &WslInfo) -> (Option<String>, Option<&'static str>) {
    if let Some(mode) = output("/usr/bin/wslinfo", &["--networking-mode"], None).await {
        let mode = mode.trim().to_ascii_lowercase();
        if !mode.is_empty() {
            return (Some(mode), Some("wslinfo"));
        }
    }
    if !info.interop {
        return (None, None);
    }
    let Some(system32) = &info.system32 else {
        return (None, None);
    };
    let Some(wslconfig) = wslconfig_path(system32).await else {
        return (None, None);
    };
    let mode = wslconfig_mode(tokio::fs::read_to_string(&wslconfig).await);
    let source = mode.is_some().then_some("wslconfig");
    (mode, source)
}

/// The networking mode `.wslconfig` selects. NAT is WSL's default when the
/// file or the key is missing; a file that exists but cannot be read leaves
/// the mode unknown.
fn wslconfig_mode(text: std::io::Result<String>) -> Option<String> {
    let text = match text {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return None,
    };
    Some(
        ini_value(&text, "wsl2", "networkingMode")
            .map_or_else(|| "nat".to_owned(), |mode| mode.to_ascii_lowercase()),
    )
}

/// `%USERPROFILE%\.wslconfig` as a Linux path: `cmd.exe` reports the Windows
/// profile, `wslpath` converts it. Run from `system32` so `cmd.exe` does not
/// warn about a `\\wsl.localhost` working directory.
async fn wslconfig_path(system32: &Path) -> Option<PathBuf> {
    let cmd = system32.join("cmd.exe");
    let profile = output(
        cmd.to_str()?,
        &["/d", "/c", "echo %USERPROFILE%"],
        Some(system32),
    )
    .await?;
    let profile = profile.trim();
    // An unset variable comes back as the literal text.
    if profile.is_empty() || profile.contains('%') {
        return None;
    }
    let linux = output("/usr/bin/wslpath", &["-u", profile], None).await?;
    Some(Path::new(linux.trim()).join(".wslconfig"))
}

/// Standard output of `program`, `None` when it cannot start, fails or takes
/// longer than [`CALL_TIMEOUT`].
async fn output(program: &str, args: &[&str], cwd: Option<&Path>) -> Option<String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = tokio::time::timeout(CALL_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The value of `key` in `[section]` of an INI file such as `wsl.conf` or
/// `.wslconfig`: names compare without case, `#` and `;` start comments, and
/// surrounding quotes are dropped. The last assignment wins.
fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut current = String::new();
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(['#', ';']) || line.is_empty() {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            current = name.trim().to_owned();
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if current.eq_ignore_ascii_case(section) && name.trim().eq_ignore_ascii_case(key) {
            let value = value.split(['#', ';']).next().unwrap_or("").trim();
            found = Some(value.trim_matches('"').to_owned());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_values_ignore_case_comments_and_quotes() {
        let text = "\
# comment
[boot]
systemd=true

[Network]
generateHosts = false # keep my entries
hostname=acme

[wsl2]
memory=8GB
NetworkingMode=\"Mirrored\"
";
        assert_eq!(ini_value(text, "boot", "systemd").as_deref(), Some("true"));
        assert_eq!(
            ini_value(text, "network", "generatehosts").as_deref(),
            Some("false")
        );
        assert_eq!(
            ini_value(text, "wsl2", "networkingMode").as_deref(),
            Some("Mirrored")
        );
        assert_eq!(ini_value(text, "boot", "hostname"), None);
        assert_eq!(ini_value("", "wsl2", "networkingMode"), None);
    }

    /// A missing `.wslconfig` means NAT; one that cannot be read is unknown.
    #[test]
    fn wslconfig_mode_defaults_to_nat_only_when_missing() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            wslconfig_mode(Ok("[wsl2]\nnetworkingMode=Mirrored\n".into())).as_deref(),
            Some("mirrored")
        );
        assert_eq!(
            wslconfig_mode(Ok("[wsl2]\nmemory=8GB\n".into())).as_deref(),
            Some("nat")
        );
        assert_eq!(
            wslconfig_mode(Err(Error::from(ErrorKind::NotFound))).as_deref(),
            Some("nat")
        );
        assert_eq!(
            wslconfig_mode(Err(Error::from(ErrorKind::PermissionDenied))),
            None
        );
        assert_eq!(wslconfig_mode(Err(Error::other("9P timed out"))), None);
    }

    #[test]
    fn privileged_ports_are_below_the_start() {
        assert_eq!(privileged(&[80, 443], Some(1024)), [80, 443]);
        assert_eq!(privileged(&[80, 8443], Some(1024)), [80]);
        assert_eq!(privileged(&[80, 443], Some(80)), Vec::<u16>::new());
        assert_eq!(privileged(&[80, 443], None), Vec::<u16>::new());
    }
}
