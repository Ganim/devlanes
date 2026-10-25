# devlanes-resolve

Turns a project config and one lane's facts into ports, hostnames, routes and launch plans for every service, task and infra command. The daemon calls it before starting a lane; it calls contributors and script runners through the `devlanes-core` traits and never spawns managed processes itself.

## Usage

```rust
use std::collections::BTreeMap;
use devlanes_config::ProjectConfig;
use devlanes_core::ports::{PortBlock, allocate_offsets, block_size};
use devlanes_resolve::{LaneFacts, ResolveInput, port_needs, resolve_lane, select};

async fn start(
    config: &ProjectConfig,
    client_env: &BTreeMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let facts = LaneFacts::new(
        "acme",
        "/work/acme",
        1,
        "/work/acme-lanes/login",
        "/home/me/.devlanes/data/acme/7",
        Some("feat/login".into()),
    );
    let needs = port_needs(config, &[])?;
    let block = PortBlock::new(21_000, block_size(needs.min_block_size(), None)?)?;
    let offsets = allocate_offsets(&BTreeMap::new(), &needs.names, &needs.reserved, &[block])?;
    let selection = select(config, None, &[])?;

    let mut input = ResolveInput::new(config, &facts, block, &offsets);
    input.client_env = client_env;
    let lane = resolve_lane(input).await?;
    lane.check_services(selection.services.iter().map(String::as_str))?;
    for service in selection.services.iter().flat_map(|name| lane.units_of(name)) {
        println!("{} -> {} {:?}", service.unit, service.url, service.launch.args);
    }
    Ok(())
}
```

In the daemon the block and offsets come from `devlanes-state`, and `ResolveInput` also carries contributors, script runners, infra endpoints, `lanes.main` and `--env` overrides.

## API

- `port_needs`: port names to allocate, offsets reserved by `lane.port + N`, services with their own `PORT`.
- `select_profiles`: services for one or more profiles and `start` selectors (names, globs, `+tag`); `select` is the single-profile form.
- `profile_summaries`: profile names, include/exclude selectors and literal `infra: false`, for clients.
- `tag_colors`: tag colors from the config's `tags:` map, for `devlanes_core::color::unit_color`.
- `lane_layout`: ports, URLs, routes and `lane_env()` without contributors, for idle lanes and lane 0's `services_namespace()`.
- `resolve_lane`: the full `ResolvedLane` with a `Launch` per unit.
- `fixed_block_start`: block start from `lanes.vars.port` or a matrix row.
- `detect_platform`: the platform for a unit without `platform:`, from contributor claims (highest confidence, then higher priority, then smaller id). resolve has no built-in file rules.

## Behaviour

Environment layers, lowest first: client env, contributors, `DEVLANES_*`/`PORT`/`HOST` and CA variables, `global_env`, the unit's `env`, the profile's `env`, `--env`. Each value keeps its layer for `env --explain`. `.env` files only feed `${{ env.X }}`, which reads the client env, then `.env`.

A unit that fails to plan is recorded in `ResolvedLane::errors` and does not fail the lane. Call `check_services` for the selection before spawning. `Launch`'s `Debug` output lists env keys and layers only.

## License

Apache-2.0
