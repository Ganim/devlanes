//! Box side of remote operation: `devlanes tunnel --stdio`.
//!
//! [`services`] answers a client daemon's mux streams from this machine's own
//! daemon, reached over its Unix socket like any other client:
//!
//! - the registry comes from `status --all` and `hosts`, refreshed on daemon events
//!   and published as incremental changes;
//! - forwarded requests each get a fresh daemon connection, so their streams
//!   (logs follow, attach) and cancellation stay independent;
//! - data streams dial loopback ports the registry advertises, nothing else;
//! - syncs stream `sync.status` entries; the client never chooses a path.

mod registry;
mod rpc;
mod sync;

use std::sync::Arc;

use devlanes_client::{Client, Paths};
use devlanes_proto::remote::RemoteClient;
use devlanes_transport::{BoxServices, LoopbackDialer, ServeError, ServeOptions};

/// Why the tunnel stopped.
#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    /// The box daemon could not be reached or started.
    #[error("box daemon: {0}")]
    Daemon(#[from] devlanes_client::Error),
    /// The session with the client failed.
    #[error(transparent)]
    Serve(#[from] ServeError),
}

/// Box services backed by the daemon behind `paths`.
///
/// `client` is listed by the box's `status` while the registry feed is open.
pub fn services(paths: Paths, client: RemoteClient) -> BoxServices {
    BoxServices {
        registry: Arc::new(registry::DaemonRegistry::new(paths.clone(), client)),
        rpc: Arc::new(rpc::DaemonRpc::new(paths.clone())),
        dialer: Arc::new(LoopbackDialer),
        sync: Arc::new(sync::DaemonSync::new(paths)),
    }
}

/// Version label sent to clients in the handshake.
pub fn server_label() -> String {
    format!("devlanes {}", env!("CARGO_PKG_VERSION"))
}

/// Serves one client over this process's stdin and stdout until it leaves.
///
/// Starts the box daemon when it is not running. Nothing but mux frames may reach
/// stdout, so diagnostics belong on stderr, which ssh hands back to the client.
pub async fn run_stdio(paths: Paths) -> Result<(), TunnelError> {
    drop(Client::connect_or_spawn(&paths).await?);
    let client = RemoteClient {
        pid: std::process::id(),
        ssh: std::env::var("SSH_CONNECTION")
            .ok()
            .filter(|value| !value.is_empty()),
        since_ms: now_ms(),
    };
    devlanes_transport::serve(
        devlanes_transport::stdio(),
        services(paths, client),
        ServeOptions::new(server_label()),
    )
    .await?;
    Ok(())
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A daemon error carried as transport RPC error data, so the client daemon can
/// hand the structured error to its CLI unchanged.
pub(crate) fn rpc_error(error: devlanes_proto::RpcError) -> devlanes_transport::RpcError {
    devlanes_transport::RpcError {
        message: error.code.clone(),
        data: serde_json::to_value(&error).ok(),
    }
}

/// `daemon_unreachable` for a failed connection to the box daemon.
pub(crate) fn unreachable(error: &devlanes_client::Error) -> devlanes_proto::RpcError {
    devlanes_proto::RpcError::new(
        "daemon_unreachable",
        serde_json::json!({"message": error.to_string(), "side": "box"}),
        4,
    )
}
