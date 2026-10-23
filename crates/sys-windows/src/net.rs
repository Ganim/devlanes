//! Socket options std and socket2 do not offer.

use std::io;
use std::os::windows::io::AsRawSocket;

use windows_sys::Win32::Networking::WinSock::{
    SO_EXCLUSIVEADDRUSE, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSAGetLastError, getsockopt, setsockopt,
};

/// Sets `SO_EXCLUSIVEADDRUSE` on a socket that is not bound yet, so no other
/// socket can bind its port while it is open, not even on a more specific
/// address. A port another socket holds then fails `bind` with `AddrInUse` or
/// `PermissionDenied`. Never combine it with `SO_REUSEADDR`.
pub fn set_exclusive_address_use(socket: &impl AsRawSocket) -> io::Result<()> {
    let enable: i32 = 1;
    // SAFETY: the socket is borrowed, so it stays open for the call; `optval`
    // points at an i32 of the length passed, which setsockopt only reads.
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&raw const enable).cast(),
            size_of::<i32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        return Err(last_socket_error());
    }
    Ok(())
}

/// Whether `SO_EXCLUSIVEADDRUSE` is set on `socket`.
pub fn exclusive_address_use(socket: &impl AsRawSocket) -> io::Result<bool> {
    let mut value: i32 = 0;
    let mut length = size_of::<i32>() as i32;
    // SAFETY: the socket is borrowed, so it stays open for the call; `optval`
    // and `optlen` point at writable values of the sizes passed.
    let result = unsafe {
        getsockopt(
            socket.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&raw mut value).cast(),
            &raw mut length,
        )
    };
    if result == SOCKET_ERROR {
        return Err(last_socket_error());
    }
    Ok(value != 0)
}

/// This thread's last Winsock error, which uses Win32 error numbers.
fn last_socket_error() -> io::Error {
    // SAFETY: no arguments; reads this thread's last Winsock error.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}
