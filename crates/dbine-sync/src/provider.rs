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

/// What a cloud call was doing when it failed: each has its own whole
/// message, so the UI translates it as a sentence.
#[derive(Clone, Copy)]
pub(crate) enum HttpOp {
    ReadAccount,
    FindBackup,
    DownloadBackup,
    UploadBackup,
    DeleteBackup,
}

/// A readable message for a failed provider response.
pub(crate) async fn http_error(
    provider: ProviderKind,
    what: HttpOp,
    resp: reqwest::Response,
) -> crate::SyncError {
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
    let (label, code) = (provider.label(), status.as_u16());
    crate::SyncError::Remote(match what {
        HttpOp::ReadAccount => format!("{label}: no se pudo leer la cuenta (HTTP {code}): {msg}"),
        HttpOp::FindBackup => format!("{label}: no se pudo buscar el backup (HTTP {code}): {msg}"),
        HttpOp::DownloadBackup => {
            format!("{label}: no se pudo descargar el backup (HTTP {code}): {msg}")
        }
        HttpOp::UploadBackup => format!("{label}: no se pudo subir el backup (HTTP {code}): {msg}"),
        HttpOp::DeleteBackup => {
            format!("{label}: no se pudo borrar el backup (HTTP {code}): {msg}")
        }
    })
}

pub(crate) fn net_error(provider: ProviderKind, e: reqwest::Error) -> crate::SyncError {
    crate::SyncError::Remote(format!("no se pudo conectar con {}: {e}", provider.label()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyncError;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A response with `status` and `body` from a local server.
    async fn response(status: &str, body: &str) -> reqwest::Response {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(reply.as_bytes()).await;
        });
        reqwest::get(format!("http://{addr}/")).await.unwrap()
    }

    fn remote(e: SyncError) -> String {
        match e {
            SyncError::Remote(m) => m,
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_401_asks_to_sign_in_again() {
        let e = http_error(
            ProviderKind::Onedrive,
            HttpOp::ReadAccount,
            response("401 Unauthorized", "{}").await,
        )
        .await;
        assert!(
            matches!(&e, SyncError::AuthRequired(p) if p == "OneDrive"),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn the_providers_message_is_kept_short() {
        // Google's shape.
        let e = http_error(
            ProviderKind::GoogleDrive,
            HttpOp::UploadBackup,
            response("403 Forbidden", r#"{"error":{"message":"quota exceeded"}}"#).await,
        )
        .await;
        assert_eq!(
            remote(e),
            "Google Drive: no se pudo subir el backup (HTTP 403): quota exceeded"
        );
        // OAuth's.
        let e = http_error(
            ProviderKind::Onedrive,
            HttpOp::FindBackup,
            response(
                "400 Bad Request",
                r#"{"error":"invalid_grant","error_description":"expired"}"#,
            )
            .await,
        )
        .await;
        assert_eq!(
            remote(e),
            "OneDrive: no se pudo buscar el backup (HTTP 400): expired"
        );
        let e = http_error(
            ProviderKind::Onedrive,
            HttpOp::DeleteBackup,
            response("409 Conflict", r#"{"error":"locked"}"#).await,
        )
        .await;
        assert_eq!(
            remote(e),
            "OneDrive: no se pudo borrar el backup (HTTP 409): locked"
        );
        // Not JSON: the first 200 characters.
        let long = "x".repeat(500);
        let e = http_error(
            ProviderKind::GoogleDrive,
            HttpOp::DownloadBackup,
            response("502 Bad Gateway", &long).await,
        )
        .await;
        let m = remote(e);
        assert!(
            m.starts_with("Google Drive: no se pudo descargar el backup (HTTP 502): xxx"),
            "{m}"
        );
        assert_eq!(m.matches('x').count(), 200);
        let e = http_error(
            ProviderKind::GoogleDrive,
            HttpOp::ReadAccount,
            response("500 Internal Server Error", "").await,
        )
        .await;
        assert_eq!(
            remote(e),
            "Google Drive: no se pudo leer la cuenta (HTTP 500): "
        );
    }

    #[tokio::test]
    async fn an_unreachable_provider() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let err = reqwest::get(format!("http://{addr}/")).await.unwrap_err();
        let m = remote(net_error(ProviderKind::Folder, err));
        assert!(m.starts_with("no se pudo conectar con la carpeta: "), "{m}");
    }

    #[test]
    fn kinds_are_named_and_serialized() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::GoogleDrive).unwrap(),
            "\"google_drive\""
        );
        assert_eq!(
            serde_json::from_str::<ProviderKind>("\"onedrive\"").unwrap(),
            ProviderKind::Onedrive
        );
        assert_eq!(ProviderKind::Folder.label(), "la carpeta");
    }
}
