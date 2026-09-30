//! OneDrive's app folder (`Apps/DBine`, Microsoft Graph's `approot`): the
//! `Files.ReadWrite.AppFolder` scope gives DBine access to that folder only.

use crate::oauth::{authed, TokenSource};
use crate::provider::{http_error, net_error, CloudStore, ProviderKind, RemoteMeta};
use crate::Result;
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

const KIND: ProviderKind = ProviderKind::Onedrive;

pub struct OneDrive {
    auth: Arc<TokenSource>,
    /// `https://graph.microsoft.com/v1.0` (a mock server in tests).
    api: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveItem {
    #[serde(default)]
    c_tag: Option<String>,
    #[serde(default)]
    e_tag: Option<String>,
    #[serde(default)]
    last_modified_date_time: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

impl DriveItem {
    fn meta(self) -> RemoteMeta {
        RemoteMeta {
            // cTag changes with the content only.
            revision: self.c_tag.or(self.e_tag).or_else(|| self.last_modified_date_time.clone()).unwrap_or_default(),
            modified: self.last_modified_date_time,
            size: self.size,
        }
    }
}

impl OneDrive {
    pub fn new(auth: Arc<TokenSource>) -> Self {
        Self::with_api(auth, "https://graph.microsoft.com/v1.0")
    }

    pub fn with_api(auth: Arc<TokenSource>, api: &str) -> Self {
        Self { auth, api: api.trim_end_matches('/').into() }
    }

    fn item(&self, name: &str) -> String {
        format!("{}/me/drive/special/approot:/{}", self.api, urlencode(name))
    }

    fn http(&self) -> &reqwest::Client {
        &self.auth.http
    }
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20")
}

#[async_trait]
impl CloudStore for OneDrive {
    fn kind(&self) -> ProviderKind {
        KIND
    }

    async fn account(&self) -> Result<String> {
        let url = format!("{}/me", self.api);
        let resp = authed(&self.auth, |t| self.http().get(&url).bearer_auth(t).query(&[("$select", "displayName,mail,userPrincipalName")])).await?;
        if !resp.status().is_success() {
            return Err(http_error(KIND, "leer la cuenta", resp).await);
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| net_error(KIND, e))?;
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string);
        Ok(s("mail").or_else(|| s("userPrincipalName")).or_else(|| s("displayName")).unwrap_or_else(|| "OneDrive".into()))
    }

    async fn stat(&self, name: &str) -> Result<Option<RemoteMeta>> {
        let url = self.item(name);
        let resp = authed(&self.auth, |t| {
            self.http().get(&url).bearer_auth(t).query(&[("$select", "cTag,eTag,lastModifiedDateTime,size")])
        })
        .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(http_error(KIND, "buscar el backup", resp).await);
        }
        Ok(Some(resp.json::<DriveItem>().await.map_err(|e| net_error(KIND, e))?.meta()))
    }

    async fn download(&self, name: &str) -> Result<Option<Vec<u8>>> {
        // Graph answers with a redirect to a pre-authenticated URL; reqwest
        // follows it and drops the Authorization header on the way.
        let url = format!("{}:/content", self.item(name));
        let resp = authed(&self.auth, |t| self.http().get(&url).bearer_auth(t)).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(http_error(KIND, "descargar el backup", resp).await);
        }
        Ok(Some(resp.bytes().await.map_err(|e| net_error(KIND, e))?.to_vec()))
    }

    async fn upload(&self, name: &str, bytes: Vec<u8>) -> Result<RemoteMeta> {
        // A simple upload takes up to 250 MB: far more than a backup needs.
        let url = format!("{}:/content", self.item(name));
        let resp = authed(&self.auth, |t| {
            self.http().put(&url).bearer_auth(t).header("Content-Type", "application/json").body(bytes.clone())
        })
        .await?;
        if !resp.status().is_success() {
            return Err(http_error(KIND, "subir el backup", resp).await);
        }
        Ok(resp.json::<DriveItem>().await.map_err(|e| net_error(KIND, e))?.meta())
    }

    async fn delete(&self, name: &str) -> Result<()> {
        let url = self.item(name);
        let resp = authed(&self.auth, |t| self.http().delete(&url).bearer_auth(t)).await?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(http_error(KIND, "borrar el backup", resp).await);
        }
        Ok(())
    }
}
