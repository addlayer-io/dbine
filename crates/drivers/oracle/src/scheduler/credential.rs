//! Credentials: `DBMS_CREDENTIAL.CREATE_CREDENTIAL` / `UPDATE_CREDENTIAL`
//! / `ENABLE_CREDENTIAL` / `DISABLE_CREDENTIAL` / `DROP_CREDENTIAL` from
//! 12c, `DBMS_SCHEDULER`'s calls before. The password is never read back
//! (Oracle doesn't show it): it's written only when the user types one,
//! bound in the block that runs and masked in the script.

use super::choices::{self, Catalog};
use super::read::{self, Rec};
use super::{
    bad, fingerprint, fits, fresh, new_name, new_owner, obj_id, parse_obj, Block, Ctx, Kind, Plan,
    Vals, A, OWNER_ONLY,
};
use dbine_driver::{
    FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem, SchedulerList,
};
use oracledb::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const ROLES: [&str; 7] = [
    "",
    "SYSDBA",
    "SYSOPER",
    "SYSBACKUP",
    "SYSDG",
    "SYSKM",
    "SYSRAC",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialSpec {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) username: String,
    /// Only when typed: `None` keeps what the server has.
    pub(crate) password: Option<String>,
    pub(crate) windows_domain: Option<String>,
    pub(crate) database_role: Option<String>,
    pub(crate) comments: Option<String>,
    pub(crate) enabled: bool,
}

impl CredentialSpec {
    fn id(&self) -> String {
        obj_id(Some(&self.owner), &self.name)
    }
}

fn view(ctx: &Ctx) -> String {
    ctx.sql(if ctx.credential_api() {
        "{v}_credentials"
    } else {
        "{v}_scheduler_credentials"
    })
}

fn read_state(c: &Connection, ctx: &Ctx, owner: &str, name: &str) -> Result<CredentialSpec> {
    let r = read::row(
        c,
        &view(ctx),
        "WHERE owner = :1 AND credential_name = :2",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| {
        bad(format!(
            "no existe la credencial «{}» o este usuario no la puede ver",
            obj_id(Some(owner), name)
        ))
    })?;
    // The database role is only in the Scheduler's (older) view.
    let role = if r.0.contains_key("DATABASE_ROLE") {
        r.t("DATABASE_ROLE")
    } else {
        read::row(
            c,
            &ctx.sql("{v}_scheduler_credentials"),
            "WHERE owner = :1 AND credential_name = :2",
            &[&owner, &name],
        )
        .ok()
        .flatten()
        .and_then(|x: Rec| x.t("DATABASE_ROLE"))
    };
    Ok(CredentialSpec {
        owner: owner.to_string(),
        name: name.to_string(),
        username: r.raw("USERNAME").unwrap_or_default(),
        password: None,
        windows_domain: r.t("WINDOWS_DOMAIN"),
        database_role: role,
        comments: r.t("COMMENTS"),
        enabled: !r.0.contains_key("ENABLED") || r.yes("ENABLED"),
    })
}

pub(crate) fn values_of(s: &CredentialSpec) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("owner".to_string(), json!(s.owner)),
        ("name".to_string(), json!(s.name)),
        ("username".to_string(), json!(s.username)),
        (
            "windows_domain".to_string(),
            json!(s.windows_domain.clone().unwrap_or_default()),
        ),
        (
            "database_role".to_string(),
            json!(s.database_role.clone().unwrap_or_default()),
        ),
        (
            "comments".to_string(),
            json!(s.comments.clone().unwrap_or_default()),
        ),
        ("enabled".to_string(), json!(s.enabled)),
    ])
}

pub(crate) fn spec_of(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&CredentialSpec>,
) -> Result<CredentialSpec> {
    let v = Vals(&values.values);
    let (owner, name) = match cur {
        Some(s) => (s.owner.clone(), s.name.clone()),
        None => (
            new_owner(v, ctx, Kind::Credential, cat)?,
            new_name(&v.raw("name"), ctx, "Nombre")?,
        ),
    };
    let username = v.text("username");
    if username.is_empty() {
        return Err(bad("Falta «Usuario».".into()));
    }
    fits(&Some(username.clone()), "Usuario", 128)?;
    let raw = v.raw("password");
    let password = (!raw.is_empty()).then_some(raw);
    if cur.is_none() && password.is_none() {
        return Err(bad("Falta «Contraseña».".into()));
    }
    fits(&password, "Contraseña", 1024)?;
    let role = v.choice("database_role", "Rol de la base", &ROLES, "")?;
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(CredentialSpec {
        owner,
        name,
        username,
        password,
        windows_domain: v.opt("windows_domain"),
        database_role: Some(role).filter(|r| !r.is_empty()),
        comments,
        enabled: v.bool_or("enabled", true),
    })
}

pub(crate) fn create_block(s: &CredentialSpec, ctx: &Ctx) -> Block {
    let id = s.id();
    let mut b = Block::new(&format!("Crea la credencial {id}."));
    let mut args = vec![
        ("credential_name", A::Name(id)),
        ("username", A::S(s.username.clone())),
        (
            "password",
            A::Secret(s.password.clone().unwrap_or_default()),
        ),
    ];
    if let Some(r) = &s.database_role {
        args.push(("database_role", A::S(r.clone())));
    }
    if let Some(d) = &s.windows_domain {
        args.push(("windows_domain", A::S(d.clone())));
    }
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    if ctx.credential_api() {
        args.push(("enabled", A::B(s.enabled)));
        b.call("DBMS_CREDENTIAL.CREATE_CREDENTIAL", args);
    } else {
        b.call("DBMS_SCHEDULER.CREATE_CREDENTIAL", args);
    }
    b
}

pub(crate) fn alter_block(old: &CredentialSpec, new: &CredentialSpec, ctx: &Ctx) -> Block {
    let id = old.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia la credencial {id}."));
    let changes: [(&str, Option<A>); 5] = [
        (
            "username",
            (old.username != new.username).then(|| A::S(new.username.clone())),
        ),
        ("password", new.password.clone().map(A::Secret)),
        (
            "windows_domain",
            (old.windows_domain != new.windows_domain).then(|| A::opt(new.windows_domain.clone())),
        ),
        (
            "database_role",
            (old.database_role != new.database_role).then(|| A::opt(new.database_role.clone())),
        ),
        (
            "comments",
            (old.comments != new.comments).then(|| A::opt(new.comments.clone())),
        ),
    ];
    for (attr, value) in changes {
        let Some(value) = value else { continue };
        if ctx.credential_api() {
            b.call(
                "DBMS_CREDENTIAL.UPDATE_CREDENTIAL",
                vec![
                    ("credential_name", target.clone()),
                    ("attribute", A::S(attr.into())),
                    ("value", value),
                ],
            );
        } else {
            b.set_attr(&target, attr, value);
        }
    }
    if ctx.credential_api() && old.enabled != new.enabled {
        if new.enabled {
            b.call(
                "DBMS_CREDENTIAL.ENABLE_CREDENTIAL",
                vec![("credential_name", target)],
            );
        } else {
            b.call(
                "DBMS_CREDENTIAL.DISABLE_CREDENTIAL",
                vec![("credential_name", target), ("force", A::B(true))],
            );
        }
    }
    b
}

pub(crate) fn items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &view(ctx),
        "ORDER BY owner, credential_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let owner = r.t("OWNER").unwrap_or_default();
                let name = r.raw("CREDENTIAL_NAME").unwrap_or_default();
                SchedulerItem {
                    id: obj_id(Some(&owner), &name),
                    name: super::item_name(ctx, &owner, &name),
                    description: r
                        .t("COMMENTS")
                        .or_else(|| r.t("USERNAME").map(|u| format!("Usuario {u}"))),
                    enabled: r.0.contains_key("ENABLED").then(|| r.yes("ENABLED")),
                    system: ctx.oracle_schema(&owner),
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

pub(crate) fn plan(
    c: &Connection,
    ctx: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let cat = choices::load(c, ctx, &["schema"]);
    match id {
        None => {
            let s = spec_of(values, ctx, &cat, None)?;
            Ok(Plan::new(create_block(&s, ctx), s.id(), &s.name))
        }
        Some(id) => {
            let (owner, name) = parse_obj(id, "credencial")?;
            let old = read_state(c, ctx, &owner, &name)?;
            if !ctx.may_change(Kind::Credential, &owner, &name, ctx.oracle_schema(&owner)) {
                return Err(bad(OWNER_ONLY.into()));
            }
            fresh(values, &fingerprint(&values_of(&old), &BTreeMap::new()))?;
            let new = spec_of(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(alter_block(&old, &new, ctx), old.id(), &name))
        }
    }
}

pub(crate) fn delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "credencial")?;
    let s = read_state(c, ctx, &owner, &name)?;
    if !ctx.may_change(Kind::Credential, &owner, &name, ctx.oracle_schema(&owner)) {
        return Err(bad(OWNER_ONLY.into()));
    }
    let mut b = Block::new(&format!("Borra la credencial {}.", s.id()));
    b.note("Sin force, Oracle no la borra mientras un job, un programa o un observador la use.");
    b.call(
        if ctx.credential_api() {
            "DBMS_CREDENTIAL.DROP_CREDENTIAL"
        } else {
            "DBMS_SCHEDULER.DROP_CREDENTIAL"
        },
        vec![("credential_name", A::Name(s.id())), ("force", A::B(false))],
    );
    Ok(b)
}

pub(crate) fn form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &["schema"]);
    let state = match id {
        Some(id) => {
            let (o, n) = parse_obj(id, "credencial")?;
            Some(read_state(c, ctx, &o, &n)?)
        }
        None => None,
    };
    let existing = state.is_some();
    let mut values = match &state {
        Some(s) => {
            let mut v = values_of(s);
            let fp = fingerprint(&v, &BTreeMap::new());
            v.insert("dbine_state".into(), json!(fp));
            v
        }
        None => BTreeMap::from([
            ("owner".to_string(), json!(ctx.user())),
            ("enabled".to_string(), json!(true)),
            ("database_role".to_string(), json!("")),
        ]),
    };
    values.insert("password".into(), Value::Null);
    let mut fields = vec![
        FormField::new("owner", "Esquema", FormKind::reference("schema", false))
            .read_only(existing || !ctx.create_any(Kind::Credential))
            .default(json!(ctx.user())),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new("username", "Usuario", FormKind::Text)
            .required()
            .help("Del sistema operativo, o de la base para un destino de base de datos o un script de SQL*Plus."),
        FormField::new("password", "Contraseña", FormKind::Secret).help(
            "Oracle no la muestra: escribí una solo para cambiarla.",
        ),
        FormField::new("windows_domain", "Dominio de Windows", FormKind::Text),
        FormField::new(
            "database_role",
            "Rol de la base",
            FormKind::select(&[
                ("", "Ninguno"),
                ("SYSDBA", "SYSDBA"),
                ("SYSOPER", "SYSOPER"),
                ("SYSBACKUP", "SYSBACKUP"),
                ("SYSDG", "SYSDG"),
                ("SYSKM", "SYSKM"),
                ("SYSRAC", "SYSRAC"),
            ]),
        )
        .help("Para conectarse con ese privilegio administrativo (scripts de RMAN o SQL*Plus)."),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ];
    if ctx.credential_api() {
        fields.push(FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)));
    }
    if !existing {
        fields[3] = fields[3].clone().required();
    }
    let mut f = ObjectForm {
        sections: vec![FormSection::new("general", "General").fields(fields)],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    match &state {
        Some(s)
            if !ctx.may_change(
                Kind::Credential,
                &s.owner,
                &s.name,
                ctx.oracle_schema(&s.owner),
            ) =>
        {
            f.read_only = true;
            f.notes.push(OWNER_ONLY.into());
        }
        None if !ctx.may_create(Kind::Credential) => {
            f.read_only = true;
            f.notes.push(
                "Este usuario no tiene el privilegio CREATE CREDENTIAL: no puede crear credenciales."
                    .into(),
            );
        }
        _ => {}
    }
    Ok(f)
}
