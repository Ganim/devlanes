# Design

A lane corresponds to a Git worktree. Lane 0 follows the primary branch; other worktrees keep persisted indexes. Worktree identity uses the Git common directory and worktree administration entry, so moving a worktree preserves its identity. Non-Git projects have one lane.

The daemon owns processes, ports, proxy routes and SQLite. CLI and TUI clients exchange structured requests and events with it. Closing a client does not stop services. Integration crates implement core traits and are registered by the binary; Docker containers belong to infrastructure adapters rather than process runners.

Each lane receives a non-overlapping block from the machine-wide port pool. Named service offsets persist within that block. A service must accept its assigned port; listening-socket inspection is diagnostic only. Hostnames put the lane in the registrable label, such as `api.acme-2.localhost`.

Startup orders infrastructure, per-lane setup, lifecycle tasks, concurrent services, then after-services tasks. There is no service dependency graph or automatic restart. Monitoring reports resource use without enforcing limits.

YAML configuration merges local maps by key and replaces lists/scalars before template expansion. `${{ ... }}` expressions use strict undefined-variable handling. Client environments and secret-provider values stay in daemon memory; `.env` files supply expression values, not implicit process environment injection.

The planned HTTPS proxy uses a per-user CA and certificates for registered hosts. SSH transport mirrors registered, non-hidden TCP ports. HTTP-based GUI viewers can be ordinary services; built-in window streaming, relay transport, public DNS resolution and iOS/Android certificate trust are outside v1.

These are architecture contracts. See the README for implementation status and [example notes](examples/README.md) for source inconsistencies.
