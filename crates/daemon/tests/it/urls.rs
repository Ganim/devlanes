//! Public URLs carry the port the proxy's HTTPS listener bound.

use std::sync::Arc;

use devlanes_daemon::integrations::{RouteEntry, RouteSink};
use devlanes_proto::{LaneSummary, Method, Page};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, harness, repo, temp};

const CONFIG: &str = r#"project: demo
services:
  web:
    run: serve
    hosts: ["web.${{ lane.domain }}", "*.pages.${{ lane.domain }}"]
"#;

/// A proxy whose HTTPS listener fell back to `port`.
struct BoundAt(u16);

impl RouteSink for BoundAt {
    fn publish(&self, _: Vec<RouteEntry>) {}

    fn status(&self) -> Value {
        json!({"https": {"bound": true, "port": self.0, "fallback_reason": "443 in use"}})
    }
}

/// Public URLs of `web` from a lane listing.
async fn web_urls(h: &crate::Harness, cwd: &std::path::Path) -> Vec<String> {
    let value = h
        .call(Method::LaneList, json!({}), cx(cwd))
        .await
        .expect("lanes");
    let page: Page<LaneSummary> = serde_json::from_value(value).expect("page");
    page.items[0]
        .units
        .iter()
        .find(|u| u.name == "web")
        .expect("web")
        .urls
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
/// With HTTPS on 8443, idle and running units, `url` and `hosts` all add `:8443`;
/// on 443 the port is left out.
async fn public_urls_use_the_bound_https_port() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                routes: Some(Arc::new(BoundAt(8443))),
                ..Setup::default()
            },
        )
        .await;
        let public = "https://web.demo.localhost:8443";
        assert!(web_urls(&h, &main).await.iter().any(|u| u == public));

        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        assert!(web_urls(&h, &main).await.iter().any(|u| u == public));
        let url = h
            .call(Method::Url, json!({"unit": "web"}), cx(&main))
            .await
            .expect("url");
        assert_eq!(url["url"], public);
        let hosts = h
            .call(Method::Hosts, json!({}), cx(&main))
            .await
            .expect("hosts");
        let items = hosts["items"].as_array().expect("items");
        let url_of = |host: &str| {
            items
                .iter()
                .find(|i| i["host"] == host)
                .map(|i| i["url"].clone())
        };
        assert_eq!(url_of("web.demo.localhost"), Some(json!(public)));
        assert_eq!(url_of("*.pages.demo.localhost"), Some(Value::Null));
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;

        let standard = harness(
            &tmp.path.join("home2"),
            Setup {
                routes: Some(Arc::new(BoundAt(443))),
                ..Setup::default()
            },
        )
        .await;
        assert!(
            web_urls(&standard, &main)
                .await
                .iter()
                .any(|u| u == "https://web.demo.localhost")
        );
        standard.close().await;
    })
    .await
    .expect("timeout");
}

/// An HTTP listener that fell back to 8080 because `requested` was taken.
struct FellBack(u16);

impl RouteSink for FellBack {
    fn publish(&self, _: Vec<RouteEntry>) {}

    fn status(&self) -> Value {
        json!({"http": {"bound": true, "port": 8080, "requested_port": self.0, "fallback_reason": "in use"}})
    }
}

/// Names this test process as the holder of every port.
struct HeldByMe;

#[async_trait::async_trait]
impl devlanes_daemon::integrations::PortInspector for HeldByMe {
    async fn holder(&self, _: u16) -> Option<u32> {
        Some(std::process::id())
    }
}

#[tokio::test(flavor = "multi_thread")]
/// `doctor` names the process holding a proxy port the listener fell back from.
async fn doctor_names_the_holder_of_a_taken_proxy_port() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                routes: Some(Arc::new(FellBack(80))),
                ports: Some(Arc::new(HeldByMe)),
                ..Setup::default()
            },
        )
        .await;
        let doctor = h
            .call(Method::Doctor, json!({}), cx(&main))
            .await
            .expect("doctor");
        let holder = &doctor["proxy"]["http"]["requested_holder"];
        assert_eq!(holder["pid"], std::process::id(), "{doctor}");
        assert!(
            holder["command"].as_str().is_some_and(|c| !c.is_empty()),
            "{holder}"
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

/// An HTTP listener on its configured port.
struct OnRequested(u16);

impl RouteSink for OnRequested {
    fn publish(&self, _: Vec<RouteEntry>) {}

    fn status(&self) -> Value {
        json!({"http": {"bound": true, "port": self.0, "requested_port": self.0, "fallback_reason": null}})
    }
}

/// Names this test process and a PID that no process has as listeners on every port.
struct SharedWithAnother;

/// Above any PID Windows, Linux or macOS hands out.
const NO_SUCH_PID: u32 = u32::MAX - 1;

#[async_trait::async_trait]
impl devlanes_daemon::integrations::PortInspector for SharedWithAnother {
    async fn holder(&self, _: u16) -> Option<u32> {
        Some(std::process::id())
    }

    async fn holders(&self, _: u16) -> Vec<u32> {
        vec![std::process::id(), NO_SUCH_PID]
    }
}

#[tokio::test(flavor = "multi_thread")]
/// `doctor` names other processes listening on the port the proxy holds, but not the daemon.
async fn doctor_names_other_listeners_on_a_proxy_port() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                routes: Some(Arc::new(OnRequested(80))),
                ports: Some(Arc::new(SharedWithAnother)),
                ..Setup::default()
            },
        )
        .await;
        let doctor = h
            .call(Method::Doctor, json!({}), cx(&main))
            .await
            .expect("doctor");
        let http = &doctor["proxy"]["http"];
        assert_eq!(
            http["shared_with"],
            json!([{"pid": NO_SUCH_PID, "command": null}]),
            "{doctor}"
        );
        assert!(http.get("requested_holder").is_none(), "{doctor}");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A routed host outside the devlanes CA's names keeps its route but is named in
/// the start result, `doctor` and `config check` with a name under the lane's
/// domain; an unconstrained CA (mkcert) gives no warning.
async fn hosts_outside_the_ca_are_warned_about() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: demo\nservices:\n  web:\n    run: serve\n    hosts: [\"web.${{ lane.domain }}\", \"web.example.com\"]\n";
        let main = repo(&tmp.path, "r", config);
        let constrained: Vec<String> = ["localhost", "test", "internal", "home.arpa"]
            .map(str::to_owned)
            .to_vec();
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                ca_permitted: Some(constrained),
                ..Setup::default()
            },
        )
        .await;
        let started = h
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let warnings = started["warnings"].as_array().expect("warnings");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0]["code"], "host_outside_ca");
        assert_eq!(warnings[0]["data"]["host"], "web.example.com");
        assert_eq!(warnings[0]["data"]["unit"], "web");
        assert_eq!(warnings[0]["data"]["suggestion"], "web.demo.localhost");
        let hosts = h.call(Method::Hosts, json!({}), cx(&main)).await.expect("hosts");
        assert!(
            hosts.to_string().contains("\"web.example.com\""),
            "the route stays"
        );
        let doctor = h.call(Method::Doctor, json!({}), cx(&main)).await.expect("doctor");
        assert_eq!(doctor["ca_warnings"][0]["data"]["host"], "web.example.com");
        let check = h
            .call(Method::ConfigCheck, json!({}), cx(&main))
            .await
            .expect("config check");
        let found = check["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .find(|w| w["code"] == "host_outside_ca")
            .expect("config check warns");
        assert_eq!(found["path"], "services.web.hosts");
        assert!(found["message"].as_str().is_some_and(|m| m.contains("web.demo.localhost")));
        h.call(Method::Stop, json!({}), cx(&main)).await.expect("stop");
        h.close().await;

        let mkcert = harness(&tmp.path.join("home2"), Setup::default()).await;
        let started = mkcert
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        assert_eq!(started["warnings"], json!([]));
        mkcert.call(Method::Stop, json!({}), cx(&main)).await.expect("stop");
        mkcert.close().await;
    })
    .await
    .expect("timeout");
}
