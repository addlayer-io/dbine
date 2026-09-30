//! `Session::permissions` against real servers with authentication on (see
//! tests/security.rs for the containers): the superuser, a role that holds
//! CREATE ON ALL KEYSPACES through another role and DROP on one keyspace,
//! and a role with SELECT only. `DBINE_TEST_CASSANDRA_URL` /
//! `DBINE_TEST_SCYLLADB_URL` (authentication off) allow everything.
//!
//! ```sh
//! DBINE_TEST_CASSANDRA_AUTH_URL=cassandra:cassandra@localhost:27121 \
//! DBINE_TEST_SCYLLADB_AUTH_URL=cassandra:cassandra@localhost:27125 \
//! DBINE_TEST_CASSANDRA_URL=localhost:25402 DBINE_TEST_SCYLLADB_URL=localhost:25413 \
//!   cargo test -p dbine-driver-cassandra --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, QueryOutcome, Session};

fn cfg(id: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig { driver: id.into(), host: host.into(), port: port.parse().unwrap(), username: user, password: pass, ..Default::default() }
}

fn driver(id: &str) -> std::sync::Arc<dyn dbine_driver::Driver> {
    dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

async fn with_auth(id: &str, url: &str) {
    let d = driver(id);
    let base = cfg(id, url);
    let mut admin = d.connect(&base, None).await.unwrap();
    let p = admin.permissions(Some("system")).await.unwrap();
    eprintln!("{id} superuser: {p:?}");
    assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));

    let replication = if id == "scylladb" { "'NetworkTopologyStrategy', 'replication_factor': 1" } else { "'SimpleStrategy', 'replication_factor': 1" };
    for cql in [
        "DROP ROLE IF EXISTS dbine_perm_mid",
        "DROP ROLE IF EXISTS dbine_perm_ltd",
        "DROP ROLE IF EXISTS dbine_perm_grp",
        &format!("CREATE KEYSPACE IF NOT EXISTS dbine_perm WITH replication = {{'class': {replication}}}"),
        "CREATE ROLE dbine_perm_grp",
        "GRANT CREATE ON ALL KEYSPACES TO dbine_perm_grp",
        "CREATE ROLE dbine_perm_mid WITH LOGIN = true AND PASSWORD = 'mid'",
        "GRANT dbine_perm_grp TO dbine_perm_mid",
        "GRANT DROP ON KEYSPACE dbine_perm TO dbine_perm_mid",
        "CREATE ROLE dbine_perm_ltd WITH LOGIN = true AND PASSWORD = 'ltd'",
        "GRANT SELECT ON KEYSPACE dbine_perm TO dbine_perm_ltd",
    ] {
        run(&mut admin, cql).await;
    }
    let user = |u: &str, pw: &str| ConnectionConfig { username: Some(u.into()), password: Some(pw.into()), ..base.clone() };

    let mut s = d.connect(&user("dbine_perm_mid", "mid"), None).await.unwrap();
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("{id} mid: {p:?}");
    assert_eq!((&p.create_database, &p.drop_database), (&Access::Allowed, &Access::Allowed));
    assert!(denied(&p.profiler, "SELECT ON"), "{p:?}");
    assert!(denied(&p.manage_security, "CREATE ON ALL ROLES"), "{p:?}");
    assert!(denied(&s.permissions(Some("system")).await.unwrap().drop_database, "DROP ON KEYSPACE system"));

    let mut s = d.connect(&user("dbine_perm_ltd", "ltd"), None).await.unwrap();
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("{id} ltd: {p:?}");
    assert!(denied(&p.create_database, "CREATE ON ALL KEYSPACES") && denied(&p.drop_database, "DROP ON KEYSPACE dbine_perm"), "{p:?}");
    // The denial is real: the server refuses it too.
    let refused = s.execute("DROP KEYSPACE dbine_perm", 10, &mut QueryOutcome::default()).await;
    assert!(refused.is_err(), "{refused:?}");

    for cql in ["DROP ROLE dbine_perm_mid", "DROP ROLE dbine_perm_ltd", "DROP ROLE dbine_perm_grp", "DROP KEYSPACE dbine_perm"] {
        run(&mut admin, cql).await;
    }
}

async fn without_auth(id: &str, url: &str) {
    let mut s = driver(id).connect(&cfg(id, url), None).await.unwrap();
    let p = s.permissions(Some("system")).await.unwrap();
    eprintln!("{id} open: {p:?}");
    assert_eq!((&p.profiler, &p.create_database, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    assert_eq!(p.manage_security, Access::Unknown);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cassandra() {
    if let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_AUTH_URL") {
        with_auth("cassandra", &url).await;
    }
    if let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_URL") {
        without_auth("cassandra", &url).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn scylladb() {
    if let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_AUTH_URL") {
        with_auth("scylladb", &url).await;
    }
    if let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_URL") {
        without_auth("scylladb", &url).await;
    }
}
