//! openGauss's own jobs (DBMS_JOB, through `pkg_service`): `pg_job` and
//! the statement in `pg_job_proc`, for every database of the server.
//!
//! openGauss keeps no history, only the last run in the job's row. Times
//! are local (`timestamp without time zone`) and are read in the session's
//! `TimeZone`: the job's worker writes them in its owner's (the server's,
//! or what `ALTER ROLE/DATABASE … SET timezone` gives them), the same as
//! the owner's session. The job scheduler, though, compares the next run
//! with the time in its own zone, which a role or database setting doesn't
//! change: `sysdate` in a session ahead of it would put the run hours
//! away. So "run now" moves the next run to a moment already past in
//! every zone (`pkg_service.job_update`, a day before now in UTC), and the
//! scheduler takes it within a second. Enabling is `pkg_service.job_finish`
//! (a finished job is a broken one). There's no way to stop a run.

use super::{flag, message, ms, not_found, prop, text, time, yes_no, Found, JobId, Part, Row};
use crate::catalog::{comment_text, lit};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{Error, JobDetail, JobOutcome, JobStep, Result, ServerJob};
use tokio_postgres::SimpleQueryRow;

const KIND: &str = "DBMS_JOB";

/// `ts`, a local time of the server, as seconds since 1970.
fn epoch(ts: &str) -> String {
    format!("extract(epoch FROM ({ts} AT TIME ZONE current_setting('TimeZone')))::text")
}

fn jobs_sql(id: Option<i64>) -> String {
    let filter = id
        .map(|n| format!("WHERE j.job_id = {n}"))
        .unwrap_or_default();
    format!(
        "SELECT j.job_id::text AS id, j.job_name, j.log_user, j.priv_user, j.dbname, j.nspname,
                j.job_status, j.interval, j.failure_count::text AS failures, j.failure_msg,
                to_char(j.next_run_date, 'YYYY-MM-DD HH24:MI:SS.US') AS next_run_text,
                {next} AS next_run, {last} AS last_start, {this} AS this_run, {suc} AS last_success,
                {start} AS start_date, {end} AS end_date,
                ((extract(epoch FROM j.last_end_date) - extract(epoch FROM j.last_start_date)) * 1000)::text AS last_ms,
                (j.last_end_date IS NOT NULL AND j.last_end_date = j.last_suc_date) AS last_ok,
                p.what
         FROM pg_catalog.pg_job j LEFT JOIN pg_catalog.pg_job_proc p ON p.job_id = j.job_id
         {filter}
         ORDER BY j.job_id",
        next = epoch("j.next_run_date"),
        last = epoch("j.last_start_date"),
        this = epoch("j.this_run_date"),
        suc = epoch("j.last_suc_date"),
        start = epoch("j.start_date"),
        end = epoch("j.end_date"),
    )
}

/// Who's asking: their name, and whether they're a system administrator.
async fn who(s: &PgSession) -> Result<(String, bool)> {
    let rows = s
        .text(
            "SELECT current_user AS usr,
                    (SELECT rolsuper OR rolsystemadmin FROM pg_catalog.pg_roles WHERE rolname = current_user) AS admin",
        )
        .await?;
    Ok(rows
        .first()
        .map(|r| (text(r, "usr").unwrap_or_default(), flag(r, "admin")))
        .unwrap_or_default())
}

/// `interval` as the schedule; `null` is a job that runs once.
fn schedule(r: &dyn Row) -> Option<String> {
    text(r, "interval").filter(|i| !i.eq_ignore_ascii_case("null"))
}

fn job(r: &dyn Row, user: &str, admin: bool) -> Option<ServerJob> {
    let n: i64 = text(r, "id")?.parse().ok()?;
    let status = text(r, "job_status");
    // d: broken (disabled); r: running; s / f: the last run's outcome.
    let enabled = status.as_deref() != Some("d");
    let running = status.as_deref() == Some("r");
    let started = time(r, "last_start");
    let last_outcome = match status.as_deref() {
        Some("r") => Some(JobOutcome::Running),
        Some("f") => Some(JobOutcome::Failed),
        Some("s") if started.is_some() => Some(JobOutcome::Succeeded),
        // Broken: the last run's end is or isn't its last success.
        Some("d") if started.is_some() => Some(if flag(r, "last_ok") {
            JobOutcome::Succeeded
        } else {
            JobOutcome::Failed
        }),
        _ => None,
    };
    let owner = text(r, "log_user");
    let mine =
        admin || owner.as_deref() == Some(user) || text(r, "priv_user").as_deref() == Some(user);
    Some(ServerJob {
        id: JobId::GaussJob(n).to_string(),
        name: text(r, "job_name").unwrap_or_else(|| format!("job {n}")),
        kind: KIND.into(),
        group: text(r, "dbname"),
        description: None,
        owner,
        enabled,
        schedule: schedule(r),
        running,
        last_outcome,
        last_started: if running {
            time(r, "this_run").or(started)
        } else {
            started
        },
        last_duration_ms: if running { None } else { ms(r, "last_ms") },
        // A broken job's next run is 4000-01-01.
        next_run: time(r, "next_run").filter(|_| enabled),
        can_run: enabled && !running && mine,
        can_stop: false,
        can_enable: mine,
        has_history: false,
        has_script: true,
        system: false,
    })
}

pub(super) async fn list(s: &PgSession) -> Result<Part> {
    let (user, admin) = who(s).await?;
    let rows = s.text(&jobs_sql(None)).await?;
    Ok(Part {
        jobs: rows.iter().filter_map(|r| job(r, &user, admin)).collect(),
        notes: Vec::new(),
    })
}

async fn ready(s: &PgSession) -> Result<()> {
    if s.variant != Variant::OpenGauss {
        return Err(Error::Query(
            "este servidor no tiene jobs de DBMS_JOB".into(),
        ));
    }
    Ok(())
}

async fn row(s: &PgSession, n: i64) -> Result<SimpleQueryRow> {
    s.text(&jobs_sql(Some(n)))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found(JobId::GaussJob(n)))
}

pub(super) async fn detail(s: &PgSession, n: i64) -> Result<JobDetail> {
    ready(s).await?;
    let (user, admin) = who(s).await?;
    let r = row(s, n).await?;
    let mut job = job(&r, &user, admin).ok_or_else(|| not_found(JobId::GaussJob(n)))?;
    // The scheduler starts nothing (the list says why).
    job.can_run &= Found::detect(s).await?.gauss_off().is_none();
    let mut properties = vec![prop("", "Id", n.to_string())];
    let mut add = |label: &str, value: Option<String>| {
        if let Some(v) = value {
            properties.push(prop("", label, v));
        }
    };
    add("Base de datos", text(&r, "dbname"));
    add("Esquema", text(&r, "nspname"));
    add("Usuario", text(&r, "log_user"));
    add("Inicio", time(&r, "start_date"));
    add("Fin", time(&r, "end_date"));
    add("Última ejecución correcta", time(&r, "last_success"));
    add("Fallas seguidas", text(&r, "failures"));
    add("Último error", message(&r, "failure_msg"));
    add(
        "Se ejecuta una sola vez",
        Some(yes_no(schedule(&r).is_none()).into()),
    );
    Ok(JobDetail {
        job,
        steps: vec![JobStep {
            name: "Comando".into(),
            kind: Some("SQL".into()),
            target: None,
            command: text(&r, "what"),
        }],
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

/// Moves job `n`'s next run to a moment already past in every time zone
/// (they span UTC−12 to UTC+14), whatever zone the scheduler compares in.
fn run_sql(n: i64) -> String {
    format!(
        "SELECT pkg_service.job_update({n}, (now() AT TIME ZONE 'UTC') - interval '1 day', NULL, NULL)"
    )
}

pub(super) async fn run(s: &PgSession, n: i64) -> Result<()> {
    ready(s).await?;
    let r = row(s, n).await?;
    if text(&r, "job_status").as_deref() == Some("d") {
        return Err(Error::Query(
            "el job está deshabilitado: openGauss no lo ejecuta hasta habilitarlo".into(),
        ));
    }
    if let Some(why) = Found::detect(s).await?.gauss_off() {
        return Err(Error::Query(why.trim_end_matches('.').into()));
    }
    s.text(&run_sql(n)).await?;
    Ok(())
}

pub(super) async fn set_enabled(s: &PgSession, n: i64, enabled: bool) -> Result<()> {
    ready(s).await?;
    row(s, n).await?;
    // A finished job is a broken one; unfinishing it schedules it from now.
    s.text(&format!("SELECT pkg_service.job_finish({n}, {})", !enabled))
        .await?;
    Ok(())
}

/// `pkg_service.job_submit` with the job's statement, next run and
/// interval, wrapped in `job_finish` when it's broken.
fn script_of(v: Variant, r: &dyn Row) -> String {
    let name =
        text(r, "job_name").unwrap_or_else(|| format!("job {}", text(r, "id").unwrap_or_default()));
    let next =
        match text(r, "next_run_text").filter(|_| text(r, "job_status").as_deref() != Some("d")) {
            Some(t) => format!("{}::timestamp", lit(v, &t)),
            None => "sysdate".into(),
        };
    let interval = match schedule(r) {
        Some(i) => lit(v, &i),
        None => "'null'".into(),
    };
    let submit = format!(
        "pkg_service.job_submit(NULL, {}, {next}, {interval})",
        lit(v, &r.get("what").unwrap_or_default())
    );
    let call = if text(r, "job_status").as_deref() == Some("d") {
        format!("pkg_service.job_finish({submit}, true)")
    } else {
        submit
    };
    format!(
        "-- openGauss: {}\n-- Se crea en la base «{}».\nSELECT {call};\n",
        comment_text(&name),
        comment_text(&text(r, "dbname").unwrap_or_default())
    )
}

pub(super) async fn script(s: &PgSession, n: i64) -> Result<String> {
    ready(s).await?;
    let r = row(s, n).await?;
    Ok(script_of(s.variant, &r))
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::*;

    fn nightly(status: &str) -> super::super::Fixture {
        fixture(&[
            ("id", "14371"),
            ("log_user", "app"),
            ("priv_user", "app"),
            ("dbname", "postgres"),
            ("job_status", status),
            ("interval", "sysdate + 1"),
            ("next_run_text", "2026-11-13 13:31:05.000000"),
            ("next_run", "1794533465"),
            ("last_start", "1794488199.6"),
            ("last_ms", "5.4"),
            ("last_ok", "t"),
            ("what", "CALL purge('it''s');"),
        ])
    }

    #[test]
    fn a_job_row_maps_to_a_server_job() {
        let j = job(&nightly("s"), "app", false).unwrap();
        assert_eq!(j.id, "dbms_job:14371");
        assert_eq!(j.kind, "DBMS_JOB");
        assert_eq!(j.name, "job 14371");
        assert_eq!(j.group.as_deref(), Some("postgres"));
        assert_eq!(j.schedule.as_deref(), Some("sysdate + 1"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T12:56:39Z"));
        assert_eq!(j.last_duration_ms, Some(5));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-13T01:31:05Z"));
        assert!(j.enabled && j.can_run && j.can_enable && j.has_script);
        assert!(!j.can_stop && !j.has_history);
        let broken = job(&nightly("d"), "bob", false).unwrap();
        assert!(
            !broken.enabled && !broken.can_run && !broken.can_enable && broken.next_run.is_none()
        );
        let running = job(&nightly("r"), "bob", true).unwrap();
        assert!(running.running && !running.can_run && running.can_enable);
        assert_eq!(running.last_outcome, Some(JobOutcome::Running));
        let once = fixture(&[("id", "1"), ("job_status", "s"), ("interval", "null")]);
        assert_eq!(job(&once, "x", false).unwrap().schedule, None);
    }

    #[test]
    fn the_script_submits_it_again() {
        let sql = script_of(Variant::OpenGauss, &nightly("s"));
        assert!(
            sql.ends_with("SELECT pkg_service.job_submit(NULL, E'CALL purge(''it''''s'');', E'2026-11-13 13:31:05.000000'::timestamp, E'sysdate + 1');\n"),
            "{sql}"
        );
        let broken = script_of(Variant::OpenGauss, &nightly("d"));
        assert!(
            broken.contains("SELECT pkg_service.job_finish(pkg_service.job_submit(NULL, E'CALL purge(''it''''s'');', sysdate, E'sysdate + 1'), true);"),
            "{broken}"
        );
    }

    #[test]
    fn local_times_are_read_in_the_sessions_zone() {
        let sql = jobs_sql(Some(3));
        assert!(sql.contains("j.next_run_date AT TIME ZONE current_setting('TimeZone')"));
        assert!(sql.contains("WHERE j.job_id = 3"));
        // The script's copy is ISO whatever the session's DateStyle.
        assert!(
            sql.contains("to_char(j.next_run_date, 'YYYY-MM-DD HH24:MI:SS.US') AS next_run_text")
        );
    }

    #[test]
    fn run_now_is_past_in_every_zone() {
        assert_eq!(
            run_sql(14371),
            "SELECT pkg_service.job_update(14371, (now() AT TIME ZONE 'UTC') - interval '1 day', NULL, NULL)"
        );
    }
}
