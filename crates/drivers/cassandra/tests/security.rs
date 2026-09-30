//! Users, roles and permissions against a real server with authentication
//! on (the default image has it off):
//! `docker run -d --name dbine-test-cassandra-auth -p 27121:9042 --entrypoint bash cassandra:5 -c "sed -i 's/^authenticator: .*/authenticator: PasswordAuthenticator/; s/^authorizer: .*/authorizer: CassandraAuthorizer/' /etc/cassandra/cassandra.yaml && exec docker-entrypoint.sh cassandra -f"`
//! `docker run -d --name dbine-test-scylla-auth -p 27125:9042 scylladb/scylla --smp 1 --memory 750M --overprovisioned 1 --developer-mode 1 --authenticator PasswordAuthenticator --authorizer CassandraAuthorizer --auth-superuser-name cassandra --auth-superuser-salted-password '\$6\$dbinetestsalt\$ycrO1BcE5XqSXEciS1PTec4kiP4ukmyB1.PfTT/HyX8VSHhEdldJDAWxhgHt/RQU4SbJWJNH.E9wHMTKtEIXd/'`
//! (Scylla 2026.x no longer creates the default `cassandra` superuser; the hash is
//! `openssl passwd -6 -salt dbinetestsalt cassandra`, and the image's entrypoint runs
//! its arguments through a shell, so each `$` needs the `\` inside the quotes.)
//! then `DBINE_TEST_CASSANDRA_AUTH_URL=cassandra:cassandra@localhost:27121 DBINE_TEST_SCYLLADB_AUTH_URL=cassandra:cassandra@localhost:27125 cargo test -p dbine-driver-cassandra --test security -- --ignored`.
//! `DBINE_TEST_CASSANDRA_URL` (tests/integration.rs, authentication off)
//! checks the explanation `principals()` gives.

use dbine_driver::{ConnectionConfig, Error, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg(id: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig { driver: id.into(), host: host.into(), port: port.parse().unwrap(), username: user, password: pass, ..Default::default() }
}

fn driver(id: &str) -> std::sync::Arc<dyn dbine_driver::Driver> {
    dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cassandra() {
    if let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_AUTH_URL") {
        roles_and_permissions("cassandra", &url).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn scylladb() {
    if let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_AUTH_URL") {
        roles_and_permissions("scylladb", &url).await;
    }
}

async fn roles_and_permissions(id: &str, url: &str) {
    let d = driver(id);
    let mut admin = d.connect(&cfg(id, url), None).await.unwrap();
    run(&mut admin, "DROP ROLE IF EXISTS dbine_ana; DROP ROLE IF EXISTS dbine_lect; DROP KEYSPACE IF EXISTS dbine_sec;").await;
    run(&mut admin, "CREATE KEYSPACE dbine_sec WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}; CREATE TABLE dbine_sec.facturas (id int PRIMARY KEY, total decimal);").await;
    // As the UI does: a session on the keyspace.
    let mut s = d.connect(&cfg(id, url), Some("dbine_sec")).await.unwrap();

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345'x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lect".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "dbine_lect".into(), member: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["SELECT".into(), "CREATE".into()], object: None, to: "dbine_lect".into(), grantable: false })).await;
    // Unqualified: the session's keyspace.
    let table = ObjectRef { kind: "table".into(), schema: None, name: "facturas".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["MODIFY".into()], object: Some(table), to: "dbine_ana".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!((ana.kind, ana.can_login, ana.superuser, ana.disabled), (PrincipalKind::User, Some(true), Some(false), Some(false)));
    assert_eq!(ana.member_of, vec!["dbine_lect".to_string()]);
    assert!(all.iter().any(|p| p.name == "dbine_lect" && p.kind == PrincipalKind::Role));
    assert!(all.iter().any(|p| p.name == "cassandra" && p.system && p.superuser == Some(true)));

    let g = s.grants("dbine_ana").await.unwrap();
    println!("{g:#?}");
    let m = g.iter().find(|x| x.privilege == "MODIFY").expect("direct MODIFY");
    assert_eq!((m.object.as_deref(), m.object_kind.as_deref(), m.via.as_deref()), (Some("dbine_sec.facturas"), Some("table"), None));
    let sel = g.iter().find(|x| x.privilege == "SELECT").expect("SELECT through the role");
    assert_eq!((sel.object.as_deref(), sel.via.as_deref()), (None, Some("dbine_lect")));

    // Revoke as the UI does: the grant's object split at the first dot.
    let table = ObjectRef { kind: "table".into(), schema: Some("dbine_sec".into()), name: "facturas".into() };
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["MODIFY".into()], object: Some(table), from: "dbine_ana".into() })).await;
    assert!(s.grants("dbine_ana").await.unwrap().iter().all(|x| x.privilege != "MODIFY"));
    // Cassandra 5 takes one password change per role every 5 s.
    tokio::time::sleep(std::time::Duration::from_millis(5500)).await;
    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987".into() })).await;
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await;
    let ana = s.principals().await.unwrap().into_iter().find(|p| p.name == "dbine_ana").unwrap();
    assert_eq!((ana.kind, ana.disabled), (PrincipalKind::User, Some(true)));
    run(&mut s, &script(SecurityAction::RemoveMember { role: "dbine_lect".into(), member: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_lect".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana" && p.name != "dbine_lect"));
    drop(s);
    run(&mut admin, "DROP KEYSPACE dbine_sec").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn without_authentication() {
    let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_URL") else { return };
    let mut s = driver("cassandra").connect(&cfg("cassandra", &url), None).await.unwrap();
    match s.principals().await {
        Err(Error::Unsupported(m)) => assert!(m.contains("PasswordAuthenticator"), "{m}"),
        other => panic!("{other:?}"),
    }
}
