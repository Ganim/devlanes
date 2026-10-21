//! The package manager enum and the rules each manager module implements.

use crate::{bun, npm, pnpm, yarn};

/// Package managers that can run `package.json` scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PackageManager {
    Npm,
    Pnpm,
    /// Yarn 1.x.
    YarnClassic,
    /// Yarn 2 and later.
    YarnBerry,
    Bun,
}

/// Argv rules owned by each manager's module.
pub(crate) trait Rules: Sync {
    /// Executable looked up on `PATH`.
    fn program(&self) -> &'static str;

    /// Whether script arguments need a `--` after the script name.
    fn needs_separator(&self) -> bool;

    /// Renders `<program> run <script> [--] <extra_args>`.
    fn run_argv(&self, script: &str, extra_args: &[String]) -> Vec<String> {
        let mut argv = vec![
            self.program().to_owned(),
            "run".to_owned(),
            script.to_owned(),
        ];
        if !extra_args.is_empty() && self.needs_separator() {
            argv.push("--".to_owned());
        }
        argv.extend(extra_args.iter().cloned());
        argv
    }
}

impl PackageManager {
    /// Returns the rules from the manager's module.
    fn rules(self) -> &'static dyn Rules {
        match self {
            Self::Npm => &npm::Npm,
            Self::Pnpm => &pnpm::Pnpm,
            Self::YarnClassic => &yarn::YarnClassic,
            Self::YarnBerry => &yarn::YarnBerry,
            Self::Bun => &bun::Bun,
        }
    }

    /// Returns the executable name looked up on `PATH`.
    pub fn program(self) -> &'static str {
        self.rules().program()
    }

    /// Returns the name used in `packageManager` and `devEngines`.
    pub fn field_name(self) -> &'static str {
        self.family().name()
    }

    /// Returns whether arguments for the script need a `--` after its name.
    /// Only npm does; each module documents its manager's behaviour.
    pub fn needs_separator(self) -> bool {
        self.rules().needs_separator()
    }

    /// Renders `<pm> run <script> [--] <extra_args>`, adding `--` only when
    /// [`Self::needs_separator`] is true and there are arguments to forward.
    pub fn run_argv(self, script: &str, extra_args: &[String]) -> Vec<String> {
        self.rules().run_argv(script, extra_args)
    }

    /// Returns the family, which is what lockfiles and declared fields name.
    pub(crate) fn family(self) -> Family {
        match self {
            Self::Npm => Family::Npm,
            Self::Pnpm => Family::Pnpm,
            Self::YarnClassic | Self::YarnBerry => Family::Yarn,
            Self::Bun => Family::Bun,
        }
    }
}

/// Managers as lockfiles and `packageManager` name them; yarn's major
/// version is resolved separately in [`yarn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl Family {
    /// Parses a manager name as written in `packageManager`/`devEngines`.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name.trim() {
            "npm" => Self::Npm,
            "pnpm" => Self::Pnpm,
            "yarn" => Self::Yarn,
            "bun" => Self::Bun,
            _ => return None,
        })
    }

    /// Returns the name used in `packageManager`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
        }
    }
}
