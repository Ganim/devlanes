//! Infra failures carry what ran; units that a failed phase kept idle say why.

use devlanes_daemon::integrations::{InfraError, STDERR_TAIL_LINES};
use devlanes_proto::{LaneSummary, Method, Page, events};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, harness, repo, temp};

const CONFIG: &str = r#"project: acme
infra:
  containers:
    db: { image: "postgres:16", ports: { pg: 5432 }, per_lane: true }
services:
  api: { run: sleep }
  worker: { run: sleep }
"#;

/// The lane of `cwd`.
async fn first_lane(h: &crate::Harness, cwd: &std::path::Path) -> LaneSummary {
    let value = h
        .call(Method::Status, json!({}), cx(cwd))
        .await
        .expect("status");
    let lanes: Page<LaneSummary> = serde_json::from_value(value).expect("page");
    lanes.items[0].clone()
}

/// The reason of `unit` in the lane of `cwd`.
async fn unit_reason(h: &crate::Harness, cwd: &std::path::Path, unit: &str) -> Value {
    let value = h
        .call(Method::Status, json!({}), cx(cwd))
        .await
        .expect("status");
    let lanes: Page<LaneSummary> = serde_json::from_value(value).expect("page");
    let summary = lanes.items[0]
        .units
        .iter()
        .find(|u| u.name == unit)
        .expect("unit")
        .clone();
    serde_json::to_value(summary.reason).expect("reason")
}

#[tokio::test(flavor = "multi_thread")]
/// A failed `compose up` reports command, status and a stripped stderr tail; the
/// selected units carry `phase_failed` until a start reaches the services phase.
async fn infra_failure_is_structured_and_marks_units() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        let stderr: String = (0..60)
            .map(|i| format!("\x1b[31mline {i}\x1b[0m\n"))
            .collect();
        let failure = InfraError::new("compose_failed", "compose up failed").with_command(
            "docker compose -p acme up --detach --wait -- db",
            Some("exit status: 1".into()),
            Some(&stderr),
        );
        *h.infra.fail_up.lock().expect("lock") = Some(failure);
        let mut bus = h.engine.subscribe();

        let error = h
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect_err("infra fails");
        assert_eq!(error.code, "compose_failed");
        assert_eq!(error.data["message"], "compose up failed");
        assert_eq!(
            error.data["command"],
            "docker compose -p acme up --detach --wait -- db"
        );
        assert_eq!(error.data["status"], "exit status: 1");
        let tail = error.data["stderr"].as_str().expect("stderr");
        assert_eq!(tail.lines().count(), STDERR_TAIL_LINES);
        assert!(!tail.contains('\x1b'), "ANSI is stripped");
        assert_eq!(tail.lines().last(), Some("line 59"));
        assert_eq!(error.data["lane"], "acme@0");

        let reason = unit_reason(&h, &main, "api").await;
        assert_eq!(reason["code"], "phase_failed");
        assert_eq!(reason["data"]["phase"], "infra");
        assert_eq!(reason["data"]["lane"], "acme@0");
        assert_eq!(reason["data"]["error"]["code"], "compose_failed");
        assert_eq!(reason["data"]["error"]["data"]["status"], "exit status: 1");
        assert_eq!(
            unit_reason(&h, &main, "worker").await["code"],
            "phase_failed"
        );
        let mut announced = Vec::new();
        while let Ok(event) = bus.try_recv() {
            if event.event == events::UNIT_CHANGED
                && event.data["unit"]["reason"]["code"] == "phase_failed"
            {
                announced.push(
                    event.data["unit"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
        }
        announced.sort();
        assert_eq!(announced, ["api", "worker"]);

        *h.infra.fail_up.lock().expect("lock") = None;
        let started = h
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        assert_eq!(started["ready"], true, "{started}");
        assert_eq!(unit_reason(&h, &main, "api").await, Value::Null);
        assert_eq!(unit_reason(&h, &main, "worker").await, Value::Null);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// After an explicit full stop, a lane whose service crashed reads `idle`; the
/// crash stays in `errors` (run history).
async fn full_stop_settles_a_crashed_lane() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let config = "project: acme\nservices:\n  api: { run: sleep }\n  crash: { run: fail }\n";
        let main = repo(&tmp.path, "r", config);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let _ = h.call(Method::Start, json!({}), cx(&main)).await;
        let failed = crate::eventually(std::time::Duration::from_secs(5), || async {
            (first_lane(&h, &main).await.state == "failed").then_some(())
        })
        .await;
        assert!(failed.is_some(), "the crash fails the lane first");

        h.call(Method::Stop, json!({"units": ["api"]}), cx(&main))
            .await
            .expect("partial stop");
        assert_eq!(
            first_lane(&h, &main).await.state,
            "failed",
            "a partial stop keeps it"
        );

        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        let summary = first_lane(&h, &main).await;
        assert_eq!(summary.state, "idle");
        let crash = summary
            .units
            .iter()
            .find(|u| u.name == "crash")
            .expect("crash");
        assert_eq!(crash.state, "stopped");
        assert_eq!(crash.exit_code, Some(1));
        let errors = h
            .call(Method::Errors, json!({}), cx(&main))
            .await
            .expect("errors");
        assert!(errors.to_string().contains("crash"), "{errors}");
        h.close().await;
    })
    .await
    .expect("timeout");
}
