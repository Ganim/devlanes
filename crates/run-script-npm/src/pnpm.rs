//! pnpm: `pnpm run <script> <args>`.
//!
//! "Options listed after the script's name are passed to the executed script"
//! (<https://pnpm.io/cli/run>). pnpm 12.10 forwards a `--` literally, and CLIs
//! such as Vite then treat the flags after it as positionals, so none is added.

use crate::manager::Rules;

/// pnpm's argv rules.
pub(crate) struct Pnpm;

impl Rules for Pnpm {
    fn program(&self) -> &'static str {
        "pnpm"
    }

    fn needs_separator(&self) -> bool {
        false
    }
}
