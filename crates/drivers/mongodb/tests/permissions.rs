//! `Session::permissions` against real servers: root, a `readWrite` user, a
//! `dbAdmin` user, DBine's read-only mode (not looked at: a server with
//! auth), a server without access control and FerretDB.
//!
//! ```sh
//! docker run -d --name dbine-test-mongodb-perm -p 27102:27017 \
//!   -e MONGO_INITDB_ROOT_USERNAME=root -e MONGO_INITDB_ROOT_PASSWORD=secret mongo:7
//! docker run -d --name dbine-test-mongodb-noauth -p 27103:27017 mongo:7
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:27102/?authSource=admin \
//! DBINE_TEST_MONGODB_NOAUTH_URL=mongodb://localhost:27103/ \
//! DBINE_TEST_FERRETDB_URL=mongodb://localhost:25203/ \
//!   cargo test -p dbine-driver-mongodb --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, Driver, QueryOutcome, Session};
use std::sync::Arc;

fn setup(var: &str, id: &str) -> Option<(Arc<dyn Driver>, ConnectionConfig)> {
    let url = std::env::var(var).ok()?;
    let mut c = ConnectionConfig { driver: id.into(), database: "dbine_perm".into(), ..Default::default() };
    c.options.insert("connection_string".into(), url);
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == id)?;
    Some((d, c))
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

#[tokio::test]
#[ignore]
async fn root_limited_users_and_read_only() {
    let Some((d, c)) = setup("DBINE_TEST_MONGODB_URL", "mongodb") else {
        eprintln!("DBINE_TEST_MONGODB_URL not set; skipping");
        return;
    };
    let mut admin = d.connect(&c, None).await.expect("connect");
    let p = admin.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("root: {p:?}");
    assert_eq!((&p.profiler, &p.kill_session, &p.create_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    assert_eq!((&p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed));
    assert_eq!(admin.permissions(None).await.unwrap().drop_database, Access::Unknown);

    let mut out = QueryOutcome::default();
    for u in ["perm_rw", "perm_dba"] {
        let _ = admin.execute(&format!("db.runCommand({{ dropUser: '{u}' }})"), 10, &mut out).await;
    }
    run(&mut admin, "db.runCommand({ createUser: 'perm_rw', pwd: 'rw', roles: [{ role: 'readWrite', db: 'dbine_perm' }] })").await;
    run(&mut admin, "db.runCommand({ createUser: 'perm_dba', pwd: 'dba', roles: [{ role: 'dbAdmin', db: 'dbine_perm' }] })").await;
    let user = |u: &str, pw: &str| {
        let mut c = c.clone();
        c.options.insert(
            "connection_string".into(),
            c.options["connection_string"].replace("root:secret@", &format!("{u}:{pw}@")).replace("authSource=admin", "authSource=dbine_perm"),
        );
        c
    };

    let mut s = d.connect(&user("perm_rw", "rw"), None).await.expect("connect as readWrite");
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("readWrite: {p:?}");
    assert!(denied(&p.profiler, "inprog"));
    assert!(denied(&p.kill_session, "killop"));
    assert_eq!(p.create_database, Access::Unknown);
    assert!(denied(&p.drop_database, "dropDatabase"));
    assert!(denied(&p.manage_security, "createUser"));

    let mut s = d.connect(&user("perm_dba", "dba"), None).await.expect("connect as dbAdmin");
    run(&mut admin, "db.runCommand({ profile: 0, sampleRate: 0.5 })").await;
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("dbAdmin, profiling off: {p:?}");
    // Off with sampleRate below 1: it can't set sampleRate (server-wide)
    // nor sample currentOp.
    assert!(denied(&p.profiler, "está apagado"));
    assert_eq!(p.drop_database, Access::Allowed);
    assert!(denied(&p.kill_session, "killop"));
    run(&mut admin, "db.runCommand({ profile: 1 })").await;
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("dbAdmin, profiling on: {p:?}");
    assert_eq!(p.profiler, Access::Allowed);
    // What it says, the server does: the profiler reads system.profile.
    let opts = dbine_driver::ProfilerOptions { database: "dbine_perm".into(), change_server: true };
    let started = s.profiler_start(&opts).await.expect("profiler as dbAdmin");
    eprintln!("dbAdmin profiler: {started:?}");
    assert_eq!(started.mode, dbine_driver::ProfilerMode::Complete);
    s.profiler_poll().await.expect("poll as dbAdmin");
    s.profiler_stop().await.expect("stop");
    run(&mut admin, "db.runCommand({ profile: 0, sampleRate: 1.0 })").await;

    let mut ro = c.clone();
    ro.read_only = true;
    let mut s = d.connect(&ro, None).await.expect("connect read-only");
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("read-only connection: {p:?}");
    // What the server grants, not DBine's read-only mode.
    assert_eq!((&p.drop_database, &p.profiler), (&Access::Allowed, &Access::Allowed));

    for u in ["perm_rw", "perm_dba"] {
        run(&mut admin, &format!("db.runCommand({{ dropUser: '{u}' }})")).await;
    }
}

/// A user with only `dbAdmin` (and `read`) on one database turns the
/// profiler on and back off: `sampleRate` (server-wide) is left alone while
/// it is already 1.
#[tokio::test]
#[ignore]
async fn db_admin_only_turns_the_profiler_on() {
    let Some((d, c)) = setup("DBINE_TEST_MONGODB_URL", "mongodb") else {
        eprintln!("DBINE_TEST_MONGODB_URL not set; skipping");
        return;
    };
    let mut admin = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    let _ = admin.execute("db.runCommand({ dropUser: 'perm_dba_only' })", 10, &mut out).await;
    run(
        &mut admin,
        "db.runCommand({ createUser: 'perm_dba_only', pwd: 'dba', roles: [{ role: 'dbAdmin', db: 'dbine_perm' }, { role: 'read', db: 'dbine_perm' }] })",
    )
    .await;
    run(&mut admin, "db.runCommand({ profile: 0, sampleRate: 1.0 })").await;
    let mut u = c.clone();
    u.options.insert(
        "connection_string".into(),
        c.options["connection_string"].replace("root:secret@", "perm_dba_only:dba@").replace("authSource=admin", "authSource=dbine_perm"),
    );
    let mut s = d.connect(&u, None).await.expect("connect as dbAdmin");
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("dbAdmin only, profiling off: {p:?}");
    assert_eq!(p.profiler, Access::Allowed);
    let opts = dbine_driver::ProfilerOptions { database: "dbine_perm".into(), change_server: true };
    let started = s.profiler_start(&opts).await.expect("profiler as dbAdmin");
    eprintln!("dbAdmin only profiler: {started:?}");
    assert_eq!(started.mode, dbine_driver::ProfilerMode::Complete);
    assert!(started.changes.iter().any(|c| c.contains("= 2 (estaba en 0)")), "{:?}", started.changes);
    assert!(!started.changes.iter().any(|c| c.contains("sampleRate")), "{:?}", started.changes);
    s.execute("db.perm_t.find({})", 10, &mut QueryOutcome::default()).await.expect("find");
    s.profiler_poll().await.expect("poll as dbAdmin");
    s.profiler_stop().await.expect("stop as dbAdmin");
    let mut after = QueryOutcome::default();
    admin.execute("db.runCommand({ profile: -1 })", 10, &mut after).await.unwrap();
    assert_eq!(after.results[0].rows[0][0].to_string(), "0", "level restored: {:?}", after.results[0].rows[0]);
    run(&mut admin, "db.runCommand({ dropUser: 'perm_dba_only' })").await;
}

#[tokio::test]
#[ignore]
async fn no_access_control_and_ferretdb() {
    if let Some((d, c)) = setup("DBINE_TEST_MONGODB_NOAUTH_URL", "mongodb") {
        let mut s = d.connect(&c, None).await.expect("connect");
        let p = s.permissions(Some("dbine_perm")).await.unwrap();
        eprintln!("no auth: {p:?}");
        assert_eq!((&p.profiler, &p.kill_session, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    }
    if let Some((d, c)) = setup("DBINE_TEST_FERRETDB_URL", "ferretdb") {
        let mut s = d.connect(&c, None).await.expect("connect");
        let p = s.permissions(Some("dbine_perm")).await.unwrap();
        eprintln!("ferretdb: {p:?}");
        assert_eq!((&p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!(p.profiler, Access::Unknown);
    }
}
