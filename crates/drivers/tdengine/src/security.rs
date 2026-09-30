//! Users and permissions of TDengine 3.x (docs/usuarios-y-permisos.md).
//!
//! TDengine has users but no roles: `SHOW USERS` lists them (super,
//! enabled, sysinfo, createdb) and `information_schema.ins_user_privileges`
//! their `READ` / `WRITE` / `ALL` on a database or a table, and `SUBSCRIBE`
//! on a topic. `root` is the built-in superuser.

use crate::ddl::{lit, q};
use crate::{text, TdSession, SUBTABLE, SUPERTABLE};
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use serde_json::{Map, Value};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["READ", "WRITE", "ALL", "SUBSCRIBE"],
        // "" = every database (`*.*`).
        object_kinds: vec!["", "database", SUPERTABLE, kinds::TABLE, SUBTABLE, kinds::TOPIC],
        create_user: true,
        create_role: false,
        passwords: true,
        membership: false,
        per_database: false,
    }
}

// -- reading -------------------------------------------------------------------

fn field(r: &Map<String, Value>, k: &str) -> String {
    r.get(k).map(text).unwrap_or_default()
}

fn flag(r: &Map<String, Value>, k: &str) -> Option<bool> {
    match r.get(k)? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => Some(n.as_i64() != Some(0)),
        Value::String(s) => Some(s == "1" || s.eq_ignore_ascii_case("true")),
        _ => None,
    }
}

fn si_no(b: bool) -> String {
    if b { "sí" } else { "no" }.into()
}

fn principal_of(r: &Map<String, Value>) -> Principal {
    let name = field(r, "name");
    let mut details = Vec::new();
    if let Some(s) = flag(r, "sysinfo") {
        details.push(("Ve la información del sistema".into(), si_no(s)));
    }
    if let Some(c) = flag(r, "createdb") {
        details.push(("Crea bases de datos".into(), si_no(c)));
    }
    let hosts = field(r, "allowed_host");
    if !hosts.is_empty() {
        details.push(("Hosts permitidos".into(), hosts));
    }
    let created = field(r, "create_time");
    if !created.is_empty() {
        details.push(("Alta".into(), created));
    }
    Principal {
        system: name == "root",
        can_login: Some(true),
        superuser: flag(r, "super"),
        disabled: flag(r, "enable").map(|e| !e),
        details,
        name,
        kind: PrincipalKind::User,
        member_of: Vec::new(),
    }
}

pub async fn principals(s: &TdSession) -> Result<Vec<Principal>> {
    Ok(s.records("SHOW USERS").await?.iter().map(principal_of).collect())
}

/// One `ins_user_privileges` row as a grant; `stables` are the
/// `db.name` of the supertables among the tables it names.
fn grant_of(r: &Map<String, Value>, stables: &[String]) -> Grant {
    let privilege = field(r, "privilege").to_uppercase();
    let db = field(r, "db_name");
    let table = field(r, "table_name");
    let (object, object_kind) = if privilege == "SUBSCRIBE" {
        // The topic's name comes in db_name.
        (Some(db), Some(kinds::TOPIC.to_string()))
    } else if db.eq_ignore_ascii_case("all") || db == "*" {
        (None, None)
    } else if table.is_empty() {
        (Some(db), Some("database".to_string()))
    } else {
        let full = format!("{db}.{table}");
        let kind = if stables.contains(&full) { SUPERTABLE } else { kinds::TABLE };
        (Some(full), Some(kind.to_string()))
    };
    Grant { privilege, object, object_kind, ..Default::default() }
}

pub async fn grants(s: &TdSession, principal: &str) -> Result<Vec<Grant>> {
    let rows = s
        .records(&format!("SELECT * FROM information_schema.ins_user_privileges WHERE user_name = {}", lit(principal)))
        .await?;
    let mut stables = Vec::new();
    let dbs: Vec<String> = rows.iter().filter(|r| !field(r, "table_name").is_empty()).map(|r| field(r, "db_name")).collect();
    if !dbs.is_empty() {
        let list = dbs.iter().map(|d| lit(d)).collect::<Vec<_>>().join(", ");
        if let Ok(st) = s.records(&format!("SELECT db_name, stable_name FROM information_schema.ins_stables WHERE db_name IN ({list})")).await {
            stables = st.iter().map(|r| format!("{}.{}", field(r, "db_name"), field(r, "stable_name"))).collect();
        }
    }
    Ok(rows.iter().map(|r| grant_of(r, &stables)).collect())
}

// -- scripts -------------------------------------------------------------------

fn privileges(p: &[String], object: &Option<ObjectRef>) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out = Vec::new();
    for x in p {
        let up = x.trim().to_uppercase();
        if !matches!(up.as_str(), "READ" | "WRITE" | "ALL" | "SUBSCRIBE" | "ALTER") {
            return Err(Error::Query(format!("«{x}» no es un permiso de TDengine (READ, WRITE, ALL o SUBSCRIBE)")));
        }
        out.push(up);
    }
    let topic = object.as_ref().is_some_and(|o| o.kind == kinds::TOPIC);
    let subscribe = out.iter().any(|p| p == "SUBSCRIBE");
    if subscribe && (!topic || out.len() > 1) {
        return Err(Error::Query("SUBSCRIBE va solo y sobre un tópico".into()));
    }
    if topic && !subscribe {
        return Err(Error::Query("sobre un tópico, TDengine solo otorga SUBSCRIBE".into()));
    }
    Ok(out.join(", "))
}

/// What a privilege applies to: every database, one database, a table or
/// a topic.
fn on(object: &Option<ObjectRef>) -> Result<String> {
    Ok(match object {
        None => "*.*".into(),
        Some(o) if o.kind == "database" || o.kind == "schema" => format!("{}.*", q(&o.name)),
        Some(o) if o.kind == kinds::TOPIC => q(&o.name),
        Some(o) => match o.schema() {
            Some(db) => format!("{}.{}", q(db), q(&o.name)),
            None => return Err(Error::Query(format!("falta la base de datos de «{}»", o.name))),
        },
    })
}

const NO_ROLES: &str = "TDengine no tiene roles: los permisos se otorgan a cada usuario";

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            format!("CREATE USER {} PASS {};", q(name), lit(pw))
        }
        SecurityAction::SetPassword { name, password } => format!("ALTER USER {} PASS {};", q(name), lit(password)),
        SecurityAction::SetLogin { name, enabled } => format!("ALTER USER {} ENABLE {};", q(name), u8::from(*enabled)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", q(name)),
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            if *grantable {
                return Err(Error::Unsupported("TDengine no permite que un usuario otorgue sus permisos a otros".into()));
            }
            format!("GRANT {} ON {} TO {};", privileges(p, object)?, on(object)?, q(to))
        }
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {} ON {} FROM {};", privileges(p, object)?, on(object)?, q(from)),
        SecurityAction::CreateRole { .. }
        | SecurityAction::Drop { kind: PrincipalKind::Role, .. }
        | SecurityAction::AddMember { .. }
        | SecurityAction::RemoveMember { .. } => return Err(Error::Unsupported(NO_ROLES.into())),
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
        let s = |a| script(&a).unwrap();
        assert_eq!(s(SecurityAction::CreateUser { name: "ana`b".into(), password: Some("p'w\\".into()) }), "CREATE USER `ana``b` PASS 'p''w\\\\';");
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "n".into() }), "ALTER USER `ana` PASS 'n';");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }), "ALTER USER `ana` ENABLE 0;");
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: true }), "ALTER USER `ana` ENABLE 1;");
        assert_eq!(s(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User }), "DROP USER `ana`;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["read".into(), "WRITE".into()], object: obj("database", None, "sd`b"), to: "ana".into(), grantable: false }),
            "GRANT READ, WRITE ON `sd``b`.* TO `ana`;"
        );
        assert_eq!(s(SecurityAction::Grant { privileges: vec!["ALL".into()], object: None, to: "ana".into(), grantable: false }), "GRANT ALL ON *.* TO `ana`;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["READ".into()], object: obj(SUPERTABLE, Some("sdb"), "st"), to: "ana".into(), grantable: false }),
            "GRANT READ ON `sdb`.`st` TO `ana`;"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["SUBSCRIBE".into()], object: obj(kinds::TOPIC, Some("sdb"), "tp"), to: "ana".into(), grantable: false }),
            "GRANT SUBSCRIBE ON `tp` TO `ana`;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["WRITE".into()], object: obj(kinds::TABLE, Some("sdb"), "t1"), from: "ana".into() }),
            "REVOKE WRITE ON `sdb`.`t1` FROM `ana`;"
        );
        for bad in [
            SecurityAction::Grant { privileges: vec!["READ; DROP DATABASE x".into()], object: None, to: "a".into(), grantable: false },
            SecurityAction::Grant { privileges: vec!["SUBSCRIBE".into()], object: None, to: "a".into(), grantable: false },
            SecurityAction::Grant { privileges: vec!["READ".into()], object: obj(kinds::TOPIC, None, "tp"), to: "a".into(), grantable: false },
            SecurityAction::Grant { privileges: vec!["READ".into()], object: obj(kinds::TABLE, None, "t"), to: "a".into(), grantable: false },
            SecurityAction::Grant { privileges: vec![], object: None, to: "a".into(), grantable: false },
        ] {
            assert!(matches!(script(&bad), Err(Error::Query(_))), "{bad:?}");
        }
        for a in [
            SecurityAction::CreateRole { name: "r".into() },
            SecurityAction::AddMember { role: "r".into(), member: "a".into() },
            SecurityAction::Grant { privileges: vec!["READ".into()], object: None, to: "a".into(), grantable: true },
        ] {
            assert!(matches!(script(&a), Err(Error::Unsupported(_))));
        }
    }

    fn row(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn reads_users_and_privileges() {
        let p = principal_of(&row(json!({"name": "ana", "super": 0, "enable": 0, "sysinfo": 1, "createdb": 0, "create_time": "2026-01-01", "allowed_host": ""})));
        assert_eq!((p.superuser, p.disabled, p.system, p.kind), (Some(false), Some(true), false, PrincipalKind::User));
        assert!(principal_of(&row(json!({"name": "root", "super": 1, "enable": 1}))).system);
        let g = |v: Value| grant_of(&row(v), &["sdb.st".to_string()]);
        let all = g(json!({"privilege": "all", "db_name": "all", "table_name": ""}));
        assert_eq!((all.privilege.as_str(), all.object), ("ALL", None));
        let db = g(json!({"privilege": "read", "db_name": "sdb", "table_name": ""}));
        assert_eq!((db.object.as_deref(), db.object_kind.as_deref()), (Some("sdb"), Some("database")));
        let st = g(json!({"privilege": "read", "db_name": "sdb", "table_name": "st"}));
        assert_eq!((st.object.as_deref(), st.object_kind.as_deref()), (Some("sdb.st"), Some(SUPERTABLE)));
        let t = g(json!({"privilege": "write", "db_name": "sdb", "table_name": "t1", "condition": "(v > 0)"}));
        assert_eq!((t.privilege.as_str(), t.object.as_deref(), t.object_kind.as_deref()), ("WRITE", Some("sdb.t1"), Some(kinds::TABLE)));
        let tp = g(json!({"privilege": "subscribe", "db_name": "tp1", "table_name": ""}));
        assert_eq!((tp.object.as_deref(), tp.object_kind.as_deref()), (Some("tp1"), Some(kinds::TOPIC)));
    }
}
