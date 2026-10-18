//! HashiCorp Vault: one key of a KV secret (v1 or v2), over Vault's HTTP
//! API. The token: the one kept for the field (typed in the form), else
//! `VAULT_TOKEN`, else `~/.vault-token` (what `vault login` writes); or one
//! AppRole gives for the role ID and the secret ID kept for the field (used
//! for one read and revoked). It only goes in the `X-Vault-Token` header to
//! the address the user gave; no redirect is followed (it would carry the
//! token elsewhere). The sign-in is shared with the database secrets engine
//! (vault_db.rs).

use super::cloud_auth::uri_encode;
use dbine_core::secret_sources::{wipe, KvVersion, VaultAuth};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

pub struct Request<'a> {
    pub address: &'a str,
    pub path: &'a str,
    pub key: &'a str,
    pub namespace: &'a str,
    pub kv_version: KvVersion,
    pub ca_file: &'a str,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")), home()) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(p),
    }
}

/// The token to use: the field's, `VAULT_TOKEN`, `~/.vault-token`.
pub fn token(kept: Option<&str>, env: impl Fn(&str) -> Option<String>, home: Option<PathBuf>) -> Result<String, String> {
    if let Some(t) = kept.map(str::trim).filter(|t| !t.is_empty()) {
        return Ok(t.to_string());
    }
    if let Some(t) = env("VAULT_TOKEN").map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Ok(t);
    }
    if let Some(t) = home.and_then(|h| std::fs::read_to_string(h.join(".vault-token")).ok()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Ok(t);
    }
    Err("no hay un token de Vault: escribilo en el formulario, definí VAULT_TOKEN o iniciá sesión con «vault login» (~/.vault-token)".into())
}

pub async fn read(req: &Request<'_>, auth: &Auth<'_>) -> Result<String, String> {
    let client = client(req.ca_file)?;
    let conn = Conn { address: req.address, namespace: req.namespace };
    let login = login(&client, &conn, auth).await?;
    let out = read_with(&client, req, &login.token).await;
    login.done(&client, &conn).await;
    out
}

/// The HTTP client for a Vault: no redirects, the CA file if given.
pub fn client(ca_file: &str) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none());
    if !ca_file.trim().is_empty() {
        let path = expand_home(ca_file.trim());
        let pem = std::fs::read(&path).map_err(|e| format!("no se pudo leer el certificado de la CA {}: {e}", path.display()))?;
        let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| format!("el certificado de la CA no es válido: {e}"))?;
        b = b.add_root_certificate(cert);
    }
    b.build().map_err(|e| e.to_string())
}

/// Where a Vault is.
pub struct Conn<'a> {
    pub address: &'a str,
    pub namespace: &'a str,
}

/// How to sign in: a token (the kept one, else the environment's), or
/// AppRole.
pub enum Auth<'a> {
    Token(Option<&'a str>),
    AppRole { mount: &'a str, role_id: &'a str, secret_id: Option<&'a str> },
}

impl<'a> Auth<'a> {
    /// A definition's sign-in, with the field's kept slots.
    pub fn of(kind: VaultAuth, approle_mount: &'a str, role_id: &'a str, slots: &'a dbine_core::secrets::Secrets) -> Self {
        match kind {
            VaultAuth::Token => Auth::Token(slots.get("").map(String::as_str)),
            VaultAuth::Approle => Auth::AppRole { mount: approle_mount, role_id, secret_id: slots.get("secret_id").map(String::as_str) },
        }
    }
}

/// A token to use; wiped on drop. `owned`: AppRole made it for DBine, so
/// it's revoked when no longer needed (and its leases with it).
pub struct Login {
    pub token: String,
    pub owned: bool,
    pub renewable: bool,
}

impl Drop for Login {
    fn drop(&mut self) {
        wipe(&mut self.token);
    }
}

impl Login {
    /// Done with a token AppRole made: revoke it (best effort).
    pub async fn done(&self, client: &reqwest::Client, conn: &Conn<'_>) {
        if self.owned {
            let _ = call(client, conn, &self.token, reqwest::Method::POST, "auth/token/revoke-self", None).await;
        }
    }
}

pub async fn login(client: &reqwest::Client, conn: &Conn<'_>, auth: &Auth<'_>) -> Result<Login, String> {
    match auth {
        Auth::Token(kept) => Ok(Login { token: token(*kept, |k| std::env::var(k).ok(), home())?, owned: false, renewable: false }),
        Auth::AppRole { mount, role_id, secret_id } => {
            let mount = Some(mount.trim().trim_matches('/')).filter(|m| !m.is_empty()).unwrap_or("approle");
            let mut body = serde_json::json!({ "role_id": role_id.trim() });
            if let Some(s) = secret_id.map(str::trim).filter(|s| !s.is_empty()) {
                body["secret_id"] = Value::String(s.to_string());
            }
            let api = format!("auth/{}/login", encode(&segments(mount)));
            let (status, json) = call(client, conn, "", reqwest::Method::POST, &api, Some(&body)).await;
            if let Value::Object(mut m) = body {
                if let Some(Value::String(s)) = m.get_mut("secret_id") {
                    wipe(s);
                }
            }
            let (status, json) = (status?, json);
            let auth = json.get("auth");
            match auth.and_then(|a| a.get("client_token")).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                Some(t) if status == 200 => Ok(Login {
                    token: t.to_string(),
                    owned: true,
                    renewable: auth.and_then(|a| a.get("renewable")).and_then(Value::as_bool).unwrap_or(false),
                }),
                _ => Err(match status {
                    400 | 401 | 403 => "Vault rechazó el role_id o el secret_id de AppRole (¿venció el secret_id?)".into(),
                    404 => format!("Vault no tiene AppRole en «auth/{mount}»"),
                    _ => status_error(status, &json, &format!("auth/{mount}")),
                }),
            }
        }
    }
}

/// A call to Vault's API (`/v1/<api>`): `(status, JSON)` (`Null` if none).
/// An empty token is sent as none (a sign-in).
pub async fn call(client: &reqwest::Client, conn: &Conn<'_>, token: &str, method: reqwest::Method, api: &str, body: Option<&Value>) -> (Result<u16, String>, Value) {
    let base = conn.address.trim().trim_end_matches('/');
    let mut r = client.request(method, format!("{base}/v1/{api}")).header("X-Vault-Request", "true");
    if !token.is_empty() {
        r = r.header("X-Vault-Token", token);
    }
    if !conn.namespace.trim().is_empty() {
        r = r.header("X-Vault-Namespace", conn.namespace.trim());
    }
    if let Some(b) = body {
        r = r.json(b);
    }
    let resp = match r.send().await {
        Ok(r) => r,
        Err(e) => return (Err(format!("no se pudo conectar con Vault en {base}: {}", e.without_url())), Value::Null),
    };
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    (Ok(status), serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// The path's segments, without empty ones.
pub(super) fn segments(path: &str) -> Vec<&str> {
    path.split('/').map(str::trim).filter(|s| !s.is_empty()).collect()
}

pub(super) fn encode(segs: &[&str]) -> String {
    segs.iter().map(|s| uri_encode(s)).collect::<Vec<_>>().join("/")
}

/// The API path of a KV read: `<path>` for v1, `<mount>/data/<rest>` for v2.
pub fn api_path(path: &str, v2: bool, mount: Option<&str>) -> String {
    let segs = segments(path);
    if !v2 {
        return encode(&segs);
    }
    let mount_segs = mount.map(segments).filter(|m| !m.is_empty() && segs.starts_with(m)).unwrap_or_else(|| segs[..1.min(segs.len())].to_vec());
    let rest = &segs[mount_segs.len()..];
    format!("{}/data/{}", encode(&mount_segs), encode(rest))
}

async fn get(client: &reqwest::Client, req: &Request<'_>, token: &str, api: &str) -> Result<(u16, Value), String> {
    let conn = Conn { address: req.address, namespace: req.namespace };
    let (status, json) = call(client, &conn, token, reqwest::Method::GET, api, None).await;
    Ok((status?, json))
}

/// The mount and its KV version, if the token may ask (it usually can).
async fn mount_of(client: &reqwest::Client, req: &Request<'_>, token: &str) -> Option<(String, bool)> {
    let (status, json) = get(client, req, token, &format!("sys/internal/ui/mounts/{}", encode(&segments(req.path)))).await.ok()?;
    if status != 200 {
        return None;
    }
    let data = json.get("data")?;
    let mount = data.get("path")?.as_str()?.to_string();
    let v2 = data.get("options").and_then(|o| o.get("version")).and_then(Value::as_str) == Some("2");
    Some((mount, v2))
}

pub async fn read_with(client: &reqwest::Client, req: &Request<'_>, token: &str) -> Result<String, String> {
    let (mount, v2) = match req.kv_version {
        KvVersion::V1 => (None, Some(false)),
        KvVersion::V2 => (mount_of(client, req, token).await.map(|m| m.0), Some(true)),
        KvVersion::Auto => match mount_of(client, req, token).await {
            Some((m, v2)) => (Some(m), Some(v2)),
            None => (None, None),
        },
    };
    let tries: Vec<bool> = match v2 {
        Some(v) => vec![v],
        // Unknown: v2 first (the default engine), then v1.
        None => vec![true, false],
    };
    let mut last = String::new();
    for (i, v2) in tries.iter().enumerate() {
        let (status, json) = get(client, req, token, &api_path(req.path, *v2, mount.as_deref())).await?;
        if status == 200 {
            return extract(&json, *v2, req.key, req.path);
        }
        last = status_error(status, &json, req.path);
        if status != 404 || i + 1 == tries.len() {
            break;
        }
    }
    Err(last)
}

pub(super) fn status_error(status: u16, json: &Value, path: &str) -> String {
    let errors = json.get("errors").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).unwrap_or_default();
    let errors = super::exec::first_lines(&errors, 300);
    match status {
        403 => format!("Vault: permiso denegado para leer «{path}» (el token venció o su política no lo permite)"),
        404 => format!("Vault: no existe el secreto «{path}»"),
        503 => "Vault está sellado o no disponible".into(),
        300..=399 => "Vault respondió con una redirección: usá la dirección del servidor activo".into(),
        _ if errors.is_empty() => format!("Vault respondió {status}"),
        _ => format!("Vault respondió {status}: {errors}"),
    }
}

/// `data.<key>` (v1) or `data.data.<key>` (v2). A v1 read of a v2 API path
/// (`secret/data/app`) also works.
pub fn extract(json: &Value, v2: bool, key: &str, path: &str) -> Result<String, String> {
    let mut data = json.get("data").ok_or_else(|| format!("Vault no devolvió datos para «{path}»"))?;
    if v2 || (data.get("data").is_some_and(Value::is_object) && data.get("metadata").is_some()) {
        data = data.get("data").filter(|d| d.is_object()).ok_or_else(|| format!("el secreto «{path}» está borrado o no tiene datos"))?;
    }
    let obj = data.as_object().ok_or_else(|| format!("Vault no devolvió datos para «{path}»"))?;
    match obj.get(key.trim()) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(v @ (Value::Number(_) | Value::Bool(_))) => Ok(v.to_string()),
        Some(_) => Err(format!("la clave «{}» del secreto de Vault no es un texto", key.trim())),
        None => {
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).take(20).collect();
            keys.sort_unstable();
            Err(format!("el secreto «{path}» de Vault no tiene la clave «{}» (tiene: {})", key.trim(), keys.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kv_paths() {
        assert_eq!(api_path("secret/myapp/db", false, None), "secret/myapp/db");
        assert_eq!(api_path("secret/myapp/db", true, None), "secret/data/myapp/db");
        assert_eq!(api_path("/team/kv/app/", true, Some("team/kv/")), "team/kv/data/app");
        assert_eq!(api_path("secret/my app/db", true, Some("secret/")), "secret/data/my%20app/db");
        // A mount that isn't a prefix of the path is ignored.
        assert_eq!(api_path("kv/app", true, Some("other/")), "kv/data/app");
    }

    #[test]
    fn values_out_of_responses() {
        let v2 = json!({ "data": { "data": { "password": "s3cr3t", "port": 5432 }, "metadata": { "version": 3 } } });
        assert_eq!(extract(&v2, true, "password", "secret/app").unwrap(), "s3cr3t");
        assert_eq!(extract(&v2, true, "port", "secret/app").unwrap(), "5432");
        // v2 API path read as v1.
        assert_eq!(extract(&v2, false, "password", "secret/data/app").unwrap(), "s3cr3t");
        let v1 = json!({ "data": { "password": "old" } });
        assert_eq!(extract(&v1, false, "password", "kv/app").unwrap(), "old");
        let e = extract(&v1, false, "pw", "kv/app").unwrap_err();
        assert!(e.contains("«pw»") && e.contains("password") && !e.contains("old"), "{e}");
        let deleted = json!({ "data": { "data": null, "metadata": {} } });
        assert!(extract(&deleted, true, "password", "secret/app").unwrap_err().contains("borrado"));
    }

    #[test]
    fn token_order() {
        let dir = super::super::tests::temp_dir("vault-home");
        std::fs::write(dir.join(".vault-token"), "file-token\n").unwrap();
        let env_with = |k: &str| (k == "VAULT_TOKEN").then(|| "env-token".to_string());
        let no_env = |_: &str| None;
        assert_eq!(token(Some("kept"), env_with, Some(dir.clone())).unwrap(), "kept");
        assert_eq!(token(None, env_with, Some(dir.clone())).unwrap(), "env-token");
        assert_eq!(token(Some("  "), no_env, Some(dir.clone())).unwrap(), "file-token");
        assert!(token(None, no_env, None).unwrap_err().contains("vault login"));
    }

    #[test]
    fn errors_by_status() {
        assert!(status_error(403, &json!({"errors": ["permission denied"]}), "secret/x").contains("permiso denegado"));
        assert!(status_error(404, &Value::Null, "secret/x").contains("no existe"));
        assert_eq!(status_error(500, &json!({"errors": ["internal", "boom"]}), "s"), "Vault respondió 500: internal; boom");
    }

    /// Against `dbine-test-vault` (dev mode):
    /// `docker run -d --name dbine-test-vault -p 28200:8200 -e VAULT_DEV_ROOT_TOKEN_ID=dbine-root hashicorp/vault`,
    /// a v2 secret at `secret/dbine/pg` and a v1 engine at `kv1/`; then
    /// `DBINE_TEST_VAULT=http://127.0.0.1:28200 cargo test -p dbine secret_sources::vault -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_vault() {
        let Ok(addr) = std::env::var("DBINE_TEST_VAULT") else { return };
        let client = reqwest::Client::new();
        let req = |path: &'static str, key: &'static str, v: KvVersion| Request { address: Box::leak(addr.clone().into_boxed_str()), path, key, namespace: "", kv_version: v, ca_file: "" };
        assert_eq!(read_with(&client, &req("secret/dbine/pg", "password", KvVersion::Auto), "dbine-root").await.unwrap(), "pw");
        assert_eq!(read_with(&client, &req("secret/dbine/pg", "password", KvVersion::V2), "dbine-root").await.unwrap(), "pw");
        assert_eq!(read_with(&client, &req("kv1/dbine/pg", "password", KvVersion::Auto), "dbine-root").await.unwrap(), "pw-v1");
        assert_eq!(read_with(&client, &req("kv1/dbine/pg", "password", KvVersion::V1), "dbine-root").await.unwrap(), "pw-v1");
        assert!(read_with(&client, &req("secret/dbine/none", "password", KvVersion::Auto), "dbine-root").await.unwrap_err().contains("no existe"));
        assert!(read_with(&client, &req("secret/dbine/pg", "nope", KvVersion::Auto), "dbine-root").await.unwrap_err().contains("«nope»"));
        assert!(read_with(&client, &req("secret/dbine/pg", "password", KvVersion::Auto), "wrong-token").await.unwrap_err().contains("permiso denegado"));
    }
}
