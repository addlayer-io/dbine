//! Schema sync against real servers (same containers as `integration.rs`):
//! `DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 DBINE_TEST_MEMGRAPH_URL=localhost:27687
//!  cargo test -p dbine-driver-neo4j --test sync -- --ignored --nocapture`

use dbine_driver::{ConnectionConfig, IndexDef, QueryOutcome, TableChange};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), username: user, password: pass, ..Default::default() }
}

fn ix(name: &str, kind: &str, props: &[&str]) -> IndexDef {
    IndexDef { name: name.into(), columns: props.iter().map(|p| p.to_string()).collect(), unique: kind == "UNIQUE", kind: Some(kind.into()), filter: None, ..Default::default() }
}

async fn run(id: &str, env: &str, setup: &[&str], cleanup: &[&str]) {
    let Ok(url) = std::env::var(env) else { return };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let mut s = d.connect(&cfg(id, &url), None).await.unwrap();
    for q in setup {
        let _ = s.execute(q, 100, &mut QueryOutcome::default()).await;
    }
    let old = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "SyncPerson").unwrap();
    println!("{id} old: {:?}", old.indexes);
    let mut new = old.clone();
    let named = id == "neo4j";
    new.indexes.retain(|i| i.columns != ["gone"]);
    new.indexes.push(ix(if named { "sync_u_code" } else { "" }, "UNIQUE", &["code"]));
    new.indexes.push(ix(if named { "sync_ix_age" } else { "" }, "RANGE", &["age"]));
    let script = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
    println!("{id}: {script:#?}");
    for q in &script.statements {
        s.execute(q, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    let after = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "SyncPerson").unwrap();
    println!("{id} after: {:?}", after.indexes);
    let has = |p: &str, k: &str| after.indexes.iter().any(|i| i.columns == [p] && i.kind.as_deref() == Some(k));
    assert!(has("code", "UNIQUE") && has("age", "RANGE") && has("name", "RANGE"), "{:?}", after.indexes);
    assert!(!after.indexes.iter().any(|i| i.columns == ["gone"]));
    for q in cleanup {
        let _ = s.execute(q, 100, &mut QueryOutcome::default()).await;
    }
}

#[tokio::test]
#[ignore]
async fn neo4j_sync() {
    let clean = [
        "MATCH (n:SyncPerson) DETACH DELETE n",
        "DROP INDEX sync_ix_name IF EXISTS",
        "DROP INDEX sync_ix_gone IF EXISTS",
        "DROP INDEX sync_ix_age IF EXISTS",
        "DROP CONSTRAINT sync_u_code IF EXISTS",
    ];
    let mut setup = clean.to_vec();
    setup.extend([
        "CREATE (:SyncPerson {name: 'a', gone: 1, code: 'x', age: 3})",
        "CREATE INDEX sync_ix_name FOR (e:SyncPerson) ON (e.name)",
        "CREATE INDEX sync_ix_gone FOR (e:SyncPerson) ON (e.gone)",
    ]);
    run("neo4j", "DBINE_TEST_NEO4J_URL", &setup, &clean).await;
}

#[tokio::test]
#[ignore]
async fn memgraph_sync() {
    let clean = [
        "MATCH (n:SyncPerson) DETACH DELETE n",
        "DROP INDEX ON :SyncPerson(name)",
        "DROP INDEX ON :SyncPerson(gone)",
        "DROP INDEX ON :SyncPerson(age)",
        "DROP CONSTRAINT ON (n:SyncPerson) ASSERT n.code IS UNIQUE",
    ];
    let mut setup = clean.to_vec();
    setup.extend(["CREATE (:SyncPerson {name: 'a', gone: 1, code: 'x', age: 3})", "CREATE INDEX ON :SyncPerson(name)", "CREATE INDEX ON :SyncPerson(gone)"]);
    run("memgraph", "DBINE_TEST_MEMGRAPH_URL", &setup, &clean).await;
}
