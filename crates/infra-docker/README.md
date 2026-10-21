# devlanes-infra-docker

Runs the plain containers from `infra.containers` through the Docker Engine API (bollard), shared per project or one copy per lane. The daemon adapts it to the core `Infra` trait and uses its event stream and label queries for health and re-adoption; it does not depend on `devlanes-core`.

## Usage

```rust
use std::collections::BTreeMap;

use devlanes_infra_docker::{
    ContainerSpec, DockerInfra, EnsureOptions, Error, NamedPort, PortBlock, Protocol, Scope,
};
use futures_util::StreamExt;

async fn start_cerbos() -> Result<(), Error> {
    let docker = DockerInfra::connect().await?;
    let spec = ContainerSpec {
        project: "acme".into(),
        name: "cerbos".into(),
        image: "ghcr.io/cerbos/cerbos:0.40.0".into(),
        command: Some(vec!["server".into(), "--config=/policies/config.yaml".into()]),
        env: BTreeMap::new(),
        volumes: vec!["./infra/cerbos:/policies:ro".into()],
        ports: vec![NamedPort {
            name: "http".into(),
            container: 3592,
            host: 21004,
            protocol: Protocol::Tcp,
        }],
        scope: Scope::Lane(2),
        base_dir: "/work/acme-lanes/feature".into(),
        block: PortBlock::new(21000, 10)?,
    };
    let ensured = docker.ensure(&spec, EnsureOptions::default()).await?;
    assert_eq!(ensured.endpoint.ports["http"], 21004);

    // After a daemon restart: find containers again without touching them.
    let managed = docker.list_managed("acme").await?;
    println!("{} containers", managed.len());

    let mut events = docker.project_events("acme");
    while let Some(event) = events.next().await {
        let event = event?;
        println!("{} {:?}", event.container_name, event.kind);
    }

    // Lane clean: per-lane containers and their named volumes.
    docker.remove_lane("acme", 2).await?;
    Ok(())
}
```

## API

- `DockerInfra`: `ensure`, `health`, `wait_healthy`, `list_managed`, `stop`, `remove_lane`, `events`, `project_events`.
- `ContainerSpec`: an evaluated `infra.containers` entry with host ports already taken from the block.
- `Ensured`: the action taken (`Created`, `Started`, `Adopted`, `Recreated`), the endpoint and the health.
- `HealthEvent` / `map_event`: Docker container events mapped to lifecycle changes.
- `resolve_volume`, `container_name`, `container_labels`: the naming and labelling rules as pure functions.

## Config example

```yaml
infra:
  # Plain Docker containers, run without a Compose file.
  containers:
    cerbos:
      image: ghcr.io/cerbos/cerbos:0.40.0
      command: [server, --config=/policies/config.yaml]
      volumes: ["./infra/cerbos:/policies:ro"]
      # Container ports by name; the host side is read as infra.cerbos.ports.http.
      ports: { http: 3592, grpc: 3593 }
      # One container per lane instead of one shared by all lanes.
      per_lane: true
```

Each lane gets a `devlanes-acme-lane<N>-cerbos` container with both ports published on 127.0.0.1 from the lane's port block, and `./infra/cerbos` resolved against that lane's worktree.

## Behaviour

Containers are named `devlanes-<project>[-lane<N>]-<name>` and labelled `dev.devlanes.*`. Host ports bind to 127.0.0.1 and must lie inside the given block.

`ensure` reuses a running container with the same config and never stops it. A changed config replaces the container. A same-named container without this project's labels is an error.

Relative volume paths resolve against `base_dir`: the lane's worktree for per-lane containers, the primary worktree for shared ones. Named volumes are created with labels and namespaced per project and lane, so `remove_lane` deletes only that lane's data.

Health comes from the image healthcheck when there is one, otherwise from a tcp probe of the first tcp port. The probe ignores connections that a Docker port proxy accepts and then closes.

`DockerInfra::connect` resolves the engine like the `docker` CLI: `DOCKER_HOST`, then `DOCKER_CONTEXT` or `currentContext` from the Docker config directory. bollard alone only reads `DOCKER_HOST`, which misses OrbStack and Colima contexts.

## License

Apache-2.0
