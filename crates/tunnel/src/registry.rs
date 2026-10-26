//! The box registry, followed through the daemon's `remote.registry` feed.

use async_trait::async_trait;
use devlanes_client::{Client, Frame, Paths};
use devlanes_proto::{
    Context, Control, Event, Method, Outcome, RpcError,
    remote::{REGISTRY_CHANGE_EVENT, REGISTRY_SNAPSHOT_EVENT, RegistryParams, RemoteClient},
};
use devlanes_transport::{
    BoxError, ClientInfo, RegistryEvent, RegistryFeed, RegistrySnapshot, RegistrySource,
    RegistryUpdate,
};
use futures_util::StreamExt;
use serde::Deserialize;

/// [`RegistrySource`] over the box daemon's socket.
pub(crate) struct DaemonRegistry {
    paths: Paths,
    client: RemoteClient,
}

impl DaemonRegistry {
    /// Reads the daemon behind `paths`; `client` describes this tunnel to `status`.
    pub(crate) fn new(paths: Paths, client: RemoteClient) -> Self {
        Self { paths, client }
    }
}

/// Why the daemon's registry feed cannot be used.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FeedError {
    /// The connection to the box daemon failed.
    #[error("box daemon: {0}")]
    Daemon(#[from] devlanes_client::Error),
    /// The daemon predates `remote.registry`: it runs an older release than this tunnel.
    #[error(
        "the box daemon runs an older devlanes than `devlanes tunnel`; run `devlanes daemon \
         restart` on the box"
    )]
    OldDaemon,
    /// The daemon refused the request.
    #[error("the box daemon refused remote.registry: {} ({})", .0.code, .0.data)]
    Refused(RpcError),
    /// The daemon ended the feed.
    #[error("the box daemon ended the registry feed")]
    Ended,
    /// The tunnel could not encode its own `remote.registry` params.
    #[error("encoding the remote.registry request: {0}")]
    Params(serde_json::Error),
    /// An event did not have the shape of its name.
    #[error("the box daemon sent a malformed {event} event: {error}")]
    Malformed {
        /// The event name.
        event: String,
        /// What did not decode.
        error: serde_json::Error,
    },
}

/// Payload of [`REGISTRY_SNAPSHOT_EVENT`].
#[derive(Deserialize)]
struct SnapshotData {
    snapshot: RegistrySnapshot,
}

/// Payload of [`REGISTRY_CHANGE_EVENT`].
#[derive(Deserialize)]
struct ChangeData {
    change: RegistryEvent,
}

#[async_trait]
impl RegistrySource for DaemonRegistry {
    /// Starts `remote.registry` for `client` and waits for its first snapshot.
    async fn subscribe(&self, client: &ClientInfo) -> Result<RegistryFeed, BoxError> {
        let mut daemon = Client::connect(&self.paths)
            .await
            .map_err(FeedError::from)?;
        let params = RegistryParams {
            client: Some(RemoteClient {
                id: client.id.clone(),
                name: client.name.clone(),
                ..self.client.clone()
            }),
            features: client.features.clone(),
        };
        let params = serde_json::to_value(params).map_err(FeedError::Params)?;
        daemon
            .begin_request(Method::RemoteRegistry, params, Context::default())
            .await
            .map_err(FeedError::from)?;
        let snapshot = match next_update(&mut daemon).await? {
            RegistryUpdate::Resync(snapshot) => snapshot,
            RegistryUpdate::Event(_) => {
                return Err(FeedError::Malformed {
                    event: REGISTRY_CHANGE_EVENT.into(),
                    error: serde::de::Error::custom("a change came before the snapshot"),
                }
                .into());
            }
        };
        // Ends after the first error: the session ends with it.
        let updates = futures_util::stream::unfold(Some(daemon), |daemon| async move {
            let mut daemon = daemon?;
            match next_update(&mut daemon).await {
                Ok(update) => Some((Ok(update), Some(daemon))),
                Err(error) => Some((Err(BoxError::from(error)), None)),
            }
        })
        .boxed();
        Ok(RegistryFeed { snapshot, updates })
    }
}

/// The next registry update of the feed on `daemon`. Other events are skipped,
/// so a newer daemon may add some.
async fn next_update(daemon: &mut Client) -> Result<RegistryUpdate, FeedError> {
    loop {
        match daemon.next_frame().await? {
            Frame::Control(Control::Event(event)) => {
                if let Some(update) = decode(&event)? {
                    return Ok(update);
                }
            }
            Frame::Control(Control::Response(response)) => {
                return Err(match response.result {
                    Outcome::Error { error } if error.code == "unknown_method" => {
                        FeedError::OldDaemon
                    }
                    Outcome::Error { error } => FeedError::Refused(error),
                    Outcome::Ok { .. } => FeedError::Ended,
                });
            }
            _ => {}
        }
    }
}

/// The update a feed event carries; `None` for other events.
fn decode(event: &Event) -> Result<Option<RegistryUpdate>, FeedError> {
    let malformed = |error| FeedError::Malformed {
        event: event.event.clone(),
        error,
    };
    match event.event.as_str() {
        REGISTRY_SNAPSHOT_EVENT => {
            let data: SnapshotData =
                serde_json::from_value(event.data.clone()).map_err(malformed)?;
            Ok(Some(RegistryUpdate::Resync(data.snapshot)))
        }
        REGISTRY_CHANGE_EVENT => {
            let data: ChangeData = serde_json::from_value(event.data.clone()).map_err(malformed)?;
            Ok(Some(RegistryUpdate::Event(data.change)))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use devlanes_transport::{LaneKey, UnitKey, UnitKind};
    use serde_json::json;

    use super::*;

    fn event(name: &str, data: serde_json::Value) -> Event {
        Event {
            seq: 0,
            event: name.into(),
            data,
        }
    }

    #[test]
    fn feed_events_decode_into_updates() -> Result<(), FeedError> {
        let snapshot = decode(&event(
            REGISTRY_SNAPSHOT_EVENT,
            json!({"snapshot": {"lanes": [], "units": []}}),
        ))?;
        assert!(matches!(snapshot, Some(RegistryUpdate::Resync(ref s)) if s.units.is_empty()));
        let change = decode(&event(
            REGISTRY_CHANGE_EVENT,
            json!({"change": {"type": "unit_removed", "unit": {
                "lane": {"project": "app", "lane": "0"}, "kind": "task", "name": "migrate"
            }}}),
        ))?;
        let removed = UnitKey {
            lane: LaneKey {
                project: "app".into(),
                lane: "0".into(),
            },
            kind: UnitKind::Task,
            name: "migrate".into(),
        };
        assert!(matches!(
            change,
            Some(RegistryUpdate::Event(RegistryEvent::UnitRemoved { ref unit })) if *unit == removed
        ));
        assert!(decode(&event("lane.changed", json!({})))?.is_none());
        Ok(())
    }

    /// A malformed answer is an error, never an empty registry that would
    /// unbind everything on the client.
    #[test]
    fn malformed_feed_events_are_errors() {
        for data in [
            json!({}),
            json!({"snapshot": {"lanes": "nope"}}),
            json!({"snapshot": null}),
        ] {
            let result = decode(&event(REGISTRY_SNAPSHOT_EVENT, data.clone()));
            assert!(
                matches!(result, Err(FeedError::Malformed { .. })),
                "{data}: {result:?}"
            );
        }
        let result = decode(&event(
            REGISTRY_CHANGE_EVENT,
            json!({"change": {"type": "x"}}),
        ));
        let message = result.expect_err("unknown change").to_string();
        assert!(message.contains("remote.registry.change"), "{message}");
    }

    /// A box daemon from before `remote.registry` answers `unknown_method`,
    /// which names the fix instead of the bare code. The request carries the
    /// client's identity and the session's features.
    #[tokio::test]
    async fn an_old_daemon_is_named_with_the_fix() -> crate::test_daemon::TestResult {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let dir = tempfile::tempdir()?;
            let paths = crate::test_daemon::home(dir.path())?;
            let daemon = crate::test_daemon::answer_one(&paths, |_| Outcome::Error {
                error: RpcError::new("unknown_method", json!({"method": "unknown"}), 2),
            })
            .await?;
            let registry = DaemonRegistry::new(paths, RemoteClient::default());
            let client = ClientInfo {
                label: "devlanes test".into(),
                id: "0123abcd".into(),
                name: "laptop".into(),
                features: vec!["mirror_report".into()],
            };
            let Err(error) = registry.subscribe(&client).await else {
                panic!("an old daemon gave a feed");
            };
            assert!(
                matches!(
                    error.downcast_ref::<FeedError>(),
                    Some(FeedError::OldDaemon)
                ),
                "{error}"
            );
            assert!(
                error.to_string().contains("`devlanes daemon restart`"),
                "{error}"
            );
            let request = daemon.await??;
            assert_eq!(request.method, Method::RemoteRegistry);
            let params: RegistryParams = serde_json::from_value(request.params)?;
            let sent = params.client.ok_or("no client in the params")?;
            assert_eq!(
                (sent.id.as_str(), sent.name.as_str()),
                ("0123abcd", "laptop")
            );
            assert_eq!(params.features, ["mirror_report"]);
            Ok(())
        })
        .await?
    }
}
