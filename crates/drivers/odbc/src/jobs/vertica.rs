//! Vertica (11.1.1 on): scheduled execution of stored procedures. A
//! schedule (`CREATE SCHEDULE … USING CRON | USING DATETIMES`) fires a
//! trigger (`CREATE TRIGGER … ON SCHEDULE … EXECUTE PROCEDURE … AS
//! DEFINER`) that runs a procedure; each trigger is a job. Triggers come
//! from `v_catalog.stored_proc_triggers`, their schedules from
//! `v_catalog.user_schedules` (matched by `attached_trigger`), and the next
//! start from `v_catalog.scheduler_time_table`, which only the active
//! scheduler node fills.
//!
//! Making schedules and triggers is a superuser's, but on 25.4 anyone
//! reads all of them in these tables (even in schemas they have no
//! USAGE on). Where the catalog keeps them from someone who isn't a
//! superuser (it refuses the query, or `system_tables.is_superuser_only`
//! says so), the list says that only a superuser sees them.
//!
//! `EXECUTE_TRIGGER` runs one now and `ENABLE_TRIGGER` turns one on and
//! off; both are superuser only. `EXECUTE_TRIGGER` is the only way to run
//! one and runs the procedure in the calling session: it returns when the
//! procedure ends. Vertica keeps no history of the runs (the scheduler's
//! statistics are counts) and nothing stops one. `procedure_args` holds
//! the values the trigger passes as text, with the quotes inside texts no
//! longer doubled: the script is offered only when they read back as
//! the same values.
//!
//! Ids are the trigger's `"schema"."trigger"`, parsed back strictly and
//! bound when looked up; the meta-functions get the names read from the
//! catalog, as literals.

use super::*;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{JobStep, ServerJob};

pub const NO_HISTORY: &str = "Vertica no guarda el historial de sus triggers programados";
const NO_ARGS_SCRIPT: &str = "el catálogo de Vertica no guarda los argumentos del trigger de forma que se puedan volver a escribir: el script no se puede armar";
const NO_SCHEDULE_SCRIPT: &str =
    "no se encontró la programación de este trigger en USER_SCHEDULES: el script no se puede armar";
const ONLY_SUPERUSER: &str =
    "en Vertica solo un superusuario ejecuta, habilita o deshabilita un trigger programado";

const NOT_HERE: &str =
    "Este servidor no tiene ejecución programada de procedimientos (llega con Vertica 11.1.1).";
const OFF: &str = "El planificador de procedimientos está apagado (EnableStoredProcedureScheduler = 0): los triggers no se ejecutan.";
const SUPERUSER_ONLY: &str = "Solo un superusuario ve los triggers programados de Vertica.";

const KIND: &str = "Trigger";

pub(super) const TRIGGERS: &str =
    "SELECT * FROM v_catalog.stored_proc_triggers ORDER BY schema_name, trigger_name";
pub(super) const ONE: &str =
    "SELECT * FROM v_catalog.stored_proc_triggers WHERE schema_name = ? AND trigger_name = ?";
pub(super) const SCHEDULES: &str = "SELECT * FROM v_catalog.user_schedules";
/// The next starts in UTC, computed by the server: the driver's text of a
/// TIMESTAMPTZ may come without its offset.
pub(super) const TIME_TABLE_UTC: &str = "SELECT schedule_name, attached_trigger, scheduled_execution_time AT TIME ZONE 'UTC' AS next_utc FROM v_catalog.scheduler_time_table";
/// [`TIME_TABLE_UTC`] where that column has another name.
pub(super) const TIME_TABLE: &str = "SELECT * FROM v_catalog.scheduler_time_table";
pub(super) const PROCEDURES: &str =
    "SELECT schema_name, procedure_name FROM v_catalog.user_procedures WHERE procedure_name = ?";
/// No row: the table isn't there (before 11.1.1).
pub(super) const TABLE: &str = "SELECT is_superuser_only AS S FROM v_catalog.system_tables WHERE table_schema = 'v_catalog' AND table_name ILIKE 'stored_proc_triggers'";
pub(super) const SUPERUSER: &str =
    "SELECT is_super_user AS S FROM v_catalog.users WHERE user_name = CURRENT_USER()";
pub(super) const CONFIG: &str =
    "SELECT GET_CONFIG_PARAMETER('EnableStoredProcedureScheduler') AS V";
pub(super) const ASN: &str = "SELECT ACTIVE_SCHEDULER_NODE() AS ASN";
pub(super) const ME: &str = "SELECT LOCAL_NODE_NAME() AS ME";

const NEXT_TIME: &[&str] = &[
    "next_utc",
    "scheduled_execution_time",
    "scheduled_time",
    "execution_time",
];

/// The trigger's arguments as the catalog keeps them (`7, 'x'`), when
/// they go back into SQL as the same values: numbers, NULL, TRUE, FALSE
/// and texts without a quote inside (the catalog doesn't double them).
/// Empty when it takes none.
pub(super) fn safe_args(v: Option<&str>) -> Option<String> {
    let mut rest = v.unwrap_or_default().trim();
    if rest.is_empty() {
        return Some(String::new());
    }
    let mut out: Vec<String> = Vec::new();
    loop {
        let (tok, after) = match rest.strip_prefix('\'') {
            Some(r) => {
                let end = r.find('\'')?;
                (format!("'{}'", &r[..end]), &r[end + 1..])
            }
            None => {
                let end = rest.find(',').unwrap_or(rest.len());
                let t = rest[..end].trim();
                let word = ["NULL", "TRUE", "FALSE"]
                    .iter()
                    .any(|w| t.eq_ignore_ascii_case(w));
                let number = t.parse::<f64>().is_ok_and(f64::is_finite)
                    && t.bytes().all(|b| {
                        b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E')
                    });
                if !(word || number) {
                    return None;
                }
                (t.to_string(), &rest[end..])
            }
        };
        out.push(tok);
        let after = after.trim_start();
        if after.is_empty() {
            break;
        }
        rest = after.strip_prefix(',')?.trim_start();
    }
    Some(out.join(", "))
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

/// A name as a meta-function takes it (Vertica folds no case).
fn meta_name(schema: &str, name: &str) -> String {
    dotted(schema, name, false)
}

fn best_effort(src: &mut dyn JobSource, sql: &str) -> Set {
    src.query(sql, &[]).unwrap_or_else(|e| {
        tracing::debug!("odbc jobs: {sql} failed: {e}");
        Set::default()
    })
}

/// The `user_schedules` row of the trigger: the one whose
/// `attached_trigger` names it, preferring one in the trigger's schema.
fn schedule_row(sch: &Set, schema: &str, trigger: &str) -> Option<usize> {
    let qualified = format!("{schema}.{trigger}");
    let hits: Vec<usize> = (0..sch.rows.len())
        .filter(|r| {
            sch.get(*r, "attached_trigger")
                .is_some_and(|a| a == trigger || a == qualified)
        })
        .collect();
    match hits.as_slice() {
        [one] => Some(*one),
        many => many
            .iter()
            .copied()
            .find(|r| sch.get(*r, "schema_name") == Some(schema)),
    }
}

/// The timestamps of a `DATE_TIME_LIST`.
fn datetimes(list: &str) -> Vec<&str> {
    list.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect()
}

/// The schedule clause: `USING CRON '0 1 * * *'` or `USING
/// DATETIMES('2026-01-01 12:00:00-00', …)`.
fn using(sch: &Set, r: usize) -> Option<String> {
    let text = sch.get(r, "date_time_string")?;
    let kind = sch
        .get(r, "date_time_type")
        .unwrap_or_default()
        .to_ascii_uppercase();
    Some(if kind.contains("LIST") {
        let list: Vec<String> = datetimes(text).into_iter().map(lit).collect();
        format!("USING DATETIMES({})", list.join(", "))
    } else {
        format!("USING CRON {}", lit(text))
    })
}

/// The next start the scheduler has for the trigger, RFC 3339 in UTC.
fn next_run(tt: &Set, schedule: Option<&str>, trigger: &str) -> Option<String> {
    (0..tt.rows.len())
        .filter(|r| {
            tt.get(*r, "attached_trigger") == Some(trigger)
                && (schedule.is_none() || tt.get(*r, "schedule_name") == schedule)
        })
        .filter_map(|r| get(tt, r, NEXT_TIME).and_then(parse_ts_utc))
        .min()
        .map(rfc3339)
}

/// The scheduler's time table, with the next starts in UTC where the
/// server can say so.
fn time_table(src: &mut dyn JobSource) -> Set {
    src.query(TIME_TABLE_UTC, &[]).unwrap_or_else(|e| {
        tracing::debug!("odbc jobs: scheduler_time_table in UTC failed: {e}");
        best_effort(src, TIME_TABLE)
    })
}

struct Seen<'a> {
    schedules: &'a Set,
    time_table: &'a Set,
    superuser: bool,
}

fn job(s: &Set, r: usize, c: &Seen) -> Option<ServerJob> {
    let schema = s.get(r, "schema_name")?;
    let name = s.get(r, "trigger_name")?;
    let sr = schedule_row(c.schedules, schema, name);
    let sched_name = sr.and_then(|i| c.schedules.get(i, "schedule_name"));
    let enabled = s.get(r, "enabled").is_none_or(|v| yes(Some(v)));
    Some(ServerJob {
        id: quote_pair(schema, name),
        name: name.to_string(),
        kind: KIND.into(),
        group: Some(schema.to_string()),
        description: None,
        owner: owned(s.get(r, "owner")),
        enabled,
        schedule: sr.and_then(|i| using(c.schedules, i)),
        running: false,
        last_outcome: None,
        last_started: None,
        last_duration_ms: None,
        // The time table keeps a disabled trigger's next start.
        next_run: enabled
            .then(|| next_run(c.time_table, sched_name, name))
            .flatten(),
        can_run: c.superuser,
        can_stop: false,
        can_enable: c.superuser,
        has_history: false,
        has_script: sr.is_some() && safe_args(s.get(r, "procedure_args")).is_some(),
    })
}

fn superuser(src: &mut dyn JobSource) -> bool {
    yes(best_effort(src, SUPERUSER).get(0, "S"))
}

/// What `system_tables` says of `stored_proc_triggers`: `None` when it
/// can't be read, `Some(None)` when the table isn't there, else whether
/// it's for superusers only.
fn table(src: &mut dyn JobSource) -> Option<Option<bool>> {
    match src.query(TABLE, &[]) {
        Ok(s) if s.rows.is_empty() => Some(None),
        Ok(s) => Some(Some(yes(s.get(0, "S")))),
        Err(e) => {
            tracing::debug!("odbc jobs: system_tables failed: {e}");
            None
        }
    }
}

fn noted(note: &str) -> JobList {
    JobList {
        jobs: Vec::new(),
        notes: vec![note.into()],
    }
}

pub fn list(src: &mut dyn JobSource) -> Result<JobList> {
    let su = superuser(src);
    let s = match src.query(TRIGGERS, &[]) {
        Ok(s) => s,
        Err(e) => {
            return match table(src) {
                Some(None) => Ok(noted(NOT_HERE)),
                // Refused to someone who isn't a superuser.
                _ if !su => Ok(noted(SUPERUSER_ONLY)),
                _ => Err(e),
            };
        }
    };
    let schedules = best_effort(src, SCHEDULES);
    let time_table = time_table(src);
    let c = Seen {
        schedules: &schedules,
        time_table: &time_table,
        superuser: su,
    };
    let jobs: Vec<ServerJob> = (0..s.rows.len()).filter_map(|r| job(&s, r, &c)).collect();
    let mut notes: Vec<String> = Vec::new();
    // A superuser-only table answers someone else with what they may see,
    // if anything; otherwise they see every trigger, as a superuser.
    if !su && table(src) == Some(Some(true)) {
        notes.push(SUPERUSER_ONLY.into());
    }
    if jobs.is_empty() {
        return Ok(JobList { jobs, notes });
    }
    if best_effort(src, CONFIG).get(0, "V").map(str::trim) == Some("0") {
        notes.push(OFF.into());
    }
    // The next starts are only known on the node that runs the scheduler.
    if time_table.rows.is_empty() && jobs.iter().any(|j| j.enabled) {
        let asn = best_effort(src, ASN).text("ASN");
        let me = best_effort(src, ME).text("ME");
        if let (Some(asn), Some(me)) = (asn, me) {
            if !asn.eq_ignore_ascii_case(&me) {
                notes.push(format!(
                    "La próxima ejecución de cada trigger se ve conectado al nodo {asn}, que coordina el planificador."
                ));
            }
        }
    }
    Ok(JobList { jobs, notes })
}

/// The trigger's row, looked up by the names in its id.
fn one(src: &mut dyn JobSource, id: &str) -> Result<Set> {
    let (schema, name) = parse_pair(id)?;
    let s = src.query(ONE, &[&schema, &name])?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok(s)
}

/// The procedure as the trigger calls it: qualified with its schema when
/// `user_procedures` has only one by that name.
fn procedure(src: &mut dyn JobSource, s: &Set) -> Option<String> {
    let name = s.get(0, "procedure_name")?;
    let p = src.query(PROCEDURES, &[name]).unwrap_or_else(|e| {
        tracing::debug!("odbc jobs: user_procedures failed: {e}");
        Set::default()
    });
    let mut schemas: Vec<&str> = (0..p.rows.len())
        .filter_map(|r| p.get(r, "schema_name"))
        .collect();
    // Overloads of it in one schema count once.
    schemas.sort_unstable();
    schemas.dedup();
    Some(match schemas.as_slice() {
        [schema] => quote_pair(schema, name),
        _ => q(name),
    })
}

pub fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let s = one(src, id)?;
    let schedules = best_effort(src, SCHEDULES);
    let time_table = time_table(src);
    let c = Seen {
        schedules: &schedules,
        time_table: &time_table,
        superuser: superuser(src),
    };
    let job = job(&s, 0, &c).ok_or_else(|| not_found(id))?;
    let args = s.get(0, "procedure_args").unwrap_or_default();
    let steps = s
        .get(0, "procedure_name")
        .map(|p| JobStep {
            name: job.name.clone(),
            kind: Some("Procedimiento".into()),
            target: None,
            command: Some(format!("{p}({args})")),
        })
        .into_iter()
        .collect();
    let sr = schedule_row(
        &schedules,
        job.group.as_deref().unwrap_or_default(),
        &job.name,
    );
    let sched = sr.and_then(|r| {
        Some(format!(
            "{}.{}",
            schedules.get(r, "schema_name")?,
            schedules.get(r, "schedule_name")?
        ))
    });
    let properties = props(vec![
        prop("", "Procedimiento", s.get(0, "procedure_name")),
        prop(
            "",
            "Argumentos del procedimiento",
            s.get(0, "procedure_args"),
        ),
        prop("", "Programación", sched.as_deref()),
        prop(
            "",
            "Dueño de la programación",
            sr.and_then(|r| schedules.get(r, "owner")),
        ),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
    })
}

/// The trigger's names from the catalog, for a superuser.
fn controlled(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let s = one(src, id)?;
    if !superuser(src) {
        return Err(Error::Unsupported(ONLY_SUPERUSER.into()));
    }
    let schema = s.get(0, "schema_name").ok_or_else(|| not_found(id))?;
    let name = s.get(0, "trigger_name").ok_or_else(|| not_found(id))?;
    Ok(meta_name(schema, name))
}

pub fn run(src: &mut dyn JobSource, id: &str) -> Result<()> {
    let name = controlled(src, id)?;
    src.query(&format!("SELECT EXECUTE_TRIGGER({})", lit(&name)), &[])
        .map(|_| ())
}

pub fn set_enabled(src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    let name = controlled(src, id)?;
    src.query(
        &format!("SELECT ENABLE_TRIGGER({}, {enabled})", lit(&name)),
        &[],
    )
    .map(|_| ())
}

pub fn script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let s = one(src, id)?;
    let schema = s.get(0, "schema_name").ok_or_else(|| not_found(id))?;
    let name = s.get(0, "trigger_name").ok_or_else(|| not_found(id))?;
    let args = safe_args(s.get(0, "procedure_args"))
        .ok_or_else(|| Error::Unsupported(NO_ARGS_SCRIPT.into()))?;
    let schedules = best_effort(src, SCHEDULES);
    let r = schedule_row(&schedules, schema, name)
        .ok_or_else(|| Error::Unsupported(NO_SCHEDULE_SCRIPT.into()))?;
    let sched = quote_pair(
        schedules.get(r, "schema_name").unwrap_or(schema),
        schedules
            .get(r, "schedule_name")
            .ok_or_else(|| Error::Unsupported(NO_SCHEDULE_SCRIPT.into()))?,
    );
    let using =
        using(&schedules, r).ok_or_else(|| Error::Unsupported(NO_SCHEDULE_SCRIPT.into()))?;
    let procedure = procedure(src, &s).ok_or_else(|| not_found(id))?;
    let trigger = quote_pair(schema, name);
    let mut out = format!(
        "CREATE SCHEDULE {sched} {using};\nCREATE TRIGGER {trigger} ON SCHEDULE {sched} EXECUTE PROCEDURE {procedure}({args}) AS DEFINER;\n"
    );
    if let Some(owner) = s.get(0, "owner") {
        out.push_str(&format!("ALTER TRIGGER {trigger} OWNER TO {};\n", q(owner)));
    }
    if s.get(0, "enabled").is_some_and(|v| !yes(Some(v))) {
        out.push_str(&format!(
            "SELECT ENABLE_TRIGGER({}, false);\n",
            lit(&meta_name(schema, name))
        ));
    }
    Ok(out)
}

#[cfg(test)]
pub(super) mod fixtures {
    pub(in crate::jobs) use super::{
        safe_args, ASN, CONFIG, ME, ONE, PROCEDURES, SCHEDULES, SUPERUSER, TABLE, TIME_TABLE,
        TIME_TABLE_UTC, TRIGGERS,
    };
}
