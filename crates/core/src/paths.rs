//! Path comparison that follows the host file system's case rules, the file
//! names user-given names (projects, services, tasks) are stored under, the
//! user's home, and whether a program is named by path.
//!
//! Identity paths (project roots, worktrees, request cwds) come from Git and
//! from canonicalization, which can disagree on case and separators on
//! Windows. Compare them through these helpers; storage and display keep the
//! original spelling. On Unix every function here is main's exact behaviour:
//! paths compare byte for byte and names are used as they are. No filesystem
//! access happens here.

use std::{borrow::Cow, path::Path};

#[cfg(windows)]
pub use os::{colliding_names, fold_case};
pub use os::{
    forward_slashes, name_on_disk, names_a_path, path_starts_with, same_path, strip_path_prefix,
    user_home,
};

#[cfg(unix)]
mod os {
    use super::{Cow, Path};
    use std::path::PathBuf;

    /// Whether `a` and `b` name the same path: exact comparison on Unix.
    pub fn same_path(a: &Path, b: &Path) -> bool {
        a == b
    }

    /// Whether `path` is `base` or lies below it, by whole components.
    pub fn path_starts_with(path: &Path, base: &Path) -> bool {
        path.starts_with(base)
    }

    /// The rest of `path` below `base`, or `None` when it is not below it.
    pub fn strip_path_prefix<'a>(path: &'a Path, base: &Path) -> Option<&'a Path> {
        path.strip_prefix(base).ok()
    }

    /// The file name `name` is stored under. On Unix that is `name` itself,
    /// so every folder keeps the spelling main gives it.
    pub fn name_on_disk(name: &str) -> Cow<'_, str> {
        Cow::Borrowed(name)
    }

    /// `path` as text for shell strings and environment values; on Unix its
    /// plain display form.
    pub fn forward_slashes(path: &Path) -> String {
        path.display().to_string()
    }

    /// Whether `program` names a file rather than a command looked up on
    /// `PATH`: it contains a `/`.
    pub fn names_a_path(program: &str) -> bool {
        program.contains('/')
    }

    /// The user's home directory from `HOME`.
    pub fn user_home() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(windows)]
mod os {
    use super::{Cow, Path};
    use std::{
        collections::HashMap,
        ffi::OsStr,
        fmt::Write as _,
        path::{Component, PathBuf, Prefix},
    };

    /// `text` with every character mapped to its simple uppercase form when
    /// that is one character, which is close to how NTFS compares names
    /// (one character for one, no `ß` to `SS`).
    pub fn fold_case(text: &str) -> String {
        text.chars()
            .map(|c| {
                let mut upper = c.to_uppercase();
                match (upper.next(), upper.next()) {
                    (Some(single), None) => single,
                    _ => c,
                }
            })
            .collect()
    }

    /// Whether two names match ignoring case. Names that are not valid
    /// Unicode compare exactly.
    fn same_name(a: &OsStr, b: &OsStr) -> bool {
        match (a.to_str(), b.to_str()) {
            (Some(a), Some(b)) => a == b || fold_case(a) == fold_case(b),
            _ => a == b,
        }
    }

    /// Whether two components match. Prefixes compare by what they name, so
    /// the verbatim `\\?\C:` equals `C:` and `\\?\UNC\server\share` equals
    /// `\\server\share`.
    fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
        match (a, b) {
            (Component::Prefix(a), Component::Prefix(b)) => same_prefix(a.kind(), b.kind()),
            (Component::Normal(a), Component::Normal(b)) => same_name(a, b),
            (a, b) => a == b,
        }
    }

    /// Whether two prefixes name the same drive or share, ignoring case and
    /// the verbatim form. Other prefixes (`\\.\pipe`, `\\?\Volume{..}`) match
    /// only their own kind with the same text.
    fn same_prefix(a: Prefix<'_>, b: Prefix<'_>) -> bool {
        match (a, b) {
            (
                Prefix::Disk(a) | Prefix::VerbatimDisk(a),
                Prefix::Disk(b) | Prefix::VerbatimDisk(b),
            ) => a.eq_ignore_ascii_case(&b),
            (
                Prefix::UNC(a_server, a_share) | Prefix::VerbatimUNC(a_server, a_share),
                Prefix::UNC(b_server, b_share) | Prefix::VerbatimUNC(b_server, b_share),
            ) => same_name(a_server, b_server) && same_name(a_share, b_share),
            (Prefix::Verbatim(a), Prefix::Verbatim(b))
            | (Prefix::DeviceNS(a), Prefix::DeviceNS(b)) => same_name(a, b),
            _ => false,
        }
    }

    /// Whether `a` and `b` name the same path, ignoring case and accepting
    /// either separator and the verbatim `\\?\` form.
    pub fn same_path(a: &Path, b: &Path) -> bool {
        let (mut a, mut b) = (a.components(), b.components());
        loop {
            match (a.next(), b.next()) {
                (None, None) => return true,
                (Some(x), Some(y)) if same_component(x, y) => {}
                _ => return false,
            }
        }
    }

    /// Whether `path` is `base` or lies below it, by whole components and
    /// ignoring case.
    pub fn path_starts_with(path: &Path, base: &Path) -> bool {
        strip_path_prefix(path, base).is_some()
    }

    /// The rest of `path` below `base` (ignoring case), or `None` when it is
    /// not below it.
    pub fn strip_path_prefix<'a>(path: &'a Path, base: &Path) -> Option<&'a Path> {
        let mut rest = path.components();
        for wanted in base.components() {
            match rest.next() {
                Some(found) if same_component(found, wanted) => {}
                _ => return None,
            }
        }
        Some(rest.as_path())
    }

    /// Characters Windows refuses in file names, besides control characters.
    const RESERVED: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

    /// The file name `name` is stored under on Windows: an injective
    /// percent-encoding, so two different names never share a folder except
    /// by case, which [`colliding_names`] reports.
    ///
    /// - `%`, `< > : " / \ | ? *` and control characters become `%XX`
    ///   (uppercase hex of the byte).
    /// - A DOS device stem (`CON`, `PRN`, `AUX`, `NUL`, `COM0`-`COM9`,
    ///   `LPT0`-`LPT9`, `COM¹²³`, `LPT¹²³`, any case, with or without an
    ///   extension or trailing spaces before it) gets its first letter as
    ///   `%XX`: `con.log` is `%63on.log`.
    /// - A trailing `.` or space, which Windows drops, becomes `%2E` or `%20`.
    ///
    /// Every `%` in the result starts an escape, so percent-decoding gives the
    /// name back. Names without any of these are returned unchanged.
    pub fn name_on_disk(name: &str) -> Cow<'_, str> {
        let mut out = String::with_capacity(name.len());
        for c in name.chars() {
            if c == '%' || c < ' ' || RESERVED.contains(&c) {
                push_escape(&mut out, c);
            } else {
                out.push(c);
            }
        }
        // Windows reads the part before the first dot, without trailing
        // spaces, as the device name: `nul.txt` and `con .log` are devices too.
        let stem = out.find('.').unwrap_or(out.len());
        if is_device_name(out[..stem].trim_end_matches(' ')) {
            let first = out.remove(0);
            let mut escaped = String::new();
            push_escape(&mut escaped, first);
            out.insert_str(0, &escaped);
        }
        if let Some(last) = out.pop() {
            if matches!(last, '.' | ' ') {
                push_escape(&mut out, last);
            } else {
                out.push(last);
            }
        }
        if out == name {
            Cow::Borrowed(name)
        } else {
            Cow::Owned(out)
        }
    }

    /// Appends `c` as `%XX` per UTF-8 byte.
    fn push_escape(out: &mut String, c: char) {
        let mut bytes = [0; 4];
        for byte in c.encode_utf8(&mut bytes).bytes() {
            let _ = write!(out, "%{byte:02X}");
        }
    }

    /// Whether `stem` is a DOS device name, ignoring case: `CON`, `PRN`,
    /// `AUX`, `NUL`, or `COM`/`LPT` followed by one digit or a superscript
    /// `¹²³`.
    fn is_device_name(stem: &str) -> bool {
        let upper = stem.to_ascii_uppercase();
        if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
            return true;
        }
        let Some(number) = upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
        else {
            return false;
        };
        let mut chars = number.chars();
        matches!(
            (chars.next(), chars.next()),
            (Some('0'..='9' | '¹' | '²' | '³'), None)
        )
    }

    /// The first two different names in `names` that would share one file
    /// name on Windows, which ignores case (`web` and `Web`).
    pub fn colliding_names<'a>(
        names: impl IntoIterator<Item = &'a str>,
    ) -> Option<(&'a str, &'a str)> {
        let mut seen: HashMap<String, &'a str> = HashMap::new();
        for name in names {
            let key = fold_case(&name_on_disk(name));
            match seen.get(&key) {
                Some(&first) if first != name => return Some((first, name)),
                Some(_) => {}
                None => {
                    seen.insert(key, name);
                }
            }
        }
        None
    }

    /// `path` as text with `/` separators, so it survives interpolation into
    /// `sh` strings (where `\` escapes) and Windows APIs still accept it.
    pub fn forward_slashes(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    /// Whether `program` names a file rather than a command looked up on
    /// `PATH`: either separator, or a drive prefix (`C:tool`).
    pub fn names_a_path(program: &str) -> bool {
        program.contains(['/', '\\'])
            || matches!(program.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic())
    }

    /// The user's profile directory. `std::env::home_dir` ignores `HOME` on
    /// Windows, which Git Bash sets to a POSIX-style path.
    pub fn user_home() -> Option<PathBuf> {
        std::env::home_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Prefixes match by whole components on every OS.
    fn prefixes_match_whole_components() {
        let base = Path::new("work").join("acme");
        assert!(path_starts_with(&base.join("web"), &base));
        assert!(path_starts_with(&base, &base));
        assert!(!path_starts_with(&Path::new("work").join("acme-2"), &base));
        assert_eq!(
            strip_path_prefix(&base.join("apps").join("web"), &base),
            Some(Path::new("apps").join("web").as_path())
        );
        assert_eq!(strip_path_prefix(&base, &base.join("web")), None);
        assert!(same_path(&base, &Path::new("work").join("acme")));
    }

    #[test]
    /// Plain names keep their spelling on every OS, so existing folders stay put.
    fn plain_names_are_unchanged() {
        for name in [
            "web",
            "Web",
            "api.2",
            "db_migrate",
            "a-b",
            "café",
            "console",
            "auxiliary",
            "com10",
            "lpt",
            "nul_",
            ".hidden",
            "acme",
        ] {
            assert!(matches!(name_on_disk(name), Cow::Borrowed(n) if n == name));
        }
    }

    #[cfg(unix)]
    #[test]
    /// Unix keeps main's behaviour: names as they are, paths compared exactly.
    fn unix_is_identity() {
        for name in ["db:migrate", "con", "aux.log", "a/b", "api.", "x%y", "a\tb"] {
            assert!(matches!(name_on_disk(name), Cow::Borrowed(n) if n == name));
        }
        assert!(!same_path(Path::new("/Work/acme"), Path::new("/work/acme")));
        assert!(!path_starts_with(
            Path::new("/Work/acme/web"),
            Path::new("/work/acme")
        ));
        assert_eq!(
            forward_slashes(Path::new("/data/a\\b")),
            "/data/a\\b".to_owned()
        );
        assert!(names_a_path("./tool") && names_a_path("/usr/bin/tool"));
        assert!(!names_a_path(r".\tool") && !names_a_path("C:tool"));
    }

    #[cfg(windows)]
    mod windows {
        use super::*;
        use std::collections::HashMap;

        /// Percent-decodes `text`; the inverse of [`name_on_disk`].
        fn decode(text: &str) -> String {
            let bytes = text.as_bytes();
            let mut out = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("hex");
                    out.push(u8::from_str_radix(hex, 16).expect("hex digits"));
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).expect("UTF-8")
        }

        #[test]
        /// Reserved, control and `%` characters become `%XX`; a trailing dot
        /// or space is escaped.
        fn reserved_characters_are_escaped() {
            for (name, expected) in [
                ("db:migrate", "db%3Amigrate"),
                ("apps/web", "apps%2Fweb"),
                (r"db\seed", "db%5Cseed"),
                ("a<b>c", "a%3Cb%3Ec"),
                ("say\"hi\"", "say%22hi%22"),
                ("a|b?c*", "a%7Cb%3Fc%2A"),
                ("a\tb\u{1}", "a%09b%01"),
                ("100%", "100%25"),
                ("db%3Amigrate", "db%253Amigrate"),
                ("api.", "api%2E"),
                ("api ", "api%20"),
                ("api..", "api.%2E"),
                (".", "%2E"),
                ("..", ".%2E"),
                ("", ""),
            ] {
                assert_eq!(name_on_disk(name), expected, "{name:?}");
            }
        }

        #[test]
        /// Device stems are escaped in any case, with or without an extension.
        fn device_names_are_escaped() {
            for (name, expected) in [
                ("aux", "%61ux"),
                ("AUX", "%41UX"),
                ("Con", "%43on"),
                ("prn", "%70rn"),
                ("nul.txt", "%6Eul.txt"),
                ("con.tar.gz", "%63on.tar.gz"),
                ("con .log", "%63on .log"),
                ("com0", "%63om0"),
                ("COM9.log", "%43OM9.log"),
                ("lpt1", "%6Cpt1"),
                ("LPT¹", "%4CPT¹"),
                ("com²", "%63om²"),
                ("lpt³.x", "%6Cpt³.x"),
                ("aux.", "%61ux%2E"),
                ("con ", "%63on%20"),
            ] {
                assert_eq!(name_on_disk(name), expected, "{name:?}");
            }
        }

        #[test]
        /// Decoding gives every name back, so no two names share a file name.
        /// Checks every name of up to four characters over an alphabet that
        /// covers each escape.
        fn encoding_round_trips() {
            let alphabet = [
                'a', 'u', 'x', 'c', 'o', 'n', '1', '.', ' ', '%', ':', '/', '2', 'A', '\t',
            ];
            let mut names = vec![String::new()];
            let mut seen = HashMap::new();
            for _ in 0..4 {
                names = names
                    .iter()
                    .flat_map(|name| alphabet.iter().map(move |c| format!("{name}{c}")))
                    .collect();
                for name in &names {
                    let encoded = name_on_disk(name);
                    assert_eq!(decode(&encoded), *name, "{name:?} -> {encoded:?}");
                    assert!(!encoded.ends_with(['.', ' ']), "{name:?} -> {encoded:?}");
                    if let Some(other) = seen.insert(encoded.into_owned(), name.clone()) {
                        panic!("{other:?} and {name:?} share a file name");
                    }
                }
            }
        }

        #[test]
        /// Only names that differ but fold to one file name collide.
        fn collisions_are_case_folds() {
            assert_eq!(colliding_names(["web", "api", "Web"]), Some(("web", "Web")));
            assert_eq!(colliding_names(["db:migrate", "db_migrate"]), None);
            assert_eq!(colliding_names(["web", "web"]), None);
            assert_eq!(colliding_names(["Straße", "STRASSE"]), None);
            assert_eq!(colliding_names(["café", "CAFÉ"]), Some(("café", "CAFÉ")));
        }

        #[test]
        /// Paths compare ignoring case and separator spelling.
        fn comparison_ignores_case_and_separators() {
            assert!(same_path(
                Path::new(r"C:\Work\Acme"),
                Path::new("c:/work/acme")
            ));
            assert!(!same_path(
                Path::new(r"C:\work\acme"),
                Path::new(r"D:\work\acme")
            ));
            assert!(path_starts_with(
                Path::new(r"c:\WORK\acme\web"),
                Path::new("C:/work/acme")
            ));
            assert!(!path_starts_with(
                Path::new(r"c:\work\acme-2"),
                Path::new(r"C:\work\acme")
            ));
            assert_eq!(
                strip_path_prefix(
                    Path::new(r"C:\Work\Acme\apps\web"),
                    Path::new(r"c:\work\acme")
                ),
                Some(Path::new(r"apps\web"))
            );
            assert!(same_path(
                Path::new(r"C:\Users\Ana Maria\src"),
                Path::new(r"c:\users\ANA MARIA\src")
            ));
            assert!(same_path(Path::new(r"C:\Café"), Path::new(r"c:\CAFÉ")));
        }

        #[test]
        /// Verbatim and plain prefixes naming the same drive or share are equal.
        fn prefixes_compare_by_drive_and_share() {
            for (a, b) in [
                (r"\\?\C:\Work\acme", r"c:\work\ACME"),
                (r"\\?\c:\work", "C:/work"),
                (r"\\Server\Share\acme", r"\\server\share\acme"),
                (r"\\server\share/acme\web", "//SERVER/share/acme/web"),
                (r"\\?\UNC\server\share\acme", r"\\SERVER\Share\acme"),
            ] {
                assert!(same_path(Path::new(a), Path::new(b)), "{a} vs {b}");
            }
            for (a, b) in [
                (r"\\?\C:\work", r"D:\work"),
                (r"\\server\share\acme", r"\\server\other\acme"),
                (r"\\server\share\acme", r"\\other\share\acme"),
                (r"\\server\share\acme", r"C:\share\acme"),
                (r"C:\work", r"C:work"),
            ] {
                assert!(!same_path(Path::new(a), Path::new(b)), "{a} vs {b}");
            }
            assert!(path_starts_with(
                Path::new(r"//server/share/acme/web"),
                Path::new(r"\\?\UNC\Server\Share\acme")
            ));
        }

        #[test]
        /// Paths for shell strings use `/`.
        fn forward_slashes_replace_backslashes() {
            assert_eq!(
                forward_slashes(Path::new(r"C:\Users\acme\.devlanes\data\acme\5")),
                "C:/Users/acme/.devlanes/data/acme/5"
            );
        }

        #[test]
        /// Either separator or a drive letter names a path; a bare name or a
        /// colon elsewhere does not.
        fn programs_named_by_path() {
            for program in [r".\tool", "bin/tool", r"C:\tools\x.exe", "c:tool"] {
                assert!(names_a_path(program), "{program}");
            }
            for program in ["tool", "tool.exe", "1:tool", "db:migrate", ""] {
                assert!(!names_a_path(program), "{program}");
            }
        }
    }
}
