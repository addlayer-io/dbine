//! What EDB Postgres Advanced Server and KingbaseES share: the Oracle-style
//! views of their `DBMS_SCHEDULER` (`DBA_`/`ALL_`/`USER_SCHEDULER_JOBS`,
//! `…_PROGRAMS`, `…_SCHEDULES`, `…_JOB_RUN_DETAILS`) and the calls on one
//! job.
//!
//! Neither documents these views the same way across versions (EDB doesn't
//! list their columns; KingbaseES lists them from V9R4 on, many always
//! NULL), so their columns are looked up in the catalog first ([`Catalog`])
//! and a query reads only the ones a view has: a missing one reads as NULL.
//! Times are read as epoch seconds whatever the column's type: a
//! `timestamp` without zone is the session's local time (that's how a view
//! turns the scheduler's `timestamptz` into one), so it's read in the
//! session's `TimeZone`.
//!
//! Only the schemas each engine keeps them in are searched (`sys`,
//! `kdb_schedule`, the catalog): a user's view or table of the same name
//! elsewhere is never read.
//!
//! None of this ran against a real server (neither engine has a public
//! image): it follows the vendors' documentation.

use super::{ident, message, ms, prop, text, time, Row};
use crate::catalog::{comment_text, lit};
use crate::Variant;
use dbine_driver::{JobOutcome, JobRun, JobStep, PropertyInfo, ServerJob};
use std::collections::HashMap;

pub(super) const KIND: &str = "DBMS_SCHEDULER";

/// The engine, for what each one spells its own way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dialect {
    Edb,
    Kingbase,
}

impl Dialect {
    pub(super) fn variant(self) -> Variant {
        match self {
            Dialect::Edb => Variant::Edb,
            Dialect::Kingbase => Variant::Kingbase,
        }
    }

    /// The schemas its job views are in, the preferred first. Only these
    /// are searched: `sys.jobs` is a name a user's table can have too.
    fn schemas(self) -> &'static [&'static str] {
        match self {
            Dialect::Edb => &["sys"],
            // `kdb_job` moved from `kdb_schedule` to the catalog; the
            // extension itself is created in `sys`.
            Dialect::Kingbase => &["kdb_schedule", "pg_catalog", "sys_catalog", "sys"],
        }
    }

    /// A package procedure called as a statement of its own, the way each
    /// documents it: an anonymous block on EDB, `CALL` on KingbaseES.
    pub(super) fn call(self, proc: &str) -> String {
        match self {
            Dialect::Edb => format!("BEGIN {proc}; END;"),
            Dialect::Kingbase => format!("CALL {proc}"),
        }
    }

    pub(super) fn lit(self, s: &str) -> String {
        lit(self.variant(), s)
    }

    /// A `timestamptz` from its ISO 8601 text.
    pub(super) fn time_lit(self, iso: &str) -> String {
        format!("CAST({} AS TIMESTAMP WITH TIME ZONE)", self.lit(iso))
    }
}

// ---------------------------------------------------------------------------
// The views and their columns

/// How a column is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    TimeTz,
    /// `timestamp` or `date`: local time.
    Time,
    Interval,
    Other,
}

impl Kind {
    fn of(type_name: &str) -> Kind {
        match type_name {
            "timestamptz" => Kind::TimeTz,
            "timestamp" | "date" => Kind::Time,
            "interval" => Kind::Interval,
            _ => Kind::Other,
        }
    }
}

/// A view (or table) of the scheduler found in the catalog.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct View {
    pub schema: String,
    pub name: String,
    /// The user may read it.
    pub readable: bool,
    /// Lower-case name → the name as the catalog has it, and its kind.
    cols: HashMap<String, (String, Kind)>,
}

impl View {
    pub(super) fn from(&self) -> String {
        format!("{}.{}", ident(&self.schema), ident(&self.name))
    }

    pub(super) fn has(&self, col: &str) -> bool {
        self.cols.contains_key(col)
    }

    /// `a."Col"`, if the view has the column.
    pub(super) fn col(&self, a: &str, col: &str) -> Option<String> {
        self.cols
            .get(col)
            .map(|(name, _)| format!("{a}.{}", ident(name)))
    }

    /// The column as text: a time as epoch seconds (UTC), an interval as
    /// milliseconds; NULL when the view has no such column.
    pub(super) fn read(&self, a: &str, col: &str) -> String {
        let Some((name, kind)) = self.cols.get(col) else {
            return "NULL::text".into();
        };
        let x = format!("{a}.{}", ident(name));
        match kind {
            Kind::TimeTz => format!("extract(epoch FROM {x})::text"),
            Kind::Time => format!(
                "extract(epoch FROM ({x}::timestamp AT TIME ZONE current_setting('TimeZone')))::text"
            ),
            Kind::Interval => format!("(extract(epoch FROM {x}) * 1000)::text"),
            Kind::Other => format!("{x}::text"),
        }
    }

    /// The column as the server writes it.
    pub(super) fn raw(&self, a: &str, col: &str) -> String {
        self.col(a, col)
            .map(|x| format!("{x}::text"))
            .unwrap_or_else(|| "NULL::text".into())
    }

    /// A time column as ISO 8601 with its offset, whatever the session's
    /// `DateStyle` (for a script); NULL when missing or not a time.
    pub(super) fn iso(&self, a: &str, col: &str) -> String {
        match self.cols.get(col) {
            Some((name, Kind::TimeTz)) => format!("to_json({a}.{}) #>> '{{}}'", ident(name)),
            Some((name, Kind::Time)) => format!(
                "to_json({a}.{}::timestamp AT TIME ZONE current_setting('TimeZone')) #>> '{{}}'",
                ident(name)
            ),
            _ => "NULL::text".into(),
        }
    }
}

/// How much of the scheduler a view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    /// `DBA_*`: every job.
    All,
    /// `ALL_*`: the jobs the user has access to.
    Visible,
    /// `USER_*`: the user's own.
    Own,
}

/// The scheduler's views and tables in this database, by lower-case name.
#[derive(Debug, Clone, Default)]
pub(super) struct Catalog {
    views: HashMap<String, View>,
}

impl Catalog {
    pub(super) async fn read(
        s: &crate::session::PgSession,
        d: Dialect,
        names: &[&str],
    ) -> dbine_driver::Result<Catalog> {
        let rows = s.text(&catalog_sql(d, names)).await?;
        Ok(catalog_of(&super::refs(&rows)))
    }

    pub(super) fn get(&self, name: &str) -> Option<&View> {
        self.views.get(name)
    }

    pub(super) fn readable(&self, name: &str) -> Option<&View> {
        self.get(name).filter(|v| v.readable)
    }

    /// The widest readable of `dba_<x>`, `all_<x>` and `user_<x>`.
    pub(super) fn widest(&self, x: &str) -> Option<(&View, Scope)> {
        [
            ("dba", Scope::All),
            ("all", Scope::Visible),
            ("user", Scope::Own),
        ]
        .into_iter()
        .find_map(|(p, scope)| self.readable(&format!("{p}_{x}")).map(|v| (v, scope)))
    }

    /// Whether any of `dba_<x>`, `all_<x>` and `user_<x>` is there,
    /// readable or not.
    pub(super) fn any(&self, x: &str) -> bool {
        ["dba", "all", "user"]
            .iter()
            .any(|p| self.views.contains_key(&format!("{p}_{x}")))
    }

    #[cfg(test)]
    pub(super) fn with(views: Vec<View>) -> Catalog {
        Catalog {
            views: views.into_iter().map(|v| (v.name.clone(), v)).collect(),
        }
    }
}

#[cfg(test)]
impl View {
    /// A readable view with these columns.
    pub(super) fn fake(schema: &str, name: &str, cols: &[(&str, Kind)]) -> View {
        View {
            schema: schema.into(),
            name: name.into(),
            readable: true,
            cols: cols
                .iter()
                .map(|(c, k)| (c.to_string(), (c.to_string(), *k)))
                .collect(),
        }
    }

    /// The same view, there but not readable by the user.
    pub(super) fn hidden(self) -> View {
        View {
            readable: false,
            ..self
        }
    }
}

/// The columns of the views and tables named `names` (lower case) in the
/// engine's schemas, the preferred schema first. A domain reads as its
/// base type.
fn catalog_sql(d: Dialect, names: &[&str]) -> String {
    let list = |xs: &[&str]| {
        xs.iter()
            .map(|x| format!("'{x}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let order = d
        .schemas()
        .iter()
        .enumerate()
        .map(|(i, s)| format!("WHEN '{s}' THEN {i}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "SELECT lower(c.relname) AS key, n.nspname AS schema, c.relname AS rel,
                (has_schema_privilege(n.oid, 'USAGE') AND has_table_privilege(c.oid, 'SELECT')) AS readable,
                a.attname AS col, coalesce(b.typname, t.typname) AS type
         FROM pg_catalog.pg_class c
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
         JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
         LEFT JOIN pg_catalog.pg_type b ON t.typtype = 'd' AND b.oid = t.typbasetype
         WHERE c.relkind IN ('r', 'v', 'm', 'p', 'f')
           AND lower(c.relname) IN ({names})
           AND n.nspname IN ({schemas})
         ORDER BY CASE n.nspname {order} ELSE {last} END, n.nspname, c.relname, a.attnum",
        names = list(names),
        schemas = list(d.schemas()),
        last = d.schemas().len(),
    )
}

/// [`catalog_sql`]'s rows as views: the first schema that has a name wins.
fn catalog_of(rows: &[&dyn Row]) -> Catalog {
    let mut views: HashMap<String, View> = HashMap::new();
    for r in rows {
        let (Some(key), Some(schema), Some(rel), Some(col)) =
            (text(*r, "key"), r.get("schema"), r.get("rel"), r.get("col"))
        else {
            continue;
        };
        let v = views.entry(key).or_insert_with(|| View {
            schema: schema.clone(),
            name: rel.clone(),
            readable: super::flag(*r, "readable"),
            cols: HashMap::new(),
        });
        if v.schema != schema || v.name != rel {
            continue;
        }
        let kind = Kind::of(&text(*r, "type").unwrap_or_default());
        v.cols.entry(col.to_lowercase()).or_insert((col, kind));
    }
    Catalog { views }
}

/// A yes/no cell however the engine writes it (`t`, `true`, `TRUE`, `Y`…).
pub(super) fn yes(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "t" | "true" | "y" | "yes" | "1"
    )
}

// ---------------------------------------------------------------------------
// The jobs

/// The columns of `*_SCHEDULER_JOBS` DBine reads (Oracle's names; each
/// engine has some of them).
const JOB_COLUMNS: &[&str] = &[
    "owner",
    "job_name",
    "job_type",
    "job_action",
    "program_name",
    "schedule_name",
    "start_date",
    "repeat_interval",
    "end_date",
    "job_class",
    "enabled",
    "state",
    "run_count",
    "failure_count",
    "retry_count",
    "last_start_date",
    "last_run_duration",
    "next_run_date",
    "comments",
];

/// A job's columns from view `v` (aliased `a`) as `v_<column>`, plus the
/// ISO text of its start, end and next run for the script.
pub(super) fn job_select(v: &View, a: &str) -> Vec<String> {
    let mut out: Vec<String> = JOB_COLUMNS
        .iter()
        .map(|c| match *c {
            // Text as the server has it: KingbaseES writes `enabled` as text.
            "enabled" | "state" | "job_type" => format!("{} AS v_{c}", v.raw(a, c)),
            _ => format!("{} AS v_{c}", v.read(a, c)),
        })
        .collect();
    out.push(format!("{} AS v_start_iso", v.iso(a, "start_date")));
    out.push(format!("{} AS v_end_iso", v.iso(a, "end_date")));
    out.push(format!("{} AS v_next_iso", v.iso(a, "next_run_date")));
    out
}

/// The named schedule of each job (`*_SCHEDULER_SCHEDULES`), as `s_*`
/// columns and the join that reads them; NULLs without that view.
pub(super) fn schedule_join(
    jobs: &View,
    a: &str,
    schedules: Option<&View>,
) -> (Vec<String>, String) {
    let cols = [
        "s_interval",
        "s_start",
        "s_start_iso",
        "s_end_iso",
        "s_comments",
    ];
    let (Some(s), Some(job_schedule)) = (schedules, jobs.col(a, "schedule_name")) else {
        return (
            cols.iter().map(|c| format!("NULL::text AS {c}")).collect(),
            String::new(),
        );
    };
    let Some(name) = s.col("s", "schedule_name") else {
        return schedule_join(jobs, a, None);
    };
    (
        cols.iter().map(|c| format!("sc.{c}")).collect(),
        format!(
            "LEFT JOIN LATERAL (SELECT {} AS s_interval, {} AS s_start, {} AS s_start_iso, {} AS s_end_iso,
                                       {} AS s_comments
                                FROM {} s WHERE {name} = {job_schedule} LIMIT 1) sc ON true",
            s.read("s", "repeat_interval"),
            s.read("s", "start_date"),
            s.iso("s", "start_date"),
            s.iso("s", "end_date"),
            s.read("s", "comments"),
            s.from(),
        ),
    )
}

/// One row per job from view `v`: the one with the latest start where a
/// view repeats a job (KingbaseES has a row per action state).
pub(super) fn jobs_sql(
    v: &View,
    schedules: Option<&View>,
    filter: Option<String>,
) -> Option<String> {
    let name = v.col("v", "job_name")?;
    let keys = match v.col("v", "owner") {
        Some(owner) => format!("{owner}, {name}"),
        None => name,
    };
    let mut cols = job_select(v, "v");
    let (sched, join) = schedule_join(v, "v", schedules);
    cols.extend(sched);
    cols.push("extract(epoch FROM now())::text AS now".into());
    let latest = v
        .col("v", "last_start_date")
        .map(|c| format!(", {c} DESC NULLS LAST"))
        .unwrap_or_default();
    Some(format!(
        "SELECT DISTINCT ON ({keys}) {cols}
         FROM {from} v {join}{filter}
         ORDER BY {keys}{latest}",
        cols = cols.join(", "),
        from = v.from(),
        filter = filter.map(|f| format!(" WHERE {f}")).unwrap_or_default(),
    ))
}

/// `v.job_name = …` (and its owner, where the view has owners and the id
/// names one), for one job.
pub(super) fn job_filter(
    d: Dialect,
    v: &View,
    a: &str,
    owner: Option<&str>,
    name: &str,
) -> Option<String> {
    let mut f = format!("{} = {}", v.col(a, "job_name")?, d.lit(name));
    if let (Some(col), Some(owner)) = (v.col(a, "owner"), owner) {
        f.push_str(&format!(" AND {col} = {}", d.lit(owner)));
    }
    Some(f)
}

/// A scheduler job as its view says it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct SchedJob {
    pub owner: Option<String>,
    pub name: String,
    pub job_type: Option<String>,
    pub action: Option<String>,
    pub program: Option<String>,
    pub schedule_name: Option<String>,
    pub start: Option<String>,
    pub start_iso: Option<String>,
    pub interval: Option<String>,
    pub end: Option<String>,
    pub end_iso: Option<String>,
    pub class: Option<String>,
    pub enabled: Option<bool>,
    pub state: Option<String>,
    pub run_count: Option<String>,
    pub failure_count: Option<String>,
    pub retry_count: Option<String>,
    pub last_start: Option<String>,
    pub last_ms: Option<u64>,
    pub next_run: Option<String>,
    pub next_iso: Option<String>,
    pub comments: Option<String>,
    /// The named schedule's comment.
    pub schedule_comments: Option<String>,
}

impl SchedJob {
    /// [`jobs_sql`]'s row; `None` without a name.
    pub(super) fn from_row(r: &dyn Row) -> Option<SchedJob> {
        Some(SchedJob {
            owner: text(r, "v_owner"),
            name: text(r, "v_job_name")?,
            job_type: text(r, "v_job_type"),
            action: text(r, "v_job_action"),
            program: text(r, "v_program_name"),
            schedule_name: text(r, "v_schedule_name"),
            start: time(r, "v_start_date").or_else(|| time(r, "s_start")),
            start_iso: text(r, "v_start_iso").or_else(|| text(r, "s_start_iso")),
            interval: text(r, "v_repeat_interval").or_else(|| text(r, "s_interval")),
            end: time(r, "v_end_date"),
            end_iso: text(r, "v_end_iso").or_else(|| text(r, "s_end_iso")),
            class: text(r, "v_job_class"),
            enabled: text(r, "v_enabled").map(|e| yes(&e)),
            state: text(r, "v_state"),
            run_count: text(r, "v_run_count"),
            failure_count: text(r, "v_failure_count"),
            retry_count: text(r, "v_retry_count"),
            last_start: time(r, "v_last_start_date"),
            last_ms: ms(r, "v_last_run_duration"),
            next_run: time(r, "v_next_run_date"),
            next_iso: text(r, "v_next_iso"),
            comments: text(r, "v_comments"),
            schedule_comments: text(r, "s_comments"),
        })
    }

    fn state_is(&self, s: &str) -> bool {
        self.state
            .as_deref()
            .is_some_and(|st| st.trim().eq_ignore_ascii_case(s))
    }

    /// Its `enabled` column, else whether its state isn't `DISABLED`.
    pub(super) fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or_else(|| !self.state_is("disabled"))
    }

    pub(super) fn running(&self) -> bool {
        self.state_is("running")
    }

    /// The last run's outcome as its state tells it, once it ran.
    fn last_outcome(&self) -> Option<JobOutcome> {
        if self.running() {
            return Some(JobOutcome::Running);
        }
        self.last_start.as_ref()?;
        self.state.as_deref().and_then(state_outcome)
    }

    /// The job as the list shows it. The caller sets its id and what the
    /// user may do with it.
    pub(super) fn server_job(&self) -> ServerJob {
        let enabled = self.is_enabled();
        ServerJob {
            id: String::new(),
            name: self.name.clone(),
            kind: KIND.into(),
            group: self.owner.clone(),
            description: self.comments.clone(),
            owner: self.owner.clone(),
            enabled,
            schedule: self.interval.clone(),
            running: self.running(),
            last_outcome: self.last_outcome(),
            last_started: self.last_start.clone(),
            last_duration_ms: self.last_ms,
            next_run: self.next_run.clone().filter(|_| enabled),
            ..Default::default()
        }
    }

    /// What it runs: its action, or its program's.
    pub(super) fn steps(&self, program: Option<&Program>) -> Vec<JobStep> {
        let command = self
            .action
            .clone()
            .or_else(|| program.and_then(|p| p.action.clone()));
        if command.is_none() && program.is_none() {
            return Vec::new();
        }
        vec![JobStep {
            name: self.program.clone().unwrap_or_else(|| "Acción".into()),
            kind: self
                .job_type
                .as_deref()
                .or(program.and_then(|p| p.kind.as_deref()))
                .map(step_kind),
            target: None,
            command,
        }]
    }

    /// The rest its view says about it.
    pub(super) fn properties(&self) -> Vec<PropertyInfo> {
        let mut out = Vec::new();
        let mut add = |label: &str, value: &Option<String>| {
            if let Some(v) = value {
                out.push(prop("", label, v.clone()));
            }
        };
        add("Propietario", &self.owner);
        add("Clase", &self.class);
        add("Programa", &self.program);
        add("Programación", &self.schedule_name);
        add("Inicio", &self.start);
        add("Fin", &self.end);
        add("Estado", &self.state);
        add("Ejecuciones", &self.run_count);
        add("Fallas", &self.failure_count);
        add("Reintentos", &self.retry_count);
        out
    }
}

/// A job's state as an outcome, when it tells one.
fn state_outcome(state: &str) -> Option<JobOutcome> {
    match state.trim().to_ascii_lowercase().as_str() {
        "running" => Some(JobOutcome::Running),
        "succeeded" | "success" => Some(JobOutcome::Succeeded),
        // "internal failure" is pgAgent's word (kdb_schedule descends from
        // it) for a run the agent couldn't start: nothing retries it.
        "failed" | "broken" | "internal failure" => Some(JobOutcome::Failed),
        "retry scheduled" => Some(JobOutcome::Retrying),
        "aborted" | "stopped" => Some(JobOutcome::Canceled),
        _ => None,
    }
}

/// A run's status (Oracle's words, or KingbaseES's lower-case ones).
fn run_outcome(status: Option<&str>) -> JobOutcome {
    status
        .and_then(state_outcome)
        .unwrap_or(JobOutcome::Unknown)
}

/// The engine's program or job type in words.
pub(super) fn step_kind(t: &str) -> String {
    let t = t.trim();
    match t.to_ascii_uppercase().as_str() {
        "PLSQL_BLOCK" => "Bloque PL/SQL",
        "STORED_PROCEDURE" | "PROCEDURE" => "Procedimiento",
        "SQL" => "SQL",
        "SQL_SCRIPT" => "Script SQL",
        "BATCH" | "EXECUTABLE" => "Comando del sistema",
        "EXTERNAL_SCRIPT" => "Script externo",
        "BACKUP_SCRIPT" => "Script de respaldo",
        _ => return t.to_string(),
    }
    .into()
}

// ---------------------------------------------------------------------------
// Programs

/// A program (`*_SCHEDULER_PROGRAMS`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Program {
    pub name: String,
    pub kind: Option<String>,
    pub action: Option<String>,
    pub enabled: Option<bool>,
    pub comments: Option<String>,
}

pub(super) fn program_sql(d: Dialect, v: &View, name: &str) -> Option<String> {
    let col = v.col("p", "program_name")?;
    Some(format!(
        "SELECT {col}::text AS name, {} AS kind, {} AS action, {} AS enabled, {} AS comments
         FROM {} p WHERE {col} = {} LIMIT 1",
        v.raw("p", "program_type"),
        v.read("p", "program_action"),
        v.raw("p", "enabled"),
        v.read("p", "comments"),
        v.from(),
        d.lit(name),
    ))
}

impl Program {
    pub(super) fn from_row(r: &dyn Row) -> Option<Program> {
        Some(Program {
            name: text(r, "name")?,
            kind: text(r, "kind"),
            action: text(r, "action"),
            enabled: text(r, "enabled").map(|e| yes(&e)),
            comments: text(r, "comments"),
        })
    }
}

// ---------------------------------------------------------------------------
// Runs

/// The newest `limit` runs of a job from `*_SCHEDULER_JOB_RUN_DETAILS`.
pub(super) fn runs_sql(
    d: Dialect,
    v: &View,
    owner: Option<&str>,
    name: &str,
    limit: u32,
) -> Option<String> {
    let filter = job_filter(d, v, "r", owner, name)?;
    let order = ["log_id", "log_date", "actual_start_date"]
        .iter()
        .find_map(|c| v.col("r", c))
        .map(|c| format!(" ORDER BY {c} DESC NULLS LAST"))
        .unwrap_or_default();
    Some(format!(
        "SELECT {} AS id, {} AS status, {} AS started, {} AS requested, {} AS logged, {} AS ms,
                {} AS errors, {} AS info, {} AS output
         FROM {} r WHERE {filter}{order} LIMIT {limit}",
        v.read("r", "log_id"),
        v.raw("r", "status"),
        v.read("r", "actual_start_date"),
        v.read("r", "req_start_date"),
        v.read("r", "log_date"),
        v.read("r", "run_duration"),
        v.read("r", "errors"),
        v.read("r", "additional_info"),
        v.read("r", "output"),
        v.from(),
    ))
}

pub(super) fn run_of(r: &dyn Row) -> JobRun {
    JobRun {
        id: text(r, "id"),
        started: time(r, "started")
            .or_else(|| time(r, "requested"))
            .or_else(|| time(r, "logged")),
        duration_ms: ms(r, "ms"),
        outcome: run_outcome(text(r, "status").as_deref()),
        trigger: None,
        message: message(r, "errors")
            .or_else(|| message(r, "info"))
            .or_else(|| message(r, "output")),
        steps: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The calls

/// Why neither engine offers "Ejecutar ahora" on a DBMS_SCHEDULER job:
/// `RUN_JOB` only runs it in the calling session (both refuse
/// `use_current_session => FALSE`), so the call would hold DBine's session
/// until the run ends, and cancelling it would abort the run. Queueing a
/// one-shot DBMS_JOB that calls `RUN_JOB` would return at once, but whether
/// such a job is removed after it runs, and isn't retried when it fails,
/// couldn't be checked on a real server of either engine.
pub(super) const RUN_WAITS: &str = "«Ejecutar ahora» no está disponible para los jobs de DBMS_SCHEDULER: RUN_JOB los ejecuta en la sesión de DBine hasta que terminan.";

pub(super) fn run_waits() -> dbine_driver::Error {
    dbine_driver::Error::Unsupported(RUN_WAITS.trim_end_matches('.').into())
}

pub(super) fn enable_sql(d: Dialect, name: &str, enabled: bool) -> String {
    let proc = if enabled { "ENABLE" } else { "DISABLE" };
    d.call(&format!("DBMS_SCHEDULER.{proc}({})", d.lit(name)))
}

/// `TRUE` / `FALSE`.
pub(super) fn bool_lit(b: bool) -> &'static str {
    if b {
        "TRUE"
    } else {
        "FALSE"
    }
}

/// A value or `NULL`.
fn or_null(d: Dialect, v: Option<&str>) -> String {
    v.map(|v| d.lit(v)).unwrap_or_else(|| "NULL".into())
}

fn time_or_null(d: Dialect, iso: Option<&str>) -> String {
    iso.map(|t| d.time_lit(t)).unwrap_or_else(|| "NULL".into())
}

/// EDB's `CREATE_JOB`: with its program and schedule when it has both,
/// else with its own action and timing. `None` when neither is known.
pub(super) fn edb_script(j: &SchedJob) -> Option<String> {
    let d = Dialect::Edb;
    let mut out = format!(
        "-- EDB Postgres Advanced Server: {} ({KIND})\n",
        comment_text(&j.name)
    );
    if let Some(owner) = &j.owner {
        out.push_str(&format!(
            "-- Se crea a nombre del usuario que lo ejecute (el original es de «{}»).\n",
            comment_text(owner)
        ));
    }
    let mut args = vec![format!("job_name => {}", d.lit(&j.name))];
    match (&j.program, &j.schedule_name) {
        (Some(program), Some(schedule)) => {
            out.push_str(&format!(
                "-- Usa el programa «{}» y la programación «{}», que tienen que existir.\n",
                comment_text(program),
                comment_text(schedule)
            ));
            args.push(format!("program_name => {}", d.lit(program)));
            args.push(format!("schedule_name => {}", d.lit(schedule)));
        }
        _ => {
            let (Some(kind), Some(action)) = (&j.job_type, &j.action) else {
                return None;
            };
            args.push(format!("job_type => {}", d.lit(kind)));
            args.push(format!("job_action => {}", d.lit(action)));
            args.push(format!(
                "start_date => {}",
                time_or_null(d, j.start_iso.as_deref())
            ));
            args.push(format!(
                "repeat_interval => {}",
                or_null(d, j.interval.as_deref())
            ));
            args.push(format!(
                "end_date => {}",
                time_or_null(d, j.end_iso.as_deref())
            ));
        }
    }
    args.push(format!("enabled => {}", bool_lit(j.is_enabled())));
    args.push(format!("comments => {}", or_null(d, j.comments.as_deref())));
    out.push_str(&format!(
        "BEGIN\n  DBMS_SCHEDULER.CREATE_JOB(\n    {});\nEND;\n",
        args.join(",\n    ")
    ));
    Some(out)
}

/// KingbaseES's `CREATE_JOB` only takes a program and a schedule: the
/// script creates both too. A program's connection string isn't copied (it
/// may carry a password). `None` without a program and a schedule.
pub(super) fn kingbase_script(
    j: &SchedJob,
    program: Option<&Program>,
    database: &str,
) -> Option<String> {
    let d = Dialect::Kingbase;
    let (Some(program_name), Some(schedule)) = (&j.program, &j.schedule_name) else {
        return None;
    };
    let action = program
        .and_then(|p| p.action.clone())
        .or_else(|| j.action.clone())?;
    // The views write a program's type as SQL or BATCH.
    let kind = match program
        .and_then(|p| p.kind.as_deref())
        .or(j.job_type.as_deref())
        .map(|k| k.trim().to_ascii_uppercase())
        .as_deref()
    {
        None | Some("SQL") => "PLSQL_BLOCK".to_string(),
        Some("BATCH") => "EXECUTABLE".to_string(),
        Some(other) => other.to_string(),
    };
    if j.interval.is_none() && j.start_iso.is_none() {
        return None;
    }
    let mut out = format!(
        "-- KingbaseES: {} ({KIND})\n\
         -- Crea también su programa y su programación: si ya existen, omití esas dos llamadas.\n\
         -- La cadena de conexión del programa no se copia (puede llevar una contraseña): completala si la usa.\n\
         -- El programa corre en la base «{}»: cambiala si corría en otra.\n",
        comment_text(&j.name),
        comment_text(database)
    );
    out.push_str(&format!(
        "CALL DBMS_SCHEDULER.CREATE_PROGRAM({}, {}, {}, NULL, {}, 0, {}, {});\n",
        d.lit(program_name),
        d.lit(&kind),
        d.lit(&action),
        d.lit(database),
        bool_lit(program.and_then(|p| p.enabled).unwrap_or(true)),
        or_null(d, program.and_then(|p| p.comments.as_deref())),
    ));
    out.push_str(&format!(
        "CALL DBMS_SCHEDULER.CREATE_SCHEDULE({}, {}, {}, {}, {});\n",
        d.lit(schedule),
        time_or_null(d, j.start_iso.as_deref()),
        or_null(d, j.interval.as_deref()),
        time_or_null(d, j.end_iso.as_deref()),
        or_null(d, j.schedule_comments.as_deref()),
    ));
    out.push_str(&format!(
        "CALL DBMS_SCHEDULER.CREATE_JOB({}, {}, {}, {}, {}, TRUE, {});\n",
        d.lit(&j.name),
        d.lit(program_name),
        d.lit(schedule),
        d.lit(j.class.as_deref().unwrap_or("Routine Maintenance")),
        bool_lit(j.is_enabled()),
        or_null(d, j.comments.as_deref()),
    ));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::*;

    #[test]
    fn the_catalog_keeps_the_preferred_schema_and_reads_types() {
        let rows = [
            fixture(&[
                ("key", "dba_scheduler_jobs"),
                ("schema", "kdb_schedule"),
                ("rel", "dba_scheduler_jobs"),
                ("readable", "t"),
                ("col", "job_name"),
                ("type", "varchar"),
            ]),
            fixture(&[
                ("key", "dba_scheduler_jobs"),
                ("schema", "kdb_schedule"),
                ("rel", "dba_scheduler_jobs"),
                ("readable", "t"),
                ("col", "State"),
                ("type", "text"),
            ]),
            fixture(&[
                ("key", "dba_scheduler_jobs"),
                ("schema", "kdb_schedule"),
                ("rel", "dba_scheduler_jobs"),
                ("readable", "t"),
                ("col", "next_run_date"),
                ("type", "timestamp"),
            ]),
            // The same name in a schema further down the list.
            fixture(&[
                ("key", "dba_scheduler_jobs"),
                ("schema", "sys"),
                ("rel", "DBA_SCHEDULER_JOBS"),
                ("readable", "f"),
                ("col", "other"),
                ("type", "text"),
            ]),
            fixture(&[
                ("key", "kdb_job"),
                ("schema", "pg_catalog"),
                ("rel", "kdb_job"),
                ("readable", "f"),
                ("col", "jobnextrun"),
                ("type", "timestamptz"),
            ]),
        ];
        let rows: Vec<&dyn Row> = rows.iter().map(|r| r as &dyn Row).collect();
        let cat = catalog_of(&rows);
        let v = cat.get("dba_scheduler_jobs").unwrap();
        assert_eq!(v.schema, "kdb_schedule");
        assert!(v.readable && v.has("job_name") && v.has("state") && !v.has("other"));
        // Mixed-case columns keep their name, quoted.
        assert_eq!(v.raw("v", "state"), "v.\"State\"::text");
        assert_eq!(
            v.read("v", "next_run_date"),
            "extract(epoch FROM (v.\"next_run_date\"::timestamp AT TIME ZONE current_setting('TimeZone')))::text"
        );
        assert_eq!(v.read("v", "missing"), "NULL::text");
        assert_eq!(
            cat.widest("scheduler_jobs")
                .map(|(v, s)| (v.name.as_str(), s)),
            Some(("dba_scheduler_jobs", Scope::All))
        );
        assert!(cat.get("kdb_job").is_some() && cat.readable("kdb_job").is_none());
        let k = cat.get("kdb_job").unwrap();
        assert_eq!(
            k.read("k", "jobnextrun"),
            "extract(epoch FROM k.\"jobnextrun\")::text"
        );
        assert_eq!(
            k.iso("k", "jobnextrun"),
            "to_json(k.\"jobnextrun\") #>> '{}'"
        );
    }

    #[test]
    fn only_the_engines_schemas_are_searched() {
        let sql = catalog_sql(Dialect::Kingbase, &["kdb_job", "all_scheduler_jobs"]);
        assert!(sql.contains("lower(c.relname) IN ('kdb_job', 'all_scheduler_jobs')"));
        assert!(
            sql.contains("AND n.nspname IN ('kdb_schedule', 'pg_catalog', 'sys_catalog', 'sys')")
        );
        assert!(sql.contains("WHEN 'kdb_schedule' THEN 0"));
        let edb = catalog_sql(Dialect::Edb, &["dba_jobs", "jobs"]);
        assert!(edb.contains("AND n.nspname IN ('sys')"));
        assert!(!edb.contains("pg_extension"));
    }

    #[test]
    fn the_widest_view_wins() {
        let cat = Catalog::with(vec![
            View {
                readable: false,
                ..View::fake("sys", "dba_scheduler_jobs", &[])
            },
            View::fake("sys", "all_scheduler_jobs", &[]),
            View::fake("sys", "user_scheduler_jobs", &[]),
        ]);
        assert_eq!(
            cat.widest("scheduler_jobs")
                .map(|(v, s)| (v.name.as_str(), s)),
            Some(("all_scheduler_jobs", Scope::Visible))
        );
        assert!(cat.any("scheduler_jobs") && !cat.any("scheduler_programs"));
    }

    fn jobs_view() -> View {
        View::fake(
            "sys",
            "dba_scheduler_jobs",
            &[
                ("owner", Kind::Other),
                ("job_name", Kind::Other),
                ("job_type", Kind::Other),
                ("job_action", Kind::Other),
                ("schedule_name", Kind::Other),
                ("start_date", Kind::TimeTz),
                ("repeat_interval", Kind::Other),
                ("enabled", Kind::Other),
                ("state", Kind::Other),
                ("last_start_date", Kind::TimeTz),
                ("last_run_duration", Kind::Interval),
                ("next_run_date", Kind::TimeTz),
            ],
        )
    }

    #[test]
    fn the_jobs_query_reads_what_the_view_has() {
        let v = jobs_view();
        let sched = View::fake(
            "sys",
            "dba_scheduler_schedules",
            &[
                ("schedule_name", Kind::Other),
                ("repeat_interval", Kind::Other),
            ],
        );
        let sql = jobs_sql(
            &v,
            Some(&sched),
            job_filter(Dialect::Edb, &v, "v", Some("enterprisedb"), "it's"),
        )
        .unwrap();
        assert!(sql.starts_with("SELECT DISTINCT ON (v.\"owner\", v.\"job_name\")"));
        assert!(sql.contains("FROM \"sys\".\"dba_scheduler_jobs\" v LEFT JOIN LATERAL"));
        assert!(sql.contains("WHERE s.\"schedule_name\" = v.\"schedule_name\" LIMIT 1) sc ON true"));
        assert!(sql.contains("WHERE v.\"job_name\" = E'it''s' AND v.\"owner\" = E'enterprisedb'"));
        assert!(sql.contains(
            "ORDER BY v.\"owner\", v.\"job_name\", v.\"last_start_date\" DESC NULLS LAST"
        ));
        assert!(sql.contains(
            "(extract(epoch FROM v.\"last_run_duration\") * 1000)::text AS v_last_run_duration"
        ));
        assert!(sql.contains("NULL::text AS v_comments"));
        assert!(sql.contains("v.\"enabled\"::text AS v_enabled"));
        // Without a schedules view, its columns are NULL.
        let alone = jobs_sql(&v, None, None).unwrap();
        assert!(alone.contains("NULL::text AS s_interval") && !alone.contains("LATERAL"));
    }

    fn row() -> super::super::Fixture {
        fixture(&[
            ("v_owner", "enterprisedb"),
            ("v_job_name", "nightly"),
            ("v_job_type", "PLSQL_BLOCK"),
            ("v_job_action", "BEGIN purge; END;"),
            ("v_repeat_interval", "FREQ=DAILY;BYHOUR=3"),
            ("v_enabled", "TRUE"),
            ("v_state", "SCHEDULED"),
            ("v_last_start_date", "1794488199"),
            ("v_last_run_duration", "1500.4"),
            ("v_next_run_date", "1794533465"),
            ("v_start_iso", "2026-11-01T03:00:00+00:00"),
            ("v_comments", "purges"),
        ])
    }

    #[test]
    fn a_row_maps_to_a_job() {
        let j = SchedJob::from_row(&row()).unwrap();
        let s = j.server_job();
        assert_eq!(s.name, "nightly");
        assert_eq!(s.kind, "DBMS_SCHEDULER");
        assert_eq!(s.group.as_deref(), Some("enterprisedb"));
        assert_eq!(s.schedule.as_deref(), Some("FREQ=DAILY;BYHOUR=3"));
        assert!(s.enabled && !s.running);
        assert_eq!(s.last_started.as_deref(), Some("2026-11-12T12:56:39Z"));
        assert_eq!(s.last_duration_ms, Some(1500));
        assert_eq!(s.next_run.as_deref(), Some("2026-11-13T01:31:05Z"));
        // SCHEDULED doesn't say how the last run went.
        assert_eq!(s.last_outcome, None);
        let failed = SchedJob {
            state: Some("failed".into()),
            ..j.clone()
        };
        assert_eq!(failed.server_job().last_outcome, Some(JobOutcome::Failed));
        let running = SchedJob {
            state: Some("RUNNING".into()),
            ..j.clone()
        };
        assert!(running.server_job().running);
        // No `enabled` column: a DISABLED state.
        let off = SchedJob {
            enabled: None,
            state: Some("DISABLED".into()),
            ..j.clone()
        };
        assert!(!off.is_enabled() && off.server_job().next_run.is_none());
        let steps = j.steps(None);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "Acción");
        assert_eq!(steps[0].kind.as_deref(), Some("Bloque PL/SQL"));
        assert_eq!(steps[0].command.as_deref(), Some("BEGIN purge; END;"));
        let labels: Vec<_> = j.properties().iter().map(|p| p.label.clone()).collect();
        assert!(
            labels.contains(&"Propietario".to_string()) && labels.contains(&"Estado".to_string())
        );
    }

    #[test]
    fn a_named_schedule_fills_in_the_timing() {
        let r = fixture(&[
            ("v_job_name", "j"),
            ("v_schedule_name", "nightly"),
            ("s_interval", "FREQ=DAILY"),
            ("s_start", "0"),
            ("s_start_iso", "1970-01-01T00:00:00+00:00"),
        ]);
        let j = SchedJob::from_row(&r).unwrap();
        assert_eq!(j.interval.as_deref(), Some("FREQ=DAILY"));
        assert_eq!(j.start.as_deref(), Some("1970-01-01T00:00:00Z"));
        assert!(j.is_enabled());
    }

    #[test]
    fn runs_read_the_run_details_view() {
        let v = View::fake(
            "kdb_schedule",
            "all_scheduler_job_run_details",
            &[
                ("log_id", Kind::Other),
                ("job_name", Kind::Other),
                ("owner", Kind::Other),
                ("status", Kind::Other),
                ("actual_start_date", Kind::Time),
                ("run_duration", Kind::Interval),
                ("output", Kind::Other),
            ],
        );
        let sql = runs_sql(Dialect::Kingbase, &v, None, "job1", 20).unwrap();
        assert!(sql.contains(
            "WHERE r.\"job_name\" = E'job1' ORDER BY r.\"log_id\" DESC NULLS LAST LIMIT 20"
        ));
        assert!(!sql.contains("r.\"owner\" ="));
        let run = run_of(&fixture(&[
            ("id", "7"),
            ("status", "internal failure"),
            ("started", "1794488199"),
            ("ms", "250"),
            ("output", "ERROR: relation does not exist"),
        ]));
        // pgAgent's word for a run it couldn't start: a failure.
        assert_eq!(run.outcome, JobOutcome::Failed);
        assert_eq!(run.started.as_deref(), Some("2026-11-12T12:56:39Z"));
        assert_eq!(run.duration_ms, Some(250));
        assert_eq!(
            run.message.as_deref(),
            Some("ERROR: relation does not exist")
        );
        for (status, outcome) in [
            ("SUCCEEDED", JobOutcome::Succeeded),
            ("success", JobOutcome::Succeeded),
            ("FAILED", JobOutcome::Failed),
            ("aborted", JobOutcome::Canceled),
            ("STOPPED", JobOutcome::Canceled),
            ("retry scheduled", JobOutcome::Retrying),
            ("running", JobOutcome::Running),
            ("weird", JobOutcome::Unknown),
        ] {
            assert_eq!(run_outcome(Some(status)), outcome, "{status}");
        }
    }

    #[test]
    fn calls_quote_the_name_as_each_engine_takes_it() {
        // RUN_JOB would hold the session to the end: never called.
        assert!(matches!(run_waits(), dbine_driver::Error::Unsupported(m) if !m.ends_with('.')));
        assert_eq!(
            enable_sql(Dialect::Kingbase, "a\\b", false),
            "CALL DBMS_SCHEDULER.DISABLE(E'a\\\\b')"
        );
        assert_eq!(
            enable_sql(Dialect::Edb, "j", true),
            "BEGIN DBMS_SCHEDULER.ENABLE(E'j'); END;"
        );
    }

    #[test]
    fn edbs_script_creates_the_job_again() {
        let j = SchedJob::from_row(&row()).unwrap();
        let sql = edb_script(&j).unwrap();
        assert!(sql.contains("job_name => E'nightly'"), "{sql}");
        assert!(sql.contains("job_type => E'PLSQL_BLOCK'"));
        assert!(sql.contains("job_action => E'BEGIN purge; END;'"));
        assert!(sql.contains(
            "start_date => CAST(E'2026-11-01T03:00:00+00:00' AS TIMESTAMP WITH TIME ZONE)"
        ));
        assert!(sql.contains("end_date => NULL"));
        assert!(sql.contains("enabled => TRUE"));
        assert!(sql.ends_with(");\nEND;\n"));
        let named = SchedJob {
            program: Some("p".into()),
            schedule_name: Some("s\nDROP".into()),
            ..j.clone()
        };
        let sql = edb_script(&named).unwrap();
        assert!(sql.contains("program_name => E'p'") && !sql.contains("job_action"));
        // A name can't end the comment line.
        assert!(sql.contains("«s?DROP»"));
        let bare = SchedJob { action: None, ..j };
        assert!(edb_script(&bare).is_none());
    }

    #[test]
    fn kingbases_script_creates_program_schedule_and_job() {
        let j = SchedJob {
            name: "job1".into(),
            program: Some("program1".into()),
            schedule_name: Some("schedule1".into()),
            interval: Some("Freq=daily;BYHOUR=10".into()),
            start_iso: Some("2026-11-01T10:00:00+08:00".into()),
            enabled: Some(false),
            ..Default::default()
        };
        let p = Program {
            name: "program1".into(),
            kind: Some("SQL".into()),
            action: Some("create table t2(A int)".into()),
            enabled: Some(true),
            comments: None,
        };
        let sql = kingbase_script(&j, Some(&p), "test").unwrap();
        assert!(sql.contains("CALL DBMS_SCHEDULER.CREATE_PROGRAM(E'program1', E'PLSQL_BLOCK', E'create table t2(A int)', NULL, E'test', 0, TRUE, NULL);"), "{sql}");
        assert!(sql.contains("CALL DBMS_SCHEDULER.CREATE_SCHEDULE(E'schedule1', CAST(E'2026-11-01T10:00:00+08:00' AS TIMESTAMP WITH TIME ZONE), E'Freq=daily;BYHOUR=10', NULL, NULL);"));
        assert!(sql.contains("CALL DBMS_SCHEDULER.CREATE_JOB(E'job1', E'program1', E'schedule1', E'Routine Maintenance', FALSE, TRUE, NULL);"));
        assert!(!sql.contains("password"));
        assert!(kingbase_script(&SchedJob { program: None, ..j }, Some(&p), "test").is_none());
    }

    #[test]
    fn step_kinds_are_words() {
        assert_eq!(step_kind("STORED_PROCEDURE"), "Procedimiento");
        assert_eq!(step_kind("batch"), "Comando del sistema");
        assert_eq!(step_kind(" CHAIN "), "CHAIN");
        assert!(yes("TRUE") && yes("t") && yes("Y") && !yes("FALSE") && !yes("N"));
    }
}
