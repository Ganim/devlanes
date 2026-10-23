//! Socket options, on loopback sockets only so no firewall prompt appears.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use devlanes_sys_windows::net::{exclusive_address_use, set_exclusive_address_use};
use tokio::net::TcpSocket;

/// The option reads back where it was set, and binding the same address and
/// port again fails, even with `SO_REUSEADDR`.
#[tokio::test]
async fn exclusive_listeners_cannot_be_shared() -> io::Result<()> {
    let first = TcpSocket::new_v4()?;
    set_exclusive_address_use(&first)?;
    first.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    let first = first.listen(16)?;
    assert!(exclusive_address_use(&first)?);
    assert!(!exclusive_address_use(&TcpSocket::new_v4()?)?);
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
