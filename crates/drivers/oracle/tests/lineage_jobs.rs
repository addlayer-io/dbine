//! Jobs in the lineage (`Session::job_dependents`, `Session::job_references`)
//! against a real Oracle with `DBMS_SCHEDULER`: a job of type
//! `STORED_PROCEDURE` (the engine states what it runs: confirmed) and a
//! PL/SQL block that calls the procedure (read from its code: probable).
//! Skipped without `DBINE_TEST_ORACLE_ADMIN_URL`:
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test lineage_jobs -- --ignored
//! ```
//!
//! The procedure and the jobs start with `DBINE_TEST_LJ_` and are dropped at
//! the end, whether the test passed or not.

use dbine_driver::{
    Confidence, ConnectionConfig, DependencyScan, DependencyTarget, ObjectRef, QueryOutcome,
    Session,
};

fn config() -> Option<ConnectionConfig> {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return None;
    };
    let rest = url.strip_prefix("oracle://")?;
    let (cred, addr) = rest.rsplit_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, service) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    Some(cfg)
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 100, &mut out).await {
        panic!("{sql}\n→ {e}");
    }
    assert!(out.errors.is_empty(), "{sql}\n→ {:?}", out.errors);
}

const CLEANUP: &str = r#"BEGIN
  FOR j IN (SELECT owner, job_name FROM dba_scheduler_jobs
             WHERE job_name LIKE 'DBINE\_TEST\_LJ\_%' ESCAPE '\') LOOP
    DBMS_SCHEDULER.DROP_JOB('"' || j.owner || '"."' || j.job_name || '"', force => TRUE);
  END LOOP;
  FOR p IN (SELECT owner, object_name FROM dba_objects
             WHERE object_name LIKE 'DBINE\_TEST\_LJ\_%' ESCAPE '\' AND object_type = 'PROCEDURE') LOOP
    EXECUTE IMMEDIATE 'DROP PROCEDURE "' || p.owner || '"."' || p.object_name || '"';
  END LOOP;
END;"#;

#[tokio::test]
#[ignore = "needs DBINE_TEST_ORACLE_ADMIN_URL"]
async fn scheduler_jobs_that_run_a_procedure_and_what_they_use() {
    let Some(c) = config() else { return };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    let _ = admin.execute(CLEANUP, 10, &mut out).await;
    // In its own task: whatever fails in it, the cleanup below still runs.
    let result = tokio::spawn(scenario(c)).await;
    let mut out = QueryOutcome::default();
    let _ = admin.execute(CLEANUP, 10, &mut out).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn scenario(c: ConnectionConfig) {
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut s = d.connect(&c, None).await.expect("connect");
    exec(
        &mut s,
        "CREATE PROCEDURE SYSTEM.DBINE_TEST_LJ_CARGA AS BEGIN NULL; END;",
    )
    .await;
    exec(
        &mut s,
        "BEGIN
           DBMS_SCHEDULER.CREATE_JOB('SYSTEM.DBINE_TEST_LJ_PROC', job_type => 'STORED_PROCEDURE',
             job_action => 'SYSTEM.DBINE_TEST_LJ_CARGA', repeat_interval => 'FREQ=DAILY', enabled => FALSE);
           DBMS_SCHEDULER.CREATE_JOB('SYSTEM.DBINE_TEST_LJ_BLOCK', job_type => 'PLSQL_BLOCK',
             job_action => 'BEGIN
  DBINE_TEST_LJ_CARGA;
END;', repeat_interval => 'FREQ=DAILY', enabled => FALSE);
         END;",
    )
    .await;
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let target = DependencyTarget {
        object: ObjectRef {
            kind: "procedure".into(),
            schema: Some("SYSTEM".into()),
            name: "DBINE_TEST_LJ_CARGA".into(),
        },
        column: None,
    };
    let r = s
        .job_dependents("SYSTEM", &target, &scan)
        .await
        .expect("job dependents");
    let ours: Vec<(&str, Confidence)> = r
        .jobs
        .iter()
        .filter(|j| j.name.starts_with("DBINE_TEST_LJ_"))
        .map(|j| (j.name.as_str(), j.confidence))
        .collect();
    assert_eq!(
        ours,
        [
            ("DBINE_TEST_LJ_PROC", Confidence::Confirmed),
            ("DBINE_TEST_LJ_BLOCK", Confidence::Probable)
        ],
        "{:?}",
        r.jobs
    );
    let block = r
        .jobs
        .iter()
        .find(|j| j.name == "DBINE_TEST_LJ_BLOCK")
        .unwrap();
    assert_eq!(block.mentions[0].line, 2);
    assert_eq!(block.mentions[0].text, "DBINE_TEST_LJ_CARGA;");

    let proc_job = r
        .jobs
        .iter()
        .find(|j| j.name == "DBINE_TEST_LJ_PROC")
        .unwrap();
    let uses = s
        .job_references("SYSTEM", &proc_job.id, &scan)
        .await
        .expect("job references");
    let p = uses
        .items
        .iter()
        .find(|i| i.name == "DBINE_TEST_LJ_CARGA")
        .unwrap_or_else(|| panic!("{:?}", uses.items));
    assert_eq!(p.kind, "procedure");
    assert_eq!(p.confidence, Confidence::Confirmed);
}
