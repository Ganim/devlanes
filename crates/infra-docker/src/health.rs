use std::{net::SocketAddr, time::Duration};

use bollard::models::{
    ContainerInspectResponse, ContainerStateStatusEnum, ContainerSummaryHealthStatusEnum,
    ContainerSummaryStateEnum, HealthStatusEnum,
};
use serde::Serialize;
use tokio::{io::AsyncReadExt, net::TcpStream};

/// Container health, from the image healthcheck when it has one, else from a tcp probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum Health {
    /// Running without a healthcheck or probe port.
    Running,
    Starting,
    Healthy,
    Unhealthy,
    Exited {
        code: i64,
    },
    /// Created, paused, restarting or another transitional state.
    Other {
        state: String,
    },
    /// No such container.
    Missing,
}

impl Health {
    /// States that will not become healthy without intervention.
    pub fn is_final_failure(&self) -> bool {
        matches!(
            self,
            Health::Unhealthy | Health::Exited { .. } | Health::Missing
        )
    }
}

/// What `inspect` says about a container. `needs_probe` is set for running containers
/// without a Docker healthcheck, where the caller decides with [`tcp_probe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectHealth {
    pub health: Health,
    pub needs_probe: bool,
}

/// Maps `State` and `State.Health` from `inspect`.
pub fn health_from_inspect(inspect: &ContainerInspectResponse) -> InspectHealth {
    let Some(state) = &inspect.state else {
        return InspectHealth {
            health: Health::Missing,
            needs_probe: false,
        };
    };
    let status = state.status.unwrap_or(ContainerStateStatusEnum::EMPTY);
    let health = match status {
        ContainerStateStatusEnum::RUNNING => {
            match state.health.as_ref().and_then(|health| health.status) {
                Some(HealthStatusEnum::HEALTHY) => Health::Healthy,
                Some(HealthStatusEnum::UNHEALTHY) => unhealthy_status(inspect),
                Some(HealthStatusEnum::STARTING) => Health::Starting,
                _ => {
                    return InspectHealth {
                        health: Health::Running,
                        needs_probe: true,
                    };
                }
            }
        }
        ContainerStateStatusEnum::EXITED | ContainerStateStatusEnum::DEAD => Health::Exited {
            code: state.exit_code.unwrap_or_default(),
        },
        other => Health::Other {
            state: other.to_string(),
        },
    };
    InspectHealth {
        health,
        needs_probe: false,
    }
}

/// Docker's default `Healthcheck.Retries`, used when the config leaves it at 0.
const DEFAULT_HEALTH_RETRIES: i64 = 3;

/// Reads a running container whose Docker health says `unhealthy`.
///
/// Docker sets `unhealthy` when it stops the health monitor on pause, keeping the
/// failing streak, and the status stays until the next check after the unpause.
/// Only a streak of `retries` failed checks makes a container really unhealthy.
/// Below that, a successful check in the log means the container was healthy when
/// it was paused, so it counts as healthy again right away; without one it was
/// still starting.
fn unhealthy_status(inspect: &ContainerInspectResponse) -> Health {
    let retries = inspect
        .config
        .as_ref()
        .and_then(|config| config.healthcheck.as_ref())
        .and_then(|check| check.retries)
        .filter(|retries| *retries > 0)
        .unwrap_or(DEFAULT_HEALTH_RETRIES);
    let Some(health) = inspect
        .state
        .as_ref()
        .and_then(|state| state.health.as_ref())
    else {
        return Health::Unhealthy;
    };
    if health.failing_streak.unwrap_or(retries) >= retries {
        return Health::Unhealthy;
    }
    let passed_before = health
        .log
        .iter()
        .flatten()
        .any(|result| result.exit_code == Some(0));
    if passed_before {
        Health::Healthy
    } else {
        Health::Starting
    }
}

/// Maps the summary returned by `list_containers`, which has no exit code.
pub fn health_from_summary(
    state: Option<ContainerSummaryStateEnum>,
    health: Option<ContainerSummaryHealthStatusEnum>,
) -> Health {
    match state {
        Some(ContainerSummaryStateEnum::RUNNING) => match health {
            Some(ContainerSummaryHealthStatusEnum::HEALTHY) => Health::Healthy,
            Some(ContainerSummaryHealthStatusEnum::UNHEALTHY) => Health::Unhealthy,
            Some(ContainerSummaryHealthStatusEnum::STARTING) => Health::Starting,
            _ => Health::Running,
        },
        Some(ContainerSummaryStateEnum::EXITED | ContainerSummaryStateEnum::DEAD) => {
            Health::Exited { code: 0 }
        }
        Some(other) => Health::Other {
            state: other.to_string(),
        },
        None => Health::Missing,
    }
}

/// Probes a published port on 127.0.0.1. Docker's port proxies (Docker Desktop, OrbStack,
/// docker-proxy) accept connections before the container listens and then close them, so
/// an accepted connection only counts when it stays open for `settle` or sends data.
pub async fn tcp_probe(port: u16, connect_timeout: Duration, settle: Duration) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(Ok(mut stream)) = tokio::time::timeout(connect_timeout, TcpStream::connect(addr)).await
    else {
        return false;
    };
    let mut buffer = [0_u8; 1];
    match tokio::time::timeout(settle, stream.read(&mut buffer)).await {
        // The server spoke first (Redis errors, SMTP banners): something is listening.
        Ok(Ok(read)) => read > 0,
        Ok(Err(_)) => false,
        // Still open and silent, which is how most servers wait for a client.
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use bollard::models::{
        ContainerConfig, ContainerState, Health as DockerHealth, HealthConfig, HealthcheckResult,
    };
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    use super::*;

    fn inspect(
        status: ContainerStateStatusEnum,
        health: Option<HealthStatusEnum>,
    ) -> ContainerInspectResponse {
        ContainerInspectResponse {
            state: Some(ContainerState {
                status: Some(status),
                exit_code: Some(3),
                health: health.map(|status| DockerHealth {
                    status: Some(status),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn maps_inspect_states() {
        use ContainerStateStatusEnum as S;
        let cases = [
            (
                inspect(S::RUNNING, Some(HealthStatusEnum::HEALTHY)),
                Health::Healthy,
                false,
            ),
            (
                inspect(S::RUNNING, Some(HealthStatusEnum::STARTING)),
                Health::Starting,
                false,
            ),
            (
                inspect(S::RUNNING, Some(HealthStatusEnum::UNHEALTHY)),
                Health::Unhealthy,
                false,
            ),
            (inspect(S::RUNNING, None), Health::Running, true),
            (
                inspect(S::RUNNING, Some(HealthStatusEnum::NONE)),
                Health::Running,
                true,
            ),
            (inspect(S::EXITED, None), Health::Exited { code: 3 }, false),
            (
                inspect(S::CREATED, None),
                Health::Other {
                    state: "created".into(),
                },
                false,
            ),
            (ContainerInspectResponse::default(), Health::Missing, false),
        ];
        for (response, health, needs_probe) in cases {
            assert_eq!(
                health_from_inspect(&response),
                InspectHealth {
                    health,
                    needs_probe
                }
            );
        }
    }

    /// A running container that Docker calls `unhealthy`, with `retries` in its
    /// healthcheck config (`None` leaves the config out), the failing streak, and
    /// the exit codes of the checks in its log.
    fn unhealthy(retries: Option<i64>, streak: i64, log: &[i64]) -> Health {
        let mut response = inspect(
            ContainerStateStatusEnum::RUNNING,
            Some(HealthStatusEnum::UNHEALTHY),
        );
        response.config = retries.map(|retries| ContainerConfig {
            healthcheck: Some(HealthConfig {
                retries: Some(retries),
                ..Default::default()
            }),
            ..Default::default()
        });
        if let Some(health) = response
            .state
            .as_mut()
            .and_then(|state| state.health.as_mut())
        {
            health.failing_streak = Some(streak);
            health.log = Some(
                log.iter()
                    .map(|code| HealthcheckResult {
                        exit_code: Some(*code),
                        ..Default::default()
                    })
                    .collect(),
            );
        }
        health_from_inspect(&response).health
    }

    /// `unhealthy` after an unpause is a real failure only once the failing streak
    /// reaches the configured retries, or Docker's default of 3.
    #[test]
    fn unhealthy_below_retries_comes_from_a_pause() {
        // Healthy before the pause, with or without failed checks since.
        assert_eq!(unhealthy(None, 0, &[0]), Health::Healthy);
        assert_eq!(unhealthy(None, 2, &[0, 1, 1]), Health::Healthy);
        assert_eq!(unhealthy(Some(0), 2, &[0, 1, 1]), Health::Healthy);
        assert_eq!(unhealthy(Some(5), 4, &[0, 1, 1, 1, 1]), Health::Healthy);
        // Paused before any check passed.
        assert_eq!(unhealthy(None, 0, &[]), Health::Starting);
        assert_eq!(unhealthy(None, 1, &[1]), Health::Starting);
        // Enough failed checks.
        assert_eq!(unhealthy(None, 3, &[0, 1, 1, 1]), Health::Unhealthy);
        assert_eq!(unhealthy(Some(1), 1, &[0, 1]), Health::Unhealthy);
        let paused = inspect(
            ContainerStateStatusEnum::PAUSED,
            Some(HealthStatusEnum::UNHEALTHY),
        );
        assert_eq!(
            health_from_inspect(&paused).health,
            Health::Other {
                state: "paused".into()
            }
        );
    }

    #[test]
    fn maps_summary_states() {
        assert_eq!(
            health_from_summary(
                Some(ContainerSummaryStateEnum::RUNNING),
                Some(ContainerSummaryHealthStatusEnum::HEALTHY)
            ),
            Health::Healthy
        );
        assert_eq!(
            health_from_summary(Some(ContainerSummaryStateEnum::RUNNING), None),
            Health::Running
        );
        assert_eq!(health_from_summary(None, None), Health::Missing);
    }

    #[tokio::test]
    async fn probe_needs_a_listener_that_keeps_the_connection() {
        let quick = Duration::from_millis(200);
        let silent = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = silent.local_addr().expect("addr").port();
        let holder = tokio::spawn(async move {
            let (_stream, _) = silent.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        assert!(
            tcp_probe(port, quick, quick).await,
            "silent open connection counts"
        );
        holder.abort();

        let closing = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = closing.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (stream, _) = closing.accept().await.expect("accept");
            drop(stream);
        });
        assert!(
            !tcp_probe(port, quick, quick).await,
            "proxy-style close does not count"
        );

        let talking = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = talking.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut stream, _) = talking.accept().await.expect("accept");
            let _ = stream.write_all(b"220 ready\r\n").await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        assert!(tcp_probe(port, quick, quick).await, "banner counts");

        let unused = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = unused.local_addr().expect("addr").port();
        drop(unused);
        assert!(!tcp_probe(port, quick, quick).await, "refused connection");
    }
}
