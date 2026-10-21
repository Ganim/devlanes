//! bun as a package manager: `bun run <script> <args>`.
//!
//! `bun [bun flags] run <script> [script flags]`: flags after the script name
//! go to the script (<https://bun.com/docs/runtime>). bun 1.3 also drops a
//! `--`, so none is added. The bun runtime's environment and trust settings
//! belong to the bun platform contributor, not here.

use crate::manager::Rules;

/// bun's argv rules.
pub(crate) struct Bun;

impl Rules for Bun {
    fn program(&self) -> &'static str {
        "bun"
    }

    fn needs_separator(&self) -> bool {
        false
    }
}
