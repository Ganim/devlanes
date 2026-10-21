use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

/// Both address families use the same public port; callers serve each listener.
#[derive(Debug)]
pub struct BoundListeners {
    pub ipv4: TcpListener,
    pub ipv6: TcpListener,
    pub port: u16,
    /// A human-readable diagnostic for CLI/doctor when the requested port was unavailable.
    pub fallback_reason: Option<String>,
}

/// Binds both families without privileges, falling back only for denied/occupied ports.
///
/// macOS privileged ports require wildcard binds, so callers must serve these through
/// `Proxy`, whose accept loop enforces the configured LAN policy. This must run inside
/// a Tokio runtime. Port zero is supported for isolated tests.
pub fn bind_listeners(port: u16, fallback_port: u16, lan: bool) -> io::Result<BoundListeners> {
    match bind_pair(port, lan) {
        Ok(listeners) => Ok(listeners),
        Err(error)
            if port != fallback_port
                && matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::AddrInUse
                ) =>
        {
            let mut listeners = bind_pair(fallback_port, lan)?;
            let reason = format!(
                "Could not bind port {port}: {error}; using {}. On Linux, configure CAP_NET_BIND_SERVICE or ip_unprivileged_port_start for low ports.",
                listeners.port
            );
            tracing::warn!("{reason}");
            listeners.fallback_reason = Some(reason);
            Ok(listeners)
        }
        Err(error) => Err(error),
    }
}

/// A failed second bind drops the first socket before any fallback attempt.
fn bind_pair(port: u16, lan: bool) -> io::Result<BoundListeners> {
    let wildcard = lan || (cfg!(target_os = "macos") && port != 0 && port < 1024);
    let ipv4 = if wildcard {
        Ipv4Addr::UNSPECIFIED
    } else {
        Ipv4Addr::LOCALHOST
    };
    let ipv6 = if wildcard {
        Ipv6Addr::UNSPECIFIED
    } else {
        Ipv6Addr::LOCALHOST
    };
    let ipv4 = bind(SocketAddr::new(ipv4.into(), port))?;
    let port = ipv4.local_addr()?.port();
    let ipv6 = bind(SocketAddr::new(ipv6.into(), port))?;
    Ok(BoundListeners {
        ipv4,
        ipv6,
        port,
        fallback_reason: None,
    })
}

/// IPV6_V6ONLY prevents the IPv6 socket from competing with the IPv4 listener.
fn bind(address: SocketAddr) -> io::Result<TcpListener> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
}
