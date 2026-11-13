//! Job classes: `CREATE_JOB_CLASS`, `SET_ATTRIBUTE` (consumer group,
//! service, logging level, log history, comments), `DROP_JOB_CLASS`. They
//! are SYS's and need MANAGE SCHEDULER. The ones Oracle installed
//! (`ORACLE_MAINTAINED`) are shown, not changed nor dropped.

use super::choices::{self, Catalog};
use super::read::{self, Rec};
use super::{
    bad, fingerprint, fits, fresh, installed, installed_by, new_name, obj_id, parse_obj, pick_ref,
    unless, Block, Ctx, Plan, Vals, A, MANAGE_ONLY, ORACLE_OWN,
};
use dbine_driver::{
    FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem, SchedulerList,
};
use oracledb::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const LOGGING: [&str; 4] = ["OFF", "RUNS", "FAILED_RUNS", "FULL"];
const SOURCES: [&str; 2] = ["consumer_group", "service"];
const TYPES: &str = "'JOB CLASS'";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassSpec {
    pub(crate) name: String,
    pub(crate) consumer_group: Option<String>,
    pub(crate) service: Option<String>,
    pub(crate) logging: String,
    pub(crate) log_history: Option<i64>,
    pub(crate) comments: Option<String>,
}

fn sys(name: &str) -> String {
    obj_id(Some("SYS"), name)
}

/// Why the login can't change (nor drop) the class, if it can't: it
/// lacks MANAGE SCHEDULER, or Oracle installed the class.
fn locked(c: &Connection, ctx: &Ctx, name: &str) -> Option<&'static str> {
    if !ctx.manage() {
        Some(MANAGE_ONLY)
    } else if installed_by(&installed(c, ctx, TYPES), name, oracle_class) {
        Some(ORACLE_OWN)
    } else {
        None
    }
}

fn refuse(why: Option<&'static str>) -> Result<()> {
    match why {
        Some(m) => Err(bad(m.into())),
        None => Ok(()),
    }
}

/// The classes Oracle installs, by their names (where the dictionary
/// doesn't say which are Oracle's, before 12.1).
fn oracle_class(name: &str) -> bool {
    name.contains('$')
        || matches!(
            name,
            "DEFAULT_JOB_CLASS" | "DEFAULT_IN_MEMORY_JOB_CLASS" | "XMLDB_NFS_JOBCLASS"
        )
}

pub(crate) fn spec_of_rec(r: &Rec) -> ClassSpec {
    ClassSpec {
        name: r.raw("JOB_CLASS_NAME").unwrap_or_default(),
        consumer_group: r.t("RESOURCE_CONSUMER_GROUP"),
        service: r.t("SERVICE"),
        logging: r
            .t("LOGGING_LEVEL")
            .map(|l| l.replace(' ', "_"))
            .filter(|l| LOGGING.contains(&l.as_str()))
            .unwrap_or_else(|| "RUNS".into()),
        log_history: r.int("LOG_HISTORY"),
        comments: r.t("COMMENTS"),
    }
}

fn read_state(c: &Connection, ctx: &Ctx, name: &str) -> Result<ClassSpec> {
    read::row(
        c,
        &ctx.sql("{v}_scheduler_job_classes"),
        "WHERE job_class_name = :1",
        &[&name],
    )
    .map_err(crate::err)?
    .map(|r| spec_of_rec(&r))
    .ok_or_else(|| bad(format!("no existe la clase de job «{}»", sys(name))))
}

pub(crate) fn values_of(s: &ClassSpec) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("name".to_string(), json!(s.name)),
        (
            "consumer_group".to_string(),
            json!(s.consumer_group.clone().unwrap_or_default()),
        ),
        (
            "service".to_string(),
            json!(s.service.clone().unwrap_or_default()),
        ),
        ("logging_level".to_string(), json!(s.logging)),
        ("log_history".to_string(), json!(s.log_history)),
        (
            "comments".to_string(),
            json!(s.comments.clone().unwrap_or_default()),
        ),
    ])
}

/// A name from a list the login may not be able to read: checked against
/// it when it can, else only its shape.
fn listed(
    v: Vals,
    key: &str,
    cat: &Catalog,
    source: &str,
    now: Option<&str>,
    label: &str,
) -> Result<Option<String>> {
    match v.opt(key) {
        None => Ok(None),
        Some(t) if cat.readable(source) => pick_ref(cat, source, &t, now, label).map(Some),
        Some(t) => {
            let u = if crate::jobs::plain(&t.to_ascii_uppercase()) {
                t.to_ascii_uppercase()
            } else {
                t
            };
            if u.chars()
                .any(|c| c.is_control() || c == '\'' || c == '"' || c == ',')
            {
                return Err(bad(format!("«{label}» no es un nombre válido.")));
            }
            Ok(Some(u))
        }
    }
}

pub(crate) fn spec_of(
    values: &FormValues,
    ctx: &Ctx,
    cat: &Catalog,
    cur: Option<&ClassSpec>,
) -> Result<ClassSpec> {
    let v = Vals(&values.values);
    let name = match cur {
        Some(s) => s.name.clone(),
        None => new_name(&v.raw("name"), ctx, "Nombre")?,
    };
    let comments = v.opt("comments");
    fits(&comments, "Descripción", 4000)?;
    Ok(ClassSpec {
        consumer_group: listed(
            v,
            "consumer_group",
            cat,
            "consumer_group",
            cur.and_then(|c| c.consumer_group.as_deref()),
            "Grupo de consumidores",
        )?,
        service: listed(
            v,
            "service",
            cat,
            "service",
            cur.and_then(|c| c.service.as_deref()),
            "Servicio",
        )?,
        logging: v.choice("logging_level", "Nivel de registro", &LOGGING, "RUNS")?,
        log_history: v.int("log_history", "Días de historial", 0, 1_000_000)?,
        comments,
        name,
    })
}

fn logging_arg(l: &str) -> A {
    A::R(format!("DBMS_SCHEDULER.LOGGING_{l}"))
}

pub(crate) fn create_block(s: &ClassSpec) -> Block {
    let id = sys(&s.name);
    let mut b = Block::new(&format!("Crea la clase de job {id}."));
    let mut args = vec![("job_class_name", A::Name(id))];
    if let Some(g) = &s.consumer_group {
        args.push(("resource_consumer_group", A::S(g.clone())));
    }
    if let Some(sv) = &s.service {
        args.push(("service", A::S(sv.clone())));
    }
    args.push(("logging_level", logging_arg(&s.logging)));
    if let Some(h) = s.log_history {
        args.push(("log_history", A::I(h)));
    }
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    b.call("DBMS_SCHEDULER.CREATE_JOB_CLASS", args);
    b
}

pub(crate) fn alter_block(old: &ClassSpec, new: &ClassSpec) -> Block {
    let id = sys(&old.name);
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia la clase de job {id}."));
    if old.consumer_group != new.consumer_group {
        b.set_attr(
            &target,
            "resource_consumer_group",
            A::opt(new.consumer_group.clone()),
        );
    }
    if old.service != new.service {
        b.set_attr(&target, "service", A::opt(new.service.clone()));
    }
    if old.logging != new.logging {
        b.set_attr(&target, "logging_level", logging_arg(&new.logging));
    }
    if old.log_history != new.log_history {
        b.set_attr(&target, "log_history", A::int(new.log_history));
    }
    if old.comments != new.comments {
        b.set_attr(&target, "comments", A::opt(new.comments.clone()));
    }
    b
}

pub(crate) fn items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    let rows = read::rows(
        c,
        &ctx.sql("{v}_scheduler_job_classes"),
        "ORDER BY job_class_name",
        &[],
        10_000,
    )
    .map_err(crate::err)?;
    let oracle = installed(c, ctx, TYPES);
    Ok(SchedulerList {
        items: rows
            .iter()
            .map(|r| {
                let name = r.raw("JOB_CLASS_NAME").unwrap_or_default();
                let own = installed_by(&oracle, &name, oracle_class);
                SchedulerItem {
                    id: sys(&name),
                    system: own,
                    can_edit: unless(own),
                    can_delete: unless(own),
                    actions: None,
                    name,
                    description: r.t("COMMENTS"),
                    enabled: None,
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

fn sys_name(id: &str) -> Result<String> {
    let (owner, name) = parse_obj(id, "clase de job")?;
    if owner != "SYS" {
        return Err(bad(format!(
            "«{}» no es un id de clase de job de Oracle (las clases son de SYS)",
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
            let name = sys_name(id)?;
            let old = read_state(c, ctx, &name)?;
            refuse(locked(c, ctx, &name))?;
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
    let name = sys_name(id)?;
    read_state(c, ctx, &name)?;
    refuse(locked(c, ctx, &name))?;
    let mut b = Block::new(&format!("Borra la clase de job {}.", sys(&name)));
    b.note("Sin force, Oracle no la borra mientras algún job sea de esta clase.");
    b.call(
        "DBMS_SCHEDULER.DROP_JOB_CLASS",
        vec![
            ("job_class_name", A::Name(sys(&name))),
            ("force", A::B(false)),
        ],
    );
    Ok(b)
}

pub(crate) fn form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &SOURCES);
    let state = match id {
        Some(id) => Some(read_state(c, ctx, &sys_name(id)?)?),
        None => None,
    };
    let existing = state.is_some();
    let values = match &state {
        Some(s) => {
            let mut v = values_of(s);
            let fp = fingerprint(&v, &BTreeMap::new());
            v.insert("dbine_state".into(), json!(fp));
            v
        }
        None => BTreeMap::from([
            ("logging_level".to_string(), json!("RUNS")),
            ("comments".to_string(), json!("")),
        ]),
    };
    let field = |key: &str, label: &str, source: &str, placeholder: &str| {
        if cat.readable(source) {
            FormField::new(key, label, FormKind::reference(source, false))
        } else {
            FormField::new(key, label, FormKind::Text).placeholder(placeholder)
        }
    };
    let general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        field("consumer_group", "Grupo de consumidores", "consumer_group", "BATCH_GROUP")
            .help("El grupo del Resource Manager en el que se ejecutan sus jobs."),
        field("service", "Servicio", "service", "BATCH_SVC")
            .help("En RAC, el servicio (y con él las instancias) donde se ejecutan sus jobs."),
        FormField::new(
            "logging_level",
            "Nivel de registro",
            FormKind::select(&[
                ("OFF", "Sin registro"),
                ("RUNS", "Ejecuciones"),
                ("FAILED_RUNS", "Ejecuciones con error"),
                ("FULL", "Completo (también los cambios de los jobs)"),
            ]),
        )
        .default(json!("RUNS")),
        FormField::new(
            "log_history",
            "Días de historial",
            FormKind::number(Some(0.0), Some(1_000_000.0)),
        )
        .help("Cuántos días se guarda el registro de sus jobs. Vacío: lo que diga el atributo log_history del Scheduler."),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ]);
    let mut f = ObjectForm {
        sections: vec![general],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    let why = match &state {
        Some(s) => locked(c, ctx, &s.name),
        None => (!ctx.manage()).then_some(MANAGE_ONLY),
    };
    if let Some(why) = why {
        f.read_only = true;
        f.notes.push(why.into());
    }
    Ok(f)
}
