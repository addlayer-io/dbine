//! Proxies: SSMS's "New Proxy Account" dialog (General: the credential,
//! the subsystems it may run; Principals: the logins and roles that may
//! use it), `sp_add_proxy` / `sp_update_proxy` / `sp_delete_proxy`, the
//! subsystem grants (`sp_grant_proxy_to_subsystem` /
//! `sp_revoke_proxy_from_subsystem`) and the principals'
//! (`sp_grant_login_to_proxy` / `sp_revoke_login_from_proxy`). sysadmin
//! only; Azure SQL Managed Instance has no proxies.

use super::choices::{self, Catalog};
use super::{
    bad, fits, int_id, pick, rows, shown, Ctx, Kind, Plan, Script, Vals, A, SYSADMIN_ONLY_FORM,
    SYSNAME,
};
use crate::jobs::{self, first_with, int, text};
use crate::SqlServerSession;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use serde_json::json;
use std::collections::BTreeMap;

const NOT_FOUND: &str = "El proxy no existe o este usuario no puede verlo.";
const KEEPS_DESCRIPTION: &str =
    "sp_update_proxy no deja vacía la descripción de un proxy: se mantiene la que tiene.";
const GRANTS_HIDDEN: &str =
    "Solo un miembro de sysadmin ve los subsistemas y las entidades de seguridad de un proxy.";

/// A proxy as msdb keeps it.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Proxy {
    pub(crate) name: String,
    pub(crate) credential: String,
    pub(crate) description: String,
    pub(crate) enabled: bool,
    pub(crate) subsystems: Vec<String>,
    pub(crate) logins: Vec<String>,
    pub(crate) msdb_roles: Vec<String>,
    pub(crate) server_roles: Vec<String>,
}

/// Every proxy the login sees (the one picked here: a missing id is an
/// error for the procedures), then, for sysadmin, its credential,
/// subsystems and principals.
const ONE_SQL: &str = "SET NOCOUNT ON;
EXEC msdb.dbo.sp_help_proxy;
IF HAS_PERMS_BY_NAME(N'msdb.dbo.sysproxies', N'OBJECT', N'SELECT') = 1
BEGIN
    IF EXISTS (SELECT 1 FROM msdb.dbo.sysproxies WHERE proxy_id = @P1)
    BEGIN
        SELECT c.name AS dbine_credential FROM msdb.dbo.sysproxies p JOIN sys.credentials c ON c.credential_id = p.credential_id WHERE p.proxy_id = @P1;
        EXEC msdb.dbo.sp_enum_proxy_for_subsystem @proxy_id = @P1;
        EXEC msdb.dbo.sp_enum_login_for_proxy @proxy_id = @P1;
    END;
END;";

struct Read {
    id: i64,
    proxy: Proxy,
    /// The grants were readable.
    grants: bool,
}

async fn read(s: &mut SqlServerSession, id: &str) -> Result<Read> {
    let n = int_id(Kind::Proxy, id)?;
    let ids = n.to_string();
    let sets = jobs::sets(s, ONE_SQL, &[&ids], None).await?;
    let Some(r) = first_with(&sets, "credential_identity_exists")
        .or_else(|| first_with(&sets, "credential_identity"))
        .and_then(|s| s.rows.iter().find(|r| int(r, "proxy_id") == Some(n)))
    else {
        return Err(Error::Query(NOT_FOUND.into()));
    };
    let mut p = Proxy {
        name: text(r, "name").unwrap_or_default(),
        credential: String::new(),
        description: text(r, "description").unwrap_or_default(),
        enabled: int(r, "enabled") == Some(1),
        ..Proxy::default()
    };
    let grants = first_with(&sets, "dbine_credential").is_some();
    if let Some(c) = first_with(&sets, "dbine_credential")
        .and_then(|s| s.rows.first())
        .and_then(|r| text(r, "dbine_credential"))
    {
        p.credential = c;
    }
    if let Some(set) = first_with(&sets, "subsystem_name") {
        p.subsystems = set
            .rows
            .iter()
            .filter_map(|r| text(r, "subsystem_name"))
            .map(|s| super::job::canonical(&s))
            .collect();
    }
    if let Some(set) = first_with(&sets, "principal_id") {
        for r in &set.rows {
            let Some(name) = text(r, "name") else {
                continue;
            };
            match int(r, "flags").unwrap_or(0) {
                1 => p.server_roles.push(name),
                2 => p.msdb_roles.push(name),
                _ => p.logins.push(name),
            }
        }
    }
    Ok(Read {
        id: n,
        proxy: p,
        grants,
    })
}

pub(crate) async fn items(s: &mut SqlServerSession, _c: &Ctx) -> Result<SchedulerList> {
    let sets = jobs::sets(
        s,
        "SET NOCOUNT ON;\nEXEC msdb.dbo.sp_help_proxy;",
        &[],
        None,
    )
    .await?;
    let items = sets
        .iter()
        .find(|s| s.has("proxy_id") && s.has("credential_identity"))
        .map(|set| {
            set.rows
                .iter()
                .filter_map(|r| {
                    Some(SchedulerItem {
                        id: int(r, "proxy_id")?.to_string(),
                        name: text(r, "name")?,
                        description: text(r, "description"),
                        enabled: Some(int(r, "enabled") == Some(1)),
                        system: false,
                        can_edit: None,
                        can_delete: None,
                        actions: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

fn subsystem_kind(available: &[String], current: &[String]) -> FormKind {
    let mut k = FormKind::multi(&[
        ("CmdExec", "Comando del sistema operativo (CmdExec)"),
        ("PowerShell", "PowerShell"),
        ("SSIS", "Paquete de SQL Server Integration Services"),
        ("ANALYSISQUERY", "Consulta de SQL Server Analysis Services"),
        ("ANALYSISCOMMAND", "Comando de SQL Server Analysis Services"),
        ("Snapshot", "Instantánea de replicación"),
        (
            "LogReader",
            "Lector del registro de transacciones de replicación",
        ),
        ("Distribution", "Distribuidor de replicación"),
        ("Merge", "Mezcla de replicación"),
        ("QueueReader", "Lector de cola de replicación"),
        ("ActiveScripting", "Script ActiveX"),
    ]);
    if let FormKind::MultiSelect { options } = &mut k {
        options.retain(|(v, _)| {
            available.is_empty()
                || available.iter().any(|a| a.eq_ignore_ascii_case(v))
                || current.iter().any(|a| a.eq_ignore_ascii_case(v))
        });
    }
    k
}

fn list_rows(key: &str, names: &[String]) -> Vec<BTreeMap<String, serde_json::Value>> {
    names
        .iter()
        .map(|n| BTreeMap::from([(key.to_string(), json!(n))]))
        .collect()
}

pub(crate) async fn form(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
) -> Result<ObjectForm> {
    let catalog = choices::load(
        s,
        c,
        &[
            "credential",
            "login",
            "msdb_role",
            "server_role",
            "subsystem",
        ],
    )
    .await?;
    let mut f = ObjectForm::default();
    let (p, grants) = match id {
        None => (
            Proxy {
                enabled: true,
                ..Proxy::default()
            },
            true,
        ),
        Some(id) => {
            let r = read(s, id).await?;
            (r.proxy, r.grants)
        }
    };
    f.values.insert("name".into(), json!(p.name));
    f.values.insert("credential".into(), json!(p.credential));
    f.values.insert("description".into(), json!(p.description));
    f.values.insert("enabled".into(), json!(p.enabled));
    f.values.insert("subsystems".into(), json!(p.subsystems));
    f.sections = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre del proxy", FormKind::Text).required(),
            FormField::new(
                "credential",
                "Nombre de la credencial",
                FormKind::reference("credential", false),
            )
            .required(),
            FormField::new("description", "Descripción", FormKind::Textarea),
            FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
            FormField::new(
                "subsystems",
                "Activo para estos subsistemas",
                subsystem_kind(&catalog.subsystems(), &p.subsystems),
            ),
        ]),
        FormSection::new("principals", "Entidades de seguridad")
            .list(
                FormList::new(
                    "logins",
                    "Inicios de sesión",
                    "login",
                    vec![FormField::new(
                        "login",
                        "Inicio de sesión",
                        FormKind::reference("login", false),
                    )
                    .required()],
                )
                .rows(list_rows("login", &p.logins)),
            )
            .list(
                FormList::new(
                    "msdb_roles",
                    "Roles de msdb",
                    "role",
                    vec![
                        FormField::new("role", "Rol", FormKind::reference("msdb_role", false))
                            .required(),
                    ],
                )
                .rows(list_rows("role", &p.msdb_roles)),
            )
            .list(
                FormList::new(
                    "server_roles",
                    "Roles de servidor",
                    "role",
                    vec![
                        FormField::new("role", "Rol", FormKind::reference("server_role", false))
                            .required(),
                    ],
                )
                .rows(list_rows("role", &p.server_roles)),
            ),
    ];
    f.choices = catalog.lists;
    if !grants {
        f.notes.push(GRANTS_HIDDEN.into());
    }
    if !c.admin() {
        f.read_only = true;
        if c.writable() {
            f.notes.insert(0, SYSADMIN_ONLY_FORM.into());
        }
    }
    Ok(f)
}

fn names(
    values: &FormValues,
    list: &str,
    field: &str,
    choices: &[(String, String)],
    missing: fn(&str) -> String,
) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for row in rows(values, list) {
        let Some(n) = row.opt(field) else { continue };
        let n = if choices.is_empty() {
            n
        } else {
            pick(&n, choices, missing)?.to_string()
        };
        if !out.iter().any(|o| o.eq_ignore_ascii_case(&n)) {
            out.push(n);
        }
    }
    Ok(out)
}

/// The proxy the form describes, checked.
pub(crate) fn from_values(values: &FormValues, catalog: &Catalog) -> Result<Proxy> {
    let v = Vals(&values.values);
    let name = v
        .opt("name")
        .ok_or_else(|| bad("El proxy necesita un nombre.".into()))?;
    fits("Nombre del proxy", &name, SYSNAME)?;
    let description = v.text("description");
    fits("Descripción", &description, 512)?;
    let credential = v
        .opt("credential")
        .ok_or_else(|| bad("El proxy necesita una credencial.".into()))?;
    let creds = catalog.get("credential");
    let credential = if creds.is_empty() {
        credential
    } else {
        pick(&credential, creds, |c| {
            format!("La credencial «{c}» no existe en este servidor.")
        })?
        .to_string()
    };
    let available = catalog.subsystems();
    let mut subsystems: Vec<String> = Vec::new();
    for sub in v.strings("subsystems") {
        let canon = super::job::canonical(&sub);
        let known = [
            "CmdExec",
            "PowerShell",
            "SSIS",
            "ANALYSISQUERY",
            "ANALYSISCOMMAND",
            "Snapshot",
            "LogReader",
            "Distribution",
            "Merge",
            "QueueReader",
            "ActiveScripting",
        ]
        .contains(&canon.as_str());
        if !known
            || !(available.is_empty() || available.iter().any(|a| a.eq_ignore_ascii_case(&canon)))
        {
            return Err(bad(format!(
                "El subsistema «{}» no está disponible en este servidor.",
                shown(&sub)
            )));
        }
        if !subsystems.contains(&canon) {
            subsystems.push(canon);
        }
    }
    Ok(Proxy {
        name,
        credential,
        description,
        enabled: if v.0.get("enabled").is_some_and(|x| !x.is_null()) {
            v.bool("enabled")
        } else {
            true
        },
        subsystems,
        logins: names(values, "logins", "login", catalog.get("login"), |n| {
            format!("«{n}» no es un inicio de sesión de este servidor.")
        })?,
        msdb_roles: names(
            values,
            "msdb_roles",
            "role",
            catalog.get("msdb_role"),
            |n| format!("«{n}» no es un rol de msdb."),
        )?,
        server_roles: names(
            values,
            "server_roles",
            "role",
            catalog.get("server_role"),
            |n| format!("«{n}» no es un rol de servidor."),
        )?,
    })
}

/// A Windows login or group: msdb impersonates it to check sysadmin,
/// which a group can't be (see [`Script::call_after_commit`]).
fn windows(name: &str) -> bool {
    name.contains('\\')
}

fn grant(sc: &mut Script, proxy: &A, kind: &'static str, name: &str) {
    let args = vec![(kind, A::T(name.into())), ("proxy_id", proxy.clone())];
    if kind == "login_name" && windows(name) {
        sc.call_after_commit("msdb.dbo.sp_grant_login_to_proxy", args);
    } else {
        sc.call("msdb.dbo.sp_grant_login_to_proxy", args);
    }
}

fn grants(sc: &mut Script, proxy: A, p: &Proxy, cur: &Proxy) {
    let missing = |list: &[String], x: &String| !list.iter().any(|y| y.eq_ignore_ascii_case(x));
    for sub in cur.subsystems.iter().filter(|s| missing(&p.subsystems, s)) {
        sc.call(
            "msdb.dbo.sp_revoke_proxy_from_subsystem",
            vec![
                ("proxy_id", proxy.clone()),
                ("subsystem_name", A::T(sub.clone())),
            ],
        );
    }
    for sub in p.subsystems.iter().filter(|s| missing(&cur.subsystems, s)) {
        sc.call(
            "msdb.dbo.sp_grant_proxy_to_subsystem",
            vec![
                ("proxy_id", proxy.clone()),
                ("subsystem_name", A::T(sub.clone())),
            ],
        );
    }
    // msdb checks whether the principal is sysadmin by impersonating it,
    // which a role or a Windows group can't be: inside the transaction
    // that dooms it, so those go after the commit.
    for (want, have, login) in [
        (&p.logins, &cur.logins, true),
        (&p.msdb_roles, &cur.msdb_roles, false),
        (&p.server_roles, &cur.server_roles, false),
    ] {
        for n in have.iter().filter(|n| missing(want, n)) {
            let args = vec![("name", A::T(n.clone())), ("proxy_id", proxy.clone())];
            if login && !windows(n) {
                sc.call("msdb.dbo.sp_revoke_login_from_proxy", args);
            } else {
                sc.call_after_commit("msdb.dbo.sp_revoke_login_from_proxy", args);
            }
        }
    }
    for (kind, want, have) in [
        ("login_name", &p.logins, &cur.logins),
        ("msdb_role", &p.msdb_roles, &cur.msdb_roles),
        ("fixed_server_role", &p.server_roles, &cur.server_roles),
    ] {
        for n in want.iter().filter(|n| missing(have, n)) {
            grant(sc, &proxy, kind, n);
        }
    }
}

pub(crate) fn create_script(p: &Proxy) -> String {
    let mut sc = Script::new(
        &format!("Proxy «{}» del Agente SQL Server", p.name),
        &["@proxyId int"],
    );
    let mut args = vec![
        ("proxy_name", A::T(p.name.clone())),
        ("credential_name", A::T(p.credential.clone())),
        ("enabled", A::I(i64::from(p.enabled))),
    ];
    if !p.description.is_empty() {
        args.push(("description", A::T(p.description.clone())));
    }
    args.push(("proxy_id", A::R("@proxyId OUTPUT".into())));
    sc.call("msdb.dbo.sp_add_proxy", args);
    grants(&mut sc, A::R("@proxyId".into()), p, &Proxy::default());
    sc.finish(Some("SELECT @proxyId AS proxy_id;"))
}

/// `sp_update_proxy` with what changed and the grants' diff, or `None`.
pub(crate) fn alter_script(id: i64, cur: &Proxy, p: &Proxy, grants_known: bool) -> Option<String> {
    let mut sc = Script::new(&format!("Proxy «{}» del Agente SQL Server", cur.name), &[]);
    let mut args = vec![("proxy_id", A::I(id))];
    if p.name != cur.name {
        args.push(("new_name", A::T(p.name.clone())));
    }
    if !p.credential.eq_ignore_ascii_case(&cur.credential) {
        args.push(("credential_name", A::T(p.credential.clone())));
    }
    if p.enabled != cur.enabled {
        args.push(("enabled", A::I(i64::from(p.enabled))));
    }
    if p.description != cur.description {
        if p.description.is_empty() {
            sc.comment(KEEPS_DESCRIPTION);
        } else {
            args.push(("description", A::T(p.description.clone())));
        }
    }
    if args.len() > 1 {
        sc.call("msdb.dbo.sp_update_proxy", args);
    }
    if grants_known {
        grants(&mut sc, A::I(id), p, cur);
    }
    (sc.calls() > 0).then(|| sc.finish(None))
}

pub(crate) fn drop_script(id: i64, name: &str) -> String {
    let mut sc = Script::new(
        &format!("Borra el proxy «{name}» del Agente SQL Server"),
        &[],
    );
    sc.call("msdb.dbo.sp_delete_proxy", vec![("proxy_id", A::I(id))]);
    sc.finish(None)
}

pub(crate) async fn plan(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    if !c.p.sysadmin {
        return Err(Error::Query(SYSADMIN_ONLY_FORM.into()));
    }
    let catalog = choices::load(
        s,
        c,
        &[
            "credential",
            "login",
            "msdb_role",
            "server_role",
            "subsystem",
        ],
    )
    .await?;
    let p = from_values(values, &catalog)?;
    match id {
        None => Ok(Plan {
            script: create_script(&p),
            empty: false,
            id: None,
            output: Some("proxy_id"),
            lookup: None,
        }),
        Some(id) => {
            let r = read(s, id).await?;
            Ok(match alter_script(r.id, &r.proxy, &p, r.grants) {
                Some(script) => Plan {
                    script,
                    empty: false,
                    id: Some(r.id.to_string()),
                    output: None,
                    lookup: None,
                },
                None => Plan::nothing(&r.proxy.name, r.id.to_string()),
            })
        }
    }
}

pub(crate) async fn delete_script(s: &mut SqlServerSession, c: &Ctx, id: &str) -> Result<String> {
    if !c.p.sysadmin && c.writable() {
        return Err(Error::Query(SYSADMIN_ONLY_FORM.into()));
    }
    let r = read(s, id).await?;
    Ok(drop_script(r.id, &r.proxy.name))
}
