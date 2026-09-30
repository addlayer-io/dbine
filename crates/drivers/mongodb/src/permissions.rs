//! What the login may do (`Session::permissions`), from `connectionStatus`
//! with `showPrivileges`: the server answers with every privilege the
//! authenticated users hold, already resolved through their roles
//! (inherited ones included), as actions on resources.
//!
//! - profiler: `inprog` on the cluster (the `currentOp` sampling), or
//!   `enableProfiler` on the database and `find` on its `system.profile`
//!   (the database profiler) with profiling already on (`profile: -1`), or
//!   off with `sampleRate` at 1 (turning it on then only sets the level), or
//!   `enableProfiler` on every database (`sampleRate` is a server-wide
//!   setting). Amazon DocumentDB only samples: `inprog`.
//! - kill: `killop` on the cluster.
//! - create: `createCollection` on any database (DBine creates a database
//!   with its first collection). Granted only on some named databases, it
//!   may still create those: unknown then.
//! - drop: `dropDatabase` on the database.
//! - security: `createUser`, `createRole` or `grantRole` on the database
//!   users live in (the session's; `admin` in DocumentDB).
//!
//! A server without access control reports no users: when `listDatabases`
//! of every database answers too, everything is allowed. FerretDB gives
//! every user full access. DBine's read-only mode isn't looked at: this
//! reports what the server grants the user, and the read-only mode blocks
//! the writes on its own. A server that doesn't answer `connectionStatus`
//! leaves everything unknown; only an unreachable server is an error.

use crate::{err, Flavor, MongoSession};
use dbine_driver::{Access, Error, Permissions, Result};
use mongodb::bson::{doc, Bson, Document};

/// A privilege's resource.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Resource {
    /// `{ anyResource: true }`.
    Any,
    /// `{ cluster: true }`.
    Cluster,
    /// `{ db, collection }`; an empty string is "any".
    Ns { db: String, collection: String },
}

#[derive(Debug, Clone)]
pub(crate) struct Privilege {
    pub resource: Resource,
    pub actions: Vec<String>,
}

/// What the login is.
#[derive(Debug, Clone)]
pub(crate) enum Login {
    /// The server didn't say.
    Unknown,
    /// No access control, or an engine that gives every user everything.
    Unrestricted,
    Privileges(Vec<Privilege>),
}

/// What an action applies to.
enum Target<'a> {
    Cluster,
    /// The database itself (`dropDatabase`, `createUser`…).
    Db(&'a str),
    /// Every database (a resource `{ db: "", collection: "" }`).
    AnyDb,
    Collection(&'a str, &'a str),
}

fn covers(r: &Resource, t: &Target) -> bool {
    match (r, t) {
        (Resource::Any, _) => true,
        (Resource::Cluster, Target::Cluster) => true,
        (Resource::Ns { db, collection }, Target::Db(d)) => collection.is_empty() && (db.is_empty() || db == d),
        (Resource::Ns { db, collection }, Target::AnyDb) => collection.is_empty() && db.is_empty(),
        // An empty collection name doesn't cover the system collections.
        (Resource::Ns { db, collection }, Target::Collection(d, c)) => {
            (db.is_empty() || db == d) && (collection == c || (collection.is_empty() && !c.starts_with("system.")))
        }
        _ => false,
    }
}

fn can(privileges: &[Privilege], action: &str, t: Target) -> bool {
    privileges.iter().any(|p| p.actions.iter().any(|a| a == action) && covers(&p.resource, &t))
}

/// The login from a `connectionStatus` answer: `None` when it doesn't say
/// (no users means no access control, to be confirmed by the caller).
pub(crate) fn parse(status: &Document) -> Option<Login> {
    let info = status.get_document("authInfo").ok()?;
    let users = info.get_array("authenticatedUsers").ok()?;
    if users.is_empty() {
        return None;
    }
    let Ok(list) = info.get_array("authenticatedUserPrivileges") else { return Some(Login::Unknown) };
    let privileges = list
        .iter()
        .filter_map(Bson::as_document)
        .filter_map(|p| {
            let r = p.get_document("resource").ok()?;
            let resource = if r.get_bool("anyResource").unwrap_or(false) {
                Resource::Any
            } else if r.get_bool("cluster").unwrap_or(false) {
                Resource::Cluster
            } else {
                Resource::Ns { db: r.get_str("db").ok()?.into(), collection: r.get_str("collection").ok()?.into() }
            };
            let actions = p.get_array("actions").ok()?.iter().filter_map(|a| a.as_str().map(str::to_string)).collect();
            Some(Privilege { resource, actions })
        })
        .collect();
    Some(Login::Privileges(privileges))
}

/// `database`: the explorer's; `home`: the session's (the profiler's and the
/// users' database when there's no explorer database); `profiling`: the
/// database's profiling level and `sampleRate` (`None`: couldn't read them).
pub(crate) fn decide(
    flavor: Flavor,
    login: &Login,
    database: Option<&str>,
    home: &str,
    profiling: Option<(i32, f64)>,
) -> Permissions {
    let sampled_only = flavor == Flavor::DocumentDb;
    let offers_ops = flavor != Flavor::Ferret;
    let db = database.unwrap_or(home);
    let users_db = if flavor == Flavor::Mongo { db } else { "admin" };
    let mut p = match login {
        Login::Unknown => Permissions::default(),
        Login::Unrestricted => Permissions {
            profiler: Access::Allowed,
            kill_session: Access::Allowed,
            create_database: Access::Allowed,
            drop_database: if database.is_some() { Access::Allowed } else { Access::Unknown },
            manage_security: Access::Allowed,
            ..Default::default()
        },
        Login::Privileges(privs) => {
            let profiler = if can(privs, "inprog", Target::Cluster) {
                Access::Allowed
            } else if sampled_only {
                Access::check(false, "inprog")
            } else if !(can(privs, "enableProfiler", Target::Db(db))
                && can(privs, "find", Target::Collection(db, "system.profile")))
            {
                Access::check(false, format!("inprog (o enableProfiler y find sobre {db}.system.profile)"))
            } else if can(privs, "enableProfiler", Target::AnyDb) {
                Access::Allowed
            } else {
                // Already on, it reads what is recorded; off, it turns it on
                // unless that means setting `sampleRate` (server-wide).
                match profiling {
                    Some((level, rate)) => Access::check(
                        level > 0 || rate >= 1.0,
                        format!(
                            "inprog (o enableProfiler sobre todas las bases: el profiling de «{db}» está apagado y su sampleRate está en {rate})"
                        ),
                    ),
                    None => Access::Unknown,
                }
            };
            let create = if can(privs, "createCollection", Target::AnyDb) {
                Access::Allowed
            } else if privs.iter().any(|p| p.actions.iter().any(|a| a == "createCollection")) {
                // Granted on some databases: those can still be created.
                Access::Unknown
            } else {
                Access::check(false, "createCollection (sobre cualquier base)")
            };
            let security = ["createUser", "createRole", "grantRole"].iter().any(|a| can(privs, a, Target::Db(users_db)));
            Permissions {
                profiler,
                kill_session: Access::check(can(privs, "killop", Target::Cluster), "killop"),
                create_database: create,
                drop_database: match database {
                    Some(d) => Access::check(can(privs, "dropDatabase", Target::Db(d)), "dropDatabase"),
                    None => Access::Unknown,
                },
                manage_security: Access::check(security, format!("createUser o grantRole sobre {users_db}")),
                ..Default::default()
            }
        }
    };
    if !offers_ops {
        p.profiler = Access::Unknown;
        p.kill_session = Access::Unknown;
    }
    p
}

/// A refused command leaves the login unknown; an unreachable server is
/// the error.
fn refused(e: mongodb::error::Error) -> Result<Login> {
    match err(e) {
        e @ Error::Connect(_) => Err(e),
        e => {
            tracing::debug!("mongodb: permissions check refused: {e}");
            Ok(Login::Unknown)
        }
    }
}

async fn login(s: &MongoSession) -> Result<Login> {
    // FerretDB 2: every user has full access.
    if s.flavor == Flavor::Ferret {
        return Ok(Login::Unrestricted);
    }
    let admin = s.client.database("admin");
    let status = match admin.run_command(doc! { "connectionStatus": 1, "showPrivileges": true }).await {
        Ok(d) => d,
        Err(e) => return refused(e),
    };
    if let Some(login) = parse(&status) {
        return Ok(login);
    }
    // No users: access control is off when every database may be listed.
    let all = doc! { "listDatabases": 1, "nameOnly": true, "authorizedDatabases": false };
    match admin.run_command(all).await {
        Ok(_) => Ok(Login::Unrestricted),
        Err(e) => refused(e),
    }
}

pub(crate) async fn check(s: &MongoSession, database: Option<&str>) -> Result<Permissions> {
    let database = database.map(str::trim).filter(|d| !d.is_empty());
    let login = login(s).await?;
    let db = database.unwrap_or(s.db.name());
    let profiling = match (&login, s.flavor) {
        (Login::Privileges(_), Flavor::Mongo) => match s.client.database(db).run_command(doc! { "profile": -1 }).await {
            Ok(d) => crate::monitor::num(&d, &["was"])
                .map(|v| (v as i32, crate::monitor::num(&d, &["sampleRate"]).unwrap_or(1.0))),
            Err(e) => {
                refused(e)?;
                None
            }
        },
        _ => None,
    };
    Ok(decide(s.flavor, &login, database, s.db.name(), profiling))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied(a: &Access, what: &str) -> bool {
        matches!(a, Access::Denied { missing } if missing.contains(what))
    }

    fn status(privileges: Vec<Document>) -> Document {
        doc! { "authInfo": {
            "authenticatedUsers": [{ "user": "ana", "db": "admin" }],
            "authenticatedUserRoles": [],
            "authenticatedUserPrivileges": privileges,
        }, "ok": 1.0 }
    }

    fn login(privileges: Vec<Document>) -> Login {
        parse(&status(privileges)).unwrap()
    }

    fn root() -> Login {
        login(vec![
            doc! { "resource": { "cluster": true }, "actions": ["inprog", "killop", "listDatabases"] },
            doc! { "resource": { "db": "", "collection": "" }, "actions": ["createCollection", "dropDatabase", "createUser", "grantRole", "enableProfiler"] },
            doc! { "resource": { "db": "", "collection": "system.profile" }, "actions": ["find"] },
        ])
    }

    #[test]
    fn root_may_do_everything_offered() {
        let p = decide(Flavor::Mongo, &root(), Some("ventas"), "test", None);
        assert_eq!((&p.profiler, &p.kill_session, &p.create_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed));
        assert_eq!((&p.backup, &p.restore), (&Access::Unknown, &Access::Unknown));
        // No explorer database: nothing to drop.
        assert_eq!(decide(Flavor::Mongo, &root(), None, "test", None).drop_database, Access::Unknown);
    }

    #[test]
    fn read_write_on_one_database() {
        let l = login(vec![doc! {
            "resource": { "db": "ventas", "collection": "" },
            "actions": ["find", "insert", "createCollection", "dropCollection"],
        }]);
        let p = decide(Flavor::Mongo, &l, Some("ventas"), "ventas", None);
        assert!(denied(&p.profiler, "inprog"));
        assert!(denied(&p.kill_session, "killop"));
        // It may create "ventas" if it doesn't exist yet.
        assert_eq!(p.create_database, Access::Unknown);
        assert!(denied(&p.drop_database, "dropDatabase"));
        assert!(denied(&p.manage_security, "createUser o grantRole sobre ventas"));
    }

    #[test]
    fn db_admin_uses_the_database_profiler() {
        let l = login(vec![
            doc! { "resource": { "db": "ventas", "collection": "" }, "actions": ["enableProfiler", "dropDatabase"] },
            doc! { "resource": { "db": "ventas", "collection": "system.profile" }, "actions": ["find"] },
        ]);
        let p = decide(Flavor::Mongo, &l, Some("ventas"), "test", Some((1, 1.0)));
        assert_eq!((&p.profiler, &p.drop_database), (&Access::Allowed, &Access::Allowed));
        // Profiling off: turning it on only sets the level while sampleRate
        // is 1; any other rate needs enableProfiler on every database
        // (sampleRate is server-wide). Unread level: unknown.
        assert_eq!(decide(Flavor::Mongo, &l, Some("ventas"), "test", Some((0, 1.0))).profiler, Access::Allowed);
        assert!(denied(&decide(Flavor::Mongo, &l, Some("ventas"), "test", Some((0, 0.5))).profiler, "está apagado"));
        assert_eq!(decide(Flavor::Mongo, &l, Some("ventas"), "test", Some((1, 0.5))).profiler, Access::Allowed);
        assert_eq!(decide(Flavor::Mongo, &l, Some("ventas"), "test", None).profiler, Access::Unknown);
        let mut any = l.clone();
        if let Login::Privileges(v) = &mut any {
            v.push(Privilege { resource: Resource::Ns { db: "".into(), collection: "".into() }, actions: vec!["enableProfiler".into()] });
        }
        assert_eq!(decide(Flavor::Mongo, &any, Some("ventas"), "test", Some((0, 0.5))).profiler, Access::Allowed);
        assert!(denied(&p.create_database, "createCollection"));
        // Another database: neither.
        let p = decide(Flavor::Mongo, &l, Some("otra"), "test", None);
        assert!(denied(&p.profiler, "otra.system.profile") && p.drop_database.is_denied());
        // DocumentDB never reads system.profile.
        assert!(denied(&decide(Flavor::DocumentDb, &l, Some("ventas"), "test", None).profiler, "inprog"));
    }

    #[test]
    fn any_collection_is_not_a_system_collection() {
        let r = Resource::Ns { db: "".into(), collection: "".into() };
        assert!(covers(&r, &Target::Collection("a", "b")));
        assert!(!covers(&r, &Target::Collection("a", "system.profile")));
        assert!(!covers(&Resource::Ns { db: "a".into(), collection: "b".into() }, &Target::Db("a")));
        assert!(covers(&Resource::Any, &Target::Cluster));
        assert!(!covers(&Resource::Cluster, &Target::Db("a")));
    }

    #[test]
    fn users_live_in_admin_on_documentdb() {
        let l = login(vec![doc! { "resource": { "db": "admin", "collection": "" }, "actions": ["createUser"] }]);
        assert_eq!(decide(Flavor::DocumentDb, &l, Some("ventas"), "ventas", None).manage_security, Access::Allowed);
        assert!(decide(Flavor::Mongo, &l, Some("ventas"), "ventas", None).manage_security.is_denied());
    }

    #[test]
    fn unrestricted_and_unknown() {
        let p = decide(Flavor::Mongo, &Login::Unrestricted, Some("x"), "x", None);
        assert_eq!((&p.profiler, &p.drop_database, &p.kill_session), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        let p = decide(Flavor::Mongo, &Login::Unknown, Some("x"), "x", None);
        assert_eq!(p, Permissions::default());
        // FerretDB has no profiler nor killOp.
        let p = decide(Flavor::Ferret, &Login::Unrestricted, Some("x"), "x", None);
        assert_eq!((&p.profiler, &p.kill_session), (&Access::Unknown, &Access::Unknown));
        assert_eq!(p.create_database, Access::Allowed);
    }

    #[test]
    fn parses_what_the_server_sends() {
        let none = doc! { "authInfo": { "authenticatedUsers": [], "authenticatedUserPrivileges": [] }, "ok": 1.0 };
        assert!(parse(&none).is_none());
        let bare = doc! { "authInfo": { "authenticatedUsers": [{ "user": "a", "db": "admin" }] }, "ok": 1.0 };
        assert!(matches!(parse(&bare), Some(Login::Unknown)));
        let odd = status(vec![doc! { "resource": { "system_buckets": "x", "db": "y" }, "actions": ["find"] }]);
        assert!(matches!(parse(&odd), Some(Login::Privileges(v)) if v.is_empty()));
        assert!(matches!(root(), Login::Privileges(v) if v.len() == 3 && v[0].resource == Resource::Cluster));
    }
}
