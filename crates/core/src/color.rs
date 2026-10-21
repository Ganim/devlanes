//! Unit colors for clients: services, tasks, exec units and containers share one
//! pure rule so the TUI, CLI and daemon agree, on every start and every machine.

use std::collections::BTreeMap;

/// Terminal palette names, the same spellings as `tags.<name>.color` in config.
pub const PALETTE: &[&str] = &[
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "gray",
    "bright-red",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-magenta",
    "bright-cyan",
];

/// Reserved for failed units; inference never returns it.
pub const FAILURE_COLOR: &str = "red";

/// Colors the hash fallback picks from: the palette without the reds, which
/// clients use for failures.
const HASHED: &[&str] = &[
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "gray",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-magenta",
    "bright-cyan",
];

/// Category words and their colors, checked against tags, the preset and name
/// tokens. Categories that often run side by side get different hues: admin
/// next to api is white rather than bright-blue, desktop next to web is
/// bright-cyan rather than bright-green.
const CATEGORIES: &[(&[&str], &str)] = &[
    (&["api", "server", "backend", "gateway"], "blue"),
    (
        &[
            "web", "app", "frontend", "ui", "site", "portal", "vite", "next",
        ],
        "green",
    ),
    (
        &[
            "worker",
            "queue",
            "consumer",
            "job",
            "jobs",
            "cron",
            "scheduler",
        ],
        "magenta",
    ),
    (
        &[
            "db", "database", "postgres", "pg", "redis", "mysql", "mongo",
        ],
        "yellow",
    ),
    (&["docs", "doc"], "cyan"),
    (&["admin"], "white"),
    (
        &["desktop", "mobile", "tauri", "electron", "ios", "android"],
        "bright-cyan",
    ),
    (&["stub", "mock", "fake"], "gray"),
];

/// Picks a unit's color.
///
/// In order: the first of `tags` with a configured color in `overrides` (tag to
/// palette name; unknown names are ignored); the first tag in the category table;
/// the preset in the table; the first name token (split on `-`, `_`, `.`) in the
/// table; else an FNV-1a hash of the first tag, or of `name` without tags, into
/// the palette minus the reds. Matching ignores case.
///
/// ```
/// use devlanes_core::color::unit_color;
/// use std::collections::BTreeMap;
/// let none = BTreeMap::new();
/// assert_eq!(unit_color(&["api".into()], "catalog", None, &none), "blue");
/// assert_eq!(unit_color(&[], "user-web", None, &none), "green");
/// ```
pub fn unit_color(
    tags: &[String],
    name: &str,
    preset: Option<&str>,
    overrides: &BTreeMap<String, String>,
) -> &'static str {
    for tag in tags {
        if let Some(color) = overrides.get(tag).and_then(|c| palette_entry(c)) {
            return color;
        }
    }
    let candidates = tags
        .iter()
        .map(String::as_str)
        .chain(preset)
        .chain(std::iter::once(name));
    for word in candidates {
        if let Some(color) = word.split(['-', '_', '.']).find_map(category) {
            return color;
        }
    }
    let seed = tags.first().map_or(name, String::as_str);
    let index = fnv1a(seed.to_ascii_lowercase().as_bytes()) % HASHED.len() as u64;
    // The modulo keeps the index in range; `usize` holds any value below 12.
    HASHED[usize::try_from(index).unwrap_or_default()]
}

/// Returns the static palette spelling for `name`, if it is one.
pub fn palette_entry(name: &str) -> Option<&'static str> {
    PALETTE.iter().copied().find(|c| *c == name)
}

/// Category color for one word.
fn category(word: &str) -> Option<&'static str> {
    let word = word.to_ascii_lowercase();
    CATEGORIES
        .iter()
        .find(|(words, _)| words.contains(&word.as_str()))
        .map(|(_, color)| *color)
}

/// 64-bit FNV-1a: fixed constants, so the result never depends on the process
/// (unlike std's randomized `DefaultHasher`).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Color with no overrides.
    fn plain(tags: &[&str], name: &str, preset: Option<&str>) -> &'static str {
        let tags: Vec<String> = tags.iter().map(|t| (*t).to_owned()).collect();
        unit_color(&tags, name, preset, &BTreeMap::new())
    }

    #[test]
    /// Tags, then preset, then name tokens, each through the category table.
    fn category_table() {
        let cases: &[(&[&str], &str, Option<&str>, &str)] = &[
            (&["api", "essential"], "catalog", None, "blue"),
            (&["essential", "web"], "x", None, "green"),
            (&["Worker"], "x", None, "magenta"),
            (&["db"], "x", None, "yellow"),
            (&[], "docs", None, "cyan"),
            (&[], "admin-panel", None, "white"),
            (&["desktop"], "x", None, "bright-cyan"),
            (&[], "sms-stub", None, "gray"),
            (&[], "user-web", Some("vite"), "green"),
            (&[], "shop", Some("next"), "green"),
            (&[], "shell", Some("tauri"), "bright-cyan"),
            (&[], "api_gateway", None, "blue"),
            (&[], "jobs.cron", None, "magenta"),
            (&[], "postgres", None, "yellow"),
            // A category tag beats the preset and the name.
            (&["db"], "web", Some("vite"), "yellow"),
            // The preset beats the name.
            (&[], "api", Some("vite"), "green"),
        ];
        for (tags, name, preset, expected) in cases {
            assert_eq!(
                plain(tags, name, *preset),
                *expected,
                "{tags:?} {name} {preset:?}"
            );
        }
    }

    #[test]
    /// The first tag with a configured color wins over every category.
    fn overrides_win() {
        let overrides = BTreeMap::from([
            ("essential".to_owned(), "bright-yellow".to_owned()),
            ("bogus".to_owned(), "not-a-color".to_owned()),
        ]);
        let tags = vec!["api".to_owned(), "essential".to_owned()];
        assert_eq!(unit_color(&tags, "api", None, &overrides), "bright-yellow");
        let invalid = vec!["bogus".to_owned(), "web".to_owned()];
        assert_eq!(unit_color(&invalid, "x", None, &overrides), "green");
        // An override may use red on purpose; only inference avoids it.
        let red = BTreeMap::from([("x".to_owned(), "red".to_owned())]);
        assert_eq!(unit_color(&["x".to_owned()], "x", None, &red), "red");
    }

    #[test]
    /// Hash fallback outputs are pinned: a change here recolors users' units.
    fn hash_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        let pinned: &[(&[&str], &str, &str)] = &[
            // The first tag is hashed, not the name.
            (&["essential"], "anything", "magenta"),
            (&["Essential"], "other", "magenta"),
            (&[], "realtime", "bright-yellow"),
            (&[], "extension", "blue"),
            (&[], "temis", "yellow"),
            (&[], "catalog", "cyan"),
        ];
        for (tags, name, expected) in pinned {
            assert_eq!(plain(tags, name, None), *expected, "{tags:?} {name}");
        }
    }

    #[test]
    /// Inference never returns red or bright-red, whatever the input.
    fn red_is_never_inferred() {
        for index in 0..5_000 {
            let name = format!("unit-{index}");
            let color = plain(&[], &name, None);
            assert!(color != FAILURE_COLOR && color != "bright-red", "{name}");
            assert!(palette_entry(color).is_some());
        }
    }
}
