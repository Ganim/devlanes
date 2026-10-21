//! `run_script` support for Makefile targets: `make <target>` in the
//! Makefile's directory.
//!
//! Target checks use a conservative parser. It reports a target as missing
//! only when the file has no construct that could define targets it cannot
//! see (includes, pattern rules, variable target names, `$(eval)`,
//! `.DEFAULT`); otherwise the check is [`TargetCheck::Unverifiable`] and make
//! decides at run time. Built-in implicit rules (`make foo` from `foo.c`) are
//! not modelled.
//!
//! [`MakeTargets`] adapts [`plan`] to `devlanes_core::contrib::ScriptRunner`.

mod adapter;

pub use adapter::MakeTargets;

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// Names GNU make tries, in order, when no `-f` is given.
const DEFAULT_NAMES: &[&str] = &["GNUmakefile", "makefile", "Makefile"];

/// Whether a target is defined, as far as the parser can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetCheck {
    Found,
    /// No rule defines it and nothing could define it indirectly.
    Missing {
        known: Vec<String>,
    },
    /// The file may define it in a way the parser does not follow.
    Unverifiable {
        reason: &'static str,
    },
}

/// Explicit targets of a Makefile and the first construct that hides others.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Targets {
    pub names: BTreeSet<String>,
    pub hidden_by: Option<&'static str>,
}

impl Targets {
    /// Checks one target against the parsed file.
    pub fn check(&self, target: &str) -> TargetCheck {
        if self.names.contains(target) {
            TargetCheck::Found
        } else if let Some(reason) = self.hidden_by {
            TargetCheck::Unverifiable { reason }
        } else {
            TargetCheck::Missing {
                known: self.names.iter().cloned().collect(),
            }
        }
    }
}

/// A `run_script` reference to a Makefile target.
#[derive(Debug, Clone, Copy)]
pub struct TargetRequest<'a> {
    /// Root of the lane's worktree; the Makefile must be inside it.
    pub worktree: &'a Path,
    /// Path to the Makefile, relative to `worktree` or absolute inside it.
    pub source: &'a Path,
    pub name: &'a str,
    /// Must be empty: make has no way to hand arguments to a recipe.
    pub extra_args: &'a [String],
}

/// What to spawn for a [`TargetRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv: Vec<String>,
    /// Directory containing the Makefile.
    pub cwd: PathBuf,
    /// [`TargetCheck::Found`] or [`TargetCheck::Unverifiable`]; missing
    /// targets are errors.
    pub check: TargetCheck,
}

/// Errors from resolving a Makefile target.
#[derive(Debug, thiserror::Error)]
pub enum MakeError {
    #[error("invalid make target `{name}`: {reason}")]
    InvalidName { name: String, reason: &'static str },
    #[error("`{}` is outside the lane worktree `{}`", .file.display(), .worktree.display())]
    OutsideWorktree { file: PathBuf, worktree: PathBuf },
    #[error("cannot read `{}`", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
    #[error("`{}` has no target `{target}` ({})", .file.display(), known_hint(.known))]
    MissingTarget {
        file: PathBuf,
        target: String,
        known: Vec<String>,
    },
    #[error(
        "make cannot pass arguments to target `{target}` (got {}); use a package.json script or a justfile recipe with a `*ARGS` parameter, or `run`",
        .args.join(" ")
    )]
    ExtraArgs { target: String, args: Vec<String> },
}

/// Formats the target list for [`MakeError::MissingTarget`].
fn known_hint(known: &[String]) -> String {
    if known.is_empty() {
        "it defines no explicit targets".to_owned()
    } else {
        format!("targets: {}", known.join(", "))
    }
}

/// Returns whether `path` names a file this runner handles.
pub fn handles(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    DEFAULT_NAMES.contains(&name)
        || path
            .extension()
            .is_some_and(|ext| ext == "mk" || ext == "mak")
}

/// Resolves a target reference into the command to spawn.
///
/// Reads the Makefile and lists its directory. Errors when the target is
/// certainly missing or arguments were supplied.
pub fn plan(request: &TargetRequest<'_>) -> Result<LaunchPlan, MakeError> {
    validate_name(request.name)?;
    let worktree = normalize(request.worktree);
    let file = normalize(&worktree.join(request.source));
    if !file.starts_with(&worktree) {
        return Err(MakeError::OutsideWorktree { file, worktree });
    }
    if !request.extra_args.is_empty() {
        return Err(MakeError::ExtraArgs {
            target: request.name.to_owned(),
            args: request.extra_args.to_vec(),
        });
    }
    let read_error = |path: &Path| {
        let path = path.to_owned();
        move |error| MakeError::Read { path, error }
    };
    let contents = std::fs::read_to_string(&file).map_err(read_error(&file))?;
    let check = parse(&contents).check(request.name);
    if let TargetCheck::Missing { known } = check {
        return Err(MakeError::MissingTarget {
            file,
            target: request.name.to_owned(),
            known,
        });
    }
    let cwd = file
        .parent()
        .map_or_else(|| worktree.clone(), Path::to_path_buf);
    let file_name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut argv = vec!["make".to_owned()];
    // `-f` only when make would pick another file by itself, so the common
    // case renders as plain `make <target>`.
    if default_makefile(&cwd).map_err(read_error(&cwd))?.as_deref() != Some(file_name.as_str()) {
        argv.push("-f".to_owned());
        argv.push(file_name);
    }
    argv.push(request.name.to_owned());
    Ok(LaunchPlan { argv, cwd, check })
}

/// Returns the file GNU make reads in `dir` without `-f`. Compares exact
/// entry names because `Path::exists` is case-insensitive on default macOS
/// volumes and would report `makefile` for a `Makefile`.
fn default_makefile(dir: &Path) -> std::io::Result<Option<String>> {
    let mut present = BTreeSet::new();
    for entry in std::fs::read_dir(dir)? {
        present.insert(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(DEFAULT_NAMES
        .iter()
        .find(|name| present.contains(**name))
        .map(|name| (*name).to_owned()))
}

/// Rejects names make would read as an option or a variable assignment.
fn validate_name(name: &str) -> Result<(), MakeError> {
    let reason = if name.is_empty() {
        "it is empty"
    } else if name.starts_with('-') {
        "it starts with `-` and would be read as an option"
    } else if name.contains('=') {
        "it contains `=` and would be read as a variable assignment"
    } else if name.chars().any(char::is_whitespace) {
        "it contains whitespace"
    } else {
        return Ok(());
    };
    Err(MakeError::InvalidName {
        name: name.to_owned(),
        reason,
    })
}

/// How a logical line starts, after joining continuations.
enum LineKind<'a> {
    Rule { targets: &'a str, rest: &'a str },
    Assignment,
    Other,
}

/// Parses the explicit targets of a Makefile.
pub fn parse(contents: &str) -> Targets {
    let mut targets = Targets::default();
    let mut hide = |reason| {
        targets.hidden_by.get_or_insert(reason);
    };
    let mut names = BTreeSet::new();
    let mut in_define = false;
    for line in logical_lines(contents) {
        // Recipe lines, including their continuations, never define targets.
        if line.starts_with('\t') {
            continue;
        }
        let line = strip_comment(&line);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let first = trimmed.split_whitespace().next().unwrap_or_default();
        if in_define {
            in_define = first != "endef";
            continue;
        }
        let word_after_modifiers = trimmed
            .split_whitespace()
            .find(|word| !matches!(*word, "export" | "override" | "private" | "unexport"))
            .unwrap_or_default();
        if word_after_modifiers == "define" {
            in_define = true;
            continue;
        }
        match first {
            "include" | "-include" | "sinclude" => {
                hide("it includes other makefiles");
                continue;
            }
            "ifeq" | "ifneq" | "ifdef" | "ifndef" | "else" | "endif" | "vpath" => continue,
            _ => {}
        }
        match classify(trimmed) {
            LineKind::Assignment => {
                if trimmed.starts_with(".RECIPEPREFIX") {
                    hide("it changes .RECIPEPREFIX");
                }
            }
            LineKind::Other => {
                if trimmed.contains("$(eval") || trimmed.contains("${eval") {
                    hide("it generates rules with $(eval)");
                }
            }
            LineKind::Rule {
                targets: left,
                rest,
            } => {
                // `target: VAR = value` sets a target-specific variable and
                // defines no rule.
                let before_recipe = rest.split(';').next().unwrap_or_default();
                if matches!(classify(before_recipe), LineKind::Assignment) {
                    continue;
                }
                for name in left.split_whitespace() {
                    if name.contains('$') {
                        hide("a target name uses a variable");
                    } else if name.contains('%') {
                        hide("it has pattern rules");
                    } else if name == ".DEFAULT" {
                        hide("a .DEFAULT rule accepts any target");
                    } else if !is_special_target(name) {
                        names.insert(name.to_owned());
                    }
                }
            }
        }
    }
    targets.names = names;
    targets
}

/// Joins backslash-continued lines, keeping the first physical line's start
/// so recipe continuations stay recognisable.
fn logical_lines(contents: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for raw in contents.lines() {
        if let Some(stripped) = raw.strip_suffix('\\') {
            current.push_str(stripped);
            current.push(' ');
        } else {
            current.push_str(raw);
            lines.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Drops an unescaped `#` comment.
fn strip_comment(line: &str) -> &str {
    let mut previous = '\0';
    for (index, c) in line.char_indices() {
        if c == '#' && previous != '\\' {
            return &line[..index];
        }
        previous = c;
    }
    line
}

/// Finds the first `:` or `=` outside `$(...)`/`${...}`, which tells rules
/// from assignments (`=`, `:=`, `::=`, `:::=`, `?=`, `+=`, `!=`).
fn classify(line: &str) -> LineKind<'_> {
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'$' && matches!(bytes.get(index + 1), Some(b'(' | b'{')) {
            depth += 1;
            index += 2;
            continue;
        }
        if depth > 0 {
            match byte {
                b'(' | b'{' => depth += 1,
                b')' | b'}' => depth -= 1,
                _ => {}
            }
            index += 1;
            continue;
        }
        match byte {
            b'=' => return LineKind::Assignment,
            b':' => {
                let after = &line[index..];
                if after.starts_with(":=") || after.starts_with("::=") || after.starts_with(":::=")
                {
                    return LineKind::Assignment;
                }
                // Double-colon rules (`target::`) still define `target`.
                let rest = after.trim_start_matches(':');
                return LineKind::Rule {
                    targets: &line[..index],
                    rest,
                };
            }
            _ => {}
        }
        index += 1;
    }
    LineKind::Other
}

/// GNU make special targets such as `.PHONY`: a dot followed by capitals.
fn is_special_target(name: &str) -> bool {
    name.strip_prefix('.').is_some_and(|rest| {
        !rest.is_empty() && rest.chars().all(|c| c.is_ascii_uppercase() || c == '_')
    })
}

/// Resolves `.` and `..` lexically so the worktree check needs no filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(contents: &str) -> Vec<String> {
        parse(contents).names.into_iter().collect()
    }

    /// Rules, double-colon rules and multi-target rules are found; variables,
    /// target-specific variables, special targets and recipes are not.
    #[test]
    fn explicit_targets() {
        let makefile = "\
CC := cc
DEST ?= out
FLAGS += -O2
.PHONY: certs build
certs: ## generate certs
\tmkdir -p certs; echo done: ok
build test: deps
deps::
release: VERSION = 1
clean: ; rm -rf out
long-target \\
  other: x
define TEMPLATE
fake: rule
endef
ifeq ($(OS),a:b)
endif
";
        assert_eq!(
            names(makefile),
            [
                "build",
                "certs",
                "clean",
                "deps",
                "long-target",
                "other",
                "test"
            ]
        );
        assert_eq!(parse(makefile).hidden_by, None);
    }

    /// Constructs that can define targets the parser cannot see.
    #[test]
    fn unverifiable_constructs() {
        let cases = [
            ("include common.mk\n", "it includes other makefiles"),
            ("-include local.mk\n", "it includes other makefiles"),
            ("%.o: %.c\n\tcc $<\n", "it has pattern rules"),
            ("$(TARGETS): x\n", "a target name uses a variable"),
            (
                "$(foreach t,a b,$(eval $(t): ; echo))\n",
                "it generates rules with $(eval)",
            ),
            (
                ".DEFAULT:\n\techo $@\n",
                "a .DEFAULT rule accepts any target",
            ),
            (".RECIPEPREFIX = >\n", "it changes .RECIPEPREFIX"),
        ];
        for (contents, reason) in cases {
            assert_eq!(
                parse(contents).check("missing"),
                TargetCheck::Unverifiable { reason },
                "{contents:?}"
            );
        }
    }

    /// Substitution references with `:` and `=` inside `$()` are not rules.
    #[test]
    fn substitution_references_are_assignments() {
        assert_eq!(
            names("OBJS = $(SRCS:.c=.o)\nall: $(OBJS:%=build/%)\n"),
            ["all"]
        );
    }

    /// Phony declarations alone do not count as rules.
    #[test]
    fn phony_without_rule_is_missing() {
        assert_eq!(
            parse(".PHONY: certs\nbuild:\n").check("certs"),
            TargetCheck::Missing {
                known: vec!["build".to_owned()]
            }
        );
    }
}
