//! npm: `npm run <script> -- <args>`.
//!
//! "Use `--` to pass `-`-prefixed flags and options which would otherwise be
//! parsed by npm" (<https://docs.npmjs.com/cli/v11/commands/npm-run>). Without
//! it npm 11.17 consumes `--port` itself and passes only `3000` on.

use crate::manager::Rules;

/// npm's argv rules.
pub(crate) struct Npm;

impl Rules for Npm {
    fn program(&self) -> &'static str {
        "npm"
    }

    fn needs_separator(&self) -> bool {
        true
    }
}
