# devlanes-run-script-deno

Runs `run_script: deno.json#<task>` as `deno task <task>` in the config file's directory. Lane resolution calls it through `DenoTasks`, a `devlanes-core` `ScriptRunner`.

## Usage

```rust
use std::path::Path;
use devlanes_run_script_deno::{TaskRequest, plan};

let args = vec!["--port".to_owned(), "20010".to_owned()];
let launch = plan(&TaskRequest {
    worktree: Path::new("/lanes/acme-1"),
    source: Path::new("apps/api/deno.json"),
    name: "dev",
    extra_args: &args,
})?;
assert_eq!(launch.argv, ["deno", "task", "dev", "--port", "20010"]);
```

The composition root registers `Arc::new(DenoTasks)` as a script runner. `plan` reads the config, so the adapter runs it on a blocking thread.

## API

- `plan`: checks the task and renders the command; `body` is the task's command for presets that inspect it.
- `read_tasks`: the `tasks` of a `deno.json` or `deno.jsonc`, string or object form.
- `handles`: accepts `deno.json` and `deno.jsonc`.

## Config example

```yaml
services:
  api:
    # Runs the dev task of deno.json with deno task.
    run_script: deno.json#dev
```

This runs `deno task dev` next to `deno.json`, with preset flags appended as `deno task dev --port <port>`.

## Behaviour

Arguments follow the task name without `--`: Deno appends them to the task command and keeps a `--` as a literal argument. `deno.jsonc` files may contain comments and trailing commas. When a directory holds both files, Deno reads `deno.json`, so a `deno.jsonc` source gets `--config deno.jsonc`.

## License

Apache-2.0
