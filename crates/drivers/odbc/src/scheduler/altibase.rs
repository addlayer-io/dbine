//! Altibase: the jobs of `CREATE JOB` managed whole: the procedure call it
//! runs, its start, end and interval, whether it's enabled and its comment
//! (those two from 6.5.1 on). Changes are one `ALTER JOB … SET …` each;
//! Altibase can't rename a job nor take its end or interval away, so those
//! drop the job and create it again (its last run, run count and last
//! error code go, and it gets a new id). Only SYS creates, alters and
//! drops jobs. DDL commits on its own: the statements run one by one.

use super::*;
use crate::jobs::altibase::{call, is_sys, off, one, q, unit, EXISTS, NOT_HERE, OFF, ONLY_SYS};
use crate::jobs::{dotted, int, lit, parse_pair, plain};
use dbine_driver::{FormField, FormKind, FormSection, SchedulerKind};
use serde_json::json;

pub(super) fn kind(kind: &str) -> Result<()> {
    (kind.trim() == "job")
        .then_some(())
        .ok_or_else(|| bad_kind(kind))
}

const UNITS: &[&str] = &["YEAR", "MONTH", "DAY", "HOUR", "MINUTE"];
const ONLY_SYS_NOTE: &str = "En Altibase solo el usuario SYS crea, cambia y borra jobs.";
const OLD_VERSION: &str =
    "Esta versión de Altibase (anterior a la 6.5.1) no habilita, deshabilita ni comenta jobs.";
const RECREATE: &str = "Altibase no cambia el nombre de un job ni le quita el fin o el intervalo: se borra y se crea de nuevo; pierde su última ejecución, la cuenta de ejecuciones y el último código de error, y recibe otro id.";
// What a script warns about (each a sentence of its own).
const W_RECREATE: &str = "Altibase no cambia el nombre de un job ni le quita el fin o el intervalo: se borra y se crea de nuevo, y recibe otro id.";
const W_RECREATE_LOST: &str =
    "Se pierden su última ejecución, la cuenta de ejecuciones y el último código de error.";
const W_RECREATE_GONE: &str =
    "Son dos sentencias sin transacción: si la creación falla, el job queda borrado.";
const PROCS: &str = "SELECT U.USER_NAME, P.PROC_NAME FROM SYSTEM_.SYS_PROCEDURES_ P, SYSTEM_.SYS_USERS_ U WHERE P.USER_ID = U.USER_ID AND P.OBJECT_TYPE = 0 ORDER BY U.USER_NAME, P.PROC_NAME";
const FULL: &str = "SELECT IS_ENABLE, \"COMMENT\" FROM SYSTEM_.SYS_JOBS_ WHERE JOB_ID = -1";

/// A job as the form edits it. Times: the server's local time in ms.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Job {
    pub(super) name: String,
    /// `USER.PROC` (or a quoted pair).
    pub(super) procedure: String,
    /// The arguments as written between the parentheses.
    pub(super) args: Option<String>,
    pub(super) start: Option<i64>,
    pub(super) end: Option<i64>,
    pub(super) interval: Option<i64>,
    pub(super) unit: String,
    pub(super) enabled: bool,
    pub(super) comment: Option<String>,
}

/// The server has `IS_ENABLE` and `COMMENT` (6.5.1 on).
fn full(src: &mut dyn JobSource) -> bool {
    src.query(FULL, &[]).is_ok()
}

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    if let Err(e) = src.query(
        "SELECT JOB_ID FROM SYSTEM_.SYS_JOBS_ WHERE JOB_ID = -1",
        &[],
    ) {
        let here = src
            .query(EXISTS, &[])
            .ok()
            .and_then(|s| int(s.get(0, "N")))
            .map(|n| n > 0);
        return match here {
            Some(false) => Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![NOT_HERE.to_string()],
            }),
            _ => Err(e),
        };
    }
    let sys = is_sys(src);
    let mut notes = Vec::new();
    if !sys {
        notes.push(ONLY_SYS_NOTE.to_string());
    }
    if off(src) {
        notes.push(OFF.to_string());
    }
    if !full(src) {
        notes.push(OLD_VERSION.to_string());
    }
    Ok(SchedulerInfo {
        kinds: vec![SchedulerKind::new("job", "Jobs", "job").abilities(sys, sys, sys)],
        logs: false,
        notes,
    })
}

/// `USER.PROC(args)` as the procedure and the arguments.
pub(super) fn split_call(c: &str) -> (String, Option<String>) {
    let c = c.trim();
    let mut quoted = false;
    for (i, ch) in c.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '(' if !quoted => {
                let inner = c[i + 1..].trim_end();
                let inner = inner.strip_suffix(')').unwrap_or(inner).trim();
                return (
                    c[..i].trim().to_string(),
                    (!inner.is_empty()).then(|| inner.to_string()),
                );
            }
            _ => {}
        }
    }
    (c.to_string(), None)
}

fn read(src: &mut dyn JobSource, id: &str) -> Result<(u64, Job, bool)> {
    let n = crate::jobs::int_id(id)?;
    let (s, full) = one(src, &n.to_string())?;
    let (procedure, args) = split_call(call(s.get(0, "EXEC_QUERY").unwrap_or_default()));
    let procedure = norm_proc(&procedure);
    let interval = int(s.get(0, "INTERVAL")).filter(|n| *n > 0);
    Ok((
        n,
        Job {
            name: s.get(0, "JOB_NAME").unwrap_or_default().to_string(),
            procedure,
            args,
            start: s.get(0, "START_TIME").and_then(crate::jobs::parse_ts),
            end: s.get(0, "END_TIME").and_then(crate::jobs::parse_ts),
            interval,
            unit: unit(s.get(0, "INTERVAL_TYPE")).unwrap_or("DAY").to_string(),
            enabled: !full || crate::jobs::yes(s.get(0, "IS_ENABLE")),
            comment: s.get(0, "COMMENT").map(str::to_string),
        },
        full,
    ))
}

pub(super) fn sections(existing: bool, full: bool) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("Cambiarlo vuelve a crear el job."),
            FormField::new("enabled", "Habilitado", FormKind::Bool)
                .default(json!(true))
                .read_only(!full),
            FormField::new("comment", "Comentario", FormKind::Text).read_only(!full),
        ]),
        FormSection::new("action", "Procedimiento").fields(vec![
            FormField::new("procedure", "Procedimiento", FormKind::reference("procedure", false))
                .required()
                .help("Un procedimiento sin parámetros OUT ni IN OUT. Sin usuario: SYS."),
            FormField::new("args", "Argumentos", FormKind::Text)
                .help("Lo que va entre los paréntesis: constantes o expresiones, separadas por comas.")
                .placeholder("1, 'texto'"),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("start", "Inicio", FormKind::DateTime)
                .help("Cuándo corre por primera vez. Vacío al crearlo: ahora."),
            FormField::new("end", "Fin", FormKind::DateTime)
                .help("Desde cuándo no corre más. Quitarlo vuelve a crear el job."),
            FormField::new("interval", "Cada", FormKind::number(Some(1.0), None))
                .help("Vacío: corre una sola vez. Quitarlo vuelve a crear el job."),
            FormField::new(
                "unit",
                "Unidad",
                FormKind::select(&[
                    ("YEAR", "Años"),
                    ("MONTH", "Meses"),
                    ("DAY", "Días"),
                    ("HOUR", "Horas"),
                    ("MINUTE", "Minutos"),
                ]),
            )
            .default(json!("DAY")),
        ]),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("id", "Id", FormKind::Text).read_only(true),
            FormField::new("last_exec", "Última ejecución", FormKind::Text).read_only(true),
            FormField::new("exec_count", "Ejecuciones", FormKind::Text).read_only(true),
            FormField::new("error_code", "Código de error de la última", FormKind::Text)
                .read_only(true),
            FormField::new("running", "Corriendo ahora", FormKind::Text).read_only(true),
        ]));
    }
    out
}

pub(super) fn values_of(j: &Job) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(j.name)),
        ("enabled", json!(j.enabled)),
        ("comment", jtext(j.comment.as_deref())),
        ("procedure", json!(j.procedure)),
        ("args", jtext(j.args.as_deref())),
        (
            "start",
            j.start.map_or(Value::Null, |ms| json!(dt_value(ms))),
        ),
        ("end", j.end.map_or(Value::Null, |ms| json!(dt_value(ms)))),
        ("interval", j.interval.map_or(Value::Null, |n| json!(n))),
        ("unit", json!(j.unit)),
    ])
}

fn procedures(src: &mut dyn JobSource) -> Vec<(String, String)> {
    src.query(PROCS, &[])
        .map(|s| {
            (0..s.rows.len())
                .filter_map(|r| {
                    let (u, p) = (s.get(r, "USER_NAME")?, s.get(r, "PROC_NAME")?);
                    Some((dotted(u, p, true), format!("{u}.{p}")))
                })
                .collect()
        })
        .unwrap_or_else(|e| {
            tracing::debug!("odbc scheduler: SYS_PROCEDURES_ failed: {e}");
            Vec::new()
        })
}

pub(super) fn form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let choices = BTreeMap::from([("procedure".to_string(), procedures(src))]);
    let sys = is_sys(src);
    let mut notes = Vec::new();
    if !sys {
        notes.push(ONLY_SYS_NOTE.to_string());
    }
    let Some(id) = id else {
        let full = full(src);
        if !full {
            notes.push(OLD_VERSION.to_string());
        }
        let j = Job {
            unit: "DAY".into(),
            enabled: true,
            ..Job::default()
        };
        return Ok(ObjectForm {
            sections: sections(false, full),
            values: values_of(&j),
            choices,
            notes,
            read_only: !sys,
        });
    };
    let (n, j, full) = read(src, id)?;
    if !full {
        notes.push(OLD_VERSION.to_string());
    }
    notes.push(RECREATE.to_string());
    let (s, _) = one(src, &n.to_string())?;
    let mut v = values_of(&j);
    v.insert("id".into(), json!(n.to_string()));
    v.insert("last_exec".into(), jtext(s.get(0, "LAST_EXEC_TIME")));
    v.insert("exec_count".into(), jtext(s.get(0, "EXEC_COUNT")));
    v.insert("error_code".into(), jtext(s.get(0, "ERROR_CODE")));
    v.insert(
        "running".into(),
        json!(if int(s.get(0, "STATE")) == Some(1) {
            "Sí"
        } else {
            "No"
        }),
    );
    Ok(ObjectForm {
        sections: sections(true, full),
        values: v,
        choices,
        notes,
        read_only: !sys,
    })
}

// ------------------------------------------------------------------ values

fn plain_ident(s: &str) -> bool {
    let mut it = s.bytes();
    it.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && it.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
}

/// A procedure name: `PROC`, `USER.PROC` or a quoted pair.
pub(super) fn proc_name(t: &str) -> bool {
    let t = t.trim();
    if t.starts_with('"') {
        return parse_pair(t).is_ok();
    }
    let parts: Vec<&str> = t.split('.').collect();
    (1..=2).contains(&parts.len()) && parts.iter().all(|p| plain_ident(p))
}

/// The arguments: quotes and parentheses balanced, and no `;` or comment
/// outside a literal (they are the call's, nothing else).
pub(super) fn args_ok(t: &str) -> bool {
    let (mut quoted, mut depth) = (false, 0i32);
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if quoted {
            if c == b'\'' {
                if b.get(i + 1) == Some(&b'\'') {
                    i += 1;
                } else {
                    quoted = false;
                }
            }
        } else {
            match c {
                b'\'' => quoted = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                }
                b';' => return false,
                b'-' if b.get(i + 1) == Some(&b'-') => return false,
                b'/' if b.get(i + 1) == Some(&b'*') => return false,
                _ => {}
            }
        }
        i += 1;
    }
    !quoted && depth == 0
}

/// The job the form describes, checked. `procs`: the server's procedures.
pub(super) fn from_values(values: &FormValues, procs: &[(String, String)]) -> Result<Job> {
    let v = Vals(&values.values);
    let name = required(v, "name", "Nombre", 128)?;
    let p = required(v, "procedure", "Procedimiento", 300)?;
    let procedure = match procs
        .iter()
        .find(|(val, shown)| val.eq_ignore_ascii_case(&p) || shown.eq_ignore_ascii_case(&p))
    {
        Some((val, _)) => val.clone(),
        None if proc_name(&p) => p,
        None => {
            return Err(bad(format!(
                "«Procedimiento»: «{}» no es un nombre de procedimiento.",
                short(&p)
            )))
        }
    };
    let args = optional(v, "args", "Argumentos", 4000)?;
    if args.as_deref().is_some_and(|a| !args_ok(a)) {
        return Err(bad(
            "«Argumentos»: las comillas o los paréntesis no cierran, o hay un ; o un comentario fuera de un texto.",
        ));
    }
    let start = datetime(v, "start", "Inicio")?;
    let end = datetime(v, "end", "Fin")?;
    if let (Some(a), Some(b)) = (start, end) {
        if b <= a {
            return Err(bad("«Fin» tiene que ser posterior a «Inicio»."));
        }
    }
    Ok(Job {
        name,
        procedure,
        args,
        start,
        end,
        interval: v.int("interval", "Cada", 1, i64::from(i32::MAX))?,
        unit: choice(v, "unit", "Unidad", UNITS, "DAY")?,
        enabled: v.bool_or("enabled", true),
        comment: optional(v, "comment", "Comentario", 4000)?,
    })
}

// ------------------------------------------------------------------ scripts

fn date_lit(ms: i64) -> String {
    format!("TO_DATE('{}', 'YYYY-MM-DD HH24:MI:SS')", plain(ms))
}

fn exec_clause(j: &Job) -> String {
    match &j.args {
        Some(a) => format!("EXEC {}({a})", j.procedure),
        None => format!("EXEC {}", j.procedure),
    }
}

/// A new job's name: as typed when it's a plain identifier (Altibase keeps
/// it in capitals), quoted otherwise.
fn new_name(name: &str) -> (String, String) {
    if plain_ident(name) {
        (name.to_string(), name.to_ascii_uppercase())
    } else {
        (q(name), name.to_string())
    }
}

pub(super) fn create_statement(j: &Job, full: bool) -> String {
    let mut out = format!("CREATE JOB {}\n{}", new_name(&j.name).0, exec_clause(j));
    out.push_str(&format!(
        "\nSTART {}",
        j.start.map(date_lit).unwrap_or_else(|| "SYSDATE".into())
    ));
    if let Some(e) = j.end {
        out.push_str(&format!("\nEND {}", date_lit(e)));
    }
    if let Some(n) = j.interval {
        out.push_str(&format!("\nINTERVAL {n} {}", j.unit));
    }
    if full {
        out.push_str(if j.enabled { "\nENABLE" } else { "\nDISABLE" });
        if let Some(c) = &j.comment {
            out.push_str(&format!("\nCOMMENT {}", lit(c)));
        }
    }
    out
}

fn lookup(stored: String) -> Lookup {
    Box::new(move |src: &mut dyn JobSource| {
        let s = src.query(
            "SELECT JOB_ID FROM SYSTEM_.SYS_JOBS_ WHERE JOB_NAME = ?",
            &[&stored],
        )?;
        Ok(int(s.get(0, "JOB_ID")).map(|n| n.to_string()))
    })
}

pub(super) fn create_plan(j: &Job, full: bool) -> Plan {
    let mut p = Plan::new(&format!("Job «{}» de Altibase", j.name));
    p.exec(create_statement(j, full));
    p.lookup = Some(lookup(new_name(&j.name).1));
    p
}

/// A procedure name as the picker lists it: `USER.PROC` in capitals (SYS
/// when it has no user), a quoted pair where the parts need it.
pub(super) fn norm_proc(s: &str) -> String {
    let t = s.trim();
    if t.starts_with('"') {
        return match parse_pair(t) {
            Ok((u, p)) => dotted(&u, &p, true),
            Err(_) => t.to_string(),
        };
    }
    let parts: Vec<String> = t
        .split('.')
        .map(|p| p.trim().to_ascii_uppercase())
        .collect();
    match parts.as_slice() {
        [p] => format!("SYS.{p}"),
        _ => parts.join("."),
    }
}

pub(super) fn alter_plan(n: u64, cur: &Job, j: &Job, full: bool) -> Result<Plan> {
    let renamed =
        !(j.name == cur.name || (plain_ident(&j.name) && j.name.to_ascii_uppercase() == cur.name));
    let start = match (cur.start, j.start) {
        (Some(_), None) => {
            return Err(bad("«Inicio»: Altibase no deja un job sin inicio."));
        }
        (_, s) => s,
    };
    if renamed
        || (cur.end.is_some() && j.end.is_none())
        || (cur.interval.is_some() && j.interval.is_none())
    {
        let mut p = Plan::new(&format!("Job «{}» de Altibase", cur.name));
        p.warn(W_RECREATE);
        p.warn(W_RECREATE_LOST);
        p.warn(W_RECREATE_GONE);
        p.exec(format!("DROP JOB {}", q(&cur.name)));
        let mut j = j.clone();
        j.start = start;
        p.exec(create_statement(&j, full));
        p.lookup = Some(lookup(new_name(&j.name).1));
        return Ok(p);
    }
    let alter = |what: String| format!("ALTER JOB {} SET {what}", q(&cur.name));
    let mut steps = Vec::new();
    if norm_proc(&cur.procedure) != norm_proc(&j.procedure) || !same(&cur.args, &j.args) {
        steps.push(alter(exec_clause(j)));
    }
    if start != cur.start {
        if let Some(s) = start {
            steps.push(alter(format!("START {}", date_lit(s))));
        }
    }
    if j.end != cur.end {
        if let Some(e) = j.end {
            steps.push(alter(format!("END {}", date_lit(e))));
        }
    }
    if j.interval != cur.interval || (j.interval.is_some() && j.unit != cur.unit) {
        if let Some(i) = j.interval {
            steps.push(alter(format!("INTERVAL {i} {}", j.unit)));
        }
    }
    if full && j.enabled != cur.enabled {
        steps.push(alter(if j.enabled { "ENABLE" } else { "DISABLE" }.into()));
    }
    if full && !same(&cur.comment, &j.comment) {
        steps.push(alter(format!(
            "COMMENT {}",
            lit(j.comment.as_deref().unwrap_or_default())
        )));
    }
    if steps.is_empty() {
        return Ok(Plan::nothing(&cur.name, n.to_string()));
    }
    let mut p = Plan::new(&format!("Job «{}» de Altibase", cur.name));
    for s in steps {
        p.exec(s);
    }
    p.one_by_one();
    p.id = Some(n.to_string());
    Ok(p)
}

pub(super) fn plan(src: &mut dyn JobSource, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    if !is_sys(src) {
        return Err(Error::Unsupported(ONLY_SYS.into()));
    }
    let procs = procedures(src);
    let j = from_values(values, &procs)?;
    match id {
        None => {
            let full = full(src);
            Ok(create_plan(&j, full))
        }
        Some(id) => {
            let (n, cur, full) = read(src, id)?;
            let mut j = j;
            if !full {
                j.enabled = cur.enabled;
                j.comment = cur.comment.clone();
            }
            alter_plan(n, &cur, &j, full)
        }
    }
}

pub(super) fn drop_plan(name: &str) -> Plan {
    let mut p = Plan::new(&format!("Borra el job «{name}» de Altibase"));
    p.exec(format!("DROP JOB {}", q(name)));
    p
}

pub(super) fn delete_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    if !is_sys(src) {
        return Err(Error::Unsupported(ONLY_SYS.into()));
    }
    let (_, j, _) = read(src, id)?;
    Ok(drop_plan(&j.name))
}
