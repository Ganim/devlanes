//! Socket options std and socket2 do not offer, and the TCP port ranges Windows
//! keeps out of reach.

use std::io;
use std::os::windows::io::{AsRawSocket, AsSocket};

use windows_sys::Win32::Networking::WinSock::{
    SO_EXCLUSIVEADDRUSE, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSAGetLastError, getsockopt, setsockopt,
};

/// Sets `SO_EXCLUSIVEADDRUSE` on a socket that is not bound yet, so no other
/// socket can bind its address and port while it is open. Meant for wildcard
/// binds: without it, another program could bind a more specific address on the
/// same port and take connections meant for this listener. On a specific
/// (loopback) bind it changes no later bind's outcome (Microsoft's
/// `SO_EXCLUSIVEADDRUSE` tables), and Microsoft documents that the port may not
/// be bound again until connections the listener accepted are gone (not seen on
/// Windows 11 loopback, where it rebinds). A port another socket holds fails
/// `bind` with `AddrInUse` or `PermissionDenied`. Never combine it with
/// `SO_REUSEADDR`.
pub fn exclusive_address_use(socket: &impl AsSocket) -> io::Result<()> {
    let enable: i32 = 1;
    let socket = socket.as_socket().as_raw_socket() as SOCKET;
    // SAFETY: the socket is borrowed, so it stays open for the call; `optval`
    // points at an i32 of the length passed, which setsockopt only reads.
    let result = unsafe {
        setsockopt(
            socket,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&raw const enable).cast(),
            size_of::<i32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        // SAFETY: no arguments; reads this thread's last Winsock error.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(())
}

/// Whether [`exclusive_address_use`] is in effect on `socket`, for tests of the
/// crates that bind listeners.
pub fn is_exclusive_address_use(socket: &impl AsSocket) -> io::Result<bool> {
    let mut value: i32 = 0;
    let mut length = size_of::<i32>() as i32;
    let socket = socket.as_socket().as_raw_socket() as SOCKET;
    // SAFETY: the socket is borrowed, so it stays open for the call; `optval` and
    // `optlen` point at writable values of the sizes passed.
    let result = unsafe {
        getsockopt(
            socket,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&raw mut value).cast(),
            &raw mut length,
        )
    };
    if result == SOCKET_ERROR {
        // SAFETY: no arguments; reads this thread's last Winsock error.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(value != 0)
}

/// The ranges in the output of `netsh interface ipv4|ipv6 show excludedportrange
/// protocol=tcp`, as inclusive `(start, end)` pairs. Hyper-V, WSL, Docker and
/// administrators reserve these; binding a port inside one fails with
/// `PermissionDenied` (WSAEACCES).
///
/// The rows are two port numbers, the second followed by `*` for administered
/// exclusions. The title and column headers are translated on non-English
/// Windows, so every line that does not start with two port numbers is skipped,
/// and so is a pair whose start lies after its end.
pub fn parse_excluded_ranges(text: &str) -> Vec<(u16, u16)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let start: u16 = fields.next()?.parse().ok()?;
            let end: u16 = fields.next()?.parse().ok()?;
            (start <= end).then_some((start, end))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};
    use tokio::net::{TcpListener, TcpSocket};

    /// `netsh int ipv4 show excludedportrange protocol=tcp` on English Windows 11.
    const ENGLISH: &str = "\r\nProtocol tcp Port Exclusion Ranges\r\n\r\nStart Port    End Port      \r\n----------    --------      \r\n     27339       27339      \r\n     49745       49844      \r\n     50000       50059     *\r\n\r\n* - Administered port exclusions.\r\n\r\n";

    /// The same with German headers, as on a localized installation.
    const GERMAN: &str = "\r\nProtokoll tcp Portausschlussbereiche\r\n\r\nStartport    Endport\r\n----------    --------\r\n      5357        5357\r\n     50000       50059     *\r\n\r\n* - Verwaltete Portausschl\u{fc}sse.\r\n";

    /// Rows are read whatever the language; headers, footers and nonsense rows
    /// are not.
    #[test]
    fn excluded_ranges_parse_numeric_rows_only() {
        assert_eq!(
            parse_excluded_ranges(ENGLISH),
            [(27339, 27339), (49745, 49844), (50000, 50059)]
        );
        assert_eq!(
            parse_excluded_ranges(GERMAN),
            [(5357, 5357), (50000, 50059)]
        );
        assert!(parse_excluded_ranges("").is_empty());
        assert!(parse_excluded_ranges("  70000 70001\n 9 8\n 12\n").is_empty());
    }

    /// An exclusive listener on loopback `port` (0 for any).
    fn exclusive(port: u16) -> io::Result<TcpListener> {
        let socket = TcpSocket::new_v4()?;
        exclusive_address_use(&socket)?;
        socket.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
        socket.listen(16)
    }

    /// The option reads back as set only where it was set, and binding the same
    /// address and port again fails, even with `SO_REUSEADDR`.
    #[tokio::test]
    async fn exclusive_listeners_cannot_be_shared() -> io::Result<()> {
        let first = exclusive(0)?;
        assert!(is_exclusive_address_use(&first)?);
        assert!(!is_exclusive_address_use(&TcpSocket::new_v4()?)?);
        let second = TcpSocket::new_v4()?;
        second.set_reuseaddr(true)?;
        let error = second
            .bind(first.local_addr()?)
            .expect_err("an exclusive port was shared");
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
            ),
            "{error}"
        );
        Ok(())
    }

    /// A restarted daemon binds its port again right away: a closed exclusive
    /// listener whose accepted connection was closed does not hold the port.
    #[tokio::test]
    async fn exclusive_port_rebinds_after_close() -> io::Result<()> {
        let listener = exclusive(0)?;
        let port = listener.local_addr()?.port();
        let client = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await?;
        let (server, _) = listener.accept().await?;
        drop(listener);
        drop(server);
        drop(client);
        exclusive(port)?;
        Ok(())
    }

    /// Winsock reports a reserved or exclusively held port as WSAEACCES (10013),
    /// which std maps to `PermissionDenied`; callers treat that as unavailable.
    #[test]
    fn access_denied_binds_are_permission_denied() {
        assert_eq!(
            io::Error::from_raw_os_error(10013).kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
