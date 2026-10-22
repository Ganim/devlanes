//! `devlanes start` (and `restart`, `lane resume`, `lane new --start`) inside
//! the TUI, as mprocs does. The request goes out first: a start the daemon
//! refuses at once (no config, unknown unit) is printed as a CLI error with its
//! usual exit status, and the TUI only opens once the start is underway.

use std::time::Duration;

use devlanes_cli::{Hint, Rendered, Runtime};
use devlanes_client::{Client, Paths};
use devlanes_proto::{Method, Request, RpcError};
use devlanes_tui::{
    ApiError,
    start::{StartExit, await_start, follow_start},
};
use serde_json::json;

use crate::{driver::rpc_error, output::emit};

/// How long a start may stay silent before the TUI opens anyway. Refusals come
/// back within milliseconds; a start that says nothing for this long is running.
const OPEN_AFTER: Duration = Duration::from_millis(300);

/// Exit status when the user leaves before the start's result, as for a
/// Ctrl-C detach: the daemon keeps starting.
const DETACHED: u8 = 130;

/// Whether `tui.open_on_start` in the user config allows it; true when unset
/// or unreadable.
pub fn open_on_start(paths: &Paths) -> bool {
    devlanes_config::load_user(paths.home.join("config.yaml"))
        .map_or(true, |config| config.tui.open_on_start)
}

/// The command's verb, shown in the TUI's header.
fn label(method: Method) -> &'static str {
    match method {
        Method::Restart => "restart",
        Method::Resume => "resume",
        Method::LaneNew => "new lane",
        _ => "start",
    }
}

/// Sends `request` on `client`, then runs the TUI on it. Returns 0 when the
/// start was ready, 1 when it failed, 130 when the user quit before its result.
pub async fn run(
    runtime: &Runtime,
    paths: &Paths,
    mut client: Client,
    request: Request,
    hint: impl Fn(&RpcError) -> Option<Hint>,
) -> Result<u8, RpcError> {
    let mut params = request.params.clone();
    params["events"] = json!(true);
    let id = client
        .begin_request(request.method, params, request.context.clone())
        .await
        .map_err(rpc_error)?;
    let stream = follow_start(client, id);
    let waited = tokio::select! {
        waited = await_start(label(request.method).into(), stream, OPEN_AFTER) => waited,
        _ = tokio::signal::ctrl_c() => {
            keeps_running();
            return Ok(DETACHED);
        }
    };
    let handoff = match waited {
        Ok(handoff) => handoff,
        Err(error) => {
            let error = api_error(error);
            let rendered = runtime.reporter().error(&error, hint(&error).as_ref());
            emit(&rendered);
            return Ok(rendered.exit_code);
        }
    };
    let options = crate::wiring::tui_options(paths).await;
    let exit = devlanes_tui::run_start(paths.clone(), options, handoff)
        .await
        .map_err(crate::wiring::tui_error)?;
    Ok(match exit {
        StartExit::Ready => 0,
        StartExit::Failed => 1,
        StartExit::Pending => {
            keeps_running();
            DETACHED
        }
    })
}

/// Says that leaving did not stop the start.
fn keeps_running() {
    emit(&Rendered {
        stderr: "the start keeps running in the daemon; devlanes status shows it\n".into(),
        ..Rendered::default()
    });
}

/// The TUI's view of a failed request as the CLI's error.
fn api_error(error: ApiError) -> RpcError {
    match error {
        ApiError::Rpc(error) => error,
        ApiError::Disconnected(message) => {
            RpcError::new("daemon_unreachable", json!({"message": message}), 4)
        }
        ApiError::Decode(message) => RpcError::new("bad_response", json!({"message": message}), 1),
    }
}
