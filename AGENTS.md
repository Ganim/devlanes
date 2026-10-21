# Repository conventions

Rust 2024, pinned stable toolchain, macOS and Linux. Packages live in `crates/<name>` as `devlanes-<name>`; the composition root is `bins/devlanes`. Add crates when needed. Shared dependencies and lints belong in the root manifest.

`core` owns domain types and traits; integrations implement them; only the binary registers concrete integrations. `testing` provides isolated homes, Git repositories and `fixture-server`, initially without a core dependency. Core unit tests must not depend on testing. Keep integration tests in one binary per crate at `tests/it/main.rs`.

Use typed library errors and propagate expected failures. Add concise rustdoc to functions and methods. Inline comments explain why, when or where. Unsafe code is denied except in explicitly approved OS-boundary crates, where each block needs a safety explanation. Keep blocking filesystem/process work off async executor threads.

The daemon owns managed resources and SQLite. Clients own rendering and actor detection. Never persist client environments or secret-provider values. Reap managed processes by PID, never `waitpid(-1)`. Do not add auto-restart or resource enforcement.

## Crate READMEs

Every crate (and `bins/devlanes`) has a `README.md`. It is for a person deciding what the crate is for, not a replacement for the code or a changelog. Keep it short and in this order:

1. `# devlanes-<name>` (the package name).
2. One or two sentences: what the crate is and where it sits in devlanes (who calls it, what it calls).
3. `## Usage`: a small Rust example of the main entry points, or the command line for binaries. Examples must match the real API.
4. `## API` (optional): the handful of public types and functions worth knowing, one line each. Not every item.
5. Optional short sections for behaviour a reader would not guess (for example "Requires `fnox` 1.39 or later"), each a few lines.
6. `## License`: `Apache-2.0`.

No badges, tables of contents, installation boilerplate, roadmaps, design essays or history. Sentence-case headings, no headings starting with what/why/where/how, no em dashes, no marketing words. Write plainly, the way you'd explain the crate to a colleague in two minutes. `crates/hosts-helper/README.md` is the one exception that documents a security model at length, because the owner asked for it.

## Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features
cargo test --workspace --doc --all-features
cargo build --workspace --all-targets --all-features
```

During parallel work, format and test only owned crates. Every process test needs a timeout and test-owned cleanup. Docker, SSH, trust-store and real framework tests use `#[ignore = "needs ..."]`; run them explicitly with their prerequisites. Prefix Docker-only external tests with `docker_` for the manual CI job. Consolidated end-to-end tests are deferred until final integration.

## Public content

Examples and fixtures use neutral names (`acme`, `example.invalid`) and never contain credentials, private paths or references to files outside this repository.
