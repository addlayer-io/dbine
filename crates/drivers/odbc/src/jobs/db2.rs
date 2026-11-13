//! Db2's Administrative Task Scheduler: on LUW, the `SYSTOOLS` views
//! `ADMIN_TASK_LIST` and `ADMIN_TASK_STATUS` (created by the first
//! `ADMIN_TASK_ADD`); on z/OS, the `DSNADM.ADMIN_TASK_LIST()` and
//! `ADMIN_TASK_STATUS()` table functions. Neither runs a task on demand or
//! has an enabled flag: a task runs while its schedule, end time and
//! maximum of runs allow it.

use super::*;
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{JobStep, ServerJob};

pub const NO_RUN_NOW: &str =
    "el planificador de tareas de Db2 no tiene «ejecutar ahora»: las tareas corren solo según su calendario";
pub const NO_ENABLE: &str =
    "el planificador de tareas de Db2 no habilita ni deshabilita tareas: se cambian con SYSPROC.ADMIN_TASK_UPDATE";

const KIND: &str = "ADMIN_TASK";

pub(crate) const NOW: &str = "SELECT CURRENT TIMESTAMP AS LOCAL_NOW, CURRENT TIMESTAMP - CURRENT TIMEZONE AS UTC_NOW FROM SYSIBM.SYSDUMMY1";

// ------------------------------------------------------------------ LUW

pub(crate) const ATS_TABLES: &str = "SELECT TABNAME FROM SYSCAT.TABLES WHERE TABSCHEMA = 'SYSTOOLS' AND TABNAME IN ('ADMIN_TASK_LIST', 'ADMIN_TASK_STATUS')";

const ATS_ENABLE: &str =
    "SELECT REG_VAR_VALUE FROM SYSIBMADM.REG_VARIABLES WHERE REG_VAR_NAME = 'DB2_ATS_ENABLE'";

pub(crate) const NO_ATS: &str = "Esta base no tiene el planificador de tareas de Db2 (SYSTOOLS.ADMIN_TASK_LIST): se crea con la primera llamada a SYSPROC.ADMIN_TASK_ADD.";
pub(crate) const ATS_OFF: &str =
    "DB2_ATS_ENABLE no está activado en la instancia: el planificador no ejecuta las tareas.";
pub(crate) const OWN_ONLY: &str = "Db2 muestra solo las tareas que creó este usuario.";
pub(crate) const NO_READ: &str = "Este usuario no puede leer el planificador de tareas de Db2.";

/// Which of the two views exist. Both, when the catalog can't be read
/// (the queries on them decide).
struct Ats {
    list: bool,
    status: bool,
}

fn probe(src: &mut dyn JobSource) -> Ats {
    match src.query(ATS_TABLES, &[]) {
        Ok(s) => {
            let has = |n: &str| (0..s.rows.len()).any(|r| s.at(r, 0).map(str::trim) == Some(n));
            Ats {
                list: has("ADMIN_TASK_LIST"),
                status: has("ADMIN_TASK_STATUS"),
            }
        }
        Err(e) => {
            tracing::debug!("odbc jobs: SYSCAT.TABLES failed: {e}");
            Ats {
                list: true,
                status: true,
            }
        }
    }
}

/// The note when `DB2_ATS_ENABLE` is off (unset is off too).
pub(crate) fn ats_off(src: &mut dyn JobSource) -> Option<&'static str> {
    let s = src.query(ATS_ENABLE, &[]).ok()?;
    let on = (0..s.rows.len()).any(|r| yes(s.get(r, "REG_VAR_VALUE")));
    (!on).then_some(ATS_OFF)
}

/// The tasks with their last finished run, how many runs and whether one
/// is going on. `task`: just that one.
fn tasks_sql(with_status: bool, task: Option<u64>) -> String {
    let filter = task
        .map(|n| format!("\n WHERE T.TASKID = {n}"))
        .unwrap_or_default();
    if !with_status {
        return format!("SELECT T.* FROM SYSTOOLS.ADMIN_TASK_LIST T{filter}\n ORDER BY T.NAME");
    }
    format!(
        "SELECT T.*, S.STATUS AS LAST_STATUS, S.BEGIN_TIME AS LAST_BEGIN, S.END_TIME AS LAST_END, S.SQLCODE AS LAST_SQLCODE,
       (SELECT MAX(I.INVOCATION) FROM SYSTOOLS.ADMIN_TASK_STATUS I WHERE I.TASKID = T.TASKID) AS INVOCATIONS,
       (SELECT COUNT(*) FROM SYSTOOLS.ADMIN_TASK_STATUS R WHERE R.TASKID = T.TASKID AND R.STATUS = 'RUNNING') AS RUNNING_NOW
  FROM SYSTOOLS.ADMIN_TASK_LIST T
  LEFT JOIN (SELECT TASKID, STATUS, BEGIN_TIME, END_TIME, SQLCODE,
                    ROW_NUMBER() OVER (PARTITION BY TASKID ORDER BY BEGIN_TIME DESC, INVOCATION DESC) AS RN
               FROM SYSTOOLS.ADMIN_TASK_STATUS WHERE STATUS <> 'RUNNING') S
    ON S.TASKID = T.TASKID AND S.RN = 1{filter}
 ORDER BY T.NAME"
    )
}

/// RUNNING, COMPLETE(D), NOTRUN (the scheduler couldn't call the
/// procedure) or UNKNOWN (the database stopped while it ran).
fn luw_outcome(status: &str, sqlcode: Option<i64>) -> JobOutcome {
    let s = status.trim().to_ascii_uppercase();
    match s.as_str() {
        "RUNNING" => JobOutcome::Running,
        "NOTRUN" => JobOutcome::Failed,
        _ if s.starts_with("COMPLETE") => {
            if sqlcode.is_some_and(|c| c < 0) {
                JobOutcome::Failed
            } else {
                JobOutcome::Succeeded
            }
        }
        _ => JobOutcome::Unknown,
    }
}

/// A task is on while its end time hasn't passed and it has runs left.
fn active(tz: &Tz, end: Option<&str>, max: Option<i64>, done: Option<i64>) -> bool {
    !tz.past(end) && !max.is_some_and(|m| done.unwrap_or(0) >= m)
}

fn proc_ref(schema: Option<&str>, name: Option<&str>) -> Option<String> {
    name.map(|n| qualified_name(Quote::Double, schema, n))
}

fn luw_job(s: &Set, r: usize, tz: &Tz, history: bool) -> Option<ServerJob> {
    let id = int(s.get(r, "TASKID"))?;
    let running = int(s.get(r, "RUNNING_NOW")).unwrap_or(0) > 0;
    let outcome = s
        .get(r, "LAST_STATUS")
        .map(|st| luw_outcome(st, int(s.get(r, "LAST_SQLCODE"))));
    Some(ServerJob {
        id: id.to_string(),
        name: s.get(r, "NAME").unwrap_or_default().to_string(),
        kind: KIND.into(),
        group: None,
        description: owned(s.get(r, "REMARKS")),
        owner: owned(s.get(r, "OWNER")),
        enabled: active(
            tz,
            s.get(r, "END_TIME"),
            int(s.get(r, "MAX_INVOCATIONS")),
            int(s.get(r, "INVOCATIONS")),
        ),
        schedule: owned(s.get(r, "SCHEDULE")),
        running,
        last_outcome: last(outcome, running),
        last_started: tz.utc(s.get(r, "LAST_BEGIN")),
        last_duration_ms: duration_ms(s.get(r, "LAST_BEGIN"), s.get(r, "LAST_END")),
        next_run: None,
        can_run: false,
        can_stop: false,
        can_enable: false,
        has_history: history,
        has_script: true,
        system: false,
    })
}

/// A permission or missing-view error turned into a note.
fn luw_note(e: &Error) -> Option<&'static str> {
    match code(e) {
        Some(-551 | -552) => Some(NO_READ),
        Some(-204) => Some(NO_ATS),
        _ => None,
    }
}

pub fn list(src: &mut dyn JobSource) -> Result<JobList> {
    let ats = probe(src);
    if !ats.list {
        return Ok(JobList {
            jobs: Vec::new(),
            notes: vec![NO_ATS.into()],
        });
    }
    let mut notes: Vec<String> = Vec::new();
    if let Some(n) = ats_off(src) {
        notes.push(n.into());
    }
    let tz = Tz::query(src, NOW);
    let s = match src.query(&tasks_sql(ats.status, None), &[]) {
        Ok(s) => s,
        Err(e) => match luw_note(&e) {
            Some(n) => {
                notes.push(n.into());
                return Ok(JobList {
                    jobs: Vec::new(),
                    notes,
                });
            }
            None => return Err(e),
        },
    };
    let jobs: Vec<ServerJob> = (0..s.rows.len())
        .filter_map(|r| luw_job(&s, r, &tz, ats.status))
        .collect();
    notes.push(OWN_ONLY.into());
    Ok(JobList { jobs, notes })
}

pub fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let n = int_id(id)?;
    let ats = probe(src);
    let tz = Tz::query(src, NOW);
    let s = src.query(&tasks_sql(ats.status, Some(n)), &[])?;
    let job = (!s.rows.is_empty())
        .then(|| luw_job(&s, 0, &tz, ats.status))
        .flatten()
        .ok_or_else(|| not_found(id))?;
    let proc = proc_ref(s.get(0, "PROCEDURE_SCHEMA"), s.get(0, "PROCEDURE_NAME"));
    let steps = proc
        .iter()
        .map(|p| JobStep {
            name: p.clone(),
            kind: Some("Procedimiento".into()),
            target: None,
            command: Some(format!("CALL {p}")),
        })
        .collect();
    let properties = props(vec![
        prop("", "Procedimiento", proc.as_deref()),
        prop("", "Consulta de parámetros", s.get(0, "PROCEDURE_INPUT")),
        prop("", "Inicio", s.get(0, "BEGIN_TIME")),
        prop("", "Fin", s.get(0, "END_TIME")),
        prop("", "Máximo de ejecuciones", s.get(0, "MAX_INVOCATIONS")),
        prop("", "Ejecuciones", s.get(0, "INVOCATIONS")),
        prop("", "Tipo de dueño", s.get(0, "OWNERTYPE")),
        prop("", "Opciones", s.get(0, "OPTIONS")),
        prop("", "Modificada", s.get(0, "UPDATE_TIME")),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

/// The message Db2 has for a negative SQLCODE, from its tokens.
const LUW_MSG: &str = ", CASE WHEN S.SQLCODE < 0 THEN VARCHAR(SQLERRM('SQL' || CASE WHEN ABS(S.SQLCODE) < 10000 THEN RIGHT('0000' || TRIM(CHAR(ABS(S.SQLCODE))), 4) ELSE TRIM(CHAR(ABS(S.SQLCODE))) END || 'N', S.SQLERRMC, X'FF', 'en_US', 1), 1024) END AS MSG";

fn luw_runs_sql(task: u64, limit: u32, with_msg: bool) -> String {
    format!(
        "SELECT S.*{} FROM SYSTOOLS.ADMIN_TASK_STATUS S WHERE S.TASKID = {task}
 ORDER BY S.BEGIN_TIME DESC, S.INVOCATION DESC FETCH FIRST {limit} ROWS ONLY",
        if with_msg { LUW_MSG } else { "" }
    )
}

/// SQLERRMC's tokens: the driver sends that FOR BIT DATA column as hex,
/// tokens separated by X'FF'.
pub(crate) fn tokens(v: &str) -> String {
    let t = v.trim();
    let hex =
        t.len().is_multiple_of(2) && !t.is_empty() && t.bytes().all(|b| b.is_ascii_hexdigit());
    if !hex {
        return t.to_string();
    }
    let bytes: Vec<u8> = (0..t.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&t[i..i + 2], 16).ok())
        .collect();
    bytes
        .split(|b| *b == 0xFF)
        .map(|p| String::from_utf8_lossy(p).trim().to_string())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn luw_run(s: &Set, r: usize, tz: &Tz) -> JobRun {
    let status = s.get(r, "STATUS").unwrap_or_default();
    let sqlcode = int(s.get(r, "SQLCODE"));
    let mut msg: Vec<String> = Vec::new();
    match s.get(r, "MSG") {
        Some(m) => msg.push(m.to_string()),
        None => {
            if let Some(c) = sqlcode.filter(|c| *c != 0) {
                let mut m = format!("SQLCODE {c}");
                if let Some(st) = s.get(r, "SQLSTATE") {
                    m.push_str(&format!(", SQLSTATE {st}"));
                }
                if let Some(t) = s.get(r, "SQLERRMC").map(tokens).filter(|t| !t.is_empty()) {
                    m.push_str(&format!(": {t}"));
                }
                msg.push(m);
            }
        }
    }
    if let Some(rc) = int(s.get(r, "RC")).filter(|v| *v != 0) {
        msg.push(format!("Código de retorno {rc}"));
    }
    JobRun {
        id: owned(s.get(r, "INVOCATION")),
        started: tz.utc(s.get(r, "BEGIN_TIME")),
        duration_ms: duration_ms(s.get(r, "BEGIN_TIME"), s.get(r, "END_TIME")),
        outcome: luw_outcome(status, sqlcode),
        trigger: Some("programada".into()),
        message: (!msg.is_empty()).then(|| msg.join("\n")),
        steps: Vec::new(),
    }
}

pub fn runs(src: &mut dyn JobSource, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let n = int_id(id)?;
    let tz = Tz::query(src, NOW);
    let s = match src.query(&luw_runs_sql(n, limit, true), &[]) {
        Ok(s) => s,
        Err(e) if code(&e) == Some(-204) => return Ok(Vec::new()),
        Err(e) => {
            tracing::debug!("odbc jobs: ADMIN_TASK_STATUS with SQLERRM failed: {e}");
            src.query(&luw_runs_sql(n, limit, false), &[])?
        }
    };
    let mut out: Vec<JobRun> = (0..s.rows.len()).map(|r| luw_run(&s, r, &tz)).collect();
    newest_first(&mut out);
    Ok(out)
}

/// `YYYY-MM-DD-HH.MM.SS.000000`, Db2's own form.
pub(crate) fn db2_ts(ms: i64) -> String {
    let iso = rfc3339(ms);
    // 2026-11-12T03:00:00Z
    format!(
        "{}-{}.{}.{}.000000",
        &iso[..10],
        &iso[11..13],
        &iso[14..16],
        &iso[17..19]
    )
}

fn sql_text(v: Option<&str>) -> String {
    v.map(lit).unwrap_or_else(|| "NULL".into())
}

fn sql_ts(v: Option<&str>) -> String {
    v.and_then(parse_ts)
        .map(|ms| format!("TIMESTAMP('{}')", db2_ts(ms)))
        .unwrap_or_else(|| "NULL".into())
}

fn sql_int(v: Option<&str>) -> String {
    int(v)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "NULL".into())
}

/// The begin and end arguments of ADMIN_TASK_ADD, which refuses both in
/// the past (on LUW and z/OS): one that has passed goes as NULL, and a
/// comment line says what it was.
fn window(tz: &Tz, begin: Option<&str>, end: Option<&str>) -> (String, String, Vec<String>) {
    let shown = |v: Option<&str>| v.and_then(parse_ts).map(db2_ts).unwrap_or_default();
    let mut comments = Vec::new();
    let begin_arg = if tz.past(begin) {
        comments.push(format!(
            "-- Inicio original: {}. ADMIN_TASK_ADD no acepta un inicio pasado: sin él, la tarea corre desde ya según su calendario.",
            shown(begin)
        ));
        "NULL".to_string()
    } else {
        sql_ts(begin)
    };
    let end_arg = if tz.past(end) {
        comments.push(format!(
            "-- La tarea terminó el {}. ADMIN_TASK_ADD no acepta un fin pasado: se crea sin fin.",
            shown(end)
        ));
        "NULL".to_string()
    } else {
        sql_ts(end)
    };
    (begin_arg, end_arg, comments)
}

/// `CALL SYSPROC.ADMIN_TASK_ADD(…)` with `comments` above it.
fn add_call(comments: &[String], args: &[String]) -> String {
    let mut out = String::new();
    for c in comments {
        out.push_str(c);
        out.push('\n');
    }
    out.push_str(&format!(
        "CALL SYSPROC.ADMIN_TASK_ADD(\n    {}\n);\n",
        args.join(",\n    ")
    ));
    out
}

pub fn script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let n = int_id(id)?;
    let tz = Tz::query(src, NOW);
    let s = src.query(
        &format!("SELECT T.* FROM SYSTOOLS.ADMIN_TASK_LIST T WHERE T.TASKID = {n}"),
        &[],
    )?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok(luw_script(&s, 0, &tz))
}

fn luw_script(s: &Set, r: usize, tz: &Tz) -> String {
    let (begin, end, comments) = window(tz, s.get(r, "BEGIN_TIME"), s.get(r, "END_TIME"));
    let args = [
        sql_text(s.get(r, "NAME")),
        begin,
        end,
        sql_int(s.get(r, "MAX_INVOCATIONS")),
        sql_text(s.get(r, "SCHEDULE")),
        sql_text(s.get(r, "PROCEDURE_SCHEMA")),
        sql_text(s.get(r, "PROCEDURE_NAME")),
        sql_text(s.get(r, "PROCEDURE_INPUT")),
        sql_text(s.get(r, "OPTIONS")),
        sql_text(s.get(r, "REMARKS")),
    ];
    add_call(&comments, &args)
}

// ----------------------------------------------------------------- z/OS

pub(crate) const ZOS_LIST: &str = "SELECT * FROM TABLE(DSNADM.ADMIN_TASK_LIST()) AS T";
pub(crate) const ZOS_STATUS: &str = "SELECT * FROM TABLE(DSNADM.ADMIN_TASK_STATUS()) AS S";

pub(crate) const NO_ZOS_ATS: &str =
    "Este subsistema no tiene el planificador de tareas administrativas (DSNADM.ADMIN_TASK_LIST).";
pub(crate) const NO_ZOS_STATUS: &str =
    "Sin el privilegio MONITOR1 no se ve cómo terminaron las tareas.";

/// A task name as the app sends it back: up to 128 characters, none of
/// them control characters. It's only ever bound as a parameter.
fn zos_name(id: &str) -> Result<String> {
    let t = id.trim();
    let ok = !t.is_empty() && t.chars().count() <= 128 && !t.chars().any(char::is_control);
    ok.then(|| t.to_string()).ok_or_else(|| bad_id(id))
}

/// The scheduler isn't there, can't be read, or doesn't answer: a note.
fn zos_note(e: &Error) -> Option<String> {
    match code(e) {
        Some(-440 | -204 | -4700) => Some(NO_ZOS_ATS.into()),
        Some(-551 | -552) => Some(NO_READ.into()),
        Some(-471 | -443) => Some(format!(
            "El planificador de tareas administrativas no respondió: {}",
            clip(&e.to_string(), 300)
        )),
        _ => None,
    }
}

fn zos_schedule(s: &Set, r: usize) -> Option<String> {
    if let Some(p) = s.get(r, "POINT_IN_TIME") {
        return Some(p.to_string());
    }
    if let Some(i) = int(s.get(r, "INTERVAL")) {
        return Some(format!("INTERVAL {i}"));
    }
    s.get(r, "TRIGGER_TASK_NAME").map(|t| {
        let mut out = format!("TRIGGER {t}");
        for c in ["TRIGGER_TASK_COND", "TRIGGER_TASK_CODE"] {
            if let Some(v) = s.get(r, c) {
                out.push(' ');
                out.push_str(v);
            }
        }
        out
    })
}

/// An abend code that says something (not empty, not all zeros).
fn nonzero(v: Option<&str>) -> bool {
    v.is_some_and(|v| !v.trim().trim_start_matches(['0', 'x', 'X']).is_empty())
}

fn zos_outcome(s: &Set, r: usize) -> JobOutcome {
    let st = s.get(r, "STATUS").unwrap_or_default().to_ascii_uppercase();
    match st.as_str() {
        "RUNNING" => JobOutcome::Running,
        "NOTRUN" => JobOutcome::Failed,
        _ if st.starts_with("COMPLETE") => {
            let failed = int(s.get(r, "SQLCODE")).is_some_and(|c| c < 0)
                || int(s.get(r, "MAXRC")).is_some_and(|c| c >= 8)
                || nonzero(s.get(r, "SYSTEM_ABENDCD"))
                || nonzero(s.get(r, "USER_ABENDCD"));
            if failed {
                JobOutcome::Failed
            } else {
                JobOutcome::Succeeded
            }
        }
        _ => JobOutcome::Unknown,
    }
}

fn zos_message(s: &Set, r: usize) -> Option<String> {
    let mut msg: Vec<String> = Vec::new();
    if let Some(m) = s.get(r, "MSG") {
        msg.push(m.to_string());
    }
    if let Some(c) = int(s.get(r, "SQLCODE")).filter(|c| *c != 0) {
        let mut m = format!("SQLCODE {c}");
        if let Some(st) = s.get(r, "SQLSTATE") {
            m.push_str(&format!(", SQLSTATE {st}"));
        }
        if let Some(t) = s.get(r, "SQLERRMC").map(tokens).filter(|t| !t.is_empty()) {
            m.push_str(&format!(": {t}"));
        }
        msg.push(m);
    }
    if let Some(rc) = s
        .get(r, "MAXRC")
        .filter(|v| int(Some(v)).is_some_and(|n| n != 0))
    {
        msg.push(format!("Código de retorno {rc}"));
    }
    for (c, label) in [
        ("SYSTEM_ABENDCD", "Abend del sistema"),
        ("USER_ABENDCD", "Abend de usuario"),
    ] {
        if let Some(v) = s.get(r, c).filter(|v| nonzero(Some(v))) {
            msg.push(format!("{label} {v}"));
        }
    }
    (!msg.is_empty()).then(|| msg.join("\n"))
}

/// The status row of a task (the latest run) in `status`.
fn status_row(status: Option<&Set>, name: &str) -> Option<usize> {
    let s = status?;
    (0..s.rows.len()).find(|r| s.get(*r, "TASK_NAME") == Some(name))
}

fn zos_job(t: &Set, r: usize, status: Option<&Set>, tz: &Tz) -> Option<ServerJob> {
    let name = t.get(r, "TASK_NAME")?.to_string();
    let st = status.and_then(|s| status_row(Some(s), &name).map(|i| (s, i)));
    let running = st.is_some_and(|(s, i)| zos_outcome(s, i) == JobOutcome::Running);
    let (outcome, started, took) = match st {
        Some((s, i)) if !running => (
            Some(zos_outcome(s, i)),
            tz.utc(s.get(i, "START_TIMESTAMP")),
            duration_ms(s.get(i, "START_TIMESTAMP"), s.get(i, "END_TIMESTAMP")),
        ),
        _ => (None, None, None),
    };
    let done = st.and_then(|(s, i)| int(s.get(i, "NUM_INVOCATIONS")));
    Some(ServerJob {
        id: name.clone(),
        name,
        kind: KIND.into(),
        group: None,
        description: owned(t.get(r, "DESCRIPTION")),
        owner: owned(get(t, r, &["CREATOR", "USERID"])),
        enabled: active(
            tz,
            t.get(r, "END_TIMESTAMP"),
            int(t.get(r, "MAX_INVOCATIONS")),
            done,
        ),
        schedule: zos_schedule(t, r),
        running,
        last_outcome: last(outcome, running),
        last_started: started,
        last_duration_ms: took,
        next_run: None,
        can_run: false,
        // ADMIN_TASK_CANCEL, through the scheduler's plan (it reads the
        // procedure's output parameters).
        can_stop: running,
        can_enable: false,
        has_history: status.is_some(),
        has_script: true,
        system: false,
    })
}

/// The task list and the status of each task's latest run (`None` when
/// the user can't read it), or the note saying why there's no list.
fn zos_read(src: &mut dyn JobSource) -> Result<std::result::Result<(Set, Option<Set>), String>> {
    let tasks = match src.query(ZOS_LIST, &[]) {
        Ok(s) => s,
        Err(e) => {
            return match zos_note(&e) {
                Some(n) => Ok(Err(n)),
                None => Err(e),
            }
        }
    };
    let status = match src.query(ZOS_STATUS, &[]) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::debug!("odbc jobs: ADMIN_TASK_STATUS failed: {e}");
            None
        }
    };
    Ok(Ok((tasks, status)))
}

pub fn zos_list(src: &mut dyn JobSource) -> Result<JobList> {
    let tz = Tz::query(src, NOW);
    let (tasks, status) = match zos_read(src)? {
        Ok(v) => v,
        Err(note) => {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec![note],
            })
        }
    };
    let mut jobs: Vec<ServerJob> = (0..tasks.rows.len())
        .filter_map(|r| zos_job(&tasks, r, status.as_ref(), &tz))
        .collect();
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    let notes = if status.is_none() {
        vec![NO_ZOS_STATUS.into()]
    } else {
        Vec::new()
    };
    Ok(JobList { jobs, notes })
}

fn zos_row(src: &mut dyn JobSource, id: &str) -> Result<(String, Set, Option<Set>)> {
    let name = zos_name(id)?;
    let (tasks, status) = zos_read(src)?.map_err(Error::Query)?;
    let r = (0..tasks.rows.len())
        .find(|r| tasks.get(*r, "TASK_NAME") == Some(name.as_str()))
        .ok_or_else(|| not_found(id))?;
    // Only that row, so the callers read row 0.
    let one = Set {
        cols: tasks.cols.clone(),
        rows: vec![tasks.rows[r].clone()],
    };
    Ok((name, one, status))
}

pub fn zos_detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let name = zos_name(id)?;
    let tz = Tz::query(src, NOW);
    let (_, t, status) = zos_row(src, &name)?;
    let job = zos_job(&t, 0, status.as_ref(), &tz).ok_or_else(|| not_found(id))?;
    let mut steps = Vec::new();
    if let Some(p) = proc_ref(t.get(0, "PROCEDURE_SCHEMA"), t.get(0, "PROCEDURE_NAME")) {
        steps.push(JobStep {
            name: p.clone(),
            kind: Some("Procedimiento".into()),
            target: owned(t.get(0, "DB2_SSID")),
            command: Some(format!("CALL {p}")),
        });
    } else if let Some(lib) = t.get(0, "JCL_LIBRARY") {
        let member = t.get(0, "JCL_MEMBER").unwrap_or_default();
        steps.push(JobStep {
            name: member.to_string(),
            kind: Some("JCL".into()),
            target: None,
            command: Some(format!("{lib}({member})")),
        });
    }
    let properties = props(vec![
        prop("", "Inicio", t.get(0, "BEGIN_TIMESTAMP")),
        prop("", "Fin", t.get(0, "END_TIMESTAMP")),
        prop("", "Máximo de ejecuciones", t.get(0, "MAX_INVOCATIONS")),
        prop("", "Intervalo (minutos)", t.get(0, "INTERVAL")),
        prop("", "Momentos", t.get(0, "POINT_IN_TIME")),
        prop("", "Tarea que la dispara", t.get(0, "TRIGGER_TASK_NAME")),
        prop("", "Condición del disparo", t.get(0, "TRIGGER_TASK_COND")),
        prop("", "Código del disparo", t.get(0, "TRIGGER_TASK_CODE")),
        prop("", "Subsistema", t.get(0, "DB2_SSID")),
        prop("", "Consulta de parámetros", t.get(0, "PROCEDURE_INPUT")),
        prop("", "Biblioteca JCL", t.get(0, "JCL_LIBRARY")),
        prop("", "Miembro JCL", t.get(0, "JCL_MEMBER")),
        prop("", "Espera al job", t.get(0, "JOB_WAIT")),
        prop("", "Usuario de ejecución", t.get(0, "USERID")),
        prop("", "Modificada", t.get(0, "LAST_MODIFIED")),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

fn zos_run(s: &Set, r: usize, tz: &Tz) -> JobRun {
    JobRun {
        id: owned(s.get(r, "NUM_INVOCATIONS")),
        started: tz.utc(s.get(r, "START_TIMESTAMP")),
        duration_ms: duration_ms(s.get(r, "START_TIMESTAMP"), s.get(r, "END_TIMESTAMP")),
        outcome: zos_outcome(s, r),
        trigger: None,
        message: zos_message(s, r),
        steps: Vec::new(),
    }
}

pub fn zos_runs(src: &mut dyn JobSource, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let name = zos_name(id)?;
    let tz = Tz::query(src, NOW);
    // Db2 10 and later keep `limit` runs per task; Db2 9 only the last.
    let sql = format!(
        "SELECT * FROM TABLE(DSNADM.ADMIN_TASK_STATUS({limit})) AS S WHERE S.TASK_NAME = ?"
    );
    let s = match src.query(&sql, &[&name]) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("odbc jobs: ADMIN_TASK_STATUS(max) failed: {e}");
            src.query(&format!("{ZOS_STATUS} WHERE S.TASK_NAME = ?"), &[&name])?
        }
    };
    let mut out: Vec<JobRun> = (0..s.rows.len()).map(|r| zos_run(&s, r, &tz)).collect();
    newest_first(&mut out);
    Ok(out)
}

pub fn zos_script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let name = zos_name(id)?;
    let tz = Tz::query(src, NOW);
    let (_, t, _) = zos_row(src, &name)?;
    let g = |c: &str| t.get(0, c);
    let (begin, end, comments) = window(&tz, g("BEGIN_TIMESTAMP"), g("END_TIMESTAMP"));
    let args = [
        // user-ID and password: the task runs as the default execution user.
        "NULL".to_string(),
        "NULL".to_string(),
        begin,
        end,
        sql_int(g("MAX_INVOCATIONS")),
        sql_int(g("INTERVAL")),
        sql_text(g("POINT_IN_TIME")),
        sql_text(g("TRIGGER_TASK_NAME")),
        sql_text(g("TRIGGER_TASK_COND")),
        sql_int(g("TRIGGER_TASK_CODE")),
        sql_text(g("DB2_SSID")),
        sql_text(g("PROCEDURE_SCHEMA")),
        sql_text(g("PROCEDURE_NAME")),
        sql_text(g("PROCEDURE_INPUT")),
        sql_text(g("JCL_LIBRARY")),
        sql_text(g("JCL_MEMBER")),
        sql_text(g("JOB_WAIT")),
        sql_text(g("TASK_NAME")),
        sql_text(g("DESCRIPTION")),
        // return-code and message (output).
        "?".to_string(),
        "?".to_string(),
    ];
    Ok(add_call(&comments, &args))
}

#[cfg(test)]
pub(super) mod fixtures {
    pub(in crate::jobs) use super::{tokens, ATS_TABLES, NOW, ZOS_LIST, ZOS_STATUS};
}
