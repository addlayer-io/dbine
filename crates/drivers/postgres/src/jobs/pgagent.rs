//! pgAgent: `pgagent.pga_job`, its steps (`pga_jobstep`), schedules
//! (`pga_schedule`, `pga_exception`) and log (`pga_joblog`,
//! `pga_jobsteplog`).
//!
//! The agent is a daemon of its own that polls the database: "run now"
//! sets the job's next run to now and an agent picks it up on its next
//! poll (an enabled job, an agent connected that may run it). pgAgent has
//! no way to stop a run.

use super::{
    dollar, dollar_tagged, flag, message, ms, not_found, prop, refs, text, time, Found, JobId,
    Part, Row,
};
use crate::catalog::{comment_text, lit};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{Error, JobDetail, JobOutcome, JobRun, JobStep, JobStepRun, Result, ServerJob};
use std::collections::HashMap;
use tokio_postgres::SimpleQueryRow;

const KIND: &str = "pgAgent";

/// What the user may do with pgAgent's tables.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Privileges {
    read: bool,
    write: bool,
    log: bool,
    agents: bool,
}

const PRIVILEGES_SQL: &str = "SELECT has_schema_privilege('pgagent', 'USAGE')
        AND has_table_privilege('pgagent.pga_job', 'SELECT')
        AND has_table_privilege('pgagent.pga_jobstep', 'SELECT')
        AND has_table_privilege('pgagent.pga_schedule', 'SELECT') AS read,
    has_table_privilege('pgagent.pga_job', 'UPDATE') AS write,
    has_table_privilege('pgagent.pga_joblog', 'SELECT')
        AND has_table_privilege('pgagent.pga_jobsteplog', 'SELECT') AS log,
    has_table_privilege('pgagent.pga_jobagent', 'SELECT') AS agents";

const UNREADABLE: &str =
    "Tu usuario no puede leer los jobs de pgAgent: necesita USAGE en el esquema pgagent y SELECT en sus tablas.";

async fn privileges(s: &PgSession) -> Result<Privileges> {
    let rows = s.text(PRIVILEGES_SQL).await?;
    Ok(rows
        .first()
        .map(|r| Privileges {
            read: flag(r, "read"),
            write: flag(r, "write"),
            log: flag(r, "log"),
            agents: flag(r, "agents"),
        })
        .unwrap_or_default())
}

async fn ready(s: &PgSession) -> Result<Privileges> {
    if !Found::detect(s).await?.pgagent {
        return Err(Error::Query(
            "pgAgent no está en esta base: sus jobs están en la base donde se instaló (casi siempre postgres)"
                .into(),
        ));
    }
    let p = privileges(s).await?;
    if !p.read {
        return Err(Error::Query(UNREADABLE.into()));
    }
    Ok(p)
}

fn jobs_sql(log: bool, id: Option<i32>) -> String {
    let (last_cols, last_joins) = if log {
        (
            "l.jlgstatus AS status, extract(epoch FROM l.jlgstart)::text AS started,
             (extract(epoch FROM l.jlgduration) * 1000)::text AS ms,
             extract(epoch FROM rn.jlgstart)::text AS running_since",
            "LEFT JOIN LATERAL (SELECT g.jlgstatus, g.jlgstart, g.jlgduration FROM pgagent.pga_joblog g
                                 WHERE g.jlgjobid = j.jobid AND g.jlgstatus <> 'r'
                                 ORDER BY g.jlgid DESC LIMIT 1) l ON true
             LEFT JOIN LATERAL (SELECT g.jlgstart FROM pgagent.pga_joblog g
                                 WHERE g.jlgjobid = j.jobid AND g.jlgstatus = 'r'
                                 ORDER BY g.jlgid DESC LIMIT 1) rn ON true",
        )
    } else {
        (
            "NULL::text AS status, NULL::text AS started, NULL::text AS ms, NULL::text AS running_since",
            "",
        )
    };
    let filter = id
        .map(|n| format!("WHERE j.jobid = {n}"))
        .unwrap_or_default();
    format!(
        "SELECT j.jobid::text AS id, j.jobname, j.jobdesc, j.jobhostagent, j.jobenabled AS enabled,
                c.jclname AS class, j.jobagentid::text AS agent,
                extract(epoch FROM j.jobnextrun)::text AS next_run,
                extract(epoch FROM j.joblastrun)::text AS last_run,
                extract(epoch FROM j.jobcreated)::text AS created,
                extract(epoch FROM j.jobchanged)::text AS changed,
                {last_cols}
         FROM pgagent.pga_job j
         LEFT JOIN pgagent.pga_jobclass c ON c.jclid = j.jobjclid
         {last_joins}
         {filter}
         ORDER BY j.jobname, j.jobid"
    )
}

/// The schedules; their start and end also as ISO text for the script
/// (`to_json` writes ISO 8601 whatever the session's `DateStyle`, which
/// `::text` follows).
fn schedules_sql(id: Option<i32>) -> String {
    let filter = id
        .map(|n| format!("WHERE jscjobid = {n}"))
        .unwrap_or_default();
    format!(
        "SELECT jscjobid::text AS job, jscid::text AS id, jscname AS name, jscdesc AS description,
                jscenabled AS enabled, to_json(jscstart) #>> '{{}}' AS start_text,
                to_json(jscend) #>> '{{}}' AS end_text,
                extract(epoch FROM jscstart)::text AS start, extract(epoch FROM jscend)::text AS until,
                jscminutes::text AS minutes, jschours::text AS hours, jscweekdays::text AS weekdays,
                jscmonthdays::text AS monthdays, jscmonths::text AS months
         FROM pgagent.pga_schedule {filter} ORDER BY jscjobid, jscname, jscid"
    )
}

/// A schedule's `{t,f,…}` array as booleans.
fn bools(array: &str) -> Vec<bool> {
    array
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .split(',')
        .map(|v| matches!(v.trim(), "t" | "true"))
        .collect()
}

/// One field of a cron expression: `*` when nothing (or everything) is
/// picked, else the picked values, consecutive runs of three or more as
/// `a-b`.
fn field(picked: &[bool], name: impl Fn(usize) -> String, first: usize) -> String {
    let on: Vec<usize> = picked
        .iter()
        .enumerate()
        .filter(|(_, b)| **b)
        .map(|(i, _)| i + first)
        .collect();
    if on.is_empty() || on.len() == picked.len() {
        return "*".into();
    }
    let mut parts = Vec::new();
    let mut i = 0;
    while i < on.len() {
        let mut j = i;
        while j + 1 < on.len() && on[j + 1] == on[j] + 1 {
            j += 1;
        }
        if j - i >= 2 {
            parts.push(format!("{}-{}", name(on[i]), name(on[j])));
        } else {
            for k in &on[i..=j] {
                parts.push(name(*k));
            }
        }
        i = j + 1;
    }
    parts.join(",")
}

/// A pgAgent schedule as `minute hour day month weekday`, with `L` for the
/// month's last day (pgAgent's 32nd day) and Sunday as 0.
fn cron_of(r: &dyn Row) -> String {
    let get = |name: &str| bools(&r.get(name).unwrap_or_default());
    let month_days = get("monthdays");
    let days = field(
        &month_days,
        |d| {
            if d == 32 {
                "L".into()
            } else {
                d.to_string()
            }
        },
        1,
    );
    format!(
        "{} {} {} {} {}",
        field(&get("minutes"), |m| m.to_string(), 0),
        field(&get("hours"), |h| h.to_string(), 0),
        days,
        field(&get("months"), |m| m.to_string(), 1),
        field(&get("weekdays"), |d| d.to_string(), 0),
    )
}

/// How the detail lists a schedule: name, expression, what limits it.
fn schedule_text(r: &dyn Row) -> String {
    let mut out = format!(
        "{}: {}",
        text(r, "name").unwrap_or_else(|| "programación".into()),
        cron_of(r)
    );
    if let Some(from) = time(r, "start") {
        out.push_str(&format!(", desde {from}"));
    }
    if let Some(until) = time(r, "until") {
        out.push_str(&format!(", hasta {until}"));
    }
    if !flag(r, "enabled") {
        out.push_str(" (deshabilitada)");
    }
    out
}

/// The job's schedule for the list: the enabled ones' expressions.
fn job_schedule(schedules: &[&SimpleQueryRow]) -> Option<String> {
    let on: Vec<String> = schedules
        .iter()
        .copied()
        .filter(|r| flag(*r, "enabled"))
        .map(|r| cron_of(r))
        .collect();
    (!on.is_empty()).then(|| on.join("; "))
}

fn outcome(status: Option<&str>) -> JobOutcome {
    match status {
        Some("r") => JobOutcome::Running,
        Some("s") => JobOutcome::Succeeded,
        // f: failed; i: the agent failed to run it (job) or a step failed
        // and its error was ignored (step).
        Some("f" | "i") => JobOutcome::Failed,
        // d: aborted (the agent died while running it).
        Some("d") => JobOutcome::Canceled,
        _ => JobOutcome::Unknown,
    }
}

/// Whether some connected agent may run a job pinned (or not) to a host.
fn agent_for(host_agent: Option<&str>, agents: &[String]) -> bool {
    match host_agent {
        None => !agents.is_empty(),
        Some(host) => agents.iter().any(|a| a == host),
    }
}

fn job(
    r: &dyn Row,
    p: Privileges,
    agents: Option<&[String]>,
    schedule: Option<String>,
) -> Option<ServerJob> {
    let n: i32 = text(r, "id")?.parse().ok()?;
    let enabled = flag(r, "enabled");
    let running = text(r, "agent").is_some();
    let host = text(r, "jobhostagent");
    // Without reading pga_jobagent there's no telling; the update works.
    let agent = agents.is_none_or(|a| agent_for(host.as_deref(), a));
    let finished = text(r, "status").map(|st| outcome(Some(&st)));
    Some(ServerJob {
        id: JobId::Agent(n).to_string(),
        name: text(r, "jobname").unwrap_or_else(|| format!("job {n}")),
        kind: KIND.into(),
        group: text(r, "class"),
        description: text(r, "jobdesc"),
        owner: None,
        enabled,
        schedule,
        running,
        last_outcome: finished.or(running.then_some(JobOutcome::Running)),
        last_started: if running {
            time(r, "running_since").or_else(|| time(r, "last_run"))
        } else {
            time(r, "started").or_else(|| time(r, "last_run"))
        },
        last_duration_ms: ms(r, "ms"),
        next_run: time(r, "next_run"),
        // A disabled job keeps no next run (its trigger clears it), and
        // only an agent that may take it runs it.
        can_run: p.write && enabled && !running && agent,
        can_stop: false,
        can_enable: p.write,
        has_history: p.log,
        has_script: true,
    })
}

/// The connected agents' hosts, `None` when the user can't read them.
async fn agents(s: &PgSession, p: Privileges) -> Result<Option<Vec<String>>> {
    if !p.agents {
        return Ok(None);
    }
    Ok(Some(
        s.text("SELECT jagstation FROM pgagent.pga_jobagent")
            .await?
            .iter()
            .filter_map(|r| text(r, "jagstation"))
            .collect(),
    ))
}

fn by_job(rows: &[SimpleQueryRow]) -> HashMap<String, Vec<&SimpleQueryRow>> {
    let mut out: HashMap<String, Vec<&SimpleQueryRow>> = HashMap::new();
    for r in rows {
        if let Some(job) = text(r, "job") {
            out.entry(job).or_default().push(r);
        }
    }
    out
}

pub(super) async fn list(s: &PgSession, _f: &Found) -> Result<Part> {
    let p = privileges(s).await?;
    let mut part = Part::default();
    if !p.read {
        part.notes.push(UNREADABLE.into());
        return Ok(part);
    }
    let rows = s.text(&jobs_sql(p.log, None)).await?;
    let schedules = s.text(&schedules_sql(None)).await?;
    let schedules = by_job(&schedules);
    let agents = agents(s, p).await?;
    part.jobs = rows
        .iter()
        .filter_map(|r| {
            let id = text(r, "id")?;
            let schedule = job_schedule(schedules.get(&id).map(Vec::as_slice).unwrap_or(&[]));
            job(r, p, agents.as_deref(), schedule)
        })
        .collect();
    if agents.as_ref().is_some_and(Vec::is_empty) && !part.jobs.is_empty() {
        part.notes.push(
            "No hay ningún agente de pgAgent conectado a este servidor: sus jobs no se ejecutan hasta que arranque uno."
                .into(),
        );
    }
    Ok(part)
}

async fn row(s: &PgSession, p: Privileges, n: i32) -> Result<SimpleQueryRow> {
    s.text(&jobs_sql(p.log, Some(n)))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found(JobId::Agent(n)))
}

fn steps_sql(n: i32) -> String {
    format!(
        "SELECT jstid::text AS id, jstname AS name, jstdesc AS description, jstenabled AS enabled,
                jstkind AS kind, jstcode AS code, jstconnstr AS connstr, jstdbname AS dbname,
                jstonerror AS onerror
         FROM pgagent.pga_jobstep WHERE jstjobid = {n} ORDER BY jstname, jstid"
    )
}

fn kind_name(kind: Option<&str>) -> Option<String> {
    match kind {
        Some("s") => Some("SQL".into()),
        Some("b") => Some("Lote".into()),
        _ => None,
    }
}

fn on_error(code: Option<&str>) -> &'static str {
    match code {
        Some("s") => "marcarlo correcto y seguir",
        Some("i") => "ignorar el error y seguir",
        _ => "detener el job",
    }
}

/// A step as the detail shows it. A remote step's connection string goes
/// without its password.
fn step(r: &dyn Row) -> JobStep {
    let mut name = text(r, "name").unwrap_or_default();
    if !flag(r, "enabled") {
        name.push_str(" (deshabilitado)");
    }
    let target = match text(r, "connstr") {
        Some(c) => Some(without_password(&c).0),
        None => text(r, "dbname"),
    };
    JobStep {
        name,
        kind: kind_name(text(r, "kind").as_deref()),
        target,
        command: r.get("code"),
    }
}

pub(super) async fn detail(s: &PgSession, n: i32) -> Result<JobDetail> {
    let p = ready(s).await?;
    let r = row(s, p, n).await?;
    let schedules = s.text(&schedules_sql(Some(n))).await?;
    let steps = s.text(&steps_sql(n)).await?;
    let agents = agents(s, p).await?;
    let refs: Vec<&SimpleQueryRow> = schedules.iter().collect();
    let job = job(&r, p, agents.as_deref(), job_schedule(&refs))
        .ok_or_else(|| not_found(JobId::Agent(n)))?;
    let mut properties = vec![prop("", "Id", n.to_string())];
    if let Some(c) = text(&r, "class") {
        properties.push(prop("", "Clase", c));
    }
    properties.push(prop(
        "",
        "Agente",
        text(&r, "jobhostagent").unwrap_or_else(|| "cualquiera".into()),
    ));
    if let Some(pid) = text(&r, "agent") {
        properties.push(prop("", "Ejecutándose en el agente (PID)", pid));
    }
    if let Some(t) = time(&r, "created") {
        properties.push(prop("", "Creado", t));
    }
    if let Some(t) = time(&r, "changed") {
        properties.push(prop("", "Modificado", t));
    }
    for st in &steps {
        properties.push(prop(
            "Pasos",
            &format!("{}: si falla", text(st, "name").unwrap_or_default()),
            on_error(text(st, "onerror").as_deref()),
        ));
    }
    Ok(JobDetail {
        job,
        steps: steps.iter().map(|r| step(r)).collect(),
        schedules: schedules.iter().map(|r| schedule_text(r)).collect(),
        properties,
    })
}

fn runs_sql(n: i32, limit: u32) -> [String; 2] {
    let runs = format!(
        "SELECT jlgid FROM pgagent.pga_joblog WHERE jlgjobid = {n} ORDER BY jlgid DESC LIMIT {limit}"
    );
    [
        format!(
            "SELECT jlgid::text AS id, jlgstatus AS status, extract(epoch FROM jlgstart)::text AS started,
                    (extract(epoch FROM jlgduration) * 1000)::text AS ms
             FROM pgagent.pga_joblog WHERE jlgjobid = {n} ORDER BY jlgid DESC LIMIT {limit}"
        ),
        format!(
            "SELECT sl.jsljlgid::text AS run, coalesce(st.jstname, 'paso ' || sl.jsljstid) AS name,
                    sl.jslstatus AS status, sl.jslresult::text AS result,
                    extract(epoch FROM sl.jslstart)::text AS started,
                    (extract(epoch FROM sl.jslduration) * 1000)::text AS ms,
                    left(sl.jsloutput, 8000) AS output
             FROM pgagent.pga_jobsteplog sl LEFT JOIN pgagent.pga_jobstep st ON st.jstid = sl.jsljstid
             WHERE sl.jsljlgid IN ({runs})
             ORDER BY sl.jsljlgid DESC, sl.jslid"
        ),
    ]
}

fn step_run(r: &dyn Row) -> JobStepRun {
    let outcome = outcome(text(r, "status").as_deref());
    let message = message(r, "output").or_else(|| {
        // A batch step's exit code, when it failed without output.
        text(r, "result")
            .filter(|c| c != "0" && outcome == JobOutcome::Failed)
            .map(|c| format!("código de salida {c}"))
    });
    JobStepRun {
        name: text(r, "name").unwrap_or_default(),
        outcome,
        started: time(r, "started"),
        duration_ms: ms(r, "ms"),
        message,
    }
}

/// The runs with their steps; a failed run's message is its first failed
/// step's.
fn assemble(runs: &[&dyn Row], steps: &[&dyn Row]) -> Vec<JobRun> {
    let mut per_run: HashMap<String, Vec<JobStepRun>> = HashMap::new();
    for r in steps {
        if let Some(run) = text(*r, "run") {
            per_run.entry(run).or_default().push(step_run(*r));
        }
    }
    runs.iter()
        .map(|r| {
            let id = text(*r, "id");
            let steps = id
                .as_ref()
                .and_then(|i| per_run.remove(i))
                .unwrap_or_default();
            let message = steps
                .iter()
                .find(|s| s.outcome == JobOutcome::Failed)
                .map(|s| match &s.message {
                    Some(m) => format!("{}: {m}", s.name),
                    None => format!("{}: falló", s.name),
                });
            JobRun {
                id,
                started: time(*r, "started"),
                duration_ms: ms(*r, "ms"),
                outcome: outcome(text(*r, "status").as_deref()),
                trigger: None,
                message,
                steps,
            }
        })
        .collect()
}

pub(super) async fn runs(s: &PgSession, n: i32, limit: u32) -> Result<Vec<JobRun>> {
    let p = ready(s).await?;
    if !p.log {
        return Err(Error::Query(
            "Tu usuario no puede leer el historial de pgAgent (pga_joblog y pga_jobsteplog)."
                .into(),
        ));
    }
    row(s, p, n).await?;
    let [runs, steps] = runs_sql(n, limit);
    let runs = s.text(&runs).await?;
    let steps = s.text(&steps).await?;
    Ok(assemble(&refs(&runs), &refs(&steps)))
}

pub(super) async fn run(s: &PgSession, n: i32) -> Result<()> {
    let p = ready(s).await?;
    let r = row(s, p, n).await?;
    if !flag(&r, "enabled") {
        return Err(Error::Query(
            "el job está deshabilitado: pgAgent no lo ejecuta hasta habilitarlo".into(),
        ));
    }
    let updated = s
        .text(&format!(
            "UPDATE pgagent.pga_job SET jobnextrun = now() WHERE jobid = {n} AND jobenabled RETURNING jobid"
        ))
        .await?;
    if updated.is_empty() {
        return Err(not_found(JobId::Agent(n)));
    }
    Ok(())
}

pub(super) async fn set_enabled(s: &PgSession, n: i32, enabled: bool) -> Result<()> {
    ready(s).await?;
    let updated = s
        .text(&format!(
            "UPDATE pgagent.pga_job SET jobenabled = {enabled}, jobchanged = now() WHERE jobid = {n} RETURNING jobid"
        ))
        .await?;
    if updated.is_empty() {
        return Err(not_found(JobId::Agent(n)));
    }
    Ok(())
}

fn exceptions_sql(n: i32) -> String {
    format!(
        "SELECT e.jexscid::text AS schedule, to_char(e.jexdate, 'YYYY-MM-DD') AS day, e.jextime::text AS at
         FROM pgagent.pga_exception e JOIN pgagent.pga_schedule s ON s.jscid = e.jexscid
         WHERE s.jscjobid = {n} ORDER BY e.jexid"
    )
}

/// A `{t,f,…}` array literal rebuilt from its booleans (never the
/// server's text as is).
fn array_lit(r: &dyn Row, name: &str) -> String {
    let items: Vec<&str> = bools(&r.get(name).unwrap_or_default())
        .into_iter()
        .map(|b| if b { "t" } else { "f" })
        .collect();
    format!("'{{{}}}'::boolean[]", items.join(","))
}

/// A one-letter code (`s`, `b`, `f`, `i`) as a literal, or the default.
fn code_lit(v: Variant, code: Option<String>, default: &str) -> String {
    let code = code
        .filter(|c| c.len() == 1 && c.chars().all(|c| c.is_ascii_lowercase()))
        .unwrap_or_else(|| default.into());
    lit(v, &code)
}

/// The `DO` block that creates the job again: the job, its steps, its
/// schedules and their exceptions.
fn script_of(
    v: Variant,
    job: &dyn Row,
    steps: &[&dyn Row],
    schedules: &[&dyn Row],
    exceptions: &[&dyn Row],
) -> String {
    let opt = |r: &dyn Row, name: &str| lit(v, &r.get(name).unwrap_or_default());
    let name = job.get("jobname").unwrap_or_default();
    let mut body = String::from("\nDECLARE\n    jid integer;\n    scid integer;\nBEGIN\n");
    body.push_str(&format!(
        "    INSERT INTO pgagent.pga_job (jobjclid, jobname, jobdesc, jobhostagent, jobenabled)\n    VALUES (coalesce((SELECT jclid FROM pgagent.pga_jobclass WHERE jclname = {}),\n                     (SELECT min(jclid) FROM pgagent.pga_jobclass)),\n            {}, {}, {}, {})\n    RETURNING jobid INTO jid;\n",
        opt(job, "class"),
        lit(v, &name),
        opt(job, "jobdesc"),
        opt(job, "jobhostagent"),
        flag(job, "enabled"),
    ));
    let mut hidden = Vec::new();
    for st in steps {
        let (connstr, had_password) = without_password(&st.get("connstr").unwrap_or_default());
        let step_name = st.get("name").unwrap_or_default();
        if had_password {
            hidden.push(step_name.clone());
        }
        body.push_str(&format!(
            "\n    INSERT INTO pgagent.pga_jobstep (jstjobid, jstname, jstdesc, jstenabled, jstkind, jstcode, jstconnstr, jstdbname, jstonerror)\n    VALUES (jid, {}, {}, {}, {}, {}, {}, {}, {});\n",
            lit(v, &step_name),
            opt(*st, "description"),
            flag(*st, "enabled"),
            code_lit(v, text(*st, "kind"), "s"),
            dollar(&st.get("code").unwrap_or_default()),
            lit(v, &connstr),
            opt(*st, "dbname"),
            code_lit(v, text(*st, "onerror"), "f"),
        ));
    }
    for sc in schedules {
        let until = match text(*sc, "end_text") {
            Some(t) => format!("{}::timestamptz", lit(v, &t)),
            None => "NULL".into(),
        };
        body.push_str(&format!(
            "\n    INSERT INTO pgagent.pga_schedule (jscjobid, jscname, jscdesc, jscenabled, jscstart, jscend,\n                                      jscminutes, jschours, jscweekdays, jscmonthdays, jscmonths)\n    VALUES (jid, {}, {}, {}, {}::timestamptz, {},\n            {},\n            {},\n            {}, {}, {})\n    RETURNING jscid INTO scid;\n",
            opt(*sc, "name"),
            opt(*sc, "description"),
            flag(*sc, "enabled"),
            lit(v, &sc.get("start_text").unwrap_or_else(|| "now".into())),
            until,
            array_lit(*sc, "minutes"),
            array_lit(*sc, "hours"),
            array_lit(*sc, "weekdays"),
            array_lit(*sc, "monthdays"),
            array_lit(*sc, "months"),
        ));
        let id = text(*sc, "id");
        for e in exceptions.iter().filter(|e| text(**e, "schedule") == id) {
            let day = text(*e, "day")
                .map(|d| format!("{}::date", lit(v, &d)))
                .unwrap_or_else(|| "NULL".into());
            let at = text(*e, "at")
                .map(|t| format!("{}::time", lit(v, &t)))
                .unwrap_or_else(|| "NULL".into());
            body.push_str(&format!(
                "    INSERT INTO pgagent.pga_exception (jexscid, jexdate, jextime) VALUES (scid, {day}, {at});\n"
            ));
        }
    }
    body.push_str("END\n");
    let mut out = format!("-- pgAgent: {}\n", comment_text(&name));
    for step in hidden {
        out.push_str(&format!(
            "-- La cadena de conexión del paso «{}» tenía contraseña: DBine no la copia; agregala si el agente no la toma de otro lado.\n",
            comment_text(&step)
        ));
    }
    out.push_str(&format!("DO {};\n", dollar_tagged(&body, "job")));
    out
}

pub(super) async fn script(s: &PgSession, n: i32) -> Result<String> {
    let p = ready(s).await?;
    let job = row(s, p, n).await?;
    let steps = s.text(&steps_sql(n)).await?;
    let schedules = s.text(&schedules_sql(Some(n))).await?;
    let exceptions = s.text(&exceptions_sql(n)).await?;
    Ok(script_of(
        s.variant,
        &job,
        &refs(&steps),
        &refs(&schedules),
        &refs(&exceptions),
    ))
}

/// A libpq connection string (`key=value …` or a `postgresql://` URI)
/// without its password, and whether it had one.
pub(super) fn without_password(conn: &str) -> (String, bool) {
    let t = conn.trim();
    if t.starts_with("postgres://") || t.starts_with("postgresql://") {
        return uri_without_password(t);
    }
    let chars: Vec<char> = t.chars().collect();
    let mut kept = Vec::new();
    let mut removed = false;
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        let key: String = chars[start..i].iter().collect();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '=' {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '\'' {
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
        }
        let token: String = chars[start..i.min(chars.len())].iter().collect();
        if key.eq_ignore_ascii_case("password") {
            removed = true;
        } else {
            kept.push(token);
        }
    }
    (kept.join(" "), removed)
}

fn uri_without_password(t: &str) -> (String, bool) {
    let (scheme, rest) = t.split_once("://").unwrap_or(("postgresql", t));
    let mut removed = false;
    // The user info ends at the last `@` (a password may hold `/` or `?`).
    let rest = match rest.rsplit_once('@') {
        Some((userinfo, host)) => match userinfo.split_once(':') {
            Some((user, _)) => {
                removed = true;
                format!("{user}@{host}")
            }
            None => rest.to_string(),
        },
        None => rest.to_string(),
    };
    let rest = match rest.split_once('?') {
        Some((path, query)) => {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|kv| {
                    let key = kv.split('=').next().unwrap_or("");
                    let password = key.eq_ignore_ascii_case("password");
                    removed |= password;
                    !password
                })
                .collect();
            if kept.is_empty() {
                path.to_string()
            } else {
                format!("{path}?{}", kept.join("&"))
            }
        }
        None => rest,
    };
    (format!("{scheme}://{rest}"), removed)
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::*;

    fn arr(on: &[usize], len: usize) -> &'static str {
        let items: Vec<&str> = (0..len)
            .map(|i| if on.contains(&i) { "t" } else { "f" })
            .collect();
        Box::leak(format!("{{{}}}", items.join(",")).into_boxed_str())
    }

    #[test]
    fn schedules_read_as_cron_expressions() {
        let every_day_at_3 = fixture(&[
            ("minutes", arr(&[0], 60)),
            ("hours", arr(&[3], 24)),
            ("weekdays", arr(&[], 7)),
            ("monthdays", arr(&[], 32)),
            ("months", arr(&[], 12)),
        ]);
        assert_eq!(cron_of(&every_day_at_3), "0 3 * * *");
        let weekdays_quarter_hours = fixture(&[
            ("minutes", arr(&[0, 15, 30, 45], 60)),
            ("hours", arr(&(8..=18).collect::<Vec<_>>(), 24)),
            ("weekdays", arr(&[1, 2, 3, 4, 5], 7)),
            ("monthdays", arr(&[0, 31], 32)),
            ("months", arr(&[0, 1], 12)),
        ]);
        assert_eq!(
            cron_of(&weekdays_quarter_hours),
            "0,15,30,45 8-18 1,L 1,2 1-5"
        );
        // Everything picked reads as `*` too.
        let all = fixture(&[("minutes", arr(&(0..60).collect::<Vec<_>>(), 60))]);
        assert!(cron_of(&all).starts_with("* "));
    }

    #[test]
    fn a_schedule_line_says_its_window_and_state() {
        let r = fixture(&[
            ("name", "nightly"),
            ("enabled", "f"),
            ("start", "1794488199"),
            ("minutes", arr(&[30], 60)),
            ("hours", arr(&[2], 24)),
        ]);
        assert_eq!(
            schedule_text(&r),
            "nightly: 30 2 * * *, desde 2026-11-12T12:56:39Z (deshabilitada)"
        );
    }

    #[test]
    fn a_job_row_maps_to_a_server_job() {
        let p = Privileges {
            read: true,
            write: true,
            log: true,
            agents: true,
        };
        let r = fixture(&[
            ("id", "4"),
            ("jobname", "dbine_test_jobs_vacuum"),
            ("jobdesc", "noche"),
            ("jobhostagent", ""),
            ("enabled", "t"),
            ("class", "Routine Maintenance"),
            ("next_run", "1794528000"),
            ("status", "f"),
            ("started", "1794488199.5"),
            ("ms", "1500.4"),
        ]);
        let agents = vec!["host1".to_string()];
        let agents = Some(agents.as_slice());
        let j = job(&r, p, agents, Some("0 3 * * *".into())).unwrap();
        assert_eq!(j.id, "pgagent:4");
        assert_eq!(j.kind, "pgAgent");
        assert_eq!(j.group.as_deref(), Some("Routine Maintenance"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T12:56:39Z"));
        assert_eq!(j.last_duration_ms, Some(1500));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-13T00:00:00Z"));
        assert!(j.enabled && !j.running && j.can_run && j.can_enable && j.has_history);
        assert!(!j.can_stop && j.has_script);
        // No agent that may take it: no "run now".
        let nobody: Vec<String> = Vec::new();
        assert!(!job(&r, p, Some(nobody.as_slice()), None).unwrap().can_run);
        let pinned = fixture(&[("id", "4"), ("enabled", "t"), ("jobhostagent", "host2")]);
        assert!(!job(&pinned, p, agents, None).unwrap().can_run);
        // Running on an agent.
        let busy = fixture(&[
            ("id", "4"),
            ("enabled", "t"),
            ("agent", "321"),
            ("running_since", "1794488199"),
        ]);
        let b = job(&busy, p, agents, None).unwrap();
        assert!(b.running && !b.can_run);
        assert_eq!(b.last_outcome, Some(JobOutcome::Running));
        // Read-only user: nothing to do but look.
        let ro = job(
            &r,
            Privileges {
                read: true,
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();
        assert!(!ro.can_run && !ro.can_enable && !ro.has_history);
    }

    #[test]
    fn runs_carry_their_steps_and_the_failure() {
        let run1 = fixture(&[
            ("id", "9"),
            ("status", "f"),
            ("started", "100"),
            ("ms", "2000"),
        ]);
        let run2 = fixture(&[
            ("id", "8"),
            ("status", "s"),
            ("started", "50"),
            ("ms", "10"),
        ]);
        let s1 = fixture(&[
            ("run", "9"),
            ("name", "a"),
            ("status", "s"),
            ("result", "0"),
        ]);
        let s2 = fixture(&[
            ("run", "9"),
            ("name", "b"),
            ("status", "f"),
            ("result", "1"),
            ("output", "ERROR: boom"),
        ]);
        let s3 = fixture(&[
            ("run", "8"),
            ("name", "a"),
            ("status", "i"),
            ("result", "2"),
        ]);
        let runs = assemble(&[&run1, &run2], &[&s1, &s2, &s3]);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].outcome, JobOutcome::Failed);
        assert_eq!(runs[0].steps.len(), 2);
        assert_eq!(runs[0].message.as_deref(), Some("b: ERROR: boom"));
        assert_eq!(runs[0].duration_ms, Some(2000));
        assert_eq!(runs[1].outcome, JobOutcome::Succeeded);
        assert_eq!(
            runs[1].steps[0].message.as_deref(),
            Some("código de salida 2")
        );
    }

    #[test]
    fn the_script_recreates_the_job_and_hides_passwords() {
        let job = fixture(&[
            ("jobname", "dbine_test_jobs_x'; DROP"),
            ("jobdesc", ""),
            ("jobhostagent", ""),
            ("enabled", "t"),
            ("class", "Routine Maintenance"),
        ]);
        let st = fixture(&[
            ("name", "one"),
            ("enabled", "t"),
            ("kind", "s"),
            ("code", "SELECT '$job$'"),
            ("connstr", "host=db password='s3 cr$t' user=a"),
            ("dbname", ""),
            ("onerror", "f"),
        ]);
        let sc = fixture(&[
            ("id", "5"),
            ("name", "every hour"),
            ("enabled", "t"),
            ("start_text", "2026-11-12 00:00:00+00"),
            ("minutes", arr(&[0], 60)),
            ("hours", arr(&[], 24)),
            ("weekdays", arr(&[], 7)),
            ("monthdays", arr(&[], 32)),
            ("months", arr(&[], 12)),
        ]);
        let ex = fixture(&[("schedule", "5"), ("day", "2026-12-25")]);
        let sql = script_of(Variant::Postgres, &job, &[&st], &[&sc], &[&ex]);
        assert!(sql.contains("E'dbine_test_jobs_x''; DROP'"), "{sql}");
        assert!(!sql.contains("s3 cr$t"), "{sql}");
        assert!(sql.contains("E'host=db user=a'"), "{sql}");
        assert!(sql.contains("tenía contraseña"));
        assert!(sql.contains("$cmd$SELECT '$job$'$cmd$"));
        // The block's own tag isn't the one inside the step's code.
        assert!(sql.contains("DO $job1$"), "{sql}");
        assert!(sql.contains("'{t,f,f"));
        assert!(sql.contains("VALUES (scid, E'2026-12-25'::date, NULL)"));
        assert!(sql.trim_end().ends_with("$job1$;"));
    }

    #[test]
    fn connection_strings_lose_their_password() {
        assert_eq!(
            without_password("host=a password=secret dbname=b"),
            ("host=a dbname=b".into(), true)
        );
        assert_eq!(
            without_password("host=a PASSWORD = 'with spaces \\' quote' port=5"),
            ("host=a port=5".into(), true)
        );
        assert_eq!(
            without_password("host=a dbname=b"),
            ("host=a dbname=b".into(), false)
        );
        assert_eq!(
            without_password("postgresql://u:p%40ss@h:5432/db?sslmode=require&password=x"),
            ("postgresql://u@h:5432/db?sslmode=require".into(), true)
        );
        assert_eq!(
            without_password("postgres://u:pa/ss?x@h/db"),
            ("postgres://u@h/db".into(), true)
        );
        assert_eq!(
            without_password("postgresql://h/db"),
            ("postgresql://h/db".into(), false)
        );
    }

    #[test]
    fn steps_show_their_target_without_secrets() {
        let remote = fixture(&[
            ("name", "s"),
            ("enabled", "f"),
            ("kind", "b"),
            ("connstr", "host=x password=y"),
            ("code", "echo hi"),
        ]);
        let st = step(&remote);
        assert_eq!(st.name, "s (deshabilitado)");
        assert_eq!(st.kind.as_deref(), Some("Lote"));
        assert_eq!(st.target.as_deref(), Some("host=x"));
        let local = fixture(&[
            ("name", "s"),
            ("enabled", "t"),
            ("kind", "s"),
            ("dbname", "app"),
        ]);
        assert_eq!(step(&local).target.as_deref(), Some("app"));
    }

    #[test]
    fn statements_take_only_numbers() {
        assert!(jobs_sql(true, Some(4)).contains("WHERE j.jobid = 4"));
        assert!(!jobs_sql(false, None).contains("pga_joblog"));
        let [runs, steps] = runs_sql(4, 20);
        assert!(runs.contains("jlgjobid = 4") && runs.ends_with("LIMIT 20"));
        assert!(steps.contains("jlgjobid = 4 ORDER BY jlgid DESC LIMIT 20"));
    }

    #[test]
    fn the_scripts_dates_dont_follow_datestyle() {
        let sql = schedules_sql(Some(4));
        assert!(sql.contains("WHERE jscjobid = 4"));
        assert!(
            sql.contains("to_json(jscstart) #>> '{}' AS start_text"),
            "{sql}"
        );
        assert!(sql.contains("to_json(jscend) #>> '{}' AS end_text"));
        assert!(!sql.contains("jscstart::text"));
        let ex = exceptions_sql(4);
        assert!(ex.contains("to_char(e.jexdate, 'YYYY-MM-DD') AS day"));
        assert!(ex.contains("s.jscjobid = 4"));
    }
}
