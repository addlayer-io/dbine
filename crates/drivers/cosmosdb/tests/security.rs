//! Users and permissions (resource tokens) against the emulator (see
//! tests/integration.rs):
//! `DBINE_TEST_COSMOSDB_URL=https://localhost:25203 cargo test -p dbine-driver-cosmosdb --test security -- --ignored`

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

const EMULATOR_KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

fn cfg(db: &str) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_COSMOSDB_URL").ok()?;
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    let mut c = ConnectionConfig { driver: "cosmosdb".into(), host: url, database: db.into(), trust_server_certificate: true, ..Default::default() };
    c.options.insert("account_key".into(), key);
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    out
}

#[tokio::test]
#[ignore]
async fn users_and_permissions() {
    let Some(c) = cfg("dbine_sec") else { return };
    let d = dbine_driver_cosmosdb::drivers().remove(0);
    assert!(d.security().is_some_and(|s| s.per_database));
    let mut admin = d.connect(&cfg("").unwrap(), None).await.unwrap();
    let _ = admin.drop_database("dbine_sec").await;
    admin.create_database("dbine_sec").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_sec")).await.unwrap();
    run(&mut s, r#"CREATE CONTAINER "items" { "partitionKey": { "paths": ["/id"], "kind": "Hash" } }"#).await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "ana".into(), password: None })).await;
    let items = ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "items".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["READ".into()], object: Some(items.clone()), to: "ana".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    assert!(all.iter().any(|p| p.name == "ana" && p.kind == PrincipalKind::User), "{all:?}");
    let g = s.grants("ana").await.unwrap();
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!((g[0].privilege.as_str(), g[0].object.as_deref(), g[0].object_kind.as_deref()), ("READ", Some("items"), Some(kinds::COLLECTION)));

    // Granting again replaces the container's permission.
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["ALL".into()], object: Some(items.clone()), to: "ana".into(), grantable: false })).await;
    let g = s.grants("ana").await.unwrap();
    assert_eq!((g.len(), g[0].privilege.as_str()), (1, "ALL"), "{g:?}");

    let out = run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["READ".into()], object: Some(items.clone()), from: "ana".into() })).await;
    assert!(out.messages.iter().any(|m| m.contains("no se revocó")), "{:?}", out.messages);
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["ALL".into()], object: Some(items), from: "ana".into() })).await;
    assert!(s.grants("ana").await.unwrap().is_empty());

    run(&mut s, &script(SecurityAction::Drop { name: "ana".into(), kind: PrincipalKind::User })).await;
    assert!(s.principals().await.unwrap().is_empty());
    admin.drop_database("dbine_sec").await.unwrap();
}
