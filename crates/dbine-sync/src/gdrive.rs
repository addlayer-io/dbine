//! Google Drive's `appDataFolder`: a hidden folder only DBine can see (the
//! `drive.appdata` scope gives no access to the user's other files). It
//! counts against the account's quota and is deleted if the user removes
//! the app's access (Drive › Settings › Manage apps).

use crate::oauth::{authed, TokenSource};
use crate::provider::{http_error, net_error, CloudStore, ProviderKind, RemoteMeta};
use crate::Result;
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

const KIND: ProviderKind = ProviderKind::GoogleDrive;

pub struct GoogleDrive {
    auth: Arc<TokenSource>,
    /// `https://www.googleapis.com` (a mock server in tests).
    api: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveFile {
    id: String,
    #[serde(default)]
    modified_time: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

impl DriveFile {
    fn meta(&self) -> RemoteMeta {
        RemoteMeta {
            revision: self.version.clone().or_else(|| self.modified_time.clone()).unwrap_or_default(),
            modified: self.modified_time.clone(),
            size: self.size.as_deref().and_then(|s| s.parse().ok()),
        }
    }
}

const FIELDS: &str = "id,modifiedTime,size,version";

impl GoogleDrive {
    pub fn new(auth: Arc<TokenSource>) -> Self {
        Self::with_api(auth, "https://www.googleapis.com")
    }

    pub fn with_api(auth: Arc<TokenSource>, api: &str) -> Self {
        Self { auth, api: api.trim_end_matches('/').into() }
    }

    fn http(&self) -> &reqwest::Client {
        &self.auth.http
    }

    async fn find(&self, name: &str) -> Result<Option<DriveFile>> {
        let q = format!("name = '{}' and trashed = false", name.replace('\'', "\\'"));
        let url = format!("{}/drive/v3/files", self.api);
        let resp = authed(&self.auth, |t| {
            self.http()
                .get(&url)
                .bearer_auth(t)
                .query(&[("spaces", "appDataFolder"), ("q", q.as_str()), ("fields", &format!("files({FIELDS})"))])
        })
        .await?;
        if !resp.status().is_success() {
            return Err(http_error(KIND, "buscar el backup", resp).await);
        }
        #[derive(Deserialize)]
        struct List {
            files: Vec<DriveFile>,
        }
        let list: List = resp.json().await.map_err(|e| net_error(KIND, e))?;
        Ok(list.files.into_iter().next())
    }
}

#[async_trait]
impl CloudStore for GoogleDrive {
    fn kind(&self) -> ProviderKind {
        KIND
    }

    async fn account(&self) -> Result<String> {
        let url = format!("{}/drive/v3/about", self.api);
        let resp = authed(&self.auth, |t| self.http().get(&url).bearer_auth(t).query(&[("fields", "user(displayName,emailAddress)")])).await?;
        if !resp.status().is_success() {
            return Err(http_error(KIND, "leer la cuenta", resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| net_error(KIND, e))?;
        let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).map(str::to_string);
        Ok(s("/user/emailAddress").or_else(|| s("/user/displayName")).unwrap_or_else(|| "Google Drive".into()))
    }

    async fn stat(&self, name: &str) -> Result<Option<RemoteMeta>> {
        Ok(self.find(name).await?.map(|f| f.meta()))
    }

    async fn download(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(f) = self.find(name).await? else { return Ok(None) };
        let url = format!("{}/drive/v3/files/{}", self.api, f.id);
        let resp = authed(&self.auth, |t| self.http().get(&url).bearer_auth(t).query(&[("alt", "media")])).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(http_error(KIND, "descargar el backup", resp).await);
        }
        Ok(Some(resp.bytes().await.map_err(|e| net_error(KIND, e))?.to_vec()))
    }

    async fn upload(&self, name: &str, bytes: Vec<u8>) -> Result<RemoteMeta> {
        let resp = match self.find(name).await? {
            // Same file, new content: Drive keeps its revision history.
            Some(f) => {
                let url = format!("{}/upload/drive/v3/files/{}", self.api, f.id);
                authed(&self.auth, |t| {
                    self.http()
                        .patch(&url)
                        .bearer_auth(t)
                        .query(&[("uploadType", "media"), ("fields", FIELDS)])
                        .header("Content-Type", "application/json")
                        .body(bytes.clone())
                })
                .await?
            }
            None => {
                let boundary = "dbine-backup-boundary-7f3a";
                let meta = serde_json::json!({ "name": name, "parents": ["appDataFolder"], "mimeType": "application/json" });
                let mut body = format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/json\r\n\r\n").into_bytes();
                body.extend_from_slice(&bytes);
                body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
                let url = format!("{}/upload/drive/v3/files", self.api);
                authed(&self.auth, |t| {
                    self.http()
                        .post(&url)
                        .bearer_auth(t)
                        .query(&[("uploadType", "multipart"), ("fields", FIELDS)])
                        .header("Content-Type", format!("multipart/related; boundary={boundary}"))
                        .body(body.clone())
                })
                .await?
            }
        };
        if !resp.status().is_success() {
            return Err(http_error(KIND, "subir el backup", resp).await);
        }
        let f: DriveFile = resp.json().await.map_err(|e| net_error(KIND, e))?;
        Ok(f.meta())
    }

    async fn delete(&self, name: &str) -> Result<()> {
        let Some(f) = self.find(name).await? else { return Ok(()) };
        let url = format!("{}/drive/v3/files/{}", self.api, f.id);
        let resp = authed(&self.auth, |t| self.http().delete(&url).bearer_auth(t)).await?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(http_error(KIND, "borrar el backup", resp).await);
        }
        Ok(())
    }
}
