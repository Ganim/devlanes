//! `run_script` support for justfile recipes: `just <recipe>` in the
//! justfile's directory.
//!
//! Extra arguments go straight after the recipe name, without `--`: just
//! 1.59 binds `just dev --port 1` to a `*ARGS` parameter but passes a `--`
//! through as a literal argument. Only recipes whose last parameter is
//! variadic (`*ARGS` or `+ARGS`, <https://just.systems/man/en/recipe-parameters.html>)
//! accept them.
//!
//! The recipe check uses a conservative parser: a recipe is reported missing
//! only when the file has no `import` or `mod` that could provide it.
//!
//! [`JustRecipes`] adapts [`plan`] to `devlanes_core::contrib::ScriptRunner`.

mod adapter;

pub use adapter::JustRecipes;

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// How many trailing arguments a variadic parameter takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variadic {
    /// `*ARGS`
    ZeroOrMore,
    /// `+ARGS`
    OneOrMore,
}

/// A recipe header as far as argument passing is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub name: String,
    /// Positional parameters without a default.
    pub required: usize,
    pub variadic: Option<Variadic>,
    /// An `[arg(...)]` attribute turns parameters into options, so the
    /// positional counts above may not apply.
    pub has_option_params: bool,
}

/// Whether a recipe is defined, as far as the parser can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeCheck {
    Found(Recipe),
    Missing { known: Vec<String> },
    Unverifiable { reason: &'static str },
}

/// Recipes and aliases of a justfile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recipes {
    pub recipes: BTreeMap<String, Recipe>,
    /// Alias name to recipe name.
    pub aliases: BTreeMap<String, String>,
    /// First construct that may define recipes the parser cannot see.
    pub hidden_by: Option<&'static str>,
}

impl Recipes {
    /// Checks one recipe or alias name against the parsed file.
    pub fn check(&self, name: &str) -> RecipeCheck {
        let target = self.aliases.get(name).map_or(name, String::as_str);
        if let Some(recipe) = self.recipes.get(target) {
            RecipeCheck::Found(recipe.clone())
        } else if let Some(reason) = self.hidden_by {
            RecipeCheck::Unverifiable { reason }
        } else {
            RecipeCheck::Missing {
                known: self
                    .recipes
                    .keys()
                    .chain(self.aliases.keys())
                    .cloned()
                    .collect(),
            }
        }
    }
}

/// A `run_script` reference to a justfile recipe.
#[derive(Debug, Clone, Copy)]
pub struct RecipeRequest<'a> {
    /// Root of the lane's worktree; the justfile must be inside it.
    pub worktree: &'a Path,
    /// Path to the justfile, relative to `worktree` or absolute inside it.
    pub source: &'a Path,
    pub name: &'a str,
    /// Arguments for the recipe's variadic parameter, typically preset flags.
    pub extra_args: &'a [String],
}

/// What to spawn for a [`RecipeRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv: Vec<String>,
    /// Directory containing the justfile.
    pub cwd: PathBuf,
    /// [`RecipeCheck::Found`] or [`RecipeCheck::Unverifiable`].
    pub check: RecipeCheck,
}

/// Errors from resolving a justfile recipe.
#[derive(Debug, thiserror::Error)]
pub enum JustError {
    #[error("invalid recipe name `{name}`: {reason}")]
    InvalidName { name: String, reason: &'static str },
    #[error("`{}` is outside the lane worktree `{}`", .file.display(), .worktree.display())]
    OutsideWorktree { file: PathBuf, worktree: PathBuf },
    #[error("cannot read `{}`", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
    #[error("`{}` has no recipe `{recipe}` ({})", .file.display(), known_hint(.known))]
    MissingRecipe {
        file: PathBuf,
        recipe: String,
        known: Vec<String>,
    },
    #[error(
        "recipe `{recipe}` takes no extra arguments (got {}); declare a variadic last parameter, e.g. `{recipe} *ARGS:`, and use `{{{{ARGS}}}}` in its body",
        .args.join(" ")
    )]
    ExtraArgs { recipe: String, args: Vec<String> },
    #[error(
        "recipe `{recipe}` needs {count} positional argument(s), which run_script cannot supply; give the parameters defaults"
    )]
    RequiresArguments { recipe: String, count: usize },
}

/// Formats the recipe list for [`JustError::MissingRecipe`].
fn known_hint(known: &[String]) -> String {
    if known.is_empty() {
        "it defines no recipes".to_owned()
    } else {
        format!("recipes: {}", known.join(", "))
    }
}

/// Returns whether `path` names a file this runner handles. just finds
/// `justfile` and `.justfile` case-insensitively; `*.just` files need
/// `--justfile`.
pub fn handles(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| is_default_name(name) || name.ends_with(".just"))
}

/// Names `just` finds by itself when run in the file's directory.
fn is_default_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("justfile") || name.eq_ignore_ascii_case(".justfile")
}

/// Resolves a recipe reference into the command to spawn.
///
/// Reads the justfile. Errors when the recipe is certainly missing, cannot
/// take the extra arguments or needs positional arguments.
pub fn plan(request: &RecipeRequest<'_>) -> Result<LaunchPlan, JustError> {
    validate_name(request.name)?;
    let worktree = normalize(request.worktree);
    let file = normalize(&worktree.join(request.source));
    if !file.starts_with(&worktree) {
        return Err(JustError::OutsideWorktree { file, worktree });
    }
    let contents = std::fs::read_to_string(&file).map_err(|error| JustError::Read {
        path: file.clone(),
        error,
    })?;
    let check = parse(&contents).check(request.name);
    match &check {
        RecipeCheck::Missing { known } => {
            return Err(JustError::MissingRecipe {
                file,
                recipe: request.name.to_owned(),
                known: known.clone(),
            });
        }
        RecipeCheck::Found(recipe) => check_arguments(recipe, request.extra_args)?,
        RecipeCheck::Unverifiable { .. } => {}
    }
    let cwd = file
        .parent()
        .map_or_else(|| worktree.clone(), Path::to_path_buf);
    let file_name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut argv = vec!["just".to_owned()];
    if !is_default_name(&file_name) {
        argv.push("--justfile".to_owned());
        argv.push(file_name);
    }
    argv.push(request.name.to_owned());
    argv.extend(request.extra_args.iter().cloned());
    Ok(LaunchPlan { argv, cwd, check })
}

/// Rejects calls just would fail with an arity error.
fn check_arguments(recipe: &Recipe, extra_args: &[String]) -> Result<(), JustError> {
    if recipe.has_option_params {
        return Ok(());
    }
    let required = recipe.required + usize::from(recipe.variadic == Some(Variadic::OneOrMore));
    if recipe.required > 0 || (required > 0 && extra_args.is_empty()) {
        return Err(JustError::RequiresArguments {
            recipe: recipe.name.clone(),
            count: required,
        });
    }
    if !extra_args.is_empty() && recipe.variadic.is_none() {
        return Err(JustError::ExtraArgs {
            recipe: recipe.name.clone(),
            args: extra_args.to_vec(),
        });
    }
    Ok(())
}

/// Rejects names just would read as one of its own options.
fn validate_name(name: &str) -> Result<(), JustError> {
    let reason = if name.is_empty() {
        "it is empty"
    } else if name.starts_with('-') {
        "it starts with `-` and would be read as an option"
    } else {
        return Ok(());
    };
    Err(JustError::InvalidName {
        name: name.to_owned(),
        reason,
    })
}

/// Parses recipe headers and aliases of a justfile.
pub fn parse(contents: &str) -> Recipes {
    let mut parsed = Recipes::default();
    let mut option_attribute = false;
    for raw in contents.lines() {
        // Indented lines are recipe bodies.
        if raw.starts_with([' ', '\t']) {
            continue;
        }
        let mut line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Attributes apply to the next recipe and may share its line.
        while let Some(rest) = line.strip_prefix('[') {
            let end = rest.find(']').map_or(rest.len(), |end| end + 1);
            option_attribute |= rest[..end.min(rest.len())].contains("arg(");
            line = rest.get(end..).unwrap_or_default().trim_start();
        }
        if line.is_empty() {
            continue;
        }
        let first = line.split_whitespace().next().unwrap_or_default();
        match first {
            "import" | "import?" => {
                parsed.hidden_by.get_or_insert("it imports other justfiles");
                continue;
            }
            "mod" | "mod?" => {
                parsed.hidden_by.get_or_insert("it has modules");
                continue;
            }
            "alias" => {
                if let Some((name, target)) = line["alias".len()..].split_once(":=") {
                    parsed
                        .aliases
                        .insert(name.trim().to_owned(), target.trim().to_owned());
                }
                continue;
            }
            "set" | "export" | "unexport" => continue,
            _ => {}
        }
        if let Some(recipe) = parse_header(line, option_attribute) {
            parsed.recipes.insert(recipe.name.clone(), recipe);
        }
        option_attribute = false;
    }
    parsed
}

/// Parses `@name param='default' +ARGS: deps`; `None` for assignments and
/// anything else that is not a recipe header.
fn parse_header(line: &str, has_option_params: bool) -> Option<Recipe> {
    let line = line.strip_prefix('@').unwrap_or(line);
    let header = &line[..header_colon(line)?];
    let mut tokens = split_tokens(header).into_iter();
    let name = tokens.next()?;
    let valid_start = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !valid_start
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let mut recipe = Recipe {
        name: name.to_owned(),
        required: 0,
        variadic: None,
        has_option_params,
    };
    for token in tokens {
        // A `$` prefix exports the parameter; it does not change arity.
        let token = token.strip_prefix('$').unwrap_or(token);
        if token.starts_with('+') {
            recipe.variadic = Some(Variadic::OneOrMore);
        } else if token.starts_with('*') {
            recipe.variadic = Some(Variadic::ZeroOrMore);
        } else if !token.contains('=') {
            recipe.required += 1;
        }
    }
    Some(recipe)
}

/// Finds the `:` ending a recipe header, skipping quotes and parentheses in
/// default values. Returns `None` for `:=` assignments.
fn header_colon(line: &str) -> Option<usize> {
    let mut quote = None;
    let mut depth = 0usize;
    let mut chars = line.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '`') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ':') if depth == 0 => {
                return match chars.peek() {
                    Some((_, '=')) => None,
                    _ => Some(index),
                };
            }
            _ => {}
        }
    }
    None
}

/// Splits a header on whitespace outside quotes and parentheses.
fn split_tokens(header: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut quote = None;
    let mut depth = 0usize;
    let mut start = None;
    for (index, c) in header.char_indices() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '`') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, c) if c.is_whitespace() && depth == 0 => {
                if let Some(begin) = start.take() {
                    tokens.push(&header[begin..index]);
                }
                continue;
            }
            _ => {}
        }
        start.get_or_insert(index);
    }
    if let Some(begin) = start {
        tokens.push(&header[begin..]);
    }
    tokens
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

    const JUSTFILE: &str = r#"
set dotenv-load
export RUST_LOG := "info"
version := `git describe`

alias f := fixtures

# Load fixtures.
@fixtures:
    ./scripts/fixtures.sh

[private]
dev *ARGS: build
    vite {{ARGS}}

serve url="http://127.0.0.1:1" +FLAGS='-q':
    echo {{url}} {{FLAGS}}

deploy env $region:
    echo {{env}} {{region}}

[arg("force", long="force", value="1")]
wipe force="0":
    echo {{force}}
"#;

    /// Headers yield arity; assignments, settings and bodies are skipped.
    #[test]
    fn parses_headers() {
        let parsed = parse(JUSTFILE);
        let summary: Vec<String> = parsed
            .recipes
            .values()
            .map(|r| {
                format!(
                    "{} required={} variadic={:?} options={}",
                    r.name, r.required, r.variadic, r.has_option_params
                )
            })
            .collect();
        insta::assert_snapshot!(summary.join("\n"), @r"
        deploy required=2 variadic=None options=false
        dev required=0 variadic=Some(ZeroOrMore) options=false
        fixtures required=0 variadic=None options=false
        serve required=0 variadic=Some(OneOrMore) options=false
        wipe required=0 variadic=None options=true
        ");
        assert_eq!(
            parsed.aliases.get("f").map(String::as_str),
            Some("fixtures")
        );
        assert_eq!(parsed.hidden_by, None);
    }

    /// Aliases resolve; unknown names list what exists.
    #[test]
    fn checks_names() {
        let parsed = parse(JUSTFILE);
        assert!(
            matches!(parsed.check("f"), RecipeCheck::Found(recipe) if recipe.name == "fixtures")
        );
        assert!(matches!(
            parsed.check("version"),
            RecipeCheck::Missing { .. }
        ));
    }

    /// Imports and modules may provide recipes the parser cannot see.
    #[test]
    fn imports_make_missing_unverifiable() {
        for (contents, reason) in [
            ("import 'common.just'\n", "it imports other justfiles"),
            ("import? 'local.just'\n", "it imports other justfiles"),
            ("mod tools\n", "it has modules"),
        ] {
            assert_eq!(
                parse(contents).check("x"),
                RecipeCheck::Unverifiable { reason }
            );
        }
    }
}
