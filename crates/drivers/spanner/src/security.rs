//! Users, roles and permissions (docs/usuarios-y-permisos.md): Spanner's
//! fine-grained access control. Users are IAM principals, managed in IAM,
//! not in the database: DBine lists and manages the database roles
//! (`CREATE ROLE`, `GRANT … ON TABLE … TO ROLE`, `GRANT ROLE … TO ROLE`),
//! read from `INFORMATION_SCHEMA.ROLES`, `ROLE_GRANTEES` and the
//! `*_PRIVILEGES` views. There is no database-wide grant and no grant
//! option.

use crate::SpannerSession;
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SchemaSpec, SecurityAction, SecuritySpec};
use serde_json::Value as Json;
use std::collections::HashMap;

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["SELECT", "INSERT", "UPDATE", "DELETE"],
        object_kinds: vec!["table", "view"],
        create_user: false,
        create_role: true,
        passwords: false,
        membership: true,
        per_database: true,
    }
}

/// Roles every database has.
const SYSTEM_ROLES: &[&str] = &["public", "spanner_info_reader", "spanner_sys_reader"];

// -- reading ---------------------------------------------------------------

type Row = HashMap<String, String>;

/// Rows of a query by lowercase column name (NULLs left out).
async fn named(s: &mut SpannerSession, sql: &str) -> Result<Vec<Row>> {
    let r = s.read(sql, None).await?;
    let names: Vec<String> = r
        .pointer("/metadata/rowType/fields")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .map(|f| f.get("name").and_then(Json::as_str).unwrap_or_default().to_ascii_lowercase())
        .collect();
    Ok(r.get("rows")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .map(|row| {
            names
                .iter()
                .zip(row.as_array().into_iter().flatten())
                .filter_map(|(n, v)| {
                    let v = match v {
                        Json::Null => return None,
                        Json::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    Some((n.clone(), v))
                })
                .collect()
        })
        .collect())
}

fn get<'a>(r: &'a Row, k: &str) -> &'a str {
    r.get(k).map(String::as_str).unwrap_or_default()
}

/// The emulator has no fine-grained access control views.
fn explain(e: Error) -> Error {
    match e {
        Error::Query(m) if m.contains("marshal") || m.contains("Not Found") || m.contains("not found") => Error::Unsupported(format!(
            "esta base no informa roles (el emulador de Spanner no tiene el control de acceso detallado): {m}"
        )),
        e => e,
    }
}

/// (member role, role) pairs.
async fn memberships(s: &mut SpannerSession) -> Result<Vec<(String, String)>> {
    let rows = named(s, "SELECT ROLE_NAME, GRANTEE FROM INFORMATION_SCHEMA.ROLE_GRANTEES").await.map_err(explain)?;
    Ok(rows.iter().map(|r| (get(r, "grantee").to_string(), get(r, "role_name").to_string())).collect())
}

pub async fn principals(s: &mut SpannerSession) -> Result<Vec<Principal>> {
    let roles = named(s, "SELECT * FROM INFORMATION_SCHEMA.ROLES ORDER BY ROLE_NAME").await.map_err(explain)?;
    let members = memberships(s).await?;
    Ok(roles
        .iter()
        .map(|r| {
            let name = get(r, "role_name").to_string();
            let system = get(r, "is_system") == "true" || SYSTEM_ROLES.contains(&name.as_str());
            let mut member_of: Vec<String> = members.iter().filter(|(m, _)| *m == name).map(|(_, r)| r.clone()).collect();
            member_of.sort();
            let what = match name.as_str() {
                "public" => "Rol del sistema: lo tienen todos los roles",
                "spanner_info_reader" => "Rol del sistema: ve INFORMATION_SCHEMA entero",
                "spanner_sys_reader" => "Rol del sistema: ve las tablas SPANNER_SYS",
                _ => "Rol de base de datos",
            };
            Principal {
                name,
                kind: PrincipalKind::Role,
                can_login: None,
                superuser: Some(false),
                disabled: None,
                member_of,
                details: vec![
                    ("Tipo".into(), what.into()),
                    (
                        "Quién lo usa".into(),
                        "principales de IAM con roles/spanner.fineGrainedAccessUser y el permiso sobre este rol (se asignan en IAM)"
                            .into(),
                    ),
                ],
                system,
            }
        })
        .collect())
}

/// The roles `principal` belongs to, directly or not, each with the direct
/// role it comes through.
fn closure(principal: &str, members: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut queue: Vec<(String, String)> =
        members.iter().filter(|(m, _)| m == principal).map(|(_, r)| (r.clone(), r.clone())).collect();
    while let Some((role, via)) = queue.pop() {
        if role == principal || out.iter().any(|(r, _)| *r == role) {
            continue;
        }
        queue.extend(members.iter().filter(|(m, _)| *m == role).map(|(_, r)| (r.clone(), via.clone())));
        out.push((role, via));
    }
    out
}

fn qualified(schema: &str, name: &str) -> String {
    if schema.is_empty() {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

pub async fn grants(s: &mut SpannerSession, principal: &str) -> Result<Vec<Grant>> {
    let members = memberships(s).await?;
    let mut via: HashMap<String, Option<String>> = HashMap::new();
    via.insert(principal.to_string(), None);
    // Every role holds public's privileges.
    if principal != "public" {
        via.insert("public".into(), Some("public".into()));
    }
    for (role, through) in closure(principal, &members) {
        via.insert(role, Some(through));
    }
    let views: Vec<String> = named(s, "SELECT TABLE_SCHEMA, TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_TYPE = 'VIEW'")
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| qualified(get(r, "table_schema"), get(r, "table_name")))
        .collect();
    // (grantee, grant)
    let mut found: Vec<(String, Grant)> = Vec::new();
    let tables = named(s, "SELECT * FROM INFORMATION_SCHEMA.TABLE_PRIVILEGES").await.map_err(explain)?;
    for r in &tables {
        let object = qualified(get(r, "table_schema"), get(r, "table_name"));
        let kind = if views.contains(&object) { kinds::VIEW } else { kinds::TABLE };
        found.push((
            get(r, "grantee").to_string(),
            Grant { privilege: get(r, "privilege_type").to_string(), object: Some(object), object_kind: Some(kind.into()), ..Default::default() },
        ));
    }
    // Column privileges, grouped per table and privilege; those the table
    // grant already gives aren't repeated.
    let mut columns: Vec<((String, String, String), Vec<String>)> = Vec::new();
    for r in named(s, "SELECT * FROM INFORMATION_SCHEMA.COLUMN_PRIVILEGES ORDER BY COLUMN_NAME").await.unwrap_or_default() {
        let key = (get(&r, "grantee").to_string(), qualified(get(&r, "table_schema"), get(&r, "table_name")), get(&r, "privilege_type").to_string());
        let whole = found.iter().any(|(g, x)| *g == key.0 && x.object.as_deref() == Some(key.1.as_str()) && x.privilege == key.2);
        if whole {
            continue;
        }
        match columns.iter_mut().find(|(k, _)| *k == key) {
            Some((_, cols)) => cols.push(get(&r, "column_name").to_string()),
            None => columns.push((key, vec![get(&r, "column_name").to_string()])),
        }
    }
    for ((grantee, object, privilege), cols) in columns {
        found.push((
            grantee,
            Grant { privilege: format!("{privilege} ({})", cols.join(", ")), object: Some(object), object_kind: Some(kinds::TABLE.into()), ..Default::default() },
        ));
    }
    for r in named(s, "SELECT * FROM INFORMATION_SCHEMA.CHANGE_STREAM_PRIVILEGES").await.unwrap_or_default() {
        found.push((
            get(&r, "grantee").to_string(),
            Grant {
                privilege: get(&r, "privilege_type").to_string(),
                object: Some(qualified(get(&r, "change_stream_schema"), get(&r, "change_stream_name"))),
                object_kind: Some("change_stream".into()),
                ..Default::default()
            },
        ));
    }
    for r in named(s, "SELECT * FROM INFORMATION_SCHEMA.ROUTINE_PRIVILEGES").await.unwrap_or_default() {
        found.push((
            get(&r, "grantee").to_string(),
            Grant {
                privilege: get(&r, "privilege_type").to_string(),
                object: Some(qualified(get(&r, "specific_schema"), get(&r, "specific_name"))),
                object_kind: Some(kinds::FUNCTION.into()),
                ..Default::default()
            },
        ));
    }
    let mut out: Vec<Grant> = found
        .into_iter()
        .filter_map(|(grantee, mut g)| {
            g.via = via.get(&grantee)?.clone();
            Some(g)
        })
        .collect();
    out.sort_by(|a, b| {
        (a.via.is_some(), &a.object_kind, &a.object, &a.privilege).cmp(&(b.via.is_some(), &b.object_kind, &b.object, &b.privilege))
    });
    out.dedup();
    Ok(out)
}

// -- scripts ---------------------------------------------------------------

/// A GoogleSQL quoted identifier: backslash escapes, not doubled backticks.
fn bq(name: &str) -> String {
    format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
}

/// `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `EXECUTE`, optionally with
/// columns (`SELECT (a, b)`).
fn privileges(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un permiso".into()));
    }
    let bad = |x: &str| Error::Query(format!("«{x}» no es un permiso de Spanner"));
    let mut out = Vec::new();
    for x in p {
        let (name, cols) = match x.split_once('(') {
            Some((n, c)) => (n.trim(), Some(c.trim().strip_suffix(')').ok_or_else(|| bad(x))?)),
            None => (x.trim(), None),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(bad(x));
        }
        let name = name.to_uppercase();
        match cols {
            Some(c) => {
                let cols: Vec<String> = c.split(',').map(|c| c.trim().trim_matches('`')).filter(|c| !c.is_empty()).map(bq).collect();
                if cols.is_empty() {
                    return Err(bad(x));
                }
                out.push(format!("{name}({})", cols.join(", ")));
            }
            None => out.push(name),
        }
    }
    Ok(out.join(", "))
}

/// `TABLE t`, `VIEW v`, `CHANGE STREAM cs`, `TABLE FUNCTION f`.
fn target(object: &Option<ObjectRef>) -> Result<String> {
    let Some(o) = object else {
        return Err(Error::Query("en Spanner los permisos se otorgan sobre una tabla, una vista o un flujo de cambios".into()));
    };
    let name = match o.schema() {
        Some(sc) => format!("{}.{}", bq(sc), bq(&o.name)),
        None => bq(&o.name),
    };
    Ok(match o.kind.as_str() {
        kinds::VIEW => format!("VIEW {name}"),
        "change_stream" => format!("CHANGE STREAM {name}"),
        kinds::FUNCTION => format!("TABLE FUNCTION {name}"),
        "schema" => format!("SCHEMA {}", bq(&o.name)),
        _ => format!("TABLE {name}"),
    })
}

fn iam_users() -> Error {
    Error::Unsupported(
        "en Spanner los usuarios son principales de IAM: se crean y habilitan en IAM (roles/spanner.fineGrainedAccessUser), no en la base".into(),
    )
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::CreateUser { .. } | SecurityAction::SetPassword { .. } | SecurityAction::SetLogin { .. } => return Err(iam_users()),
        SecurityAction::Drop { kind: PrincipalKind::User, .. } => return Err(iam_users()),
        SecurityAction::CreateRole { name } => format!("CREATE ROLE {};", bq(name)),
        SecurityAction::Drop { name, kind: PrincipalKind::Role } => format!(
            "-- Antes hay que revocarle sus permisos y membresías, y quitarlo de los permisos de IAM que lo nombran.\nDROP ROLE {};",
            bq(name)
        ),
        SecurityAction::Grant { grantable: true, .. } => {
            return Err(Error::Query("Spanner no permite que un rol otorgue a otros sus permisos (no hay WITH GRANT OPTION)".into()))
        }
        SecurityAction::Grant { privileges: p, object, to, .. } => {
            format!("GRANT {} ON {} TO ROLE {};", privileges(p)?, target(object)?, bq(to))
        }
        SecurityAction::Revoke { privileges: p, object, from } => {
            format!("REVOKE {} ON {} FROM ROLE {};", privileges(p)?, target(object)?, bq(from))
        }
        SecurityAction::AddMember { role, member } => format!("GRANT ROLE {} TO ROLE {};", bq(role), bq(member)),
        SecurityAction::RemoveMember { role, member } => format!("REVOKE ROLE {} FROM ROLE {};", bq(role), bq(member)),
    })
}

// -- named schemas -----------------------------------------------------------

/// "Nuevo esquema…": named schemas have no owner and `DROP SCHEMA` takes
/// only an empty one (no CASCADE). Fine-grained access control grants
/// `USAGE ON SCHEMA` to a database role (what lets it reach the schema's
/// objects); the emulator doesn't parse `ON SCHEMA`, production Spanner
/// does. No grant option (Spanner has no WITH GRANT OPTION).
pub fn schema_spec() -> SchemaSpec {
    // No WITH GRANT OPTION in Spanner (see `script`).
    SchemaSpec { privileges: vec!["USAGE"], grant_option: false, ..SchemaSpec::default() }
}

fn schema_name(name: &str) -> Result<String> {
    match name.trim() {
        "" => Err(Error::Query("escribí el nombre del esquema".into())),
        n => Ok(bq(n)),
    }
}

pub fn create_schema(name: &str) -> Result<String> {
    Ok(format!("CREATE SCHEMA {};", schema_name(name)?))
}

pub fn drop_schema(name: &str) -> Result<String> {
    Ok(format!("DROP SCHEMA {};", schema_name(name)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, name: &str) -> Option<ObjectRef> {
        Some(ObjectRef { kind: kind.into(), schema: None, name: name.into() })
    }

    #[test]
    fn writes_the_scripts() {
        let s = |a| script(&a).unwrap();
        assert_eq!(s(SecurityAction::CreateRole { name: "lec`tores".into() }), "CREATE ROLE `lec\\`tores`;");
        assert!(s(SecurityAction::Drop { name: "r".into(), kind: PrincipalKind::Role }).ends_with("\nDROP ROLE `r`;"));
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["select".into(), "INSERT".into()], object: obj("table", "Facturas"), to: "r".into(), grantable: false }),
            "GRANT SELECT, INSERT ON TABLE `Facturas` TO ROLE `r`;"
        );
        assert_eq!(
            s(SecurityAction::Grant {
                privileges: vec!["SELECT (id, `total`)".into()],
                object: Some(ObjectRef { kind: "table".into(), schema: Some("ventas".into()), name: "f".into() }),
                to: "r".into(),
                grantable: false
            }),
            "GRANT SELECT(`id`, `total`) ON TABLE `ventas`.`f` TO ROLE `r`;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: obj("view", "v"), from: "r".into() }),
            "REVOKE SELECT ON VIEW `v` FROM ROLE `r`;"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["SELECT".into()], object: obj("change_stream", "cs"), from: "r".into() }),
            "REVOKE SELECT ON CHANGE STREAM `cs` FROM ROLE `r`;"
        );
        assert_eq!(s(SecurityAction::AddMember { role: "a".into(), member: "b".into() }), "GRANT ROLE `a` TO ROLE `b`;");
        assert_eq!(s(SecurityAction::RemoveMember { role: "a".into(), member: "b".into() }), "REVOKE ROLE `a` FROM ROLE `b`;");
        for a in [
            SecurityAction::CreateUser { name: "a".into(), password: None },
            SecurityAction::SetLogin { name: "a".into(), enabled: true },
            SecurityAction::Drop { name: "a".into(), kind: PrincipalKind::User },
        ] {
            assert!(matches!(script(&a), Err(Error::Unsupported(_))));
        }
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: None, to: "r".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: obj("table", "t"), to: "r".into(), grantable: true }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT; DROP".into()], object: obj("table", "t"), to: "r".into(), grantable: false }).is_err());
        assert!(script(&SecurityAction::Grant { privileges: vec!["SELECT (a".into()], object: obj("table", "t"), to: "r".into(), grantable: false }).is_err());
    }

    #[test]
    fn schema_scripts() {
        assert_eq!(create_schema(" ventas ").unwrap(), "CREATE SCHEMA `ventas`;");
        assert_eq!(drop_schema("ven`tas").unwrap(), "DROP SCHEMA `ven\\`tas`;");
        assert!(create_schema("").is_err());
        let spec = schema_spec();
        assert!(!spec.owner && !spec.cascade && spec.privileges == vec!["USAGE"]);
        let schema = obj("schema", "ven`tas");
        assert_eq!(
            script(&SecurityAction::Grant { privileges: vec!["usage".into()], object: schema.clone(), to: "lect".into(), grantable: false }).unwrap(),
            "GRANT USAGE ON SCHEMA `ven\\`tas` TO ROLE `lect`;"
        );
        assert!(script(&SecurityAction::Grant { privileges: vec!["USAGE".into()], object: schema, to: "lect".into(), grantable: true }).is_err());
    }
}
