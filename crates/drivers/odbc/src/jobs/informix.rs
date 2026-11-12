//! Informix and GBase 8s: the DB Scheduler's tasks and sensors, in the
//! `sysadmin` database (`ph_task`, the runs in `ph_run`, failures in
//! `ph_alert`), read from any database as `sysadmin:table`. Enabling is an
//! UPDATE of `tk_enable`; "run now" is `exectask_async`, which sysadmin's
//! functions only accept from a session connected to `sysadmin`.
//! Times are DATETIME values in the server's time zone, cast to text so
//! the ODBC driver doesn't pad them into timestamps.

use super::*;
use dbine_driver::{JobStep, ServerJob};

pub(super) const NOW: &str = "SELECT FIRST 1 CURRENT YEAR TO SECOND AS LOCAL_NOW, DBINFO('utc_current') AS UTC_NOW FROM systables";

pub(super) const ASYNC: &str =
    "SELECT COUNT(*) AS N FROM sysadmin:sysprocedures WHERE procname = 'exectask_async'";

const NO_SYSADMIN: &str =
    "Este servidor no tiene la base sysadmin, donde vive el planificador (DB Scheduler).";
const NO_ACCESS: &str = "Este usuario no tiene acceso a la base sysadmin: el planificador lo ve el usuario informix o quien tenga permisos sobre sysadmin.";
const NO_TABLES: &str = "La base sysadmin no tiene las tablas del planificador (ph_task).";
const RUN_FROM_SYSADMIN: &str =
    "«Ejecutar ahora» funciona con la conexión abierta en la base sysadmin.";

const COLUMNS: &str = "t.tk_id, t.tk_name, t.tk_type, t.tk_group, t.tk_description, t.tk_dbs, t.tk_execute,
       CAST(t.tk_start_time AS CHAR(8)) AS tk_start_time, CAST(t.tk_stop_time AS CHAR(8)) AS tk_stop_time,
       CAST(t.tk_frequency AS CHAR(20)) AS tk_frequency, CAST(t.tk_next_execution AS CHAR(19)) AS tk_next_execution,
       CAST(t.tk_delete AS CHAR(20)) AS tk_delete, t.tk_total_executions, t.tk_total_time,
       t.tk_monday, t.tk_tuesday, t.tk_wednesday, t.tk_thursday, t.tk_friday, t.tk_saturday, t.tk_sunday,
       t.tk_enable, t.tk_priority, t.tk_result_table, t.tk_create,
       CAST(r.run_time AS CHAR(19)) AS last_run, r.run_retcode AS last_retcode, r.run_duration AS last_duration";

pub(super) fn tasks_sql(task: Option<u64>) -> String {
    let filter = task
        .map(|n| format!("\n WHERE t.tk_id = {n}"))
        .unwrap_or_default();
    format!(
        "SELECT {COLUMNS}
  FROM sysadmin:ph_task t
  LEFT OUTER JOIN (SELECT run_task_id, MAX(run_id) AS last_id FROM sysadmin:ph_run GROUP BY run_task_id) m
    ON m.run_task_id = t.tk_id
  LEFT OUTER JOIN sysadmin:ph_run r ON r.run_id = m.last_id{filter}
 ORDER BY t.tk_group, t.tk_name"
    )
}

const DAYS: [(&str, &str); 7] = [
    ("tk_monday", "MON"),
    ("tk_tuesday", "TUE"),
    ("tk_wednesday", "WED"),
    ("tk_thursday", "THU"),
    ("tk_friday", "FRI"),
    ("tk_saturday", "SAT"),
    ("tk_sunday", "SUN"),
];

fn startup(s: &Set, r: usize) -> bool {
    s.get(r, "tk_type")
        .is_some_and(|t| t.to_ascii_uppercase().starts_with("STARTUP"))
}

/// TASK, SENSOR, STARTUP TASK, STARTUP SENSOR as the tag shows them.
fn kind(t: Option<&str>) -> String {
    match t.map(|t| t.trim().to_ascii_uppercase()).as_deref() {
        Some("TASK") => "Task".into(),
        Some("SENSOR") | None => "Sensor".into(),
        Some("STARTUP TASK") => "Startup task".into(),
        Some("STARTUP SENSOR") => "Startup sensor".into(),
        Some(other) => other.to_string(),
    }
}

/// `EVERY 1 00:00:00 FROM 03:00:00 TO 19:00:00 ON MON,TUE`, in
/// ph_task's terms; startup tasks have none.
fn schedule(s: &Set, r: usize) -> Option<String> {
    if startup(s, r) {
        return None;
    }
    let mut out = match s.get(r, "tk_frequency") {
        Some(f) => format!(
            "EVERY {}",
            f.split_whitespace().collect::<Vec<_>>().join(" ")
        ),
        None => "ONCE".to_string(),
    };
    if let Some(start) = s.get(r, "tk_start_time") {
        out.push_str(&format!(" FROM {start}"));
    }
    if let Some(stop) = s.get(r, "tk_stop_time") {
        out.push_str(&format!(" TO {stop}"));
    }
    let days: Vec<&str> = DAYS
        .iter()
        .filter(|(c, _)| s.get(r, c).is_none_or(|v| yes(Some(v))))
        .map(|(_, d)| *d)
        .collect();
    if days.len() < 7 {
        out.push_str(&format!(" ON {}", days.join(",")));
    }
    Some(out)
}

/// A run's return code: negative is an SQL error.
fn outcome(retcode: Option<i64>) -> JobOutcome {
    if retcode.is_some_and(|c| c < 0) {
        JobOutcome::Failed
    } else {
        JobOutcome::Succeeded
    }
}

fn secs_ms(v: Option<&str>) -> Option<u64> {
    let f: f64 = v?.trim().parse().ok()?;
    (f.is_finite() && f >= 0.0).then(|| (f * 1000.0).round() as u64)
}

fn job(s: &Set, r: usize, tz: &Tz, can_run: bool) -> Option<ServerJob> {
    let id = int(s.get(r, "tk_id"))?;
    let ran = s.get(r, "last_run").is_some();
    Some(ServerJob {
        id: id.to_string(),
        name: s.get(r, "tk_name").unwrap_or_default().to_string(),
        kind: kind(s.get(r, "tk_type")),
        group: owned(s.get(r, "tk_group")),
        description: owned(s.get(r, "tk_description")),
        owner: None,
        enabled: yes(s.get(r, "tk_enable")),
        schedule: schedule(s, r),
        running: false,
        last_outcome: ran.then(|| outcome(int(s.get(r, "last_retcode")))),
        last_started: tz.utc(s.get(r, "last_run")),
        last_duration_ms: secs_ms(s.get(r, "last_duration")),
        next_run: tz.utc(s.get(r, "tk_next_execution")),
        can_run,
        can_stop: false,
        can_enable: true,
        has_history: true,
        has_script: true,
    })
}

fn note(e: &Error) -> Option<&'static str> {
    match code(e) {
        Some(-329) => Some(NO_SYSADMIN),
        Some(-387 | -272 | -389) => Some(NO_ACCESS),
        Some(-206) => Some(NO_TABLES),
        _ => None,
    }
}

/// `exectask_async` works here: connected to sysadmin, on a server that
/// has it.
fn can_run(src: &mut dyn JobSource, ctx: &Ctx) -> bool {
    in_sysadmin(ctx)
        && src
            .query(ASYNC, &[])
            .ok()
            .and_then(|s| int(s.get(0, "N")))
            .is_some_and(|n| n > 0)
}

fn in_sysadmin(ctx: &Ctx) -> bool {
    ctx.database.trim().eq_ignore_ascii_case("sysadmin")
}

pub fn list(src: &mut dyn JobSource, ctx: &Ctx) -> Result<JobList> {
    let tz = Tz::query(src, NOW);
    let s = match src.query(&tasks_sql(None), &[]) {
        Ok(s) => s,
        Err(e) => match note(&e) {
            Some(n) => {
                return Ok(JobList {
                    jobs: Vec::new(),
                    notes: vec![n.into()],
                })
            }
            None => return Err(e),
        },
    };
    let run = can_run(src, ctx);
    let jobs: Vec<ServerJob> = (0..s.rows.len())
        .filter_map(|r| job(&s, r, &tz, run))
        .collect();
    let notes = if in_sysadmin(ctx) || jobs.is_empty() {
        Vec::new()
    } else {
        vec![RUN_FROM_SYSADMIN.into()]
    };
    Ok(JobList { jobs, notes })
}

fn one(src: &mut dyn JobSource, id: &str) -> Result<Set> {
    let n = int_id(id)?;
    let s = src.query(&tasks_sql(Some(n)), &[])?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok(s)
}

pub fn detail(src: &mut dyn JobSource, ctx: &Ctx, id: &str) -> Result<JobDetail> {
    let tz = Tz::query(src, NOW);
    let s = one(src, id)?;
    let run = can_run(src, ctx);
    let job = job(&s, 0, &tz, run).ok_or_else(|| not_found(id))?;
    let exec = s.get(0, "tk_execute");
    // A routine's name alone, or a statement.
    let routine = exec.is_some_and(|e| !e.contains(char::is_whitespace));
    let steps = exec
        .map(|e| JobStep {
            name: job.name.clone(),
            kind: Some(if routine { "Rutina" } else { "SQL" }.into()),
            target: owned(s.get(0, "tk_dbs")),
            command: Some(e.to_string()),
        })
        .into_iter()
        .collect();
    let days: Vec<&str> = DAYS
        .iter()
        .filter(|(c, _)| yes(s.get(0, c)))
        .map(|(_, d)| *d)
        .collect();
    let days = days.join(", ");
    let properties = props(vec![
        prop("", "Tipo", s.get(0, "tk_type")),
        prop("", "Base", s.get(0, "tk_dbs")),
        prop("", "Frecuencia", s.get(0, "tk_frequency")),
        prop("", "Desde", s.get(0, "tk_start_time")),
        prop("", "Hasta", s.get(0, "tk_stop_time")),
        prop("", "Días", Some(&days)),
        prop("", "Próxima ejecución", s.get(0, "tk_next_execution")),
        prop("", "Prioridad", s.get(0, "tk_priority")),
        prop("", "Ejecuciones", s.get(0, "tk_total_executions")),
        prop("", "Tiempo total (s)", s.get(0, "tk_total_time")),
        prop("Sensor", "Tabla de resultados", s.get(0, "tk_result_table")),
        prop("Sensor", "Conservar datos", s.get(0, "tk_delete")),
        prop("Sensor", "Creación de la tabla", s.get(0, "tk_create")),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
    })
}

pub(super) fn runs_sql(task: u64, limit: u32) -> String {
    format!(
        "SELECT FIRST {limit} run_id, run_task_seq, CAST(run_time AS CHAR(19)) AS run_time, run_retcode, run_duration
  FROM sysadmin:ph_run WHERE run_task_id = {task}
 ORDER BY run_id DESC"
    )
}

pub fn runs(src: &mut dyn JobSource, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let n = int_id(id)?;
    let tz = Tz::query(src, NOW);
    let s = src.query(&runs_sql(n, limit), &[])?;
    // The alerts a run raised, by the task's sequence number (best effort).
    let alerts = src
        .query(
            &format!(
                "SELECT alert_task_seq, alert_message FROM sysadmin:ph_alert WHERE alert_task_id = {n}"
            ),
            &[],
        )
        .unwrap_or_else(|e| {
            tracing::debug!("odbc jobs: ph_alert failed: {e}");
            Set::default()
        });
    let alert = |seq: Option<&str>| -> Option<String> {
        let seq = seq?;
        let msgs: Vec<&str> = (0..alerts.rows.len())
            .filter(|r| alerts.get(*r, "alert_task_seq") == Some(seq))
            .filter_map(|r| alerts.get(r, "alert_message"))
            .collect();
        (!msgs.is_empty()).then(|| msgs.join("\n"))
    };
    let mut out: Vec<JobRun> = (0..s.rows.len())
        .map(|r| {
            let rc = int(s.get(r, "run_retcode"));
            let mut msg: Vec<String> = Vec::new();
            if let Some(c) = rc.filter(|c| *c != 0) {
                msg.push(format!("Código de retorno {c}"));
            }
            if let Some(a) = alert(s.get(r, "run_task_seq")) {
                msg.push(a);
            }
            JobRun {
                id: owned(s.get(r, "run_id")),
                started: tz.utc(s.get(r, "run_time")),
                duration_ms: secs_ms(s.get(r, "run_duration")),
                outcome: outcome(rc),
                trigger: None,
                message: (!msg.is_empty()).then(|| msg.join("\n")),
                steps: Vec::new(),
            }
        })
        .collect();
    newest_first(&mut out);
    Ok(out)
}

pub fn run(src: &mut dyn JobSource, ctx: &Ctx, id: &str) -> Result<()> {
    if !in_sysadmin(ctx) {
        return Err(Error::Unsupported(
            "«ejecutar ahora» usa exectask_async, que solo se puede llamar con la conexión abierta en la base sysadmin"
                .into(),
        ));
    }
    let s = one(src, id)?;
    let name = s.get(0, "tk_name").ok_or_else(|| not_found(id))?;
    src.query(
        &format!("EXECUTE FUNCTION exectask_async({})", lit(name)),
        &[],
    )
    .map(|_| ())
}

pub fn set_enabled(src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    let n = int_id(id)?;
    let changed = src.exec(
        &format!(
            "UPDATE sysadmin:ph_task SET tk_enable = '{}' WHERE tk_id = {n}",
            if enabled { "t" } else { "f" }
        ),
        &[],
    )?;
    if changed == Some(0) {
        return Err(not_found(id));
    }
    Ok(())
}

/// A DATETIME HOUR TO SECOND literal, from text with that shape only.
fn dt_literal(v: Option<&str>) -> String {
    match v.map(str::trim).filter(|t| parse_time(t).is_some()) {
        Some(t) if t.bytes().all(|b| b.is_ascii_digit() || b == b':') => {
            format!("DATETIME({t}) HOUR TO SECOND")
        }
        _ => "NULL".into(),
    }
}

/// An INTERVAL DAY TO SECOND literal (`1 00:00:00`), from text with that
/// shape only.
fn interval_literal(v: Option<&str>) -> String {
    let t = v.map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "));
    match t {
        Some(t)
            if !t.is_empty()
                && t.bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b' ' | b':' | b'.'))
                && t.bytes().next().is_some_and(|b| b.is_ascii_digit()) =>
        {
            let qualifier = if t.contains(' ') {
                "DAY TO SECOND"
            } else {
                "HOUR TO SECOND"
            };
            format!("INTERVAL({t}) {qualifier}")
        }
        _ => "NULL".into(),
    }
}

fn bool_literal(v: Option<&str>) -> &'static str {
    if yes(v) {
        "'t'"
    } else {
        "'f'"
    }
}

pub fn script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let s = one(src, id)?;
    let text = |c: &str| s.get(0, c).map(lit).unwrap_or_else(|| "NULL".into());
    let mut cols: Vec<(&str, String)> = vec![
        ("tk_name", text("tk_name")),
        ("tk_description", text("tk_description")),
        ("tk_type", text("tk_type")),
        ("tk_group", text("tk_group")),
        ("tk_dbs", text("tk_dbs")),
        ("tk_execute", text("tk_execute")),
        ("tk_start_time", dt_literal(s.get(0, "tk_start_time"))),
        ("tk_stop_time", dt_literal(s.get(0, "tk_stop_time"))),
        ("tk_frequency", interval_literal(s.get(0, "tk_frequency"))),
    ];
    for (c, _) in DAYS {
        cols.push((c, bool_literal(s.get(0, c)).into()));
    }
    cols.push(("tk_enable", bool_literal(s.get(0, "tk_enable")).into()));
    if let Some(p) = int(s.get(0, "tk_priority")) {
        cols.push(("tk_priority", p.to_string()));
    }
    if s.get(0, "tk_result_table").is_some() {
        cols.push(("tk_result_table", text("tk_result_table")));
        cols.push(("tk_create", text("tk_create")));
        cols.push(("tk_delete", interval_literal(s.get(0, "tk_delete"))));
    }
    let newline = cols.iter().any(|(_, v)| v.contains('\n'));
    let mut out = String::from("DATABASE sysadmin;\n");
    if newline {
        // Quoted strings with line breaks need this in Informix.
        out.push_str("EXECUTE PROCEDURE IFX_ALLOW_NEWLINE('t');\n");
    }
    out.push_str(&format!(
        "INSERT INTO ph_task (\n    {}\n) VALUES (\n    {}\n);\n",
        cols.iter()
            .map(|(c, _)| *c)
            .collect::<Vec<_>>()
            .join(",\n    "),
        cols.iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join(",\n    "),
    ));
    Ok(out)
}

#[cfg(test)]
pub(super) mod fixtures {
    pub(in crate::jobs) use super::{runs_sql, tasks_sql, ASYNC, NOW};
}
