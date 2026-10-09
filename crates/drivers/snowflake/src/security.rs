//! Users, roles and permissions (docs/users-and-permissions.md) for Snowflake.
//!
//! Snowflake's access control is role based: privileges on objects go to
//! roles, and users hold roles (`GRANT ROLE … TO USER`). A user has no
//! privileges of its own, so a grant "to a user" is written to the role of
//! that name and the script says so. `SHOW USERS` / `SHOW ROLES` list them
//! (SHOW USERS needs MANAGE GRANTS or ownership; without it only the own user
//! is listed), `SHOW GRANTS OF ROLE` gives each role's members and `SHOW
//! GRANTS TO ROLE / USER` the privileges, followed through the roles.
//!
//! Users and roles are separate namespaces and the script doesn't know which
//! one a name is, so membership changes are a Snowflake Scripting block
//! that looks the member up among the users when it runs.

use crate::SnowflakeSession;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{
    Error, Grant, ObjectRef, Principal, PrincipalKind, QueryOutcome, Result, SchemaSpec, SecurityAction, SecuritySpec,
};
use serde_json::Value as Json;
use std::collections::{HashMap, HashSet, VecDeque};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            // On objects.
            "SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "USAGE", "MONITOR", "OPERATE", "MODIFY",
            "CREATE SCHEMA", "CREATE TABLE", "CREATE VIEW", "ALL PRIVILEGES",
            // On the account (granted without an object).
            "CREATE DATABASE", "CREATE WAREHOUSE", "CREATE ROLE", "CREATE USER", "MANAGE GRANTS", "MONITOR USAGE",
            "EXECUTE TASK", "IMPORTED PRIVILEGES",
        ],
        // "" = the account.
        object_kinds: vec!["", "database", "schema", "table", "view", "materialized_view"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: false,
    }
}

/// Roles every account has.
const SYSTEM_ROLES: &[&str] = &["ACCOUNTADMIN", "SECURITYADMIN", "SYSADMIN", "USERADMIN", "ORGADMIN", "GLOBALORGADMIN", "PUBLIC"];

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

// -- reading -----------------------------------------------------------------

type Row = HashMap<String, String>;

fn get<'a>(r: &'a Row, k: &str) -> Option<&'a str> {
    r.get(k).map(String::as_str).filter(|v| !v.is_empty() && *v != "null")
}

fn yes(v: Option<&str>) -> bool {
    v.is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn user_of(r: &Row) -> Principal {
    let name = get(r, "name").unwrap_or_default().to_string();
    let mut details = Vec::new();
    for (label, k) in [
        ("Login", "login_name"),
        ("Nombre visible", "display_name"),
        ("Email", "email"),
        ("Tipo", "type"),
        ("Rol predeterminado", "default_role"),
        ("Warehouse predeterminado", "default_warehouse"),
        ("Espacio de nombres predeterminado", "default_namespace"),
        ("Creado", "created_on"),
        ("Último ingreso", "last_success_login"),
        ("Vence", "expires_at_time"),
        ("Bloqueado hasta", "locked_until_time"),
        ("Dueño", "owner"),
        ("Comentario", "comment"),
    ] {
        if let Some(v) = get(r, k) {
            if !(k == "login_name" && v.eq_ignore_ascii_case(&name)) {
                details.push((label.to_string(), v.to_string()));
            }
        }
    }
    let mut auth = Vec::new();
    if yes(get(r, "has_password")) {
        auth.push("contraseña");
    }
    if yes(get(r, "has_rsa_public_key")) {
        auth.push("par de claves");
    }
    if yes(get(r, "has_mfa")) || yes(get(r, "ext_authn_duo")) {
        auth.push("MFA");
    }
    if !auth.is_empty() {
        details.push(("Autenticación".into(), auth.join(", ")));
    }
    if yes(get(r, "must_change_password")) {
        details.push(("Contraseña".into(), "debe cambiarla".into()));
    }
    Principal {
        kind: PrincipalKind::User,
        can_login: Some(true),
        superuser: None,
        disabled: Some(yes(get(r, "disabled"))),
        member_of: Vec::new(),
        details,
        system: name == "SNOWFLAKE",
        name,
    }
}

fn role_of(r: &Row) -> Principal {
    let name = get(r, "name").unwrap_or_default().to_string();
    let mut details = Vec::new();
    for (label, k) in [
        ("Usuarios", "assigned_to_users"),
        ("Otorgado a roles", "granted_to_roles"),
        ("Roles que tiene", "granted_roles"),
        ("Dueño", "owner"),
        ("Creado", "created_on"),
        ("Comentario", "comment"),
    ] {
        if let Some(v) = get(r, k) {
            details.push((label.to_string(), v.to_string()));
        }
    }
    Principal {
        kind: PrincipalKind::Role,
        superuser: Some(name == "ACCOUNTADMIN"),
        system: SYSTEM_ROLES.contains(&name.as_str()),
        details,
        name,
        ..Default::default()
    }
}

/// Whether a role has members worth asking for (SHOW ROLES counts them).
fn has_members(r: &Row) -> bool {
    ["assigned_to_users", "granted_to_roles"].iter().any(|k| get(r, k).and_then(|v| v.parse::<u64>().ok()).is_none_or(|n| n > 0))
}

fn text(v: &Json) -> Option<String> {
    match v {
        Json::String(s) => Some(s.clone()),
        Json::Null => None,
        v => Some(v.to_string()),
    }
}

/// `SHOW GRANTS OF ROLE` rows as (role, member): one statement per role in
/// one request; statement by statement when a role vanished in between.
async fn memberships(s: &SnowflakeSession, roles: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let collect = |o: &QueryOutcome, out: &mut Vec<(String, String)>| {
        for r in &o.results {
            let col = |n: &str| r.columns.iter().position(|c| c.name.eq_ignore_ascii_case(n));
            let (Some(ri), Some(gi)) = (col("role"), col("grantee_name")) else { continue };
            for row in &r.rows {
                if let (Some(role), Some(member)) = (row.get(ri).and_then(text), row.get(gi).and_then(text)) {
                    out.push((role, member));
                }
            }
        }
    };
    for chunk in roles.chunks(50) {
        let script: String = chunk.iter().map(|r| format!("SHOW GRANTS OF ROLE {};\n", q(r))).collect();
        let mut o = QueryOutcome::default();
        if s.run_script(&script, 100_000, &mut o).await.is_ok() {
            collect(&o, &mut out);
            continue;
        }
        for r in chunk {
            let mut o = QueryOutcome::default();
            if s.run_script(&format!("SHOW GRANTS OF ROLE {}", q(r)), 100_000, &mut o).await.is_ok() {
                collect(&o, &mut out);
            }
        }
    }
    out
}

pub async fn principals(s: &SnowflakeSession) -> Result<Vec<Principal>> {
    let mut out: Vec<Principal> = match s.named_rows("SHOW USERS").await {
        Ok(rows) => rows.iter().map(user_of).collect(),
        Err(_) => {
            let me = s.text_rows("SELECT CURRENT_USER()", &[]).await?;
            let name = me.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default();
            vec![Principal {
                name,
                kind: PrincipalKind::User,
                can_login: Some(true),
                details: vec![("Nota".into(), "El rol actual no puede listar usuarios (SHOW USERS): se ve solo el propio".into())],
                ..Default::default()
            }]
        }
    };
    let roles = s.named_rows("SHOW ROLES").await?;
    let asked: Vec<String> = roles.iter().filter(|r| has_members(r)).filter_map(|r| get(r, "name").map(str::to_string)).collect();
    out.extend(roles.iter().map(role_of));
    for (role, member) in memberships(s, &asked).await {
        // A user and a role can share a name: the membership row doesn't
        // say which, so both get it.
        for p in out.iter_mut().filter(|p| p.name == member) {
            if !p.member_of.contains(&role) {
                if role == "ACCOUNTADMIN" {
                    p.superuser = Some(true);
                }
                p.member_of.push(role.clone());
            }
        }
    }
    for p in out.iter_mut().filter(|p| p.kind == PrincipalKind::User && p.superuser.is_none()) {
        p.superuser = Some(false);
    }
    Ok(out)
}

/// `DB.SCHEMA."t"` split into its parts, without quotes.
fn split_name(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            '.' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// One row of `SHOW GRANTS TO ROLE`: the grant, or the role it inherits.
/// Names in the session's database lose the database.
fn grant_of(r: &Row, database: Option<&str>) -> std::result::Result<Grant, String> {
    let privilege = get(r, "privilege").unwrap_or_default().to_string();
    let on = get(r, "granted_on").unwrap_or_default().to_ascii_uppercase();
    let name = get(r, "name").unwrap_or_default();
    if on == "ROLE" && privilege == "USAGE" {
        return Err(split_name(name).join("."));
    }
    let mut parts = split_name(name);
    let in_db = |p: &[String]| database.is_some_and(|d| p.first().is_some_and(|f| f.eq_ignore_ascii_case(d)));
    let (object, object_kind) = match on.as_str() {
        "ACCOUNT" => (None, None),
        "DATABASE" => (Some(parts.join(".")), Some("database".to_string())),
        _ => {
            if parts.len() > 1 && in_db(&parts) && on != "DATABASE_ROLE" {
                parts.remove(0);
            }
            (Some(parts.join(".")), Some(on.to_ascii_lowercase()))
        }
    };
    Ok(Grant { privilege, object, object_kind, grantable: yes(get(r, "grant_option")), denied: false, via: None })
}

pub async fn grants(s: &SnowflakeSession, principal: &str) -> Result<Vec<Grant>> {
    let db = s.ctx.database.clone();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>)> = VecDeque::new();
    // A user's grants are its roles; a name that isn't a user is a role.
    match s.named_rows(&format!("SHOW GRANTS TO USER {}", q(principal))).await {
        Ok(rows) => {
            for r in &rows {
                if let Some(role) = get(r, "role") {
                    queue.push_back((role.to_string(), Some(role.to_string())));
                }
            }
        }
        Err(_) => queue.push_back((principal.to_string(), None)),
    }
    while let Some((role, via)) = queue.pop_front() {
        if !seen.insert(role.clone()) || seen.len() > 256 {
            continue;
        }
        let rows = match s.named_rows(&format!("SHOW GRANTS TO ROLE {}", q(&role))).await {
            Ok(r) => r,
            Err(e) if via.is_none() => return Err(e),
            Err(_) => continue,
        };
        for r in &rows {
            match grant_of(r, db.as_deref()) {
                Ok(mut g) => {
                    g.via = via.clone();
                    out.push(g);
                }
                Err(inherited) => {
                    let v = via.clone().unwrap_or_else(|| inherited.clone());
                    queue.push_back((inherited, Some(v)));
                }
            }
        }
    }
    Ok(out)
}

// -- scripts -----------------------------------------------------------------

/// A new user's or role's name: a plain identifier is uppercased, as
/// Snowflake does with unquoted names; anything else is kept as typed.
fn new_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Query("escribí el nombre".into()));
    }
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    Ok(q(&if plain { name.to_ascii_uppercase() } else { name.to_string() }))
}

/// A string literal: Snowflake reads backslash escapes in them.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn password(p: Option<&str>) -> Result<String> {
    match p.filter(|p| !p.is_empty()) {
        Some(p) => Ok(lit(p)),
        None => Err(Error::Query("escribí la contraseña del usuario".into())),
    }
}

/// What a privilege applies to: `ACCOUNT`, `DATABASE "x"`, `TABLE "s"."t"`…
fn on(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else { return Ok("ACCOUNT".into()) };
    if o.kind.is_empty() || !o.kind.chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
        return Err(Error::Query(format!("«{}» no es un tipo de objeto de Snowflake", o.kind)));
    }
    let kind = o.kind.to_ascii_uppercase().replace('_', " ");
    Ok(match (o.kind.as_str(), o.schema()) {
        ("database", _) | (_, None) => format!("{kind} {}", q(&o.name)),
        (_, Some(sc)) => format!("{kind} {}.{}", q(sc), q(&o.name)),
    })
}

/// Privilege names: letters, spaces and underscores.
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out: Vec<String> = Vec::new();
    for x in p {
        let name = x.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '_') {
            return Err(Error::Query(format!("«{x}» no es un permiso de Snowflake")));
        }
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase();
        if !out.contains(&name) {
            out.push(name);
        }
    }
    Ok(out.join(", "))
}

/// A `SHOW … LIKE` pattern that matches only `name`: its wildcards (and
/// the backslash that escapes them) escaped.
fn like_exact(name: &str) -> String {
    name.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Grants (or revokes) a role to a member that may be a user or a role: the
/// script can't know which, so a Snowflake Scripting block asks the server
/// at run time (SHOW USERS, then the exact name among its rows). The block
/// is a `$$` string, so no name may contain `$$`.
fn membership(role: &str, member: &str, grant: bool) -> Result<String> {
    if role.contains("$$") || member.contains("$$") {
        return Err(Error::Query("el nombre no puede contener «$$»".into()));
    }
    let (r, m) = (q(role), q(member));
    let (verb, prep) = if grant { ("GRANT", "TO") } else { ("REVOKE", "FROM") };
    Ok(format!(
        "EXECUTE IMMEDIATE $$\n\
         DECLARE\n  users INTEGER DEFAULT 0;\n\
         BEGIN\n  \
         SHOW USERS LIKE {pattern};\n  \
         SELECT COUNT(*) INTO :users FROM TABLE(RESULT_SCAN(LAST_QUERY_ID())) WHERE \"name\" = {name};\n  \
         IF (users > 0) THEN\n    {verb} ROLE {r} {prep} USER {m};\n  \
         ELSE\n    {verb} ROLE {r} {prep} ROLE {m};\n  \
         END IF;\n\
         END;\n\
         $$;",
        pattern = lit(&like_exact(member)),
        name = lit(member),
    ))
}

const TO_ROLES: &str = "-- En Snowflake los permisos se otorgan a roles: si es un usuario, otorgáselos a un rol y agregalo a ese rol.";

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name, password: pw } => {
            format!("CREATE USER {} PASSWORD = {};", new_name(name)?, password(pw.as_deref())?)
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", new_name(name)?),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => format!("DROP USER {};", q(name)),
        SecurityAction::SetPassword { name, password: pw } => {
            format!("ALTER USER {} SET PASSWORD = {};", q(name), password(Some(pw))?)
        }
        SecurityAction::SetLogin { name, enabled } => {
            format!("ALTER USER {} SET DISABLED = {};", q(name), if *enabled { "FALSE" } else { "TRUE" })
        }
        SecurityAction::Grant { privileges: p, object, to, grantable } => format!(
            "{TO_ROLES}\nGRANT {} ON {} TO ROLE {}{};",
            privileges(p)?,
            on(object)?,
            q(to),
            if *grantable { " WITH GRANT OPTION" } else { "" }
        ),
        SecurityAction::Revoke { privileges: p, object, from } => {
            format!("REVOKE {} ON {} FROM ROLE {};", privileges(p)?, on(object)?, q(from))
        }
        SecurityAction::AddMember { role, member } => membership(role, member, true)?,
        SecurityAction::RemoveMember { role, member } => membership(role, member, false)?,
    })
}

// -- schemas -----------------------------------------------------------------

/// "Nuevo esquema…": the owner is a role (`GRANT OWNERSHIP … TO ROLE`,
/// there's no `AUTHORIZATION` and a user can't own one), handed over after
/// the grants; dropping always takes the contents with it: the default is
/// CASCADE, and RESTRICT only refuses when another schema's foreign keys
/// point in, so there's no "only if empty" drop to offer.
pub fn schema_spec() -> SchemaSpec {
    SchemaSpec {
        owner: true,
        owner_kinds: dbine_driver::SchemaOwnerKinds::Roles,
        cascade: false,
        privileges: vec![
            "USAGE", "MONITOR", "MODIFY", "CREATE TABLE", "CREATE VIEW", "CREATE MATERIALIZED VIEW", "CREATE SEQUENCE",
            "CREATE FUNCTION", "CREATE PROCEDURE", "CREATE STAGE", "CREATE FILE FORMAT", "CREATE STREAM", "CREATE TASK",
            "CREATE PIPE", "ALL PRIVILEGES",
        ],
        grant_option: true,
    }
}

/// The name as typed, quoted: the grants that follow name it the same way.
fn schema_name(name: &str) -> Result<String> {
    match name.trim() {
        "" => Err(Error::Query("escribí el nombre del esquema".into())),
        n => Ok(q(n)),
    }
}

/// Owned by the creating role; `schema_owner` hands it over afterwards.
pub fn create_schema(name: &str) -> Result<String> {
    Ok(format!("CREATE SCHEMA {};", schema_name(name)?))
}

/// Hands the schema to role `owner`, keeping the grants already made on it
/// (without COPY CURRENT GRANTS, Snowflake refuses the transfer when the
/// schema has any; REVOKE would drop the ones the dialog just made). Runs
/// after the grants: once it's given away, the creating role can't grant
/// on it unless it inherits `owner` or has MANAGE GRANTS.
pub fn schema_owner(name: &str, owner: &str) -> Result<String> {
    match owner.trim() {
        "" => Err(Error::Query("elegí el rol dueño del esquema".into())),
        o => Ok(format!(
            "-- En Snowflake el dueño es un rol; COPY CURRENT GRANTS conserva los permisos ya otorgados.\nGRANT OWNERSHIP ON SCHEMA {} TO ROLE {} COPY CURRENT GRANTS;",
            schema_name(name)?,
            q(o)
        )),
    }
}

pub fn drop_schema(name: &str) -> Result<String> {
    Ok(format!("-- En Snowflake, borrar un esquema borra también todo lo que contiene.\nDROP SCHEMA {};", schema_name(name)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    fn row(pairs: &[(&str, &str)]) -> Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w\\x".into()) }),
            "CREATE USER \"ANA\" PASSWORD = 'p\\'w\\\\x';"
        );
        assert_eq!(s(SecurityAction::CreateUser { name: "ana.b".into(), password: Some("x".into()) }), "CREATE USER \"ana.b\" PASSWORD = 'x';");
        assert!(script(&SecurityAction::CreateUser { name: "ana".into(), password: None }).is_err());
        assert_eq!(s(SecurityAction::CreateRole { name: "lectores".into() }), "CREATE ROLE \"LECTORES\";");
        assert_eq!(s(SecurityAction::Drop { name: "A\"B".into(), kind: PrincipalKind::User }), "DROP USER \"A\"\"B\";");
        assert_eq!(s(SecurityAction::Drop { name: "LECT".into(), kind: PrincipalKind::Role }), "DROP ROLE \"LECT\";");
        assert_eq!(s(SecurityAction::SetPassword { name: "ANA".into(), password: "n'".into() }), "ALTER USER \"ANA\" SET PASSWORD = 'n\\'';");
        assert_eq!(s(SecurityAction::SetLogin { name: "ANA".into(), enabled: false }), "ALTER USER \"ANA\" SET DISABLED = TRUE;");
        assert_eq!(s(SecurityAction::SetLogin { name: "ANA".into(), enabled: true }), "ALTER USER \"ANA\" SET DISABLED = FALSE;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), "INSERT".into()], object: obj("table", Some("PUBLIC"), "fac\"t"), to: "LECT".into(), grantable: true }),
            format!("{TO_ROLES}\nGRANT SELECT, INSERT ON TABLE \"PUBLIC\".\"fac\"\"t\" TO ROLE \"LECT\" WITH GRANT OPTION;")
        );
        assert!(s(SecurityAction::Grant { privileges: vec!["create database".into()], object: None, to: "R".into(), grantable: false })
            .ends_with("\nGRANT CREATE DATABASE ON ACCOUNT TO ROLE \"R\";"));
        assert!(s(SecurityAction::Grant { privileges: vec!["USAGE".into()], object: obj("database", None, "VENTAS"), to: "R".into(), grantable: false })
            .ends_with("GRANT USAGE ON DATABASE \"VENTAS\" TO ROLE \"R\";"));
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: obj("materialized_view", Some("S"), "MV"), from: "R".into() }),
            "REVOKE SELECT ON MATERIALIZED VIEW \"S\".\"MV\" FROM ROLE \"R\";"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["USAGE".into()], object: obj("schema", None, "S"), from: "R".into() }),
            "REVOKE USAGE ON SCHEMA \"S\" FROM ROLE \"R\";"
        );
        assert_eq!(
            s(SecurityAction::AddMember { role: "LECT".into(), member: "ANA".into() }),
            "EXECUTE IMMEDIATE $$\nDECLARE\n  users INTEGER DEFAULT 0;\nBEGIN\n  SHOW USERS LIKE 'ANA';\n  \
             SELECT COUNT(*) INTO :users FROM TABLE(RESULT_SCAN(LAST_QUERY_ID())) WHERE \"name\" = 'ANA';\n  \
             IF (users > 0) THEN\n    GRANT ROLE \"LECT\" TO USER \"ANA\";\n  ELSE\n    GRANT ROLE \"LECT\" TO ROLE \"ANA\";\n  END IF;\nEND;\n$$;"
        );
        // Wildcards, backslashes and quotes in the member's name.
        let r = s(SecurityAction::RemoveMember { role: "R\"1".into(), member: "a_b%c\\d'e\"f".into() });
        assert!(r.contains("SHOW USERS LIKE 'a\\\\_b\\\\%c\\\\\\\\d\\'e\"f';"), "{r}");
        assert!(r.contains("WHERE \"name\" = 'a_b%c\\\\d\\'e\"f';"), "{r}");
        assert!(r.contains("REVOKE ROLE \"R\"\"1\" FROM USER \"a_b%c\\d'e\"\"f\";"), "{r}");
        assert!(r.contains("REVOKE ROLE \"R\"\"1\" FROM ROLE \"a_b%c\\d'e\"\"f\";"), "{r}");
        // The block's $$ can't be closed by a name.
        assert!(script(&SecurityAction::AddMember { role: "R".into(), member: "a$$b".into() }).is_err());
        assert!(script(&SecurityAction::AddMember { role: "x$$".into(), member: "a".into() }).is_err());
        assert!(s(SecurityAction::AddMember { role: "R".into(), member: "a$".into() }).ends_with("END;\n$$;"));
        // One statement for the driver (no MULTI_STATEMENT_COUNT).
        assert_eq!(crate::split_script(&r).len(), 1);
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT; DROP TABLE x".into()], object: None, to: "a".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("table; x", None, "t"), to: "a".into(), grantable: false }).is_err());
    }

    #[test]
    fn reads_show_grants() {
        assert_eq!(split_name("DB.\"My \"\"S\".T"), vec!["DB", "My \"S", "T"]);
        let g = grant_of(&row(&[("privilege", "SELECT"), ("granted_on", "TABLE"), ("name", "VENTAS.PUBLIC.FACT"), ("grant_option", "true")]), Some("VENTAS")).unwrap();
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref(), g.grantable), (Some("PUBLIC.FACT"), Some("table"), true));
        let g = grant_of(&row(&[("privilege", "SELECT"), ("granted_on", "VIEW"), ("name", "OTRA.PUBLIC.V"), ("grant_option", "false")]), Some("VENTAS")).unwrap();
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref()), (Some("OTRA.PUBLIC.V"), Some("view")));
        let g = grant_of(&row(&[("privilege", "USAGE"), ("granted_on", "DATABASE"), ("name", "VENTAS")]), Some("VENTAS")).unwrap();
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref()), (Some("VENTAS"), Some("database")));
        let g = grant_of(&row(&[("privilege", "CREATE ROLE"), ("granted_on", "ACCOUNT"), ("name", "AB12345")]), None).unwrap();
        assert_eq!(g.object, None);
        assert_eq!(grant_of(&row(&[("privilege", "USAGE"), ("granted_on", "ROLE"), ("name", "SYSADMIN")]), None), Err("SYSADMIN".to_string()));
        assert!(has_members(&row(&[("assigned_to_users", "0"), ("granted_to_roles", "1")])));
        assert!(!has_members(&row(&[("assigned_to_users", "0"), ("granted_to_roles", "0")])));
    }

    #[test]
    fn schema_scripts() {
        assert_eq!(create_schema(" ventas ").unwrap(), "CREATE SCHEMA \"ventas\";");
        assert_eq!(create_schema("Ven\"tas").unwrap(), "CREATE SCHEMA \"Ven\"\"tas\";");
        let o = schema_owner("Ven\"tas", " R1 ").unwrap();
        assert!(o.starts_with("-- ") && o.ends_with("\nGRANT OWNERSHIP ON SCHEMA \"Ven\"\"tas\" TO ROLE \"R1\" COPY CURRENT GRANTS;"), "{o}");
        assert!(schema_owner("ventas", " ").is_err());
        assert!(create_schema(" ").is_err());
        assert!(drop_schema("ventas").unwrap().ends_with("\nDROP SCHEMA \"ventas\";"));
        let spec = schema_spec();
        assert!(spec.owner && !spec.cascade && spec.owner_kinds == dbine_driver::SchemaOwnerKinds::Roles);
        // Every offered privilege goes through the grant script on the schema.
        let g = script(&SecurityAction::Grant {
            privileges: spec.privileges.iter().map(|p| p.to_string()).collect(),
            object: obj("schema", None, "ventas"),
            to: "LECT".into(),
            grantable: true,
        })
        .unwrap();
        assert!(g.ends_with("ON SCHEMA \"ventas\" TO ROLE \"LECT\" WITH GRANT OPTION;"), "{g}");
        assert!(g.contains("GRANT USAGE, MONITOR, MODIFY, CREATE TABLE"), "{g}");
    }
}
