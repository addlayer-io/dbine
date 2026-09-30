//! `Session::permissions` against a SolrCloud with security.json (ignored
//! by default): the admin, a user who may read the authorization list but
//! holds nothing else, a user who can't read it, and DBine's read-only mode.
//!
//! ```sh
//! docker run -d --name dbine-test-solr-perm -p 25541:8983 solr:9 solr -c -f
//! docker cp security.json dbine-test-solr-perm:/tmp/security.json
//! docker exec dbine-test-solr-perm solr zk cp file:/tmp/security.json zk:/security.json -z localhost:9983
//! DBINE_TEST_SOLR_PERM_URL=http://localhost:25541 cargo test -p dbine-driver-solr --test permissions -- --ignored --nocapture
//! ```
//!
//! security.json: `solr.BasicAuthPlugin` with user `solr` / `SolrRocks`
//! (`"credentials": {"solr": "IV0EHq1OnNrj6gvRCwvFwTrZ1+z1oBbnQdiVC3otuq0= Ndd7LKvVBAaZIF0QAVi1ekCfAJXr1GGfLtRUXhgrF8c="}`)
//! and `solr.RuleBasedAuthorizationPlugin` with `"user-role": {"solr": "admin"}`
//! and the permissions `security-edit`, `security-read`,
//! `collection-admin-edit` and `all`, all for `admin`.

use dbine_driver::{Access, ConnectionConfig, QueryOutcome, Session};

fn cfg(user: &str, pass: &str) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_SOLR_PERM_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (host, port) = rest.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "solr".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

#[tokio::test]
#[ignore]
async fn admin_limited_users_and_read_only() {
    let Some(c) = cfg("solr", "SolrRocks") else {
        eprintln!("DBINE_TEST_SOLR_PERM_URL not set; skipping");
        return;
    };
    let d = dbine_driver_solr::drivers().remove(0);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let p = admin.permissions(None).await.unwrap();
    eprintln!("admin: {p:?}");
    assert_eq!((&p.backup, &p.restore, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed));

    run(&mut admin, "POST /solr/admin/authentication\n{\"set-user\": {\"perm_aud\": \"aud_pw\", \"perm_rd\": \"rd_pw\"}}").await;
    run(&mut admin, "POST /solr/admin/authorization\n{\"set-user-role\": {\"perm_aud\": [\"auditor\"], \"perm_rd\": [\"reader\"]}}").await;
    // First match wins: both go before the admin-only ones. `config-read`
    // lets anyone read /admin/info/system, which connecting needs.
    run(&mut admin, "POST /solr/admin/authorization\n{\"set-permission\": {\"name\": \"security-read\", \"role\": [\"admin\", \"auditor\"], \"before\": 1}}").await;
    run(&mut admin, "POST /solr/admin/authorization\n{\"set-permission\": {\"name\": \"config-read\", \"role\": \"*\", \"before\": 1}}").await;
    // Security plugins reload asynchronously in SolrCloud.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let mut s = d.connect(&cfg("perm_aud", "aud_pw").unwrap(), None).await.expect("connect as auditor");
    let p = s.permissions(None).await.unwrap();
    eprintln!("auditor (reads the list): {p:?}");
    assert!(denied(&p.backup, "collection-admin-edit") && denied(&p.restore, "collection-admin-edit"));
    assert!(denied(&p.manage_security, "security-edit"));
    // What it says, the server does.
    let mut out = QueryOutcome::default();
    let edit = s.execute("POST /solr/admin/authentication\n{\"set-user\": {\"perm_x\": \"x\"}}", 10, &mut out).await;
    assert!(edit.is_err() || out.error.is_some(), "the auditor edited security");

    let mut s = d.connect(&cfg("perm_rd", "rd_pw").unwrap(), None).await.expect("connect as reader");
    let p = s.permissions(None).await.unwrap();
    eprintln!("reader (can't read the list): {p:?}");
    assert_eq!((&p.backup, &p.manage_security), (&Access::Unknown, &Access::Unknown));

    let mut s = d.connect(&ConnectionConfig { read_only: true, ..c.clone() }, None).await.expect("connect read-only");
    let p = s.permissions(None).await.unwrap();
    eprintln!("read-only connection: {p:?}");
    // What the server grants: DBine's read-only mode isn't a missing privilege.
    assert_eq!((&p.backup, &p.manage_security), (&Access::Allowed, &Access::Allowed));

    run(&mut admin, "POST /solr/admin/authentication\n{\"delete-user\": [\"perm_aud\", \"perm_rd\"]}").await;
    // The two permissions added above (indexes 1 and 2), the new first ones.
    run(&mut admin, "POST /solr/admin/authorization\n{\"delete-permission\": 1}").await;
    run(&mut admin, "POST /solr/admin/authorization\n{\"delete-permission\": 1}").await;
}
