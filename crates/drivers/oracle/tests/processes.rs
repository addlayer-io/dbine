//! The process list and cancelling another session's statement, against a
//! real server. The user needs to read V$SESSION / V$SQL and ALTER SYSTEM,
//! so this takes the admin URL (SYSTEM on the test container), and CANCEL
//! SQL needs 18c+:
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::Duration;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL").ok()?;
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

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let o = run(s, sql).await.unwrap();
    let v = &o.results.last().unwrap().rows[0][0];
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

const SESSION_ID: &str =
    "SELECT SYS_CONTEXT('USERENV', 'SID') || ',' || (SELECT serial# FROM v$session WHERE sid = SYS_CONTEXT('USERENV', 'SID')) FROM dual";

/// A session sleeping in a SELECT shows up active with its text, the
/// lister's own row is flagged, and cancelling stops the statement while
/// the session stays connected.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_and_cancels() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let d = dbine_driver_oracle::drivers().remove(0);
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let own = scalar(&mut admin, SESSION_ID).await;

    let mut worker = d.connect(&cfg, None).await.unwrap();
    let worker_id = scalar(&mut worker, SESSION_ID).await;
    let sleeping = tokio::spawn(async move {
        let r = run(
            &mut worker,
            "WITH FUNCTION dbine_nap RETURN NUMBER IS BEGIN DBMS_SESSION.SLEEP(30); RETURN 1; END;
             SELECT dbine_nap() AS dbine_processes_test FROM dual",
        )
        .await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.id == worker_id).expect("the worker is listed");
    eprintln!("{w:#?}");
    assert!(w.active, "{w:?}");
    assert!(w.sql.as_deref().unwrap_or("").contains("dbine_processes_test"), "{w:?}");
    assert!(w.elapsed_ms.unwrap_or(0) >= 1000, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert!(!w.own && !w.system);
    assert!(list.iter().find(|p| p.id == own).expect("its own session is listed").own);
    assert!(list.iter().any(|p| p.system), "background processes are listed");

    // The worker is a DBine session: the server would stop the statement,
    // but this client library never returns from the break it gets for it,
    // so cancelling is refused (that tab would hang) and ending it works.
    let refused = admin.cancel_query(&worker_id).await.unwrap_err().to_string();
    assert!(refused.contains("es de DBine"), "{refused}");
    assert!(admin.processes().await.unwrap().iter().find(|p| p.id == worker_id).expect("still connected").active);
    admin.kill_session(&worker_id).await.unwrap();
    let (r, _) = tokio::time::timeout(Duration::from_secs(15), sleeping).await.expect("the worker was freed").unwrap();
    assert!(r.is_err());
    assert!(admin.cancel_query("1; DROP TABLE x").await.is_err(), "the id is validated");
    assert!(admin.cancel_query(&own).await.is_err(), "not its own session");
}
