//! Hive 4 (and Cloudera's Hive): scheduled queries, from
//! `information_schema.scheduled_queries`, and their executions from
//! `sys.scheduled_executions`, which keeps each one's scheduled query id
//! (the information_schema view only has the name, which repeats across
//! namespaces: it's the fallback, for names no other query has).
//! `ALTER SCHEDULED QUERY … EXECUTE` moves the next run to now (the
//! HiveServer2 executor picks it up), `ENABLED` / `DISABLED` switch it, and
//! `KILL QUERY` stops the execution in flight. The ALTER statements act on
//! the session's namespace (`hive.scheduled.queries.namespace`), so a
//! schedule of another namespace is only read.
//!
//! The views' timestamps are in the HiveServer2 time zone. Names go in
//! statements only after checking they're plain identifiers; ids are the
//! numeric `scheduled_query_id`.

use super::*;
use dbine_driver::{JobStep, ServerJob};

const KIND: &str = "Scheduled query";

pub(crate) const NOW: &str = "SELECT current_timestamp() AS local_now, unix_timestamp() AS utc_now";
pub(super) const NAMESPACE: &str = "SET hive.scheduled.queries.namespace";

const COLUMNS: &str = "q.scheduled_query_id AS id, q.schedule_name AS name, q.enabled AS enabled,
       q.cluster_namespace AS namespace, q.schedule AS schedule, q.`user` AS owner, q.query AS query,
       q.next_execution AS next_execution";

pub(crate) const NO_SCHEDULES: &str = "Este servidor no tiene consultas programadas: llegaron con Hive 4.0 y se leen de la base information_schema.";

/// A column by name, also when HiveServer2 prefixes it with its table.
pub(crate) fn col<'a>(s: &'a Set, r: usize, name: &str) -> Option<&'a str> {
    s.get(r, name).or_else(|| {
        let i = s.cols.iter().position(|c| {
            c.rsplit_once('.')
                .is_some_and(|(_, n)| n.trim().eq_ignore_ascii_case(name))
        })?;
        s.at(r, i).map(str::trim).filter(|v| !v.is_empty())
    })
}

pub(crate) fn queries_sql(id: Option<u64>) -> String {
    let filter = id
        .map(|n| format!(" WHERE q.scheduled_query_id = {n}"))
        .unwrap_or_default();
    format!("SELECT {COLUMNS}\n  FROM information_schema.scheduled_queries q{filter}")
}

/// A scheduled query's name: a plain identifier, so it can go between
/// backticks and quotes as is.
pub(crate) fn ident(name: &str) -> Option<&str> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    ok.then_some(name)
}

/// The session's namespace (`hive` unless configured).
pub(crate) fn namespace(src: &mut dyn JobSource) -> String {
    src.query(NAMESPACE, &[])
        .ok()
        .and_then(|s| s.at(0, 0).map(str::to_string))
        .and_then(|v| v.split_once('=').map(|(_, v)| v.trim().to_string()))
        .filter(|v| !v.is_empty() && !v.contains("undefined"))
        .unwrap_or_else(|| "hive".into())
}

/// A view time: a timestamp in the server's zone, or epoch seconds.
fn when(tz: &Tz, v: Option<&str>) -> Option<String> {
    let t = v?.trim();
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        let secs: i64 = t.parse().ok()?;
        return (secs > 0).then(|| rfc3339(secs * 1000));
    }
    tz.utc(Some(t))
}

fn outcome(state: Option<&str>) -> JobOutcome {
    match state.map(|s| s.trim().to_ascii_uppercase()).as_deref() {
        Some("FINISHED") => JobOutcome::Succeeded,
        Some("FAILED") => JobOutcome::Failed,
        Some("TIMED_OUT") => JobOutcome::Canceled,
        Some("INITED" | "EXECUTING" | "STARTED") => JobOutcome::Running,
        _ => JobOutcome::Unknown,
    }
}

fn job(s: &Set, r: usize, tz: &Tz, ns: &str) -> Option<ServerJob> {
    let id = int(col(s, r, "id"))?;
    let name = col(s, r, "name")?.to_string();
    let enabled = yes(col(s, r, "enabled"));
    // ALTER reaches only this session's namespace and plain names.
    let here = col(s, r, "namespace").is_none_or(|n| n == ns) && ident(&name).is_some();
    Some(ServerJob {
        id: id.to_string(),
        kind: KIND.into(),
        group: owned(col(s, r, "namespace")),
        description: None,
        owner: owned(col(s, r, "owner")),
        enabled,
        schedule: owned(col(s, r, "schedule")),
        running: false,
        last_outcome: None,
        last_started: None,
        last_duration_ms: None,
        next_run: when(tz, col(s, r, "next_execution")).filter(|_| enabled),
        can_run: here && enabled,
        can_stop: false,
        can_enable: here,
        has_history: true,
        has_script: ident(&name).is_some(),
        name,
        system: false,
    })
}

/// Executions from the metastore's own table, which keeps the id of the
/// scheduled query each one belongs to (times as epoch seconds). `query`:
/// only that scheduled query's.
fn sys_executions_sql(query: Option<u64>, limit: u32) -> String {
    let filter = query
        .map(|n| format!("\n WHERE e.scheduled_query_id = {n}"))
        .unwrap_or_default();
    format!(
        "SELECT e.scheduled_execution_id AS id, e.scheduled_query_id AS sq_id, e.executor_query_id AS query_id,
       e.state AS state, e.start_time AS start_time, e.end_time AS end_time,
       e.end_time - e.start_time AS elapsed, e.error_message AS error_message
  FROM sys.scheduled_executions e{filter}
 ORDER BY id DESC LIMIT {limit}"
    )
}

/// The same from the information_schema view, for a user who can't read
/// `sys`: it names the scheduled query but has no id, and a name repeats
/// across namespaces. `name` is a checked identifier.
fn view_executions_sql(name: Option<&str>, limit: u32) -> String {
    let filter = name
        .map(|n| format!("\n WHERE e.schedule_name = '{n}'"))
        .unwrap_or_default();
    format!(
        "SELECT e.scheduled_execution_id AS id, e.schedule_name AS name, e.executor_query_id AS query_id,
       e.state AS state, e.start_time AS start_time, e.end_time AS end_time, e.elapsed AS elapsed,
       e.error_message AS error_message
  FROM information_schema.scheduled_executions e{filter}
 ORDER BY id DESC LIMIT {limit}"
    )
}

/// Executions, and whether each row has its scheduled query's id
/// (`sq_id`) or only its name (the view).
struct Execs {
    set: Set,
    by_id: bool,
}

impl Execs {
    /// The rows of scheduled query `id` named `name`.
    fn of(&self, id: &str, name: &str) -> Vec<usize> {
        (0..self.set.rows.len())
            .filter(|r| {
                if self.by_id {
                    col(&self.set, *r, "sq_id").map(str::trim) == Some(id)
                } else {
                    col(&self.set, *r, "name") == Some(name)
                }
            })
            .collect()
    }
}

/// The latest executions of every scheduled query (best effort).
fn all_executions(src: &mut dyn JobSource, limit: u32) -> Option<Execs> {
    match src.query(&sys_executions_sql(None, limit), &[]) {
        Ok(set) => Some(Execs { set, by_id: true }),
        Err(e) => {
            tracing::debug!("odbc jobs: sys.scheduled_executions failed, matching by name: {e}");
            match src.query(&view_executions_sql(None, limit), &[]) {
                Ok(set) => Some(Execs { set, by_id: false }),
                Err(e) => {
                    tracing::debug!("odbc jobs: scheduled_executions failed: {e}");
                    None
                }
            }
        }
    }
}

fn mixed(name: &str) -> Error {
    Error::Query(format!(
        "las ejecuciones de «{name}» se mezclan con las de otra consulta programada con el mismo nombre: hace falta poder leer sys.scheduled_executions"
    ))
}

/// Another scheduled query (of another namespace) has the name `name` too.
/// When that can't be read, it's taken as shared.
fn shared(src: &mut dyn JobSource, name: &str) -> bool {
    let sql = format!(
        "SELECT q.scheduled_query_id AS id, q.schedule_name AS name\n  FROM information_schema.scheduled_queries q WHERE q.schedule_name = '{name}'"
    );
    match src.query(&sql, &[]) {
        Ok(s) => {
            (0..s.rows.len())
                .filter(|r| col(&s, *r, "name") == Some(name))
                .count()
                != 1
        }
        Err(e) => {
            tracing::debug!("odbc jobs: scheduled_queries by name failed: {e}");
            true
        }
    }
}

/// An execution and the HiveServer2 query that runs it.
struct Exec {
    run: JobRun,
    executor: Option<String>,
}

/// The executions of scheduled query `n` named `name`, newest first. From
/// the view only when its name is a plain identifier no other scheduled
/// query has; an error when it's shared.
fn executions_of(
    src: &mut dyn JobSource,
    tz: &Tz,
    n: u64,
    name: &str,
    limit: u32,
) -> Result<Vec<Exec>> {
    let e = match src.query(&sys_executions_sql(Some(n), limit), &[]) {
        Ok(set) => Execs { set, by_id: true },
        Err(e) => {
            tracing::debug!("odbc jobs: sys.scheduled_executions failed, matching by name: {e}");
            let Some(plain) = ident(name) else {
                return Ok(Vec::new());
            };
            if shared(src, plain) {
                return Err(mixed(plain));
            }
            Execs {
                set: src.query(&view_executions_sql(Some(plain), limit), &[])?,
                by_id: false,
            }
        }
    };
    let mut out: Vec<Exec> = e
        .of(&n.to_string(), name)
        .into_iter()
        .map(|r| Exec {
            run: run_of(&e.set, r, tz),
            executor: col(&e.set, r, "query_id")
                .and_then(query_id)
                .map(str::to_string),
        })
        .collect();
    out.sort_by_key(|x| std::cmp::Reverse(int(x.run.id.as_deref())));
    Ok(out)
}

fn run_of(s: &Set, r: usize, tz: &Tz) -> JobRun {
    let started = when(tz, col(s, r, "start_time"));
    let duration = duration_ms(col(s, r, "start_time"), col(s, r, "end_time")).or_else(|| {
        int(col(s, r, "elapsed"))
            .and_then(|v| u64::try_from(v).ok())
            .map(|v| v * 1000)
    });
    JobRun {
        id: owned(col(s, r, "id")),
        started,
        duration_ms: duration,
        outcome: outcome(col(s, r, "state")),
        trigger: None,
        message: owned(col(s, r, "error_message")),
        steps: Vec::new(),
    }
}

/// Last finished run and running flag from the executions (newest first).
/// Stop goes through the same checks as ALTER (this namespace, a plain
/// name), which `can_enable` already says.
fn with_runs(j: &mut ServerJob, runs: &[JobRun]) {
    if let Some(last) = runs.iter().find(|r| r.outcome != JobOutcome::Running) {
        j.last_outcome = Some(last.outcome);
        j.last_started = last.started.clone();
        j.last_duration_ms = last.duration_ms;
    }
    if runs.iter().any(|r| r.outcome == JobOutcome::Running) {
        j.running = true;
        j.can_stop = j.can_enable;
        j.last_outcome.get_or_insert(JobOutcome::Running);
    }
}

const OTHER_NAMESPACES: &str =
    "Las consultas programadas de otros namespaces se ven solo para lectura.";
const ODD_NAMES: &str =
    "DBine solo maneja las consultas programadas con nombres simples: las demás se ven solo para lectura.";

pub(crate) fn not_there(e: &Error) -> bool {
    let m = e.to_string().to_ascii_lowercase();
    matches!(code(e), Some(10001 | 10072))
        || m.contains("table not found")
        || m.contains("database does not exist")
        || m.contains("scheduled_queries")
}

pub fn list(src: &mut dyn JobSource) -> Result<JobList> {
    let tz = Tz::query(src, NOW);
    let s = match src.query(&queries_sql(None), &[]) {
        Ok(s) => s,
        Err(e) if not_there(&e) => {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec![NO_SCHEDULES.into()],
            })
        }
        Err(e) => return Err(e),
    };
    let ns = namespace(src);
    let mut jobs: Vec<ServerJob> = (0..s.rows.len())
        .filter_map(|r| job(&s, r, &tz, &ns))
        .collect();
    jobs.sort_by(|a, b| (&a.group, &a.name).cmp(&(&b.group, &b.name)));
    // The latest executions of all of them in one read (best effort). By
    // name only for names no other scheduled query has.
    if !jobs.is_empty() {
        if let Some(e) = all_executions(src, 2000) {
            let names: Vec<String> = jobs.iter().map(|j| j.name.clone()).collect();
            for j in jobs.iter_mut() {
                if !e.by_id && names.iter().filter(|n| **n == j.name).count() > 1 {
                    continue;
                }
                let mine: Vec<JobRun> = e
                    .of(&j.id, &j.name)
                    .into_iter()
                    .map(|r| run_of(&e.set, r, &tz))
                    .collect();
                with_runs(j, &mine);
            }
        }
    }
    let mut notes = Vec::new();
    if jobs
        .iter()
        .any(|j| j.group.as_deref().is_some_and(|g| g != ns))
    {
        notes.push(OTHER_NAMESPACES.to_string());
    }
    if jobs.iter().any(|j| ident(&j.name).is_none()) {
        notes.push(ODD_NAMES.to_string());
    }
    Ok(JobList { jobs, notes })
}

fn one(src: &mut dyn JobSource, id: &str) -> Result<(u64, Set)> {
    let n = int_id(id)?;
    let s = src.query(&queries_sql(Some(n)), &[])?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok((n, s))
}

/// The name of job `id`, checked for statements, and whether it's in this
/// session's namespace.
fn name_here(src: &mut dyn JobSource, id: &str) -> Result<(u64, String)> {
    let (n, s) = one(src, id)?;
    let name = col(&s, 0, "name").and_then(ident).ok_or_else(|| {
        Error::Unsupported("DBine solo maneja consultas programadas con nombres simples".into())
    })?;
    let ns = namespace(src);
    if col(&s, 0, "namespace").is_some_and(|n| n != ns) {
        return Err(Error::Unsupported(format!(
            "esta consulta programada es del namespace «{}» y la sesión trabaja en «{ns}»",
            col(&s, 0, "namespace").unwrap_or_default()
        )));
    }
    Ok((n, name.to_string()))
}

pub fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let n = int_id(id)?;
    let tz = Tz::query(src, NOW);
    let (_, s) = one(src, id)?;
    let ns = namespace(src);
    let mut job = job(&s, 0, &tz, &ns).ok_or_else(|| not_found(id))?;
    match executions_of(src, &tz, n, &job.name.clone(), 50) {
        Ok(e) => {
            let runs: Vec<JobRun> = e.into_iter().map(|x| x.run).collect();
            with_runs(&mut job, &runs);
        }
        Err(e) => tracing::debug!("odbc jobs: executions of {n} failed: {e}"),
    }
    let steps = col(&s, 0, "query")
        .map(|q| JobStep {
            name: job.name.clone(),
            kind: Some("HiveQL".into()),
            target: None,
            command: Some(q.to_string()),
        })
        .into_iter()
        .collect();
    let properties = props(vec![
        prop("", "Namespace", col(&s, 0, "namespace")),
        prop("", "Ejecuta como", col(&s, 0, "owner")),
        prop("", "Próxima ejecución", col(&s, 0, "next_execution")),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

pub fn runs(src: &mut dyn JobSource, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let (n, s) = one(src, id)?;
    let name = col(&s, 0, "name").unwrap_or_default().to_string();
    let tz = Tz::query(src, NOW);
    Ok(executions_of(src, &tz, n, &name, limit)?
        .into_iter()
        .map(|x| x.run)
        .collect())
}

pub fn run(src: &mut dyn JobSource, id: &str) -> Result<()> {
    let (_, name) = name_here(src, id)?;
    src.exec(&format!("ALTER SCHEDULED QUERY `{name}` EXECUTE"), &[])
        .map(|_| ())
}

pub fn set_enabled(src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    let (_, name) = name_here(src, id)?;
    let what = if enabled { "ENABLED" } else { "DISABLED" };
    src.exec(&format!("ALTER SCHEDULED QUERY `{name}` {what}"), &[])
        .map(|_| ())
}

/// A HiveServer2 query id (`hive_20200131103008_c9a39b8d-…`).
fn query_id(v: &str) -> Option<&str> {
    let ok = !v.is_empty()
        && v.len() <= 200
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    ok.then_some(v)
}

pub fn stop(src: &mut dyn JobSource, id: &str) -> Result<()> {
    let (n, name) = name_here(src, id)?;
    // Only the states matter here, not the times.
    let running: Vec<String> = executions_of(src, &Tz::default(), n, &name, 20)?
        .into_iter()
        .filter(|x| x.run.outcome == JobOutcome::Running)
        .filter_map(|x| x.executor)
        .collect();
    if running.is_empty() {
        return Err(Error::Query(format!(
            "la consulta programada «{name}» no se está ejecutando"
        )));
    }
    for q in running {
        src.exec(&format!("KILL QUERY '{q}'"), &[])?;
    }
    Ok(())
}

/// A Hive string literal (backslash escapes, as Hive reads them).
pub(crate) fn hive_lit(v: &str) -> String {
    format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub fn script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let (_, s) = one(src, id)?;
    let name = col(&s, 0, "name")
        .and_then(ident)
        .ok_or_else(|| not_found(id))?;
    let mut out = format!("CREATE SCHEDULED QUERY `{name}`");
    if let Some(c) = col(&s, 0, "schedule") {
        out.push_str(&format!("\nCRON {}", hive_lit(c)));
    }
    if let Some(u) = col(&s, 0, "owner").and_then(ident) {
        out.push_str(&format!("\nEXECUTED AS `{u}`"));
    }
    out.push_str(if yes(col(&s, 0, "enabled")) {
        "\nENABLED"
    } else {
        "\nDISABLED"
    });
    out.push_str(&format!(
        "\nDEFINED AS {};\n",
        col(&s, 0, "query").unwrap_or_default()
    ));
    Ok(out)
}
