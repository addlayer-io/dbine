//! What the role may do (`Session::permissions`), from `LIST ROLES OF
//! <me>` (a superuser role, its own or inherited, may do everything) and
//! `LIST ALL PERMISSIONS OF <me>`, which includes the permissions of the
//! roles it holds. Both work for any role asking about itself.
//!
//! - profiler: SELECT on the table it reads (Cassandra
//!   `system_views.queries`, ScyllaDB `audit.audit_log`) or on a parent
//!   (its keyspace, all keyspaces).
//! - create: CREATE on ALL KEYSPACES. Drop: DROP on the keyspace or on
//!   ALL KEYSPACES.
//! - security: CREATE on ALL ROLES. Without it, a role with some other
//!   permission on roles, or AUTHORIZE on something, can still change part
//!   of it: left unknown.
//!
//! Without authentication (AllowAllAuthenticator) or authorization
//! (AllowAllAuthorizer) every role may do everything; users and roles
//! can't be managed then, so security stays unknown. Cassandra and ScyllaDB
//! have no backups, restores or ending sessions in CQL; Amazon Keyspaces
//! decides everything in AWS IAM: unknown.

use crate::{boolean, cql, text, CassandraSession, Flavor};
use dbine_driver::{Access, Error, Permissions, Result};

/// What the server told about the role.
#[derive(Debug, Clone)]
pub(crate) enum Auth {
    /// Authentication or authorization is off: everything is allowed.
    Open,
    /// A superuser role, its own or inherited.
    Superuser,
    /// (resource, permission) as `LIST ALL PERMISSIONS` gives them:
    /// `<all keyspaces>`, `<keyspace ks>`, `<table ks.t>`, `<all roles>`…
    Granted(Vec<(String, String)>),
}

fn has(grants: &[(String, String)], permission: &str, resources: &[String]) -> bool {
    grants.iter().any(|(r, p)| p.eq_ignore_ascii_case(permission) && resources.iter().any(|x| x == r))
}

/// The table and its parents, as resources.
fn table_and_parents(ks: &str, table: &str) -> Vec<String> {
    vec![format!("<table {ks}.{table}>"), format!("<keyspace {ks}>"), "<all keyspaces>".into()]
}

pub(crate) fn decide(flavor: Flavor, auth: &Auth, database: Option<&str>) -> Permissions {
    let database = database.map(str::trim).filter(|d| !d.is_empty());
    let (profiler_ks, profiler_table) = if flavor == Flavor::Scylla { ("audit", "audit_log") } else { ("system_views", "queries") };
    let grants = match auth {
        Auth::Open | Auth::Superuser => {
            return Permissions {
                profiler: Access::Allowed,
                create_database: Access::Allowed,
                drop_database: if database.is_some() { Access::Allowed } else { Access::Unknown },
                manage_security: if matches!(auth, Auth::Superuser) { Access::Allowed } else { Access::Unknown },
                ..Default::default()
            }
        }
        Auth::Granted(g) => g,
    };
    let all_keyspaces = vec!["<all keyspaces>".to_string()];
    let on_roles = |r: &str| r == "<all roles>" || r.starts_with("<role ");
    let security = if has(grants, "CREATE", &["<all roles>".into()]) {
        Access::Allowed
    } else if grants.iter().any(|(r, p)| on_roles(r) || p.eq_ignore_ascii_case("AUTHORIZE")) {
        Access::Unknown
    } else {
        Access::check(false, "CREATE ON ALL ROLES")
    };
    Permissions {
        profiler: Access::check(
            has(grants, "SELECT", &table_and_parents(profiler_ks, profiler_table)),
            format!("SELECT ON {profiler_ks}.{profiler_table}"),
        ),
        create_database: Access::check(has(grants, "CREATE", &all_keyspaces), "CREATE ON ALL KEYSPACES"),
        drop_database: match database {
            Some(ks) => Access::check(
                has(grants, "DROP", &[format!("<keyspace {ks}>"), "<all keyspaces>".into()]),
                format!("DROP ON KEYSPACE {}", cql::ident(ks)),
            ),
            None => Access::Unknown,
        },
        manage_security: security,
        ..Default::default()
    }
}

/// Authentication or authorization is off (the server says so when asked).
fn is_open(e: &Error) -> bool {
    let m = e.to_string().to_lowercase();
    m.contains("anonymous") || m.contains("allowallauthenticator") || m.contains("allowallauthorizer")
}

pub(crate) async fn check(s: &CassandraSession, database: Option<&str>) -> Result<Permissions> {
    if s.flavor == Flavor::Keyspaces {
        return Ok(Permissions::default());
    }
    let roles = match &s.user {
        Some(u) => format!("LIST ROLES OF {}", cql::ident(u)),
        None => "LIST ROLES".into(),
    };
    let auth = match s.rows(&roles, ()).await {
        Err(e) if is_open(&e) => Auth::Open,
        Err(e) => {
            tracing::debug!("cassandra: permissions check refused: {e}");
            return Ok(Permissions::default());
        }
        // Anonymous but listed: can't tell whose roles these are.
        Ok(_) if s.user.is_none() => return Ok(Permissions::default()),
        // role | super | login | …
        Ok(rows) if rows.iter().any(|r| boolean(r, 1)) => Auth::Superuser,
        Ok(_) => {
            let user = s.user.as_deref().unwrap_or_default();
            match s.rows(&format!("LIST ALL PERMISSIONS OF {}", cql::ident(user)), ()).await {
                Err(e) if is_open(&e) => Auth::Open,
                Err(e) => {
                    tracing::debug!("cassandra: permissions check refused: {e}");
                    return Ok(Permissions::default());
                }
                // role | username | resource | permission
                Ok(rows) => Auth::Granted(rows.iter().map(|r| (text(r, 2), text(r, 3))).collect()),
            }
        }
    };
    Ok(decide(s.flavor, &auth, database))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    fn granted(g: &[(&str, &str)]) -> Auth {
        Auth::Granted(g.iter().map(|(r, p)| (r.to_string(), p.to_string())).collect())
    }

    #[test]
    fn superusers_and_open_servers() {
        let p = decide(Flavor::Cassandra, &Auth::Superuser, Some("ks"));
        assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&p.backup, &p.restore, &p.kill_session), (&Access::Unknown, &Access::Unknown, &Access::Unknown));
        let p = decide(Flavor::Scylla, &Auth::Open, None);
        assert_eq!((&p.profiler, &p.create_database), (&Access::Allowed, &Access::Allowed));
        // No keyspace to drop, no users to manage.
        assert_eq!((&p.drop_database, &p.manage_security), (&Access::Unknown, &Access::Unknown));
    }

    #[test]
    fn a_role_with_nothing_is_denied() {
        let p = decide(Flavor::Cassandra, &granted(&[("<keyspace ks>", "SELECT")]), Some("ks"));
        assert!(denied(&p.profiler, "SELECT ON system_views.queries"));
        assert!(denied(&p.create_database, "CREATE ON ALL KEYSPACES"));
        assert!(denied(&p.drop_database, "DROP ON KEYSPACE ks"));
        assert!(denied(&p.manage_security, "CREATE ON ALL ROLES"));
        let p = decide(Flavor::Scylla, &granted(&[]), None);
        assert!(denied(&p.profiler, "audit.audit_log"));
        assert_eq!(p.drop_database, Access::Unknown);
    }

    #[test]
    fn grants_on_parents_count() {
        let g = granted(&[
            ("<all keyspaces>", "CREATE"),
            ("<keyspace ks>", "DROP"),
            ("<keyspace system_views>", "SELECT"),
            ("<all roles>", "CREATE"),
        ]);
        let p = decide(Flavor::Cassandra, &g, Some("ks"));
        assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert!(denied(&decide(Flavor::Cassandra, &g, Some("other")).drop_database, "DROP ON KEYSPACE other"));
        let g = granted(&[("<all keyspaces>", "DROP"), ("<table audit.audit_log>", "SELECT")]);
        let p = decide(Flavor::Scylla, &g, Some("any"));
        assert_eq!((&p.profiler, &p.drop_database), (&Access::Allowed, &Access::Allowed));
    }

    #[test]
    fn partial_security_is_unknown() {
        let p = decide(Flavor::Cassandra, &granted(&[("<role ana>", "ALTER")]), None);
        assert_eq!(p.manage_security, Access::Unknown);
        let p = decide(Flavor::Cassandra, &granted(&[("<keyspace ks>", "AUTHORIZE")]), None);
        assert_eq!(p.manage_security, Access::Unknown);
    }

    #[test]
    fn open_servers_are_recognized() {
        assert!(is_open(&Error::Query("You have to be logged in and not anonymous to perform this request".into())));
        assert!(is_open(&Error::Query("LIST PERMISSIONS operation is not supported by AllowAllAuthorizer".into())));
        assert!(!is_open(&Error::Query("You are not authorized to view x's permissions".into())));
    }
}
