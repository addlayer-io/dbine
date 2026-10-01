//! Users, groups and permissions (docs/usuarios-y-permisos.md) for
//! Databricks with Unity Catalog.
//!
//! Users, service principals and groups are managed in the account or the
//! workspace (console, SCIM, Terraform), not with SQL: DBine lists them
//! (`SHOW USERS`, `SHOW GROUPS`) but doesn't create, drop or change them,
//! nor their memberships. What SQL does is `GRANT` / `REVOKE` on the
//! metastore, catalogs, schemas, tables, views and functions, which is what
//! the tab offers. Grants are read from `system.information_schema` (the
//! ones inherited from a parent object are left out: they're the parent's
//! grant), following the groups a user belongs to.

use crate::DatabricksSession;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SchemaSpec, SecurityAction, SecuritySpec};
use serde_json::json;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "SELECT", "MODIFY", "EXECUTE", "USE CATALOG", "USE SCHEMA", "BROWSE", "READ VOLUME", "WRITE VOLUME",
            "CREATE SCHEMA", "CREATE TABLE", "CREATE FUNCTION", "CREATE VOLUME", "CREATE MATERIALIZED VIEW", "APPLY TAG",
            "REFRESH", "MANAGE", "ALL PRIVILEGES",
            // On the metastore (granted without an object).
            "CREATE CATALOG", "CREATE EXTERNAL LOCATION", "CREATE CONNECTION", "CREATE SHARE", "CREATE RECIPIENT",
        ],
        // "" = the metastore; "database" = a catalog.
        object_kinds: vec!["", "database", "schema", "table", "view", "function"],
        create_user: false,
        create_role: false,
        passwords: false,
        membership: false,
        per_database: false,
    }
}

/// Groups every workspace has.
const SYSTEM_GROUPS: &[&str] = &["admins", "users", "account users"];
/// Above this many users and groups, their memberships (one statement each)
/// aren't read up front.
const MEMBERSHIP_LIMIT: usize = 60;

fn q(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

const MANAGED_OUTSIDE: &str =
    "en Databricks los usuarios, las entidades de servicio y los grupos se administran en la consola de la cuenta o del workspace (o por SCIM), no con SQL";

// -- reading -----------------------------------------------------------------

/// The direct groups of a user (`WITH USER`) or a group (`WITH GROUP`).
async fn groups_of(s: &DatabricksSession, with: &str, name: &str, direct_only: bool) -> Result<Vec<String>> {
    let rows = s.named_rows(&format!("SHOW GROUPS WITH {with} {}", q(name))).await?;
    Ok(rows
        .into_iter()
        .filter(|r| !direct_only || r.get("directgroup").is_none_or(|v| v.eq_ignore_ascii_case("true")))
        .filter_map(|mut r| r.remove("name"))
        .collect())
}

pub async fn principals(s: &DatabricksSession) -> Result<Vec<Principal>> {
    let mut out: Vec<Principal> = match s.named_rows("SHOW USERS").await {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|mut r| r.remove("name"))
            .map(|name| Principal { name, kind: PrincipalKind::User, can_login: Some(true), ..Default::default() })
            .collect(),
        Err(_) => {
            let me = s.text_rows("SELECT current_user()", &[]).await?;
            let name = me.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default();
            vec![Principal {
                name,
                kind: PrincipalKind::User,
                can_login: Some(true),
                details: vec![("Nota".into(), "Este warehouse no lista usuarios (SHOW USERS): se ve solo el propio".into())],
                ..Default::default()
            }]
        }
    };
    let groups = s.named_rows("SHOW GROUPS").await.unwrap_or_default();
    out.extend(groups.into_iter().filter_map(|mut r| r.remove("name")).map(|name| Principal {
        kind: PrincipalKind::Role,
        system: SYSTEM_GROUPS.contains(&name.as_str()),
        superuser: Some(name == "admins"),
        details: vec![("Tipo".into(), "grupo".into())],
        name,
        ..Default::default()
    }));
    let read_members = out.len() <= MEMBERSHIP_LIMIT;
    for p in out.iter_mut() {
        if !read_members {
            if p.kind == PrincipalKind::User {
                p.details.push(("Grupos".into(), "se ven en sus permisos".into()));
            }
            continue;
        }
        let with = if p.kind == PrincipalKind::User { "USER" } else { "GROUP" };
        if let Ok(g) = groups_of(s, with, &p.name, true).await {
            p.member_of = g;
        }
        if p.kind == PrincipalKind::User {
            p.superuser = Some(p.member_of.iter().any(|g| g == "admins"));
        }
    }
    Ok(out)
}

/// The grants of some principals, by kind: privilege, kind, catalog,
/// schema, name, grantable, grantee. `:p0, :p1…` are the grantees.
fn grants_query(n: usize, direct_only: bool) -> String {
    let who = (0..n).map(|i| format!(":p{i}")).collect::<Vec<_>>().join(", ");
    let filter = if direct_only { " AND (inherited_from IS NULL OR inherited_from = 'NONE')" } else { "" };
    let null = "CAST(NULL AS STRING)";
    let views = [
        ("metastore", format!("{null}, {null}, {null}"), "metastore_privileges"),
        ("catalog", format!("catalog_name, {null}, {null}"), "catalog_privileges"),
        ("schema", format!("catalog_name, schema_name, {null}"), "schema_privileges"),
        ("table", "table_catalog, table_schema, table_name".to_string(), "table_privileges"),
        ("function", "specific_catalog, specific_schema, specific_name".to_string(), "routine_privileges"),
        ("volume", "volume_catalog, volume_schema, volume_name".to_string(), "volume_privileges"),
    ];
    views
        .iter()
        .map(|(kind, cols, view)| {
            format!(
                "SELECT privilege_type, '{kind}', {cols}, is_grantable, grantee FROM system.information_schema.{view} WHERE grantee IN ({who}){filter}"
            )
        })
        .collect::<Vec<_>>()
        .join("\nUNION ALL\n")
}

/// A row of [`grants_query`] as a grant; names in the session's catalog
/// lose the catalog.
fn grant_of(r: &[Option<String>], catalog: Option<&str>) -> Grant {
    let at = |i: usize| r.get(i).cloned().flatten().filter(|s| !s.is_empty());
    let kind = at(1).unwrap_or_default();
    let parts: Vec<String> = [at(2), at(3), at(4)].into_iter().flatten().collect();
    let here = catalog.is_some_and(|c| parts.first().is_some_and(|f| f.eq_ignore_ascii_case(c)));
    let (object, object_kind) = match kind.as_str() {
        "metastore" => (None, None),
        "catalog" => (parts.first().cloned(), Some("database".to_string())),
        k => {
            let name = if here && parts.len() > 1 { parts[1..].join(".") } else { parts.join(".") };
            (Some(name), Some(k.to_string()))
        }
    };
    Grant {
        privilege: at(0).unwrap_or_default(),
        object,
        object_kind,
        grantable: at(5).is_some_and(|v| v.eq_ignore_ascii_case("YES") || v.eq_ignore_ascii_case("true")),
        denied: false,
        via: None,
    }
}

pub async fn grants(s: &DatabricksSession, principal: &str) -> Result<Vec<Grant>> {
    // Its groups, direct and nested: a user's, or a group's.
    let groups = match groups_of(s, "USER", principal, false).await {
        Ok(g) => g,
        Err(_) => groups_of(s, "GROUP", principal, false).await.unwrap_or_default(),
    };
    let mut who = vec![principal.to_string()];
    who.extend(groups.into_iter().filter(|g| g != principal));
    let params = json!(who.iter().enumerate().map(|(i, w)| json!({ "name": format!("p{i}"), "value": w })).collect::<Vec<_>>());
    let rows = match s.run(&grants_query(who.len(), true), 100_000, Some(params.clone())).await {
        Ok(st) => st,
        // Before inherited_from: every row, inherited ones included.
        Err(_) => s.run(&grants_query(who.len(), false), 100_000, Some(params)).await?,
    };
    let catalog = s.catalog.as_deref();
    Ok(rows
        .rows
        .into_iter()
        .map(|r| {
            let r: Vec<Option<String>> = r.into_iter().map(|v| v.as_str().map(str::to_string)).collect();
            let mut g = grant_of(&r, catalog);
            g.via = r.get(6).cloned().flatten().filter(|grantee| grantee != principal);
            g
        })
        .collect())
}

// -- scripts -----------------------------------------------------------------

/// What a privilege applies to: the metastore, a catalog, a schema, an object.
fn on(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else { return Ok("METASTORE".into()) };
    let name = match o.schema() {
        Some(sc) => format!("{}.{}", q(sc), q(&o.name)),
        None => q(&o.name),
    };
    let kind = match o.kind.as_str() {
        "database" | "catalog" => return Ok(format!("CATALOG {}", q(&o.name))),
        "schema" => "SCHEMA",
        "table" | "materialized_view" => "TABLE",
        "view" => "VIEW",
        "function" => "FUNCTION",
        "volume" => "VOLUME",
        other => return Err(Error::Query(format!("en Databricks no se otorgan permisos sobre «{other}»"))),
    };
    Ok(format!("{kind} {name}"))
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
            return Err(Error::Query(format!("«{x}» no es un permiso de Databricks")));
        }
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase();
        if !out.contains(&name) {
            out.push(name);
        }
    }
    Ok(out.join(", "))
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::Grant { privileges: p, object, to, grantable } => {
            let grant = format!("GRANT {} ON {} TO {};", privileges(p)?, on(object)?, q(to));
            if *grantable {
                format!("-- Unity Catalog no tiene WITH GRANT OPTION: para que pueda otorgar permisos sobre el objeto, otorgale MANAGE.\n{grant}")
            } else {
                grant
            }
        }
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {} ON {} FROM {};", privileges(p)?, on(object)?, q(from)),
        _ => return Err(Error::Unsupported(MANAGED_OUTSIDE.into())),
    })
}

// -- schemas -----------------------------------------------------------------

/// "Nuevo esquema…" in the session's catalog: Unity Catalog schemas have an
/// owner (`ALTER SCHEMA … OWNER TO`) and drop with `CASCADE` / `RESTRICT`.
pub fn schema_spec() -> SchemaSpec {
    SchemaSpec {
        owner: true,
        owner_kinds: dbine_driver::SchemaOwnerKinds::Both,
        cascade: true,
        privileges: vec![
            "USE SCHEMA", "SELECT", "MODIFY", "EXECUTE", "READ VOLUME", "WRITE VOLUME", "CREATE TABLE", "CREATE FUNCTION",
            "CREATE VOLUME", "CREATE MATERIALIZED VIEW", "CREATE MODEL", "APPLY TAG", "MANAGE", "ALL PRIVILEGES",
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

/// Owned by the creator; `schema_owner` hands it over afterwards.
pub fn create_schema(name: &str) -> Result<String> {
    Ok(format!("CREATE SCHEMA {};", schema_name(name)?))
}

/// A grant on the new schema. Unity Catalog has no WITH GRANT OPTION: the
/// right to grant on a schema is MANAGE on it, so "con opción de otorgar"
/// adds MANAGE (which ALL PRIVILEGES doesn't include).
pub fn schema_grant(name: &str, p: &[String], to: &str, grantable: bool) -> Result<String> {
    let mut p = p.to_vec();
    if grantable && !p.iter().any(|x| x.trim().eq_ignore_ascii_case("MANAGE")) {
        p.push("MANAGE".into());
    }
    let object = Some(ObjectRef { kind: "schema".into(), schema: None, name: name.to_string() });
    let grant = script(&SecurityAction::Grant { privileges: p, object, to: to.to_string(), grantable: false })?;
    Ok(if grantable {
        format!("-- Unity Catalog no tiene WITH GRANT OPTION: poder otorgar permisos sobre el esquema es MANAGE.\n{grant}")
    } else {
        grant
    })
}

/// Hands the schema to `owner` (a user, group or service principal). Meant
/// to run after the grants: once it's given away, the creator can't grant on
/// it without MANAGE on the schema or owning the catalog.
pub fn schema_owner(name: &str, owner: &str) -> Result<String> {
    match owner.trim() {
        "" => Err(Error::Query("elegí el dueño del esquema".into())),
        o => Ok(format!("ALTER SCHEMA {} OWNER TO {};", schema_name(name)?, q(o))),
    }
}

pub fn drop_schema(name: &str, cascade: bool) -> Result<String> {
    Ok(format!("DROP SCHEMA {} {};", schema_name(name)?, if cascade { "CASCADE" } else { "RESTRICT" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() })
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), "MODIFY".into()], object: obj("table", Some("ventas"), "fac`t"), to: "ana@x.com".into(), grantable: false }),
            "GRANT SELECT, MODIFY ON TABLE `ventas`.`fac``t` TO `ana@x.com`;"
        );
        assert!(s(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("view", None, "v"), to: "g".into(), grantable: true })
            .ends_with("\nGRANT SELECT ON VIEW `v` TO `g`;"));
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["create  catalog".into()], object: None, to: "admins".into(), grantable: false }),
            "GRANT CREATE CATALOG ON METASTORE TO `admins`;"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["USE CATALOG".into()], object: obj("database", None, "main"), to: "g".into(), grantable: false }),
            "GRANT USE CATALOG ON CATALOG `main` TO `g`;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["USE SCHEMA".into()], object: obj("schema", None, "ventas"), from: "g".into() }),
            "REVOKE USE SCHEMA ON SCHEMA `ventas` FROM `g`;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["EXECUTE".into()], object: obj("function", Some("s"), "f"), from: "g".into() }),
            "REVOKE EXECUTE ON FUNCTION `s`.`f` FROM `g`;"
        );
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT; DROP TABLE x".into()], object: None, to: "a".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("trigger", None, "t"), to: "a".into(), grantable: false }).is_err());
        for a in [
            SecurityAction::CreateUser { name: "a".into(), password: Some("x".into()) },
            SecurityAction::CreateRole { name: "g".into() },
            SecurityAction::Drop { name: "g".into(), kind: PrincipalKind::Role },
            SecurityAction::SetPassword { name: "a".into(), password: "x".into() },
            SecurityAction::SetLogin { name: "a".into(), enabled: false },
            SecurityAction::AddMember { role: "g".into(), member: "a".into() },
        ] {
            assert!(matches!(script(&a), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn reads_information_schema_rows() {
        let r = |v: &[Option<&str>]| v.iter().map(|x| x.map(str::to_string)).collect::<Vec<_>>();
        let g = grant_of(&r(&[Some("SELECT"), Some("table"), Some("main"), Some("ventas"), Some("fact"), Some("NO"), Some("ana")]), Some("main"));
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref(), g.grantable), (Some("ventas.fact"), Some("table"), false));
        let g = grant_of(&r(&[Some("SELECT"), Some("table"), Some("otro"), Some("ventas"), Some("fact"), Some("YES"), Some("ana")]), Some("main"));
        assert_eq!((g.object.as_deref(), g.grantable), (Some("otro.ventas.fact"), true));
        let g = grant_of(&r(&[Some("USE CATALOG"), Some("catalog"), Some("main"), None, None, Some("NO"), Some("ana")]), Some("main"));
        assert_eq!((g.object.as_deref(), g.object_kind.as_deref()), (Some("main"), Some("database")));
        let g = grant_of(&r(&[Some("CREATE CATALOG"), Some("metastore"), None, None, None, Some("NO"), Some("ana")]), None);
        assert_eq!((g.object, g.object_kind), (None, None));
        assert!(grants_query(2, true).contains("grantee IN (:p0, :p1) AND (inherited_from IS NULL"));
        assert!(!grants_query(1, false).contains("inherited_from"));
    }

    #[test]
    fn schema_scripts() {
        assert_eq!(create_schema(" ventas ").unwrap(), "CREATE SCHEMA `ventas`;");
        assert_eq!(create_schema("ven`tas").unwrap(), "CREATE SCHEMA `ven``tas`;");
        assert_eq!(schema_owner("ven`tas", " grupo ").unwrap(), "ALTER SCHEMA `ven``tas` OWNER TO `grupo`;");
        assert!(schema_owner("ventas", "").is_err());
        assert!(create_schema("").is_err());
        // "Con opción de otorgar": MANAGE, once.
        let g = schema_grant("ventas", &["SELECT".into(), "manage".into()], "ana", true).unwrap();
        assert!(g.starts_with("-- ") && g.ends_with("\nGRANT SELECT, MANAGE ON SCHEMA `ventas` TO `ana`;"), "{g}");
        assert!(schema_grant("ventas", &["ALL PRIVILEGES".into()], "ana", true).unwrap().ends_with("GRANT ALL PRIVILEGES, MANAGE ON SCHEMA `ventas` TO `ana`;"));
        assert_eq!(drop_schema("ventas", true).unwrap(), "DROP SCHEMA `ventas` CASCADE;");
        assert_eq!(drop_schema("ventas", false).unwrap(), "DROP SCHEMA `ventas` RESTRICT;");
        let spec = schema_spec();
        assert!(spec.owner && spec.cascade);
        let g = script(&SecurityAction::Grant {
            privileges: spec.privileges.iter().map(|p| p.to_string()).collect(),
            object: obj("schema", None, "ventas"),
            to: "analistas".into(),
            grantable: false,
        })
        .unwrap();
        assert!(g.starts_with("GRANT USE SCHEMA, SELECT, MODIFY") && g.ends_with(" ON SCHEMA `ventas` TO `analistas`;"), "{g}");
    }
}
