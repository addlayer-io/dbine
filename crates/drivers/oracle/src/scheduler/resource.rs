//! Resources (`CREATE_RESOURCE`, `SET_ATTRIBUTE` units / state / comments,
//! `SET_RESOURCE_CONSTRAINT` for the jobs and programs that use them,
//! `DROP_RESOURCE`) and incompatibilities (`CREATE_INCOMPATIBILITY`,
//! `ADD_TO_INCOMPATIBILITY` / `REMOVE_FROM_INCOMPATIBILITY`, `ENABLE` /
//! `DISABLE`, `DROP_INCOMPATIBILITY`), where the version has them (12.2+).

use super::choices::{self, Catalog};
use super::read::{self, Rec};
use super::script::name_lit;
use super::{
    bad, fingerprint, fits, fresh, new_name, new_owner, obj_id, parse_obj, pick_ref, rows, Block,
    Ctx, Kind, Plan, Vals, A, OWNER_ONLY,
};
use dbine_driver::{
    FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result, SchedulerItem,
    SchedulerList,
};
use oracledb::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const STATUS: [&str; 2] = ["ENFORCE_CONSTRAINTS", "IGNORE_CONSTRAINTS"];
const LEVELS: [&str; 2] = ["JOB_LEVEL", "PROGRAM_LEVEL"];
const SOURCES: [&str; 2] = ["schema", "job_or_program"];

type Rows = Vec<BTreeMap<String, Value>>;

fn may(ctx: &Ctx, kind: Kind, owner: &str, name: &str) -> bool {
    ctx.may_change(kind, owner, name, ctx.oracle_schema(owner))
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceSpec {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) units: i64,
    pub(crate) status: String,
    /// Only when it's created: the dictionary doesn't show it.
    pub(crate) level: String,
    pub(crate) comments: Option<String>,
    /// (job or program id, units it takes).
    pub(crate) constraints: Vec<(String, i64)>,
}

impl ResourceSpec {
    fn id(&self) -> String {
        obj_id(Some(&self.owner), &self.name)
    }
}

fn read_resource(c: &Connection, ctx: &Ctx, owner: &str, name: &str) -> Result<ResourceSpec> {
    let r = read::row(
        c,
        &ctx.sql("{v}_scheduler_resources"),
        "WHERE owner = :1 AND resource_name = :2",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| {
        bad(format!(
            "no existe el recurso «{}» o este usuario no lo puede ver",
            obj_id(Some(owner), name)
        ))
    })?;
    let mut constraints: Vec<(String, i64)> = read::rows(
        c,
        &ctx.sql("{v}_scheduler_rsc_constraints"),
        "WHERE resource_owner = :1 AND resource_name = :2",
        &[&owner, &name],
        1000,
    )
    .map_err(crate::err)?
    .iter()
    .filter_map(|x: &Rec| {
        Some((
            obj_id(x.t("OBJECT_OWNER").as_deref(), &x.raw("OBJECT_NAME")?),
            x.int("UNITS_USED").unwrap_or(1),
        ))
    })
    .collect();
    constraints.sort();
    Ok(ResourceSpec {
        owner: owner.to_string(),
        name: name.to_string(),
        units: r.int("RESOURCE_UNITS").unwrap_or(1),
        status: r
            .t("STATUS")
            .filter(|s| STATUS.contains(&s.as_str()))
            .unwrap_or_else(|| "ENFORCE_CONSTRAINTS".into()),
        level: String::new(),
        comments: r.t("COMMENTS"),
        constraints,
    })
}

pub(crate) fn resource_values(
    s: &ResourceSpec,
) -> (BTreeMap<String, Value>, BTreeMap<String, Rows>) {
    let values = BTreeMap::from([
        ("owner".to_string(), json!(s.owner)),
        ("name".to_string(), json!(s.name)),
        ("units".to_string(), json!(s.units)),
        ("status".to_string(), json!(s.status)),
        (
            "comments".to_string(),
            json!(s.comments.clone().unwrap_or_default()),
        ),
    ]);
    let rows = s
        .constraints
        .iter()
        .map(|(o, u)| {
            BTreeMap::from([
                ("object".to_string(), json!(o)),
                ("units".to_string(), json!(u)),
            ])
        })
        .collect();
    (values, BTreeMap::from([("constraints".to_string(), rows)]))
}

pub(crate) fn resource_spec(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&ResourceSpec>,
) -> Result<ResourceSpec> {
    let v = Vals(&values.values);
    let (owner, name) = match cur {
        Some(s) => (s.owner.clone(), s.name.clone()),
        None => (
            new_owner(v, ctx, Kind::Resource, cat)?,
            new_name(&v.raw("name"), ctx, "Nombre")?,
        ),
    };
    let units = v
        .int("units", "Unidades", 1, i64::from(i32::MAX))?
        .ok_or_else(|| bad("Falta «Unidades»: cuántas tiene el recurso.".into()))?;
    let mut constraints: Vec<(String, i64)> = Vec::new();
    for r in rows(values, "constraints") {
        let o = r.text("object");
        let now = cur.and_then(|c| c.constraints.iter().find(|(x, _)| *x == o));
        let object = pick_ref(
            cat,
            "job_or_program",
            &o,
            now.map(|(x, _)| x.as_str()),
            "Job o programa",
        )?;
        let u = r.int("units", "Unidades que usa", 1, units)?.unwrap_or(1);
        if constraints.iter().any(|(x, _)| *x == object) {
            return Err(bad(format!(
                "«{}» está dos veces en la lista de quienes usan el recurso.",
                super::shown(&object)
            )));
        }
        constraints.push((object, u));
    }
    constraints.sort();
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(ResourceSpec {
        owner,
        name,
        units,
        status: v.choice("status", "Estado", &STATUS, "ENFORCE_CONSTRAINTS")?,
        level: v.choice("constraint_level", "Nivel", &LEVELS, "JOB_LEVEL")?,
        comments,
        constraints,
    })
}

fn constraint(b: &mut Block, target: &A, object: &str, units: i64) {
    b.call(
        "DBMS_SCHEDULER.SET_RESOURCE_CONSTRAINT",
        vec![
            ("object_name", A::Name(object.to_string())),
            ("resource_name", target.clone()),
            ("units", A::I(units)),
        ],
    );
}

pub(crate) fn resource_create(s: &ResourceSpec) -> Block {
    let id = s.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Crea el recurso {id}."));
    let mut args = vec![
        ("resource_name", target.clone()),
        ("units", A::I(s.units)),
        ("status", A::S(s.status.clone())),
        ("constraint_level", A::S(s.level.clone())),
    ];
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    b.call("DBMS_SCHEDULER.CREATE_RESOURCE", args);
    if !s.constraints.is_empty() {
        b.created(&format!(
            "DBMS_SCHEDULER.DROP_RESOURCE(resource_name => {}, force => TRUE);",
            name_lit(&id)
        ));
    }
    for (o, u) in &s.constraints {
        constraint(&mut b, &target, o, *u);
    }
    b
}

pub(crate) fn resource_alter(old: &ResourceSpec, new: &ResourceSpec) -> Block {
    let id = old.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia el recurso {id}."));
    if old.units != new.units {
        b.set_attr(&target, "units", A::I(new.units));
    }
    if old.status != new.status {
        b.set_attr(&target, "state", A::S(new.status.clone()));
    }
    if old.comments != new.comments {
        b.set_attr(&target, "comments", A::opt(new.comments.clone()));
    }
    for (o, _) in &old.constraints {
        if !new.constraints.iter().any(|(x, _)| x == o) {
            constraint(&mut b, &target, o, 0);
        }
    }
    for (o, u) in &new.constraints {
        if !old.constraints.iter().any(|(x, y)| x == o && y == u) {
            constraint(&mut b, &target, o, *u);
        }
    }
    b
}

pub(crate) fn items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_resources"),
        "ORDER BY owner, resource_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let owner = r.t("OWNER").unwrap_or_default();
                let name = r.raw("RESOURCE_NAME").unwrap_or_default();
                SchedulerItem {
                    id: obj_id(Some(&owner), &name),
                    name: super::item_name(ctx, &owner, &name),
                    description: r
                        .t("COMMENTS")
                        .or_else(|| r.t("RESOURCE_UNITS").map(|u| format!("{u} unidades"))),
                    enabled: Some(r.t("STATUS").as_deref() != Some("IGNORE_CONSTRAINTS")),
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
    let cat = choices::load(c, ctx, &SOURCES);
    match id {
        None => {
            let s = resource_spec(values, ctx, &cat, None)?;
            Ok(Plan::new(resource_create(&s), s.id(), &s.name))
        }
        Some(id) => {
            let (owner, name) = parse_obj(id, "recurso")?;
            let old = read_resource(c, ctx, &owner, &name)?;
            if !may(ctx, Kind::Resource, &owner, &name) {
                return Err(bad(OWNER_ONLY.into()));
            }
            let (v, l) = resource_values(&old);
            fresh(values, &fingerprint(&v, &l))?;
            let new = resource_spec(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(resource_alter(&old, &new), old.id(), &name))
        }
    }
}

pub(crate) fn delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "recurso")?;
    let s = read_resource(c, ctx, &owner, &name)?;
    if !may(ctx, Kind::Resource, &owner, &name) {
        return Err(bad(OWNER_ONLY.into()));
    }
    let mut b = Block::new(&format!(
        "Borra el recurso {} y lo que los jobs y programas tenían de él.",
        s.id()
    ));
    b.call(
        "DBMS_SCHEDULER.DROP_RESOURCE",
        vec![("resource_name", A::Name(s.id())), ("force", A::B(true))],
    );
    Ok(b)
}

pub(crate) fn form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &SOURCES);
    let state = match id {
        Some(id) => {
            let (o, n) = parse_obj(id, "recurso")?;
            Some(read_resource(c, ctx, &o, &n)?)
        }
        None => None,
    };
    let existing = state.is_some();
    let spec = state.clone().unwrap_or(ResourceSpec {
        owner: ctx.user().into(),
        name: String::new(),
        units: 1,
        status: "ENFORCE_CONSTRAINTS".into(),
        level: "JOB_LEVEL".into(),
        comments: None,
        constraints: Vec::new(),
    });
    let (mut values, lists) = resource_values(&spec);
    if existing {
        let fp = fingerprint(&values, &lists);
        values.insert("dbine_state".into(), json!(fp));
    } else {
        values.insert("constraint_level".into(), json!("JOB_LEVEL"));
    }
    let mut fields = vec![
        FormField::new("owner", "Esquema", FormKind::reference("schema", false))
            .read_only(existing || !ctx.create_any(Kind::Resource))
            .default(json!(ctx.user())),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new("units", "Unidades", FormKind::number(Some(1.0), None))
            .required()
            .help("Cuántas unidades tiene: los jobs que lo usan no se ejecutan juntos si entre todos pasan de este número."),
        FormField::new(
            "status",
            "Estado",
            FormKind::select(&[
                ("ENFORCE_CONSTRAINTS", "Se respeta"),
                ("IGNORE_CONSTRAINTS", "Se ignora"),
            ]),
        )
        .default(json!("ENFORCE_CONSTRAINTS")),
    ];
    if !existing {
        fields.push(
            FormField::new(
                "constraint_level",
                "Nivel",
                FormKind::select(&[("JOB_LEVEL", "Jobs"), ("PROGRAM_LEVEL", "Programas")]),
            )
            .help("Si lo usan jobs o programas. No se cambia después.")
            .default(json!("JOB_LEVEL")),
        );
    }
    fields.push(FormField::new(
        "comments",
        "Descripción",
        FormKind::Textarea,
    ));
    let users = FormSection::new("constraints", "Quiénes lo usan").list(
        FormList::new(
            "constraints",
            "Jobs y programas",
            "object",
            vec![
                FormField::new(
                    "object",
                    "Job o programa",
                    FormKind::reference("job_or_program", false),
                )
                .required(),
                FormField::new(
                    "units",
                    "Unidades que usa",
                    FormKind::number(Some(1.0), None),
                )
                .default(json!(1)),
            ],
        )
        .rows(lists.get("constraints").cloned().unwrap_or_default()),
    );
    let mut f = ObjectForm {
        sections: vec![FormSection::new("general", "General").fields(fields), users],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    match &state {
        Some(s) if !may(ctx, Kind::Resource, &s.owner, &s.name) => {
            f.read_only = true;
            f.notes.push(OWNER_ONLY.into());
        }
        None if !ctx.may_create(Kind::Resource) => {
            f.read_only = true;
            f.notes.push(super::NO_CREATE_JOB.into());
        }
        _ => {}
    }
    Ok(f)
}

// ---------------------------------------------------------------------------
// Incompatibilities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IncompatSpec {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) level: String,
    pub(crate) members: Vec<String>,
    pub(crate) enabled: bool,
    pub(crate) comments: Option<String>,
}

impl IncompatSpec {
    fn id(&self) -> String {
        obj_id(Some(&self.owner), &self.name)
    }
}

fn read_incompat(c: &Connection, ctx: &Ctx, owner: &str, name: &str) -> Result<IncompatSpec> {
    let r = read::row(
        c,
        &ctx.sql("{v}_scheduler_incompats"),
        "WHERE owner = :1 AND incompatibility_name = :2",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| {
        bad(format!(
            "no existe la incompatibilidad «{}» o este usuario no la puede ver",
            obj_id(Some(owner), name)
        ))
    })?;
    let mut members: Vec<String> = read::rows(
        c,
        &ctx.sql("{v}_scheduler_incompat_member"),
        "WHERE incompatibility_owner = :1 AND incompatibility_name = :2",
        &[&owner, &name],
        1000,
    )
    .map_err(crate::err)?
    .iter()
    .filter_map(|m| {
        Some(obj_id(
            m.t("OBJECT_OWNER").as_deref(),
            &m.raw("OBJECT_NAME")?,
        ))
    })
    .collect();
    members.sort();
    Ok(IncompatSpec {
        owner: owner.to_string(),
        name: name.to_string(),
        level: r
            .t("CONSTRAINT_LEVEL")
            .unwrap_or_else(|| "JOB_LEVEL".into()),
        members,
        enabled: r.yes("ENABLED"),
        comments: r.t("COMMENTS"),
    })
}

pub(crate) fn incompat_values(s: &IncompatSpec) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("owner".to_string(), json!(s.owner)),
        ("name".to_string(), json!(s.name)),
        ("constraint_level".to_string(), json!(s.level)),
        ("members".to_string(), json!(s.members)),
        ("enabled".to_string(), json!(s.enabled)),
        (
            "comments".to_string(),
            json!(s.comments.clone().unwrap_or_default()),
        ),
    ])
}

pub(crate) fn incompat_spec(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&IncompatSpec>,
) -> Result<IncompatSpec> {
    let v = Vals(&values.values);
    let (owner, name) = match cur {
        Some(s) => (s.owner.clone(), s.name.clone()),
        None => (
            new_owner(v, ctx, Kind::Incompatibility, cat)?,
            new_name(&v.raw("name"), ctx, "Nombre")?,
        ),
    };
    let mut members = Vec::new();
    for m in v.strings("members") {
        let now = cur
            .and_then(|c| c.members.iter().find(|x| **x == m))
            .map(String::as_str);
        let id = pick_ref(cat, "job_or_program", &m, now, "Jobs o programas")?;
        if !members.contains(&id) {
            members.push(id);
        }
    }
    if members.len() < 2 {
        return Err(bad(
            "Una incompatibilidad necesita al menos dos jobs o programas.".into(),
        ));
    }
    members.sort();
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(IncompatSpec {
        level: match cur {
            Some(c) => c.level.clone(),
            None => v.choice("constraint_level", "Nivel", &LEVELS, "JOB_LEVEL")?,
        },
        owner,
        name,
        members,
        enabled: v.bool_or("enabled", true),
        comments,
    })
}

/// `'"A"."J1", "A"."J2"'`.
pub(crate) fn object_list(ids: &[String]) -> A {
    A::S(
        ids.iter()
            .filter_map(|i| parse_obj(i, "objeto").ok())
            .map(|(o, n)| format!("{}.{}", crate::quote(&o), crate::quote(&n)))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

pub(crate) fn incompat_create(s: &IncompatSpec) -> Block {
    let id = s.id();
    let mut b = Block::new(&format!("Crea la incompatibilidad {id}."));
    let mut args = vec![
        ("incompatibility_name", A::Name(id)),
        ("object_name", object_list(&s.members)),
        ("constraint_level", A::S(s.level.clone())),
        ("enabled", A::B(s.enabled)),
    ];
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    b.call("DBMS_SCHEDULER.CREATE_INCOMPATIBILITY", args);
    b
}

pub(crate) fn incompat_alter(old: &IncompatSpec, new: &IncompatSpec) -> Block {
    let id = old.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia la incompatibilidad {id}."));
    let added: Vec<String> = new
        .members
        .iter()
        .filter(|m| !old.members.contains(m))
        .cloned()
        .collect();
    let removed: Vec<String> = old
        .members
        .iter()
        .filter(|m| !new.members.contains(m))
        .cloned()
        .collect();
    // Added first: the incompatibility never drops below two members.
    if !added.is_empty() {
        b.call(
            "DBMS_SCHEDULER.ADD_TO_INCOMPATIBILITY",
            vec![
                ("incompatibility_name", target.clone()),
                ("object_name", object_list(&added)),
            ],
        );
    }
    if !removed.is_empty() {
        b.call(
            "DBMS_SCHEDULER.REMOVE_FROM_INCOMPATIBILITY",
            vec![
                ("incompatibility_name", target.clone()),
                ("object_name", object_list(&removed)),
            ],
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

pub(crate) fn incompat_items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_incompats"),
        "ORDER BY owner, incompatibility_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let owner = r.t("OWNER").unwrap_or_default();
                let name = r.raw("INCOMPATIBILITY_NAME").unwrap_or_default();
                SchedulerItem {
                    id: obj_id(Some(&owner), &name),
                    name: super::item_name(ctx, &owner, &name),
                    description: r.t("COMMENTS"),
                    enabled: Some(r.yes("ENABLED")),
                    system: ctx.oracle_schema(&owner),
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

pub(crate) fn incompat_plan(
    c: &Connection,
    ctx: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let cat = choices::load(c, ctx, &SOURCES);
    match id {
        None => {
            let s = incompat_spec(values, ctx, &cat, None)?;
            Ok(Plan::new(incompat_create(&s), s.id(), &s.name))
        }
        Some(id) => {
            let (owner, name) = parse_obj(id, "incompatibilidad")?;
            let old = read_incompat(c, ctx, &owner, &name)?;
            if !may(ctx, Kind::Incompatibility, &owner, &name) {
                return Err(bad(OWNER_ONLY.into()));
            }
            fresh(
                values,
                &fingerprint(&incompat_values(&old), &BTreeMap::new()),
            )?;
            let new = incompat_spec(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(incompat_alter(&old, &new), old.id(), &name))
        }
    }
}

pub(crate) fn incompat_delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "incompatibilidad")?;
    let s = read_incompat(c, ctx, &owner, &name)?;
    if !may(ctx, Kind::Incompatibility, &owner, &name) {
        return Err(bad(OWNER_ONLY.into()));
    }
    let mut b = Block::new(&format!("Borra la incompatibilidad {}.", s.id()));
    b.call(
        "DBMS_SCHEDULER.DROP_INCOMPATIBILITY",
        vec![("incompatibility_name", A::Name(s.id()))],
    );
    Ok(b)
}

pub(crate) fn incompat_form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &SOURCES);
    let state = match id {
        Some(id) => {
            let (o, n) = parse_obj(id, "incompatibilidad")?;
            Some(read_incompat(c, ctx, &o, &n)?)
        }
        None => None,
    };
    let existing = state.is_some();
    let values = match &state {
        Some(s) => {
            let mut v = incompat_values(s);
            let fp = fingerprint(&v, &BTreeMap::new());
            v.insert("dbine_state".into(), json!(fp));
            v
        }
        None => BTreeMap::from([
            ("owner".to_string(), json!(ctx.user())),
            ("constraint_level".to_string(), json!("JOB_LEVEL")),
            ("members".to_string(), json!([])),
            ("enabled".to_string(), json!(true)),
            ("comments".to_string(), json!("")),
        ]),
    };
    let fields = vec![
        FormField::new("owner", "Esquema", FormKind::reference("schema", false))
            .read_only(existing || !ctx.create_any(Kind::Incompatibility))
            .default(json!(ctx.user())),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new(
            "constraint_level",
            "Nivel",
            FormKind::select(&[
                ("JOB_LEVEL", "Jobs: no se ejecutan dos de ellos a la vez"),
                (
                    "PROGRAM_LEVEL",
                    "Programas: no se ejecutan a la vez jobs de dos de ellos",
                ),
            ]),
        )
        .read_only(existing)
        .default(json!("JOB_LEVEL")),
        FormField::new(
            "members",
            "Jobs o programas",
            FormKind::reference("job_or_program", true),
        )
        .required(),
        FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ];
    let mut f = ObjectForm {
        sections: vec![FormSection::new("general", "General").fields(fields)],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    match &state {
        Some(s) if !may(ctx, Kind::Incompatibility, &s.owner, &s.name) => {
            f.read_only = true;
            f.notes.push(OWNER_ONLY.into());
        }
        None if !ctx.may_create(Kind::Incompatibility) => {
            f.read_only = true;
            f.notes.push(super::NO_CREATE_JOB.into());
        }
        _ => {}
    }
    Ok(f)
}
