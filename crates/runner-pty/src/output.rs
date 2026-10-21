//! Reader tasks that drain PTY masters and pipes into a broadcast channel.

use std::{io, os::fd::OwnedFd, sync::Arc};

use bytes::Bytes;
use devlanes_core::{OutputChunk, OutputStream};
use tokio::{
    io::{AsyncReadExt, unix::AsyncFd},
    net::unix::pipe,
    sync::{broadcast, watch},
    task::JoinSet,
};

/// Read size per syscall; large enough for bursty build output.
const CHUNK: usize = 16 * 1024;

/// Starts one reader per stream and flips `done` once all reached EOF.
///
/// Readers never wait for subscribers: a lagging receiver loses chunks rather than
/// stalling the process on a full PTY buffer.
pub(crate) fn start(
    master: Option<Arc<AsyncFd<OwnedFd>>>,
    pipes: Vec<(OutputStream, OwnedFd)>,
    sender: broadcast::Sender<OutputChunk>,
    done: watch::Sender<bool>,
) -> io::Result<()> {
    // Convert every pipe before spawning so a failure cannot leave half-started readers.
    let pipes = pipes
        .into_iter()
        .map(|(stream, fd)| Ok((stream, pipe::Receiver::from_owned_fd(fd)?)))
        .collect::<io::Result<Vec<_>>>()?;
    let mut readers = JoinSet::new();
    if let Some(master) = master {
        readers.spawn(read_terminal(master, sender.clone()));
    }
    for (stream, receiver) in pipes {
        readers.spawn(read_pipe(stream, receiver, sender.clone()));
    }
    drop(sender);
    tokio::spawn(async move {
        while readers.join_next().await.is_some() {}
        let _ = done.send(true);
    });
    Ok(())
}

/// Reads the master until EOF. Linux reports EIO once the last slave descriptor closes.
async fn read_terminal(master: Arc<AsyncFd<OwnedFd>>, sender: broadcast::Sender<OutputChunk>) {
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        let Ok(mut guard) = master.readable().await else {
            return;
        };
        let read = guard
            .try_io(|fd| rustix::io::read(fd.get_ref(), &mut buffer[..]).map_err(io::Error::from));
        match read {
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => publish(&sender, OutputStream::Terminal, &buffer[..n]),
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(error)) => {
                if error.raw_os_error() != Some(rustix::io::Errno::IO.raw_os_error()) {
                    tracing::debug!(%error, "pty read failed");
                }
                return;
            }
            // Spurious readiness; try_io cleared it.
            Err(_) => {}
        }
    }
}

/// Reads a pipe until every writer in the unit closed it.
async fn read_pipe(
    stream: OutputStream,
    mut receiver: pipe::Receiver,
    sender: broadcast::Sender<OutputChunk>,
) {
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        match receiver.read(&mut buffer).await {
            Ok(0) => return,
            Ok(n) => publish(&sender, stream, &buffer[..n]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                tracing::debug!(%error, "pipe read failed");
                return;
            }
        }
    }
}

/// Broadcasts one chunk; having no subscribers is not an error.
fn publish(sender: &broadcast::Sender<OutputChunk>, stream: OutputStream, bytes: &[u8]) {
    let _ = sender.send(OutputChunk {
        stream,
        bytes: Bytes::copy_from_slice(bytes),
    });
}
