//! The committed screens in `crates/tui/screens/` match what the TUI draws.
//! The scenes and the file format come from the `screens` example; see
//! `crates/tui/screens/AGENTS.md`.

#[path = "../../examples/screens/data.rs"]
mod data;
#[path = "../../examples/screens/scenes.rs"]
mod scenes;

use std::{error::Error, fs, path::Path};

/// Fails when a committed screen is stale, missing or no longer drawn;
/// `DEVLANES_BLESS=1` rewrites them.
#[test]
fn screens_are_current() -> Result<(), Box<dyn Error>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(scenes::DIR);
    if std::env::var("DEVLANES_BLESS").as_deref() == Ok("1") {
        return scenes::bless(&dir);
    }
    let files = scenes::files()?;
    let mut stale: Vec<String> = files
        .iter()
        .filter(|(name, text)| {
            fs::read_to_string(dir.join(name)).ok().as_deref() != Some(text.as_str())
        })
        .map(|(name, _)| format!("changed: {name}"))
        .collect();
    for entry in fs::read_dir(&dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.ends_with(".txt") && !files.contains_key(&name) {
            stale.push(format!("no longer drawn: {name}"));
        }
    }
    let shown = stale.len().min(20);
    assert!(
        stale.is_empty(),
        "{} file(s) in crates/tui/screens/ differ from what the TUI draws now:\n  {}{}\n\n\
         This is expected when a change alters what the TUI shows. Regenerate them with\n  \
         DEVLANES_BLESS=1 cargo nextest run -p devlanes-tui -E 'test(screens_are_current)'\n\
         then check `git diff crates/tui/screens` shows what you meant to change and commit \
         the files with your change. CI fails while they are stale. See \
         crates/tui/screens/AGENTS.md.",
        stale.len(),
        stale[..shown].join("\n  "),
        if stale.len() > shown { "\n  …" } else { "" },
    );
    Ok(())
}
