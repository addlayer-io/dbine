//! Users and permissions against a standalone Solr with security.json
//! (ignored by default):
//!
//! ```sh
//! docker create --name dbine-test-solr-sec -p 25524:8983 solr:9 solr-precreate testcore
//! docker start dbine-test-solr-sec   # wait until it answers, so /var/solr/data exists
//! docker cp security.json dbine-test-solr-sec:/var/solr/data/security.json && docker restart dbine-test-solr-sec
//! DBINE_TEST_SOLR_SECURE_URL=http://solr:SolrRocks@localhost:25524 cargo test -p dbine-driver-solr --test security -- --ignored
//! ```
//!
//! security.json: `solr.BasicAuthPlugin` with user `solr` / `SolrRocks`
//! (`"credentials": {"solr": "IV0EHq1OnNrj6gvRCwvFwTrZ1+z1oBbnQdiVC3otuq0= Ndd7LKvVBAaZIF0QAVi1ekCfAJXr1GGfLtRUXhgrF8c="}`)
//! and `solr.RuleBasedAuthorizationPlugin` with `"user-role": {"solr": "admin"}`
//! and the permissions `security-edit` and `all` for `admin` and `read` on
//! `testcore` for `["lector", "admin"]`.

use dbine_driver::{ConnectionConfig, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg(url: &str, user: &str, pass: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let hostport = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let (host, port) = hostport.trim_end_matches('/').rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "solr".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

#[tokio::test]
#[ignore]
async fn users_and_permissions() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLR_SECURE_URL") else { return };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, _) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let d = dbine_driver_solr::drivers().pop().unwrap();
    let mut s = d.connect(&cfg(&url, user, pass), None).await.unwrap();

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "ana b".into(), password: Some("p\"w 1".into()) })).await;
    run(&mut s, "POST /solr/admin/authorization\n{\"set-user-role\": {\"ana b\": [\"lector\"]}}").await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "ana b").unwrap();
    assert_eq!((ana.kind, ana.can_login, ana.superuser), (PrincipalKind::User, Some(true), Some(false)));
    assert_eq!(ana.member_of, ["lector"]);
    assert!(all.iter().any(|p| p.name == user && p.superuser == Some(true)));
    assert!(all.iter().any(|p| p.name == "lector" && p.kind == PrincipalKind::Role));

    let g = s.grants("ana b").await.unwrap();
    assert!(g.iter().any(|g| g.privilege == "read" && g.object.as_deref() == Some("testcore") && g.via.as_deref() == Some("lector")), "{g:?}");
    let g = s.grants("admin").await.unwrap();
    assert!(g.iter().any(|g| g.privilege == "all" && g.object.is_none() && g.via.is_none()), "{g:?}");

    // The password signs in: 401 is a wrong password; 403 (the `all`
    // permission, first in the list, is admin's) means it was right.
    let select = format!("http://{}/solr/testcore/select?q=*:*", rest.rsplit_once('@').unwrap().1);
    let status = |p: &'static str| {
        let select = select.clone();
        async move { reqwest::Client::new().get(select).basic_auth("ana b", Some(p)).send().await.unwrap().status().as_u16() }
    };
    assert_ne!(status("p\"w 1").await, 401);
    run(&mut s, &script(SecurityAction::SetPassword { name: "ana b".into(), password: "nueva".into() })).await;
    assert_ne!(status("nueva").await, 401);
    assert_eq!(status("p\"w 1").await, 401);

    run(&mut s, &script(SecurityAction::Drop { name: "ana b".into(), kind: PrincipalKind::User })).await;
    let all = s.principals().await.unwrap();
    assert!(!all.iter().any(|p| p.name == "ana b"));
    assert!(all.iter().any(|p| p.name == "lector"), "still named by a permission");
}
