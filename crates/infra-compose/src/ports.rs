use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::{
    Error,
    model::{ComposeModel, ContainerStatus},
};

/// A lane's consecutive host ports, as allocated from the machine-wide pool.
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

/// A container port of a per-lane service that needs a host port from the lane block.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct PortRequest {
    pub service: String,
    pub target: u16,
    pub protocol: String,
}

impl PortRequest {
    /// Stable allocation name (`mailpit/8025`) for the caller's persisted offsets.
    pub fn key(&self) -> String {
        if self.protocol == "tcp" {
            format!("{}/{}", self.service, self.target)
        } else {
            format!("{}/{}/{}", self.service, self.target, self.protocol)
        }
    }
}

/// Host ports chosen for per-lane services: service -> container port -> host port.
pub type LanePorts = BTreeMap<String, BTreeMap<u16, u16>>;

/// Host ports of one service. `port` is the first published port in file order
/// (`infra.X.port`); `ports` is keyed by container port (`infra.X.ports['5432']`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Endpoint {
    pub port: Option<u16>,
    pub ports: BTreeMap<u16, u16>,
}

/// Endpoints per compose service name.
pub type Endpoints = BTreeMap<String, Endpoint>;

/// Container ports of `services` that need lane host ports, in file order. A target listed
/// twice (for example tcp and udp) yields one request per protocol.
pub fn port_requests(model: &ComposeModel, services: &[String]) -> Result<Vec<PortRequest>, Error> {
    model.require_services(services)?;
    let mut requests = Vec::new();
    let mut seen = BTreeSet::new();
    for name in services {
        let Some(service) = model.services.get(name) else {
            continue;
        };
        for port in &service.ports {
            if seen.insert((name.clone(), port.target, port.protocol.clone())) {
                requests.push(PortRequest {
                    service: name.clone(),
                    target: port.target,
                    protocol: port.protocol.clone(),
                });
            }
        }
    }
    Ok(requests)
}

/// Checks that every requested container port has a host port inside `block` and that no
/// host port is used twice, then returns the endpoints those ports produce.
pub fn lane_endpoints(
    requests: &[PortRequest],
    ports: &LanePorts,
    block: PortBlock,
) -> Result<Endpoints, Error> {
    let mut endpoints = Endpoints::new();
    let mut used = BTreeSet::new();
    for request in requests {
        let host = ports
            .get(&request.service)
            .and_then(|targets| targets.get(&request.target))
            .copied()
            .ok_or_else(|| Error::MissingPortAssignment {
                service: request.service.clone(),
                target: request.target,
            })?;
        if !block.contains(host) {
            return Err(Error::PortOutsideBlock {
                service: request.service.clone(),
                target: request.target,
                port: host,
                first: block.first,
                last: block.last(),
            });
        }
        let endpoint = endpoints.entry(request.service.clone()).or_default();
        // tcp and udp on the same target share one host port; only distinct targets count.
        if !endpoint.ports.contains_key(&request.target) && !used.insert(host) {
            return Err(Error::DuplicatePort { port: host });
        }
        endpoint.port.get_or_insert(host);
        endpoint.ports.entry(request.target).or_insert(host);
    }
    Ok(endpoints)
}

/// Endpoints of running containers from `ps` publishers. The first port follows the
/// model's file order because Docker reports publishers sorted by its own rules.
/// Unpublished (`PublishedPort` 0) entries and duplicate IPv4/IPv6 bindings are skipped.
pub fn endpoints_from_ps(model: &ComposeModel, statuses: &[ContainerStatus]) -> Endpoints {
    let mut endpoints = Endpoints::new();
    for status in statuses {
        let order: Vec<u16> = model
            .services
            .get(&status.service)
            .map(|service| service.ports.iter().map(|port| port.target).collect())
            .unwrap_or_default();
        let mut publishers: Vec<_> = status
            .publishers
            .iter()
            .filter(|publisher| publisher.published_port != 0)
            .collect();
        publishers.sort_by_key(|publisher| {
            let position = order
                .iter()
                .position(|target| *target == publisher.target_port)
                .unwrap_or(usize::MAX);
            (position, publisher.target_port, publisher.protocol != "tcp")
        });
        let endpoint = endpoints.entry(status.service.clone()).or_default();
        for publisher in publishers {
            endpoint.port.get_or_insert(publisher.published_port);
            endpoint
                .ports
                .entry(publisher.target_port)
                .or_insert(publisher.published_port);
        }
    }
    endpoints
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::parse_ps;

    fn model(json: &str) -> ComposeModel {
        ComposeModel::from_json(json).expect("fixture parses")
    }

    fn request(service: &str, target: u16) -> PortRequest {
        PortRequest {
            service: service.into(),
            target,
            protocol: "tcp".into(),
        }
    }

    #[test]
    fn block_bounds() {
        let block = PortBlock::new(21000, 10).expect("valid block");
        assert_eq!(block.last(), 21009);
        assert!(block.contains(21000) && block.contains(21009));
        assert!(!block.contains(20999) && !block.contains(21010));
        assert!(PortBlock::new(65530, 7).is_err());
        assert!(PortBlock::new(65530, 6).is_ok());
        assert!(PortBlock::new(1000, 0).is_err());
    }

    #[test]
    fn requests_follow_file_order() {
        let model = model(include_str!("../tests/fixtures/config.json"));
        let requests = port_requests(&model, &["mailpit".to_owned()]).expect("known service");
        assert_eq!(
            requests,
            [request("mailpit", 8025), request("mailpit", 1025)]
        );
        assert_eq!(requests[0].key(), "mailpit/8025");
        assert!(port_requests(&model, &["nope".to_owned()]).is_err());
    }

    #[test]
    fn lane_endpoints_validate_assignments() {
        let block = PortBlock::new(21000, 10).expect("valid block");
        let requests = [request("mailpit", 8025), request("mailpit", 1025)];
        let mut ports = LanePorts::new();
        ports.insert(
            "mailpit".into(),
            BTreeMap::from([(8025, 21000), (1025, 21001)]),
        );
        let endpoints = lane_endpoints(&requests, &ports, block).expect("valid");
        assert_eq!(endpoints["mailpit"].port, Some(21000));
        assert_eq!(endpoints["mailpit"].ports[&1025], 21001);

        ports.insert("mailpit".into(), BTreeMap::from([(8025, 21000)]));
        assert!(matches!(
            lane_endpoints(&requests, &ports, block),
            Err(Error::MissingPortAssignment { target: 1025, .. })
        ));
        ports.insert(
            "mailpit".into(),
            BTreeMap::from([(8025, 21000), (1025, 22000)]),
        );
        assert!(matches!(
            lane_endpoints(&requests, &ports, block),
            Err(Error::PortOutsideBlock { port: 22000, .. })
        ));
        ports.insert(
            "mailpit".into(),
            BTreeMap::from([(8025, 21000), (1025, 21000)]),
        );
        assert!(matches!(
            lane_endpoints(&requests, &ports, block),
            Err(Error::DuplicatePort { port: 21000 })
        ));
    }

    #[test]
    fn tcp_and_udp_on_one_target_share_a_host_port() {
        let block = PortBlock::new(21000, 10).expect("valid block");
        let mut udp = request("dns", 53);
        udp.protocol = "udp".into();
        let requests = [request("dns", 53), udp];
        let ports = LanePorts::from([("dns".into(), BTreeMap::from([(53, 21005)]))]);
        let endpoints = lane_endpoints(&requests, &ports, block).expect("valid");
        assert_eq!(endpoints["dns"].port, Some(21005));
    }

    #[test]
    fn endpoints_from_running_containers() {
        let model = model(include_str!("../tests/fixtures/config.json"));
        let statuses = parse_ps(include_str!("../tests/fixtures/ps.ndjson")).expect("fixture");
        let endpoints = endpoints_from_ps(&model, &statuses);
        assert_eq!(endpoints["postgres"].port, Some(15432));
        assert_eq!(endpoints["postgres"].ports, BTreeMap::from([(5432, 15432)]));
        assert_eq!(endpoints["redis"].port, Some(32768));
    }

    #[test]
    fn first_port_follows_model_order() {
        let model = model(include_str!("../tests/fixtures/config.json"));
        // Docker sorts publishers by target port; mailpit declares 8025 before 1025.
        let line = r#"{"ID":"x","Name":"m","Service":"mailpit","State":"running","Publishers":[
            {"URL":"0.0.0.0","TargetPort":1025,"PublishedPort":21001,"Protocol":"tcp"},
            {"URL":"0.0.0.0","TargetPort":8025,"PublishedPort":21000,"Protocol":"tcp"},
            {"URL":"","TargetPort":9999,"PublishedPort":0,"Protocol":"tcp"}]}"#
            .replace('\n', "");
        let statuses = parse_ps(&line).expect("valid line");
        let endpoints = endpoints_from_ps(&model, &statuses);
        assert_eq!(endpoints["mailpit"].port, Some(21000));
        assert_eq!(endpoints["mailpit"].ports.len(), 2);
    }

    #[test]
    fn lane_model_publishes_assigned_ports() {
        let model = model(include_str!("../tests/fixtures/lane-config.json"));
        let mailpit = &model.services["mailpit"];
        assert_eq!(
            mailpit.container_name, None,
            "!reset removed container_name"
        );
        let published: Vec<_> = mailpit
            .ports
            .iter()
            .map(|port| (port.target, port.published.clone(), port.host_ip.clone()))
            .collect();
        assert_eq!(
            published,
            [
                (8025, Some("21000".into()), Some("127.0.0.1".into())),
                (1025, Some("21001".into()), Some("127.0.0.1".into()))
            ]
        );
        assert_eq!(mailpit.labels["dev.devlanes.lane"], "2");
    }
}
