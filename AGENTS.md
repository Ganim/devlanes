# Repository conventions

Rust 2024, pinned stable toolchain, macOS, Linux and Windows 11 24H2 or later. Packages live in `crates/<name>` as `devlanes-<name>`; the composition root is `bins/devlanes`. Add crates when needed. Shared dependencies and lints belong in the root manifest.

`core` owns domain types and traits; integrations implement them; only the binary registers concrete integrations. `testing` provides isolated homes, Git repositories and `fixture-server`, initially without a core dependency. Core unit tests must not depend on testing. Keep integration tests in one binary per crate at `tests/it/main.rs`.

Use typed library errors and propagate expected failures. Add concise rustdoc to functions and methods. Inline comments explain why, when or where. Unsafe code is denied except in the approved OS-boundary crates (`sys`, `runner-pty`, `sys-windows`), where each block needs a safety explanation. A `-windows` crate exists only when it is a backend next to sibling OS crates behind a shared trait (trust, metrics, runner), the Windows FFI boundary (`sys-windows`, the Windows counterpart of rustix and libc; `sys` is the portable process facade over both), or code several crates need that cannot depend on each other (`shell-windows`); name a crate after its mechanism when that is the point (`resolver-nrpt`). Otherwise Windows code goes in a `cfg(windows)` module next to the Unix code for the same concern. Adding support for a feature on one OS must not break or change that feature on the others. Keep blocking filesystem/process work off async executor threads.

Accepted exception: the daemon depends on `git`, `resolver-dns`, `resolver-hosts` and `sync` directly instead of through core traits. They are daemon-internal mechanisms with one implementation each, so a trait would add indirection without a second implementer; reviews should not flag it.

The daemon owns managed resources and SQLite. Clients own rendering and actor detection. Never persist client environments. Reap managed processes by PID, never `waitpid(-1)`. Do not add auto-restart or resource enforcement.

## Worktrees and delivery

- Before changing files for a new pull request, fetch `origin/main` and create a fresh feature branch and Git worktree from the latest `origin/main`.
- Put project worktrees under the repository's ignored `.worktrees/` directory, creating the directory when needed. Implement, test, commit, push, and ship the pull request from its feature
  worktree.
- After a pull request is merged, safely remove its clean worktrees and prune stale worktree metadata with `git worktree prune`. Never remove a dirty or unmerged worktree without confirming its work is preserved.

## Engineering Rules

- Prefer simple, maintainable solutions with minimal abstractions and changes.
- Follow existing project conventions and avoid unnecessary refactoring.
- Keep changes within the requested scope. Report concrete risks without speculating.
- Write brief, high-signal comments where they add value.
- Comments should explain **why**, **when**, or **where**, especially for non-obvious logic, constraints, or tradeoffs. Never comment **what** the code already makes obvious.

## Commits, Branches and PRs

- Name branches using `<type>/<short-kebab-case-description>` with types like `feat`, `fix`, `chore`, `docs`, `refactor`, `test`, and `ci` (e.g., `feat/add-login`).
- Never use Conventional Commits or PR titles; omit type prefixes (`feat:`, `fix:`, etc.) and use concise, imperative descriptions (e.g., `Add login page`, `Fix auth error`).
- Every PR opened by an AI agent MUST explicitly disclose the agent and model used (e.g., `Claude Code / Claude Opus 4.1`, `Codex / GPT-6`) in the PR description. This is mandatory, even for minor changes. Never omit, guess, or misrepresent this information.

## Writing style

- Write naturally, directly, and concisely. Prioritize clarity, specificity, and substance over polished or dramatic prose.
- Avoid common AI writing tropes: "It's not X, it's Y", rhetorical questions, forced metaphors, grandiose claims, false suspense, and manufactured one-liners.
- Avoid excessive em dashes, short punchy fragments, repetitive sentence structures, rule-of-three patterns, and unnecessary adverbs.
- Skip preambles, self-narration, filler transitions, redundant explanations, repeated summaries, and conclusions that restate previous points.
- Prefer simple, precise vocabulary. Avoid corporate jargon, promotional language, invented terminology, vague attributions, and synonym cycling.
- Use natural paragraph lengths and sentence structures. Avoid excessive lists, bold-first bullets, title-case headings, and decorative Unicode.
- Preserve the author's voice. Don't force collective language ("we"), unnecessary enthusiasm, artificial humility, or exaggerated certainty.
- Treat these as patterns to avoid overusing, not absolute bans. Favor context and natural writing over rigid rules.

## Crate READMEs

Every crate (and `bins/devlanes`) has a `README.md`. It is for a person deciding what the crate is for, not a replacement for the code or a changelog. Keep it short and in this order:

1. `# devlanes-<name>` (the package name).
2. One or two sentences: what the crate is and where it sits in devlanes (who calls it, what it calls).
3. `## Usage`: a small Rust example of the main entry points, or the command line for binaries. Examples must match the real API.
4. `## API` (optional): the handful of public types and functions worth knowing, one line each. Not every item.
5. Optional short sections for behaviour a reader would not guess (for example "Requires Docker Compose v2"), each a few lines.
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

- `.cargo/config.toml` sets `build.warnings = "deny"`, so any rustc or clippy warning fails the build; fix warnings instead of allowing them.
- Agents build in the shared `target/` directory and never set `CARGO_TARGET_DIR`.
- Every compile goes through kache (`build.rustc-wrapper` in `.cargo/config.toml`), so it must be installed; see `CONTRIBUTING.md`.
- During parallel work, format and test only owned crates.
- Run one nextest at a time per machine and keep the profile defaults in `.config/nextest.toml` (thread counts and test groups); more parallel test binaries have overwhelmed macOS.
- Every process test needs a timeout and test-owned cleanup.
- Docker, SSH, trust-store and real framework tests use `#[ignore = "needs ..."]`; run them explicitly with their prerequisites.
- Prefix Docker-only tests with `docker_`; the Linux CI jobs run them.
- End-to-end tests live in `e2e/` (`devlanes-e2e`) and drive the real `devlanes` binary as separate processes, each with its own short `DEVLANES_HOME`, Git repository and locked port slot; the default nextest profile skips them, so run them with `cargo nextest run --profile e2e`, and the Docker ones by adding `--run-ignored only -E 'test(/^docker_/)'`.

## Change files

Every change a devlanes user could notice gets a change file in `.sampo/changesets/`, in the same commit as the change. Write it yourself without asking; ask only when you can't tell whether users notice. "Change files" in `CONTRIBUTING.md` is the full guide. The rules that matter most:

- Create it with `sampo add -p cargo/<crate> -b <bump> -m '<line>'` or by hand. List each changed crate as `cargo/<package name>`, plus `cargo/devlanes` whenever someone running devlanes notices.
- Until 1.0: `minor` for anything that breaks what worked (removed or renamed commands, flags or config keys, configs that stop loading, protocol or state changes), `patch` for everything else. Never `major`.
- The first line is the whole entry for most changes: one sentence starting with Added, Changed, Fixed, Deprecated or Removed, naming the command, flag or config key, and saying what changes for the user. No implementation details.
- Add a body only for breaking changes, deprecations, removals and large features: what to do instead, or the smallest example to get started. A few lines at most.
- The writing style above applies. Cut what a user wouldn't miss before committing.
- No change file for refactors, tests, CI or documentation; say so in the pull request description.
- One change file per change. Update it when the change grows instead of adding another.

Never edit versions or `CHANGELOG.md` files by hand; the release pull request does. A new crate goes into `packages.linked` in `.sampo/config.toml`. Code that reports the devlanes version uses `devlanes_version::VERSION`, never its own crate's `CARGO_PKG_VERSION`.

## Public content

Examples and fixtures use neutral names (`acme`, `example.invalid`) and never contain credentials, private paths or references to files outside this repository.
