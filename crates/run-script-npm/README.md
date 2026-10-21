# devlanes-run-script-npm

Runs `run_script: <dir>/package.json#<name>` with the package manager the repository uses. Lane resolution calls it through `NpmScripts`, a `devlanes-core` `ScriptRunner`, and the preset crates use `forward_args` to add their flags to a launch.

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
// With a package-lock.json: npm run dev -- --port 20012, run in apps/web.
assert_eq!(launch.argv, ["npm", "run", "dev", "--", "--port", "20012"]);
```

The composition root registers `Arc::new(NpmScripts)` as a script runner. `plan` reads files, so the adapter runs it on a blocking thread.

## API

- `plan`: renders `<pm> run <name>` in the manifest's directory and returns the script body for presets that inspect it.
- `detect_package_manager`: the lockfile lookup on its own.
- `read_script`: one script from a `package.json`; a missing script lists the available ones.
- `PackageManager::run_argv`: argv for a known manager.
- `forward_args`: appends preset arguments to a `LaunchDraft`, adding `--` for a plain `run: npm run dev`.

## Lockfile detection

The lookup starts in the manifest's directory and walks up to the lane worktree, never above it. The nearest directory with any of `pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, `bun.lock` or `bun.lockb` decides. Lockfiles of different managers in one directory need a `packageManager` field in that directory's `package.json`, or detection fails.

Yarn 2+ is recognised by the `__metadata:` lockfile header, then by the `packageManager` major version, then by `.yarnrc.yml`; otherwise yarn 1 is assumed. Without any lockfile the nearest `packageManager` field decides, and npm is the fallback.

## Argument forwarding

Only npm needs `--` before arguments for the script. pnpm, yarn 2+ and bun forward a `--` literally, which makes Vite and similar CLIs ignore the flags after it, so they get the arguments right after the script name. Yarn 1 drops a `--` with a deprecation warning and gets the same treatment.

## License

Apache-2.0
