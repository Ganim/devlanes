# devlanes-run-script-npm

Runs `run_script: { file: <dir>/package.json, name: <name> }` with the package manager the project declares. Lane resolution calls it through `NpmScripts`, a `devlanes-core` `ScriptRunner`, which also places preset flags after the script name.

## Usage

```rust
use std::path::Path;
use devlanes_run_script_npm::{ScriptRequest, plan};

let args = vec!["--port".to_owned(), "20012".to_owned()];
let launch = plan(&ScriptRequest {
    worktree: Path::new("/lanes/acme-1"),
    source: Path::new("apps/web/package.json"),
    name: "dev",
    extra_args: &args,
})?;
// With "packageManager": "pnpm@10.4.1" in the root package.json:
assert_eq!(launch.argv, ["pnpm", "run", "dev", "--port", "20012"]);
assert_eq!(launch.version.as_deref(), Some("10.4.1"));
```

The composition root registers `Arc::new(NpmScripts)` as a script runner. `plan` reads files, so the adapter runs it on a blocking thread.

## API

- `plan`: renders `<pm> run <name>` in the manifest's directory, with the declared version, notes and the script body for presets that inspect it.
- `detect_package_manager`: the manager lookup on its own.
- `PackageManager::run_argv`: argv for a known manager; each manager's rules live in its own module.
- `yarn_linker`: the yarn 2+ `nodeLinker` of a directory.

## Config example

```yaml
services:
  api:
    # Runs the dev script of apps/api/package.json with the project's package manager.
    run_script:
      file: apps/api/package.json
      name: dev
```

With `"packageManager": "pnpm@10.4.1"` this runs `pnpm run dev` in `apps/api`; with npm it is `npm run dev`, and preset flags follow a `--`.

## Behaviour

`packageManager` (corepack's field, such as `"pnpm@10.4.1"`) decides. npm's `devEngines.packageManager` is read too, as an object or an array whose first supported entry wins. The lookup starts at the script's `package.json` and stops at the workspace root: the first directory with `pnpm-workspace.yaml`, a `workspaces` field or a lockfile, else the lane worktree.

Without a field, the nearest lockfile decides (`pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `bun.lock`, `bun.lockb`) and the plan carries a note asking for the field. A lockfile that contradicts the field is also a note; the field wins. Yarn 2+ is recognised by version, the `__metadata:` lockfile header or `.yarnrc.yml`, and a Plug'n'Play install without `.pnp.cjs` gets a note.

Only npm needs `--` before script arguments. pnpm, yarn 2+ and bun pass a `--` through literally, which makes Vite and similar CLIs ignore the flags after it; yarn 1 drops it with a deprecation warning. All four get the arguments right after the script name.

## License

Apache-2.0
