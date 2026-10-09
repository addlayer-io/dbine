//! Drivers as separate processes, downloaded on first use (docs/on-demand-drivers.md).
//!
//! - [`proto`]: the messages and their framing.
//! - [`host`]: a driver crate serving the app over stdin / stdout
//!   (`dbine-plugin-host`).
//! - [`install`]: downloading a driver's host on first use.
//! - [`index`]: the signed index of published hosts, and which version of
//!   each driver this app runs.
//! - [`state`]: what this machine knows about its downloaded drivers.
//! - [`updater`]: which version of each driver runs, and getting new ones.
//! - [`remote`]: the app side, `RemoteDriver` / `RemoteSession`, which
//!   implement the driver contract by forwarding every call to the host.

pub mod host;
pub mod index;
pub mod install;
pub mod proto;
pub mod remote;
pub mod state;
pub mod updater;

pub use proto::DriverMeta;
pub use remote::{Host, HostSource, Launcher, RemoteDriver, Target};

/// The plugins' manifest: what every downloadable driver says about itself.
pub fn parse_manifest(json: &str) -> serde_json::Result<Vec<DriverMeta>> {
    serde_json::from_str(json)
}
