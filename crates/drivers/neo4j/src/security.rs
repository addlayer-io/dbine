//! Users, roles and permissions (docs/usuarios-y-permisos.md).
//!
//! - **Neo4j:** users and roles are server-wide, in the `system` database.
//!   Privileges go to roles only (a user holds them through its roles); they
//!   apply to a graph (`ON GRAPH`), a database (`ON DATABASE`) or the whole
//!   server (`ON DBMS`), and graph privileges can be narrowed to a label or
//!   a relationship type. Community has users only: no roles, privileges or
//!   suspension.
//! - **Memgraph:** users with passwords in every edition; roles and
//!   privileges (global, or per label / edge type) need Enterprise. A user
//!   can hold privileges directly.
//! - **Neptune:** access goes through AWS IAM, not the database.

use crate::{as_text, cypher, strs, Flavor, GraphSession};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use serde_json::{Map, Value};

pub const NEPTUNE: &str = "Neptune controla el acceso con IAM de AWS: los usuarios y permisos se administran en IAM, no en la base";

pub fn spec(f: Flavor) -> Option<SecuritySpec> {
    match f {
        Flavor::Neo4j => Some(SecuritySpec {
            privileges: vec![
                "ACCESS", "TRAVERSE", "READ {*}", "MATCH {*}", "WRITE", "CREATE", "DELETE", "SET PROPERTY {*}", "MERGE {*}",
                "SET LABEL *", "REMOVE LABEL *", "ALL GRAPH PRIVILEGES", "INDEX MANAGEMENT", "CONSTRAINT MANAGEMENT",
                "NAME MANAGEMENT", "TRANSACTION MANAGEMENT", "START", "STOP", "ALL DATABASE PRIVILEGES", "USER MANAGEMENT",
                "ROLE MANAGEMENT", "PRIVILEGE MANAGEMENT", "DATABASE MANAGEMENT", "ALL DBMS PRIVILEGES",
            ],
            object_kinds: vec!["", crate::LABEL, crate::RELATIONSHIP],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        }),
        Flavor::Memgraph => Some(SecuritySpec {
            privileges: vec![
                "MATCH", "CREATE", "MERGE", "SET", "DELETE", "REMOVE", "INDEX", "CONSTRAINT", "STATS", "AUTH", "DUMP",
                "TRIGGER", "STREAM", "CONFIG", "DURABILITY", "REPLICATION", "READ_FILE", "FREE_MEMORY", "MODULE_READ",
                "MODULE_WRITE", "WEBSOCKET", "TRANSACTION_MANAGEMENT", "STORAGE_MODE", "MULTI_DATABASE_EDIT",
                "MULTI_DATABASE_USE", "IMPERSONATE_USER", "ALL PRIVILEGES", "READ", "UPDATE",
            ],
            object_kinds: vec!["", crate::LABEL, crate::RELATIONSHIP],
            create_user: true,
            create_role: true,
            passwords: true,
            membership: true,
            per_database: false,
        }),
        Flavor::Neptune => None,
    }
}

// -- reading -----------------------------------------------------------------

async fn records(s: &mut GraphSession, q: &str, db: Option<&str>) -> Result<Vec<Map<String, Value>>> {
    let (cols, rows) = s.query_on(q, db).await?;
    Ok(rows.into_iter().map(|r| cols.iter().cloned().zip(r).collect()).collect())
}

fn text(r: &Map<String, Value>, k: &str) -> Option<String> {
    r.get(k).map(as_text).filter(|s| !s.is_empty())
}

fn yes(b: bool) -> String {
    if b { "Sí" } else { "No" }.into()
}

/// A Community-edition refusal (Neo4j roles, privileges, `SET STATUS`;
/// Memgraph roles and privileges), explained.
pub fn edition_hint(e: Error) -> Error {
    match e {
        Error::Query(m) if m.contains("Unsupported administration command") || m.contains("not available in community") => {
            Error::Query(format!("Esto requiere Neo4j Enterprise (roles, permisos, usuarios suspendidos, varias bases…). ({m})"))
        }
        Error::Query(m) if m.contains("advanced authentication features") => {
            Error::Query(format!("Los roles y permisos de Memgraph requieren la edición Enterprise. ({m})"))
        }
        e => e,
    }
}

async fn neo4j_community(s: &mut GraphSession) -> Result<bool> {
    let r = records(s, "CALL dbms.components() YIELD edition RETURN edition", None).await?;
    Ok(r.first().and_then(|r| text(r, "edition")).is_some_and(|e| e.eq_ignore_ascii_case("community")))
}

const NEO4J_BUILTIN_ROLES: &[&str] = &["PUBLIC", "admin", "architect", "editor", "publisher", "reader"];

pub async fn principals(s: &mut GraphSession) -> Result<Vec<Principal>> {
    match s.flavor {
        Flavor::Neo4j => neo4j_principals(s).await,
        Flavor::Memgraph => memgraph_principals(s).await,
        Flavor::Neptune => Err(Error::Unsupported(NEPTUNE.into())),
    }
}

pub async fn grants(s: &mut GraphSession, principal: &str) -> Result<Vec<Grant>> {
    match s.flavor {
        Flavor::Neo4j => neo4j_grants(s, principal).await,
        Flavor::Memgraph => memgraph_grants(s, principal).await,
        Flavor::Neptune => Err(Error::Unsupported(NEPTUNE.into())),
    }
}

async fn neo4j_principals(s: &mut GraphSession) -> Result<Vec<Principal>> {
    let community = neo4j_community(s).await?;
    let users = records(s, "SHOW USERS YIELD *", Some("system")).await?;
    let mut out: Vec<Principal> = users
        .iter()
        .map(|r| {
            let name = text(r, "user").unwrap_or_default();
            let roles = r.get("roles").filter(|v| !v.is_null()).map(|v| strs(Some(v)));
            let mut details = vec![("Tipo".to_string(), "Usuario".to_string())];
            if let Some(b) = r.get("passwordChangeRequired").and_then(Value::as_bool) {
                details.push(("Debe cambiar la contraseña".into(), yes(b)));
            }
            if let Some(h) = text(r, "home") {
                details.push(("Base de inicio".into(), h));
            }
            if community {
                details.push(("Edición".into(), "Community: todos los usuarios tienen todos los permisos".into()));
            }
            Principal {
                kind: PrincipalKind::User,
                can_login: Some(true),
                superuser: if community { Some(true) } else { roles.as_ref().map(|r| r.iter().any(|x| x == "admin")) },
                disabled: r.get("suspended").and_then(Value::as_bool),
                member_of: roles.unwrap_or_default().into_iter().filter(|x| x != "PUBLIC").collect(),
                details,
                system: name == "neo4j",
                name,
            }
        })
        .collect();
    if !community {
        for r in records(s, "SHOW ROLES YIELD *", Some("system")).await? {
            let name = text(&r, "role").unwrap_or_default();
            let builtin = NEO4J_BUILTIN_ROLES.contains(&name.as_str());
            let immutable = r.get("immutable").and_then(Value::as_bool).unwrap_or(false);
            let mut details = vec![("Tipo".to_string(), if builtin { "Rol integrado" } else { "Rol" }.to_string())];
            if name == "PUBLIC" {
                details.push(("Miembros".into(), "todos los usuarios".into()));
            }
            if immutable {
                details.push(("Inmutable".into(), yes(true)));
            }
            out.push(Principal {
                kind: PrincipalKind::Role,
                can_login: None,
                superuser: Some(name == "admin"),
                disabled: None,
                member_of: Vec::new(),
                details,
                system: builtin || immutable,
                name,
            });
        }
    }
    Ok(out)
}

async fn neo4j_grants(s: &mut GraphSession, principal: &str) -> Result<Vec<Grant>> {
    if neo4j_community(s).await? {
        return Err(Error::Unsupported(
            "Neo4j Community no tiene roles ni permisos: todo usuario puede hacer todo. Los permisos son de Enterprise.".into(),
        ));
    }
    let name = cypher::string(principal);
    let is_user = !records(s, &format!("SHOW USERS YIELD user WHERE user = {name} RETURN user"), Some("system")).await?.is_empty();
    let q = if is_user {
        format!("SHOW USER {} PRIVILEGES YIELD *", cypher::ident(principal))
    } else {
        format!("SHOW ROLE {} PRIVILEGES YIELD *", cypher::ident(principal))
    };
    let mut out: Vec<Grant> = Vec::new();
    for r in records(s, &q, Some("system")).await? {
        let g = neo4j_grant(&r, is_user);
        if !out.contains(&g) {
            out.push(g);
        }
    }
    Ok(out)
}

/// `NODE(Person)` → ("NODE", "Person").
fn segment(seg: &str) -> Option<(&str, &str)> {
    let (k, rest) = seg.split_once('(')?;
    Some((k, rest.strip_suffix(')')?))
}

/// One row of `SHOW … PRIVILEGES` as a grant, with the privilege written
/// the way `GRANT` takes it (so revoking it round-trips).
fn neo4j_grant(r: &Map<String, Value>, is_user: bool) -> Grant {
    let action = text(r, "action").unwrap_or_default();
    let resource = text(r, "resource").unwrap_or_default();
    let graph = text(r, "graph").unwrap_or_else(|| "*".into());
    let seg = text(r, "segment").unwrap_or_default();
    let inner = |s: &str| s.split_once('(').and_then(|(_, x)| x.strip_suffix(')')).map(str::to_string);
    let props = || match resource.as_str() {
        "all_properties" => "{*}".to_string(),
        r => format!("{{{}}}", inner(r).map(|p| cypher::ident(&p)).unwrap_or_else(|| "*".into())),
    };
    let privilege = match action.as_str() {
        "read" | "match" | "merge" => format!("{} {}", action.to_uppercase(), props()),
        "set_property" => format!("SET PROPERTY {}", props()),
        "set_label" | "remove_label" => {
            let l = if resource == "all_labels" { "*".into() } else { inner(&resource).map(|l| cypher::ident(&l)).unwrap_or_else(|| "*".into()) };
            format!("{} LABEL {l}", if action == "set_label" { "SET" } else { "REMOVE" })
        }
        "create_element" => "CREATE".into(),
        "delete_element" => "DELETE".into(),
        "graph_actions" => "ALL GRAPH PRIVILEGES".into(),
        "database_actions" => "ALL DATABASE PRIVILEGES".into(),
        "dbms_actions" => "ALL DBMS PRIVILEGES".into(),
        "start_database" => "START".into(),
        "stop_database" => "STOP".into(),
        "index" => "INDEX MANAGEMENT".into(),
        "constraint" => "CONSTRAINT MANAGEMENT".into(),
        "token" => "NAME MANAGEMENT".into(),
        "create_label" => "CREATE NEW NODE LABEL".into(),
        "create_reltype" => "CREATE NEW RELATIONSHIP TYPE".into(),
        "create_propertykey" => "CREATE NEW PROPERTY NAME".into(),
        "transaction_management" | "show_transaction" | "terminate_transaction" | "impersonate" => {
            let base = action.replace('_', " ").to_uppercase();
            match segment(&seg) {
                Some((_, u)) if u != "*" => format!("{base} ({})", u.split(',').map(|x| cypher::ident(x.trim())).collect::<Vec<_>>().join(", ")),
                _ => base,
            }
        }
        "execute" | "execute_boosted" => {
            let boosted = if action == "execute_boosted" { "BOOSTED " } else { "" };
            match segment(&seg) {
                Some(("FUNCTION", p)) => format!("EXECUTE {boosted}FUNCTION {p}"),
                Some((_, p)) => format!("EXECUTE {boosted}PROCEDURE {p}"),
                None => format!("EXECUTE {boosted}PROCEDURE *"),
            }
        }
        "execute_admin" => "EXECUTE ADMIN PROCEDURES".into(),
        other => other.replace('_', " ").to_uppercase(),
    };
    let named = |g: &str| (g != "*").then(|| g.to_string());
    let (object, object_kind) = if action == "load" {
        match segment(&seg) {
            Some(("URL", u)) => (Some(u.to_string()), Some("url".to_string())),
            Some(("CIDR", c)) => (Some(c.to_string()), Some("cidr".to_string())),
            _ => (None, None),
        }
    } else {
        match segment(&seg) {
            Some((k @ ("NODE" | "RELATIONSHIP"), l)) if l != "*" => {
                let kind = if k == "NODE" { crate::LABEL } else { crate::RELATIONSHIP };
                (Some(named(&graph).map_or(l.to_string(), |g| format!("{g}.{l}"))), Some(kind.to_string()))
            }
            _ if scope(&privilege) == Scope::Dbms => (None, Some("dbms".to_string())),
            _ => {
                let o = named(&graph);
                let k = o.as_ref().map(|_| "graph".to_string());
                (o, k)
            }
        }
    };
    Grant {
        privilege,
        object,
        object_kind,
        grantable: false,
        denied: text(r, "access").as_deref() == Some("DENIED"),
        via: if is_user { text(r, "role") } else { None },
    }
}

async fn memgraph_principals(s: &mut GraphSession) -> Result<Vec<Principal>> {
    let users = records(s, "SHOW USERS", None).await?;
    // Community: no roles (the command asks for a license).
    let roles = records(s, "SHOW ROLES", None).await.unwrap_or_default();
    let mut out = Vec::new();
    for r in &users {
        let Some(name) = r.values().next().map(as_text) else { continue };
        let member_of = if roles.is_empty() {
            Vec::new()
        } else {
            let q = format!("SHOW ROLES FOR {}", cypher::ident(&name));
            match records(s, &q, None).await {
                Ok(v) => v.iter().filter_map(|r| r.values().next().map(as_text)).filter(|x| !x.is_empty() && x != "null").collect(),
                Err(_) => Vec::new(),
            }
        };
        out.push(Principal {
            name,
            kind: PrincipalKind::User,
            can_login: Some(true),
            superuser: None,
            disabled: None,
            member_of,
            details: vec![("Tipo".into(), "Usuario".into())],
            system: false,
        });
    }
    for r in &roles {
        let Some(name) = r.values().next().map(as_text) else { continue };
        out.push(Principal { name, kind: PrincipalKind::Role, details: vec![("Tipo".into(), "Rol".into())], ..Default::default() });
    }
    Ok(out)
}

async fn memgraph_grants(s: &mut GraphSession, principal: &str) -> Result<Vec<Grant>> {
    let q = format!("SHOW PRIVILEGES FOR {}", cypher::ident(principal));
    let rows = match records(s, &q, None).await {
        Ok(r) => r,
        Err(Error::Query(m)) if m.contains("requires an enterprise") => {
            return Err(Error::Unsupported(
                "Los permisos de Memgraph son de la edición Enterprise; en Community todo usuario puede hacer todo.".into(),
            ))
        }
        Err(e) => return Err(e),
    };
    // Roles the privileges can come through.
    let roles = records(s, &format!("SHOW ROLES FOR {}", cypher::ident(principal)), None)
        .await
        .map(|v| v.iter().filter_map(|r| r.values().next().map(as_text)).filter(|x| !x.is_empty()).collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(rows
        .iter()
        .map(|r| {
            let privilege = text(r, "privilege").unwrap_or_default();
            let effective = text(r, "effective").unwrap_or_default();
            let description = text(r, "description").unwrap_or_default().to_uppercase();
            let direct = description.contains("TO USER") || !description.contains("ROLE");
            // Fine-grained rows: `LABEL :Person` / `EDGE_TYPE :KNOWS`, their level in `effective`.
            let (privilege, object, object_kind, denied) = match privilege.split_once(' ') {
                Some((k @ ("LABEL" | "EDGE_TYPE"), l)) => {
                    let kind = if k == "LABEL" { crate::LABEL } else { crate::RELATIONSHIP };
                    (effective.clone(), Some(l.trim_start_matches(':').to_string()), Some(kind.to_string()), effective == "NOTHING")
                }
                _ => (privilege, None, None, effective == "DENY"),
            };
            Grant {
                privilege,
                object,
                object_kind,
                grantable: false,
                denied,
                via: if direct { None } else { Some(roles.join(", ")).filter(|v| !v.is_empty()).or(Some("rol".into())) },
            }
        })
        .collect())
}

// -- scripts -----------------------------------------------------------------

pub fn script(f: Flavor, a: &SecurityAction) -> Result<String> {
    match f {
        Flavor::Neo4j => neo4j_script(a),
        Flavor::Memgraph => memgraph_script(a),
        Flavor::Neptune => Err(Error::Unsupported(NEPTUNE.into())),
    }
}

fn name(n: &str) -> Result<String> {
    let n = n.trim();
    if n.is_empty() {
        return Err(Error::Query("escribí un nombre".into()));
    }
    Ok(cypher::ident(n))
}

fn password(p: Option<&str>) -> Result<String> {
    p.filter(|p| !p.is_empty()).map(cypher::string).ok_or_else(|| Error::Query("escribí la contraseña del usuario".into()))
}

fn no_grant_option() -> Error {
    Error::Unsupported("Neo4j y Memgraph no tienen la opción de otorgar a otros: para eso está el permiso de administrar permisos".into())
}

/// Where a Neo4j privilege applies.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Scope {
    /// `ON GRAPH`; `element`: it can be narrowed to `NODES` / `RELATIONSHIPS`.
    Graph { element: bool },
    Database,
    Dbms,
    Load,
}

const DBMS_PREFIXES: &[&str] = &[
    "ALL DBMS", "DBMS", "USER", "ROLE", "PRIVILEGE", "ALIAS", "SERVER", "COMPOSITE", "DATABASE MANAGEMENT", "EXECUTE",
    "IMPERSONATE", "CREATE USER", "CREATE ROLE", "CREATE DATABASE", "CREATE ALIAS", "CREATE COMPOSITE", "DROP USER",
    "DROP ROLE", "DROP DATABASE", "DROP ALIAS", "DROP COMPOSITE", "ALTER", "RENAME", "SHOW USER", "SHOW ROLE",
    "SHOW PRIVILEGE", "SHOW ALIAS", "SHOW SETTING", "SHOW SERVER", "SET PASSWORD", "SET PASSWORDS", "SET USER", "SET AUTH",
    "SET DATABASE", "ASSIGN", "REMOVE ROLE", "REMOVE PRIVILEGE",
];

fn scope(privilege: &str) -> Scope {
    let key = privilege.split(['{', '(']).next().unwrap_or_default().trim();
    let has = |p: &str| key == p || key.starts_with(&format!("{p} "));
    match key {
        "TRAVERSE" | "CREATE" | "DELETE" => Scope::Graph { element: true },
        "WRITE" | "ALL GRAPH PRIVILEGES" => Scope::Graph { element: false },
        _ if ["READ", "MATCH", "MERGE", "SET PROPERTY"].iter().any(|p| has(p)) => Scope::Graph { element: true },
        _ if has("SET LABEL") || has("REMOVE LABEL") => Scope::Graph { element: false },
        _ if has("LOAD") => Scope::Load,
        _ if DBMS_PREFIXES.iter().any(|p| has(p)) => Scope::Dbms,
        _ => Scope::Database,
    }
}

/// Words that name a privilege (upper-cased); others (labels, procedure
/// patterns) keep their case.
const KEYWORDS: &[&str] = &[
    "ACCESS", "TRAVERSE", "READ", "MATCH", "WRITE", "CREATE", "DELETE", "SET", "PROPERTY", "MERGE", "REMOVE", "LABEL",
    "ALL", "GRAPH", "DATABASE", "DBMS", "PRIVILEGES", "PRIVILEGE", "INDEX", "INDEXES", "CONSTRAINT", "CONSTRAINTS", "NAME",
    "MANAGEMENT", "TRANSACTION", "TRANSACTIONS", "START", "STOP", "USER", "USERS", "ROLE", "ROLES", "NEW", "NODE",
    "RELATIONSHIP", "TYPE", "SHOW", "DROP", "ALTER", "RENAME", "TERMINATE", "EXECUTE", "BOOSTED", "ADMIN", "ADMINISTRATOR",
    "PROCEDURE", "PROCEDURES", "FUNCTION", "FUNCTIONS", "DEFINED", "IMPERSONATE", "ALIAS", "SERVER", "COMPOSITE",
    "PASSWORD", "PASSWORDS", "STATUS", "HOME", "ASSIGN", "SETTING", "SETTINGS", "LOAD", "AUTH", "DATABASES",
];

/// A Neo4j privilege as `GRANT` takes it: keywords upper-cased, the
/// property / label part completed (`READ` → `READ {*}`). Only plain names,
/// `*`, `?`, `.`, `,`, braces and parentheses are accepted.
fn neo4j_privilege(p: &str) -> Result<String> {
    let p = p.trim();
    let ok = |c: char| c.is_ascii_alphanumeric() || " _.*?,{}()".contains(c);
    if p.is_empty() || !p.chars().all(ok) {
        return Err(Error::Query(format!("«{p}» no es un permiso de Neo4j")));
    }
    let cut = p.find(['{', '(']).unwrap_or(p.len());
    let (head, tail) = p.split_at(cut);
    let mut words: Vec<String> = Vec::new();
    let mut after_label = false;
    for w in head.split_whitespace() {
        let up = w.to_ascii_uppercase();
        if !after_label && KEYWORDS.contains(&up.as_str()) {
            after_label = up == "LABEL";
            words.push(up);
        } else {
            words.push(w.to_string());
        }
    }
    let head = words.join(" ");
    let tail = tail.trim();
    let needs_props = ["READ", "MATCH", "MERGE", "SET PROPERTY"].contains(&head.as_str());
    Ok(match (needs_props, tail.is_empty()) {
        (true, true) => format!("{head} {{*}}"),
        _ if (head == "SET LABEL" || head == "REMOVE LABEL") && tail.is_empty() => format!("{head} *"),
        (_, true) => head,
        _ => format!("{head} {tail}"),
    })
}

/// A database's name, or `HOME` / `DEFAULT`, after `GRAPH` or `DATABASE`.
fn target(word: &str, db: Option<&str>) -> String {
    match db {
        None | Some("*") => format!("{word} *"),
        Some("HOME") => format!("HOME {word}"),
        Some("DEFAULT") => format!("DEFAULT {word}"),
        Some(d) => format!("{word} {}", cypher::ident(d)),
    }
}

/// The object's full name (a database name may have a dot the UI split).
fn full(o: &ObjectRef) -> String {
    match o.schema() {
        Some(s) => format!("{s}.{}", o.name),
        None => o.name.clone(),
    }
}

fn neo4j_on(privilege: &str, object: &Option<ObjectRef>) -> Result<String> {
    let sc = scope(privilege);
    let kind = object.as_ref().map(|o| o.kind.as_str());
    Ok(match (sc, kind) {
        (Scope::Dbms, None | Some("dbms")) => "ON DBMS".into(),
        (Scope::Dbms, _) => return Err(Error::Query(format!("«{privilege}» es de todo el servidor: elegí «todo» como objeto"))),
        (Scope::Load, None) => "ON ALL DATA".into(),
        (Scope::Load, Some("url")) => format!("ON URL {}", cypher::string(&full(object.as_ref().unwrap()))),
        (Scope::Load, Some("cidr")) => format!("ON CIDR {}", cypher::string(&full(object.as_ref().unwrap()))),
        (Scope::Load, _) => return Err(Error::Query("LOAD se otorga sobre todos los datos, una URL o un rango CIDR".into())),
        (Scope::Database, None) => format!("ON {}", target("DATABASE", None)),
        (Scope::Database, Some("graph" | "database")) => format!("ON {}", target("DATABASE", Some(&full(object.as_ref().unwrap())))),
        (Scope::Database, _) => {
            return Err(Error::Query(format!("«{privilege}» se otorga sobre una base entera, no sobre una etiqueta o un tipo de relación")))
        }
        (Scope::Graph { .. }, None) => format!("ON {}", target("GRAPH", None)),
        (Scope::Graph { .. }, Some("graph" | "database")) => format!("ON {}", target("GRAPH", Some(&full(object.as_ref().unwrap())))),
        (Scope::Graph { element: true }, Some(k @ (crate::LABEL | crate::RELATIONSHIP))) => {
            let o = object.as_ref().unwrap();
            let el = if k == crate::LABEL { "NODES" } else { "RELATIONSHIPS" };
            let what = if o.name == "*" { "*".to_string() } else { cypher::ident(&o.name) };
            format!("ON {} {el} {what}", target("GRAPH", o.schema()))
        }
        (Scope::Graph { element: false }, Some(crate::LABEL | crate::RELATIONSHIP)) => {
            return Err(Error::Query(format!("«{privilege}» se otorga sobre el grafo entero, no sobre una etiqueta o un tipo de relación")))
        }
        (_, Some(k)) => return Err(Error::Query(format!("Neo4j no otorga permisos sobre objetos «{k}»"))),
    })
}

fn neo4j_privileges(p: &[String], object: &Option<ObjectRef>) -> Result<Vec<(String, String)>> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    p.iter()
        .map(|x| {
            let x = neo4j_privilege(x)?;
            let on = neo4j_on(&x, object)?;
            Ok((x, on))
        })
        .collect()
}

fn neo4j_script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name: n, password: p } => {
            format!("CREATE USER {} SET PASSWORD {} CHANGE NOT REQUIRED;", name(n)?, password(p.as_deref())?)
        }
        SecurityAction::CreateRole { name: n } => format!("CREATE ROLE {};", name(n)?),
        SecurityAction::Drop { name: n, kind: PrincipalKind::User } => format!("DROP USER {};", name(n)?),
        SecurityAction::Drop { name: n, kind: PrincipalKind::Role } => format!("DROP ROLE {};", name(n)?),
        SecurityAction::SetPassword { name: n, password: p } => {
            format!("ALTER USER {} SET PASSWORD {} CHANGE NOT REQUIRED;", name(n)?, password(Some(p))?)
        }
        SecurityAction::SetLogin { name: n, enabled } => {
            format!("// Suspender usuarios requiere Neo4j Enterprise.\nALTER USER {} SET STATUS {};", name(n)?, if *enabled { "ACTIVE" } else { "SUSPENDED" })
        }
        SecurityAction::Grant { privileges, object, to, grantable } => {
            if *grantable {
                return Err(no_grant_option());
            }
            let to = name(to)?;
            let lines: Vec<String> = neo4j_privileges(privileges, object)?.into_iter().map(|(p, on)| format!("GRANT {p} {on} TO {to};")).collect();
            format!("// En Neo4j los permisos se otorgan a roles; un usuario los tiene por sus roles.\n{}", lines.join("\n"))
        }
        SecurityAction::Revoke { privileges, object, from } => {
            let from = name(from)?;
            neo4j_privileges(privileges, object)?.into_iter().map(|(p, on)| format!("REVOKE {p} {on} FROM {from};")).collect::<Vec<_>>().join("\n")
        }
        SecurityAction::AddMember { role, member } => format!("GRANT ROLE {} TO {};", name(role)?, name(member)?),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE ROLE {} FROM {};", name(role)?, name(member)?),
    })
}

/// Memgraph's fine-grained levels (on labels and edge types).
const MEMGRAPH_FINE: &[&str] = &["READ", "UPDATE", "CREATE", "DELETE"];

fn memgraph_privileges(p: &[String], fine: bool) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let mut out = Vec::new();
    for x in p {
        let up = x.trim().to_ascii_uppercase();
        if up.is_empty() || !up.chars().all(|c| c.is_ascii_alphabetic() || c == '_' || c == ' ') {
            return Err(Error::Query(format!("«{x}» no es un permiso de Memgraph")));
        }
        if fine && !MEMGRAPH_FINE.contains(&up.as_str()) {
            return Err(Error::Query(format!("sobre etiquetas y tipos de relación Memgraph otorga {}; «{x}» es global", MEMGRAPH_FINE.join(", "))));
        }
        if up == "ALL PRIVILEGES" && p.len() > 1 {
            return Err(Error::Query("ALL PRIVILEGES va solo".into()));
        }
        out.push(up);
    }
    Ok(out.join(", "))
}

/// ` ON NODES CONTAINING LABELS :L` / ` ON EDGES OF TYPE :T` (or nothing).
fn memgraph_on(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else { return Ok(String::new()) };
    let what = if o.name == "*" { "*".to_string() } else { format!(":{}", cypher::ident(&full(o))) };
    match o.kind.as_str() {
        crate::LABEL => Ok(format!(" ON NODES CONTAINING LABELS {what}")),
        crate::RELATIONSHIP => Ok(format!(" ON EDGES OF TYPE {what}")),
        k => Err(Error::Query(format!("Memgraph no otorga permisos sobre objetos «{k}»"))),
    }
}

fn memgraph_script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { name: n, password: p } => match p.as_deref().filter(|p| !p.is_empty()) {
            Some(p) => format!("CREATE USER {} IDENTIFIED BY {};", name(n)?, cypher::string(p)),
            None => format!("CREATE USER {};", name(n)?),
        },
        SecurityAction::CreateRole { name: n } => format!("CREATE ROLE {};", name(n)?),
        SecurityAction::Drop { name: n, kind: PrincipalKind::User } => format!("DROP USER {};", name(n)?),
        SecurityAction::Drop { name: n, kind: PrincipalKind::Role } => format!("DROP ROLE {};", name(n)?),
        SecurityAction::SetPassword { name: n, password: p } => format!("SET PASSWORD FOR {} TO {};", name(n)?, password(Some(p))?),
        SecurityAction::SetLogin { .. } => {
            return Err(Error::Unsupported("Memgraph no deshabilita usuarios: cambiale la contraseña o borralo".into()))
        }
        SecurityAction::Grant { privileges, object, to, grantable } => {
            if *grantable {
                return Err(no_grant_option());
            }
            format!("GRANT {}{} TO {};", memgraph_privileges(privileges, object.is_some())?, memgraph_on(object)?, name(to)?)
        }
        SecurityAction::Revoke { privileges, object, from } => {
            format!("REVOKE {}{} FROM {};", memgraph_privileges(privileges, object.is_some())?, memgraph_on(object)?, name(from)?)
        }
        SecurityAction::AddMember { role, member } => format!("GRANT ROLE {} TO {};", name(role)?, name(member)?),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE ROLE {} FROM {};", name(role)?, name(member)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }
    fn grant(p: &[&str], object: Option<ObjectRef>, to: &str) -> SecurityAction {
        SecurityAction::Grant { privileges: p.iter().map(|x| x.to_string()).collect(), object, to: to.into(), grantable: false }
    }

    #[test]
    fn neo4j_scripts() {
        let s = |a| script(Flavor::Neo4j, &a).unwrap();
        assert_eq!(
            s(SecurityAction::CreateUser { name: "ana x".into(), password: Some("p'w".into()) }),
            "CREATE USER `ana x` SET PASSWORD 'p\\'w' CHANGE NOT REQUIRED;"
        );
        assert_eq!(s(SecurityAction::CreateRole { name: "a`b".into() }), "CREATE ROLE `a``b`;");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "x".into() }), "ALTER USER ana SET PASSWORD 'x' CHANGE NOT REQUIRED;");
        assert!(s(SecurityAction::SetLogin { name: "ana".into(), enabled: false }).ends_with("ALTER USER ana SET STATUS SUSPENDED;"));
        assert_eq!(s(SecurityAction::AddMember { role: "lect".into(), member: "ana".into() }), "GRANT ROLE lect TO ana;");
        assert_eq!(s(SecurityAction::RemoveMember { role: "lect".into(), member: "ana".into() }), "REVOKE ROLE lect FROM ana;");
        assert_eq!(s(SecurityAction::Drop { name: "lect".into(), kind: PrincipalKind::Role }), "DROP ROLE lect;");
        let g = s(grant(&["read", "access", "user management", "set label Foo", "match {name, age}"], None, "lect"));
        assert_eq!(
            g.lines().skip(1).collect::<Vec<_>>(),
            vec![
                "GRANT READ {*} ON GRAPH * TO lect;",
                "GRANT ACCESS ON DATABASE * TO lect;",
                "GRANT USER MANAGEMENT ON DBMS TO lect;",
                "GRANT SET LABEL Foo ON GRAPH * TO lect;",
                "GRANT MATCH {name, age} ON GRAPH * TO lect;",
            ]
        );
        assert_eq!(
            s(grant(&["TRAVERSE"], obj("label", None, "Person"), "lect")).lines().nth(1).unwrap(),
            "GRANT TRAVERSE ON GRAPH * NODES Person TO lect;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["SET PROPERTY {age}".into()], object: obj("relationship", Some("neo4j"), "KNOWS"), from: "lect".into() }),
            "REVOKE SET PROPERTY {age} ON GRAPH neo4j RELATIONSHIPS KNOWS FROM lect;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["ACCESS".into()], object: obj("graph", None, "HOME"), from: "lect".into() }),
            "REVOKE ACCESS ON HOME DATABASE FROM lect;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["INDEX MANAGEMENT".into()], object: obj("graph", Some("my"), "db"), from: "lect".into() }),
            "REVOKE INDEX MANAGEMENT ON DATABASE `my.db` FROM lect;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["EXECUTE PROCEDURE apoc.*".into()], object: None, from: "lect".into() }),
            "REVOKE EXECUTE PROCEDURE apoc.* ON DBMS FROM lect;"
        );
        let e = |a| script(Flavor::Neo4j, &a).unwrap_err().to_string();
        assert!(e(grant(&["READ; DROP USER x"], None, "r")).contains("no es un permiso"));
        assert!(e(grant(&["READ `x`"], None, "r")).contains("no es un permiso"));
        assert!(e(grant(&["WRITE"], obj("label", None, "P"), "r")).contains("grafo entero"));
        assert!(e(grant(&["ACCESS"], obj("label", None, "P"), "r")).contains("base entera"));
        assert!(e(grant(&[], None, "r")).contains("al menos"));
        assert!(e(SecurityAction::CreateUser { name: "x".into(), password: None }).contains("contraseña"));
        assert!(matches!(
            script(Flavor::Neo4j, &SecurityAction::Grant { privileges: vec!["READ".into()], object: None, to: "r".into(), grantable: true }),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn neo4j_rows_round_trip() {
        let row = |a: &str, act: &str, res: &str, g: &str, seg: &str| {
            let v = serde_json::json!({"access": a, "action": act, "resource": res, "graph": g, "segment": seg, "role": "lect"});
            neo4j_grant(v.as_object().unwrap(), true)
        };
        let g = row("GRANTED", "read", "all_properties", "*", "NODE(*)");
        assert_eq!((g.privilege.as_str(), g.object.as_deref(), g.via.as_deref()), ("READ {*}", None, Some("lect")));
        let g = row("GRANTED", "set_property", "property(age)", "neo4j", "RELATIONSHIP(KNOWS)");
        assert_eq!((g.privilege.as_str(), g.object.as_deref(), g.object_kind.as_deref()), ("SET PROPERTY {age}", Some("neo4j.KNOWS"), Some("relationship")));
        let g = row("DENIED", "write", "graph", "*", "NODE(*)");
        assert!(g.denied && g.privilege == "WRITE");
        let g = row("GRANTED", "user_management", "database", "*", "database");
        assert_eq!((g.privilege.as_str(), g.object_kind.as_deref()), ("USER MANAGEMENT", Some("dbms")));
        let g = row("GRANTED", "access", "database", "HOME", "database");
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref()), (Some("HOME"), Some("graph")));
        let g = row("GRANTED", "transaction_management", "database", "*", "USER(*)");
        assert_eq!(g.privilege, "TRANSACTION MANAGEMENT");
        assert_eq!(scope(&g.privilege), Scope::Database);
        let g = row("GRANTED", "set_label", "label(Foo)", "*", "NODE(*)");
        assert_eq!(g.privilege, "SET LABEL Foo");
    }

    #[test]
    fn memgraph_scripts() {
        let s = |a| script(Flavor::Memgraph, &a).unwrap();
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: Some("p'w".into()) }), "CREATE USER ana IDENTIFIED BY 'p\\'w';");
        assert_eq!(s(SecurityAction::CreateUser { name: "ana".into(), password: None }), "CREATE USER ana;");
        assert_eq!(s(SecurityAction::SetPassword { name: "ana".into(), password: "x".into() }), "SET PASSWORD FOR ana TO 'x';");
        assert_eq!(s(grant(&["match", "create"], None, "ana")), "GRANT MATCH, CREATE TO ana;");
        assert_eq!(s(grant(&["read"], obj("label", None, "Person"), "lect")), "GRANT READ ON NODES CONTAINING LABELS :Person TO lect;");
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["UPDATE".into()], object: obj("relationship", None, "KNOWS"), from: "lect".into() }),
            "REVOKE UPDATE ON EDGES OF TYPE :KNOWS FROM lect;"
        );
        assert_eq!(s(SecurityAction::AddMember { role: "lect".into(), member: "ana".into() }), "GRANT ROLE lect TO ana;");
        assert!(script(Flavor::Memgraph, &grant(&["MATCH"], obj("label", None, "P"), "r")).is_err());
        assert!(script(Flavor::Memgraph, &grant(&["MATCH;"], None, "r")).is_err());
        assert!(matches!(script(Flavor::Memgraph, &SecurityAction::SetLogin { name: "a".into(), enabled: false }), Err(Error::Unsupported(_))));
        assert!(matches!(script(Flavor::Neptune, &SecurityAction::CreateRole { name: "a".into() }), Err(Error::Unsupported(_))));
        assert!(spec(Flavor::Neptune).is_none());
    }
}
