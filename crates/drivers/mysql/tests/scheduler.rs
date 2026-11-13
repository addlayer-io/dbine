//! The scheduler designer against real servers: MySQL's and MariaDB's (and
//! OceanBase's) events, Doris' jobs and StarRocks' tasks, with the refresh
//! settings of their materialized views. Each test reads
//! `DBINE_TEST_<ENGINE>_URL` (`mysql://user:pass@host:port`) and is skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_OCEANBASE_URL=mysql://root@test:pw@localhost:25035 \
//! DBINE_TEST_STARROCKS_URL=mysql://root@localhost:25030 \
//! DBINE_TEST_DORIS_URL=mysql://root@localhost:25031 \
//!   cargo test -p dbine-driver-mysql --test scheduler -- --ignored --test-threads=1
//! ```
//!
//! Everything a test creates is named `dbine_test_sched_*` and dropped at
//! the end, also when an assertion fails (the checks run under
//! `catch_unwind`, the cleanup after them on a fresh connection).

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{
    ConnectionConfig, Error, FormValues, ObjectForm, QueryOutcome, Session, TxState,
};
use futures::FutureExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

const DB: &str = "dbine_test_sched_db";
const DB2: &str = "dbine_test_sched_db2";

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn connect(driver: &str, cfg: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    assert!(d.capabilities().scheduler, "{driver} has the scheduler");
    d.connect(cfg, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.map(|_| out)
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The last result's rows, each cell as text (NULL as "NULL").
async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<String>> {
    let o = run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    o.results
        .iter()
        .rfind(|r| !r.columns.is_empty())
        .map(|r| {
            r.rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|c| match c {
                            Value::String(v) => v.clone(),
                            Value::Null => "NULL".into(),
                            v => v.to_string(),
                        })
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn one_row(s: &mut Box<dyn Session>, sql: &str) -> Vec<String> {
    rows(s, sql)
        .await
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("a row for {sql}"))
}

/// Runs `checks`, then `cleanup` on a fresh connection whatever happened,
/// then fails if the checks did.
async fn guarded<F, C>(driver: &str, cfg: &ConnectionConfig, checks: F, cleanup: C)
where
    F: std::future::Future<Output = ()>,
    C: AsyncFnOnce(&mut Box<dyn Session>),
{
    let outcome = AssertUnwindSafe(checks).catch_unwind().await;
    let mut s = connect(driver, cfg).await;
    cleanup(&mut s).await;
    if let Err(p) = outcome {
        std::panic::resume_unwind(p);
    }
}

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

fn set(v: &mut FormValues, k: &str, x: Value) {
    v.values.insert(k.into(), x);
}

fn props(v: &mut FormValues, pairs: &[(&str, &str)]) {
    v.lists.insert(
        "properties".into(),
        pairs
            .iter()
            .map(|(k, x)| {
                BTreeMap::from([
                    ("key".to_string(), json!(k)),
                    ("value".to_string(), json!(x)),
                ])
            })
            .collect(),
    );
}

async fn script(s: &mut Box<dyn Session>, kind: &str, id: Option<&str>, v: &FormValues) -> String {
    let script = s
        .scheduler_script(kind, id, v)
        .await
        .unwrap_or_else(|e| panic!("script {kind} {id:?}: {e:?}"));
    eprintln!("--- {kind} {id:?}\n{script}");
    script
}

async fn save(s: &mut Box<dyn Session>, kind: &str, id: Option<&str>, v: &FormValues) -> String {
    let sc = script(s, kind, id, v).await;
    s.scheduler_save(kind, id, v)
        .await
        .unwrap_or_else(|e| panic!("save {kind}: {e:?}\n{sc}"))
        .expect("an id")
}

async fn query_err(
    s: &mut Box<dyn Session>,
    kind: &str,
    id: Option<&str>,
    v: &FormValues,
) -> String {
    match s.scheduler_script(kind, id, v).await {
        Err(Error::Query(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Ids the driver didn't hand out are refused before reaching the server.
async fn refuses_foreign_ids(s: &mut Box<dyn Session>, kinds: &[&str], sample: &str) {
    let evil = [
        format!("{sample}; DROP DATABASE {DB}"),
        format!("`{DB}`.`x`; DROP DATABASE {DB}"),
        "x' OR '1'='1".to_string(),
        "job:`a` OR 1=1".to_string(),
        "task:`a`; DROP TASK b".to_string(),
        String::new(),
    ];
    let v = FormValues::default();
    for kind in kinds {
        for id in &evil {
            assert!(s.scheduler_form(kind, Some(id)).await.is_err(), "{id}");
            assert!(
                s.scheduler_script(kind, Some(id), &v).await.is_err(),
                "{id}"
            );
            assert!(s.scheduler_save(kind, Some(id), &v).await.is_err(), "{id}");
            assert!(s.scheduler_delete_script(kind, id).await.is_err(), "{id}");
            assert!(s.scheduler_delete(kind, id).await.is_err(), "{id}");
        }
    }
    assert!(s.scheduler_items("operator").await.is_err());
    let dbs: Vec<String> = rows(s, "SHOW DATABASES")
        .await
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert!(dbs.iter().any(|d| d == DB), "{DB} still exists");
}

/// The form's values, read-only and not, for a field.
fn val(form: &ObjectForm, k: &str) -> Value {
    form.values.get(k).cloned().unwrap_or(Value::Null)
}

// ---- MySQL, MariaDB and OceanBase: events ----

async fn drop_events(s: &mut Box<dyn Session>) {
    for db in [DB, DB2] {
        // OceanBase keeps a dropped database's events: each one first.
        if run(s, &format!("USE {db}")).await.is_ok() {
            let names: Vec<String> = rows(
                s,
                &format!(
                    "SELECT EVENT_NAME FROM information_schema.EVENTS WHERE EVENT_SCHEMA = '{db}'"
                ),
            )
            .await
            .into_iter()
            .map(|r| r[0].clone())
            .collect();
            for n in names {
                let _ = run(s, &format!("DROP EVENT IF EXISTS `{db}`.`{n}`")).await;
            }
            let _ = run(s, "USE information_schema").await;
        }
        let _ = run(s, &format!("DROP DATABASE IF EXISTS {db}")).await;
    }
    for user in [LOGIN, OTHER] {
        let _ = run(s, &format!("DROP USER IF EXISTS '{user}'@'%'")).await;
    }
    if let Ok(o) = run(
        s,
        &format!("SELECT COUNT(*) FROM mysql.user WHERE User IN ('{LOGIN}', '{OTHER}')"),
    )
    .await
    {
        let n = o
            .results
            .iter()
            .rfind(|r| !r.columns.is_empty())
            .map(|r| r.rows[0][0].clone());
        assert!(
            matches!(n, Some(ref v) if v == &json!(0) || v == &json!("0")),
            "login left behind: {n:?}"
        );
    }
    let left = rows(
        s,
        "SELECT COUNT(*) FROM information_schema.EVENTS WHERE EVENT_NAME LIKE 'dbine\\_test\\_sched\\_%' OR EVENT_SCHEMA LIKE 'dbine\\_test\\_sched\\_%'",
    )
    .await;
    assert_eq!(left[0][0], "0", "events left behind");
}

const EV: &str = "dbine_test_sched_ev";
const EV2: &str = "dbine_test_sched_ev 'renamed'";
const ONCE: &str = "dbine_test_sched_once";
const FAILS: &str = "dbine_test_sched_fails";
const TAKEN: &str = "dbine_test_sched_taken";
const OWN: &str = "dbine_test_sched_own";
/// A login with only the EVENT privilege.
const LOGIN: &str = "dbine_test_sched_login";
const LOGIN_PW: &str = "Dbine_test_1";
/// Another plain account (no SYSTEM_USER), and its event.
const OTHER: &str = "dbine_test_sched_other";
const OTHERS: &str = "dbine_test_sched_others";

fn event_row_sql(db: &str, name: &str) -> String {
    format!(
        "SELECT STATUS, INTERVAL_VALUE, INTERVAL_FIELD, DATE_FORMAT(STARTS, '%Y-%m-%d %H:%i:%s'), IFNULL(DATE_FORMAT(ENDS, '%Y-%m-%d %H:%i:%s'), 'NULL'), ON_COMPLETION, EVENT_COMMENT, TIME_ZONE, SQL_MODE, EVENT_DEFINITION, DEFINER, EVENT_TYPE
           FROM information_schema.EVENTS WHERE EVENT_SCHEMA = '{db}' AND EVENT_NAME = '{}'",
        name.replace('\'', "''")
    )
}

async fn events(driver: &str, env: &str) {
    let Some(cfg) = cfg(driver, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let ob = driver == "oceanbase";
    let c2 = cfg.clone();
    guarded(
        driver,
        &cfg,
        async move {
            let cfg = c2;
            let mut s = connect(driver, &cfg).await;
            drop_events(&mut s).await;
            exec(&mut s, &format!("CREATE DATABASE {DB}")).await;
            exec(&mut s, &format!("CREATE DATABASE {DB2}")).await;
            exec(&mut s, &format!("CREATE TABLE {DB}.t (a INT)")).await;
            let scheduler = one_row(&mut s, "SELECT @@global.event_scheduler").await[0].clone();
            let version = one_row(&mut s, "SELECT VERSION()").await[0].clone();

            // What the scheduler has.
            let info = s.scheduler_info().await.expect("info");
            eprintln!("{info:?}");
            let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
            assert_eq!(keys, ["job"]);
            assert_eq!(info.kinds[0].label, "Eventos");
            assert!(info.kinds[0].can_create && info.kinds[0].can_edit && info.kinds[0].can_delete);
            // No run now, no steps, no history to clear.
            assert!(info.kinds[0].actions.is_empty(), "{info:?}");
            assert!(!info.kinds[0].run_from_step && !info.kinds[0].clear_history);
            let on = scheduler.eq_ignore_ascii_case("ON") || scheduler == "1";
            assert_eq!(
                info.notes.iter().any(|n| n.contains("event_scheduler")),
                !on,
                "{info:?}"
            );
            if driver == "mysql" {
                assert!(info.logs, "root reads performance_schema.error_log on {version}");
            } else {
                assert!(!info.logs);
            }

            // A new event: every 1:30 HOUR_MINUTE from 2030 to 2031, kept,
            // disabled, with a comment holding a quote and a backslash.
            let form = s.scheduler_form("job", None).await.expect("form");
            assert!(!form.read_only, "{:?}", form.notes);
            assert!(form.choices["database"].iter().any(|(d, _)| d == DB));
            let user = one_row(&mut s, "SELECT CURRENT_USER()").await[0].clone();
            assert_eq!(val(&form, "definer"), json!(user));
            let mut v = values_of(&form);
            set(&mut v, "schema", json!(DB));
            set(&mut v, "name", json!(EV));
            set(&mut v, "type", json!("every"));
            set(&mut v, "every", json!("1:30"));
            set(&mut v, "unit", json!("HOUR_MINUTE"));
            set(&mut v, "starts", json!("2030-01-01T00:00:00"));
            set(&mut v, "ends", json!("2031-01-01T00:00:00"));
            set(&mut v, "completion", json!("preserve"));
            set(&mut v, "status", json!("disable"));
            set(&mut v, "comment", json!("it's a \\ test"));
            set(&mut v, "body", json!("INSERT INTO t VALUES (1);"));
            let before = one_row(&mut s, "SELECT @@session.sql_mode, @@session.time_zone").await;
            let id = save(&mut s, "job", None, &v).await;
            // The session's SQL mode and zone are put back.
            let after = one_row(&mut s, "SELECT @@session.sql_mode, @@session.time_zone").await;
            assert_eq!(after, before);
            assert_eq!(id, format!("`{DB}`.`{EV}`"));
            let listed = s.jobs().await.expect("jobs");
            assert!(listed.jobs.iter().any(|j| j.id == id), "the job list has it");
            let items = s.scheduler_items("job").await.expect("items");
            assert!(items.items.iter().any(|i| i.id == id
                && i.enabled == Some(false)
                && i.can_edit.is_none()
                && i.can_delete.is_none()));
            // The server runs an event only on its schedule.
            match s.scheduler_action_script("job", &id, "run").await {
                Err(Error::Unsupported(m)) => assert!(m.contains("a pedido"), "{m}"),
                other => panic!("an event has no run now: {other:?}"),
            }
            match s.scheduler_action_script("job", &id, "refresh").await {
                Err(Error::Query(m)) => assert!(m.contains("no es una acción"), "{m}"),
                other => panic!("an event has no refresh: {other:?}"),
            }
            assert!(s.scheduler_action("job", &id, "run").await.is_err());
            assert!(s
                .scheduler_action_script("job", &format!("{id}; DROP DATABASE {DB}"), "run")
                .await
                .is_err());
            let r = one_row(&mut s, &event_row_sql(DB, EV)).await;
            assert_eq!(r[0], "DISABLED");
            assert_eq!(r[1].trim_matches('\''), "1:30");
            assert_eq!(r[2], "HOUR_MINUTE");
            assert_eq!(r[3], "2030-01-01 00:00:00");
            assert_eq!(r[4], "2031-01-01 00:00:00");
            assert_eq!(r[5], "PRESERVE");
            assert_eq!(r[6], "it's a \\ test");
            assert_eq!(r[9], "INSERT INTO t VALUES (1)");
            assert_eq!(r[10], user);

            // Opened again, it reads back; saved unchanged, nothing to do.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            assert_eq!(val(&form, "every"), json!("1:30"));
            assert_eq!(val(&form, "starts"), json!("2030-01-01T00:00:00"));
            assert_eq!(val(&form, "status"), json!("disable"));
            assert_eq!(val(&form, "comment"), json!("it's a \\ test"));
            assert_eq!(val(&form, "body"), json!("INSERT INTO t VALUES (1)"));
            let sc = script(&mut s, "job", Some(&id), &values_of(&form)).await;
            assert!(sc.contains("no hay cambios"), "{sc}");

            // The comment alone: only COMMENT, the definer kept.
            let mut v = values_of(&form);
            set(&mut v, "comment", json!("changed"));
            let sc = script(&mut s, "job", Some(&id), &v).await;
            assert!(sc.contains("ALTER DEFINER = ") && sc.contains("COMMENT 'changed'"), "{sc}");
            assert!(!sc.contains("ON SCHEDULE") && !sc.contains("\nDO "), "{sc}");
            save(&mut s, "job", Some(&id), &v).await;
            let r = one_row(&mut s, &event_row_sql(DB, EV)).await;
            assert_eq!(r[6], "changed");
            assert_eq!(r[3], "2030-01-01 00:00:00", "STARTS kept");

            // A change that fails (the new name is another event's) leaves
            // the schedule as it was, also where a first ALTER clears its
            // end (MariaDB, OceanBase): the event gets it back.
            exec(
                &mut s,
                &format!("CREATE EVENT {DB}.{TAKEN} ON SCHEDULE EVERY 1 DAY STARTS '2030-01-01 00:00:00' DISABLE DO SELECT 1"),
            )
            .await;
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "ends", json!(""));
            set(&mut v, "name", json!(TAKEN));
            let sc = script(&mut s, "job", Some(&id), &v).await;
            let e = s.scheduler_save("job", Some(&id), &v).await.unwrap_err();
            if driver != "mysql" {
                assert!(sc.contains("2037-12-31 23:59:59") && sc.contains("\n-- ⚠ El cambio va en dos ALTER EVENT") && sc.contains("le devuelve al evento su programación"), "{sc}");
                assert!(e.to_string().contains("dejó el objeto como estaba"), "{e:?}");
            }
            let r = one_row(&mut s, &event_row_sql(DB, EV)).await;
            assert_eq!(r[11], "RECURRING", "{r:?}");
            assert_eq!((r[1].trim_matches('\''), r[2].as_str()), ("1:30", "HOUR_MINUTE"));
            assert_eq!(r[3], "2030-01-01 00:00:00");
            assert_eq!(r[4], "2031-01-01 00:00:00");
            assert_eq!(r[6], "changed");
            exec(&mut s, &format!("DROP EVENT {DB}.{TAKEN}")).await;

            // No end: the schedule goes whole, STARTS kept.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "ends", json!(""));
            if !ob {
                set(&mut v, "time_zone", json!("+03:00"));
            }
            let sc = script(&mut s, "job", Some(&id), &v).await;
            assert!(sc.contains("ON SCHEDULE EVERY '1:30' HOUR_MINUTE STARTS '2030-01-01 00:00:00'"), "{sc}");
            assert!(!sc.contains("ENDS"), "{sc}");
            save(&mut s, "job", Some(&id), &v).await;
            let r = one_row(&mut s, &event_row_sql(DB, EV)).await;
            assert_eq!(r[4], "NULL");
            assert_eq!(r[3], "2030-01-01 00:00:00");
            if !ob {
                assert_eq!(r[7], "+03:00");
            }

            // A BEGIN … END body under another SQL mode, disabled on the
            // replicas, dropped when it ends.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            let mut v = values_of(&form);
            let body = "BEGIN\n  INSERT INTO t VALUES (2);\n  INSERT INTO t VALUES (3);\nEND";
            set(&mut v, "body", json!(body));
            set(&mut v, "sql_mode", json!("ANSI_QUOTES,STRICT_ALL_TABLES"));
            set(&mut v, "status", json!("replica"));
            set(&mut v, "completion", json!("drop"));
            let sc = script(&mut s, "job", Some(&id), &v).await;
            assert!(sc.contains("DELIMITER") && sc.contains("ON COMPLETION NOT PRESERVE"), "{sc}");
            // REPLICA from MySQL 8.2.0; SLAVE before it and elsewhere.
            let nums: Vec<u64> = version
                .split(|c: char| !c.is_ascii_digit())
                .take(2)
                .map(|p| p.parse().unwrap_or(0))
                .collect();
            let replica = driver == "mysql" && (nums[0], nums[1]) >= (8, 2);
            assert!(sc.contains(if replica { "DISABLE ON REPLICA" } else { "DISABLE ON SLAVE" }), "{sc}");
            save(&mut s, "job", Some(&id), &v).await;
            let r = one_row(&mut s, &event_row_sql(DB, EV)).await;
            assert!(r[0].ends_with("SIDE_DISABLED"), "{r:?}");
            assert_eq!(r[5], "NOT PRESERVE");
            assert!(r[8].contains("ANSI_QUOTES") && r[8].contains("STRICT_ALL_TABLES"), "{r:?}");
            assert_eq!(r[9], body);

            // Moved to another database and renamed (OceanBase doesn't move
            // events): the id follows.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            let mut v = values_of(&form);
            let id = if ob {
                set(&mut v, "schema", json!(DB2));
                let m = query_err(&mut s, "job", Some(&id), &v).await;
                assert!(m.contains("OceanBase no mueve"), "{m}");
                id
            } else {
                set(&mut v, "schema", json!(DB2));
                set(&mut v, "name", json!(EV2));
                set(&mut v, "status", json!("enable"));
                let new_id = save(&mut s, "job", Some(&id), &v).await;
                assert_eq!(new_id, format!("`{DB2}`.`{}`", EV2));
                let r = one_row(&mut s, &event_row_sql(DB2, EV2)).await;
                assert_eq!(r[0], "ENABLED");
                new_id
            };

            // Saving a form older than the event is refused.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            tokio::time::sleep(Duration::from_millis(1100)).await;
            let (sdb, sname) = if ob { (DB, EV) } else { (DB2, EV2) };
            exec(
                &mut s,
                &format!("ALTER EVENT `{sdb}`.`{}` COMMENT 'from elsewhere'", sname.replace('`', "``")),
            )
            .await;
            let mut v = values_of(&form);
            set(&mut v, "comment", json!("mine"));
            let m = query_err(&mut s, "job", Some(&id), &v).await;
            assert!(m.contains("cambió en el servidor"), "{m}");

            // What the form refuses before the server sees it.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            for (k, bad, says) in [
                ("every", json!("1:2:3"), "no es un intervalo"),
                ("every", json!("0"), "no es un intervalo"),
                ("every", json!("1; DROP TABLE t"), "no es un intervalo"),
                ("unit", json!("FORTNIGHT"), "no es una opción válida"),
                ("starts", json!("2030-13-01T00:00:00"), "no es una fecha"),
                ("definer", json!("nobody_here@nowhere"), "no es una cuenta"),
                ("schema", json!("dbine_test_sched_nope"), "No existe la base"),
                ("sql_mode", json!("ANSI'; DROP"), "no es un modo SQL"),
                ("body", json!("  "), "necesita una instrucción"),
                // A second statement would run when saving.
                ("body", json!("INSERT INTO t VALUES (9); DELETE FROM t"), "tiene que ser una sola"),
                ("body", json!("BEGIN INSERT INTO t VALUES (9); END; DELETE FROM t"), "tiene que ser una sola"),
            ] {
                let mut v = values_of(&form);
                set(&mut v, k, bad);
                let m = query_err(&mut s, "job", Some(&id), &v).await;
                assert!(m.contains(says), "{k}: {m}");
            }
            if !ob {
                let mut v = values_of(&form);
                set(&mut v, "time_zone", json!("+03:00'; DROP DATABASE x; --"));
                let m = query_err(&mut s, "job", Some(&id), &v).await;
                assert!(m.contains("no es una zona"), "{m}");
            }

            // Once, at a time: AT.
            let form = s.scheduler_form("job", None).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "schema", json!(DB));
            set(&mut v, "name", json!(ONCE));
            set(&mut v, "type", json!("once"));
            set(&mut v, "at", json!("2030-06-01T12:00:00"));
            set(&mut v, "completion", json!("preserve"));
            set(&mut v, "body", json!("DELETE FROM t"));
            let once = save(&mut s, "job", None, &v).await;
            let r = one_row(
                &mut s,
                &format!("SELECT EVENT_TYPE, DATE_FORMAT(EXECUTE_AT, '%Y-%m-%d %H:%i:%s') FROM information_schema.EVENTS WHERE EVENT_SCHEMA = '{DB}' AND EVENT_NAME = '{ONCE}'"),
            )
            .await;
            assert_eq!(r, ["ONE TIME", "2030-06-01 12:00:00"]);
            let form = s.scheduler_form("job", Some(&once)).await.expect("form");
            assert_eq!(val(&form, "type"), json!("once"));
            assert_eq!(val(&form, "at"), json!("2030-06-01T12:00:00"));

            // An open transaction would be committed by the DDL: refused.
            if s.set_autocommit(false).await.is_ok() {
                exec(&mut s, &format!("INSERT INTO {DB}.t VALUES (42)")).await;
                let form = s.scheduler_form("job", Some(&once)).await.expect("form");
                let mut v = values_of(&form);
                set(&mut v, "comment", json!("in a transaction"));
                let e = s.scheduler_save("job", Some(&once), &v).await.unwrap_err();
                assert!(matches!(&e, Error::State(m) if m.contains("transacción abierta")), "{e:?}");
                exec(&mut s, "ROLLBACK").await;
                s.set_autocommit(true).await.expect("autocommit");
                // Autocommit off with nothing open: the form's catalog
                // reads leave no transaction behind, and the save goes.
                s.set_autocommit(false).await.expect("autocommit off");
                let form = s.scheduler_form("job", Some(&once)).await.expect("form");
                assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
                let mut v = values_of(&form);
                set(&mut v, "comment", json!("autocommit off"));
                save(&mut s, "job", Some(&once), &v).await;
                assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
                s.set_autocommit(true).await.expect("autocommit");
                let r = one_row(&mut s, &event_row_sql(DB, ONCE)).await;
                assert_eq!(r[6], "autocommit off");
            }

            // Read-only connections read forms and scripts, change nothing.
            {
                let mut ro = ReadOnlySession::new(connect(driver, &cfg).await);
                let form = ro.scheduler_form("job", Some(&once)).await.expect("form");
                assert!(form.read_only);
                let mut v = values_of(&form);
                set(&mut v, "comment", json!("read-only"));
                assert!(ro.scheduler_script("job", Some(&once), &v).await.is_ok());
                assert!(ro.scheduler_save("job", Some(&once), &v).await.is_err());
                assert!(ro.scheduler_delete("job", &once).await.is_err());
            }

            // Another login, with only the EVENT privilege, on root's event
            // and on one of its own. MySQL 8.0.16+ locks root's (root holds
            // SYSTEM_USER) to it, even to take it over or drop it; MariaDB
            // has no SYSTEM_USER: it takes root's event over as definer.
            if !ob {
                exec(&mut s, &format!("CREATE USER '{LOGIN}'@'%' IDENTIFIED BY '{LOGIN_PW}'")).await;
                exec(&mut s, &format!("GRANT EVENT, INSERT ON {DB}.* TO '{LOGIN}'@'%'")).await;
                // And another plain account's event (it lacks SYSTEM_USER).
                exec(&mut s, &format!("CREATE USER '{OTHER}'@'%' IDENTIFIED BY '{LOGIN_PW}'")).await;
                exec(&mut s, &format!("GRANT EVENT ON {DB}.* TO '{OTHER}'@'%'")).await;
                exec(
                    &mut s,
                    &format!("CREATE DEFINER = '{OTHER}'@'%' EVENT {DB}.{OTHERS} ON SCHEDULE EVERY 1 DAY STARTS '2030-01-01 00:00:00' DISABLE DO SELECT 1"),
                )
                .await;
                let mut lcfg = cfg.clone();
                lcfg.username = Some(LOGIN.into());
                lcfg.password = Some(LOGIN_PW.into());
                let mut l = connect(driver, &lcfg).await;
                let account = one_row(&mut l, "SELECT CURRENT_USER()").await[0].clone();
                exec(
                    &mut l,
                    &format!("CREATE EVENT {DB}.{OWN} ON SCHEDULE EVERY 1 DAY STARTS '2030-01-01 00:00:00' DISABLE DO SELECT 1"),
                )
                .await;
                let own = format!("`{DB}`.`{OWN}`");
                let items = l.scheduler_items("job").await.expect("items");
                eprintln!("{items:?}");
                let item = |id: &str| items.items.iter().find(|i| i.id == id).cloned().expect(id);
                assert_eq!((item(&own).can_edit, item(&own).can_delete), (None, None));
                let nums: Vec<u64> = version
                    .split(|c: char| !c.is_ascii_digit())
                    .take(3)
                    .map(|p| p.parse().unwrap_or(0))
                    .collect();
                let locked = driver == "mysql" && (nums[0], nums[1], nums[2]) >= (8, 0, 16);
                let form = l.scheduler_form("job", Some(&once)).await.expect("form");
                let mut v = values_of(&form);
                set(&mut v, "definer", json!(account));
                set(&mut v, "comment", json!("taken over"));
                if locked {
                    let root = item(&once);
                    assert_eq!((root.can_edit, root.can_delete), (Some(false), Some(false)));
                    assert!(items.notes.iter().any(|n| n.contains("SYSTEM_USER")), "{items:?}");
                    assert!(form.read_only && form.notes.iter().any(|n| n.contains("SYSTEM_USER")), "{:?}", form.notes);
                    let m = query_err(&mut l, "job", Some(&once), &v).await;
                    assert!(m.contains("no le deja cambiar ni borrar"), "{m}");
                    assert!(l.scheduler_delete_script("job", &once).await.is_err());
                    // As the server has it: neither taken over nor dropped.
                    for sql in [
                        format!("ALTER DEFINER = CURRENT_USER EVENT {DB}.{ONCE} COMMENT 'x'"),
                        format!("DROP EVENT {DB}.{ONCE}"),
                    ] {
                        let e = run(&mut l, &sql).await.unwrap_err();
                        assert!(e.to_string().contains("SYSTEM_USER"), "{sql}: {e}");
                    }
                } else {
                    let root = item(&once);
                    assert_eq!((root.can_edit, root.can_delete), (None, None));
                    assert!(!form.read_only, "{:?}", form.notes);
                    save(&mut l, "job", Some(&once), &v).await;
                    let r = one_row(&mut s, &event_row_sql(DB, ONCE)).await;
                    assert_eq!((r[6].as_str(), r[10].as_str()), ("taken over", account.as_str()));
                }
                // Another plain account's event stays open on every server
                // (MySQL 8.0.16+ included: its definer lacks SYSTEM_USER): the
                // login takes it over by naming itself definer.
                let others = format!("`{DB}`.`{OTHERS}`");
                let other = item(&others);
                assert_eq!((other.can_edit, other.can_delete), (None, None), "{other:?}");
                let form = l.scheduler_form("job", Some(&others)).await.expect("form");
                assert!(!form.read_only, "{:?}", form.notes);
                assert!(
                    val(&form, "definer").as_str().is_some_and(|d| d.starts_with(&format!("{OTHER}@"))),
                    "{:?}",
                    form.values
                );
                assert!(l.scheduler_delete_script("job", &others).await.is_ok());
                let mut v = values_of(&form);
                set(&mut v, "definer", json!(account));
                // The new definer's privileges, and no way back for a login
                // that can't name another account.
                let sc = script(&mut l, "job", Some(&others), &v).await;
                assert!(
                    sc.contains(&format!("\n-- ⚠ El definidor del evento pasa de «{OTHER}@"))
                        && sc.contains(&format!("» a «{account}»: desde ahora el evento corre con los privilegios de esa cuenta."))
                        && sc.contains("\n-- ⚠ Este usuario no puede nombrar a otra cuenta como definidor"),
                    "{sc}"
                );
                save(&mut l, "job", Some(&others), &v).await;
                let r = one_row(&mut s, &event_row_sql(DB, OTHERS)).await;
                assert_eq!(r[10], account);
                drop(l);
                exec(&mut s, &format!("DROP EVENT {DB}.{OWN}")).await;
                exec(&mut s, &format!("DROP EVENT {DB}.{OTHERS}")).await;
                exec(&mut s, &format!("DROP USER '{LOGIN}'@'%'")).await;
                exec(&mut s, &format!("DROP USER '{OTHER}'@'%'")).await;
            }

            // The scheduler's log (MySQL 8.0.22+): a failing event's error.
            if info.logs && on {
                let form = s.scheduler_form("job", None).await.expect("form");
                let mut v = values_of(&form);
                set(&mut v, "schema", json!(DB));
                set(&mut v, "name", json!(FAILS));
                set(&mut v, "every", json!("1"));
                set(&mut v, "unit", json!("SECOND"));
                set(&mut v, "body", json!("INSERT INTO dbine_test_sched_missing VALUES (1)"));
                let fails = save(&mut s, "job", None, &v).await;
                let until = Instant::now() + Duration::from_secs(20);
                let event = format!("{DB}.{FAILS}");
                loop {
                    let logs = s.scheduler_logs().await.expect("logs");
                    assert_eq!(logs[0].columns, ["Fecha (UTC)", "Tipo", "Código", "Evento", "Mensaje"]);
                    if let Some(row) = logs[0].rows.iter().find(|r| r[3] == json!(event)) {
                        assert_eq!(row[1], json!("Error"));
                        assert!(row[4].as_str().unwrap().contains("dbine_test_sched_missing"), "{row:?}");
                        assert!(row[0].as_str().unwrap().len() == 19, "{row:?}");
                        break;
                    }
                    assert!(Instant::now() < until, "the failing event's error is in the log");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                s.scheduler_delete("job", &fails).await.expect("delete");
            } else if !info.logs {
                assert!(s.scheduler_logs().await.is_err());
            }

            // Foreign ids, then the delete.
            refuses_foreign_ids(&mut s, &["job"], &id).await;
            let sc = s.scheduler_delete_script("job", &once).await.expect("delete script");
            assert!(sc.contains(&format!("DROP EVENT `{DB}`.`{ONCE}`")), "{sc}");
            s.scheduler_delete("job", &once).await.expect("delete");
            s.scheduler_delete("job", &id).await.expect("delete");
            let n = one_row(
                &mut s,
                "SELECT COUNT(*) FROM information_schema.EVENTS WHERE EVENT_NAME LIKE 'dbine\\_test\\_sched\\_%'",
            )
            .await;
            assert_eq!(n[0], "0");
            assert!(s.scheduler_form("job", Some(&id)).await.is_err(), "gone");
        },
        async |s| drop_events(s).await,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql_events() {
    events("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb_events() {
    events("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn oceanbase_events() {
    events("oceanbase", "DBINE_TEST_OCEANBASE_URL").await;
}

// ---- Doris: jobs and the refresh of materialized views ----

const JOB: &str = "dbine_test_sched_job";
const JOB2: &str = "dbine_test_sched_job2";
const ATJOB: &str = "dbine_test_sched_at";
const PASTJOB: &str = "dbine_test_sched_past";
const MV: &str = "dbine_test_sched_mv";
/// Another account's job, created again by the test's login.
const OWNED: &str = "dbine_test_sched_owned";
const OWNER: &str = "dbine_test_sched_owner";

async fn drop_doris(s: &mut Box<dyn Session>) {
    for j in [JOB, JOB2, ATJOB, PASTJOB, OWNED] {
        let _ = run(s, &format!("DROP JOB WHERE jobname = '{j}'")).await;
    }
    let _ = run(s, &format!("DROP USER IF EXISTS '{OWNER}'@'%'")).await;
    let _ = run(s, &format!("DROP DATABASE IF EXISTS {DB} FORCE")).await;
    let left = rows(s, "SHOW ALL GRANTS").await;
    assert!(
        !left.iter().any(|r| r.iter().any(|c| c.contains(OWNER))),
        "{OWNER} left behind"
    );
    let left = rows(
        s,
        "SELECT Name FROM jobs('type'='insert') WHERE Name LIKE 'dbine_test_sched%'",
    )
    .await;
    assert!(left.is_empty(), "jobs left behind: {left:?}");
    let left = rows(
        s,
        "SELECT MvName FROM jobs('type'='mv') WHERE MvName LIKE 'dbine_test_sched%'",
    )
    .await;
    assert!(left.is_empty(), "views left behind: {left:?}");
}

async fn doris_job(s: &mut Box<dyn Session>, name: &str) -> Vec<String> {
    one_row(
        s,
        &format!("SELECT ExecuteType, RecurringStrategy, Status, ExecuteSql, Comment FROM jobs('type'='insert') WHERE Name = '{name}'"),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn doris_jobs_and_views() {
    let driver = "doris";
    let Some(cfg) = cfg(driver, "DBINE_TEST_DORIS_URL") else {
        eprintln!("DBINE_TEST_DORIS_URL not set; skipping");
        return;
    };
    let c2 = cfg.clone();
    guarded(
        driver,
        &cfg,
        async move {
            let cfg = c2;
            let mut s = connect(driver, &cfg).await;
            drop_doris(&mut s).await;
            exec(&mut s, &format!("CREATE DATABASE {DB}")).await;
            for t in ["src", "dst"] {
                exec(
                    &mut s,
                    &format!("CREATE TABLE {DB}.{t} (k INT, v INT) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k) BUCKETS 1 PROPERTIES ('replication_num'='1')"),
                )
                .await;
            }
            exec(&mut s, &format!("USE {DB}")).await;

            let info = s.scheduler_info().await.expect("info");
            eprintln!("{info:?}");
            let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
            assert_eq!(keys, ["job", "mv_refresh"]);
            assert!(info.kinds[0].can_create && info.kinds[0].can_edit && info.kinds[0].can_delete);
            assert!(!info.kinds[1].can_create && info.kinds[1].can_edit && !info.kinds[1].can_delete);
            // The views' refresh, now; no run now for a job, no steps, no
            // history to clear.
            for k in &info.kinds {
                let actions: Vec<&str> = k.actions.iter().map(|a| a.key.as_str()).collect();
                assert_eq!(actions, ["refresh", "refresh_complete"], "{}", k.key);
                assert!(!k.run_from_step && !k.clear_history, "{}", k.key);
            }

            // A recurring job, from 2030 to 2031, with a comment.
            let form = s.scheduler_form("job", None).await.expect("form");
            assert_eq!(val(&form, "database"), json!(DB));
            let mut v = values_of(&form);
            set(&mut v, "name", json!(JOB));
            set(&mut v, "type", json!("every"));
            set(&mut v, "every", json!(2));
            set(&mut v, "unit", json!("HOUR"));
            set(&mut v, "starts", json!("2030-01-01T00:00:00"));
            set(&mut v, "ends", json!("2031-01-01T00:00:00"));
            set(&mut v, "comment", json!("it's DBine's \\ job"));
            set(&mut v, "sql", json!("INSERT INTO dst SELECT * FROM src;"));
            let id = save(&mut s, "job", None, &v).await;
            assert_eq!(id, format!("job:`{JOB}`"));
            assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));
            let r = doris_job(&mut s, JOB).await;
            assert_eq!(r[0], "RECURRING");
            assert_eq!(r[1], "EVERY 2 HOUR STARTS 2030-01-01 00:00:00 ENDS 2031-01-01 00:00:00");
            assert_eq!(r[2], "RUNNING");
            assert_eq!(r[3], "INSERT INTO dst SELECT * FROM src");
            assert_eq!(r[4], "it's DBine's \\ job");
            // A job offers no action: Doris runs it only on its schedule.
            let items = s.scheduler_items("job").await.expect("items");
            let it = items.items.iter().find(|i| i.id == id).expect("listed");
            assert_eq!((it.can_edit, it.can_delete), (None, None));
            assert_eq!(it.actions, Some(Vec::new()));
            match s.scheduler_action_script("job", &id, "run").await {
                Err(Error::Unsupported(m)) => assert!(m.contains("a pedido"), "{m}"),
                other => panic!("a Doris job has no run now: {other:?}"),
            }
            match s.scheduler_action_script("job", &id, "refresh").await {
                Err(Error::Query(m)) => assert!(m.contains("no es una acción"), "{m}"),
                other => panic!("a job isn't refreshed: {other:?}"),
            }
            assert!(s.scheduler_action("job", &id, "refresh").await.is_err());

            // Read back; unchanged, nothing to do.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            assert_eq!(val(&form, "every"), json!(2));
            assert_eq!(val(&form, "starts"), json!("2030-01-01T00:00:00"));
            assert_eq!(val(&form, "comment"), json!("it's DBine's \\ job"));
            assert!(form.notes.iter().any(|n| n.contains("lo borra y lo vuelve a crear")));
            let sc = script(&mut s, "job", Some(&id), &values_of(&form)).await;
            assert!(sc.contains("no hay cambios"), "{sc}");

            // Disabled alone: PAUSE, in place.
            let mut v = values_of(&form);
            set(&mut v, "enabled", json!(false));
            let sc = script(&mut s, "job", Some(&id), &v).await;
            assert!(sc.contains("PAUSE JOB WHERE jobname = 'dbine_test_sched_job'") && !sc.contains("DROP"), "{sc}");
            save(&mut s, "job", Some(&id), &v).await;
            assert_eq!(doris_job(&mut s, JOB).await[2], "PAUSED");

            // The schedule and the comment: dropped and created again, in
            // the database the form names (its statement names bare
            // tables), still paused.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "every", json!(1));
            set(&mut v, "unit", json!("DAY"));
            set(&mut v, "comment", json!("recreated"));
            set(&mut v, "database", json!(""));
            let m = query_err(&mut s, "job", Some(&id), &v).await;
            assert!(m.contains("elegí «Base de datos»"), "{m}");
            set(&mut v, "database", json!(DB));
            let sc = script(&mut s, "job", Some(&id), &v).await;
            assert!(sc.contains("\n-- ⚠ Doris no cambia un job programado: guardar un cambio lo borra y lo vuelve a crear"), "{sc}");
            // Still paused: paused after it's created, which is said.
            assert!(sc.contains("\n-- ⚠ Doris no crea un job pausado: el script lo pausa después de crearlo"), "{sc}");
            // Root's job saved by root: its owner stays.
            assert!(!sc.contains("El job pasa de"), "{sc}");
            assert!(sc.contains("DROP JOB WHERE jobname = 'dbine_test_sched_job'") && sc.contains("CREATE JOB `dbine_test_sched_job`"), "{sc}");
            exec(&mut s, "USE information_schema").await;
            save(&mut s, "job", Some(&id), &v).await;
            // The script's USE doesn't stay: the session is back.
            assert_eq!(one_row(&mut s, "SELECT DATABASE()").await[0], "information_schema");
            let r = doris_job(&mut s, JOB).await;
            assert_eq!(r[1], "EVERY 1 DAY STARTS 2030-01-01 00:00:00 ENDS 2031-01-01 00:00:00");
            assert_eq!(r[2], "PAUSED");
            assert_eq!(r[4], "recreated");

            // A create that fails puts the old job back.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "database", json!(DB));
            set(&mut v, "sql", json!("INSERT INTO dbine_test_sched_missing SELECT * FROM src"));
            let e = s.scheduler_save("job", Some(&id), &v).await.unwrap_err();
            assert!(e.to_string().contains("dejó el objeto como estaba"), "{e:?}");
            let r = doris_job(&mut s, JOB).await;
            assert_eq!(r[3], "INSERT INTO dst SELECT * FROM src");
            assert_eq!(r[2], "PAUSED");
            // One statement: a second one would run when saving.
            set(&mut v, "sql", json!("INSERT INTO dst SELECT * FROM src; DELETE FROM dst WHERE k > 0"));
            let m = query_err(&mut s, "job", Some(&id), &v).await;
            assert!(m.contains("tiene que ser una sola"), "{m}");

            // Renamed and enabled again.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "database", json!(DB));
            set(&mut v, "name", json!(JOB2));
            set(&mut v, "enabled", json!(true));
            let id2 = save(&mut s, "job", Some(&id), &v).await;
            assert_eq!(id2, format!("job:`{JOB2}`"));
            assert_eq!(doris_job(&mut s, JOB2).await[2], "RUNNING");
            assert!(rows(&mut s, &format!("SELECT Name FROM jobs('type'='insert') WHERE Name = '{JOB}'")).await.is_empty());

            // Once, at a time.
            let form = s.scheduler_form("job", None).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "name", json!(ATJOB));
            set(&mut v, "database", json!(DB));
            set(&mut v, "type", json!("at"));
            set(&mut v, "at", json!("2030-06-01T12:00:00"));
            set(&mut v, "sql", json!(format!("INSERT INTO {DB}.dst SELECT * FROM {DB}.src")));
            let at = save(&mut s, "job", None, &v).await;
            let r = doris_job(&mut s, ATJOB).await;
            assert_eq!(&r[..2], ["ONE_TIME", "AT 2030-06-01 12:00:00"]);
            // Doris has the form's newer parts: streaming jobs, properties.
            // The properties tab and the progress show for a streaming job
            // only (Doris neither keeps nor applies a scheduled job's).
            let form = s.scheduler_form("job", Some(&at)).await.expect("form");
            assert!(form.sections.iter().any(|s| s.lists.iter().any(|l| l.key == "properties")));
            assert!(!form.notes.iter().any(|n| n.contains("no tiene jobs continuos")), "{:?}", form.notes);
            let streaming_only = Some(("type".to_string(), vec!["streaming".to_string()]));
            for key in ["properties", "progress"] {
                let sec = form.sections.iter().find(|s| s.key == key).expect(key);
                assert_eq!(sec.when, streaming_only, "{key}");
                assert!(sec.lists.iter().all(|l| l.when == streaming_only), "{key}");
            }
            assert_eq!(val(&form, "type"), json!("at"));

            // A one-time job at a past time: refused before Doris refuses it.
            let mut pv = v.clone();
            set(&mut pv, "name", json!(PASTJOB));
            set(&mut pv, "at", json!("2020-01-01T00:00:00"));
            let m = query_err(&mut s, "job", None, &pv).await;
            assert!(m.contains("ya pasó"), "{m}");
            // One whose time passes can't be created again: read-only,
            // and a change that would drop it is refused.
            let soon = one_row(
                &mut s,
                "SELECT DATE_FORMAT(DATE_ADD(NOW(), INTERVAL 4 SECOND), '%Y-%m-%d %H:%i:%s')",
            )
            .await[0]
                .clone();
            set(&mut pv, "at", json!(soon));
            let past = save(&mut s, "job", None, &pv).await;
            tokio::time::sleep(Duration::from_secs(6)).await;
            let form = s.scheduler_form("job", Some(&past)).await.expect("form");
            assert!(form.notes.iter().any(|n| n.contains("ya pasó su hora")), "{:?}", form.notes);
            let ro = |k: &str| {
                form.sections
                    .iter()
                    .flat_map(|s| s.fields.iter())
                    .find(|f| f.key == k)
                    .unwrap()
                    .read_only
            };
            assert!(ro("name") && ro("comment") && ro("at") && ro("sql"));
            let mut v2 = values_of(&form);
            set(&mut v2, "comment", json!("again"));
            let m = query_err(&mut s, "job", Some(&past), &v2).await;
            assert!(m.contains("ya pasó su hora"), "{m}");
            assert!(!rows(&mut s, &format!("SELECT Name FROM jobs('type'='insert') WHERE Name = '{PASTJOB}'")).await.is_empty(), "still there");
            // Once it ran, its row says there's nothing to change.
            if doris_job(&mut s, PASTJOB).await[2] == "FINISHED" {
                let items = s.scheduler_items("job").await.expect("items");
                let it = items.items.iter().find(|i| i.id == past).expect("listed");
                assert_eq!((it.can_edit, it.can_delete), (Some(false), None));
            }
            s.scheduler_delete("job", &past).await.expect("delete");

            // A streaming job's script (its S3 source isn't reachable here).
            let mut sv = v.clone();
            set(&mut sv, "type", json!("streaming"));
            set(&mut sv, "sql", json!("INSERT INTO dst SELECT * FROM S3('uri'='s3://b/p/*.csv', 'format'='csv')"));
            props(&mut sv, &[("max_interval", "10")]);
            let sc = script(&mut s, "job", None, &sv).await;
            assert!(sc.contains("PROPERTIES ('max_interval' = '10')\nON STREAMING\nDO INSERT INTO dst"), "{sc}");
            // Only INSERTs.
            set(&mut sv, "sql", json!("DELETE FROM dst"));
            let m = query_err(&mut s, "job", None, &sv).await;
            assert!(m.contains("INSERT"), "{m}");

            // A materialized view's refresh.
            exec(
                &mut s,
                &format!("CREATE MATERIALIZED VIEW {DB}.{MV} BUILD DEFERRED REFRESH AUTO ON MANUAL DISTRIBUTED BY RANDOM BUCKETS 1 PROPERTIES ('replication_num'='1') AS SELECT k, SUM(v) AS s FROM {DB}.src GROUP BY k"),
            )
            .await;
            let items = s.scheduler_items("mv_refresh").await.expect("items");
            let mv_id = items
                .items
                .iter()
                .find(|i| i.name == MV)
                .unwrap_or_else(|| panic!("{MV} listed: {items:?}"))
                .id
                .clone();
            assert!(mv_id.starts_with("mv:`"), "{mv_id}");
            // Under "job" the same id opens the same form.
            let via_job = s.scheduler_form("job", Some(&mv_id)).await.expect("form");
            let form = s.scheduler_form("mv_refresh", Some(&mv_id)).await.expect("form");
            assert_eq!(via_job.values, form.values);
            assert_eq!(val(&form, "trigger"), json!("MANUAL"));
            assert_eq!(val(&form, "method"), json!("AUTO"));
            let mut v = values_of(&form);
            set(&mut v, "method", json!("COMPLETE"));
            set(&mut v, "trigger", json!("SCHEDULE"));
            set(&mut v, "every", json!(1));
            set(&mut v, "unit", json!("DAY"));
            set(&mut v, "starts", json!("2030-01-01T00:00:00"));
            set(&mut v, "enabled", json!(false));
            props(&mut v, &[("grace_period", "10"), ("excluded_trigger_tables", "src")]);
            // A new method and trigger: Doris creates the view's job again,
            // running, so the script pauses it after; both are warned of.
            let sc = script(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            assert!(sc.contains("\n-- ⚠ Doris vuelve a crear el job de refresco de la vista") && sc.contains("\n-- ⚠ El script pausa el job después"), "{sc}");
            assert!(sc.trim_end().ends_with(&format!("PAUSE MATERIALIZED VIEW JOB ON `{DB}`.`{MV}`;")), "{sc}");
            save(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            let r = one_row(
                &mut s,
                &format!("SELECT RefreshInfo, MvProperties FROM mv_infos('database'='{DB}') WHERE Name = '{MV}'"),
            )
            .await;
            assert_eq!(r[0], "BUILD DEFERRED REFRESH COMPLETE ON SCHEDULE EVERY 1 DAY STARTS \"2030-01-01 00:00:00\"");
            assert!(r[1].contains("grace_period=10") && r[1].contains("excluded_trigger_tables=src"), "{r:?}");
            let st = one_row(&mut s, &format!("SELECT Status FROM jobs('type'='mv') WHERE MvName = '{MV}'")).await;
            assert_eq!(st[0], "PAUSED");
            let form = s.scheduler_form("mv_refresh", Some(&mv_id)).await.expect("form");
            assert_eq!(val(&form, "starts"), json!("2030-01-01T00:00:00"));
            let sc = script(&mut s, "mv_refresh", Some(&mv_id), &values_of(&form)).await;
            assert!(sc.contains("no hay cambios"), "{sc}");
            let mut v = values_of(&form);
            props(&mut v, &[("grace_period", "10")]);
            let m = query_err(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            assert!(m.contains("no quita una propiedad"), "{m}");
            set(&mut v, "trigger", json!("COMMIT"));
            props(&mut v, &[("grace_period", "20"), ("excluded_trigger_tables", "src")]);
            set(&mut v, "enabled", json!(true));
            // Paused, then enabled with a new trigger: the job Doris creates
            // again already runs (no RESUME after it).
            let mv_job = format!("SELECT Status, CreateTime FROM jobs('type'='mv') WHERE MvName = '{MV}'");
            let before = one_row(&mut s, &mv_job).await;
            std::thread::sleep(std::time::Duration::from_millis(1100));
            let sc = script(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            assert!(!sc.contains("RESUME") && !sc.contains("El script pausa") && sc.contains("-- ⚠ Doris vuelve a crear"), "{sc}");
            save(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            let r = one_row(
                &mut s,
                &format!("SELECT RefreshInfo, MvProperties FROM mv_infos('database'='{DB}') WHERE Name = '{MV}'"),
            )
            .await;
            assert_eq!(r[0], "BUILD DEFERRED REFRESH COMPLETE ON COMMIT");
            assert!(r[1].contains("grace_period=20"), "{r:?}");
            let after = one_row(&mut s, &mv_job).await;
            assert_eq!(after[0], "RUNNING", "{before:?} -> {after:?}");
            assert_ne!(after[1], before[1], "the job is created again");
            // Paused (in place: no warning), then a new trigger while it
            // stays paused: paused again after Doris creates it running.
            let form = s.scheduler_form("mv_refresh", Some(&mv_id)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "enabled", json!(false));
            let sc = script(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            assert!(!sc.contains('⚠') && !sc.contains(" REFRESH "), "{sc}");
            save(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            assert_eq!(one_row(&mut s, &mv_job).await[0], "PAUSED");
            let form = s.scheduler_form("mv_refresh", Some(&mv_id)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "trigger", json!("MANUAL"));
            let sc = script(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            assert!(sc.contains("-- ⚠ El script pausa el job después"), "{sc}");
            save(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            let r = one_row(
                &mut s,
                &format!("SELECT RefreshInfo FROM mv_infos('database'='{DB}') WHERE Name = '{MV}'"),
            )
            .await;
            assert_eq!(r[0], "BUILD DEFERRED REFRESH COMPLETE ON MANUAL");
            assert_eq!(one_row(&mut s, &mv_job).await[0], "PAUSED");
            // Running again for the checks below.
            let form = s.scheduler_form("mv_refresh", Some(&mv_id)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "trigger", json!("COMMIT"));
            set(&mut v, "enabled", json!(true));
            save(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            assert_eq!(one_row(&mut s, &mv_job).await[0], "RUNNING");
            assert!(s.scheduler_form("mv_refresh", None).await.is_err());
            assert!(s.scheduler_delete("mv_refresh", &mv_id).await.is_err());

            // The view's row, under both kinds: edited and refreshed, never
            // deleted here (Doris' own job, made and dropped with the view).
            for kind in ["mv_refresh", "job"] {
                let items = s.scheduler_items(kind).await.expect("items");
                let it = items.items.iter().find(|i| i.id == mv_id).expect("listed");
                assert_eq!((it.can_edit, it.can_delete), (Some(true), Some(false)), "{kind}");
                assert_eq!(
                    it.actions,
                    Some(vec!["refresh".to_string(), "refresh_complete".to_string()]),
                    "{kind}"
                );
            }
            // Refreshed now: what changed (AUTO), then all of it (COMPLETE),
            // each a task of the view's job.
            let job_name = val(&form, "job").as_str().expect("the view's job").to_string();
            let count = format!("SELECT COUNT(*) FROM tasks('type'='mv') WHERE JobName = '{job_name}'");
            let going = format!("SELECT TaskId FROM tasks('type'='mv') WHERE JobName = '{job_name}' AND Status IN ('PENDING', 'RUNNING')");
            for (action, mode) in [("refresh", "AUTO"), ("refresh_complete", "COMPLETE")] {
                let sc = s
                    .scheduler_action_script("job", &mv_id, action)
                    .await
                    .expect("action script");
                eprintln!("--- {action}\n{sc}");
                assert!(sc.contains(&format!("REFRESH MATERIALIZED VIEW `{DB}`.`{MV}` {mode};")), "{sc}");
                let before: u64 = one_row(&mut s, &count).await[0].parse().unwrap();
                s.scheduler_action("mv_refresh", &mv_id, action).await.expect(action);
                let until = Instant::now() + Duration::from_secs(20);
                while one_row(&mut s, &count).await[0].parse::<u64>().unwrap() <= before {
                    assert!(Instant::now() < until, "{action}: a task of the view's job");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                // Let it end before the next one.
                let until = Instant::now() + Duration::from_secs(60);
                while !rows(&mut s, &going).await.is_empty() {
                    assert!(Instant::now() < until, "{action}: the refresh ends");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            match s.scheduler_action_script("mv_refresh", &mv_id, "pause").await {
                Err(Error::Query(m)) => assert!(m.contains("no es una acción"), "{m}"),
                other => panic!("no pause on a view: {other:?}"),
            }
            assert!(s
                .scheduler_action_script("mv_refresh", &format!("{mv_id}; DROP DATABASE {DB}"), "refresh")
                .await
                .is_err());
            {
                let mut ro = ReadOnlySession::new(connect(driver, &cfg).await);
                assert!(ro.scheduler_action_script("mv_refresh", &mv_id, "refresh").await.is_ok());
                assert!(ro.scheduler_action("mv_refresh", &mv_id, "refresh").await.is_err());
            }

            // Another account's job: created again, it becomes the login's
            // and runs with its privileges, and the script says so.
            exec(&mut s, &format!("CREATE USER '{OWNER}'@'%' IDENTIFIED BY 'dbine_Pw_1'")).await;
            exec(&mut s, &format!("GRANT ADMIN_PRIV ON *.*.* TO '{OWNER}'@'%'")).await;
            {
                let mut oc = cfg.clone();
                oc.username = Some(OWNER.into());
                oc.password = Some("dbine_Pw_1".into());
                let mut o = connect(driver, &oc).await;
                exec(&mut o, &format!("USE {DB}")).await;
                exec(
                    &mut o,
                    &format!("CREATE JOB {OWNED} ON SCHEDULE EVERY 1 DAY STARTS '2030-01-01 00:00:00' DO INSERT INTO {DB}.dst SELECT * FROM {DB}.src"),
                )
                .await;
            }
            let owned = format!("job:`{OWNED}`");
            let form = s.scheduler_form("job", Some(&owned)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "comment", json!("taken over"));
            let sc = script(&mut s, "job", Some(&owned), &v).await;
            assert!(
                sc.contains(&format!("\n-- ⚠ El job pasa de «{OWNER}» a «root»: Doris lo vuelve a crear a nombre de quien guarda el cambio")),
                "{sc}"
            );
            save(&mut s, "job", Some(&owned), &v).await;
            let r = one_row(&mut s, &format!("SELECT Definer, Comment FROM jobs('type'='insert') WHERE Name = '{OWNED}'")).await;
            assert_eq!(r, ["root", "taken over"]);
            s.scheduler_delete("job", &owned).await.expect("delete");
            exec(&mut s, &format!("DROP USER '{OWNER}'@'%'")).await;

            refuses_foreign_ids(&mut s, &["job", "mv_refresh"], &id2).await;
            let sc = s.scheduler_delete_script("job", &at).await.expect("delete script");
            assert!(sc.contains("DROP JOB WHERE jobname = 'dbine_test_sched_at'") && sc.contains("\n-- ⚠ Doris borra con el job su historial de tareas"), "{sc}");
            s.scheduler_delete("job", &at).await.expect("delete");
            s.scheduler_delete("job", &id2).await.expect("delete");
            assert!(rows(&mut s, "SELECT Name FROM jobs('type'='insert') WHERE Name LIKE 'dbine_test_sched%'").await.is_empty());
        },
        async |s| drop_doris(s).await,
    )
    .await;
}

// ---- StarRocks: tasks and the refresh of materialized views ----

const TASK: &str = "dbine_test_sched_task";
const TASK2: &str = "dbine_test_sched_task2";
const TASK3: &str = "dbine_test_sched_task3";

async fn drop_starrocks(s: &mut Box<dyn Session>) {
    for t in [TASK, TASK2, TASK3] {
        let _ = run(s, &format!("DROP TASK IF EXISTS `{t}`")).await;
    }
    let _ = run(s, &format!("DROP DATABASE IF EXISTS {DB} FORCE")).await;
    let left = rows(
        s,
        "SELECT TASK_NAME FROM information_schema.tasks WHERE TASK_NAME LIKE 'dbine_test_sched%' OR `DATABASE` LIKE 'dbine_test_sched%'",
    )
    .await;
    assert!(left.is_empty(), "tasks left behind: {left:?}");
}

async fn sr_task(s: &mut Box<dyn Session>, name: &str) -> Vec<String> {
    one_row(
        s,
        &format!("SELECT SCHEDULE, `DATABASE`, DEFINITION, PROPERTIES FROM information_schema.tasks WHERE TASK_NAME = '{name}'"),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn starrocks_tasks_and_views() {
    let driver = "starrocks";
    let Some(cfg) = cfg(driver, "DBINE_TEST_STARROCKS_URL") else {
        eprintln!("DBINE_TEST_STARROCKS_URL not set; skipping");
        return;
    };
    let c2 = cfg.clone();
    guarded(
        driver,
        &cfg,
        async move {
            let cfg = c2;
            let mut s = connect(driver, &cfg).await;
            drop_starrocks(&mut s).await;
            exec(&mut s, &format!("CREATE DATABASE {DB}")).await;
            for t in ["src", "dst"] {
                exec(
                    &mut s,
                    &format!("CREATE TABLE {DB}.{t} (k INT, v INT) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k) BUCKETS 1 PROPERTIES ('replication_num'='1')"),
                )
                .await;
            }

            let info = s.scheduler_info().await.expect("info");
            eprintln!("{info:?}");
            let keys: Vec<&str> = info.kinds.iter().map(|k| k.key.as_str()).collect();
            assert_eq!(keys, ["job", "mv_refresh"]);
            assert_eq!(info.kinds[0].label, "Tareas");
            // A task's schedule suspended and resumed, the views' refresh;
            // no run now (StarRocks has none), no steps, no history to
            // clear.
            let actions = |i: usize| -> Vec<String> {
                info.kinds[i].actions.iter().map(|a| a.key.clone()).collect()
            };
            assert_eq!(actions(0), ["pause", "resume", "refresh", "refresh_complete"]);
            assert_eq!(actions(1), ["refresh", "refresh_complete"]);
            assert!(info.kinds.iter().all(|k| !k.run_from_step && !k.clear_history));

            // A periodic task from 2030, with a session variable.
            let form = s.scheduler_form("job", None).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "name", json!(TASK));
            set(&mut v, "database", json!(DB));
            set(&mut v, "type", json!("every"));
            set(&mut v, "starts", json!("2030-01-01T00:00:00"));
            set(&mut v, "every", json!(1));
            set(&mut v, "unit", json!("HOUR"));
            set(&mut v, "definition", json!("INSERT INTO dst SELECT * FROM src;"));
            props(&mut v, &[("query_timeout", "120")]);
            exec(&mut s, "USE information_schema").await;
            let id = save(&mut s, "job", None, &v).await;
            assert_eq!(id, format!("task:`{TASK}`"));
            // The script's USE doesn't stay: the session is back.
            assert_eq!(one_row(&mut s, "SELECT DATABASE()").await[0], "information_schema");
            assert!(s.jobs().await.unwrap().jobs.iter().any(|j| j.id == id));
            let r = sr_task(&mut s, TASK).await;
            assert_eq!(r[0], "PERIODICAL START(2030-01-01T00:00) EVERY(1 HOURS)");
            assert_eq!(r[1], DB);
            assert_eq!(r[2], "INSERT INTO dst SELECT * FROM src");
            assert!(r[3].contains("'query_timeout'='120'"), "{r:?}");
            // Its schedule suspended, then resumed, as actions.
            let items = s.scheduler_items("job").await.expect("items");
            let it = items.items.iter().find(|i| i.id == id).expect("listed");
            assert_eq!(it.actions, Some(vec!["pause".to_string(), "resume".to_string()]));
            assert_eq!((it.can_edit, it.can_delete, it.system), (None, None, false));
            for (action, verb) in [("pause", "SUSPEND"), ("resume", "RESUME")] {
                let sc = s.scheduler_action_script("job", &id, action).await.expect(action);
                eprintln!("--- {action}\n{sc}");
                assert!(sc.contains(&format!("ALTER TASK `{TASK}` {verb};")), "{sc}");
                s.scheduler_action("job", &id, action).await.expect(action);
            }
            match s.scheduler_action_script("job", &id, "run").await {
                Err(Error::Unsupported(m)) => assert!(m.contains("a pedido"), "{m}"),
                other => panic!("StarRocks has no run now: {other:?}"),
            }
            match s.scheduler_action_script("job", &id, "refresh").await {
                Err(Error::Query(m)) => assert!(m.contains("no es una acción"), "{m}"),
                other => panic!("a task isn't refreshed: {other:?}"),
            }
            {
                let mut ro = ReadOnlySession::new(connect(driver, &cfg).await);
                assert!(ro.scheduler_action_script("job", &id, "pause").await.is_ok());
                assert!(ro.scheduler_action("job", &id, "pause").await.is_err());
            }
            assert_eq!(sr_task(&mut s, TASK).await[0], "PERIODICAL START(2030-01-01T00:00) EVERY(1 HOURS)");

            // Read back; unchanged, nothing to do.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            assert_eq!(val(&form, "starts"), json!("2030-01-01T00:00:00"));
            assert_eq!(val(&form, "unit"), json!("HOUR"));
            let sc = script(&mut s, "job", Some(&id), &values_of(&form)).await;
            assert!(sc.contains("no hay cambios"), "{sc}");

            // Properties: in place; suspended.
            let mut v = values_of(&form);
            props(&mut v, &[("query_timeout", "60"), ("session.query_timeout", "70")]);
            set(&mut v, "state", json!("suspend"));
            let sc = script(&mut s, "job", Some(&id), &v).await;
            assert!(sc.contains("ALTER TASK `dbine_test_sched_task` SET ('query_timeout' = '60', 'session.query_timeout' = '70')"), "{sc}");
            assert!(sc.contains("ALTER TASK `dbine_test_sched_task` SUSPEND") && !sc.contains("DROP"), "{sc}");
            save(&mut s, "job", Some(&id), &v).await;
            let r = sr_task(&mut s, TASK).await;
            assert!(r[3].contains("'query_timeout'='60'") && r[3].contains("'session.query_timeout'='70'"), "{r:?}");

            // The schedule, and a property taken away: submitted again.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "every", json!(2));
            set(&mut v, "unit", json!("DAY"));
            props(&mut v, &[("query_timeout", "60")]);
            set(&mut v, "state", json!("resume"));
            let sc = script(&mut s, "job", Some(&id), &v).await;
            assert!(sc.contains("DROP TASK `dbine_test_sched_task`") && sc.contains("SUBMIT TASK"), "{sc}");
            assert!(sc.contains("\n-- ⚠ StarRocks no cambia la programación, la instrucción, la base ni el nombre de una tarea"), "{sc}");
            // Resumed: the state is set, no suspension to lose.
            assert!(sc.contains("ALTER TASK `dbine_test_sched_task` RESUME") && !sc.contains("StarRocks no informa si la tarea estaba suspendida") && !sc.contains("su suspensión"), "{sc}");
            // Root's task submitted again by root: its creator stays.
            assert!(!sc.contains("La tarea pasa de"), "{sc}");
            save(&mut s, "job", Some(&id), &v).await;
            let r = sr_task(&mut s, TASK).await;
            assert_eq!(r[0], "PERIODICAL START(2030-01-01T00:00) EVERY(2 DAYS)");
            assert!(!r[3].contains("session.query_timeout"), "{r:?}");

            // A submit that fails puts the old task back.
            let form = s.scheduler_form("job", Some(&id)).await.expect("form");
            let mut v = values_of(&form);
            set(&mut v, "definition", json!("INSERT INTO dbine_test_sched_missing SELECT * FROM src"));
            let e = s.scheduler_save("job", Some(&id), &v).await.unwrap_err();
            assert!(e.to_string().contains("dejó el objeto como estaba"), "{e:?}");
            assert_eq!(sr_task(&mut s, TASK).await[2], "INSERT INTO dst SELECT * FROM src");
            // One statement: a second one would run when saving.
            set(&mut v, "definition", json!("INSERT INTO dst SELECT * FROM src; TRUNCATE TABLE dst"));
            let m = query_err(&mut s, "job", Some(&id), &v).await;
            assert!(m.contains("tiene que ser una sola"), "{m}");

            // A task without a schedule runs when submitted; a change that
            // fails leaves it dropped rather than run it again.
            {
                exec(&mut s, &format!("INSERT INTO {DB}.src VALUES (1, 1)")).await;
                let form = s.scheduler_form("job", None).await.expect("form");
                let mut v = values_of(&form);
                set(&mut v, "name", json!(TASK3));
                set(&mut v, "database", json!(DB));
                set(&mut v, "type", json!("once"));
                set(&mut v, "definition", json!("INSERT INTO dst SELECT * FROM src"));
                let once = save(&mut s, "job", None, &v).await;
                let dst = format!("SELECT COUNT(*) FROM {DB}.dst");
                let until = Instant::now() + Duration::from_secs(60);
                while one_row(&mut s, &dst).await[0] != "1" {
                    assert!(Instant::now() < until, "the task ran once");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                // Without a schedule, nothing to suspend.
                let items = s.scheduler_items("job").await.expect("items");
                let it = items.items.iter().find(|i| i.id == once).expect("listed");
                assert_eq!(it.actions, Some(Vec::new()));
                let m = match s.scheduler_action_script("job", &once, "pause").await {
                    Err(Error::Query(m)) => m,
                    other => panic!("expected a refusal, got {other:?}"),
                };
                assert!(m.contains("sin programación"), "{m}");
                let form = s.scheduler_form("job", Some(&once)).await.expect("form");
                let mut v = values_of(&form);
                set(&mut v, "definition", json!("INSERT INTO dbine_test_sched_missing SELECT * FROM src"));
                let sc = script(&mut s, "job", Some(&once), &v).await;
                assert!(sc.contains("\n-- ⚠ Si la creación falla, la tarea anterior queda borrada") && !sc.contains("DBine vuelve a crear"), "{sc}");
                let e = s.scheduler_save("job", Some(&once), &v).await.unwrap_err();
                assert!(!e.to_string().contains("como estaba"), "{e:?}");
                assert!(rows(&mut s, &format!("SELECT TASK_NAME FROM information_schema.tasks WHERE TASK_NAME = '{TASK3}'")).await.is_empty());
                tokio::time::sleep(Duration::from_secs(3)).await;
                assert_eq!(one_row(&mut s, &dst).await[0], "1", "not run again");
                exec(&mut s, &format!("TRUNCATE TABLE {DB}.dst")).await;
            }

            // Renamed.
            let mut v = values_of(&form);
            set(&mut v, "name", json!(TASK2));
            let id2 = save(&mut s, "job", Some(&id), &v).await;
            assert_eq!(id2, format!("task:`{TASK2}`"));
            assert_eq!(sr_task(&mut s, TASK2).await[1], DB);
            assert!(rows(&mut s, &format!("SELECT TASK_NAME FROM information_schema.tasks WHERE TASK_NAME = '{TASK}'")).await.is_empty());
            // The warehouse isn't a task property.
            let form = s.scheduler_form("job", Some(&id2)).await.expect("form");
            let mut v = values_of(&form);
            props(&mut v, &[("warehouse", "x")]);
            let m = query_err(&mut s, "job", Some(&id2), &v).await;
            assert!(m.contains("warehouse"), "{m}");

            // A materialized view's refresh.
            exec(
                &mut s,
                &format!("CREATE MATERIALIZED VIEW {DB}.{MV} DISTRIBUTED BY HASH(k) BUCKETS 1 REFRESH MANUAL PROPERTIES ('replication_num'='1') AS SELECT k, SUM(v) AS s FROM {DB}.src GROUP BY k"),
            )
            .await;
            let items = s.scheduler_items("mv_refresh").await.expect("items");
            let mv_id = items
                .items
                .iter()
                .find(|i| i.name == MV)
                .unwrap_or_else(|| panic!("{MV} listed: {items:?}"))
                .id
                .clone();
            let via_job = s.scheduler_form("job", Some(&mv_id)).await.expect("form");
            let form = s.scheduler_form("mv_refresh", Some(&mv_id)).await.expect("form");
            assert_eq!(via_job.values, form.values);
            assert_eq!(val(&form, "trigger"), json!("MANUAL"));
            let mut v = values_of(&form);
            set(&mut v, "trigger", json!("SCHEDULE"));
            set(&mut v, "starts", json!("2030-01-01T00:00:00"));
            set(&mut v, "every", json!(1));
            set(&mut v, "unit", json!("DAY"));
            set(&mut v, "active", json!(false));
            props(&mut v, &[("session.query_timeout", "500"), ("mv_rewrite_staleness_second", "30")]);
            save(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            let r = one_row(
                &mut s,
                &format!("SELECT t.SCHEDULE, v.IS_ACTIVE FROM information_schema.materialized_views v JOIN information_schema.tasks t ON t.TASK_NAME = v.TASK_NAME WHERE v.TABLE_SCHEMA = '{DB}' AND v.TABLE_NAME = '{MV}'"),
            )
            .await;
            assert_eq!(r[0], "PERIODICAL START(2030-01-01T00:00) EVERY(1 DAYS)");
            assert_eq!(r[1], "false");
            let form = s.scheduler_form("mv_refresh", Some(&mv_id)).await.expect("form");
            assert_eq!(val(&form, "active"), json!(false));
            // Inactive: StarRocks doesn't refresh it.
            let items = s.scheduler_items("mv_refresh").await.expect("items");
            let it = items.items.iter().find(|i| i.id == mv_id).expect("listed");
            assert_eq!(it.actions, Some(Vec::new()));
            let m = match s.scheduler_action_script("mv_refresh", &mv_id, "refresh").await {
                Err(Error::Query(m)) => m,
                other => panic!("expected a refusal, got {other:?}"),
            };
            assert!(m.contains("inactiva"), "{m}");
            let pr = &form.sections[1].lists[0].rows;
            assert!(pr.iter().any(|r| r["key"] == json!("session.query_timeout") && r["value"] == json!("500")), "{pr:?}");
            let sc = script(&mut s, "mv_refresh", Some(&mv_id), &values_of(&form)).await;
            assert!(sc.contains("no hay cambios"), "{sc}");
            let mut v = values_of(&form);
            set(&mut v, "trigger", json!("ASYNC"));
            set(&mut v, "active", json!(true));
            save(&mut s, "mv_refresh", Some(&mv_id), &v).await;
            let r = one_row(
                &mut s,
                &format!("SELECT t.SCHEDULE, v.IS_ACTIVE FROM information_schema.materialized_views v JOIN information_schema.tasks t ON t.TASK_NAME = v.TASK_NAME WHERE v.TABLE_SCHEMA = '{DB}' AND v.TABLE_NAME = '{MV}'"),
            )
            .await;
            assert_eq!(r, ["EVENT_TRIGGERED", "true"]);
            assert!(s.scheduler_delete("mv_refresh", &mv_id).await.is_err());

            // Active, under both kinds: refreshed now, never deleted here.
            for kind in ["mv_refresh", "job"] {
                let items = s.scheduler_items(kind).await.expect("items");
                let it = items.items.iter().find(|i| i.id == mv_id).expect("listed");
                assert_eq!(it.can_delete, Some(false), "{kind}");
                assert_eq!(
                    it.actions,
                    Some(vec!["refresh".to_string(), "refresh_complete".to_string()]),
                    "{kind}"
                );
            }
            // What changed, then all of it (FORCE): each a run of its task.
            let task = val(&form, "task").as_str().expect("the view's task").to_string();
            let count = format!("SELECT COUNT(*) FROM information_schema.task_runs WHERE TASK_NAME = '{task}'");
            let going = format!("SELECT QUERY_ID FROM information_schema.task_runs WHERE TASK_NAME = '{task}' AND STATE IN ('PENDING', 'RUNNING')");
            for (action, force) in [("refresh", ""), ("refresh_complete", " FORCE")] {
                let sc = s
                    .scheduler_action_script("job", &mv_id, action)
                    .await
                    .expect("action script");
                eprintln!("--- {action}\n{sc}");
                assert!(sc.contains(&format!("REFRESH MATERIALIZED VIEW `{DB}`.`{MV}`{force} WITH ASYNC MODE;")), "{sc}");
                let before: u64 = one_row(&mut s, &count).await[0].parse().unwrap();
                s.scheduler_action("mv_refresh", &mv_id, action).await.expect(action);
                let until = Instant::now() + Duration::from_secs(30);
                while one_row(&mut s, &count).await[0].parse::<u64>().unwrap() <= before {
                    assert!(Instant::now() < until, "{action}: a run of the view's task");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                let until = Instant::now() + Duration::from_secs(60);
                while !rows(&mut s, &going).await.is_empty() {
                    assert!(Instant::now() < until, "{action}: the refresh ends");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            match s.scheduler_action_script("mv_refresh", &mv_id, "pause").await {
                Err(Error::Query(m)) => assert!(m.contains("no es una acción"), "{m}"),
                other => panic!("no pause on a view: {other:?}"),
            }

            refuses_foreign_ids(&mut s, &["job", "mv_refresh"], &id2).await;
            let sc = s.scheduler_delete_script("job", &id2).await.expect("delete script");
            assert!(sc.contains("DROP TASK `dbine_test_sched_task2`"), "{sc}");
            s.scheduler_delete("job", &id2).await.expect("delete");
            assert!(rows(&mut s, "SELECT TASK_NAME FROM information_schema.tasks WHERE TASK_NAME LIKE 'dbine_test_sched_task%'").await.is_empty());
        },
        async |s| drop_starrocks(s).await,
    )
    .await;
}
