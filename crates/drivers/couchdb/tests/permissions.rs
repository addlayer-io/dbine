//! `Session::permissions` against a real server: the server admin, a plain
//! user, a database admin and DBine's read-only mode.
//!
//! ```sh
//! docker run -d --name dbine-test-couchdb-perm -p 25542:5984 \
//!   -e COUCHDB_USER=admin -e COUCHDB_PASSWORD=secret couchdb:3
//! curl -X PUT http://admin:secret@localhost:25542/_users
//! DBINE_TEST_COUCHDB_PERM_URL=http://admin:secret@localhost:25542 \
//!   cargo test -p dbine-driver-couchdb --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, QueryOutcome, Session};

/// `http://user:pass@host:port` → config.
fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_COUCHDB_PERM_URL").ok()?;
    let rest = url.strip_prefix("http://")?;
    let (auth, host) = rest.split_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (h, p) = host.trim_end_matches('/').split_once(':')?;
    Some(ConnectionConfig {
        driver: "couchdb".into(),
        host: h.into(),
        port: p.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<()> {
    let mut out = QueryOutcome::default();
    s.execute(q, 10, &mut out).await
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

#[tokio::test]
#[ignore]
async fn admin_users_and_read_only() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_COUCHDB_PERM_URL not set; skipping");
        return;
    };
    let d = dbine_driver_couchdb::drivers().remove(0);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let p = admin.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("admin: {p:?}");
    assert_eq!((&p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));

    let _ = run(&mut admin, "DELETE /dbine_perm").await;
    run(&mut admin, "PUT /dbine_perm").await.unwrap();
    for u in ["perm_plain", "perm_dba"] {
        run(&mut admin, &format!("PUT /_users/org.couchdb.user:{u} {{\"name\": \"{u}\", \"password\": \"pw\", \"roles\": [], \"type\": \"user\"}}"))
            .await
            .unwrap();
    }
    run(&mut admin, "PUT /dbine_perm/_security {\"admins\": {\"names\": [\"perm_dba\"], \"roles\": []}, \"members\": {\"names\": [\"perm_dba\", \"perm_plain\"], \"roles\": []}}")
        .await
        .unwrap();
    let user = |u: &str| ConnectionConfig { username: Some(u.into()), password: Some("pw".into()), ..c.clone() };

    let mut s = d.connect(&user("perm_plain"), Some("dbine_perm")).await.expect("connect as plain user");
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("plain user: {p:?}");
    assert!(denied(&p.create_database, "_admin") && denied(&p.drop_database, "_admin"));
    assert_eq!(p.manage_security, Access::Unknown);
    // What it says, the server does.
    assert!(s.drop_database("dbine_perm").await.is_err());

    let mut s = d.connect(&user("perm_dba"), Some("dbine_perm")).await.expect("connect as database admin");
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("database admin: {p:?}");
    assert_eq!(p.manage_security, Access::Allowed);
    assert!(denied(&p.drop_database, "_admin"));

    let mut s = d.connect(&ConnectionConfig { read_only: true, ..c.clone() }, None).await.expect("connect read-only");
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("read-only connection: {p:?}");
    // What the server grants: DBine's read-only mode isn't a missing privilege.
    assert_eq!((&p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));

    run(&mut admin, "DELETE /dbine_perm").await.unwrap();
    // A user document is deleted by its current revision.
    let http = reqwest::Client::new();
    let base = format!("http://{}:{}/_users/org.couchdb.user:", c.host, c.port);
    for u in ["perm_plain", "perm_dba"] {
        let doc: serde_json::Value =
            http.get(format!("{base}{u}")).basic_auth("admin", c.password.as_deref()).send().await.unwrap().json().await.unwrap();
        let rev = doc["_rev"].as_str().unwrap();
        http.delete(format!("{base}{u}?rev={rev}")).basic_auth("admin", c.password.as_deref()).send().await.unwrap();
    }
}
