//! `Session::permissions` against a real Couchbase Server, provisioned as
//! in tests/integration.rs: the administrator, a `ro_admin`, a
//! `bucket_full_access` user and DBine's read-only mode.
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig};

const USER: &str = "Administrator";
const PASS: &str = "secreto1";
const BUCKET: &str = "nsq";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

/// Users through the cluster manager's REST API.
async fn user(c: &ConnectionConfig, name: &str, roles: Option<&str>) {
    let url = format!("http://{}:{}/settings/rbac/users/local/{name}", c.host, c.options["mgmt_port"]);
    let http = reqwest::Client::new();
    let rb = match roles {
        Some(r) => http.put(url).form(&[("password", "perm_pw_1"), ("roles", r)]),
        None => http.delete(url),
    };
    let resp = rb.basic_auth(USER, Some(PASS)).send().await.unwrap();
    assert!(resp.status().is_success() || roles.is_none(), "{name}: {}", resp.status());
}

#[tokio::test]
#[ignore]
async fn admin_limited_users_and_read_only() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_COUCHBASE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_couchbase::drivers().remove(0);
    let mut admin = d.connect(&c, Some(BUCKET)).await.expect("connect");
    let p = admin.permissions(Some(BUCKET)).await.unwrap();
    eprintln!("admin: {p:?}");
    assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));

    user(&c, "perm_ro", Some("ro_admin")).await;
    user(&c, "perm_bfa", Some(&format!("bucket_full_access[{BUCKET}]"))).await;
    let as_user = |u: &str| ConnectionConfig { username: Some(u.into()), password: Some("perm_pw_1".into()), ..c.clone() };

    let mut s = d.connect(&as_user("perm_ro"), Some(BUCKET)).await.expect("connect as ro_admin");
    let p = s.permissions(Some(BUCKET)).await.unwrap();
    eprintln!("ro_admin: {p:?}");
    assert_eq!(p.profiler, Access::Allowed);
    assert!(denied(&p.create_database, "cluster.buckets!create"));
    assert!(denied(&p.drop_database, "!delete"));
    assert!(denied(&p.manage_security, "security.local!write"));

    let mut s = d.connect(&as_user("perm_bfa"), Some(BUCKET)).await.expect("connect as bucket_full_access");
    let p = s.permissions(Some(BUCKET)).await.unwrap();
    eprintln!("bucket_full_access: {p:?}");
    assert!(denied(&p.profiler, "cluster.n1ql.meta!read"));
    assert!(denied(&p.drop_database, "!delete"));
    // What it says, the server does.
    assert!(s.drop_database(BUCKET).await.is_err());
    let opts = dbine_driver::ProfilerOptions { database: BUCKET.into(), change_server: false };
    if s.profiler_start(&opts).await.is_ok() {
        assert!(s.profiler_poll().await.is_err(), "the profiler read system:completed_requests");
    }

    let mut s = d.connect(&ConnectionConfig { read_only: true, ..c.clone() }, Some(BUCKET)).await.expect("connect read-only");
    let p = s.permissions(Some(BUCKET)).await.unwrap();
    eprintln!("read-only connection: {p:?}");
    // What the server grants: DBine's read-only mode isn't a missing privilege.
    assert_eq!(p.manage_security, Access::Allowed);

    user(&c, "perm_ro", None).await;
    user(&c, "perm_bfa", None).await;
}
