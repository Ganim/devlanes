# Repository conventions

Rust 2024, pinned stable toolchain, macOS and Linux. Packages live in `crates/<name>` as `devlanes-<name>`; the composition root is `bins/devlanes`. Add crates when needed. Shared dependencies and lints belong in the root manifest.

`core` owns domain types and traits; integrations implement them; only the binary registers concrete integrations. `testing` provides isolated homes, Git repositories and `fixture-server`, initially without a core dependency. Core unit tests must not depend on testing. Keep integration tests in one binary per crate at `tests/it/main.rs`.

Use typed library errors and propagate expected failures. Add concise rustdoc to functions and methods. Inline comments explain why, when or where. Unsafe code is denied except in explicitly approved OS-boundary crates, where each block needs a safety explanation. Keep blocking filesystem/process work off async executor threads.

The daemon owns managed resources and SQLite. Clients own rendering and actor detection. Never persist client environments or secret-provider values. Reap managed processes by PID, never `waitpid(-1)`. Do not add auto-restart or resource enforcement.

## Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace --all-features
cargo test --workspace --doc --all-features
cargo build --workspace --all-targets --all-features
```

During parallel work, format and test only owned crates. Every process test needs a timeout and test-owned cleanup. Docker, SSH, trust-store and real framework tests use `#[ignore = "needs ..."]`; run them explicitly with their prerequisites. Prefix Docker-only external tests with `docker_` for the manual CI job. Consolidated end-to-end tests are deferred until final integration.

## Checkout planning sources

Original plan: `/Users/hzk/dev/devlanes/implementation-plan.md`.

Execution plan: `/Users/hzk/.opencode/plan/devlanes-execution.md`.

Include both paths in handoffs and compaction summaries. These files are checkout-local planning inputs, not public documentation dependencies. Do not copy private research, repository inventories, credentials or private paths into public examples.

The source local override references an absent `admin-web`; it needs a matching base service. The source user config includes unsupported mobile trust targets. Preserve explicit diagnostics rather than presenting those combinations as valid v1 configurations.
