//! WSL detection for the Linux build. WSL is a runtime variant of Linux, so it
//! is detected from files rather than chosen with `cfg`; every read goes
//! through a root path (`/` outside tests) so tests can lay out fixtures.
//! macOS always answers `None` or empty.

use std::path::{Path, PathBuf};

/// The WSL distro this process runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wsl {
    /// `WSL_DISTRO_NAME`. WSL sets it in the shells it starts, not in services
    /// systemd starts, so a daemon run by systemd usually has `None`.
    pub distro: Option<String>,
    /// 1 or 2.
    pub version: u8,
    /// Whether Linux can start Windows programs (`[interop] enabled` in
    /// `/etc/wsl.conf`, on by default).
    pub interop: bool,
    /// Whether systemd is the distro's init (`[boot] systemd` in `/etc/wsl.conf`).
    pub systemd: bool,
}

/// Where Windows' System32 is when no drive mount has one: WSL's default
/// automount of the usual system drive.
const DEFAULT_SYSTEM32: &str = "/mnt/c/Windows/System32";

/// Detects WSL once per process; `None` outside WSL. Blocking on the first call
/// (a few procfs reads).
pub fn wsl() -> Option<&'static Wsl> {
    #[cfg(target_os = "linux")]
    {
        static WSL: std::sync::OnceLock<Option<Wsl>> = std::sync::OnceLock::new();
        WSL.get_or_init(|| detect(Path::new("/"), std::env::var("WSL_DISTRO_NAME").ok()))
            .as_ref()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The Windows drives mounted into the distro, read now from
/// `/proc/self/mountinfo`; empty outside WSL or when the table cannot be read.
/// Blocking.
pub fn windows_drives() -> WindowsDrives {
    #[cfg(target_os = "linux")]
    if wsl().is_some() {
        return read_drives(Path::new("/"));
    }
    WindowsDrives::default()
}

/// Windows' System32 directory as Linux sees it: `Windows/System32` under the
/// first drive mount that has one, else `/mnt/c/Windows/System32`. Blocking
/// (reads the mount table and stats the drives).
pub fn windows_system32() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        system32(Path::new("/"), &windows_drives())
    }
    #[cfg(not(target_os = "linux"))]
    {
        PathBuf::from(DEFAULT_SYSTEM32)
    }
}

/// A program and its arguments that open a URL in the user's browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlOpener {
    /// A name looked up on `PATH`, or an absolute path.
    pub program: PathBuf,
    /// The arguments, the URL last.
    pub args: Vec<String>,
}

/// How to open `url`: `open` on macOS and `xdg-open` on Linux. Inside WSL with
/// interop the browser is the Windows default one: `wslview` when it is on
/// `PATH`, else Windows' `rundll32.exe url.dll,FileProtocolHandler`. Blocking
/// inside WSL (reads the mount table and `PATH`).
pub fn url_opener(url: &str) -> UrlOpener {
    #[cfg(target_os = "linux")]
    if wsl().is_some_and(|wsl| wsl.interop) {
        return wsl_opener(url, on_path("wslview"), &windows_system32());
    }
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    UrlOpener {
        program: program.into(),
        args: vec![url.to_owned()],
    }
}

/// The opener inside WSL. `rundll32` hands everything after the entry point to
/// `FileProtocolHandler` without a shell, and WSL quotes an argument for the
/// Windows command line only when it has spaces or quotes, so `&` and `?` in
/// a URL arrive unchanged.
#[cfg(any(target_os = "linux", test))]
fn wsl_opener(url: &str, wslview: Option<PathBuf>, system32: &Path) -> UrlOpener {
    match wslview {
        Some(program) => UrlOpener {
            program,
            args: vec![url.to_owned()],
        },
        None => UrlOpener {
            program: system32.join("rundll32.exe"),
            args: vec!["url.dll,FileProtocolHandler".to_owned(), url.to_owned()],
        },
    }
}

/// The first executable file named `name` in an absolute `PATH` directory.
#[cfg(target_os = "linux")]
fn on_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// The binfmt entries WSL registers for Windows programs. `WSLInterop-late` is
/// the name some WSL releases used when systemd runs.
#[cfg(any(target_os = "linux", test))]
const BINFMT_ENTRIES: [&str; 2] = [
    "proc/sys/fs/binfmt_misc/WSLInterop",
    "proc/sys/fs/binfmt_misc/WSLInterop-late",
];

/// WSL when the kernel release names Microsoft and WSL registered its interop
/// binfmt entry or named the distro. The release alone is not enough: Docker
/// Desktop's containers run on the WSL kernel without being a distro, and
/// have neither. WSL 1 reports a release ending in `-Microsoft`; WSL 2
/// kernels say `microsoft-standard` (and `WSL2` since 5.10).
#[cfg(any(target_os = "linux", test))]
fn detect(root: &Path, distro: Option<String>) -> Option<Wsl> {
    let release = std::fs::read_to_string(root.join("proc/sys/kernel/osrelease")).ok()?;
    if !release.to_ascii_lowercase().contains("microsoft") {
        return None;
    }
    let binfmt = BINFMT_ENTRIES
        .iter()
        .find_map(|entry| std::fs::read_to_string(root.join(entry)).ok());
    let distro = distro.filter(|name| !name.is_empty());
    if binfmt.is_none() && distro.is_none() {
        return None;
    }
    let version = if release.contains("WSL2") || release.contains("microsoft-standard") {
        2
    } else {
        1
    };
    Some(Wsl {
        distro,
        version,
        // WSL 1 starts Windows programs without binfmt_misc.
        interop: match binfmt {
            Some(entry) => entry.lines().next() == Some("enabled"),
            None => version == 1,
        },
        // What `sd_booted()` checks.
        systemd: root.join("run/systemd/system").is_dir(),
    })
}

/// A Windows drive mounted into the distro (DrvFs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsDrive {
    /// Where the drive is mounted, normally `/mnt/<letter>`.
    pub mount_point: PathBuf,
    /// The Windows path of the mount's root, such as `C:\`.
    pub root: String,
}

/// A snapshot of the mount table, for asking which paths are on Windows drives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsDrives {
    /// Every mount, with the Windows root for drives.
    mounts: Vec<(PathBuf, Option<String>)>,
}

impl WindowsDrives {
    /// The Windows drive holding the absolute `path`, judged by the deepest
    /// mount containing it, so a Linux filesystem mounted below a drive does
    /// not count.
    pub fn drive_of(&self, path: &Path) -> Option<WindowsDrive> {
        let (mount_point, root) = self
            .mounts
            .iter()
            .filter(|(mount_point, _)| path.starts_with(mount_point))
            .max_by_key(|(mount_point, _)| mount_point.components().count())?;
        Some(WindowsDrive {
            mount_point: mount_point.clone(),
            root: root.clone()?,
        })
    }

    /// Every Windows drive, in mount table order.
    pub fn drives(&self) -> impl Iterator<Item = WindowsDrive> + '_ {
        self.mounts.iter().filter_map(|(mount_point, root)| {
            Some(WindowsDrive {
                mount_point: mount_point.clone(),
                root: root.clone()?,
            })
        })
    }
}

/// `root/proc/self/mountinfo` parsed; empty when it cannot be read.
#[cfg(any(target_os = "linux", test))]
fn read_drives(root: &Path) -> WindowsDrives {
    std::fs::read_to_string(root.join("proc/self/mountinfo"))
        .map(|table| parse_mountinfo(&table))
        .unwrap_or_default()
}

/// [`windows_system32`] with the drive mounts looked up under `root`.
#[cfg(any(target_os = "linux", test))]
fn system32(root: &Path, drives: &WindowsDrives) -> PathBuf {
    drives
        .drives()
        .map(|drive| drive.mount_point.join("Windows/System32"))
        .find(|dir| root.join(dir.strip_prefix("/").unwrap_or(dir)).is_dir())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SYSTEM32))
}

/// Parses mountinfo lines
/// (`id parent dev root mount-point options [optional...] - type source super-options`).
/// Windows drives are `9p` mounts with `aname=drvfs` (WSL 2) or `drvfs` mounts
/// (WSL 1), and their source is the Windows root. Malformed lines are skipped.
#[cfg(any(target_os = "linux", test))]
fn parse_mountinfo(table: &str) -> WindowsDrives {
    let mounts = table
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let mount_point = unescape(before.split(' ').nth(4)?);
            let mut fields = after.split(' ');
            let fstype = fields.next()?;
            let source = unescape(fields.next()?);
            let options = fields.next().unwrap_or("");
            let drvfs = fstype == "drvfs"
                || (fstype == "9p" && options.split([',', ';']).any(|o| o == "aname=drvfs"));
            Some((PathBuf::from(mount_point), drvfs.then_some(source)))
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

    /// The mount table of a WSL 2 Ubuntu with drives C and D.
    const MOUNTINFO: &str = "\
82 1 8:48 / / rw,relatime - ext4 /dev/sdd rw,discard,errors=remount-ro,data=ordered
79 82 0:36 / /usr/lib/wsl/drivers ro,nosuid,nodev,noatime - 9p drivers ro,aname=drivers;fmask=222;dmask=222,cache=0x5,access=client,msize=65536,trans=fd,rfd=8,wfd=8
133 82 0:71 / /mnt/c rw,noatime - 9p C:\\134 rw,aname=drvfs;path=C:\\;uid=1000;gid=1000;symlinkroot=/mnt/,cache=0x5,access=client,msize=65536,trans=fd,rfd=6,wfd=6
134 82 0:72 / /mnt/my\\040drive rw,noatime - 9p D:\\134 rw,aname=drvfs;path=D:\\;uid=1000;gid=1000,cache=0x5
140 133 0:80 / /mnt/c/linux-cache rw,relatime - tmpfs tmpfs rw
";

    /// A fixture root with these files (relative paths, contents).
    fn root(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, contents) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(path, contents).expect("write");
        }
        dir
    }

    const WSL2_RELEASE: &str = "6.18.33.2-microsoft-standard-WSL2\n";
    const INTEROP_ON: &str = "enabled\ninterpreter /init\nflags: P\n";

    /// WSL 2 with interop and systemd, from the binfmt entry and the distro name.
    #[test]
    fn wsl2_with_interop() {
        let root = root(&[
            ("proc/sys/kernel/osrelease", WSL2_RELEASE),
            ("proc/sys/fs/binfmt_misc/WSLInterop", INTEROP_ON),
            ("run/systemd/system/.keep", ""),
        ]);
        assert_eq!(
            detect(root.path(), Some("Ubuntu".into())),
            Some(Wsl {
                distro: Some("Ubuntu".into()),
                version: 2,
                interop: true,
                systemd: true,
            })
        );
        // A daemon systemd started has no distro name; the binfmt entry is enough.
        let daemon = detect(root.path(), None).expect("wsl");
        assert_eq!(daemon.distro, None);
        assert!(daemon.interop);
    }

    /// The entry name some releases used with systemd counts too.
    #[test]
    fn late_binfmt_entry() {
        let root = root(&[
            ("proc/sys/kernel/osrelease", WSL2_RELEASE),
            ("proc/sys/fs/binfmt_misc/WSLInterop-late", INTEROP_ON),
        ]);
        let wsl = detect(root.path(), None).expect("wsl");
        assert!(wsl.interop);
        assert!(!wsl.systemd);
    }

    /// A disabled entry, or none in WSL 2, means interop is off.
    #[test]
    fn interop_off() {
        let disabled = root(&[
            ("proc/sys/kernel/osrelease", WSL2_RELEASE),
            ("proc/sys/fs/binfmt_misc/WSLInterop", "disabled\n"),
        ]);
        assert!(!detect(disabled.path(), None).expect("wsl").interop);
        let missing = root(&[("proc/sys/kernel/osrelease", WSL2_RELEASE)]);
        let wsl = detect(missing.path(), Some("Ubuntu".into())).expect("wsl");
        assert!(!wsl.interop);
        assert_eq!(wsl.version, 2);
    }

    /// WSL 1 has no binfmt entry but runs Windows programs; older WSL 2
    /// kernels have no `WSL2` in their release.
    #[test]
    fn wsl1_and_older_wsl2_kernels() {
        let wsl1 = root(&[("proc/sys/kernel/osrelease", "4.4.0-22621-Microsoft\n")]);
        let wsl = detect(wsl1.path(), Some("Debian".into())).expect("wsl");
        assert_eq!(wsl.version, 1);
        assert!(wsl.interop);
        let older = root(&[("proc/sys/kernel/osrelease", "4.19.104-microsoft-standard\n")]);
        assert_eq!(
            detect(older.path(), Some("Ubuntu".into()))
                .expect("wsl")
                .version,
            2
        );
    }

    /// Native Linux, a container on the WSL kernel, and a distro name copied
    /// to another machine are not WSL.
    #[test]
    fn not_wsl() {
        let native = root(&[("proc/sys/kernel/osrelease", "6.8.0-45-generic\n")]);
        assert_eq!(detect(native.path(), Some("Ubuntu".into())), None);
        let container = root(&[("proc/sys/kernel/osrelease", WSL2_RELEASE)]);
        assert_eq!(detect(container.path(), None), None);
        assert_eq!(detect(container.path(), Some(String::new())), None);
        let unreadable = root(&[]);
        assert_eq!(detect(unreadable.path(), Some("Ubuntu".into())), None);
    }

    /// Drive mounts come from `9p` with `aname=drvfs`; other 9p mounts and
    /// Linux filesystems below a drive do not count.
    #[test]
    fn drives_from_mountinfo() {
        let drives = parse_mountinfo(MOUNTINFO);
        assert_eq!(
            drives.drives().collect::<Vec<_>>(),
            [
                WindowsDrive {
                    mount_point: "/mnt/c".into(),
                    root: r"C:\".into(),
                },
                WindowsDrive {
                    mount_point: "/mnt/my drive".into(),
                    root: r"D:\".into(),
                },
            ]
        );
        let on = |path: &str| drives.drive_of(Path::new(path)).map(|drive| drive.root);
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
            parse_mountinfo(table)
                .drive_of(Path::new("/mnt/c/src"))
                .map(|drive| drive.root),
            Some(r"C:\".into())
        );
    }

    /// System32 is found on the drive that has it, under the root path; without
    /// one the default automount path is used.
    #[test]
    fn system32_from_the_drive_mounts() {
        let root = root(&[
            ("proc/self/mountinfo", MOUNTINFO),
            ("mnt/my drive/Windows/System32/.keep", ""),
        ]);
        let drives = read_drives(root.path());
        assert_eq!(
            system32(root.path(), &drives),
            Path::new("/mnt/my drive/Windows/System32")
        );
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            system32(empty.path(), &read_drives(empty.path())),
            Path::new(DEFAULT_SYSTEM32)
        );
    }

    /// `wslview` wins when installed; otherwise `rundll32.exe` from System32
    /// gets the URL as one argument after the entry point.
    #[test]
    fn opener_inside_wsl() {
        let url = "https://web.acme-2.localhost/?a=1&b=2";
        let system32 = Path::new("/mnt/c/Windows/System32");
        assert_eq!(
            wsl_opener(url, Some("/usr/bin/wslview".into()), system32),
            UrlOpener {
                program: "/usr/bin/wslview".into(),
                args: vec![url.into()],
            }
        );
        assert_eq!(
            wsl_opener(url, None, system32),
            UrlOpener {
                program: "/mnt/c/Windows/System32/rundll32.exe".into(),
                args: vec!["url.dll,FileProtocolHandler".into(), url.into()],
            }
        );
    }

    /// Outside WSL the desktop's opener gets the URL alone.
    #[test]
    fn opener_outside_wsl() {
        if wsl().is_some() {
            return;
        }
        let opener = url_opener("https://example.invalid");
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        assert_eq!(opener.program, Path::new(program));
        assert_eq!(opener.args, ["https://example.invalid"]);
    }

    #[test]
    fn unescapes_octal_sequences() {
        assert_eq!(unescape(r"a\040b\134c\011"), "a b\\c\t");
        assert_eq!(unescape(r"trailing\04"), r"trailing\04");
    }
}
