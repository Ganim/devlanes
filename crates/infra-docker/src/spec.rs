use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Component, Path, PathBuf},
};

use serde::Serialize;

use crate::Error;

/// Label keys on every container and volume this crate creates.
pub mod labels {
    pub const PROJECT: &str = "dev.devlanes.project";
    pub const CONTAINER: &str = "dev.devlanes.container";
    pub const SCOPE: &str = "dev.devlanes.scope";
    pub const LANE: &str = "dev.devlanes.lane";
    pub const KIND: &str = "dev.devlanes.kind";
    pub const CONFIG_HASH: &str = "dev.devlanes.config-hash";
    pub const VOLUME: &str = "dev.devlanes.volume";
}

/// Whether a container is shared by all lanes of a project or belongs to one lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Shared,
    Lane(u32),
}

impl Scope {
    /// The `dev.devlanes.scope` label value.
    pub fn label(self) -> &'static str {
        match self {
            Scope::Shared => "shared",
            Scope::Lane(_) => "lane",
        }
    }

    /// Reads the scope back from container or volume labels.
    pub fn from_labels(labels: &BTreeMap<String, String>) -> Option<Self> {
        match labels.get(labels::SCOPE).map(String::as_str) {
            Some("shared") => Some(Scope::Shared),
            Some("lane") => labels.get(labels::LANE)?.parse().ok().map(Scope::Lane),
            _ => None,
        }
    }

    /// Name infix: empty for shared, `-lane<N>` for lanes.
    fn infix(self) -> String {
        match self {
            Scope::Shared => String::new(),
            Scope::Lane(lane) => format!("-lane{lane}"),
        }
    }
}

/// Transport protocol of a published port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[default]
    Tcp,
    Udp,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        })
    }
}

/// Consecutive host ports from the machine-wide pool: the lane block for per-lane
/// containers, or the project-level block for shared ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PortBlock {
    pub first: u16,
    pub size: u16,
}

impl PortBlock {
    /// Rejects empty blocks and blocks that run past port 65535.
    pub fn new(first: u16, size: u16) -> Result<Self, Error> {
        if size == 0 || first == 0 || u32::from(first) + u32::from(size) - 1 > u32::from(u16::MAX) {
            return Err(Error::InvalidBlock { first, size });
        }
        Ok(Self { first, size })
    }

    /// The highest port inside the block.
    pub fn last(self) -> u16 {
        self.first.saturating_add(self.size - 1)
    }

    /// Whether `port` belongs to the block.
    pub fn contains(self, port: u16) -> bool {
        (self.first..=self.last()).contains(&port)
    }
}

/// One `ports:` entry: `http: 3592` with the host port the caller allocated for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NamedPort {
    pub name: String,
    pub container: u16,
    pub host: u16,
    pub protocol: Protocol,
}

/// An `infra.containers.<name>` entry after expression evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    /// devlanes project name.
    pub project: String,
    /// The config key (`cerbos`).
    pub name: String,
    pub image: String,
    pub command: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    /// `-v` syntax: `./dir:/target[:mode]`, `/abs:/target`, `named:/target`, `/target`.
    pub volumes: Vec<String>,
    /// In declaration order; the first tcp port is `infra.X.port` and the probe target.
    pub ports: Vec<NamedPort>,
    pub scope: Scope,
    /// Relative volume paths resolve here: the lane's worktree for per-lane containers,
    /// the primary worktree for shared ones.
    pub base_dir: PathBuf,
    pub block: PortBlock,
}

/// A resolved volume entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Mount {
    /// A host directory or file.
    Bind {
        source: PathBuf,
        target: String,
        mode: Option<String>,
    },
    /// A named volume, namespaced per project and lane so lanes keep separate data.
    Named {
        name: String,
        key: String,
        target: String,
        mode: Option<String>,
    },
    /// A container path without a source; Docker creates an anonymous volume.
    Anonymous { target: String },
}

impl Mount {
    /// The `HostConfig.Binds` entry, or `None` for anonymous volumes.
    pub fn bind(&self) -> Option<String> {
        let (source, target, mode) = match self {
            Mount::Bind {
                source,
                target,
                mode,
            } => (source.to_string_lossy().into_owned(), target, mode),
            Mount::Named {
                name, target, mode, ..
            } => (name.clone(), target, mode),
            Mount::Anonymous { .. } => return None,
        };
        Some(match mode {
            Some(mode) => format!("{source}:{target}:{mode}"),
            None => format!("{source}:{target}"),
        })
    }
}

/// `devlanes-<project>-<name>` for shared containers, `devlanes-<project>-lane<N>-<name>`
/// for per-lane ones.
pub fn container_name(project: &str, name: &str, scope: Scope) -> String {
    format!("devlanes-{project}{}-{name}", scope.infix())
}

/// Docker volume name for a named volume key, namespaced like containers.
pub fn volume_name(project: &str, key: &str, scope: Scope) -> String {
    format!("devlanes-{project}{}-{key}", scope.infix())
}

/// Labels that identify devlanes resources for re-adoption, events and clean.
pub fn scope_labels(project: &str, scope: Scope) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([
        (labels::PROJECT.to_owned(), project.to_owned()),
        (labels::SCOPE.to_owned(), scope.label().to_owned()),
        (labels::KIND.to_owned(), "container".to_owned()),
    ]);
    if let Scope::Lane(lane) = scope {
        labels.insert(labels::LANE.to_owned(), lane.to_string());
    }
    labels
}

/// Labels of the container for `spec`; `hash` detects config drift on re-adoption.
pub fn container_labels(spec: &ContainerSpec, hash: &str) -> BTreeMap<String, String> {
    let mut labels = scope_labels(&spec.project, spec.scope);
    labels.insert(labels::CONTAINER.to_owned(), spec.name.clone());
    labels.insert(labels::CONFIG_HASH.to_owned(), hash.to_owned());
    labels
}

/// Resolves one volume entry. Sources starting with `/` are absolute, sources starting with
/// `.` or containing `/` are relative to `base_dir`, other sources are named volumes.
/// `~` is rejected because the daemon's home is not necessarily the user's.
pub fn resolve_volume(
    spec: &str,
    container: &str,
    project: &str,
    scope: Scope,
    base_dir: &Path,
) -> Result<Mount, Error> {
    let invalid = |reason| Error::InvalidVolume {
        container: container.to_owned(),
        spec: spec.to_owned(),
        reason,
    };
    let parts: Vec<&str> = spec.split(':').collect();
    let (source, target, mode) = match parts.as_slice() {
        [target] => (None, *target, None),
        [source, target] => (Some(*source), *target, None),
        [source, target, mode] => (Some(*source), *target, Some(*mode)),
        _ => return Err(invalid("expected source:target[:mode]")),
    };
    if !target.starts_with('/') {
        return Err(invalid("the container path must be absolute"));
    }
    if mode.is_some_and(str::is_empty) {
        return Err(invalid("empty mode"));
    }
    let target = target.to_owned();
    let mode = mode.map(str::to_owned);
    let Some(source) = source else {
        return Ok(Mount::Anonymous { target });
    };
    if source.is_empty() {
        return Err(invalid("empty source"));
    }
    if source.starts_with('~') {
        return Err(invalid(
            "`~` is not expanded; use an absolute or relative path",
        ));
    }
    if source.starts_with('/') {
        return Ok(Mount::Bind {
            source: normalize(Path::new(source)),
            target,
            mode,
        });
    }
    if source.starts_with('.') || source.contains('/') {
        return Ok(Mount::Bind {
            source: normalize(&base_dir.join(source)),
            target,
            mode,
        });
    }
    validate_name(source)?;
    Ok(Mount::Named {
        name: volume_name(project, source, scope),
        key: source.to_owned(),
        target,
        mode,
    })
}

/// Removes `.` and resolves `..` without touching the filesystem, since bind sources may
/// not exist yet (Docker creates missing directories).
fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Docker object names: `[a-zA-Z0-9][a-zA-Z0-9_.-]*`.
pub fn validate_name(name: &str) -> Result<(), Error> {
    let mut chars = name.chars();
    let valid_start = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if valid_start && valid_rest {
        Ok(())
    } else {
        Err(Error::InvalidName(name.to_owned()))
    }
}

/// Checks names, port names and host ports, then resolves volumes.
pub fn validate(spec: &ContainerSpec) -> Result<Vec<Mount>, Error> {
    validate_name(&spec.project)?;
    validate_name(&spec.name)?;
    let mut names = BTreeSet::new();
    let mut hosts = BTreeSet::new();
    for port in &spec.ports {
        if !names.insert(&port.name) {
            return Err(Error::DuplicatePortName {
                container: spec.name.clone(),
                name: port.name.clone(),
            });
        }
        if !spec.block.contains(port.host) {
            return Err(Error::PortOutsideBlock {
                container: spec.name.clone(),
                name: port.name.clone(),
                port: port.host,
                first: spec.block.first,
                last: spec.block.last(),
            });
        }
        if !hosts.insert((port.host, port.protocol)) {
            return Err(Error::DuplicatePort {
                container: spec.name.clone(),
                port: port.host,
            });
        }
    }
    spec.volumes
        .iter()
        .map(|volume| {
            resolve_volume(
                volume,
                &spec.name,
                &spec.project,
                spec.scope,
                &spec.base_dir,
            )
        })
        .collect()
}

/// Fingerprint of everything that requires recreating the container when it changes.
/// FNV-1a over canonical JSON, because std's hasher is not stable across releases.
pub fn config_hash(spec: &ContainerSpec, mounts: &[Mount]) -> String {
    #[derive(Serialize)]
    struct Canonical<'a> {
        image: &'a str,
        command: &'a Option<Vec<String>>,
        env: &'a BTreeMap<String, String>,
        mounts: &'a [Mount],
        ports: &'a [NamedPort],
    }
    let canonical = Canonical {
        image: &spec.image,
        command: &spec.command,
        env: &spec.env,
        mounts,
        ports: &spec.ports,
    };
    // Serializing plain strings, maps and integers cannot fail.
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Host port of the first tcp port, used for `infra.X.port` and the tcp probe.
pub fn primary_port(ports: &[NamedPort]) -> Option<u16> {
    ports
        .iter()
        .find(|port| port.protocol == Protocol::Tcp)
        .map(|port| port.host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ContainerSpec {
        ContainerSpec {
            project: "acme".into(),
            name: "cerbos".into(),
            image: "ghcr.io/cerbos/cerbos:0.40.0".into(),
            command: Some(vec!["server".into()]),
            env: BTreeMap::new(),
            volumes: vec!["./infra/cerbos:/policies:ro".into()],
            ports: vec![
                NamedPort {
                    name: "http".into(),
                    container: 3592,
                    host: 21000,
                    protocol: Protocol::Tcp,
                },
                NamedPort {
                    name: "grpc".into(),
                    container: 3593,
                    host: 21001,
                    protocol: Protocol::Tcp,
                },
            ],
            scope: Scope::Lane(2),
            base_dir: "/work/acme-lanes/feature".into(),
            block: PortBlock::new(21000, 10).expect("valid block"),
        }
    }

    fn resolve(volume: &str, scope: Scope) -> Result<Mount, Error> {
        resolve_volume(volume, "cerbos", "acme", scope, Path::new("/work/acme"))
    }

    #[test]
    fn names_by_scope() {
        assert_eq!(
            container_name("acme", "cerbos", Scope::Shared),
            "devlanes-acme-cerbos"
        );
        assert_eq!(
            container_name("acme", "cerbos", Scope::Lane(3)),
            "devlanes-acme-lane3-cerbos"
        );
        assert_eq!(
            volume_name("acme", "data", Scope::Lane(1)),
            "devlanes-acme-lane1-data"
        );
    }

    #[test]
    fn label_sets() {
        let labels = container_labels(&spec(), "abc");
        let expected = BTreeMap::from([
            ("dev.devlanes.config-hash".to_owned(), "abc".to_owned()),
            ("dev.devlanes.container".to_owned(), "cerbos".to_owned()),
            ("dev.devlanes.kind".to_owned(), "container".to_owned()),
            ("dev.devlanes.lane".to_owned(), "2".to_owned()),
            ("dev.devlanes.project".to_owned(), "acme".to_owned()),
            ("dev.devlanes.scope".to_owned(), "lane".to_owned()),
        ]);
        assert_eq!(labels, expected);
        assert_eq!(Scope::from_labels(&labels), Some(Scope::Lane(2)));
        let shared = scope_labels("acme", Scope::Shared);
        assert!(!shared.contains_key(labels::LANE));
        assert_eq!(Scope::from_labels(&shared), Some(Scope::Shared));
        assert_eq!(Scope::from_labels(&BTreeMap::new()), None);
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "Unix path fixtures render with `\\` on Windows, ported in the Windows path step"
    )]
    fn relative_paths_resolve_against_base_dir() {
        assert_eq!(
            resolve("./infra/cerbos:/policies:ro", Scope::Shared).ok(),
            Some(Mount::Bind {
                source: "/work/acme/infra/cerbos".into(),
                target: "/policies".into(),
                mode: Some("ro".into()),
            })
        );
        assert_eq!(
            resolve("../shared/./data:/data", Scope::Shared).ok(),
            Some(Mount::Bind {
                source: "/work/shared/data".into(),
                target: "/data".into(),
                mode: None,
            })
        );
        assert_eq!(
            resolve("infra/x:/x", Scope::Shared).ok(),
            Some(Mount::Bind {
                source: "/work/acme/infra/x".into(),
                target: "/x".into(),
                mode: None,
            })
        );
        assert_eq!(
            resolve("/etc/hosts:/etc/hosts:ro", Scope::Lane(1))
                .ok()
                .and_then(|mount| mount.bind()),
            Some("/etc/hosts:/etc/hosts:ro".to_owned())
        );
    }

    #[test]
    fn named_and_anonymous_volumes() {
        let mount = resolve("data:/var/lib/data", Scope::Lane(4)).expect("valid");
        assert_eq!(
            mount.bind().as_deref(),
            Some("devlanes-acme-lane4-data:/var/lib/data")
        );
        assert_eq!(
            resolve("/cache", Scope::Shared).ok(),
            Some(Mount::Anonymous {
                target: "/cache".into()
            })
        );
        assert_eq!(
            resolve("/cache", Scope::Shared).ok().and_then(|m| m.bind()),
            None
        );
    }

    #[test]
    fn rejects_bad_volumes() {
        for volume in [
            "~/x:/x",
            "./x:relative",
            "a:b:c:d",
            ":/x",
            "./x:/x:",
            "bad name:/x",
        ] {
            assert!(resolve(volume, Scope::Shared).is_err(), "{volume}");
        }
    }

    #[test]
    fn validates_ports_against_block() {
        assert_eq!(validate(&spec()).map(|mounts| mounts.len()).ok(), Some(1));
        let mut outside = spec();
        outside.ports[1].host = 21010;
        assert!(matches!(
            validate(&outside),
            Err(Error::PortOutsideBlock { port: 21010, .. })
        ));
        let mut twice = spec();
        twice.ports[1].host = 21000;
        assert!(matches!(
            validate(&twice),
            Err(Error::DuplicatePort { port: 21000, .. })
        ));
        let mut udp = spec();
        udp.ports[1].host = 21000;
        udp.ports[1].protocol = Protocol::Udp;
        assert!(validate(&udp).is_ok(), "tcp and udp may share a host port");
        let mut named = spec();
        named.ports[1].name = "http".into();
        assert!(matches!(
            validate(&named),
            Err(Error::DuplicatePortName { .. })
        ));
        let mut bad = spec();
        bad.name = "-x".into();
        assert!(matches!(validate(&bad), Err(Error::InvalidName(_))));
    }

    #[test]
    fn hash_tracks_config_changes() {
        let base = spec();
        let mounts = validate(&base).expect("valid");
        let hash = config_hash(&base, &mounts);
        assert_eq!(hash.len(), 16);
        assert_eq!(
            hash,
            config_hash(&base.clone(), &mounts),
            "stable for equal specs"
        );
        let mut changed = base.clone();
        changed.image = "ghcr.io/cerbos/cerbos:0.41.0".into();
        assert_ne!(hash, config_hash(&changed, &mounts));
        let mut env = base;
        env.env.insert("A".into(), "1".into());
        assert_ne!(hash, config_hash(&env, &mounts));
    }

    #[test]
    fn primary_port_is_first_tcp() {
        let mut ports = spec().ports;
        assert_eq!(primary_port(&ports), Some(21000));
        ports[0].protocol = Protocol::Udp;
        assert_eq!(primary_port(&ports), Some(21001));
    }
}
