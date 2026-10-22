//! Named-pipe endpoints that only the current user can open, and checks of who is
//! on the other end.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr;

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{RevertToSelf, TOKEN_QUERY};
use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, ImpersonateNamedPipeClient};
use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

use super::security::{SecurityAttributes, SecurityDescriptor};
use super::token::{self, Sid};

/// Creates one instance of the pipe `name`, readable and writable only by the
/// current user and refusing remote clients.
///
/// `first` adds `FILE_FLAG_FIRST_PIPE_INSTANCE`: creation then fails when any
/// process already created the name, so another account cannot squat on it
/// before the daemon starts. Later instances of the same server pass `false`.
/// Must run inside a tokio runtime with I/O enabled.
pub fn create_server(name: &OsStr, first: bool) -> io::Result<NamedPipeServer> {
    PipeFactory::new(name)?.create(first)
}

/// A pipe name and the user-only security descriptor its instances get, built
/// once so a server does not redo the token and SDDL work on every accept.
pub struct PipeFactory {
    name: OsString,
    attributes: SecurityAttributes,
}

impl PipeFactory {
    /// Prepares instances of the pipe `name`.
    pub fn new(name: &OsStr) -> io::Result<Self> {
        Ok(Self {
            name: name.to_owned(),
            attributes: SecurityAttributes::current_user_only()?,
        })
    }

    /// Prepares instances of the pipe `name` with `descriptor` instead of the
    /// current-user-only one. The descriptor must grant this process
    /// `FILE_CREATE_PIPE_INSTANCE` (part of `GA`), or only the first instance can
    /// be created; clients should get [`CLIENT_RIGHTS`], which lacks it.
    pub fn with_descriptor(name: &OsStr, descriptor: SecurityDescriptor) -> Self {
        Self {
            name: name.to_owned(),
            attributes: SecurityAttributes::new(descriptor),
        }
    }

    /// Creates one instance, as [`create_server`] describes.
    pub fn create(&mut self, first: bool) -> io::Result<NamedPipeServer> {
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .access_inbound(true)
            .access_outbound(true);
        // SAFETY: the attributes and their descriptor live in `self` past this
        // call; the pipe copies the descriptor.
        unsafe {
            options.create_with_security_attributes_raw(
                &self.name,
                self.attributes.as_mut_ptr().cast(),
            )
        }
    }
}

/// Whether the client connected to `server` is as trusted as this process: the
/// same user, not sandboxed, at no lower integrity level.
///
/// Windows only lets a server impersonate a client after reading from the pipe, so
/// call this after the first message. Impersonation is per thread; it starts and
/// ends inside this call with no await in between.
pub fn client_is_current_user(server: &NamedPipeServer) -> io::Result<bool> {
    let own = token::current_facts()?;
    Ok(client_facts(server.as_raw_handle())?.as_trusted_as(&own))
}

/// Whether the process serving `client` is as trusted as this process. Catches a
/// pipe name created first by another account, or by a sandboxed or
/// lower-integrity process of the same user, before anything is sent to it.
pub fn server_is_current_user(client: &NamedPipeClient) -> io::Result<bool> {
    let own = token::current_facts()?;
    match server_facts(client) {
        Ok(facts) => Ok(facts.may_serve(&own)),
        // Another account's process refuses the query; that is the answer.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(false),
        Err(error) => Err(error),
    }
}

/// The account that serves `client`'s pipe.
pub fn server_user(client: &NamedPipeClient) -> io::Result<Sid> {
    Ok(server_facts(client)?.user)
}

/// Rights a client needs to talk over a pipe and check who serves it: read and
/// write data, read attributes, read the security descriptor, synchronize.
///
/// `GENERIC_WRITE` would also grant `FILE_APPEND_DATA`, which for pipes is
/// `FILE_CREATE_PIPE_INSTANCE`: a client holding it can create server instances
/// of the same pipe and answer the next client in the server's place. Grant this
/// mask instead and open with [`open_client`].
pub const CLIENT_RIGHTS: u32 = 0x0012_0083;

/// `ERROR_PIPE_BUSY`: every instance of the pipe is serving another client.
pub const ERROR_PIPE_BUSY: i32 = 231;

/// Opens the client end of the pipe `name` with exactly [`CLIENT_RIGHTS`], for
/// pipes whose access list grants clients no more than that (tokio's client
/// asks for `GENERIC_WRITE`). The server may identify the caller but not act as
/// it (`SECURITY_IDENTIFICATION`). Must run inside a tokio runtime with I/O
/// enabled. A busy pipe fails with [`ERROR_PIPE_BUSY`]; retrying is the caller's.
pub fn open_client(name: &OsStr) -> io::Result<NamedPipeClient> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, SECURITY_IDENTIFICATION};
    let file = std::fs::OpenOptions::new()
        .access_mode(CLIENT_RIGHTS)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(name)?;
    // SAFETY: the handle was just opened for overlapped I/O on a pipe and its
    // ownership moves into the client; tokio registers it with the runtime.
    unsafe { NamedPipeClient::from_raw_handle(file.into_raw_handle()) }
}

/// The owner of the pipe `client` is connected to, as a SID string. Only the
/// account itself (or an administrator) can create an object owned by an
/// account, so a pipe owned by `S-1-5-18` was created by a SYSTEM process.
/// `None` when the pipe has no owner.
pub fn server_owner(client: &NamedPipeClient) -> io::Result<Option<String>> {
    super::security::kernel_object_owner(client.as_raw_handle())
}

/// The process ID of the server end of `client`'s pipe.
pub fn server_process_id(client: &NamedPipeClient) -> io::Result<u32> {
    let mut pid = 0;
    // SAFETY: the handle belongs to `client`, which outlives the call.
    if unsafe { GetNamedPipeServerProcessId(client.as_raw_handle(), &raw mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Token facts (user, integrity, sandboxing) of the client connected to
/// `server`. As with [`client_is_current_user`], call it after the first read;
/// the impersonation starts and ends inside this call.
pub fn client_token_facts(server: &NamedPipeServer) -> io::Result<token::TokenFacts> {
    client_facts(server.as_raw_handle())
}

/// Token facts of the process serving `client`'s pipe.
fn server_facts(client: &NamedPipeClient) -> io::Result<token::TokenFacts> {
    token::process_facts(server_process_id(client)?)
}

/// Token facts of the client on `pipe`, read by impersonating it for the moment
/// it takes to open the thread token.
fn client_facts(pipe: HANDLE) -> io::Result<token::TokenFacts> {
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
    token::token_facts(&token::owned(thread_token)?)
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

    /// A pipe with an explicit descriptor: the client opens it with only
    /// `CLIENT_RIGHTS`, reads its owner, and the server reads the client's token.
    /// A client granted only `CLIENT_RIGHTS` cannot create another server
    /// instance.
    #[tokio::test]
    async fn explicit_descriptor_and_limited_clients() -> io::Result<()> {
        let name = name();
        let user = token::current_user()?.to_sddl()?;
        // The owner may create instances; the client ACE is what the hosts
        // service grants its user. Same account here, so the owner ACE carries it.
        let descriptor = SecurityDescriptor::from_sddl(&format!(
            "O:{user}D:P(A;;GA;;;{user})(A;;{CLIENT_RIGHTS:#x};;;S-1-5-18)"
        ))?;
        let mut factory = PipeFactory::with_descriptor(OsStr::new(&name), descriptor);
        let server = factory.create(true)?;
        let _second = factory.create(false)?;
        let mut client = open_client(OsStr::new(&name))?;
        server.connect().await?;
        let mut server = server;
        assert_eq!(server_owner(&client)?.as_deref(), Some(user.as_str()));
        assert_eq!(server_process_id(&client)?, std::process::id());
        client.write_all(b"hi").await?;
        let mut buffer = [0; 2];
        server.read_exact(&mut buffer).await?;
        let facts = client_token_facts(&server)?;
        assert_eq!(facts.user.to_sddl()?, user);

        // A pipe that grants this user only CLIENT_RIGHTS: opening as a client
        // works, creating a server instance is refused.
        let limited = format!("{name}-limited");
        let descriptor = SecurityDescriptor::from_sddl(&format!(
            "D:P(A;;GA;;;S-1-5-18)(A;;{CLIENT_RIGHTS:#x};;;{user})"
        ))?;
        let mut factory = PipeFactory::with_descriptor(OsStr::new(&limited), descriptor);
        let _first = factory.create(true)?;
        let _client = open_client(OsStr::new(&limited))?;
        let error = factory.create(false).err().map(|error| error.kind());
        assert_eq!(error, Some(io::ErrorKind::PermissionDenied));
        Ok(())
    }
}
