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
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn expand_home(p: &str) -> PathBuf {
    match (
        p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")),
        home(),
    ) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(p),
    }
}

/// The token to use: the field's, `VAULT_TOKEN`, `~/.vault-token`.
pub fn token(
    kept: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
    home: Option<PathBuf>,
) -> Result<String, String> {
    if let Some(t) = kept.map(str::trim).filter(|t| !t.is_empty()) {
        return Ok(t.to_string());
    }
    if let Some(t) = env("VAULT_TOKEN")
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        return Ok(t);
    }
    if let Some(t) = home
        .and_then(|h| std::fs::read_to_string(h.join(".vault-token")).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        return Ok(t);
    }
    Err("no hay un token de Vault: escribilo en el formulario, definí VAULT_TOKEN o iniciá sesión con «vault login» (~/.vault-token)".into())
}

pub async fn read(req: &Request<'_>, auth: &Auth<'_>) -> Result<String, String> {
    let client = client(req.ca_file)?;
    let conn = Conn {
        address: req.address,
        namespace: req.namespace,
    };
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
        let pem = std::fs::read(&path).map_err(|e| {
            format!(
                "no se pudo leer el certificado de la CA {}: {e}",
                path.display()
            )
        })?;
        let cert = reqwest::Certificate::from_pem(&pem)
            .map_err(|e| format!("el certificado de la CA no es válido: {e}"))?;
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
    AppRole {
        mount: &'a str,
        role_id: &'a str,
        secret_id: Option<&'a str>,
    },
}

impl<'a> Auth<'a> {
    /// A definition's sign-in, with the field's kept slots.
    pub fn of(
        kind: VaultAuth,
        approle_mount: &'a str,
        role_id: &'a str,
        slots: &'a dbine_core::secrets::Secrets,
    ) -> Self {
        match kind {
            VaultAuth::Token => Auth::Token(slots.get("").map(String::as_str)),
            VaultAuth::Approle => Auth::AppRole {
                mount: approle_mount,
                role_id,
                secret_id: slots.get("secret_id").map(String::as_str),
            },
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
            let _ = call(
                client,
                conn,
                &self.token,
                reqwest::Method::POST,
                "auth/token/revoke-self",
                None,
            )
            .await;
        }
    }
}

pub async fn login(
    client: &reqwest::Client,
    conn: &Conn<'_>,
    auth: &Auth<'_>,
) -> Result<Login, String> {
    match auth {
        Auth::Token(kept) => Ok(Login {
            token: token(*kept, |k| std::env::var(k).ok(), home())?,
            owned: false,
            renewable: false,
        }),
        Auth::AppRole {
            mount,
            role_id,
            secret_id,
        } => {
            let mount = Some(mount.trim().trim_matches('/'))
                .filter(|m| !m.is_empty())
                .unwrap_or("approle");
            let mut body = serde_json::json!({ "role_id": role_id.trim() });
            if let Some(s) = secret_id.map(str::trim).filter(|s| !s.is_empty()) {
                body["secret_id"] = Value::String(s.to_string());
            }
            let api = format!("auth/{}/login", encode(&segments(mount)));
            let (status, json) =
                call(client, conn, "", reqwest::Method::POST, &api, Some(&body)).await;
            if let Value::Object(mut m) = body {
                if let Some(Value::String(s)) = m.get_mut("secret_id") {
                    wipe(s);
                }
            }
            let (status, json) = (status?, json);
            let auth = json.get("auth");
            match auth
                .and_then(|a| a.get("client_token"))
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                Some(t) if status == 200 => Ok(Login {
                    token: t.to_string(),
                    owned: true,
                    renewable: auth
                        .and_then(|a| a.get("renewable"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }),
                _ => Err(match status {
                    400 | 401 | 403 => {
                        "Vault rechazó el role_id o el secret_id de AppRole (¿venció el secret_id?)"
                            .into()
                    }
                    404 => format!("Vault no tiene AppRole en «auth/{mount}»"),
                    _ => status_error(status, &json, &format!("auth/{mount}")),
                }),
            }
        }
    }
}

/// A call to Vault's API (`/v1/<api>`): `(status, JSON)` (`Null` if none).
/// An empty token is sent as none (a sign-in).
pub async fn call(
    client: &reqwest::Client,
    conn: &Conn<'_>,
    token: &str,
    method: reqwest::Method,
    api: &str,
    body: Option<&Value>,
) -> (Result<u16, String>, Value) {
    let base = conn.address.trim().trim_end_matches('/');
    let mut r = client
        .request(method, format!("{base}/v1/{api}"))
        .header("X-Vault-Request", "true");
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
        Err(e) => {
            return (
                Err(format!(
                    "no se pudo conectar con Vault en {base}: {}",
                    e.without_url()
                )),
                Value::Null,
            )
        }
    };
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    (
        Ok(status),
        serde_json::from_str(&body).unwrap_or(Value::Null),
    )
}

/// The account wizard's check: sign in (an AppRole token is revoked right
/// after) and `auth/token/lookup-self`. `(display name, policies)`; the
/// answer (it carries the token) is wiped.
pub async fn whoami(
    conn: &Conn<'_>,
    ca_file: &str,
    auth: &Auth<'_>,
) -> Result<(String, Vec<String>), String> {
    let client = client(ca_file)?;
    whoami_with(&client, conn, auth).await
}

pub async fn whoami_with(
    client: &reqwest::Client,
    conn: &Conn<'_>,
    auth: &Auth<'_>,
) -> Result<(String, Vec<String>), String> {
    let login = login(client, conn, auth).await?;
    let (status, mut json) = call(
        client,
        conn,
        &login.token,
        reqwest::Method::GET,
        "auth/token/lookup-self",
        None,
    )
    .await;
    login.done(client, conn).await;
    let status = status?;
    let out = match status {
        200 => {
            let data = json.get("data");
            let name = data
                .and_then(|d| d.get("display_name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut policies: Vec<String> = ["policies", "identity_policies"]
                .iter()
                .filter_map(|k| data.and_then(|d| d.get(*k)).and_then(Value::as_array))
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            policies.sort();
            policies.dedup();
            Ok((name, policies))
        }
        403 => Err(
            "Vault rechazó el token (¿venció, o es de otro servidor o espacio de nombres?)"
                .to_string(),
        ),
        _ => Err(status_error(status, &json, "auth/token/lookup-self")),
    };
    super::browse::wipe_json(&mut json);
    out
}

/// The path's segments, without empty ones.
pub(super) fn segments(path: &str) -> Vec<&str> {
    path.split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

pub(super) fn encode(segs: &[&str]) -> String {
    segs.iter()
        .map(|s| uri_encode(s))
        .collect::<Vec<_>>()
        .join("/")
}

/// The API path of a KV read: `<path>` for v1, `<mount>/data/<rest>` for v2.
pub fn api_path(path: &str, v2: bool, mount: Option<&str>) -> String {
    let segs = segments(path);
    if !v2 {
        return encode(&segs);
    }
    let mount_segs = mount
        .map(segments)
        .filter(|m| !m.is_empty() && segs.starts_with(m))
        .unwrap_or_else(|| segs[..1.min(segs.len())].to_vec());
    let rest = &segs[mount_segs.len()..];
    format!("{}/data/{}", encode(&mount_segs), encode(rest))
}

async fn get(
    client: &reqwest::Client,
    req: &Request<'_>,
    token: &str,
    api: &str,
) -> Result<(u16, Value), String> {
    let conn = Conn {
        address: req.address,
        namespace: req.namespace,
    };
    let (status, json) = call(client, &conn, token, reqwest::Method::GET, api, None).await;
    Ok((status?, json))
}

/// The mount and its KV version, if the token may ask (it usually can).
async fn mount_of(
    client: &reqwest::Client,
    req: &Request<'_>,
    token: &str,
) -> Option<(String, bool)> {
    let (status, json) = get(
        client,
        req,
        token,
        &format!("sys/internal/ui/mounts/{}", encode(&segments(req.path))),
    )
    .await
    .ok()?;
    if status != 200 {
        return None;
    }
    let data = json.get("data")?;
    let mount = data.get("path")?.as_str()?.to_string();
    let v2 = data
        .get("options")
        .and_then(|o| o.get("version"))
        .and_then(Value::as_str)
        == Some("2");
    Some((mount, v2))
}

pub async fn read_with(
    client: &reqwest::Client,
    req: &Request<'_>,
    token: &str,
) -> Result<String, String> {
    let (mut json, v2) = read_json(client, req, token).await?;
    let r = extract(&json, v2, req.key, req.path);
    super::browse::wipe_json(&mut json);
    r
}

/// The secret at `req.path` as Vault answers it, and whether it's KV v2.
async fn read_json(
    client: &reqwest::Client,
    req: &Request<'_>,
    token: &str,
) -> Result<(Value, bool), String> {
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
        let (status, json) = get(
            client,
            req,
            token,
            &api_path(req.path, *v2, mount.as_deref()),
        )
        .await?;
        if status == 200 {
            return Ok((json, *v2));
        }
        last = status_error(status, &json, req.path);
        if status != 404 || i + 1 == tries.len() {
            break;
        }
    }
    Err(last)
}

pub(super) fn status_error(status: u16, json: &Value, path: &str) -> String {
    let errors = json
        .get("errors")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
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
    let mut data = json
        .get("data")
        .ok_or_else(|| format!("Vault no devolvió datos para «{path}»"))?;
    if v2 || (data.get("data").is_some_and(Value::is_object) && data.get("metadata").is_some()) {
        data = data
            .get("data")
            .filter(|d| d.is_object())
            .ok_or_else(|| format!("el secreto «{path}» está borrado o no tiene datos"))?;
    }
    let obj = data
        .as_object()
        .ok_or_else(|| format!("Vault no devolvió datos para «{path}»"))?;
    match obj.get(key.trim()) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(v @ (Value::Number(_) | Value::Bool(_))) => Ok(v.to_string()),
        Some(_) => Err(format!(
            "la clave «{}» del secreto de Vault no es un texto",
            key.trim()
        )),
        None => {
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).take(20).collect();
            keys.sort_unstable();
            Err(format!(
                "el secreto «{path}» de Vault no tiene la clave «{}» (tiene: {})",
                key.trim(),
                keys.join(", ")
            ))
        }
    }
}

// ---- browse ------------------------------------------------------------------

/// The KV mounts (no `path`), the keys under a path (one level: a key
/// ending in `/` is a folder), or a secret's keys (`item`; read, its keys
/// taken and the values wiped).
pub async fn browse(
    req: &Request<'_>,
    auth: &Auth<'_>,
    at: super::browse::At<'_>,
) -> Result<super::browse::Listing, String> {
    let client = client(req.ca_file)?;
    let conn = Conn {
        address: req.address,
        namespace: req.namespace,
    };
    let login = login(&client, &conn, auth).await?;
    let out = browse_with(&client, req, &login.token, at).await;
    login.done(&client, &conn).await;
    out
}

pub async fn browse_with(
    client: &reqwest::Client,
    req: &Request<'_>,
    token: &str,
    at: super::browse::At<'_>,
) -> Result<super::browse::Listing, String> {
    use super::browse::{Entry, Listing};
    if let Some(item) = at.item {
        let r = Request { path: item, ..*req };
        let (mut json, v2) = read_json(client, &r, token).await?;
        let keys = data_keys(&json, v2);
        super::browse::wipe_json(&mut json);
        return Ok(Listing::fields(keys));
    }
    if segments(at.path).is_empty() {
        let (status, json) = get(client, req, token, "sys/internal/ui/mounts").await?;
        if status != 200 {
            return Err(status_error(status, &json, "sys/internal/ui/mounts"));
        }
        let mounts = json
            .pointer("/data/secret")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut out: Vec<Entry> = mounts
            .iter()
            .filter(|(_, m)| {
                matches!(
                    m.get("type").and_then(Value::as_str),
                    Some("kv" | "generic")
                )
            })
            .map(|(path, _)| Entry::group(path.clone(), path.trim_end_matches('/'), ""))
            .collect();
        out.sort_by_key(|e| e.name.to_lowercase());
        return Ok(Listing::level(out));
    }
    let r = Request {
        path: at.path,
        ..*req
    };
    let (mount, v2) = match req.kv_version {
        KvVersion::V1 => (None, Some(false)),
        KvVersion::V2 => (mount_of(client, &r, token).await.map(|m| m.0), Some(true)),
        KvVersion::Auto => match mount_of(client, &r, token).await {
            Some((m, v2)) => (Some(m), Some(v2)),
            None => (None, None),
        },
    };
    let tries: Vec<bool> = match v2 {
        Some(v) => vec![v],
        None => vec![true, false],
    };
    let base = format!("{}/", segments(at.path).join("/"));
    for (i, v2) in tries.iter().enumerate() {
        let (status, json) = get(
            client,
            req,
            token,
            &format!("{}?list=true", list_path(at.path, *v2, mount.as_deref())),
        )
        .await?;
        if status == 200 || (status == 404 && i + 1 == tries.len()) {
            let keys: Vec<&str> = json
                .pointer("/data/keys")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let shown = base.trim_end_matches('/').to_string();
            let mut out: Vec<Entry> = keys
                .into_iter()
                .map(|k| match k.strip_suffix('/') {
                    Some(folder) => Entry::group(format!("{base}{k}"), folder, shown.clone()),
                    None => Entry::item(format!("{base}{k}"), k, shown.clone(), ""),
                })
                .collect();
            out.sort_by(|a, b| {
                (
                    a.kind == super::browse::EntryKind::Item,
                    a.name.to_lowercase(),
                )
                    .cmp(&(
                        b.kind == super::browse::EntryKind::Item,
                        b.name.to_lowercase(),
                    ))
            });
            return Ok(Listing::level(out));
        }
        if status != 404 {
            return Err(status_error(status, &json, at.path));
        }
    }
    Ok(Listing::level(Vec::new()))
}

/// Where a path's keys are listed: `<mount>/metadata/<rest>` in KV v2.
pub fn list_path(path: &str, v2: bool, mount: Option<&str>) -> String {
    let segs = segments(path);
    if !v2 {
        return encode(&segs);
    }
    let mount_segs = mount
        .map(segments)
        .filter(|m| !m.is_empty() && segs.starts_with(m))
        .unwrap_or_else(|| segs[..1.min(segs.len())].to_vec());
    let rest = &segs[mount_segs.len()..];
    if rest.is_empty() {
        format!("{}/metadata", encode(&mount_segs))
    } else {
        format!("{}/metadata/{}", encode(&mount_segs), encode(rest))
    }
}

/// A secret's keys (never its values).
pub fn data_keys(json: &Value, v2: bool) -> Vec<String> {
    let mut data = json.get("data");
    if v2
        || data.is_some_and(|d| {
            d.get("data").is_some_and(Value::is_object) && d.get("metadata").is_some()
        })
    {
        data = data.and_then(|d| d.get("data"));
    }
    let mut keys: Vec<String> = data
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort_unstable();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kv_paths() {
        assert_eq!(api_path("secret/myapp/db", false, None), "secret/myapp/db");
        assert_eq!(
            api_path("secret/myapp/db", true, None),
            "secret/data/myapp/db"
        );
        assert_eq!(
            api_path("/team/kv/app/", true, Some("team/kv/")),
            "team/kv/data/app"
        );
        assert_eq!(
            api_path("secret/my app/db", true, Some("secret/")),
            "secret/data/my%20app/db"
        );
        // A mount that isn't a prefix of the path is ignored.
        assert_eq!(api_path("kv/app", true, Some("other/")), "kv/data/app");
    }

    #[test]
    fn values_out_of_responses() {
        let v2 = json!({ "data": { "data": { "password": "s3cr3t", "port": 5432 }, "metadata": { "version": 3 } } });
        assert_eq!(
            extract(&v2, true, "password", "secret/app").unwrap(),
            "s3cr3t"
        );
        assert_eq!(extract(&v2, true, "port", "secret/app").unwrap(), "5432");
        // v2 API path read as v1.
        assert_eq!(
            extract(&v2, false, "password", "secret/data/app").unwrap(),
            "s3cr3t"
        );
        let v1 = json!({ "data": { "password": "old" } });
        assert_eq!(extract(&v1, false, "password", "kv/app").unwrap(), "old");
        let e = extract(&v1, false, "pw", "kv/app").unwrap_err();
        assert!(
            e.contains("«pw»") && e.contains("password") && !e.contains("old"),
            "{e}"
        );
        let deleted = json!({ "data": { "data": null, "metadata": {} } });
        assert!(extract(&deleted, true, "password", "secret/app")
            .unwrap_err()
            .contains("borrado"));
    }

    #[test]
    fn list_paths_and_keys() {
        assert_eq!(
            list_path("secret/", true, Some("secret/")),
            "secret/metadata"
        );
        assert_eq!(
            list_path("secret/myapp/", true, Some("secret/")),
            "secret/metadata/myapp"
        );
        assert_eq!(
            list_path("team/kv/app", true, Some("team/kv/")),
            "team/kv/metadata/app"
        );
        assert_eq!(list_path("kv/app/", false, None), "kv/app");
        let v2 = json!({ "data": { "data": { "password": "s3cr3t", "port": 5432 }, "metadata": { "version": 3 } } });
        assert_eq!(data_keys(&v2, true), ["password", "port"]);
        assert_eq!(data_keys(&json!({ "data": { "pw": "x" } }), false), ["pw"]);
    }

    /// Browsing against a mock of Vault: the KV mounts, one level of keys
    /// (v2 metadata, v1 paths), a secret's keys and never its values.
    #[tokio::test]
    async fn browse_against_a_mock() {
        use axum::extract::{Path as AxPath, Query};
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::get as aget;
        use axum::{Json, Router};
        use std::collections::HashMap;
        let ok =
            |h: &HeaderMap| h.get("x-vault-token").and_then(|v| v.to_str().ok()) == Some("root");
        let app = Router::new()
            .route("/v1/sys/internal/ui/mounts", aget(move |h: HeaderMap| async move {
                if !ok(&h) { return (StatusCode::FORBIDDEN, Json(json!({ "errors": ["permission denied"] }))); }
                (StatusCode::OK, Json(json!({ "data": { "secret": {
                    "secret/": { "type": "kv", "options": { "version": "2" } },
                    "old/": { "type": "kv", "options": { "version": "1" } },
                    "database/": { "type": "database" }
                } } })))
            }))
            .route("/v1/sys/internal/ui/mounts/{*p}", aget(|AxPath(p): AxPath<String>| async move {
                let (path, v) = if p.starts_with("secret") { ("secret/", "2") } else { ("old/", "1") };
                Json(json!({ "data": { "path": path, "type": "kv", "options": { "version": v } } }))
            }))
            .route("/v1/{*p}", aget(|AxPath(p): AxPath<String>, Query(q): Query<HashMap<String, String>>| async move {
                let list = q.get("list").is_some_and(|l| l == "true");
                match (p.as_str(), list) {
                    ("secret/metadata", true) => (StatusCode::OK, Json(json!({ "data": { "keys": ["myapp/", "shared"] } }))),
                    ("secret/metadata/myapp", true) => (StatusCode::OK, Json(json!({ "data": { "keys": ["db"] } }))),
                    ("old/team", true) => (StatusCode::OK, Json(json!({ "data": { "keys": ["pg"] } }))),
                    ("secret/data/myapp/db", false) => (StatusCode::OK, Json(json!({ "data": { "data": { "password": "s3cr3t", "username": "app" }, "metadata": {} } }))),
                    _ => (StatusCode::NOT_FOUND, Json(json!({ "errors": [] }))),
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let req = Request {
            address: &addr,
            path: "",
            key: "",
            namespace: "",
            kv_version: KvVersion::Auto,
            ca_file: "",
        };
        let c = reqwest::Client::new();
        let at = |path: &'static str| super::super::browse::At {
            path,
            ..Default::default()
        };
        let l = browse_with(&c, &req, "root", at("")).await.unwrap();
        assert_eq!(
            l.entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["old/", "secret/"],
            "KV mounts only"
        );
        let l = browse_with(&c, &req, "root", at("secret/")).await.unwrap();
        assert_eq!(
            l.entries
                .iter()
                .map(|e| (e.id.as_str(), e.name.as_str()))
                .collect::<Vec<_>>(),
            [("secret/myapp/", "myapp"), ("secret/shared", "shared")]
        );
        let l = browse_with(&c, &req, "root", at("secret/myapp/"))
            .await
            .unwrap();
        assert_eq!(l.entries[0].id, "secret/myapp/db");
        let l = browse_with(&c, &req, "root", at("old/team")).await.unwrap();
        assert_eq!(l.entries[0].id, "old/team/pg", "KV v1");
        let l = browse_with(
            &c,
            &req,
            "root",
            super::super::browse::At {
                item: Some("secret/myapp/db"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(l.fields.as_deref().unwrap(), ["password", "username"]);
        assert!(!serde_json::to_string(&l).unwrap().contains("s3cr3t"));
        assert!(browse_with(&c, &req, "nope", at(""))
            .await
            .unwrap_err()
            .contains("permiso denegado"));
    }

    /// Against Vault in dev mode as `dbine-test-vault` (port 28200, root
    /// token `dbine-root`) with `secret/dbine/pg` (KV v2), `old/team/pg`
    /// (KV v1 at `old/`) and a database role `ro`; see docs/secret-sources.md:
    /// `DBINE_TEST_VAULT=http://127.0.0.1:28200 cargo test -p dbine live_vault_browse -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_vault_browse() {
        use super::super::browse::At;
        let Ok(addr) = std::env::var("DBINE_TEST_VAULT") else {
            return;
        };
        let req = Request {
            address: &addr,
            path: "",
            key: "",
            namespace: "",
            kv_version: KvVersion::Auto,
            ca_file: "",
        };
        let auth = Auth::Token(Some("dbine-root"));
        let ids = |l: super::super::browse::Listing| {
            l.entries.into_iter().map(|e| e.id).collect::<Vec<_>>()
        };
        let mounts = ids(browse(&req, &auth, At::default()).await.unwrap());
        assert!(
            mounts.contains(&"secret/".to_string()) && mounts.contains(&"old/".to_string()),
            "{mounts:?}"
        );
        assert!(ids(browse(
            &req,
            &auth,
            At {
                path: "secret/",
                ..Default::default()
            }
        )
        .await
        .unwrap())
        .contains(&"secret/dbine/".to_string()));
        assert!(ids(browse(
            &req,
            &auth,
            At {
                path: "secret/dbine/",
                ..Default::default()
            }
        )
        .await
        .unwrap())
        .contains(&"secret/dbine/pg".to_string()));
        assert_eq!(
            ids(browse(
                &req,
                &auth,
                At {
                    path: "old/team/",
                    ..Default::default()
                }
            )
            .await
            .unwrap()),
            ["old/team/pg"]
        );
        let l = browse(
            &req,
            &auth,
            At {
                item: Some("secret/dbine/pg"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(l.fields.unwrap(), ["password", "username"]);
        let db = super::super::vault_db::Source {
            req: super::super::vault_db::Request {
                address: &addr,
                namespace: "",
                ca_file: "",
                mount: "",
                role: "",
                static_role: false,
            },
            auth: Auth::Token(Some("dbine-root")),
        };
        assert!(ids(super::super::vault_db::browse(&db).await.unwrap()).contains(&"ro".to_string()));
    }

    #[test]
    fn token_order() {
        let dir = super::super::tests::temp_dir("vault-home");
        std::fs::write(dir.join(".vault-token"), "file-token\n").unwrap();
        let env_with = |k: &str| (k == "VAULT_TOKEN").then(|| "env-token".to_string());
        let no_env = |_: &str| None;
        assert_eq!(
            token(Some("kept"), env_with, Some(dir.clone())).unwrap(),
            "kept"
        );
        assert_eq!(
            token(None, env_with, Some(dir.clone())).unwrap(),
            "env-token"
        );
        assert_eq!(
            token(Some("  "), no_env, Some(dir.clone())).unwrap(),
            "file-token"
        );
        assert!(token(None, no_env, None)
            .unwrap_err()
            .contains("vault login"));
    }

    #[test]
    fn errors_by_status() {
        assert!(
            status_error(403, &json!({"errors": ["permission denied"]}), "secret/x")
                .contains("permiso denegado")
        );
        assert!(status_error(404, &Value::Null, "secret/x").contains("no existe"));
        assert_eq!(
            status_error(500, &json!({"errors": ["internal", "boom"]}), "s"),
            "Vault respondió 500: internal; boom"
        );
    }

    /// Against `dbine-test-vault` (dev mode):
    /// `docker run -d --name dbine-test-vault -p 28200:8200 -e VAULT_DEV_ROOT_TOKEN_ID=dbine-root hashicorp/vault`,
    /// a v2 secret at `secret/dbine/pg` and a v1 engine at `kv1/`; then
    /// `DBINE_TEST_VAULT=http://127.0.0.1:28200 cargo test -p dbine secret_sources::vault -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_vault() {
        let Ok(addr) = std::env::var("DBINE_TEST_VAULT") else {
            return;
        };
        let client = reqwest::Client::new();
        let req = |path: &'static str, key: &'static str, v: KvVersion| Request {
            address: Box::leak(addr.clone().into_boxed_str()),
            path,
            key,
            namespace: "",
            kv_version: v,
            ca_file: "",
        };
        assert_eq!(
            read_with(
                &client,
                &req("secret/dbine/pg", "password", KvVersion::Auto),
                "dbine-root"
            )
            .await
            .unwrap(),
            "pw"
        );
        assert_eq!(
            read_with(
                &client,
                &req("secret/dbine/pg", "password", KvVersion::V2),
                "dbine-root"
            )
            .await
            .unwrap(),
            "pw"
        );
        assert_eq!(
            read_with(
                &client,
                &req("kv1/dbine/pg", "password", KvVersion::Auto),
                "dbine-root"
            )
            .await
            .unwrap(),
            "pw-v1"
        );
        assert_eq!(
            read_with(
                &client,
                &req("kv1/dbine/pg", "password", KvVersion::V1),
                "dbine-root"
            )
            .await
            .unwrap(),
            "pw-v1"
        );
        assert!(read_with(
            &client,
            &req("secret/dbine/none", "password", KvVersion::Auto),
            "dbine-root"
        )
        .await
        .unwrap_err()
        .contains("no existe"));
        assert!(read_with(
            &client,
            &req("secret/dbine/pg", "nope", KvVersion::Auto),
            "dbine-root"
        )
        .await
        .unwrap_err()
        .contains("«nope»"));
        assert!(read_with(
            &client,
            &req("secret/dbine/pg", "password", KvVersion::Auto),
            "wrong-token"
        )
        .await
        .unwrap_err()
        .contains("permiso denegado"));
    }

    /// The account wizard's check against a mock: a token's lookup-self
    /// (name and policies, never the token), AppRole's login with its token
    /// revoked right after, and the refusals.
    #[tokio::test]
    async fn whoami_against_a_mock() {
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::{get as aget, post};
        use axum::{Json, Router};
        use std::sync::{Arc, Mutex};
        let revoked = Arc::new(Mutex::new(0u32));
        let r2 = revoked.clone();
        let tok = |h: &HeaderMap| {
            h.get("x-vault-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        let app = Router::new()
            .route("/v1/auth/approle/login", post(|Json(b): Json<Value>| async move {
                if b["role_id"] == "r-1" && b["secret_id"] == "s-1" {
                    (StatusCode::OK, Json(json!({ "auth": { "client_token": "ar-token", "renewable": true } })))
                } else {
                    (StatusCode::BAD_REQUEST, Json(json!({ "errors": ["invalid role or secret ID"] })))
                }
            }))
            .route("/v1/auth/token/lookup-self", aget(move |h: HeaderMap| async move {
                match tok(&h).as_str() {
                    "root" => (StatusCode::OK, Json(json!({ "data": { "display_name": "token", "id": "root", "policies": ["root"] } }))),
                    "ar-token" => (StatusCode::OK, Json(json!({ "data": { "display_name": "approle", "id": "ar-token", "policies": ["default", "dbine"], "identity_policies": ["team", "dbine"] } }))),
                    _ => (StatusCode::FORBIDDEN, Json(json!({ "errors": ["permission denied"] }))),
                }
            }))
            .route("/v1/auth/token/revoke-self", post(move |h: HeaderMap| {
                let r = r2.clone();
                async move {
                    if tok(&h) == "ar-token" {
                        *r.lock().unwrap() += 1;
                    }
                    StatusCode::NO_CONTENT
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let c = client("").unwrap();
        let conn = Conn {
            address: &addr,
            namespace: "",
        };
        let (name, policies) = whoami_with(&c, &conn, &Auth::Token(Some("root")))
            .await
            .unwrap();
        assert_eq!(
            (name.as_str(), policies),
            ("token", vec!["root".to_string()])
        );
        let e = whoami_with(&c, &conn, &Auth::Token(Some("nope")))
            .await
            .unwrap_err();
        assert!(e.contains("rechazó el token") && !e.contains("nope"), "{e}");
        let ar = Auth::AppRole {
            mount: "",
            role_id: "r-1",
            secret_id: Some("s-1"),
        };
        let (name, policies) = whoami_with(&c, &conn, &ar).await.unwrap();
        assert_eq!(
            (name.as_str(), policies.join(",")),
            ("approle", "dbine,default,team".to_string())
        );
        assert_eq!(
            *revoked.lock().unwrap(),
            1,
            "AppRole's token is revoked after the check"
        );
        let e = whoami_with(
            &c,
            &conn,
            &Auth::AppRole {
                mount: "",
                role_id: "r-1",
                secret_id: Some("wrong"),
            },
        )
        .await
        .unwrap_err();
        assert!(
            e.contains("role_id o el secret_id") && !e.contains("wrong"),
            "{e}"
        );
    }
}
