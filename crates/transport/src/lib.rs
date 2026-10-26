//! Transport-independent remote protocol between a client daemon and a box.
//!
//! A [`Transport`] yields one duplex byte stream. On the box, `devlanes tunnel
//! --stdio` first writes [`TUNNEL_PREAMBLE`]; transports that run it read up to
//! that line with [`read_preamble`]. yamux then multiplexes the stream, and only
//! the client opens streams:
//!
//! - one control stream: protocol check, features, client identity, box info,
//!   registry snapshot, registry events and pings (every 10 s, dead after 20 s
//!   without a pong);
//! - data streams to registered endpoints ([`Session::open`]);
//! - sync subscriptions ([`Session::subscribe_sync`]).
//!
//! [`PROTOCOL_VERSION`] must match exactly on both ends and changes only for
//! breaking changes. Anything additive is a named feature instead: each end
//! lists the features it implements in the handshake ([`FEATURES`]), and a side
//! uses a feature only when the other side lists it too
//! ([`Session::has_feature`], [`ClientInfo::features`]).
//!
//! The box side is [`serve`], driven by injected [`BoxServices`]. [`Link`]
//! reconnects with backoff and keeps one [`ClientRegistry`] across sessions.
//! [`memory`] provides an in-process transport with fault injection for tests.

pub mod memory;
mod mux;
mod preamble;
mod reconnect;
mod registry;
mod server;
mod session;
mod splice;
mod sync;
mod transport;
mod wire;

pub use mux::{MuxError, MuxStream};
pub use preamble::{PREAMBLE_LIMIT, Preamble, PreambleError, TUNNEL_PREAMBLE, read_preamble};
pub use reconnect::{Backoff, Link, LinkError, LinkState};
pub use registry::{
    AuthorizedTarget, ClientRegistry, HostHeader, HostInfo, LaneInfo, LaneKey, PortInfo, Registry,
    RegistryEvent, RegistryFeed, RegistrySnapshot, RegistrySource, RegistryUpdate, SharedRegistry,
    UnitInfo, UnitKey, UnitKind, serving,
};
pub use server::{
    BoxServices, ClientInfo, Dialer, LoopbackDialer, ServeError, ServeOptions, serve,
};
pub use session::{
    DataStream, OpenError, Session, SessionEnd, SessionError, SessionOptions, Timing,
};
pub use splice::{HALF_CLOSE_IDLE, splice};
pub use sync::{
    NoSync, SyncError, SyncFeed, SyncInput, SyncInputEvent, SyncMessage, SyncSource,
    SyncStreamError, SyncSubscription,
};
pub use transport::{
    BoxDuplex, BoxError, Duplex, EXIT_DAEMON_STOPPED, Joined, Transport, TransportError, stdio,
};
pub use wire::{
    BoxInfo, DialTarget, FEATURES, PROTOCOL_VERSION, Prefixed, RejectReason, WireError,
};
