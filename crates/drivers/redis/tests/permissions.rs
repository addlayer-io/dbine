//! `Session::permissions` against a real server: the default user, a user
//! without MONITOR and the backup commands, a user that may ask `ACL
//! DRYRUN` but little else (not on Dragonfly, which has no subcommand
//! rules), and a user that can't ask at all.
//!
//! ```sh
//! DBINE_TEST_REDIS_URL=redis://localhost:25400 \
//! DBINE_TEST_VALKEY_URL=redis://localhost:25401 \
//! DBINE_TEST_DRAGONFLY_URL=redis://localhost:25407 \
//!   cargo test -p dbine-driver-redis --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, QueryOutcome, Session};

fn cfg(driver: &str, url: &str, user: Option<&str>, password: Option<&str>) -> ConnectionConfig {
    let rest = url.trim_start_matches("redis://");
    let (host, port) = rest.split_once(':').unwrap_or((rest, "6379"));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: user.map(Into::into),
        password: password.map(Into::into),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

async fn check(driver: &str, url: &str) {
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let mut admin = d.connect(&cfg(driver, url, None, None), None).await.unwrap();
    let p = admin.permissions(Some("db0")).await.unwrap();
    eprintln!("{driver} default: {p:?}");
    assert_eq!((&p.backup, &p.profiler, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    assert_eq!((&p.restore, &p.kill_session, &p.drop_database), (&Access::Unknown, &Access::Unknown, &Access::Unknown));

    let users = "dbine_perm_ops dbine_perm_ask dbine_perm_plain";
    let _ = admin.execute(&format!("ACL DELUSER {users}"), 10, &mut QueryOutcome::default()).await;
    run(&mut admin, "ACL SETUSER dbine_perm_ops on >ops ~* &* +@all -monitor -bgsave -save").await;
    run(&mut admin, "ACL SETUSER dbine_perm_plain on >plain ~* +@read").await;
    let dragonfly = driver == "dragonfly";
    if !dragonfly {
        run(&mut admin, "ACL SETUSER dbine_perm_ask on >ask ~* +@read +acl|whoami +acl|dryrun").await;
    }

    let mut s = d.connect(&cfg(driver, url, Some("dbine_perm_ops"), Some("ops")), None).await.unwrap();
    let p = s.permissions(None).await.unwrap();
    eprintln!("{driver} ops: {p:?}");
    assert!(denied(&p.backup, "BGSAVE"), "{p:?}");
    assert!(denied(&p.profiler, "MONITOR"), "{p:?}");
    assert_eq!(p.manage_security, Access::Allowed);

    if !dragonfly {
        let mut s = d.connect(&cfg(driver, url, Some("dbine_perm_ask"), Some("ask")), None).await.unwrap();
        let p = s.permissions(None).await.unwrap();
        eprintln!("{driver} ask: {p:?}");
        assert!(denied(&p.backup, "BGSAVE") && denied(&p.profiler, "MONITOR") && denied(&p.manage_security, "ACL SETUSER"), "{p:?}");
    }

    // Can't run ACL DRYRUN: nothing is known, nothing is denied.
    let mut s = d.connect(&cfg(driver, url, Some("dbine_perm_plain"), Some("plain")), None).await.unwrap();
    let p = s.permissions(None).await.unwrap();
    eprintln!("{driver} plain: {p:?}");
    assert_eq!(p, dbine_driver::Permissions::default());

    run(&mut admin, &format!("ACL DELUSER {users}")).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn redis_permissions() {
    if let Ok(url) = std::env::var("DBINE_TEST_REDIS_URL") {
        check("redis", &url).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn valkey_permissions() {
    if let Ok(url) = std::env::var("DBINE_TEST_VALKEY_URL") {
        check("valkey", &url).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn dragonfly_permissions() {
    if let Ok(url) = std::env::var("DBINE_TEST_DRAGONFLY_URL") {
        check("dragonfly", &url).await;
    }
}
