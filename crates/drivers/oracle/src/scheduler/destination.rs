//! Remote destinations (12c+): database destinations
//! (`CREATE_DATABASE_DESTINATION`, `SET_ATTRIBUTE`,
//! `DROP_DATABASE_DESTINATION`), external destinations (the Scheduler
//! agents that registered with this database: listed, shown and dropped
//! with `DROP_AGENT_DESTINATION`; an agent creates its own by registering)
//! and destination groups (`CREATE_GROUP` of type DB_DEST or
//! EXTERNAL_DEST, `ADD_GROUP_MEMBER` / `REMOVE_GROUP_MEMBER`,
//! `DROP_GROUP`).

use super::choices::{self, Catalog};
use super::read::{self, Rec};
use super::script::lit;
use super::{
    bad, bare, fingerprint, fits, fresh, new_name, new_owner, obj_id, parse_obj, pick_ref, rows,
    shown, Block, Ctx, Kind, Plan, Vals, A, MANAGE_ONLY, OWNER_ONLY,
};
use crate::jobs;
use dbine_driver::{
    FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result, SchedulerItem,
    SchedulerList,
};
use oracledb::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const AGENT_ONLY: &str = "Un destino externo lo crea el agente del Scheduler al registrarse en esta base (schagent -registerdatabase): no se crea ni se cambia desde DBine.";

// ---------------------------------------------------------------------------
// Database destinations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DbDestSpec {
    pub(crate) owner: String,
    pub(crate) name: String,
    /// The agent's external destination (id, `SYS.NAME`).
    pub(crate) agent: String,
    pub(crate) connect_info: String,
    pub(crate) enabled: bool,
    pub(crate) comments: Option<String>,
}

impl DbDestSpec {
    fn id(&self) -> String {
        obj_id(Some(&self.owner), &self.name)
    }
}

fn read_db(c: &Connection, ctx: &Ctx, owner: &str, name: &str) -> Result<DbDestSpec> {
    let r = read::row(
        c,
        &ctx.sql("{v}_scheduler_db_dests"),
        "WHERE owner = :1 AND destination_name = :2",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| {
        bad(format!(
            "no existe el destino «{}» o este usuario no lo puede ver",
            obj_id(Some(owner), name)
        ))
    })?;
    Ok(DbDestSpec {
        owner: owner.to_string(),
        name: name.to_string(),
        agent: r
            .t("AGENT")
            .map(|a| obj_id(Some("SYS"), &a))
            .unwrap_or_default(),
        connect_info: r.raw("CONNECT_INFO").unwrap_or_default(),
        enabled: r.yes("ENABLED"),
        comments: r.t("COMMENTS"),
    })
}

pub(crate) fn db_values(s: &DbDestSpec) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("owner".to_string(), json!(s.owner)),
        ("name".to_string(), json!(s.name)),
        ("agent".to_string(), json!(s.agent)),
        ("connect_info".to_string(), json!(s.connect_info)),
        ("enabled".to_string(), json!(s.enabled)),
        (
            "comments".to_string(),
            json!(s.comments.clone().unwrap_or_default()),
        ),
    ])
}

pub(crate) fn db_spec(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&DbDestSpec>,
) -> Result<DbDestSpec> {
    let v = Vals(&values.values);
    let (owner, name) = match cur {
        Some(s) => (s.owner.clone(), s.name.clone()),
        None => (
            new_owner(v, ctx, Kind::DbDestination, cat)?,
            new_name(&v.raw("name"), ctx, "Nombre")?,
        ),
    };
    let agent = v.text("agent");
    if agent.is_empty() {
        return Err(bad(
            "Falta «Agente»: el destino externo que se conecta a esa base.".into(),
        ));
    }
    let connect_info = v.text("connect_info");
    if connect_info.is_empty() {
        return Err(bad(
            "Falta «Conexión»: un alias de tnsnames.ora o un descriptor.".into(),
        ));
    }
    fits(&Some(connect_info.clone()), "Conexión", 4000)?;
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(DbDestSpec {
        agent: pick_ref(
            cat,
            "external_destination",
            &agent,
            cur.map(|c| c.agent.as_str()),
            "Agente",
        )?,
        owner,
        name,
        connect_info,
        enabled: v.bool_or("enabled", true),
        comments,
    })
}

pub(crate) fn db_create(s: &DbDestSpec) -> Block {
    let id = s.id();
    let mut b = Block::new(&format!("Crea el destino de base de datos {id}."));
    let mut args = vec![
        ("destination_name", A::Name(id.clone())),
        ("agent", bare(&s.agent)),
        ("tns_name", A::S(s.connect_info.clone())),
    ];
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    b.call("DBMS_SCHEDULER.CREATE_DATABASE_DESTINATION", args);
    if !s.enabled {
        b.created(&format!(
            "DBMS_SCHEDULER.DROP_DATABASE_DESTINATION(destination_name => {});",
            super::script::name_lit(&id)
        ));
        b.pcall("DBMS_SCHEDULER.DISABLE", vec![A::Name(id)]);
    }
    b
}

pub(crate) fn db_alter(old: &DbDestSpec, new: &DbDestSpec) -> Block {
    let id = old.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia el destino de base de datos {id}."));
    if old.agent != new.agent {
        b.set_attr(&target, "agent", bare(&new.agent));
    }
    if old.connect_info != new.connect_info {
        b.set_attr(&target, "connect_info", A::S(new.connect_info.clone()));
    }
    if old.comments != new.comments {
        b.set_attr(&target, "comments", A::opt(new.comments.clone()));
    }
    if old.enabled != new.enabled {
        let p = if new.enabled {
            "DBMS_SCHEDULER.ENABLE"
        } else {
            "DBMS_SCHEDULER.DISABLE"
        };
        b.pcall(p, vec![target]);
    }
    b
}

pub(crate) fn db_items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_db_dests"),
        "ORDER BY owner, destination_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let owner = r.t("OWNER").unwrap_or_default();
                let name = r.raw("DESTINATION_NAME").unwrap_or_default();
                let locked = ctx
                    .locked(
                        Kind::DbDestination,
                        &owner,
                        &name,
                        ctx.oracle_schema(&owner),
                    )
                    .is_some();
                SchedulerItem {
                    id: obj_id(Some(&owner), &name),
                    name: super::item_name(ctx, &owner, &name),
                    description: r.t("COMMENTS").or_else(|| r.t("CONNECT_INFO")),
                    enabled: Some(r.yes("ENABLED")),
                    system: ctx.oracle_schema(&owner),
                    can_edit: super::unless(locked),
                    can_delete: super::unless(locked),
                    actions: None,
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

pub(crate) fn db_plan(
    c: &Connection,
    ctx: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let cat = choices::load(c, ctx, &["schema", "external_destination"]);
    match id {
        None => {
            let s = db_spec(values, ctx, &cat, None)?;
            Ok(Plan::new(db_create(&s), s.id(), &s.name))
        }
        Some(id) => {
            let (owner, name) = parse_obj(id, "destino")?;
            let old = read_db(c, ctx, &owner, &name)?;
            ctx.check_change(
                Kind::DbDestination,
                &owner,
                &name,
                ctx.oracle_schema(&owner),
            )?;
            fresh(values, &fingerprint(&db_values(&old), &BTreeMap::new()))?;
            let new = db_spec(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(db_alter(&old, &new), old.id(), &name))
        }
    }
}

pub(crate) fn db_delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "destino")?;
    let s = read_db(c, ctx, &owner, &name)?;
    ctx.check_change(
        Kind::DbDestination,
        &owner,
        &name,
        ctx.oracle_schema(&owner),
    )?;
    let mut b = Block::new(&format!("Borra el destino de base de datos {}.", s.id()));
    b.call(
        "DBMS_SCHEDULER.DROP_DATABASE_DESTINATION",
        vec![("destination_name", A::Name(s.id()))],
    );
    Ok(b)
}

pub(crate) fn db_form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &["schema", "external_destination"]);
    let state = match id {
        Some(id) => {
            let (o, n) = parse_obj(id, "destino")?;
            Some(read_db(c, ctx, &o, &n)?)
        }
        None => None,
    };
    let existing = state.is_some();
    let values = match &state {
        Some(s) => {
            let mut v = db_values(s);
            let fp = fingerprint(&v, &BTreeMap::new());
            v.insert("dbine_state".into(), json!(fp));
            v
        }
        None => BTreeMap::from([
            ("owner".to_string(), json!(ctx.user())),
            ("enabled".to_string(), json!(true)),
            ("comments".to_string(), json!("")),
        ]),
    };
    let fields = vec![
        FormField::new("owner", "Esquema", FormKind::reference("schema", false))
            .read_only(existing || !ctx.create_any(Kind::DbDestination))
            .default(json!(ctx.user())),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new(
            "agent",
            "Agente",
            FormKind::reference("external_destination", false),
        )
        .required()
        .help("El agente del Scheduler (destino externo) que se conecta a la base remota."),
        FormField::new("connect_info", "Conexión", FormKind::Textarea)
            .required()
            .placeholder("VENTAS_PROD")
            .help("Un alias de tnsnames.ora del host del agente, o un descriptor de conexión."),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ];
    let no_agents = cat.get("external_destination").is_empty();
    let mut f = ObjectForm {
        sections: vec![FormSection::new("general", "General").fields(fields)],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    if no_agents {
        f.notes.push("No hay agentes registrados en esta base: un destino de base de datos necesita uno (schagent -registerdatabase en el host remoto).".into());
    }
    let lock = state.as_ref().and_then(|s| {
        ctx.locked(
            Kind::DbDestination,
            &s.owner,
            &s.name,
            ctx.oracle_schema(&s.owner),
        )
    });
    match &state {
        Some(_) if lock.is_some() => {
            f.read_only = true;
            f.notes.insert(0, lock.unwrap_or(OWNER_ONLY).into());
        }
        None if !ctx.may_create(Kind::DbDestination) => {
            f.read_only = true;
            f.notes.insert(0, super::NO_CREATE_JOB.into());
        }
        _ => {}
    }
    Ok(f)
}

// ---------------------------------------------------------------------------
// External destinations
// ---------------------------------------------------------------------------

fn read_external(c: &Connection, ctx: &Ctx, name: &str) -> Result<Rec> {
    read::row(
        c,
        &ctx.sql("{v}_scheduler_external_dests"),
        "WHERE destination_name = :1",
        &[&name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| bad(format!("no existe el destino externo «{}»", shown(name))))
}

fn sys_name(id: &str) -> Result<String> {
    let (owner, name) = parse_obj(id, "destino externo")?;
    if owner != "SYS" {
        return Err(bad(format!(
            "«{}» no es un id de destino externo de Oracle",
            shown(id)
        )));
    }
    Ok(name)
}

pub(crate) fn external_items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_external_dests"),
        "ORDER BY destination_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let name = r.raw("DESTINATION_NAME").unwrap_or_default();
                SchedulerItem {
                    id: obj_id(Some("SYS"), &name),
                    name,
                    description: r.t("COMMENTS").or_else(|| {
                        r.t("HOSTNAME")
                            .map(|h| format!("{h}:{}", r.t("PORT").unwrap_or_default()))
                    }),
                    enabled: Some(r.yes("ENABLED")),
                    system: false,
                    can_edit: None,
                    can_delete: None,
                    actions: None,
                }
            })
            .collect(),
        notes: vec![AGENT_ONLY.into()],
    })
}

pub(crate) fn external_form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let name = sys_name(id.unwrap_or_default())?;
    let r = read_external(c, ctx, &name)?;
    let mut values = BTreeMap::new();
    for (k, col) in [
        ("name", "DESTINATION_NAME"),
        ("hostname", "HOSTNAME"),
        ("port", "PORT"),
        ("ip_address", "IP_ADDRESS"),
        ("comments", "COMMENTS"),
    ] {
        values.insert(k.to_string(), json!(r.t(col).unwrap_or_default()));
    }
    values.insert("enabled".into(), json!(r.yes("ENABLED")));
    let fields = vec![
        FormField::new("name", "Nombre", FormKind::Text).read_only(true),
        FormField::new("hostname", "Host", FormKind::Text).read_only(true),
        FormField::new("port", "Puerto", FormKind::Text).read_only(true),
        FormField::new("ip_address", "Dirección IP", FormKind::Text).read_only(true),
        FormField::new("enabled", "Habilitado", FormKind::Bool).read_only(true),
        FormField::new("comments", "Descripción", FormKind::Textarea).read_only(true),
    ];
    Ok(ObjectForm {
        sections: vec![FormSection::new("general", "General").fields(fields)],
        values,
        notes: vec![AGENT_ONLY.into()],
        read_only: true,
        ..ObjectForm::default()
    })
}

pub(crate) fn external_delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    if !ctx.manage() {
        return Err(bad(MANAGE_ONLY.into()));
    }
    let name = sys_name(id)?;
    read_external(c, ctx, &name)?;
    let mut b = Block::new(&format!(
        "Quita el agente {} de los destinos de esta base (para volver, tiene que registrarse de nuevo).",
        name
    ));
    b.call(
        "DBMS_SCHEDULER.DROP_AGENT_DESTINATION",
        vec![("destination_name", A::R(lit(&crate::quote(&name))))],
    );
    Ok(b)
}

// ---------------------------------------------------------------------------
// Destination groups
// ---------------------------------------------------------------------------

/// A member: `LOCAL` or a destination (id), with the credential it runs
/// with there (id).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Member {
    pub(crate) destination: String,
    pub(crate) credential: Option<String>,
}

impl Member {
    /// As `CREATE_GROUP` / `ADD_GROUP_MEMBER` take it:
    /// `"CRED_OWNER"."CRED"@"OWNER"."DEST"`.
    pub(crate) fn text(&self) -> String {
        let q = |id: &str| match jobs::parse_id(id) {
            Ok(jobs::JobId::Scheduler { owner, name }) => {
                format!("{}.{}", crate::quote(&owner), crate::quote(&name))
            }
            _ => id.to_string(),
        };
        let dest = if self.destination.eq_ignore_ascii_case("LOCAL") {
            "LOCAL".to_string()
        } else {
            q(&self.destination)
        };
        match &self.credential {
            Some(c) => format!("{}@{dest}", q(c)),
            None => dest,
        }
    }

    /// From the dictionary's `MEMBER_NAME`.
    pub(crate) fn parse(text: &str, owner: &str) -> Member {
        // The `@` outside quotes.
        let mut quoted = false;
        let mut at = None;
        for (i, ch) in text.char_indices() {
            match ch {
                '"' => quoted = !quoted,
                '@' if !quoted => at = Some(i),
                _ => {}
            }
        }
        let id = |t: &str| -> String {
            let t = t.trim();
            if t.eq_ignore_ascii_case("LOCAL") {
                return "LOCAL".into();
            }
            match jobs::object_name(t, owner) {
                Some((o, n)) => obj_id(Some(&o), &n),
                None => t.to_string(),
            }
        };
        match at {
            Some(i) => Member {
                credential: Some(id(&text[..i])),
                destination: id(&text[i + 1..]),
            },
            None => Member {
                destination: id(text),
                credential: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupSpec {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) group_type: String,
    pub(crate) members: Vec<Member>,
    pub(crate) enabled: bool,
    pub(crate) comments: Option<String>,
}

impl GroupSpec {
    fn id(&self) -> String {
        obj_id(Some(&self.owner), &self.name)
    }
}

fn read_group(c: &Connection, ctx: &Ctx, owner: &str, name: &str) -> Result<GroupSpec> {
    let r = read::row(
        c,
        &ctx.sql("{v}_scheduler_groups"),
        "WHERE owner = :1 AND group_name = :2 AND group_type IN ('DB_DEST', 'EXTERNAL_DEST')",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| {
        bad(format!(
            "no existe el grupo de destinos «{}» o este usuario no lo puede ver",
            obj_id(Some(owner), name)
        ))
    })?;
    let mut members: Vec<Member> = read::rows(
        c,
        &ctx.sql("{v}_scheduler_group_members"),
        "WHERE owner = :1 AND group_name = :2",
        &[&owner, &name],
        1000,
    )
    .map_err(crate::err)?
    .iter()
    .filter_map(|m| m.raw("MEMBER_NAME"))
    .map(|m| Member::parse(&m, "SYS"))
    .collect();
    members.sort();
    Ok(GroupSpec {
        owner: owner.to_string(),
        name: name.to_string(),
        group_type: r.t("GROUP_TYPE").unwrap_or_else(|| "DB_DEST".into()),
        members,
        enabled: r.yes("ENABLED"),
        comments: r.t("COMMENTS"),
    })
}

type Rows = Vec<BTreeMap<String, Value>>;

pub(crate) fn group_values(s: &GroupSpec) -> (BTreeMap<String, Value>, BTreeMap<String, Rows>) {
    let values = BTreeMap::from([
        ("owner".to_string(), json!(s.owner)),
        ("name".to_string(), json!(s.name)),
        ("group_type".to_string(), json!(s.group_type)),
        ("enabled".to_string(), json!(s.enabled)),
        (
            "comments".to_string(),
            json!(s.comments.clone().unwrap_or_default()),
        ),
    ]);
    let rows = s
        .members
        .iter()
        .map(|m| {
            BTreeMap::from([
                ("destination".to_string(), json!(m.destination)),
                (
                    "credential".to_string(),
                    json!(m.credential.clone().unwrap_or_default()),
                ),
            ])
        })
        .collect();
    (values, BTreeMap::from([("members".to_string(), rows)]))
}

pub(crate) fn group_spec(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&GroupSpec>,
) -> Result<GroupSpec> {
    let v = Vals(&values.values);
    let (owner, name) = match cur {
        Some(s) => (s.owner.clone(), s.name.clone()),
        None => (
            new_owner(v, ctx, Kind::DestinationGroup, cat)?,
            new_name(&v.raw("name"), ctx, "Nombre")?,
        ),
    };
    let group_type = match cur {
        Some(s) => s.group_type.clone(),
        None => v.choice(
            "group_type",
            "Tipo",
            &["DB_DEST", "EXTERNAL_DEST"],
            "DB_DEST",
        )?,
    };
    let source = if group_type == "DB_DEST" {
        "member:db"
    } else {
        "member:external"
    };
    let mut members: Vec<Member> = Vec::new();
    for r in rows(values, "members") {
        let d = r.text("destination");
        let now = cur.and_then(|c| c.members.iter().find(|m| m.destination == d));
        let destination = pick_ref(
            cat,
            source,
            &d,
            now.map(|m| m.destination.as_str()),
            "Destino",
        )?;
        let credential = match r.opt("credential") {
            None => None,
            Some(cr) => Some(pick_ref(
                cat,
                "credential",
                &cr,
                now.and_then(|m| m.credential.as_deref()),
                "Credencial",
            )?),
        };
        if group_type == "DB_DEST" && destination == "LOCAL" && credential.is_some() {
            return Err(bad(
                "En un grupo de destinos de base de datos, LOCAL no lleva credencial: ahí los jobs se ejecutan en esta base, como su dueño.".into(),
            ));
        }
        let m = Member {
            destination,
            credential,
        };
        if !members.contains(&m) {
            members.push(m);
        }
    }
    members.sort();
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(GroupSpec {
        owner,
        name,
        group_type,
        members,
        enabled: v.bool_or("enabled", true),
        comments,
    })
}

pub(crate) fn members_arg(members: &[Member]) -> A {
    if members.is_empty() {
        return A::Null;
    }
    A::S(
        members
            .iter()
            .map(Member::text)
            .collect::<Vec<_>>()
            .join(", "),
    )
}

pub(crate) fn group_create(s: &GroupSpec) -> Block {
    let id = s.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Crea el grupo de destinos {id}."));
    b.call(
        "DBMS_SCHEDULER.CREATE_GROUP",
        vec![
            ("group_name", target.clone()),
            ("group_type", A::S(s.group_type.clone())),
            ("member", members_arg(&s.members)),
            ("comments", A::opt(s.comments.clone())),
        ],
    );
    if !s.enabled {
        b.created(&format!(
            "DBMS_SCHEDULER.DROP_GROUP(group_name => {}, force => TRUE);",
            super::script::name_lit(&id)
        ));
        b.pcall("DBMS_SCHEDULER.DISABLE", vec![target]);
    }
    b
}

pub(crate) fn group_alter(old: &GroupSpec, new: &GroupSpec) -> Block {
    let id = old.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia el grupo de destinos {id}."));
    for m in old.members.iter().filter(|m| !new.members.contains(m)) {
        b.call(
            "DBMS_SCHEDULER.REMOVE_GROUP_MEMBER",
            vec![("group_name", target.clone()), ("member", A::S(m.text()))],
        );
    }
    for m in new.members.iter().filter(|m| !old.members.contains(m)) {
        b.call(
            "DBMS_SCHEDULER.ADD_GROUP_MEMBER",
            vec![("group_name", target.clone()), ("member", A::S(m.text()))],
        );
    }
    if old.comments != new.comments {
        b.set_attr(&target, "comments", A::opt(new.comments.clone()));
    }
    if old.enabled != new.enabled {
        let p = if new.enabled {
            "DBMS_SCHEDULER.ENABLE"
        } else {
            "DBMS_SCHEDULER.DISABLE"
        };
        b.pcall(p, vec![target]);
    }
    b
}

pub(crate) fn group_items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_groups"),
        "WHERE group_type IN ('DB_DEST', 'EXTERNAL_DEST') ORDER BY owner, group_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let owner = r.t("OWNER").unwrap_or_default();
                let name = r.raw("GROUP_NAME").unwrap_or_default();
                let locked = ctx
                    .locked(
                        Kind::DestinationGroup,
                        &owner,
                        &name,
                        ctx.oracle_schema(&owner),
                    )
                    .is_some();
                SchedulerItem {
                    id: obj_id(Some(&owner), &name),
                    name: super::item_name(ctx, &owner, &name),
                    description: r.t("COMMENTS"),
                    enabled: Some(r.yes("ENABLED")),
                    system: ctx.oracle_schema(&owner),
                    can_edit: super::unless(locked),
                    can_delete: super::unless(locked),
                    actions: None,
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

const GROUP_SOURCES: [&str; 5] = [
    "schema",
    "member:db",
    "member:external",
    "member:any",
    "credential",
];

pub(crate) fn group_plan(
    c: &Connection,
    ctx: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let cat = choices::load(c, ctx, &GROUP_SOURCES);
    match id {
        None => {
            let s = group_spec(values, ctx, &cat, None)?;
            Ok(Plan::new(group_create(&s), s.id(), &s.name))
        }
        Some(id) => {
            let (owner, name) = parse_obj(id, "grupo de destinos")?;
            let old = read_group(c, ctx, &owner, &name)?;
            ctx.check_change(
                Kind::DestinationGroup,
                &owner,
                &name,
                ctx.oracle_schema(&owner),
            )?;
            let (v, l) = group_values(&old);
            fresh(values, &fingerprint(&v, &l))?;
            let new = group_spec(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(group_alter(&old, &new), old.id(), &name))
        }
    }
}

pub(crate) fn group_delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "grupo de destinos")?;
    let s = read_group(c, ctx, &owner, &name)?;
    ctx.check_change(
        Kind::DestinationGroup,
        &owner,
        &name,
        ctx.oracle_schema(&owner),
    )?;
    let mut b = Block::new(&format!(
        "Borra el grupo de destinos {} (sus destinos quedan).",
        s.id()
    ));
    b.call(
        "DBMS_SCHEDULER.DROP_GROUP",
        vec![("group_name", A::Name(s.id())), ("force", A::B(false))],
    );
    Ok(b)
}

pub(crate) fn group_form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &GROUP_SOURCES);
    let state = match id {
        Some(id) => {
            let (o, n) = parse_obj(id, "grupo de destinos")?;
            Some(read_group(c, ctx, &o, &n)?)
        }
        None => None,
    };
    let existing = state.is_some();
    let spec = state.clone().unwrap_or(GroupSpec {
        owner: ctx.user().into(),
        name: String::new(),
        group_type: "DB_DEST".into(),
        members: Vec::new(),
        enabled: true,
        comments: None,
    });
    let (mut values, lists) = group_values(&spec);
    if existing {
        let fp = fingerprint(&values, &lists);
        values.insert("dbine_state".into(), json!(fp));
    }
    let source = match &state {
        None => "member:any",
        Some(s) if s.group_type == "DB_DEST" => "member:db",
        Some(_) => "member:external",
    };
    let general = FormSection::new("general", "General").fields(vec![
        FormField::new("owner", "Esquema", FormKind::reference("schema", false))
            .read_only(existing || !ctx.create_any(Kind::DestinationGroup))
            .default(json!(ctx.user())),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new(
            "group_type",
            "Tipo",
            FormKind::select(&[
                ("DB_DEST", "Destinos de base de datos"),
                ("EXTERNAL_DEST", "Destinos externos"),
            ]),
        )
        .read_only(existing)
        .default(json!("DB_DEST")),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ]);
    let members = FormSection::new("members", "Miembros").list(
        FormList::new(
            "members",
            "Miembros",
            "destination",
            vec![
                FormField::new("destination", "Destino", FormKind::reference(source, false))
                    .required()
                    .help("LOCAL es esta base (o este host)."),
                FormField::new(
                    "credential",
                    "Credencial",
                    FormKind::reference("credential", false),
                )
                .help("Con la que se ejecutan ahí los jobs del grupo. Vacía: la del job."),
            ],
        )
        .rows(lists.get("members").cloned().unwrap_or_default()),
    );
    let mut f = ObjectForm {
        sections: vec![general, members],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    if !existing {
        f.notes.push("Los miembros tienen que ser del tipo del grupo: destinos de base de datos o destinos externos (agentes), además de LOCAL.".into());
    }
    let lock = state.as_ref().and_then(|s| {
        ctx.locked(
            Kind::DestinationGroup,
            &s.owner,
            &s.name,
            ctx.oracle_schema(&s.owner),
        )
    });
    match &state {
        Some(_) if lock.is_some() => {
            f.read_only = true;
            f.notes.insert(0, lock.unwrap_or(OWNER_ONLY).into());
        }
        None if !ctx.may_create(Kind::DestinationGroup) => {
            f.read_only = true;
            f.notes.insert(0, super::NO_CREATE_JOB.into());
        }
        _ => {}
    }
    Ok(f)
}
