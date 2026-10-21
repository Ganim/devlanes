//! Remote entry points that run in a terminal or over SSH: `devlanes tunnel --stdio`
//! (started on the box by the client's SSH transport) and `devlanes askpass` (run by ssh).

use std::io::Write;

use devlanes_cli::Action;
use devlanes_client::Paths;
use devlanes_proto::RpcError;
use serde_json::json;

/// Routes the remote-only actions.
pub async fn run(action: Action) -> Result<u8, RpcError> {
    match action {
        Action::Tunnel => tunnel().await,
        Action::Askpass(prompt) => Ok(askpass(prompt.as_deref().unwrap_or_default()).await),
        _ => Err(RpcError::new(
            "usage",
            json!({"message": "unexpected action"}),
            2,
        )),
    }
}

/// Serves the client daemon's mux over stdio, backed by this machine's daemon.
///
/// stdout carries only mux frames; failures go straight to stderr (whatever the
/// output mode, since a JSON error on stdout would corrupt the pipe), and the
/// client's SSH transport returns that text as the connection error.
async fn tunnel() -> Result<u8, RpcError> {
    let result = match Paths::discover() {
        Ok(paths) => devlanes_tunnel::run_stdio(paths)
            .await
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    match result {
        Ok(()) => Ok(0),
        Err(message) => {
            eprintln!("devlanes tunnel: {message}");
            Ok(1)
        }
    }
}

/// Asks the daemon that launched ssh, which relays to the CLI running `connect`.
///
/// ssh reads the answer from stdout; exit status 1 tells it the prompt was cancelled.
async fn askpass(prompt: &str) -> u8 {
    match devlanes_transport_ssh::run_askpass(prompt).await {
        Ok(answer) => {
            let mut stdout = std::io::stdout();
            if writeln!(stdout, "{answer}")
                .and_then(|()| stdout.flush())
                .is_ok()
            {
                0
            } else {
                1
            }
        }
        Err(error) => {
            eprintln!("devlanes askpass: {error}");
            1
        }
    }
}
