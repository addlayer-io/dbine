//! `Session::permissions` against a real server: `root`, a user with
//! READ_DATA only, and one that gets MANAGE_DATABASE and MAINTAIN through a
//! role.
//!
//! ```sh
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 \
//!   cargo test -p dbine-driver-iotdb --test permissions -- --ignored --nocapture
//! ```
//! (`DBINE_TEST_IOTDB2_URL=http://localhost:27150` for a 2.x server too.)

use dbine_driver::{Access, ConnectionConfig, QueryOutcome, Session};

fn cfg(url: &str, user: &str, password: &str) -> ConnectionConfig {
    ConnectionConfig { driver: "iotdb".into(), host: url.into(), username: Some(user.into()), password: Some(password.into()), ..Default::default() }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

async fn check(url: &str) {
    let d = dbine_driver_iotdb::drivers().remove(0);
    let mut root = d.connect(&cfg(url, "root", "root"), None).await.unwrap();
    let p = root.permissions(Some("root.dbine_perm")).await.unwrap();
    eprintln!("{url} root: {p:?}");
    assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));

    for sql in ["DROP USER dbine_perm_ltd", "DROP USER dbine_perm_ops", "DROP ROLE dbine_perm_ops"] {
        let _ = root.execute(sql, 10, &mut QueryOutcome::default()).await;
    }
    for sql in [
        "CREATE USER dbine_perm_ltd 'ltd_pw_123'",
        "GRANT READ_DATA ON root.** TO USER dbine_perm_ltd",
        "CREATE USER dbine_perm_ops 'ops_pw_123'",
        "CREATE ROLE dbine_perm_ops",
        "GRANT MANAGE_DATABASE ON root.** TO ROLE dbine_perm_ops",
        "GRANT MAINTAIN ON root.** TO ROLE dbine_perm_ops",
        "GRANT ROLE dbine_perm_ops TO dbine_perm_ops",
    ] {
        run(&mut root, sql).await;
    }

    let mut s = d.connect(&cfg(url, "dbine_perm_ltd", "ltd_pw_123"), None).await.unwrap();
    let p = s.permissions(Some("root.dbine_perm")).await.unwrap();
    eprintln!("{url} reader: {p:?}");
    assert!(denied(&p.profiler, "MAINTAIN") && denied(&p.create_database, "MANAGE_DATABASE"), "{p:?}");
    assert!(denied(&p.drop_database, "MANAGE_DATABASE") && denied(&p.manage_security, "MANAGE_USER"), "{p:?}");

    let mut s = d.connect(&cfg(url, "dbine_perm_ops", "ops_pw_123"), None).await.unwrap();
    let p = s.permissions(Some("root.dbine_perm")).await.unwrap();
    eprintln!("{url} ops: {p:?}");
    assert_eq!((&p.profiler, &p.create_database, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    assert!(denied(&p.manage_security, "MANAGE_USER"), "{p:?}");

    for sql in ["DROP USER dbine_perm_ltd", "DROP USER dbine_perm_ops", "DROP ROLE dbine_perm_ops"] {
        run(&mut root, sql).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn users_and_roles() {
    for var in ["DBINE_TEST_IOTDB_URL", "DBINE_TEST_IOTDB2_URL"] {
        if let Ok(url) = std::env::var(var) {
            check(&url).await;
        }
    }
}
