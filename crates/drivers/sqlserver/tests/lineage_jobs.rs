//! Jobs in the lineage (`Session::job_dependents`, `Session::job_references`)
//! against a SQL Server with the Agent running (`DBINE_TEST_SQLSERVER_AGENT_URL`,
//! a container started with `MSSQL_AGENT_ENABLED=true`). Skipped without it:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_AGENT_URL='mssql://sa:Pw_12345!@localhost:26131' \
//!   cargo test -p dbine-driver-sqlserver --test lineage_jobs -- --ignored --test-threads=1
//! ```
//!
//! The database `dbine_test_lj` and the jobs `dbine_test_lj_*` are made by
//! the test and dropped at the end, whether it passed or not.

use dbine_driver::{
    Confidence, ConnectionConfig, DependencyScan, DependencyTarget, Driver, ObjectRef,
    QueryOutcome, Session,
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

const DB: &str = "dbine_test_lj";

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

fn driver() -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == "sqlserver")
        .unwrap()
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

async fn cleanup(master: &mut Box<dyn Session>) {
    let mut out = QueryOutcome::default();
    let _ = master
        .execute(
            "DECLARE @n sysname;
             DECLARE c CURSOR LOCAL FOR SELECT name FROM msdb.dbo.sysjobs WHERE name LIKE N'dbine\\_test\\_lj\\_%' ESCAPE N'\\';
             OPEN c; FETCH NEXT FROM c INTO @n;
             WHILE @@FETCH_STATUS = 0 BEGIN EXEC msdb.dbo.sp_delete_job @job_name = @n; FETCH NEXT FROM c INTO @n; END;
             CLOSE c; DEALLOCATE c;
             IF DB_ID(N'dbine_test_lj') IS NOT NULL BEGIN
               ALTER DATABASE dbine_test_lj SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_test_lj;
             END",
            10,
            &mut out,
        )
        .await;
}

#[tokio::test]
#[ignore = "needs DBINE_TEST_SQLSERVER_AGENT_URL"]
async fn agent_jobs_that_run_a_procedure_and_what_they_use() {
    let Some(c) = cfg() else { return };
    let d = driver();
    let mut master = d.connect(&c, None).await.expect("connect");
    cleanup(&mut master).await;
    run(&mut master, "CREATE DATABASE dbine_test_lj").await;
    let body = async {
        let mut s = d
            .connect(
                &ConnectionConfig {
                    database: DB.into(),
                    ..c.clone()
                },
                None,
            )
            .await
            .expect("connect to the test database");
        for sql in [
            "CREATE TABLE dbo.ventas (id int)",
            "CREATE VIEW dbo.v_lj AS SELECT id FROM dbo.ventas",
            "CREATE PROCEDURE dbo.sp_lj_carga AS SELECT id FROM dbo.v_lj",
        ] {
            run(&mut s, sql).await;
        }
        run(
            &mut master,
            "EXEC msdb.dbo.sp_add_job @job_name = N'dbine_test_lj_job';
             EXEC msdb.dbo.sp_add_jobstep @job_name = N'dbine_test_lj_job', @step_name = N'Cargar ventas', @subsystem = N'TSQL',
                  @database_name = N'dbine_test_lj', @command = N'EXEC dbo.sp_lj_carga;', @on_success_action = 3;
             EXEC msdb.dbo.sp_add_jobstep @job_name = N'dbine_test_lj_job', @step_name = N'Desde master', @subsystem = N'TSQL',
                  @database_name = N'master', @command = N'SELECT 1;
EXEC dbine_test_lj.dbo.sp_lj_carga;';
             EXEC msdb.dbo.sp_add_jobserver @job_name = N'dbine_test_lj_job';
             EXEC msdb.dbo.sp_add_job @job_name = N'dbine_test_lj_other';
             EXEC msdb.dbo.sp_add_jobstep @job_name = N'dbine_test_lj_other', @step_name = N'Otra base', @subsystem = N'TSQL',
                  @database_name = N'master', @command = N'EXEC dbo.sp_lj_carga;';
             EXEC msdb.dbo.sp_add_jobserver @job_name = N'dbine_test_lj_other';",
        )
        .await;
        let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
        let target = DependencyTarget {
            object: ObjectRef {
                kind: "procedure".into(),
                schema: Some("dbo".into()),
                name: "sp_lj_carga".into(),
            },
            column: None,
        };
        let r = s
            .job_dependents(DB, &target, &scan)
            .await
            .expect("job dependents");
        let ours: Vec<_> = r
            .jobs
            .iter()
            .filter(|j| j.name.starts_with("dbine_test_lj_"))
            .collect();
        assert_eq!(ours.len(), 1, "{:?}", r.jobs);
        let job = ours[0];
        assert_eq!(job.name, "dbine_test_lj_job");
        assert_eq!(job.kind, "SQL Server Agent");
        assert_eq!(job.steps, ["Cargar ventas", "Desde master"]);
        assert_eq!(job.confidence, Confidence::Probable);
        assert!(
            job.mentions
                .iter()
                .any(|m| m.text == "EXEC dbo.sp_lj_carga;"),
            "{:?}",
            job.mentions
        );
        assert!(
            job.mentions
                .iter()
                .any(|m| m.line == 2 && m.text.contains("dbine_test_lj.dbo.sp_lj_carga")),
            "{:?}",
            job.mentions
        );

        // What the job uses: the procedure, from both steps.
        let uses = s
            .job_references(DB, &job.id, &scan)
            .await
            .expect("job references");
        let proc_ = uses
            .items
            .iter()
            .find(|i| i.name == "sp_lj_carga")
            .expect("the procedure");
        assert_eq!(proc_.kind, "procedure");
        assert_eq!(proc_.mentions.len(), 2);

        // Through the app's merge, next to the procedure's other dependents.
        let mut deps = s.dependents(&target, &scan).await.expect("dependents");
        dbine_driver::dependencies::merge_jobs(&mut deps, r);
        assert!(deps.jobs.iter().any(|j| j.name == "dbine_test_lj_job"));
        drop(s);
    };
    let result = AssertUnwindSafe(body).catch_unwind().await;
    cleanup(&mut master).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
