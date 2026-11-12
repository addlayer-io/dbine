//! pg_cron: `cron.job`, and its runs in `cron.job_run_details` (1.3+);
//! and Apache Cloudberry's tasks (`CREATE TASK`), which are pg_cron built
//! into the server ([`Flavor::Task`]).
//!
//! pg_cron has no "run now" and no way to stop a run; a job is enabled or
//! disabled with `cron.alter_job` (1.4+), a task with `ALTER TASK … ACTIVE`,
//! which only reaches the current user's tasks (it finds them by name and
//! user). pg_cron's tables have row-level security: without superuser or
//! `BYPASSRLS` each user sees only the jobs they scheduled.

use super::{
    dollar, flag, ident, message, ms, not_found, prop, refs, text, time, Found, JobId, Part, Row,
    HISTORY_LIMIT,
};
use crate::catalog::{comment_text, lit};
use crate::session::PgSession;
use dbine_driver::{Error, JobDetail, JobOutcome, JobRun, JobStep, Result, ServerJob};
use std::collections::HashMap;
use tokio_postgres::SimpleQueryRow;

/// pg_cron itself, or Apache Cloudberry's tasks: the same columns in
/// `pg_catalog.pg_task` and `pg_task_run_history`, changed with `ALTER
/// TASK` and created with `CREATE TASK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flavor {
    Cron,
    Task,
}

impl Flavor {
    fn jobs(self) -> &'static str {
        match self {
            Flavor::Cron => "cron.job",
            Flavor::Task => "pg_catalog.pg_task",
        }
    }

    fn history(self) -> &'static str {
        match self {
            Flavor::Cron => "cron.job_run_details",
            Flavor::Task => "pg_catalog.pg_task_run_history",
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Flavor::Cron => "pg_cron",
            Flavor::Task => "Task",
        }
    }

    fn id(self, n: i64) -> JobId {
        match self {
            Flavor::Cron => JobId::Cron(n),
            Flavor::Task => JobId::Task(n),
        }
    }

    fn unreadable(self) -> &'static str {
        match self {
            Flavor::Cron => {
                "Tu usuario no puede leer los jobs de pg_cron: necesita USAGE en el esquema cron."
            }
            Flavor::Task => "Tu usuario no puede leer las tareas del servidor (pg_task).",
        }
    }

    /// The note when the last runs couldn't be read.
    fn last_runs_failed(self, e: &Error) -> String {
        match self {
            Flavor::Cron => {
                format!("No se pudo leer la última ejecución de los jobs de pg_cron: {e}.")
            }
            Flavor::Task => format!("No se pudo leer la última ejecución de las tareas: {e}."),
        }
    }
}

/// What the installed pg_cron has, and what the user may read of it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Features {
    /// `USAGE` on the schema and `SELECT` on `cron.job`.
    readable: bool,
    /// `cron.job.jobname` (1.3+).
    name: bool,
    /// `cron.job.active` (1.3+).
    active: bool,
    /// `cron.job_run_details` (1.3+), readable.
    runs: bool,
    /// `cron.alter_job` (1.4+).
    alter: bool,
    /// The user may run `cron.alter_job`: pg_cron grants it to its owner
    /// only.
    may_alter: bool,
    /// `cron.schedule_in_database` (1.4+), which takes the user too.
    in_database: bool,
    /// `cron.log_run` isn't off: runs are recorded.
    log_run: bool,
}

/// What pg_cron (or the tasks) has here. Settings come from `pg_settings`:
/// pg_cron's are superuser-only, and `current_setting` would fail for
/// anyone else.
fn features_sql(flavor: Flavor) -> String {
    if flavor == Flavor::Task {
        return "SELECT has_table_privilege('pg_catalog.pg_task', 'SELECT') AS readable,
                       true AS has_name, true AS has_active,
                       has_table_privilege('pg_catalog.pg_task_run_history', 'SELECT') AS runs,
                       true AS alter_job, true AS may_alter, false AS in_database,
                       coalesce((SELECT setting FROM pg_catalog.pg_settings WHERE name = 'task_log_run'), 'on') AS log_run"
            .into();
    }
    let rel = |name: &str| {
        format!(
            "(SELECT has_table_privilege(c.oid, 'SELECT') FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'cron' AND c.relname = '{name}')"
        )
    };
    let col = |name: &str| {
        format!(
            "EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = 'cron' AND c.relname = 'job' AND a.attname = '{name}' AND NOT a.attisdropped)"
        )
    };
    let func = |name: &str, and: &str| {
        format!(
            "EXISTS (SELECT 1 FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
                     WHERE n.nspname = 'cron' AND p.proname = '{name}'{and})"
        )
    };
    format!(
        "SELECT has_schema_privilege('cron', 'USAGE') AND coalesce({job}, false) AS readable,
                {name} AS has_name, {active} AS has_active, coalesce({runs}, false) AS runs,
                {alter} AS alter_job, {may_alter} AS may_alter, {in_database} AS in_database,
                coalesce((SELECT setting FROM pg_catalog.pg_settings WHERE name = 'cron.log_run'), 'on') AS log_run",
        job = rel("job"),
        name = col("jobname"),
        active = col("active"),
        runs = rel("job_run_details"),
        alter = func("alter_job", ""),
        may_alter = func(
            "alter_job",
            " AND has_function_privilege(p.oid, 'EXECUTE')"
        ),
        in_database = func("schedule_in_database", ""),
    )
}

fn features_of(r: &dyn Row) -> Features {
    Features {
        readable: flag(r, "readable"),
        name: flag(r, "has_name"),
        active: flag(r, "has_active"),
        runs: flag(r, "runs"),
        alter: flag(r, "alter_job"),
        may_alter: flag(r, "may_alter"),
        in_database: flag(r, "in_database"),
        log_run: text(r, "log_run").as_deref() != Some("off"),
    }
}

async fn features(s: &PgSession, flavor: Flavor) -> Result<Features> {
    let rows = s.text(&features_sql(flavor)).await?;
    Ok(rows.first().map(|r| features_of(r)).unwrap_or_default())
}

/// pg_cron (or the tasks) is here and readable: what it has.
async fn ready(s: &PgSession, flavor: Flavor) -> Result<(Found, Features)> {
    let f = Found::detect(s).await?;
    if flavor == Flavor::Task && !f.task {
        return Err(Error::Query(
            "este servidor no tiene tareas (CREATE TASK)".into(),
        ));
    }
    if flavor == Flavor::Cron && !f.cron {
        return Err(Error::Query(match &f.cron_database {
            Some(db) if *db != f.database => {
                format!("pg_cron no está en esta base: sus jobs están en la base «{db}»")
            }
            _ => "pg_cron no está instalado en esta base".into(),
        }));
    }
    let feat = features(s, flavor).await?;
    if !feat.readable {
        return Err(Error::Query(flavor.unreadable().into()));
    }
    Ok((f, feat))
}

/// The jobs (or the one with `id`), with the columns this version has.
fn jobs_sql(flavor: Flavor, feat: Features, id: Option<i64>) -> String {
    let name = if feat.name { "jobname" } else { "NULL::text" };
    let active = if feat.active { "active" } else { "true" };
    let filter = id
        .map(|n| format!(" WHERE jobid = {n}"))
        .unwrap_or_default();
    format!(
        "SELECT jobid::text AS id, {name} AS jobname, schedule, command, nodename, nodeport::text AS nodeport,
                database, username, {active} AS active
         FROM {}{filter} ORDER BY jobid",
        flavor.jobs()
    )
}

/// The last finished run of each job and the start of the one going on.
#[derive(Debug, Default, Clone, PartialEq)]
struct Last {
    running_since: Option<String>,
    outcome: Option<JobOutcome>,
    started: Option<String>,
    duration_ms: Option<u64>,
}

/// The two latest runs of each job (or of the one with `id`), newest first.
/// pg_cron doesn't run a job again while it's still running, so they are
/// the run going on, if any, and the last one that ended. One look-up per
/// job down the history's primary key (`runid`): the table has no index on
/// `jobid` and pg_cron never purges it.
fn last_sql(flavor: Flavor, id: Option<i64>) -> String {
    let filter = id
        .map(|n| format!(" WHERE j.jobid = {n}"))
        .unwrap_or_default();
    format!(
        "SELECT j.jobid::text AS id, l.status, extract(epoch FROM l.start_time)::text AS started,
                (extract(epoch FROM l.end_time - l.start_time) * 1000)::text AS ms
         FROM {jobs} j
         CROSS JOIN LATERAL (SELECT d.runid, d.status, d.start_time, d.end_time FROM {history} d
                             WHERE d.jobid = j.jobid ORDER BY d.runid DESC LIMIT 2) l{filter}
         ORDER BY j.jobid, l.runid DESC",
        jobs = flavor.jobs(),
        history = flavor.history(),
    )
}

/// [`last_sql`]'s rows as each job's [`Last`], by id.
fn last_of(rows: &[&dyn Row]) -> HashMap<String, Last> {
    let mut out: HashMap<String, Last> = HashMap::new();
    for r in rows {
        let Some(id) = text(*r, "id") else { continue };
        let l = out.entry(id).or_default();
        match outcome(text(*r, "status").as_deref()) {
            // Only the newest run can be the one going on.
            JobOutcome::Running if l.outcome.is_none() && l.running_since.is_none() => {
                l.running_since = time(*r, "started").or_else(|| Some(String::new()));
            }
            o @ (JobOutcome::Succeeded | JobOutcome::Failed) if l.outcome.is_none() => {
                l.outcome = Some(o);
                l.started = time(*r, "started");
                l.duration_ms = ms(*r, "ms");
            }
            _ => {}
        }
    }
    out
}

/// Each job's [`Last`], by id, within [`HISTORY_LIMIT`].
async fn last_runs(
    s: &PgSession,
    flavor: Flavor,
    id: Option<i64>,
) -> Result<HashMap<String, Last>> {
    let rows = s.text_within(&last_sql(flavor, id), HISTORY_LIMIT).await?;
    Ok(last_of(&refs(&rows)))
}

fn outcome(status: Option<&str>) -> JobOutcome {
    match status {
        Some("succeeded") => JobOutcome::Succeeded,
        Some("failed") => JobOutcome::Failed,
        Some("starting" | "running" | "sending" | "connecting") => JobOutcome::Running,
        _ => JobOutcome::Unknown,
    }
}

fn job(
    flavor: Flavor,
    r: &dyn Row,
    f: &Found,
    feat: Features,
    last: &HashMap<String, Last>,
) -> Option<ServerJob> {
    let n: i64 = text(r, "id")?.parse().ok()?;
    let user = text(r, "username");
    let l = last.get(&n.to_string()).cloned().unwrap_or_default();
    let running = l.running_since.is_some();
    Some(ServerJob {
        id: flavor.id(n).to_string(),
        name: text(r, "jobname").unwrap_or_else(|| format!("job {n}")),
        kind: flavor.kind().into(),
        group: text(r, "database"),
        description: None,
        owner: user.clone(),
        enabled: flag(r, "active"),
        schedule: text(r, "schedule"),
        running,
        last_outcome: l.outcome.or(running.then_some(JobOutcome::Running)),
        last_started: l.running_since.filter(|t| !t.is_empty()).or(l.started),
        last_duration_ms: l.duration_ms,
        // Neither says when a job runs next.
        next_run: None,
        can_run: false,
        can_stop: false,
        can_enable: may_change(flavor, f, feat, user.as_deref()),
        has_history: feat.runs,
        has_script: true,
        system: false,
    })
}

/// A user changes their own jobs, given `EXECUTE` on `cron.alter_job`; a
/// superuser, anyone's pg_cron job (`cron.alter_job` takes the id).
/// `ALTER TASK` finds the task by name among the current user's, so only
/// its owner changes a task.
fn may_change(flavor: Flavor, f: &Found, feat: Features, owner: Option<&str>) -> bool {
    let mine = owner == Some(f.user.as_str());
    match flavor {
        Flavor::Cron => feat.may_alter && (f.superuser || mine),
        Flavor::Task => feat.alter && mine,
    }
}

pub(super) async fn list(s: &PgSession, f: &Found, flavor: Flavor) -> Result<Part> {
    let feat = features(s, flavor).await?;
    let mut part = Part::default();
    if !feat.readable {
        part.notes.push(flavor.unreadable().into());
        return Ok(part);
    }
    let rows = s.text(&jobs_sql(flavor, feat, None)).await?;
    let last = if feat.runs {
        match last_runs(s, flavor, None).await {
            Ok(last) => last,
            Err(e) => {
                part.notes.push(flavor.last_runs_failed(&e));
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };
    part.jobs = rows
        .iter()
        .filter_map(|r| job(flavor, r, f, feat, &last))
        .collect();
    if flavor == Flavor::Cron && !f.sees_all {
        part.notes.push(format!(
            "pg_cron muestra a cada usuario solo sus jobs: se ven los de «{}».",
            f.user
        ));
    }
    if feat.runs && !feat.log_run {
        part.notes.push(
            match flavor {
                Flavor::Cron => "pg_cron no guarda el historial de ejecuciones en este servidor (cron.log_run está en off).",
                Flavor::Task => "El servidor no guarda el historial de ejecuciones de sus tareas (task_log_run está en off).",
            }
            .into(),
        );
    }
    Ok(part)
}

async fn row(s: &PgSession, flavor: Flavor, feat: Features, n: i64) -> Result<SimpleQueryRow> {
    s.text(&jobs_sql(flavor, feat, Some(n)))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found(flavor.id(n)))
}

pub(super) async fn detail(s: &PgSession, flavor: Flavor, n: i64) -> Result<JobDetail> {
    let (f, feat) = ready(s, flavor).await?;
    let r = row(s, flavor, feat, n).await?;
    let last = if feat.runs {
        last_runs(s, flavor, Some(n)).await.unwrap_or_else(|e| {
            tracing::debug!("{}: last run of {n}: {e}", flavor.kind());
            HashMap::new()
        })
    } else {
        HashMap::new()
    };
    let job = job(flavor, &r, &f, feat, &last).ok_or_else(|| not_found(flavor.id(n)))?;
    let mut properties = vec![prop("", "Id", n.to_string())];
    if let Some(db) = text(&r, "database") {
        properties.push(prop("", "Base de datos", db));
    }
    if let Some(u) = text(&r, "username") {
        properties.push(prop("", "Usuario", u));
    }
    if let (Some(host), Some(port)) = (text(&r, "nodename"), text(&r, "nodeport")) {
        properties.push(prop("", "Servidor", format!("{host}:{port}")));
    }
    Ok(JobDetail {
        job,
        steps: vec![JobStep {
            name: "Comando".into(),
            kind: Some("SQL".into()),
            target: None,
            command: text(&r, "command"),
        }],
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

fn runs_sql(flavor: Flavor, n: i64, limit: u32) -> String {
    format!(
        "SELECT runid::text AS id, status, return_message AS message,
                extract(epoch FROM start_time)::text AS started,
                (extract(epoch FROM end_time - start_time) * 1000)::text AS ms
         FROM {} WHERE jobid = {n} ORDER BY runid DESC LIMIT {limit}",
        flavor.history()
    )
}

fn run_of(r: &dyn Row) -> JobRun {
    JobRun {
        id: text(r, "id"),
        started: time(r, "started"),
        duration_ms: ms(r, "ms"),
        outcome: outcome(text(r, "status").as_deref()),
        // pg_cron (and a task) only runs on its schedule.
        trigger: Some("programada".into()),
        message: message(r, "message"),
        steps: Vec::new(),
    }
}

pub(super) async fn runs(s: &PgSession, flavor: Flavor, n: i64, limit: u32) -> Result<Vec<JobRun>> {
    let (_, feat) = ready(s, flavor).await?;
    if !feat.runs {
        return Err(Error::Unsupported(
            "este pg_cron no guarda el historial de ejecuciones (cron.job_run_details)".into(),
        ));
    }
    row(s, flavor, feat, n).await?;
    Ok(s.text(&runs_sql(flavor, n, limit))
        .await?
        .iter()
        .map(|r| run_of(r))
        .collect())
}

pub(super) async fn set_enabled(
    s: &PgSession,
    flavor: Flavor,
    n: i64,
    enabled: bool,
) -> Result<()> {
    let (f, feat) = ready(s, flavor).await?;
    if flavor == Flavor::Task {
        let r = row(s, flavor, feat, n).await?;
        let name = text(&r, "jobname").ok_or_else(|| not_found(flavor.id(n)))?;
        let owner = text(&r, "username");
        if !may_change(flavor, &f, feat, owner.as_deref()) {
            // ALTER TASK would reach the current user's task of that name.
            return Err(Error::Query(format!(
                "la tarea es de «{}»: solo ese usuario puede habilitarla o deshabilitarla",
                owner.unwrap_or_default()
            )));
        }
        let state = if enabled { "ACTIVE" } else { "NOT ACTIVE" };
        s.text(&format!("ALTER TASK {} {state}", ident(&name)))
            .await?;
        return Ok(());
    }
    if !feat.alter {
        return Err(Error::Unsupported(
            "esta versión de pg_cron no permite habilitar o deshabilitar jobs: cron.alter_job llegó en la 1.4"
                .into(),
        ));
    }
    if !feat.may_alter {
        return Err(Error::Query(
            "tu usuario no puede ejecutar cron.alter_job: pg_cron solo se lo permite a su dueño, salvo que se le dé EXECUTE".into(),
        ));
    }
    row(s, flavor, feat, n).await?;
    s.text(&format!(
        "SELECT cron.alter_job(job_id := {n}, active := {enabled})"
    ))
    .await?;
    Ok(())
}

/// The statement that schedules the job again as it is: `cron.schedule`;
/// for what it can't say (another database, another user, inactive),
/// `cron.schedule_in_database` for a named job and `cron.alter_job` around
/// `cron.schedule` for an unnamed one. A named `cron.schedule` replaces
/// the current user's job of that name: another user's job scheduled
/// with it would overwrite the runner's own and then hand it over, while
/// `schedule_in_database` replaces the job of the user it's given.
fn script_of(v: crate::Variant, r: &dyn Row, f: &Found, feat: Features) -> String {
    let n = text(r, "id").unwrap_or_default();
    let name = text(r, "jobname");
    let schedule = cell_or_empty(r, "schedule");
    let command = cell_or_empty(r, "command");
    let database = text(r, "database").unwrap_or_else(|| f.database.clone());
    let user = text(r, "username").unwrap_or_else(|| f.user.clone());
    let active = flag(r, "active");
    let base = match &name {
        Some(name) => format!(
            "cron.schedule({}, {}, {})",
            lit(v, name),
            lit(v, &schedule),
            dollar(&command)
        ),
        None => format!("cron.schedule({}, {})", lit(v, &schedule), dollar(&command)),
    };
    let mut extra = Vec::new();
    let mut unsaid = Vec::new();
    if database != f.database {
        extra.push(format!("database := {}", lit(v, &database)));
        unsaid.push(format!(
            "se ejecuta en la base «{}»",
            comment_text(&database)
        ));
    }
    // Who it runs as, named for schedule_in_database even when it's the
    // current user (that function replaces the job of the user it's given).
    let as_user = format!("username := {}", lit(v, &user));
    if user != f.user {
        extra.push(as_user.clone());
        unsaid.push(format!("se ejecuta como «{}»", comment_text(&user)));
    }
    if !active {
        extra.push("active := false".into());
        unsaid.push("está inactivo".into());
    }
    let mut out = format!(
        "-- pg_cron: {}\n-- Se crea en la base de pg_cron («{}»).\n",
        comment_text(name.as_deref().unwrap_or(&format!("job {n}"))),
        comment_text(&f.database)
    );
    if extra.is_empty() {
        out.push_str(&format!("SELECT {base};\n"));
    } else if let (Some(name), true) = (&name, feat.in_database) {
        let mut args = vec![
            lit(v, name),
            lit(v, &schedule),
            dollar(&command),
            lit(v, &database),
            as_user,
        ];
        if !active {
            args.push("active := false".into());
        }
        out.push_str(&format!(
            "SELECT cron.schedule_in_database({});\n",
            args.join(", ")
        ));
    } else if feat.alter {
        out.push_str(&format!(
            "SELECT cron.alter_job({base}, {});\n",
            extra.join(", ")
        ));
    } else {
        out.push_str(&format!(
            "-- Este pg_cron no permite fijarlo al crearlo, pero el job {}.\nSELECT {base};\n",
            unsaid.join(", ")
        ));
    }
    out
}

/// A cell as it is (spaces kept), empty when null.
fn cell_or_empty(r: &dyn Row, name: &str) -> String {
    r.get(name).unwrap_or_default()
}

/// `CREATE TASK`, with its database and user, and `ALTER TASK … NOT
/// ACTIVE` when it's inactive. `ALTER TASK` only finds the current user's
/// tasks: for another user's task it runs as that user (`SET LOCAL ROLE`
/// in a transaction), or it would reach the runner's own task of that
/// name. The `CREATE` stays the runner's: `USER` names the owner, and the
/// owner may lack the privileges on the database it takes.
fn task_script(v: crate::Variant, r: &dyn Row, f: &Found) -> String {
    let name = cell_or_empty(r, "jobname");
    let owner = text(r, "username");
    let mut out = format!(
        "-- Tarea: {}\nCREATE TASK {} SCHEDULE {}",
        comment_text(&name),
        ident(&name),
        lit(v, &cell_or_empty(r, "schedule"))
    );
    if let Some(db) = text(r, "database") {
        out.push_str(&format!(" DATABASE {}", ident(&db)));
    }
    if let Some(user) = &owner {
        out.push_str(&format!(" USER {}", ident(user)));
    }
    out.push_str(&format!(" AS {};\n", dollar(&cell_or_empty(r, "command"))));
    if !flag(r, "active") {
        let alter = format!("ALTER TASK {} NOT ACTIVE;\n", ident(&name));
        match owner.filter(|o| *o != f.user) {
            Some(other) => out.push_str(&format!(
                "BEGIN;\nSET LOCAL ROLE {};\n{alter}COMMIT;\n",
                ident(&other)
            )),
            None => out.push_str(&alter),
        }
    }
    out
}

pub(super) async fn script(s: &PgSession, flavor: Flavor, n: i64) -> Result<String> {
    let (f, feat) = ready(s, flavor).await?;
    let r = row(s, flavor, feat, n).await?;
    Ok(match flavor {
        Flavor::Cron => script_of(s.variant, &r, &f, feat),
        Flavor::Task => task_script(s.variant, &r, &f),
    })
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::*;
    use crate::Variant;

    fn found(user: &str, superuser: bool) -> Found {
        super::super::tests::found_for(user, superuser)
    }

    const ALL: Features = Features {
        readable: true,
        name: true,
        active: true,
        runs: true,
        alter: true,
        may_alter: true,
        in_database: true,
        log_run: true,
    };

    fn vacuum(database: &str, user: &str, active: &str) -> super::super::Fixture {
        fixture(&[
            ("id", "12"),
            ("jobname", "dbine_test_jobs_vacuum"),
            ("schedule", "0 3 * * *"),
            ("command", "VACUUM ANALYZE"),
            ("database", database),
            ("username", user),
            ("active", active),
            ("nodename", "localhost"),
            ("nodeport", "5432"),
        ])
    }

    #[test]
    fn a_job_row_maps_to_a_server_job() {
        let mut last = HashMap::new();
        last.insert(
            "12".to_string(),
            Last {
                outcome: Some(JobOutcome::Failed),
                started: Some("2026-11-12T03:00:00Z".into()),
                duration_ms: Some(1234),
                ..Default::default()
            },
        );
        let j = job(
            Flavor::Cron,
            &vacuum("app", "postgres", "t"),
            &found("postgres", true),
            ALL,
            &last,
        )
        .unwrap();
        assert_eq!(j.id, "pg_cron:12");
        assert_eq!(j.name, "dbine_test_jobs_vacuum");
        assert_eq!(j.kind, "pg_cron");
        assert_eq!(j.group.as_deref(), Some("app"));
        assert_eq!(j.owner.as_deref(), Some("postgres"));
        assert_eq!(j.schedule.as_deref(), Some("0 3 * * *"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T03:00:00Z"));
        assert_eq!(j.last_duration_ms, Some(1234));
        assert!(j.enabled && j.can_enable && j.has_history && j.has_script);
        assert!(!j.can_run && !j.can_stop && !j.running && j.next_run.is_none());
        // A run going on, none finished before.
        let mut going = HashMap::new();
        going.insert(
            "12".to_string(),
            Last {
                running_since: Some("2026-11-12T04:00:00Z".into()),
                ..Default::default()
            },
        );
        let j = job(
            Flavor::Cron,
            &vacuum("app", "postgres", "t"),
            &found("postgres", true),
            ALL,
            &going,
        )
        .unwrap();
        assert!(j.running);
        assert_eq!(j.last_outcome, Some(JobOutcome::Running));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T04:00:00Z"));
        // Another user's job (seen with BYPASSRLS): not theirs to change.
        let j = job(
            Flavor::Cron,
            &vacuum("app", "bob", "f"),
            &found("alice", false),
            ALL,
            &HashMap::new(),
        )
        .unwrap();
        assert!(!j.enabled && !j.can_enable);
        // Without EXECUTE on cron.alter_job, not even their own.
        let no_execute = Features {
            may_alter: false,
            ..ALL
        };
        assert!(
            !job(
                Flavor::Cron,
                &vacuum("app", "alice", "t"),
                &found("alice", false),
                no_execute,
                &HashMap::new()
            )
            .unwrap()
            .can_enable
        );
        assert!(
            job(
                Flavor::Cron,
                &vacuum("app", "alice", "t"),
                &found("alice", false),
                ALL,
                &HashMap::new()
            )
            .unwrap()
            .can_enable
        );
        // pg_cron 1.3: no alter_job; 1.2: no history either.
        let old = Features {
            alter: false,
            may_alter: false,
            ..ALL
        };
        assert!(
            !job(
                Flavor::Cron,
                &vacuum("app", "postgres", "t"),
                &found("postgres", true),
                old,
                &HashMap::new()
            )
            .unwrap()
            .can_enable
        );
        let older = Features { runs: false, ..old };
        assert!(
            !job(
                Flavor::Cron,
                &vacuum("app", "postgres", "t"),
                &found("postgres", true),
                older,
                &HashMap::new()
            )
            .unwrap()
            .has_history
        );
        // An unnamed job (pg_cron before 1.3, or scheduled without a name).
        let unnamed = fixture(&[
            ("id", "3"),
            ("schedule", "* * * * *"),
            ("command", "SELECT 1"),
            ("active", "t"),
        ]);
        assert_eq!(
            job(
                Flavor::Cron,
                &unnamed,
                &found("postgres", true),
                ALL,
                &HashMap::new()
            )
            .unwrap()
            .name,
            "job 3"
        );
    }

    #[test]
    fn the_script_schedules_it_again() {
        let f = found("postgres", true);
        let same = script_of(
            Variant::Postgres,
            &vacuum("postgres", "postgres", "t"),
            &f,
            ALL,
        );
        assert!(
            same.ends_with("SELECT cron.schedule(E'dbine_test_jobs_vacuum', E'0 3 * * *', $cmd$VACUUM ANALYZE$cmd$);\n"),
            "{same}"
        );
        // Another user's job: schedule_in_database replaces that user's job
        // of the name, not the runner's own.
        let elsewhere = script_of(Variant::Postgres, &vacuum("app", "bob", "f"), &f, ALL);
        assert!(
            elsewhere.ends_with("SELECT cron.schedule_in_database(E'dbine_test_jobs_vacuum', E'0 3 * * *', $cmd$VACUUM ANALYZE$cmd$, E'app', username := E'bob', active := false);\n"),
            "{elsewhere}"
        );
        // Only inactive, the user named all the same.
        let off = script_of(
            Variant::Postgres,
            &vacuum("postgres", "postgres", "f"),
            &f,
            ALL,
        );
        assert!(
            off.ends_with("SELECT cron.schedule_in_database(E'dbine_test_jobs_vacuum', E'0 3 * * *', $cmd$VACUUM ANALYZE$cmd$, E'postgres', username := E'postgres', active := false);\n"),
            "{off}"
        );
        // Without schedule_in_database (a pg_cron that isn't 1.4).
        let wrapped = script_of(
            Variant::Postgres,
            &vacuum("app", "bob", "f"),
            &f,
            Features {
                in_database: false,
                ..ALL
            },
        );
        assert!(
            wrapped.contains("SELECT cron.alter_job(cron.schedule(E'dbine_test_jobs_vacuum', E'0 3 * * *', $cmd$VACUUM ANALYZE$cmd$), database := E'app', username := E'bob', active := false);"),
            "{wrapped}"
        );
        // An unnamed job is always a new one: alter_job around it.
        let unnamed_bob = fixture(&[
            ("id", "5"),
            ("schedule", "@daily"),
            ("command", "SELECT 1"),
            ("active", "t"),
            ("database", "postgres"),
            ("username", "bob"),
        ]);
        assert!(script_of(Variant::Postgres, &unnamed_bob, &f, ALL).contains(
            "SELECT cron.alter_job(cron.schedule(E'@daily', $cmd$SELECT 1$cmd$), username := E'bob');"
        ));
        let old = script_of(
            Variant::Postgres,
            &vacuum("app", "postgres", "t"),
            &f,
            Features {
                alter: false,
                in_database: false,
                ..ALL
            },
        );
        assert!(old.contains("-- Este pg_cron no permite fijarlo al crearlo, pero el job se ejecuta en la base «app»."), "{old}");
        let unnamed = fixture(&[
            ("id", "3"),
            ("schedule", "* * * * *"),
            ("command", "SELECT '$cmd$'"),
            ("active", "t"),
            ("database", "postgres"),
            ("username", "postgres"),
        ]);
        let sql = script_of(Variant::Postgres, &unnamed, &f, ALL);
        assert!(sql.contains("-- pg_cron: job 3\n"));
        assert!(
            sql.contains("SELECT cron.schedule(E'* * * * *', $cmd1$SELECT '$cmd$'$cmd1$);"),
            "{sql}"
        );
        // A name can't break out of its literal or comment.
        let evil = fixture(&[
            ("id", "4"),
            ("jobname", "x');\nDROP TABLE t; --"),
            ("schedule", "@daily"),
            ("command", "SELECT 1"),
            ("active", "t"),
            ("database", "postgres"),
            ("username", "postgres"),
        ]);
        let sql = script_of(Variant::Postgres, &evil, &f, ALL);
        assert!(sql.contains("-- pg_cron: x');?DROP TABLE t; --\n"), "{sql}");
        assert!(
            sql.contains("cron.schedule(E'x'');\nDROP TABLE t; --', "),
            "{sql}"
        );
    }

    #[test]
    fn features_read_columns_and_functions_not_versions() {
        let sql = features_sql(Flavor::Cron);
        for part in [
            "has_schema_privilege('cron', 'USAGE')",
            "a.attname = 'jobname'",
            "a.attname = 'active'",
            "c.relname = 'job_run_details'",
            "p.proname = 'alter_job'",
            "p.proname = 'schedule_in_database'",
            "has_function_privilege(p.oid, 'EXECUTE')",
            "pg_catalog.pg_settings WHERE name = 'cron.log_run'",
        ] {
            assert!(sql.contains(part), "{part}");
        }
        // pg_cron's settings are superuser-only: current_setting would fail.
        assert!(!sql.contains("current_setting"));
        let task = features_sql(Flavor::Task);
        assert!(task.contains("pg_catalog.pg_task_run_history"));
        assert!(task.contains("name = 'task_log_run'") && !task.contains("current_setting"));
    }

    #[test]
    fn old_versions_get_placeholders_for_missing_columns() {
        let old = jobs_sql(Flavor::Cron, Features::default(), Some(7));
        assert!(old.contains("NULL::text AS jobname") && old.contains("true AS active"));
        assert!(old.contains("WHERE jobid = 7"));
        let new = jobs_sql(
            Flavor::Task,
            Features {
                name: true,
                active: true,
                ..Default::default()
            },
            None,
        );
        assert!(new.contains("jobname AS jobname") && new.contains("active AS active"));
        assert!(new.contains("FROM pg_catalog.pg_task ORDER BY"));
        assert!(!new.contains("WHERE"));
    }

    #[test]
    fn history_queries_take_only_the_number() {
        let last = last_sql(Flavor::Cron, Some(12));
        assert!(last.contains("WHERE j.jobid = 12"), "{last}");
        assert!(last.contains("FROM cron.job j") && last.contains("FROM cron.job_run_details d"));
        // One look-up per job, down the primary key.
        assert!(last.contains("WHERE d.jobid = j.jobid ORDER BY d.runid DESC LIMIT 2"));
        assert!(!last_sql(Flavor::Task, None).contains("WHERE j.jobid"));
        let task = runs_sql(Flavor::Task, 12, 50);
        assert!(task.contains("FROM pg_catalog.pg_task_run_history WHERE jobid = 12"));
        let sql = runs_sql(Flavor::Cron, 12, 50);
        assert!(sql.contains("WHERE jobid = 12") && sql.ends_with("LIMIT 50"));
    }

    #[test]
    fn the_latest_runs_give_the_last_outcome_and_the_run_going_on() {
        let rows = [
            // Job 1: running now, failed before.
            fixture(&[("id", "1"), ("status", "running"), ("started", "200")]),
            fixture(&[
                ("id", "1"),
                ("status", "failed"),
                ("started", "100"),
                ("ms", "5"),
            ]),
            // Job 2: succeeded last; an older run left "running" is stale.
            fixture(&[
                ("id", "2"),
                ("status", "succeeded"),
                ("started", "300"),
                ("ms", "1500.4"),
            ]),
            fixture(&[("id", "2"), ("status", "running"), ("started", "100")]),
            // Job 3: only a run starting, without a time yet.
            fixture(&[("id", "3"), ("status", "starting")]),
        ];
        let refs: Vec<&dyn Row> = rows.iter().map(|r| r as &dyn Row).collect();
        let last = last_of(&refs);
        let one = &last["1"];
        assert_eq!(one.running_since.as_deref(), Some("1970-01-01T00:03:20Z"));
        assert_eq!(one.outcome, Some(JobOutcome::Failed));
        assert_eq!(one.started.as_deref(), Some("1970-01-01T00:01:40Z"));
        let two = &last["2"];
        assert_eq!(two.running_since, None);
        assert_eq!(two.outcome, Some(JobOutcome::Succeeded));
        assert_eq!(two.duration_ms, Some(1500));
        assert_eq!(last["3"].running_since.as_deref(), Some(""));
        assert_eq!(last["3"].outcome, None);
    }

    #[test]
    fn statuses_map_to_outcomes() {
        assert_eq!(outcome(Some("succeeded")), JobOutcome::Succeeded);
        assert_eq!(outcome(Some("failed")), JobOutcome::Failed);
        assert_eq!(outcome(Some("connecting")), JobOutcome::Running);
        assert_eq!(outcome(Some("other")), JobOutcome::Unknown);
        assert_eq!(outcome(None), JobOutcome::Unknown);
    }

    #[test]
    fn a_task_maps_and_scripts_as_create_task() {
        let r = fixture(&[
            ("id", "16384"),
            ("jobname", "dbine_test_jobs \"task\""),
            ("schedule", "*/5 * * * *"),
            ("command", "VACUUM"),
            ("database", "demo"),
            ("username", "gpadmin"),
            ("active", "f"),
        ]);
        let f = found("gpadmin", true);
        let j = job(Flavor::Task, &r, &f, ALL, &HashMap::new()).unwrap();
        assert_eq!(j.id, "task:16384");
        assert_eq!(j.kind, "Task");
        assert!(!j.enabled && j.can_enable && !j.can_run);
        let sql = task_script(Variant::Cloudberry, &r, &f);
        assert_eq!(
            sql,
            "-- Tarea: dbine_test_jobs \"task\"\nCREATE TASK \"dbine_test_jobs \"\"task\"\"\" SCHEDULE E'*/5 * * * *' DATABASE \"demo\" USER \"gpadmin\" AS $cmd$VACUUM$cmd$;\nALTER TASK \"dbine_test_jobs \"\"task\"\"\" NOT ACTIVE;\n"
        );
    }

    #[test]
    fn another_users_task_is_changed_only_by_its_owner() {
        let r = fixture(&[
            ("id", "17233"),
            ("jobname", "nightly"),
            ("schedule", "0 3 * * *"),
            ("command", "SELECT 1"),
            ("database", "demo"),
            ("username", "bob"),
            ("active", "f"),
        ]);
        // ALTER TASK would reach the superuser's own "nightly".
        let admin = found("gpadmin", true);
        let j = job(Flavor::Task, &r, &admin, ALL, &HashMap::new()).unwrap();
        assert!(!j.can_enable);
        assert!(
            job(Flavor::Task, &r, &found("bob", false), ALL, &HashMap::new())
                .unwrap()
                .can_enable
        );
        // A pg_cron job goes by id: a superuser changes anyone's.
        assert!(
            job(Flavor::Cron, &r, &admin, ALL, &HashMap::new())
                .unwrap()
                .can_enable
        );
        // The script creates it for bob and deactivates it as bob.
        let sql = task_script(Variant::Cloudberry, &r, &admin);
        assert_eq!(
            sql,
            "-- Tarea: nightly\nCREATE TASK \"nightly\" SCHEDULE E'0 3 * * *' DATABASE \"demo\" USER \"bob\" AS $cmd$SELECT 1$cmd$;\nBEGIN;\nSET LOCAL ROLE \"bob\";\nALTER TASK \"nightly\" NOT ACTIVE;\nCOMMIT;\n"
        );
        assert!(!task_script(Variant::Cloudberry, &r, &found("bob", false)).contains("ROLE"));
    }
}
