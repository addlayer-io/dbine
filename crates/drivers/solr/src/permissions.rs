//! What the login may do (`Session::permissions`), from security.json's
//! rule-based authorization. Collections and cores live under the server:
//! every check is server-wide.
//!
//! `/admin/info/system` says which authorization plugin runs and, for the
//! rule-based one, the user's roles and the permissions they hold. A
//! request is decided by the first permission that matches it, and one
//! that matches nothing is allowed, so a permission not held proves
//! nothing: only the whole list (`/admin/authorization`, readable with
//! `security-read`) does.
//!
//! - backup / restore (SolrCloud, Collections API): `collection-admin-edit`.
//!   Standalone's replication handler has no predefined permission: unknown.
//! - security: `security-edit`.
//!
//! Without an authorization plugin everything is allowed. DBine's read-only
//! mode isn't looked at: this reports what the server grants the user, and
//! the read-only mode blocks the writes on its own. Solr has no profiler, no sessions to end and
//! no databases. A check the server refuses leaves its action unknown;
//! only an unreachable server is an error.

use crate::SolrSession;
use dbine_driver::{Access, Permissions, Result};
use dbine_driver_elasticsearch::http;
use serde_json::Value;

/// Solr's predefined permissions (any other name is a custom one, matched
/// by its path).
const PREDEFINED: &[&str] = &[
    "security-edit",
    "security-read",
    "schema-edit",
    "schema-read",
    "config-edit",
    "config-read",
    "core-admin-read",
    "core-admin-edit",
    "collection-admin-read",
    "collection-admin-edit",
    "update",
    "read",
    "health",
    "metrics-read",
    "metrics-history-read",
    "filestore-read",
    "filestore-write",
    "package-edit",
    "package-read",
    "zk-read",
    "all",
];

/// The authorization in force.
#[derive(Debug, Clone)]
pub(crate) enum Authz {
    /// No authorization plugin: every authenticated user may do anything.
    Off,
    /// Rule-based: the user's roles and the permissions they hold, and the
    /// whole list of permissions when readable.
    RuleBased { roles: Vec<String>, held: Vec<String>, config: Option<Vec<Value>> },
    /// Another plugin, or the server didn't say.
    Unknown,
}

fn strs(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// From `/admin/info/system`'s `security`.
pub(crate) fn authz(system: &Value) -> Authz {
    let Some(sec) = system.get("security") else { return Authz::Off };
    match sec.get("authorizationPlugin").and_then(Value::as_str) {
        None => Authz::Off,
        Some(p) if p.ends_with(".RuleBasedAuthorizationPlugin") => Authz::RuleBased {
            roles: strs(sec.get("roles")),
            held: strs(sec.get("permissions")),
            config: None,
        },
        Some(_) => Authz::Unknown,
    }
}

/// Whether `roles` may make a request governed by the predefined permission
/// `name`, walking the permissions in order as Solr does; `None` when a
/// custom permission or a restricted one (by collection, method or params)
/// might match first.
pub(crate) fn decided_by(config: &[Value], roles: &[String], name: &str) -> Option<bool> {
    for p in config {
        let pname = p.get("name").and_then(Value::as_str).unwrap_or_default();
        let custom = !PREDEFINED.contains(&pname) || p.get("path").is_some();
        if custom {
            return None;
        }
        if pname != name && pname != "all" {
            continue;
        }
        if ["collection", "method", "params"].iter().any(|k| p.get(*k).is_some_and(|v| !v.is_null())) {
            return None;
        }
        let allowed = match p.get("role") {
            None | Some(Value::Null) => true,
            role => {
                let want = strs(role);
                want.iter().any(|r| r == "*" || roles.contains(r))
            }
        };
        return Some(allowed);
    }
    // No permission matches: allowed.
    Some(true)
}

fn may(a: &Authz, name: &str) -> Option<bool> {
    match a {
        Authz::Off => Some(true),
        Authz::Unknown => None,
        Authz::RuleBased { roles, held, config } => match config {
            Some(c) => decided_by(c, roles, name),
            None if held.iter().any(|h| h == name) => Some(true),
            None => None,
        },
    }
}

pub(crate) fn decide(a: &Authz, cloud: bool) -> Permissions {
    let access = |name: &str| may(a, name).map_or(Access::Unknown, |ok| Access::check(ok, name));
    let snapshots = if cloud {
        access("collection-admin-edit")
    } else if matches!(a, Authz::Off) {
        Access::Allowed
    } else {
        Access::Unknown
    };
    Permissions {
        backup: snapshots.clone(),
        restore: snapshots,
        manage_security: access("security-edit"),
        ..Default::default()
    }
}

/// A JSON answer; `None` when the server refused it.
async fn get(s: &SolrSession, path: &str) -> Result<Option<Value>> {
    let (status, body) = http::send(s.client.get(format!("{}{path}", s.base))).await?;
    if status >= 300 {
        return Ok(None);
    }
    Ok(serde_json::from_str(&body).ok())
}

pub(crate) async fn check(s: &SolrSession) -> Result<Permissions> {
    let mut a = match get(s, "/solr/admin/info/system?wt=json").await? {
        Some(system) => authz(&system),
        None => Authz::Unknown,
    };
    if let Authz::RuleBased { config, .. } = &mut a {
        *config = get(s, "/solr/admin/authorization")
            .await?
            .and_then(|v| v.pointer("/authorization/permissions").and_then(Value::as_array).cloned());
    }
    Ok(decide(&a, s.cloud))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    fn rule_based(roles: &[&str], held: &[&str], config: Option<Value>) -> Authz {
        Authz::RuleBased {
            roles: roles.iter().map(|r| r.to_string()).collect(),
            held: held.iter().map(|r| r.to_string()).collect(),
            config: config.map(|c| c.as_array().unwrap().clone()),
        }
    }

    fn standard() -> Value {
        json!([
            { "name": "read", "role": "reader" },
            { "name": "security-edit", "role": "admin" },
            { "name": "collection-admin-edit", "role": ["admin", "ops"] },
            { "name": "all", "role": "admin" },
        ])
    }

    #[test]
    fn without_authorization_everything_is_allowed() {
        let a = authz(&json!({ "security": { "authenticationPlugin": "org.apache.solr.security.BasicAuthPlugin" } }));
        assert!(matches!(a, Authz::Off));
        let p = decide(&a, true);
        assert_eq!((&p.backup, &p.restore, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!(decide(&a, false).backup, Access::Allowed);
        assert!(matches!(authz(&json!({})), Authz::Off));
        assert!(matches!(authz(&json!({ "security": { "authorizationPlugin": "x.ExternalRoleRuleBasedAuthorizationPlugin" } })), Authz::Unknown));
    }

    #[test]
    fn the_whole_list_decides() {
        let p = decide(&rule_based(&["reader"], &["read"], Some(standard())), true);
        assert!(denied(&p.backup, "collection-admin-edit") && denied(&p.restore, "collection-admin-edit"));
        assert!(denied(&p.manage_security, "security-edit"));
        let p = decide(&rule_based(&["ops"], &["collection-admin-edit"], Some(standard())), true);
        assert_eq!(p.backup, Access::Allowed);
        assert!(p.manage_security.is_denied());
        // Standalone: the replication handler has no predefined permission.
        assert_eq!(decide(&rule_based(&["admin"], &[], Some(standard())), false).backup, Access::Unknown);
    }

    #[test]
    fn first_match_wins_and_custom_ones_are_unknown() {
        let roles = ["x".to_string()];
        // Nothing matches: allowed.
        assert_eq!(decided_by(&[json!({ "name": "read", "role": "admin" })], &roles, "security-edit"), Some(true));
        // "all" first decides for everything after it.
        let c = [json!({ "name": "all", "role": "*" }), json!({ "name": "security-edit", "role": "admin" })];
        assert_eq!(decided_by(&c, &roles, "security-edit"), Some(true));
        let c = [json!({ "name": "security-edit", "role": null })];
        assert_eq!(decided_by(&c, &roles, "security-edit"), Some(true));
        let c = [json!({ "name": "mine", "path": "/admin/authentication", "role": "x" }), json!({ "name": "all", "role": "admin" })];
        assert_eq!(decided_by(&c, &roles, "security-edit"), None);
        let c = [json!({ "name": "all", "collection": "c1", "role": "admin" })];
        assert_eq!(decided_by(&c, &roles, "collection-admin-edit"), None);
    }

    #[test]
    fn without_the_list_a_permission_not_held_proves_nothing() {
        let p = decide(&rule_based(&["reader"], &["read"], None), true);
        assert_eq!((&p.backup, &p.manage_security), (&Access::Unknown, &Access::Unknown));
        let p = decide(&rule_based(&["admin"], &["security-edit", "collection-admin-edit"], None), true);
        assert_eq!((&p.backup, &p.manage_security), (&Access::Allowed, &Access::Allowed));
        let p = decide(&Authz::Unknown, true);
        assert_eq!(p, Permissions::default());
    }

    #[test]
    fn reads_the_system_info() {
        let a = authz(&json!({ "security": {
            "authenticationPlugin": "org.apache.solr.security.BasicAuthPlugin",
            "authorizationPlugin": "org.apache.solr.security.RuleBasedAuthorizationPlugin",
            "username": "solr", "roles": ["admin"], "permissions": ["all", "security-edit"],
        } }));
        assert!(matches!(a, Authz::RuleBased { ref roles, ref held, .. } if roles == &["admin"] && held.len() == 2));
    }
}
