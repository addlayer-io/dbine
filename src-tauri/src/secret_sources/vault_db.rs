//! HashiCorp Vault's database secrets engine: a user and a password Vault
//! makes for this connection (`<mount>/creds/<role>`, with a lease), or a
//! static role's current ones (`<mount>/static-creds/<role>`, no lease).
//! Both come from one response, so the user and the password always match.
//!
//! The lease lives as long as the connection is open:
//!
//! - A saved connection's credentials are held here, in memory only, by
//!   (connection, exact definition). Every session the connection opens
//!   (explorer, editor tabs, a reconnect) reuses them while the lease has
//!   time left; near the end it's renewed when Vault allows, else new
//!   credentials are asked for and the old lease is kept aside until the
//!   connection closes (sessions opened with it may still be using it).
//! - A renewer task renews the lease (and an AppRole token) before it ends,
//!   as long as it's held and the connection has a session open (else it
//!   revokes it); it stops at the lease's maximum TTL.
//! - Disconnecting, editing or deleting the connection, or closing DBine,
//!   revokes every lease of it (`sys/leases/revoke`, best effort) and the
//!   AppRole token that made it.
//! - Credentials asked by the form (Test, Test connection) aren't held:
//!   Test revokes them right away, and so does a test connection once its
//!   session is closed ([`form_scope`]); one asked outside that scope is
//!   revoked two minutes later.
//!
//! The lease ID, the token and the credentials never go to disk or to a log;
//! errors carry Vault's messages, never a value.

use super::vault::{self, Auth, Conn, Login};
use super::Resolved;
use dbine_core::secret_sources::wipe;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

pub struct Request<'a> {
    pub address: &'a str,
    pub namespace: &'a str,
    pub ca_file: &'a str,
    /// Empty: `database`.
    pub mount: &'a str,
    pub role: &'a str,
    pub static_role: bool,
}

/// Credentials Vault gave; wiped on drop.
pub struct Issued {
    pub username: String,
    pub password: String,
    /// Empty for a static role.
    pub lease_id: String,
    /// The lease's duration (a static role: until the next rotation).
    pub ttl: Duration,
    pub renewable: bool,
}

impl Drop for Issued {
    fn drop(&mut self) {
        wipe(&mut self.username);
        wipe(&mut self.password);
        wipe(&mut self.lease_id);
    }
}

fn creds_path(req: &Request<'_>) -> String {
    let mount = Some(req.mount.trim().trim_matches('/')).filter(|m| !m.is_empty()).unwrap_or("database");
    let which = if req.static_role { "static-creds" } else { "creds" };
    format!("{}/{which}/{}", vault::encode(&vault::segments(mount)), super::cloud_auth::uri_encode(req.role.trim()))
}

/// Ask Vault for the role's credentials.
pub async fn issue(client: &reqwest::Client, conn: &Conn<'_>, token: &str, req: &Request<'_>) -> Result<Issued, String> {
    let api = creds_path(req);
    let (status, json) = vault::call(client, conn, token, reqwest::Method::GET, &api, None).await;
    let status = status?;
    if status != 200 {
        return Err(issue_error(status, &json, req));
    }
    parse_issued(&json, req.static_role)
}

pub fn issue_error(status: u16, json: &Value, req: &Request<'_>) -> String {
    let role = req.role.trim();
    let errors = json.get("errors").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).unwrap_or_default();
    let errors = super::exec::first_lines(&errors, 300);
    match status {
        403 => format!("Vault: permiso denegado para pedir las credenciales del rol «{role}» (el token venció o su política no lo permite)"),
        400 | 404 if errors.contains("unknown role") || errors.contains("not found") || errors.is_empty() => format!("Vault: no existe el rol «{role}» en ese motor de bases de datos"),
        _ => vault::status_error(status, json, role),
    }
}

pub fn parse_issued(json: &Value, static_role: bool) -> Result<Issued, String> {
    let data = json.get("data").filter(|d| d.is_object()).ok_or("Vault no devolvió credenciales")?;
    let text = |k: &str| data.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let (Some(username), Some(password)) = (text("username"), text("password")) else {
        return Err("Vault no devolvió el usuario y la contraseña del rol".into());
    };
    let secs = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
    let (lease_id, ttl, renewable) = if static_role {
        (String::new(), secs(data.get("ttl")), false)
    } else {
        let id = json.get("lease_id").and_then(Value::as_str).unwrap_or("").to_string();
        (id, secs(json.get("lease_duration")), json.get("renewable").and_then(Value::as_bool).unwrap_or(false))
    };
    Ok(Issued { username, password, lease_id, ttl: Duration::from_secs(ttl), renewable })
}

/// Renew a lease for `increment`: its new duration and whether it can be
/// renewed again.
pub async fn renew(client: &reqwest::Client, conn: &Conn<'_>, token: &str, lease_id: &str, increment: Duration) -> Result<(Duration, bool), String> {
    let body = serde_json::json!({ "lease_id": lease_id, "increment": increment.as_secs() });
    let (status, json) = vault::call(client, conn, token, reqwest::Method::PUT, "sys/leases/renew", Some(&body)).await;
    match status? {
        200 => Ok((
            Duration::from_secs(json.get("lease_duration").and_then(Value::as_u64).unwrap_or(0)),
            json.get("renewable").and_then(Value::as_bool).unwrap_or(false),
        )),
        s => Err(vault::status_error(s, &json, "sys/leases/renew")),
    }
}

/// Revoke a lease (Vault drops the database user).
pub async fn revoke(client: &reqwest::Client, conn: &Conn<'_>, token: &str, lease_id: &str) -> Result<(), String> {
    let body = serde_json::json!({ "lease_id": lease_id });
    let (status, json) = vault::call(client, conn, token, reqwest::Method::PUT, "sys/leases/revoke", Some(&body)).await;
    match status? {
        200 | 204 => Ok(()),
        s => Err(vault::status_error(s, &json, "sys/leases/revoke")),
    }
}

// ---- the leases held -----------------------------------------------------------

/// A lease held for a connection (or for a form's test, until it's revoked).
struct Held {
    resolved: Arc<Resolved>,
    lease_id: String,
    /// When the lease ends.
    expires: Instant,
    /// Its duration when issued (sets how early it's renewed).
    ttl: Duration,
    renewable: bool,
    address: String,
    namespace: String,
    ca_file: String,
    /// The token that made it (renews and revokes it). Vault revokes a
    /// lease with its token, so an AppRole token is kept alive with it.
    token: String,
    owned_token: bool,
    token_renewable: bool,
}

impl Drop for Held {
    fn drop(&mut self) {
        wipe(&mut self.token);
        wipe(&mut self.lease_id);
    }
}

impl Held {
    fn conn(&self) -> Conn<'_> {
        Conn { address: &self.address, namespace: &self.namespace }
    }

    /// Revoke the lease, then the AppRole token (best effort; Vault would
    /// end both at their TTL anyway).
    async fn revoke(self) {
        let Ok(client) = vault::client(&self.ca_file) else { return };
        if !self.lease_id.is_empty() {
            if let Err(e) = revoke(&client, &self.conn(), &self.token, &self.lease_id).await {
                tracing::warn!("vault lease revoke failed: {e}");
            }
        }
        if self.owned_token {
            let _ = vault::call(&client, &self.conn(), &self.token, reqwest::Method::POST, "auth/token/revoke-self", None).await;
        }
    }
}

#[derive(Default)]
struct Leases {
    /// The current lease, by `connection \0 digest` (fill's cache key).
    current: HashMap<String, Held>,
    /// Leases replaced while their connection stayed open, by connection.
    retired: Vec<(String, Held)>,
    /// A form's, until their revocation (a test connection's).
    form: Vec<(u64, Held)>,
}

static LEASES: LazyLock<Mutex<Leases>> = LazyLock::new(Mutex::default);
/// One acquisition at a time per key: sessions opening together share one
/// lease instead of each asking for one.
static GATES: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Mutex::default);

fn leases() -> std::sync::MutexGuard<'static, Leases> {
    LEASES.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long before its end a lease is renewed or replaced: a third of its
/// duration, at most five minutes, at least ten seconds.
pub fn margin(ttl: Duration) -> Duration {
    (ttl / 3).clamp(Duration::from_secs(10), Duration::from_secs(300))
}

/// How long a form's credentials asked outside a [`form_scope`] are kept
/// (and the fallback if the scope is dropped before it ends).
const FORM_GRACE: Duration = Duration::from_secs(120);

tokio::task_local! {
    /// The leases a form's test connection got (see [`form_scope`]).
    static FORM_SCOPE: std::cell::RefCell<Vec<u64>>;
}

/// Run `f` (a form's test connection: resolve, connect, close) and revoke
/// the form's leases it got as soon as it ends, instead of after
/// [`FORM_GRACE`].
pub async fn form_scope<F: std::future::Future>(f: F) -> F::Output {
    let (out, ids) = FORM_SCOPE
        .scope(std::cell::RefCell::new(Vec::new()), async {
            let out = f.await;
            (out, FORM_SCOPE.with(|s| s.take()))
        })
        .await;
    for id in ids {
        release_form(id).await;
    }
    out
}

/// The definition's parts the lease store needs.
pub struct Source<'a> {
    pub req: Request<'a>,
    pub auth: Auth<'a>,
}

/// The connection of a `connection \0 digest` key.
fn connection_of(key: &str) -> &str {
    key.split('\u{0}').next().unwrap_or("")
}

/// The credentials for `key`: the held ones while their lease has time
/// left (renewed near its end when Vault allows), else new ones. `hold`:
/// a saved connection (held until it closes); else a form's (revoked when
/// its [`form_scope`] ends, else after [`FORM_GRACE`]).
pub async fn acquire(key: &str, hold: bool, src: &Source<'_>) -> Result<Arc<Resolved>, String> {
    if !hold {
        // A form's key is used once: no gate to share.
        return issue_new(key, false, src).await;
    }
    let gate = GATES.lock().unwrap_or_else(|e| e.into_inner()).entry(key.to_string()).or_default().clone();
    let _turn = gate.lock().await;
    if let Some(r) = reuse(key).await {
        return Ok(r);
    }
    issue_new(key, true, src).await
}

/// New credentials for `key`, held (a saved connection) or a form's.
async fn issue_new(key: &str, hold: bool, src: &Source<'_>) -> Result<Arc<Resolved>, String> {
    let client = vault::client(src.req.ca_file)?;
    let conn = Conn { address: src.req.address, namespace: src.req.namespace };
    let login = vault::login(&client, &conn, &src.auth).await?;
    let issued = match issue(&client, &conn, &login.token, &src.req).await {
        Ok(i) => i,
        Err(e) => {
            login.done(&client, &conn).await;
            return Err(e);
        }
    };
    let held = held_of(&issued, &login, src);
    let resolved = held.resolved.clone();
    let lease_id = held.lease_id.clone();
    if hold {
        let old = leases().current.insert(key.to_string(), held);
        if let Some(old) = old {
            retire(key, old);
        }
        if !lease_id.is_empty() {
            tokio::spawn(renewer(key.to_string(), lease_id));
        }
    } else if !lease_id.is_empty() || login.owned {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        leases().form.push((id, held));
        // In a test connection's scope: revoked when it ends. The timer
        // stays as the fallback (a no-op once the lease is gone).
        let _ = FORM_SCOPE.try_with(|s| s.borrow_mut().push(id));
        tokio::spawn(async move {
            tokio::time::sleep(FORM_GRACE).await;
            release_form(id).await;
        });
    }
    Ok(resolved)
}

fn held_of(i: &Issued, login: &Login, src: &Source<'_>) -> Held {
    Held {
        resolved: Arc::new(Resolved { value: i.password.clone(), username: Some(i.username.clone()) }),
        lease_id: i.lease_id.clone(),
        expires: Instant::now() + i.ttl,
        ttl: i.ttl,
        renewable: i.renewable,
        address: src.req.address.to_string(),
        namespace: src.req.namespace.to_string(),
        ca_file: src.req.ca_file.to_string(),
        token: login.token.clone(),
        owned_token: login.owned,
        token_renewable: login.renewable,
    }
}

/// A replaced lease: a static role's has nothing to revoke; a dynamic one
/// waits for its connection to close.
fn retire(key: &str, old: Held) {
    if !old.lease_id.is_empty() || old.owned_token {
        leases().retired.push((connection_of(key).to_string(), old));
    }
}

/// The held credentials, if their lease has time left (renewing it first
/// when it's close to the end).
async fn reuse(key: &str) -> Option<Arc<Resolved>> {
    let (resolved, left, ttl, renewable, lease_id, call) = {
        let l = leases();
        let h = l.current.get(key)?;
        let left = h.expires.saturating_duration_since(Instant::now());
        (h.resolved.clone(), left, h.ttl, h.renewable, h.lease_id.clone(), (h.address.clone(), h.namespace.clone(), h.ca_file.clone(), h.token.clone()))
    };
    let mut token = call.3;
    let out = if left > margin(ttl) {
        Some(resolved)
    } else if renewable && !lease_id.is_empty() {
        let renewed = match vault::client(&call.2) {
            Ok(client) => renew(&client, &Conn { address: &call.0, namespace: &call.1 }, &token, &lease_id, ttl).await.ok(),
            Err(_) => None,
        };
        match renewed {
            Some((new_ttl, again)) if new_ttl > margin(ttl) => {
                if let Some(h) = leases().current.get_mut(key).filter(|h| h.lease_id == lease_id) {
                    h.expires = Instant::now() + new_ttl;
                    h.renewable = again;
                }
                Some(resolved)
            }
            _ => None,
        }
    } else {
        None
    };
    wipe(&mut token);
    if out.is_none() {
        let old = leases().current.remove(key);
        if let Some(old) = old {
            retire(key, old);
        }
    }
    out
}

/// Renews the lease held under `key` before it ends, and the AppRole token
/// that made it, while it's held and Vault lets it.
async fn renewer(key: String, lease_id: String) {
    loop {
        let wait = {
            let l = leases();
            let Some(h) = l.current.get(&key).filter(|h| h.lease_id == lease_id) else { return };
            if !h.renewable {
                return;
            }
            h.expires.saturating_duration_since(Instant::now()).saturating_sub(margin(h.ttl))
        };
        tokio::time::sleep(wait.max(Duration::from_secs(5))).await;
        // Nothing of the connection is open any more (a scheduled task's or
        // the assistant's session that ended): revoke instead of renewing.
        let connection = connection_of(&key).to_string();
        if crate::state::connection_in_use(&connection) == Some(false) {
            release(&connection).await;
            return;
        }
        let (address, namespace, ca_file, mut token, ttl, owned, token_renewable) = {
            let l = leases();
            let Some(h) = l.current.get(&key).filter(|h| h.lease_id == lease_id) else { return };
            (h.address.clone(), h.namespace.clone(), h.ca_file.clone(), h.token.clone(), h.ttl, h.owned_token, h.token_renewable)
        };
        let Ok(client) = vault::client(&ca_file) else { return };
        let conn = Conn { address: &address, namespace: &namespace };
        if owned && token_renewable {
            let _ = vault::call(&client, &conn, &token, reqwest::Method::POST, "auth/token/renew-self", None).await;
        }
        let r = renew(&client, &conn, &token, &lease_id, ttl).await;
        wipe(&mut token);
        let Ok((new_ttl, again)) = r else { return };
        let mut l = leases();
        let Some(h) = l.current.get_mut(&key).filter(|h| h.lease_id == lease_id) else { return };
        h.expires = Instant::now() + new_ttl;
        h.renewable = again;
        // At its maximum TTL: the next session asks for new credentials.
        if new_ttl <= margin(ttl) {
            return;
        }
    }
}

async fn release_form(id: u64) {
    let found = {
        let mut l = leases();
        l.form.iter().position(|(i, _)| *i == id).map(|i| l.form.swap_remove(i).1)
    };
    if let Some(h) = found {
        h.revoke().await;
    }
}

/// Revoke every lease of a connection (disconnect, edit, delete).
pub async fn release(connection_id: &str) {
    let prefix = format!("{connection_id}\u{0}");
    let taken: Vec<Held> = {
        let mut l = leases();
        let keys: Vec<String> = l.current.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
        let mut out: Vec<Held> = keys.iter().filter_map(|k| l.current.remove(k)).collect();
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut l.retired).into_iter().partition(|(c, _)| c == connection_id);
        l.retired = rest;
        out.extend(mine.into_iter().map(|(_, h)| h));
        out
    };
    GATES.lock().unwrap_or_else(|e| e.into_inner()).retain(|k, _| !k.starts_with(&prefix));
    for h in taken {
        h.revoke().await;
    }
}

/// Revoke every lease held (DBine is closing).
pub async fn release_all() {
    let taken: Vec<Held> = {
        let mut l = leases();
        let mut out: Vec<Held> = l.current.drain().map(|(_, h)| h).collect();
        out.extend(l.retired.drain(..).map(|(_, h)| h));
        out.extend(l.form.drain(..).map(|(_, h)| h));
        out
    };
    let mut all = tokio::task::JoinSet::new();
    for h in taken {
        all.spawn(h.revoke());
    }
    while all.join_next().await.is_some() {}
}

/// Get credentials once and revoke them right away (the form's Test).
pub async fn test(src: &Source<'_>) -> Result<(String, Option<String>), String> {
    let client = vault::client(src.req.ca_file)?;
    let conn = Conn { address: src.req.address, namespace: src.req.namespace };
    let login = vault::login(&client, &conn, &src.auth).await?;
    let r = issue(&client, &conn, &login.token, &src.req).await;
    if let Ok(i) = &r {
        if !i.lease_id.is_empty() {
            let _ = revoke(&client, &conn, &login.token, &i.lease_id).await;
        }
    }
    login.done(&client, &conn).await;
    let i = r?;
    Ok((i.password.clone(), Some(i.username.clone())))
}

/// The engine's roles (`roles`) and static roles (`static-roles`): names
/// only, each under the list it's in.
pub async fn browse(src: &Source<'_>) -> Result<super::browse::Listing, String> {
    let client = vault::client(src.req.ca_file)?;
    let conn = Conn { address: src.req.address, namespace: src.req.namespace };
    let login = vault::login(&client, &conn, &src.auth).await?;
    let r = browse_with(&client, &conn, &login.token, &src.req).await;
    login.done(&client, &conn).await;
    r
}

pub async fn browse_with(client: &reqwest::Client, conn: &Conn<'_>, token: &str, req: &Request<'_>) -> Result<super::browse::Listing, String> {
    use super::browse::{Entry, Listing};
    let mount = Some(req.mount.trim().trim_matches('/')).filter(|m| !m.is_empty()).unwrap_or("database");
    let mut out = Vec::new();
    for which in ["roles", "static-roles"] {
        let api = format!("{}/{which}?list=true", vault::encode(&vault::segments(mount)));
        let (status, json) = vault::call(client, conn, token, reqwest::Method::GET, &api, None).await;
        match status? {
            200 => {
                let keys = json.pointer("/data/keys").and_then(Value::as_array).cloned().unwrap_or_default();
                out.extend(keys.iter().filter_map(Value::as_str).map(|k| Entry::item(k, k, which, "")));
            }
            // None of that kind (or no static roles in this Vault).
            404 | 405 => {}
            403 => return Err(format!("Vault: permiso denegado para listar los roles de «{mount}» (la política tiene que permitir list en {mount}/{which})")),
            s => return Err(vault::status_error(s, &json, &format!("{mount}/{which}"))),
        }
    }
    out.sort_by(|a, b| (a.path.as_str(), a.name.to_lowercase()).cmp(&(b.path.as_str(), b.name.to_lowercase())));
    Ok(Listing::complete(out))
}

/// How many leases are held (tests).
#[cfg(test)]
pub fn held_count(connection_id: &str) -> (usize, usize) {
    let prefix = format!("{connection_id}\u{0}");
    let l = leases();
    (l.current.keys().filter(|k| k.starts_with(&prefix)).count(), l.retired.iter().filter(|(c, _)| c == connection_id).count())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    fn req(role: &'static str, static_role: bool) -> Request<'static> {
        Request { address: "http://v", namespace: "", ca_file: "", mount: "", role, static_role }
    }

    #[test]
    fn creds_paths() {
        assert_eq!(creds_path(&req("readonly", false)), "database/creds/readonly");
        assert_eq!(creds_path(&req("app", true)), "database/static-creds/app");
        let r = Request { mount: "/pg/prod/", ..req("a b", false) };
        assert_eq!(creds_path(&r), "pg/prod/creds/a%20b");
    }

    #[test]
    fn credentials_out_of_responses() {
        let dynamic = json!({ "lease_id": "database/creds/ro/abc", "lease_duration": 3600, "renewable": true, "data": { "username": "v-tok-ro-1", "password": "A1a-xyz" } });
        let i = parse_issued(&dynamic, false).unwrap();
        assert_eq!((i.username.as_str(), i.password.as_str(), i.lease_id.as_str(), i.ttl.as_secs(), i.renewable), ("v-tok-ro-1", "A1a-xyz", "database/creds/ro/abc", 3600, true));
        let fixed = json!({ "lease_id": "", "lease_duration": 0, "data": { "username": "app", "password": "rotated", "ttl": 86000, "last_vault_rotation": "2026-10-18T10:00:00Z" } });
        let i = parse_issued(&fixed, true).unwrap();
        assert_eq!((i.lease_id.as_str(), i.ttl.as_secs(), i.renewable), ("", 86000, false));
        assert!(parse_issued(&json!({ "data": { "username": "x" } }), false).is_err());
        assert!(parse_issued(&Value::Null, false).is_err());
    }

    #[test]
    fn issue_errors_never_carry_values() {
        let r = req("ro", false);
        assert!(issue_error(403, &json!({"errors":["permission denied"]}), &r).contains("permiso denegado"));
        assert!(issue_error(400, &json!({"errors":["unknown role: ro"]}), &r).contains("no existe el rol «ro»"));
        assert_eq!(issue_error(500, &json!({"errors":["boom"]}), &r), "Vault respondió 500: boom");
    }

    #[test]
    fn margins() {
        assert_eq!(margin(Duration::from_secs(3600)), Duration::from_secs(300));
        assert_eq!(margin(Duration::from_secs(60)), Duration::from_secs(20));
        assert_eq!(margin(Duration::from_secs(5)), Duration::from_secs(10));
    }
}
