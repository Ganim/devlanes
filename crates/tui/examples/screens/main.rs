//! Draws every TUI page and interaction state against a fake daemon, for
//! design review: `cargo run -p devlanes-tui --example screens`.
//!
//! Each state is written at several terminal sizes into `crates/tui/screens/`:
//! the committed text (the screen and the styles of its cells, see
//! `scenes.rs`) with an `index.md` listing them, and next to it a PNG and an
//! ANSI dump (`cat` it in a true-color terminal), which are gitignored. A
//! first argument replaces the directory; `DEVLANES_SCREENS_FONT` names a
//! font file to draw with first.

mod data;
mod paint;
mod scenes;

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use devlanes_tui::theme::Tier;

fn main() -> Result<(), Box<dyn Error>> {
    let out = std::env::args().nth(1).map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join(scenes::DIR),
        PathBuf::from,
    );
    scenes::bless(&out)?;
    let fonts = paint::Fonts::load();
    if fonts.is_none() {
        eprintln!(
            "no monospace font found; writing text and ANSI only (set DEVLANES_SCREENS_FONT)"
        );
    }
    for shot in scenes::shots() {
        let buffer = scenes::render(&shot.app)?;
        fs::write(
            out.join(format!("{}.ansi", shot.base)),
            paint::ansi(&buffer),
        )?;
        if let Some(fonts) = &fonts {
            let light = shot.app.theme.tier == Tier::Light;
            paint::png(
                &buffer,
                fonts,
                &out.join(format!("{}.png", shot.base)),
                light,
            )?;
        }
    }
    println!("{}", out.join("index.md").display());
    Ok(())
}
