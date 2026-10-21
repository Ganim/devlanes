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
