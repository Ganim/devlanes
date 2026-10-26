//! A fake box daemon for unit tests: it answers one request on a test home's
//! endpoint, so tests can feed the tunnel answers a real daemon would not give.

use std::path::Path;

use devlanes_client::{Frame, Paths};
use devlanes_ipc::{Connection, local::LocalListener};
use devlanes_proto::{Control, Hello, Outcome, Request, Response};
use tokio::task::JoinHandle;

pub(crate) type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Paths of a prepared home below `dir`, with the endpoint inside it on Unix
/// so a host's `XDG_RUNTIME_DIR` stays untouched.
pub(crate) fn home(dir: &Path) -> devlanes_client::Result<Paths> {
    #[cfg_attr(windows, expect(unused_mut, reason = "the pipe name stays"))]
    let mut paths = Paths::new(dir.join("home"))?;
    #[cfg(unix)]
    {
        paths.socket = paths.run.join("daemon.sock");
    }
    paths.prepare()?;
    Ok(paths)
}

/// Listens at `paths`, then for one connection answers the hello and the
/// first request with `answer`. The task returns that request after the
/// client hangs up.
pub(crate) async fn answer_one(
    paths: &Paths,
    answer: impl FnOnce(&Request) -> Outcome + Send + 'static,
) -> Result<JoinHandle<Result<Request, devlanes_ipc::Error>>, devlanes_ipc::local::BindError> {
    let mut listener = LocalListener::bind(&paths.socket).await?;
    Ok(tokio::spawn(async move {
        let mut connection = Connection::new(listener.accept().await?);
        connection.recv().await?;
        connection
            .send(Frame::Control(Control::Hello(Hello::current(0))))
            .await?;
        let request = loop {
            if let Frame::Control(Control::Request(request)) = connection.recv().await? {
                break request;
            }
        };
        connection
            .send(Frame::Control(Control::Response(Response {
                id: request.id,
                result: answer(&request),
            })))
            .await?;
        // Held until the client leaves, as a daemon would.
        while connection.recv().await.is_ok() {}
        Ok(request)
    }))
}
