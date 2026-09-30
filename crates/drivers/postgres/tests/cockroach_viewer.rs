//! CockroachDB's profiler, Monitor and permission check for users that
//! aren't `admin`: one with the VIEWACTIVITY system privilege (sees every
//! session) and one without grants (sees only its own). The test creates and
//! drops both users; it needs an insecure cluster (no passwords):
//!
//! ```sh
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//!   cargo test -p dbine-driver-postgres --test cockroach_viewer -- --ignored
//! ```

use dbine_driver::{Access, ConnectionConfig, Driver, ProfiledStatement, ProfilerOptions, QueryOutcome, Session};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn cfg(env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "cockroachdb".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == "cockroachdb").unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

/// Profiles as `user` while the admin runs a slow, marked statement.
async fn profile_as(d: &Arc<dyn Driver>, admin: &ConnectionConfig, user: &str) -> (Option<String>, Vec<ProfiledStatement>, String) {
    let cfg = ConnectionConfig { username: Some(user.into()), password: None, ..admin.clone() };
    let mut p = d.connect(&cfg, None).await.unwrap_or_else(|e| panic!("connect as {user}: {e}"));
    let mut w = d.connect(admin, None).await.unwrap();
    let started = p
        .profiler_start(&ProfilerOptions { database: admin.database.clone(), change_server: false })
        .await
        .unwrap_or_else(|e| panic!("{user}: profiler_start: {e}"));
    let marker = format!("dbine_viewer_{}_{user}", std::process::id());
    let slow = format!("SELECT pg_sleep(1.2), 1 AS {marker}");
    let work = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut out = QueryOutcome::default();
        w.execute(&slow, 10, &mut out).await.expect("slow");
    };
    let watch = async {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            got.extend(p.profiler_poll().await.unwrap_or_else(|e| panic!("{user}: profiler_poll: {e}")));
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.unwrap();
    (started.note, got, marker)
}

#[tokio::test]
#[ignore]
async fn non_admin_users() {
    let env = "DBINE_TEST_COCKROACH_URL";
    let Some(admin) = cfg(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver();
    let mut s = d.connect(&admin, None).await.unwrap();
    // The system grant must go before the user (the REVOKE fails when the
    // user doesn't exist yet: ignored).
    let cleanup = async |s: &mut Box<dyn Session>| {
        let mut out = QueryOutcome::default();
        let _ = s.execute("REVOKE SYSTEM VIEWACTIVITY FROM dbine_viewer_t", 10, &mut out).await;
        run(s, "DROP USER IF EXISTS dbine_viewer_t").await;
        run(s, "DROP USER IF EXISTS dbine_nogrant_t").await;
    };
    cleanup(&mut s).await;
    run(&mut s, "CREATE USER dbine_viewer_t").await;
    run(&mut s, "GRANT SYSTEM VIEWACTIVITY TO dbine_viewer_t").await;
    run(&mut s, "CREATE USER dbine_nogrant_t").await;

    // VIEWACTIVITY: sees the admin's statement, no warning.
    let (note, got, marker) = profile_as(&d, &admin, "dbine_viewer_t").await;
    eprintln!("viewer: note {note:?}");
    assert!(!note.as_deref().unwrap_or("").contains("VIEWACTIVITY"), "{note:?}");
    assert_eq!(got.iter().filter(|st| st.text.contains(&marker)).count(), 1, "viewer sees the admin's statement once");

    let viewer = ConnectionConfig { username: Some("dbine_viewer_t".into()), ..admin.clone() };
    let mut v = d.connect(&viewer, None).await.unwrap();
    let perms = v.permissions(Some(&admin.database)).await.unwrap();
    eprintln!("viewer: {perms:?}");
    assert_eq!(perms.profiler, Access::Allowed);
    let snap = v.monitor().await.expect("monitor as viewer");
    for n in &snap.notes {
        eprintln!("viewer: note {n}");
    }
    let sessions = snap.tables.iter().find(|t| t.key == "sessions").expect("sessions table");
    assert!(sessions.rows.len() >= 2, "viewer sees other sessions: {}", sessions.rows.len());
    assert!(snap.tables.iter().any(|t| t.key == "queries"), "queries table");
    assert!(!snap.notes.iter().any(|n| n.contains("sesiones")), "{:?}", snap.notes);
    drop(v);

    // No grants: only its own statements, and the note says why.
    let (note, got, marker) = profile_as(&d, &admin, "dbine_nogrant_t").await;
    eprintln!("nogrant: note {note:?}");
    assert!(note.as_deref().unwrap_or("").contains("GRANT SYSTEM VIEWACTIVITY"), "{note:?}");
    assert!(!got.iter().any(|st| st.text.contains(&marker)), "nogrant can't see the admin's statement");

    cleanup(&mut s).await;
}
