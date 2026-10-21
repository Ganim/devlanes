# devlanes-run-script-just

Runs `run_script: justfile#<recipe>` as `just <recipe>` in the justfile's directory. Lane resolution calls it through `JustRecipes`, a `devlanes-core` `ScriptRunner`.

## Usage

```rust
use std::path::Path;
use devlanes_run_script_just::{RecipeRequest, plan};

// justfile:  dev *ARGS:
//                vite {{ARGS}}
let args = vec!["--port".to_owned(), "20012".to_owned()];
let launch = plan(&RecipeRequest {
    worktree: Path::new("/lanes/acme-1"),
    source: Path::new("justfile"),
    name: "dev",
    extra_args: &args,
})?;
assert_eq!(launch.argv, ["just", "dev", "--port", "20012"]);
```

The composition root registers `Arc::new(JustRecipes)` as a script runner. `plan` reads the justfile, so the adapter runs it on a blocking thread.

## API

- `plan`: checks the recipe and its parameters, then renders the command. Files other than `justfile` or `.justfile` (any case) get `--justfile <file>`.
- `parse` and `Recipes::check`: the recipe check on its own, following `alias` lines.
- `handles`: accepts `justfile`, `.justfile` and `*.just`.

## Config example

```yaml
services:
  web:
    # Runs the dev recipe of the justfile with just.
    run_script: justfile#dev
    # Adds Vite's port and host flags, which need a variadic last parameter on the recipe.
    preset: vite
```

With `dev *ARGS:` in the justfile this runs `just dev --port <port> --strictPort --host 127.0.0.1` next to the justfile.

## Behaviour

Extra arguments go right after the recipe name without `--`, because just passes a `--` through as a literal argument. They need a variadic last parameter (`*ARGS` or `+ARGS`); otherwise `plan` fails and suggests one. Recipes with required positional parameters fail too, since `run_script` has no way to fill them.

A recipe is reported missing only when the file has no `import` or `mod`; with either, the check is `Unverifiable` and just decides at run time.

## License

Apache-2.0
