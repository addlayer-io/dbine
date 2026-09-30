//! DBine cloud backup (docs/sincronizacion.md): the whole IDE state
//! (connections, folders, saved queries, preferences and, encrypted with
//! the rest, the connections' passwords) as one end-to-end encrypted file in
//! the user's own storage: Google Drive's or OneDrive's private app folder,
//! or any folder on disk. AddLayer runs no server and never sees it.

pub mod crypto;
pub mod engine;
pub mod folder;
pub mod gdrive;
pub mod oauth;
pub mod onedrive;
pub mod provider;

pub use engine::{Backup, SecretsIo, SyncAction, SyncEngine};
pub use provider::{CloudStore, ProviderKind, RemoteMeta};

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("la frase clave no es correcta (o el backup está dañado)")]
    WrongPassphrase,
    /// The account's authorization is gone (revoked, expired): sign in again.
    #[error("la sesión con {0} venció o fue revocada: volvé a conectar la cuenta")]
    AuthRequired(String),
    #[error("{0}")]
    Remote(String),
    #[error("{0}")]
    Local(String),
    #[error("{0}")]
    Format(String),
    #[error("se canceló")]
    Cancelled,
}

impl From<serde_json::Error> for SyncError {
    fn from(e: serde_json::Error) -> Self {
        SyncError::Format(e.to_string())
    }
}

impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        SyncError::Local(e.to_string())
    }
}

impl From<dbine_core::Error> for SyncError {
    fn from(e: dbine_core::Error) -> Self {
        SyncError::Local(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SyncError>;

/// Where the backup lives in the provider (its app folder) or the folder.
pub const BACKUP_FILE: &str = "dbine-backup.json";
/// The backup that was there before this machine overwrote another
/// machine's changes.
pub const PREVIOUS_FILE: &str = "dbine-backup.previous.json";
