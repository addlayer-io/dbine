use crate::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    GoogleDrive,
    Onedrive,
    Folder,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::GoogleDrive => "Google Drive",
            ProviderKind::Onedrive => "OneDrive",
            ProviderKind::Folder => "la carpeta",
        }
    }
}

/// A stored file's version, as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteMeta {
    /// Changes whenever the content does (Drive's `version`, OneDrive's
    /// `cTag`, a file's mtime + size).
    pub revision: String,
    pub modified: Option<String>,
    pub size: Option<u64>,
}

/// Where the backup is kept: a few files by name in a private place.
#[async_trait]
pub trait CloudStore: Send + Sync {
    fn kind(&self) -> ProviderKind;
    /// Who's signed in (email or name), or the folder's path.
    async fn account(&self) -> Result<String>;
    async fn stat(&self, name: &str) -> Result<Option<RemoteMeta>>;
    async fn download(&self, name: &str) -> Result<Option<Vec<u8>>>;
    async fn upload(&self, name: &str, bytes: Vec<u8>) -> Result<RemoteMeta>;
    async fn delete(&self, name: &str) -> Result<()>;
}

/// A readable message for a failed provider response.
pub(crate) async fn http_error(provider: ProviderKind, what: &str, resp: reqwest::Response) -> crate::SyncError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return crate::SyncError::AuthRequired(provider.label().into());
    }
    // Error bodies are JSON with a message somewhere; keep it short.
    let msg = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.pointer("/error_description"))
                .or_else(|| v.pointer("/error"))
                .and_then(|m| m.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    crate::SyncError::Remote(format!("{} ({what}): HTTP {} {msg}", provider.label(), status.as_u16()))
}

pub(crate) fn net_error(provider: ProviderKind, e: reqwest::Error) -> crate::SyncError {
    crate::SyncError::Remote(format!("no se pudo conectar con {}: {e}", provider.label()))
}
