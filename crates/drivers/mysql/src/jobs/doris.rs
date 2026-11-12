//! Doris (and VeloDB) jobs, from the `jobs()` and `tasks()` table
//! functions (Doris 2.1+):
//!
//! - `CREATE JOB` jobs (type `insert`): an `INSERT … SELECT` once (`AT`),
//!   `EVERY` so often, or streaming. `PAUSE JOB` / `RESUME JOB` turn them
//!   off and on and `CANCEL TASK` stops a run; Doris can't run one on
//!   demand.
//! - The jobs that refresh asynchronous materialized views (type `mv`),
//!   shown as the view: `REFRESH MATERIALIZED VIEW … AUTO` runs one now (it
//!   returns once the task is queued), `PAUSE` / `RESUME MATERIALIZED VIEW
//!   JOB` and `CANCEL MATERIALIZED VIEW TASK`.
//!
//! A recurring job fires at `STARTS + k × interval` in absolute time, so
//! its next run is worked out from the strategy while the job is active.
//! Times are the session time zone's wall clock. `jobs()` doesn't say which
//! database an insert job was created in.

use super::clock::Civil;
use super::{
    bad_id, denied, duration_ms, number, one_name, outcome, prop, qualify, quoted, rows_if_present,
    session_clock, text, to_utc, DENIED_NOTE,
};
use crate::session::{lit, MySqlSession};
use dbine_driver::jobs::{JobDetail, JobList, JobOutcome, JobRun, JobStep, ServerJob};
use dbine_driver::sql::{name_tokens, TokenKind};
use dbine_driver::{Error, Result};
use std::collections::HashMap;

pub(super) const JOB: &str = "Job";
pub(super) const MV: &str = "Materialized view";

/// Tasks per job the list reads, to find the last finished one.
const RECENT: usize = 3;

/// The two job types `jobs()` and `tasks()` take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    Insert,
    Mv,
}

impl Type {
    fn arg(self) -> &'static str {
        match self {
            Type::Insert => "insert",
            Type::Mv => "mv",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Type::Insert => "job:",
            Type::Mv => "mv:",
        }
    }
}

/// A job's id: `` job:`name` `` or `` mv:`name` `` (the view's job).
fn job_id(t: Type, name: &str) -> String {
    format!("{}{}", t.prefix(), quoted(name))
}

fn parse_id(id: &str) -> Result<(Type, String)> {
    for t in [Type::Insert, Type::Mv] {
        if let Some(n) = one_name(id, t.prefix()) {
            return Ok((t, n));
        }
    }
    Err(bad_id(id, "job de Doris"))
}

fn not_found(name: &str) -> Error {
    Error::Query(format!("no existe el job {name} o este usuario no lo ve"))
}

/// How a job is scheduled, from `RecurringStrategy`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Strategy {
    At(String),
    Every {
        n: u64,
        unit: String,
        starts: Option<String>,
        ends: Option<String>,
    },
}

/// `AT 2027-01-01 03:00:00`, `EVERY 1 HOUR STARTS 2026-11-12 14:04:28
/// [ENDS …]`.
fn strategy(s: &str) -> Option<Strategy> {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let kw = |t: &str| t.eq_ignore_ascii_case("STARTS") || t.eq_ignore_ascii_case("ENDS");
    let after = |key: &str| -> Option<String> {
        let i = toks.iter().position(|t| t.eq_ignore_ascii_case(key))?;
        let v: Vec<&str> = toks[i + 1..]
            .iter()
            .take_while(|t| !kw(t))
            .copied()
            .collect();
        (!v.is_empty()).then(|| v.join(" "))
    };
    match toks.first()?.to_ascii_uppercase().as_str() {
        "AT" => (toks.len() > 1).then(|| Strategy::At(toks[1..].join(" "))),
        "EVERY" => Some(Strategy::Every {
            n: toks.get(1)?.parse().ok()?,
            unit: toks.get(2)?.to_ascii_uppercase(),
            starts: after("STARTS"),
            ends: after("ENDS"),
        }),
        _ => None,
    }
}

impl Strategy {
    /// As `CREATE JOB … ON SCHEDULE` takes it.
    fn sql(&self) -> String {
        match self {
            Strategy::At(at) => format!("AT {}", lit(at)),
            Strategy::Every {
                n,
                unit,
                starts,
                ends,
            } => {
                let mut s = format!("EVERY {n} {unit}");
                if let Some(st) = starts {
                    s.push_str(&format!(" STARTS {}", lit(st)));
                }
                if let Some(e) = ends {
                    s.push_str(&format!(" ENDS {}", lit(e)));
                }
                s
            }
        }
    }

    /// The next start after `now` (UTC), with the strategy's times read
    /// through `utc`.
    fn next(&self, now: Civil, utc: impl Fn(&str) -> Option<Civil>) -> Option<Civil> {
        match self {
            Strategy::At(at) => utc(at).filter(|t| *t > now),
            Strategy::Every {
                n,
                unit,
                starts,
                ends,
            } => {
                let step = i64::try_from(*n).ok()?.checked_mul(match unit.as_str() {
                    "SECOND" => 1,
                    "MINUTE" => 60,
                    "HOUR" => 3600,
                    "DAY" => 86_400,
                    "WEEK" => 604_800,
                    _ => return None,
                })?;
                if step <= 0 {
                    return None;
                }
                let start = utc(starts.as_deref()?)?;
                let d = now.seconds() - start.seconds();
                let next = if d < 0 {
                    start
                } else {
                    start.add_seconds((d / step + 1) * step)
                };
                match ends {
                    Some(e) => (next <= utc(e)?).then_some(next),
                    None => Some(next),
                }
            }
        }
    }
}

/// One row of `jobs()`.
#[derive(Debug, Clone, Default, PartialEq)]
struct Job {
    t: Option<Type>,
    name: String,
    definer: Option<String>,
    execute_type: String,
    strategy: Option<String>,
    status: String,
    sql: Option<String>,
    created: Option<String>,
    comment: Option<String>,
    succeeded: Option<u64>,
    failed: Option<u64>,
    canceled: Option<u64>,
    error: Option<String>,
    mv_name: Option<String>,
    mv_db: Option<String>,
}

fn job_row(t: Type, r: &mysql_async::Row) -> Job {
    Job {
        t: Some(t),
        name: text(r, &["Name"]).unwrap_or_default(),
        definer: text(r, &["Definer"]),
        execute_type: text(r, &["ExecuteType"])
            .unwrap_or_default()
            .to_ascii_uppercase(),
        strategy: text(r, &["RecurringStrategy"]),
        status: text(r, &["Status"])
            .unwrap_or_default()
            .to_ascii_uppercase(),
        sql: crate::session::named(r, &["ExecuteSql"])
            .filter(|s| !s.trim().is_empty() && s != "\\N"),
        created: text(r, &["CreateTime"]),
        comment: text(r, &["Comment"]),
        succeeded: number(r, &["SucceedTaskCount"]),
        failed: number(r, &["FailedTaskCount"]),
        canceled: number(r, &["CanceledTaskCount"]),
        error: text(r, &["ErrorMsg"]),
        mv_name: text(r, &["MvName"]),
        mv_db: text(r, &["MvDatabaseName"]),
    }
}

impl Job {
    fn ty(&self) -> Type {
        self.t.unwrap_or(Type::Insert)
    }

    /// Its schedule fires: Doris calls an active job RUNNING.
    fn active(&self) -> bool {
        self.status == "RUNNING"
    }

    fn strategy(&self) -> Option<Strategy> {
        match self.execute_type.as_str() {
            "RECURRING" | "ONE_TIME" => strategy(self.strategy.as_deref()?),
            _ => None,
        }
    }

    /// The `CREATE JOB` that recreates an insert job. `jobs()` doesn't say
    /// which database the job was created in, where its SQL's bare table
    /// names point: the script then says to run it there.
    fn create(&self) -> Option<String> {
        let schedule = self.strategy()?.sql();
        let sql = self.sql.as_deref()?.trim().trim_end_matches(';');
        let mut out = String::new();
        if names_bare_tables(sql) {
            out.push_str(BARE_TABLES_COMMENT);
            out.push('\n');
        }
        out.push_str(&format!(
            "CREATE JOB {}\nON SCHEDULE {schedule}",
            quoted(&self.name)
        ));
        if let Some(c) = &self.comment {
            out.push_str(&format!("\nCOMMENT {}", lit(c)));
        }
        out.push_str(&format!("\nDO {sql};\n"));
        Some(out)
    }
}

/// The first line of the script of a job whose SQL names tables without
/// their database.
const BARE_TABLES_COMMENT: &str =
    "-- Doris no informa en qué base se creó el job y su SQL nombra tablas sin la base: ejecutar antes USE con esa base.";

/// Whether an insert job's SQL names a table without its database (or in a
/// way this can't tell): the `INSERT INTO` / `INSERT OVERWRITE TABLE`
/// target and each table after `FROM` (a list too) or `JOIN`. Subqueries
/// and table functions (`numbers(…)`, `s3(…)`) aren't tables.
fn names_bare_tables(sql: &str) -> bool {
    let d = crate::script_dialect(crate::Variant::Doris);
    let toks = name_tokens(sql, &d);
    let name = |i: usize| toks.get(i).is_some_and(|t| t.kind == TokenKind::Name);
    let word = |i: usize, w: &str| {
        name(i) && !sql[toks[i].start..].starts_with('`') && toks[i].text.eq_ignore_ascii_case(w)
    };
    let punct = |i: usize, p: &str| {
        toks.get(i)
            .is_some_and(|t| t.kind == TokenKind::Punct && t.text == p)
    };
    for i in 0..toks.len() {
        let from = word(i, "from");
        let target = word(i, "into") || word(i, "table");
        if !(from || target || word(i, "join")) {
            continue;
        }
        let mut j = i + 1;
        // Each table of the reference (several after FROM a, b).
        while name(j) {
            let mut parts = 1;
            j += 1;
            while punct(j, ".") && name(j + 1) {
                parts += 1;
                j += 2;
            }
            // A table function; after INTO, the column list.
            if punct(j, "(") && !target {
                break;
            }
            if parts == 1 {
                return true;
            }
            if !from {
                break;
            }
            // An alias, then a comma for the next table.
            if word(j, "as") {
                j += 1;
            }
            if name(j) && punct(j + 1, ",") {
                j += 1;
            }
            if !punct(j, ",") {
                break;
            }
            j += 1;
        }
    }
    false
}

/// One row of `tasks()`, times still local.
#[derive(Debug, Clone, Default, PartialEq)]
struct Task {
    id: Option<String>,
    job: String,
    status: String,
    created: Option<String>,
    started: Option<String>,
    finished: Option<String>,
    duration_ms: Option<u64>,
    error: Option<String>,
    context: Option<String>,
}

fn task_row(r: &mysql_async::Row) -> Task {
    Task {
        id: text(r, &["TaskId"]),
        job: text(r, &["JobName"]).unwrap_or_default(),
        status: text(r, &["Status"]).unwrap_or_default(),
        created: text(r, &["CreateTime"]),
        started: text(r, &["StartTime"]),
        finished: text(r, &["FinishTime"]),
        duration_ms: number(r, &["DurationMs"]),
        error: text(r, &["ErrorMsg"]).or_else(|| text(r, &["FirstErrorMsg"])),
        context: text(r, &["TaskContext"]),
    }
}

impl Task {
    fn start(&self) -> Option<&str> {
        self.started.as_deref().or(self.created.as_deref())
    }

    fn running(&self) -> bool {
        matches!(
            self.status.to_ascii_uppercase().as_str(),
            "RUNNING" | "PENDING"
        )
    }

    fn duration(&self) -> Option<u64> {
        if self.running() {
            return None;
        }
        self.duration_ms.or_else(|| {
            duration_ms(
                self.start().and_then(Civil::parse),
                self.finished.as_deref().and_then(Civil::parse),
            )
        })
    }

    /// What started a view's refresh (`TaskContext.triggerMode`).
    fn trigger(&self) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(self.context.as_deref()?).ok()?;
        Some(
            match v
                .get("triggerMode")?
                .as_str()?
                .to_ascii_uppercase()
                .as_str()
            {
                "SYSTEM" => "programada",
                "MANUAL" => "manual",
                "COMMIT" => "cambio en las tablas base",
                _ => return None,
            }
            .into(),
        )
    }
}

/// What the list and the detail read: the jobs, their latest tasks and the
/// UTC times of what they show.
struct Catalog {
    jobs: Vec<Job>,
    tasks: Vec<Task>,
    tz: String,
    now: Option<Civil>,
    utc: HashMap<(Civil, String), Civil>,
}

impl Catalog {
    fn utc(&self, local: Option<&str>) -> Option<Civil> {
        let t = Civil::parse(local?)?;
        self.utc.get(&(t, self.tz.clone())).copied()
    }

    fn job(&self, j: &Job) -> ServerJob {
        let mut tasks: Vec<&Task> = self.tasks.iter().filter(|t| t.job == j.name).collect();
        tasks.sort_by(|a, b| b.created.cmp(&a.created));
        let running = tasks.iter().any(|t| t.running());
        let finished = tasks.iter().find(|t| !t.running());
        let strategy = j.strategy();
        let next = match (&strategy, self.now) {
            (Some(s), Some(now)) if j.active() => s.next(now, |l| self.utc(Some(l))),
            _ => None,
        };
        let switchable = matches!(j.status.as_str(), "RUNNING" | "PAUSED");
        let mv = j.ty() == Type::Mv;
        ServerJob {
            id: job_id(j.ty(), &j.name),
            name: if mv {
                j.mv_name.clone().unwrap_or_else(|| j.name.clone())
            } else {
                j.name.clone()
            },
            kind: if mv { MV } else { JOB }.into(),
            group: if mv { j.mv_db.clone() } else { None },
            description: j.comment.clone(),
            owner: j.definer.clone(),
            enabled: j.active(),
            schedule: j.strategy.clone().filter(|_| strategy.is_some()),
            running,
            last_outcome: match finished {
                Some(t) => Some(outcome(&t.status)),
                None if running => Some(JobOutcome::Running),
                None => None,
            },
            last_started: tasks
                .first()
                .and_then(|t| self.utc(t.start()))
                .map(Civil::utc),
            last_duration_ms: finished.and_then(|t| t.duration()),
            next_run: next.map(Civil::utc),
            can_run: mv && switchable,
            can_stop: running,
            can_enable: switchable,
            has_history: true,
            has_script: if mv {
                j.mv_db.is_some() && j.mv_name.is_some()
            } else {
                j.create().is_some()
            },
            system: false,
        }
    }
}

/// The jobs of a type, or `None` before `jobs()` (Doris 2.1).
async fn jobs_of(s: &mut MySqlSession, t: Type, only: Option<&str>) -> Result<Option<Vec<Job>>> {
    let filter = only
        .map(|n| format!(" WHERE Name = {}", lit(n)))
        .unwrap_or_default();
    let rows = rows_if_present(
        s,
        &format!("SELECT * FROM jobs('type'='{}'){filter}", t.arg()),
    )
    .await?;
    Ok(rows.map(|rows| {
        rows.iter()
            .map(|r| job_row(t, r))
            .filter(|j| !j.name.is_empty())
            .collect()
    }))
}

/// The latest tasks of each job (or of one), newest first.
async fn tasks_of(s: &mut MySqlSession, t: Type, only: Option<&str>, limit: usize) -> Vec<Task> {
    let sql = match only {
        Some(n) => format!(
            "SELECT * FROM tasks('type'='{}') WHERE JobName = {} ORDER BY CreateTime DESC LIMIT {limit}",
            t.arg(),
            lit(n)
        ),
        None => format!("SELECT * FROM tasks('type'='{}')", t.arg()),
    };
    let mut tasks: Vec<Task> = s.optional_rows(&sql).await.iter().map(task_row).collect();
    tasks.sort_by(|a, b| b.created.cmp(&a.created));
    if only.is_none() {
        // Keep the newest few of each job.
        let mut seen: HashMap<String, usize> = HashMap::new();
        tasks.retain(|t| {
            let n = seen.entry(t.job.clone()).or_default();
            *n += 1;
            *n <= limit
        });
    }
    tasks
}

/// The times to read in UTC: when tasks started and what strategies name.
async fn catalog(s: &mut MySqlSession, jobs: Vec<Job>, tasks: Vec<Task>) -> Result<Catalog> {
    let (tz, now) = session_clock(s).await?;
    let mut want: Vec<(Civil, String)> = Vec::new();
    let mut add = |v: Option<&str>| {
        if let Some(t) = v.and_then(Civil::parse) {
            want.push((t, tz.clone()));
        }
    };
    for t in &tasks {
        add(t.start());
    }
    for j in &jobs {
        match j.strategy() {
            Some(Strategy::At(at)) => add(Some(&at)),
            Some(Strategy::Every { starts, ends, .. }) => {
                add(starts.as_deref());
                add(ends.as_deref());
            }
            None => {}
        }
    }
    let utc = to_utc(s, &want, false).await;
    Ok(Catalog {
        jobs,
        tasks,
        tz,
        now,
        utc,
    })
}

/// What `jobs()` gave for a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    All,
    /// Refused, or (Doris 3+) left out: insert jobs need ADMIN.
    Denied,
    /// Before `jobs()` (Doris 2.1).
    Missing,
}

/// The list's notes from what each type gave.
fn notes(insert: Seen, mv: Seen) -> Vec<String> {
    let notes: &[&str] = match (insert, mv) {
        (Seen::Missing, _) | (_, Seen::Missing) => {
            &["Esta versión de Doris no tiene jobs (llegaron en Doris 2.1)."]
        }
        (Seen::Denied, Seen::Denied) => &[DENIED_NOTE],
        (Seen::All, Seen::Denied) => {
            &["Este usuario no tiene permiso para ver los jobs de las vistas materializadas."]
        }
        // Without ADMIN, the views' jobs are those of the views the user
        // may see (Doris filters them by the view's privileges).
        (Seen::Denied, Seen::All) => &[
            "Los jobs de INSERT requieren el privilegio ADMIN: no se muestran.",
            "Se ven solo los jobs de las vistas materializadas a las que este usuario tiene acceso.",
        ],
        (Seen::All, Seen::All) => &[],
    };
    notes.iter().map(|n| n.to_string()).collect()
}

pub(super) async fn list(s: &mut MySqlSession) -> Result<JobList> {
    let mut jobs = Vec::new();
    let mut tasks = Vec::new();
    let mut seen = [Seen::All; 2];
    for (i, t) in [Type::Insert, Type::Mv].into_iter().enumerate() {
        match jobs_of(s, t, None).await {
            Ok(Some(j)) => {
                // Newer versions leave insert jobs out for a user without
                // ADMIN instead of refusing.
                if t == Type::Insert && j.is_empty() && !is_admin(s).await {
                    seen[i] = Seen::Denied;
                }
                if !j.is_empty() {
                    tasks.extend(tasks_of(s, t, None, RECENT).await);
                }
                jobs.extend(j);
            }
            Ok(None) => seen[i] = Seen::Missing,
            Err(e) if denied(&e) => {
                tracing::debug!("Doris jobs('{}'): {e}", t.arg());
                seen[i] = Seen::Denied;
            }
            Err(e) => return Err(e),
        }
    }
    let c = catalog(s, jobs, tasks).await?;
    let mut list: Vec<ServerJob> = c.jobs.iter().map(|j| c.job(j)).collect();
    list.sort_by(|a, b| (&a.group, &a.name).cmp(&(&b.group, &b.name)));
    Ok(JobList {
        jobs: list,
        notes: notes(seen[0], seen[1]),
    })
}

/// Whether the user holds ADMIN: `SHOW GRANTS` lists it among the global
/// privileges, its roles' included (`Node_priv,Admin_priv`).
async fn is_admin(s: &mut MySqlSession) -> bool {
    s.optional_rows("SHOW GRANTS")
        .await
        .iter()
        .filter_map(|r| text(r, &["GlobalPrivs"]))
        .any(|p| grants_admin(&p))
}

fn grants_admin(global_privs: &str) -> bool {
    global_privs
        .split(',')
        .any(|p| p.trim().eq_ignore_ascii_case("Admin_priv"))
}

/// One job and what its row shows, or the error for an unknown one.
async fn one(s: &mut MySqlSession, id: &str) -> Result<(Job, Catalog)> {
    let (t, name) = parse_id(id)?;
    let jobs = jobs_of(s, t, Some(&name)).await?.unwrap_or_default();
    let j = jobs
        .iter()
        .find(|j| j.name == name)
        .cloned()
        .ok_or_else(|| not_found(&name))?;
    let tasks = tasks_of(s, t, Some(&name), RECENT).await;
    let c = catalog(s, vec![j.clone()], tasks).await?;
    Ok((j, c))
}

pub(super) async fn detail(s: &mut MySqlSession, id: &str) -> Result<JobDetail> {
    let (j, c) = one(s, id).await?;
    let mut properties = vec![prop("", "Tipo de ejecución", j.execute_type.clone())];
    if j.ty() == Type::Mv {
        properties.push(prop("", "Job", j.name.clone()));
    }
    for (label, v) in [
        ("Estado", Some(j.status.clone())),
        ("Creado", j.created.clone()),
    ] {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            properties.push(prop("", label, v));
        }
    }
    for (label, v) in [
        ("Tareas correctas", j.succeeded),
        ("Tareas con error", j.failed),
        ("Tareas canceladas", j.canceled),
    ] {
        if let Some(n) = v {
            properties.push(prop("Estadísticas", label, n.to_string()));
        }
    }
    if let Some(e) = &j.error {
        properties.push(prop("", "Último error", e.clone()));
    }
    let job = c.job(&j);
    Ok(JobDetail {
        steps: vec![JobStep {
            name: job.name.clone(),
            kind: Some("SQL".into()),
            target: j.mv_db.clone(),
            command: j.sql.clone(),
        }],
        job,
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

pub(super) async fn runs(s: &mut MySqlSession, id: &str, limit: usize) -> Result<Vec<JobRun>> {
    let (t, name) = parse_id(id)?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT * FROM tasks('type'='{}') WHERE JobName = {} ORDER BY CreateTime DESC LIMIT {limit}",
        t.arg(),
        lit(&name)
    );
    let tasks: Vec<Task> = s.rows(&sql).await?.iter().map(task_row).collect();
    let c = catalog(s, Vec::new(), tasks).await?;
    Ok(c.tasks.iter().map(|t| job_run(t, &c)).collect())
}

fn job_run(t: &Task, c: &Catalog) -> JobRun {
    JobRun {
        id: t.id.clone(),
        started: c.utc(t.start()).map(Civil::utc),
        duration_ms: t.duration(),
        outcome: outcome(&t.status),
        trigger: t.trigger(),
        message: t.error.clone(),
        steps: Vec::new(),
    }
}

/// The view of an `mv:` job, quoted for a statement.
fn view(j: &Job) -> Result<String> {
    match (&j.mv_db, &j.mv_name) {
        (Some(db), Some(mv)) => Ok(format!("{}.{}", quoted(db), quoted(mv))),
        _ => Err(not_found(&j.name)),
    }
}

pub(super) async fn run(s: &mut MySqlSession, id: &str) -> Result<()> {
    let (t, _) = parse_id(id)?;
    if t == Type::Insert {
        return Err(Error::Unsupported(
            "Doris no ejecuta un job a pedido: solo según su programación".into(),
        ));
    }
    let (j, _) = one(s, id).await?;
    s.rows(&format!("REFRESH MATERIALIZED VIEW {} AUTO", view(&j)?))
        .await
        .map(|_| ())
}

/// A task id from `tasks()`: digits only.
fn task_number(id: &str) -> Option<&str> {
    (!id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

pub(super) async fn stop(s: &mut MySqlSession, id: &str) -> Result<()> {
    let (t, name) = parse_id(id)?;
    let (j, c) = one(s, id).await?;
    let running: Vec<&str> = c
        .tasks
        .iter()
        .filter(|t| t.running())
        .filter_map(|t| t.id.as_deref().and_then(task_number))
        .collect();
    if running.is_empty() {
        return Err(Error::State(format!("el job {name} no se está ejecutando")));
    }
    for task in running {
        let sql = match t {
            Type::Insert => format!(
                "CANCEL TASK WHERE jobName = {} AND taskId = {task}",
                lit(&j.name)
            ),
            Type::Mv => format!("CANCEL MATERIALIZED VIEW TASK {task} ON {}", view(&j)?),
        };
        s.rows(&sql).await?;
    }
    Ok(())
}

pub(super) async fn set_enabled(s: &mut MySqlSession, id: &str, enabled: bool) -> Result<()> {
    let (t, _) = parse_id(id)?;
    let (j, _) = one(s, id).await?;
    let verb = if enabled { "RESUME" } else { "PAUSE" };
    let sql = match t {
        Type::Insert => format!("{verb} JOB WHERE jobname = {}", lit(&j.name)),
        Type::Mv => format!("{verb} MATERIALIZED VIEW JOB ON {}", view(&j)?),
    };
    s.rows(&sql).await.map(|_| ())
}

pub(super) async fn script(s: &mut MySqlSession, id: &str) -> Result<String> {
    let (t, name) = parse_id(id)?;
    let (j, _) = one(s, id).await?;
    match t {
        Type::Insert => j.create().ok_or_else(|| {
            Error::Unsupported(format!(
                "Doris no da lo necesario para recrear el job {name}"
            ))
        }),
        Type::Mv => {
            let rows = s
                .rows(&format!("SHOW CREATE MATERIALIZED VIEW {}", view(&j)?))
                .await?;
            let create = rows
                .first()
                .and_then(|r| crate::session::named(r, &["Create Materialized View"]))
                .ok_or_else(|| not_found(&name))?;
            let (Some(db), Some(mv)) = (&j.mv_db, &j.mv_name) else {
                return Err(not_found(&name));
            };
            Ok(qualify(&create, "MATERIALIZED VIEW", db, mv))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Civil {
        Civil::parse(s).unwrap()
    }

    #[test]
    fn ids_round_trip_and_foreign_ones_are_refused() {
        assert_eq!(job_id(Type::Insert, "j1"), "job:`j1`");
        assert_eq!(parse_id("job:`j1`").unwrap(), (Type::Insert, "j1".into()));
        assert_eq!(
            parse_id("mv:`inner_mtmv_1`").unwrap(),
            (Type::Mv, "inner_mtmv_1".into())
        );
        for bad in [
            "j1",
            "job:j1",
            "task:`j1`",
            "job:`j1' OR '1'='1",
            "job:`j1``",
        ] {
            assert!(parse_id(bad).is_err(), "{bad}");
        }
        assert_eq!(task_number("297912210799585"), Some("297912210799585"));
        assert_eq!(task_number("1 OR 1=1"), None);
        assert_eq!(task_number(""), None);
    }

    #[test]
    fn strategies_parse_and_rebuild() {
        let every = strategy("EVERY 1 MINUTE STARTS 2026-11-12 13:03:58").unwrap();
        assert_eq!(
            every,
            Strategy::Every {
                n: 1,
                unit: "MINUTE".into(),
                starts: Some("2026-11-12 13:03:58".into()),
                ends: None
            }
        );
        assert_eq!(every.sql(), "EVERY 1 MINUTE STARTS '2026-11-12 13:03:58'");
        let both =
            strategy("EVERY 2 HOUR STARTS 2026-11-12 14:00:00 ENDS 2027-01-01 00:00:00").unwrap();
        assert_eq!(
            both.sql(),
            "EVERY 2 HOUR STARTS '2026-11-12 14:00:00' ENDS '2027-01-01 00:00:00'"
        );
        assert_eq!(
            strategy("AT 2027-01-01 03:00:00").unwrap().sql(),
            "AT '2027-01-01 03:00:00'"
        );
        assert_eq!(strategy("\\N"), None);
        assert_eq!(strategy("EVERY x MINUTE"), None);
    }

    #[test]
    fn next_runs_step_from_starts_in_absolute_time() {
        let same = |l: &str| Civil::parse(l);
        let every = strategy("EVERY 1 MINUTE STARTS 2026-11-12 13:03:58").unwrap();
        assert_eq!(
            every.next(c("2026-11-12 13:10:00"), same),
            Some(c("2026-11-12 13:10:58"))
        );
        assert_eq!(
            every.next(c("2026-11-12 13:10:58"), same),
            Some(c("2026-11-12 13:11:58"))
        );
        assert_eq!(
            every.next(c("2026-11-12 13:00:00"), same),
            Some(c("2026-11-12 13:03:58"))
        );
        let ending =
            strategy("EVERY 1 HOUR STARTS 2026-11-12 00:00:00 ENDS 2026-11-12 05:00:00").unwrap();
        assert_eq!(ending.next(c("2026-11-12 06:00:00"), same), None);
        assert_eq!(
            ending.next(c("2026-11-12 04:30:00"), same),
            Some(c("2026-11-12 05:00:00"))
        );
        let at = strategy("AT 2027-01-01 03:00:00").unwrap();
        assert_eq!(
            at.next(c("2026-11-12 00:00:00"), same),
            Some(c("2027-01-01 03:00:00"))
        );
        assert_eq!(at.next(c("2027-01-02 00:00:00"), same), None);
        let month = strategy("EVERY 1 MONTH STARTS 2026-01-01 00:00:00").unwrap();
        assert_eq!(month.next(c("2026-11-12 00:00:00"), same), None);
    }

    fn j1() -> Job {
        Job {
            t: Some(Type::Insert),
            name: "j1".into(),
            definer: Some("root".into()),
            execute_type: "RECURRING".into(),
            strategy: Some("EVERY 1 MINUTE STARTS 2026-11-12 13:03:58".into()),
            status: "RUNNING".into(),
            sql: Some("INSERT INTO db.t SELECT 1, now()".into()),
            comment: Some("cada minuto".into()),
            ..Default::default()
        }
    }

    fn task(job: &str, status: &str, created: &str, finished: Option<&str>) -> Task {
        Task {
            id: Some("1".into()),
            job: job.into(),
            status: status.into(),
            created: Some(created.into()),
            started: Some(created.into()),
            finished: finished.map(Into::into),
            ..Default::default()
        }
    }

    fn catalog_of(jobs: Vec<Job>, tasks: Vec<Task>, now: &str) -> Catalog {
        let mut utc = HashMap::new();
        for l in [
            "2026-11-12 13:03:58",
            "2026-11-12 13:10:00",
            "2026-11-12 13:09:00",
        ] {
            utc.insert((c(l), "UTC".to_string()), c(l));
        }
        Catalog {
            jobs,
            tasks,
            tz: "UTC".into(),
            now: Some(c(now)),
            utc,
        }
    }

    #[test]
    fn rows_become_jobs() {
        let cat = catalog_of(
            vec![j1()],
            vec![
                task("j1", "RUNNING", "2026-11-12 13:10:00", None),
                task(
                    "j1",
                    "FAILED",
                    "2026-11-12 13:09:00",
                    Some("2026-11-12 13:09:03"),
                ),
            ],
            "2026-11-12 13:10:30",
        );
        let j = cat.job(&cat.jobs[0]);
        assert_eq!(j.id, "job:`j1`");
        assert_eq!(j.kind, "Job");
        assert!(j.enabled && j.running && j.can_stop && j.can_enable && j.has_history);
        assert!(!j.can_run && j.has_script);
        assert_eq!(
            j.schedule.as_deref(),
            Some("EVERY 1 MINUTE STARTS 2026-11-12 13:03:58")
        );
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T13:10:00Z"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.last_duration_ms, Some(3000));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-12T13:10:58Z"));

        let mut paused = j1();
        paused.status = "PAUSED".into();
        let j = cat.job(&paused);
        assert!(!j.enabled && j.can_enable && j.next_run.is_none());

        let mv = Job {
            t: Some(Type::Mv),
            name: "inner_mtmv_1".into(),
            execute_type: "RECURRING".into(),
            strategy: Some("EVERY 1 HOUR STARTS 2026-11-12 14:04:28".into()),
            status: "RUNNING".into(),
            mv_name: Some("mv1".into()),
            mv_db: Some("db".into()),
            ..Default::default()
        };
        let j = cat.job(&mv);
        assert_eq!(j.id, "mv:`inner_mtmv_1`");
        assert_eq!((j.name.as_str(), j.group.as_deref()), ("mv1", Some("db")));
        assert_eq!(j.kind, "Materialized view");
        assert!(j.can_run && j.can_enable && !j.can_stop && j.has_script);
        // Its STARTS wasn't converted: no guess.
        assert!(j.next_run.is_none());

        let finished = Job {
            execute_type: "ONE_TIME".into(),
            strategy: Some("AT 2026-01-01 00:00:00".into()),
            status: "FINISHED".into(),
            ..j1()
        };
        let j = cat.job(&finished);
        assert!(!j.enabled && !j.can_enable && !j.can_run);
    }

    #[test]
    fn insert_jobs_script_as_create_job() {
        assert_eq!(
            j1().create().unwrap(),
            "CREATE JOB `j1`\nON SCHEDULE EVERY 1 MINUTE STARTS '2026-11-12 13:03:58'\nCOMMENT 'cada minuto'\nDO INSERT INTO db.t SELECT 1, now();\n"
        );
        let streaming = Job {
            execute_type: "STREAMING".into(),
            strategy: None,
            ..j1()
        };
        assert!(streaming.create().is_none());
        // Bare table names: the script says where to run it.
        let bare = Job {
            sql: Some("INSERT INTO t SELECT 1, now()".into()),
            comment: None,
            ..j1()
        };
        assert_eq!(
            bare.create().unwrap(),
            format!("{BARE_TABLES_COMMENT}\nCREATE JOB `j1`\nON SCHEDULE EVERY 1 MINUTE STARTS '2026-11-12 13:03:58'\nDO INSERT INTO t SELECT 1, now();\n")
        );
    }

    #[test]
    fn bare_table_names_are_found() {
        for bare in [
            "INSERT INTO t SELECT 1",
            "INSERT INTO t (a, b) SELECT 1, 2",
            "INSERT INTO `t` SELECT 1",
            "INSERT OVERWRITE TABLE t PARTITION (p1) SELECT 1",
            "INSERT INTO db.t SELECT * FROM src",
            "INSERT INTO db.t SELECT * FROM db.a JOIN b ON a.id = b.id",
            "INSERT INTO db.t SELECT * FROM db.a, b",
            "INSERT INTO db.t SELECT * FROM db.a x, db.b y, c",
            "INSERT INTO db.t SELECT * FROM db.a AS x, c",
        ] {
            assert!(names_bare_tables(bare), "{bare}");
        }
        for qualified in [
            "INSERT INTO db.t SELECT 1, now()",
            "INSERT INTO `d b`.`t` (a) SELECT a FROM db.src WHERE a > 0",
            "INSERT INTO internal.db.t SELECT * FROM db.a JOIN db.b ON a.id = b.id",
            "INSERT INTO db.t SELECT * FROM db.a x, db.b y WHERE x.id = y.id",
            "INSERT INTO db.t SELECT * FROM (SELECT 1) s",
            "INSERT INTO db.t SELECT number FROM numbers('number' = '10')",
            "INSERT INTO db.t SELECT 'FROM t', `from` FROM db.a",
            "INSERT INTO db.t SELECT 1 -- FROM t\n",
        ] {
            assert!(!names_bare_tables(qualified), "{qualified}");
        }
    }

    #[test]
    fn admin_is_read_from_the_global_privileges() {
        assert!(grants_admin("Node_priv,Admin_priv"));
        assert!(grants_admin("Admin_priv"));
        assert!(!grants_admin("Node_priv"));
        assert!(!grants_admin("NULL"));
    }

    #[test]
    fn notes_say_what_the_user_does_not_see() {
        assert!(notes(Seen::All, Seen::All).is_empty());
        let n = notes(Seen::Denied, Seen::All);
        assert_eq!(n.len(), 2, "{n:?}");
        assert!(n[0].contains("ADMIN") && n[1].contains("vistas materializadas"));
        assert_eq!(notes(Seen::Denied, Seen::Denied), vec![DENIED_NOTE]);
        assert!(notes(Seen::Missing, Seen::Missing)[0].contains("Doris 2.1"));
        assert!(notes(Seen::All, Seen::Denied)[0].contains("vistas materializadas"));
    }

    #[test]
    fn tasks_read_their_trigger_and_duration() {
        let mut t = task(
            "mv",
            "SUCCESS",
            "2026-11-12 13:04:28",
            Some("2026-11-12 13:04:29"),
        );
        assert_eq!(t.duration(), Some(1000));
        t.duration_ms = Some(397);
        assert_eq!(t.duration(), Some(397));
        t.context = Some(r#"{"triggerMode":"SYSTEM","isComplete":true}"#.into());
        assert_eq!(t.trigger().as_deref(), Some("programada"));
        t.context = Some(r#"{"triggerMode":"MANUAL"}"#.into());
        assert_eq!(t.trigger().as_deref(), Some("manual"));
        let r = job_run(
            &t,
            &catalog_of(Vec::new(), Vec::new(), "2026-11-12 14:00:00"),
        );
        assert_eq!(r.outcome, JobOutcome::Succeeded);
        assert_eq!(r.duration_ms, Some(397));
        let running = task("j1", "RUNNING", "2026-11-12 13:10:00", None);
        assert!(running.running() && running.duration().is_none());
    }
}
