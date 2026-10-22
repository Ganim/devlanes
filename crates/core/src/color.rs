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

/// Colors the hash fallback picks from. Not red (failures), yellow (focus and
/// attention), white and gray (they vanish on light or read as disabled) or a
/// bright variant, which differs from its base only in lightness on most
/// themes, so two families would read as one.
const HASHED: &[&str] = &["green", "blue", "magenta", "cyan"];

/// Category words and their colors, checked against tags, the preset and name
/// tokens. Categories that often run side by side get different hues: admin
/// next to api is bright-magenta rather than bright-blue, desktop next to web
/// is bright-cyan rather than bright-green.
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
            "workers",
            "wrk",
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
        "cyan",
    ),
    (&["docs", "doc"], "cyan"),
    (&["admin"], "bright-magenta"),
    (
        &["desktop", "mobile", "tauri", "electron", "ios", "android"],
        "bright-cyan",
    ),
    // Gray reads as "support service" in the TUI's bar, never as disabled.
    (&["stub", "stubs", "mock", "fake"], "gray"),
];

/// Picks a unit's color.
///
/// In order: the first of `tags` with a configured color in `overrides` (tag to
/// palette name; unknown names are ignored); the first tag in the category table;
/// the preset in the table; the name's first segment (split on `-`, `_`, `.`) in
/// the table; else an FNV-1a hash of the first tag, or of that first segment
/// without tags, into green, blue, magenta and cyan. Matching ignores case. Units
/// named with a shared prefix (`wrk-email`, `wrk-index`) so share a color.
///
/// ```
/// use devlanes_core::color::unit_color;
/// use std::collections::BTreeMap;
/// let none = BTreeMap::new();
/// assert_eq!(unit_color(&["api".into()], "catalog", None, &none), "blue");
/// assert_eq!(unit_color(&[], "web-user", None, &none), "green");
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
    // Only the name's first segment: a family such as `wrk-*` must not split
    // when one member's later word happens to be a category (`wrk-cron`).
    let prefix = name.split(['-', '_', '.']).next().unwrap_or(name);
    let candidates = tags.iter().map(String::as_str).chain(preset);
    for word in candidates {
        if let Some(color) = word.split(['-', '_', '.']).find_map(category) {
            return color;
        }
    }
    if let Some(color) = category(prefix) {
        return color;
    }
    let seed = tags.first().map_or(prefix, String::as_str);
    let index = fnv1a(seed.to_ascii_lowercase().as_bytes()) % HASHED.len() as u64;
    // The modulo keeps the index in range; `usize` holds any value below 4.
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
            (&["workers"], "x", None, "magenta"),
            (&[], "wrk-email", None, "magenta"),
            (&["stubs"], "x", None, "gray"),
            (&["db"], "x", None, "cyan"),
            (&[], "docs", None, "cyan"),
            (&[], "admin-panel", None, "bright-magenta"),
            (&["desktop"], "x", None, "bright-cyan"),
            (&[], "stub-sms", None, "gray"),
            (&[], "user-web", Some("vite"), "green"),
            (&[], "shop", Some("next"), "green"),
            (&[], "shell", Some("tauri"), "bright-cyan"),
            (&[], "api_gateway", None, "blue"),
            (&[], "jobs.cron", None, "magenta"),
            (&[], "postgres", None, "cyan"),
            // A category tag beats the preset and the name.
            (&["db"], "web", Some("vite"), "cyan"),
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
    /// Units sharing a name prefix share a color, whatever their other words;
    /// a tag still decides first.
    fn a_name_prefix_is_one_color() {
        for family in [
            ["wrk-email", "wrk-index", "wrk-cron"],
            ["api-gateway", "api-users", "api_admin"],
            ["acme.ledger", "acme.docs", "acme-web"],
        ] {
            let colors: Vec<&str> = family.iter().map(|name| plain(&[], name, None)).collect();
            assert!(
                colors.windows(2).all(|pair| pair[0] == pair[1]),
                "{family:?}: {colors:?}"
            );
        }
        assert_eq!(plain(&[], "api-gateway", None), "blue");
        assert_eq!(plain(&["web"], "wrk-email", None), "green");
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
            (&["essential"], "anything", "cyan"),
            (&["Essential"], "other", "cyan"),
            (&[], "realtime", "green"),
            (&[], "extension", "magenta"),
            (&[], "temis", "blue"),
            (&[], "catalog", "green"),
        ];
        for (tags, name, expected) in pinned {
            assert_eq!(plain(tags, name, None), *expected, "{tags:?} {name}");
        }
    }

    #[test]
    /// The hash never returns red, yellow, white, gray or a bright variant,
    /// whatever the input.
    fn the_hash_avoids_reserved_and_faint_colors() {
        for index in 0..5_000 {
            let name = format!("u{index}-x");
            let color = plain(&[], &name, None);
            assert!(
                matches!(color, "green" | "blue" | "magenta" | "cyan"),
                "{name}: {color}"
            );
            assert!(palette_entry(color).is_some());
        }
        assert_ne!(plain(&[], "unit-9", None), FAILURE_COLOR);
    }
}
