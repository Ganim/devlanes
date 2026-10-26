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
/// client's SSH transport returns that text as the connection error. The user
/// config's `remote.start_daemon` decides whether a stopped daemon is started;
/// it is read leniently so an unrelated bad key does not cut clients off.
#[cfg(unix)]
async fn tunnel() -> Result<u8, RpcError> {
    let result = match Paths::discover() {
        Ok(paths) => {
            let start_daemon = crate::user_config::lenient(&paths)
                .await
                .remote
                .start_daemon;
            devlanes_tunnel::run_stdio(paths, start_daemon)
                .await
                .map_err(|error| (error.to_string(), error.exit_code()))
        }
        Err(error) => Err((error.to_string(), 1)),
    };
    match result {
        Ok(()) => Ok(0),
        Err((message, code)) => {
            eprintln!("devlanes tunnel: {message}");
            Ok(code)
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

/// Refuses on stderr with exit 2: a Windows machine is never a box.
#[cfg(windows)]
async fn tunnel() -> Result<u8, RpcError> {
    eprintln!("devlanes tunnel: not supported on Windows");
    Ok(2)
}

/// Answers ssh's prompt when ssh started this process as `SSH_ASKPASS`:
/// Win32-OpenSSH runs `<program> <prompt>`, and Windows has no wrapper script
/// to add the subcommand. That is the case when the relay variable holds a
/// pipe name of the shape the daemon creates and there is exactly one
/// argument. `None` means a normal command line, run as usual.
///
/// The argument is the prompt even when it starts with `-`, since nothing
/// limits what ssh may ask. ssh's other children (a `ProxyCommand`) inherit
/// the same environment, so a one-argument `devlanes` call from one of them
/// is answered as a prompt too.
#[cfg(windows)]
pub fn askpass_from_env() -> Option<u8> {
    let relay = std::env::var_os(devlanes_transport_ssh::askpass::SOCKET_ENV);
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let prompt = askpass_prompt(relay.as_deref(), &args)?;
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

/// The prompt when `relay` and the arguments after the program name say ssh
/// started this process as askpass; see [`askpass_from_env`].
#[cfg(windows)]
fn askpass_prompt(relay: Option<&std::ffi::OsStr>, args: &[std::ffi::OsString]) -> Option<String> {
    if !relay.is_some_and(devlanes_transport_ssh::askpass::is_relay_name) {
        return None;
    }
    let [prompt] = args else {
        return None;
    };
    Some(prompt.to_string_lossy().into_owned())
}

#[cfg(all(test, windows))]
mod tests {
    use std::ffi::{OsStr, OsString};

    use super::askpass_prompt;

    /// A relay name of the daemon's shape and a single argument together mean
    /// ssh's askpass, whatever the argument looks like; anything else (no
    /// relay, a foreign or UNC name, other argument counts) is an ordinary
    /// command line.
    #[test]
    fn askpass_needs_a_relay_and_one_prompt() {
        let prompt = [OsString::from(r"Enter passphrase for key 'C:\k': ")];
        let relay = Some(OsStr::new(
            r"\\.\pipe\devlanes-askpass-42-0123456789abcdef0123456789abcdef",
        ));
        let unc = Some(OsStr::new(
            r"\\example.invalid\pipe\devlanes-askpass-42-0123456789abcdef0123456789abcdef",
        ));
        assert_eq!(
            askpass_prompt(relay, &prompt).as_deref(),
            Some(r"Enter passphrase for key 'C:\k': ")
        );
        assert_eq!(
            askpass_prompt(relay, &[OsString::from("-x")]).as_deref(),
            Some("-x")
        );
        assert_eq!(askpass_prompt(None, &prompt), None);
        assert_eq!(askpass_prompt(unc, &prompt), None);
        assert_eq!(askpass_prompt(Some(OsStr::new("1")), &prompt), None);
        assert_eq!(
            askpass_prompt(relay, &[OsString::from("remote"), OsString::from("ls")]),
            None
        );
        assert_eq!(askpass_prompt(relay, &[]), None);
    }

    /// The daemon and the client strip the relay variable under the name the
    /// transport sets.
    #[test]
    fn the_stripped_name_is_the_relay_variable() {
        assert_eq!(
            devlanes_proto::remote::ASKPASS_ENV,
            devlanes_transport_ssh::askpass::SOCKET_ENV
        );
    }
}
