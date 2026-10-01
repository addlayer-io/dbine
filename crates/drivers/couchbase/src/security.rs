//! Users, groups and roles of Couchbase Server (docs/usuarios-y-permisos.md).
//!
//! Couchbase's permissions are built-in roles (`data_reader`,
//! `query_select`, `bucket_full_access`, `admin`…), each on the whole
//! cluster, a bucket or a collection. A user holds them directly or through
//! its groups (Enterprise Edition). Both are read from the query service's
//! `system:user_info` and `system:group_info`; each role carries its
//! origins (the user itself or a group).
//!
//! Scripts are SQL++: `CREATE USER`, `ALTER USER`, `DROP USER`,
//! `CREATE GROUP`, `DROP GROUP` (Couchbase Server 8.0+), and `GRANT` /
//! `REVOKE` role. `GRANT` says whether it's for a user or a group
//! (`TO GROUP g`), so DBine names groups `group:<name>`.

use crate::ddl::{q, scope_ref};
use crate::{text, CbSession};
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use serde_json::Value;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "data_reader", "data_writer", "query_select", "query_insert", "query_update", "query_delete",
            "query_manage_index", "query_execute_functions", "query_manage_functions", "bucket_full_access", "bucket_admin",
            "scope_admin", "data_backup", "data_monitoring", "views_reader", "fts_searcher", "ro_admin", "cluster_admin",
            "security_admin_local", "admin",
        ],
        // "" = the cluster (roles without a bucket); a bucket; a collection.
        object_kinds: vec!["", "database", kinds::COLLECTION],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: false,
        per_database: false,
    }
}

const GROUP_PREFIX: &str = "group:";

fn group_name(g: &str) -> String {
    format!("{GROUP_PREFIX}{g}")
}

/// A principal's name as `(is group, name in Couchbase)`.
fn grantee(name: &str) -> (bool, &str) {
    match name.strip_prefix(GROUP_PREFIX) {
        Some(g) => (true, g),
        None => (false, name),
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).map(text).unwrap_or_default()
}

fn array<'a>(v: &'a Value, k: &str) -> impl Iterator<Item = &'a Value> {
    v.get(k).and_then(Value::as_array).into_iter().flatten()
}

// -- reading -------------------------------------------------------------------

fn user_principal(u: &Value) -> Principal {
    let domain = s(u, "domain");
    let mut details = vec![(
        "Dominio".into(),
        match domain.as_str() {
            "local" => "local (Couchbase)".to_string(),
            "external" => "externo (LDAP, PAM…)".to_string(),
            "builtin" => "administrador del cluster".to_string(),
            d => d.to_string(),
        },
    )];
    let full = s(u, "name");
    if !full.is_empty() {
        details.push(("Nombre".into(), full));
    }
    let changed = s(u, "password_change_date");
    if !changed.is_empty() {
        details.push(("Contraseña cambiada".into(), changed));
    }
    let external: Vec<String> = array(u, "external_groups").map(text).collect();
    if !external.is_empty() {
        details.push(("Grupos externos".into(), external.join(", ")));
    }
    Principal {
        name: s(u, "id"),
        kind: PrincipalKind::User,
        can_login: Some(true),
        superuser: Some(array(u, "roles").any(|r| s(r, "role") == "admin")),
        disabled: u.get("locked").and_then(Value::as_bool),
        member_of: array(u, "groups").map(|g| group_name(&text(g))).collect(),
        details,
        system: domain == "builtin",
    }
}

fn group_principal(g: &Value) -> Principal {
    let mut details = Vec::new();
    let d = s(g, "description");
    if !d.is_empty() {
        details.push(("Descripción".into(), d));
    }
    let ldap = s(g, "ldap_group_ref");
    if !ldap.is_empty() {
        details.push(("Grupo LDAP".into(), ldap));
    }
    Principal {
        name: group_name(&s(g, "id")),
        kind: PrincipalKind::Role,
        can_login: Some(false),
        superuser: Some(array(g, "roles").any(|r| s(r, "role") == "admin")),
        details,
        ..Default::default()
    }
}

async fn users(c: &CbSession) -> Result<Vec<Value>> {
    c.results("SELECT RAW u FROM system:user_info AS u").await
}

/// Groups are an Enterprise Edition feature: none on Community.
async fn groups(c: &CbSession) -> Vec<Value> {
    c.results("SELECT RAW g FROM system:group_info AS g").await.unwrap_or_default()
}

pub async fn principals(c: &CbSession) -> Result<Vec<Principal>> {
    let mut out: Vec<Principal> = users(c).await?.iter().map(user_principal).collect();
    out.extend(groups(c).await.iter().map(group_principal));
    Ok(out)
}

/// What a role applies to: the cluster, a bucket (`*`: every bucket) or a
/// collection.
fn role_object(r: &Value) -> (Option<String>, Option<String>) {
    let b = s(r, "bucket_name");
    let sc = s(r, "scope_name");
    let c = s(r, "collection_name");
    if b.is_empty() || b == "*" {
        (None, None)
    } else if sc.is_empty() || sc == "*" {
        (Some(b), Some("database".into()))
    } else if c.is_empty() || c == "*" {
        (Some(format!("{b}.{sc}")), Some("schema".into()))
    } else {
        (Some(format!("{b}.{sc}.{c}")), Some(kinds::COLLECTION.into()))
    }
}

/// A user's or group's roles, one grant per origin (the user itself or
/// each group it comes from).
fn grants_of(p: &Value, is_group: bool) -> Vec<Grant> {
    let mut out = Vec::new();
    for r in array(p, "roles") {
        let (object, object_kind) = role_object(r);
        let grant = |via: Option<String>| Grant { privilege: s(r, "role"), object: object.clone(), object_kind: object_kind.clone(), via, ..Default::default() };
        let origins: Vec<&Value> = array(r, "origins").collect();
        if is_group || origins.is_empty() {
            out.push(grant(None));
        }
        for o in origins.into_iter().filter(|_| !is_group) {
            out.push(grant((s(o, "type") == "group").then(|| group_name(&s(o, "name")))));
        }
    }
    out
}

pub async fn grants(c: &CbSession, principal: &str) -> Result<Vec<Grant>> {
    let (is_group, name) = grantee(principal);
    let found = if is_group {
        groups(c).await.into_iter().find(|g| s(g, "id") == name)
    } else {
        users(c).await?.into_iter().find(|u| s(u, "id") == name)
    };
    Ok(found.map(|p| grants_of(&p, is_group)).unwrap_or_default())
}

// -- scripts -------------------------------------------------------------------

/// A JSON string is a SQL++ string literal.
fn lit(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

fn roles(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un rol".into()));
    }
    let mut out = Vec::new();
    for x in p {
        let r = x.trim();
        if r.is_empty() || !r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::Query(format!("«{x}» no es un rol de Couchbase")));
        }
        out.push(r.to_ascii_lowercase());
    }
    Ok(out.join(", "))
}

/// Roles that take a scope (`role[bucket:scope]`, Enterprise Edition):
/// what "Nuevo esquema…" offers to grant on the new scope.
pub fn scope_roles() -> Vec<&'static str> {
    vec![
        "data_reader", "data_writer", "query_select", "query_insert", "query_update", "query_delete", "query_manage_index",
        "query_execute_functions", "query_manage_functions", "scope_admin", "data_monitoring", "fts_searcher",
    ]
}

/// ` ON <keyspace>`: a bucket, a scope or a collection, always from the `default`
/// namespace (the session's query context would otherwise read a bare name
/// as a collection).
fn on(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else { return Ok(String::new()) };
    if o.kind == "database" {
        return Ok(format!(" ON default:{}", q(&o.name)));
    }
    // A scope: `bucket.scope` as the explorer and a listed grant name it.
    if o.kind == "schema" {
        let full = match o.schema() {
            Some(b) => format!("{b}.{}", o.name),
            None => o.name.clone(),
        };
        return Ok(format!(" ON {}", scope_ref(&full)?));
    }
    // `bucket.scope` + `collection` (the explorer) or `bucket` +
    // `scope.collection` (a listed grant): scopes and collections have no
    // dots, buckets may.
    let full = match o.schema() {
        Some(sc) => format!("{sc}.{}", o.name),
        None => o.name.clone(),
    };
    let mut parts = full.rsplitn(3, '.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(c), Some(sc), Some(b)) if o.kind == kinds::COLLECTION && ![c, sc, b].contains(&"") => {
            Ok(format!(" ON default:{}.{}.{}", q(b), q(sc), q(c)))
        }
        _ => Err(Error::Query(format!("«{full}» no es una colección (bucket.scope.colección)"))),
    }
}

fn to_whom(name: &str) -> String {
    match grantee(name) {
        (true, g) => format!("GROUP {}", q(g)),
        (false, u) => q(u),
    }
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} PASSWORD {};", q(name), lit(pw))
        }
        SecurityAction::CreateRole { name } => format!("CREATE GROUP {};", q(grantee(name).1)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP GROUP {};", q(grantee(name).1)),
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} PASSWORD {};", q(name), lit(password)),
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported("SQL++ no bloquea usuarios: cambiale la contraseña o borralo".into()))
        }
        SecurityAction::Grant { privileges, object, to, grantable } => {
            if *grantable {
                return Err(Error::Unsupported("en Couchbase un rol no se otorga con permiso para otorgarlo a otros".into()));
            }
            format!("GRANT {}{} TO {};", roles(privileges)?, on(object)?, to_whom(to))
        }
        SecurityAction::Revoke { privileges, object, from } => format!("REVOKE {}{} FROM {};", roles(privileges)?, on(object)?, to_whom(from)),
        SecurityAction::AddMember { .. } | SecurityAction::RemoveMember { .. } => {
            return Err(Error::Unsupported(
                "SQL++ reemplaza la lista entera de grupos de un usuario (ALTER USER … GROUPS …): escribila completa en el editor".into(),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    #[test]
    fn writes_the_scripts() {
        let sc = |a| script(&a).unwrap();
        assert_eq!(sc(SecurityAction::CreateUser { name: "ana`b".into(), password: Some("p\"w'\\".into()) }), r#"CREATE USER `ana``b` PASSWORD "p\"w'\\";"#);
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert_eq!(sc(SecurityAction::CreateRole { name: "lect".into() }), "CREATE GROUP `lect`;");
        assert_eq!(sc(SecurityAction::Drop { name: "group:lect".into(), kind: PrincipalKind::Role }), "DROP GROUP `lect`;");
        assert_eq!(sc(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), "DROP USER `ana`;");
        assert_eq!(sc(SecurityAction::SetPassword { name: "ana".into(), password: "n".into() }), r#"ALTER USER `ana` PASSWORD "n";"#);
        assert_eq!(sc(SecurityAction::Grant { privileges: vec!["ro_admin".into()], object: None, to: "ana".into(), grantable: false }), "GRANT ro_admin TO `ana`;");
        assert_eq!(
            sc(SecurityAction::Grant { privileges: vec!["data_reader".into(), "query_select".into()], object: obj("database", None, "b.1"), to: "group:lect".into(), grantable: false }),
            "GRANT data_reader, query_select ON default:`b.1` TO GROUP `lect`;"
        );
        // From the explorer (schema `bucket.scope`) and from a listed grant (split at the first dot).
        let want = "GRANT query_select ON default:`b.1`.`s`.`c` TO `ana`;";
        assert_eq!(sc(SecurityAction::Grant { privileges: vec!["query_select".into()], object: obj(kinds::COLLECTION, Some("b.1.s"), "c"), to: "ana".into(), grantable: false }), want);
        assert_eq!(sc(SecurityAction::Grant { privileges: vec!["query_select".into()], object: obj(kinds::COLLECTION, Some("b"), "1.s.c"), to: "ana".into(), grantable: false }), want);
        assert_eq!(
            sc(SecurityAction::Revoke { privileges: vec!["bucket_full_access".into()], object: obj("database", None, "b1"), from: "ana".into() }),
            "REVOKE bucket_full_access ON default:`b1` FROM `ana`;"
        );
        // A scope (Enterprise Edition's scope roles), as "Nuevo esquema…" and a listed grant name it.
        assert_eq!(
            sc(SecurityAction::Grant { privileges: vec!["query_select".into(), "data_reader".into()], object: obj("schema", None, "b.1.s"), to: "ana".into(), grantable: false }),
            "GRANT query_select, data_reader ON default:`b.1`.`s` TO `ana`;"
        );
        assert_eq!(
            sc(SecurityAction::Revoke { privileges: vec!["scope_admin".into()], object: obj("schema", Some("b"), "s"), from: "group:g".into() }),
            "REVOKE scope_admin ON default:`b`.`s` FROM GROUP `g`;"
        );
        assert!(script(&SecurityAction::Grant { privileges: vec!["query_select".into()], object: obj("schema", None, "s"), to: "a".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["admin TO x; DROP USER y".into()], object: None, to: "a".into(), grantable: false }).is_err());
        for a in [
            SecurityAction::SetLogin { name: "a".into(), enabled: false },
            SecurityAction::AddMember { role: "group:g".into(), member: "a".into() },
            SecurityAction::Grant { privileges: vec!["admin".into()], object: None, to: "a".into(), grantable: true },
        ] {
            assert!(matches!(script(&a), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn reads_users_groups_and_roles() {
        let u = json!({"domain": "local", "id": "ana", "locked": false, "name": "Ana", "groups": ["lect"], "roles": [
            {"role": "bucket_full_access", "bucket_name": "b1", "origins": [{"type": "user"}, {"type": "group", "name": "lect"}]},
            {"role": "query_select", "bucket_name": "b1", "scope_name": "s", "collection_name": "c", "origins": [{"type": "group", "name": "lect"}]},
            {"role": "ro_admin", "origins": [{"type": "user"}]},
        ]});
        let p = user_principal(&u);
        assert_eq!((p.name.as_str(), p.superuser, p.disabled, p.system), ("ana", Some(false), Some(false), false));
        assert_eq!(p.member_of, vec!["group:lect".to_string()]);
        let admin = user_principal(&json!({"domain": "builtin", "id": "Administrator", "roles": [{"role": "admin"}]}));
        assert!(admin.system && admin.superuser == Some(true));
        let g = grants_of(&u, false);
        assert_eq!(g.len(), 4);
        assert!(g.iter().any(|x| x.privilege == "bucket_full_access" && x.via.is_none() && x.object.as_deref() == Some("b1") && x.object_kind.as_deref() == Some("database")));
        assert!(g.iter().any(|x| x.privilege == "bucket_full_access" && x.via.as_deref() == Some("group:lect")));
        assert!(g.iter().any(|x| x.privilege == "query_select" && x.object.as_deref() == Some("b1.s.c") && x.object_kind.as_deref() == Some(kinds::COLLECTION)));
        assert!(g.iter().any(|x| x.privilege == "ro_admin" && x.object.is_none()));
        let grp = json!({"id": "lect", "description": "lectores", "roles": [{"role": "data_reader", "bucket_name": "*"}]});
        let gp = group_principal(&grp);
        assert_eq!((gp.name.as_str(), gp.kind), ("group:lect", PrincipalKind::Role));
        let gg = grants_of(&grp, true);
        assert_eq!((gg.len(), gg[0].object.clone(), gg[0].via.clone()), (1, None, None));
    }
}
