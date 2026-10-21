//! Forwarded daemon requests (`devlanes --host <box> <cmd>`).

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use devlanes_client::{Client, Frame, Paths};
use devlanes_proto::{
    Control, Method, Outcome, ResizeParams, RpcError,
    remote::{Forward, Input, Reply},
};
use devlanes_transport::{RpcHandler, RpcInput, RpcReplyStream};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{rpc_error, unreachable};

/// [`RpcHandler`] that replays each request on its own daemon connection.
pub(crate) struct DaemonRpc {
    paths: Paths,
}

impl DaemonRpc {
    /// Forwards to the daemon behind `paths`.
    pub(crate) fn new(paths: Paths) -> Self {
        Self { paths }
    }
}

/// Methods a client may not run on the box through the tunnel.
fn refused(method: Method) -> Option<RpcError> {
    match method {
        // Secrets travel from a terminal on the box itself, never from another machine.
        Method::Unlock => Some(RpcError::new(
            "not_supported_yet",
            json!({"feature": "remote unlock", "hint": "run devlanes secrets unlock on the box"}),
            1,
        )),
        // A box connecting onward to further boxes is not part of v1.
        Method::Connect | Method::Disconnect | Method::HostList => Some(RpcError::new(
            "not_supported_yet",
            json!({"feature": "nested remote connections"}),
            1,
        )),
        _ => None,
    }
}

#[async_trait]
impl RpcHandler for DaemonRpc {
    /// Sends the request, then relays events, bytes and the answer as [`Reply`]s.
    async fn call(
        &self,
        request: Value,
        input: RpcInput,
    ) -> Result<RpcReplyStream, devlanes_transport::RpcError> {
        let forward: Forward = serde_json::from_value(request).map_err(|error| {
            rpc_error(RpcError::new(
                "bad_request",
                json!({"message": error.to_string()}),
                2,
            ))
        })?;
        if let Some(error) = refused(forward.method) {
            return Err(rpc_error(error));
        }
        let mut client = Client::connect(&self.paths)
            .await
            .map_err(|error| rpc_error(unreachable(&error)))?;
        let id = client
            .begin_request(forward.method, forward.params, forward.context)
            .await
            .map_err(|error| rpc_error(unreachable(&error)))?;
        let (replies, receiver) = mpsc::channel(64);
        tokio::spawn(relay(
            client,
            id,
            forward.method == Method::Attach,
            input,
            replies,
        ));
        Ok(
            futures_util::stream::unfold(receiver, |mut receiver| async move {
                receiver.recv().await.map(|item| (item, receiver))
            })
            .boxed(),
        )
    }
}

/// A reply item for the stream.
type Item = Result<Value, devlanes_transport::RpcError>;

/// Pumps one daemon connection until the answer (or, for attach, the stream's end)
/// or until the client cancels, which drops the connection and so the request.
async fn relay(
    mut client: Client,
    id: u64,
    attach: bool,
    mut input: RpcInput,
    replies: mpsc::Sender<Item>,
) {
    let mut input_open = true;
    let mut stream: Option<u64> = None;
    loop {
        tokio::select! {
            frame = client.next_frame() => {
                let reply = match frame {
                    Ok(Frame::Control(Control::Event(event))) => Reply::Event { event },
                    Ok(Frame::Control(Control::Response(response))) if response.id == id => {
                        let opened = match &response.result {
                            Outcome::Ok { data } if attach => {
                                data.get("stream_id").and_then(Value::as_u64)
                            }
                            _ => None,
                        };
                        let reply = Reply::Response { outcome: response.result };
                        if !send(&replies, &reply).await {
                            return;
                        }
                        match opened {
                            Some(opened) => {
                                stream = Some(opened);
                                continue;
                            }
                            None => return,
                        }
                    }
                    // Answers to resizes sent on this connection.
                    Ok(Frame::Control(_)) => continue,
                    Ok(Frame::Bytes { stream_id, data }) => {
                        let end = data.is_empty() && stream == Some(stream_id);
                        let reply = Reply::Bytes {
                            stream_id,
                            data: base64::engine::general_purpose::STANDARD.encode(&data),
                        };
                        if !send(&replies, &reply).await || end {
                            return;
                        }
                        continue;
                    }
                    Err(error) => {
                        if stream.is_none() {
                            let _ = replies.send(Err(rpc_error(unreachable(&error)))).await;
                        }
                        return;
                    }
                };
                if !send(&replies, &reply).await {
                    return;
                }
            }
            value = input.recv(), if input_open => match value {
                None => input_open = false,
                Some(value) => {
                    if apply_input(&mut client, stream, value).await.is_err() {
                        return;
                    }
                }
            },
            () = replies.closed() => return,
        }
    }
}

/// Serializes and sends one reply; false once the client is gone.
async fn send(replies: &mpsc::Sender<Item>, reply: &Reply) -> bool {
    match serde_json::to_value(reply) {
        Ok(value) => replies.send(Ok(value)).await.is_ok(),
        Err(_) => false,
    }
}

/// Applies attach input to the open stream; input for other streams is dropped.
async fn apply_input(
    client: &mut Client,
    stream: Option<u64>,
    value: Value,
) -> Result<(), devlanes_client::Error> {
    let Ok(input) = serde_json::from_value::<Input>(value) else {
        return Ok(());
    };
    match input {
        Input::Bytes { stream_id, data } if Some(stream_id) == stream => {
            let data = base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap_or_default();
            client.send_bytes(stream_id, Bytes::from(data)).await
        }
        Input::Resize {
            stream_id,
            columns,
            rows,
        } if Some(stream_id) == stream => {
            let params = serde_json::to_value(ResizeParams {
                stream_id,
                columns,
                rows,
            })
            .unwrap_or(Value::Null);
            client
                .begin_request(Method::Resize, params, devlanes_proto::Context::default())
                .await
                .map(drop)
        }
        _ => Ok(()),
    }
}
