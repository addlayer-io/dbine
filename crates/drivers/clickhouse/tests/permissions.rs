//! `Session::permissions` against a real server: an administrator, a user
//! with only SELECT, a user with CREATE DATABASE on a single name, a user whose profile is read-only, and DBine's own
//! read-only mode.
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

#[tokio::test]
#[ignore]
async fn admin_plain_and_read_only_users() {
    let Some(base) = cfg() else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().remove(0);
    let mut admin = d.connect(&base, None).await.expect("connect");
    let p = admin.permissions(Some("default")).await.unwrap();
    eprintln!("admin: {p:?}");
    assert_eq!((&p.backup, &p.restore, &p.profiler), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    assert_eq!((&p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    let p = admin.permissions(None).await.unwrap();
    assert_eq!((&p.backup, &p.restore, &p.drop_database), (&Access::Unknown, &Access::Unknown, &Access::Unknown));

    for sql in [
        "DROP USER IF EXISTS dbine_perm_ltd, dbine_perm_ro, dbine_perm_one",
        "CREATE USER dbine_perm_one IDENTIFIED BY 'one'",
        "GRANT CREATE DATABASE ON dbine_perm_only.* TO dbine_perm_one",
        "CREATE USER dbine_perm_ltd IDENTIFIED BY 'ltd'",
        "GRANT SELECT ON default.* TO dbine_perm_ltd",
        "CREATE USER dbine_perm_ro IDENTIFIED BY 'ro' SETTINGS readonly = 2",
        "GRANT ALL ON default.* TO dbine_perm_ro",
    ] {
        run(&mut admin, sql).await;
    }
    let user = |u: &str, pw: &str| ConnectionConfig { username: Some(u.into()), password: Some(pw.into()), ..base.clone() };

    let mut s = d.connect(&user("dbine_perm_ltd", "ltd"), None).await.expect("connect as plain user");
    let p = s.permissions(Some("default")).await.unwrap();
    eprintln!("plain: {p:?}");
    assert!(denied(&p.backup, "BACKUP"));
    assert!(denied(&p.restore, "CREATE TABLE e INSERT"));
    assert!(denied(&p.profiler, "system.query_log"));
    assert!(denied(&p.create_database, "CREATE DATABASE"));
    assert!(denied(&p.drop_database, "DROP DATABASE"));
    assert!(denied(&p.manage_security, "ACCESS MANAGEMENT"));

    // CREATE DATABASE on a single name: it may create that one, so unknown.
    let mut s = d.connect(&user("dbine_perm_one", "one"), None).await.expect("connect as named-grant user");
    let p = s.permissions(Some("default")).await.unwrap();
    eprintln!("named create: {p:?}");
    assert_eq!(p.create_database, Access::Unknown);
    assert!(denied(&p.drop_database, "DROP DATABASE"));

    let mut s = d.connect(&user("dbine_perm_ro", "ro"), None).await.expect("connect as read-only user");
    let p = s.permissions(Some("default")).await.unwrap();
    eprintln!("read-only profile: {p:?}");
    assert!(denied(&p.drop_database, "readonly = 0"));
    assert!(denied(&p.restore, "readonly = 0"));

    let mut s = d.connect(&ConnectionConfig { read_only: true, ..base.clone() }, None).await.expect("connect read-only");
    let p = s.permissions(Some("default")).await.unwrap();
    eprintln!("read-only connection: {p:?}");
    // What the server grants: DBine's read-only mode isn't a missing privilege.
    assert_eq!((&p.drop_database, &p.restore, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    assert_eq!(p.profiler, Access::Allowed);

    run(&mut admin, "DROP USER IF EXISTS dbine_perm_ltd, dbine_perm_ro, dbine_perm_one").await;
}
