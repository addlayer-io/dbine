//! Drivers as separate processes, downloaded on first use (docs/drivers-bajo-demanda.md).
//!
//! - [`proto`]: the messages and their framing.
//! - [`host`]: a driver crate serving the app over stdin / stdout
//!   (`dbine-plugin-host`).
//! - [`install`]: downloading a driver's host on first use.
//! - [`remote`]: the app side, `RemoteDriver` / `RemoteSession`, which
//!   implement the driver contract by forwarding every call to the host.

pub mod host;
pub mod install;
pub mod proto;
pub mod remote;

pub use proto::DriverMeta;
pub use remote::{Host, Launcher, RemoteDriver};

/// The plugins' manifest: what every downloadable driver says about itself.
pub fn parse_manifest(json: &str) -> serde_json::Result<Vec<DriverMeta>> {
    serde_json::from_str(json)
}
