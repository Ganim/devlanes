# devlanes-run-script-python

Runs `run_script: { file: pyproject.toml, name: <script> }` as `uv run <script>` or `poetry run <script>` in the project's directory. Lane resolution calls it through `PythonScripts`, a `devlanes-core` `ScriptRunner`.

## Usage

```rust
use std::path::Path;
use devlanes_run_script_python::{PythonRunner, ScriptRequest, plan};

let args = vec!["--port".to_owned(), "20010".to_owned()];
let launch = plan(&ScriptRequest {
    worktree: Path::new("/lanes/acme-1"),
    source: Path::new("services/api/pyproject.toml"),
    name: "serve",
    extra_args: &args,
})?;
// With a uv.lock in services/api or at the worktree root:
assert_eq!(launch.runner, PythonRunner::Uv);
assert_eq!(launch.argv, ["uv", "run", "serve", "--port", "20010"]);
```

The composition root registers `Arc::new(PythonScripts)` as a script runner. `plan` reads the project file and looks for lockfiles, so the adapter runs it on a blocking thread.

## API

- `plan`: checks the script, picks the tool and renders the command, with the entry point and notes.
- `PythonRunner::run_argv`: argv for a known tool.
- `handles`: accepts `pyproject.toml`.

## Config example

```yaml
services:
  api:
    # Runs the serve script of pyproject.toml with uv or poetry.
    run_script:
      file: pyproject.toml
      name: serve
```

With `serve = "acme.server:main"` under `[project.scripts]` and a `uv.lock`, this runs `uv run serve`; with a `poetry.lock` instead it runs `poetry run serve`.

## Behaviour

Scripts come from `[project.scripts]` or `[tool.poetry.scripts]`. uv is used when a `uv.lock` exists between the project and the worktree root, or the project has a `[tool.uv]` table; poetry when `poetry.lock` sits next to `pyproject.toml`. uv wins when both apply, with a note, and neither is an error that suggests `uv lock` or `poetry lock`.

uv installs `[project.scripts]` only for packaged projects, so a project without `[build-system]` or `tool.uv.package = true` gets a note. Arguments follow the script name without `--`, because uv passes a `--` to the script literally.

## License

Apache-2.0
