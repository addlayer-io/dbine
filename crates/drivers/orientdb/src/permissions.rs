//! What the login may do (`Session::permissions`).
//!
//! - create / drop a database: only server users (orientdb-server-config.xml
//!   or security.json, like `root`) with `database.create` /
//!   `database.drop`. A server user answers `GET /server` (`server.info`),
//!   and those that do are taken as able. A 401 there doesn't tell a
//!   database user from a server user without `server.info`: unknown.
//! - security: a server user, or a database user with an all-powerful role
//!   (`ALLOW_ALL_BUT` mode or every right on `*`, itself or inherited, like
//!   `admin`). Other roles may still hold rights on `OUser` / `ORole`
//!   (rules or security policies): unknown.
//!
//! OrientDB has no backups DBine runs, no profiler and no sessions to end.
//! DBine's read-only mode isn't looked at: this reports what the server
//! grants the user, and the read-only mode blocks the writes on its own. A
//! check the server refuses leaves its action unknown; only an unreachable
//! server is an error.

use crate::OrientSession;
use dbine_driver::{Access, Error, Permissions, Result};
use reqwest::Method;
use serde_json::Value;
use std::collections::HashMap;

type Record = Vec<(String, Value)>;

fn field<'a>(r: &'a Record, k: &str) -> Option<&'a Value> {
    r.iter().find(|(n, _)| n == k).map(|(_, v)| v).filter(|v| !v.is_null())
}

fn names(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// A role that can do everything: `ALLOW_ALL_BUT` mode or every right on `*`.
fn all_powerful(r: &Record) -> bool {
    field(r, "mode").and_then(Value::as_u64) == Some(1)
        || field(r, "rules").and_then(|x| x.get("*")).and_then(Value::as_u64) == Some(31)
}

/// Whether any of `held` is all-powerful, itself or through the roles it
/// inherits; `roles`: every `ORole` (name, mode, rules, parent).
pub(crate) fn admin_role(held: &[String], roles: &[Record]) -> bool {
    let by_name: HashMap<String, &Record> =
        roles.iter().filter_map(|r| Some((field(r, "name")?.as_str()?.to_string(), r))).collect();
    held.iter().any(|name| {
        let mut cur = Some(name.clone());
        // Inheritance chains are short; the bound guards against a cycle.
        for _ in 0..16 {
            let Some(r) = cur.as_ref().and_then(|n| by_name.get(n)) else { return false };
            if all_powerful(r) {
                return true;
            }
            cur = field(r, "parent").and_then(Value::as_str).map(str::to_string);
        }
        false
    })
}

/// `server_user`: `GET /server` answered (`None`: not known);
/// `db_admin`: the user holds an all-powerful role in the database.
pub(crate) fn decide(server_user: Option<bool>, db_admin: Option<bool>, database: bool) -> Permissions {
    let server = server_user == Some(true);
    let allowed_or_unknown = |yes: bool| if yes { Access::Allowed } else { Access::Unknown };
    Permissions {
        create_database: allowed_or_unknown(server),
        drop_database: allowed_or_unknown(server && database),
        manage_security: allowed_or_unknown(server || db_admin == Some(true)),
        ..Default::default()
    }
}

/// The user's roles in the session's database and whether one is
/// all-powerful; `None` when the records can't be read.
async fn database_admin(s: &OrientSession, user: &str) -> Result<Option<bool>> {
    let me = format!("SELECT roles.name AS roles FROM OUser WHERE name = {}", crate::ddl::string(user));
    let read = async {
        let held = s.command(&me, 1).await?.records.first().map(|r| names(field(r, "roles"))).unwrap_or_default();
        let roles = s.command("SELECT name, mode, inheritedRole.name AS parent, rules FROM ORole", -1).await?.records;
        Ok::<_, Error>(admin_role(&held, &roles))
    };
    match read.await {
        Ok(v) => Ok(Some(v)),
        Err(e @ Error::Connect(_)) => Err(e),
        Err(e) => {
            tracing::debug!("orientdb: permissions check refused: {e}");
            Ok(None)
        }
    }
}

pub(crate) async fn check(s: &OrientSession, database: Option<&str>) -> Result<Permissions> {
    let database = database.map(str::trim).filter(|d| !d.is_empty());
    let server_user = match s.call(Method::GET, "/server", None).await {
        Ok(_) => Some(true),
        Err(e @ Error::Connect(_)) => return Err(e),
        Err(_) => None,
    };
    let user = s.auth.as_ref().map(|(u, _)| u.as_str()).unwrap_or_default();
    let same_db = database.is_none_or(|d| d == s.db);
    let db_admin = if server_user != Some(true) && same_db && !s.db.is_empty() && !user.is_empty() {
        database_admin(s, user).await?
    } else {
        None
    };
    Ok(decide(server_user, db_admin, database.is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn role(name: &str, mode: u64, rules: Value, parent: Option<&str>) -> Record {
        vec![
            ("name".into(), json!(name)),
            ("mode".into(), json!(mode)),
            ("parent".into(), parent.map_or(Value::Null, |p| json!(p))),
            ("rules".into(), rules),
        ]
    }

    fn roles() -> Vec<Record> {
        vec![
            role("admin", 0, json!({ "*": 31, "database.class.*": 31 }), None),
            role("legacy", 1, json!({}), None),
            role("jefe", 0, json!({ "database.class.venta": 31 }), Some("admin")),
            role("writer", 0, json!({ "database.class.*": 15, "*": 2 }), None),
            role("loop", 0, json!({}), Some("loop")),
        ]
    }

    #[test]
    fn all_powerful_roles_and_their_heirs() {
        let r = roles();
        let has = |v: &[&str]| admin_role(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &r);
        assert!(has(&["admin"]) && has(&["legacy"]) && has(&["writer", "jefe"]));
        assert!(!has(&["writer"]) && !has(&["loop"]) && !has(&["missing"]) && !has(&[]));
    }

    #[test]
    fn a_server_user_may_do_everything_offered() {
        let p = decide(Some(true), None, true);
        assert_eq!((&p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&p.backup, &p.profiler, &p.kill_session), (&Access::Unknown, &Access::Unknown, &Access::Unknown));
        assert_eq!(decide(Some(true), None, false).drop_database, Access::Unknown);
    }

    #[test]
    fn a_database_user_is_never_proven_denied() {
        let p = decide(None, Some(true), true);
        assert_eq!((&p.create_database, &p.drop_database, &p.manage_security), (&Access::Unknown, &Access::Unknown, &Access::Allowed));
        assert_eq!(decide(None, Some(false), true), Permissions::default());
        assert_eq!(decide(None, None, true), Permissions::default());
    }
}
