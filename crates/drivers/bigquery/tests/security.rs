//! DCL scripts and the access list reading against bigquery-emulator (see
//! tests/integration.rs):
//!   DBINE_TEST_BIGQUERY_URL=http://localhost:25302 cargo test -p dbine-driver-bigquery --test security -- --ignored
//! The emulator takes GRANT / REVOKE without applying them and keeps no
//! access lists: this proves the requests and the statements' syntax, not
//! the permissions.

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, SecurityAction};

#[tokio::test]
#[ignore]
async fn dcl_and_access_lists() {
    let Ok(url) = std::env::var("DBINE_TEST_BIGQUERY_URL") else { return };
    let mut c = ConnectionConfig { driver: "bigquery".into(), ..Default::default() };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url);
    let d = dbine_driver_bigquery::drivers().pop().unwrap();

    let mut none = d.connect(&c, None).await.unwrap();
    assert!(none.principals().await.is_err(), "without a dataset there's nothing to list");

    let mut s = d.connect(&c, Some("ds1")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("CREATE TABLE IF NOT EXISTS sec_t (id INT64)", 10, &mut out).await.unwrap();
    let principals = s.principals().await.unwrap();
    for p in &principals {
        s.grants(&p.name).await.unwrap();
    }

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    for sql in [
        script(SecurityAction::Grant {
            privileges: vec!["roles/bigquery.dataViewer".into()],
            object: Some(ObjectRef { kind: "schema".into(), schema: None, name: "ds1".into() }),
            to: "user:ana@example.com".into(),
            grantable: false,
        }),
        script(SecurityAction::Revoke {
            privileges: vec!["roles/bigquery.dataViewer".into()],
            object: Some(ObjectRef { kind: "table".into(), schema: Some("ds1".into()), name: "sec_t".into() }),
            from: "group:equipo@example.com".into(),
        }),
    ] {
        let mut out = QueryOutcome::default();
        s.execute(&sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
