//! Transport-independent remote protocol between a client daemon and a box.
//!
//! A [`Transport`] yields one duplex byte stream. yamux multiplexes it, and only
//! the client opens streams:
//!
//! - one control stream: protocol check, registry snapshot, registry events and
//!   pings (every 10 s, dead after 20 s without a pong);
//! - one stream per forwarded daemon request ([`Session::call`]);
//! - data streams to registered endpoints ([`Session::open`]);
//! - sync subscriptions ([`Session::subscribe_sync`]).
//!
//! The box side is [`serve`], driven by injected [`BoxServices`]. [`Link`]
//! reconnects with backoff and keeps one [`ClientRegistry`] across sessions.
//! [`memory`] provides an in-process transport with fault injection for tests.

pub mod memory;
mod mux;
mod reconnect;
mod registry;
mod rpc;
mod server;
mod session;
mod sync;
mod transport;
mod wire;

pub use mux::{MuxError, MuxStream};
pub use reconnect::{Backoff, Link, LinkError, LinkState};
pub use registry::{
    AuthorizedTarget, ClientRegistry, HostHeader, HostInfo, LaneInfo, LaneKey, PortInfo, Registry,
    RegistryEvent, RegistryFeed, RegistrySnapshot, RegistrySource, RegistryUpdate, SharedRegistry,
    UnitInfo, UnitKey, UnitKind,
};
pub use rpc::{
    RpcCall, RpcCallError, RpcError, RpcHandler, RpcInput, RpcReceiver, RpcReplyStream, RpcSender,
};
pub use server::{BoxServices, Dialer, LoopbackDialer, ServeError, ServeOptions, serve};
pub use session::{
    DataStream, OpenError, Session, SessionEnd, SessionError, SessionOptions, Timing,
};
pub use sync::{
    NoSync, SyncError, SyncFeed, SyncMessage, SyncSource, SyncStreamError, SyncSubscription,
};
pub use transport::{BoxDuplex, BoxError, Duplex, Joined, Transport, TransportError, stdio};
pub use wire::{DialTarget, PROTOCOL_VERSION, Prefixed, RejectReason, WireError};
