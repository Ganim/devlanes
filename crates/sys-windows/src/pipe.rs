//! Named pipe servers with an explicit security descriptor, and the owner
//! check a client runs before trusting a server.
//!
//! The server and client types are tokio's, so these must run inside a tokio
//! runtime with I/O enabled. Nothing here blocks.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::io::AsHandle;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

use crate::security::{SecurityAttributes, SecurityDescriptor, kernel_object_owner};
use crate::token::{self, Sid};

/// `ERROR_PIPE_BUSY`: every instance of the pipe is serving another client.
/// A client waits and opens again.
pub const ERROR_PIPE_BUSY: i32 = 231;

/// `ERROR_NO_DATA`: a client connected and closed before the server accepted
/// it. A server creates another instance and keeps listening.
pub const ERROR_NO_DATA: i32 = 232;

/// `ERROR_FILE_NOT_FOUND`: no server has created the pipe.
pub const ERROR_FILE_NOT_FOUND: i32 = 2;

/// The descriptor of the daemon's pipe: owned by the current user, full
/// access for that user only, and a medium mandatory label that keeps
/// lower-integrity processes of the same user (sandboxes, low-integrity
/// browsers) from reading or writing it. The explicit owner keeps the pipe
/// the user's even when the daemon runs elevated, so the client's owner check
/// still passes, and the medium label lets unelevated clients in.
pub fn current_user_descriptor() -> io::Result<SecurityDescriptor> {
    let user = token::current_user()?;
    SecurityDescriptor::from_sddl(&format!("O:{user}D:P(A;;GA;;;{user})S:(ML;;NWNR;;;ME)"))
}

/// Creates instances of one pipe name with one descriptor, built once so a
/// server does not redo it on every accept.
pub struct PipeFactory {
    name: OsString,
    attributes: SecurityAttributes,
}

impl PipeFactory {
    /// Prepares instances of the pipe `name` (`\\.\pipe\...`) with
    /// `descriptor`, which must grant this process `FILE_CREATE_PIPE_INSTANCE`
    /// (part of `GA`) or only the first instance can be created.
    pub fn new(name: impl AsRef<OsStr>, descriptor: SecurityDescriptor) -> Self {
        Self {
            name: name.as_ref().to_owned(),
            attributes: SecurityAttributes::new(descriptor),
        }
    }

    /// Prepares instances of `name` with [`current_user_descriptor`].
    pub fn current_user(name: impl AsRef<OsStr>) -> io::Result<Self> {
        Ok(Self::new(name, current_user_descriptor()?))
    }

    /// Creates one server instance for reading and writing that refuses
    /// remote clients.
    ///
    /// `first` adds `FILE_FLAG_FIRST_PIPE_INSTANCE`: creation then fails with
    /// access denied when any process already created the name, so another
    /// process cannot squat on it before the server starts. Later instances
    /// of the same server pass `false`.
    pub fn create(&self, first: bool) -> io::Result<NamedPipeServer> {
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .access_inbound(true)
            .access_outbound(true);
        // SAFETY: the attributes and their descriptor live in `self` past this
        // call; CreateNamedPipeW only reads them and copies the descriptor.
        unsafe {
            options.create_with_security_attributes_raw(
                &self.name,
                self.attributes.as_ptr().cast_mut().cast(),
            )
        }
    }
}

/// The owner of the pipe `client` is connected to. Only an account itself (or
/// an administrator) can create an object owned by that account, so a client
/// that finds its own user here talks to a server of that user. The handle
/// needs `READ_CONTROL`, which tokio's client options include.
pub fn server_owner(client: &impl AsHandle) -> io::Result<Sid> {
    kernel_object_owner(&client.as_handle())?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the pipe has no owner"))
}
