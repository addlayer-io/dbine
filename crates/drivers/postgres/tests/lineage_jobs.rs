//! Jobs in the lineage (`Session::job_dependents`, `Session::job_references`)
//! against a PostgreSQL with pg_cron (`DBINE_TEST_PGCRON_URL`, the container
//! described in tests/jobs.rs). Skipped without it:
//!
//! ```sh
//! DBINE_TEST_PGCRON_URL=postgres://postgres:pw@localhost:25040/postgres \
//!   cargo test -p dbine-driver-postgres --test lineage_jobs -- --ignored
//! ```
//!
//! The procedure, the database and the jobs it makes start with
//! `dbine_test_lj` and are dropped at the end, whether the test passed or
//! not.

use dbine_driver::{
    ConnectionConfig, DependencyScan, DependencyTarget, ObjectRef, QueryOutcome, Session,
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;

fn cfg() -> Option<ConnectionConfig> {
    let Ok(url) = std::env::var("DBINE_TEST_PGCRON_URL") else {
        eprintln!("DBINE_TEST_PGCRON_URL not set; skipping");
        return None;
    };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (hostport, db) = hostpart.split_once('/')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "postgres".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        options: [("sslmode".to_string(), "disable".to_string())].into(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let r = s.execute(sql, 100, &mut out).await;
    assert!(
        r.is_ok() && out.errors.is_empty(),
        "{sql}: {r:?} {:?}",
        out.errors
    );
}

async fn cleanup(s: &mut Box<dyn Session>) {
    let mut out = QueryOutcome::default();
    let _ = s
        .execute(
            "SELECT count(cron.unschedule(jobid)) FROM cron.job WHERE jobname LIKE 'dbine\\_test\\_lj\\_%';
             DROP PROCEDURE IF EXISTS public.dbine_test_lj_carga();",
            10,
            &mut out,
        )
        .await;
    let _ = s
        .execute("DROP DATABASE IF EXISTS dbine_test_lj_db", 10, &mut out)
        .await;
}

#[tokio::test]
#[ignore = "needs DBINE_TEST_PGCRON_URL"]
async fn pg_cron_jobs_that_call_a_procedure_and_what_they_use() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == "postgres")
        .unwrap();
    let mut s = d.connect(&c, None).await.expect("connect");
    cleanup(&mut s).await;
    let body = async {
        run(&mut s, "CREATE DATABASE dbine_test_lj_db").await;
        run(
            &mut s,
            "CREATE PROCEDURE public.dbine_test_lj_carga() LANGUAGE sql AS $$ SELECT 1 $$",
        )
        .await;
        run(&mut s, "SELECT cron.schedule('dbine_test_lj_cron', '0 3 * * *', 'CALL public.dbine_test_lj_carga()')").await;
        // The same call in another database: that database's procedure.
        run(
            &mut s,
            "SELECT cron.schedule_in_database('dbine_test_lj_other', '0 4 * * *', 'CALL public.dbine_test_lj_carga()', 'dbine_test_lj_db')",
        )
        .await;
        let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
        let target = DependencyTarget {
            object: ObjectRef {
                kind: "procedure".into(),
                schema: Some("public".into()),
                name: "dbine_test_lj_carga".into(),
            },
            column: None,
        };
        let r = s
            .job_dependents(&c.database, &target, &scan)
            .await
            .expect("job dependents");
        let ours: Vec<&str> = r
            .jobs
            .iter()
            .filter(|j| j.name.starts_with("dbine_test_lj_"))
            .map(|j| j.name.as_str())
            .collect();
        assert_eq!(ours, ["dbine_test_lj_cron"], "{:?}", r.jobs);
        let job = r
            .jobs
            .iter()
            .find(|j| j.name == "dbine_test_lj_cron")
            .unwrap();
        assert_eq!(job.kind, "pg_cron");
        assert_eq!(job.mentions[0].text, "CALL public.dbine_test_lj_carga()");

        let uses = s
            .job_references(&c.database, &job.id, &scan)
            .await
            .expect("job references");
        assert!(
            uses.items
                .iter()
                .any(|i| i.kind == "procedure" && i.name == "dbine_test_lj_carga"),
            "{:?}",
            uses.items
        );
    };
    let result = AssertUnwindSafe(body).catch_unwind().await;
    cleanup(&mut s).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
