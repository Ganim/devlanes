//! The `firefox` target on Windows: Firefox imports the roots in the Windows store
//! when `security.enterprise_roots.enabled` is true (the default), so this store
//! changes and reports the user root store and only adds a hint when a profile
//! turned the preference off. Profiles are read, never written.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use devlanes_trust::{CaCertificate, TrustError, TrustStatus, TrustStore, TrustTarget};

use crate::{FIREFOX_ID, UserRootStore, blocking};

/// The preference that makes Firefox import roots from the OS store.
pub const ENTERPRISE_ROOTS_PREF: &str = "security.enterprise_roots.enabled";

/// Where Firefox keeps profiles on Windows, under `%APPDATA%`.
pub fn firefox_profile_roots(appdata: &Path) -> Vec<PathBuf> {
    vec![appdata.join(r"Mozilla\Firefox\Profiles")]
}

/// Firefox reporting on, and changing, the Windows user root store.
pub struct FirefoxStore {
    system: UserRootStore,
    roots: Vec<PathBuf>,
}

impl FirefoxStore {
    /// Reports on `system` and reads the profiles under `roots` for the hint.
    pub fn new(system: UserRootStore, roots: Vec<PathBuf>) -> Self {
        Self { system, roots }
    }
}

#[async_trait]
impl TrustStore for FirefoxStore {
    /// Always [`TrustTarget::Firefox`].
    fn target(&self) -> TrustTarget {
        TrustTarget::Firefox
    }

    /// [`FIREFOX_ID`], so the CLI reports Firefox on its own line even when
    /// `system` already changed the store.
    fn id(&self) -> &'static str {
        FIREFOX_ID
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

    /// Names the profiles that turned [`ENTERPRISE_ROOTS_PREF`] off.
    async fn hint(&self) -> Option<String> {
        let roots = self.roots.clone();
        let disabled = blocking("reading Firefox profiles", move || {
            enterprise_roots_disabled(&roots)
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

/// Names of the profiles under `roots` whose preferences turn
/// [`ENTERPRISE_ROOTS_PREF`] off, sorted. Unreadable roots and files count as
/// default (on): a missing Firefox is the common case.
pub fn enterprise_roots_disabled(roots: &[PathBuf]) -> Vec<String> {
    let mut disabled = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let profile = entry.path();
            if profile_value(&profile) == Some(false) {
                disabled.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    disabled.sort();
    disabled
}

/// The preference's value in one profile: `user.js` is applied after `prefs.js`
/// when Firefox starts, so its value wins.
fn profile_value(profile: &Path) -> Option<bool> {
    ["prefs.js", "user.js"]
        .iter()
        .filter_map(|file| std::fs::read_to_string(profile.join(file)).ok())
        .filter_map(|text| pref_value(&text, ENTERPRISE_ROOTS_PREF))
        .next_back()
}

/// The last boolean `user_pref("<name>", ...)` or `pref(...)` in a prefs file.
fn pref_value(text: &str, name: &str) -> Option<bool> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line
                .strip_prefix("user_pref(")
                .or_else(|| line.strip_prefix("pref("))?;
            let rest = rest
                .trim_start()
                .strip_prefix('"')?
                .strip_prefix(name)?
                .strip_prefix('"')?;
            let value = rest.trim_start().strip_prefix(',')?.trim_start();
            if value.starts_with("true") {
                Some(true)
            } else if value.starts_with("false") {
                Some(false)
            } else {
                None
            }
        })
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_last_boolean_value() {
        let text = "// comment\nuser_pref(\"security.enterprise_roots.enabled\", true);\n\
                    user_pref(\"security.enterprise_roots.enabled.other\", false);\n\
                    user_pref( \"security.enterprise_roots.enabled\" ,false );\n";
        assert_eq!(pref_value(text, ENTERPRISE_ROOTS_PREF), Some(false));
        assert_eq!(pref_value("", ENTERPRISE_ROOTS_PREF), None);
        assert_eq!(
            pref_value(
                "user_pref(\"security.enterprise_roots.enabled\", \"x\");",
                ENTERPRISE_ROOTS_PREF
            ),
            None
        );
    }
}
