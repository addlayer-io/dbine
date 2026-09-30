//! `Session::permissions` against a real server: the database's owner (an
//! administrator) and a plain user created here.
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, QueryOutcome};

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url.strip_prefix("firebird://").expect("firebird://user:pass@host:port/path");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, path) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

#[tokio::test]
#[ignore]
async fn admin_and_plain_user() {
    let Some(base) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    let mut admin = d.connect(&base, None).await.expect("connect");
    let db = base.database.clone();
    let p = admin.permissions(Some(&db)).await.unwrap();
    eprintln!("admin: {p:?}");
    assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
    assert_eq!(admin.permissions(None).await.unwrap().drop_database, Access::Unknown);

    let mut out = QueryOutcome::default();
    admin.execute("CREATE OR ALTER USER DBINE_PERM_LTD PASSWORD 'ltd'", 10, &mut out).await.unwrap();
    admin.execute("COMMIT", 10, &mut out).await.ok();
    let plain = ConnectionConfig { username: Some("DBINE_PERM_LTD".into()), password: Some("ltd".into()), ..base.clone() };
    let mut s = d.connect(&plain, None).await.expect("connect as plain user");
    let p = s.permissions(Some(&db)).await.unwrap();
    eprintln!("plain: {p:?}");
    assert!(p.profiler.is_denied());
    // Firebird 4+: CREATE_DATABASE may come from a security-database role.
    assert_eq!(p.create_database, Access::Unknown);
    assert!(p.drop_database.is_denied());
    assert!(p.manage_security.is_denied());
    assert_eq!((p.backup, p.restore, p.kill_session), (Access::Unknown, Access::Unknown, Access::Unknown));
    drop(s);
    admin.execute("DROP USER DBINE_PERM_LTD", 10, &mut out).await.unwrap();
}
