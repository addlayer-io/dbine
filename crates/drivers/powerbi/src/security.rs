//! Users and permissions of a model (spec §4.6, decision D4). Owned by
//! part C. Changes are the driver's own commands ([`crate::admin`]).
//!
//! - `principals`: workspace users (`GET /groups/{g}/users`: Admin, Member,
//!   Contributor, Viewer; User, Group, App), model users (`GET
//!   …/datasets/{d}/users`, needs ReadWriteReshare), RLS roles with their
//!   members (Tier A admin, Tier B); each part skipped with a note when not
//!   readable.
//! - `grants`: workspace role, model rights (`Read`, `Build` =
//!   `ReadExplore`, `Reshare`, `Write`), RLS role membership (`via`).
//! - `privileges`: the documented model rights.
//! - `permissions`: backup, restore, profiler, kill_session,
//!   create_database, create_schema: Denied with the reason;
//!   `drop_database` and `manage_security`: allowed only on connections
//!   that aren't read-only for users who may (write permission / workspace
//!   role), else Denied with the reason.
//!
//! Power BI keeps one combined right per principal and model
//! (`datasetUserAccessRight`: `Read`, `ReadExplore`, `ReadReshareExplore`,
//! `ReadWrite`…): [`rights_of`] takes it apart and [`combined`] puts it
//! back together, in the API's order (Read, Write, Reshare, Explore).

use crate::catalog::Model;
use crate::dax::quote::column_ref;
use crate::http::Priority;
use crate::meta::service::{named_model, session_model, Service};
use crate::Ctx;
use dbine_driver::{
    Access, Error, Grant, Permissions, Principal, PrincipalKind, Privilege, Result, SecuritySpec,
};
use serde_json::Value;

/// What the Users and permissions tab offers: per model, grants of the
/// model's rights to a principal (no users, roles or passwords created
/// here: principals are Entra identities). `Write` isn't offered: the API
/// neither grants nor revokes it (a workspace role gives it); `grants`
/// still shows it.
pub fn spec() -> SecuritySpec {
    SecuritySpec {
        privileges: vec!["Read", "Build", "Reshare"],
        object_kinds: vec![""],
        create_user: false,
        create_role: false,
        passwords: false,
        membership: false,
        per_database: true,
    }
}

/// The model rights, in the order the tab shows them.
pub const RIGHTS: &[&str] = &["Read", "Build", "Reshare", "Write"];

/// `ReadWriteReshareExplore` → `["Read", "Build", "Reshare", "Write"]`
/// (`Explore` is Build); `None` → nothing.
pub fn rights_of(combined: &str) -> Vec<&'static str> {
    let c = combined.to_ascii_lowercase();
    if c == "none" || c.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if c.contains("read") {
        out.push("Read");
    }
    if c.contains("explore") {
        out.push("Build");
    }
    if c.contains("reshare") {
        out.push("Reshare");
    }
    if c.contains("write") {
        out.push("Write");
    }
    out
}

/// The API's combined value of a set of rights: `Read` + `Write` +
/// `Reshare` + `Explore` (Build), or `None` without Read.
pub fn combined(rights: &[&str]) -> String {
    let has = |r: &str| rights.iter().any(|x| x.eq_ignore_ascii_case(r));
    if !has("Read") {
        return "None".into();
    }
    let mut s = String::from("Read");
    if has("Write") {
        s.push_str("Write");
    }
    if has("Reshare") {
        s.push_str("Reshare");
    }
    if has("Build") {
        s.push_str("Explore");
    }
    s
}

/// The rights in Spanish, for details and messages.
pub fn rights_text(rights: &[&str]) -> String {
    if rights.is_empty() {
        return "ninguno".into();
    }
    rights
        .iter()
        .map(|r| match *r {
            "Read" => "lectura",
            "Build" => "compilación",
            "Reshare" => "volver a compartir",
            "Write" => "escritura",
            other => other,
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A workspace role in Spanish.
pub fn role_text(role: &str) -> &str {
    match role {
        "Admin" => "Administrador",
        "Member" => "Miembro",
        "Contributor" => "Colaborador",
        "Viewer" => "Lector",
        other => other,
    }
}

fn kind_text(t: &str) -> &str {
    match t {
        "User" => "Usuario",
        "Group" => "Grupo",
        "App" => "Entidad de servicio",
        other => other,
    }
}

/// One entry of a users listing (`value[]` of the group or dataset users).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub identifier: String,
    pub principal_type: String,
    /// `groupUserAccessRight` / `datasetUserAccessRight`.
    pub right: String,
    pub display_name: Option<String>,
}

pub fn entries(v: &Value, right_key: &str) -> Vec<Entry> {
    v.get("value")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let s = |k: &str| e.get(k).and_then(Value::as_str).map(str::to_string);
                    Some(Entry {
                        identifier: s("identifier")?,
                        principal_type: s("principalType").unwrap_or_else(|| "User".into()),
                        right: s(right_key).unwrap_or_default(),
                        display_name: s("displayName"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) async fn workspace_users<S: Service>(s: &S, m: &Model) -> Result<Vec<Entry>> {
    let Some(g) = &m.workspace.id else {
        return Ok(Vec::new());
    };
    let v = s
        .get(&format!("/groups/{g}/users"), Priority::Background)
        .await?;
    Ok(entries(&v, "groupUserAccessRight"))
}

pub(crate) async fn model_users<S: Service>(
    s: &S,
    m: &Model,
    priority: Priority,
) -> Result<Vec<Entry>> {
    let v = s.get(&format!("{}/users", m.path()), priority).await?;
    Ok(entries(&v, "datasetUserAccessRight"))
}

fn fatal(e: Error) -> Result<()> {
    if crate::meta::is_fatal(&e) {
        Err(e)
    } else {
        Ok(())
    }
}

pub async fn principals(cx: &mut Ctx) -> Result<Vec<Principal>> {
    principals_in(cx).await
}

fn find<'a>(list: &'a mut [Principal], id: &str) -> Option<&'a mut Principal> {
    list.iter_mut().find(|p| p.name.eq_ignore_ascii_case(id))
}

pub(crate) async fn principals_in<S: Service>(s: &mut S) -> Result<Vec<Principal>> {
    let m = session_model(s)?;
    let mut out: Vec<Principal> = Vec::new();
    match workspace_users(s, &m).await {
        Ok(users) => {
            for u in users {
                let mut details = Vec::new();
                if let Some(n) = &u.display_name {
                    details.push(("Nombre".to_string(), n.clone()));
                }
                details.push(("Tipo".into(), kind_text(&u.principal_type).into()));
                details.push((
                    "Rol en el área de trabajo".into(),
                    role_text(&u.right).into(),
                ));
                out.push(Principal {
                    name: u.identifier,
                    kind: if u.principal_type == "Group" {
                        PrincipalKind::Role
                    } else {
                        PrincipalKind::User
                    },
                    can_login: Some(u.principal_type != "Group"),
                    superuser: Some(u.right == "Admin"),
                    disabled: None,
                    member_of: Vec::new(),
                    details,
                    system: false,
                });
            }
        }
        Err(e) => fatal(e)?,
    }
    match model_users(s, &m, Priority::Background).await {
        Ok(users) => {
            for u in users {
                let rights = rights_text(&rights_of(&u.right));
                match find(&mut out, &u.identifier) {
                    Some(p) => p.details.push(("Permiso en el modelo".into(), rights)),
                    None => out.push(Principal {
                        name: u.identifier.clone(),
                        kind: if u.principal_type == "Group" {
                            PrincipalKind::Role
                        } else {
                            PrincipalKind::User
                        },
                        can_login: Some(u.principal_type != "Group"),
                        superuser: Some(false),
                        disabled: None,
                        member_of: Vec::new(),
                        details: vec![
                            ("Tipo".into(), kind_text(&u.principal_type).into()),
                            ("Permiso en el modelo".into(), rights),
                        ],
                        system: false,
                    }),
                }
            }
        }
        Err(e) => fatal(e)?,
    }
    if let Ok(meta) = crate::meta::model_for(s, &m).await {
        for r in &meta.roles {
            for member in &r.members {
                match find(&mut out, member) {
                    Some(p) => p.member_of.push(r.name.clone()),
                    None => out.push(Principal {
                        name: member.clone(),
                        kind: PrincipalKind::User,
                        can_login: Some(true),
                        superuser: None,
                        disabled: None,
                        member_of: vec![r.name.clone()],
                        details: vec![(
                            "Origen".into(),
                            "miembro de roles de seguridad (RLS)".into(),
                        )],
                        system: false,
                    }),
                }
            }
            let mut details = vec![("Tipo".to_string(), "Rol de seguridad (RLS)".to_string())];
            if let Some(d) = &r.description {
                details.push(("Descripción".into(), d.clone()));
            }
            details.push(("Miembros".into(), r.members.len().to_string()));
            details.push(("Filtros".into(), r.table_filters.len().to_string()));
            out.push(Principal {
                name: r.name.clone(),
                kind: PrincipalKind::Role,
                can_login: Some(false),
                superuser: Some(r.model_permission.as_deref() == Some("administrator")),
                disabled: None,
                member_of: Vec::new(),
                details,
                system: false,
            });
        }
    }
    Ok(out)
}

pub async fn grants(cx: &mut Ctx, principal: &str) -> Result<Vec<Grant>> {
    grants_in(cx, principal).await
}

pub(crate) async fn grants_in<S: Service>(s: &mut S, principal: &str) -> Result<Vec<Grant>> {
    let m = session_model(s)?;
    let mut out = Vec::new();
    match workspace_users(s, &m).await {
        Ok(users) => {
            if let Some(u) = users
                .iter()
                .find(|u| u.identifier.eq_ignore_ascii_case(principal))
            {
                out.push(Grant {
                    privilege: u.right.clone(),
                    object: Some(m.workspace.name.clone()),
                    object_kind: Some("workspace".into()),
                    grantable: matches!(u.right.as_str(), "Admin" | "Member"),
                    denied: false,
                    via: None,
                });
            }
        }
        Err(e) => fatal(e)?,
    }
    match model_users(s, &m, Priority::Background).await {
        Ok(users) => {
            if let Some(u) = users
                .iter()
                .find(|u| u.identifier.eq_ignore_ascii_case(principal))
            {
                let rights = rights_of(&u.right);
                let reshare = rights.contains(&"Reshare");
                for r in rights {
                    out.push(Grant {
                        privilege: r.into(),
                        object: None,
                        object_kind: None,
                        grantable: reshare,
                        denied: false,
                        via: None,
                    });
                }
            }
        }
        Err(e) => fatal(e)?,
    }
    if let Ok(meta) = crate::meta::model_for(s, &m).await {
        for role in &meta.roles {
            let own = role.name.eq_ignore_ascii_case(principal);
            let member = role
                .members
                .iter()
                .any(|x| x.eq_ignore_ascii_case(principal));
            if !own && !member {
                continue;
            }
            let via = (!own).then(|| role.name.clone());
            for (t, f) in &role.table_filters {
                out.push(Grant {
                    privilege: format!("Filtro de filas: {}", f.trim()),
                    object: Some(t.clone()),
                    object_kind: Some(crate::TABLE.into()),
                    grantable: false,
                    denied: false,
                    via: via.clone(),
                });
            }
            for (t, c, p) in &role.column_permissions {
                out.push(Grant {
                    privilege: if p.eq_ignore_ascii_case("none") {
                        "Sin acceso".into()
                    } else {
                        p.clone()
                    },
                    object: Some(column_ref(t, c)),
                    object_kind: Some("column".into()),
                    grantable: false,
                    denied: p.eq_ignore_ascii_case("none"),
                    via: via.clone(),
                });
            }
            if member && role.table_filters.is_empty() && role.column_permissions.is_empty() {
                out.push(Grant {
                    privilege: "Miembro del rol".into(),
                    object: Some(role.name.clone()),
                    object_kind: Some(crate::ROLE.into()),
                    grantable: false,
                    denied: false,
                    via: via.clone(),
                });
            }
        }
    }
    Ok(out)
}

pub async fn privileges(cx: &mut Ctx) -> Result<Vec<Privilege>> {
    let _ = cx;
    Ok(spec()
        .privileges
        .iter()
        .map(|p| Privilege::new(*p, &[""]))
        .collect())
}

pub async fn permissions(cx: &mut Ctx, database: Option<&str>) -> Result<Permissions> {
    permissions_in(cx, database).await
}

pub(crate) async fn permissions_in<S: Service>(
    s: &mut S,
    database: Option<&str>,
) -> Result<Permissions> {
    let denied = |m: &str| Access::Denied { missing: m.into() };
    let mut p = Permissions {
        backup: denied("XMLA (las copias de seguridad de un modelo no están en la API REST)"),
        restore: denied("XMLA (las restauraciones no están en la API REST)"),
        profiler: denied("XMLA (las trazas no están en la API REST)"),
        kill_session: denied("XMLA (la API REST no lista sesiones)"),
        create_database: denied(
            "Power BI Desktop o Fabric (los modelos se publican, no se crean desde DBine)",
        ),
        create_schema: denied("esquemas (un modelo no los tiene)"),
        drop_database: Access::Unknown,
        manage_security: Access::Unknown,
    };
    let read_only = s.settings().read_only;
    if read_only {
        p.drop_database = denied("una conexión que no sea de solo lectura");
        p.manage_security = denied("una conexión que no sea de solo lectura");
        return Ok(p);
    }
    let m = match database {
        Some(d) => named_model(s, d).await,
        None => session_model(s),
    };
    let Ok(m) = m else {
        return Ok(p);
    };
    p.drop_database = Access::check(m.write_permission, "permiso de escritura en el modelo");
    p.manage_security = match model_users(s, &m, Priority::Background).await {
        Ok(_) => Access::Allowed,
        Err(e) if crate::meta::is_fatal(&e) => return Err(e),
        Err(_) => denied("ReadWriteReshare en el modelo"),
    };
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;
    use crate::meta::fake::{Fake, GROUP};

    const DATASET_USERS: &str = include_str!("../tests/fixtures/rest_dataset_users.json");
    const GROUP_USERS: &str = include_str!("../tests/fixtures/rest_group_users.json");

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn rights_round_trip() {
        for c in [
            "Read",
            "ReadWrite",
            "ReadReshare",
            "ReadExplore",
            "ReadWriteReshare",
            "ReadWriteExplore",
            "ReadReshareExplore",
            "ReadWriteReshareExplore",
        ] {
            assert_eq!(combined(&rights_of(c)), c);
        }
        assert_eq!(combined(&["Build", "Reshare"]), "None");
        assert!(rights_of("None").is_empty());
        assert_eq!(
            rights_text(&rights_of("ReadExplore")),
            "lectura, compilación"
        );
    }

    fn routes(f: &Fake) {
        let m = f.model.clone().unwrap();
        f.route(
            Method::Get,
            &format!("/groups/{GROUP}/users"),
            200,
            v(GROUP_USERS),
        );
        f.route(
            Method::Get,
            &format!("{}/users", m.path()),
            200,
            v(DATASET_USERS),
        );
    }

    #[tokio::test]
    async fn principals_join_the_workspace_the_model_and_the_roles() {
        let mut f = Fake::writer();
        routes(&f);
        f.definition(crate::definition::tmsl::tests::SAMPLE);
        let p = principals_in(&mut f).await.unwrap();
        let john = p.iter().find(|x| x.name == "john@contoso.com").unwrap();
        assert_eq!(john.superuser, Some(true));
        assert!(john
            .details
            .contains(&("Rol en el área de trabajo".into(), "Administrador".into())));
        assert!(john
            .details
            .contains(&("Permiso en el modelo".into(), "lectura".into())));
        let group = p
            .iter()
            .find(|x| x.name == "154aef10-47b8-48c4-ab97-f0bf9d5f8fcf")
            .unwrap();
        assert_eq!(group.kind, PrincipalKind::Role);
        let ana = p.iter().find(|x| x.name == "ana@contoso.com").unwrap();
        assert_eq!(ana.member_of, ["West"]);
        let west = p.iter().find(|x| x.name == "West").unwrap();
        assert_eq!(west.kind, PrincipalKind::Role);
        // Grants: workspace role, model rights, and the role's filters.
        let g = grants_in(&mut f, "JOHN@contoso.com").await.unwrap();
        assert_eq!(g[0].privilege, "Admin");
        assert_eq!(g[0].object_kind.as_deref(), Some("workspace"));
        assert_eq!(g[1].privilege, "Read");
        let g = grants_in(&mut f, "3d9b93c6-7b6d-4801-a491-1738910904fd")
            .await
            .unwrap();
        assert!(g.iter().any(|x| x.privilege == "Write"));
        assert!(g.iter().any(|x| x.privilege == "Build" && x.grantable));
        let g = grants_in(&mut f, "ana@contoso.com").await.unwrap();
        assert!(g
            .iter()
            .any(|x| x.via.as_deref() == Some("West") && x.object.as_deref() == Some("Customer")));
        let hidden = g
            .iter()
            .find(|x| x.object.as_deref() == Some("'Customer'[Cost]"))
            .unwrap();
        assert!(hidden.denied);
    }

    #[tokio::test]
    async fn unreadable_parts_are_skipped() {
        let mut f = Fake::new();
        // Nothing answers: an empty list, not an error.
        let p = principals_in(&mut f).await.unwrap();
        assert!(p.is_empty());
    }

    #[tokio::test]
    async fn permissions_follow_the_connection_and_the_rights() {
        let mut f = Fake::new();
        let p = permissions_in(&mut f, Some("Ventas")).await.unwrap();
        assert!(p.drop_database.is_denied() && p.manage_security.is_denied());
        assert!(p.backup.is_denied() && p.profiler.is_denied() && p.create_database.is_denied());
        assert!(f.requests().is_empty());
        let mut f = Fake::writer();
        routes(&f);
        let p = permissions_in(&mut f, Some("Ventas")).await.unwrap();
        assert_eq!(p.drop_database, Access::Allowed);
        assert_eq!(p.manage_security, Access::Allowed);
        let mut f = Fake::writer();
        f.model.as_mut().unwrap().write_permission = false;
        let p = permissions_in(&mut f, None).await.unwrap();
        assert!(p.drop_database.is_denied() && p.manage_security.is_denied());
        assert_eq!(spec().privileges, ["Read", "Build", "Reshare"]);
    }
}
