//! Processes a task left running or that escaped its sweep reach `run`, `start`,
//! unit summaries and `why`.

use devlanes_proto::{LaneSummary, LeftProcess, Leftovers, Method, Page, StartResult};
use serde_json::json;

use crate::{LIMIT, Setup, cx, harness, repo, temp};

const CONFIG: &str = r#"project: acme
services:
  api: { run: sleep }
tasks:
  db-start: { run: "daemonize pg_ctl; exit 0", when: lane.start }
  quiet: { run: "escape node; exit 0" }
  plain: { run: "echo hi" }
  boot: { steps: [plain, quiet] }
"#;

/// Process names of `processes`.
fn names(processes: &[LeftProcess]) -> Vec<&str> {
    processes.iter().map(|p| p.command.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
/// A `lane.start` task's kept daemon shows in the start result; a step's escaped
/// process in the `run` result; both in unit summaries and `why`.
async fn leftovers_reach_results_status_and_why() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;

        let start = h
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let start: StartResult = serde_json::from_value(start).expect("start result");
        let [
            Leftovers {
                unit,
                left_running,
                escaped,
            },
        ] = start.leftovers.as_slice()
        else {
            panic!("one task left something: {:?}", start.leftovers);
        };
        assert_eq!(unit, "db-start");
        assert_eq!(names(left_running), ["pg_ctl"]);
        assert!(escaped.is_empty());

        // Steps report every unit that left something; clean ones stay out.
        let run = h
            .call(Method::Run, json!({"task": "boot"}), cx(&main))
            .await
            .expect("run");
        assert_eq!(run["exit_code"], 0);
        assert_eq!(run["leftovers"][0]["unit"], "quiet");
        assert_eq!(run["leftovers"][0]["escaped"][0]["command"], "node");
        assert!(run["leftovers"][0].get("left_running").is_none());
        assert_eq!(run["leftovers"].as_array().map(Vec::len), Some(1));
        let plain = h
            .call(Method::Run, json!({"task": "plain"}), cx(&main))
            .await
            .expect("run");
        assert!(plain.get("leftovers").is_none(), "{plain}");

        let status = h
            .call(Method::Status, json!({}), cx(&main))
            .await
            .expect("status");
        let lanes: Page<LaneSummary> = serde_json::from_value(status).expect("page");
        let unit = |name: &str| {
            lanes.items[0]
                .units
                .iter()
                .find(|u| u.name == name)
                .cloned()
                .expect("unit")
        };
        assert_eq!(names(&unit("db-start").left_running), ["pg_ctl"]);
        assert_eq!(names(&unit("quiet").escaped), ["node"]);
        assert!(unit("api").left_running.is_empty());

        let why = h
            .call(Method::Why, json!({"unit": "db-start"}), cx(&main))
            .await
            .expect("why");
        assert_eq!(why["left_running"][0]["command"], "pg_ctl");
        assert!(why.get("escaped").is_none());

        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}
