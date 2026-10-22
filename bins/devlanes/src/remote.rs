//! Remote entry points that run in a terminal or over SSH: `devlanes tunnel --stdio`
//! (started on the box by the client's SSH transport) and `devlanes askpass` (run by ssh).

use std::io::Write;

use devlanes_cli::Action;
#[cfg(unix)]
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
#[cfg(unix)]
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

/// A Windows machine is never a box, so it has no tunnel to serve. The message
/// goes to stderr like every tunnel failure; exit 2 marks it as unsupported.
#[cfg(windows)]
async fn tunnel() -> Result<u8, RpcError> {
    eprintln!("devlanes tunnel: not supported on Windows");
    Ok(2)
}

/// Answers ssh's prompt when this process was started as `SSH_ASKPASS` on
/// Windows: the marker variable is `1`, the relay variable holds a pipe name of
/// the shape the daemon creates, and there is exactly one argument, the prompt
/// (ssh runs `<program> <prompt>`, and Windows has no wrapper script to add the
/// subcommand). `None` means a normal command line, run as usual.
///
/// The argument is taken as the prompt even when it starts with `-`, since
/// nothing limits what ssh may ask. ssh's other children (a `ProxyCommand`)
/// inherit the same environment, so a single-argument `devlanes` call from one
/// of them is answered as a prompt too.
#[cfg(windows)]
pub fn askpass_from_env() -> Option<u8> {
    use devlanes_transport_ssh::askpass::{MARKER_ENV, SOCKET_ENV};
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let prompt = askpass_prompt(
        std::env::var_os(MARKER_ENV).as_deref(),
        std::env::var_os(SOCKET_ENV).as_deref(),
        &args,
    )?;
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => Some(runtime.block_on(askpass(&prompt))),
        Err(error) => {
            eprintln!("devlanes askpass: {error}");
            Some(1)
        }
    }
}

/// The prompt when `marker`, `relay` and the arguments after the program name
/// say ssh started this process as askpass; see [`askpass_from_env`].
#[cfg(windows)]
fn askpass_prompt(
    marker: Option<&std::ffi::OsStr>,
    relay: Option<&std::ffi::OsStr>,
    args: &[std::ffi::OsString],
) -> Option<String> {
    use devlanes_transport_ssh::askpass::relay_process_id;
    if marker.is_none_or(|marker| marker != "1") || relay.and_then(relay_process_id).is_none() {
        return None;
    }
    let [prompt] = args else {
        return None;
    };
    Some(prompt.to_string_lossy().into_owned())
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

#[cfg(all(test, windows))]
mod tests {
    use std::ffi::{OsStr, OsString};

    use super::askpass_prompt;

    /// The marker, a relay name of the daemon's shape and a single argument
    /// together mean ssh's askpass, whatever the argument looks like; anything
    /// else (no marker, a missing or foreign relay name, other argument counts)
    /// is an ordinary command line.
    #[test]
    fn askpass_needs_marker_relay_and_one_prompt() {
        let prompt = [OsString::from("Enter passphrase for key 'C:\\k': ")];
        let on = Some(OsStr::new("1"));
        let relay = Some(OsStr::new(
            r"\\.\pipe\devlanes-askpass-42-0123456789abcdef0123456789abcdef",
        ));
        let unc = Some(OsStr::new(
            r"\\example.invalid\pipe\devlanes-askpass-42-0123456789abcdef0123456789abcdef",
        ));
        assert_eq!(
            askpass_prompt(on, relay, &prompt).as_deref(),
            Some("Enter passphrase for key 'C:\\k': ")
        );
        assert_eq!(
            askpass_prompt(on, relay, &[OsString::from("-x")]).as_deref(),
            Some("-x")
        );
        assert_eq!(askpass_prompt(None, relay, &prompt), None);
        assert_eq!(askpass_prompt(Some(OsStr::new("0")), relay, &prompt), None);
        assert_eq!(askpass_prompt(on, None, &prompt), None);
        assert_eq!(askpass_prompt(on, unc, &prompt), None);
        // `DEVLANES_ASKPASS=1 devlanes --version` without a relay runs normally.
        assert_eq!(
            askpass_prompt(on, None, &[OsString::from("--version")]),
            None
        );
        assert_eq!(
            askpass_prompt(on, relay, &[OsString::from("remote"), OsString::from("ls")]),
            None
        );
        assert_eq!(askpass_prompt(on, relay, &[]), None);
    }
}
