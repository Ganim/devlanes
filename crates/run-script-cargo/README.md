# devlanes-run-script-cargo

Runs `run_script: Cargo.toml#<bin>` as `cargo run --bin <bin> -- <args>` in the manifest's directory. Lane resolution calls it through `CargoBins`, a `devlanes-core` `ScriptRunner`.

## Usage

```rust
use std::path::Path;
use devlanes_run_script_cargo::{BinRequest, plan};

let args = vec!["--port".to_owned(), "20010".to_owned()];
let launch = plan(&BinRequest {
    worktree: Path::new("/lanes/acme-1"),
    source: Path::new("crates/api/Cargo.toml"),
    name: "server",
    extra_args: &args,
})?;
assert_eq!(launch.argv, ["cargo", "run", "--bin", "server", "--", "--port", "20010"]);
```

The composition root registers `Arc::new(CargoBins)` as a script runner. `plan` reads manifests and lists `src/bin`, so the adapter runs it on a blocking thread.

## API

- `plan`: checks that the binary exists and renders the command, with the owning package.
- `package_bins`: a package's binaries from `[[bin]]`, `src/main.rs` and `src/bin/`.
- `handles`: accepts `Cargo.toml`.

## Config example

```yaml
services:
  api:
    # Runs the server binary with cargo run.
    run_script: Cargo.toml#server
```

At a virtual workspace root this runs `cargo run -p <member> --bin server` for the member that builds `server`; at a member's own `Cargo.toml` it runs `cargo run --bin server` in that directory.

## Behaviour

Binaries follow Cargo's target auto-discovery, including `autobins = false`. Workspace members are found from `members` patterns (`*` and `?` within a segment), minus `exclude`. A binary built by more than one member is an error that asks for the member's manifest. The manifest's directory is the working directory, so no `--manifest-path` is passed.

## License

Apache-2.0
