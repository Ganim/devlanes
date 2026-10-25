# Design

A lane corresponds to a Git worktree. Lane 0 follows the primary branch; other worktrees keep persisted indexes. A worktree other than the primary one gets its lane the first time devlanes runs in it, and a lane whose folder is gone is cleaned once nothing runs in it. Worktree identity uses the Git common directory and worktree administration entry, so moving a worktree preserves its identity. `lane new --name` adds another lane for one folder: a worktree row whose identity is the folder's plus `#lane=<name>`, which follows that folder and never takes lane 0. Non-Git projects have one lane per folder unless extra lanes are added.

The daemon owns processes, ports, proxy routes and SQLite. CLI and TUI clients exchange structured requests and events with it. Closing a client does not stop services. Integration crates implement core traits and are registered by the binary; Docker containers belong to infrastructure adapters rather than process runners.

Each lane receives a non-overlapping block from the machine-wide port pool. Named service offsets persist within that block. A service must accept its assigned port; listening-socket inspection is diagnostic only. Hostnames put the lane in the registrable label, such as `api.acme-2.localhost`.

Startup orders infrastructure, per-lane setup, lifecycle tasks, concurrent services, then after-services tasks. There is no service dependency graph or automatic restart. Monitoring reports resource use without enforcing limits.

YAML configuration merges local maps by key and replaces lists/scalars before template expansion. `${{ ... }}` expressions use strict undefined-variable handling. Client environments stay in daemon memory; `.env` files supply expression values, not implicit process environment injection.

The daemon's HTTPS proxy signs certificates for registered hosts with a per-user CA, devlanes' own or mkcert's (`tls.ca`). SSH transport mirrors registered, non-hidden TCP ports. HTTP-based GUI viewers can be ordinary services; built-in window streaming, relay transport, public DNS resolution and iOS/Android certificate trust are outside v1.

[examples](examples/README.md) has commented configuration files for these contracts.
