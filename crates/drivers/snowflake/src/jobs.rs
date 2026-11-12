//! Server jobs ([`dbine_driver::Session::jobs`] and the calls that follow
//! it): Snowflake tasks.
//!
//! `SHOW TASKS IN ACCOUNT` lists them (the session database's when the role
//! can't list the account's), without a warehouse. A task graph is one job:
//! its root, with the child tasks and the finalizer as its steps (a graph
//! lives in one schema). A child task whose root the role doesn't see is
//! listed alone, without actions.
//!
//! The last and next runs come from each database's
//! `INFORMATION_SCHEMA.TASK_HISTORY` (the last 7 days), which needs a running
//! warehouse: as in the monitor and the process list, a suspended one is
//! never resumed. The list says so in a note; the history refuses. A graph's
//! run is its tasks' runs with the same `GRAPH_RUN_GROUP_ID`, folded the same
//! way in the list, the detail and the history: running while any task runs,
//! failed when any failed.
//!
//! The id is the task's qualified name with each part quoted
//! (`"DB"."SCHEMA"."TASK"`). One that comes back is parsed and quoted again
//! before it goes into a statement; anything else is refused.

use crate::monitor::{quote, Runner, Set};
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, JobStepRun, PropertyInfo, Result,
    ServerJob,
};
use serde_json::Value as Json;
use std::collections::{BTreeSet, HashMap};

/// The engine's word for them, shown as a tag.
const KIND: &str = "Task";
/// Databases whose history the list reads, at most (one statement).
const MAX_HISTORY_DATABASES: usize = 50;
/// Tasks of one graph read, at most.
const MAX_GRAPH_TASKS: usize = 100;
/// Rows `SHOW TASKS` gives at most.
const SHOW_LIMIT: usize = 10_000;
/// Rows a `TASK_HISTORY` call gives at most (its `RESULT_LIMIT` maximum).
const HISTORY_LIMIT: usize = 10_000;
/// Newest runs of each task the list reads: a graph run going on, the one
/// before it (finished), and one more for a retried task.
const RUNS_PER_TASK: usize = 3;
const GRAPH: &str = "Grafo de tareas";

/// A task's `(database, schema, name)`.
type Key = (String, String, String);

/// One row of `SHOW TASKS`.
#[derive(Debug, Clone, Default)]
struct Task {
    database: String,
    schema: String,
    name: String,
    /// Snowflake's own id for it.
    sf_id: Option<String>,
    owner: Option<String>,
    comment: Option<String>,
    warehouse: Option<String>,
    schedule: Option<String>,
    /// The names (last part) of the tasks it runs after.
    predecessors: Vec<String>,
    state: Option<String>,
    definition: Option<String>,
    condition: Option<String>,
    overlap: Option<String>,
    error_integration: Option<String>,
    config: Option<String>,
    created_on: Option<String>,
    last_suspended_on: Option<String>,
    last_suspended_reason: Option<String>,
    /// The root (name) it is the finalizer of.
    finalizes: Option<String>,
}

impl Task {
    fn id(&self) -> String {
        job_id(&self.database, &self.schema, &self.name)
    }

    fn started(&self) -> bool {
        self.state
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("started"))
    }

    /// A standalone task or a graph's root.
    fn is_root(&self) -> bool {
        self.predecessors.is_empty() && self.finalizes.is_none()
    }

    fn same_schema(&self, o: &Task) -> bool {
        self.database == o.database && self.schema == o.schema
    }

    fn key(&self) -> Key {
        (
            self.database.clone(),
            self.schema.clone(),
            self.name.clone(),
        )
    }
}

/// `"DB"."SCHEMA"."TASK"`.
pub(crate) fn job_id(database: &str, schema: &str, name: &str) -> String {
    format!("{}.{}.{}", quote(database), quote(schema), quote(name))
}

/// A job id back into its three names: three quoted identifiers (`""` for a
/// quote inside) joined by dots, nothing else.
pub(crate) fn parse_id(id: &str) -> Option<(String, String, String)> {
    let mut parts: Vec<String> = Vec::new();
    let mut chars = id.trim().chars().peekable();
    loop {
        if chars.next()? != '"' {
            return None;
        }
        let mut part = String::new();
        loop {
            match chars.next()? {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    part.push('"');
                }
                '"' => break,
                c if c.is_control() => return None,
                c => part.push(c),
            }
        }
        if part.is_empty() || part.chars().count() > 255 {
            return None;
        }
        parts.push(part);
        match chars.next() {
            None => break,
            Some('.') if parts.len() < 3 => {}
            Some(_) => return None,
        }
    }
    let [db, schema, name]: [String; 3] = parts.try_into().ok()?;
    Some((db, schema, name))
}

fn checked(id: &str) -> Result<(String, String, String)> {
    parse_id(id)
        .ok_or_else(|| Error::Query(format!("«{}» no es un id de tarea de Snowflake", id.trim())))
}

/// A string literal: Snowflake reads backslash escapes in them.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// The names of a qualified name, quoted (`"DB"."S"."T"`) or not (`DB.S.T`).
fn name_parts(q: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut quoted = false;
    let mut chars = q.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                if let Some(p) = parts.last_mut() {
                    p.push('"');
                }
            }
            '"' => quoted = !quoted,
            '.' if !quoted => parts.push(String::new()),
            c => {
                if let Some(p) = parts.last_mut() {
                    p.push(c);
                }
            }
        }
    }
    parts
}

fn last_name(q: &str) -> String {
    name_parts(q).pop().unwrap_or_default()
}

/// `predecessors` of `SHOW TASKS`: a JSON array of qualified names (a
/// comma-separated list in older accounts).
fn predecessors(v: Option<&str>) -> Vec<String> {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
        return Vec::new();
    };
    let names: Vec<String> = serde_json::from_str(v).unwrap_or_else(|_| {
        v.trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|s| s.trim().to_string())
            .collect()
    });
    names
        .iter()
        .map(|n| last_name(n))
        .filter(|n| !n.is_empty())
        .collect()
}

/// The root a finalizer task finalizes (`task_relations`).
fn finalized_root(v: Option<&str>) -> Option<String> {
    let j: Json = serde_json::from_str(v?).ok()?;
    j.get("FinalizedRootTask")
        .and_then(Json::as_str)
        .map(last_name)
        .filter(|n| !n.is_empty())
}

fn get<'a>(s: &'a Set, row: usize, name: &str) -> Option<&'a str> {
    let i = s.cols.iter().position(|c| c.eq_ignore_ascii_case(name))?;
    s.rows
        .get(row)?
        .get(i)?
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "null")
}

fn text(s: &Set, row: usize, name: &str) -> Option<String> {
    get(s, row, name).map(str::to_string)
}

fn int(s: &Set, row: usize, name: &str) -> Option<i64> {
    get(s, row, name)
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v as i64)
}

/// Epoch milliseconds → RFC 3339 in UTC.
fn rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// A `SHOW` timestamp (epoch seconds as text, as the SQL API gives it) →
/// RFC 3339 in UTC.
fn shown_time(v: Option<&str>) -> Option<String> {
    let (secs, nanos) = crate::epoch(v?)?;
    chrono::DateTime::from_timestamp(secs, nanos)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// The rows of `SHOW TASKS`.
fn tasks(s: &Set) -> Vec<Task> {
    (0..s.rows.len())
        .filter_map(|i| {
            Some(Task {
                database: text(s, i, "database_name")?,
                schema: text(s, i, "schema_name")?,
                name: text(s, i, "name")?,
                sf_id: text(s, i, "id"),
                owner: text(s, i, "owner"),
                comment: text(s, i, "comment"),
                warehouse: text(s, i, "warehouse"),
                schedule: text(s, i, "schedule"),
                predecessors: predecessors(get(s, i, "predecessors")),
                state: text(s, i, "state"),
                definition: text(s, i, "definition"),
                condition: text(s, i, "condition"),
                overlap: text(s, i, "allow_overlapping_execution"),
                error_integration: text(s, i, "error_integration"),
                config: text(s, i, "config"),
                created_on: shown_time(get(s, i, "created_on")),
                last_suspended_on: shown_time(get(s, i, "last_suspended_on")),
                last_suspended_reason: text(s, i, "last_suspended_reason"),
                finalizes: finalized_root(get(s, i, "task_relations")),
            })
        })
        .collect()
}

/// `(database, schema, name)` → the task's index.
fn index(tasks: &[Task]) -> HashMap<(&str, &str, &str), usize> {
    tasks
        .iter()
        .enumerate()
        .map(|(i, t)| ((t.database.as_str(), t.schema.as_str(), t.name.as_str()), i))
        .collect()
}

/// Whether the root of a child task (or a finalizer) is among `tasks`.
fn has_visible_root(tasks: &[Task], at: &HashMap<(&str, &str, &str), usize>, i: usize) -> bool {
    let mut cur = i;
    for _ in 0..MAX_GRAPH_TASKS {
        let t = &tasks[cur];
        if t.is_root() {
            return true;
        }
        let up = t
            .finalizes
            .as_deref()
            .or(t.predecessors.first().map(String::as_str));
        match up.and_then(|n| at.get(&(t.database.as_str(), t.schema.as_str(), n))) {
            Some(&j) => cur = j,
            None => return false,
        }
    }
    false
}

/// A graph's tasks in run order: the root, its children level by level, the
/// finalizer last.
fn graph(tasks: &[Task], root: usize) -> Vec<usize> {
    let r = &tasks[root];
    let mut order = vec![root];
    let mut at = 0;
    while at < order.len() && order.len() < MAX_GRAPH_TASKS {
        let parent = tasks[order[at]].name.clone();
        for (j, t) in tasks.iter().enumerate() {
            if order.len() >= MAX_GRAPH_TASKS {
                break;
            }
            if t.same_schema(r) && !order.contains(&j) && t.predecessors.contains(&parent) {
                order.push(j);
            }
        }
        at += 1;
    }
    for (j, t) in tasks.iter().enumerate() {
        if t.same_schema(r)
            && t.finalizes.as_deref() == Some(r.name.as_str())
            && !order.contains(&j)
        {
            order.push(j);
        }
    }
    order
}

/// The tasks a job covers, in run order: a root's graph, or the task alone.
fn order_of(tasks: &[Task], at: usize) -> Vec<usize> {
    if tasks[at].is_root() {
        graph(tasks, at)
    } else {
        vec![at]
    }
}

/// `"true"` / `"false"` as the properties show them.
fn yes_no(v: Option<String>) -> Option<String> {
    v.map(|v| match v.to_ascii_lowercase().as_str() {
        "true" => "Sí".into(),
        "false" => "No".into(),
        _ => v,
    })
}

/// A refusal for lack of privileges (`Insufficient privileges`, SQLSTATE
/// 42501, `does not exist or not authorized`), not any other failure.
fn denied(e: &Error) -> bool {
    let Error::Query(m) = e else {
        return false;
    };
    let m = m.to_ascii_lowercase();
    m.contains("insufficient privileges")
        || m.contains("sqlstate 42501")
        || m.contains("not authorized")
        || m.contains("access control")
}

/// A task as a job. `actions`: a standalone task or a visible graph's root,
/// which can be run, stopped and resumed.
fn server_job(t: &Task, actions: bool) -> ServerJob {
    ServerJob {
        id: t.id(),
        name: t.name.clone(),
        kind: KIND.into(),
        group: Some(format!("{}.{}", t.database, t.schema)),
        description: t.comment.clone(),
        owner: t.owner.clone(),
        enabled: t.started(),
        // A triggered task has no schedule: it runs when its streams have data.
        schedule: t
            .schedule
            .clone()
            .or_else(|| t.condition.as_ref().map(|c| format!("WHEN {c}"))),
        can_run: actions,
        can_stop: actions,
        // Without a schedule or a condition there's nothing to resume.
        can_enable: actions && (t.schedule.is_some() || t.condition.is_some()),
        has_history: true,
        has_script: true,
        ..Default::default()
    }
}

fn outcome(state: &str) -> JobOutcome {
    match state.to_ascii_uppercase().as_str() {
        "SUCCEEDED" => JobOutcome::Succeeded,
        "FAILED" | "FAILED_AND_AUTO_SUSPENDED" => JobOutcome::Failed,
        "CANCELLED" | "CANCELED" => JobOutcome::Canceled,
        "SKIPPED" => JobOutcome::Skipped,
        "EXECUTING" => JobOutcome::Running,
        _ => JobOutcome::Unknown,
    }
}

/// What started a run (`scheduled_from`).
fn trigger(from: &str) -> String {
    match from.to_ascii_uppercase().as_str() {
        "SCHEDULE" => "programada".into(),
        "EXECUTE TASK" => "manual".into(),
        "TRIGGER" => "datos nuevos".into(),
        _ => from.to_string(),
    }
}

/// The session's warehouse, and whether it runs.
#[derive(Debug, PartialEq)]
enum Warehouse {
    Running,
    Suspended(String),
    Missing,
}

async fn warehouse(r: &(dyn Runner + Sync)) -> Result<Warehouse> {
    let wh = match r.warehouse() {
        Some(w) => Some(w),
        None => {
            let s = r.rows("SELECT CURRENT_WAREHOUSE() AS warehouse").await?;
            text(&s, 0, "warehouse")
        }
    };
    let Some(wh) = wh else {
        return Ok(Warehouse::Missing);
    };
    let w = r
        .rows(&format!("SHOW WAREHOUSES LIKE {}", literal(&wh)))
        .await?;
    let on = (0..w.rows.len()).any(|i| {
        get(&w, i, "name").is_some_and(|n| n.eq_ignore_ascii_case(&wh))
            && get(&w, i, "state").is_some_and(|s| {
                s.eq_ignore_ascii_case("STARTED") || s.eq_ignore_ascii_case("RESIZING")
            })
    });
    Ok(if on {
        Warehouse::Running
    } else {
        Warehouse::Suspended(wh)
    })
}

/// What the history says of a job's runs, for the list and the detail.
#[derive(Debug, Default, Clone, PartialEq)]
struct Latest {
    /// The run going on: when it started.
    running: Option<Option<i64>>,
    /// The last finished run: how it ended, when it started, how long.
    finished: Option<(JobOutcome, Option<i64>, Option<u64>)>,
    next_ms: Option<i64>,
}

/// Each task's newest runs (`RUNS_PER_TASK`) and its next scheduled one, in
/// each of `databases` (one statement), with how many rows each database's
/// history gave (counted before `QUALIFY` keeps a few).
fn latest_sql(databases: &[String]) -> String {
    databases
        .iter()
        .map(|db| {
            format!(
                "(SELECT database_name, schema_name, name, state, graph_run_group_id, run_id,
        DATE_PART(EPOCH_MILLISECOND, scheduled_time) AS scheduled_ms,
        DATE_PART(EPOCH_MILLISECOND, query_start_time) AS started_ms,
        DATE_PART(EPOCH_MILLISECOND, completed_time) AS completed_ms,
        COUNT(*) OVER () AS history_rows
   FROM TABLE({}.INFORMATION_SCHEMA.TASK_HISTORY(RESULT_LIMIT => {HISTORY_LIMIT}))
 QUALIFY ROW_NUMBER() OVER (
           PARTITION BY database_name, schema_name, name, IFF(state = 'SCHEDULED', 0, 1)
           ORDER BY IFF(state = 'SCHEDULED', -1, 1) * DATE_PART(EPOCH_MILLISECOND, scheduled_time) DESC)
         <= IFF(state = 'SCHEDULED', 1, {RUNS_PER_TASK}))",
                quote(db)
            )
        })
        .collect::<Vec<_>>()
        .join("\nUNION ALL\n")
}

/// What the list's history statement read.
#[derive(Debug, Default)]
struct Recent {
    /// Each task's newest runs.
    runs: HashMap<Key, Vec<HistoryRow>>,
    /// Each task's next scheduled start.
    next: HashMap<Key, i64>,
    /// The databases whose history reached `HISTORY_LIMIT`: older runs were
    /// left out, maybe some task's last one.
    full: BTreeSet<String>,
}

fn read_recent(s: &Set, into: &mut Recent) {
    for i in 0..s.rows.len() {
        let (Some(db), Some(schema), Some(name), Some(state)) = (
            text(s, i, "database_name"),
            text(s, i, "schema_name"),
            text(s, i, "name"),
            get(s, i, "state"),
        ) else {
            continue;
        };
        if int(s, i, "history_rows").is_some_and(|n| n >= HISTORY_LIMIT as i64) {
            into.full.insert(db.clone());
        }
        let key = (db, schema, name);
        if state.eq_ignore_ascii_case("SCHEDULED") {
            if let Some(t) = int(s, i, "scheduled_ms") {
                into.next.insert(key, t);
            }
        } else if let Some(h) = history_row(s, i) {
            into.runs.entry(key).or_default().push(h);
        }
    }
}

/// A job's state from its tasks' newest runs (`members`: the job's own task
/// first, then the rest of its graph): each run of its own task folded with
/// the graph's task runs of the same graph run, as the history does.
fn latest_of(recent: &Recent, members: &[Key]) -> Option<Latest> {
    let own = members.first()?;
    let next_ms = recent.next.get(own).copied();
    let mut roots: Vec<&HistoryRow> = recent.runs.get(own).into_iter().flatten().collect();
    if roots.is_empty() && next_ms.is_none() {
        return None;
    }
    roots.sort_by_key(|h| std::cmp::Reverse(h.started));
    let kids: Vec<&HistoryRow> = members[1..]
        .iter()
        .filter_map(|k| recent.runs.get(k))
        .flatten()
        .collect();
    let mut l = Latest {
        next_ms,
        ..Default::default()
    };
    for h in roots {
        let same: Vec<&HistoryRow> = kids
            .iter()
            .copied()
            .filter(|k| k.group.is_some() && k.group == h.group)
            .collect();
        let (outcome, end) = fold(h, &same);
        if outcome == JobOutcome::Running {
            l.running.get_or_insert(h.started);
        } else if l.finished.is_none() {
            l.finished = Some((outcome, h.started, duration(h.started, end)));
        }
    }
    Some(l)
}

fn apply(job: &mut ServerJob, l: &Latest) {
    job.running = l.running.is_some();
    if let Some((o, started, duration)) = &l.finished {
        job.last_outcome = Some(*o);
        job.last_started = started.and_then(rfc3339);
        job.last_duration_ms = *duration;
    } else if l.running.is_some() {
        job.last_outcome = Some(JobOutcome::Running);
    }
    if let Some(Some(started)) = l.running {
        job.last_started = rfc3339(started);
    }
    if job.enabled {
        job.next_run = l.next_ms.and_then(rfc3339);
    }
}

/// Fills the jobs' last and next runs from the history of their databases
/// (`members`: each job's tasks, its own first), when the warehouse runs;
/// what's missing goes to `notes`. `false`: the session has no warehouse,
/// so there's no history to read at all.
async fn fill_latest(
    r: &(dyn Runner + Sync),
    jobs: &mut [ServerJob],
    members: &[Vec<Key>],
    notes: &mut Vec<String>,
) -> bool {
    let databases: Vec<String> = members
        .iter()
        .filter_map(|m| m.first())
        .map(|k| k.0.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if databases.is_empty() {
        return true;
    }
    match warehouse(r).await {
        Ok(Warehouse::Running) => {}
        Ok(Warehouse::Suspended(wh)) => {
            notes.push(format!("el warehouse {wh} está suspendido: no se muestran la última ni la próxima ejecución de las tareas, para no reanudarlo y consumir créditos"));
            return true;
        }
        Ok(Warehouse::Missing) => {
            notes.push("la conexión no tiene warehouse: no se muestran la última ni la próxima ejecución de las tareas, ni su historial".into());
            return false;
        }
        Err(e) => {
            notes.push(format!("no se pudo leer el historial de las tareas: {e}"));
            return true;
        }
    }
    let read = &databases[..databases.len().min(MAX_HISTORY_DATABASES)];
    if read.len() < databases.len() {
        notes.push(format!("la última ejecución se muestra solo para las tareas de las primeras {MAX_HISTORY_DATABASES} bases de datos"));
    }
    let mut recent = Recent::default();
    match r.rows(&latest_sql(read)).await {
        Ok(s) => read_recent(&s, &mut recent),
        Err(e) if read.len() == 1 => {
            notes.push(format!("no se pudo leer el historial de las tareas: {e}"));
            return true;
        }
        // One database fails the whole statement (no USAGE on it, dropped
        // since SHOW TASKS…): each one alone, then.
        Err(e) => {
            tracing::debug!("snowflake: task history of every database at once: {e}");
            let mut failed = Vec::new();
            let mut first = None;
            for db in read {
                match r.rows(&latest_sql(std::slice::from_ref(db))).await {
                    Ok(s) => read_recent(&s, &mut recent),
                    Err(e) => {
                        failed.push(db.as_str());
                        first.get_or_insert(e);
                    }
                }
            }
            if let Some(e) = first {
                notes.push(format!(
                    "no se pudo leer el historial de las tareas en {}: {e}",
                    failed.join(", ")
                ));
            }
        }
    }
    if !recent.full.is_empty() {
        let full: Vec<&str> = recent.full.iter().map(String::as_str).collect();
        notes.push(format!(
            "en {} hay más de {HISTORY_LIMIT} ejecuciones recientes: puede faltar la última ejecución de algunas tareas",
            full.join(", ")
        ));
    }
    for (job, m) in jobs.iter_mut().zip(members) {
        if let Some(l) = latest_of(&recent, m) {
            apply(job, &l);
        }
    }
    true
}

/// The jobs of a `SHOW TASKS`: roots and standalone tasks, and the child
/// tasks whose root isn't visible; with each one's tasks (its own first).
fn jobs_of(tasks: &[Task]) -> (Vec<ServerJob>, Vec<Vec<Key>>, bool) {
    let at = index(tasks);
    let mut jobs = Vec::new();
    let mut members = Vec::new();
    let mut orphans = false;
    for (i, t) in tasks.iter().enumerate() {
        let actions = t.is_root();
        if !actions {
            if has_visible_root(tasks, &at, i) {
                continue;
            }
            orphans = true;
        }
        jobs.push(server_job(t, actions));
        members.push(order_of(tasks, i).iter().map(|&j| tasks[j].key()).collect());
    }
    (jobs, members, orphans)
}

/// `SHOW TASKS` shows only the tasks the role holds some privilege on: a
/// note, unless the role is ACCOUNTADMIN.
async fn partial_note(r: &(dyn Runner + Sync)) -> Option<String> {
    let role = r
        .rows("SELECT CURRENT_ROLE() AS role")
        .await
        .ok()
        .and_then(|s| text(&s, 0, "role"));
    match role {
        Some(role) if role.eq_ignore_ascii_case("ACCOUNTADMIN") => None,
        Some(role) => Some(format!(
            "se ven solo las tareas sobre las que el rol {role} tiene algún privilegio"
        )),
        None => Some("se ven solo las tareas sobre las que el rol tiene algún privilegio".into()),
    }
}

pub(crate) async fn list(r: &(dyn Runner + Sync)) -> Result<JobList> {
    let mut notes = Vec::new();
    let shown = match r.rows("SHOW TASKS IN ACCOUNT").await {
        Ok(s) => s,
        // Only a refusal falls back to the session's database; a timeout or
        // a lost connection is the list's failure.
        Err(e) if denied(&e) => {
            let Some(db) = r.database() else {
                return Err(e);
            };
            tracing::debug!("snowflake: SHOW TASKS IN ACCOUNT refused: {e}");
            let s = r
                .rows(&format!("SHOW TASKS IN DATABASE {}", quote(&db)))
                .await?;
            notes.push(format!(
                "se ven solo las tareas de la base {db}: el rol no puede listar las de toda la cuenta"
            ));
            s
        }
        Err(e) => return Err(e),
    };
    if shown.rows.len() >= SHOW_LIMIT {
        notes.push(format!(
            "Snowflake muestra hasta {SHOW_LIMIT} tareas: puede haber más"
        ));
    }
    let all = tasks(&shown);
    let (mut jobs, members, orphans) = jobs_of(&all);
    if orphans {
        notes.push("algunas tareas hijas se muestran sueltas y sin acciones: el rol no ve la tarea raíz de su grafo".into());
    }
    if jobs.is_empty() {
        notes.push("el rol actual no ve ninguna tarea: Snowflake muestra solo aquellas sobre las que tiene algún privilegio".into());
    } else if let Some(n) = partial_note(r).await {
        notes.push(n);
    }
    if !fill_latest(r, &mut jobs, &members, &mut notes).await {
        for j in &mut jobs {
            j.has_history = false;
        }
    }
    Ok(JobList { jobs, notes })
}

/// The tasks of the job's schema (cloud services: no warehouse), and the
/// job's index among them.
async fn schema_tasks(
    r: &(dyn Runner + Sync),
    db: &str,
    schema: &str,
    name: &str,
) -> Result<(Vec<Task>, Option<usize>)> {
    let s = r
        .rows(&format!(
            "SHOW TASKS IN SCHEMA {}.{}",
            quote(db),
            quote(schema)
        ))
        .await?;
    let tasks = tasks(&s);
    let at = tasks
        .iter()
        .position(|t| t.name == name && t.database == db && t.schema == schema);
    Ok((tasks, at))
}

fn not_found(db: &str, schema: &str, name: &str) -> Error {
    Error::Query(format!(
        "no existe la tarea {db}.{schema}.{name} o el rol no la ve"
    ))
}

fn prop(group: &str, label: &str, value: Option<String>) -> Option<PropertyInfo> {
    value.map(|value| PropertyInfo {
        group: group.into(),
        label: label.into(),
        value,
    })
}

fn detail_of(tasks: &[Task], at: usize) -> JobDetail {
    let t = &tasks[at];
    let rooted = t.is_root();
    let order = order_of(tasks, at);
    let steps = order
        .iter()
        .map(|&i| {
            let s = &tasks[i];
            JobStep {
                name: s.name.clone(),
                kind: Some("SQL".into()),
                // Only where it isn't the job's own.
                target: (i != at && s.warehouse != t.warehouse)
                    .then(|| s.warehouse.clone().unwrap_or_else(|| "serverless".into())),
                command: s.definition.clone(),
            }
        })
        .collect();
    let mut properties: Vec<PropertyInfo> = [
        prop(
            "",
            "Warehouse",
            Some(t.warehouse.clone().unwrap_or_else(|| "serverless".into())),
        ),
        prop("", "Condición", t.condition.clone()),
        prop("", "Ejecuciones superpuestas", yes_no(t.overlap.clone())),
        prop("", "Integración de errores", t.error_integration.clone()),
        prop("", "Configuración", t.config.clone()),
        prop("", "Creada", t.created_on.clone()),
        prop("", "Última suspensión", t.last_suspended_on.clone()),
        prop(
            "",
            "Motivo de la última suspensión",
            t.last_suspended_reason.clone(),
        ),
        prop("", "Id de Snowflake", t.sf_id.clone()),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut notes = Vec::new();
    if order.len() > 1 {
        notes.push(GRAPH_ENABLE.into());
        for &i in &order {
            let s = &tasks[i];
            let state = s.state.clone().unwrap_or_default();
            let value = if i == at {
                format!("{state}, raíz")
            } else if s.finalizes.is_some() {
                format!("{state}, finalizador")
            } else {
                format!("{state}, después de {}", s.predecessors.join(", "))
            };
            properties.push(PropertyInfo {
                group: GRAPH.into(),
                label: s.name.clone(),
                value,
            });
        }
    }
    JobDetail {
        job: server_job(t, rooted),
        steps,
        schedules: Vec::new(),
        properties,
        notes,
    }
}

/// A graph's detail: what enabling and disabling it does.
const GRAPH_ENABLE: &str = "habilitar y deshabilitar actúa sobre la tarea raíz: las hijas corren cuando corre la raíz, salvo las suspendidas";

pub(crate) async fn detail(r: &(dyn Runner + Sync), id: &str) -> Result<JobDetail> {
    let (db, schema, name) = checked(id)?;
    let (tasks, at) = schema_tasks(r, &db, &schema, &name).await?;
    let at = at.ok_or_else(|| not_found(&db, &schema, &name))?;
    let mut d = detail_of(&tasks, at);
    // Without a running warehouse the last and next runs stay empty: a note
    // says why.
    match warehouse(r).await {
        Ok(Warehouse::Running) => match r.rows(&latest_sql(std::slice::from_ref(&db))).await {
            Ok(s) => {
                let mut recent = Recent::default();
                read_recent(&s, &mut recent);
                let members: Vec<Key> = order_of(&tasks, at)
                    .iter()
                    .map(|&i| tasks[i].key())
                    .collect();
                if let Some(l) = latest_of(&recent, &members) {
                    apply(&mut d.job, &l);
                }
            }
            Err(e) => tracing::debug!("snowflake: task history not read: {e}"),
        },
        // No warehouse at all: no history to offer either.
        Ok(Warehouse::Missing) => {
            d.job.has_history = false;
            d.notes.push("la conexión no tiene warehouse: no se muestran la última ni la próxima ejecución de la tarea, ni su historial".into());
        }
        Ok(Warehouse::Suspended(wh)) => d.notes.push(format!("el warehouse {wh} está suspendido: no se muestran la última ni la próxima ejecución de la tarea, para no reanudarlo y consumir créditos")),
        Err(e) => d.notes.push(format!("no se pudo leer el historial de la tarea: {e}")),
    }
    Ok(d)
}

/// One task's runs from `TASK_HISTORY`, newest first: `TASK_NAME` matches
/// the name in any schema (and in any case), so the rows are filtered by
/// the exact names.
fn runs_sql(db: &str, schema: &str, names: &[String], limit: usize) -> String {
    let per_task = (limit * 2 + 1).min(10_000);
    names
        .iter()
        .map(|n| {
            format!(
                "(SELECT name, state, scheduled_from, error_code, error_message, return_value, query_id,
        graph_run_group_id, run_id,
        DATE_PART(EPOCH_MILLISECOND, scheduled_time) AS scheduled_ms,
        DATE_PART(EPOCH_MILLISECOND, query_start_time) AS started_ms,
        DATE_PART(EPOCH_MILLISECOND, completed_time) AS completed_ms
   FROM TABLE({}.INFORMATION_SCHEMA.TASK_HISTORY(TASK_NAME => {}, RESULT_LIMIT => {per_task}))
  WHERE database_name = {} AND schema_name = {} AND name = {} AND state <> 'SCHEDULED')",
                quote(db),
                literal(n),
                literal(db),
                literal(schema),
                literal(n),
            )
        })
        .collect::<Vec<_>>()
        .join("\nUNION ALL\n")
}

/// One task run of `TASK_HISTORY` (the history's and the list's).
#[derive(Debug, Clone)]
struct HistoryRow {
    name: String,
    outcome: JobOutcome,
    from: Option<String>,
    message: Option<String>,
    output: Option<String>,
    /// The graph run it belongs to.
    group: Option<String>,
    query_id: Option<String>,
    started: Option<i64>,
    completed: Option<i64>,
}

fn history_row(s: &Set, i: usize) -> Option<HistoryRow> {
    let scheduled = int(s, i, "scheduled_ms");
    Some(HistoryRow {
        name: text(s, i, "name")?,
        outcome: outcome(get(s, i, "state")?),
        from: text(s, i, "scheduled_from"),
        message: match (get(s, i, "error_code"), get(s, i, "error_message")) {
            (Some(c), Some(m)) => Some(format!("{m} ({c})")),
            (_, m) => m.map(str::to_string),
        },
        output: text(s, i, "return_value"),
        group: text(s, i, "graph_run_group_id").or_else(|| text(s, i, "run_id")),
        query_id: text(s, i, "query_id"),
        started: int(s, i, "started_ms").or(scheduled),
        completed: int(s, i, "completed_ms"),
    })
}

fn history_rows(s: &Set) -> Vec<HistoryRow> {
    (0..s.rows.len())
        .filter_map(|i| history_row(s, i))
        .collect()
}

/// How one graph run went, from its root's (or standalone task's) run and
/// the graph's other task runs of the same graph run: running while any
/// task runs, else failed when any failed, else canceled when any was, else
/// as the root ended. Only each task's latest attempt counts (a retried
/// graph runs its failed tasks again). With the time the last task ended.
fn fold(root: &HistoryRow, kids: &[&HistoryRow]) -> (JobOutcome, Option<i64>) {
    let mut last: HashMap<&str, &HistoryRow> = HashMap::new();
    for h in std::iter::once(root).chain(kids.iter().copied()) {
        match last.get(h.name.as_str()) {
            Some(seen) if seen.started >= h.started => {}
            _ => {
                last.insert(&h.name, h);
            }
        }
    }
    let any = |o: JobOutcome| last.values().any(|h| h.outcome == o);
    let outcome = if any(JobOutcome::Running) {
        JobOutcome::Running
    } else if any(JobOutcome::Failed) {
        JobOutcome::Failed
    } else if any(JobOutcome::Canceled) {
        JobOutcome::Canceled
    } else {
        root.outcome
    };
    let end = if outcome == JobOutcome::Running {
        None
    } else {
        std::iter::once(root)
            .chain(kids.iter().copied())
            .filter_map(|h| h.completed)
            .max()
    };
    (outcome, end)
}

fn step_run(h: &HistoryRow) -> JobStepRun {
    JobStepRun {
        name: h.name.clone(),
        outcome: h.outcome,
        started: h.started.and_then(rfc3339),
        duration_ms: duration(h.started, h.completed),
        message: h.message.clone().or_else(|| h.output.clone()),
    }
}

fn duration(start: Option<i64>, end: Option<i64>) -> Option<u64> {
    Some((end? - start?).max(0) as u64)
}

/// The root's runs, newest first, each with the graph's task runs as its
/// steps (graph runs only).
fn group_runs(rows: Vec<HistoryRow>, root: &str, order: &[String], limit: usize) -> Vec<JobRun> {
    let (mut roots, children): (Vec<HistoryRow>, Vec<HistoryRow>) =
        rows.into_iter().partition(|h| h.name == root);
    roots.sort_by_key(|h| std::cmp::Reverse(h.started));
    roots.truncate(limit);
    let mut by_group: HashMap<&str, Vec<&HistoryRow>> = HashMap::new();
    for c in &children {
        if let Some(g) = c.group.as_deref() {
            by_group.entry(g).or_default().push(c);
        }
    }
    let rank = |n: &str| order.iter().position(|o| o == n).unwrap_or(usize::MAX);
    roots
        .iter()
        .map(|h| {
            let mut kids: Vec<&HistoryRow> = h
                .group
                .as_deref()
                .and_then(|g| by_group.get(g))
                .cloned()
                .unwrap_or_default();
            kids.sort_by_key(|k| (rank(&k.name), k.started));
            let all: Vec<&HistoryRow> = std::iter::once(h).chain(kids.iter().copied()).collect();
            let (outcome, end) = fold(h, &kids);
            let message = h.message.clone().or_else(|| {
                kids.iter()
                    .find_map(|k| k.message.as_ref().map(|m| format!("{}: {m}", k.name)))
                    .or_else(|| h.output.clone())
            });
            JobRun {
                id: h.group.clone().or_else(|| h.query_id.clone()),
                started: h.started.and_then(rfc3339),
                duration_ms: duration(h.started, end),
                outcome,
                trigger: h.from.as_deref().map(trigger),
                message,
                steps: if kids.is_empty() {
                    Vec::new()
                } else {
                    all.iter().map(|k| step_run(k)).collect()
                },
            }
        })
        .collect()
}

pub(crate) async fn runs(r: &(dyn Runner + Sync), id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let (db, schema, name) = checked(id)?;
    let limit = limit.min(MAX_JOB_RUNS) as usize;
    if limit == 0 {
        return Ok(Vec::new());
    }
    match warehouse(r).await? {
        Warehouse::Running => {}
        Warehouse::Suspended(wh) => {
            return Err(Error::Query(format!(
                "el warehouse {wh} está suspendido: el historial de las tareas no lo reanuda para no consumir créditos"
            )))
        }
        Warehouse::Missing => {
            return Err(Error::Query(
                "Snowflake necesita un warehouse para leer el historial de las tareas: la conexión no tiene uno".into(),
            ))
        }
    }
    let (tasks, at) = schema_tasks(r, &db, &schema, &name).await?;
    let order: Vec<String> = match at {
        Some(i) => order_of(&tasks, i)
            .into_iter()
            .map(|j| tasks[j].name.clone())
            .collect(),
        None => vec![name.clone()],
    };
    let s = r.rows(&runs_sql(&db, &schema, &order, limit)).await?;
    Ok(group_runs(history_rows(&s), &name, &order, limit))
}

/// `EXECUTE TASK`: a run of the task (or its whole graph) now, in the
/// background.
pub(crate) async fn run(r: &(dyn Runner + Sync), id: &str) -> Result<()> {
    let (db, schema, name) = checked(id)?;
    r.rows(&format!("EXECUTE TASK {}", job_id(&db, &schema, &name)))
        .await
        .map(|_| ())
}

/// Cancels the graph's (or the task's) runs in progress.
pub(crate) async fn stop(r: &(dyn Runner + Sync), id: &str) -> Result<()> {
    let (db, schema, name) = checked(id)?;
    r.rows(&format!(
        "SELECT SYSTEM$USER_TASK_CANCEL_ONGOING_EXECUTIONS({})",
        literal(&job_id(&db, &schema, &name))
    ))
    .await
    .map(|_| ())
}

/// `ALTER TASK … RESUME | SUSPEND` on the task (a graph's root: the graph
/// runs with it).
pub(crate) async fn set_enabled(r: &(dyn Runner + Sync), id: &str, enabled: bool) -> Result<()> {
    let (db, schema, name) = checked(id)?;
    r.rows(&format!(
        "ALTER TASK {} {}",
        job_id(&db, &schema, &name),
        if enabled { "RESUME" } else { "SUSPEND" }
    ))
    .await
    .map(|_| ())
}

/// The `GET_DDL` of each task in run order, joined, then the resumes of the
/// started ones (children before the root: tasks are created suspended,
/// and a child can't change once its root runs).
fn script_of(tasks: &[Task], order: &[usize], ddl: &Set) -> String {
    let mut out = String::new();
    for n in 0..order.len() {
        let d = get(ddl, 0, &format!("d{n}")).unwrap_or_default();
        out.push_str(d.trim_end());
        if !d.trim_end().ends_with(';') {
            out.push(';');
        }
        out.push_str("\n\n");
    }
    let started: Vec<&Task> = order
        .iter()
        .rev()
        .map(|&i| &tasks[i])
        .filter(|t| t.started())
        .collect();
    if !started.is_empty() {
        out.push_str("-- Las tareas se crean suspendidas: se reanudan las que estaban activas, la raíz al final.\n");
        for t in started {
            out.push_str(&format!("ALTER TASK {} RESUME;\n", t.id()));
        }
    }
    out.trim_end().to_string() + "\n"
}

fn ddl_sql(tasks: &[Task], order: &[usize]) -> String {
    let cols: Vec<String> = order
        .iter()
        .enumerate()
        .map(|(n, &i)| format!("GET_DDL('TASK', {}, TRUE) AS d{n}", literal(&tasks[i].id())))
        .collect();
    format!("SELECT {}", cols.join(", "))
}

pub(crate) async fn script(r: &(dyn Runner + Sync), id: &str) -> Result<String> {
    let (db, schema, name) = checked(id)?;
    let (tasks, at) = schema_tasks(r, &db, &schema, &name).await?;
    let at = at.ok_or_else(|| not_found(&db, &schema, &name))?;
    let order = order_of(&tasks, at);
    let ddl = r.rows(&ddl_sql(&tasks, &order)).await?;
    Ok(script_of(&tasks, &order, &ddl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        wh: Option<String>,
        /// Statement part → its answer (`Err`: the server refuses it).
        answers: Vec<(&'static str, std::result::Result<Set, &'static str>)>,
        seen: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(answers: Vec<(&'static str, std::result::Result<Set, &'static str>)>) -> Self {
            Fake {
                wh: Some("WH".into()),
                answers,
                seen: Mutex::new(Vec::new()),
            }
        }
        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[dbine_driver::async_trait]
    impl Runner for Fake {
        async fn rows(&self, sql: &str) -> Result<Set> {
            self.seen.lock().unwrap().push(sql.to_string());
            match self.answers.iter().find(|(n, _)| sql.contains(n)) {
                Some((_, Ok(s))) => Ok(s.clone()),
                Some((_, Err(m))) => Err(Error::Query(m.to_string())),
                None => Ok(Set::default()),
            }
        }
        fn warehouse(&self) -> Option<String> {
            self.wh.clone()
        }
        fn database(&self) -> Option<String> {
            Some("DB".into())
        }
    }

    fn set(cols: &[&str], rows: &[&[&str]]) -> Set {
        Set {
            cols: cols.iter().map(|c| c.to_string()).collect(),
            rows: rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|c| (!c.is_empty()).then(|| c.to_string()))
                        .collect()
                })
                .collect(),
        }
    }

    const SHOW_COLS: &[&str] = &[
        "created_on",
        "name",
        "id",
        "database_name",
        "schema_name",
        "owner",
        "comment",
        "warehouse",
        "schedule",
        "predecessors",
        "state",
        "definition",
        "condition",
        "allow_overlapping_execution",
        "task_relations",
    ];

    /// A graph ROOT → CHILD (+ FIN as finalizer), a standalone task with a
    /// quote in its name, an orphan child (its root isn't visible) and one
    /// in another database.
    fn shown() -> Set {
        set(
            SHOW_COLS,
            &[
                &[
                    "1731380400.000000000",
                    "ROOT",
                    "01",
                    "DB",
                    "ETL",
                    "SYSADMIN",
                    "carga diaria",
                    "WH",
                    "USING CRON 0 3 * * * UTC",
                    "[]",
                    "started",
                    "CALL load()",
                    "",
                    "false",
                    r#"{"Predecessors":[],"FinalizerTask":"DB.ETL.FIN"}"#,
                ],
                &[
                    "1731380400.000000000",
                    "CHILD",
                    "02",
                    "DB",
                    "ETL",
                    "SYSADMIN",
                    "",
                    "BIG_WH",
                    "",
                    "[\n  \"\\\"DB\\\".\\\"ETL\\\".\\\"ROOT\\\"\"\n]",
                    "started",
                    "INSERT INTO t SELECT 1",
                    "",
                    "false",
                    r#"{"Predecessors":["\"DB\".\"ETL\".\"ROOT\""]}"#,
                ],
                &[
                    "1731380400.000000000",
                    "FIN",
                    "03",
                    "DB",
                    "ETL",
                    "SYSADMIN",
                    "",
                    "",
                    "",
                    "[]",
                    "suspended",
                    "SELECT 1",
                    "",
                    "false",
                    r#"{"Predecessors":[],"FinalizedRootTask":"DB.ETL.ROOT"}"#,
                ],
                &[
                    "",
                    "it's \"x\"",
                    "04",
                    "DB",
                    "PUBLIC",
                    "ANA",
                    "",
                    "",
                    "",
                    "",
                    "suspended",
                    "SELECT 2",
                    "SYSTEM$STREAM_HAS_DATA('S')",
                    "",
                    "",
                ],
                &[
                    "",
                    "LOST",
                    "05",
                    "DB",
                    "PUBLIC",
                    "ANA",
                    "",
                    "WH",
                    "",
                    "[\"DB.PUBLIC.HIDDEN\"]",
                    "started",
                    "SELECT 3",
                    "",
                    "",
                    "",
                ],
                &[
                    "",
                    "OTHER",
                    "06",
                    "DB2",
                    "S",
                    "ANA",
                    "",
                    "WH",
                    "60 MINUTE",
                    "",
                    "suspended",
                    "SELECT 4",
                    "",
                    "",
                    "",
                ],
            ],
        )
    }

    fn warehouses(state: &str) -> Set {
        set(&["name", "state"], &[&["WH", state]])
    }

    const LATEST_COLS: &[&str] = &[
        "database_name",
        "schema_name",
        "name",
        "state",
        "graph_run_group_id",
        "run_id",
        "scheduled_ms",
        "started_ms",
        "completed_ms",
        "history_rows",
    ];

    /// The list's history: graph run g1 of ROOT, where ROOT succeeded and
    /// CHILD failed; an older g0 that went well; OTHER running now.
    fn history() -> Set {
        set(
            LATEST_COLS,
            &[
                &[
                    "DB",
                    "ETL",
                    "ROOT",
                    "SUCCEEDED",
                    "g1",
                    "1",
                    "1762916400000",
                    "1762916401000",
                    "1762916402000",
                    "7",
                ],
                &[
                    "DB",
                    "ETL",
                    "CHILD",
                    "FAILED",
                    "g1",
                    "1",
                    "1762916402000",
                    "1762916403000",
                    "1762916407000",
                    "7",
                ],
                &[
                    "DB",
                    "ETL",
                    "ROOT",
                    "SUCCEEDED",
                    "g0",
                    "0",
                    "1762830000000",
                    "1762830001000",
                    "1762830003000",
                    "7",
                ],
                &[
                    "DB",
                    "ETL",
                    "ROOT",
                    "SCHEDULED",
                    "",
                    "",
                    "1763002800000",
                    "",
                    "",
                    "7",
                ],
                &[
                    "DB2",
                    "S",
                    "OTHER",
                    "EXECUTING",
                    "g9",
                    "9",
                    "1762920000000",
                    "1762920001000",
                    "",
                    "2",
                ],
                &[
                    "DB2",
                    "S",
                    "OTHER",
                    "SCHEDULED",
                    "",
                    "",
                    "1762923600000",
                    "",
                    "",
                    "2",
                ],
            ],
        )
    }

    fn role(name: &str) -> Set {
        set(&["role"], &[&[name]])
    }

    #[test]
    fn ids_round_trip_and_are_checked() {
        let id = job_id("DB", "my \"s\"", "t.1");
        assert_eq!(id, r#""DB"."my ""s"""."t.1""#);
        assert_eq!(
            parse_id(&id),
            Some(("DB".into(), "my \"s\"".into(), "t.1".into()))
        );
        for bad in [
            "",
            "DB.S.T",
            r#""DB"."S""#,
            r#""DB"."S"."T"."X""#,
            r#""DB"."S"."T"; DROP TABLE x"#,
            r#""DB"."S".""#,
            r#""DB"."S"."T"#,
            "\"DB\".\"S\".\"T\nX\"",
            r#""DB"."S"."T"--"#,
            r#""DB" . "S"."T""#,
        ] {
            assert_eq!(parse_id(bad), None, "{bad}");
        }
        assert!(checked("x'); DROP TASK y; --").is_err());
    }

    #[test]
    fn literals_escape_quotes_and_backslashes() {
        assert_eq!(literal(r#"it's \ "x""#), r#"'it''s \\ "x"'"#);
    }

    #[test]
    fn predecessors_read_both_forms() {
        assert_eq!(
            predecessors(Some("[\n  \"\\\"DB\\\".\\\"S\\\".\\\"A.b\\\"\"\n]")),
            vec!["A.b"]
        );
        assert_eq!(predecessors(Some("DB.S.A, DB.S.B")), vec!["A", "B"]);
        assert!(predecessors(Some("[]")).is_empty());
        assert!(predecessors(None).is_empty());
    }

    #[test]
    fn show_rows_become_jobs() {
        let all = tasks(&shown());
        let (jobs, members, orphans) = jobs_of(&all);
        let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, ["ROOT", "it's \"x\"", "LOST", "OTHER"]);
        assert!(orphans);
        let graph: Vec<&str> = members[0].iter().map(|k| k.2.as_str()).collect();
        assert_eq!(graph, ["ROOT", "CHILD", "FIN"], "the root first");
        assert_eq!(members[0][0], ("DB".into(), "ETL".into(), "ROOT".into()));
        assert_eq!(members[2].len(), 1, "an orphan is alone");
        let root = &jobs[0];
        assert_eq!(root.id, r#""DB"."ETL"."ROOT""#);
        assert_eq!(root.kind, "Task");
        assert_eq!(root.group.as_deref(), Some("DB.ETL"));
        assert_eq!(root.description.as_deref(), Some("carga diaria"));
        assert_eq!(root.schedule.as_deref(), Some("USING CRON 0 3 * * * UTC"));
        assert!(root.enabled && root.can_run && root.can_stop && root.can_enable);
        assert!(root.has_history && root.has_script);
        // A triggered task: its condition is its schedule.
        let triggered = &jobs[1];
        assert_eq!(
            triggered.schedule.as_deref(),
            Some("WHEN SYSTEM$STREAM_HAS_DATA('S')")
        );
        assert!(!triggered.enabled && triggered.can_enable);
        assert_eq!(parse_id(&triggered.id).unwrap().2, "it's \"x\"");
        // The orphan child: listed, no actions.
        let lost = &jobs[2];
        assert!(!lost.can_run && !lost.can_stop && !lost.can_enable);
        assert!(lost.has_history && lost.has_script);
    }

    #[test]
    fn a_graph_runs_root_children_finalizer() {
        let all = tasks(&shown());
        let names: Vec<&str> = graph(&all, 0)
            .into_iter()
            .map(|i| all[i].name.as_str())
            .collect();
        assert_eq!(names, ["ROOT", "CHILD", "FIN"]);
    }

    #[tokio::test]
    async fn the_list_reads_the_history_with_a_running_warehouse() {
        let f = Fake::new(vec![
            ("SHOW TASKS IN ACCOUNT", Ok(shown())),
            ("CURRENT_ROLE", Ok(role("SYSADMIN"))),
            ("SHOW WAREHOUSES", Ok(warehouses("STARTED"))),
            ("TASK_HISTORY", Ok(history())),
        ]);
        let list = list(&f).await.unwrap();
        let seen = f.seen();
        let hist = seen.iter().find(|s| s.contains("TASK_HISTORY")).unwrap();
        assert!(hist.contains(r#""DB".INFORMATION_SCHEMA.TASK_HISTORY"#));
        assert!(hist.contains(r#""DB2".INFORMATION_SCHEMA.TASK_HISTORY"#));
        assert!(
            hist.contains("graph_run_group_id")
                && hist.contains("<= IFF(state = 'SCHEDULED', 1, 3)")
        );
        assert_eq!(
            list.notes,
            [
                "algunas tareas hijas se muestran sueltas y sin acciones: el rol no ve la tarea raíz de su grafo",
                "se ven solo las tareas sobre las que el rol SYSADMIN tiene algún privilegio",
            ]
        );
        // The root succeeded but its child failed in the same graph run: the
        // job failed, as the history says.
        let root = &list.jobs[0];
        assert_eq!(root.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(root.last_started.as_deref(), Some("2025-11-12T03:00:01Z"));
        assert_eq!(root.last_duration_ms, Some(6000), "until the child ended");
        assert_eq!(root.next_run.as_deref(), Some("2025-11-13T03:00:00Z"));
        assert!(!root.running && root.has_history);
        let other = &list.jobs[3];
        assert!(other.running);
        assert_eq!(other.last_outcome, Some(JobOutcome::Running));
        assert_eq!(other.last_started.as_deref(), Some("2025-11-12T04:00:01Z"));
        assert_eq!(other.next_run, None, "suspended: no next run");
    }

    #[tokio::test]
    async fn a_graph_runs_while_a_child_runs() {
        let mut h = history();
        // Graph run g2: the root already ended, its child goes on.
        for row in [
            [
                "DB",
                "ETL",
                "ROOT",
                "SUCCEEDED",
                "g2",
                "2",
                "1762920000000",
                "1762920001000",
                "1762920002000",
                "9",
            ],
            [
                "DB",
                "ETL",
                "CHILD",
                "EXECUTING",
                "g2",
                "2",
                "1762920002000",
                "1762920003000",
                "",
                "9",
            ],
        ] {
            h.rows.push(
                row.iter()
                    .map(|c| (!c.is_empty()).then(|| c.to_string()))
                    .collect(),
            );
        }
        let f = Fake::new(vec![
            ("SHOW TASKS IN ACCOUNT", Ok(shown())),
            ("SHOW WAREHOUSES", Ok(warehouses("STARTED"))),
            ("TASK_HISTORY", Ok(h)),
        ]);
        let list = list(&f).await.unwrap();
        let root = &list.jobs[0];
        assert!(
            root.running,
            "the child of the newest graph run is executing"
        );
        assert_eq!(root.last_started.as_deref(), Some("2025-11-12T04:00:01Z"));
        assert_eq!(
            root.last_outcome,
            Some(JobOutcome::Failed),
            "the last finished graph run"
        );
        assert_eq!(root.last_duration_ms, Some(6000));
    }

    #[test]
    fn a_retried_task_counts_by_its_last_attempt() {
        let row = |name: &str, outcome, started, completed| HistoryRow {
            name: name.into(),
            outcome,
            from: None,
            message: None,
            output: None,
            group: Some("g".into()),
            query_id: None,
            started: Some(started),
            completed: Some(completed),
        };
        let root = row("ROOT", JobOutcome::Succeeded, 0, 10);
        let failed = row("CHILD", JobOutcome::Failed, 10, 20);
        let retried = row("CHILD", JobOutcome::Succeeded, 100, 130);
        assert_eq!(
            fold(&root, &[&failed, &retried]),
            (JobOutcome::Succeeded, Some(130))
        );
        assert_eq!(fold(&root, &[&failed]), (JobOutcome::Failed, Some(20)));
        assert_eq!(fold(&root, &[]), (JobOutcome::Succeeded, Some(10)));
    }

    #[tokio::test]
    async fn a_database_that_fails_the_history_is_read_alone() {
        let mut h = history();
        h.rows[0][9] = Some("10000".into());
        let f = Fake::new(vec![
            ("SHOW TASKS IN ACCOUNT", Ok(shown())),
            ("CURRENT_ROLE", Ok(role("ACCOUNTADMIN"))),
            ("SHOW WAREHOUSES", Ok(warehouses("STARTED"))),
            (
                r#""DB2".INFORMATION_SCHEMA.TASK_HISTORY"#,
                Err("Database 'DB2' does not exist or not authorized."),
            ),
            ("TASK_HISTORY", Ok(h)),
        ]);
        let list = list(&f).await.unwrap();
        let reads = f
            .seen()
            .iter()
            .filter(|s| s.contains("TASK_HISTORY"))
            .count();
        assert_eq!(reads, 3, "all at once, then each database alone");
        assert_eq!(
            list.notes,
            [
                "algunas tareas hijas se muestran sueltas y sin acciones: el rol no ve la tarea raíz de su grafo",
                "no se pudo leer el historial de las tareas en DB2: Database 'DB2' does not exist or not authorized.",
                "en DB hay más de 10000 ejecuciones recientes: puede faltar la última ejecución de algunas tareas",
            ],
            "ACCOUNTADMIN sees every task"
        );
        assert_eq!(list.jobs[0].last_outcome, Some(JobOutcome::Failed));
    }

    #[tokio::test]
    async fn without_a_warehouse_there_is_no_history() {
        let mut f = Fake::new(vec![
            ("SHOW TASKS IN ACCOUNT", Ok(shown())),
            ("SHOW TASKS IN SCHEMA", Ok(shown())),
        ]);
        f.wh = None;
        let list = list(&f).await.unwrap();
        assert!(list.jobs.iter().all(|j| !j.has_history));
        assert!(list
            .notes
            .iter()
            .any(|n| n.starts_with("la conexión no tiene warehouse")));
        assert!(!f.seen().iter().any(|s| s.contains("TASK_HISTORY")));
        let d = detail(&f, r#""DB"."ETL"."ROOT""#).await.unwrap();
        assert!(!d.job.has_history);
        assert!(d
            .notes
            .iter()
            .any(|n| n.starts_with("la conexión no tiene warehouse")));
    }

    #[tokio::test]
    async fn a_suspended_warehouse_is_not_resumed_by_the_list() {
        let f = Fake::new(vec![
            ("SHOW TASKS IN ACCOUNT", Ok(shown())),
            ("SHOW WAREHOUSES", Ok(warehouses("SUSPENDED"))),
        ]);
        let list = list(&f).await.unwrap();
        assert!(!f.seen().iter().any(|s| s.contains("TASK_HISTORY")));
        assert!(list.notes.iter().any(|n| n.contains("WH está suspendido")));
        assert_eq!(list.jobs[0].last_outcome, None);
        // And the history refuses.
        let e = runs(&f, r#""DB"."ETL"."ROOT""#, 10).await.unwrap_err();
        assert!(e.to_string().contains("suspendido"), "{e}");
        // The detail says it in a note, and doesn't resume it either.
        let g = Fake::new(vec![
            ("SHOW TASKS IN SCHEMA \"DB\".\"ETL\"", Ok(shown())),
            ("SHOW WAREHOUSES", Ok(warehouses("SUSPENDED"))),
        ]);
        let d = detail(&g, r#""DB"."ETL"."ROOT""#).await.unwrap();
        assert!(!g.seen().iter().any(|s| s.contains("TASK_HISTORY")));
        assert_eq!(d.job.last_outcome, None);
        assert!(
            d.notes.iter().any(|n| n.contains("WH está suspendido")),
            "{:?}",
            d.notes
        );
    }

    #[tokio::test]
    async fn without_the_account_the_database_is_listed() {
        let f = Fake::new(vec![
            ("SHOW TASKS IN ACCOUNT", Err("Insufficient privileges")),
            ("SHOW TASKS IN DATABASE \"DB\"", Ok(set(SHOW_COLS, &[]))),
        ]);
        let list = list(&f).await.unwrap();
        assert!(list.jobs.is_empty());
        assert_eq!(list.notes.len(), 2, "{:?}", list.notes);
        assert!(list.notes[0].contains("solo las tareas de la base DB"));

        // Any other failure is the list's, not a fallback.
        let f = Fake::new(vec![(
            "SHOW TASKS IN ACCOUNT",
            Err("Statement reached its statement or warehouse timeout"),
        )]);
        assert!(super::list(&f).await.is_err());
        assert_eq!(f.seen().len(), 1);
        assert!(denied(&Error::Query(
            "SQL access control error: Insufficient privileges to operate on account 'X' (SQLSTATE 42501)".into()
        )));
        assert!(!denied(&Error::Connect("connection reset".into())));
    }

    #[tokio::test]
    async fn the_detail_has_the_graph_as_steps() {
        let f = Fake::new(vec![
            ("SHOW TASKS IN SCHEMA \"DB\".\"ETL\"", Ok(shown())),
            ("SHOW WAREHOUSES", Ok(warehouses("STARTED"))),
            ("TASK_HISTORY", Ok(history())),
        ]);
        let d = detail(&f, r#""DB"."ETL"."ROOT""#).await.unwrap();
        let steps: Vec<(&str, Option<&str>)> = d
            .steps
            .iter()
            .map(|s| (s.name.as_str(), s.target.as_deref()))
            .collect();
        assert_eq!(
            steps,
            [
                ("ROOT", None),
                ("CHILD", Some("BIG_WH")),
                ("FIN", Some("serverless"))
            ]
        );
        assert_eq!(d.steps[0].command.as_deref(), Some("CALL load()"));
        assert_eq!(d.job.last_outcome, Some(JobOutcome::Failed));
        let p = |l: &str| {
            d.properties
                .iter()
                .find(|p| p.label == l)
                .map(|p| p.value.as_str())
        };
        assert_eq!(p("Warehouse"), Some("WH"));
        assert_eq!(p("Ejecuciones superpuestas"), Some("No"));
        assert_eq!(p("Creada"), Some("2024-11-12T03:00:00Z"));
        assert_eq!(d.job.last_duration_ms, Some(6000), "the graph's run");
        assert!(d.job.has_history);
        assert_eq!(p("CHILD"), Some("started, después de ROOT"));
        assert_eq!(p("FIN"), Some("suspended, finalizador"));
        // What enabling a graph does is a note, not a property.
        assert_eq!(p("Habilitar y deshabilitar"), None);
        assert_eq!(d.notes, [GRAPH_ENABLE]);
        assert!(detail(&f, r#""DB"."ETL"."NOPE""#).await.is_err());
        assert!(detail(&f, "DB.ETL.ROOT").await.is_err());
    }

    #[tokio::test]
    async fn runs_group_the_graph_by_its_run() {
        let hist = set(
            &[
                "name",
                "state",
                "scheduled_from",
                "error_code",
                "error_message",
                "return_value",
                "query_id",
                "graph_run_group_id",
                "run_id",
                "scheduled_ms",
                "started_ms",
                "completed_ms",
            ],
            &[
                &[
                    "ROOT",
                    "SUCCEEDED",
                    "SCHEDULE",
                    "",
                    "",
                    "",
                    "q1",
                    "g1",
                    "1",
                    "1762916400000",
                    "1762916401000",
                    "1762916402000",
                ],
                &[
                    "CHILD",
                    "FAILED",
                    "SCHEDULE",
                    "100038",
                    "Numeric value 'x' is not recognized",
                    "",
                    "q2",
                    "g1",
                    "1",
                    "1762916402000",
                    "1762916403000",
                    "1762916405000",
                ],
                &[
                    "FIN",
                    "SUCCEEDED",
                    "SCHEDULE",
                    "",
                    "",
                    "",
                    "q3",
                    "g1",
                    "1",
                    "1762916405000",
                    "1762916406000",
                    "1762916407000",
                ],
                &[
                    "ROOT",
                    "EXECUTING",
                    "EXECUTE TASK",
                    "",
                    "",
                    "",
                    "q4",
                    "g2",
                    "2",
                    "1762920000000",
                    "1762920001000",
                    "",
                ],
                &[
                    "ROOT",
                    "SUCCEEDED",
                    "SCHEDULE",
                    "",
                    "",
                    "listo",
                    "q0",
                    "g0",
                    "0",
                    "1762830000000",
                    "1762830001000",
                    "1762830003000",
                ],
            ],
        );
        let f = Fake::new(vec![
            ("SHOW TASKS IN SCHEMA", Ok(shown())),
            ("SHOW WAREHOUSES", Ok(warehouses("STARTED"))),
            ("TASK_HISTORY", Ok(hist)),
        ]);
        let runs = runs(&f, r#""DB"."ETL"."ROOT""#, 2).await.unwrap();
        let sql = f
            .seen()
            .into_iter()
            .find(|s| s.contains("TASK_HISTORY"))
            .unwrap();
        for n in ["'ROOT'", "'CHILD'", "'FIN'"] {
            assert!(sql.contains(&format!("TASK_NAME => {n}")), "{sql}");
        }
        assert!(sql.contains("schema_name = 'ETL'") && sql.contains("RESULT_LIMIT => 5"));
        assert_eq!(runs.len(), 2, "newest first, at most the limit");
        assert_eq!(runs[0].outcome, JobOutcome::Running);
        assert_eq!(runs[0].trigger.as_deref(), Some("manual"));
        assert_eq!(runs[0].duration_ms, None);
        let g1 = &runs[1];
        assert_eq!(g1.id.as_deref(), Some("g1"));
        assert_eq!(g1.outcome, JobOutcome::Failed, "a child failed");
        assert_eq!(g1.trigger.as_deref(), Some("programada"));
        assert_eq!(g1.started.as_deref(), Some("2025-11-12T03:00:01Z"));
        assert_eq!(g1.duration_ms, Some(6000));
        assert_eq!(
            g1.message.as_deref(),
            Some("CHILD: Numeric value 'x' is not recognized (100038)")
        );
        let steps: Vec<(&str, JobOutcome)> = g1
            .steps
            .iter()
            .map(|s| (s.name.as_str(), s.outcome))
            .collect();
        assert_eq!(
            steps,
            [
                ("ROOT", JobOutcome::Succeeded),
                ("CHILD", JobOutcome::Failed),
                ("FIN", JobOutcome::Succeeded)
            ]
        );
        assert_eq!(g1.steps[1].duration_ms, Some(2000));
        assert!(runs_sql("DB", "S", &["x'y".into()], 1).contains("'x''y'"));
        assert!(super::runs(&f, r#""DB"."ETL"."ROOT""#, 0)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn actions_quote_the_parsed_id() {
        let f = Fake::new(vec![]);
        let id = r#""DB"."S"."it's ""x""""#;
        run(&f, id).await.unwrap();
        stop(&f, id).await.unwrap();
        set_enabled(&f, id, false).await.unwrap();
        set_enabled(&f, id, true).await.unwrap();
        assert_eq!(
            f.seen(),
            [
                r#"EXECUTE TASK "DB"."S"."it's ""x""""#,
                r#"SELECT SYSTEM$USER_TASK_CANCEL_ONGOING_EXECUTIONS('"DB"."S"."it''s ""x"""')"#,
                r#"ALTER TASK "DB"."S"."it's ""x""" SUSPEND"#,
                r#"ALTER TASK "DB"."S"."it's ""x""" RESUME"#,
            ]
        );
        for bad in ["x'); DROP TASK y; --", r#""DB"."S"."T" SUSPEND; DROP"#] {
            assert!(run(&f, bad).await.is_err());
            assert!(stop(&f, bad).await.is_err());
            assert!(set_enabled(&f, bad, true).await.is_err());
            assert!(script(&f, bad).await.is_err());
        }
        assert_eq!(f.seen().len(), 4, "nothing ran for a bad id");
    }

    #[tokio::test]
    async fn the_script_recreates_the_graph_and_resumes_it() {
        let ddl = set(
            &["d0", "d1", "d2"],
            &[&[
                "create or replace task DB.ETL.ROOT\n\twarehouse=WH\n\tschedule='USING CRON 0 3 * * * UTC'\n\tas CALL load();",
                "create or replace task DB.ETL.CHILD\n\twarehouse=BIG_WH\n\tafter DB.ETL.ROOT\n\tas INSERT INTO t SELECT 1;",
                "create or replace task DB.ETL.FIN\n\tfinalize=DB.ETL.ROOT\n\tas SELECT 1",
            ]],
        );
        let f = Fake::new(vec![
            ("SHOW TASKS IN SCHEMA", Ok(shown())),
            ("GET_DDL", Ok(ddl)),
        ]);
        let s = script(&f, r#""DB"."ETL"."ROOT""#).await.unwrap();
        let get_ddl = f
            .seen()
            .into_iter()
            .find(|s| s.contains("GET_DDL"))
            .unwrap();
        assert_eq!(
            get_ddl,
            r#"SELECT GET_DDL('TASK', '"DB"."ETL"."ROOT"', TRUE) AS d0, GET_DDL('TASK', '"DB"."ETL"."CHILD"', TRUE) AS d1, GET_DDL('TASK', '"DB"."ETL"."FIN"', TRUE) AS d2"#
        );
        let root = s.find("task DB.ETL.ROOT").unwrap();
        let child = s.find("task DB.ETL.CHILD").unwrap();
        let fin = s.find("task DB.ETL.FIN").unwrap();
        assert!(root < child && child < fin);
        assert!(s.contains("as SELECT 1;\n"), "a missing ; is added");
        let resume_child = s.find(r#"ALTER TASK "DB"."ETL"."CHILD" RESUME;"#).unwrap();
        let resume_root = s.find(r#"ALTER TASK "DB"."ETL"."ROOT" RESUME;"#).unwrap();
        assert!(fin < resume_child && resume_child < resume_root);
        assert!(
            !s.contains(r#""FIN" RESUME"#),
            "the finalizer was suspended"
        );
    }
}
