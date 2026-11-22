//! DBine core: the local state store (saved connections and queries) and
//! keychain secrets. Drivers live in their own crates (see `dbine-driver`).

pub mod cache;
pub mod conn_import;
pub mod export;
pub mod file_grants;
pub mod import;
pub mod lineage_link;
pub mod purge;
pub mod secret_accounts;
pub mod secret_sources;
pub mod secrets;
pub mod ssh_profiles;
pub mod state;
pub mod tasks;

pub use cache::ExplorerCache;
pub use dbine_driver::{Error, Result};
pub use lineage_link::{
    match_connections, new_connection_from_source, source_key_of_config, source_key_of_external,
    Match, SourceKey,
};
pub use ssh_profiles::SshProfile;
pub use state::{
    BackupCopy, ConnectionFolder, ExplorerItem, HistoryEntry, LibraryScript, Project,
    ProjectBinding, ProjectTarget, QueryVersion, SavedConnection, SavedContext, SavedMigration,
    SavedQuery, StateChange, StateSnapshot, StateStore,
};
