//! Clickable links in terminal output (`OSC 8`): service URLs, config error
//! locations and log or bundle paths. Where links are off the text is printed
//! exactly as before.

use std::path::Path;

use crate::detect::{Emulator, Mode, gate};

/// Environment override for the config's `terminal.links`: `0` prints plain
/// text, `1` writes links in any terminal.
pub const LINKS_ENV: &str = "DEVLANES_TERM_LINKS";

/// How links are written for this terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Links {
    /// The terminal opens `file://…#line:column` at that line (iTerm2 3.4+
    /// semantic history); elsewhere a fragment could keep the file from opening.
    line_fragment: bool,
}

/// Whether to write links, for a client writing to a terminal (`tty`).
/// [`LINKS_ENV`] overrides `mode`. Auto writes them in terminals that
/// implement `OSC 8`: iTerm2, kitty, WezTerm, Ghostty, Windows Terminal, VTE
/// 0.50+, Konsole, Alacritty and foot.
pub fn links_support(mode: Mode, tty: bool, env: impl Fn(&str) -> Option<String>) -> Option<Links> {
    let host = gate(mode, tty, LINKS_ENV, &env)?;
    let supported = match host.emulator {
        Emulator::Iterm2
        | Emulator::Kitty
        | Emulator::WezTerm
        | Emulator::Ghostty
        | Emulator::WindowsTerminal
        | Emulator::Konsole
        | Emulator::Alacritty
        | Emulator::Foot => true,
        Emulator::Vte(version) => version >= 5000,
        Emulator::Xterm | Emulator::AppleTerminal | Emulator::VsCode | Emulator::Unknown => false,
    };
    (supported || host.mode == Mode::Always).then_some(Links {
        line_fragment: host.emulator == Emulator::Iterm2,
    })
}

/// [`links_support`] with the process environment.
pub fn links_support_from_env(mode: Mode, tty: bool) -> Option<Links> {
    links_support(mode, tty, |key| std::env::var(key).ok())
}

impl Links {
    /// Links for a terminal that opens `#line:column` fragments, for tests and
    /// for callers that detected it themselves.
    pub fn new(line_fragment: bool) -> Self {
        Self { line_fragment }
    }

    /// `text` as a link to `url`: `ESC ] 8 ; ; url ESC \ text ESC ] 8 ; ; ESC \`.
    /// A `url` that is empty after encoding leaves `text` as it is.
    pub fn url(&self, text: &str, url: &str) -> String {
        let url = encode_uri(url);
        if url.is_empty() || text.is_empty() {
            return text.to_owned();
        }
        format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
    }

    /// `text` as a link to the local file `path`, at `line` (and `column`)
    /// where the terminal opens files there. A relative path stays plain text:
    /// the terminal would resolve it against its own directory.
    pub fn file(&self, text: &str, path: &Path, line: Option<u32>, column: Option<u32>) -> String {
        if !path.is_absolute() {
            return text.to_owned();
        }
        let mut url = file_url(path);
        if self.line_fragment
            && let Some(line) = line
        {
            url.push_str(&format!("#{line}"));
            if let Some(column) = column {
                url.push_str(&format!(":{column}"));
            }
        }
        self.url(text, &url)
    }
}

/// `text` linked to `url` when `links` is on, else `text` unchanged.
pub fn link(links: Option<&Links>, text: &str, url: &str) -> String {
    links.map_or_else(|| text.to_owned(), |links| links.url(text, url))
}

/// `file://` and the absolute path, percent-encoded. The host is left out,
/// which RFC 8089 reads as this machine.
#[cfg(unix)]
pub fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

/// `file:///C:/x/y` for a drive path and `file://host/share/x` for a UNC one
/// (RFC 8089), the verbatim `\\?\` forms included, with each segment
/// percent-encoded. A rooted path without a drive (`\x\y`) keeps its
/// segments after `file://`, as on Unix.
#[cfg(windows)]
pub fn file_url(path: &Path) -> String {
    use std::path::{Component, Prefix};
    let mut url = String::from("file://");
    let mut segments = 0;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                    url.push('/');
                    url.push(char::from(drive.to_ascii_uppercase()));
                    url.push(':');
                }
                Prefix::UNC(host, share) | Prefix::VerbatimUNC(host, share) => {
                    push_segment(&mut url, &host.to_string_lossy());
                    url.push('/');
                    push_segment(&mut url, &share.to_string_lossy());
                }
                // Device and volume paths have no file URL form; encoded
                // whole they at least stay one segment.
                Prefix::Verbatim(_) | Prefix::DeviceNS(_) => {
                    url.push('/');
                    push_segment(&mut url, &prefix.as_os_str().to_string_lossy());
                }
            },
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                url.push_str("/..");
                segments += 1;
            }
            Component::Normal(name) => {
                url.push('/');
                push_segment(&mut url, &name.to_string_lossy());
                segments += 1;
            }
        }
    }
    // `C:\` itself.
    if segments == 0 {
        url.push('/');
    }
    url
}

/// `segment` percent-encoded, keeping the characters [`file_url`] keeps on
/// Unix except the separator.
#[cfg(windows)]
fn push_segment(url: &mut String, segment: &str) {
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
}

/// `url` with every byte outside printable ASCII percent-encoded, so neither
/// a control character nor a space can end the sequence or the link early.
fn encode_uri(url: &str) -> String {
    let mut encoded = String::with_capacity(url.len());
    for byte in url.bytes() {
        if (0x21..=0x7e).contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn support(mode: Mode, tty: bool, pairs: &[(&str, &str)]) -> Option<Links> {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        links_support(mode, tty, |key| env.get(key).cloned())
    }

    #[test]
    fn links_are_osc_8_with_encoded_targets() {
        let links = Links::new(false);
        assert_eq!(
            links.url("api", "https://api.acme-0.localhost:8443/"),
            "\x1b]8;;https://api.acme-0.localhost:8443/\x1b\\api\x1b]8;;\x1b\\"
        );
        // A control character or space in a URL could end the sequence early.
        assert_eq!(
            links.url("x", "http://a b/\x1b\\\x07é"),
            "\x1b]8;;http://a%20b/%1B\\%07%C3%A9\x1b\\x\x1b]8;;\x1b\\"
        );
        assert_eq!(links.url("x", ""), "x");
        assert_eq!(link(None, "plain", "https://example.invalid"), "plain");
        assert_eq!(
            file_url(Path::new("/home/acme/my app/devlanes.yaml")),
            "file:///home/acme/my%20app/devlanes.yaml"
        );
    }

    /// An absolute config path and its `file://` URL.
    #[cfg(unix)]
    const CONFIG: (&str, &str) = ("/srv/acme/devlanes.yaml", "file:///srv/acme/devlanes.yaml");
    #[cfg(windows)]
    const CONFIG: (&str, &str) = (
        r"C:\srv\acme\devlanes.yaml",
        "file:///C:/srv/acme/devlanes.yaml",
    );

    #[test]
    fn file_links_carry_the_line_only_where_the_terminal_opens_it() {
        let path = Path::new(CONFIG.0);
        let iterm = Links::new(true);
        assert_eq!(
            iterm.file("devlanes.yaml:5:12", path, Some(5), Some(12)),
            format!(
                "\x1b]8;;{}#5:12\x1b\\devlanes.yaml:5:12\x1b]8;;\x1b\\",
                CONFIG.1
            )
        );
        assert!(iterm.file("x", path, Some(5), None).contains("#5\x1b"));
        let other = Links::new(false);
        assert!(!other.file("x", path, Some(5), Some(12)).contains('#'));
        // Relative paths stay text.
        assert_eq!(
            other.file("devlanes.yaml", Path::new("devlanes.yaml"), None, None),
            "devlanes.yaml"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_urls_name_the_drive_or_the_share() {
        for (path, url) in [
            (
                r"C:\Users\acme\my app\devlanes.yaml",
                "file:///C:/Users/acme/my%20app/devlanes.yaml",
            ),
            (r"d:/work/acme", "file:///D:/work/acme"),
            (r"\\?\C:\work\café", "file:///C:/work/caf%C3%A9"),
            (r"C:\", "file:///C:/"),
            (
                r"\\server\share\acme\a#b.yaml",
                "file://server/share/acme/a%23b.yaml",
            ),
            (r"\\?\UNC\server\share\acme", "file://server/share/acme"),
            (r"\\server\share\", "file://server/share/"),
        ] {
            assert_eq!(file_url(Path::new(path)), url, "{path}");
        }
        // Links reach the encoder with the drive's colon intact.
        assert_eq!(
            Links::new(false).file("x", Path::new(r"C:\a b"), None, None),
            "\x1b]8;;file:///C:/a%20b\x1b\\x\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn detection_matrix() {
        use Mode::{Always, Auto, Never};
        for (pairs, expected) in [
            (&[("TERM_PROGRAM", "iTerm.app")][..], true),
            (&[("TERM", "xterm-kitty")][..], true),
            (&[("TERM_PROGRAM", "WezTerm")][..], true),
            (&[("TERM_PROGRAM", "ghostty")][..], true),
            (&[("WT_SESSION", "x")][..], true),
            (&[("VTE_VERSION", "5000")][..], true),
            (&[("VTE_VERSION", "4803")][..], false),
            (&[("KONSOLE_VERSION", "250800")][..], true),
            (&[("TERM", "alacritty")][..], true),
            (&[("TERM", "foot")][..], true),
            (&[("XTERM_VERSION", "XTerm(397)")][..], false),
            (&[("TERM_PROGRAM", "Apple_Terminal")][..], false),
            (&[][..], false),
            (
                &[("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/t")][..],
                false,
            ),
            (&[("TERM_PROGRAM", "ghostty"), ("CI", "1")][..], false),
            (&[("TERM_PROGRAM", "ghostty"), (LINKS_ENV, "0")][..], false),
        ] {
            assert_eq!(support(Auto, true, pairs).is_some(), expected, "{pairs:?}");
        }
        let ghostty = [("TERM_PROGRAM", "ghostty")];
        assert_eq!(support(Auto, false, &ghostty), None, "piped");
        assert_eq!(support(Never, true, &ghostty), None);
        assert!(support(Always, true, &[("TERM_PROGRAM", "Apple_Terminal")]).is_some());
        assert_eq!(
            support(Auto, true, &[("TERM_PROGRAM", "iTerm.app")]),
            Some(Links::new(true))
        );
    }
}
