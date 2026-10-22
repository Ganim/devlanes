//! Every error, warning and reason code the daemon sends has CLI wording.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Codes that only carry a message for logs and bug reports: internal failures
/// (`internal(code, error)`) and words that look like codes but are states.
const INTERNAL: &[&str] = &[
    // `internal()` failures: the generic `code: message` line is enough.
    "config_check",
    "encode",
    "input_failed",
    "io",
    "log_open_failed",
    "log_query_failed",
    "log_task_failed",
    // A spawn failure's note that could not reach the unit's log; logged only.
    "log_write_failed",
    "resize_failed",
    "spawn_task",
    "stop_failed",
    // Hosts sync states in `doctor`, matched by the tuple pattern.
    "applied",
    // Config check warnings rendered by `config_check` (its own wording, or the
    // daemon's `message`).
    "unknown_platform",
    "shell_fallback",
];

/// `.rs` files under `dir`, recursively.
fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read daemon sources") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// The `snake_case` string literal right after each `marker` in `text`, skipping
/// whitespace (code calls are often split over lines).
fn literals_after(text: &str, marker: &str, codes: &mut BTreeSet<String>) {
    for (index, _) in text.match_indices(marker) {
        let rest = text[index + marker.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else { continue };
        let literal = &rest[..end];
        let is_code =
            !literal.is_empty() && literal.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
        // A code is followed by `,` or `)` (a call or tuple), or is a json value.
        let after = rest[end + 1..].trim_start();
        let shaped = after.starts_with(',') || after.starts_with(')') || after.starts_with('}');
        if is_code && shaped {
            codes.insert(literal.to_owned());
        }
    }
}

/// Every code literal of the daemon: `RpcError::new("x", ..)`, `internal("x", ..)`,
/// phase `reason("x", ..)`, `("x", json!(..))` tuples, `"code": "x"` and
/// `code: "x".into()`.
fn daemon_codes() -> BTreeSet<String> {
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../daemon/src"),
        &mut files,
    );
    let mut codes = BTreeSet::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        for marker in [
            "RpcError::new(",
            "internal(",
            "reason(",
            "\"code\":",
            "code:",
        ] {
            literals_after(&text, marker, &mut codes);
        }
        // `("pool_exhausted", json!(..))` in error mapping tables.
        for (index, _) in text.match_indices("\",") {
            let after = text[index + 2..].trim_start();
            if !after.starts_with("json!") {
                continue;
            }
            let before = &text[..index];
            let Some(open) = before.rfind('"') else {
                continue;
            };
            if before[..open].trim_end().ends_with('(') {
                let literal = &before[open + 1..];
                if literal.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                    codes.insert(literal.to_owned());
                }
            }
        }
    }
    codes
}

#[test]
fn every_daemon_code_has_wording() {
    let codes = daemon_codes();
    assert!(
        codes.len() > 40 && codes.contains("pool_exhausted") && codes.contains("lane_running"),
        "the scan found too little: {codes:?}"
    );
    let missing: Vec<&String> = codes
        .iter()
        .filter(|code| !INTERNAL.contains(&code.as_str()))
        .filter(|code| !devlanes_cli::has_wording(code))
        .collect();
    assert!(
        missing.is_empty(),
        "daemon codes without CLI wording (add them to render/words.rs, or to INTERNAL here when only logs need them): {missing:?}"
    );
    // CLI-side codes and the ones the daemon is adding.
    for code in ["start_failed", "no_url", "no_config", "unsafe_home"] {
        assert!(devlanes_cli::has_wording(code), "{code}");
    }
    // The allowlist only names codes the daemon still uses.
    for code in INTERNAL {
        assert!(codes.contains(*code), "{code} is no longer sent; drop it");
    }
}
