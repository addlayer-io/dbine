//! `Session::permissions` against a real TDengine: `root`, a user with
//! SYSINFO 1 and CREATEDB 0, and one with SYSINFO 0 (which can't read
//! `SHOW USERS`). Neither is denied anything: the server doesn't refuse
//! them what a superuser does.
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//!   cargo test -p dbine-driver-tdengine --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, Permissions, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TDENGINE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn root_and_plain_users() {
    let Some(base) = cfg() else { return };
    let d = dbine_driver_tdengine::drivers().remove(0);
    let mut root = d.connect(&base, None).await.unwrap();
    let p = root.permissions(Some("log")).await.unwrap();
    eprintln!("root: {p:?}");
    assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));

    for sql in ["DROP USER dbine_perm_sys", "DROP USER dbine_perm_min"] {
        let _ = root.execute(sql, 10, &mut QueryOutcome::default()).await;
    }
    run(&mut root, "CREATE USER dbine_perm_sys PASS 'Sys_pw_123' SYSINFO 1 CREATEDB 0").await;
    run(&mut root, "CREATE USER dbine_perm_min PASS 'Min_pw_123' SYSINFO 0").await;
    let user = |u: &str, pw: &str| ConnectionConfig { username: Some(u.into()), password: Some(pw.into()), ..base.clone() };

    let mut s = d.connect(&user("dbine_perm_sys", "Sys_pw_123"), None).await.unwrap();
    let p = s.permissions(Some("log")).await.unwrap();
    eprintln!("sysinfo: {p:?}");
    // CREATEDB 0 doesn't stop it (TDengine 3.3.6 OSS), so nothing is denied.
    assert_eq!(p, Permissions { profiler: Access::Allowed, ..Default::default() });

    let mut s = d.connect(&user("dbine_perm_min", "Min_pw_123"), None).await.unwrap();
    let p = s.permissions(Some("log")).await.unwrap();
    eprintln!("no sysinfo: {p:?}");
    assert_eq!(p, Permissions::default());

    run(&mut root, "DROP USER dbine_perm_sys").await;
    run(&mut root, "DROP USER dbine_perm_min").await;
}
