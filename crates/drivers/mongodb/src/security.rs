//! Users, roles and permissions (docs/users-and-permissions.md).
//!
//! MongoDB's users and roles live in a database (the session's). A user
//! holds roles (`readWrite@ventas`, `root@admin`, or custom ones); a custom
//! role holds privileges (actions on a resource) and other roles. DBine
//! shows:
//!
//! - roles held, as `member_of` (`role`, or `role@db` when the role is in
//!   another database);
//! - built-in roles held, also as grants: privilege = the role, object =
//!   its database;
//! - a custom role's privileges as one grant per action, `via` the role
//!   (a role's own privileges are direct when the role itself is shown).
//!
//! Scripts are `db.runCommand({…})` calls of the editor's language
//! (`shell.rs`). A granted role or a membership doesn't say whether the
//! grantee is a user or a role, so the script carries both commands with
//! DBine's `{ ifExists: true }` extension: each runs only if its principal
//! exists ([`missing_principal`]).
//!
//! FerretDB 2 has users (all in `admin`, every one with full access) but no
//! roles: only users and passwords. Amazon DocumentDB keeps its users and
//! roles in `admin`, so its scripts use `db.adminCommand`.

use crate::{err, Flavor, MongoSession};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use mongodb::bson::{doc, Bson, Document};
use mongodb::Database;
use serde_json::{json, Value};

/// Built-in roles offered when granting (on the session's database unless
/// they only exist in `admin`).
const BUILTIN_ROLES: &[&str] = &[
    "read",
    "readWrite",
    "dbAdmin",
    "userAdmin",
    "dbOwner",
    "readAnyDatabase",
    "readWriteAnyDatabase",
    "userAdminAnyDatabase",
    "dbAdminAnyDatabase",
    "clusterMonitor",
    "clusterManager",
    "clusterAdmin",
    "hostManager",
    "backup",
    "restore",
    "root",
];

/// Built-in roles that exist only in `admin`: a bare name means `role@admin`.
const ADMIN_ONLY: &[&str] = &[
    "readAnyDatabase",
    "readWriteAnyDatabase",
    "userAdminAnyDatabase",
    "dbAdminAnyDatabase",
    "clusterMonitor",
    "clusterManager",
    "clusterAdmin",
    "hostManager",
    "backup",
    "restore",
    "root",
    "__system",
    "enableSharding",
    "searchCoordinator",
    "directShardOperations",
];

const FERRET_NO_ROLES: &str = "FerretDB no tiene roles ni permisos: todo usuario que inicia sesión tiene acceso completo";

pub fn spec(flavor: Flavor) -> SecuritySpec {
    match flavor {
        Flavor::Ferret => SecuritySpec {
            privileges: vec![],
            object_kinds: vec![""],
            create_user: true,
            create_role: false,
            passwords: true,
            membership: false,
            per_database: false,
        },
        Flavor::Mongo | Flavor::DocumentDb => SecuritySpec {
            privileges: BUILTIN_ROLES.to_vec(),
            // Roles are granted on the whole database; privileges on a
            // collection go in a custom role (revoked from its grants).
            object_kinds: vec![""],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: flavor == Flavor::Mongo,
        },
    }
}

// -- reading ------------------------------------------------------------------

/// The database users and roles are read from and written to.
fn home(s: &MongoSession) -> Database {
    match s.flavor {
        Flavor::Mongo => s.db.clone(),
        Flavor::Ferret | Flavor::DocumentDb => s.client.database("admin"),
    }
}

fn docs<'a>(d: &'a Document, key: &str) -> impl Iterator<Item = &'a Document> {
    d.get_array(key).map(|a| a.as_slice()).unwrap_or(&[]).iter().filter_map(Bson::as_document)
}

/// `{ role, db }` of a role reference.
fn role_ref(d: &Document) -> Option<(String, String)> {
    Some((d.get_str("role").ok()?.to_string(), d.get_str("db").ok()?.to_string()))
}

/// How a role is named in the UI: bare in its own database (or when it only
/// exists in `admin`), `role@db` otherwise.
fn role_label(role: &str, db: &str, home: &str) -> String {
    if db == home || (db == "admin" && ADMIN_ONLY.contains(&role)) {
        role.to_string()
    } else {
        format!("{role}@{db}")
    }
}

fn is_superuser_role(role: &str, db: &str) -> bool {
    db == "admin" && matches!(role, "root" | "__system")
}

pub async fn principals(s: &MongoSession) -> Result<Vec<Principal>> {
    let db = home(s);
    let home_name = db.name().to_string();
    let users = db.run_command(doc! { "usersInfo": 1 }).await.map_err(err)?;
    let roles = match s.flavor {
        Flavor::Ferret => Document::new(),
        _ => db.run_command(doc! { "rolesInfo": 1, "showPrivileges": false, "showBuiltinRoles": false }).await.map_err(err)?,
    };
    // Custom roles that are (or inherit) root.
    let super_roles: Vec<(String, String)> = docs(&roles, "roles")
        .filter(|r| docs(r, "inheritedRoles").filter_map(role_ref).any(|(n, d)| is_superuser_role(&n, &d)))
        .filter_map(role_ref)
        .collect();
    let mut out = Vec::new();
    for u in docs(&users, "users") {
        let Ok(name) = u.get_str("user") else { continue };
        let held: Vec<(String, String)> = docs(u, "roles").filter_map(role_ref).collect();
        let mut details = vec![("Base".to_string(), u.get_str("db").unwrap_or(&home_name).to_string())];
        let mechs: Vec<&str> = u.get_array("mechanisms").map(|a| a.iter().filter_map(Bson::as_str).collect()).unwrap_or_default();
        if !mechs.is_empty() {
            details.push(("Autenticación".into(), mechs.join(", ")));
        }
        out.push(Principal {
            name: name.to_string(),
            kind: PrincipalKind::User,
            can_login: Some(true),
            superuser: Some(held.iter().any(|(n, d)| is_superuser_role(n, d) || super_roles.contains(&(n.clone(), d.clone())))),
            disabled: None,
            member_of: held.iter().map(|(n, d)| role_label(n, d, &home_name)).collect(),
            details,
            // FerretDB's internal worker.
            system: s.flavor == Flavor::Ferret && name.starts_with("documentdb_"),
        });
    }
    for r in docs(&roles, "roles") {
        let Some((name, rdb)) = role_ref(r) else { continue };
        out.push(Principal {
            superuser: Some(super_roles.contains(&(name.clone(), rdb.clone()))),
            kind: PrincipalKind::Role,
            can_login: Some(false),
            disabled: None,
            member_of: docs(r, "roles").filter_map(role_ref).map(|(n, d)| role_label(&n, &d, &home_name)).collect(),
            details: vec![("Base".into(), rdb.clone())],
            system: r.get_bool("isBuiltin").unwrap_or(false),
            name,
        });
    }
    Ok(out)
}

/// A privilege's resource as (object, object_kind): `db.coll`, `db.*`
/// (every collection of a database), `*.*`, `cluster`, `anyResource`.
fn resource(r: &Document) -> (String, &'static str) {
    if r.get_bool("cluster").unwrap_or(false) {
        return ("cluster".into(), "cluster");
    }
    if r.get_bool("anyResource").unwrap_or(false) {
        return ("anyResource".into(), "anyResource");
    }
    if let (Ok(db), Ok(coll)) = (r.get_str("db"), r.get_str("collection")) {
        let db = if db.is_empty() { "*" } else { db };
        let coll = if coll.is_empty() { "*" } else { coll };
        return (format!("{db}.{coll}"), "collection");
    }
    let (k, v) = r.iter().next().map(|(k, v)| (k.as_str(), v.to_string())).unwrap_or(("?", String::new()));
    (format!("{k}: {v}"), "resource")
}

fn privilege_grants(role: &Document, key: &str, via: Option<&str>, out: &mut Vec<Grant>) {
    for p in docs(role, key) {
        let (object, kind) = p.get_document("resource").map(resource).unwrap_or(("?".into(), "resource"));
        for a in p.get_array("actions").map(|a| a.as_slice()).unwrap_or(&[]).iter().filter_map(Bson::as_str) {
            out.push(Grant {
                privilege: a.to_string(),
                object: Some(object.clone()),
                object_kind: Some(kind.into()),
                via: via.map(str::to_string),
                ..Default::default()
            });
        }
    }
}

fn role_grant(role: &str, db: &str, home: &str, via: Option<&str>) -> Grant {
    Grant {
        privilege: role_label(role, db, home),
        object: Some(db.to_string()),
        object_kind: Some("database".into()),
        via: via.map(str::to_string),
        ..Default::default()
    }
}

async fn roles_info(db: &Database, refs: &[(String, String)]) -> Result<Vec<Document>> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let list: Vec<Bson> = refs.iter().map(|(r, d)| Bson::Document(doc! { "role": r, "db": d })).collect();
    let cmd = doc! { "rolesInfo": list, "showPrivileges": true, "showBuiltinRoles": true };
    let r = db.run_command(cmd).await.map_err(err)?;
    Ok(docs(&r, "roles").cloned().collect())
}

/// A principal's permissions: its own privileges (a role's), the built-in
/// roles it holds, and what its custom roles give, `via` each of them.
pub async fn grants(s: &MongoSession, principal: &str) -> Result<Vec<Grant>> {
    let db = home(s);
    let home_name = db.name().to_string();
    let users = db.run_command(doc! { "usersInfo": 1 }).await.map_err(err)?;
    let mut out = Vec::new();
    let held: Vec<(String, String)> = if let Some(u) = docs(&users, "users").find(|u| u.get_str("user") == Ok(principal)) {
        docs(u, "roles").filter_map(role_ref).collect()
    } else if s.flavor == Flavor::Ferret {
        return Err(Error::Query(format!("no hay un usuario «{principal}»")));
    } else {
        let r = db.run_command(doc! { "rolesInfo": principal, "showPrivileges": true }).await.map_err(err)?;
        let Some(role) = docs(&r, "roles").next() else {
            return Err(Error::Query(format!("no hay un usuario ni un rol «{principal}» en «{home_name}»")));
        };
        privilege_grants(role, "privileges", None, &mut out);
        docs(role, "roles").filter_map(role_ref).collect()
    };
    if s.flavor == Flavor::Ferret {
        out.extend(held.iter().map(|(r, d)| role_grant(r, d, &home_name, None)));
        return Ok(out);
    }
    let direct = roles_info(&db, &held).await?;
    // Custom roles inherited through a held custom role, by the held one.
    let mut inherited: Vec<(String, (String, String))> = Vec::new();
    for (role, rdb) in &held {
        let Some(info) = direct.iter().find(|d| role_ref(d).as_ref() == Some(&(role.clone(), rdb.clone()))) else {
            // Not found (dropped since): show it as held.
            out.push(role_grant(role, rdb, &home_name, None));
            continue;
        };
        if info.get_bool("isBuiltin").unwrap_or(false) {
            out.push(role_grant(role, rdb, &home_name, None));
            continue;
        }
        let via = role_label(role, rdb, &home_name);
        privilege_grants(info, "privileges", Some(&via), &mut out);
        for r in docs(info, "inheritedRoles").filter_map(role_ref) {
            inherited.push((via.clone(), r));
        }
    }
    let mut refs: Vec<(String, String)> = inherited.iter().map(|(_, r)| r.clone()).collect();
    refs.sort();
    refs.dedup();
    let infos = roles_info(&db, &refs).await?;
    for (via, (role, rdb)) in &inherited {
        let info = infos.iter().find(|d| role_ref(d).as_ref() == Some(&(role.clone(), rdb.clone())));
        match info {
            Some(i) if !i.get_bool("isBuiltin").unwrap_or(false) => privilege_grants(i, "privileges", Some(via), &mut out),
            _ => out.push(role_grant(role, rdb, &home_name, Some(via))),
        }
    }
    Ok(out)
}

/// For a command run with `{ ifExists: true }`: `Some(message)` when its
/// user or role doesn't exist (the command is skipped), `None` to run it.
pub async fn missing_principal(db: &Database, cmd: &Document) -> Result<Option<String>> {
    let Some((name, Bson::String(who))) = cmd.iter().next() else {
        return Err(Error::Query("ifExists necesita un comando de usuarios o roles, con el nombre como texto".into()));
    };
    let user = match name.as_str() {
        "grantRolesToUser" | "revokeRolesFromUser" | "updateUser" | "dropUser" => true,
        "grantRolesToRole" | "revokeRolesFromRole" | "grantPrivilegesToRole" | "revokePrivilegesFromRole" | "updateRole"
        | "dropRole" => false,
        other => return Err(Error::Query(format!("ifExists no se aplica a `{other}`: solo a comandos de usuarios y roles"))),
    };
    let (probe, list) = if user { (doc! { "usersInfo": who.as_str() }, "users") } else { (doc! { "rolesInfo": who.as_str() }, "roles") };
    let r = db.run_command(probe).await.map_err(err)?;
    if docs(&r, list).next().is_some() {
        return Ok(None);
    }
    let what = if user { "usuario" } else { "rol" };
    Ok(Some(format!("No hay un {what} «{who}» en «{}»: se salteó `{name}`.", db.name())))
}

// -- scripts ------------------------------------------------------------------

fn check_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::Query("escribí un nombre".into()));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::Query(format!("el nombre «{}» tiene caracteres de control", name.escape_debug())));
    }
    Ok(())
}

fn ident_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A granted role as the command takes it: `"readWrite"` (the command's
/// database), `{ role, db }` for `role@db` and for roles only in `admin`.
fn role_value(p: &str) -> Result<Value> {
    let (role, db) = match p.rsplit_once('@') {
        Some((r, d)) => (r, Some(d)),
        None if ADMIN_ONLY.contains(&p) => (p, Some("admin")),
        None => (p, None),
    };
    if !ident_ok(role) || db.is_some_and(|d| !ident_ok(d) || d.contains('.')) {
        return Err(Error::Query(format!(
            "«{p}» no es un rol válido: usá el nombre del rol (letras, números, _ - .), opcionalmente con @base"
        )));
    }
    Ok(match db {
        Some(d) => json!({ "role": role, "db": d }),
        None => json!(role),
    })
}

/// An action of a custom role's privilege (`find`, `insert`…).
fn action_value(p: &str) -> Result<Value> {
    if p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Error::Query(format!("«{p}» no es una acción válida de MongoDB (find, insert, update, remove…)")));
    }
    Ok(json!(p))
}

/// The resource of a privilege on an object; see [`resource`] for the names.
fn resource_value(o: &ObjectRef) -> Result<Value> {
    match o.kind.as_str() {
        "cluster" => return Ok(json!({ "cluster": true })),
        "anyResource" => return Ok(json!({ "anyResource": true })),
        _ => {}
    }
    let Some(db) = o.schema() else {
        return Err(Error::Unsupported(
            "en MongoDB se otorgan roles sobre la base; los permisos sobre una colección van en un rol propio: \
             creá el rol y agregale privilegios con grantPrivilegesToRole en el editor"
                .into(),
        ));
    };
    let db = if db == "*" { "" } else { db };
    let coll = if o.name == "*" { "" } else { o.name.as_str() };
    Ok(json!({ "db": db, "collection": coll }))
}

/// `{ "k": v, … }` in the order given, as the editor reads it.
fn command(pairs: &[(&str, Value)]) -> String {
    let body: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    format!("{{ {} }}", body.join(", "))
}

struct Out {
    admin: bool,
}

impl Out {
    fn run(&self, pairs: &[(&str, Value)]) -> String {
        let m = if self.admin { "adminCommand" } else { "runCommand" };
        format!("db.{m}({})", command(pairs))
    }

    /// The same change for a user and for a role, each only if it exists.
    fn both(&self, user_cmd: &str, role_cmd: &str, who: &str, key: &str, value: Value) -> String {
        let m = if self.admin { "adminCommand" } else { "runCommand" };
        let one = |c: &str| format!("db.{m}({}, {{}}, {{ ifExists: true }})", command(&[(c, json!(who)), (key, value.clone())]));
        format!(
            "// «{}» puede ser un usuario o un rol: cada comando se aplica solo si existe (extensión de DBine).\n{}\n{}",
            who.replace('\n', " "),
            one(user_cmd),
            one(role_cmd)
        )
    }
}

pub fn script(flavor: Flavor, action: &SecurityAction) -> Result<String> {
    let out = Out { admin: flavor != Flavor::Mongo };
    let ferret = flavor == Flavor::Ferret;
    fn no_roles() -> Result<String> {
        Err(Error::Unsupported(FERRET_NO_ROLES.into()))
    }
    Ok(match action {
        SecurityAction::CreateUser { name, password } => {
            check_name(name)?;
            let mut p = vec![("createUser", json!(name))];
            if let Some(pw) = password.as_deref().filter(|p| !p.is_empty()) {
                p.push(("pwd", json!(pw)));
            }
            p.push(("roles", json!([])));
            out.run(&p)
        }
        SecurityAction::CreateRole { .. } if ferret => return no_roles(),
        SecurityAction::CreateRole { name } => {
            check_name(name)?;
            out.run(&[("createRole", json!(name)), ("privileges", json!([])), ("roles", json!([]))])
        }
        SecurityAction::Drop { name, kind } => {
            check_name(name)?;
            match kind {
                PrincipalKind::User => out.run(&[("dropUser", json!(name))]),
                PrincipalKind::Role if ferret => return no_roles(),
                PrincipalKind::Role => out.run(&[("dropRole", json!(name))]),
            }
        }
        SecurityAction::SetPassword { name, password } => {
            check_name(name)?;
            if password.is_empty() {
                return Err(Error::Query("escribí la contraseña nueva".into()));
            }
            out.run(&[("updateUser", json!(name)), ("pwd", json!(password))])
        }
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported(
                "MongoDB no permite deshabilitar el ingreso de un usuario: sacale los roles o borralo".into(),
            ))
        }
        _ if ferret => return no_roles(),
        SecurityAction::Grant { grantable: true, .. } => {
            return Err(Error::Unsupported(
                "MongoDB no tiene permisos «con opción de otorgar»: para que alguien otorgue roles, dale userAdmin \
                 o un rol con la acción grantRole"
                    .into(),
            ))
        }
        SecurityAction::Grant { privileges, object, to, .. } | SecurityAction::Revoke { privileges, object, from: to, .. } => {
            check_name(to)?;
            if privileges.is_empty() {
                return Err(Error::Query("elegí al menos un permiso".into()));
            }
            let grant = matches!(action, SecurityAction::Grant { .. });
            match object {
                None => {
                    let roles = privileges.iter().map(|p| role_value(p)).collect::<Result<Vec<_>>>()?;
                    let (u, r) = if grant {
                        ("grantRolesToUser", "grantRolesToRole")
                    } else {
                        ("revokeRolesFromUser", "revokeRolesFromRole")
                    };
                    out.both(u, r, to, "roles", Value::Array(roles))
                }
                // Actions on a resource: only a role holds them.
                Some(o) => {
                    let actions = privileges.iter().map(|p| action_value(p)).collect::<Result<Vec<_>>>()?;
                    let privs = json!([{ "resource": resource_value(o)?, "actions": actions }]);
                    let c = if grant { "grantPrivilegesToRole" } else { "revokePrivilegesFromRole" };
                    out.run(&[(c, json!(to)), ("privileges", privs)])
                }
            }
        }
        SecurityAction::AddMember { role, member } | SecurityAction::RemoveMember { role, member } => {
            check_name(member)?;
            let roles = json!([role_value(role)?]);
            if matches!(action, SecurityAction::AddMember { .. }) {
                out.both("grantRolesToUser", "grantRolesToRole", member, "roles", roles)
            } else {
                out.both("revokeRolesFromUser", "revokeRolesFromRole", member, "roles", roles)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{self, Shape};

    fn s(a: SecurityAction) -> String {
        script(Flavor::Mongo, &a).unwrap()
    }

    fn cmds(text: &str) -> Vec<(Document, Shape, bool)> {
        shell::parse_script(text).unwrap().into_iter().map(|st| (st.cmd, st.shape, st.admin)).collect()
    }

    #[test]
    fn user_scripts_parse_back() {
        let t = s(SecurityAction::CreateUser { name: "ana\"}) x".into(), password: Some("p'w\"\\".into()) });
        assert_eq!(t, r#"db.runCommand({ createUser: "ana\"}) x", pwd: "p'w\"\\", roles: [] })"#);
        let c = cmds(&t);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, doc! { "createUser": "ana\"}) x", "pwd": "p'w\"\\", "roles": [] });
        let t = s(SecurityAction::SetPassword { name: "ana".into(), password: "n€w".into() });
        assert_eq!(cmds(&t)[0].0, doc! { "updateUser": "ana", "pwd": "n€w" });
        assert_eq!(cmds(&s(SecurityAction::Drop { name: "r".into(), kind: PrincipalKind::Role }))[0].0, doc! { "dropRole": "r" });
        assert_eq!(cmds(&s(SecurityAction::CreateRole { name: "r".into() }))[0].0, doc! { "createRole": "r", "privileges": [], "roles": [] });
        assert!(script(Flavor::Mongo, &SecurityAction::CreateUser { name: " ".into(), password: None }).is_err());
    }

    #[test]
    fn roles_go_to_users_and_roles_if_they_exist() {
        let t = s(SecurityAction::Grant {
            privileges: vec!["readWrite".into(), "clusterMonitor".into(), "read@ventas".into()],
            object: None,
            to: "ana".into(),
            grantable: false,
        });
        let c = cmds(&t);
        let roles = vec![Bson::from("readWrite"), doc! { "role": "clusterMonitor", "db": "admin" }.into(), doc! { "role": "read", "db": "ventas" }.into()];
        assert_eq!(c[0], (doc! { "grantRolesToUser": "ana", "roles": roles.clone() }, Shape::IfExists, false));
        assert_eq!(c[1], (doc! { "grantRolesToRole": "ana", "roles": roles }, Shape::IfExists, false));
        let c = cmds(&s(SecurityAction::RemoveMember { role: "lectores".into(), member: "ana".into() }));
        assert_eq!(c[0].0, doc! { "revokeRolesFromUser": "ana", "roles": ["lectores"] });
        assert_eq!(c[1].0, doc! { "revokeRolesFromRole": "ana", "roles": ["lectores"] });
        // Injection through a privilege is refused.
        for bad in ["read\"]})", "a b", "x@y.z", ""] {
            let a = SecurityAction::Grant { privileges: vec![bad.into()], object: None, to: "ana".into(), grantable: false };
            assert!(script(Flavor::Mongo, &a).is_err(), "{bad}");
        }
        let a = SecurityAction::Grant { privileges: vec!["read".into()], object: None, to: "ana".into(), grantable: true };
        assert!(matches!(script(Flavor::Mongo, &a), Err(Error::Unsupported(_))));
    }

    #[test]
    fn actions_on_resources_go_to_roles() {
        let o = ObjectRef { kind: "collection".into(), schema: Some("ventas".into()), name: "facturas".into() };
        let t = s(SecurityAction::Revoke { privileges: vec!["find".into()], object: Some(o), from: "lect".into() });
        let c = cmds(&t);
        let privs = vec![Bson::from(doc! { "resource": { "db": "ventas", "collection": "facturas" }, "actions": ["find"] })];
        assert_eq!(c[0].0, doc! { "revokePrivilegesFromRole": "lect", "privileges": privs });
        let all = ObjectRef { kind: "collection".into(), schema: Some("ventas".into()), name: "*".into() };
        let c = cmds(&s(SecurityAction::Grant { privileges: vec!["insert".into()], object: Some(all), to: "lect".into(), grantable: false }));
        assert_eq!(c[0].0.get_array("privileges").unwrap()[0].as_document().unwrap().get_document("resource").unwrap(), &doc! { "db": "ventas", "collection": "" });
        let cl = ObjectRef { kind: "cluster".into(), schema: None, name: "cluster".into() };
        let c = cmds(&s(SecurityAction::Grant { privileges: vec!["serverStatus".into()], object: Some(cl), to: "lect".into(), grantable: false }));
        let p = c[0].0.get_array("privileges").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(p.get_document("resource").unwrap(), &doc! { "cluster": true });
        // A collection without its database can't be written.
        let bare = ObjectRef { kind: "collection".into(), schema: None, name: "facturas".into() };
        let a = SecurityAction::Grant { privileges: vec!["find".into()], object: Some(bare), to: "lect".into(), grantable: false };
        assert!(matches!(script(Flavor::Mongo, &a), Err(Error::Unsupported(_))));
    }

    #[test]
    fn variants() {
        let off = SecurityAction::SetLogin { name: "ana".into(), enabled: false };
        assert!(matches!(script(Flavor::Mongo, &off), Err(Error::Unsupported(_))));
        let add = SecurityAction::AddMember { role: "r".into(), member: "ana".into() };
        assert!(matches!(script(Flavor::Ferret, &add), Err(Error::Unsupported(_))));
        let u = script(Flavor::Ferret, &SecurityAction::CreateUser { name: "ana".into(), password: Some("x".into()) }).unwrap();
        assert!(cmds(&u)[0].2, "FerretDB's users live in admin");
        let d = script(Flavor::DocumentDb, &add).unwrap();
        assert!(cmds(&d).iter().all(|c| c.2 && c.1 == Shape::IfExists));
        assert!(spec(Flavor::Mongo).per_database && !spec(Flavor::Ferret).create_role);
    }

    #[test]
    fn labels_and_resources() {
        assert_eq!(role_label("readWrite", "ventas", "ventas"), "readWrite");
        assert_eq!(role_label("root", "admin", "ventas"), "root");
        assert_eq!(role_label("read", "otra", "ventas"), "read@otra");
        assert_eq!(resource(&doc! { "db": "", "collection": "" }), ("*.*".into(), "collection"));
        assert_eq!(resource(&doc! { "db": "v", "collection": "system.js" }), ("v.system.js".into(), "collection"));
        assert_eq!(resource(&doc! { "cluster": true }), ("cluster".into(), "cluster"));
    }
}
