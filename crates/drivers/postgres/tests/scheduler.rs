//! The scheduler designer against real servers: pg_cron (PostgreSQL and
//! YugabyteDB), pgAgent, TimescaleDB and Apache Cloudberry's tasks,
//! CockroachDB's schedules, changefeeds and row-level TTL, openGauss's
//! DBMS_JOB, plus servers without any (PostgreSQL, Greengage). Each test reads
//! `DBINE_TEST_<NAME>_URL` (`postgres://user:pass@host:port/db`) and is
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_PGCRON_URL=postgres://postgres:pw@localhost:25040/postgres \
//! DBINE_TEST_YUGABYTEDB_CRON_URL=postgres://yugabyte@localhost:25041/yugabyte \
//! DBINE_TEST_TIMESCALEDB_URL=postgres://postgres:pw@localhost:25015/postgres \
//! DBINE_TEST_CLOUDBERRY_URL=postgres://gpadmin:pw@localhost:25017/demo \
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_GREENGAGE_URL=postgres://gpadmin:pw@localhost:25018/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_OPENGAUSS_URL='postgres://gaussdb:Dbine@1234@localhost:25020/postgres' \
//!   cargo test -p dbine-driver-postgres --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! The pg_cron container is `postgres:16` with `postgresql-16-cron` and
//! `pgagent` (see tests/jobs.rs): pg_cron and pgAgent in the same database.
//! Every object a test makes is named `dbine_test_sched_*` and is removed at
//! the end, run history included, whether the test passed or not.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Error, FormValues, ObjectForm, QueryOutcome, Session};
use futures::FutureExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return None;
    };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn connect(cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == cfg.driver)
        .unwrap();
    d.connect(cfg, None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    let r = s.execute(sql, 1000, &mut out).await;
    assert!(
        r.is_ok() && out.errors.is_empty(),
        "{sql}: {r:?} {:?}",
        out.errors
    );
    out
}

/// The last result's rows.
/// Cells as text (`t`/`f` for booleans), whatever protocol answered.
async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    let text = |v: &Value| match v {
        Value::Bool(b) => json!(if *b { "t" } else { "f" }),
        Value::Number(n) => json!(n.to_string()),
        other => other.clone(),
    };
    run(s, sql)
        .await
        .results
        .iter()
        .rfind(|r| !r.columns.is_empty())
        .map(|r| {
            r.rows
                .iter()
                .map(|row| row.iter().map(text).collect())
                .collect()
        })
        .unwrap_or_default()
}

/// The first cell as text.
async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    match rows(s, sql).await.first().and_then(|r| r.first()) {
        Some(Value::String(v)) => v.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

/// Runs the test, then `cleanup` on a connection of its own, then lets a
/// failed assertion through; then checks nothing is left (`left` answers 0).
async fn with_cleanup(
    cfg: &ConnectionConfig,
    cleanup: &str,
    left: &str,
    body: impl Future<Output = ()>,
) {
    let result = AssertUnwindSafe(body).catch_unwind().await;
    let mut s = connect(cfg).await;
    let mut out = QueryOutcome::default();
    let r = s.execute(cleanup, 100, &mut out).await;
    if r.is_err() || !out.errors.is_empty() {
        eprintln!("cleanup failed: {r:?} {:?}", out.errors);
    }
    let n = scalar(&mut s, left).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(r.is_ok() && out.errors.is_empty(), "cleanup failed");
    assert_eq!(n, "0", "objects left behind");
}

/// What the designer would send back for a form as it came.
fn values_of(form: &ObjectForm) -> FormValues {
    let mut values = form.values.clone();
    for f in form.sections.iter().flat_map(|s| s.fields.iter()) {
        if let Some(d) = &f.default {
            values.entry(f.key.clone()).or_insert_with(|| d.clone());
        }
    }
    let lists = form
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .map(|l| (l.key.clone(), l.rows.clone()))
        .collect();
    FormValues { values, lists }
}

/// A new list row: the list's defaults, then `fields`.
fn new_row(form: &ObjectForm, list: &str, fields: Value) -> BTreeMap<String, Value> {
    let l = form
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .find(|l| l.key == list)
        .unwrap_or_else(|| panic!("list {list}"));
    let mut row: BTreeMap<String, Value> = l
        .fields
        .iter()
        .filter_map(|f| Some((f.key.clone(), f.default.clone()?)))
        .collect();
    for (k, v) in fields.as_object().unwrap() {
        row.insert(k.clone(), v.clone());
    }
    row
}

fn set(v: &mut FormValues, k: &str, x: Value) {
    v.values.insert(k.into(), x);
}

/// Sets the field `k` the designer shows for the scheduler picked in `v`:
/// a combined new-job form gives a scheduler's field that differs from
/// another's its own key, `<source>.<k>`.
fn set_in(form: &ObjectForm, v: &mut FormValues, k: &str, x: Value) {
    let source = v
        .values
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let own = format!("{source}.{k}");
    let key = if form
        .sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .any(|f| f.key == own)
    {
        own
    } else {
        k.to_string()
    };
    v.values.insert(key, x);
}

async fn save(s: &mut Box<dyn Session>, kind: &str, id: Option<&str>, v: &FormValues) -> String {
    let script = s.scheduler_script(kind, id, v).await.expect("script");
    eprintln!("--- {kind} {id:?}\n{script}");
    let r = s.scheduler_save(kind, id, v).await;
    r.unwrap_or_else(|e| panic!("save {kind}: {e:?}\n{script}"))
        .expect("an id")
}

fn field<'a>(form: &'a ObjectForm, key: &str) -> &'a dbine_driver::FormField {
    form.sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|f| f.key == key)
        .unwrap_or_else(|| panic!("field {key}"))
}

fn number(id: &str) -> &str {
    id.split_once(':').map_or(id, |(_, n)| n)
}

// ---------------------------------------------------------------------------
// pg_cron
// ---------------------------------------------------------------------------

const CRON_CLEANUP: &str = r"DELETE FROM cron.job_run_details WHERE jobid IN (SELECT jobid FROM cron.job WHERE jobname LIKE 'dbine\_test\_sched\_%');
SELECT cron.unschedule(jobid) FROM cron.job WHERE jobname LIKE 'dbine\_test\_sched\_%';";
const CRON_LEFT: &str = r"SELECT count(*) FROM cron.job WHERE jobname LIKE 'dbine\_test\_sched\_%'";

const CRON_JOB: &str = "dbine_test_sched_cron 'a'";
const CRON_JOB2: &str = "dbine_test_sched_cron b";

async fn cron_job_row(s: &mut Box<dyn Session>, id: &str) -> Vec<Value> {
    rows(
        s,
        &format!(
            "SELECT jobname, schedule, command, database, username, active FROM cron.job WHERE jobid = {}",
            number(id)
        ),
    )
    .await
    .first()
    .cloned()
    .unwrap_or_default()
}

/// pg_cron managed whole as a superuser.
async fn cron_on(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else { return };
    let c2 = cfg.clone();
    with_cleanup(&cfg, CRON_CLEANUP, CRON_LEFT, async move {
        let cfg = c2;
        let mut s = connect(&cfg).await;
        run(&mut s, CRON_CLEANUP).await;
        let db = scalar(&mut s, "SELECT current_database()").await;
        let user = scalar(&mut s, "SELECT current_user").await;
        let info = s.scheduler_info().await.expect("info");
        eprintln!("{info:?}");
        let job_kind = info.kinds.iter().find(|k| k.key == "job").expect("job kind");
        assert!(job_kind.can_create && job_kind.can_edit && job_kind.can_delete);
        assert!(!info.logs);

        // A new job: in a database with pgAgent too, the form asks which.
        let form = s.scheduler_form("job", None).await.expect("form");
        let mut v = values_of(&form);
        if form.sections.iter().flat_map(|x| x.fields.iter()).any(|f| f.key == "source") {
            set(&mut v, "source", json!("pg_cron"));
        }
        assert_eq!(v.values["database"], json!(db));
        set_in(&form, &mut v, "name", json!(CRON_JOB));
        set_in(&form, &mut v, "schedule", json!("*/5 * * * *"));
        set_in(&form, &mut v, "command", json!("SELECT 'dbine $cmd$'"));
        set_in(&form, &mut v, "active", json!(false));
        // A bad schedule never reaches the server.
        let mut bad = v.clone();
        set_in(&form, &mut bad, "schedule", json!("* * *"));
        let e = s.scheduler_script("job", None, &bad).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("Programación")), "{e:?}");
        let script = s.scheduler_script("job", None, &v).await.unwrap();
        assert!(script.contains("cron.schedule_in_database("), "{script}");
        let id = save(&mut s, "job", None, &v).await;
        assert!(id.starts_with("pg_cron:"), "{id}");
        assert_eq!(
            cron_job_row(&mut s, &id).await,
            [json!(CRON_JOB), json!("*/5 * * * *"), json!("SELECT 'dbine $cmd$'"), json!(db), json!(user), json!("f")]
        );
        // The tree's job is the same id.
        let jobs = s.jobs().await.unwrap();
        assert!(jobs.jobs.iter().any(|j| j.id == id), "{jobs:?}");
        let items = s.scheduler_items("job").await.unwrap();
        assert!(items.items.iter().any(|i| i.id == id && i.enabled == Some(false)));
        // The same name again is refused (pg_cron would replace it).
        let e = s.scheduler_script("job", None, &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("Ya hay")), "{e:?}");

        // Edit: schedule in seconds, command, active.
        let form = s.scheduler_form("job", Some(&id)).await.expect("form");
        assert_eq!(form.values["name"], json!(CRON_JOB));
        assert!(field(&form, "jobid").read_only);
        let mut v = values_of(&form);
        set(&mut v, "schedule", json!("30 seconds"));
        set(&mut v, "command", json!("SELECT 2"));
        set(&mut v, "active", json!(true));
        let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
        assert!(
            script.contains(&format!("cron.alter_job(job_id := {}, schedule := E'30 seconds', command := $cmd$SELECT 2$cmd$, active := true)", number(&id))),
            "{script}"
        );
        assert_eq!(save(&mut s, "job", Some(&id), &v).await, id);
        assert_eq!(
            cron_job_row(&mut s, &id).await,
            [json!(CRON_JOB), json!("30 seconds"), json!("SELECT 2"), json!(db), json!(user), json!("t")]
        );
        // A new name, in place: the same id.
        set(&mut v, "name", json!(CRON_JOB2));
        let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
        assert!(script.contains("UPDATE cron.job SET jobname"), "{script}");
        assert_eq!(save(&mut s, "job", Some(&id), &v).await, id);
        assert_eq!(cron_job_row(&mut s, &id).await[0], json!(CRON_JOB2));
        // Unchanged.
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        let script = s.scheduler_script("job", Some(&id), &values_of(&form)).await.unwrap();
        assert!(script.contains("no hay cambios"), "{script}");
        // History: a run of it, cleared.
        run(
            &mut s,
            &format!(
                "INSERT INTO cron.job_run_details (jobid, runid, job_pid, database, username, command, status, start_time, end_time)
                 VALUES ({n}, nextval('cron.runid_seq'), 1, current_database(), current_user, 'SELECT 2', 'succeeded', now(), now())",
                n = number(&id)
            ),
        )
        .await;
        s.clear_job_history(&id).await.expect("clear");
        let n = scalar(&mut s, &format!("SELECT count(*) FROM cron.job_run_details WHERE jobid = {}", number(&id))).await;
        assert_eq!(n, "0");
        // No steps.
        assert!(matches!(s.run_job_from(&id, "x").await, Err(Error::Unsupported(_))));
        // Read-only connections read, never write.
        let mut ro = ReadOnlySession::new(connect(&cfg).await);
        let form = ro.scheduler_form("job", Some(&id)).await.expect("form");
        assert!(form.read_only);
        let v = values_of(&form);
        assert!(ro.scheduler_script("job", Some(&id), &v).await.is_ok());
        assert!(ro.scheduler_save("job", Some(&id), &v).await.is_err());
        assert!(ro.scheduler_delete("job", &id).await.is_err());
        assert!(ro.clear_job_history(&id).await.is_err());
        // Delete.
        let script = s.scheduler_delete_script("job", &id).await.unwrap();
        assert!(script.contains(&format!("SELECT cron.unschedule({});", number(&id))), "{script}");
        s.scheduler_delete("job", &id).await.expect("delete");
        assert!(cron_job_row(&mut s, &id).await.is_empty());
        // Ids that weren't written here.
        for bad in ["pg_cron:0", "pg_cron:1; DROP TABLE x", "crdb_schedule:1", "x"] {
            assert!(s.scheduler_form("job", Some(bad)).await.is_err(), "{bad}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pg_cron_managed_whole() {
    cron_on("postgres", "DBINE_TEST_PGCRON_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pg_cron_on_yugabytedb() {
    cron_on("yugabytedb", "DBINE_TEST_YUGABYTEDB_CRON_URL").await;
}

const USER: &str = "dbine_test_sched_user";
const USER_PW: &str = "pw_sched_1";

/// A user without pg_cron's restricted functions: jobs in this database,
/// as themselves, changed by name.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pg_cron_as_a_plain_user() {
    let Some(cfg) = cfg("postgres", "DBINE_TEST_PGCRON_URL") else {
        return;
    };
    let cleanup = format!(
        "{CRON_CLEANUP}
         DO $$BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{USER}') THEN
             EXECUTE 'REVOKE USAGE ON SCHEMA cron FROM {USER}'; EXECUTE 'DROP ROLE {USER}'; END IF; END$$;"
    );
    let left =
        format!("SELECT ({CRON_LEFT}) + (SELECT count(*) FROM pg_roles WHERE rolname = '{USER}')");
    let c2 = cfg.clone();
    let cl = cleanup.clone();
    with_cleanup(&cfg, &cleanup, &left, async move {
        let cfg = c2;
        let mut admin = connect(&cfg).await;
        run(&mut admin, &cl).await;
        run(
            &mut admin,
            &format!("CREATE ROLE {USER} LOGIN PASSWORD '{USER_PW}'; GRANT USAGE ON SCHEMA cron TO {USER};"),
        )
        .await;
        let ucfg = ConnectionConfig {
            username: Some(USER.into()),
            password: Some(USER_PW.into()),
            ..cfg.clone()
        };
        let mut s = connect(&ucfg).await;
        let info = s.scheduler_info().await.unwrap();
        assert!(info.notes.iter().any(|n| n.contains("cron.schedule_in_database")), "{info:?}");
        let form = s.scheduler_form("job", None).await.unwrap();
        // pgAgent isn't this user's: only pg_cron.
        assert!(field(&form, "database").read_only && field(&form, "username").read_only && field(&form, "active").read_only);
        let mut v = values_of(&form);
        set_in(&form, &mut v, "name", json!("dbine_test_sched_mine"));
        set_in(&form, &mut v, "schedule", json!("0 3 * * *"));
        set_in(&form, &mut v, "command", json!("SELECT 1"));
        let id = save(&mut s, "job", None, &v).await;
        assert_eq!(cron_job_row(&mut admin, &id).await[4], json!(USER));
        // The user's own job: changed and deleted, never run now (pg_cron
        // runs a job only on its schedule; pgAgent isn't this user's).
        assert!(info.kinds[0].actions.is_empty(), "{info:?}");
        let it = item(&mut s, "job", &id).await;
        assert_eq!(
            (it.can_edit, it.can_delete, it.actions),
            (Some(true), Some(true), offers(&[]))
        );
        // Rescheduled by its name: the same id.
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "schedule", json!("0 4 * * *"));
        let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
        assert!(script.contains("SELECT cron.schedule(job_name := E'dbine_test_sched_mine'"), "{script}");
        assert_eq!(save(&mut s, "job", Some(&id), &v).await, id);
        assert_eq!(cron_job_row(&mut admin, &id).await[1], json!("0 4 * * *"));
        // Inactive: not without alter_job.
        let mut off = v.clone();
        set(&mut off, "active", json!(false));
        assert!(s.scheduler_script("job", Some(&id), &off).await.is_err());
        // A new name: scheduled again, another id.
        set(&mut v, "name", json!("dbine_test_sched_mine2"));
        let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
        assert!(script.contains("cron.unschedule"), "{script}");
        let id2 = save(&mut s, "job", Some(&id), &v).await;
        assert_ne!(id2, id);
        assert!(cron_job_row(&mut admin, &id).await.is_empty());
        assert_eq!(cron_job_row(&mut admin, &id2).await[0], json!("dbine_test_sched_mine2"));
        s.scheduler_delete("job", &id2).await.expect("delete");
        assert!(cron_job_row(&mut admin, &id2).await.is_empty());
    })
    .await;
}

// ---------------------------------------------------------------------------
// pgAgent
// ---------------------------------------------------------------------------

const AGENT_CLEANUP: &str = r"DELETE FROM pgagent.pga_job WHERE jobname LIKE 'dbine\_test\_sched\_%';
DELETE FROM pgagent.pga_jobclass WHERE jclname LIKE 'dbine\_test\_sched\_%';";
const AGENT_LEFT: &str = r"SELECT (SELECT count(*) FROM pgagent.pga_job WHERE jobname LIKE 'dbine\_test\_sched\_%')
     + (SELECT count(*) FROM pgagent.pga_jobclass WHERE jclname LIKE 'dbine\_test\_sched\_%')";

const AGENT_JOB: &str = "dbine_test_sched_agent";
const CLASS: &str = "dbine_test_sched_class";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pgagent_managed_whole() {
    let Some(cfg) = cfg("postgres", "DBINE_TEST_PGCRON_URL") else {
        return;
    };
    let c2 = cfg.clone();
    with_cleanup(&cfg, AGENT_CLEANUP, AGENT_LEFT, async move {
        let cfg = c2;
        let mut s = connect(&cfg).await;
        run(&mut s, AGENT_CLEANUP).await;
        let info = s.scheduler_info().await.unwrap();
        let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
        assert!(keys.starts_with(&["job", "job_class"]), "{keys:?}");

        // A job class.
        let form = s.scheduler_form("job_class", None).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "name", json!(format!("{CLASS}_x")));
        let class_id = save(&mut s, "job_class", None, &v).await;
        let form = s.scheduler_form("job_class", Some(&class_id)).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "name", json!(CLASS));
        assert_eq!(save(&mut s, "job_class", Some(&class_id), &v).await, class_id);
        let items = s.scheduler_items("job_class").await.unwrap();
        assert!(items.items.iter().any(|i| i.id == class_id && i.name == CLASS));

        // A job with three kinds of step, two schedules and exceptions.
        let form = s.scheduler_form("job", None).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "source", json!("pgagent"));
        set_in(&form, &mut v, "name", json!(AGENT_JOB));
        set_in(&form, &mut v, "job_class", json!(CLASS));
        set_in(&form, &mut v, "description", json!("DBine's test"));
        v.lists.insert(
            "steps".into(),
            vec![
                new_row(&form, "steps", json!({"name": "01 local", "kind": "sql", "dbname": "postgres", "code": "SELECT 1"})),
                new_row(&form, "steps", json!({"name": "02 remote", "kind": "remote",
                    "connstr": "host=127.0.0.1 dbname=postgres user=postgres", "password": "pw'x", "code": "SELECT 2", "on_error": "i"})),
                new_row(&form, "steps", json!({"name": "03 batch", "kind": "batch", "code_batch": "echo dbine", "enabled": false})),
            ],
        );
        v.lists.insert(
            "schedules".into(),
            vec![
                new_row(&form, "schedules", json!({"name": "s1", "start": "2026-01-01T00:00:00Z",
                    "minutes": ["0", "30"], "hours": ["3"], "weekdays": ["1", "5"]})),
                new_row(&form, "schedules", json!({"name": "s2", "monthdays": ["32"], "end": "2030-01-01T00:00:00Z"})),
            ],
        );
        v.lists.insert(
            "exceptions".into(),
            vec![
                new_row(&form, "exceptions", json!({"schedule": "s1", "date": "2031-12-25"})),
                new_row(&form, "exceptions", json!({"schedule": "s2", "date": "2031-12-26", "time": "10:17"})),
            ],
        );
        let script = s.scheduler_script("job", None, &v).await.unwrap();
        assert!(script.contains("password=********") && !script.contains("pw'x") && !script.contains("pw\\'x"), "{script}");
        let id = save(&mut s, "job", None, &v).await;
        assert!(id.starts_with("pgagent:"), "{id}");
        let n = number(&id).to_string();
        let r = rows(&mut s, &format!(
            "SELECT j.jobname, c.jclname, j.jobdesc, j.jobenabled FROM pgagent.pga_job j JOIN pgagent.pga_jobclass c ON c.jclid = j.jobjclid WHERE j.jobid = {n}"
        )).await;
        assert_eq!(r[0], [json!(AGENT_JOB), json!(CLASS), json!("DBine's test"), json!("t")]);
        let r = rows(&mut s, &format!(
            "SELECT jstname, jstkind::text, jstdbname::text, jstconnstr, jstonerror::text, jstenabled FROM pgagent.pga_jobstep WHERE jstjobid = {n} ORDER BY jstname"
        )).await;
        assert_eq!(r[0], [json!("01 local"), json!("s"), json!("postgres"), json!(""), json!("f"), json!("t")]);
        assert_eq!(r[1][3], json!("host=127.0.0.1 dbname=postgres user=postgres password='pw\\'x'"));
        assert_eq!(r[1][4], json!("i"));
        assert_eq!(r[2], [json!("03 batch"), json!("b"), json!(""), json!(""), json!("f"), json!("f")]);
        let r = rows(&mut s, &format!(
            "SELECT jscname, jscminutes[1]::text || jscminutes[31]::text, jschours[4], jscweekdays[2], jscweekdays[6], jscmonthdays[32],
                    to_char(jscstart AT TIME ZONE 'UTC', 'YYYY-MM-DD'), jscend IS NOT NULL
             FROM pgagent.pga_schedule WHERE jscjobid = {n} ORDER BY jscname"
        )).await;
        assert_eq!(r[0], [json!("s1"), json!("truetrue"), json!("t"), json!("t"), json!("t"), json!("f"), json!("2026-01-01"), json!("f")]);
        assert_eq!(r[1][5], json!("t"));
        assert_eq!(r[1][7], json!("t"));
        let r = rows(&mut s, &format!(
            "SELECT s.jscname, e.jexdate::text, e.jextime::text FROM pgagent.pga_exception e JOIN pgagent.pga_schedule s ON s.jscid = e.jexscid
             WHERE s.jscjobid = {n} ORDER BY 1"
        )).await;
        assert_eq!(r, vec![
            vec![json!("s1"), json!("2031-12-25"), Value::Null],
            vec![json!("s2"), json!("2031-12-26"), json!("10:17:00")],
        ]);

        // The form as the server has it: the password stays out.
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        let steps = &form.sections.iter().flat_map(|x| x.lists.iter()).find(|l| l.key == "steps").unwrap().rows;
        assert_eq!(steps[1]["connstr"], json!("host=127.0.0.1 dbname=postgres user=postgres"));
        assert_eq!(steps[1]["password"], Value::Null);
        assert!(!serde_json::to_string(&form).unwrap().contains("pw'x"));
        // Unchanged.
        let script = s.scheduler_script("job", Some(&id), &values_of(&form)).await.unwrap();
        assert!(script.contains("no hay cambios"), "{script}");

        // Edit: a step changed, one removed, one added; the remote one to
        // another host keeps its password; schedules and exceptions.
        let mut v = values_of(&form);
        set(&mut v, "enabled", json!(false));
        {
            let steps = v.lists.get_mut("steps").unwrap();
            steps[0].insert("code".into(), json!("SELECT 11"));
            steps[1].insert("connstr".into(), json!("host=localhost dbname=postgres user=postgres"));
            steps.remove(2);
        }
        let row = new_row(&form, "steps", json!({"name": "04 new", "kind": "sql", "dbname": "postgres", "code": "SELECT 4"}));
        v.lists.get_mut("steps").unwrap().push(row);
        {
            let schedules = v.lists.get_mut("schedules").unwrap();
            schedules[0].insert("hours".into(), json!(["4", "5"]));
            schedules.remove(1);
        }
        {
            let exceptions = v.lists.get_mut("exceptions").unwrap();
            exceptions.retain(|e| e["schedule"] == json!("s1"));
            exceptions.push(BTreeMap::from([
                ("schedule".to_string(), json!("s1")),
                ("time".to_string(), json!("03:31:00")),
            ]));
        }
        let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
        assert!(script.contains("password=********") && !script.contains("pw"), "{script}");
        assert_eq!(save(&mut s, "job", Some(&id), &v).await, id);
        let r = rows(&mut s, &format!(
            "SELECT jstname, jstcode, jstconnstr FROM pgagent.pga_jobstep WHERE jstjobid = {n} ORDER BY jstname"
        )).await;
        assert_eq!(r.len(), 3);
        assert_eq!(r[0][1], json!("SELECT 11"));
        assert_eq!(r[1][2], json!("host=localhost dbname=postgres user=postgres password='pw\\'x'"));
        assert_eq!(r[2][0], json!("04 new"));
        let r = rows(&mut s, &format!(
            "SELECT count(*), bool_and(jschours[5] AND jschours[6]) FROM pgagent.pga_schedule WHERE jscjobid = {n}"
        )).await;
        assert_eq!(r[0], [json!("1"), json!("t")]);
        let ex = scalar(&mut s, &format!(
            "SELECT count(*) FROM pgagent.pga_exception e JOIN pgagent.pga_schedule s ON s.jscid = e.jexscid WHERE s.jscjobid = {n}"
        )).await;
        assert_eq!(ex, "2");
        assert_eq!(scalar(&mut s, &format!("SELECT jobenabled FROM pgagent.pga_job WHERE jobid = {n}")).await, "f");

        // The list's order has to be the names' order.
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        let mut v = values_of(&form);
        v.lists.get_mut("steps").unwrap()[0].insert("name".into(), json!("zz last"));
        let e = s.scheduler_script("job", Some(&id), &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("iría antes")), "{e:?}");
        // The password removed.
        let mut v = values_of(&form);
        v.lists.get_mut("steps").unwrap()[1].insert("drop_password".into(), json!(true));
        save(&mut s, "job", Some(&id), &v).await;
        let c = scalar(&mut s, &format!("SELECT jstconnstr FROM pgagent.pga_jobstep WHERE jstjobid = {n} AND jstname = '02 remote'")).await;
        assert_eq!(c, "host=localhost dbname=postgres user=postgres");
        // A form older than the job is refused.
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        run(&mut s, &format!("UPDATE pgagent.pga_job SET jobchanged = jobchanged + interval '1 second' WHERE jobid = {n}")).await;
        let mut v = values_of(&form);
        set(&mut v, "description", json!("x"));
        let e = s.scheduler_script("job", Some(&id), &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("cambió en el servidor")), "{e:?}");

        // History: a run of it, cleared (the running one stays).
        run(&mut s, &format!(
            "INSERT INTO pgagent.pga_joblog (jlgjobid, jlgstatus, jlgstart, jlgduration) VALUES ({n}, 's', now(), interval '1 second');
             INSERT INTO pgagent.pga_jobsteplog (jsljlgid, jsljstid, jslstatus, jslresult, jslstart, jslduration, jsloutput)
               SELECT currval(pg_get_serial_sequence('pgagent.pga_joblog', 'jlgid')), jstid, 's', 0, now(), interval '1 second', ''
               FROM pgagent.pga_jobstep WHERE jstjobid = {n} LIMIT 1;"
        )).await;
        s.clear_job_history(&id).await.expect("clear");
        assert_eq!(scalar(&mut s, &format!("SELECT count(*) FROM pgagent.pga_joblog WHERE jlgjobid = {n}")).await, "0");
        assert!(matches!(s.run_job_from(&id, "01 local").await, Err(Error::Unsupported(_))));

        // A class in use can't go.
        let e = s.scheduler_delete_script("job_class", &class_id).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("cambiales la clase")), "{e:?}");
        // Delete the job, then the class.
        let script = s.scheduler_delete_script("job", &id).await.unwrap();
        assert!(script.contains(&format!("DELETE FROM pgagent.pga_job WHERE jobid = {n};")), "{script}");
        s.scheduler_delete("job", &id).await.expect("delete");
        assert_eq!(scalar(&mut s, &format!("SELECT count(*) FROM pgagent.pga_jobstep WHERE jstjobid = {n}")).await, "0");
        s.scheduler_delete("job_class", &class_id).await.expect("delete class");
        assert!(s.scheduler_form("job_class", Some(&class_id)).await.is_err());
    })
    .await;
}

// ---------------------------------------------------------------------------
// TimescaleDB
// ---------------------------------------------------------------------------

const TS_SETUP: &str = r"CREATE TABLE dbine_test_sched_ht (t timestamptz NOT NULL, v int);
SELECT create_hypertable('dbine_test_sched_ht', 't');
CREATE INDEX dbine_test_sched_ix ON dbine_test_sched_ht (v, t);
ALTER TABLE dbine_test_sched_ht SET (timescaledb.compress);
CREATE TABLE dbine_test_sched_ht2 (t timestamptz NOT NULL, v int);
SELECT create_hypertable('dbine_test_sched_ht2', 't');
ALTER TABLE dbine_test_sched_ht2 SET (timescaledb.compress);
CREATE TABLE dbine_test_sched_ints (t bigint NOT NULL, v int);
SELECT create_hypertable('dbine_test_sched_ints', 't', chunk_time_interval => 1000);
CREATE FUNCTION dbine_test_sched_now() RETURNS bigint LANGUAGE sql STABLE AS $$SELECT 0::bigint$$;
SELECT set_integer_now_func('dbine_test_sched_ints', 'dbine_test_sched_now');
CREATE MATERIALIZED VIEW dbine_test_sched_cagg WITH (timescaledb.continuous) AS
  SELECT time_bucket('1 hour', t) AS b, count(*) FROM dbine_test_sched_ht GROUP BY 1 WITH NO DATA;
CREATE PROCEDURE dbine_test_sched_proc(job_id int, config jsonb) LANGUAGE plpgsql AS $$BEGIN NULL; END$$;
CREATE PROCEDURE dbine_test_sched_proc2(job_id int, config jsonb) LANGUAGE plpgsql AS $$BEGIN NULL; END$$;
CREATE FUNCTION dbine_test_sched_chk(config jsonb) RETURNS void LANGUAGE plpgsql AS $$BEGIN NULL; END$$;";

const TS_CLEANUP: &str = r"DELETE FROM _timescaledb_internal.bgw_job_stat_history WHERE job_id IN
  (SELECT job_id FROM timescaledb_information.jobs WHERE application_name LIKE 'dbine\_test\_sched\_%'
      OR hypertable_name LIKE 'dbine\_test\_sched\_%' OR proc_name LIKE 'dbine\_test\_sched\_%');
SELECT delete_job(job_id) FROM timescaledb_information.jobs WHERE application_name LIKE 'dbine\_test\_sched\_%'
   OR proc_name LIKE 'dbine\_test\_sched\_%';
DROP MATERIALIZED VIEW IF EXISTS dbine_test_sched_cagg;
DROP TABLE IF EXISTS dbine_test_sched_ht, dbine_test_sched_ht2, dbine_test_sched_ints;
DROP FUNCTION IF EXISTS dbine_test_sched_now();
DROP PROCEDURE IF EXISTS dbine_test_sched_proc(int, jsonb);
DROP PROCEDURE IF EXISTS dbine_test_sched_proc2(int, jsonb);
DROP FUNCTION IF EXISTS dbine_test_sched_chk(jsonb);";

const TS_LEFT: &str = r"SELECT (SELECT count(*) FROM timescaledb_information.jobs WHERE application_name LIKE 'dbine\_test\_sched\_%'
        OR hypertable_name LIKE 'dbine\_test\_sched\_%' OR proc_name LIKE 'dbine\_test\_sched\_%')
     + (SELECT count(*) FROM pg_class WHERE relname LIKE 'dbine\_test\_sched\_%')
     + (SELECT count(*) FROM pg_proc WHERE proname LIKE 'dbine\_test\_sched\_%')";

async fn ts_row(s: &mut Box<dyn Session>, id: &str) -> Vec<Value> {
    rows(
        s,
        &format!(
            "SELECT j.application_name::text, j.proc_name::text, j.schedule_interval::text, j.max_retries, j.scheduled,
                    j.config::text, j.check_name::text, b.timezone, j.hypertable_name::text, j.fixed_schedule
             FROM timescaledb_information.jobs j JOIN _timescaledb_config.bgw_job b ON b.id = j.job_id
             WHERE j.job_id = {}",
            number(id)
        ),
    )
    .await
    .first()
    .cloned()
    .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescaledb_managed_whole() {
    let Some(cfg) = cfg("timescaledb", "DBINE_TEST_TIMESCALEDB_URL") else {
        return;
    };
    let c2 = cfg.clone();
    with_cleanup(&cfg, TS_CLEANUP, TS_LEFT, async move {
        let cfg = c2;
        let mut s = connect(&cfg).await;
        run(&mut s, TS_CLEANUP).await;
        run(&mut s, TS_SETUP).await;
        let info = s.scheduler_info().await.unwrap();
        let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
        for k in ["job", "compression_policy", "retention_policy", "refresh_policy", "reorder_policy"] {
            assert!(keys.contains(&k), "{keys:?}");
        }
        let compaction = keys.contains(&"compaction_policy");

        // A user-defined job.
        let form = s.scheduler_form("job", None).await.unwrap();
        let mut v = values_of(&form);
        let proc = r#""public"."dbine_test_sched_proc""#;
        assert!(form.choices["procedure"].iter().any(|(x, _)| x == proc), "{:?}", form.choices["procedure"]);
        set_in(&form, &mut v, "name", json!("dbine_test_sched_job"));
        set_in(&form, &mut v, "proc", json!(proc));
        set_in(&form, &mut v, "schedule_interval", json!("1 hour"));
        set_in(&form, &mut v, "config", json!("{\"a\": 1}"));
        set_in(&form, &mut v, "timezone", json!("Europe/Madrid"));
        set_in(&form, &mut v, "check_config", json!(r#""public"."dbine_test_sched_chk""#));
        set_in(&form, &mut v, "max_retries", json!(3));
        let mut bad = v.clone();
        set_in(&form, &mut bad, "schedule_interval", json!("every hour"));
        let e = s.scheduler_script("job", None, &bad).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("Intervalo")), "{e:?}");
        let mut bad = v.clone();
        set_in(&form, &mut bad, "timezone", json!("Mars/Olympus"));
        assert!(s.scheduler_script("job", None, &bad).await.is_err());
        let id = save(&mut s, "job", None, &v).await;
        assert!(id.starts_with("timescaledb:"), "{id}");
        let r = ts_row(&mut s, &id).await;
        assert_eq!(r[0], json!(format!("dbine_test_sched_job [{}]", number(&id))));
        assert_eq!(&r[1..8], [json!("dbine_test_sched_proc"), json!("01:00:00"), json!("3"), json!("t"), json!("{\"a\": 1}"), json!("dbine_test_sched_chk"), json!("Europe/Madrid")]);
        // Edit: interval, disabled; the time zone stays.
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        assert_eq!(form.values["name"], json!("dbine_test_sched_job"));
        let mut v = values_of(&form);
        set(&mut v, "schedule_interval", json!("2 hours"));
        set(&mut v, "scheduled", json!(false));
        set(&mut v, "check_config", json!(""));
        let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
        assert!(script.contains("timezone => E'Europe/Madrid'") && script.contains("check_config => 0"), "{script}");
        assert_eq!(save(&mut s, "job", Some(&id), &v).await, id);
        let r = ts_row(&mut s, &id).await;
        assert_eq!((&r[2], &r[4], &r[6], &r[7]), (&json!("02:00:00"), &json!("f"), &Value::Null, &json!("Europe/Madrid")));
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        let script = s.scheduler_script("job", Some(&id), &values_of(&form)).await.unwrap();
        assert!(script.contains("no hay cambios"), "{script}");
        // Another procedure: a new job.
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "proc", json!(r#""public"."dbine_test_sched_proc2""#));
        let id2 = save(&mut s, "job", Some(&id), &v).await;
        assert_ne!(id2, id);
        assert!(ts_row(&mut s, &id).await.is_empty());
        assert_eq!(ts_row(&mut s, &id2).await[1], json!("dbine_test_sched_proc2"));
        // History: a run of it, cleared.
        run(&mut s, &format!(
            "INSERT INTO _timescaledb_internal.bgw_job_stat_history (job_id, pid, execution_start, execution_finish, succeeded)
             VALUES ({}, 1, now(), now(), true)",
            number(&id2)
        )).await;
        s.clear_job_history(&id2).await.expect("clear");
        let n = scalar(&mut s, &format!("SELECT count(*) FROM _timescaledb_internal.bgw_job_stat_history WHERE job_id = {}", number(&id2))).await;
        assert_eq!(n, "0");
        s.scheduler_delete("job", &id2).await.expect("delete");
        assert!(ts_row(&mut s, &id2).await.is_empty());

        // Compression: after → created before → another hypertable.
        let ht = r#""public"."dbine_test_sched_ht""#;
        let form = s.scheduler_form("compression_policy", None).await.unwrap();
        assert!(form.choices["compressed_hypertable"].iter().any(|(x, _)| x == ht));
        let mut v = values_of(&form);
        set(&mut v, "target", json!(ht));
        set(&mut v, "compress_after", json!("7 days"));
        set(&mut v, "name", json!("dbine_test_sched_compress"));
        let mut bad = v.clone();
        set(&mut bad, "compress_created_before", json!("1 day"));
        assert!(s.scheduler_script("compression_policy", None, &bad).await.is_err());
        let pid = save(&mut s, "compression_policy", None, &v).await;
        let r = ts_row(&mut s, &pid).await;
        assert_eq!(r[1], json!("policy_compression"));
        assert!(r[5].as_str().unwrap().contains("\"compress_after\": \"7 days\""), "{r:?}");
        assert!(s.scheduler_items("compression_policy").await.unwrap().items.iter().any(|i| i.id == pid && i.name == "public.dbine_test_sched_ht"));
        // The job's id opens the policy's form.
        let form = s.scheduler_form("job", Some(&pid)).await.unwrap();
        assert_eq!(form.values["target"], json!(ht));
        assert_eq!(form.values["compress_after"], json!("7 days"));
        let mut v = values_of(&form);
        set(&mut v, "compress_after", json!(""));
        set(&mut v, "compress_created_before", json!("10 days"));
        set(&mut v, "max_retries", json!(2));
        assert_eq!(save(&mut s, "compression_policy", Some(&pid), &v).await, pid);
        let r = ts_row(&mut s, &pid).await;
        assert!(r[5].as_str().unwrap().contains("\"compress_created_before\": \"10 days\"") && !r[5].as_str().unwrap().contains("compress_after"), "{r:?}");
        assert_eq!(r[3], json!("2"));
        set(&mut v, "target", json!(r#""public"."dbine_test_sched_ht2""#));
        let pid2 = save(&mut s, "compression_policy", Some(&pid), &v).await;
        assert_ne!(pid2, pid);
        assert!(ts_row(&mut s, &pid).await.is_empty());
        assert_eq!(ts_row(&mut s, &pid2).await[8], json!("dbine_test_sched_ht2"));
        s.scheduler_delete("compression_policy", &pid2).await.expect("delete");
        assert!(ts_row(&mut s, &pid2).await.is_empty());

        // Retention on integer time.
        let form = s.scheduler_form("retention_policy", None).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "target", json!(r#""public"."dbine_test_sched_ints""#));
        set(&mut v, "drop_after", json!("100000"));
        set(&mut v, "schedule_interval", json!("1 day"));
        let rid = save(&mut s, "retention_policy", None, &v).await;
        let r = ts_row(&mut s, &rid).await;
        assert!(r[5].as_str().unwrap().contains("\"drop_after\": 100000"), "{r:?}");
        assert_eq!(r[2], json!("1 day"));
        let script = s.scheduler_delete_script("retention_policy", &rid).await.unwrap();
        assert!(script.contains("remove_retention_policy"), "{script}");
        s.scheduler_delete("retention_policy", &rid).await.expect("delete");

        // Continuous aggregate refresh: the window opened at its start.
        let form = s.scheduler_form("refresh_policy", None).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "target", json!(r#""public"."dbine_test_sched_cagg""#));
        set(&mut v, "start_offset", json!("3 days"));
        set(&mut v, "end_offset", json!("1 hour"));
        set(&mut v, "schedule_interval", json!("1 hour"));
        let fid = save(&mut s, "refresh_policy", None, &v).await;
        let form = s.scheduler_form("refresh_policy", Some(&fid)).await.unwrap();
        assert_eq!(form.values["end_offset"], json!("01:00:00"));
        let mut v = values_of(&form);
        set(&mut v, "start_offset", json!(""));
        save(&mut s, "refresh_policy", Some(&fid), &v).await;
        let r = ts_row(&mut s, &fid).await;
        assert!(r[5].as_str().unwrap().contains("\"start_offset\": null"), "{r:?}");
        s.scheduler_delete("refresh_policy", &fid).await.expect("delete");
        // A new one with its window open: the offsets go as NULL (they have
        // no default), and its interval is required.
        let form = s.scheduler_form("refresh_policy", None).await.unwrap();
        assert!(field(&form, "schedule_interval").required);
        let mut v = values_of(&form);
        set(&mut v, "target", json!(r#""public"."dbine_test_sched_cagg""#));
        set(&mut v, "end_offset", json!("1 hour"));
        let e = s.scheduler_script("refresh_policy", None, &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("«Intervalo»")), "{e:?}");
        set(&mut v, "schedule_interval", json!("1 hour"));
        let mut bad = v.clone();
        set(&mut bad, "buckets_per_batch", json!(-1));
        let e = s.scheduler_script("refresh_policy", None, &bad).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("«Buckets por lote»")), "{e:?}");
        let script = s.scheduler_script("refresh_policy", None, &v).await.unwrap();
        assert!(script.contains("start_offset => NULL"), "{script}");
        let fid = save(&mut s, "refresh_policy", None, &v).await;
        let r = ts_row(&mut s, &fid).await;
        assert!(r[5].as_str().unwrap().contains("\"start_offset\": null"), "{r:?}");
        s.scheduler_delete("refresh_policy", &fid).await.expect("delete");
        set(&mut v, "end_offset", json!(""));
        let fid = save(&mut s, "refresh_policy", None, &v).await;
        let r = ts_row(&mut s, &fid).await;
        let cfg = r[5].as_str().unwrap();
        assert!(cfg.contains("\"start_offset\": null") && cfg.contains("\"end_offset\": null"), "{r:?}");
        s.scheduler_delete("refresh_policy", &fid).await.expect("delete");

        // Reorder: the index has to be the hypertable's.
        let form = s.scheduler_form("reorder_policy", None).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "target", json!(ht));
        set(&mut v, "index_name", json!("nope"));
        let e = s.scheduler_script("reorder_policy", None, &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("no tiene un índice")), "{e:?}");
        set(&mut v, "index_name", json!("dbine_test_sched_ix"));
        let oid = save(&mut s, "reorder_policy", None, &v).await;
        assert!(ts_row(&mut s, &oid).await[5].as_str().unwrap().contains("dbine_test_sched_ix"));
        s.scheduler_delete("reorder_policy", &oid).await.expect("delete");

        // Compaction, where this version has it.
        if compaction {
            let form = s.scheduler_form("compaction_policy", None).await.unwrap();
            let mut v = values_of(&form);
            set(&mut v, "target", json!(ht));
            set(&mut v, "max_chunks", json!(5));
            set(&mut v, "inactive_for", json!("2 hours"));
            let cid = save(&mut s, "compaction_policy", None, &v).await;
            let r = ts_row(&mut s, &cid).await;
            assert!(r[5].as_str().unwrap().contains("\"max_chunks\": 5"), "{r:?}");
            s.scheduler_delete("compaction_policy", &cid).await.expect("delete");
        }

        // The extension's own jobs: rescheduled, never deleted.
        let form = s.scheduler_form("job", Some("timescaledb:3")).await.unwrap();
        assert!(form.notes.iter().any(|n| n.contains("no borrarlo")), "{:?}", form.notes);
        assert!(s.scheduler_delete_script("job", "timescaledb:3").await.is_err());
        // A policy's id under another policy kind.
        assert!(s.scheduler_form("retention_policy", Some("timescaledb:3")).await.is_err());
    })
    .await;
}

// ---------------------------------------------------------------------------
// Apache Cloudberry's tasks
// ---------------------------------------------------------------------------

const TASK_CLEANUP: &str = r"DO $$DECLARE t record;
BEGIN
  FOR t IN SELECT jobname FROM pg_catalog.pg_task WHERE jobname LIKE 'dbine\_test\_sched\_%' AND username = current_user LOOP
    EXECUTE format('DROP TASK %I', t.jobname);
  END LOOP;
END$$;";
const TASK_LEFT: &str =
    r"SELECT count(*) FROM pg_catalog.pg_task WHERE jobname LIKE 'dbine\_test\_sched\_%'";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cloudberry_tasks_managed_whole() {
    let Some(cfg) = cfg("cloudberry", "DBINE_TEST_CLOUDBERRY_URL") else {
        return;
    };
    let c2 = cfg.clone();
    with_cleanup(&cfg, TASK_CLEANUP, TASK_LEFT, async move {
        let cfg = c2;
        let mut s = connect(&cfg).await;
        run(&mut s, TASK_CLEANUP).await;
        let db = scalar(&mut s, "SELECT current_database()").await;
        let info = s.scheduler_info().await.unwrap();
        assert!(info.kinds[0].can_create, "{info:?}");
        let form = s.scheduler_form("job", None).await.unwrap();
        let mut v = values_of(&form);
        if form.sections.iter().flat_map(|x| x.fields.iter()).any(|f| f.key == "source") {
            set(&mut v, "source", json!("task"));
        }
        set_in(&form, &mut v, "name", json!("dbine_test_sched_task"));
        set_in(&form, &mut v, "schedule", json!("0 3 * * *"));
        set_in(&form, &mut v, "command", json!("SELECT 'dbine'"));
        set_in(&form, &mut v, "active", json!(false));
        let id = save(&mut s, "job", None, &v).await;
        assert!(id.starts_with("task:"), "{id}");
        // Tasks run only on their schedule, and their history is a system
        // catalog: no actions, nothing to clear (unless pg_cron is here too).
        if info.kinds[0].actions.is_empty() {
            assert!(!info.kinds[0].clear_history, "{info:?}");
        }
        assert!(!info.kinds[0].run_from_step);
        let it = item(&mut s, "job", &id).await;
        assert_eq!(
            (it.can_edit, it.can_delete, it.actions),
            (Some(true), Some(true), offers(&[]))
        );
        let e = s.scheduler_action_script("job", &id, "run", &Default::default()).await.unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("tarea")), "{e:?}");
        let row = |n: &str| format!("SELECT jobname, schedule, command, database, active FROM pg_catalog.pg_task WHERE jobid = {n}");
        let r = rows(&mut s, &row(number(&id))).await;
        assert_eq!(r[0], [json!("dbine_test_sched_task"), json!("0 3 * * *"), json!("SELECT 'dbine'"), json!(db), json!("f")]);
        // In place.
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "schedule", json!("10 seconds"));
        set(&mut v, "command", json!("SELECT 2"));
        set(&mut v, "active", json!(true));
        let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
        assert!(script.contains("ALTER TASK \"dbine_test_sched_task\" SCHEDULE E'10 seconds' ACTIVE AS $cmd$SELECT 2$cmd$;"), "{script}");
        assert_eq!(save(&mut s, "job", Some(&id), &v).await, id);
        let r = rows(&mut s, &row(number(&id))).await;
        assert_eq!((&r[0][1], &r[0][2], &r[0][4]), (&json!("10 seconds"), &json!("SELECT 2"), &json!("t")));
        // Renamed: created again, another id.
        set(&mut v, "name", json!("dbine_test_sched_task2"));
        set(&mut v, "active", json!(false));
        let id2 = save(&mut s, "job", Some(&id), &v).await;
        assert_ne!(id2, id);
        assert!(rows(&mut s, &row(number(&id))).await.is_empty());
        assert_eq!(rows(&mut s, &row(number(&id2))).await[0][0], json!("dbine_test_sched_task2"));
        assert!(matches!(s.clear_job_history(&id2).await, Err(Error::Unsupported(_))));
        let script = s.scheduler_delete_script("job", &id2).await.unwrap();
        assert!(script.contains("DROP TASK \"dbine_test_sched_task2\";"), "{script}");
        s.scheduler_delete("job", &id2).await.expect("delete");
        assert!(rows(&mut s, &row(number(&id2))).await.is_empty());
    })
    .await;
}

// ---------------------------------------------------------------------------
// Servers without a scheduler here
// ---------------------------------------------------------------------------

async fn nothing_on(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else { return };
    let mut s = connect(&cfg).await;
    let found = scalar(
        &mut s,
        "SELECT count(*) FROM pg_extension WHERE extname IN ('pg_cron', 'pgagent', 'timescaledb')",
    )
    .await;
    if found != "0" {
        eprintln!("{driver}: has a scheduler; skipping");
        return;
    }
    let info = s.scheduler_info().await.expect("info");
    assert_eq!(info.kinds.len(), 1, "{info:?}");
    let k = &info.kinds[0];
    assert!(
        k.key == "job" && !k.can_create && !k.can_edit && !k.can_delete,
        "{info:?}"
    );
    assert!(matches!(
        s.scheduler_form("job", None).await,
        Err(Error::Unsupported(_))
    ));
    let items = s.scheduler_items("job").await.unwrap();
    assert!(
        items.items.is_empty() && !items.notes.is_empty(),
        "{items:?}"
    );
    assert!(s
        .scheduler_items("job_class")
        .await
        .unwrap()
        .items
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres_without_a_scheduler() {
    nothing_on("postgres", "DBINE_TEST_POSTGRES_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn greengage_without_pg_cron() {
    nothing_on("greengage", "DBINE_TEST_GREENGAGE_URL").await;
}

// ---------------------------------------------------------------------------
// CockroachDB
// ---------------------------------------------------------------------------

const CRDB_DB: &str = "dbine_test_sched_db";
const CRDB_SETUP: &str = "CREATE DATABASE IF NOT EXISTS dbine_test_sched_db;
CREATE TABLE IF NOT EXISTS dbine_test_sched_db.t (id INT PRIMARY KEY, v STRING, created TIMESTAMPTZ NOT NULL DEFAULT now());
CREATE TABLE IF NOT EXISTS dbine_test_sched_db.u (id INT PRIMARY KEY);
SET CLUSTER SETTING kv.rangefeed.enabled = true";
const CRDB_CLEANUP: &str = "DROP SCHEDULES SELECT id FROM [SHOW SCHEDULES] WHERE label LIKE 'dbine_test_sched%';
CANCEL JOBS SELECT job_id FROM [SHOW CHANGEFEED JOBS] WHERE description LIKE '%dbine_test_sched%' AND status NOT IN ('canceled', 'failed', 'succeeded', 'cancel-requested', 'reverting');
DROP DATABASE IF EXISTS dbine_test_sched_db CASCADE";
const CRDB_LEFT: &str = "SELECT (SELECT count(*) FROM [SHOW SCHEDULES] WHERE label LIKE 'dbine_test_sched%')
  + (SELECT count(*) FROM [SHOW CHANGEFEED JOBS] WHERE description LIKE '%dbine_test_sched%' AND status NOT IN ('canceled', 'failed', 'succeeded', 'cancel-requested', 'reverting'))
  + (SELECT count(*) FROM [SHOW DATABASES] WHERE database_name = 'dbine_test_sched_db')";

/// [`with_cleanup`] for CockroachDB: each cleanup statement on its own
/// (`SET CLUSTER SETTING` can't share a transaction).
async fn crdb_cleanup(cfg: &ConnectionConfig) -> String {
    let mut s = connect(cfg).await;
    for st in CRDB_CLEANUP.split(";\n") {
        let mut out = QueryOutcome::default();
        let r = s.execute(st, 100, &mut out).await;
        if r.is_err() || !out.errors.is_empty() {
            eprintln!("cleanup failed: {st}: {r:?} {:?}", out.errors);
        }
    }
    scalar(&mut s, CRDB_LEFT).await
}

/// The cluster's SQL stats compaction as the test found it (its
/// recurrence setting, and whether its schedule was paused), to put back.
struct StatsCompaction {
    recurrence: String,
    default: bool,
    paused: bool,
}

const STATS_SCHEDULE: &str = "SELECT id FROM [SHOW SCHEDULES] WHERE label = 'sql-stats-compaction'";

async fn stats_compaction(s: &mut Box<dyn Session>) -> StatsCompaction {
    let r = rows(
        s,
        "SELECT value, origin FROM [SHOW ALL CLUSTER SETTINGS] WHERE variable = 'sql.stats.cleanup.recurrence'",
    )
    .await;
    let cell = |i: usize| {
        r.first()
            .and_then(|row| row.get(i))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let status = scalar(
        s,
        "SELECT schedule_status FROM [SHOW SCHEDULES] WHERE label = 'sql-stats-compaction'",
    )
    .await;
    StatsCompaction {
        recurrence: cell(0),
        default: cell(1) == "default",
        paused: status == "PAUSED",
    }
}

/// Puts the SQL stats compaction back as it was (each statement on its
/// own: `SET CLUSTER SETTING` can't share a transaction).
async fn restore_stats_compaction(cfg: &ConnectionConfig, was: &StatsCompaction) {
    let mut s = connect(cfg).await;
    let setting = if was.default {
        "RESET CLUSTER SETTING sql.stats.cleanup.recurrence".to_string()
    } else {
        format!(
            "SET CLUSTER SETTING sql.stats.cleanup.recurrence = '{}'",
            was.recurrence.replace('\'', "''")
        )
    };
    let schedule = format!(
        "{} SCHEDULES {STATS_SCHEDULE}",
        if was.paused { "PAUSE" } else { "RESUME" }
    );
    for st in [setting, schedule] {
        let mut out = QueryOutcome::default();
        let r = s.execute(&st, 100, &mut out).await;
        if r.is_err() || !out.errors.is_empty() {
            eprintln!("restore failed: {st}: {r:?} {:?}", out.errors);
        }
    }
}

/// A schedule's columns: label, status, recurrence, on_previous_running,
/// on_execution_failure, command.
async fn crdb_sched(s: &mut Box<dyn Session>, id: &str) -> Vec<Value> {
    rows(
        s,
        &format!(
            "SELECT label, schedule_status, recurrence, on_previous_running, on_execution_failure, command::STRING
             FROM [SHOW SCHEDULES] WHERE id = {}",
            number(id)
        ),
    )
    .await
    .first()
    .cloned()
    .unwrap_or_default()
}

async fn crdb_count(s: &mut Box<dyn Session>, label: &str) -> String {
    scalar(
        s,
        &format!("SELECT count(*) FROM [SHOW SCHEDULES] WHERE label = '{label}'"),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroach_managed_whole() {
    let Some(base) = cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL") else {
        return;
    };
    let mut admin = connect(&base).await;
    // What the test changes in the cluster goes back as it was.
    let stats = stats_compaction(&mut admin).await;
    crdb_cleanup(&base).await;
    // Changefeeds need rangefeeds: the setting goes back as it was.
    let rangefeed = scalar(&mut admin, "SHOW CLUSTER SETTING kv.rangefeed.enabled").await;
    for st in CRDB_SETUP.split(";\n") {
        run(&mut admin, st).await;
    }
    let cfg = ConnectionConfig {
        database: CRDB_DB.into(),
        ..base.clone()
    };
    let body = async move {
        let mut s = connect(&cfg).await;
        let info = s.scheduler_info().await.expect("info");
        eprintln!("{info:#?}");
        let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(keys, ["job", "ttl"]);
        assert!(info
            .kinds
            .iter()
            .all(|k| k.can_create && k.can_edit && k.can_delete));

        // A new "job" offers the three kinds.
        let form = s.scheduler_form("job", None).await.expect("new form");
        match &field(&form, "source").kind {
            dbine_driver::FormKind::Select { options } => {
                let keys: Vec<&str> = options.iter().map(|(k, _)| k.as_str()).collect();
                assert_eq!(keys, ["crdb_backup", "crdb_changefeed", "crdb_job"]);
            }
            k => panic!("{k:?}"),
        }

        // ---- A backup schedule: a full + incremental pair.
        let mut v = values_of(&form);
        set(&mut v, "source", json!("crdb_backup"));
        set_in(&form, &mut v, "name", json!("dbine_test_sched_backup"));
        set_in(&form, &mut v, "backup_target", json!("database"));
        set_in(&form, &mut v, "databases", json!([CRDB_DB]));
        set(
            &mut v,
            "locations",
            json!("nodelocal://1/dbine_test_sched_backup"),
        );
        set_in(&form, &mut v, "revision_history", json!(true));
        set_in(&form, &mut v, "encryption", json!("passphrase"));
        set_in(&form, &mut v, "encryption_passphrase", json!("dbine pw 1"));
        set_in(&form, &mut v, "recurring", json!("@daily"));
        set_in(&form, &mut v, "full_backup", json!("cron"));
        set_in(&form, &mut v, "full_cron", json!("@weekly"));
        set_in(&form, &mut v, "first_run", json!("2031-01-01T00:00:00Z"));
        set_in(&form, &mut v, "on_previous_running", json!("skip"));
        let shown = s.scheduler_script("job", None, &v).await.unwrap();
        assert!(
            shown.contains("encryption_passphrase = '********'") && !shown.contains("dbine pw 1"),
            "{shown}"
        );
        let id = save(&mut s, "job", None, &v).await;
        assert!(id.starts_with("crdb_schedule:"), "{id}");
        assert_eq!(crdb_count(&mut s, "dbine_test_sched_backup").await, "2");
        let row = crdb_sched(&mut s, &id).await;
        assert_eq!(row[2], json!("@weekly"), "the full one: {row:?}");
        assert_eq!(row[3], json!("SKIP"), "{row:?}");

        let form = s.scheduler_form("job", Some(&id)).await.expect("form");
        eprintln!("{:#?}", form.values);
        assert!(!form.read_only);
        assert_eq!(form.values["name"], json!("dbine_test_sched_backup"));
        assert_eq!(form.values["recurring"], json!("@daily"));
        assert_eq!(form.values["full_backup"], json!("cron"));
        assert_eq!(form.values["full_cron"], json!("@weekly"));
        assert_eq!(form.values["revision_history"], json!(true));
        assert_eq!(form.values["encryption"], json!("passphrase"));
        assert_eq!(form.values["encryption_passphrase"], Value::Null);
        assert_eq!(form.values["on_previous_running"], json!("skip"));
        assert_eq!(form.values["databases"], json!([CRDB_DB]));

        // Unchanged: nothing to save.
        let same = values_of(&form);
        assert!(s
            .scheduler_script("job", Some(&id), &same)
            .await
            .unwrap()
            .contains("no hay cambios"));

        // In place: label, location, recurrences, failure policy, paused.
        let mut e = values_of(&form);
        set(&mut e, "name", json!("dbine_test_sched_backup2"));
        set(
            &mut e,
            "locations",
            json!("nodelocal://1/dbine_test_sched_backup2"),
        );
        set(&mut e, "recurring", json!("@hourly"));
        set(&mut e, "full_cron", json!("@daily"));
        set(&mut e, "on_execution_failure", json!("pause"));
        set(&mut e, "revision_history", json!(false));
        set(&mut e, "enabled", json!(false));
        let script = s.scheduler_script("job", Some(&id), &e).await.unwrap();
        assert!(
            script.contains(&format!("ALTER BACKUP SCHEDULE {}", number(&id))),
            "{script}"
        );
        let id2 = save(&mut s, "job", Some(&id), &e).await;
        assert_eq!(id2, id);
        let row = crdb_sched(&mut s, &id).await;
        assert_eq!(row[0], json!("dbine_test_sched_backup2"), "{row:?}");
        assert_eq!(row[1], json!("PAUSED"), "{row:?}");
        assert_eq!(row[2], json!("@daily"), "{row:?}");
        assert_eq!(row[4], json!("PAUSE_SCHED"), "{row:?}");
        assert!(
            row[5]
                .as_str()
                .unwrap()
                .contains("dbine_test_sched_backup2"),
            "{row:?}"
        );
        assert_eq!(crdb_count(&mut s, "dbine_test_sched_backup2").await, "2");
        let paused = scalar(
            &mut s,
            "SELECT count(*) FROM [SHOW SCHEDULES] WHERE label = 'dbine_test_sched_backup2' AND schedule_status = 'PAUSED'",
        )
        .await;
        assert_eq!(paused, "2");
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        assert_eq!(form.values["recurring"], json!("@hourly"));
        assert_eq!(form.values["revision_history"], json!(false));

        // Resumed: the full one (the incremental waits for its first full
        // backup).
        let mut e = values_of(&form);
        set(&mut e, "enabled", json!(true));
        save(&mut s, "job", Some(&id), &e).await;
        assert_eq!(crdb_sched(&mut s, &id).await[1], json!("ACTIVE"));

        // What it backs up can't change in place: dropped and created again,
        // which needs the passphrase typed again.
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        let mut e = values_of(&form);
        set(&mut e, "backup_target", json!("table"));
        set(
            &mut e,
            "tables",
            json!("dbine_test_sched_db.public.t\ndbine_test_sched_db.public.u"),
        );
        let err = s.scheduler_script("job", Some(&id), &e).await.unwrap_err();
        assert!(err.to_string().contains("escribirlas"), "{err}");
        set(&mut e, "encryption_passphrase", json!("dbine pw 1"));
        let script = s.scheduler_script("job", Some(&id), &e).await.unwrap();
        assert!(
            script.contains("DROP SCHEDULES") && script.contains("ignore_existing_backups"),
            "{script}"
        );
        let id3 = save(&mut s, "job", Some(&id), &e).await;
        assert_ne!(id3, id);
        assert_eq!(crdb_count(&mut s, "dbine_test_sched_backup2").await, "2");
        let form = s.scheduler_form("job", Some(&id3)).await.unwrap();
        assert_eq!(form.values["backup_target"], json!("table"));
        assert_eq!(
            form.values["tables"],
            json!("dbine_test_sched_db.public.t\ndbine_test_sched_db.public.u")
        );

        // Always full: a single schedule.
        let mut e = values_of(&form);
        set(&mut e, "full_backup", json!("always"));
        save(&mut s, "job", Some(&id3), &e).await;
        assert_eq!(crdb_count(&mut s, "dbine_test_sched_backup2").await, "1");
        let form = s.scheduler_form("job", Some(&id3)).await.unwrap();
        assert_eq!(form.values["full_backup"], json!("always"));

        assert!(matches!(
            s.clear_job_history(&id3).await,
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            s.run_job_from(&id3, "x").await,
            Err(Error::Unsupported(_))
        ));
        let script = s.scheduler_delete_script("job", &id3).await.unwrap();
        assert!(script.contains("DROP SCHEDULES"), "{script}");
        s.scheduler_delete("job", &id3).await.expect("delete");
        assert_eq!(crdb_count(&mut s, "dbine_test_sched_backup2").await, "0");

        // ---- A changefeed schedule.
        let form = s.scheduler_form("job", None).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "source", json!("crdb_changefeed"));
        set_in(&form, &mut v, "name", json!("dbine_test_sched_cf"));
        set_in(
            &form,
            &mut v,
            "targets",
            json!("dbine_test_sched_db.public.t"),
        );
        set_in(
            &form,
            &mut v,
            "sink",
            json!("nodelocal://1/dbine_test_sched_cf"),
        );
        set_in(&form, &mut v, "options", json!("format = csv"));
        set_in(&form, &mut v, "recurring", json!("@daily"));
        set_in(&form, &mut v, "first_run", json!("2031-01-01T00:00:00Z"));
        let cf = save(&mut s, "job", None, &v).await;
        let row = crdb_sched(&mut s, &cf).await;
        assert!(
            row[5].as_str().unwrap().contains("format = 'csv'"),
            "{row:?}"
        );
        let form = s.scheduler_form("job", Some(&cf)).await.unwrap();
        assert_eq!(
            form.values["targets"],
            json!("dbine_test_sched_db.public.t")
        );
        assert_eq!(
            form.values["sink"],
            json!("nodelocal://1/dbine_test_sched_cf")
        );
        assert!(form.values["options"]
            .as_str()
            .unwrap()
            .contains("format = csv"));
        // Paused in place.
        let mut e = values_of(&form);
        set(&mut e, "enabled", json!(false));
        let script = s.scheduler_script("job", Some(&cf), &e).await.unwrap();
        assert!(
            script.contains(&format!("PAUSE SCHEDULE {}", number(&cf))),
            "{script}"
        );
        assert_eq!(save(&mut s, "job", Some(&cf), &e).await, cf);
        assert_eq!(crdb_sched(&mut s, &cf).await[1], json!("PAUSED"));
        // Anything else: dropped and created again, still paused.
        let form = s.scheduler_form("job", Some(&cf)).await.unwrap();
        let mut e = values_of(&form);
        set(&mut e, "options", json!("format = parquet"));
        set(
            &mut e,
            "targets",
            json!("dbine_test_sched_db.public.t\ndbine_test_sched_db.public.u"),
        );
        let cf2 = save(&mut s, "job", Some(&cf), &e).await;
        assert_ne!(cf2, cf);
        let row = crdb_sched(&mut s, &cf2).await;
        assert_eq!(row[1], json!("PAUSED"), "{row:?}");
        assert!(row[5].as_str().unwrap().contains("parquet"), "{row:?}");
        assert!(crdb_sched(&mut s, &cf).await.is_empty());
        s.scheduler_delete("job", &cf2).await.expect("delete");
        assert!(crdb_sched(&mut s, &cf2).await.is_empty());

        // ---- A changefeed (a job that keeps running).
        let form = s.scheduler_form("job", None).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "source", json!("crdb_job"));
        set_in(
            &form,
            &mut v,
            "targets",
            json!("dbine_test_sched_db.public.t"),
        );
        set_in(&form, &mut v, "sink", json!("null://dbine_test_sched_feed"));
        set_in(&form, &mut v, "options", json!("format = json"));
        let feed = save(&mut s, "job", None, &v).await;
        assert!(feed.starts_with("crdb_job:"), "{feed}");
        let status = |n: &str| format!("SELECT status FROM [SHOW CHANGEFEED JOB {n}]");
        let form = s.scheduler_form("job", Some(&feed)).await.unwrap();
        eprintln!("{:#?}", form.values);
        assert_eq!(
            form.values["targets"],
            json!("dbine_test_sched_db.public.t")
        );
        // Changed while running: paused, changed, resumed.
        let mut e = values_of(&form);
        set(
            &mut e,
            "targets",
            json!("dbine_test_sched_db.public.t\ndbine_test_sched_db.public.u"),
        );
        set(&mut e, "options", json!("format = json\nresolved = 30s"));
        let script = s.scheduler_script("job", Some(&feed), &e).await.unwrap();
        assert!(
            script.contains("PAUSE JOB") && script.contains("ALTER CHANGEFEED"),
            "{script}"
        );
        save(&mut s, "job", Some(&feed), &e).await;
        let desc = scalar(
            &mut s,
            &format!(
                "SELECT description FROM [SHOW CHANGEFEED JOB {}]",
                number(&feed)
            ),
        )
        .await;
        assert!(
            desc.contains("resolved = '30s'") && desc.contains(".u"),
            "{desc}"
        );
        assert_ne!(scalar(&mut s, &status(number(&feed))).await, "paused");
        // A change the server refuses (a table that isn't there): the
        // changefeed DBine paused for it runs again, and the error says so.
        let form = s.scheduler_form("job", Some(&feed)).await.unwrap();
        let mut e = values_of(&form);
        set(
            &mut e,
            "targets",
            json!("dbine_test_sched_db.public.t\ndbine_test_sched_db.public.u\ndbine_test_sched_db.public.nope"),
        );
        let err = s
            .scheduler_save("job", Some(&feed), &e)
            .await
            .expect_err("a missing table");
        assert!(
            matches!(&err, Error::Query(m) if m.contains("reanudó el changefeed")),
            "{err:?}"
        );
        let mut st = String::new();
        for _ in 0..120 {
            st = scalar(&mut s, &status(number(&feed))).await;
            if st == "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        assert_eq!(st, "running");
        // Paused.
        let form = s.scheduler_form("job", Some(&feed)).await.unwrap();
        let mut e = values_of(&form);
        set(&mut e, "enabled", json!(false));
        save(&mut s, "job", Some(&feed), &e).await;
        // A changefeed resumed a moment ago can take a while to pause.
        for _ in 0..360 {
            if scalar(&mut s, &status(number(&feed))).await == "paused" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        assert_eq!(scalar(&mut s, &status(number(&feed))).await, "paused");
        // Changed while paused: stays paused.
        let form = s.scheduler_form("job", Some(&feed)).await.unwrap();
        let mut e = values_of(&form);
        set(&mut e, "targets", json!("dbine_test_sched_db.public.t"));
        let script = s.scheduler_script("job", Some(&feed), &e).await.unwrap();
        assert!(
            !script.contains("PAUSE JOB") && !script.contains("RESUME"),
            "{script}"
        );
        save(&mut s, "job", Some(&feed), &e).await;
        assert_eq!(scalar(&mut s, &status(number(&feed))).await, "paused");
        s.scheduler_delete("job", &feed).await.expect("cancel");
        let st = scalar(&mut s, &status(number(&feed))).await;
        assert!(st == "canceled" || st == "cancel-requested", "{st}");

        // ---- Row-level TTL.
        assert!(s.scheduler_items("ttl").await.unwrap().items.is_empty());
        let form = s.scheduler_form("ttl", None).await.unwrap();
        let table = form.choices["table"]
            .iter()
            .find(|(_, l)| l == "dbine_test_sched_db.public.t")
            .expect("the table")
            .0
            .clone();
        let mut v = values_of(&form);
        set(&mut v, "table", json!(table));
        set(&mut v, "ttl_expire_after", json!("3 days"));
        set(&mut v, "ttl_job_cron", json!("@daily"));
        set(&mut v, "ttl_select_batch_size", json!(1000));
        let ttl = save(&mut s, "ttl", None, &v).await;
        assert_eq!(ttl, table);
        let items = s.scheduler_items("ttl").await.unwrap();
        assert_eq!(items.items.len(), 1, "{items:?}");
        let opts = || {
            "SELECT array_to_string(reloptions, ' ') FROM dbine_test_sched_db.pg_catalog.pg_class WHERE relname = 't'"
        };
        let o = scalar(&mut s, opts()).await;
        assert!(
            o.contains("ttl_job_cron='@daily'") && o.contains("ttl_select_batch_size=1000"),
            "{o}"
        );
        let form = s.scheduler_form("ttl", Some(&ttl)).await.unwrap();
        assert_eq!(form.values["ttl_expire_after"], json!("3 days"));
        assert_eq!(form.values["enabled"], json!(true));
        // The TTL schedule's id in the jobs list opens the same form.
        let sched_id = format!(
            "crdb_schedule:{}",
            form.values["schedule_id"].as_str().unwrap()
        );
        let jf = s.scheduler_form("job", Some(&sched_id)).await.unwrap();
        assert_eq!(jf.values["table"], form.values["table"]);
        // Another way to expire, a parameter reset, the schedule paused.
        let mut e = values_of(&form);
        set(&mut e, "expire_mode", json!("expression"));
        set(
            &mut e,
            "ttl_expiration_expression",
            json!("created + INTERVAL '30 days'"),
        );
        set(&mut e, "ttl_select_batch_size", Value::Null);
        set(&mut e, "ttl_delete_batch_size", json!(50));
        set(&mut e, "enabled", json!(false));
        save(&mut s, "ttl", Some(&ttl), &e).await;
        let o = scalar(&mut s, opts()).await;
        assert!(
            o.contains("ttl_expiration_expression")
                && !o.contains("ttl_expire_after")
                && !o.contains("ttl_select_batch_size")
                && o.contains("ttl_delete_batch_size=50"),
            "{o}"
        );
        assert_eq!(crdb_sched(&mut s, &sched_id).await[1], json!("PAUSED"));
        let form = s.scheduler_form("ttl", Some(&ttl)).await.unwrap();
        assert_eq!(form.values["expire_mode"], json!("expression"));
        assert_eq!(form.values["enabled"], json!(false));
        let script = s.scheduler_delete_script("job", &sched_id).await.unwrap();
        assert!(script.contains("RESET (ttl)"), "{script}");
        s.scheduler_delete("ttl", &ttl).await.expect("delete ttl");
        assert!(!scalar(&mut s, opts()).await.contains("ttl"));

        // ---- The cluster's own schedules.
        let sys = scalar(
            &mut s,
            "SELECT id::STRING FROM [SHOW SCHEDULES] WHERE label = 'sql-stats-compaction'",
        )
        .await;
        let sys = format!("crdb_schedule:{sys}");
        let form = s.scheduler_form("job", Some(&sys)).await.unwrap();
        assert_eq!(
            form.values["setting"],
            json!("sql.stats.cleanup.recurrence")
        );
        let mut e = values_of(&form);
        set(&mut e, "recurring", json!("0 * * * *"));
        set(&mut e, "enabled", json!(false));
        let script = s.scheduler_script("job", Some(&sys), &e).await.unwrap();
        assert!(
            script.contains("SET CLUSTER SETTING sql.stats.cleanup.recurrence = '0 * * * *'"),
            "{script}"
        );
        save(&mut s, "job", Some(&sys), &e).await;
        assert_eq!(
            scalar(&mut s, "SHOW CLUSTER SETTING sql.stats.cleanup.recurrence").await,
            "0 * * * *"
        );
        assert_eq!(crdb_sched(&mut s, &sys).await[1], json!("PAUSED"));
        let mut e = values_of(&s.scheduler_form("job", Some(&sys)).await.unwrap());
        set(&mut e, "recurring", json!("@hourly"));
        set(&mut e, "enabled", json!(true));
        save(&mut s, "job", Some(&sys), &e).await;
        assert_eq!(crdb_sched(&mut s, &sys).await[1], json!("ACTIVE"));
        assert_eq!(
            scalar(&mut s, "SHOW CLUSTER SETTING sql.stats.cleanup.recurrence").await,
            "@hourly"
        );
        assert!(s.scheduler_delete_script("job", &sys).await.is_err());

        // Ids that weren't written here.
        for bad in [
            "crdb_schedule:0",
            "crdb_schedule:1; DROP DATABASE x",
            "pg_cron:1",
        ] {
            assert!(s.scheduler_form("job", Some(bad)).await.is_err(), "{bad}");
        }
        assert!(s.scheduler_form("ttl", Some("1 OR 1=1")).await.is_err());
    };
    let result = AssertUnwindSafe(body).catch_unwind().await;
    let left = crdb_cleanup(&base).await;
    restore_stats_compaction(&base, &stats).await;
    run(
        &mut admin,
        &format!(
            "SET CLUSTER SETTING kv.rangefeed.enabled = {}",
            rangefeed == "true"
        ),
    )
    .await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert_eq!(left, "0", "objects left behind");
}

// ---------------------------------------------------------------------------
// openGauss (DBMS_JOB through pkg_service)
// ---------------------------------------------------------------------------

const GAUSS_CLEANUP: &str = "SELECT pkg_service.job_cancel(job_id) FROM pg_catalog.pg_job_proc WHERE what LIKE '%dbine_test_sched%';
DROP TABLE IF EXISTS dbine_test_sched_log";
const GAUSS_LEFT: &str =
    "SELECT (SELECT count(*) FROM pg_catalog.pg_job_proc WHERE what LIKE '%dbine_test_sched%')
  + (SELECT count(*) FROM pg_catalog.pg_class WHERE relname = 'dbine_test_sched_log')";

async fn gauss_row(s: &mut Box<dyn Session>, id: &str) -> Vec<Value> {
    rows(
        s,
        &format!(
            "SELECT j.job_status, to_char(j.next_run_date, 'YYYY-MM-DD HH24:MI:SS'), j.interval, p.what
             FROM pg_catalog.pg_job j JOIN pg_catalog.pg_job_proc p ON p.job_id = j.job_id WHERE j.job_id = {}",
            number(id)
        ),
    )
    .await
    .first()
    .cloned()
    .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss_dbms_job_managed_whole() {
    let Some(cfg) = cfg("opengauss", "DBINE_TEST_OPENGAUSS_URL") else {
        return;
    };
    let mut first = connect(&cfg).await;
    run(&mut first, GAUSS_CLEANUP).await;
    let c2 = cfg.clone();
    with_cleanup(&cfg, GAUSS_CLEANUP, GAUSS_LEFT, async move {
        let mut s = connect(&c2).await;
        run(&mut s, "CREATE TABLE dbine_test_sched_log (n int)").await;
        let info = s.scheduler_info().await.expect("info");
        eprintln!("{info:#?}");
        assert_eq!(info.kinds[0].key, "job");
        assert!(info.kinds[0].can_create && info.kinds[0].can_edit && info.kinds[0].can_delete);
        let form = s.scheduler_form("job", None).await.expect("new form");
        eprintln!("{form:#?}");
        let mut v = values_of(&form);
        set(&mut v, "source", json!("dbms_job"));
        set(
            &mut v,
            "what",
            json!("insert into dbine_test_sched_log values (1);"),
        );
        set_in(&form, &mut v, "next_date", json!("2031-01-02T03:04:05"));
        set_in(&form, &mut v, "interval", json!("sysdate + 1"));
        let id = save(&mut s, "job", None, &v).await;
        assert!(id.starts_with("dbms_job:"), "{id}");
        let row = gauss_row(&mut s, &id).await;
        assert_eq!(row[0], json!("s"), "{row:?}");
        assert_eq!(row[1], json!("2031-01-02 03:04:05"), "{row:?}");
        assert_eq!(row[2], json!("sysdate + 1"), "{row:?}");
        // It's in the jobs list with the same id.
        let items = s.scheduler_items("job").await.unwrap();
        assert!(items.items.iter().any(|i| i.id == id), "{items:?}");

        let form = s.scheduler_form("job", Some(&id)).await.expect("form");
        assert!(!form.read_only, "{form:?}");
        assert_eq!(form.values["next_date"], json!("2031-01-02T03:04:05"));
        assert_eq!(form.values["interval"], json!("sysdate + 1"));
        assert_eq!(
            form.values["what"],
            json!("insert into dbine_test_sched_log values (1);")
        );
        assert!(s
            .scheduler_script("job", Some(&id), &values_of(&form))
            .await
            .unwrap()
            .contains("no hay cambios"));

        // The interval goes with the command (openGauss's job_update needs
        // it); the next date alone.
        let mut e = values_of(&form);
        set(&mut e, "interval", json!("trunc(sysdate) + 2"));
        let script = s.scheduler_script("job", Some(&id), &e).await.unwrap();
        assert!(script.contains("pkg_service.job_update"), "{script}");
        save(&mut s, "job", Some(&id), &e).await;
        assert_eq!(gauss_row(&mut s, &id).await[2], json!("trunc(sysdate) + 2"));
        let mut e = values_of(&s.scheduler_form("job", Some(&id)).await.unwrap());
        set(&mut e, "next_date", json!("2031-02-03T04:05:06"));
        set(
            &mut e,
            "what",
            json!("insert into dbine_test_sched_log values (2);"),
        );
        save(&mut s, "job", Some(&id), &e).await;
        let row = gauss_row(&mut s, &id).await;
        assert_eq!(row[1], json!("2031-02-03 04:05:06"), "{row:?}");
        assert_eq!(
            row[3],
            json!("insert into dbine_test_sched_log values (2);"),
            "{row:?}"
        );
        // Disabled (broken), enabled at a date.
        let mut e = values_of(&s.scheduler_form("job", Some(&id)).await.unwrap());
        set(&mut e, "enabled", json!(false));
        save(&mut s, "job", Some(&id), &e).await;
        assert_eq!(gauss_row(&mut s, &id).await[0], json!("d"));
        let form = s.scheduler_form("job", Some(&id)).await.unwrap();
        assert_eq!(form.values["enabled"], json!(false));
        let mut e = values_of(&form);
        set(&mut e, "enabled", json!(true));
        set(&mut e, "next_date", json!("2031-03-04T05:06:07"));
        save(&mut s, "job", Some(&id), &e).await;
        let row = gauss_row(&mut s, &id).await;
        assert_eq!(row[0], json!("s"), "{row:?}");
        assert_eq!(row[1], json!("2031-03-04 05:06:07"), "{row:?}");
        // Runs once.
        let mut e = values_of(&s.scheduler_form("job", Some(&id)).await.unwrap());
        set(&mut e, "interval", json!(""));
        save(&mut s, "job", Some(&id), &e).await;
        assert_eq!(gauss_row(&mut s, &id).await[2], json!("null"));

        // Created disabled.
        let mut v = values_of(&s.scheduler_form("job", None).await.unwrap());
        set(&mut v, "source", json!("dbms_job"));
        set(
            &mut v,
            "what",
            json!("insert into dbine_test_sched_log values (3);"),
        );
        set(&mut v, "next_date", json!("2031-01-01T00:00:00"));
        set(&mut v, "enabled", json!(false));
        let off = save(&mut s, "job", None, &v).await;
        assert_eq!(gauss_row(&mut s, &off).await[0], json!("d"));

        assert!(matches!(
            s.clear_job_history(&id).await,
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            s.run_job_from(&id, "x").await,
            Err(Error::Unsupported(_))
        ));
        for bad in ["dbms_job:0", "dbms_job:1 OR 1=1", "edb_job:1", "crdb_job:1"] {
            assert!(s.scheduler_form("job", Some(bad)).await.is_err(), "{bad}");
        }
        let script = s.scheduler_delete_script("job", &id).await.unwrap();
        assert!(
            script.contains(&format!(
                "pkg_service.job_cancel((SELECT {}::bigint))",
                number(&id)
            )),
            "{script}"
        );
        s.scheduler_delete("job", &id).await.expect("delete");
        s.scheduler_delete("job", &off).await.expect("delete");
        assert!(gauss_row(&mut s, &id).await.is_empty());
        assert!(gauss_row(&mut s, &off).await.is_empty());
    })
    .await;
}

const CRDB_USER: &str = "dbine_test_sched_user";
/// The plain user's test database.
const CRDB_PLAIN_DB: &str = "dbine_test_sched_plain";

/// A user who isn't admin: without VIEWJOB the schedules can't be read;
/// with it, another user's schedule shows read-only and isn't changed.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroach_as_a_plain_user() {
    let Some(base) = cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL") else {
        return;
    };
    let mut admin = connect(&base).await;
    // The database goes first: its tables' TTL schedules go with it, and
    // the user's grants and table on it with them.
    let drop_user = format!(
        "DROP SCHEDULES SELECT id FROM [SHOW SCHEDULES] WHERE label LIKE 'dbine_test_sched%';
DROP DATABASE IF EXISTS {CRDB_PLAIN_DB} CASCADE;
REVOKE SYSTEM VIEWJOB FROM {CRDB_USER};
DROP USER IF EXISTS {CRDB_USER}"
    );
    let cleanup = |sql: String| {
        let base = base.clone();
        async move {
            let mut s = connect(&base).await;
            for st in sql.split(";\n") {
                let mut out = QueryOutcome::default();
                let _ = s.execute(st, 100, &mut out).await;
            }
            scalar(
                &mut s,
                &format!(
                    "SELECT (SELECT count(*) FROM [SHOW SCHEDULES] WHERE label LIKE 'dbine_test_sched%') + (SELECT count(*) FROM [SHOW USERS] WHERE username = '{CRDB_USER}') + (SELECT count(*) FROM [SHOW DATABASES] WHERE database_name = '{CRDB_PLAIN_DB}')"
                ),
            )
            .await
        }
    };
    cleanup(drop_user.clone()).await;
    run(&mut admin, &format!("CREATE USER {CRDB_USER}")).await;
    // A table with a TTL, root's (and so its TTL schedule), that the user
    // may read; the user may create tables of their own beside it.
    for st in [
        format!("CREATE DATABASE {CRDB_PLAIN_DB}"),
        format!("CREATE TABLE {CRDB_PLAIN_DB}.t (id INT PRIMARY KEY, created TIMESTAMPTZ NOT NULL DEFAULT now()) WITH (ttl_expire_after = '30 days')"),
        format!("GRANT SELECT ON TABLE {CRDB_PLAIN_DB}.t TO {CRDB_USER}"),
        format!("GRANT CREATE ON DATABASE {CRDB_PLAIN_DB} TO {CRDB_USER}"),
    ] {
        run(&mut admin, &st).await;
    }
    run(
        &mut admin,
        "CREATE SCHEDULE 'dbine_test_sched_root' FOR BACKUP INTO 'nodelocal://1/dbine_test_sched_root' RECURRING '@daily' FULL BACKUP ALWAYS WITH SCHEDULE OPTIONS first_run = '2031-01-01 00:00:00+00'",
    )
    .await;
    let id = format!(
        "crdb_schedule:{}",
        scalar(
            &mut admin,
            "SELECT id::STRING FROM [SHOW SCHEDULES] WHERE label = 'dbine_test_sched_root'"
        )
        .await
    );
    let user = ConnectionConfig {
        username: Some(CRDB_USER.into()),
        password: None,
        ..base.clone()
    };
    let body = async {
        let mut s = connect(&user).await;
        let info = s.scheduler_info().await.expect("info");
        eprintln!("{info:#?}");
        assert!(info.notes.iter().any(|n| n.contains("VIEWJOB")), "{info:?}");
        assert!(
            info.notes.iter().any(|n| n.contains("no es admin")),
            "{info:?}"
        );
        // Without BACKUP, CHANGEFEED nor VIEWJOB: nothing to create or change.
        let job = |info: &dbine_driver::SchedulerInfo| {
            let k = info.kinds.iter().find(|k| k.key == "job").unwrap();
            (k.can_create, k.can_edit, k.can_delete)
        };
        assert_eq!(job(&info), (false, false, false));
        assert!(matches!(
            s.scheduler_form("job", None).await,
            Err(Error::Unsupported(_))
        ));
        assert!(s.scheduler_form("job", Some(&id)).await.is_err());
        run(&mut admin, &format!("GRANT SYSTEM VIEWJOB TO {CRDB_USER}")).await;
        let mut s = connect(&user).await;
        // With VIEWJOB: schedules to change (the server checks whose).
        assert_eq!(job(&s.scheduler_info().await.unwrap()), (false, true, true));
        let form = s.scheduler_form("job", Some(&id)).await.expect("form");
        assert!(form.read_only, "{form:?}");
        assert!(form.notes[0].contains("otro usuario"), "{:?}", form.notes);
        // The tree says so too, and offers nothing to run; the cluster's
        // own schedules aren't this user's either.
        let it = item(&mut s, "job", &id).await;
        assert_eq!(
            (it.can_edit, it.can_delete, it.actions),
            (Some(false), Some(false), offers(&[]))
        );
        assert!(s
            .scheduler_action_script("job", &id, "run_now", &Default::default())
            .await
            .is_err());
        let items = s.scheduler_items("job").await.unwrap().items;
        let sys = items
            .iter()
            .find(|i| i.name == "sql-stats-compaction")
            .expect("sql-stats-compaction");
        assert_eq!(
            (sys.can_edit, sys.can_delete, sys.actions.clone()),
            (Some(false), Some(false), offers(&[]))
        );
        let mut v = values_of(&form);
        set(&mut v, "recurring", json!("@hourly"));
        assert!(s.scheduler_script("job", Some(&id), &v).await.is_err());
        assert!(s.scheduler_delete_script("job", &id).await.is_err());
        assert!(s.scheduler_delete("job", &id).await.is_err());
        assert_eq!(
            scalar(
                &mut admin,
                "SELECT recurrence FROM [SHOW SCHEDULES] WHERE label = 'dbine_test_sched_root'"
            )
            .await,
            "@daily"
        );

        // A table's TTL: EXECUTE SCHEDULE takes the schedule's owner (the
        // table's creator) or an admin, so root's isn't offered to this
        // user, in the TTL kind or among the schedules, and the user's own
        // is.
        let info = s.scheduler_info().await.unwrap();
        let ttl_kind = info.kinds.iter().find(|k| k.key == "ttl").unwrap();
        assert_eq!(action_keys(ttl_kind), ["run_now"], "26.1+ with VIEWJOB");
        run(
            &mut s,
            &format!(
                "CREATE TABLE {CRDB_PLAIN_DB}.mine (id INT PRIMARY KEY, created TIMESTAMPTZ NOT NULL DEFAULT now()) WITH (ttl_expire_after = '1 day')"
            ),
        )
        .await;
        let ttl = s.scheduler_items("ttl").await.unwrap().items;
        let find = |name: &str| {
            ttl.iter()
                .find(|i| i.name == format!("{CRDB_PLAIN_DB}.public.{name}"))
                .unwrap_or_else(|| panic!("{name}: {ttl:?}"))
                .clone()
        };
        let (roots, mine) = (find("t"), find("mine"));
        assert_eq!(roots.actions, offers(&[]), "{roots:?}");
        assert_eq!(mine.actions, offers(&["run_now"]), "{mine:?}");
        let e = s
            .scheduler_action_script("ttl", &roots.id, "run_now", &Default::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("otro usuario"), "{e}");
        let script = s
            .scheduler_action_script("ttl", &mine.id, "run_now", &Default::default())
            .await
            .unwrap();
        assert!(script.contains("\nEXECUTE SCHEDULE "), "{script}");
        // The same schedules in the jobs list: the same rule.
        let sched_sql = |table: &str| {
            format!(
                "SELECT 'crdb_schedule:' || id::STRING FROM [SHOW SCHEDULES] WHERE label = 'row-level-ttl: {table} [' || '{CRDB_PLAIN_DB}.{table}'::REGCLASS::OID::STRING || ']'"
            )
        };
        let roots_sched = scalar(&mut admin, &sched_sql("t")).await;
        let mine_sched = scalar(&mut admin, &sched_sql("mine")).await;
        assert_eq!(item(&mut s, "job", &roots_sched).await.actions, offers(&[]));
        assert_eq!(
            item(&mut s, "job", &mine_sched).await.actions,
            offers(&["run_now"])
        );
        assert!(s
            .scheduler_action_script("job", &roots_sched, "run_now", &Default::default())
            .await
            .is_err());
    };
    let result = AssertUnwindSafe(body).catch_unwind().await;
    let left = cleanup(drop_user).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert_eq!(left, "0", "objects left behind");
}

// ---------------------------------------------------------------------------
// Actions ("Ejecutar ahora", "Programar para ahora"), what each object
// allows, and the tabs of a new job
// ---------------------------------------------------------------------------

async fn item(s: &mut Box<dyn Session>, kind: &str, id: &str) -> dbine_driver::SchedulerItem {
    s.scheduler_items(kind)
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|i| i.id == id)
        .unwrap_or_else(|| panic!("{kind} {id}"))
}

fn action_keys(k: &dbine_driver::SchedulerKind) -> Vec<&str> {
    k.actions.iter().map(|a| a.key.as_str()).collect()
}

fn only(source: &str) -> Option<(String, Vec<String>)> {
    Some(("source".to_string(), vec![source.to_string()]))
}

fn tab<'a>(form: &'a ObjectForm, key: &str) -> &'a dbine_driver::FormSection {
    form.sections
        .iter()
        .find(|t| t.key == key)
        .unwrap_or_else(|| panic!("tab {key}"))
}

fn offers(keys: &[&str]) -> Option<Vec<String>> {
    Some(keys.iter().map(|k| k.to_string()).collect())
}

/// pg_cron (no "run now") and pgAgent ("programar para ahora", as pgAdmin
/// does it) in one database; a new job shows only the picked scheduler's
/// tabs.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pg_cron_and_pgagent_actions() {
    let Some(cfg) = cfg("postgres", "DBINE_TEST_PGCRON_URL") else {
        return;
    };
    let cleanup = format!("{CRON_CLEANUP}\n{AGENT_CLEANUP}");
    let left = format!("SELECT ({CRON_LEFT}) + ({AGENT_LEFT})");
    let c2 = cfg.clone();
    let cleanup2 = cleanup.clone();
    with_cleanup(&cfg, &cleanup, &left, async move {
        let cfg = c2;
        let mut s = connect(&cfg).await;
        run(&mut s, &cleanup2).await;
        let info = s.scheduler_info().await.unwrap();
        let job = &info.kinds[0];
        assert!(action_keys(job).contains(&"run_now"), "{job:?}");
        assert!(job.clear_history && !job.run_from_step, "{job:?}");

        // A new job: pgAgent's tabs and lists only while it's picked.
        let form = s.scheduler_form("job", None).await.unwrap();
        assert_eq!(tab(&form, "general").when, None);
        assert_eq!(tab(&form, "command").when, only("pg_cron"));
        for t in ["steps", "schedules"] {
            assert_eq!(tab(&form, t).when, only("pgagent"), "{t}");
            assert!(tab(&form, t).lists.iter().all(|l| l.when == only("pgagent")), "{t}");
        }
        assert!(form.notes.iter().all(|n| !n.contains("pasos y las programaciones")));

        // pg_cron: shown without actions; "run now" says why.
        let mut v = values_of(&form);
        set(&mut v, "source", json!("pg_cron"));
        set_in(&form, &mut v, "name", json!("dbine_test_sched_act"));
        set_in(&form, &mut v, "schedule", json!("0 3 * * *"));
        set_in(&form, &mut v, "command", json!("SELECT 1"));
        let cron_id = save(&mut s, "job", None, &v).await;
        let it = item(&mut s, "job", &cron_id).await;
        assert_eq!(
            (it.can_edit, it.can_delete, it.actions),
            (Some(true), Some(true), offers(&[]))
        );
        let e = s
            .scheduler_action_script("job", &cron_id, "run_now", &Default::default())
            .await
            .unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("pg_cron")), "{e:?}");

        // pgAgent: programmed for now.
        let mut v = values_of(&form);
        set(&mut v, "source", json!("pgagent"));
        set_in(&form, &mut v, "name", json!(AGENT_JOB));
        v.lists.insert(
            "steps".into(),
            vec![new_row(&form, "steps", json!({"name": "01", "kind": "sql", "dbname": "postgres", "code": "SELECT 1"}))],
        );
        let id = save(&mut s, "job", None, &v).await;
        let n = number(&id).to_string();
        run(&mut s, &format!("UPDATE pgagent.pga_job SET jobnextrun = '2031-01-01' WHERE jobid = {n}")).await;
        let it = item(&mut s, "job", &id).await;
        assert_eq!(it.actions, offers(&["run_now"]), "{it:?}");
        assert_eq!((it.can_edit, it.can_delete), (Some(true), Some(true)));
        let script = s.scheduler_action_script("job", &id, "run_now", &Default::default()).await.unwrap();
        eprintln!("{script}");
        assert!(
            script.ends_with(&format!("UPDATE pgagent.pga_job SET jobnextrun = now() WHERE jobid = {n} AND jobenabled;\n")),
            "{script}"
        );
        assert!(s.scheduler_action_script("job", &id, "run", &Default::default()).await.is_err());
        // Read-only connections show the script, never run it.
        let mut ro = ReadOnlySession::new(connect(&cfg).await);
        assert!(ro.scheduler_action_script("job", &id, "run_now", &Default::default()).await.is_ok());
        assert!(ro.scheduler_action("job", &id, "run_now", &Default::default()).await.is_err());
        s.scheduler_action("job", &id, "run_now", &Default::default()).await.expect("run now");
        let next = scalar(&mut s, &format!(
            "SELECT jobnextrun <= now() + interval '1 second' OR joblastrun IS NOT NULL FROM pgagent.pga_job WHERE jobid = {n}"
        )).await;
        assert_eq!(next, "t");
        // Its class is in use: it can't be deleted.
        let class = scalar(&mut s, &format!("SELECT jobjclid FROM pgagent.pga_job WHERE jobid = {n}")).await;
        assert_eq!(item(&mut s, "job_class", &class).await.can_delete, Some(false));
        // Disabled: nothing to run.
        run(&mut s, &format!("UPDATE pgagent.pga_job SET jobenabled = false WHERE jobid = {n}")).await;
        assert_eq!(item(&mut s, "job", &id).await.actions, offers(&[]));
        let e = s.scheduler_action_script("job", &id, "run_now", &Default::default()).await.unwrap_err();
        assert!(e.to_string().contains("deshabilitado"), "{e}");
        // Ids and actions that weren't written here.
        assert!(s.scheduler_action_script("job", "pgagent:0", "run_now", &Default::default()).await.is_err());
        assert!(s.scheduler_action_script("job", &id, "x'; DROP TABLE t", &Default::default()).await.is_err());
        assert!(s.scheduler_action_script("job_class", &class, "run_now", &Default::default()).await.is_err());
    })
    .await;
}

const TS_ACT_SETUP: &str = r"CREATE TABLE dbine_test_sched_runs (job int, at timestamptz DEFAULT now());
CREATE TABLE dbine_test_sched_ht (t timestamptz NOT NULL, v int);
SELECT create_hypertable('dbine_test_sched_ht', 't');
CREATE PROCEDURE dbine_test_sched_proc(job_id int, config jsonb) LANGUAGE plpgsql AS
  $$BEGIN INSERT INTO dbine_test_sched_runs (job) VALUES (job_id); END$$;";

/// TimescaleDB: "Ejecutar ahora" is `CALL run_job`, for jobs (scheduled or
/// not) and policies; the extension's own jobs aren't deleted.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescaledb_actions() {
    let Some(cfg) = cfg("timescaledb", "DBINE_TEST_TIMESCALEDB_URL") else {
        return;
    };
    let cleanup = format!("{TS_CLEANUP}\nDROP TABLE IF EXISTS dbine_test_sched_runs;");
    let c2 = cfg.clone();
    let cleanup2 = cleanup.clone();
    with_cleanup(&cfg, &cleanup, TS_LEFT, async move {
        let cfg = c2;
        let mut s = connect(&cfg).await;
        run(&mut s, &cleanup2).await;
        run(&mut s, TS_ACT_SETUP).await;
        let info = s.scheduler_info().await.unwrap();
        for k in &info.kinds {
            if k.key == "job" || k.key.ends_with("_policy") {
                assert_eq!(action_keys(k), ["run"], "{k:?}");
                assert!(k.clear_history && !k.run_from_step, "{k:?}");
            }
        }
        // The extension's own jobs: changed, never deleted.
        let items = s.scheduler_items("job").await.unwrap().items;
        let own = items.iter().find(|i| i.system).expect("a system job");
        assert_eq!(
            (own.can_edit, own.can_delete),
            (Some(true), Some(false)),
            "{own:?}"
        );

        // A job, unscheduled: run here all the same.
        let form = s.scheduler_form("job", None).await.unwrap();
        let mut v = values_of(&form);
        set_in(&form, &mut v, "name", json!("dbine_test_sched_act"));
        set_in(
            &form,
            &mut v,
            "proc",
            json!(r#""public"."dbine_test_sched_proc""#),
        );
        set_in(&form, &mut v, "schedule_interval", json!("1 day"));
        let id = save(&mut s, "job", None, &v).await;
        let n = number(&id).to_string();
        run(
            &mut s,
            &format!("SELECT alter_job({n}, scheduled => false)"),
        )
        .await;
        let it = item(&mut s, "job", &id).await;
        assert_eq!(
            (it.can_edit, it.can_delete, it.actions),
            (Some(true), Some(true), offers(&["run"]))
        );
        let script = s
            .scheduler_action_script("job", &id, "run", &Default::default())
            .await
            .unwrap();
        eprintln!("{script}");
        assert!(
            script.ends_with(&format!("\nCALL \"public\".run_job({n});\n")),
            "{script}"
        );
        assert!(s
            .scheduler_action_script("job", &id, "run_now", &Default::default())
            .await
            .is_err());
        s.scheduler_action("job", &id, "run", &Default::default())
            .await
            .expect("run");
        assert_eq!(
            scalar(
                &mut s,
                &format!("SELECT count(*) FROM dbine_test_sched_runs WHERE job = {n}")
            )
            .await,
            "1"
        );

        // A policy runs the same way.
        let p = scalar(
            &mut s,
            "SELECT add_retention_policy('dbine_test_sched_ht', INTERVAL '30 days')",
        )
        .await;
        let pid = format!("timescaledb:{p}");
        let it = item(&mut s, "retention_policy", &pid).await;
        assert_eq!(
            (it.can_edit, it.can_delete, it.actions),
            (Some(true), Some(true), offers(&["run"]))
        );
        let script = s
            .scheduler_action_script("retention_policy", &pid, "run", &Default::default())
            .await
            .unwrap();
        assert!(script.contains("política de retención"), "{script}");
        s.scheduler_action("retention_policy", &pid, "run", &Default::default())
            .await
            .expect("run the policy");
        // Not a compression policy; not a job of this server.
        assert!(s
            .scheduler_action_script("compression_policy", &pid, "run", &Default::default())
            .await
            .is_err());
        assert!(s
            .scheduler_action_script("job", "timescaledb:0", "run", &Default::default())
            .await
            .is_err());
        let mut ro = ReadOnlySession::new(connect(&cfg).await);
        assert!(ro
            .scheduler_action("job", &id, "run", &Default::default())
            .await
            .is_err());
    })
    .await;
}

/// openGauss has no DBMS_JOB.RUN: "programar para ahora" moves the next
/// run to the past and the job scheduler takes it.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss_actions() {
    let Some(cfg) = cfg("opengauss", "DBINE_TEST_OPENGAUSS_URL") else {
        return;
    };
    let mut first = connect(&cfg).await;
    run(&mut first, GAUSS_CLEANUP).await;
    let c2 = cfg.clone();
    with_cleanup(&cfg, GAUSS_CLEANUP, GAUSS_LEFT, async move {
        let mut s = connect(&c2).await;
        run(&mut s, "CREATE TABLE dbine_test_sched_log (n int)").await;
        let info = s.scheduler_info().await.expect("info");
        assert_eq!(action_keys(&info.kinds[0]), ["run_now"]);
        assert!(!info.kinds[0].clear_history && !info.kinds[0].run_from_step);
        let form = s.scheduler_form("job", None).await.unwrap();
        let mut v = values_of(&form);
        set(&mut v, "source", json!("dbms_job"));
        set(
            &mut v,
            "what",
            json!("insert into dbine_test_sched_log values (1);"),
        );
        set_in(&form, &mut v, "next_date", json!("2031-01-02T03:04:05"));
        set_in(&form, &mut v, "interval", json!("sysdate + 1"));
        let id = save(&mut s, "job", None, &v).await;
        let it = item(&mut s, "job", &id).await;
        assert_eq!(
            (it.can_edit, it.can_delete, it.actions),
            (Some(true), Some(true), offers(&["run_now"]))
        );
        assert!(s
            .scheduler_action_script("job", &id, "run", &Default::default())
            .await
            .is_err());
        let script = s
            .scheduler_action_script("job", &id, "run_now", &Default::default())
            .await
            .unwrap();
        eprintln!("{script}");
        assert!(
            script.contains(&format!(
                "pkg_service.job_update((SELECT {}::bigint)",
                number(&id)
            )),
            "{script}"
        );
        s.scheduler_action("job", &id, "run_now", &Default::default())
            .await
            .expect("run now");
        // The scheduler takes it within seconds: the row it inserts.
        let mut ran = false;
        for _ in 0..30 {
            if scalar(&mut s, "SELECT count(*) FROM dbine_test_sched_log").await != "0" {
                ran = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let row = gauss_row(&mut s, &id).await;
        assert!(ran, "the job didn't run: {row:?}");
        // Disabled: nothing to run.
        let mut e = values_of(&s.scheduler_form("job", Some(&id)).await.unwrap());
        set(&mut e, "enabled", json!(false));
        save(&mut s, "job", Some(&id), &e).await;
        assert_eq!(item(&mut s, "job", &id).await.actions, offers(&[]));
        assert!(s
            .scheduler_action_script("job", &id, "run_now", &Default::default())
            .await
            .is_err());
        s.scheduler_delete("job", &id).await.expect("delete");
    })
    .await;
}

/// CockroachDB: "programar para ahora" (ALTER BACKUP SCHEDULE … EXECUTE
/// IMMEDIATELY, EXECUTE SCHEDULE for a table's TTL) with the cluster's
/// scheduler stopped, so nothing runs; the cluster's own schedules aren't
/// deleted.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroach_actions() {
    let Some(base) = cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL") else {
        return;
    };
    let mut admin = connect(&base).await;
    crdb_cleanup(&base).await;
    let rangefeed = scalar(&mut admin, "SHOW CLUSTER SETTING kv.rangefeed.enabled").await;
    let scheduler = rows(
        &mut admin,
        "SELECT value, origin FROM [SHOW ALL CLUSTER SETTINGS] WHERE variable = 'jobs.scheduler.enabled'",
    )
    .await;
    let scheduler_default = scheduler
        .first()
        .and_then(|r| r.get(1))
        .and_then(Value::as_str)
        == Some("default");
    let scheduler_on = scheduler
        .first()
        .and_then(|r| r.first())
        .and_then(Value::as_str)
        .unwrap_or("true")
        .to_string();
    for st in CRDB_SETUP.split(";\n") {
        run(&mut admin, st).await;
    }
    run(
        &mut admin,
        "SET CLUSTER SETTING jobs.scheduler.enabled = false",
    )
    .await;
    let cfg = ConnectionConfig {
        database: CRDB_DB.into(),
        ..base.clone()
    };
    let body = async move {
        let mut s = connect(&cfg).await;
        let info = s.scheduler_info().await.expect("info");
        assert_eq!(action_keys(&info.kinds[0]), ["run_now"]);
        assert_eq!(action_keys(&info.kinds[1]), ["run_now"], "TTL on 26.1+");
        assert!(info
            .kinds
            .iter()
            .all(|k| !k.clear_history && !k.run_from_step));
        // The cluster's own: never deleted.
        let items = s.scheduler_items("job").await.unwrap().items;
        let sys = items
            .iter()
            .find(|i| i.name == "sql-stats-compaction")
            .expect("sql-stats-compaction");
        assert_eq!(
            (sys.can_edit, sys.can_delete),
            (Some(true), Some(false)),
            "{sys:?}"
        );

        // A backup schedule, never due on its own.
        let form = s.scheduler_form("job", None).await.unwrap();
        assert_eq!(tab(&form, "backup").when, only("crdb_backup"));
        let mut v = values_of(&form);
        set(&mut v, "source", json!("crdb_backup"));
        set_in(&form, &mut v, "name", json!("dbine_test_sched_act"));
        set_in(&form, &mut v, "databases", json!([CRDB_DB]));
        set(
            &mut v,
            "locations",
            json!("nodelocal://1/dbine_test_sched_act"),
        );
        set_in(&form, &mut v, "recurring", json!("@daily"));
        set_in(&form, &mut v, "full_backup", json!("always"));
        set_in(&form, &mut v, "first_run", json!("2031-01-01T00:00:00Z"));
        let id = save(&mut s, "job", None, &v).await;
        let it = item(&mut s, "job", &id).await;
        assert_eq!(
            (it.can_edit, it.can_delete, it.actions),
            (Some(true), Some(true), offers(&["run_now"]))
        );
        let script = s
            .scheduler_action_script("job", &id, "run_now", &Default::default())
            .await
            .unwrap();
        eprintln!("{script}");
        assert!(
            script.ends_with(&format!(
                "\nALTER BACKUP SCHEDULE {} EXECUTE FULL IMMEDIATELY;\n",
                number(&id)
            )),
            "{script}"
        );
        assert!(s
            .scheduler_action_script("job", &id, "run", &Default::default())
            .await
            .is_err());
        s.scheduler_action("job", &id, "run_now", &Default::default())
            .await
            .expect("run now");
        let due = scalar(
            &mut s,
            &format!(
                "SELECT next_run < now() + INTERVAL '1 minute' FROM [SHOW SCHEDULES] WHERE id = {}",
                number(&id)
            ),
        )
        .await;
        assert_eq!(due, "t");
        // Paused: refused.
        run(&mut s, &format!("PAUSE SCHEDULE {}", number(&id))).await;
        assert_eq!(item(&mut s, "job", &id).await.actions, offers(&[]));
        let e = s
            .scheduler_action_script("job", &id, "run_now", &Default::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("pausa"), "{e}");

        // A table's TTL: its schedule, programmed for now.
        run(
            &mut s,
            "ALTER TABLE dbine_test_sched_db.t SET (ttl_expire_after = '30 days')",
        )
        .await;
        let ttl = s.scheduler_items("ttl").await.unwrap().items;
        let t = ttl
            .iter()
            .find(|i| i.name.ends_with(".t"))
            .unwrap_or_else(|| panic!("{ttl:?}"));
        assert_eq!(t.actions, offers(&["run_now"]), "{t:?}");
        let script = s
            .scheduler_action_script("ttl", &t.id, "run_now", &Default::default())
            .await
            .unwrap();
        eprintln!("{script}");
        assert!(script.contains("\nEXECUTE SCHEDULE "), "{script}");
        s.scheduler_action("ttl", &t.id, "run_now", &Default::default())
            .await
            .expect("run the TTL now");
        let mut ro = ReadOnlySession::new(connect(&cfg).await);
        assert!(ro
            .scheduler_action_script("ttl", &t.id, "run_now", &Default::default())
            .await
            .is_ok());
        assert!(ro
            .scheduler_action("ttl", &t.id, "run_now", &Default::default())
            .await
            .is_err());
    };
    let result = AssertUnwindSafe(body).catch_unwind().await;
    let left = crdb_cleanup(&base).await;
    run(
        &mut admin,
        &format!(
            "SET CLUSTER SETTING kv.rangefeed.enabled = {}",
            rangefeed == "true"
        ),
    )
    .await;
    if scheduler_default {
        run(&mut admin, "RESET CLUSTER SETTING jobs.scheduler.enabled").await;
    } else {
        run(
            &mut admin,
            &format!(
                "SET CLUSTER SETTING jobs.scheduler.enabled = {}",
                scheduler_on == "true"
            ),
        )
        .await;
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert_eq!(left, "0", "objects left behind");
}
