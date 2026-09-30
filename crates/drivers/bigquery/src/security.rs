//! Users, roles and permissions (docs/usuarios-y-permisos.md) for BigQuery.
//! Identities are Google Cloud IAM principals (`user:…`, `group:…`,
//! `serviceAccount:…`, `domain:…`, `specialGroup:…`), which BigQuery can't
//! create; what it manages is who holds which IAM role on a dataset, table
//! or view, with DCL (`GRANT \`roles/bigquery.dataViewer\` ON SCHEMA …
//! TO "user:…"`).
//!
//! Principals are per dataset (the session's): the grantees of the
//! dataset's access list (`datasets.get`) and of its tables' and views' IAM
//! policies (`tables.getIamPolicy`). `INFORMATION_SCHEMA.OBJECT_PRIVILEGES`
//! would need one query per object too, and a region. Project-level IAM
//! roles (which also reach every dataset) are Resource Manager's, not
//! listed.

use crate::ddl::{ident, lit};
use crate::BigQuerySession;
use dbine_driver::{kinds, Error, Grant, ObjectRef, Principal, PrincipalKind, Result, SecurityAction, SecuritySpec};
use serde_json::{json, Value as Json};

pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec![
            "roles/bigquery.dataViewer",
            "roles/bigquery.dataEditor",
            "roles/bigquery.dataOwner",
            "roles/bigquery.metadataViewer",
            "roles/bigquery.filteredDataViewer",
            "roles/bigquery.admin",
        ],
        // "schema" = a dataset (its name is written in); no project-wide
        // grants: they need the project's name.
        object_kinds: vec!["schema", "table", "view"],
        create_user: false,
        create_role: false,
        passwords: false,
        membership: false,
        per_database: true,
    }
}

/// Tables whose IAM policy is read at most (one request each).
const MAX_TABLES: usize = 200;

/// One role binding: who, which role, on what.
#[derive(Debug, Clone, PartialEq)]
struct Binding {
    member: String,
    role: String,
    /// `dataset` or `dataset.table`.
    object: String,
    kind: &'static str,
}

/// A legacy dataset access role as its IAM role.
fn iam_role(role: &str) -> String {
    match role {
        "READER" => "roles/bigquery.dataViewer".into(),
        "WRITER" => "roles/bigquery.dataEditor".into(),
        "OWNER" => "roles/bigquery.dataOwner".into(),
        r => r.into(),
    }
}

/// A dataset `access` entry's principal, as DCL writes it; `None` for
/// authorized views, routines and datasets.
fn access_member(e: &Json) -> Option<String> {
    let s = |k: &str| e.get(k).and_then(Json::as_str);
    if let Some(u) = s("userByEmail") {
        let kind = if u.ends_with(".gserviceaccount.com") { "serviceAccount" } else { "user" };
        return Some(format!("{kind}:{u}"));
    }
    if let Some(g) = s("groupByEmail") {
        return Some(format!("group:{g}"));
    }
    if let Some(d) = s("domain") {
        return Some(format!("domain:{d}"));
    }
    if let Some(g) = s("specialGroup") {
        return Some(format!("specialGroup:{g}"));
    }
    s("iamMember").map(str::to_string)
}

fn dataset_bindings(dataset: &str, ds: &Json) -> Vec<Binding> {
    ds.get("access")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some(Binding {
                member: access_member(e)?,
                role: iam_role(e.get("role").and_then(Json::as_str)?),
                object: dataset.to_string(),
                kind: "schema",
            })
        })
        .collect()
}

fn policy_bindings(object: &str, kind: &'static str, policy: &Json) -> Vec<Binding> {
    let mut out = Vec::new();
    for b in policy.get("bindings").and_then(Json::as_array).into_iter().flatten() {
        let Some(role) = b.get("role").and_then(Json::as_str) else { continue };
        for m in b.get("members").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str) {
            out.push(Binding { member: m.to_string(), role: role.to_string(), object: object.to_string(), kind });
        }
    }
    out
}

async fn bindings(s: &BigQuerySession) -> Result<Vec<Binding>> {
    let dataset = s
        .dataset
        .clone()
        .ok_or_else(|| Error::Query("elegí un dataset: en BigQuery los permisos se ven por dataset".into()))?;
    let ds = s.api.get(&["datasets", &dataset], &[]).await?;
    let mut out = dataset_bindings(&dataset, &ds);
    let tables = s.api.list_all(&["datasets", &dataset, "tables"], "tables").await.unwrap_or_default();
    for t in tables.iter().take(MAX_TABLES) {
        let Some(name) = t.pointer("/tableReference/tableId").and_then(Json::as_str) else { continue };
        let kind = match t.get("type").and_then(Json::as_str) {
            Some("VIEW") => kinds::VIEW,
            Some("MATERIALIZED_VIEW") => kinds::MATERIALIZED_VIEW,
            _ => kinds::TABLE,
        };
        let resource = format!("{name}:getIamPolicy");
        match s.api.post(&["datasets", &dataset, "tables", &resource], &[], &json!({})).await {
            Ok(p) => out.extend(policy_bindings(&format!("{dataset}.{name}"), kind, &p)),
            // Emulators and restricted accounts: the dataset's list is still there.
            Err(e) => tracing::debug!("bigquery: IAM policy of {dataset}.{name} unavailable: {e}"),
        }
    }
    Ok(out)
}

fn principal(member: &str) -> Principal {
    let (kind, id) = member.split_once(':').unwrap_or(("", member));
    let (pk, label) = match kind {
        "user" => (PrincipalKind::User, "Cuenta de Google"),
        "serviceAccount" => (PrincipalKind::User, "Cuenta de servicio"),
        "group" => (PrincipalKind::Role, "Grupo de Google"),
        "domain" => (PrincipalKind::Role, "Dominio de Google Workspace"),
        "specialGroup" => (PrincipalKind::Role, "Grupo especial"),
        _ => (PrincipalKind::Role, "Principal de IAM"),
    };
    let mut details = vec![("Tipo".to_string(), label.to_string())];
    if !id.is_empty() && id != member {
        details.push(("Identidad".into(), id.into()));
    }
    details.push(("Se administra en".into(), "Google Cloud IAM".into()));
    Principal {
        name: member.to_string(),
        kind: pk,
        can_login: None,
        superuser: None,
        disabled: None,
        member_of: Vec::new(),
        details,
        // Project owners / writers / readers come from the project's roles.
        system: kind == "specialGroup" && id.starts_with("project"),
    }
}

fn principals_of(b: &[Binding]) -> Vec<Principal> {
    let mut names: Vec<&str> = b.iter().map(|b| b.member.as_str()).collect();
    names.sort();
    names.dedup();
    names.into_iter().map(principal).collect()
}

fn grants_of(b: &[Binding], member: &str) -> Vec<Grant> {
    let mut out: Vec<Grant> = b
        .iter()
        .filter(|b| b.member == member)
        .map(|b| Grant {
            privilege: b.role.clone(),
            object: Some(b.object.clone()),
            object_kind: Some(b.kind.to_string()),
            ..Default::default()
        })
        .collect();
    out.sort_by(|a, b| (&a.object_kind, &a.object, &a.privilege).cmp(&(&b.object_kind, &b.object, &b.privilege)));
    out.dedup();
    out
}

pub async fn principals(s: &BigQuerySession) -> Result<Vec<Principal>> {
    Ok(principals_of(&bindings(s).await?))
}

pub async fn grants(s: &BigQuerySession, member: &str) -> Result<Vec<Grant>> {
    Ok(grants_of(&bindings(s).await?, member))
}

// -- scripts ---------------------------------------------------------------

/// IAM role ids: `roles/bigquery.dataViewer`, a custom
/// `projects/p/roles/r`; a bare `dataViewer` is BigQuery's.
fn roles(p: &[String]) -> Result<String> {
    if p.is_empty() {
        return Err(Error::Query("elegí al menos un rol".into()));
    }
    let mut out = Vec::new();
    for x in p {
        let x = x.trim().trim_matches('`');
        if x.is_empty() || !x.chars().all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c)) {
            return Err(Error::Query(format!("«{x}» no es un rol de IAM")));
        }
        let id = if x.contains('/') { x.to_string() } else { format!("roles/bigquery.{x}") };
        out.push(format!("`{id}`"));
    }
    Ok(out.join(", "))
}

/// `SCHEMA \`ds\``, `TABLE \`ds\`.\`t\``, `VIEW …`.
fn target(object: &Option<ObjectRef>) -> Result<String> {
    let o = object
        .as_ref()
        .filter(|o| !o.name.trim().is_empty())
        .ok_or_else(|| Error::Query("en BigQuery los roles se otorgan sobre un dataset, una tabla o una vista".into()))?;
    let name = match o.schema() {
        Some(ds) => format!("{}.{}", ident(ds), ident(&o.name)),
        None => ident(&o.name),
    };
    Ok(match o.kind.as_str() {
        "schema" | "database" | "dataset" => format!("SCHEMA {}", ident(&o.name)),
        kinds::VIEW | kinds::MATERIALIZED_VIEW => format!("VIEW {name}"),
        _ => format!("TABLE {name}"),
    })
}

/// A principal as DCL takes it: `"user:ana@x.com"`.
fn grantee(name: &str) -> Result<String> {
    let name = name.trim();
    match name.split_once(':') {
        Some((kind, id)) if !kind.is_empty() && !id.is_empty() => Ok(lit(name)),
        _ => Err(Error::Query(format!(
            "escribí «{name}» con su tipo: user:…, group:…, serviceAccount:…, domain:… o specialGroup:…"
        ))),
    }
}

fn iam() -> Error {
    Error::Unsupported(
        "en BigQuery los usuarios, grupos y cuentas de servicio son de Google Cloud IAM: se crean, habilitan y agrupan allí, no en BigQuery".into(),
    )
}

pub fn script(a: &SecurityAction) -> Result<String> {
    Ok(match a {
        SecurityAction::Grant { grantable: true, .. } => {
            return Err(Error::Query(
                "BigQuery no permite que un principal otorgue a otros sus roles (no hay WITH GRANT OPTION): para eso, dale roles/bigquery.dataOwner".into(),
            ))
        }
        SecurityAction::Grant { privileges: p, object, to, .. } => format!("GRANT {} ON {} TO {};", roles(p)?, target(object)?, grantee(to)?),
        SecurityAction::Revoke { privileges: p, object, from } => format!("REVOKE {} ON {} FROM {};", roles(p)?, target(object)?, grantee(from)?),
        _ => return Err(iam()),
    })
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
            s(SecurityAction::Grant { privileges: vec!["roles/bigquery.dataViewer".into(), "dataEditor".into()], object: obj("schema", None, "ven`tas"), to: "user:ana@x.com".into(), grantable: false }),
            "GRANT `roles/bigquery.dataViewer`, `roles/bigquery.dataEditor` ON SCHEMA `ven\\`tas` TO 'user:ana@x.com';"
        );
        assert_eq!(
            s(SecurityAction::Grant { privileges: vec!["projects/p-1/roles/lector_x".into()], object: obj("table", Some("ventas"), "f"), to: "group:o'k@x.com".into(), grantable: false }),
            "GRANT `projects/p-1/roles/lector_x` ON TABLE `ventas`.`f` TO 'group:o\\'k@x.com';"
        );
        assert_eq!(
            s(SecurityAction::Revoke { privileges: vec!["roles/bigquery.dataViewer".into()], object: obj("view", None, "v"), from: "specialGroup:allAuthenticatedUsers".into() }),
            "REVOKE `roles/bigquery.dataViewer` ON VIEW `v` FROM 'specialGroup:allAuthenticatedUsers';"
        );
        let bad = |p: &str, o: Option<ObjectRef>, to: &str, grantable| script(&SecurityAction::Grant { privileges: vec![p.into()], object: o, to: to.into(), grantable });
        assert!(bad("roles/x` ON x; --", obj("table", None, "t"), "user:a@b", false).is_err());
        assert!(bad("dataViewer", None, "user:a@b", false).is_err());
        assert!(bad("dataViewer", obj("table", None, "t"), "ana@b.com", false).is_err());
        assert!(bad("dataViewer", obj("table", None, "t"), "user:a@b", true).is_err());
        for a in [
            SecurityAction::CreateUser { name: "a".into(), password: None },
            SecurityAction::CreateRole { name: "a".into() },
            SecurityAction::AddMember { role: "a".into(), member: "b".into() },
        ] {
            assert!(matches!(script(&a), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn reads_access_lists_and_policies() {
        let ds = json!({ "access": [
            { "role": "WRITER", "specialGroup": "projectWriters" },
            { "role": "OWNER", "userByEmail": "ana@x.com" },
            { "role": "READER", "userByEmail": "bot@p.iam.gserviceaccount.com" },
            { "role": "roles/bigquery.dataViewer", "groupByEmail": "equipo@x.com" },
            { "view": { "projectId": "p", "datasetId": "d2", "tableId": "v" } },
        ]});
        let mut b = dataset_bindings("ventas", &ds);
        b.extend(policy_bindings("ventas.f", kinds::TABLE, &json!({ "bindings": [
            { "role": "roles/bigquery.dataViewer", "members": ["user:ana@x.com", "domain:x.com"] },
        ]})));
        let p = principals_of(&b);
        let names: Vec<&str> = p.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["domain:x.com", "group:equipo@x.com", "serviceAccount:bot@p.iam.gserviceaccount.com", "specialGroup:projectWriters", "user:ana@x.com"]);
        assert_eq!(p[0].kind, PrincipalKind::Role);
        assert_eq!(p[2].kind, PrincipalKind::User);
        assert!(p[3].system && !p[4].system);
        let g = grants_of(&b, "user:ana@x.com");
        assert_eq!(g.len(), 2);
        assert_eq!((g[0].privilege.as_str(), g[0].object.as_deref(), g[0].object_kind.as_deref()), ("roles/bigquery.dataOwner", Some("ventas"), Some("schema")));
        assert_eq!((g[1].privilege.as_str(), g[1].object.as_deref(), g[1].object_kind.as_deref()), ("roles/bigquery.dataViewer", Some("ventas.f"), Some("table")));
    }
}
