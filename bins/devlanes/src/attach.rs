//! Raw terminal attach (`devlanes attach`). Detaching never stops the unit.

use std::io::{Read, Write};

use bytes::Bytes;
use devlanes_client::{Client, Paths};
use devlanes_ipc::Frame;
use devlanes_proto::{AttachParams, AttachResult, Control, Method, Outcome, Request, ResizeParams};
use tokio::sync::mpsc;

/// Detach chord, as in tmux/screen: Ctrl-A then `d`.
const PREFIX: u8 = 0x01;

/// Restores the terminal mode even when attach ends through an error path.
struct RawGuard(bool);

impl RawGuard {
    /// Only an interactive terminal is switched to raw mode.
    fn enable(interactive: bool) -> std::io::Result<Self> {
        if interactive {
            crossterm::terminal::enable_raw_mode()?;
        }
        Ok(Self(interactive))
    }
}

impl Drop for RawGuard {
    /// Leaving raw mode enabled would break the user's shell.
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Attaches stdin/stdout to a unit's terminal until detach or unit exit.
pub async fn run(paths: &Paths, request: Request) -> anyhow::Result<u8> {
    let mut client = Client::connect_or_spawn(paths).await?;
    let unit = request
        .params
        .get("unit")
        .and_then(|unit| unit.as_str())
        .unwrap_or_default()
        .to_owned();
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let (columns, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let params = serde_json::to_value(AttachParams {
        unit,
        interactive,
        columns: Some(columns),
        rows: Some(rows),
    })?;
    let attach: AttachResult = serde_json::from_value(
        client
            .request(Method::Attach, params, request.context.clone())
            .await?,
    )?;
    let _raw = RawGuard::enable(interactive)?;
    let mut stdout = std::io::stdout();
    if interactive {
        let _ = write!(
            stdout,
            "\r\n[attached to {}:{}; C-a d detaches]\r\n",
            attach.lane, attach.unit
        );
    }

    // stdin blocks; a thread keeps it off the runtime and is abandoned at exit.
    let (input_tx, mut input) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = [0_u8; 4096];
        while let Ok(read) = stdin.read(&mut buffer) {
            if read == 0 || input_tx.blocking_send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut prefix = false;
    loop {
        tokio::select! {
            frame = client.next_frame() => match frame? {
                Frame::Bytes { stream_id, data } if stream_id == attach.stream_id => {
                    if data.is_empty() {
                        if interactive { let _ = write!(stdout, "\r\n[unit exited]\r\n"); }
                        return Ok(0);
                    }
                    stdout.write_all(&data)?;
                    stdout.flush()?;
                }
                Frame::Control(Control::Response(response)) => {
                    if let Outcome::Error { error } = response.result {
                        tracing::debug!(code = %error.code, "attach side request failed");
                    }
                }
                _ => {}
            },
            Some(bytes) = input.recv() => {
                let mut forward = Vec::with_capacity(bytes.len());
                for byte in bytes {
                    if prefix {
                        prefix = false;
                        if byte == b'd' {
                            if interactive { let _ = write!(stdout, "\r\n[detached]\r\n"); }
                            return Ok(0);
                        }
                        // C-a C-a sends a literal C-a.
                        forward.push(PREFIX);
                        if byte != PREFIX { forward.push(byte); }
                    } else if byte == PREFIX && interactive {
                        prefix = true;
                    } else {
                        forward.push(byte);
                    }
                }
                if !forward.is_empty() {
                    client.send_bytes(attach.stream_id, Bytes::from(forward)).await?;
                }
            },
            _ = winch.recv() => {
                if let Ok((columns, rows)) = crossterm::terminal::size() {
                    let params = serde_json::to_value(ResizeParams { stream_id: attach.stream_id, columns, rows })?;
                    client.begin_request(Method::Resize, params, request.context.clone()).await?;
                }
            }
        }
    }
}
