//! `Session::permissions` against a real server (see tests/integration.rs):
//! root (a server user), a database `admin`, a database `writer` and
//! DBine's read-only mode. Works on its own database, `dbine_perm`.
//!
//! ```sh
//! DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 \
//!   cargo test -p dbine-driver-orientdb --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, QueryOutcome, Session};

fn cfg(user: Option<(&str, &str)>) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ORIENTDB_URL").ok()?;
    let (auth, hp) = url.rsplit_once('@')?;
    let (u, p) = user.or_else(|| auth.split_once(':'))?;
    let (host, port) = hp.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(u.into()),
        password: Some(p.into()),
        database: "dbine_perm".into(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

#[tokio::test]
#[ignore]
async fn server_user_database_users_and_read_only() {
    let Some(c) = cfg(None) else {
        eprintln!("DBINE_TEST_ORIENTDB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_orientdb::drivers().remove(0);
    let mut root = d.connect(&ConnectionConfig { database: String::new(), ..c.clone() }, None).await.expect("connect");
    let _ = root.drop_database("dbine_perm").await;
    root.create_database("dbine_perm").await.expect("create dbine_perm");
    let mut root = d.connect(&c, Some("dbine_perm")).await.expect("connect to dbine_perm");
    let p = root.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("root: {p:?}");
    assert_eq!((&p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    run(&mut root, "CREATE USER perm_a IDENTIFIED BY 'pw' ROLE admin").await;
    run(&mut root, "CREATE USER perm_w IDENTIFIED BY 'pw' ROLE writer").await;

    let mut s = d.connect(&cfg(Some(("perm_a", "pw"))).unwrap(), Some("dbine_perm")).await.expect("connect as admin");
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("database admin: {p:?}");
    assert_eq!(p.manage_security, Access::Allowed);
    // Not a server user, but that isn't proven: unknown.
    assert_eq!((&p.create_database, &p.drop_database), (&Access::Unknown, &Access::Unknown));
    // What it says, the server does.
    run(&mut s, "CREATE USER perm_x IDENTIFIED BY 'pw' ROLE reader").await;
    assert!(s.create_database("dbine_perm_x").await.is_err());

    let mut s = d.connect(&cfg(Some(("perm_w", "pw"))).unwrap(), Some("dbine_perm")).await.expect("connect as writer");
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("database writer: {p:?}");
    assert_eq!(p, dbine_driver::Permissions::default());

    let mut s = d.connect(&ConnectionConfig { read_only: true, ..c.clone() }, Some("dbine_perm")).await.expect("connect read-only");
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("read-only connection: {p:?}");
    // What the server grants: DBine's read-only mode isn't a missing privilege.
    assert_eq!(p.drop_database, Access::Allowed);

    root.drop_database("dbine_perm").await.expect("drop dbine_perm");
}
