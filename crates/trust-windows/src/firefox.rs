//! The `firefox` target on Windows: Firefox imports the roots in the Windows store
//! when `security.enterprise_roots.enabled` is true (the default), so this store
//! changes and reports the user root store and only adds a hint when a profile
//! turned the preference off. Profiles are read, never written.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use devlanes_trust::{CaCertificate, TrustError, TrustStatus, TrustStore, TrustTarget};

use crate::{FIREFOX_ID, STORE_ID, UserRootStore, blocking};

/// The preference that makes Firefox import roots from the OS store.
pub const ENTERPRISE_ROOTS_PREF: &str = "security.enterprise_roots.enabled";

/// Where Firefox keeps this user's profiles.
#[derive(Debug, Clone, Default)]
pub struct FirefoxDirs {
    /// `%APPDATA%`; the regular install keeps `Mozilla\Firefox` here.
    pub appdata: Option<PathBuf>,
    /// `%LOCALAPPDATA%`; the Microsoft Store (MSIX) build keeps
    /// `Packages\Mozilla.Firefox_<id>\LocalCache\Roaming\Mozilla\Firefox` here.
    pub local_appdata: Option<PathBuf>,
}

impl FirefoxDirs {
    /// `%APPDATA%` and `%LOCALAPPDATA%` from the environment.
    pub fn from_env() -> Self {
        Self {
            appdata: std::env::var_os("APPDATA").map(PathBuf::from),
            local_appdata: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        }
    }

    /// Firefox data directories (each with `profiles.ini` and `Profiles`) that
    /// may exist: the regular one, then one per MSIX package. Blocking: lists
    /// `%LOCALAPPDATA%\Packages`.
    pub fn data_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .appdata
            .iter()
            .map(|appdata| appdata.join(r"Mozilla\Firefox"))
            .collect();
        let packages = self
            .local_appdata
            .as_ref()
            .and_then(|local| std::fs::read_dir(local.join("Packages")).ok());
        let mut msix: Vec<PathBuf> = packages
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("Mozilla.Firefox_"))
            })
            .map(|entry| entry.path().join(r"LocalCache\Roaming\Mozilla\Firefox"))
            .collect();
        msix.sort();
        dirs.extend(msix);
        dirs
    }

    /// Every profile directory of every data directory, without repeats. Blocking.
    pub fn profiles(&self) -> Vec<PathBuf> {
        let mut profiles = Vec::new();
        for dir in self.data_dirs() {
            for profile in data_dir_profiles(&dir) {
                if !profiles.contains(&profile) {
                    profiles.push(profile);
                }
            }
        }
        profiles
    }
}

/// The profiles of one Firefox data directory: those `profiles.ini` lists when
/// it can be read (Firefox ignores folders it no longer lists), otherwise every
/// folder under `Profiles`.
fn data_dir_profiles(dir: &Path) -> Vec<PathBuf> {
    if let Ok(ini) = std::fs::read_to_string(dir.join("profiles.ini")) {
        return ini_profiles(&ini, dir);
    }
    let Ok(entries) = std::fs::read_dir(dir.join("Profiles")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

/// The `Path=` of each `[Profile...]` section of `profiles.ini` in `dir`.
/// `IsRelative=1` paths (with `/` separators) are relative to `dir`, `IsRelative=0`
/// paths absolute; without the key, a path that is not absolute counts as relative.
fn ini_profiles(ini: &str, dir: &Path) -> Vec<PathBuf> {
    let mut profiles = Vec::new();
    // Path and IsRelative of the current `[Profile...]` section, if in one.
    let mut section: Option<(Option<String>, Option<String>)> = None;
    let mut finish = |section: Option<(Option<String>, Option<String>)>| {
        if let Some((Some(path), relative)) = section {
            let relative =
                relative.map_or_else(|| !Path::new(&path).is_absolute(), |flag| flag == "1");
            profiles.push(if relative {
                path.split(['/', '\\'])
                    .filter(|part| !part.is_empty())
                    .fold(dir.to_owned(), |joined, part| joined.join(part))
            } else {
                PathBuf::from(path)
            });
        }
    };
    for line in ini.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            finish(section.take());
            section = name.starts_with("Profile").then_some((None, None));
        } else if let (Some((path, relative)), Some((key, value))) =
            (section.as_mut(), line.split_once('='))
        {
            match key.trim() {
                "Path" => *path = Some(value.trim().to_owned()),
                "IsRelative" => *relative = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    finish(section);
    profiles
}

/// Firefox reporting on, and changing, the Windows user root store.
pub struct FirefoxStore {
    system: UserRootStore,
    dirs: FirefoxDirs,
}

impl FirefoxStore {
    /// Reports on `system` and reads the profiles in `dirs` for the hint.
    pub fn new(system: UserRootStore, dirs: FirefoxDirs) -> Self {
        Self { system, dirs }
    }
}

#[async_trait]
impl TrustStore for FirefoxStore {
    /// Always [`TrustTarget::Firefox`].
    fn target(&self) -> TrustTarget {
        TrustTarget::Firefox
    }

    /// [`FIREFOX_ID`], so the CLI reports Firefox on its own line.
    fn id(&self) -> &'static str {
        FIREFOX_ID
    }

    /// [`STORE_ID`]: the change is the system store's, so the CLI makes it (and
    /// shows its dialog) once.
    fn changes(&self) -> &'static str {
        STORE_ID
    }

    /// The system store, through Firefox.
    fn describe(&self) -> String {
        format!("Firefox, through the {}", self.system.describe())
    }

    /// The system store's location; Firefox reads it when it starts.
    fn location(&self) -> Option<String> {
        self.system
            .location()
            .map(|location| format!("{location}, read by Firefox at startup"))
    }

    /// The system store's dialog.
    fn approval(&self) -> Option<String> {
        self.system.approval()
    }

    /// The system store's pending dialog.
    async fn pending_approval(&self, ca: &CaCertificate, install: bool) -> Option<String> {
        self.system.pending_approval(ca, install).await
    }

    /// Names the profiles that turned [`ENTERPRISE_ROOTS_PREF`] off.
    async fn hint(&self) -> Option<String> {
        let dirs = self.dirs.clone();
        let disabled = blocking("reading Firefox profiles", move || {
            enterprise_roots_disabled(&dirs.profiles())
        })
        .await
        .ok()?;
        if disabled.is_empty() {
            return None;
        }
        let (noun, verb) = if disabled.len() == 1 {
            ("profile", "sets")
        } else {
            ("profiles", "set")
        };
        Some(format!(
            "Firefox {noun} {} {verb} {ENTERPRISE_ROOTS_PREF} to false, so Firefox ignores the \
             Windows user root store; set it to true in about:config and restart Firefox",
            disabled.join(", ")
        ))
    }

    /// Adds the CA to the system store.
    async fn install(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        self.system.install(ca).await
    }

    /// Removes the CA from the system store, which is what made Firefox trust it.
    async fn uninstall(&self, ca: &CaCertificate) -> Result<(), TrustError> {
        self.system.uninstall(ca).await
    }

    /// The system store's status.
    async fn status(&self, ca: &CaCertificate) -> Result<TrustStatus, TrustError> {
        self.system.status(ca).await
    }
}

/// Folder names of the `profiles` whose preferences turn [`ENTERPRISE_ROOTS_PREF`]
/// off, sorted. Unreadable profiles and files count as default (on): a missing
/// Firefox is the common case.
pub fn enterprise_roots_disabled(profiles: &[PathBuf]) -> Vec<String> {
    let mut disabled: Vec<String> = profiles
        .iter()
        .filter(|profile| profile_value(profile) == Some(false))
        .filter_map(|profile| profile.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    disabled.sort();
    disabled
}

/// The preference's value in one profile. A `user_pref` wins over any default
/// (`pref` or `sticky_pref`), and `user.js`, applied after `prefs.js` when
/// Firefox starts, wins over `prefs.js`.
fn profile_value(profile: &Path) -> Option<bool> {
    let read = |file: &str| {
        std::fs::read_to_string(profile.join(file))
            .map(|text| file_prefs(&text, ENTERPRISE_ROOTS_PREF))
            .unwrap_or_default()
    };
    let (prefs, user) = (read("prefs.js"), read("user.js"));
    user.user.or(prefs.user).or(user.default).or(prefs.default)
}

/// The boolean values one prefs file gives a preference; the last of each kind wins.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PrefValues {
    /// From `user_pref(...)`.
    user: Option<bool>,
    /// From `pref(...)` or `sticky_pref(...)`, which only set the default.
    default: Option<bool>,
}

/// Reads `kind("name", true|false ...)` statements for `name` from a prefs file,
/// skipping `//`, `#` and `/* */` comments. Other value types are ignored.
fn file_prefs(text: &str, name: &str) -> PrefValues {
    let mut values = PrefValues::default();
    for window in tokens(text).windows(5) {
        let [
            Token::Word(kind),
            Token::Punct('('),
            Token::Str(pref),
            Token::Punct(','),
            Token::Word(value),
        ] = window
        else {
            continue;
        };
        if pref != name {
            continue;
        }
        let value = match value.as_str() {
            "true" => true,
            "false" => false,
            _ => continue,
        };
        match kind.as_str() {
            "user_pref" => values.user = Some(value),
            "pref" | "sticky_pref" => values.default = Some(value),
            _ => {}
        }
    }
    values
}

/// A token of the prefs file syntax.
#[derive(Debug, PartialEq, Eq)]
enum Token {
    /// A name, keyword or number.
    Word(String),
    /// A quoted string, escapes resolved to the escaped character.
    Str(String),
    /// Any other character outside whitespace.
    Punct(char),
}

/// Splits a prefs file into tokens, dropping comments.
fn tokens(text: &str) -> Vec<Token> {
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '+');
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                chars.by_ref().find(|&c| c == '\n');
            }
            '#' => {
                chars.by_ref().find(|&c| c == '\n');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for c in chars.by_ref() {
                    if previous == '*' && c == '/' {
                        break;
                    }
                    previous = c;
                }
            }
            quote @ ('"' | '\'') => {
                let mut string = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => string.extend(chars.next()),
                        _ if c == quote => break,
                        _ => string.push(c),
                    }
                }
                tokens.push(Token::Str(string));
            }
            c if word(c) => {
                let mut name = String::from(c);
                while let Some(&next) = chars.peek().filter(|&&next| word(next)) {
                    name.push(next);
                    chars.next();
                }
                tokens.push(Token::Word(name));
            }
            c if c.is_whitespace() => {}
            c => tokens.push(Token::Punct(c)),
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREF: &str = ENTERPRISE_ROOTS_PREF;

    #[test]
    fn reads_the_last_boolean_value_of_each_kind() {
        let text = "// comment\nuser_pref(\"security.enterprise_roots.enabled\", true);\n\
                    user_pref(\"security.enterprise_roots.enabled.other\", false);\n\
                    user_pref( \"security.enterprise_roots.enabled\" ,false );\n\
                    pref('security.enterprise_roots.enabled', true);\n";
        assert_eq!(
            file_prefs(text, PREF),
            PrefValues {
                user: Some(false),
                default: Some(true)
            }
        );
        assert_eq!(file_prefs("", PREF), PrefValues::default());
        assert_eq!(
            file_prefs(
                "user_pref(\"security.enterprise_roots.enabled\", \"x\");",
                PREF
            ),
            PrefValues::default()
        );
    }

    #[test]
    fn comments_are_skipped() {
        let text = "/* user_pref(\"security.enterprise_roots.enabled\", false);\n\
                    still a comment */ user_pref(\"security.enterprise_roots.enabled\", true);\n\
                    # user_pref(\"security.enterprise_roots.enabled\", false);\n\
                    // user_pref(\"security.enterprise_roots.enabled\", false);\n\
                    user_pref(\"a\", \"/* not a comment\"); sticky_pref(\"security.enterprise_roots.enabled\", false);\n\
                    /**/ user_pref(\"b\", 'it\\'s');";
        assert_eq!(
            file_prefs(text, PREF),
            PrefValues {
                user: Some(true),
                default: Some(false)
            }
        );
    }

    #[test]
    fn profiles_ini_lists_relative_and_absolute_paths() {
        let dir = Path::new(r"C:\Users\u\AppData\Roaming\Mozilla\Firefox");
        let ini = "\u{feff}[General]\nStartWithLastProfile=1\n\n\
                   [Profile1]\nName=work\nIsRelative=0\nPath=D:\\firefox\\work\n\n\
                   [Install308046B0AF4A39CB]\nDefault=Profiles/x.ignored\n\n\
                   [Profile0]\r\nName=default-release\r\nIsRelative=1\r\nPath=Profiles/a.default-release\r\nDefault=1\r\n\
                   [Profile2]\nName=no path\nIsRelative=1\n";
        assert_eq!(
            ini_profiles(ini, dir),
            [
                PathBuf::from(r"D:\firefox\work"),
                dir.join(r"Profiles\a.default-release"),
            ]
        );
    }
}
