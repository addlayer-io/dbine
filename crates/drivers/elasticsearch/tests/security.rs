//! Users, roles and permissions against real servers with security on
//! (ignored by default):
//!
//! ```sh
//! docker run -d --name dbine-test-elasticsearch-auth -p 27140:9200 -e discovery.type=single-node \
//!   -e ELASTIC_PASSWORD=Dbine_pw1 -e xpack.security.http.ssl.enabled=false \
//!   -e "ES_JAVA_OPTS=-Xms512m -Xmx512m" docker.elastic.co/elasticsearch/elasticsearch:8.15.3
//! docker run -d --name dbine-test-opensearch-auth -p 27141:9200 -e discovery.type=single-node \
//!   -e 'OPENSEARCH_INITIAL_ADMIN_PASSWORD=Dbine_Pw_2024!x' -e "OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m" \
//!   opensearchproject/opensearch:2.17.1
//! DBINE_TEST_ELASTICSEARCH_AUTH_URL=http://elastic:Dbine_pw1@localhost:27140 \
//! DBINE_TEST_OPENSEARCH_AUTH_URL='https://admin:Dbine_Pw_2024!x@localhost:27141' \
//!   cargo test -p dbine-driver-elasticsearch --test security -- --ignored
//! ```
//!
//! `DBINE_TEST_ELASTICSEARCH_URL` / `DBINE_TEST_OPENSEARCH_URL` (servers with
//! security off, see tests/integration.rs) check the error when there's none.

use dbine_driver::{ConnectionConfig, Driver, Error, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};
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

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn elasticsearch_users_and_roles() {
    let Some(cfg) = cfg("elasticsearch", "DBINE_TEST_ELASTICSEARCH_AUTH_URL") else { return };
    let d = driver("elasticsearch");
    let mut s = d.connect(&cfg, None).await.unwrap();
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    // Leftovers from a failed run.
    let mut out = QueryOutcome::default();
    let _ = s.execute("DELETE /_security/user/dbine_ana", 10, &mut out).await;
    let _ = s.execute("DELETE /_security/role/dbine_lectores", 10, &mut out).await;

    run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lectores".into() })).await;
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345!x".into()) })).await;
    // Roles are defined and assigned whole, from the console.
    run(
        &mut s,
        "PUT /_security/role/dbine_lectores\n{\"cluster\": [\"monitor\"], \"indices\": [{\"names\": [\"facturas-*\"], \"privileges\": [\"read\", \"view_index_metadata\"]}]}\n\nPUT /_security/user/dbine_ana\n{\"roles\": [\"dbine_lectores\"], \"full_name\": \"Ana\"}",
    )
    .await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!((ana.kind, ana.disabled, ana.system, ana.superuser), (PrincipalKind::User, Some(false), false, Some(false)));
    assert_eq!(ana.member_of, vec!["dbine_lectores".to_string()]);
    assert!(all.iter().any(|p| p.name == "elastic" && p.system && p.superuser == Some(true)));
    assert!(all.iter().any(|p| p.name == "dbine_lectores" && p.kind == PrincipalKind::Role && !p.system));
    assert!(all.iter().any(|p| p.name == "superuser" && p.kind == PrincipalKind::Role && p.system));

    let g = s.grants("dbine_ana").await.unwrap();
    let read = g.iter().find(|x| x.privilege == "read").expect("read through the role");
    assert_eq!((read.object.as_deref(), read.object_kind.as_deref(), read.via.as_deref()), (Some("facturas-*"), Some("index"), Some("dbine_lectores")));
    assert!(g.iter().any(|x| x.privilege == "monitor" && x.object.is_none()));
    assert!(s.grants("dbine_lectores").await.unwrap().iter().all(|x| x.via.is_none()));

    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987!".into() })).await;
    let as_ana = ConnectionConfig { username: Some("dbine_ana".into()), password: Some("Otra_Pw_987!".into()), ..cfg.clone() };
    d.connect(&as_ana, None).await.expect("the new password works");
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().disabled, Some(true));
    assert!(d.connect(&as_ana, None).await.is_err(), "a disabled user can't sign in");
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: true })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_lectores".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana" && p.name != "dbine_lectores"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opensearch_users_roles_and_grants() {
    let Some(cfg) = cfg("opensearch", "DBINE_TEST_OPENSEARCH_AUTH_URL") else { return };
    let d = driver("opensearch");
    let mut s = d.connect(&cfg, None).await.unwrap();
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    let api = "/_plugins/_security/api";
    let mut out = QueryOutcome::default();
    let _ = s.execute(&format!("DELETE {api}/internalusers/dbine_ana"), 10, &mut out).await;
    let _ = s.execute(&format!("DELETE {api}/rolesmapping/dbine_lectores"), 10, &mut out).await;
    let _ = s.execute(&format!("DELETE {api}/roles/dbine_lectores"), 10, &mut out).await;

    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345!xYz".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lectores".into() })).await;
    let index = ObjectRef { kind: "index".into(), schema: None, name: "facturas-*".into() };
    let read = script(SecurityAction::Grant { privileges: vec!["read".into(), "search".into()], object: Some(index), to: "dbine_lectores".into(), grantable: false });
    // JSON Patch can't append to an empty list: the error says how to add the first one.
    let e = s.execute(&read, 10, &mut out).await.unwrap_err().to_string();
    assert!(e.contains("lista vacía") && e.contains("/index_permissions"), "{e}");
    run(&mut s, &format!("PATCH {api}/roles/dbine_lectores\n[{{\"op\": \"add\", \"path\": \"/index_permissions\", \"value\": [{{\"index_patterns\": [\"otros\"], \"allowed_actions\": [\"get\"]}}]}}, {{\"op\": \"add\", \"path\": \"/cluster_permissions\", \"value\": [\"cluster_composite_ops_ro\"]}}]")).await;
    run(&mut s, &read).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["cluster_monitor".into()], object: None, to: "dbine_lectores".into(), grantable: false })).await;
    run(&mut s, &format!("PUT {api}/rolesmapping/dbine_lectores\n{{\"users\": [\"dbine_otro\"]}}")).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "dbine_lectores".into(), member: "dbine_ana".into() })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!((ana.kind, ana.system, ana.superuser), (PrincipalKind::User, false, Some(false)));
    assert_eq!(ana.member_of, vec!["dbine_lectores".to_string()]);
    assert!(all.iter().any(|p| p.name == "admin" && p.superuser == Some(true)));
    assert!(all.iter().any(|p| p.name == "all_access" && p.kind == PrincipalKind::Role && p.system));

    let g = s.grants("dbine_ana").await.unwrap();
    let search = g.iter().find(|x| x.privilege == "search").expect("search through the role");
    assert_eq!((search.object.as_deref(), search.object_kind.as_deref(), search.via.as_deref()), (Some("facturas-*"), Some("index"), Some("dbine_lectores")));
    assert!(g.iter().any(|x| x.privilege == "cluster_monitor" && x.object.is_none()));
    // Appended: what the role had is still there.
    assert!(g.iter().any(|x| x.privilege == "get" && x.object.as_deref() == Some("otros")));
    assert!(g.iter().any(|x| x.privilege == "cluster_composite_ops_ro"));

    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987!xYz".into() })).await;
    let as_ana = ConnectionConfig { username: Some("dbine_ana".into()), password: Some("Otra_Pw_987!xYz".into()), ..cfg.clone() };
    d.connect(&as_ana, None).await.expect("the new password works");
    // The user keeps its role after the password change.
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().member_of, vec!["dbine_lectores".to_string()]);

    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &format!("DELETE {api}/rolesmapping/dbine_lectores")).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_lectores".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana" && p.name != "dbine_lectores"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn without_security_says_so() {
    for (id, var) in [("elasticsearch", "DBINE_TEST_ELASTICSEARCH_URL"), ("opensearch", "DBINE_TEST_OPENSEARCH_URL")] {
        let Some(cfg) = cfg(id, var) else { continue };
        let mut s = driver(id).connect(&cfg, None).await.unwrap();
        match s.principals().await {
            Err(Error::Unsupported(m)) => assert!(m.contains("deshabilitad"), "{m}"),
            other => panic!("{id}: {other:?}"),
        }
    }
}
