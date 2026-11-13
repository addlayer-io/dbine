//! StarRocks tasks, from `information_schema.tasks` and their runs from
//! `information_schema.task_runs` (kept a few days).
//!
//! Two kinds share the catalog:
//!
//! - Tasks made with `SUBMIT TASK`, run once or on a `SCHEDULE … EVERY`.
//!   StarRocks has no statement to run one again (checked up to 4.1.4:
//!   `ALTER TASK` takes only `SUSPEND`, `RESUME` and `SET`), and `ALTER TASK
//!   … SUSPEND | RESUME` exists but the catalog doesn't say which tasks are
//!   suspended: no enable switch here (the scheduler designer offers both
//!   as actions). A pipe's tasks are the server's (`system`).
//! - The refresh task of each asynchronous materialized view (`mv-<id>`),
//!   shown as the view: `REFRESH MATERIALIZED VIEW … WITH ASYNC MODE` runs
//!   it now, `CANCEL REFRESH MATERIALIZED VIEW` stops the refresh going on.
//!
//! Times are the session time zone's wall clock.

use super::clock::Civil;
use super::{
    bad_id, denied, duration_ms, one_name, outcome, prop, qualify, quoted, rows_if_present,
    session_clock, text, to_utc, DENIED_NOTE,
};
use crate::session::{lit, MySqlSession};
use dbine_driver::jobs::{JobDetail, JobList, JobOutcome, JobRun, JobStep, ServerJob};
use dbine_driver::{Error, Result};
use std::collections::HashMap;

pub(crate) const TASK: &str = "Task";
pub(crate) const MV: &str = "Materialized view";

/// Runs per task the list reads, to find the last finished one.
const RECENT: usize = 3;

/// The note for a server from before `information_schema.tasks`.
const NO_TASKS_NOTE: &str = "Esta versión de StarRocks no tiene tareas.";

/// The note for a user who doesn't see every database's tasks.
pub(crate) const PARTIAL_NOTE: &str =
    "Se ven solo las tareas de las bases en las que este usuario tiene privilegios.";

/// The built-in roles that hold privileges on every database.
const ALL_DATABASES_ROLES: [&str; 2] = ["root", "db_admin"];

const TASKS: &str = "SELECT * FROM information_schema.tasks{filter} ORDER BY TASK_NAME";
pub(crate) const VIEWS: &str = "SELECT * FROM information_schema.materialized_views";
const RECENT_RUNS: &str = "SELECT * FROM (SELECT r.*, ROW_NUMBER() OVER (PARTITION BY r.TASK_NAME ORDER BY r.CREATE_TIME DESC) AS dbine_rn FROM information_schema.task_runs r) x WHERE dbine_rn <= {RECENT}";

/// A task's id: `` task:`name` ``, or `` mv:`name` `` for a materialized
/// view's refresh task.
pub(crate) fn task_id(name: &str, mv: bool) -> String {
    format!("{}:{}", if mv { "mv" } else { "task" }, quoted(name))
}

/// The task name in an id and whether it's a view's.
pub(crate) fn parse_id(id: &str) -> Result<(String, bool)> {
    if let Some(n) = one_name(id, "task:") {
        return Ok((n, false));
    }
    one_name(id, "mv:")
        .map(|n| (n, true))
        .ok_or_else(|| bad_id(id, "tarea de StarRocks"))
}

pub(crate) fn not_found(name: &str) -> Error {
    Error::Query(format!("no existe la tarea {name} o este usuario no la ve"))
}

/// A task a pipe runs (`CREATE PIPE`): StarRocks names them
/// `pipe-<pipe>-task-<id>-<n>`, makes them and drops them with the pipe's
/// loads. They're the server's, not the user's.
pub(crate) fn pipe_task(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("pipe-") else {
        return false;
    };
    let mut parts = rest.rsplitn(3, '-');
    let (Some(n), Some(id), Some(head)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(n)
        && digits(id)
        && head
            .strip_suffix("-task")
            .is_some_and(|pipe| !pipe.is_empty())
}

/// One row of `information_schema.tasks`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Task {
    pub(crate) name: String,
    pub(crate) created: Option<String>,
    /// `PERIODICAL START(…) EVERY(1 HOURS)`, `MANUAL`, `EVENT_TRIGGERED`.
    pub(crate) schedule: Option<String>,
    pub(crate) catalog: Option<String>,
    pub(crate) database: Option<String>,
    pub(crate) definition: Option<String>,
    pub(crate) expire: Option<String>,
    pub(crate) properties: Option<String>,
    pub(crate) creator: Option<String>,
}

fn task(r: &mysql_async::Row) -> Task {
    Task {
        name: text(r, &["TASK_NAME"]).unwrap_or_default(),
        created: text(r, &["CREATE_TIME"]),
        schedule: text(r, &["SCHEDULE"]),
        catalog: text(r, &["CATALOG"]),
        database: text(r, &["DATABASE"]),
        definition: crate::session::named(r, &["DEFINITION"]).filter(|d| !d.trim().is_empty()),
        expire: text(r, &["EXPIRE_TIME"]),
        properties: text(r, &["PROPERTIES"]),
        creator: text(r, &["CREATOR"]),
    }
}

/// The view a refresh task belongs to.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct View {
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) active: bool,
    pub(crate) refresh_type: Option<String>,
    pub(crate) inactive_reason: Option<String>,
}

pub(crate) fn views(rows: &[mysql_async::Row]) -> HashMap<String, View> {
    rows.iter()
        .filter_map(|r| {
            Some((
                text(r, &["TASK_NAME"])?,
                View {
                    schema: text(r, &["TABLE_SCHEMA"])?,
                    name: text(r, &["TABLE_NAME"])?,
                    active: text(r, &["IS_ACTIVE"])
                        .is_none_or(|a| matches!(a.to_ascii_lowercase().as_str(), "true" | "1")),
                    refresh_type: text(r, &["REFRESH_TYPE"]),
                    inactive_reason: text(r, &["INACTIVE_REASON"]),
                },
            ))
        })
        .collect()
}

/// One row of `information_schema.task_runs`, times still local.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Run {
    task: String,
    query_id: Option<String>,
    state: String,
    created: Option<String>,
    processed: Option<String>,
    finished: Option<String>,
    error: Option<String>,
    extra: Option<String>,
}

fn run_row(r: &mysql_async::Row) -> Run {
    Run {
        task: text(r, &["TASK_NAME"]).unwrap_or_default(),
        query_id: text(r, &["QUERY_ID"]),
        state: text(r, &["STATE"]).unwrap_or_default(),
        created: text(r, &["CREATE_TIME"]),
        processed: text(r, &["PROCESS_TIME"]),
        finished: text(r, &["FINISH_TIME"]),
        error: text(r, &["ERROR_MESSAGE"]).filter(|e| !e.eq_ignore_ascii_case("NULL")),
        extra: text(r, &["EXTRA_MESSAGE"]),
    }
}

impl Run {
    fn started(&self) -> Option<&str> {
        self.processed.as_deref().or(self.created.as_deref())
    }

    fn duration_ms(&self) -> Option<u64> {
        if outcome(&self.state) == JobOutcome::Running {
            return None;
        }
        duration_ms(
            self.started().and_then(Civil::parse),
            self.finished.as_deref().and_then(Civil::parse),
        )
    }

    /// What started a view's refresh, from its `EXTRA_MESSAGE`.
    fn trigger(&self) -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(self.extra.as_deref()?).ok()?;
        let manual = v
            .get("executeOption")
            .and_then(|o| o.get("isManual"))
            .or_else(|| v.get("isManual"))?
            .as_bool()?;
        Some(if manual { "manual" } else { "programada" }.into())
    }
}

impl Task {
    /// What follows `PERIODICAL`: `START(2026-11-12T12:58:19) EVERY(1 MINUTES)`.
    pub(crate) fn periodical(&self) -> Option<&str> {
        let s = self.schedule.as_deref()?.trim();
        s.strip_prefix("PERIODICAL")
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// The start and step of a periodical schedule: `2026-11-12 12:58:19`,
    /// `(1, "MINUTE")`.
    pub(crate) fn start_every(&self) -> (Option<String>, Option<(u64, String)>) {
        let Some(p) = self.periodical() else {
            return (None, None);
        };
        let inside = |key: &str| {
            let i = p.find(key)? + key.len();
            let rest = p[i..].strip_prefix('(')?;
            rest.split_once(')').map(|(v, _)| v.trim().to_string())
        };
        let start = inside("START")
            .and_then(|s| Civil::parse(&s))
            .map(Civil::sql);
        let every = inside("EVERY").and_then(|e| {
            let e = e.trim_start_matches("INTERVAL").trim();
            let (n, unit) = e.split_once(char::is_whitespace)?;
            let unit = unit.trim().to_ascii_uppercase();
            let unit = unit.strip_suffix('S').unwrap_or(&unit).to_string();
            Some((n.trim().parse().ok()?, unit))
        });
        (start, every)
    }

    /// The `SUBMIT TASK` that recreates it.
    fn submit(&self) -> String {
        let name = match &self.database {
            Some(db) => format!("{}.{}", quoted(db), quoted(&self.name)),
            None => quoted(&self.name),
        };
        let mut out = format!("SUBMIT TASK {name}");
        if let (start, Some((n, unit))) = self.start_every() {
            out.push_str("\nSCHEDULE ");
            if let Some(st) = start {
                out.push_str(&format!("START({}) ", lit(&st)));
            }
            out.push_str(&format!("EVERY(INTERVAL {n} {unit})"));
        }
        // The warehouse isn't a SUBMIT TASK property.
        let props: Vec<(String, String)> = self
            .properties
            .as_deref()
            .map(parse_properties)
            .unwrap_or_default()
            .into_iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("warehouse"))
            .collect();
        if !props.is_empty() {
            let list: Vec<String> = props
                .iter()
                .map(|(k, v)| format!("{} = {}", lit(k), lit(v)))
                .collect();
            out.push_str(&format!("\nPROPERTIES ({})", list.join(", ")));
        }
        let def = self.definition.as_deref().unwrap_or("").trim();
        // No `;` after it: StarRocks keeps everything after AS, up to the
        // end of what it was sent, as the task's definition.
        out.push_str(&format!("\nAS {}", def.trim_end_matches(';').trim_end()));
        out
    }
}

/// `('k1'='v1','k2'='v2')` as pairs.
pub(crate) fn parse_properties(s: &str) -> Vec<(String, String)> {
    let s = s.trim();
    let s = s
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(s);
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    let read = |chars: &mut std::iter::Peekable<std::str::Chars>| -> Option<String> {
        while chars.peek().is_some_and(|c| c.is_whitespace() || *c == ',') {
            chars.next();
        }
        let q = chars.next().filter(|c| *c == '\'' || *c == '"')?;
        let mut v = String::new();
        loop {
            match chars.next()? {
                c if c == q && chars.peek() == Some(&q) => {
                    v.push(q);
                    chars.next();
                }
                c if c == q => return Some(v),
                '\\' => v.push(chars.next()?),
                c => v.push(c),
            }
        }
    };
    while let Some(k) = read(&mut chars) {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.next() != Some('=') {
            break;
        }
        match read(&mut chars) {
            Some(v) => out.push((k, v)),
            None => break,
        }
    }
    out
}

/// What the list shows of the last runs (newest first).
fn summary(runs: &[&Run], utc: &HashMap<(Civil, String), Civil>, tz: &str) -> Summary {
    let at = |v: Option<&str>| {
        v.and_then(Civil::parse)
            .and_then(|t| utc.get(&(t, tz.to_string())))
            .map(|t| t.utc())
    };
    // Queued (PENDING) counts: it's under way and CANCEL REFRESH stops it.
    let running = runs
        .iter()
        .any(|r| outcome(&r.state) == JobOutcome::Running);
    let finished = runs
        .iter()
        .find(|r| outcome(&r.state) != JobOutcome::Running);
    Summary {
        running,
        last_started: runs.first().and_then(|r| at(r.started())),
        last_outcome: match finished {
            Some(r) => Some(outcome(&r.state)),
            None if !runs.is_empty() => Some(JobOutcome::Running),
            None => None,
        },
        last_duration_ms: finished.and_then(|r| r.duration_ms()),
    }
}

#[derive(Debug, Default, PartialEq)]
struct Summary {
    running: bool,
    last_started: Option<String>,
    last_outcome: Option<JobOutcome>,
    last_duration_ms: Option<u64>,
}

fn job(t: &Task, view: Option<&View>, s: Summary) -> ServerJob {
    let schedule = t.periodical().map(str::to_string);
    match view {
        Some(v) => ServerJob {
            id: task_id(&t.name, true),
            name: v.name.clone(),
            kind: MV.into(),
            group: Some(v.schema.clone()),
            description: None,
            owner: t.creator.clone(),
            enabled: v.active,
            schedule,
            can_run: v.active,
            can_stop: s.running,
            can_enable: false,
            has_history: true,
            has_script: true,
            running: s.running,
            last_outcome: s.last_outcome,
            last_started: s.last_started,
            last_duration_ms: s.last_duration_ms,
            next_run: None,
            system: false,
        },
        None => ServerJob {
            id: task_id(&t.name, false),
            name: t.name.clone(),
            kind: TASK.into(),
            group: t.database.clone(),
            description: None,
            owner: t.creator.clone(),
            enabled: schedule.is_some(),
            schedule,
            can_run: false,
            can_stop: false,
            can_enable: false,
            has_history: true,
            has_script: true,
            running: s.running,
            last_outcome: s.last_outcome,
            last_started: s.last_started,
            last_duration_ms: s.last_duration_ms,
            next_run: None,
            system: pipe_task(&t.name),
        },
    }
}

/// Everything the list and the detail read.
pub(crate) struct Catalog {
    pub(crate) tasks: Vec<Task>,
    pub(crate) views: HashMap<String, View>,
    pub(crate) runs: Vec<Run>,
    pub(crate) tz: String,
    pub(crate) utc: HashMap<(Civil, String), Civil>,
}

/// The catalog, or the note for a server that has none to show (no
/// `information_schema.tasks`, or, listing all, the user may not read it).
pub(crate) async fn catalog(
    s: &mut MySqlSession,
    only: Option<&str>,
) -> Result<std::result::Result<Catalog, &'static str>> {
    let filter = only
        .map(|n| format!(" WHERE TASK_NAME = {}", lit(n)))
        .unwrap_or_default();
    let tasks = match rows_if_present(s, &TASKS.replace("{filter}", &filter)).await {
        Ok(Some(rows)) => rows
            .iter()
            .map(task)
            .filter(|t| !t.name.is_empty())
            .collect(),
        // Before information_schema.tasks.
        Ok(None) => return Ok(Err(NO_TASKS_NOTE)),
        // The server refused the catalog: no tasks it can show.
        Err(e) if only.is_none() && denied(&e) => {
            tracing::debug!("StarRocks tasks: {e}");
            return Ok(Err(DENIED_NOTE));
        }
        Err(e) => return Err(e),
    };
    let views = views(&s.optional_rows(VIEWS).await);
    let runs_sql = match only {
        Some(n) => format!(
            "SELECT * FROM information_schema.task_runs WHERE TASK_NAME = {} ORDER BY CREATE_TIME DESC LIMIT {RECENT}",
            lit(n)
        ),
        None => RECENT_RUNS.replace("{RECENT}", &RECENT.to_string()),
    };
    let runs: Vec<Run> = s
        .optional_rows(&runs_sql)
        .await
        .iter()
        .map(run_row)
        .collect();
    let (tz, _) = session_clock(s).await?;
    let want: Vec<(Civil, String)> = runs
        .iter()
        .filter_map(|r| r.started().and_then(Civil::parse))
        .map(|t| (t, tz.clone()))
        .collect();
    let utc = to_utc(s, &want, false).await;
    Ok(Ok(Catalog {
        tasks,
        views,
        runs,
        tz,
        utc,
    }))
}

impl Catalog {
    fn job(&self, t: &Task) -> ServerJob {
        let mut runs: Vec<&Run> = self.runs.iter().filter(|r| r.task == t.name).collect();
        runs.sort_by(|a, b| b.created.cmp(&a.created));
        job(
            t,
            self.views.get(&t.name),
            summary(&runs, &self.utc, &self.tz),
        )
    }
}

pub(super) async fn list(s: &mut MySqlSession) -> Result<JobList> {
    let c = match catalog(s, None).await? {
        Ok(c) => c,
        Err(note) => {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec![note.into()],
            })
        }
    };
    let jobs: Vec<ServerJob> = c.tasks.iter().map(|t| c.job(t)).collect();
    let mut notes = Vec::new();
    if !sees_all_databases(s).await {
        notes.push(PARTIAL_NOTE.into());
    }
    if jobs.iter().any(|j| j.kind == TASK && j.schedule.is_some()) {
        notes.push(
            "StarRocks no informa si una tarea periódica está suspendida (ALTER TASK … SUSPEND): se muestran como habilitadas."
                .into(),
        );
    }
    Ok(JobList { jobs, notes })
}

/// Whether the user sees every database's tasks and views: StarRocks leaves
/// out those of databases the user has no privilege on or in.
pub(crate) async fn sees_all_databases(s: &mut MySqlSession) -> bool {
    let rows = s
        .optional_rows("SELECT CURRENT_USER() AS sr_user, CURRENT_ROLE() AS sr_roles")
        .await;
    let user = rows.first().and_then(|r| text(r, &["sr_user"]));
    let roles = active_roles(
        rows.first()
            .and_then(|r| text(r, &["sr_roles"]))
            .as_deref()
            .unwrap_or(""),
    );
    if user.as_deref().is_some_and(is_root)
        || roles
            .iter()
            .any(|r| ALL_DATABASES_ROLES.contains(&r.as_str()))
    {
        return true;
    }
    let mut grants: Vec<String> = s
        .optional_rows("SHOW GRANTS")
        .await
        .iter()
        .filter_map(|r| text(r, &["Grants"]))
        .collect();
    for role in &roles {
        grants.extend(
            s.optional_rows(&format!("SHOW GRANTS FOR ROLE {}", quoted(role)))
                .await
                .iter()
                .filter_map(|r| text(r, &["Grants"])),
        );
    }
    grants_every_database(&grants)
}

/// `CURRENT_USER()` (`'root'@'%'`) names StarRocks' root user.
fn is_root(user: &str) -> bool {
    user.split('@')
        .next()
        .is_some_and(|u| u.trim().trim_matches(|c| c == '\'' || c == '`') == "root")
}

/// The roles in `CURRENT_ROLE()` (`root`, `public, db_admin`, `NONE`).
fn active_roles(s: &str) -> Vec<String> {
    s.split(',')
        .map(|r| r.trim().trim_matches(|c| c == '\'' || c == '`').to_string())
        .filter(|r| !r.is_empty() && !r.eq_ignore_ascii_case("NONE"))
        .collect()
}

/// A grant on every database (`… ON ALL DATABASES`, `… ON ALL TABLES IN ALL
/// DATABASES`) among `SHOW GRANTS` rows.
fn grants_every_database(grants: &[String]) -> bool {
    grants
        .iter()
        .any(|g| g.to_ascii_uppercase().contains("ALL DATABASES"))
}

/// One task and its view, or the error for an unknown one.
pub(crate) async fn one(s: &mut MySqlSession, id: &str) -> Result<(Task, Option<View>, Catalog)> {
    let (name, mv) = parse_id(id)?;
    let c = catalog(s, Some(&name))
        .await?
        .map_err(|_| not_found(&name))?;
    let t = c
        .tasks
        .iter()
        .find(|t| t.name == name)
        .cloned()
        .ok_or_else(|| not_found(&name))?;
    let view = c.views.get(&name).cloned();
    if view.is_some() != mv {
        return Err(bad_id(id, "tarea de StarRocks"));
    }
    Ok((t, view, c))
}

pub(super) async fn detail(s: &mut MySqlSession, id: &str) -> Result<JobDetail> {
    let (t, view, c) = one(s, id).await?;
    let mut properties = Vec::new();
    if let Some(v) = &view {
        properties.push(prop("", "Tarea", t.name.clone()));
        if let Some(r) = &v.refresh_type {
            properties.push(prop("", "Refresco", r.clone()));
        }
        if !v.active {
            properties.push(prop(
                "",
                "Estado",
                match &v.inactive_reason {
                    Some(r) => format!("Inactiva: {r}"),
                    None => "Inactiva".into(),
                },
            ));
        }
    }
    for (label, v) in [
        ("Catálogo", &t.catalog),
        ("Creada", &t.created),
        ("Vence", &t.expire),
    ] {
        if let Some(v) = v {
            properties.push(prop("", label, v.clone()));
        }
    }
    if let Some(p) = &t.properties {
        for (k, v) in parse_properties(p) {
            properties.push(prop("Propiedades", &k, v));
        }
    }
    Ok(JobDetail {
        job: c.job(&t),
        steps: vec![JobStep {
            name: view.as_ref().map_or(t.name.clone(), |v| v.name.clone()),
            kind: Some("SQL".into()),
            target: t.database.clone(),
            command: t.definition.clone(),
        }],
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

pub(super) async fn runs(s: &mut MySqlSession, id: &str, limit: usize) -> Result<Vec<JobRun>> {
    let (name, _) = parse_id(id)?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let rows = s
        .rows(&format!(
            "SELECT * FROM information_schema.task_runs WHERE TASK_NAME = {} ORDER BY CREATE_TIME DESC LIMIT {limit}",
            lit(&name)
        ))
        .await?;
    let runs: Vec<Run> = rows.iter().map(run_row).collect();
    let (tz, _) = session_clock(s).await?;
    let want: Vec<(Civil, String)> = runs
        .iter()
        .filter_map(|r| r.started().and_then(Civil::parse))
        .map(|t| (t, tz.clone()))
        .collect();
    let utc = to_utc(s, &want, false).await;
    Ok(runs.iter().map(|r| job_run(r, &utc, &tz)).collect())
}

fn job_run(r: &Run, utc: &HashMap<(Civil, String), Civil>, tz: &str) -> JobRun {
    JobRun {
        id: r.query_id.clone(),
        started: r
            .started()
            .and_then(Civil::parse)
            .and_then(|t| utc.get(&(t, tz.to_string())))
            .map(|t| t.utc()),
        duration_ms: r.duration_ms(),
        outcome: outcome(&r.state),
        trigger: r.trigger(),
        message: r.error.clone(),
        steps: Vec::new(),
    }
}

/// The view of a refresh task, or why it can't be acted on (`refused`: the
/// whole message, so the UI translates it as one).
async fn view_of(s: &mut MySqlSession, id: &str, refused: &'static str) -> Result<View> {
    let (name, mv) = parse_id(id)?;
    if !mv {
        return Err(Error::Unsupported(refused.into()));
    }
    let (_, view, _) = one(s, id).await?;
    view.ok_or_else(|| not_found(&name))
}

pub(super) async fn run(s: &mut MySqlSession, id: &str) -> Result<()> {
    let v = view_of(s, id, "StarRocks no permite ejecutar una tarea desde SQL").await?;
    s.rows(&format!(
        "REFRESH MATERIALIZED VIEW {}.{} WITH ASYNC MODE",
        quoted(&v.schema),
        quoted(&v.name)
    ))
    .await
    .map(|_| ())
}

pub(super) async fn stop(s: &mut MySqlSession, id: &str) -> Result<()> {
    let v = view_of(s, id, "StarRocks no permite detener una tarea desde SQL").await?;
    s.rows(&format!(
        "CANCEL REFRESH MATERIALIZED VIEW {}.{}",
        quoted(&v.schema),
        quoted(&v.name)
    ))
    .await
    .map(|_| ())
}

pub(super) async fn set_enabled(_s: &mut MySqlSession, id: &str, _enabled: bool) -> Result<()> {
    parse_id(id)?;
    Err(Error::Unsupported(
        "StarRocks no informa si una tarea está suspendida: DBine no la habilita ni la deshabilita"
            .into(),
    ))
}

pub(super) async fn script(s: &mut MySqlSession, id: &str) -> Result<String> {
    let (t, view, _) = one(s, id).await?;
    let Some(v) = view else {
        return Ok(t.submit());
    };
    let rows = s
        .rows(&format!(
            "SHOW CREATE MATERIALIZED VIEW {}.{}",
            quoted(&v.schema),
            quoted(&v.name)
        ))
        .await?;
    let create = rows
        .first()
        .and_then(|r| crate::session::named(r, &["Create Materialized View"]))
        .ok_or_else(|| not_found(&t.name))?;
    Ok(qualify(&create, "MATERIALIZED VIEW", &v.schema, &v.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t1() -> Task {
        Task {
            name: "dbine_t1".into(),
            schedule: Some("PERIODICAL START(2026-11-12T12:58:19) EVERY(1 MINUTES)".into()),
            database: Some("db".into()),
            definition: Some("INSERT INTO db.t SELECT 1, now()".into()),
            properties: Some(
                "('warehouse'='default_warehouse','session.query_timeout'='100')".into(),
            ),
            creator: Some("'root'@'%'".into()),
            ..Default::default()
        }
    }

    #[test]
    fn ids_round_trip_and_foreign_ones_are_refused() {
        assert_eq!(task_id("mv-1", true), "mv:`mv-1`");
        assert_eq!(parse_id("mv:`mv-1`").unwrap(), ("mv-1".into(), true));
        assert_eq!(parse_id("task:`a``b`").unwrap(), ("a`b".into(), false));
        for bad in [
            "mv-1",
            "task:mv-1",
            "job:`x`",
            "task:`x`; DROP TASK y",
            "task:``",
        ] {
            assert!(parse_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn schedules_read_and_scripts_rebuild_the_task() {
        let t = t1();
        assert_eq!(
            t.periodical(),
            Some("START(2026-11-12T12:58:19) EVERY(1 MINUTES)")
        );
        assert_eq!(
            t.start_every(),
            (
                Some("2026-11-12 12:58:19".into()),
                Some((1, "MINUTE".into()))
            )
        );
        assert_eq!(
            t.submit(),
            "SUBMIT TASK `db`.`dbine_t1`\nSCHEDULE START('2026-11-12 12:58:19') EVERY(INTERVAL 1 MINUTE)\nPROPERTIES ('session.query_timeout' = '100')\nAS INSERT INTO db.t SELECT 1, now()"
        );
        let manual = Task {
            name: "once".into(),
            schedule: Some("MANUAL".into()),
            definition: Some("INSERT INTO t SELECT 1;".into()),
            ..Default::default()
        };
        assert_eq!(manual.periodical(), None);
        assert_eq!(
            manual.submit(),
            "SUBMIT TASK `once`\nAS INSERT INTO t SELECT 1"
        );
    }

    #[test]
    fn task_properties_parse() {
        assert_eq!(
            parse_properties("('a'='1', 'b'='x''y','c'=\"z\")"),
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "x'y".into()),
                ("c".into(), "z".into())
            ]
        );
        assert!(parse_properties("").is_empty());
        assert!(parse_properties("garbage").is_empty());
    }

    fn run(task: &str, state: &str, created: &str, finished: Option<&str>) -> Run {
        Run {
            task: task.into(),
            query_id: Some(format!("q-{created}")),
            state: state.into(),
            created: Some(created.into()),
            processed: Some(created.into()),
            finished: finished.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn runs_summarize_into_the_job() {
        let utc: HashMap<(Civil, String), Civil> = ["2026-11-12 13:00:00", "2026-11-12 12:59:00"]
            .iter()
            .map(|l| {
                let t = Civil::parse(l).unwrap();
                ((t, "Etc/UTC".to_string()), t)
            })
            .collect();
        let a = run("t", "RUNNING", "2026-11-12 13:00:00", None);
        let b = run(
            "t",
            "FAILED",
            "2026-11-12 12:59:00",
            Some("2026-11-12 12:59:02"),
        );
        let s = summary(&[&a, &b], &utc, "Etc/UTC");
        assert!(s.running);
        assert_eq!(s.last_started.as_deref(), Some("2026-11-12T13:00:00Z"));
        assert_eq!(s.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(s.last_duration_ms, Some(2000));
        assert_eq!(summary(&[], &utc, "Etc/UTC"), Summary::default());

        let j = job(&t1(), None, summary(&[&b], &utc, "Etc/UTC"));
        assert_eq!(j.id, "task:`dbine_t1`");
        assert_eq!(j.kind, "Task");
        assert!(j.enabled && j.has_history && j.has_script);
        assert!(!j.can_run && !j.can_stop && !j.can_enable);
        assert_eq!(j.group.as_deref(), Some("db"));

        let v = View {
            schema: "db".into(),
            name: "mv1".into(),
            active: true,
            ..Default::default()
        };
        let mut mv_task = t1();
        mv_task.name = "mv-42".into();
        let j = job(&mv_task, Some(&v), summary(&[&a], &utc, "Etc/UTC"));
        assert_eq!(j.id, "mv:`mv-42`");
        assert_eq!(j.name, "mv1");
        assert_eq!(j.kind, "Materialized view");
        assert!(j.can_run && j.can_stop && j.running);
        assert_eq!(j.last_outcome, Some(JobOutcome::Running));

        let r = job_run(&b, &utc, "Etc/UTC");
        assert_eq!(r.outcome, JobOutcome::Failed);
        assert_eq!(r.started.as_deref(), Some("2026-11-12T12:59:00Z"));
        assert_eq!(r.duration_ms, Some(2000));
    }

    #[test]
    fn refresh_triggers_read_from_the_extra_message() {
        let mut r = run("mv-1", "SUCCESS", "2026-11-12 13:00:00", None);
        r.extra = Some(r#"{"executeOption":{"isManual":true}}"#.into());
        assert_eq!(r.trigger().as_deref(), Some("manual"));
        r.extra = Some(r#"{"executeOption":{"isManual":false}}"#.into());
        assert_eq!(r.trigger().as_deref(), Some("programada"));
        r.extra = Some("".into());
        assert_eq!(r.trigger(), None);
    }

    #[test]
    fn users_without_every_database_see_part_of_the_list() {
        assert!(is_root("'root'@'%'"));
        assert!(is_root("root@localhost"));
        assert!(!is_root("'rooter'@'%'"));
        assert!(!is_root("'app'@'%'"));
        assert_eq!(active_roles("public, db_admin"), vec!["public", "db_admin"]);
        assert_eq!(active_roles("`root`"), vec!["root"]);
        assert!(active_roles("NONE").is_empty());
        assert!(active_roles("").is_empty());
        assert!(grants_every_database(&[
            "GRANT SELECT ON ALL TABLES IN ALL DATABASES TO USER 'u'@'%'".into()
        ]));
        assert!(grants_every_database(&[
            "GRANT ALTER, DROP ON ALL DATABASES TO ROLE 'r'".into()
        ]));
        assert!(!grants_every_database(&[
            "GRANT SELECT ON ALL TABLES IN DATABASE db TO USER 'u'@'%'".into(),
            "GRANT USAGE ON ALL CATALOGS TO USER 'u'@'%'".into(),
        ]));
        assert!(!grants_every_database(&[]));
    }

    #[test]
    fn view_scripts_are_qualified() {
        let create = "CREATE MATERIALIZED VIEW `mv1` (`id`)\nREFRESH ASYNC\nAS SELECT 1;";
        assert_eq!(
            qualify(create, "MATERIALIZED VIEW", "db", "mv1"),
            "CREATE MATERIALIZED VIEW `db`.`mv1` (`id`)\nREFRESH ASYNC\nAS SELECT 1;\n"
        );
        assert!(qualify(create, "MATERIALIZED VIEW", "db", "other").starts_with("USE `db`;\n"));
        // Doris writes it unquoted.
        assert_eq!(
            qualify(
                "CREATE MATERIALIZED VIEW mv1\n(id)\nAS SELECT 1",
                "MATERIALIZED VIEW",
                "db",
                "mv1"
            ),
            "CREATE MATERIALIZED VIEW `db`.`mv1`\n(id)\nAS SELECT 1;\n"
        );
        assert!(qualify(
            "CREATE MATERIALIZED VIEW mv10 AS SELECT 1",
            "MATERIALIZED VIEW",
            "db",
            "mv1"
        )
        .starts_with("USE"));
    }
}
