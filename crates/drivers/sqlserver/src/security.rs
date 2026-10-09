//! Users, roles and permissions of the current database
//! (docs/users-and-permissions.md). SQL Server's users live in a database and
//! (outside Azure SQL's contained users) sign in through a server login.

use crate::variant::Variant;
use crate::SqlServerSession;
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SchemaSpec, SecurityAction, SecuritySpec};
use tiberius::Row;

pub fn spec(v: Variant) -> SecuritySpec {
    let full = SecuritySpec {
        privileges: vec![
            "SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "REFERENCES", "VIEW DEFINITION", "ALTER", "CONTROL", "CONNECT",
            "CREATE TABLE", "CREATE VIEW", "CREATE PROCEDURE", "CREATE FUNCTION", "CREATE SCHEMA",
        ],
        object_kinds: vec!["", "schema", "table", "view", "procedure", "function"],
        create_user: true,
        create_role: true,
        passwords: true,
        membership: true,
        per_database: true,
    };
    match v {
        // Fabric: Microsoft Entra ID principals only (no logins, no
        // passwords, no CREATE USER: a GRANT creates the user), custom and
        // built-in database roles, GRANT / REVOKE / DENY on the warehouse,
        // schemas and objects (docs: "SQL granular permissions in Fabric
        // Data Warehouse"). UNMASK goes with its dynamic data masking.
        Variant::Fabric => SecuritySpec {
            privileges: vec![
                "SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "REFERENCES", "VIEW DEFINITION", "ALTER", "CONTROL", "UNMASK",
                "CONNECT", "CREATE TABLE", "CREATE VIEW", "CREATE PROCEDURE", "CREATE FUNCTION", "CREATE SCHEMA",
            ],
            passwords: false,
            ..full
        },
        // Babelfish maps object and schema GRANTs onto PostgreSQL's ACLs;
        // it rejects database-wide GRANTs and ALTER / CONTROL / VIEW
        // DEFINITION (Babelfish 5.4, "is not currently supported").
        Variant::Babelfish => SecuritySpec {
            privileges: BABELFISH_PRIVILEGES.to_vec(),
            object_kinds: vec!["schema", "table", "view", "procedure", "function"],
            ..full
        },
        Variant::SqlServer | Variant::AzureSql => full,
    }
}

/// What Babelfish grants through T-SQL, in PostgreSQL's ACL names too.
const BABELFISH_PRIVILEGES: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "REFERENCES", "EXECUTE"];

/// "Nuevo esquema…" / "Borrar esquema…". T-SQL's DROP SCHEMA has no
/// CASCADE (it fails while the schema holds objects), so `cascade` is off
/// everywhere.
/// - SQL Server and Azure SQL: `CREATE SCHEMA … AUTHORIZATION`, and the
///   schema class's permissions (`sys.fn_builtin_permissions('SCHEMA')`,
///   without SQL Server 2022's UNMASK, which older servers reject).
/// - Fabric: no owner offered (its CREATE SCHEMA's AUTHORIZATION isn't
///   verifiable without a warehouse); the schema grants its granular
///   permissions document.
/// - Babelfish: `AUTHORIZATION` works (a database user); grants on a schema
///   are the object privileges it maps onto PostgreSQL's ACLs.
pub fn schema_spec(v: Variant) -> SchemaSpec {
    match v {
        Variant::SqlServer | Variant::AzureSql => SchemaSpec {
            owner: true,
            owner_kinds: dbine_driver::SchemaOwnerKinds::Both,
            cascade: false,
            privileges: vec![
                "SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "REFERENCES", "VIEW DEFINITION", "ALTER", "CONTROL", "TAKE OWNERSHIP",
                "CREATE SEQUENCE", "VIEW CHANGE TRACKING",
            ],
            grant_option: true,
        },
        Variant::Fabric => SchemaSpec {
            owner: false,
            owner_kinds: dbine_driver::SchemaOwnerKinds::Both,
            cascade: false,
            privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "EXECUTE", "REFERENCES", "VIEW DEFINITION", "ALTER", "CONTROL"],
            grant_option: true,
        },
        // "GRANT on SCHEMA .. WITH GRANT OPTION is not yet supported in Babelfish".
        Variant::Babelfish => SchemaSpec {
            owner: true,
            owner_kinds: dbine_driver::SchemaOwnerKinds::Both,
            cascade: false,
            privileges: BABELFISH_PRIVILEGES.to_vec(),
            grant_option: false,
        },
    }
}

/// Babelfish 5.4 can't read `]]` inside brackets (see `script`).
fn babelfish_name(v: Variant, names: &[&str]) -> Result<()> {
    if v == Variant::Babelfish && names.iter().any(|n| n.contains(']')) {
        return Err(Error::Query("Babelfish no acepta «]» en los nombres de esquemas ni de usuarios".into()));
    }
    Ok(())
}

/// `CREATE SCHEMA` must open its batch: the Tauri command closes it with
/// `GO` (`script_separator`) before the grants.
pub fn create_schema(v: Variant, name: &str, owner: Option<&str>) -> Result<String> {
    let owner = owner.map(str::trim).filter(|o| !o.is_empty());
    if owner.is_some() && !schema_spec(v).owner {
        return Err(Error::Unsupported("en este motor el esquema se crea sin dueño explícito".into()));
    }
    babelfish_name(v, &[name, owner.unwrap_or_default()])?;
    Ok(match owner {
        Some(o) => format!("CREATE SCHEMA {} AUTHORIZATION {};", q(name), q(o)),
        None => format!("CREATE SCHEMA {};", q(name)),
    })
}

pub fn drop_schema(v: Variant, name: &str, cascade: bool) -> Result<String> {
    if cascade {
        return Err(Error::Unsupported("SQL Server no borra un esquema con su contenido: primero hay que borrar o mover sus objetos".into()));
    }
    babelfish_name(v, &[name])?;
    Ok(format!("DROP SCHEMA {};", q(name)))
}

/// Every schema of the current database (`Session::list_schemas`). System:
/// `sys`, `INFORMATION_SCHEMA`, `guest` and the schemas the fixed database
/// roles own (`db_owner`, `db_datareader`…); Fabric adds `queryinsights`,
/// its read-only views of the warehouse's query history.
pub fn list_schemas_sql(v: Variant) -> String {
    let extra = if v == Variant::Fabric { ", 'queryinsights'" } else { "" };
    format!(
        "SELECT s.name,
                CAST(CASE WHEN s.name IN ('sys', 'INFORMATION_SCHEMA', 'guest'{extra}) OR p.is_fixed_role = 1 THEN 1 ELSE 0 END AS int)
           FROM sys.schemas s
           LEFT JOIN sys.database_principals p ON p.principal_id = s.principal_id
          ORDER BY s.name"
    )
}

const PRINCIPALS: &str = "
SELECT dp.name, dp.type,
       CAST(ISNULL(sp.is_disabled, 0) AS int),
       CAST(CASE WHEN dp.name = 'dbo' OR IS_SRVROLEMEMBER('sysadmin', sp.name) = 1 THEN 1 ELSE 0 END AS int),
       CAST(CASE WHEN dp.is_fixed_role = 1 OR dp.principal_id < 5 OR dp.name IN ('public', 'guest', 'INFORMATION_SCHEMA', 'sys') THEN 1 ELSE 0 END AS int),
       dp.default_schema_name, dp.authentication_type_desc, sp.name,
       CONVERT(nvarchar(30), dp.create_date, 120)
  FROM sys.database_principals dp
  LEFT JOIN sys.server_principals sp ON sp.sid = dp.sid
 WHERE dp.type IN ('S', 'U', 'G', 'R', 'E', 'X')
 ORDER BY CASE dp.type WHEN 'R' THEN 1 ELSE 0 END, dp.name";

/// Fabric has no server principals (`sys.server_principals`,
/// `IS_SRVROLEMEMBER`): its users are Microsoft Entra ID identities.
const FABRIC_PRINCIPALS: &str = "
SELECT dp.name, dp.type,
       CAST(0 AS int),
       CAST(CASE WHEN dp.name = 'dbo' THEN 1 ELSE 0 END AS int),
       CAST(CASE WHEN dp.is_fixed_role = 1 OR dp.principal_id < 5 OR dp.name IN ('public', 'guest', 'INFORMATION_SCHEMA', 'sys') THEN 1 ELSE 0 END AS int),
       dp.default_schema_name, dp.authentication_type_desc, CAST(NULL AS sysname),
       CONVERT(nvarchar(30), dp.create_date, 120)
  FROM sys.database_principals dp
 WHERE dp.type IN ('S', 'U', 'G', 'R', 'E', 'X')
 ORDER BY CASE dp.type WHEN 'R' THEN 1 ELSE 0 END, dp.name";

/// Every explicit permission of the warehouse, by grantee. Fabric runs no
/// recursive queries, so the roles a principal holds are followed in Rust
/// (`held_roles`).
const FABRIC_GRANTS: &str = "
SELECT dp.name, p.permission_name, p.state,
       CASE p.class WHEN 0 THEN NULL
                    WHEN 1 THEN OBJECT_SCHEMA_NAME(p.major_id) + N'.' + OBJECT_NAME(p.major_id)
                                + CASE WHEN p.minor_id > 0 THEN N'.' + COL_NAME(p.major_id, p.minor_id) ELSE N'' END
                    WHEN 3 THEN SCHEMA_NAME(p.major_id)
                    ELSE p.class_desc END,
       CASE p.class WHEN 0 THEN N'database' WHEN 3 THEN N'schema'
                    WHEN 1 THEN CASE WHEN o.type IN ('U') THEN N'table' WHEN o.type = 'V' THEN N'view'
                                     WHEN o.type IN ('P', 'PC') THEN N'procedure'
                                     WHEN o.type IN ('FN', 'IF', 'TF', 'FS', 'FT') THEN N'function' ELSE N'object' END
                    ELSE LOWER(p.class_desc) END
  FROM sys.database_permissions p
  JOIN sys.database_principals dp ON dp.principal_id = p.grantee_principal_id
  LEFT JOIN sys.objects o ON p.class = 1 AND o.object_id = p.major_id
 ORDER BY 4, 2";

/// Babelfish leaves `sys.database_permissions` empty: its GRANTs are
/// PostgreSQL ACLs on the objects (`relacl`, `proacl`), held by the
/// PostgreSQL role behind each T-SQL user (`sys.babelfish_authid_user_ext`).
/// A schema GRANT shows on each of the schema's objects: the catalog that
/// keeps it (`sys.babelfish_schema_permissions`) isn't readable over TDS.
const BABELFISH_GRANTS: &str = "
WITH r AS (
  SELECT principal_id, name, CAST(NULL AS sysname) AS via FROM sys.database_principals WHERE name = @P1
  UNION ALL
  SELECT rp.principal_id, rp.name, CAST(COALESCE(r.via, rp.name) AS sysname)
    FROM sys.database_role_members rm
    JOIN r ON rm.member_principal_id = r.principal_id
    JOIN sys.database_principals rp ON rp.principal_id = rm.role_principal_id
)
SELECT CAST(a.privilege_type AS nvarchar(30)),
       CASE CAST(a.is_grantable AS int) WHEN 1 THEN N'W' ELSE N'G' END,
       OBJECT_SCHEMA_NAME(o.object_id) + N'.' + o.name,
       CASE WHEN o.type = 'U' THEN N'table' WHEN o.type = 'V' THEN N'view'
            WHEN o.type IN ('P', 'PC') THEN N'procedure'
            WHEN o.type IN ('FN', 'IF', 'TF', 'FS', 'FT') THEN N'function' ELSE N'object' END,
       r.via
  FROM r
  JOIN sys.babelfish_authid_user_ext u ON u.orig_username = r.name AND u.database_name = DB_NAME()
  JOIN pg_catalog.pg_roles g ON g.rolname = u.rolname
  CROSS JOIN sys.objects o
  LEFT JOIN pg_catalog.pg_class c ON c.oid = o.object_id
  LEFT JOIN pg_catalog.pg_proc pp ON pp.oid = o.object_id
  CROSS APPLY pg_catalog.aclexplode(COALESCE(c.relacl, pp.proacl)) a
 WHERE a.grantee = g.oid AND a.privilege_type IN ('SELECT', 'INSERT', 'UPDATE', 'DELETE', 'REFERENCES', 'EXECUTE')
 ORDER BY CASE WHEN r.via IS NULL THEN 0 ELSE 1 END, 3, 1";

const MEMBERSHIPS: &str = "
SELECT m.name, r.name
  FROM sys.database_role_members rm
  JOIN sys.database_principals r ON r.principal_id = rm.role_principal_id
  JOIN sys.database_principals m ON m.principal_id = rm.member_principal_id";

/// A principal's permissions, directly and through its roles (recursively).
const GRANTS: &str = "
WITH r AS (
  SELECT principal_id, CAST(NULL AS sysname) AS via FROM sys.database_principals WHERE name = @P1
  UNION ALL
  SELECT rm.role_principal_id, CAST(COALESCE(r.via, rp.name) AS sysname)
    FROM sys.database_role_members rm
    JOIN r ON rm.member_principal_id = r.principal_id
    JOIN sys.database_principals rp ON rp.principal_id = rm.role_principal_id
)
SELECT p.permission_name, p.state,
       CASE p.class WHEN 0 THEN NULL
                    WHEN 1 THEN OBJECT_SCHEMA_NAME(p.major_id) + N'.' + OBJECT_NAME(p.major_id)
                                + CASE WHEN p.minor_id > 0 THEN N'.' + COL_NAME(p.major_id, p.minor_id) ELSE N'' END
                    WHEN 3 THEN SCHEMA_NAME(p.major_id)
                    ELSE p.class_desc END,
       CASE p.class WHEN 0 THEN N'database' WHEN 3 THEN N'schema'
                    WHEN 1 THEN CASE WHEN o.type IN ('U') THEN N'table' WHEN o.type = 'V' THEN N'view'
                                     WHEN o.type IN ('P', 'PC') THEN N'procedure'
                                     WHEN o.type IN ('FN', 'IF', 'TF', 'FS', 'FT') THEN N'function' ELSE N'object' END
                    ELSE LOWER(p.class_desc) END,
       r.via
  FROM r
  JOIN sys.database_permissions p ON p.grantee_principal_id = r.principal_id
  LEFT JOIN sys.objects o ON p.class = 1 AND o.object_id = p.major_id
 ORDER BY CASE WHEN r.via IS NULL THEN 0 ELSE 1 END, 3, 1
OPTION (MAXRECURSION 32)";

fn text(r: &Row, i: usize) -> Option<String> {
    r.try_get::<&str, _>(i).ok().flatten().map(str::to_string).filter(|s| !s.is_empty())
}
fn flag(r: &Row, i: usize) -> bool {
    r.try_get::<i32, _>(i).ok().flatten().unwrap_or(0) != 0
}

/// One row of `PRINCIPALS` / `FABRIC_PRINCIPALS`, read into plain values.
#[derive(Default)]
struct PrincipalRow {
    name: String,
    /// `sys.database_principals.type`: S, U, G, E, X, R.
    kind: String,
    disabled: bool,
    superuser: bool,
    system: bool,
    default_schema: Option<String>,
    authentication: Option<String>,
    login: Option<String>,
    created: Option<String>,
}

/// `logins`: the engine has server logins that can be disabled (not
/// Fabric, whose users come from Microsoft Entra ID).
fn principal(r: PrincipalRow, logins: bool) -> Principal {
    let role = r.kind.trim() == "R";
    let mut details = Vec::new();
    let kind = match r.kind.trim() {
        "S" => "Usuario SQL",
        "U" => "Usuario de Windows",
        "G" => "Grupo de Windows",
        "E" => "Usuario de Entra ID",
        "X" => "Grupo de Entra ID",
        _ => "Rol de base de datos",
    };
    details.push(("Tipo".into(), kind.into()));
    if let Some(v) = r.default_schema {
        details.push(("Esquema predeterminado".into(), v));
    }
    if let Some(v) = r.authentication.filter(|v| v != "NONE") {
        details.push(("Autenticación".into(), v));
    }
    if let Some(v) = r.login {
        details.push(("Login".into(), v));
    }
    if let Some(v) = r.created {
        details.push(("Creado".into(), v));
    }
    Principal {
        name: r.name,
        kind: if role { PrincipalKind::Role } else { PrincipalKind::User },
        can_login: (!role).then_some(true),
        superuser: Some(r.superuser),
        disabled: (!role && logins).then_some(r.disabled),
        member_of: Vec::new(),
        details,
        system: r.system,
    }
}

pub async fn principals(s: &mut SqlServerSession) -> Result<Vec<Principal>> {
    let fabric = s.variant == Variant::Fabric;
    let rows = s.rows(if fabric { FABRIC_PRINCIPALS } else { PRINCIPALS }, &[]).await?;
    let members = s.rows(MEMBERSHIPS, &[]).await?;
    let mut out: Vec<Principal> = rows
        .iter()
        .map(|r| {
            let row = PrincipalRow {
                name: text(r, 0).unwrap_or_default(),
                kind: text(r, 1).unwrap_or_default().trim().to_string(),
                disabled: flag(r, 2),
                superuser: flag(r, 3),
                system: flag(r, 4),
                default_schema: text(r, 5),
                authentication: text(r, 6),
                login: text(r, 7),
                created: text(r, 8),
            };
            principal(row, !fabric)
        })
        .collect();
    for m in &members {
        if let (Some(member), Some(role)) = (text(m, 0), text(m, 1)) {
            if let Some(p) = out.iter_mut().find(|p| p.name == member) {
                p.member_of.push(role);
            }
        }
    }
    Ok(out)
}

/// A permission row. `state`: G grant, W grant with grant option, D deny,
/// R revoke.
fn grant(privilege: String, state: &str, object: Option<String>, object_kind: Option<String>, via: Option<String>) -> Grant {
    Grant { privilege, object, object_kind, grantable: state == "W", denied: state == "D", via }
}

/// The principal itself (no `via`) and every role it holds, directly or
/// through other roles, each with the role it holds it through (the
/// first one on the way). `memberships`: (member, role).
fn held_roles(principal: &str, memberships: &[(String, String)]) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = vec![(principal.to_string(), None)];
    let mut i = 0;
    while i < out.len() {
        let (name, via) = out[i].clone();
        for (member, role) in memberships {
            if *member == name && !out.iter().any(|(n, _)| n == role) {
                out.push((role.clone(), Some(via.clone().unwrap_or_else(|| role.clone()))));
            }
        }
        i += 1;
    }
    out
}

/// Fabric's rows (grantee, privilege, state, object, kind) as the grants of
/// `principal`: its own first, then the ones it holds through its roles.
fn fabric_grants(principal: &str, memberships: &[(String, String)], rows: Vec<[Option<String>; 5]>) -> Vec<Grant> {
    let held = held_roles(principal, memberships);
    let mut out = Vec::new();
    for (name, via) in &held {
        for [grantee, privilege, state, object, kind] in &rows {
            if grantee.as_deref() == Some(name.as_str()) {
                let state = state.as_deref().unwrap_or_default();
                out.push(grant(privilege.clone().unwrap_or_default(), state, object.clone(), kind.clone(), via.clone()));
            }
        }
    }
    out
}

pub async fn grants(s: &mut SqlServerSession, principal: &str) -> Result<Vec<Grant>> {
    match s.variant {
        Variant::Fabric => {
            let members = s.rows(MEMBERSHIPS, &[]).await?;
            let members: Vec<(String, String)> = members.iter().filter_map(|m| Some((text(m, 0)?, text(m, 1)?))).collect();
            let rows = s.rows(FABRIC_GRANTS, &[]).await?;
            let rows = rows.iter().map(|r| [text(r, 0), text(r, 1), text(r, 2), text(r, 3), text(r, 4)]).collect();
            Ok(fabric_grants(principal, &members, rows))
        }
        v => {
            let rows = s.rows(if v == Variant::Babelfish { BABELFISH_GRANTS } else { GRANTS }, &[principal]).await?;
            Ok(unique(
                rows.iter()
                    .map(|r| grant(text(r, 0).unwrap_or_default(), text(r, 1).unwrap_or_default().trim(), text(r, 2), text(r, 3), text(r, 4)))
                    .collect(),
            ))
        }
    }
}

/// The same permission reached twice through one role (it holds two roles
/// that have it, or one role by two paths) shows once.
fn unique(rows: Vec<Grant>) -> Vec<Grant> {
    let mut out: Vec<Grant> = Vec::with_capacity(rows.len());
    for g in rows {
        if !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

// -- scripts ---------------------------------------------------------------

fn q(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}
fn lit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

/// What a privilege applies to: nothing (the database), a schema or an object.
fn on(object: &Option<ObjectRef>) -> String {
    match object {
        None => String::new(),
        Some(o) if o.kind == "schema" => format!(" ON SCHEMA::{}", q(&o.name)),
        Some(o) => match o.schema() {
            Some(sc) => format!(" ON {}.{}", q(sc), q(&o.name)),
            None => format!(" ON {}", q(&o.name)),
        },
    }
}

/// Words that end a permission list. T-SQL needs no `;` between
/// statements, so "SELECT TO x GRANT CONTROL" would close the GRANT and
/// start another one: no permission name has them.
const CLAUSE_WORDS: &[&str] = &["TO", "ON", "FROM", "WITH", "AS", "CASCADE", "GRANT", "DENY", "REVOKE", "GO"];

/// Privilege names come from `spec()` or the user: letters and spaces only,
/// and none of T-SQL's clause words.
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out = Vec::new();
    for x in p {
        let up = x.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase();
        if up.is_empty() || !up.chars().all(|c| c.is_ascii_alphabetic() || c == ' ') || up.split(' ').any(|w| CLAUSE_WORDS.contains(&w)) {
            return Err(Error::Query(format!("«{x}» no es un permiso de SQL Server")));
        }
        out.push(up);
    }
    Ok(out.join(", "))
}

/// Fabric: users are Microsoft Entra ID identities, with no login or
/// password in the warehouse; `CREATE USER` isn't available and a GRANT
/// creates the user (docs: "SQL granular permissions in Fabric Data
/// Warehouse"). Signing in takes a workspace role or the item's Read
/// permission, which live in Fabric, not in T-SQL.
fn fabric_script(a: &SecurityAction) -> Option<Result<String>> {
    const NO_PASSWORDS: &str = "Fabric no usa contraseñas: los usuarios son de Microsoft Entra ID";
    Some(match a {
        SecurityAction::CreateUser { name, password } => {
            if password.as_deref().is_some_and(|p| !p.is_empty()) {
                return Some(Err(Error::Unsupported(NO_PASSWORDS.into())));
            }
            Ok(format!(
                "-- Fabric crea el usuario de Entra ID (usuario@dominio) al otorgarle un permiso.\n\
                 -- Para entrar necesita además un rol del área de trabajo o el permiso Read del almacén.\n\
                 GRANT CONNECT TO {};",
                q(name)
            ))
        }
        SecurityAction::SetPassword { .. } => Err(Error::Unsupported(NO_PASSWORDS.into())),
        SecurityAction::SetLogin { .. } => Err(Error::Unsupported(
            "en Fabric el ingreso se da o se quita con los roles del área de trabajo o el permiso Read del almacén".into(),
        )),
        SecurityAction::Drop { kind: PrincipalKind::User, .. } => Err(Error::Unsupported(
            "Fabric no borra usuarios con T-SQL: el acceso se quita en los roles del área de trabajo o los permisos del almacén; acá se le pueden revocar los permisos".into(),
        )),
        _ => return None,
    })
}

/// Every identifier an action writes.
fn names(a: &SecurityAction) -> Vec<&str> {
    fn object(o: &Option<ObjectRef>) -> Vec<&str> {
        o.as_ref().map(|o| vec![o.name.as_str(), o.schema().unwrap_or_default()]).unwrap_or_default()
    }
    match a {
        SecurityAction::CreateUser { name, .. }
        | SecurityAction::CreateRole { name }
        | SecurityAction::Drop { name, .. }
        | SecurityAction::SetPassword { name, .. }
        | SecurityAction::SetLogin { name, .. } => vec![name],
        SecurityAction::Grant { object: o, to: p, .. } | SecurityAction::Revoke { object: o, from: p, .. } => {
            let mut v = object(o);
            v.push(p);
            v
        }
        SecurityAction::AddMember { role, member } | SecurityAction::RemoveMember { role, member } => vec![role, member],
    }
}

pub fn script(v: Variant, a: &SecurityAction) -> Result<String> {
    if v == Variant::Fabric {
        if let Some(s) = fabric_script(a) {
            return s;
        }
    }
    if v == Variant::Babelfish {
        // Babelfish 5.4 doesn't read `]]` inside brackets ("syntax error at
        // or near ]") nor double-quoted principal names: such a name can't
        // be written at all.
        if names(a).iter().any(|n| n.contains(']')) {
            return Err(Error::Query("Babelfish no acepta «]» en los nombres de usuarios, roles ni objetos".into()));
        }
        match a {
            // "GRANT on SCHEMA .. WITH GRANT OPTION is not yet supported in Babelfish".
            SecurityAction::Grant { object: Some(o), grantable: true, .. } if o.kind == "schema" => {
                return Err(Error::Unsupported(
                    "Babelfish no permite otorgar sobre un esquema con opción de otorgar: hacelo sin ella o sobre cada objeto".into(),
                ));
            }
            SecurityAction::Grant { object: None, .. } | SecurityAction::Revoke { object: None, .. } => {
                return Err(Error::Unsupported("Babelfish no admite permisos sobre la base entera: elegí un esquema o un objeto".into()));
            }
            // "REVOKE on SCHEMA .. CASCADE is not yet supported in Babelfish".
            SecurityAction::Revoke { privileges: p, object: Some(o), from } if o.kind == "schema" => {
                return Ok(format!("REVOKE {}{} FROM {};", privileges(p)?, on(&Some(o.clone())), q(from)));
            }
            _ => {}
        }
    }
    let contained = v == Variant::AzureSql;
    Ok(match a {
        SecurityAction::CreateUser { name, password } => {
            let pw = password.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))?;
            if contained {
                format!("CREATE USER {} WITH PASSWORD = {};", q(name), lit(pw))
            } else {
                format!(
                    "-- El login es del servidor; el usuario, de esta base.\nCREATE LOGIN {n} WITH PASSWORD = {p};\nCREATE USER {n} FOR LOGIN {n};",
                    n = q(name),
                    p = lit(pw)
                )
            }
        }
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!("DROP ROLE {};", q(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::User } => {
            if contained {
                format!("DROP USER {};", q(name))
            } else {
                format!("DROP USER {n};\n-- Si el login no se usa en otras bases:\n-- DROP LOGIN {c};", n = q(name), c = crate::comment_text(&q(name)))
            }
        }
        SecurityAction::SetPassword { name, password } => {
            if contained {
                format!("ALTER USER {} WITH PASSWORD = {};", q(name), lit(password))
            } else {
                format!("ALTER LOGIN {} WITH PASSWORD = {};", q(name), lit(password))
            }
        }
        SecurityAction::SetLogin { name, enabled } => {
            if contained {
                // A contained user can't be disabled: take away CONNECT.
                format!("{} CONNECT TO {};", if *enabled { "GRANT" } else { "REVOKE" }, q(name))
            } else {
                format!("ALTER LOGIN {} {};", q(name), if *enabled { "ENABLE" } else { "DISABLE" })
            }
        }
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            format!("GRANT {}{} TO {}{};", privileges(p)?, on(object), q(to), if *grantable { " WITH GRANT OPTION" } else { "" })
        }
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {}{} FROM {} CASCADE;", privileges(p)?, on(object), q(from)),
        SecurityAction::AddMember { role, member } => format!("ALTER ROLE {} ADD MEMBER {};", q(role), q(member)),
        SecurityAction::RemoveMember { role, member } => format!("ALTER ROLE {} DROP MEMBER {};", q(role), q(member)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(Variant::SqlServer, &a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "ana]x".into(), password: Some("p'w".into()) }),
            "-- El login es del servidor; el usuario, de esta base.\nCREATE LOGIN [ana]]x] WITH PASSWORD = N'p''w';\nCREATE USER [ana]]x] FOR LOGIN [ana]]x];"
        );
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["select".into(), "UPDATE".into()],
                object: Some(ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "facturas".into() }),
                to: "ana".into(),
                grantable: true,
            }),
            "GRANT SELECT, UPDATE ON [dbo].[facturas] TO [ana] WITH GRANT OPTION;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["EXECUTE".into()], object: Some(ObjectRef { kind: "schema".into(), schema: None, name: "ventas".into() }), from: "r1".into() }),
            "REVOKE EXECUTE ON SCHEMA::[ventas] FROM [r1] CASCADE;"
        );
        assert!(script(Variant::SqlServer, &SecurityAction::Grant { privileges: vec!["SELECT; DROP TABLE x".into()], object: None, to: "a".into(), grantable: false }).is_err());
        assert_eq!(script(Variant::AzureSql, &SecurityAction::CreateUser { name: "ana".into(), password: Some("x".into()) }).unwrap(), "CREATE USER [ana] WITH PASSWORD = N'x';");
    }

    fn table() -> Option<ObjectRef> {
        Some(ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "facturas".into() })
    }

    #[test]
    fn fabric_scripts_have_no_logins() {
        let s = |a| script(Variant::Fabric, &a);
        let create = s(SecurityAction::CreateUser { name: "ana@contoso.com]".into(), password: None }).unwrap();
        assert!(create.ends_with("\nGRANT CONNECT TO [ana@contoso.com]]];"), "{create}");
        assert!(create.lines().filter(|l| !l.starts_with("--")).count() == 1);
        assert!(matches!(s(SecurityAction::CreateUser { name: "a".into(), password: Some("x".into()) }), Err(Error::Unsupported(_))));
        assert!(matches!(s(SecurityAction::SetPassword { name: "a".into(), password: "x".into() }), Err(Error::Unsupported(_))));
        assert!(matches!(s(SecurityAction::SetLogin { name: "a".into(), enabled: false }), Err(Error::Unsupported(_))));
        assert!(matches!(s(SecurityAction::Drop { name: "a".into(), kind: PrincipalKind::User }), Err(Error::Unsupported(_))));
        assert_eq!(s(SecurityAction::Drop { name: "r'1".into(), kind: PrincipalKind::Role }).unwrap(), "DROP ROLE [r'1];");
        assert_eq!(s(SecurityAction::CreateRole { name: "lectores".into() }).unwrap(), "CREATE ROLE [lectores];");
        assert_eq!(
            s(SecurityAction::AddMember { role: "lectores".into(), member: "ana@contoso.com".into() }).unwrap(),
            "ALTER ROLE [lectores] ADD MEMBER [ana@contoso.com];"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into()], object: table(), to: "Ventas (grupo)".into(), grantable: false }).unwrap(),
            "GRANT SELECT ON [dbo].[facturas] TO [Ventas (grupo)];"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["UNMASK".into()], object: None, to: "a".into(), grantable: false }).unwrap(),
            "GRANT UNMASK TO [a];"
        );
        assert!(s(SecurityAction::Revoke { privileges: vec!["SELECT ON x TO y --".into()], object: None, from: "a".into() }).is_err());
        let spec = spec(Variant::Fabric);
        assert!(!spec.passwords && spec.create_user && spec.create_role && spec.membership && spec.object_kinds.contains(&""));
    }

    #[test]
    fn babelfish_scripts_follow_what_it_implements() {
        let s = |a| script(Variant::Babelfish, &a);
        // Logins as in SQL Server: CREATE LOGIN, ALTER LOGIN … DISABLE.
        assert!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p".into()) }).unwrap().contains("CREATE LOGIN [ana] WITH PASSWORD = N'p';\nCREATE USER [ana] FOR LOGIN [ana];"));
        assert_eq!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }).unwrap(), "ALTER LOGIN [ana] DISABLE;");
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["SELECT".into(), "INSERT".into()], object: Some(ObjectRef { kind: "schema".into(), schema: None, name: "v'e.ntas".into() }), to: "r".into(), grantable: false })
                .unwrap(),
            "GRANT SELECT, INSERT ON SCHEMA::[v'e.ntas] TO [r];"
        );
        // No CASCADE on a schema's REVOKE; objects keep it.
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["EXECUTE".into()], object: Some(ObjectRef { kind: "schema".into(), schema: None, name: "dbo".into() }), from: "r".into() }).unwrap(),
            "REVOKE EXECUTE ON SCHEMA::[dbo] FROM [r];"
        );
        assert_eq!(s(SecurityAction::Revoke { privileges: vec!["UPDATE".into()], object: table(), from: "ana".into() }).unwrap(), "REVOKE UPDATE ON [dbo].[facturas] FROM [ana] CASCADE;");
        assert!(matches!(s(SecurityAction::Grant { privileges: vec!["CREATE TABLE".into()], object: None, to: "a".into(), grantable: false }), Err(Error::Unsupported(_))));
        assert!(s(SecurityAction::Grant { privileges: vec!["SELECT'".into()], object: table(), to: "a".into(), grantable: false }).is_err());
        let spec = spec(Variant::Babelfish);
        assert!(!spec.object_kinds.contains(&"") && !spec.privileges.contains(&"CONTROL") && spec.passwords);
    }

    #[test]
    fn privileges_cannot_close_the_statement() {
        // T-SQL runs "GRANT SELECT TO a GRANT CONTROL TO [b];" as two GRANTs.
        for bad in ["SELECT TO a GRANT CONTROL", "select on x", "CONTROL WITH", "EXECUTE AS", "  ", "SELECT\nGO"] {
            for v in Variant::ALL {
                let g = SecurityAction::Grant { privileges: vec![bad.into()], object: table(), to: "b".into(), grantable: false };
                assert!(script(v, &g).is_err(), "{v:?} {bad:?}");
                let r = SecurityAction::Revoke { privileges: vec![bad.into()], object: table(), from: "b".into() };
                assert!(script(v, &r).is_err(), "{v:?} {bad:?}");
            }
        }
        assert_eq!(
            script(Variant::SqlServer, &SecurityAction::Grant { privileges: vec![" view   definition ".into(), "CREATE TABLE".into()], object: None, to: "a".into(), grantable: false })
                .unwrap(),
            "GRANT VIEW DEFINITION, CREATE TABLE TO [a];"
        );
    }

    #[test]
    fn babelfish_rejects_what_it_cannot_parse() {
        let s = |a| script(Variant::Babelfish, &a);
        let ventas = || Some(ObjectRef { kind: "schema".into(), schema: None, name: "ventas".into() });
        // "GRANT on SCHEMA .. WITH GRANT OPTION is not yet supported in Babelfish".
        assert!(matches!(s(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: ventas(), to: "a".into(), grantable: true }), Err(Error::Unsupported(_))));
        assert!(s(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: table(), to: "a".into(), grantable: true }).unwrap().ends_with(" WITH GRANT OPTION;"));
        // `]]` is a syntax error in Babelfish 5.4; SQL Server takes it.
        for a in [
            SecurityAction::CreateRole { name: "a]b".into() },
            SecurityAction::AddMember { role: "r".into(), member: "a]b".into() },
            SecurityAction::Grant { privileges: vec!["SELECT".into()], object: Some(ObjectRef { kind: "table".into(), schema: Some("v]x".into()), name: "t".into() }), to: "a".into(), grantable: false },
        ] {
            assert!(matches!(s(a.clone()), Err(Error::Query(_))), "{a:?}");
            assert!(script(Variant::SqlServer, &a).unwrap().contains("]]"));
        }
        // Quotes, dots and backslashes stay inside the brackets and literals.
        assert_eq!(
            s(SecurityAction::CreateUser { name: "o'k.a".into(), password: Some("p'w\\x".into()) }).unwrap(),
            "-- El login es del servidor; el usuario, de esta base.\nCREATE LOGIN [o'k.a] WITH PASSWORD = N'p''w\\x';\nCREATE USER [o'k.a] FOR LOGIN [o'k.a];"
        );
        assert_eq!(s(SecurityAction::AddMember { role: "e\"f".into(), member: "o'k.a".into() }).unwrap(), "ALTER ROLE [e\"f] ADD MEMBER [o'k.a];");
    }

    #[test]
    fn repeated_grants_show_once() {
        let g = |p: &str, via: Option<&str>| grant(p.into(), "G", Some("dbo.t".into()), Some("table".into()), via.map(str::to_string));
        let rows = vec![g("SELECT", Some("r")), g("INSERT", Some("r")), g("SELECT", Some("r")), g("SELECT", None)];
        assert_eq!(unique(rows), vec![g("SELECT", Some("r")), g("INSERT", Some("r")), g("SELECT", None)]);
    }

    #[test]
    fn reads_principal_rows() {
        let row = || PrincipalRow {
            name: "ana".into(),
            kind: "S ".into(),
            disabled: true,
            login: Some("ana".into()),
            authentication: Some("NONE".into()),
            ..Default::default()
        };
        let p = principal(row(), true);
        assert_eq!((p.kind, p.disabled, p.can_login), (PrincipalKind::User, Some(true), Some(true)));
        assert_eq!(p.details, vec![("Tipo".to_string(), "Usuario SQL".to_string()), ("Login".to_string(), "ana".to_string())]);
        // Fabric: no logins to disable.
        assert_eq!(principal(PrincipalRow { kind: "E".into(), ..row() }, false).disabled, None);
        let r = principal(PrincipalRow { kind: "R".into(), ..row() }, true);
        assert_eq!((r.kind, r.disabled, r.can_login), (PrincipalKind::Role, None, None));
    }

    #[test]
    fn follows_roles_for_fabric_grants() {
        let m = |a: &str, b: &str| (a.to_string(), b.to_string());
        // ana ∈ lectores ∈ base; ana ∈ db_datareader; a cycle doesn't loop.
        let members = vec![m("ana", "lectores"), m("lectores", "base"), m("ana", "db_datareader"), m("base", "lectores")];
        let held = held_roles("ana", &members);
        let via = |n: &str| held.iter().find(|(x, _)| x == n).map(|(_, v)| v.clone());
        assert_eq!(held.len(), 4);
        assert_eq!(via("ana"), Some(None));
        assert_eq!(via("base"), Some(Some("lectores".into())));
        assert_eq!(via("db_datareader"), Some(Some("db_datareader".into())));

        let o = |s: &str| Some(s.to_string());
        let rows = vec![
            [o("base"), o("SELECT"), o("G"), o("dbo.facturas"), o("table")],
            [o("ana"), o("UPDATE"), o("W"), o("dbo.facturas"), o("table")],
            [o("ana"), o("DELETE"), o("D"), o("dbo.facturas"), o("table")],
            [o("otro"), o("CONNECT"), o("G"), None, o("database")],
        ];
        let g = fabric_grants("ana", &members, rows);
        assert_eq!(g.len(), 3);
        assert_eq!((g[0].privilege.as_str(), g[0].grantable, g[0].via.as_deref()), ("UPDATE", true, None));
        assert!(g[1].denied && g[1].privilege == "DELETE");
        assert_eq!((g[2].privilege.as_str(), g[2].via.as_deref()), ("SELECT", Some("lectores")));
    }

    #[test]
    fn lists_schemas_with_the_system_ones() {
        for v in Variant::ALL {
            let sql = list_schemas_sql(v);
            assert!(sql.contains("'sys', 'INFORMATION_SCHEMA', 'guest'") && sql.contains("p.is_fixed_role = 1"), "{v:?}");
            assert_eq!(sql.contains("'queryinsights'"), v == Variant::Fabric, "{v:?}");
        }
    }

    #[test]
    fn writes_schema_scripts() {
        assert_eq!(create_schema(Variant::SqlServer, "ven]tas", Some("ana")).unwrap(), "CREATE SCHEMA [ven]]tas] AUTHORIZATION [ana];");
        assert_eq!(create_schema(Variant::AzureSql, "ventas", Some("  ")).unwrap(), "CREATE SCHEMA [ventas];");
        assert_eq!(create_schema(Variant::Fabric, "ventas", None).unwrap(), "CREATE SCHEMA [ventas];");
        assert!(matches!(create_schema(Variant::Fabric, "ventas", Some("ana")), Err(Error::Unsupported(_))));
        assert_eq!(create_schema(Variant::Babelfish, "v'e.ntas", Some("dbo")).unwrap(), "CREATE SCHEMA [v'e.ntas] AUTHORIZATION [dbo];");
        assert!(matches!(create_schema(Variant::Babelfish, "a]b", None), Err(Error::Query(_))));
        for v in Variant::ALL {
            assert_eq!(drop_schema(v, "ventas", false).unwrap(), "DROP SCHEMA [ventas];");
            assert!(matches!(drop_schema(v, "ventas", true), Err(Error::Unsupported(_))));
            let spec = schema_spec(v);
            assert!(!spec.cascade && !spec.privileges.is_empty());
            // Every privilege offered goes through the GRANT on the schema.
            let g = SecurityAction::Grant {
                privileges: spec.privileges.iter().map(|p| p.to_string()).collect(),
                object: Some(ObjectRef { kind: "schema".into(), schema: None, name: "ventas".into() }),
                to: "ana".into(),
                grantable: false,
            };
            assert!(script(v, &g).unwrap().starts_with(&format!("GRANT {} ON SCHEMA::[ventas] TO [ana]", spec.privileges.join(", "))), "{v:?}");
        }
        assert_eq!(
            script(
                Variant::SqlServer,
                &SecurityAction::Grant {
                    privileges: vec!["SELECT".into(), "TAKE OWNERSHIP".into()],
                    object: Some(ObjectRef { kind: "schema".into(), schema: None, name: "ventas".into() }),
                    to: "lectores".into(),
                    grantable: true
                }
            )
            .unwrap(),
            "GRANT SELECT, TAKE OWNERSHIP ON SCHEMA::[ventas] TO [lectores] WITH GRANT OPTION;"
        );
    }
}
