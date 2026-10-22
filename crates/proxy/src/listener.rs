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
    /// `None` when the host cannot bind IPv6 at all, for example with IPv6 disabled.
    pub ipv6: Option<TcpListener>,
    pub port: u16,
    /// A human-readable diagnostic for CLI/doctor when the requested port was unavailable.
    pub fallback_reason: Option<String>,
    /// Why the IPv6 listener is missing, for doctor output.
    pub ipv6_error: Option<String>,
}

impl BoundListeners {
    /// Every bound socket, for callers that serve each one through `Proxy`.
    pub fn listeners(self) -> impl Iterator<Item = TcpListener> {
        std::iter::once(self.ipv4).chain(self.ipv6)
    }
}

/// Binds both families without privileges, falling back only for denied/occupied ports.
///
/// macOS privileged ports require wildcard binds, so callers must serve these through
/// `Proxy`, whose accept loop enforces the configured LAN policy. This must run inside
/// a Tokio runtime. Port zero is supported for isolated tests.
pub fn bind_listeners(port: u16, fallback_port: u16, lan: bool) -> io::Result<BoundListeners> {
    bind_listeners_with(port, fallback_port, lan, bind)
}

/// `bind_listeners` with an injectable socket binder, so tests can simulate missing IPv6.
fn bind_listeners_with(
    port: u16,
    fallback_port: u16,
    lan: bool,
    bind: impl Fn(SocketAddr) -> io::Result<TcpListener>,
) -> io::Result<BoundListeners> {
    match bind_pair(port, lan, &bind) {
        Ok(listeners) => Ok(listeners),
        Err(error) if port != fallback_port && port_unavailable(&error) => {
            let mut listeners = bind_pair(fallback_port, lan, &bind)?;
            let reason = if error.kind() == io::ErrorKind::AddrInUse {
                format!(
                    "Port {port} is in use by another process; using {}.",
                    listeners.port
                )
            } else {
                format!(
                    "Could not bind port {port}: {error}; using {}. {DENIED_HINT}",
                    listeners.port
                )
            };
            tracing::warn!("{reason}");
            listeners.fallback_reason = Some(reason);
            Ok(listeners)
        }
        Err(error) => Err(error),
    }
}

/// What usually denies a port, for the fallback message.
#[cfg(unix)]
const DENIED_HINT: &str =
    "On Linux, configure CAP_NET_BIND_SERVICE or ip_unprivileged_port_start for low ports.";

/// Windows needs no privilege for low ports. Access denied means another program
/// holds the port exclusively (port 80 is often held by http.sys, owner PID 4),
/// or Windows reserved it (Hyper-V, WSL or Docker excluded ranges).
#[cfg(windows)]
const DENIED_HINT: &str = "On Windows, another program may hold it: port 80 is often held by http.sys (PID 4, System), listed by `netsh http show servicestate`. Reserved ports are listed by `netsh int ipv4 show excludedportrange protocol=tcp`.";

/// Errors that mean this port is taken or privileged, as opposed to the host lacking a family.
/// On Windows `PermissionDenied` (WSAEACCES) also covers excluded port ranges and
/// ports another socket holds exclusively.
fn port_unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::AddrInUse
    )
}

/// A failed second bind drops the first socket before any fallback attempt.
///
/// IPv6 failures other than port errors leave the proxy IPv4-only instead of failing it.
fn bind_pair(
    port: u16,
    lan: bool,
    bind: &impl Fn(SocketAddr) -> io::Result<TcpListener>,
) -> io::Result<BoundListeners> {
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
    let (ipv6, ipv6_error) = match bind(SocketAddr::new(ipv6.into(), port)) {
        Ok(listener) => (Some(listener), None),
        Err(error) if port_unavailable(&error) => return Err(error),
        Err(error) => {
            let reason = format!("IPv6 listener unavailable, serving IPv4 only: {error}");
            tracing::warn!("{reason}");
            (None, Some(reason))
        }
    };
    Ok(BoundListeners {
        ipv4,
        ipv6,
        port,
        fallback_reason: None,
        ipv6_error,
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
    exclusive(&socket)?;
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
}

/// Unix has no such option; the listener keeps the default socket options.
#[cfg(unix)]
fn exclusive(_socket: &Socket) -> io::Result<()> {
    Ok(())
}

/// Windows lets another program bind a more specific address on the same port and
/// take connections, unless the listener claims the port exclusively.
#[cfg(windows)]
fn exclusive(socket: &Socket) -> io::Result<()> {
    devlanes_sys::windows::net::exclusive_address_use(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Binds IPv4 normally and fails every IPv6 bind with `kind`.
    fn without_ipv6(kind: io::ErrorKind) -> impl Fn(SocketAddr) -> io::Result<TcpListener> {
        move |address| {
            if address.is_ipv6() {
                Err(io::Error::new(kind, "simulated IPv6 failure"))
            } else {
                bind(address)
            }
        }
    }

    /// A host without IPv6 still gets a working IPv4 proxy, with the reason recorded.
    #[tokio::test]
    async fn missing_ipv6_degrades_to_ipv4() -> io::Result<()> {
        let bound =
            bind_listeners_with(0, 0, false, without_ipv6(io::ErrorKind::AddrNotAvailable))?;
        assert!(bound.ipv6.is_none());
        assert!(bound.ipv6_error.is_some());
        assert!(bound.fallback_reason.is_none());
        assert_eq!(bound.listeners().count(), 1);
        Ok(())
    }

    /// A configured port another process listens on falls back like a privileged one.
    #[tokio::test]
    async fn occupied_port_falls_back() -> io::Result<()> {
        let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = taken.local_addr()?.port();
        let bound = bind_listeners_with(port, 0, false, bind)?;
        assert_ne!(bound.port, port);
        let reason = bound.fallback_reason.unwrap_or_default();
        assert!(reason.contains("in use"), "{reason}");
        Ok(())
    }

    /// WSAEACCES (10013), what an excluded port range or an exclusively held port
    /// returns, makes the proxy fall back with the Windows hint.
    #[cfg(windows)]
    #[tokio::test]
    async fn access_denied_port_falls_back_with_the_windows_hint() -> io::Result<()> {
        assert!(port_unavailable(&io::Error::from_raw_os_error(10013)));
        let denied = |address: SocketAddr| {
            if address.port() == 80 {
                Err(io::Error::from_raw_os_error(10013))
            } else {
                bind(address)
            }
        };
        let bound = bind_listeners_with(80, 0, false, denied)?;
        assert_ne!(bound.port, 80);
        let reason = bound.fallback_reason.unwrap_or_default();
        assert!(
            reason.contains("http.sys") && reason.contains("excludedportrange"),
            "{reason}"
        );
        Ok(())
    }

    /// Every proxy listener claims its port exclusively.
    #[cfg(windows)]
    #[tokio::test]
    async fn listeners_hold_their_port_exclusively() -> io::Result<()> {
        for listener in bind_listeners_with(0, 0, false, bind)?.listeners() {
            assert!(devlanes_sys::windows::net::is_exclusive_address_use(
                &listener
            )?);
        }
        Ok(())
    }

    /// An occupied IPv6 port is a port problem and still fails the pair.
    #[tokio::test]
    async fn occupied_ipv6_port_still_fails() {
        let result = bind_listeners_with(0, 0, false, without_ipv6(io::ErrorKind::AddrInUse));
        assert!(result.is_err_and(|error| error.kind() == io::ErrorKind::AddrInUse));
    }
}
