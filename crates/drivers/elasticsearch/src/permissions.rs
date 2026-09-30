//! What the login may do (`Session::permissions`). A cluster has no
//! databases: every check is cluster-wide.
//!
//! - Elasticsearch: `POST _security/user/_has_privileges` answers for the
//!   cluster privileges `monitor` (the profiler reads `_tasks`),
//!   `create_snapshot` (backup), `manage` (restoring a snapshot) and
//!   `manage_security`, through every role of the user.
//! - OpenSearch / Open Distro: the security plugin has no such check.
//!   `authinfo` gives the user's roles (`all_access`: everything); the
//!   profiler is proven by reading `_tasks` itself, and security by reading
//!   the internal users from the security REST API. Snapshots without
//!   `all_access` stay unknown (the roles' definitions are only readable
//!   with REST API access).
//!
//! Without security (Elasticsearch with it off, OpenSearch without the
//! plugin) everything is allowed. DBine's read-only mode isn't looked at:
//! this reports what the server grants the user, and the read-only mode
//! blocks the writes on its own. A check the server refuses leaves its
//! action unknown; only an unreachable server is an error.

use crate::{http, EsSession};
use dbine_driver::{Access, Permissions, Result};
use serde_json::{json, Value};

/// `Some(true/false)` per check; `None`: not answered.
#[derive(Debug, Clone, Default)]
pub(crate) struct Checks {
    pub monitor: Option<bool>,
    pub create_snapshot: Option<bool>,
    pub manage: Option<bool>,
    pub manage_security: Option<bool>,
    pub opensearch: bool,
}

impl Checks {
    fn all(v: bool) -> Self {
        Checks { monitor: Some(v), create_snapshot: Some(v), manage: Some(v), manage_security: Some(v), ..Default::default() }
    }
}

pub(crate) fn decide(c: &Checks) -> Permissions {
    let access = |v: Option<bool>, missing: &str| v.map_or(Access::Unknown, |ok| Access::check(ok, missing));
    let (monitor, security) = if c.opensearch {
        ("cluster:monitor/tasks/lists (cluster_monitor)", "acceso a la API REST de seguridad (all_access o security_rest_api_access)")
    } else {
        ("monitor", "manage_security")
    };
    Permissions {
        backup: access(c.create_snapshot, "create_snapshot"),
        restore: access(c.manage, "manage"),
        profiler: access(c.monitor, monitor),
        manage_security: access(c.manage_security, security),
        ..Default::default()
    }
}

/// A server without the security feature says so instead of answering.
pub(crate) fn security_off(status: u16, body: &str) -> bool {
    (status == 400 && body.contains("no handler found"))
        || body.contains("Security must be explicitly enabled")
        || body.contains("security is not enabled")
}

/// `_has_privileges`' answer for the cluster privileges.
pub(crate) fn cluster_privileges(body: &str) -> Option<Checks> {
    let v: Value = serde_json::from_str(body).ok()?;
    let c = v.get("cluster")?;
    let one = |k: &str| c.get(k).and_then(Value::as_bool);
    Some(Checks {
        monitor: one("monitor"),
        create_snapshot: one("create_snapshot"),
        manage: one("manage"),
        manage_security: one("manage_security"),
        ..Default::default()
    })
}

/// 2xx → allowed, 401/403 → not, anything else unanswered.
fn answered(status: u16) -> Option<bool> {
    match status {
        200..=299 => Some(true),
        401 | 403 => Some(false),
        _ => None,
    }
}

async fn send(s: &EsSession, method: &str, path: &str, body: Option<Value>) -> Result<(u16, String)> {
    let mut rb = s.request(method, path);
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    http::send(rb).await
}

async fn elastic(s: &EsSession) -> Result<Checks> {
    let body = json!({ "cluster": ["monitor", "create_snapshot", "manage", "manage_security"] });
    let (status, text) = send(s, "POST", "/_security/user/_has_privileges", Some(body)).await?;
    if status < 300 {
        return Ok(cluster_privileges(&text).unwrap_or_default());
    }
    if security_off(status, &text) {
        return Ok(Checks::all(true));
    }
    tracing::debug!("elasticsearch: permissions check refused: HTTP {status}");
    Ok(Checks::default())
}

async fn opensearch(s: &EsSession) -> Result<Checks> {
    let prefix = if s.opendistro { "/_opendistro/_security" } else { "/_plugins/_security" };
    let (status, text) = send(s, "GET", &format!("{prefix}/authinfo"), None).await?;
    if status >= 300 {
        if security_off(status, &text) {
            return Ok(Checks::all(true));
        }
        tracing::debug!("opensearch: permissions check refused: HTTP {status}");
        return Ok(Checks::default());
    }
    let roles: Vec<String> = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("roles").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.as_str().map(str::to_string))
        .collect();
    if roles.iter().any(|r| r == "all_access") {
        return Ok(Checks::all(true));
    }
    let users_path = format!("{prefix}/api/internalusers");
    let (tasks, users) =
        tokio::join!(send(s, "GET", "/_tasks?actions=indices:data/read/*", None), send(s, "GET", &users_path, None));
    Ok(Checks { monitor: answered(tasks?.0), manage_security: answered(users?.0), ..Default::default() })
}

pub(crate) async fn check(s: &EsSession) -> Result<Permissions> {
    let mut c = if s.opensearch { opensearch(s).await? } else { elastic(s).await? };
    c.opensearch = s.opensearch;
    Ok(decide(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    #[test]
    fn a_superuser_may_do_everything_offered() {
        let p = decide(&Checks::all(true));
        assert_eq!((&p.backup, &p.restore, &p.profiler, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        // No databases nor sessions to end.
        assert_eq!((&p.create_database, &p.drop_database, &p.kill_session), (&Access::Unknown, &Access::Unknown, &Access::Unknown));
    }

    #[test]
    fn missing_privileges_by_name() {
        let p = decide(&Checks::all(false));
        assert!(denied(&p.backup, "create_snapshot"));
        assert!(denied(&p.restore, "manage"));
        assert!(denied(&p.profiler, "monitor"));
        assert!(denied(&p.manage_security, "manage_security"));
        let c = Checks { monitor: Some(false), manage_security: Some(false), opensearch: true, ..Default::default() };
        let p = decide(&c);
        assert!(denied(&p.profiler, "cluster_monitor"));
        assert!(denied(&p.manage_security, "API REST de seguridad"));
        assert_eq!((&p.backup, &p.restore), (&Access::Unknown, &Access::Unknown));
    }

    #[test]
    fn read_only_mode_is_not_a_missing_privilege() {
        // `decide` has no read-only input: the server's answer is all.
        let p = decide(&Checks::all(true));
        assert!(![&p.backup, &p.restore, &p.profiler, &p.manage_security].iter().any(|a| denied(a, "solo lectura")));
    }

    #[test]
    fn parses_has_privileges() {
        let c = cluster_privileges(
            r#"{"username":"ana","has_all_requested":false,"cluster":{"monitor":true,"manage_security":false,"create_snapshot":false,"manage":false},"index":{}}"#,
        )
        .unwrap();
        assert_eq!((c.monitor, c.create_snapshot, c.manage, c.manage_security), (Some(true), Some(false), Some(false), Some(false)));
        assert!(cluster_privileges("{}").is_none());
    }

    #[test]
    fn recognizes_a_server_without_security() {
        assert!(security_off(400, r#"{"error":"no handler found for uri [/_security/user/_has_privileges] and method [POST]"}"#));
        assert!(security_off(500, r#"{"error":{"reason":"Security must be explicitly enabled when using a [basic] license."}}"#));
        assert!(!security_off(403, r#"{"error":{"type":"security_exception"}}"#));
        assert_eq!((answered(200), answered(403), answered(401), answered(500)), (Some(true), Some(false), Some(false), None));
    }
}
