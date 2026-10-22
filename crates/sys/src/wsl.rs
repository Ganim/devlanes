//! WSL detection: whether this Linux process runs in a WSL distro, and which of
//! its mounts are Windows drives.
//!
//! WSL is a runtime fact of a Linux build, so it is detected once and cached
//! rather than chosen with `cfg`. Other platforms always answer `None` or empty.

use std::path::{Path, PathBuf};

/// The WSL distro this process runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslInfo {
    /// `WSL_DISTRO_NAME`. WSL sets it in shells it starts, not in processes
    /// systemd starts, so the daemon run as a service usually has `None`.
    pub distro: Option<String>,
    /// 1 or 2.
    pub version: u8,
    /// Whether Linux can start Windows programs (`[interop] enabled` in
    /// `/etc/wsl.conf`, on by default).
    pub interop: bool,
    /// Windows' `System32` directory as Linux sees it, normally
    /// `/mnt/c/Windows/System32`; `None` when no Windows drive mount has one.
    pub system32: Option<PathBuf>,
}

/// A Windows drive mounted into the distro (DrvFs), where file access is slow and
/// inotify misses changes Windows programs make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveMount {
    /// Where the drive is mounted, normally `/mnt/<letter>`.
    pub mount_point: PathBuf,
    /// The Windows path of the mount's root, such as `C:\`.
    pub windows_root: String,
}

/// A snapshot of the mount table, for asking which paths are on Windows drives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsDrives {
    mounts: Vec<Mount>,
}

impl WindowsDrives {
    /// The Windows drive holding `path`, judged by the deepest mount containing
    /// it, so a Linux filesystem mounted below a drive does not count. `path`
    /// should be absolute and free of `..`.
    pub fn drive_of(&self, path: &Path) -> Option<DriveMount> {
        let mount = self
            .mounts
            .iter()
            .filter(|mount| path.starts_with(&mount.mount_point))
            .max_by_key(|mount| mount.mount_point.components().count())?;
        mount.windows_root.as_ref().map(|root| DriveMount {
            mount_point: mount.mount_point.clone(),
            windows_root: root.clone(),
        })
    }

    /// Every Windows drive mount, in mount table order.
    pub fn drives(&self) -> Vec<DriveMount> {
        self.mounts
            .iter()
            .filter_map(|mount| {
                Some(DriveMount {
                    mount_point: mount.mount_point.clone(),
                    windows_root: mount.windows_root.clone()?,
                })
            })
            .collect()
    }
}

/// One mount table entry; `windows_root` is set for Windows drives.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mount {
    mount_point: PathBuf,
    windows_root: Option<String>,
}

/// Detects WSL once per process; `None` outside WSL. Blocking on the first call
/// (a few procfs reads and `stat`s).
#[cfg(target_os = "linux")]
pub(crate) fn wsl() -> Option<&'static WslInfo> {
    static WSL: std::sync::OnceLock<Option<WslInfo>> = std::sync::OnceLock::new();
    WSL.get_or_init(linux::detect).as_ref()
}

/// Only Linux runs inside WSL.
#[cfg(not(target_os = "linux"))]
pub(crate) fn wsl() -> Option<&'static WslInfo> {
    None
}

/// The Windows drive mounts, read now from `/proc/self/mountinfo`; empty outside
/// WSL or when the table cannot be read. Blocking.
#[cfg(target_os = "linux")]
pub(crate) fn windows_drives() -> WindowsDrives {
    if wsl().is_none() {
        return WindowsDrives::default();
    }
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|table| parse_mountinfo(&table))
        .unwrap_or_default()
}

/// No Windows drives are mounted outside WSL.
#[cfg(not(target_os = "linux"))]
pub(crate) fn windows_drives() -> WindowsDrives {
    WindowsDrives::default()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;

    use super::{Markers, WslInfo, classify, parse_mountinfo};

    /// binfmt entries WSL registers for Windows executables. `WSLInterop-late`
    /// is the name WSL used for a while when systemd runs (microsoft/WSL#13449);
    /// microsoft/WSL#13467 went back to `WSLInterop`.
    const BINFMT_ENTRIES: [&str; 2] = [
        "/proc/sys/fs/binfmt_misc/WSLInterop",
        "/proc/sys/fs/binfmt_misc/WSLInterop-late",
    ];

    /// Reads the markers and the drive mounts.
    pub(super) fn detect() -> Option<WslInfo> {
        let binfmt = BINFMT_ENTRIES.iter().find_map(|path| {
            let entry = std::fs::read_to_string(path).ok()?;
            Some(entry.lines().next() == Some("enabled"))
        });
        let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        let markers = Markers {
            osrelease: std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default(),
            binfmt,
            distro: env("WSL_DISTRO_NAME"),
            interop_socket: env("WSL_INTEROP").is_some(),
            // WSL init keeps the interop sockets here; containers on a WSL
            // kernel (Docker Desktop) share the kernel but not this directory.
            run_wsl: Path::new("/run/WSL").is_dir(),
        };
        let mut info = classify(markers)?;
        let drives = std::fs::read_to_string("/proc/self/mountinfo")
            .map(|table| parse_mountinfo(&table))
            .unwrap_or_default();
        info.system32 = drives
            .drives()
            .into_iter()
            .map(|drive| drive.mount_point.join("Windows/System32"))
            .find(|dir| dir.is_dir());
        Some(info)
    }
}

/// What WSL detection reads, kept as data for tests.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Default)]
struct Markers {
    /// `/proc/sys/kernel/osrelease`.
    osrelease: String,
    /// The WSL binfmt entry: `Some(enabled)` when registered.
    binfmt: Option<bool>,
    /// `WSL_DISTRO_NAME`.
    distro: Option<String>,
    /// `WSL_INTEROP` is set.
    interop_socket: bool,
    /// `/run/WSL` exists.
    run_wsl: bool,
}

/// WSL when a distro marker is present. The kernel release alone is not
/// enough: Docker Desktop's containers run on a WSL kernel without being a
/// distro (no interop, no drive mounts). `WSL_DISTRO_NAME` alone is not enough
/// either, as copied environments carry it to other machines (`ssh` with
/// `SendEnv`, `env` dumps), so it counts only on a kernel whose release names
/// Microsoft.
/// WSL 1 reports a fake kernel whose release ends in `-Microsoft`; anything
/// else is WSL 2, custom kernels included.
///
/// Interop follows the binfmt entry when WSL registered one, so a disabled
/// entry wins over a leftover `WSL_INTEROP`. WSL 1 runs Windows programs
/// without binfmt_misc, so there a missing entry still means on.
#[cfg(any(target_os = "linux", test))]
fn classify(markers: Markers) -> Option<WslInfo> {
    let release = markers.osrelease.trim();
    let microsoft_kernel = release.to_ascii_lowercase().contains("microsoft");
    let present = markers.binfmt.is_some()
        || markers.interop_socket
        || markers.run_wsl
        || (markers.distro.is_some() && microsoft_kernel);
    if !present {
        return None;
    }
    let wsl2 = release.contains("WSL2") || release.contains("microsoft-standard");
    let version = if !wsl2 && microsoft_kernel { 1 } else { 2 };
    Some(WslInfo {
        distro: markers.distro,
        version,
        interop: markers
            .binfmt
            .unwrap_or(markers.interop_socket || version == 1),
        system32: None,
    })
}

/// Parses `/proc/self/mountinfo`. Windows drives are `9p` mounts with
/// `aname=drvfs` (WSL 2) or `drvfs` mounts (WSL 1); their source is the Windows
/// root, such as `C:\`. Malformed lines are skipped.
#[cfg(any(target_os = "linux", test))]
fn parse_mountinfo(table: &str) -> WindowsDrives {
    let mounts = table
        .lines()
        .filter_map(|line| {
            // `id parent major:minor root mount-point options [optional...] - type source super-options`
            let (before, after) = line.split_once(" - ")?;
            let mount_point = unescape(before.split(' ').nth(4)?);
            let mut fields = after.split(' ');
            let fstype = fields.next()?;
            let source = unescape(fields.next()?);
            let options = fields.next().unwrap_or("");
            let drvfs = fstype == "drvfs"
                || (fstype == "9p" && options.split([',', ';']).any(|o| o == "aname=drvfs"));
            Some(Mount {
                mount_point: PathBuf::from(mount_point),
                windows_root: drvfs.then_some(source),
            })
        })
        .collect();
    WindowsDrives { mounts }
}

/// Decodes the octal escapes mountinfo uses for space, tab, newline and backslash.
#[cfg(any(target_os = "linux", test))]
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let octal = bytes
            .get(index + 1..index + 4)
            .filter(|_| bytes[index] == b'\\')
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        match octal {
            Some(byte) => {
                out.push(byte);
                index += 4;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mount table of a WSL 2 Ubuntu with drives C and D (from WSL 2.7).
    const MOUNTINFO: &str = "\
370 255 8:80 / / rw,relatime - ext4 /dev/sdf rw,discard,errors=remount-ro,data=ordered
367 370 0:36 / /usr/lib/wsl/drivers ro,nosuid,nodev,noatime - 9p drivers ro,aname=drivers;fmask=222;dmask=222,cache=0x5,access=client,msize=65536,trans=fd,rfd=8,wfd=8
421 370 0:128 / /mnt/c rw,noatime - 9p C:\\134 rw,aname=drvfs;path=C:\\;uid=1000;gid=1000;symlinkroot=/mnt/,cache=0x5,access=client,msize=65536,trans=fd,rfd=6,wfd=6
422 370 0:129 / /mnt/my\\040drive rw,noatime - 9p D:\\134 rw,aname=drvfs;path=D:\\;uid=1000;gid=1000,cache=0x5
430 421 0:140 / /mnt/c/linux-cache rw,relatime - tmpfs tmpfs rw
";

    fn markers(osrelease: &str) -> Markers {
        Markers {
            osrelease: osrelease.into(),
            binfmt: Some(true),
            distro: Some("Ubuntu".into()),
            ..Markers::default()
        }
    }

    /// Release strings of WSL 2 (current and older kernels) and WSL 1.
    #[test]
    fn versions_from_the_kernel_release() {
        for (release, version) in [
            ("6.18.33.2-microsoft-standard-WSL2", 2),
            ("5.15.167.4-microsoft-standard-WSL2+", 2),
            ("4.19.104-microsoft-standard", 2),
            ("4.4.0-22621-Microsoft", 1),
            ("6.9.0-custom", 2),
        ] {
            let info = classify(markers(release)).expect(release);
            assert_eq!(info.version, version, "{release}");
            assert_eq!(info.distro.as_deref(), Some("Ubuntu"));
            assert!(info.interop);
        }
    }

    /// A WSL kernel without a distro marker is a container, not WSL.
    #[test]
    fn a_container_on_a_wsl_kernel_is_not_wsl() {
        let container = Markers {
            osrelease: "6.18.33.2-microsoft-standard-WSL2".into(),
            ..Markers::default()
        };
        assert_eq!(classify(container), None);
        let plain = Markers {
            osrelease: "6.8.0-45-generic".into(),
            ..Markers::default()
        };
        assert_eq!(classify(plain), None);
    }

    /// A disabled binfmt entry and no interop socket mean interop is off; the
    /// systemd-started daemon has no distro name but still detects WSL.
    #[test]
    fn interop_and_distro_markers() {
        let disabled = Markers {
            osrelease: "6.6.36.6-microsoft-standard-WSL2".into(),
            binfmt: Some(false),
            run_wsl: true,
            ..Markers::default()
        };
        let info = classify(disabled).expect("wsl");
        assert!(!info.interop);
        assert_eq!(info.distro, None);
        let socket_only = Markers {
            interop_socket: true,
            ..Markers::default()
        };
        assert!(classify(socket_only).expect("wsl").interop);
        // A disabled entry wins over a WSL_INTEROP left in the environment.
        let disabled_with_socket = Markers {
            osrelease: "6.6.36.6-microsoft-standard-WSL2".into(),
            binfmt: Some(false),
            interop_socket: true,
            ..Markers::default()
        };
        assert!(!classify(disabled_with_socket).expect("wsl").interop);
    }

    /// WSL 1 has no binfmt entry and no interop socket but runs Windows
    /// programs; a registered, disabled entry still turns interop off.
    #[test]
    fn wsl1_interop_without_binfmt() {
        let wsl1 = Markers {
            osrelease: "4.4.0-22621-Microsoft".into(),
            distro: Some("Ubuntu".into()),
            ..Markers::default()
        };
        let info = classify(wsl1.clone()).expect("wsl");
        assert_eq!(info.version, 1);
        assert!(info.interop);
        let off = Markers {
            binfmt: Some(false),
            ..wsl1
        };
        assert!(!classify(off).expect("wsl").interop);
    }

    /// A `WSL_DISTRO_NAME` copied into a process on another Linux machine (an
    /// SSH session started from WSL) is not WSL.
    #[test]
    fn a_leaked_distro_name_alone_is_not_wsl() {
        let leaked = Markers {
            osrelease: "6.8.0-45-generic".into(),
            distro: Some("Ubuntu".into()),
            ..Markers::default()
        };
        assert_eq!(classify(leaked), None);
        let wsl2 = Markers {
            osrelease: "6.6.36.6-microsoft-standard-WSL2".into(),
            distro: Some("Ubuntu".into()),
            ..Markers::default()
        };
        assert_eq!(classify(wsl2).expect("wsl").version, 2);
    }

    /// Drive mounts come from `9p` with `aname=drvfs`; other 9p mounts and
    /// Linux filesystems below a drive do not count.
    #[test]
    fn drive_mounts_from_mountinfo() {
        let drives = parse_mountinfo(MOUNTINFO);
        assert_eq!(
            drives.drives(),
            [
                DriveMount {
                    mount_point: "/mnt/c".into(),
                    windows_root: r"C:\".into(),
                },
                DriveMount {
                    mount_point: "/mnt/my drive".into(),
                    windows_root: r"D:\".into(),
                },
            ]
        );
        let on = |path: &str| drives.drive_of(Path::new(path)).map(|d| d.windows_root);
        assert_eq!(on("/mnt/c/src/acme"), Some(r"C:\".into()));
        assert_eq!(on("/mnt/c"), Some(r"C:\".into()));
        assert_eq!(on("/mnt/my drive/acme"), Some(r"D:\".into()));
        assert_eq!(on("/mnt/c/linux-cache/acme"), None);
        assert_eq!(on("/mnt/cc/acme"), None);
        assert_eq!(on("/home/acme/src"), None);
        assert_eq!(on("/usr/lib/wsl/drivers/x"), None);
    }

    /// WSL 1 mounts drives with the `drvfs` type.
    #[test]
    fn wsl1_drvfs_mounts() {
        let table = "45 23 0:42 / /mnt/c rw,noatime - drvfs C:\\134 rw,case=off\n";
        assert_eq!(
            parse_mountinfo(table).drives(),
            [DriveMount {
                mount_point: "/mnt/c".into(),
                windows_root: r"C:\".into(),
            }]
        );
    }

    #[test]
    fn unescapes_octal_sequences() {
        assert_eq!(unescape(r"a\040b\134c\011"), "a b\\c\t");
        assert_eq!(unescape(r"trailing\04"), r"trailing\04");
    }
}
