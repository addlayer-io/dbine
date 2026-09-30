//! `Session::permissions` against real servers (URLs as in
//! tests/security.rs, `user:password@host:port`):
//!
//! - Neo4j Enterprise: the admin, a reader, and a user whose role may see
//!   one database's transactions, end only bob's and create databases.
//! - Neo4j Community: any user may do everything but create or drop a
//!   database (Community has one).
//! - Memgraph Community: with users (a throwaway server: the first user
//!   turns authentication on) and without them, everything is allowed.
//!
//! ```sh
//! DBINE_TEST_NEO4J_EE_URL=neo4j:dbine-test-pass@localhost:17688 \
//! DBINE_TEST_NEO4J_CE_URL=neo4j:dbine-test-pass@localhost:17687 \
//! DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//! DBINE_TEST_MEMGRAPH_AUTH_URL=ltd:ltdpw@localhost:27688 \
//!   cargo test -p dbine-driver-neo4j --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, QueryOutcome, Session};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), username: user, password: pass, ..Default::default() }
}

fn driver(id: &str) -> std::sync::Arc<dyn dbine_driver::Driver> {
    dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_enterprise() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_EE_URL") else { return };
    let d = driver("neo4j");
    let base = cfg("neo4j", &url);
    let mut admin = d.connect(&base, Some("system")).await.unwrap();
    let p = admin.permissions(Some("neo4j")).await.unwrap();
    eprintln!("admin: {p:?}");
    assert_eq!((&p.profiler, &p.kill_session, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));

    for q in [
        "DROP USER dbine_perm_ltd IF EXISTS",
        "DROP USER dbine_perm_ops IF EXISTS",
        "DROP ROLE dbine_perm_ops IF EXISTS",
        "CREATE USER dbine_perm_ltd SET PASSWORD 'ltd-pass-1' CHANGE NOT REQUIRED",
        "GRANT ROLE reader TO dbine_perm_ltd",
        "CREATE ROLE dbine_perm_ops",
        "GRANT SHOW TRANSACTION (*) ON DATABASE neo4j TO dbine_perm_ops",
        "GRANT TERMINATE TRANSACTION (bob) ON DATABASE * TO dbine_perm_ops",
        "GRANT CREATE DATABASE ON DBMS TO dbine_perm_ops",
        "CREATE USER dbine_perm_ops SET PASSWORD 'ops-pass-1' CHANGE NOT REQUIRED",
        "GRANT ROLE dbine_perm_ops TO dbine_perm_ops",
    ] {
        run(&mut admin, q).await;
    }
    let user = |u: &str, pw: &str| ConnectionConfig { username: Some(u.into()), password: Some(pw.into()), ..base.clone() };

    let mut s = d.connect(&user("dbine_perm_ltd", "ltd-pass-1"), Some("neo4j")).await.unwrap();
    let p = s.permissions(Some("neo4j")).await.unwrap();
    eprintln!("reader: {p:?}");
    assert!(denied(&p.profiler, "SHOW TRANSACTION") && denied(&p.kill_session, "TERMINATE TRANSACTION"), "{p:?}");
    assert!(denied(&p.create_database, "CREATE DATABASE") && denied(&p.drop_database, "DROP DATABASE"), "{p:?}");
    assert!(denied(&p.manage_security, "USER MANAGEMENT"), "{p:?}");

    let mut s = d.connect(&user("dbine_perm_ops", "ops-pass-1"), Some("neo4j")).await.unwrap();
    let p = s.permissions(Some("neo4j")).await.unwrap();
    eprintln!("ops: {p:?}");
    assert_eq!((&p.profiler, &p.kill_session, &p.create_database), (&Access::Allowed, &Access::Unknown, &Access::Allowed));
    assert!(denied(&p.drop_database, "DROP DATABASE"), "{p:?}");
    let p = s.permissions(Some("system")).await.unwrap();
    assert!(denied(&p.profiler, "SHOW TRANSACTION"), "{p:?}");

    for q in ["DROP USER dbine_perm_ltd", "DROP USER dbine_perm_ops", "DROP ROLE dbine_perm_ops"] {
        run(&mut admin, q).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_community() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_CE_URL") else { return };
    let d = driver("neo4j");
    let base = cfg("neo4j", &url);
    let mut admin = d.connect(&base, Some("system")).await.unwrap();
    let _ = admin.execute("DROP USER dbine_perm_ce IF EXISTS", 10, &mut QueryOutcome::default()).await;
    run(&mut admin, "CREATE USER dbine_perm_ce SET PASSWORD 'ce-pass-12' CHANGE NOT REQUIRED").await;
    let user = ConnectionConfig { username: Some("dbine_perm_ce".into()), password: Some("ce-pass-12".into()), ..base.clone() };
    let mut s = d.connect(&user, None).await.unwrap();
    let p = s.permissions(Some("neo4j")).await.unwrap();
    eprintln!("community user: {p:?}");
    assert_eq!((&p.profiler, &p.kill_session, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    assert_eq!((&p.create_database, &p.drop_database), (&Access::Unknown, &Access::Unknown));
    run(&mut admin, "DROP USER dbine_perm_ce").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn memgraph() {
    for var in ["DBINE_TEST_MEMGRAPH_URL", "DBINE_TEST_MEMGRAPH_AUTH_URL"] {
        let Ok(url) = std::env::var(var) else { continue };
        let mut s = driver("memgraph").connect(&cfg("memgraph", &url), None).await.unwrap();
        let p = s.permissions(Some("memgraph")).await.unwrap();
        eprintln!("{var}: {p:?}");
        assert_eq!((&p.backup, &p.restore, &p.profiler, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!((&p.create_database, &p.kill_session), (&Access::Unknown, &Access::Unknown));
    }
}
