//! Named schedules: `CREATE_SCHEDULE` (once, or a calendaring or PL/SQL
//! repeat interval), `CREATE_EVENT_SCHEDULE` (a queue and a condition);
//! `SET_ATTRIBUTE` for their dates, interval, event and comments;
//! `DROP_SCHEDULE`. A schedule keeps its family: one by time doesn't
//! become one by event.

use super::choices::{self, Catalog};
use super::read::{self, Rec};
use super::timing::{self, When};
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

const TIME_MODES: [&str; 3] = ["once", "calendar", "expression"];
const EVENT_MODES: [&str; 1] = ["event"];
const FAMILY: &str =
    "Una programación por tiempo no se puede cambiar a una por evento (ni al revés): creá otra.";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScheduleSpec {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) when: When,
    pub(crate) comments: Option<String>,
}

impl ScheduleSpec {
    fn id(&self) -> String {
        obj_id(Some(&self.owner), &self.name)
    }
    fn event(&self) -> bool {
        matches!(self.when, When::Event { .. })
    }
}

/// When a schedule runs: its row, but for its own name (in the column
/// that, in a job's or a window's row, names the schedule they use).
pub(crate) fn when_of(r: &Rec) -> When {
    let mut own = r.clone();
    own.0.remove("SCHEDULE_NAME");
    own.0.remove("SCHEDULE_OWNER");
    When::of(&own)
}

pub(crate) fn spec_of_rec(r: &Rec) -> ScheduleSpec {
    ScheduleSpec {
        owner: r.t("OWNER").unwrap_or_default(),
        name: r.raw("SCHEDULE_NAME").unwrap_or_default(),
        when: when_of(r),
        comments: r.t("COMMENTS"),
    }
}

fn read_state(c: &Connection, ctx: &Ctx, owner: &str, name: &str) -> Result<ScheduleSpec> {
    read::row(
        c,
        &ctx.sql("{v}_scheduler_schedules"),
        "WHERE owner = :1 AND schedule_name = :2",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .map(|r| spec_of_rec(&r))
    .ok_or_else(|| {
        bad(format!(
            "no existe la programación «{}» o este usuario no la puede ver",
            obj_id(Some(owner), name)
        ))
    })
}

pub(crate) fn values_of(s: &ScheduleSpec) -> BTreeMap<String, Value> {
    let mut v = BTreeMap::new();
    let mut put = |k: &str, x: Value| {
        v.insert(k.to_string(), x);
    };
    put("owner", json!(s.owner));
    put("name", json!(s.name));
    s.when.values(&mut put);
    put("comments", json!(s.comments.clone().unwrap_or_default()));
    v
}

pub(crate) fn spec_of(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&ScheduleSpec>,
) -> Result<ScheduleSpec> {
    let v = Vals(&values.values);
    let (owner, name) = match cur {
        Some(s) => (s.owner.clone(), s.name.clone()),
        None => (
            new_owner(v, ctx, Kind::Schedule, cat)?,
            new_name(&v.raw("name"), ctx, "Nombre")?,
        ),
    };
    let modes: Vec<&str> = match cur {
        Some(s) if s.event() => EVENT_MODES.to_vec(),
        Some(_) => TIME_MODES.to_vec(),
        None => TIME_MODES.iter().chain(&EVENT_MODES).copied().collect(),
    };
    let mode = v.text("schedule_mode");
    if cur.is_some() && !modes.contains(&mode.as_str()) && !mode.is_empty() {
        return Err(bad(FAMILY.into()));
    }
    let when = When::from_values(v, &modes)?;
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(ScheduleSpec {
        owner,
        name,
        when,
        comments,
    })
}

pub(crate) fn create_block(s: &ScheduleSpec) -> Block {
    let id = s.id();
    let mut b = Block::new(&format!("Crea la programación {id}."));
    let mut args = vec![("schedule_name", A::Name(id))];
    args.extend(timing::create_args(&s.when));
    if matches!(s.when, When::Once(_)) {
        // The calendar overload takes the interval without a default.
        args.push(("repeat_interval", A::Null));
    }
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    let proc_name = if s.event() {
        "DBMS_SCHEDULER.CREATE_EVENT_SCHEDULE"
    } else {
        "DBMS_SCHEDULER.CREATE_SCHEDULE"
    };
    b.call(proc_name, args);
    b
}

pub(crate) fn alter_block(old: &ScheduleSpec, new: &ScheduleSpec) -> Block {
    let id = old.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia la programación {id}."));
    timing::check_first(&mut b, &old.when, &new.when);
    timing::changes(&mut b, &target, &old.when, &new.when);
    if old.comments != new.comments {
        b.set_attr(&target, "comments", A::opt(new.comments.clone()));
    }
    b
}

pub(crate) fn items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_schedules"),
        "ORDER BY owner, schedule_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let owner = r.t("OWNER").unwrap_or_default();
                let name = r.raw("SCHEDULE_NAME").unwrap_or_default();
                let when = when_of(r);
                SchedulerItem {
                    id: obj_id(Some(&owner), &name),
                    name: super::item_name(ctx, &owner, &name),
                    description: r.t("COMMENTS").or_else(|| when.repeat()),
                    enabled: None,
                    system: ctx.oracle_schema(&owner),
                    can_edit: None,
                    can_delete: None,
                    actions: None,
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
            Ok(Plan::new(create_block(&s), s.id(), &s.name))
        }
        Some(id) => {
            let (owner, name) = parse_obj(id, "programación")?;
            let old = read_state(c, ctx, &owner, &name)?;
            if !ctx.may_change(Kind::Schedule, &owner, &name, ctx.oracle_schema(&owner)) {
                return Err(bad(OWNER_ONLY.into()));
            }
            fresh(values, &fingerprint(&values_of(&old), &BTreeMap::new()))?;
            let new = spec_of(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(alter_block(&old, &new), old.id(), &name))
        }
    }
}

pub(crate) fn delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "programación")?;
    let s = read_state(c, ctx, &owner, &name)?;
    if !ctx.may_change(Kind::Schedule, &owner, &name, ctx.oracle_schema(&owner)) {
        return Err(bad(OWNER_ONLY.into()));
    }
    let mut b = Block::new(&format!("Borra la programación {}.", s.id()));
    b.note("Sin force, Oracle no la borra mientras un job o una ventana la use.");
    b.call(
        "DBMS_SCHEDULER.DROP_SCHEDULE",
        vec![("schedule_name", A::Name(s.id())), ("force", A::B(false))],
    );
    Ok(b)
}

pub(crate) fn form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &["schema"]);
    let state = match id {
        Some(id) => {
            let (o, n) = parse_obj(id, "programación")?;
            Some(read_state(c, ctx, &o, &n)?)
        }
        None => None,
    };
    let modes: Vec<&str> = match &state {
        Some(s) if s.event() => EVENT_MODES.to_vec(),
        Some(_) => TIME_MODES.to_vec(),
        None => TIME_MODES.iter().chain(&EVENT_MODES).copied().collect(),
    };
    let existing = state.is_some();
    let mut values = match &state {
        Some(s) => {
            let v = values_of(s);
            let fp = fingerprint(&v, &BTreeMap::new());
            let mut v = v;
            v.insert("dbine_state".into(), json!(fp));
            v
        }
        None => BTreeMap::from([
            ("owner".to_string(), json!(ctx.user())),
            ("schedule_mode".to_string(), json!("calendar")),
            ("time_zone".to_string(), json!(ctx.default_tz())),
            ("freq".to_string(), json!("DAILY")),
            ("interval".to_string(), json!(1)),
        ]),
    };
    values.entry("comments".into()).or_insert(json!(""));
    let general = FormSection::new("general", "General").fields(vec![
        FormField::new("owner", "Esquema", FormKind::reference("schema", false))
            .read_only(existing || !ctx.create_any(Kind::Schedule))
            .default(json!(ctx.user())),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ]);
    let when = FormSection::new("schedule", "Programación")
        .fields(timing::fields(&modes, &ctx.default_tz()));
    let mut f = ObjectForm {
        sections: vec![general, when],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    match &state {
        Some(s) => {
            if s.event() {
                f.notes.push(
                    "Es una programación por evento: no se puede cambiar a una por tiempo.".into(),
                );
            }
            if !ctx.may_change(
                Kind::Schedule,
                &s.owner,
                &s.name,
                ctx.oracle_schema(&s.owner),
            ) {
                f.read_only = true;
                f.notes.insert(0, OWNER_ONLY.into());
            }
        }
        None if !ctx.may_create(Kind::Schedule) => {
            f.read_only = true;
            f.notes.push(super::NO_CREATE_JOB.into());
        }
        None => {}
    }
    Ok(f)
}
