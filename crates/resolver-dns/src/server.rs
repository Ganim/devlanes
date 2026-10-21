use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::{DnsError, Zone, answer};

/// Default port: unprivileged, so the daemon never needs root to serve DNS.
pub const DEFAULT_PORT: u16 = 15353;

/// Large enough for any query a stub resolver sends, EDNS options included.
const UDP_BUFFER: usize = 4096;

/// Bounds TCP clients; the OS resolver uses TCP only as a UDP fallback.
const MAX_TCP_CONNECTIONS: usize = 64;

/// Closes TCP connections that send nothing, so stuck clients cannot hold permits.
const TCP_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Attempts to find a free port for both UDP and TCP when binding port 0.
const EPHEMERAL_ATTEMPTS: usize = 16;

/// `127.0.0.1:15353`.
pub fn default_listen() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT)
}

/// A bound DNS server over UDP and TCP on the same port.
#[derive(Debug)]
pub struct DnsServer {
    udp: UdpSocket,
    tcp: TcpListener,
    zone: Arc<Zone>,
}

impl DnsServer {
    /// Binds UDP and TCP on `listen`.
    ///
    /// Port 0 picks one ephemeral port free for both protocols, which tests use.
    pub async fn bind(listen: SocketAddr, zone: Zone) -> Result<Self, DnsError> {
        let bind_error = |addr, source| DnsError::Bind { addr, source };
        if listen.port() != 0 {
            let udp = UdpSocket::bind(listen)
                .await
                .map_err(|e| bind_error(listen, e))?;
            let tcp = TcpListener::bind(listen)
                .await
                .map_err(|e| bind_error(listen, e))?;
            return Ok(Self::new(udp, tcp, zone));
        }
        let mut last = io::Error::new(io::ErrorKind::AddrInUse, "no shared free port");
        for _ in 0..EPHEMERAL_ATTEMPTS {
            let udp = UdpSocket::bind(listen)
                .await
                .map_err(|e| bind_error(listen, e))?;
            let addr = udp.local_addr().map_err(|e| bind_error(listen, e))?;
            match TcpListener::bind(addr).await {
                Ok(tcp) => return Ok(Self::new(udp, tcp, zone)),
                // The UDP port's TCP twin is taken; another ephemeral port will do.
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => last = error,
                Err(error) => return Err(bind_error(addr, error)),
            }
        }
        Err(bind_error(listen, last))
    }

    /// Wraps bound sockets.
    fn new(udp: UdpSocket, tcp: TcpListener, zone: Zone) -> Self {
        Self {
            udp,
            tcp,
            zone: Arc::new(zone),
        }
    }

    /// The bound address, shared by UDP and TCP.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    /// The zone being answered.
    pub fn zone(&self) -> &Zone {
        &self.zone
    }

    /// Serves until `shutdown` is cancelled, then closes every connection.
    ///
    /// Per-packet and per-connection errors are logged and skipped so one bad client
    /// cannot stop resolution for the whole machine.
    pub async fn serve(self, shutdown: CancellationToken) {
        let Self { udp, tcp, zone } = self;
        let permits = Arc::new(Semaphore::new(MAX_TCP_CONNECTIONS));
        let mut connections = JoinSet::new();
        let mut buf = vec![0u8; UDP_BUFFER];
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                received = udp.recv_from(&mut buf) => match received {
                    Ok((len, peer)) => {
                        if let Some(response) = answer(&buf[..len], &zone)
                            && let Err(error) = udp.send_to(&response, peer).await
                        {
                            tracing::debug!(%peer, %error, "dns udp send failed");
                        }
                    }
                    // ICMP errors from earlier sends surface here on some platforms.
                    Err(error) => tracing::debug!(%error, "dns udp receive failed"),
                },
                accepted = tcp.accept() => match accepted {
                    Ok((stream, peer)) => {
                        let Ok(permit) = permits.clone().try_acquire_owned() else {
                            tracing::debug!(%peer, "dns tcp connection limit reached");
                            continue;
                        };
                        let zone = zone.clone();
                        connections.spawn(async move {
                            if let Err(error) = serve_tcp(stream, &zone).await {
                                tracing::debug!(%peer, %error, "dns tcp connection ended");
                            }
                            drop(permit);
                        });
                    }
                    Err(error) => tracing::debug!(%error, "dns tcp accept failed"),
                },
                // Reap finished connections so the set does not grow without bound.
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        connections.shutdown().await;
    }
}

/// Answers length-prefixed queries on one TCP connection (RFC 7766) until it closes.
async fn serve_tcp(mut stream: TcpStream, zone: &Zone) -> io::Result<()> {
    loop {
        let mut len = [0u8; 2];
        match tokio::time::timeout(TCP_IDLE_TIMEOUT, stream.read_exact(&mut len)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Ok(Err(error)) => return Err(error),
            Err(_) => return Ok(()),
        }
        let mut message = vec![0u8; usize::from(u16::from_be_bytes(len))];
        tokio::time::timeout(TCP_IDLE_TIMEOUT, stream.read_exact(&mut message))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "partial dns message"))??;
        let Some(response) = answer(&message, zone) else {
            continue;
        };
        let response_len = u16::try_from(response.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "response too large"))?;
        let mut framed = Vec::with_capacity(response.len() + 2);
        framed.extend_from_slice(&response_len.to_be_bytes());
        framed.extend_from_slice(&response);
        stream.write_all(&framed).await?;
    }
}
