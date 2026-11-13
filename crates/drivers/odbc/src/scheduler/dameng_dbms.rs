//! Dameng's other two schedulers, next to its job system
//! ([`super::dameng`]):
//!
//! - DBMS_JOB: a job is what it runs, its next date, the interval that
//!   gives the one after, and whether it's broken; `DBMS_JOB.SUBMIT`,
//!   `WHAT`, `NEXT_DATE`, `INTERVAL`, `BROKEN` and `REMOVE`, and
//!   `SP_ENABLE_JOB` for the job system's own switch. The id is the job
//!   number.
//! - DBMS_SCHEDULER: `CREATE_JOB`, `SET_ATTRIBUTE` (and
//!   `SET_ATTRIBUTE_NULL`) per attribute, `ENABLE` / `DISABLE`, `DROP_JOB`
//!   and `PURGE_LOG` for its history. A job that runs a program or follows
//!   a named schedule keeps them: those parts are shown read only. Ids are
//!   `"OWNER"."JOB"`.
//!
//! Both are changed by a DBA or the job's owner, in one transaction.

use super::*;
use crate::jobs::dameng::scheduler::{one as sched_one, read as sched_read};
use crate::jobs::dameng::{dbms_job_row, interval_expr, me, one};
use crate::jobs::{dotted, int, lit, parse_ts, plain, quote_pair, yes};
use dbine_driver::{FormField, FormKind, FormSection};
use serde_json::json;

const NOT_MINE: &str = "Solo un DBA o el dueño del job lo cambia o lo borra.";
const NOT_DBMS_JOB: &str = "Este job no es de DBMS_JOB.";
const SCHED_RENAME: &str = "DBMS_SCHEDULER no cambia el nombre de un job: se borra y se crea de nuevo; su historial se pierde.";
// What a rename's script warns about (each a sentence of its own).
const W_RENAME: &str =
    "DBMS_SCHEDULER no cambia el nombre de un job: se borra y se crea de nuevo con el nombre nuevo.";
const W_RENAME_HISTORY: &str =
    "El job nuevo empieza sin historial: el de sus ejecuciones anteriores se pierde.";
const W_RENAME_ATTRS: &str = "Los atributos del job que este formulario no muestra (como su clase) no se copian: el job nuevo los toma por defecto.";
const W_RENAME_COMMIT: &str = "DBMS_SCHEDULER puede confirmar cada llamada por su cuenta: si la creación falla, el job puede quedar borrado.";
// The same premise as W_RENAME_COMMIT (not yet tried on a DM server): an
// in-place edit of two or more calls can stop half way.
const W_CALLS: &str = "DBMS_SCHEDULER puede confirmar cada llamada por su cuenta: si una falla, las anteriores pueden quedar hechas.";
const W_AUTO_DROP: &str = "El job tiene AUTO_DROP: DM puede borrarlo al detenerlo.";
const PROGRAM_KEPT: &str = "El job corre un programa de DBMS_SCHEDULER: el programa se cambia aparte y se ve solo para lectura.";
const SCHEDULE_KEPT: &str = "El job sigue un calendario con nombre de DBMS_SCHEDULER: el calendario se cambia aparte y se ve solo para lectura.";
const ARGS_KEPT: &str =
    "El job toma argumentos que DBine no lee: lo que corre se ve solo para lectura.";
const TYPE_KEPT: &str = "El job es de un tipo que DBine no escribe (ni bloque PL/SQL ni procedimiento): lo que corre se ve solo para lectura.";
const KEPT_RENAME: &str = "Este job no puede cambiar de nombre desde DBine: DBMS_SCHEDULER lo borra y lo crea de nuevo, y al crearlo se perderían su programa, sus argumentos o su calendario con nombre.";
/// The job types the form writes.
const TYPES: &[&str] = &["PLSQL_BLOCK", "STORED_PROCEDURE"];

fn datetime_lit(ms: i64) -> String {
    format!("CAST('{}' AS DATETIME)", plain(ms))
}

fn ts_lit(ms: i64) -> String {
    format!("TIMESTAMP '{}'", plain(ms))
}

// ------------------------------------------------------------------ DBMS_JOB

#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct DbmsJob {
    pub(super) what: String,
    /// The server's local time, ms.
    pub(super) next: Option<i64>,
    pub(super) interval: Option<String>,
    pub(super) broken: bool,
    /// The job system's own switch (`SP_ENABLE_JOB`).
    pub(super) enabled: bool,
}

/// Job `id` of DBMS_JOB: its number, its job system row and its DBA_JOBS
/// row.
fn dbms_read(src: &mut dyn JobSource, id: &str) -> Result<(String, Set, Set, DbmsJob)> {
    let (s, t, n) = one(src, id)?;
    let d = dbms_job_row(src, &s, &t, &n)?.ok_or_else(|| bad(NOT_DBMS_JOB))?;
    let j = DbmsJob {
        what: d.get(0, "WHAT").unwrap_or_default().to_string(),
        next: d.get(0, "NEXT_DATE").and_then(parse_ts),
        interval: d.get(0, "INTERVAL").map(interval_expr),
        broken: yes(d.get(0, "BROKEN")),
        enabled: int(s.get(0, "ENABLE")) != Some(0),
    };
    Ok((n, s, d, j))
}

pub(super) fn dbms_job_sections(existing: bool) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("what", "Qué corre", FormKind::code("plsql"))
                .required()
                .help("Un bloque o una llamada PL/SQL, como lo toma DBMS_JOB.SUBMIT."),
            FormField::new("next_date", "Próxima ejecución", FormKind::DateTime)
                .help("Vacío al crearlo: ahora."),
            FormField::new("interval", "Intervalo", FormKind::Text)
                .help("La expresión que da la fecha de la siguiente ejecución. Vacío: corre una sola vez.")
                .placeholder("SYSDATE + 1"),
            FormField::new("broken", "Interrumpido (BROKEN)", FormKind::Bool).default(json!(false)),
            FormField::new("enabled", "Habilitado en el sistema de jobs", FormKind::Bool)
                .default(json!(true))
                .read_only(!existing)
                .help("El interruptor del sistema de jobs (SP_ENABLE_JOB); un job nuevo empieza habilitado."),
        ]),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("id", "Número", FormKind::Text).read_only(true),
            FormField::new("owner", "Dueño", FormKind::Text).read_only(true),
            FormField::new("last_date", "Última ejecución correcta", FormKind::Text)
                .read_only(true),
            FormField::new("failures", "Fallas seguidas", FormKind::Text).read_only(true),
        ]));
    }
    out
}

fn dbms_values(j: &DbmsJob) -> BTreeMap<String, Value> {
    values(vec![
        ("what", json!(j.what)),
        (
            "next_date",
            j.next.map_or(Value::Null, |ms| json!(dt_value(ms))),
        ),
        ("interval", jtext(j.interval.as_deref())),
        ("broken", json!(j.broken)),
        ("enabled", json!(j.enabled)),
    ])
}

pub(super) fn dbms_job_form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let Some(id) = id else {
        return Ok(ObjectForm {
            sections: dbms_job_sections(false),
            values: dbms_values(&DbmsJob {
                enabled: true,
                ..DbmsJob::default()
            }),
            ..ObjectForm::default()
        });
    };
    let (n, s, d, j) = dbms_read(src, id)?;
    let mine = me(src).manages(s.get(0, "USERNAME"));
    let mut v = dbms_values(&j);
    v.insert("id".into(), json!(n));
    v.insert("owner".into(), jtext(s.get(0, "USERNAME")));
    v.insert("last_date".into(), jtext(d.get(0, "LAST_DATE")));
    v.insert("failures".into(), jtext(d.get(0, "FAILURES")));
    Ok(ObjectForm {
        sections: dbms_job_sections(true),
        values: v,
        notes: if mine {
            Vec::new()
        } else {
            vec![NOT_MINE.to_string()]
        },
        read_only: !mine,
        ..ObjectForm::default()
    })
}

pub(super) fn dbms_job_from(values: &FormValues) -> Result<DbmsJob> {
    let v = Vals(&values.values);
    Ok(DbmsJob {
        what: code_text(v, "what", "Qué corre", 1_000_000)?
            .ok_or_else(|| bad("Falta «Qué corre»."))?,
        next: datetime(v, "next_date", "Próxima ejecución")?,
        interval: optional(v, "interval", "Intervalo", 4000)?,
        broken: v.bool_or("broken", false),
        enabled: v.bool_or("enabled", true),
    })
}

fn interval_arg(i: Option<&str>) -> String {
    i.map(lit).unwrap_or_else(|| "NULL".into())
}

pub(super) fn dbms_create_plan(j: &DbmsJob) -> Plan {
    let mut p = Plan::new("Job de DBMS_JOB de DM");
    p.atomic = true;
    let next = j.next.map(datetime_lit).unwrap_or_else(|| "SYSDATE".into());
    let mut block = format!(
        "DECLARE\n  JOB_NO INT;\nBEGIN\n  DBMS_JOB.SUBMIT(JOB_NO, {}, {next}, {});",
        lit(&j.what),
        interval_arg(j.interval.as_deref())
    );
    if j.broken {
        block.push_str("\n  DBMS_JOB.BROKEN(JOB_NO, TRUE);");
    }
    block.push_str("\nEND");
    p.exec(block);
    let what = j.what.clone();
    p.lookup = Some(Box::new(move |src: &mut dyn JobSource| {
        for t in ["DBA_JOBS", "USER_JOBS"] {
            if let Ok(s) = src.query(
                &format!("SELECT MAX(JOB) AS JOB FROM {t} WHERE WHAT = ?"),
                &[&what],
            ) {
                return Ok(int(s.get(0, "JOB")).map(|n| n.to_string()));
            }
        }
        Ok(None)
    }));
    p
}

pub(super) fn dbms_alter_plan(n: &str, name: &str, cur: &DbmsJob, j: &DbmsJob) -> Plan {
    let mut steps = Vec::new();
    if j.what.trim() != cur.what.trim() {
        steps.push(format!("CALL DBMS_JOB.WHAT({n}, {})", lit(&j.what)));
    }
    if j.next.is_some() && j.next != cur.next {
        steps.push(format!(
            "CALL DBMS_JOB.NEXT_DATE({n}, {})",
            datetime_lit(j.next.unwrap_or_default())
        ));
    }
    if !same(&j.interval, &cur.interval) {
        steps.push(format!(
            "CALL DBMS_JOB.INTERVAL({n}, {})",
            interval_arg(j.interval.as_deref())
        ));
    }
    if j.broken != cur.broken {
        steps.push(match (j.broken, j.next) {
            (false, Some(at)) => format!("CALL DBMS_JOB.BROKEN({n}, FALSE, {})", datetime_lit(at)),
            (b, _) => format!(
                "CALL DBMS_JOB.BROKEN({n}, {})",
                if b { "TRUE" } else { "FALSE" }
            ),
        });
    }
    if j.enabled != cur.enabled {
        steps.push(format!(
            "CALL SP_ENABLE_JOB({}, {})",
            lit(name),
            if j.enabled { 1 } else { 0 }
        ));
    }
    if steps.is_empty() {
        return Plan::nothing(&format!("DBMS_JOB {n}"), n.to_string());
    }
    let mut p = Plan::new(&format!("Job {n} de DBMS_JOB de DM"));
    p.atomic = true;
    for s in steps {
        p.exec(s);
    }
    p.id = Some(n.to_string());
    p
}

pub(super) fn dbms_job_plan(
    src: &mut dyn JobSource,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let j = dbms_job_from(values)?;
    let Some(id) = id else {
        return Ok(dbms_create_plan(&j));
    };
    let (n, s, _, cur) = dbms_read(src, id)?;
    if !me(src).manages(s.get(0, "USERNAME")) {
        return Err(Error::Unsupported(NOT_MINE.into()));
    }
    Ok(dbms_alter_plan(
        &n,
        s.get(0, "NAME").unwrap_or_default(),
        &cur,
        &j,
    ))
}

pub(super) fn dbms_remove_plan(n: &str) -> Plan {
    let mut p = Plan::new(&format!("Borra el job {n} de DBMS_JOB de DM"));
    p.atomic = true;
    p.exec(format!("CALL DBMS_JOB.REMOVE({n})"));
    p
}

pub(super) fn dbms_job_delete_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (n, s, _, _) = dbms_read(src, id)?;
    if !me(src).manages(s.get(0, "USERNAME")) {
        return Err(Error::Unsupported(NOT_MINE.into()));
    }
    Ok(dbms_remove_plan(&n))
}

// ------------------------------------------------------------------ DBMS_SCHEDULER

#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct SchedJob {
    pub(super) name: String,
    /// "PLSQL_BLOCK" or "STORED_PROCEDURE".
    pub(super) job_type: String,
    pub(super) action: String,
    pub(super) start: Option<i64>,
    pub(super) repeat: Option<String>,
    pub(super) end: Option<i64>,
    pub(super) enabled: bool,
    pub(super) auto_drop: bool,
    pub(super) comments: Option<String>,
}

/// What a job has that DBine leaves as it is.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Kept {
    /// What it runs: a program, arguments DBine doesn't read, or a type
    /// the form doesn't write (type and action are kept).
    pub(super) program: bool,
    /// A named schedule (start, repeat and end are kept).
    pub(super) schedule: bool,
}

fn sched_job_read(
    src: &mut dyn JobSource,
    id: &str,
) -> Result<(String, String, Set, SchedJob, Kept)> {
    let (s, _, owner, name) = sched_one(src, id)?;
    let j = SchedJob {
        name: name.clone(),
        job_type: s
            .get(0, "JOB_TYPE")
            .unwrap_or("PLSQL_BLOCK")
            .to_ascii_uppercase(),
        action: s.get(0, "JOB_ACTION").unwrap_or_default().to_string(),
        start: s.get(0, "START_DATE").and_then(parse_ts),
        repeat: s.get(0, "REPEAT_INTERVAL").map(str::to_string),
        end: s.get(0, "END_DATE").and_then(parse_ts),
        enabled: yes(s.get(0, "ENABLED")),
        auto_drop: yes(s.get(0, "AUTO_DROP")),
        comments: s.get(0, "COMMENTS").map(str::to_string),
    };
    let kept = Kept {
        program: s.get(0, "PROGRAM_NAME").is_some()
            || int(s.get(0, "NUMBER_OF_ARGUMENTS")).unwrap_or(0) > 0
            || !TYPES.contains(&j.job_type.as_str()),
        schedule: s.get(0, "SCHEDULE_NAME").is_some(),
    };
    Ok((owner, name, s, j, kept))
}

pub(super) fn scheduler_sections(existing: bool, kept: Kept) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .read_only(kept.program || kept.schedule)
                .help("Cambiarlo vuelve a crear el job (su historial se pierde)."),
            FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
            FormField::new("auto_drop", "Borrarlo al terminar", FormKind::Bool)
                .default(json!(false)),
            FormField::new("comments", "Comentario", FormKind::Textarea),
        ]),
        FormSection::new("action", "Qué corre").fields(vec![
            FormField::new(
                "job_type",
                "Tipo",
                FormKind::select(&[
                    ("PLSQL_BLOCK", "Bloque PL/SQL"),
                    ("STORED_PROCEDURE", "Procedimiento"),
                ]),
            )
            .default(json!("PLSQL_BLOCK"))
            .read_only(kept.program),
            FormField::new("action", "Acción", FormKind::code("plsql"))
                .required()
                .read_only(kept.program)
                .help("El bloque PL/SQL, o el nombre del procedimiento."),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("start", "Inicio", FormKind::DateTime).read_only(kept.schedule),
            FormField::new("repeat", "Repetición", FormKind::Text)
                .read_only(kept.schedule)
                .help("Una expresión de calendario (FREQ=DAILY;BYHOUR=3) o una de fecha. Vacío: corre una vez.")
                .placeholder("FREQ=DAILY;BYHOUR=3"),
            FormField::new("end", "Fin", FormKind::DateTime).read_only(kept.schedule),
        ]),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("owner", "Dueño", FormKind::Text).read_only(true),
            FormField::new("program", "Programa", FormKind::Text).read_only(true),
            FormField::new("named_schedule", "Calendario con nombre", FormKind::Text)
                .read_only(true),
            FormField::new("state", "Estado", FormKind::Text).read_only(true),
            FormField::new("run_count", "Ejecuciones", FormKind::Text).read_only(true),
            FormField::new("failure_count", "Fallas", FormKind::Text).read_only(true),
            FormField::new("next_run", "Próxima ejecución", FormKind::Text).read_only(true),
        ]));
    }
    out
}

fn sched_values(j: &SchedJob) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(j.name)),
        ("enabled", json!(j.enabled)),
        ("auto_drop", json!(j.auto_drop)),
        ("comments", jtext(j.comments.as_deref())),
        ("job_type", json!(j.job_type)),
        ("action", json!(j.action)),
        (
            "start",
            j.start.map_or(Value::Null, |ms| json!(dt_value(ms))),
        ),
        ("repeat", jtext(j.repeat.as_deref())),
        ("end", j.end.map_or(Value::Null, |ms| json!(dt_value(ms)))),
    ])
}

pub(super) fn scheduler_form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let Some(id) = id else {
        return Ok(ObjectForm {
            sections: scheduler_sections(false, Kept::default()),
            values: sched_values(&SchedJob {
                job_type: "PLSQL_BLOCK".into(),
                enabled: true,
                ..SchedJob::default()
            }),
            notes: vec![SCHED_RENAME.to_string()],
            ..ObjectForm::default()
        });
    };
    let (owner, _, s, j, kept) = sched_job_read(src, id)?;
    let mine = me(src).manages(Some(&owner));
    let mut v = sched_values(&j);
    v.insert("owner".into(), json!(owner));
    for (k, c) in [
        ("program", "PROGRAM_NAME"),
        ("named_schedule", "SCHEDULE_NAME"),
        ("state", "STATE"),
        ("run_count", "RUN_COUNT"),
        ("failure_count", "FAILURE_COUNT"),
        ("next_run", "NEXT_RUN_DATE"),
    ] {
        v.insert(k.into(), jtext(s.get(0, c)));
    }
    let mut notes = vec![if kept.program || kept.schedule {
        KEPT_RENAME
    } else {
        SCHED_RENAME
    }
    .to_string()];
    if kept.program {
        notes.push(
            if s.get(0, "PROGRAM_NAME").is_some() {
                PROGRAM_KEPT
            } else if int(s.get(0, "NUMBER_OF_ARGUMENTS")).unwrap_or(0) > 0 {
                ARGS_KEPT
            } else {
                TYPE_KEPT
            }
            .to_string(),
        );
    }
    if kept.schedule {
        notes.push(SCHEDULE_KEPT.to_string());
    }
    if !mine {
        notes.insert(0, NOT_MINE.to_string());
    }
    Ok(ObjectForm {
        sections: scheduler_sections(true, kept),
        values: v,
        notes,
        read_only: !mine,
        ..ObjectForm::default()
    })
}

/// The job the form describes, checked. What `cur` keeps (its program or
/// unwritable type, its named schedule) is taken from `cur`, not the form,
/// whose fields for it are read only and may come back empty.
pub(super) fn scheduler_from(
    values: &FormValues,
    cur: Option<(&SchedJob, Kept)>,
) -> Result<SchedJob> {
    let v = Vals(&values.values);
    let kept = cur.map(|(_, k)| k).unwrap_or_default();
    let (start, repeat, end) = match cur {
        Some((c, _)) if kept.schedule => (c.start, c.repeat.clone(), c.end),
        _ => {
            let start = datetime(v, "start", "Inicio")?;
            let end = datetime(v, "end", "Fin")?;
            if let (Some(a), Some(b)) = (start, end) {
                if b <= a {
                    return Err(bad("«Fin» tiene que ser posterior a «Inicio»."));
                }
            }
            (start, optional(v, "repeat", "Repetición", 4000)?, end)
        }
    };
    let (job_type, action) = match cur {
        Some((c, _)) if kept.program => (c.job_type.clone(), c.action.clone()),
        _ => (
            choice(v, "job_type", "Tipo", TYPES, "PLSQL_BLOCK")?,
            code_text(v, "action", "Acción", 1_000_000)?.ok_or_else(|| bad("Falta «Acción»."))?,
        ),
    };
    Ok(SchedJob {
        name: required(v, "name", "Nombre", 128)?,
        job_type,
        action,
        start,
        repeat,
        end,
        enabled: v.bool_or("enabled", true),
        auto_drop: v.bool_or("auto_drop", false),
        comments: code_text(v, "comments", "Comentario", 4000)?,
    })
}

/// A plain identifier, which DM folds to capitals.
fn plain_ident(name: &str) -> bool {
    name.bytes().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
}

/// The name DM stores for a new job named `name`: in capitals when it's a
/// plain identifier, as typed otherwise.
fn stored_name(name: &str) -> String {
    if plain_ident(name) {
        name.to_ascii_uppercase()
    } else {
        name.to_string()
    }
}

/// A new job's name for the package, in the session's schema: in capitals
/// when it's a plain identifier (as DM folds it), quoted otherwise.
fn new_name(name: &str) -> String {
    if plain_ident(name) {
        name.to_ascii_uppercase()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn tf(b: bool) -> &'static str {
    if b {
        "TRUE"
    } else {
        "FALSE"
    }
}

/// `CREATE_JOB` for `j`: in `owner`'s schema when given (a job recreated
/// keeps its owner, also when a DBA saves it), else the session's.
fn create_block(j: &SchedJob, owner: Option<&str>) -> String {
    let job_name = match owner {
        Some(o) => dotted(o, &stored_name(&j.name), true),
        None => new_name(&j.name),
    };
    let mut args: Vec<(&str, String)> = vec![
        ("JOB_NAME", lit(&job_name)),
        ("JOB_TYPE", lit(&j.job_type)),
        ("JOB_ACTION", lit(&j.action)),
    ];
    if let Some(s) = j.start {
        args.push(("START_DATE", ts_lit(s)));
    }
    if let Some(r) = &j.repeat {
        args.push(("REPEAT_INTERVAL", lit(r)));
    }
    if let Some(e) = j.end {
        args.push(("END_DATE", ts_lit(e)));
    }
    args.push(("ENABLED", tf(j.enabled).into()));
    args.push(("AUTO_DROP", tf(j.auto_drop).into()));
    if let Some(c) = &j.comments {
        args.push(("COMMENTS", lit(c)));
    }
    let body: Vec<String> = args
        .iter()
        .map(|(k, v)| format!("    {k} => {v}"))
        .collect();
    format!(
        "BEGIN\n  DBMS_SCHEDULER.CREATE_JOB(\n{}\n  );\nEND",
        body.join(",\n")
    )
}

/// The id of job `stored` (as DM stores its name) of `owner`, or of the
/// session's user when `None`.
fn sched_lookup(owner: Option<String>, stored: String) -> Lookup {
    Box::new(move |src: &mut dyn JobSource| {
        let Some(owner) = owner.or_else(|| me(src).user) else {
            return Ok(None);
        };
        let (s, _) = sched_read(src, Some((&owner, &stored)))?;
        Ok((!s.rows.is_empty()).then(|| quote_pair(&owner, &stored)))
    })
}

pub(super) fn scheduler_create_plan(j: &SchedJob) -> Plan {
    let mut p = Plan::new(&format!("Job «{}» de DBMS_SCHEDULER de DM", j.name));
    p.atomic = true;
    p.exec(create_block(j, None));
    p.lookup = Some(sched_lookup(None, stored_name(&j.name)));
    p
}

pub(super) fn scheduler_alter_plan(
    owner: &str,
    cur: &SchedJob,
    j: &SchedJob,
    kept: Kept,
) -> Result<Plan> {
    let job = lit(&dotted(owner, &cur.name, true));
    let id = quote_pair(owner, &cur.name);
    if j.name != cur.name && stored_name(&j.name) != cur.name {
        // CREATE_JOB would lose the program, its arguments or the named
        // schedule (and run at once, without its schedule).
        if kept.program || kept.schedule {
            return Err(bad(KEPT_RENAME));
        }
        let mut p = Plan::new(&format!("Job «{}» de DBMS_SCHEDULER de DM", cur.name));
        p.atomic = true;
        p.warn(W_RENAME);
        p.warn(W_RENAME_HISTORY);
        p.warn(W_RENAME_ATTRS);
        p.warn(W_RENAME_COMMIT);
        p.exec(format!("CALL DBMS_SCHEDULER.DROP_JOB({job})"));
        // Same owner: in the owner's schema, not the DBA's who saves it.
        p.exec(create_block(j, Some(owner)));
        let stored = stored_name(&j.name);
        p.id = Some(quote_pair(owner, &stored));
        p.lookup = Some(sched_lookup(Some(owner.to_string()), stored));
        return Ok(p);
    }
    let set =
        |attr: &str, v: String| format!("  DBMS_SCHEDULER.SET_ATTRIBUTE({job}, '{attr}', {v});");
    let null = |attr: &str| format!("  DBMS_SCHEDULER.SET_ATTRIBUTE_NULL({job}, '{attr}');");
    let mut lines = Vec::new();
    if !kept.program {
        if j.job_type != cur.job_type {
            lines.push(set("JOB_TYPE", lit(&j.job_type)));
        }
        if j.action.trim() != cur.action.trim() {
            lines.push(set("JOB_ACTION", lit(&j.action)));
        }
    }
    if !kept.schedule {
        if j.start != cur.start {
            lines.push(match j.start {
                Some(s) => set("START_DATE", ts_lit(s)),
                None => null("START_DATE"),
            });
        }
        if !same(&j.repeat, &cur.repeat) {
            lines.push(match &j.repeat {
                Some(r) => set("REPEAT_INTERVAL", lit(r)),
                None => null("REPEAT_INTERVAL"),
            });
        }
        if j.end != cur.end {
            lines.push(match j.end {
                Some(e) => set("END_DATE", ts_lit(e)),
                None => null("END_DATE"),
            });
        }
    }
    if j.auto_drop != cur.auto_drop {
        lines.push(set("AUTO_DROP", tf(j.auto_drop).into()));
    }
    if !same(&j.comments, &cur.comments) {
        lines.push(match &j.comments {
            Some(c) => set("COMMENTS", lit(c)),
            None => null("COMMENTS"),
        });
    }
    if j.enabled != cur.enabled {
        lines.push(format!(
            "  DBMS_SCHEDULER.{}({job});",
            if j.enabled { "ENABLE" } else { "DISABLE" }
        ));
    }
    if lines.is_empty() {
        return Ok(Plan::nothing(&cur.name, id));
    }
    let mut p = Plan::new(&format!("Job «{}» de DBMS_SCHEDULER de DM", cur.name));
    p.atomic = true;
    // One block, but the package may commit each call: the transaction
    // can't take back the ones before a failure.
    if lines.len() > 1 {
        p.warn(W_CALLS);
    }
    p.exec(format!("BEGIN\n{}\nEND", lines.join("\n")));
    p.id = Some(id);
    Ok(p)
}

pub(super) fn scheduler_plan(
    src: &mut dyn JobSource,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let Some(id) = id else {
        return Ok(scheduler_create_plan(&scheduler_from(values, None)?));
    };
    let (owner, _, _, cur, kept) = sched_job_read(src, id)?;
    let j = scheduler_from(values, Some((&cur, kept)))?;
    if !me(src).manages(Some(&owner)) {
        return Err(Error::Unsupported(NOT_MINE.into()));
    }
    scheduler_alter_plan(&owner, &cur, &j, kept)
}

pub(super) fn scheduler_drop_plan(owner: &str, name: &str) -> Plan {
    let mut p = Plan::new(&format!("Borra el job «{name}» de DBMS_SCHEDULER de DM"));
    p.atomic = true;
    p.exec(format!(
        "CALL DBMS_SCHEDULER.DROP_JOB({})",
        lit(&dotted(owner, name, true))
    ));
    p
}

pub(super) fn scheduler_delete_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (owner, name, _, _, _) = sched_job_read(src, id)?;
    if !me(src).manages(Some(&owner)) {
        return Err(Error::Unsupported(NOT_MINE.into()));
    }
    Ok(scheduler_drop_plan(&owner, &name))
}

/// Stopping a job with AUTO_DROP may drop it: the stop's warning, when
/// the job has it or that can't be read.
pub(super) fn stop_warning(src: &mut dyn JobSource, id: &str, plan: &mut Plan) {
    let auto_drop = match sched_job_read(src, id) {
        Ok((_, _, s, j, _)) => j.auto_drop || s.get(0, "AUTO_DROP").is_none(),
        Err(e) => {
            tracing::debug!("odbc scheduler: the job's AUTO_DROP wasn't read: {e}");
            true
        }
    };
    if auto_drop {
        plan.warn(W_AUTO_DROP);
    }
}

pub(super) fn scheduler_purge_plan(owner: &str, name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra el historial del job «{name}» de DBMS_SCHEDULER de DM"
    ));
    p.atomic = true;
    p.exec(format!(
        "CALL DBMS_SCHEDULER.PURGE_LOG(0, 'JOB_LOG', {})",
        lit(&dotted(owner, name, true))
    ));
    p
}

pub(super) fn scheduler_clear_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (owner, name, _, _, _) = sched_job_read(src, id)?;
    if !me(src).manages(Some(&owner)) {
        return Err(Error::Unsupported(NOT_MINE.into()));
    }
    Ok(scheduler_purge_plan(&owner, &name))
}
