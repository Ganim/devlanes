//! `devlanes with` and `stop --logs-dir` over the real client.

use std::{collections::BTreeMap, path::Path};

use async_trait::async_trait;
use devlanes_cli::{
    CommandExit, LaneLogQuery, ResourcePeak, UnitFailure, UnitLogs, WithDriver, WithOwnership,
    WithStart,
};
use devlanes_client::{Client, Error as ClientError};
use devlanes_ipc::Frame;
use devlanes_proto::{
    Context, Control, Event, Method, Outcome, Request, RpcError, Selection, StartResult, UnitPeak,
};
use serde_json::{Value, json};

/// Keeps one connection and the invocation context for every follow-up query.
pub struct ClientDriver {
    client: Client,
    context: Context,
    ownership: WithOwnership,
}

impl ClientDriver {
    /// Records whether this connection spawned the daemon before anything else happens.
    pub fn new(client: Client, context: Context) -> Self {
        let ownership = WithOwnership {
            daemon_spawned: client.spawned,
            ..WithOwnership::default()
        };
        Self {
            client,
            context,
            ownership,
        }
    }

    /// Lane-scoped context; the client env is only sent with process-starting requests.
    fn lane_context(&self, lane: &str) -> Context {
        Context {
            lane: Some(lane.to_owned()),
            env: None,
            ..self.context.clone()
        }
    }

    /// One request, mapping transport failures to the structured error shape.
    async fn call(
        &mut self,
        method: Method,
        params: Value,
        context: Context,
    ) -> Result<Value, RpcError> {
        self.client
            .request(method, params, context)
            .await
            .map_err(rpc_error)
    }
}

/// Transport errors become exit code 4 (daemon unreachable), RPC errors keep their own.
pub fn rpc_error(error: ClientError) -> RpcError {
    match error {
        ClientError::Rpc(error) => error,
        other => RpcError::new(
            "daemon_unreachable",
            json!({"message": other.to_string()}),
            4,
        ),
    }
}

#[async_trait]
impl WithDriver for ClientDriver {
    /// Streams start events and keeps ownership even when the start fails part-way.
    async fn start(
        &mut self,
        request: &Request,
        events: &mut (dyn FnMut(Event) + Send),
    ) -> Result<WithStart, RpcError> {
        let id = self
            .client
            .begin_request(
                request.method,
                request.params.clone(),
                request.context.clone(),
            )
            .await
            .map_err(rpc_error)?;
        loop {
            match self.client.next_frame().await.map_err(rpc_error)? {
                Frame::Control(Control::Event(event)) => events(event),
                Frame::Control(Control::Response(response)) if response.id == id => {
                    return match response.result {
                        Outcome::Ok { data } => {
                            let result: StartResult =
                                serde_json::from_value(data).map_err(|error| {
                                    RpcError::new(
                                        "bad_response",
                                        json!({"message": error.to_string()}),
                                        1,
                                    )
                                })?;
                            self.ownership.lane.clone_from(&result.lane);
                            self.ownership.units.clone_from(&result.started);
                            self.ownership.whole_lane = result.whole_lane;
                            Ok(WithStart {
                                ready: result.ready && result.failures.is_empty(),
                                failures: result
                                    .failures
                                    .into_iter()
                                    .map(|failure| UnitFailure {
                                        lane: failure.lane,
                                        unit: failure.unit,
                                        exit_code: failure.exit_code,
                                        log_tail: failure.log_tail,
                                    })
                                    .collect(),
                            })
                        }
                        Outcome::Error { error } => {
                            // A failed start may still name its lane, so teardown can find it.
                            if let Some(lane) = error.data.get("lane").and_then(Value::as_str) {
                                self.ownership.lane = lane.to_owned();
                            }
                            if let Some(started) =
                                error.data.get("started").and_then(Value::as_array)
                            {
                                self.ownership.units = started
                                    .iter()
                                    .filter_map(|v| v.as_str().map(str::to_owned))
                                    .collect();
                            }
                            Err(error)
                        }
                    };
                }
                _ => {}
            }
        }
    }

    /// The journal recorded by `start`.
    fn ownership(&self) -> WithOwnership {
        self.ownership.clone()
    }

    /// Lane-level env: DEVLANES_*, DEVLANES_PUBLIC_URL_<SERVICE>, CA variables.
    async fn env(&mut self, lane: &str) -> Result<BTreeMap<String, String>, RpcError> {
        let context = self.lane_context(lane);
        let data = self
            .call(Method::Env, json!({"lane_env": true}), context)
            .await?;
        serde_json::from_value(data.get("env").cloned().unwrap_or_default()).map_err(|error| {
            RpcError::new("bad_response", json!({"message": error.to_string()}), 1)
        })
    }

    /// All lines of every unit since the lane's current start.
    async fn logs(&mut self, lane: &str, query: LaneLogQuery) -> Result<Vec<UnitLogs>, RpcError> {
        let context = self.lane_context(lane);
        let params = serde_json::to_value(&query).map_err(|error| {
            RpcError::new("bad_request", json!({"message": error.to_string()}), 1)
        })?;
        let data = self.call(Method::Logs, params, context).await?;
        serde_json::from_value(data.get("units").cloned().unwrap_or_else(|| json!([]))).map_err(
            |error| RpcError::new("bad_response", json!({"message": error.to_string()}), 1),
        )
    }

    /// Peaks from run history for the summary table.
    async fn metrics(&mut self, lane: &str) -> Result<Vec<ResourcePeak>, RpcError> {
        let context = self.lane_context(lane);
        let data = self
            .call(Method::Metrics, json!({"peaks": true}), context)
            .await?;
        let peaks: Vec<UnitPeak> =
            serde_json::from_value(data.get("peaks").cloned().unwrap_or_else(|| json!([])))
                .map_err(|error| {
                    RpcError::new("bad_response", json!({"message": error.to_string()}), 1)
                })?;
        Ok(peaks
            .into_iter()
            .map(|peak| ResourcePeak {
                unit: peak.unit,
                peak_memory_bytes: peak.peak_memory_bytes,
                peak_cpu_percent: peak.peak_cpu_percent,
            })
            .collect())
    }

    /// Stops exactly the selection in that lane.
    async fn stop(&mut self, lane: &str, selection: Selection) -> Result<(), RpcError> {
        let context = self.lane_context(lane);
        let params = serde_json::to_value(selection).map_err(|error| {
            RpcError::new("bad_request", json!({"message": error.to_string()}), 1)
        })?;
        self.call(Method::Stop, params, context).await.map(drop)
    }

    /// Only reached when this invocation spawned the daemon.
    async fn stop_daemon_if_spawned(&mut self) -> Result<(), RpcError> {
        let context = Context {
            yes: true,
            ..self.lane_context("")
        };
        self.call(Method::DaemonShutdown, json!({}), context)
            .await
            .map(drop)
    }

    /// Runs the test command in the foreground; the first Ctrl-C goes to the command only.
    async fn spawn_command(
        &mut self,
        argv: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<CommandExit, RpcError> {
        let Some((program, args)) = argv.split_first() else {
            return Err(RpcError::new(
                "usage",
                json!({"message": "missing command"}),
                2,
            ));
        };
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .env_clear()
            .envs(env)
            .current_dir(cwd)
            .spawn()
            .map_err(|error| {
                RpcError::new(
                    "spawn_failed",
                    json!({"message": error.to_string(), "program": program}),
                    127,
                )
            })?;
        let pid = child.id();
        loop {
            tokio::select! {
                status = child.wait() => {
                    let status = status.map_err(|error| RpcError::new("wait_failed", json!({"message": error.to_string()}), 1))?;
                    use std::os::unix::process::ExitStatusExt;
                    return Ok(match (status.code(), status.signal()) {
                        (Some(code), _) => CommandExit::Code(u8::try_from(code).unwrap_or(1)),
                        (None, Some(signal)) => CommandExit::Signal(u8::try_from(signal).unwrap_or(1)),
                        (None, None) => CommandExit::Code(1),
                    });
                }
                // The child shares our process group, so the terminal already delivered SIGINT;
                // forwarding covers non-terminal senders (CI cancellation sends to us only).
                _ = tokio::signal::ctrl_c() => {
                    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()).and_then(rustix::process::Pid::from_raw) {
                        let _ = rustix::process::kill_process(pid, rustix::process::Signal::INT);
                    }
                }
            }
        }
    }
}
