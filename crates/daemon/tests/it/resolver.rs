//! The `resolver` hostname strategy and recorded log segments.

use std::time::Duration;

use devlanes_config::{Resolver, UserConfig};
use devlanes_proto::{LaneSummary, Method, Page};
use serde_json::json;

use crate::{LIMIT, Setup, cx, eventually, harness, repo, temp};

const CONFIG: &str = r#"project: demo
services:
  web:
    run: serve
    hosts: ["web.${{ lane.domain }}"]
  worker:
    run: "echo hello; sleep"
"#;

/// A standard A query for `name`.
fn query(id: u16, name: &str) -> Vec<u8> {
    let mut packet = id.to_be_bytes().to_vec();
    packet.extend([0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.') {
        packet.push(u8::try_from(label.len()).expect("label"));
        packet.extend(label.as_bytes());
    }
    packet.extend([0, 0, 1, 0, 1]);
    packet
}

/// The first lane of the project at `cwd`.
async fn lane(h: &crate::Harness, cwd: &std::path::Path) -> LaneSummary {
    let value = h
        .call(Method::LaneList, json!({}), cx(cwd))
        .await
        .expect("lanes");
    let page: Page<LaneSummary> = serde_json::from_value(value).expect("page");
    page.items.into_iter().next().expect("lane")
}

#[tokio::test(flavor = "multi_thread")]
/// With `resolver: resolver` and a TLD, lane domains move under the TLD, the
/// embedded server answers them over UDP, and an explicit domain still wins.
async fn resolver_strategy_serves_lane_hostnames() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let custom = repo(
            &tmp.path,
            "c",
            &CONFIG.replace("project: demo", "project: other").replace(
                "services:",
                "lanes:\n  vars:\n    domain: other.localhost\nservices:",
            ),
        );
        let mut user = UserConfig::default();
        user.hostnames.resolver = Resolver::Resolver;
        user.hostnames.tld = Some("test".into());
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                user: Some(user),
                ..Setup::default()
            },
        )
        .await;

        let summary = lane(&h, &main).await;
        assert_eq!(summary.domain.as_deref(), Some("demo.test"));
        let web = summary.units.iter().find(|u| u.name == "web").expect("web");
        assert!(
            web.urls.iter().any(|u| u == "https://web.demo.test"),
            "{:?}",
            web.urls
        );
        assert_eq!(
            lane(&h, &custom).await.domain.as_deref(),
            Some("other.localhost")
        );
        let hosts = h
            .call(Method::Hosts, json!({}), cx(&main))
            .await
            .expect("hosts");
        let names: Vec<&str> = hosts["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|i| i["host"].as_str())
            .collect();
        assert!(names.contains(&"web.demo.test"), "{names:?}");

        let doctor = h
            .call(Method::Doctor, json!({}), cx(&main))
            .await
            .expect("doctor");
        assert_eq!(doctor["dns"]["running"], true, "{}", doctor["dns"]);
        assert_eq!(doctor["dns"]["tld"], "test");
        let listen: std::net::SocketAddr =
            serde_json::from_value(doctor["dns"]["listen"].clone()).expect("listen");
        assert_ne!(listen.port(), 0);

        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        socket
            .send_to(&query(0x4242, "web.demo.test"), listen)
            .await
            .expect("send");
        let mut buffer = [0u8; 512];
        let (read, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buffer))
            .await
            .expect("answer in time")
            .expect("recv");
        let answer = &buffer[..read];
        assert_eq!(&answer[..2], &0x4242u16.to_be_bytes());
        assert_eq!(answer[3] & 0x0f, 0, "NOERROR");
        assert!(
            u16::from_be_bytes([answer[6], answer[7]]) >= 1,
            "one answer"
        );
        assert_eq!(&answer[read - 4..], &[127, 0, 0, 1]);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A TLD outside the special-use names starts no server, never reaches the
/// lane domain expression, and `doctor` says why.
async fn public_tld_is_refused() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let mut user = UserConfig::default();
        user.hostnames.resolver = Resolver::Resolver;
        user.hostnames.tld = Some("com".into());
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                user: Some(user),
                ..Setup::default()
            },
        )
        .await;

        let domain = lane(&h, &main).await.domain.unwrap_or_default();
        assert!(!domain.ends_with(".com"), "{domain}");
        let doctor = h
            .call(Method::Doctor, json!({}), cx(&main))
            .await
            .expect("doctor");
        assert_eq!(doctor["dns"]["running"], false, "{}", doctor["dns"]);
        assert!(
            doctor["dns"]["error"]
                .as_str()
                .is_some_and(|e| e.contains("home.arpa")),
            "{}",
            doctor["dns"]
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A finished run's log segments are recorded in `log_segments`.
async fn finished_runs_record_log_segments() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let home = tmp.path.join("home");
        let h = harness(&home, Setup::default()).await;
        h.call(Method::Start, json!({"units": ["worker"]}), cx(&main))
            .await
            .expect("start");
        eventually(Duration::from_secs(5), || async {
            let page = h
                .call(Method::Logs, json!({"units": ["worker"]}), cx(&main))
                .await
                .ok()?;
            (!page["items"].as_array()?.is_empty()).then_some(())
        })
        .await
        .expect("logged");
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        let store = devlanes_state::Store::open(home.join("state.db"))
            .await
            .expect("store");
        let lane = store.lanes(None).await.expect("lanes")[0].id;
        let run = store
            .unit_runs(lane, 10)
            .await
            .expect("runs")
            .into_iter()
            .find(|r| r.unit == "worker")
            .expect("run");
        let segments = eventually(Duration::from_secs(10), || async {
            let segments = store.log_segments(run.id).await.ok()?;
            (!segments.is_empty()).then_some(segments)
        })
        .await
        .expect("segments recorded");
        assert_eq!(segments[0].first_seq, 1);
        assert!(segments[0].bytes > 0);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `doctor` checks `config.yaml` on disk at every call: an edit after startup
/// shows up located, without a restart; `daemon status` reports what startup
/// applied.
async fn doctor_checks_the_user_config_on_disk() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let home = tmp.path.join("home");
        let h = harness(&home, Setup::default()).await;
        let section = |doctor: &serde_json::Value| doctor["user_config"].clone();
        let doctor = h
            .call(Method::Doctor, json!({}), cx(&main))
            .await
            .expect("doctor");
        assert_eq!(section(&doctor)["valid"], true);
        std::fs::write(home.join("config.yaml"), "proxy:\n  http_prot: 8080\n").expect("edit");
        let doctor = h
            .call(Method::Doctor, json!({}), cx(&main))
            .await
            .expect("doctor");
        let user = section(&doctor);
        assert_eq!(user["valid"], false, "{user}");
        assert_eq!(user["errors"][0]["line"], 2, "{user}");
        assert!(
            user["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("proxy.http_prot")),
            "{user}"
        );
        let status = h
            .call(Method::DaemonStatus, json!({}), cx(&main))
            .await
            .expect("status");
        assert_eq!(status["user_config"]["errors"], json!([]));
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `doctor` lists the user config's warnings (a deferred trust target here).
async fn doctor_shows_user_config_warnings() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let user = devlanes_config::parse_user("tls:\n  trust: [system, ios-simulator]\n")
            .expect("user config");
        assert!(!user.warnings().is_empty());
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                user: Some(user),
                ..Setup::default()
            },
        )
        .await;
        let doctor = h
            .call(Method::Doctor, json!({}), cx(&main))
            .await
            .expect("doctor");
        let section = &doctor["user_config"];
        assert_eq!(section["valid"], true);
        assert_eq!(section["error"], serde_json::Value::Null);
        assert_eq!(section["warnings"][0]["path"], "tls.trust.1");
        assert!(
            section["warnings"][0]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty())
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}
