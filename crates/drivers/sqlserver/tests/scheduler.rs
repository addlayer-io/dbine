//! SQL Server Agent managed through the scheduler contract, against a
//! server with the Agent running (`DBINE_TEST_SQLSERVER_AGENT_URL`, a
//! container started with `MSSQL_AGENT_ENABLED=true`). Skipped without it:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_AGENT_URL='mssql://sa:Pw_12345!@localhost:26131' \
//!   cargo test -p dbine-driver-sqlserver --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! Every object a test makes is named `dbine_test_sched_*` and is dropped
//! at the end, whether the test passed or not.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Error, FormValues, ObjectForm, QueryOutcome, Session};
use futures::FutureExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

const PASSWORD: &str = "Pw_12345!sched";

fn cfg() -> Option<ConnectionConfig> {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_AGENT_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_AGENT_URL not set; skipping");
        return None;
    };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

async fn connect(cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_sqlserver::drivers()
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
async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    run(s, sql)
        .await
        .results
        .iter()
        .rfind(|r| !r.columns.is_empty())
        .map(|r| r.rows.clone())
        .unwrap_or_default()
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> Value {
    rows(s, sql).await[0][0].clone()
}

/// Runs the test, then the cleanup on a connection of its own, and only
/// then lets a failed assertion through.
async fn with_cleanup(cfg: &ConnectionConfig, body: impl Future<Output = ()>) {
    let result = AssertUnwindSafe(body).catch_unwind().await;
    let mut s = connect(cfg).await;
    let mut out = QueryOutcome::default();
    let r = s.execute(CLEANUP, 100, &mut out).await;
    if r.is_err() || !out.errors.is_empty() {
        eprintln!("cleanup failed: {r:?} {:?}", out.errors);
    }
    let left = rows(&mut s, LEFT).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(r.is_ok() && out.errors.is_empty(), "cleanup failed");
    assert_eq!(left[0][0], json!(0), "objects left behind");
}

/// Drops everything named `dbine_test_sched_*`.
const CLEANUP: &str = r"SET NOCOUNT ON;
DECLARE @n sysname, @id int, @spid int, @tries int, @kill nvarchar(40);
WHILE EXISTS (SELECT 1 FROM msdb.dbo.sysjobs WHERE name LIKE N'dbine[_]test[_]sched[_]%')
BEGIN
    SELECT TOP (1) @n = name FROM msdb.dbo.sysjobs WHERE name LIKE N'dbine[_]test[_]sched[_]%';
    IF EXISTS (SELECT 1 FROM msdb.dbo.sysjobactivity a JOIN msdb.dbo.sysjobs j ON j.job_id = a.job_id
                WHERE j.name = @n AND a.start_execution_date IS NOT NULL AND a.stop_execution_date IS NULL
                  AND a.session_id = (SELECT MAX(session_id) FROM msdb.dbo.syssessions))
        EXEC msdb.dbo.sp_stop_job @job_name = @n;
    EXEC msdb.dbo.sp_delete_job @job_name = @n, @delete_unused_schedule = 1;
END;
WHILE EXISTS (SELECT 1 FROM msdb.dbo.sysalerts WHERE name LIKE N'dbine[_]test[_]sched[_]%')
BEGIN
    SELECT TOP (1) @n = name FROM msdb.dbo.sysalerts WHERE name LIKE N'dbine[_]test[_]sched[_]%';
    EXEC msdb.dbo.sp_delete_alert @name = @n;
END;
WHILE EXISTS (SELECT 1 FROM msdb.dbo.sysoperators WHERE name LIKE N'dbine[_]test[_]sched[_]%')
BEGIN
    SELECT TOP (1) @n = name FROM msdb.dbo.sysoperators WHERE name LIKE N'dbine[_]test[_]sched[_]%';
    EXEC msdb.dbo.sp_delete_operator @name = @n;
END;
WHILE EXISTS (SELECT 1 FROM msdb.dbo.sysproxies WHERE name LIKE N'dbine[_]test[_]sched[_]%')
BEGIN
    SELECT TOP (1) @n = name FROM msdb.dbo.sysproxies WHERE name LIKE N'dbine[_]test[_]sched[_]%';
    EXEC msdb.dbo.sp_delete_proxy @proxy_name = @n;
END;
WHILE EXISTS (SELECT 1 FROM msdb.dbo.sysschedules WHERE name LIKE N'dbine[_]test[_]sched[_]%')
BEGIN
    SELECT TOP (1) @id = schedule_id FROM msdb.dbo.sysschedules WHERE name LIKE N'dbine[_]test[_]sched[_]%';
    EXEC msdb.dbo.sp_delete_schedule @schedule_id = @id, @force_delete = 1;
END;
WHILE EXISTS (SELECT 1 FROM msdb.dbo.syscategories WHERE name LIKE N'dbine[_]test[_]sched[_]%')
BEGIN
    SELECT TOP (1) @n = name, @id = category_class FROM msdb.dbo.syscategories WHERE name LIKE N'dbine[_]test[_]sched[_]%';
    IF @id = 1 EXEC msdb.dbo.sp_delete_category @class = N'JOB', @name = @n;
    IF @id = 2 EXEC msdb.dbo.sp_delete_category @class = N'ALERT', @name = @n;
    IF @id = 3 EXEC msdb.dbo.sp_delete_category @class = N'OPERATOR', @name = @n;
END;
IF EXISTS (SELECT 1 FROM sys.credentials WHERE name = N'dbine_test_sched_cred')
    DROP CREDENTIAL [dbine_test_sched_cred];
DECLARE @drop nvarchar(400);
WHILE EXISTS (SELECT 1 FROM sys.server_principals WHERE name LIKE N'dbine[_]test[_]sched[_]%')
BEGIN
    SELECT TOP (1) @n = name FROM sys.server_principals WHERE name LIKE N'dbine[_]test[_]sched[_]%';
    SET @tries = 0;
    WHILE EXISTS (SELECT 1 FROM sys.dm_exec_sessions WHERE login_name = @n) AND @tries < 20
    BEGIN
        SELECT TOP (1) @spid = session_id FROM sys.dm_exec_sessions WHERE login_name = @n;
        SET @kill = N'KILL ' + CAST(@spid AS nvarchar(10));
        BEGIN TRY EXEC (@kill); END TRY BEGIN CATCH END CATCH;
        SET @tries += 1;
    END;
    IF EXISTS (SELECT 1 FROM msdb.sys.database_principals WHERE name = @n)
    BEGIN
        SET @drop = N'DROP USER ' + QUOTENAME(@n);
        EXEC msdb.sys.sp_executesql @drop;
    END;
    SET @drop = N'DROP LOGIN ' + QUOTENAME(@n);
    EXEC (@drop);
END;";

/// How many test objects are left.
const LEFT: &str = r"SELECT (SELECT COUNT(*) FROM msdb.dbo.sysjobs WHERE name LIKE N'dbine[_]test[_]sched[_]%')
     + (SELECT COUNT(*) FROM msdb.dbo.sysalerts WHERE name LIKE N'dbine[_]test[_]sched[_]%')
     + (SELECT COUNT(*) FROM msdb.dbo.sysoperators WHERE name LIKE N'dbine[_]test[_]sched[_]%')
     + (SELECT COUNT(*) FROM msdb.dbo.sysproxies WHERE name LIKE N'dbine[_]test[_]sched[_]%')
     + (SELECT COUNT(*) FROM msdb.dbo.sysschedules WHERE name LIKE N'dbine[_]test[_]sched[_]%')
     + (SELECT COUNT(*) FROM msdb.dbo.syscategories WHERE name LIKE N'dbine[_]test[_]sched[_]%')
     + (SELECT COUNT(*) FROM sys.credentials WHERE name LIKE N'dbine[_]test[_]sched[_]%')
     + (SELECT COUNT(*) FROM sys.server_principals WHERE name LIKE N'dbine[_]test[_]sched[_]%')";

/// What the designer would send back for a form as it came: its values,
/// the fields' defaults where a value is missing, and its lists' rows.
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

async fn save(s: &mut Box<dyn Session>, kind: &str, id: Option<&str>, v: &FormValues) -> String {
    let script = s.scheduler_script(kind, id, v).await.expect("script");
    eprintln!("--- {kind} {id:?}\n{script}");
    let r = s.scheduler_save(kind, id, v).await;
    r.unwrap_or_else(|e| panic!("save {kind}: {e:?}\n{script}"))
        .expect("an id")
}

async fn find_item(s: &mut Box<dyn Session>, kind: &str, name: &str) -> String {
    let list = s.scheduler_items(kind).await.expect("items");
    list.items
        .iter()
        .find(|i| i.name == name)
        .unwrap_or_else(|| panic!("{name} listed as {kind}: {list:?}"))
        .id
        .clone()
}

const JOB: &str = "dbine_test_sched_job";
const JOB2: &str = "dbine_test_sched_job 'renamed'";
const JOB_B: &str = "dbine_test_sched_job_b";
const CAT: &str = "dbine_test_sched_cat";
const OP: &str = "dbine_test_sched_op";
const OP2: &str = "dbine_test_sched_op2";
const SHARED2: &str = "dbine_test_sched_shared 2";
const CAT2: &str = "dbine_test_sched_cat2";
const SHARED: &str = "dbine_test_sched_shared";
const OWN: &str = "dbine_test_sched_own";
const ALERT: &str = "dbine_test_sched_alert";
const PROXY: &str = "dbine_test_sched_proxy";
const CRED: &str = "dbine_test_sched_cred";
const LOGIN: &str = "dbine_test_sched_login";

fn job_steps_sql(id: &str) -> String {
    format!(
        "SELECT step_id, step_name, on_success_action, on_success_step_id, on_fail_action, on_fail_step_id, command
           FROM msdb.dbo.sysjobsteps WHERE job_id = '{id}' ORDER BY step_id"
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn the_agent_managed_whole() {
    let Some(cfg) = cfg() else { return };
    let c2 = cfg.clone();
    with_cleanup(&cfg, async move {
        let cfg = c2;
        let mut s = connect(&cfg).await;
        // Leftovers of an earlier run.
        run(&mut s, CLEANUP).await;
        run(
            &mut s,
            &format!(
                // msdb wants the credential's identity to resolve to an
                // account (SUSER_SID): the test login.
                "CREATE LOGIN [{LOGIN}] WITH PASSWORD = N'{PASSWORD}', CHECK_POLICY = OFF;
                 CREATE CREDENTIAL [{CRED}] WITH IDENTITY = N'{LOGIN}', SECRET = N'{PASSWORD}';"
            ),
        )
        .await;

        // What the scheduler has.
        let info = s.scheduler_info().await.expect("info");
        eprintln!("{info:?}");
        let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(
            keys,
            ["job", "schedule", "operator", "alert", "proxy", "category"]
        );
        assert!(info.kinds.iter().all(|k| k.can_create && k.can_edit && k.can_delete));
        // Jobs start at a step and clear their history (the Agent runs);
        // alerts reset their count, enable and disable.
        let kind = |k: &str| info.kinds.iter().find(|x| x.key == k).unwrap().clone();
        assert!(kind("job").run_from_step && kind("job").clear_history);
        assert!(!kind("alert").run_from_step && !kind("alert").clear_history);
        let actions: Vec<String> = kind("alert").actions.iter().map(|a| a.key.clone()).collect();
        assert_eq!(actions, ["reset_count", "enable", "disable"]);
        assert!(kind("job").actions.is_empty());
        // msdb's own categories can't be changed or deleted; the tree knows.
        let cats = s.scheduler_items("category").await.expect("items");
        let system = cats.items.iter().find(|i| i.system).expect("a system category");
        assert_eq!((system.can_edit, system.can_delete), (Some(false), Some(false)));
        assert!(info.logs, "sa reads the Agent's log");
        let logs = s.scheduler_logs().await.expect("logs");
        assert!(!logs.is_empty() && !logs[0].rows.is_empty(), "{logs:?}");
        assert_eq!(logs[0].title, "Registro actual");
        assert_eq!(logs[0].columns, ["Fecha", "Tipo", "Mensaje"]);

        // A category.
        let form = s.scheduler_form("category", None).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "name", json!(CAT));
        let cat_id = save(&mut s, "category", None, &v).await;
        assert_eq!(cat_id, find_item(&mut s, "category", CAT).await);
        let cats = s.scheduler_items("category").await.expect("items");
        let mine = cats.items.iter().find(|i| i.id == cat_id).unwrap();
        assert_eq!((mine.can_edit, mine.can_delete), (None, None), "as its kind says");
        let n = scalar(
            &mut s,
            &format!("SELECT COUNT(*) FROM msdb.dbo.syscategories WHERE name = N'{CAT}' AND category_class = 1 AND category_type = 1"),
        )
        .await;
        assert_eq!(n, json!(1));

        // An operator.
        let form = s.scheduler_form("operator", None).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "name", json!(OP));
        set(&mut v, "email", json!("dba@example.com"));
        set(&mut v, "pager_days", json!(["monday", "friday"]));
        set(&mut v, "weekday_start", json!("07:30:00"));
        let op_id = save(&mut s, "operator", None, &v).await;
        let r = rows(
            &mut s,
            &format!("SELECT email_address, pager_days, weekday_pager_start_time, enabled FROM msdb.dbo.sysoperators WHERE id = {op_id}"),
        )
        .await;
        assert_eq!(r[0], [json!("dba@example.com"), json!(2 | 32), json!(73000), json!(1)]);

        // A shared schedule.
        let form = s.scheduler_form("schedule", None).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "name", json!(SHARED));
        set(&mut v, "type", json!("weekly"));
        set(&mut v, "weekdays", json!(["monday", "wednesday"]));
        set(&mut v, "every_weeks", json!(2));
        set(&mut v, "time", json!("02:30:00"));
        let shared_id = save(&mut s, "schedule", None, &v).await;
        let r = rows(
            &mut s,
            &format!("SELECT name, freq_type, freq_interval, freq_recurrence_factor, active_start_time FROM msdb.dbo.sysschedules WHERE schedule_id = {shared_id}"),
        )
        .await;
        assert_eq!(r[0], [json!(SHARED), json!(8), json!(2 | 8), json!(2), json!(23000)]);
        // Another job uses it too, so it's shared.
        run(
            &mut s,
            &format!(
                "EXEC msdb.dbo.sp_add_job @job_name = N'{JOB_B}';
                 EXEC msdb.dbo.sp_attach_schedule @job_name = N'{JOB_B}', @schedule_id = {shared_id};"
            ),
        )
        .await;
        // Weekly without a day is refused before it reaches the server.
        set(&mut v, "weekdays", json!([]));
        let e = s.scheduler_script("schedule", None, &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("al menos un día")), "{e:?}");

        // A proxy for a subsystem the Agent runs here, with a login and an
        // msdb role.
        let form = s.scheduler_form("proxy", None).await.expect("form");
        let subs: Vec<String> = form.choices.get("subsystem").unwrap().iter().map(|(v, _)| v.clone()).collect();
        let sub = subs
            .iter()
            .find(|x| !x.eq_ignore_ascii_case("TSQL"))
            .expect("a subsystem besides T-SQL")
            .clone();
        let mut v = values_of(&form);
        set(&mut v, "name", json!(PROXY));
        set(&mut v, "credential", json!(CRED));
        set(&mut v, "description", json!("DBine's test proxy"));
        set(&mut v, "subsystems", json!([sub]));
        v.lists.insert("logins".into(), vec![BTreeMap::from([("login".to_string(), json!(LOGIN))])]);
        v.lists.insert(
            "msdb_roles".into(),
            vec![BTreeMap::from([("role".to_string(), json!("SQLAgentOperatorRole"))])],
        );
        let proxy_id = save(&mut s, "proxy", None, &v).await;
        let n = scalar(
            &mut s,
            &format!("SELECT COUNT(*) FROM msdb.dbo.sysproxysubsystem WHERE proxy_id = {proxy_id}"),
        )
        .await;
        assert_eq!(n, json!(1));
        let n = scalar(
            &mut s,
            &format!("SELECT COUNT(*) FROM msdb.dbo.sysproxylogin WHERE proxy_id = {proxy_id}"),
        )
        .await;
        assert_eq!(n, json!(2));
        // Edit: rename, disable, take the role away.
        let form = s.scheduler_form("proxy", Some(&proxy_id)).await.expect("form");
        assert_eq!(form.values["credential"], json!(CRED));
        let mut v = values_of(&form);
        assert_eq!(v.lists["logins"].len(), 1);
        set(&mut v, "enabled", json!(false));
        v.lists.insert("msdb_roles".into(), Vec::new());
        let script = s.scheduler_script("proxy", Some(&proxy_id), &v).await.unwrap();
        assert!(script.contains("sp_update_proxy") && script.contains("@enabled = 0"), "{script}");
        assert!(script.contains("sp_revoke_login_from_proxy @name = N'SQLAgentOperatorRole'"), "{script}");
        assert!(!script.contains("sp_grant"), "{script}");
        save(&mut s, "proxy", Some(&proxy_id), &v).await;
        let r = rows(
            &mut s,
            &format!("SELECT enabled, (SELECT COUNT(*) FROM msdb.dbo.sysproxylogin WHERE proxy_id = {proxy_id}) FROM msdb.dbo.sysproxies WHERE proxy_id = {proxy_id}"),
        )
        .await;
        assert_eq!(r[0], [json!(0), json!(1)]);
        // Saved again unchanged: nothing to do.
        let form = s.scheduler_form("proxy", Some(&proxy_id)).await.expect("form");
        let script = s
            .scheduler_script("proxy", Some(&proxy_id), &values_of(&form))
            .await
            .unwrap();
        assert!(script.contains("no hay cambios"), "{script}");

        // The job: three steps, its own schedule and the shared one,
        // e-mail to the operator on failure, its category.
        let form = s.scheduler_form("job", None).await.expect("form");
        let sections: Vec<&str> = form.sections.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(
            sections,
            ["general", "steps", "schedules", "alerts", "notifications", "targets"]
        );
        let mut v = values_of(&form);
        set(&mut v, "name", json!(JOB));
        set(&mut v, "category", json!(CAT));
        set(&mut v, "description", json!("DBine's test job"));
        set(&mut v, "notify_email", json!(true));
        set(&mut v, "email_operator", json!(OP));
        set(&mut v, "email_when", json!("failure"));
        set(&mut v, "notify_eventlog", json!(true));
        set(&mut v, "eventlog_when", json!("failure"));
        set(&mut v, "start_step", json!("one"));
        v.lists.insert(
            "steps".into(),
            vec![
                new_row(&form, "steps", json!({"name": "one", "command": "SELECT 1;", "database": "master"})),
                new_row(
                    &form,
                    "steps",
                    json!({"name": "two", "command": "PRINT 'two''s'\nGO\nSELECT 2;", "database": "tempdb",
                           "on_fail": "goto", "on_fail_step": "one", "retry_attempts": 1, "retry_interval": 1}),
                ),
                new_row(
                    &form,
                    "steps",
                    json!({"name": "three", "command": "SELECT 3;", "on_success": "quit_success", "output_in_history": true}),
                ),
            ],
        );
        v.lists.insert(
            "schedules".into(),
            vec![new_row(
                &form,
                "schedules",
                json!({"name": OWN, "type": "daily", "every_days": 1, "time": "03:00:00"}),
            )],
        );
        v.lists.insert(
            "shared_schedules".into(),
            vec![BTreeMap::from([("schedule".to_string(), json!(shared_id))])],
        );
        let script = s.scheduler_script("job", None, &v).await.unwrap();
        assert!(script.contains("BEGIN TRANSACTION;") && script.contains("COMMIT TRANSACTION;"));
        assert!(script.contains("@notify_email_operator_name = N'dbine_test_sched_op'"), "{script}");
        let job_id = save(&mut s, "job", None, &v).await;
        assert_eq!(job_id.len(), 36);
        assert_eq!(job_id, find_item(&mut s, "job", JOB).await.to_lowercase());
        let steps = rows(&mut s, &job_steps_sql(&job_id)).await;
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0][1], json!("one"));
        assert_eq!(steps[0][2], json!(3), "go to the next step");
        assert_eq!(steps[1][1], json!("two"));
        assert_eq!((steps[1][4].clone(), steps[1][5].clone()), (json!(4), json!(1)));
        assert_eq!(steps[1][6], json!("PRINT 'two''s'\nGO\nSELECT 2;"));
        assert_eq!(steps[2][2], json!(1));
        let r = rows(
            &mut s,
            &format!(
                "SELECT j.notify_level_email, o.name, c.name, j.start_step_id, j.notify_level_eventlog,
                        (SELECT COUNT(*) FROM msdb.dbo.sysjobschedules WHERE job_id = j.job_id),
                        (SELECT COUNT(*) FROM msdb.dbo.sysjobservers WHERE job_id = j.job_id AND server_id = 0)
                   FROM msdb.dbo.sysjobs j
                   JOIN msdb.dbo.sysoperators o ON o.id = j.notify_email_operator_id
                   JOIN msdb.dbo.syscategories c ON c.category_id = j.category_id
                  WHERE j.job_id = '{job_id}'"
            ),
        )
        .await;
        assert_eq!(
            r[0],
            [json!(2), json!(OP), json!(CAT), json!(1), json!(2), json!(2), json!(1)]
        );

        // An alert that runs it and e-mails the operator.
        let form = s.scheduler_form("alert", None).await.expect("form");
        // A WMI alert would watch this instance's events (the default one
        // here).
        assert_eq!(
            form.values["wmi_namespace"],
            json!(r"\\.\root\Microsoft\SqlServer\ServerEvents\MSSQLSERVER")
        );
        let mut v = values_of(&form);
        set(&mut v, "name", json!(ALERT));
        set(&mut v, "type", json!("severity"));
        set(&mut v, "severity", json!("21"));
        set(&mut v, "run_job", json!(true));
        set(&mut v, "job", json!(job_id));
        set(&mut v, "delay_minutes", json!(1));
        set(&mut v, "delay_seconds", json!(30));
        set(&mut v, "include_email", json!(true));
        v.lists.insert(
            "operators".into(),
            vec![BTreeMap::from([
                ("operator".to_string(), json!(OP)),
                ("email".to_string(), json!(true)),
            ])],
        );
        let alert_id = save(&mut s, "alert", None, &v).await;
        let r = rows(
            &mut s,
            &format!(
                "SELECT severity, LOWER(CONVERT(nvarchar(36), job_id)), delay_between_responses, include_event_description,
                        (SELECT COUNT(*) FROM msdb.dbo.sysnotifications WHERE alert_id = a.id)
                   FROM msdb.dbo.sysalerts a WHERE id = {alert_id}"
            ),
        )
        .await;
        assert_eq!(r[0], [json!(21), json!(job_id), json!(90), json!(1), json!(1)]);
        // Edit: a performance condition instead, pager too. Object,
        // counter and instance come from the form's choices, each list
        // under the source its field names, filled with the field before.
        let form = s.scheduler_form("alert", Some(&alert_id)).await.expect("form");
        let mut v = values_of(&form);
        assert_eq!(v.values["severity"], json!("21"));
        assert!(!v.values.contains_key("reset_count"), "the count is reset by an action");
        let source = |k: &str| {
            let f = form.sections.iter().flat_map(|s| s.fields.iter()).find(|f| f.key == k).unwrap();
            match &f.kind {
                dbine_driver::FormKind::Ref { source, .. } => source.clone(),
                other => panic!("{k}: {other:?}"),
            }
        };
        let filled = |k: &str, object: &str, counter: &str| {
            source(k).replace("{perf_object}", object).replace("{perf_counter}", counter)
        };
        let values = |src: &str| -> Vec<String> {
            form.choices
                .get(src)
                .unwrap_or_else(|| panic!("{src} among the choices"))
                .iter()
                .map(|(v, _)| v.clone())
                .collect()
        };
        let object = values(&source("perf_object"))
            .into_iter()
            .find(|v| v.ends_with(":General Statistics"))
            .expect("the General Statistics counters");
        assert!(values(&filled("perf_counter", &object, "")).contains(&"User Connections".to_string()));
        // A counter without instances offers "none" only.
        let none = &form.choices[&filled("perf_instance", &object, "User Connections")];
        assert_eq!(none, &[(String::new(), "(sin instancia)".to_string())]);
        // One per database: msdb among them, and not "none".
        let databases = values(&source("perf_object"))
            .into_iter()
            .find(|v| v.ends_with(":Databases"))
            .expect("the Databases counters");
        let used = "Log File(s) Used Size (KB)";
        let instances = values(&filled("perf_instance", &databases, used));
        assert!(instances.contains(&"msdb".to_string()) && !instances.contains(&String::new()), "{instances:?}");
        set(&mut v, "type", json!("performance"));
        set(&mut v, "perf_object", json!(databases));
        set(&mut v, "perf_counter", json!(used));
        set(&mut v, "perf_instance", json!(""));
        set(&mut v, "perf_comparison", json!(">"));
        set(&mut v, "perf_value", json!(500));
        // That counter needs an instance; one it hasn't is refused too,
        // both before the server.
        let e = s.scheduler_script("alert", Some(&alert_id), &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("Elegí una instancia")), "{e:?}");
        set(&mut v, "perf_instance", json!("dbine_test_sched_nodb"));
        let e = s.scheduler_script("alert", Some(&alert_id), &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("no tiene la instancia")), "{e:?}");
        set(&mut v, "perf_counter", json!("dbine_test_sched_nocounter"));
        let e = s.scheduler_script("alert", Some(&alert_id), &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("no tiene el contador")), "{e:?}");
        set(&mut v, "perf_counter", json!(used));
        set(&mut v, "perf_instance", json!("msdb"));
        save(&mut s, "alert", Some(&alert_id), &v).await;
        let cond = scalar(&mut s, &format!("SELECT performance_condition FROM msdb.dbo.sysalerts WHERE id = {alert_id}")).await;
        assert_eq!(cond, json!(format!("{databases}|{used}|msdb|>|500")));
        // Then the counter without instances, in another case: the
        // server's spelling is saved.
        set(&mut v, "perf_object", json!(object.to_uppercase()));
        set(&mut v, "perf_counter", json!("user connections"));
        set(&mut v, "perf_instance", json!(""));
        v.lists.get_mut("operators").unwrap()[0].insert("pager".into(), json!(true));
        save(&mut s, "alert", Some(&alert_id), &v).await;
        let r = rows(
            &mut s,
            &format!(
                "SELECT severity, performance_condition,
                        (SELECT notification_method FROM msdb.dbo.sysnotifications WHERE alert_id = a.id)
                   FROM msdb.dbo.sysalerts a WHERE id = {alert_id}"
            ),
        )
        .await;
        assert_eq!(r[0], [json!(0), json!(format!("{object}|User Connections||>|500")), json!(3)]);
        // A condition scripted on another instance (its prefix) or without
        // a prefix: msdb takes it as this server's object, and so does the
        // designer when only its threshold changes, saving it with this
        // server's prefix.
        for stored in ["MSSQL$DBINE_OTHER:General Statistics", "General Statistics"] {
            run(
                &mut s,
                &format!(
                    "EXEC msdb.dbo.sp_update_alert @name = N'{ALERT}', @performance_condition = N'{stored}|User Connections||>|500';"
                ),
            )
            .await;
            let mut w = values_of(&s.scheduler_form("alert", Some(&alert_id)).await.expect("form"));
            assert_eq!(w.values["perf_object"], json!(stored));
            set(&mut w, "perf_value", json!(600));
            save(&mut s, "alert", Some(&alert_id), &w).await;
            let cond = scalar(&mut s, &format!("SELECT performance_condition FROM msdb.dbo.sysalerts WHERE id = {alert_id}")).await;
            assert_eq!(cond, json!(format!("{object}|User Connections||>|600")), "{stored}");
        }
        run(
            &mut s,
            &format!(
                "EXEC msdb.dbo.sp_update_alert @name = N'{ALERT}', @performance_condition = N'{object}|User Connections||>|500';"
            ),
        )
        .await;

        // Its actions: the count reset (counted since 2020 until now),
        // disable and enable, each a script first.
        let actions_of = |list: &dbine_driver::SchedulerList| {
            list.items.iter().find(|i| i.id == alert_id).unwrap().actions.clone()
        };
        let list = s.scheduler_items("alert").await.expect("items");
        assert_eq!(actions_of(&list), Some(vec!["reset_count".to_string(), "disable".to_string()]));
        run(
            &mut s,
            &format!(
                "EXEC msdb.dbo.sp_update_alert @name = N'{ALERT}', @count_reset_date = 20200101, @count_reset_time = 0;"
            ),
        )
        .await;
        let script = s.scheduler_action_script("alert", &alert_id, "reset_count").await.expect("script");
        assert!(script.contains("@occurrence_count = 0") && script.contains("@count_reset_date = @resetDate"), "{script}");
        s.scheduler_action("alert", &alert_id, "reset_count").await.expect("reset");
        let r = rows(
            &mut s,
            &format!(
                "SELECT occurrence_count,
                        CASE WHEN count_reset_date = CONVERT(int, CONVERT(char(8), GETDATE(), 112)) THEN 1 ELSE count_reset_date END
                   FROM msdb.dbo.sysalerts WHERE id = {alert_id}"
            ),
        )
        .await;
        assert_eq!(r[0], [json!(0), json!(1)], "the count starts again today");
        let form = s.scheduler_form("alert", Some(&alert_id)).await.expect("form");
        assert_ne!(form.values["count_reset"], json!("2020-01-01 00:00:00"));
        let enabled = format!("SELECT enabled FROM msdb.dbo.sysalerts WHERE id = {alert_id}");
        s.scheduler_action("alert", &alert_id, "disable").await.expect("disable");
        assert_eq!(scalar(&mut s, &enabled).await, json!(0));
        let list = s.scheduler_items("alert").await.expect("items");
        assert_eq!(actions_of(&list), Some(vec!["reset_count".to_string(), "enable".to_string()]));
        s.scheduler_action("alert", &alert_id, "enable").await.expect("enable");
        assert_eq!(scalar(&mut s, &enabled).await, json!(1));
        // Keys it doesn't have, on alerts or other kinds, are refused.
        for (kind, id, action) in [
            ("alert", alert_id.as_str(), "run"),
            ("alert", alert_id.as_str(), "reset_count'; DROP TABLE t; --"),
            ("job", job_id.as_str(), "reset_count"),
            ("operator", op_id.as_str(), "enable"),
        ] {
            let e = s.scheduler_action_script(kind, id, action).await.unwrap_err();
            assert!(matches!(&e, Error::Query(m) if m.contains("no es una acción")), "{kind} {action}: {e:?}");
            let e = s.scheduler_action(kind, id, action).await.unwrap_err();
            assert!(matches!(&e, Error::Query(m) if m.contains("no es una acción")), "{kind} {action}: {e:?}");
        }
        let e = s.scheduler_action_script("alert", "1; DROP TABLE t", "enable").await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("no es un id")), "{e:?}");

        // The job's form reads it all back.
        let form = s.scheduler_form("job", Some(&job_id)).await.expect("form");
        assert!(!form.read_only, "{:?}", form.notes);
        let v = values_of(&form);
        assert_eq!(v.values["name"], json!(JOB));
        assert_eq!(v.values["start_step"], json!("one"));
        assert_eq!(v.values["email_operator"], json!(OP));
        assert_eq!(v.lists["steps"].len(), 3);
        assert_eq!(v.lists["steps"][1]["on_fail_step"], json!("one"));
        assert_eq!(v.lists["schedules"].len(), 1);
        assert_eq!(v.lists["schedules"][0]["name"], json!(OWN));
        assert_eq!(v.lists["shared_schedules"].len(), 1);
        assert_eq!(v.lists["shared_schedules"][0]["schedule"], json!(shared_id));
        assert_eq!(v.lists["alerts"].len(), 1);
        assert_eq!(v.lists["alerts"][0]["alert"], json!(ALERT));
        // Unchanged, it changes nothing.
        let script = s.scheduler_script("job", Some(&job_id), &v).await.unwrap();
        assert!(script.contains("no hay cambios"), "{script}");

        // A run, so its history can be seen surviving the edit.
        s.run_job(&job_id).await.expect("run");
        wait_for_runs(&mut s, &job_id, 1).await;

        // Edit in place: rename the job and a step, move "three" first,
        // insert a step, change the schedule to weekly, swap e-mail for
        // the pager.
        let mut v = values_of(&form);
        set(&mut v, "name", json!(JOB2));
        set(&mut v, "notify_email", json!(false));
        set(&mut v, "notify_page", json!(true));
        set(&mut v, "page_operator", json!(OP));
        set(&mut v, "page_when", json!("completion"));
        let mut steps = v.lists["steps"].clone();
        steps[1].insert("name".into(), json!("two (renamed)"));
        let three = steps.remove(2);
        steps.insert(0, three);
        steps.insert(
            2,
            new_row(&form, "steps", json!({"name": "inserted", "command": "SELECT 'new';", "on_fail": "goto", "on_fail_step": "three"})),
        );
        v.lists.insert("steps".into(), steps);
        set(&mut v, "start_step", json!("one"));
        let own = &mut v.lists.get_mut("schedules").unwrap()[0];
        own.insert("type".into(), json!("weekly"));
        own.insert("weekdays".into(), json!(["tuesday", "thursday"]));
        own.insert("every_weeks".into(), json!(1));
        let script = s.scheduler_script("job", Some(&job_id), &v).await.unwrap();
        assert!(!script.contains("sp_delete_job") && !script.contains("sp_add_job "), "{script}");
        assert!(script.contains("sp_update_schedule"), "{script}");
        save(&mut s, "job", Some(&job_id), &v).await;
        let steps = rows(&mut s, &job_steps_sql(&job_id)).await;
        let names: Vec<&str> = steps.iter().map(|r| r[1].as_str().unwrap()).collect();
        assert_eq!(names, ["three", "one", "inserted", "two (renamed)"]);
        // "inserted" fails over to "three" (step 1); "two" to "one" (2).
        assert_eq!((steps[2][4].clone(), steps[2][5].clone()), (json!(4), json!(1)));
        assert_eq!((steps[3][4].clone(), steps[3][5].clone()), (json!(4), json!(2)));
        let r = rows(
            &mut s,
            &format!(
                "SELECT j.name, j.start_step_id, j.notify_level_email, j.notify_level_page,
                        (SELECT freq_type FROM msdb.dbo.sysschedules WHERE name = N'{OWN}'),
                        (SELECT freq_interval FROM msdb.dbo.sysschedules WHERE name = N'{OWN}'),
                        (SELECT COUNT(*) FROM msdb.dbo.sysjobhistory WHERE job_id = j.job_id AND step_id = 0)
                   FROM msdb.dbo.sysjobs j WHERE j.job_id = '{job_id}'"
            ),
        )
        .await;
        assert_eq!(
            r[0],
            [json!(JOB2), json!(2), json!(0), json!(3), json!(8), json!(4 | 16), json!(1)],
            "edited in place: same job, its history kept"
        );

        // Delete a step and detach the shared schedule.
        let form = s.scheduler_form("job", Some(&job_id)).await.expect("form");
        let mut v = values_of(&form);
        v.lists.get_mut("steps").unwrap().remove(2);
        v.lists.insert("shared_schedules".into(), Vec::new());
        let script = s.scheduler_script("job", Some(&job_id), &v).await.unwrap();
        assert!(script.contains("sp_delete_jobstep @job_id = @jobId,\n    @step_id = 3"), "{script}");
        assert!(script.contains("sp_detach_schedule"), "{script}");
        save(&mut s, "job", Some(&job_id), &v).await;
        let names: Vec<String> = rows(&mut s, &job_steps_sql(&job_id))
            .await
            .iter()
            .map(|r| r[1].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["three", "one", "two (renamed)"]);
        let n = scalar(
            &mut s,
            &format!("SELECT COUNT(*) FROM msdb.dbo.sysjobschedules WHERE job_id = '{job_id}'"),
        )
        .await;
        assert_eq!(n, json!(1));
        let n = scalar(
            &mut s,
            &format!("SELECT COUNT(*) FROM msdb.dbo.sysschedules WHERE schedule_id = {shared_id}"),
        )
        .await;
        assert_eq!(n, json!(1), "a shared schedule stays when detached");

        // Start at step 2 ("one"): steps 2 and 3 run, not 1.
        let before = run_count(&mut s, &job_id).await;
        s.run_job_from(&job_id, "one").await.expect("run from");
        wait_for_runs(&mut s, &job_id, before + 1).await;
        let first = scalar(
            &mut s,
            &format!(
                "SELECT MIN(step_id) FROM msdb.dbo.sysjobhistory
                  WHERE job_id = '{job_id}' AND step_id > 0
                    AND instance_id > (SELECT MAX(instance_id) FROM msdb.dbo.sysjobhistory WHERE job_id = '{job_id}' AND step_id = 0 AND instance_id < (SELECT MAX(instance_id) FROM msdb.dbo.sysjobhistory WHERE job_id = '{job_id}' AND step_id = 0))"
            ),
        )
        .await;
        assert_eq!(first, json!(2), "the run started at step 2");
        let e = s.run_job_from(&job_id, "no such step").await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("no tiene un paso")), "{e:?}");

        // Clear its history.
        s.clear_job_history(&job_id).await.expect("clear");
        assert_eq!(run_count(&mut s, &job_id).await, 0);

        // The operator: renamed, a pager, its alert by e-mail only.
        let form = s.scheduler_form("operator", Some(&op_id)).await.expect("form");
        let mut v = values_of(&form);
        assert_eq!(v.values["pager_days"], json!(["monday", "friday"]));
        assert_eq!(v.lists["alerts"].len(), 1, "the alert notifies it");
        assert_eq!(v.lists["jobs"].len(), 1, "the job pages it");
        // When, by each way, in words the app translates whole.
        let job_row = &v.lists["jobs"][0];
        assert_eq!(job_row["pager"], json!("Cuando el job se complete"), "{job_row:?}");
        assert_eq!(job_row["email"], json!(""), "{job_row:?}");
        set(&mut v, "name", json!(OP2));
        set(&mut v, "pager", json!("pager@example.com"));
        v.lists.get_mut("alerts").unwrap()[0].insert("pager".into(), json!(false));
        let script = s.scheduler_script("operator", Some(&op_id), &v).await.unwrap();
        assert!(script.contains("@new_name = N'dbine_test_sched_op2'"), "{script}");
        assert!(script.contains("sp_update_notification"), "{script}");
        save(&mut s, "operator", Some(&op_id), &v).await;
        let r = rows(
            &mut s,
            &format!(
                "SELECT o.name, o.pager_address, n.notification_method
                   FROM msdb.dbo.sysoperators o JOIN msdb.dbo.sysnotifications n ON n.operator_id = o.id
                  WHERE o.id = {op_id}"
            ),
        )
        .await;
        assert_eq!(r[0], [json!(OP2), json!("pager@example.com"), json!(1)]);

        // The shared schedule: renamed, a quarter past four.
        let form = s.scheduler_form("schedule", Some(&shared_id)).await.expect("form");
        assert_eq!(form.sections[1].lists[0].rows.len(), 1, "job B still uses it");
        let mut v = values_of(&form);
        assert_eq!(v.values["time"], json!("02:30:00"));
        set(&mut v, "name", json!(SHARED2));
        set(&mut v, "time", json!("04:15:00"));
        let script = s.scheduler_script("schedule", Some(&shared_id), &v).await.unwrap();
        assert!(script.contains("sp_update_schedule @schedule_id = "), "{script}");
        assert!(!script.contains("@freq_type"), "only what changed: {script}");
        save(&mut s, "schedule", Some(&shared_id), &v).await;
        let r = rows(
            &mut s,
            &format!("SELECT name, active_start_time, freq_interval FROM msdb.dbo.sysschedules WHERE schedule_id = {shared_id}"),
        )
        .await;
        assert_eq!(r[0], [json!(SHARED2), json!(41500), json!(2 | 8)]);

        // The category, renamed: the job keeps it.
        let form = s.scheduler_form("category", Some(&cat_id)).await.expect("form");
        assert_eq!(form.sections[1].lists[0].rows.len(), 1, "the job is in it");
        let mut v = values_of(&form);
        set(&mut v, "name", json!(CAT2));
        save(&mut s, "category", Some(&cat_id), &v).await;
        let r = rows(
            &mut s,
            &format!(
                "SELECT c.name FROM msdb.dbo.sysjobs j JOIN msdb.dbo.syscategories c ON c.category_id = j.category_id WHERE j.job_id = '{job_id}'"
            ),
        )
        .await;
        assert_eq!(r[0][0], json!(CAT2));

        // Read-only: the forms show, nothing changes.
        let mut ro = ReadOnlySession::new(connect(&cfg).await);
        let form = ro.scheduler_form("job", Some(&job_id)).await.expect("form");
        assert!(form.read_only);
        let v = values_of(&form);
        assert!(ro.scheduler_script("job", Some(&job_id), &v).await.is_ok());
        assert!(ro.scheduler_action_script("alert", &alert_id, "disable").await.is_ok());
        for r in [
            ro.scheduler_save("job", Some(&job_id), &v).await.map(|_| ()),
            ro.scheduler_delete("job", &job_id).await,
            ro.run_job_from(&job_id, "one").await,
            ro.clear_job_history(&job_id).await,
            ro.scheduler_action("alert", &alert_id, "disable").await,
        ] {
            assert!(matches!(&r, Err(Error::Query(m)) if m.contains("solo lectura")), "{r:?}");
        }
        drop(ro);

        // Ids from the app are checked before they reach the server.
        for (kind, bad) in [
            ("job", "x'; DROP TABLE t; --"),
            ("schedule", "1; DROP TABLE t"),
            ("operator", "-1"),
            ("alert", "1e3"),
            ("proxy", "' OR 1=1 --"),
            ("category", "99999999999"),
        ] {
            let e = s.scheduler_delete_script(kind, bad).await.unwrap_err();
            assert!(matches!(&e, Error::Query(m) if m.contains("no es un id")), "{kind} {bad}: {e:?}");
        }
        assert!(s.scheduler_items("x'; --").await.is_err());

        // Delete everything through the designer's calls.
        for (kind, id) in [
            ("alert", alert_id.clone()),
            ("job", job_id.clone()),
            ("schedule", shared_id.clone()),
            ("proxy", proxy_id.clone()),
            ("operator", op_id.clone()),
            ("category", cat_id.clone()),
        ] {
            let script = s.scheduler_delete_script(kind, &id).await.expect("delete script");
            assert!(script.contains("sp_delete_"), "{script}");
            s.scheduler_delete(kind, &id).await.unwrap_or_else(|e| panic!("delete {kind}: {e:?}"));
            let left = s.scheduler_items(kind).await.expect("items");
            assert!(!left.items.iter().any(|i| i.id == id), "{kind} {id} still there");
        }
        // The job's own schedule went with it.
        let n = scalar(
            &mut s,
            &format!("SELECT COUNT(*) FROM msdb.dbo.sysschedules WHERE name = N'{OWN}'"),
        )
        .await;
        assert_eq!(n, json!(0));
        // A system category can't be deleted.
        let e = s.scheduler_delete_script("category", "0").await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("del sistema")), "{e:?}");
        // The session is left as it was: nothing open.
        assert_eq!(scalar(&mut s, "SELECT @@TRANCOUNT").await, json!(0));
    })
    .await;
}

async fn run_count(s: &mut Box<dyn Session>, job: &str) -> i64 {
    scalar(
        s,
        &format!(
            "SELECT COUNT(*) FROM msdb.dbo.sysjobhistory WHERE job_id = '{job}' AND step_id = 0"
        ),
    )
    .await
    .as_i64()
    .unwrap()
}

async fn wait_for_runs(s: &mut Box<dyn Session>, job: &str, n: i64) {
    let start = Instant::now();
    while run_count(s, job).await < n {
        assert!(start.elapsed() < Duration::from_secs(90), "{job}: no run");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// A refused change leaves nothing behind: the script's transaction is
/// rolled back and the server's own message comes back.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_refused_change_is_rolled_back() {
    let Some(cfg) = cfg() else { return };
    let c2 = cfg.clone();
    with_cleanup(&cfg, async move {
        let mut s = connect(&c2).await;
        run(&mut s, CLEANUP).await;
        // The job and its schedule go in first; msdb then refuses the
        // step's user (it checks that a SQL user exists in the database).
        let form = s.scheduler_form("job", None).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "name", json!(JOB));
        v.lists.insert(
            "steps".into(),
            vec![new_row(
                &form,
                "steps",
                json!({"name": "one", "command": "SELECT 1;", "database_user": "dbine_test_sched_nobody"}),
            )],
        );
        v.lists.insert(
            "schedules".into(),
            vec![new_row(&form, "schedules", json!({"name": OWN, "type": "daily"}))],
        );
        let e = s.scheduler_save("job", None, &v).await.unwrap_err();
        eprintln!("{e:?}");
        assert!(
            matches!(&e, Error::Query(m) if m.contains("dbine_test_sched_nobody")),
            "the server's own message: {e:?}"
        );
        for table in ["sysjobs", "sysschedules"] {
            let n = scalar(
                &mut s,
                &format!("SELECT COUNT(*) FROM msdb.dbo.{table} WHERE name LIKE N'dbine[_]test[_]sched[_]%'"),
            )
            .await;
            assert_eq!(n, json!(0), "nothing of the refused script stays in {table}");
        }
        assert_eq!(scalar(&mut s, "SELECT @@TRANCOUNT").await, json!(0));
    })
    .await;
}

const USER: &str = "dbine_test_sched_user";

/// A login that isn't sysadmin (SQLAgentReaderRole): jobs and schedules
/// of its own, the rest read-only; a job of another login read-only.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_login_without_sysadmin() {
    let Some(cfg) = cfg() else { return };
    let c2 = cfg.clone();
    with_cleanup(&cfg, async move {
        let mut sa = connect(&c2).await;
        run(&mut sa, CLEANUP).await;
        run(
            &mut sa,
            &format!(
                "CREATE LOGIN [{USER}] WITH PASSWORD = N'{PASSWORD}', CHECK_POLICY = OFF;
                 EXEC msdb.sys.sp_executesql N'CREATE USER [{USER}] FOR LOGIN [{USER}]; ALTER ROLE SQLAgentReaderRole ADD MEMBER [{USER}];';
                 EXEC msdb.dbo.sp_add_operator @name = N'{OP}', @email_address = N'x@example.com';
                 EXEC msdb.dbo.sp_add_job @job_name = N'{JOB_B}';
                 EXEC msdb.dbo.sp_add_jobstep @job_name = N'{JOB_B}', @step_name = N'sa step', @command = N'SELECT 1';
                 EXEC msdb.dbo.sp_add_schedule @schedule_name = N'{SA_OWN}', @freq_type = 4, @freq_interval = 1;"
            ),
        )
        .await;
        let user_cfg = ConnectionConfig {
            username: Some(USER.into()),
            password: Some(PASSWORD.into()),
            ..c2.clone()
        };
        let mut s = connect(&user_cfg).await;
        let info = s.scheduler_info().await.expect("info");
        eprintln!("{info:?}");
        // It starts its own jobs at a step; purging history is
        // SQLAgentOperatorRole's (msdb grants it to no other role).
        let job_kind = info.kinds.iter().find(|k| k.key == "job").unwrap();
        assert!(job_kind.run_from_step && !job_kind.clear_history, "{job_kind:?}");
        // sa's job and schedule: listed, but not offered for changes.
        let jobs = s.scheduler_items("job").await.expect("items");
        let theirs = jobs.items.iter().find(|i| i.name == JOB_B).unwrap();
        assert_eq!((theirs.can_edit, theirs.can_delete), (Some(false), Some(false)));
        let e = s.scheduler_delete_script("job", &theirs.id).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("propietario")), "{e:?}");
        let schedules = s.scheduler_items("schedule").await.expect("items");
        let theirs = schedules.items.iter().find(|i| i.name == SA_OWN).expect("sa's schedule listed");
        assert_eq!((theirs.can_edit, theirs.can_delete), (Some(false), Some(false)));
        let e = s.scheduler_delete_script("schedule", &theirs.id).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("propietario")), "{e:?}");
        let kinds: Vec<(&str, bool)> = info
            .kinds
            .iter()
            .map(|k| (k.key.as_str(), k.can_create))
            .collect();
        assert_eq!(
            kinds,
            [
                ("job", true),
                ("schedule", true),
                ("operator", false),
                ("proxy", false),
                ("category", false)
            ],
            "no alerts without SQLAgentOperatorRole"
        );
        assert!(!info.logs);
        assert!(info.notes.iter().any(|n| n.contains("sysadmin")), "{:?}", info.notes);

        // An operator: shown, not changed.
        let op_id = find_item(&mut s, "operator", OP).await;
        let form = s.scheduler_form("operator", Some(&op_id)).await.expect("form");
        assert!(form.read_only, "{form:?}");
        let mut v = values_of(&form);
        set(&mut v, "email", json!("y@example.com"));
        let e = s.scheduler_save("operator", Some(&op_id), &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("sysadmin")), "{e:?}");

        // sa's job: read-only here.
        let other = find_item(&mut s, "job", JOB_B).await;
        let form = s.scheduler_form("job", Some(&other)).await.expect("form");
        assert!(form.read_only, "{:?}", form.notes);
        assert!(form.notes.iter().any(|n| n.contains("propietario")), "{:?}", form.notes);
        let e = s
            .scheduler_save("job", Some(&other), &values_of(&form))
            .await
            .unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("propietario")), "{e:?}");

        // A job of its own: created, edited, deleted; what only sysadmin
        // sets stays out.
        let form = s.scheduler_form("job", None).await.expect("form");
        let owner = form
            .sections
            .iter()
            .flat_map(|s| s.fields.iter())
            .find(|f| f.key == "owner")
            .unwrap();
        assert!(owner.read_only);
        let mut v = values_of(&form);
        set(&mut v, "name", json!(JOB));
        set(&mut v, "owner", json!("sa"));
        v.lists.insert(
            "steps".into(),
            vec![new_row(
                &form,
                "steps",
                json!({"name": "mine", "command": "SELECT 1;", "output_file": "/tmp/x.txt"}),
            )],
        );
        v.lists.insert(
            "schedules".into(),
            vec![new_row(&form, "schedules", json!({"name": OWN, "type": "daily"}))],
        );
        let id = save(&mut s, "job", None, &v).await;
        let r = rows(
            &mut sa,
            &format!(
                "SELECT SUSER_SNAME(j.owner_sid), s.output_file_name
                   FROM msdb.dbo.sysjobs j JOIN msdb.dbo.sysjobsteps s ON s.job_id = j.job_id
                  WHERE j.job_id = '{id}'"
            ),
        )
        .await;
        assert_eq!(r[0], [json!(USER), Value::Null]);
        let form = s.scheduler_form("job", Some(&id)).await.expect("form");
        assert!(!form.read_only, "{:?}", form.notes);
        let mut v = values_of(&form);
        v.lists.get_mut("steps").unwrap()[0].insert("name".into(), json!("mine, renamed"));
        save(&mut s, "job", Some(&id), &v).await;
        let name = scalar(
            &mut sa,
            &format!("SELECT step_name FROM msdb.dbo.sysjobsteps WHERE job_id = '{id}'"),
        )
        .await;
        assert_eq!(name, json!("mine, renamed"));
        // Its own job and schedule: as their kinds say.
        let jobs = s.scheduler_items("job").await.expect("items");
        let mine = jobs.items.iter().find(|i| i.id.eq_ignore_ascii_case(&id)).unwrap();
        assert_eq!((mine.can_edit, mine.can_delete), (None, None));
        let schedules = s.scheduler_items("schedule").await.expect("items");
        let own = schedules.items.iter().find(|i| i.name == OWN).expect("its schedule listed");
        assert_eq!((own.can_edit, own.can_delete), (None, None));
        // It starts its job at a step; clearing the history, which the
        // job kind doesn't offer it, msdb refuses.
        s.run_job_from(&id, "mine, renamed").await.expect("run from");
        wait_for_runs(&mut sa, &id, 1).await;
        let e = s.clear_job_history(&id).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("sp_purge_jobhistory")), "{e:?}");
        assert_eq!(run_count(&mut sa, &id).await, 1);
        // A form read before a change made elsewhere is refused.
        run(
            &mut sa,
            &format!("EXEC msdb.dbo.sp_update_job @job_id = '{id}', @description = N'elsewhere';"),
        )
        .await;
        let e = s.scheduler_save("job", Some(&id), &v).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("recargalo")), "{e:?}");
        s.scheduler_delete("job", &id).await.expect("delete");
        let n = scalar(
            &mut sa,
            &format!("SELECT COUNT(*) FROM msdb.dbo.sysjobs WHERE job_id = '{id}'"),
        )
        .await;
        assert_eq!(n, json!(0));
        drop(s);
    })
    .await;
}

const SA_OWN: &str = "dbine_test_sched_sa_own";

/// sysadmin makes a job for a login that isn't (SQLAgentReaderRole): the
/// job's schedule is the login's, as SSMS makes it, so the login changes
/// and removes it; a schedule sysadmin attached to it comes among the
/// shared ones and is only detached. Also: every step taken off a job
/// whose start step wasn't 1, and the Agent's log with VIEW ANY ERROR LOG.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn a_job_made_by_sysadmin_for_another_login() {
    let Some(cfg) = cfg() else { return };
    let c2 = cfg.clone();
    with_cleanup(&cfg, async move {
        let mut sa = connect(&c2).await;
        run(&mut sa, CLEANUP).await;
        run(
            &mut sa,
            &format!(
                "CREATE LOGIN [{USER}] WITH PASSWORD = N'{PASSWORD}', CHECK_POLICY = OFF;
                 EXEC msdb.sys.sp_executesql N'CREATE USER [{USER}] FOR LOGIN [{USER}]; ALTER ROLE SQLAgentReaderRole ADD MEMBER [{USER}];';"
            ),
        )
        .await;

        // sa: the job, owned by the login, three steps starting at the
        // third, its own schedule.
        let form = sa.scheduler_form("job", None).await.expect("form");
        let mut v = values_of(&form);
        set(&mut v, "name", json!(JOB));
        set(&mut v, "owner", json!(USER));
        set(&mut v, "start_step", json!("three"));
        v.lists.insert(
            "steps".into(),
            ["one", "two", "three"]
                .iter()
                .map(|n| new_row(&form, "steps", json!({"name": n, "command": "SELECT 1;"})))
                .collect(),
        );
        v.lists.insert(
            "schedules".into(),
            vec![new_row(
                &form,
                "schedules",
                json!({"name": OWN, "type": "daily", "every_days": 1, "time": "03:00:00"}),
            )],
        );
        let script = sa.scheduler_script("job", None, &v).await.unwrap();
        assert!(script.contains("sp_add_jobschedule @job_id = @jobId"), "{script}");
        let id = save(&mut sa, "job", None, &v).await;
        let r = rows(
            &mut sa,
            &format!(
                "SELECT SUSER_SNAME(j.owner_sid), j.start_step_id, SUSER_SNAME(s.owner_sid)
                   FROM msdb.dbo.sysjobs j
                   JOIN msdb.dbo.sysjobschedules js ON js.job_id = j.job_id
                   JOIN msdb.dbo.sysschedules s ON s.schedule_id = js.schedule_id
                  WHERE j.job_id = '{id}'"
            ),
        )
        .await;
        assert_eq!(r, [[json!(USER), json!(3), json!(USER)]], "the schedule is the job owner's");
        // sa attaches a schedule of its own to the job.
        run(
            &mut sa,
            &format!(
                "DECLARE @sid int;
                 EXEC msdb.dbo.sp_add_schedule @schedule_name = N'{SA_OWN}', @freq_type = 4, @freq_interval = 1, @schedule_id = @sid OUTPUT;
                 EXEC msdb.dbo.sp_attach_schedule @job_id = '{id}', @schedule_id = @sid;"
            ),
        )
        .await;
        let sa_own = scalar(
            &mut sa,
            &format!("SELECT schedule_id FROM msdb.dbo.sysschedules WHERE name = N'{SA_OWN}'"),
        )
        .await;

        // The login: its schedule is its own to change; sa's comes with
        // the shared ones.
        let user_cfg = ConnectionConfig {
            username: Some(USER.into()),
            password: Some(PASSWORD.into()),
            ..c2.clone()
        };
        let mut s = connect(&user_cfg).await;
        let form = s.scheduler_form("job", Some(&id)).await.expect("form");
        assert!(!form.read_only, "{:?}", form.notes);
        let mut v = values_of(&form);
        assert_eq!(v.lists["schedules"].len(), 1, "{:?}", v.lists["schedules"]);
        assert_eq!(v.lists["schedules"][0]["name"], json!(OWN));
        assert_eq!(v.lists["shared_schedules"].len(), 1);
        assert_eq!(v.lists["shared_schedules"][0]["schedule"], json!(sa_own.to_string()));
        v.lists.get_mut("schedules").unwrap()[0].insert("time".into(), json!("04:00:00"));
        save(&mut s, "job", Some(&id), &v).await;
        let t = scalar(
            &mut sa,
            &format!("SELECT active_start_time FROM msdb.dbo.sysschedules WHERE name = N'{OWN}'"),
        )
        .await;
        assert_eq!(t, json!(40000), "the login changed its schedule");

        // Both taken off: its own deleted, sa's only detached (msdb
        // refuses the login deleting sa's).
        let form = s.scheduler_form("job", Some(&id)).await.expect("form");
        let mut v = values_of(&form);
        v.lists.insert("schedules".into(), Vec::new());
        v.lists.insert("shared_schedules".into(), Vec::new());
        let script = s.scheduler_script("job", Some(&id), &v).await.unwrap();
        assert!(
            script.contains(&format!("@schedule_id = {sa_own},\n    @delete_unused_schedule = 0;")),
            "{script}"
        );
        save(&mut s, "job", Some(&id), &v).await;
        let r = rows(
            &mut sa,
            &format!(
                "SELECT (SELECT COUNT(*) FROM msdb.dbo.sysjobschedules WHERE job_id = '{id}'),
                        (SELECT COUNT(*) FROM msdb.dbo.sysschedules WHERE name = N'{OWN}'),
                        (SELECT COUNT(*) FROM msdb.dbo.sysschedules WHERE name = N'{SA_OWN}')"
            ),
        )
        .await;
        assert_eq!(r[0], [json!(0), json!(0), json!(1)]);

        // Every step off a job that starts at the third, with another
        // change of the job: it starts at 1 again.
        let form = s.scheduler_form("job", Some(&id)).await.expect("form");
        let mut v = values_of(&form);
        v.lists.insert("steps".into(), Vec::new());
        set(&mut v, "description", json!("no steps"));
        save(&mut s, "job", Some(&id), &v).await;
        let r = rows(
            &mut sa,
            &format!(
                "SELECT start_step_id, description, (SELECT COUNT(*) FROM msdb.dbo.sysjobsteps WHERE job_id = '{id}')
                   FROM msdb.dbo.sysjobs WHERE job_id = '{id}'"
            ),
        )
        .await;
        assert_eq!(r[0], [json!(1), json!("no steps"), json!(0)]);

        // VIEW ANY ERROR LOG (SQL Server 2022) reads the Agent's log.
        assert!(!s.scheduler_info().await.expect("info").logs);
        drop(s);
        run(&mut sa, &format!("GRANT VIEW ANY ERROR LOG TO [{USER}];")).await;
        let mut s = connect(&user_cfg).await;
        let info = s.scheduler_info().await.expect("info");
        assert!(info.logs, "{info:?}");
        let logs = s.scheduler_logs().await.expect("logs");
        assert!(!logs.is_empty());
        drop(s);
    })
    .await;
}
