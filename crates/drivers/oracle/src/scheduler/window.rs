//! Windows (`CREATE_WINDOW`, `SET_ATTRIBUTE`, `DROP_WINDOW`) and window
//! groups (`CREATE_GROUP` of type WINDOW with `ADD_GROUP_MEMBER` /
//! `REMOVE_GROUP_MEMBER` from 12c, `CREATE_WINDOW_GROUP` and its member
//! calls before; `DROP_GROUP` / `DROP_WINDOW_GROUP`). Both are SYS's and
//! need MANAGE SCHEDULER.
//!
//! What Oracle installed (`ORACLE_MAINTAINED`) isn't dropped. Its
//! maintenance windows and `MAINTENANCE_WINDOW_GROUP` can be changed, as
//! Oracle documents (when the automatic maintenance tasks run, which
//! windows they use); its internal ones (`ORA$…`) can't.

use super::action::{CLOSE_WINDOW, OPEN_WINDOW};
use super::choices::{self, Catalog};
use super::read::{self, Rec};
use super::script::name_lit;
use super::timing::{self, When};
use super::{
    bad, fingerprint, fits, fresh, installed, installed_by, new_name, obj_id, parse_obj, pick_ref,
    secs, unless, Block, Ctx, Plan, Vals, A, MANAGE_ONLY, ORACLE_NO_DROP, ORACLE_OWN,
};
use dbine_driver::{
    FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem, SchedulerList,
};
use oracledb::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

const MODES: [&str; 4] = ["once", "calendar", "expression", "named"];
const SOURCES: [&str; 2] = ["resource_plan", "schedule"];

/// The dictionary's types of windows and of window groups (12c's groups,
/// 11.2's window groups).
const WINDOW_TYPES: &str = "'WINDOW'";
const GROUP_TYPES: &str = "'SCHEDULER GROUP', 'WINDOW GROUP'";

pub(crate) fn sys(name: &str) -> String {
    obj_id(Some("SYS"), name)
}

/// The maintenance windows and groups Oracle installs, by their names
/// (where the dictionary doesn't say which are Oracle's, before 12.1).
fn oracle_window(name: &str) -> bool {
    name.ends_with("DAY_WINDOW")
        || name.starts_with("WEEK")
        || name.starts_with("ORA$")
        || name == "MAINTENANCE_WINDOW_GROUP"
}

/// Why the login can't change the window or group `name` (Oracle's when
/// `oracle`), if it can't.
pub(crate) fn edit_locked(ctx: &Ctx, name: &str, oracle: bool) -> Option<&'static str> {
    if !ctx.manage() {
        Some(MANAGE_ONLY)
    } else if oracle && name.starts_with("ORA$") {
        Some(ORACLE_OWN)
    } else {
        None
    }
}

/// Why the login can't drop it, if it can't: why it can't change it (an
/// internal one of Oracle's says it can't be changed either), else
/// Oracle installed it.
pub(crate) fn drop_locked(ctx: &Ctx, name: &str, oracle: bool) -> Option<&'static str> {
    edit_locked(ctx, name, oracle).or(oracle.then_some(ORACLE_NO_DROP))
}

fn refuse(why: Option<&'static str>) -> Result<()> {
    match why {
        Some(m) => Err(bad(m.into())),
        None => Ok(()),
    }
}

/// The actions a window offers now: close it while it's open, open it
/// while it's closed and enabled (Oracle doesn't open a disabled one).
pub(crate) fn window_actions(r: &Rec) -> Vec<String> {
    if r.yes("ACTIVE") {
        vec![CLOSE_WINDOW.to_string()]
    } else if r.yes("ENABLED") {
        vec![OPEN_WINDOW.to_string()]
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WindowSpec {
    pub(crate) name: String,
    pub(crate) resource_plan: Option<String>,
    pub(crate) when: When,
    pub(crate) duration: i64,
    pub(crate) priority: String,
    pub(crate) enabled: bool,
    pub(crate) comments: Option<String>,
}

pub(crate) fn spec_of_rec(r: &Rec) -> WindowSpec {
    WindowSpec {
        name: r.raw("WINDOW_NAME").unwrap_or_default(),
        resource_plan: r.t("RESOURCE_PLAN"),
        when: When::of(r),
        duration: r.int("DURATION").unwrap_or(0),
        priority: r.t("WINDOW_PRIORITY").unwrap_or_else(|| "LOW".into()),
        enabled: r.yes("ENABLED"),
        comments: r.t("COMMENTS"),
    }
}

pub(crate) fn read_state(c: &Connection, ctx: &Ctx, name: &str) -> Result<(WindowSpec, Rec)> {
    let r = read::row(
        c,
        &ctx.sql("{v}_scheduler_windows"),
        "WHERE window_name = :1",
        &[&name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| bad(format!("no existe la ventana «{}»", sys(name))))?;
    Ok((spec_of_rec(&r), r))
}

pub(crate) fn values_of(s: &WindowSpec) -> BTreeMap<String, Value> {
    let mut v = BTreeMap::new();
    let mut put = |k: &str, x: Value| {
        v.insert(k.to_string(), x);
    };
    put("name", json!(s.name));
    put(
        "resource_plan",
        json!(s.resource_plan.clone().unwrap_or_default()),
    );
    s.when.values(&mut put);
    put("duration", json!(s.duration));
    put("window_priority", json!(s.priority));
    put("enabled", json!(s.enabled));
    put("comments", json!(s.comments.clone().unwrap_or_default()));
    v
}

pub(crate) fn spec_of(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&WindowSpec>,
) -> Result<WindowSpec> {
    let v = Vals(&values.values);
    let name = match cur {
        Some(s) => s.name.clone(),
        None => new_name(&v.raw("name"), ctx, "Nombre")?,
    };
    let mut when = When::from_values(v, &MODES)?;
    if let When::Named(n) = &when {
        let now = cur.and_then(|c| c.when.schedule_name());
        when = When::Named(pick_ref(cat, "schedule", n, now, "Programación")?);
    }
    let duration = secs(v, "duration", "Duración")?
        .filter(|n| *n > 0)
        .ok_or_else(|| bad("Falta «Duración»: cuánto está abierta la ventana.".into()))?;
    let resource_plan = match v.opt("resource_plan") {
        Some(p) if cat.readable("resource_plan") => Some(pick_ref(
            cat,
            "resource_plan",
            &p,
            cur.and_then(|c| c.resource_plan.as_deref()),
            "Plan de recursos",
        )?),
        Some(p) => {
            if !crate::jobs::plain(&p.to_ascii_uppercase()) {
                return Err(bad("«Plan de recursos» no es un nombre de plan.".into()));
            }
            Some(p.to_ascii_uppercase())
        }
        None => None,
    };
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(WindowSpec {
        name,
        resource_plan,
        when,
        duration,
        priority: v.choice("window_priority", "Prioridad", &["LOW", "HIGH"], "LOW")?,
        enabled: v.bool_or("enabled", true),
        comments,
    })
}

pub(crate) fn create_block(s: &WindowSpec) -> Block {
    let id = sys(&s.name);
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Crea la ventana {id}."));
    let mut args = vec![
        ("window_name", target.clone()),
        ("resource_plan", A::opt(s.resource_plan.clone())),
    ];
    args.extend(timing::create_args(&s.when));
    if matches!(s.when, When::Once(_)) {
        args.push(("repeat_interval", A::Null));
    }
    args.push(("duration", A::secs(Some(s.duration))));
    args.push(("window_priority", A::S(s.priority.clone())));
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    b.call("DBMS_SCHEDULER.CREATE_WINDOW", args);
    if !s.enabled {
        b.created(&format!(
            "DBMS_SCHEDULER.DROP_WINDOW(window_name => {}, force => TRUE);",
            name_lit(&id)
        ));
        b.call(
            "DBMS_SCHEDULER.DISABLE",
            vec![("name", target), ("force", A::B(true))],
        );
    }
    b
}

pub(crate) fn alter_block(old: &WindowSpec, new: &WindowSpec) -> Block {
    let id = sys(&old.name);
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia la ventana {id}."));
    timing::check_first(&mut b, &old.when, &new.when);
    if old.resource_plan != new.resource_plan {
        b.set_attr(&target, "resource_plan", A::opt(new.resource_plan.clone()));
    }
    timing::changes(&mut b, &target, &old.when, &new.when);
    if old.duration != new.duration {
        b.set_attr(&target, "duration", A::secs(Some(new.duration)));
    }
    if old.priority != new.priority {
        b.set_attr(&target, "window_priority", A::S(new.priority.clone()));
    }
    if old.comments != new.comments {
        b.set_attr(&target, "comments", A::opt(new.comments.clone()));
    }
    if old.enabled != new.enabled {
        if new.enabled {
            b.pcall("DBMS_SCHEDULER.ENABLE", vec![target]);
        } else {
            b.call(
                "DBMS_SCHEDULER.DISABLE",
                vec![("name", target), ("force", A::B(true))],
            );
        }
    }
    b
}

pub(crate) fn items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_windows"),
        "ORDER BY window_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    let oracle: Option<HashSet<String>> = installed(c, ctx, WINDOW_TYPES);
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let name = r.raw("WINDOW_NAME").unwrap_or_default();
                let own = installed_by(&oracle, &name, oracle_window);
                SchedulerItem {
                    id: sys(&name),
                    system: own,
                    can_edit: unless(edit_locked(ctx, &name, own).is_some()),
                    can_delete: unless(drop_locked(ctx, &name, own).is_some()),
                    actions: ctx.manage().then(|| window_actions(r)),
                    name,
                    description: r.t("COMMENTS"),
                    enabled: Some(r.yes("ENABLED")),
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

pub(crate) fn sys_name(id: &str, what: &str) -> Result<String> {
    let (owner, name) = parse_obj(id, what)?;
    if owner != "SYS" {
        return Err(bad(format!(
            "«{}» no es un id de {what} de Oracle (las ventanas son de SYS)",
            super::shown(id)
        )));
    }
    Ok(name)
}

pub(crate) fn plan(
    c: &Connection,
    ctx: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    if !ctx.manage() {
        return Err(bad(MANAGE_ONLY.into()));
    }
    let cat = choices::load(c, ctx, &SOURCES);
    match id {
        None => {
            let s = spec_of(values, ctx, &cat, None)?;
            Ok(Plan::new(create_block(&s), sys(&s.name), &s.name))
        }
        Some(id) => {
            let name = sys_name(id, "ventana")?;
            let (old, _) = read_state(c, ctx, &name)?;
            let own = installed_by(&installed(c, ctx, WINDOW_TYPES), &name, oracle_window);
            refuse(edit_locked(ctx, &name, own))?;
            fresh(values, &fingerprint(&values_of(&old), &BTreeMap::new()))?;
            let new = spec_of(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(alter_block(&old, &new), sys(&name), &name))
        }
    }
}

pub(crate) fn delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    if !ctx.manage() {
        return Err(bad(MANAGE_ONLY.into()));
    }
    let name = sys_name(id, "ventana")?;
    read_state(c, ctx, &name)?;
    let own = installed_by(&installed(c, ctx, WINDOW_TYPES), &name, oracle_window);
    refuse(drop_locked(ctx, &name, own))?;
    let mut b = Block::new(&format!("Borra la ventana {}.", sys(&name)));
    b.note("Sin force, Oracle no la borra si está abierta o la usa un job.");
    b.call(
        "DBMS_SCHEDULER.DROP_WINDOW",
        vec![("window_name", A::Name(sys(&name))), ("force", A::B(false))],
    );
    Ok(b)
}

pub(crate) fn form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &SOURCES);
    let state = match id {
        Some(id) => Some(read_state(c, ctx, &sys_name(id, "ventana")?)?),
        None => None,
    };
    let existing = state.is_some();
    let mut values = match &state {
        Some((s, r)) => {
            let mut v = values_of(s);
            let fp = fingerprint(&v, &BTreeMap::new());
            v.insert("dbine_state".into(), json!(fp));
            for (k, col) in [
                ("next_start_date", "NEXT_START_DATE"),
                ("last_start_date", "LAST_START_DATE"),
            ] {
                let t = timing::ts_of(r, col)
                    .map(|t| format!("{} {}", t.local.replace('T', " "), t.tz))
                    .unwrap_or_default();
                v.insert(k.into(), json!(t));
            }
            v.insert(
                "active".into(),
                json!(if r.yes("ACTIVE") { "Sí" } else { "No" }),
            );
            v
        }
        None => BTreeMap::from([
            ("schedule_mode".to_string(), json!("calendar")),
            ("time_zone".to_string(), json!(ctx.default_tz())),
            ("freq".to_string(), json!("DAILY")),
            ("interval".to_string(), json!(1)),
            ("window_priority".to_string(), json!("LOW")),
            ("enabled".to_string(), json!(true)),
            ("duration".to_string(), json!(3600)),
        ]),
    };
    values.entry("comments".into()).or_insert(json!(""));
    let plan_field = if cat.readable("resource_plan") {
        FormField::new(
            "resource_plan",
            "Plan de recursos",
            FormKind::reference("resource_plan", false),
        )
    } else {
        FormField::new("resource_plan", "Plan de recursos", FormKind::Text)
            .placeholder("DEFAULT_MAINTENANCE_PLAN")
    };
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        plan_field.help("El plan del Resource Manager que se activa mientras la ventana está abierta. Vacío: no cambia el plan."),
        FormField::new("duration", "Duración", FormKind::Duration).required(),
        FormField::new(
            "window_priority",
            "Prioridad",
            FormKind::select(&[("LOW", "Baja"), ("HIGH", "Alta")]),
        )
        .help("Si dos ventanas se superponen, se abre la de prioridad alta.")
        .default(json!("LOW")),
        FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ]);
    if existing {
        general = general.fields(vec![
            FormField::new("active", "Abierta ahora", FormKind::Text).read_only(true),
            FormField::new("next_start_date", "Próxima apertura", FormKind::Text).read_only(true),
            FormField::new("last_start_date", "Última apertura", FormKind::Text).read_only(true),
        ]);
    }
    let when = FormSection::new("schedule", "Programación")
        .fields(timing::fields(&MODES, &ctx.default_tz()));
    let mut f = ObjectForm {
        sections: vec![general, when],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    let own = state.as_ref().is_some_and(|(s, _)| {
        installed_by(&installed(c, ctx, WINDOW_TYPES), &s.name, oracle_window)
    });
    match &state {
        Some((s, _)) => {
            if let Some(why) = edit_locked(ctx, &s.name, own) {
                f.read_only = true;
                f.notes.push(why.into());
            } else if own {
                f.notes.push("Es una ventana de mantenimiento de Oracle: cambiarla cambia cuándo corren sus tareas automáticas (estadísticas, segmentos, ajuste de SQL).".into());
            }
        }
        None if !ctx.manage() => {
            f.read_only = true;
            f.notes.push(MANAGE_ONLY.into());
        }
        None => {}
    }
    Ok(f)
}

// ---------------------------------------------------------------------------
// Window groups
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupSpec {
    pub(crate) name: String,
    pub(crate) members: Vec<String>,
    pub(crate) enabled: bool,
    pub(crate) comments: Option<String>,
}

fn read_group(c: &Connection, ctx: &Ctx, name: &str) -> Result<GroupSpec> {
    let r = read::row(
        c,
        &ctx.sql("{v}_scheduler_window_groups"),
        "WHERE window_group_name = :1",
        &[&name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| bad(format!("no existe el grupo de ventanas «{}»", sys(name))))?;
    let members = read::rows(
        c,
        &ctx.sql("{v}_scheduler_wingroup_members"),
        "WHERE window_group_name = :1 ORDER BY window_name",
        &[&name],
        1000,
    )
    .map_err(crate::err)?;
    Ok(GroupSpec {
        name: name.to_string(),
        members: members
            .iter()
            .filter_map(|m| m.raw("WINDOW_NAME"))
            .map(|w| sys(&w))
            .collect(),
        enabled: r.yes("ENABLED"),
        comments: r.t("COMMENTS"),
    })
}

pub(crate) fn group_values(s: &GroupSpec) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("name".to_string(), json!(s.name)),
        ("members".to_string(), json!(s.members)),
        ("enabled".to_string(), json!(s.enabled)),
        (
            "comments".to_string(),
            json!(s.comments.clone().unwrap_or_default()),
        ),
    ])
}

pub(crate) fn group_spec(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&GroupSpec>,
) -> Result<GroupSpec> {
    let v = Vals(&values.values);
    let name = match cur {
        Some(s) => s.name.clone(),
        None => new_name(&v.raw("name"), ctx, "Nombre")?,
    };
    let mut members = Vec::new();
    for m in v.strings("members") {
        let now = cur
            .and_then(|c| c.members.iter().find(|x| **x == m))
            .map(String::as_str);
        let id = pick_ref(cat, "window", &m, now, "Ventanas")?;
        if !members.contains(&id) {
            members.push(id);
        }
    }
    members.sort();
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(GroupSpec {
        name,
        members,
        enabled: v.bool_or("enabled", true),
        comments,
    })
}

/// `'"SYS"."A", "SYS"."B"'`.
pub(crate) fn member_list(members: &[String]) -> A {
    A::R(super::script::lit(
        &members
            .iter()
            .filter_map(|m| parse_obj(m, "ventana").ok())
            .map(|(o, n)| format!("{}.{}", crate::quote(&o), crate::quote(&n)))
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

pub(crate) fn group_create(s: &GroupSpec, ctx: &Ctx) -> Block {
    let id = sys(&s.name);
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Crea el grupo de ventanas {id}."));
    let members = if s.members.is_empty() {
        A::Null
    } else {
        member_list(&s.members)
    };
    if ctx.groups_api() {
        b.call(
            "DBMS_SCHEDULER.CREATE_GROUP",
            vec![
                ("group_name", target.clone()),
                ("group_type", A::S("WINDOW".into())),
                ("member", members),
                ("comments", A::opt(s.comments.clone())),
            ],
        );
    } else {
        b.call(
            "DBMS_SCHEDULER.CREATE_WINDOW_GROUP",
            vec![
                ("group_name", target.clone()),
                ("window_list", members),
                ("comments", A::opt(s.comments.clone())),
            ],
        );
    }
    if !s.enabled {
        b.created(&format!(
            "DBMS_SCHEDULER.{}(group_name => {}, force => TRUE);",
            if ctx.groups_api() {
                "DROP_GROUP"
            } else {
                "DROP_WINDOW_GROUP"
            },
            name_lit(&id)
        ));
        b.pcall("DBMS_SCHEDULER.DISABLE", vec![target]);
    }
    b
}

pub(crate) fn group_alter(old: &GroupSpec, new: &GroupSpec, ctx: &Ctx) -> Block {
    let id = sys(&old.name);
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia el grupo de ventanas {id}."));
    let (add, remove, key) = if ctx.groups_api() {
        ("ADD_GROUP_MEMBER", "REMOVE_GROUP_MEMBER", "member")
    } else {
        (
            "ADD_WINDOW_GROUP_MEMBER",
            "REMOVE_WINDOW_GROUP_MEMBER",
            "window_list",
        )
    };
    for m in old.members.iter().filter(|m| !new.members.contains(m)) {
        b.call(
            &format!("DBMS_SCHEDULER.{remove}"),
            vec![
                ("group_name", target.clone()),
                (key, member_list(std::slice::from_ref(m))),
            ],
        );
    }
    for m in new.members.iter().filter(|m| !old.members.contains(m)) {
        b.call(
            &format!("DBMS_SCHEDULER.{add}"),
            vec![
                ("group_name", target.clone()),
                (key, member_list(std::slice::from_ref(m))),
            ],
        );
    }
    if old.comments != new.comments {
        b.set_attr(&target, "comments", A::opt(new.comments.clone()));
    }
    if old.enabled != new.enabled {
        let proc_name = if new.enabled {
            "DBMS_SCHEDULER.ENABLE"
        } else {
            "DBMS_SCHEDULER.DISABLE"
        };
        b.pcall(proc_name, vec![target]);
    }
    b
}

pub(crate) fn group_items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_window_groups"),
        "ORDER BY window_group_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    let oracle = installed(c, ctx, GROUP_TYPES);
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let name = r.raw("WINDOW_GROUP_NAME").unwrap_or_default();
                let own = installed_by(&oracle, &name, oracle_window);
                SchedulerItem {
                    id: sys(&name),
                    system: own,
                    can_edit: unless(edit_locked(ctx, &name, own).is_some()),
                    can_delete: unless(drop_locked(ctx, &name, own).is_some()),
                    actions: None,
                    name,
                    description: r.t("COMMENTS"),
                    enabled: Some(r.yes("ENABLED")),
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

pub(crate) fn group_plan(
    c: &Connection,
    ctx: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    if !ctx.manage() {
        return Err(bad(MANAGE_ONLY.into()));
    }
    let cat = choices::load(c, ctx, &["window"]);
    match id {
        None => {
            let s = group_spec(values, ctx, &cat, None)?;
            Ok(Plan::new(group_create(&s, ctx), sys(&s.name), &s.name))
        }
        Some(id) => {
            let name = sys_name(id, "grupo de ventanas")?;
            let old = read_group(c, ctx, &name)?;
            let own = installed_by(&installed(c, ctx, GROUP_TYPES), &name, oracle_window);
            refuse(edit_locked(ctx, &name, own))?;
            fresh(values, &fingerprint(&group_values(&old), &BTreeMap::new()))?;
            let new = group_spec(values, ctx, &cat, Some(&old))?;
            Ok(Plan::changes(
                group_alter(&old, &new, ctx),
                sys(&name),
                &name,
            ))
        }
    }
}

pub(crate) fn group_delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    if !ctx.manage() {
        return Err(bad(MANAGE_ONLY.into()));
    }
    let name = sys_name(id, "grupo de ventanas")?;
    read_group(c, ctx, &name)?;
    let own = installed_by(&installed(c, ctx, GROUP_TYPES), &name, oracle_window);
    refuse(drop_locked(ctx, &name, own))?;
    let mut b = Block::new(&format!(
        "Borra el grupo de ventanas {} (sus ventanas quedan).",
        sys(&name)
    ));
    b.call(
        if ctx.groups_api() {
            "DBMS_SCHEDULER.DROP_GROUP"
        } else {
            "DBMS_SCHEDULER.DROP_WINDOW_GROUP"
        },
        vec![("group_name", A::Name(sys(&name))), ("force", A::B(false))],
    );
    Ok(b)
}

pub(crate) fn group_form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &["window"]);
    let state = match id {
        Some(id) => Some(read_group(c, ctx, &sys_name(id, "grupo de ventanas")?)?),
        None => None,
    };
    let existing = state.is_some();
    let mut values = match &state {
        Some(s) => {
            let mut v = group_values(s);
            let fp = fingerprint(&v, &BTreeMap::new());
            v.insert("dbine_state".into(), json!(fp));
            v
        }
        None => BTreeMap::from([
            ("enabled".to_string(), json!(true)),
            ("comments".to_string(), json!("")),
        ]),
    };
    values.entry("members".into()).or_insert(json!([]));
    let general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new("members", "Ventanas", FormKind::reference("window", true)),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ]);
    let mut f = ObjectForm {
        sections: vec![general],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    let why = match &state {
        Some(s) => {
            let own = installed_by(&installed(c, ctx, GROUP_TYPES), &s.name, oracle_window);
            edit_locked(ctx, &s.name, own)
        }
        None => (!ctx.manage()).then_some(MANAGE_ONLY),
    };
    if let Some(why) = why {
        f.read_only = true;
        f.notes.push(why.into());
    }
    Ok(f)
}
