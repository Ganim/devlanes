use std::{
    collections::BTreeMap,
    future::Future,
    io,
    sync::{Arc, Mutex, PoisonError},
};

use devlanes_pki::CertificateAuthority;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::Proxy;

/// The scoped HTTPS ports a proxy serves ([`Proxy::listen_scoped`]), by port, so
/// each port opens once and closing one waits until its sockets are gone.
#[derive(Debug, Default)]
pub struct ScopedPorts {
    open: Mutex<BTreeMap<u16, Listening>>,
}

/// The listeners of one scoped port.
#[derive(Debug)]
struct Listening {
    stop: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl ScopedPorts {
    /// Serves `port` with `proxy`'s routes for that port and `ca` until
    /// [`Self::close`] or `shutdown`. A port open already stays as it is.
    pub fn open(
        &self,
        proxy: &Proxy,
        port: u16,
        ca: Arc<CertificateAuthority>,
        shutdown: &CancellationToken,
    ) -> io::Result<()> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if open.contains_key(&port) {
            return Ok(());
        }
        let stop = shutdown.child_token();
        let tasks = proxy.listen_scoped(port, ca, stop.clone())?;
        open.insert(port, Listening { stop, tasks });
        Ok(())
    }

    /// Stops serving `port`. The future resolves once its listeners are closed,
    /// so the port can be bound again right after, by this proxy or another
    /// process; dropping it still closes them, only later.
    pub fn close(&self, port: u16) -> impl Future<Output = ()> + Send + 'static {
        let listening = self
            .open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&port);
        async move {
            let Some(listening) = listening else { return };
            listening.stop.cancel();
            for task in listening.tasks {
                let _ = task.await;
            }
        }
    }

    /// The open ports, each with whether all of its listeners still accept.
    pub fn ports(&self) -> Vec<(u16, bool)> {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(port, listening)| {
                let serving = listening.tasks.iter().all(|task| !task.is_finished());
                (*port, serving)
            })
            .collect()
    }
}
