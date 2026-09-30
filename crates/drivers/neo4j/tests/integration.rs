//! Against real servers:
//! `docker run -d --name dbine-test-neo4j -p 17687:7687 -e NEO4J_AUTH=neo4j/dbine-test-pass neo4j:5`
//! `docker run -d --name dbine-test-memgraph -p 27687:7687 memgraph/memgraph`
//! then
//! `DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 DBINE_TEST_MEMGRAPH_URL=localhost:27687 cargo test -p dbine-driver-neo4j -- --ignored`.
//! URLs: `[user:password@]host:port`.

use dbine_driver::{kinds, ConnectionConfig, DdlParts, Error, ObjectRef, QueryOutcome, Session, TableSchema};
use serde_json::json;

fn cfg(driver: &str, url: &str, read_only: bool) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: user,
        password: pass,
        read_only,
        ..Default::default()
    }
}

fn driver(id: &str) -> std::sync::Arc<dyn dbine_driver::Driver> {
    dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn open(id: &str, url: &str, read_only: bool) -> Box<dyn Session> {
    driver(id).connect(&cfg(id, url, read_only), None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 100, &mut out).await {
        panic!("{text}: {e}");
    }
    out
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

async fn round_trip(id: &str, url: &str) {
    let d = driver(id);
    let mut s = open(id, url, false).await;
    println!("{id}: {}", s.server_version().await.unwrap());
    println!("{id} databases: {:?}", s.list_databases().await.unwrap());
    run(&mut s, "MATCH (n) DETACH DELETE n").await;
    let out = run(
        &mut s,
        "CREATE (a:DbineP {name: 'Ann', age: 30, born: date('1994-05-01')})-[:DBINE_KNOWS {since: 2020}]->(b:DbineP {name: 'Bob'});
         CREATE (:DbineP:DbineDev {name: 'Cy', tags: ['x', 'y'], score: 1.5});
         MATCH (p:DbineP) RETURN p.name AS name, p.age AS age, p.born AS born ORDER BY name;",
    )
    .await;
    assert_eq!(out.results.len(), 3, "{out:?}");
    assert!(out.results[0].rows_affected.unwrap() >= 3, "{out:?}");
    let r = &out.results[2];
    assert_eq!(r.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["name", "age", "born"]);
    assert_eq!(r.rows[0], vec![json!("Ann"), json!(30), json!("1994-05-01")]);

    // Empty result keeps its columns.
    let out = run(&mut s, "MATCH (p:DbineNothing) RETURN p.x AS x").await;
    assert_eq!(out.results[0].columns.len(), 1);
    assert!(out.results[0].rows.is_empty());

    // Explorer.
    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == "label" && o.name == "DbineP"), "{objs:?}");
    assert!(objs.iter().any(|o| o.kind == "relationship" && o.name == "DBINE_KNOWS"));
    assert!(objs.iter().any(|o| o.kind == kinds::PROCEDURE));
    let cols = s.columns(&obj("label", "DbineP")).await.unwrap();
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"name") && names.contains(&"tags"), "{cols:?}");
    assert!(!cols.iter().find(|c| c.name == "name").unwrap().nullable);
    let rc = s.columns(&obj("relationship", "DBINE_KNOWS")).await.unwrap();
    assert_eq!(rc[0].data_type, "INTEGER");

    // Browse: nodes as JSON.
    let q = s.browse_query(&obj("label", "DbineDev"), 10);
    let out = run(&mut s, &q).await;
    let cell = out.results[0].rows[0][0].as_str().unwrap().to_string();
    let v: serde_json::Value = serde_json::from_str(&cell).unwrap();
    assert_eq!(v["~entityType"], "node");
    assert_eq!(v["~properties"]["tags"], json!(["x", "y"]));
    let q = s.browse_query(&obj("relationship", "DBINE_KNOWS"), 10);
    let out = run(&mut s, &q).await;
    let rel: serde_json::Value = serde_json::from_str(out.results[0].rows[0][0].as_str().unwrap()).unwrap();
    assert_eq!(rel["~type"], "DBINE_KNOWS");

    // Paths.
    let out = run(&mut s, "MATCH p = (:DbineP)-[:DBINE_KNOWS]->(:DbineP) RETURN p").await;
    let p: serde_json::Value = serde_json::from_str(out.results[0].rows[0][0].as_str().unwrap()).unwrap();
    assert_eq!(p["~nodes"].as_array().unwrap().len(), 2);

    // Insert script round trip.
    let script = d.insert_script(&obj("label", "DbineDev"), &["n".into()], &[vec![json!(cell)]]).unwrap();
    assert!(script.starts_with("CREATE (:"), "{script}");
    run(&mut s, &script).await;
    let out = run(&mut s, "MATCH (n:DbineDev) RETURN count(n) AS c").await;
    assert_eq!(out.results[0].rows[0][0], json!(2));
    run(&mut s, "MATCH (n:DbineDev) WITH n ORDER BY id(n) SKIP 1 DETACH DELETE n").await;
    let script = d.insert_script(&obj("relationship", "DBINE_KNOWS"), &["r".into()], &[vec![json!(rel.to_string())]]).unwrap();
    run(&mut s, &script).await;
    let out = run(&mut s, "MATCH ()-[r:DBINE_KNOWS]->() RETURN count(r) AS c").await;
    assert_eq!(out.results[0].rows[0][0], json!(2));

    // Designer: index + constraint, then they're listed with a definition.
    let mut t = TableSchema {
        kind: kinds::INDEX.into(),
        name: "dbine_ix".into(),
        columns: vec![dbine_driver::ColumnDef { name: "age".into(), ..Default::default() }],
        ..Default::default()
    };
    t.options.insert("target".into(), "DbineP".into());
    t.options.insert("entity".into(), "node".into());
    t.options.insert("index_type".into(), "RANGE".into());
    let create = DdlParts { create: true, if_exists: true, ..Default::default() };
    let ddl = d.table_ddl(&t, create).unwrap();
    run(&mut s, &ddl).await;
    t.name = "dbine_uq".into();
    t.columns[0].name = "name".into();
    t.options.insert("index_type".into(), "UNIQUE".into());
    let ddl = d.table_ddl(&t, create).unwrap();
    run(&mut s, &ddl).await;
    let objs = s.list_objects().await.unwrap();
    let ix = objs.iter().find(|o| o.kind == kinds::INDEX && o.name.contains("age") || o.name == "dbine_ix").expect("index listed");
    let def = s.definition(&obj(&ix.kind, &ix.name)).await.unwrap().unwrap();
    assert!(def.contains("INDEX"), "{def}");
    let cs = objs.iter().find(|o| o.kind == "constraint").expect("constraint listed");
    let def = s.definition(&obj("constraint", &cs.name)).await.unwrap().unwrap();
    assert!(def.to_uppercase().contains("UNIQUE"), "{def}");
    let def = s.definition(&obj("label", "DbineP")).await.unwrap().unwrap();
    assert!(def.contains("nodos") && def.contains("UNIQUE"), "{def}");
    let schema = s.database_schema().await.unwrap();
    let p = schema.iter().find(|t| t.name == "DbineP").unwrap();
    assert!(p.indexes.len() >= 2, "{p:?}");
    let script = d.table_ddl(p, DdlParts { create: true, indexes: true, drop: true, if_exists: true, ..Default::default() }).unwrap();
    println!("{id} label script:\n{script}");
    // A constraint violation is the server's error, as a Query error.
    let mut out = QueryOutcome::default();
    let e = s.execute("CREATE (:DbineP {name: 'Ann'})", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(_)), "{e:?}");
    // The session still works after a failure.
    run(&mut s, "RETURN 1").await;
    // Drop what the designer made.
    let drop = DdlParts { drop: true, if_exists: true, ..Default::default() };
    run(&mut s, &d.table_ddl(&t, drop).unwrap()).await;
    t.name = "dbine_ix".into();
    t.columns[0].name = "age".into();
    t.options.insert("index_type".into(), "RANGE".into());
    run(&mut s, &d.table_ddl(&t, drop).unwrap()).await;

    // Plans.
    let mut out = QueryOutcome::default();
    s.explain("MATCH (p:DbineP)-[:DBINE_KNOWS]->(q) RETURN p, q", false, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1, "{:?}", out.messages);
    assert!(!out.plans[0].actual);
    println!("{id} plan root: {} children {}", out.plans[0].root.op, out.plans[0].root.children.len());
    assert!(!out.plans[0].root.children.is_empty());
    let mut out = QueryOutcome::default();
    s.explain("MATCH (p:DbineP) RETURN p.name", true, 10, &mut out).await.unwrap();
    assert!(out.plans[0].actual, "{:?}", out.plans);
    assert_eq!(out.results.last().unwrap().rows.len(), 3);
    // A plan from EXPLAIN typed in the editor (Neo4j sends it as metadata).
    let out = run(&mut s, "EXPLAIN MATCH (n) RETURN n").await;
    assert!(!out.plans.is_empty() || !out.results[0].rows.is_empty());

    // Read-only.
    let mut ro = open(id, url, true).await;
    run(&mut ro, "MATCH (n:DbineP) RETURN n.name").await;
    let mut out = QueryOutcome::default();
    assert!(ro.execute("CREATE (:X)", 10, &mut out).await.is_err());
    assert!(ro.execute("MATCH (n) SET n.x = 1", 10, &mut out).await.is_err());

    // Cancel a long query from another thread.
    let stop = s.interrupter().expect("interrupter");
    let t0 = std::time::Instant::now();
    let h = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        stop();
    });
    let mut out = QueryOutcome::default();
    let r = s.execute("UNWIND range(1, 2000000000) AS x WITH x WHERE x % 7 = 99 RETURN count(x)", 10, &mut out).await;
    h.await.unwrap();
    println!("{id} cancel: {r:?} after {:?}", t0.elapsed());
    assert!(r.is_err() && t0.elapsed().as_secs() < 30);
    run(&mut s, "RETURN 1").await;

    run(&mut s, "MATCH (n) DETACH DELETE n").await;
}

async fn monitor(id: &str, url: &str) {
    let mut s = open(id, url, false).await;
    let snap = s.monitor().await.unwrap();
    let with_value: Vec<String> = snap.metrics.iter().filter(|m| m.value.is_some()).map(|m| format!("{}={:?}", m.key, m.value)).collect();
    println!("{id} metrics: {with_value:?}");
    println!("{id} tables: {:?}", snap.tables.iter().map(|t| (t.key.as_str(), t.rows.len())).collect::<Vec<_>>());
    println!("{id} info: {:?}", snap.info);
    println!("{id} notes: {:?}", snap.notes);
    assert!(with_value.len() >= 4, "{with_value:?}");
    assert!(snap.tables.iter().any(|t| t.key == "sessions" && !t.rows.is_empty()));
    // A second snapshot on the same session works too.
    s.monitor().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn neo4j() {
    let url = std::env::var("DBINE_TEST_NEO4J_URL").expect("DBINE_TEST_NEO4J_URL");
    round_trip("neo4j", &url).await;
    // Community: CREATE DATABASE isn't there; the error says so.
    let mut s = open("neo4j", &url, false).await;
    match s.create_database("dbinetest").await {
        Ok(()) => s.drop_database("dbinetest").await.unwrap(),
        Err(e) => println!("create database: {e}"),
    }
}

#[tokio::test]
#[ignore]
async fn neo4j_monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_URL") else { return };
    monitor("neo4j", &url).await;
}

#[tokio::test]
#[ignore]
async fn memgraph() {
    let url = std::env::var("DBINE_TEST_MEMGRAPH_URL").expect("DBINE_TEST_MEMGRAPH_URL");
    round_trip("memgraph", &url).await;
}

#[tokio::test]
#[ignore]
async fn memgraph_monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_MEMGRAPH_URL") else { return };
    monitor("memgraph", &url).await;
}

#[tokio::test]
#[ignore]
async fn bad_password_is_auth_failed() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_URL") else { return };
    let host = url.rsplit_once('@').map_or(url.as_str(), |x| x.1);
    let r = driver("neo4j").connect(&cfg("neo4j", &format!("neo4j:wrong@{host}"), false), None).await;
    assert!(matches!(r, Err(Error::AuthFailed(_))), "{:?}", r.err());
}

// -- profiler -----------------------------------------------------------------------------

/// The profiler sees another session's slow query once, with its duration,
/// and leaves out its own.
async fn profile(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    assert!(driver(id).supports_profiler(), "{id}");
    let mut p = open(id, &url, true).await;
    let mut w = open(id, &url, false).await;
    let database = if id == "neo4j" { "neo4j" } else { "" };
    let opts = dbine_driver::ProfilerOptions { database: database.into(), change_server: false };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{id}: {started:?}");
    let marker = format!("dbine_prof_{}", std::process::id());
    let n = 20_000_000;
    let slow = format!("UNWIND range(1, {n}) AS x WITH x WHERE x % 7 = 0 RETURN count(*) AS {marker}_slow");
    let work = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let t = std::time::Instant::now();
        run(&mut w, &slow).await;
        eprintln!("{id}: slow query took {:?}", t.elapsed());
        run(&mut w, &format!("RETURN 1 AS {marker}_fast")).await;
    };
    let watch = async {
        let mut got = Vec::new();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            if got.iter().any(|s| s.text.contains(&marker)) {
                break;
            }
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&format!("{marker}_slow"))).collect();
    eprintln!("{id}: {mine:#?}");
    assert_eq!(mine.len(), 1, "{id}: the slow query once");
    assert!(mine[0].duration_ms.unwrap_or(0.0) >= 200.0, "{id}: duration {:?}", mine[0].duration_ms);
    if id == "neo4j" {
        assert_eq!(mine[0].database.as_deref(), Some("neo4j"));
        assert_eq!(started.reads_unit.as_deref(), Some("páginas"));
        assert!(mine[0].reads.is_some(), "neo4j: page hits and faults");
    }
    assert!(got.iter().all(|s| !s.text.contains("SHOW TRANSACTIONS")), "{id}: its own queries are left out");
}

#[tokio::test]
#[ignore]
async fn neo4j_profiler() {
    profile("neo4j", "DBINE_TEST_NEO4J_URL").await;
}

#[tokio::test]
#[ignore]
async fn memgraph_profiler() {
    profile("memgraph", "DBINE_TEST_MEMGRAPH_URL").await;
}
