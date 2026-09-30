//! DBine core: the local state store (saved connections and queries) and
//! keychain secrets. Drivers live in their own crates (see `dbine-driver`).

pub mod cache;
pub mod conn_import;
pub mod export;
pub mod import;
pub mod secrets;
pub mod state;

pub use dbine_driver::{Error, Result};
pub use cache::ExplorerCache;
pub use state::{BackupCopy, ConnectionFolder, HistoryEntry, LibraryScript, SavedConnection, SavedMigration, SavedQuery, StateSnapshot, StateStore};
