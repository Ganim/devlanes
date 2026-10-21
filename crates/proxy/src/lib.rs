//! Streaming HTTP/TLS reverse proxy with atomically replaceable routing snapshots.
//!
//! ```
//! use devlanes_proxy::{Proxy, ProxyConfig, Route, RouteTable, Upstream};
//! let routes = RouteTable::new(vec![Route::new(
//!     "app.example.localhost", Upstream::Local(3000),
//! )])?;
//! let proxy = Proxy::new(ProxyConfig::default(), routes);
//! // The daemon publishes a replacement after a lane changes state.
//! proxy.replace_routes(RouteTable::default());
//! # Ok::<(), devlanes_proxy::RouteError>(())
//! ```

mod activity;
mod health;
mod listener;
mod pool;
mod routes;
mod server;

pub use health::ListenerHealth;
pub use listener::{BoundListeners, bind_listeners};
pub use routes::{HostHeader, Rejected, Route, RouteError, RouteState, RouteTable, Upstream};
pub use server::{Proxy, ProxyConfig, ProxyLimits, is_loopback};
