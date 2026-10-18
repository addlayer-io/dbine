//! Google Secret Manager: `versions/<v>:access`, with an access token from
//! the same chain as the Google Cloud IAM sign-in (cloud_auth.rs: gcloud,
//! the application default credentials, the metadata server). The endpoint
//! is Google's; only the project, secret and version come from the form.

use super::cloud_auth::{self, uri_encode};
use base64::Engine;
use serde_json::Value;
use std::time::Duration;

pub const ENDPOINT: &str = "https://secretmanager.googleapis.com";

pub async fn get_secret(project: &str, secret: &str, version: &str) -> Result<String, String> {
    let (token, _, _) = cloud_auth::gcp_token(None, true).await.map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    get_secret_at(&client, ENDPOINT, project.trim(), secret.trim(), version.trim(), &token).await
}

pub fn access_url(base: &str, project: &str, secret: &str, version: &str) -> String {
    let version = if version.is_empty() { "latest" } else { version };
    format!("{}/v1/projects/{}/secrets/{}/versions/{}:access", base.trim_end_matches('/'), uri_encode(project), uri_encode(secret), uri_encode(version))
}

pub async fn get_secret_at(client: &reqwest::Client, base: &str, project: &str, secret: &str, version: &str, token: &str) -> Result<String, String> {
    let resp = client
        .get(access_url(base, project, secret, version))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("no se pudo conectar con Google Secret Manager: {}", e.without_url()))?;
    let status = resp.status().as_u16();
    let json: Value = serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    if status == 200 {
        let data = json.pointer("/payload/data").and_then(Value::as_str).ok_or_else(|| format!("Google Secret Manager no devolvió un valor para «{secret}»"))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| "el valor de Google Secret Manager no es base64 válido".to_string())?;
        return String::from_utf8(bytes).map_err(|_| "el valor de Google Secret Manager no es texto".to_string());
    }
    let msg = json.pointer("/error/message").and_then(Value::as_str).unwrap_or("");
    let msg = super::exec::first_lines(msg, 300);
    Err(match status {
        401 => format!("Google rechazó el token (¿«gcloud auth login»?): {msg}"),
        403 => format!("Google Secret Manager: permiso denegado (hace falta el rol «Secret Manager Secret Accessor»): {msg}"),
        404 => format!("Google Secret Manager: no existe el secreto «{secret}» (o esa versión) en el proyecto «{project}»"),
        _ if msg.is_empty() => format!("Google Secret Manager respondió {status}"),
        _ => format!("Google Secret Manager respondió {status}: {msg}"),
    })
}

// ---- browse ------------------------------------------------------------------

/// The project's secret names (paged), or a secret's JSON keys (`item`,
/// its latest version).
pub async fn browse(project: &str, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    let (token, _, _) = cloud_auth::gcp_token(None, true).await.map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    browse_at(&client, ENDPOINT, project.trim(), &token, at).await
}

pub async fn browse_at(client: &reqwest::Client, base: &str, project: &str, token: &str, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    use super::browse::{Entry, Listing, MAX_ENTRIES};
    if let Some(item) = at.item {
        let raw = get_secret_at(client, base, project, item, "", token).await?;
        return Ok(Listing::fields(super::browse::json_keys(raw)));
    }
    let mut out = Vec::new();
    let mut page = String::new();
    loop {
        let mut url = format!("{}/v1/projects/{}/secrets?pageSize=250", base.trim_end_matches('/'), uri_encode(project));
        if !page.is_empty() {
            url.push_str(&format!("&pageToken={}", uri_encode(&page)));
        }
        let resp = client.get(url).bearer_auth(token).send().await.map_err(|e| format!("no se pudo conectar con Google Secret Manager: {}", e.without_url()))?;
        let status = resp.status().as_u16();
        let json: Value = serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
        if status != 200 {
            let msg = super::exec::first_lines(json.pointer("/error/message").and_then(Value::as_str).unwrap_or(""), 300);
            return Err(match status {
                401 => format!("Google rechazó el token (¿«gcloud auth login»?): {msg}"),
                403 => format!("Google Secret Manager: permiso denegado para listar los secretos (hace falta el rol «Secret Manager Viewer»): {msg}"),
                404 => format!("Google Secret Manager: no existe el proyecto «{project}»"),
                _ if msg.is_empty() => format!("Google Secret Manager respondió {status}"),
                _ => format!("Google Secret Manager respondió {status}: {msg}"),
            });
        }
        for s in json.get("secrets").and_then(Value::as_array).into_iter().flatten() {
            if let Some(name) = s.get("name").and_then(Value::as_str).and_then(|n| n.rsplit('/').next()) {
                out.push(Entry::item(name, name, "", ""));
            }
        }
        page = json.get("nextPageToken").and_then(Value::as_str).unwrap_or("").to_string();
        if page.is_empty() || out.len() > MAX_ENTRIES {
            break;
        }
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(Listing::complete(out))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn urls() {
        assert_eq!(access_url(ENDPOINT, "my-proj", "db-pw", ""), "https://secretmanager.googleapis.com/v1/projects/my-proj/secrets/db-pw/versions/latest:access");
        assert_eq!(access_url(ENDPOINT, "my-proj", "db-pw", "3"), "https://secretmanager.googleapis.com/v1/projects/my-proj/secrets/db-pw/versions/3:access");
    }

    /// A one-route HTTP server: answers `body` with `status` to every
    /// request whose first line contains `path` and carries the bearer
    /// token, 404 otherwise.
    pub async fn mock(path: &'static str, token: &'static str, status: u16, body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { return };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first = req.lines().next().unwrap_or("");
                    let authorized = req.lines().any(|l| l.eq_ignore_ascii_case(&format!("authorization: Bearer {token}")));
                    let (status, body) = if first.contains(path) && authorized { (status, body) } else { (404, r#"{"error":{"code":404,"message":"not found"}}"#.to_string()) };
                    let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn responses() {
        let ok = serde_json::json!({ "name": "projects/1/secrets/db-pw/versions/3", "payload": { "data": base64::engine::general_purpose::STANDARD.encode("{\"password\":\"pw\"}") } }).to_string();
        let base = mock("/v1/projects/my-proj/secrets/db-pw/versions/latest:access", "tok", 200, ok).await;
        let client = reqwest::Client::new();
        let raw = get_secret_at(&client, &base, "my-proj", "db-pw", "", "tok").await.unwrap();
        assert_eq!(dbine_core::secret_sources::pick_json(&raw, "password").unwrap(), "pw");
        let e = get_secret_at(&client, &base, "my-proj", "other", "", "tok").await.unwrap_err();
        assert!(e.contains("no existe el secreto «other»"), "{e}");
        let denied = mock("/", "tok", 403, r#"{"error":{"code":403,"message":"Permission 'secretmanager.versions.access' denied","status":"PERMISSION_DENIED"}}"#.into()).await;
        let e = get_secret_at(&client, &denied, "p", "s", "", "tok").await.unwrap_err();
        assert!(e.contains("permiso denegado") && e.contains("secretmanager.versions.access"), "{e}");
    }

    /// The whole path with a fake `gcloud` on PATH (its token) and the mock
    /// API: `DBINE_TEST_FAKE_GCLOUD=1 cargo test -p dbine secret_sources::gcp -- --ignored --test-threads=1`.
    #[tokio::test]
    async fn browse_names_then_keys() {
        let list = serde_json::json!({ "secrets": [{ "name": "projects/my-proj/secrets/db-pw" }, { "name": "projects/my-proj/secrets/api" }] }).to_string();
        let base = mock("/v1/projects/my-proj/secrets?pageSize=250", "tok", 200, list).await;
        let client = reqwest::Client::new();
        let l = browse_at(&client, &base, "my-proj", "tok", Default::default()).await.unwrap();
        assert_eq!(l.entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["api", "db-pw"]);
        let ok = serde_json::json!({ "payload": { "data": base64::engine::general_purpose::STANDARD.encode("{\"password\":\"pw\",\"user\":\"u\"}") } }).to_string();
        let base = mock("/v1/projects/my-proj/secrets/db-pw/versions/latest:access", "tok", 200, ok).await;
        let at = super::super::browse::At { item: Some("db-pw"), ..Default::default() };
        let l = browse_at(&client, &base, "my-proj", "tok", at).await.unwrap();
        assert_eq!(l.fields.as_deref().unwrap(), ["password", "user"]);
        assert!(!serde_json::to_string(&l).unwrap().contains("pw\""));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn live_fake_gcloud() {
        if std::env::var("DBINE_TEST_FAKE_GCLOUD").is_err() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = super::super::tests::temp_dir("fake-gcloud");
        let gcloud = dir.join("gcloud");
        std::fs::write(&gcloud, "#!/bin/sh\n[ \"$1 $2\" = 'auth print-access-token' ] && { echo ya29.fake-token; exit 0; }\nexit 7\n").unwrap();
        std::fs::set_permissions(&gcloud, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![dir.clone()];
        paths.extend(std::env::split_paths(&old));
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        let (token, _, _) = cloud_auth::gcp_token(None, true).await.unwrap();
        std::env::set_var("PATH", old);
        assert_eq!(token, "ya29.fake-token");
        let ok = serde_json::json!({ "payload": { "data": base64::engine::general_purpose::STANDARD.encode("pw") } }).to_string();
        let base = mock("/v1/projects/p1/secrets/s1/versions/2:access", "ya29.fake-token", 200, ok).await;
        assert_eq!(get_secret_at(&reqwest::Client::new(), &base, "p1", "s1", "2", &token).await.unwrap(), "pw");
    }
}
