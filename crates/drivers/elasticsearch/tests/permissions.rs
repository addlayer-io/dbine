//! `Session::permissions` against real servers (ignored by default): the
//! superuser, a user with only `cluster:monitor/main`, a user with `snapshot_user`, DBine's
//! read-only mode, and servers with security off.
//!
//! ```sh
//! docker run -d --name dbine-test-es-perm -p 25531:9200 -e discovery.type=single-node \
//!   -e ELASTIC_PASSWORD=secret -e xpack.security.http.ssl.enabled=false \
//!   -e "ES_JAVA_OPTS=-Xms512m -Xmx512m" docker.elastic.co/elasticsearch/elasticsearch:8.15.3
//! docker run -d --name dbine-test-opensearch-perm -p 25532:9200 -e discovery.type=single-node \
//!   -e 'OPENSEARCH_INITIAL_ADMIN_PASSWORD=Dbine_Perm_2024!' -e "OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m" \
//!   opensearchproject/opensearch:2.17.1
//! DBINE_TEST_ELASTICSEARCH_AUTH_URL=http://elastic:secret@localhost:25531 \
//! DBINE_TEST_OPENSEARCH_AUTH_URL='https://admin:Dbine_Perm_2024!@localhost:25532' \
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//!   cargo test -p dbine-driver-elasticsearch --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, Driver, Permissions, QueryOutcome, Session};
use std::sync::Arc;

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

/// `scheme://user:pass@host:port` → the connection.
fn cfg(id: &str, var: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(var).ok()?;
    let (scheme, rest) = url.split_once("://")?;
    let (auth, host) = rest.rsplit_once('@').map_or((None, rest), |(a, h)| (Some(a), h));
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    Some(ConnectionConfig {
        driver: id.into(),
        host: format!("{scheme}://{host}"),
        username: user,
        password: pass,
        trust_server_certificate: true,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

fn everything(p: &Permissions) -> bool {
    [&p.backup, &p.restore, &p.profiler, &p.manage_security].iter().all(|a| **a == Access::Allowed)
}

fn as_user(c: &ConnectionConfig, user: &str, pw: &str) -> ConnectionConfig {
    ConnectionConfig { username: Some(user.into()), password: Some(pw.into()), ..c.clone() }
}

#[tokio::test]
#[ignore]
async fn elasticsearch_users() {
    let Some(c) = cfg("elasticsearch", "DBINE_TEST_ELASTICSEARCH_AUTH_URL") else {
        eprintln!("DBINE_TEST_ELASTICSEARCH_AUTH_URL not set; skipping");
        return;
    };
    let d = driver("elasticsearch");
    let mut admin = d.connect(&c, None).await.expect("connect");
    let p = admin.permissions(None).await.unwrap();
    eprintln!("elastic: {p:?}");
    assert!(everything(&p));
    // Connecting reads `GET /` (cluster:monitor/main): the least a user needs.
    run(&mut admin, "PUT /_security/role/perm_main\n{\"cluster\": [\"cluster:monitor/main\"]}").await;
    run(&mut admin, "PUT /_security/user/perm_ltd\n{\"password\": \"perm_ltd_pw\", \"roles\": [\"perm_main\"]}").await;
    run(&mut admin, "PUT /_security/user/perm_snap\n{\"password\": \"perm_snap_pw\", \"roles\": [\"snapshot_user\", \"perm_main\"]}").await;

    let mut s = d.connect(&as_user(&c, "perm_ltd", "perm_ltd_pw"), None).await.expect("connect as a limited user");
    let p = s.permissions(None).await.unwrap();
    eprintln!("only cluster:monitor/main: {p:?}");
    assert!(denied(&p.backup, "create_snapshot"));
    assert!(denied(&p.restore, "manage"));
    assert!(denied(&p.profiler, "monitor"));
    assert!(denied(&p.manage_security, "manage_security"));
    // What it says, the server does.
    let opts = dbine_driver::ProfilerOptions { database: String::new(), change_server: true };
    assert!(s.profiler_start(&opts).await.is_err());

    let mut s = d.connect(&as_user(&c, "perm_snap", "perm_snap_pw"), None).await.expect("connect as snapshot_user");
    let p = s.permissions(None).await.unwrap();
    eprintln!("snapshot_user: {p:?}");
    assert_eq!(p.backup, Access::Allowed);
    assert!(denied(&p.restore, "manage"));

    let mut s = d.connect(&ConnectionConfig { read_only: true, ..c.clone() }, None).await.expect("connect read-only");
    let p = s.permissions(None).await.unwrap();
    eprintln!("read-only connection: {p:?}");
    // What the server grants: DBine's read-only mode isn't a missing privilege.
    assert_eq!((&p.backup, &p.restore, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
    assert_eq!(p.profiler, Access::Allowed);

    run(&mut admin, "DELETE /_security/user/perm_ltd").await;
    run(&mut admin, "DELETE /_security/user/perm_snap").await;
    run(&mut admin, "DELETE /_security/role/perm_main").await;
}

#[tokio::test]
#[ignore]
async fn opensearch_users() {
    let Some(c) = cfg("opensearch", "DBINE_TEST_OPENSEARCH_AUTH_URL") else {
        eprintln!("DBINE_TEST_OPENSEARCH_AUTH_URL not set; skipping");
        return;
    };
    let d = driver("opensearch");
    let mut admin = d.connect(&c, None).await.expect("connect");
    let p = admin.permissions(None).await.unwrap();
    eprintln!("admin: {p:?}");
    assert!(everything(&p));
    run(&mut admin, "PUT /_plugins/_security/api/internalusers/perm_ltd\n{\"password\": \"Xq7_Zebra_2024!\"}").await;
    run(&mut admin, "PUT /_plugins/_security/api/roles/perm_main\n{\"cluster_permissions\": [\"cluster:monitor/main\"]}").await;
    run(&mut admin, "PUT /_plugins/_security/api/rolesmapping/perm_main\n{\"users\": [\"perm_ltd\"]}").await;

    let mut s = d.connect(&as_user(&c, "perm_ltd", "Xq7_Zebra_2024!"), None).await.expect("connect as a limited user");
    let p = s.permissions(None).await.unwrap();
    eprintln!("only cluster:monitor/main: {p:?}");
    assert!(denied(&p.profiler, "cluster_monitor"));
    assert!(denied(&p.manage_security, "API REST de seguridad"));
    assert_eq!((&p.backup, &p.restore), (&Access::Unknown, &Access::Unknown));
    let opts = dbine_driver::ProfilerOptions { database: String::new(), change_server: true };
    assert!(s.profiler_start(&opts).await.is_err());

    run(&mut admin, "DELETE /_plugins/_security/api/internalusers/perm_ltd").await;
    run(&mut admin, "DELETE /_plugins/_security/api/rolesmapping/perm_main").await;
    run(&mut admin, "DELETE /_plugins/_security/api/roles/perm_main").await;
}

#[tokio::test]
#[ignore]
async fn security_off() {
    for (id, var) in [("elasticsearch", "DBINE_TEST_ELASTICSEARCH_URL"), ("opensearch", "DBINE_TEST_OPENSEARCH_URL")] {
        let Some(c) = cfg(id, var) else { continue };
        let mut s = driver(id).connect(&c, None).await.expect("connect");
        let p = s.permissions(None).await.unwrap();
        eprintln!("{id} without security: {p:?}");
        assert!(everything(&p));
        // DBine's read-only mode isn't a missing privilege.
        let mut s = driver(id).connect(&ConnectionConfig { read_only: true, ..c.clone() }, None).await.expect("connect read-only");
        let p = s.permissions(None).await.unwrap();
        eprintln!("{id} without security, read-only connection: {p:?}");
        assert!(everything(&p));
    }
}
