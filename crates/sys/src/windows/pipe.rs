//! Named-pipe endpoints that only the current user can open, and checks of who is
//! on the other end.

use std::ffi::OsStr;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{RevertToSelf, TOKEN_QUERY};
use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, ImpersonateNamedPipeClient};
use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

use super::security::SecurityAttributes;
use super::token::{self, Sid};

/// Creates one instance of the pipe `name`, readable and writable only by the
/// current user and refusing remote clients.
///
/// `first` adds `FILE_FLAG_FIRST_PIPE_INSTANCE`: creation then fails when any
/// process already created the name, so another account cannot squat on it
/// before the daemon starts. Later instances of the same server pass `false`.
/// Must run inside a tokio runtime with I/O enabled.
pub fn create_server(name: &OsStr, first: bool) -> io::Result<NamedPipeServer> {
    let mut attributes = SecurityAttributes::current_user_only()?;
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .access_inbound(true)
        .access_outbound(true);
    // SAFETY: `attributes` points at a valid SECURITY_ATTRIBUTES whose descriptor
    // lives until it is dropped after this call; the pipe copies it.
    unsafe { options.create_with_security_attributes_raw(name, attributes.as_mut_ptr().cast()) }
}

/// Whether the client connected to `server` runs as the current user.
///
/// Windows only lets a server impersonate a client after reading from the pipe, so
/// call this after the first message. Impersonation is per thread; it starts and
/// ends inside this call with no await in between.
pub fn client_is_current_user(server: &NamedPipeServer) -> io::Result<bool> {
    let me = token::current_user()?;
    Ok(client_user(server.as_raw_handle())? == me)
}

/// Whether the process serving `client` runs as the current user. Catches a pipe
/// name created first by another account.
pub fn server_is_current_user(client: &NamedPipeClient) -> io::Result<bool> {
    Ok(server_user(client)? == token::current_user()?)
}

/// The account that serves `client`'s pipe.
pub fn server_user(client: &NamedPipeClient) -> io::Result<Sid> {
    let mut pid = 0;
    // SAFETY: the handle belongs to `client`, which outlives the call.
    if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle(), &raw mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    token::process_user(pid)
}

fn client_user(pipe: HANDLE) -> io::Result<Sid> {
    // SAFETY: `pipe` is a connected server end owned by the caller.
    if unsafe { ImpersonateNamedPipeClient(pipe) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut thread_token: HANDLE = ptr::null_mut();
    // SAFETY: opens this thread's impersonation token; open-as-self so the query
    // uses the daemon's own rights rather than the client's.
    let opened =
        unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &raw mut thread_token) };
    let open_error = (opened == 0).then(io::Error::last_os_error);
    // SAFETY: ends the impersonation started above; must run on every path.
    let reverted = unsafe { RevertToSelf() };
    if reverted == 0 {
        // Continuing would leave this worker thread acting as the client.
        std::process::abort();
    }
    if let Some(error) = open_error {
        return Err(error);
    }
    token::token_user(&token::owned(thread_token)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;

    fn name() -> String {
        format!(
            r"\\.\pipe\devlanes-sys-test-{}-{}",
            std::process::id(),
            line!()
        )
    }

    /// Both ends recognise each other as the same user; a second first instance of
    /// the same name is refused.
    #[tokio::test]
    async fn same_user_both_ways_and_no_squatting() -> io::Result<()> {
        let name = name();
        let server = create_server(OsStr::new(&name), true)?;
        assert!(
            create_server(OsStr::new(&name), true).is_err(),
            "first instance is unique"
        );
        let mut client = ClientOptions::new().open(&name)?;
        let mut server = {
            server.connect().await?;
            server
        };
        assert!(server_is_current_user(&client)?);
        client.write_all(b"hi").await?;
        let mut buffer = [0; 2];
        server.read_exact(&mut buffer).await?;
        assert!(client_is_current_user(&server)?);
        Ok(())
    }
}
