//! Lane resolution: turns a project config plus one lane's facts into launch
//! plans, ports, hostnames and environments. Nothing here spawns processes;
//! contributors, secrets providers and script runners are called through the
//! `devlanes-core` traits and may run helper commands of their own.
//!
//! Typical daemon flow for one lane start:
//!
//! 1. [`port_needs`] to size and fill the lane's port block (with `devlanes-core`'s
//!    allocator and the state store).
//! 2. [`select`] for the requested profile and selectors.
//! 3. [`resolve_lane`] with the block, offsets, infra endpoints and the client env.
//!
//! [`lane_layout`] is the synchronous subset (ports, URLs, routes) for idle lanes,
//! routing tables and the `lanes.main.svc` namespace of lane 0.

mod error;
mod facts;
mod layout;
mod model;
mod plan;
mod ports;
mod select;
mod shell;
mod util;

pub use error::ResolveError;
pub use facts::{LaneFacts, branch_label, lane_name};
pub use layout::{fixed_block_start, https_url, lane_layout, tag_colors};
pub use model::*;
pub use plan::{container_ready, detect_platform, resolve_lane};
pub use ports::{PortNeeds, port_needs, port_unit, reserved_offsets};
pub use select::{ProfileSummary, Selection, profile_summaries, select, select_profiles};
pub use shell::shell_warnings;
pub use util::{cmd_quote, public_url_env_key, shell_quote};
