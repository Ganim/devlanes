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
#[cfg(unix)]
pub const DEFAULT_PORT: u16 = 15353;

/// Default port: NRPT rules name DNS servers by address only, so the server must
/// answer on 53. Windows has no privileged ports, so this needs no admin.
#[cfg(windows)]
pub const DEFAULT_PORT: u16 = 53;

/// The Windows listen address: a loopback address of its own, because
/// `127.0.0.1:53` and `0.0.0.0:53` are often held already (Internet Connection
/// Sharing, Docker, WSL), and an exact-address bind coexists with those
/// wildcard binds. Windows routes all of `127.0.0.0/8` to loopback.
#[cfg(windows)]
const WINDOWS_LISTEN_IP: Ipv4Addr = Ipv4Addr::new(127, 53, 0, 1);

/// Large enough for any query a stub resolver sends, EDNS options included.
const UDP_BUFFER: usize = 4096;

/// Bounds TCP clients; the OS resolver uses TCP only as a UDP fallback.
const MAX_TCP_CONNECTIONS: usize = 64;

/// Closes TCP connections that send nothing, so stuck clients cannot hold permits.
const TCP_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause after a failed TCP accept, typically EMFILE.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Attempts to find a free port for both UDP and TCP when binding port 0.
const EPHEMERAL_ATTEMPTS: usize = 16;

/// `127.0.0.1:15353`, the address the OS resolver file names.
#[cfg(unix)]
pub fn default_listen() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT)
}

/// `127.53.0.1:53`, the address the NRPT rule names.
#[cfg(windows)]
pub fn default_listen() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(WINDOWS_LISTEN_IP), DEFAULT_PORT)
}

/// A bound DNS server over UDP and TCP on the same port.
#[derive(Debug)]
pub struct DnsServer {
    udp: UdpSocket,
    tcp: TcpListener,
    zone: Arc<Zone>,
}

/// Whether a failed TCP bind on the UDP port's number should try another
/// ephemeral port: the TCP twin is taken.
#[cfg(not(windows))]
fn retry_ephemeral(kind: io::ErrorKind) -> bool {
    kind == io::ErrorKind::AddrInUse
}

/// Whether a failed TCP bind on the UDP port's number should try another
/// ephemeral port: the TCP twin is taken, or it lies inside a TCP range that
/// Hyper-V or WSL reserved, which Windows reports as access denied.
#[cfg(windows)]
fn retry_ephemeral(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

/// A port in the IANA dynamic range (49152-65535), different per attempt.
fn random_dynamic_port(seed: &std::hash::RandomState, attempt: usize) -> u16 {
    use std::hash::BuildHasher as _;
    const FIRST: u16 = 49152;
    let span = u64::from(u16::MAX - FIRST) + 1;
    let offset = u16::try_from(seed.hash_one(attempt) % span).unwrap_or(0);
    FIRST + offset
}

/// Nothing to add to a bind error outside Windows.
#[cfg(not(windows))]
pub(crate) fn bind_hint(_addr: &SocketAddr, _source: &io::Error) -> &'static str {
    ""
}

/// Where to look when port 53 cannot be bound: a socket on `0.0.0.0:53` with
/// `SO_EXCLUSIVEADDRUSE` makes Windows refuse every other address on the port
/// with `WSAEACCES`, and one on this exact address reports it in use.
#[cfg(windows)]
pub(crate) fn bind_hint(addr: &SocketAddr, source: &io::Error) -> &'static str {
    let held = matches!(
        source.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::AddrInUse
    );
    if addr.port() == 53 && held {
        "; another program holds port 53 on this address or on all addresses (see Get-NetUDPEndpoint -LocalPort 53 and Get-NetTCPConnection -LocalPort 53)"
    } else {
        ""
    }
}

impl DnsServer {
    /// Binds UDP and TCP on `listen`.
    ///
    /// Port 0 picks one ephemeral port free for both protocols, which tests use.
    pub async fn bind(listen: SocketAddr, zone: Zone) -> Result<Self, DnsError> {
        let bind_error = |addr, source| DnsError::Bind { addr, source };
        // No SO_EXCLUSIVEADDRUSE on Windows: on our address it would make a
        // later wildcard bind of the port fail (Internet Connection Sharing
        // binding 0.0.0.0:53 after the daemon started), and Windows already
        // refuses other accounts a bind of this exact address and port.
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
        let seed = std::hash::RandomState::new();
        for attempt in 0..EPHEMERAL_ATTEMPTS {
            // Windows hands out UDP ports in sequence, so after one lands in a
            // reserved TCP block the next ones would too: retry at random ports.
            let mut candidate = listen;
            if attempt > 0 {
                candidate.set_port(random_dynamic_port(&seed, attempt));
            }
            let udp = match UdpSocket::bind(candidate).await {
                Ok(udp) => udp,
                Err(error) if attempt > 0 && retry_ephemeral(error.kind()) => {
                    last = error;
                    continue;
                }
                Err(error) => return Err(bind_error(candidate, error)),
            };
            let addr = udp.local_addr().map_err(|e| bind_error(listen, e))?;
            match TcpListener::bind(addr).await {
                Ok(tcp) => return Ok(Self::new(udp, tcp, zone)),
                Err(error) if retry_ephemeral(error.kind()) => last = error,
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
                    Err(error) => {
                        tracing::debug!(%error, "dns tcp accept failed");
                        // EMFILE/ENFILE keep failing until a descriptor frees up; without
                        // a pause this arm would spin and starve the UDP side.
                        tokio::select! {
                            () = shutdown.cancelled() => break,
                            () = tokio::time::sleep(ACCEPT_BACKOFF) => {}
                        }
                    }
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
        // A client that stops reading must not pin the task and its permit forever.
        tokio::time::timeout(TCP_IDLE_TIMEOUT, stream.write_all(&framed))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dns client stopped reading"))??;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Access denied means a reserved range only on Windows; elsewhere it is a
    /// real error that a retry would hide.
    #[test]
    fn ephemeral_retries_follow_the_os() {
        assert!(retry_ephemeral(io::ErrorKind::AddrInUse));
        assert!(!retry_ephemeral(io::ErrorKind::AddrNotAvailable));
        #[cfg(windows)]
        assert!(retry_ephemeral(io::ErrorKind::PermissionDenied));
        #[cfg(not(windows))]
        assert!(!retry_ephemeral(io::ErrorKind::PermissionDenied));
    }

    /// Retries spread over the dynamic range instead of following the OS's
    /// sequential pick into the same reserved block.
    #[test]
    fn retry_ports_are_dynamic_and_spread() {
        let seed = std::hash::RandomState::new();
        let ports: std::collections::BTreeSet<u16> = (1..EPHEMERAL_ATTEMPTS)
            .map(|attempt| random_dynamic_port(&seed, attempt))
            .collect();
        assert!(ports.iter().all(|port| *port >= 49152));
        assert!(ports.len() > EPHEMERAL_ATTEMPTS / 2, "{ports:?}");
    }

    /// A held port 53 on Windows says where to look; other failures, ports and
    /// OSes keep the plain message.
    #[test]
    fn bind_errors_name_the_port_53_holder_on_windows() {
        let message = |port: u16, kind: io::ErrorKind| {
            DnsError::Bind {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 53, 0, 1)), port),
                source: io::Error::from(kind),
            }
            .to_string()
        };
        let hint = "Get-NetUDPEndpoint -LocalPort 53";
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::AddrInUse] {
            assert_eq!(message(53, kind).contains(hint), cfg!(windows), "{kind:?}");
            assert!(!message(5353, kind).contains(hint));
        }
        assert!(!message(53, io::ErrorKind::AddrNotAvailable).contains(hint));
        assert!(
            message(53, io::ErrorKind::AddrInUse)
                .starts_with("cannot bind the DNS server on 127.53.0.1:53: ")
        );
    }

    /// Unix resolver files name a port; NRPT rules cannot, so Windows uses 53 on
    /// an address of its own.
    #[test]
    fn default_listen_follows_the_os() {
        #[cfg(unix)]
        assert_eq!(default_listen(), "127.0.0.1:15353".parse().expect("addr"));
        #[cfg(windows)]
        assert_eq!(default_listen(), "127.53.0.1:53".parse().expect("addr"));
        assert!(default_listen().ip().is_loopback());
    }

    /// The Windows address binds UDP and TCP without admin rights, next to
    /// whatever holds `0.0.0.0:53` (Internet Connection Sharing on many machines).
    #[cfg(windows)]
    #[tokio::test]
    async fn the_windows_address_binds_without_admin() {
        match DnsServer::bind(default_listen(), Zone::new("test").expect("zone")).await {
            Ok(server) => assert_eq!(server.local_addr().expect("addr"), default_listen()),
            // A devlanes daemon with the resolver strategy holds it on this machine.
            Err(DnsError::Bind { source, .. }) if source.kind() == io::ErrorKind::AddrInUse => {
                eprintln!("skipped: {source}");
            }
            Err(error) => panic!("binding {}: {error}", default_listen()),
        }
    }
}
