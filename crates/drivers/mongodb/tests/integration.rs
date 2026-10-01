//! Against a real server:
//!
//! ```sh
//! docker run -d --name dbine-test-mongodb -p 25201:27017 \
//!   -e MONGO_INITDB_ROOT_USERNAME=root -e MONGO_INITDB_ROOT_PASSWORD=secret mongo:7
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//!   cargo test -p dbine-driver-mongodb -- --ignored
//! docker rm -f dbine-test-mongodb
//! ```

use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome};

fn cfg(read_only: bool) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_MONGODB_URL").ok()?;
    let mut c = ConnectionConfig { driver: "mongodb".into(), database: "dbine_it".into(), read_only, ..Default::default() };
    c.options.insert("connection_string".into(), url);
    Some(c)
}

fn coll(name: &str) -> ObjectRef {
    ObjectRef { kind: "collection".into(), schema: None, name: name.into() }
}

#[tokio::test]
#[ignore]
async fn end_to_end() {
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_mongodb::drivers()[0];
    let mut s = d.connect(&c, None).await.expect("connect");
    assert!(s.server_version().await.unwrap().starts_with("MongoDB 7"));

    let mut out = QueryOutcome::default();
    s.execute("db.people.drop()", 10, &mut out).await.ok();
    s.execute("db.v_adults.drop()", 10, &mut out).await.ok();
    let mut out = QueryOutcome::default();
    s.execute(
        r#"db.people.insertMany([
             { _id: 1, name: 'Ana', age: 31, tags: ['a', 'b'], at: ISODate("2024-01-31T13:45:00Z") },
             { _id: 2, name: "Bruno", age: 25, addr: { city: 'Rosario' } },
             { _id: 3, name: 'Carla', age: 40, oid: ObjectId("65a1b2c3d4e5f60718293a4b") },
           ])"#,
        10,
        &mut out,
    )
    .await
    .expect("insert");
    assert_eq!(out.results[0].rows_affected, Some(3));

    let mut out = QueryOutcome::default();
    s.execute(
        "db.runCommand({ createIndexes: 'people', indexes: [{ key: { name: 1 }, name: 'name_1' }] })\n\
         db.runCommand({ create: 'v_adults', viewOn: 'people', pipeline: [{ $match: { age: { $gte: 30 } } }] })",
        10,
        &mut out,
    )
    .await
    .expect("index + view");

    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"dbine_it".to_string()), "{dbs:?}");
    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.name == "people" && o.kind == "collection"));
    assert!(objs.iter().any(|o| o.name == "v_adults" && o.kind == "view"));

    let cols = s.columns(&coll("people")).await.unwrap();
    assert_eq!(cols[0].name, "_id");
    let age = cols.iter().find(|c| c.name == "age").unwrap();
    assert_eq!(age.data_type, "int");
    assert!(!age.nullable);
    assert!(cols.iter().find(|c| c.name == "addr").unwrap().nullable);

    let def = s.definition(&coll("people")).await.unwrap().unwrap();
    assert!(def.contains("name_1"), "{def}");
    let vdef = s.definition(&ObjectRef { kind: "view".into(), schema: None, name: "v_adults".into() }).await.unwrap().unwrap();
    assert!(vdef.starts_with("db.createView(\"v_adults\", \"people\""), "{vdef}");

    // Browse, and the flattening of documents.
    let q = s.browse_query(&coll("people"), 50);
    let mut out = QueryOutcome::default();
    s.execute(&q, 100, &mut out).await.unwrap();
    let r = &out.results[0];
    let names: Vec<_> = r.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["_id", "name", "age", "tags", "at", "addr", "oid"]);
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0][3], serde_json::json!(r#"["a","b"]"#));
    assert_eq!(r.rows[0][4], serde_json::json!("2024-01-31 13:45:00"));
    assert_eq!(r.rows[2][6], serde_json::json!("65a1b2c3d4e5f60718293a4b"));

    // Shell syntax, several statements, raw command, max_rows.
    let mut out = QueryOutcome::default();
    s.execute(
        "db.people.find({ age: { $gt: 26 } }, { name: 1, _id: 0 })\n  .sort({ age: -1 })\n  .limit(5);\n\
         db.people.countDocuments({})\n\
         db.people.distinct('name')\n\
         { \"find\": \"people\", \"filter\": {}, \"sort\": {\"_id\": 1} }\n\
         db.people.aggregate([{ $group: { _id: null, avg: { $avg: '$age' } } }])\n\
         db.getCollectionNames()",
        2,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[0].rows, vec![vec![serde_json::json!("Carla")], vec![serde_json::json!("Ana")]]);
    assert_eq!(out.results[1].rows[0][0], serde_json::json!(3));
    assert_eq!(out.results[2].columns[0].name, "name");
    assert!(out.results[2].truncated);
    assert!(out.results[3].truncated);
    assert_eq!(out.results[3].rows.len(), 2);
    assert_eq!(out.results[4].rows.len(), 1);
    assert!(out.results[5].rows.len() <= 2);

    // Writes report affected counts.
    let mut out = QueryOutcome::default();
    s.execute("db.people.updateMany({ age: { $gt: 30 } }, { $set: { senior: true } }); db.people.deleteOne({ _id: 2 })", 10, &mut out)
        .await
        .unwrap();
    assert_eq!(out.results[0].rows_affected, Some(2));
    assert_eq!(out.results[1].rows_affected, Some(1));

    // Server errors and parse errors come back as query errors (with code and place).
    let mut out = QueryOutcome::default();
    let e = s.execute("db.people.aggregate([{ $nope: 1 }])", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    let e = s.execute("db.people.find({ a: })", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");

    // Cancel handle exists and doesn't panic with nothing running.
    let stop = s.interrupter().expect("interrupter");
    stop();
}

#[tokio::test]
#[ignore]
async fn read_only_blocks_writes() {
    let Some(c) = cfg(true) else { return };
    let d = &dbine_driver_mongodb::drivers()[0];
    let mut s = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    s.execute("db.people.find({}).limit(1); db.people.countDocuments({})", 10, &mut out).await.unwrap();
    for w in [
        "db.people.insertOne({ a: 1 })",
        "db.people.aggregate([{ $out: 'x' }])",
        "db.runCommand({ dropDatabase: 1 })",
        "{ \"delete\": \"people\", \"deletes\": [] }",
    ] {
        let e = s.execute(w, 10, &mut out).await.unwrap_err();
        assert!(e.to_string().contains("solo lectura"), "{w}: {e}");
    }
}

#[tokio::test]
#[ignore]
async fn bad_password_is_auth_failed() {
    let Some(url) = std::env::var("DBINE_TEST_MONGODB_URL").ok() else { return };
    let port: u16 = url.rsplit(':').next().and_then(|p| p.split('/').next()).and_then(|p| p.parse().ok()).unwrap_or(27017);
    let c = ConnectionConfig {
        driver: "mongodb".into(),
        host: "localhost".into(),
        port,
        username: Some("root".into()),
        password: Some("wrong".into()),
        ..Default::default()
    };
    let d = &dbine_driver_mongodb::drivers()[0];
    let e = d.connect(&c, None).await.err().expect("must fail");
    assert!(matches!(e, Error::AuthFailed(_)), "{e:?}");
    // And the typed fields work with the right password.
    let ok = ConnectionConfig { password: Some("secret".into()), ..c };
    d.connect(&ok, Some("admin")).await.expect("field-based connect");
}

#[tokio::test]
#[ignore]
async fn interrupter_kills_a_running_find() {
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_mongodb::drivers()[0];
    let mut s = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    s.execute("db.slow.drop(); db.slow.insertOne({ a: 1 })", 10, &mut out).await.ok();
    let stop = s.interrupter().expect("interrupter");
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        stop();
    });
    let t = std::time::Instant::now();
    let mut out = QueryOutcome::default();
    let r = s.execute("db.slow.find({ $where: 'sleep(8000) || true' })", 10, &mut out).await;
    assert!(r.is_err(), "{r:?}");
    assert!(t.elapsed().as_secs() < 5, "took {:?}", t.elapsed());
}

#[tokio::test]
#[ignore]
async fn explain_plans() {
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_mongodb::drivers()[0];
    assert!(d.supports_explain());
    let mut s = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    s.execute("db.plan_t.drop()", 10, &mut out).await.ok();
    let docs: Vec<String> = (0..500).map(|i| format!("{{ _id: {i}, n: {i}, g: {} }}", i % 5)).collect();
    s.execute(&format!("db.plan_t.insertMany([{}])", docs.join(",")), 10, &mut out).await.unwrap();

    // Estimated: nothing runs, not even the write.
    let mut out = QueryOutcome::default();
    s.explain("db.plan_t.find({ n: { $gt: 490 } }).sort({ g: 1 })\ndb.plan_t.deleteMany({ n: { $lt: 100 } })", false, 50, &mut out)
        .await
        .unwrap();
    assert!(out.results.is_empty());
    assert_eq!(out.plans.len(), 2);
    assert!(!out.plans[0].actual);
    let text = format!("{:?}", out.plans[0].root);
    assert!(text.contains("COLLSCAN: recorre toda la colección"), "{text}");
    assert_eq!(out.plans[1].root.op, "delete");
    let mut cnt = QueryOutcome::default();
    s.execute("db.plan_t.countDocuments({})", 10, &mut cnt).await.unwrap();
    assert_eq!(cnt.results[0].rows[0][0], serde_json::json!(500));

    // Actual: results + figures; the write runs exactly once.
    let mut out = QueryOutcome::default();
    s.explain(
        "db.plan_t.find({ n: 7 })\ndb.plan_t.aggregate([{ $match: { n: { $gte: 10 } } }, { $group: { _id: '$g', c: { $sum: 1 } } }, { $sort: { _id: 1 } }])\ndb.plan_t.updateMany({ g: 1 }, { $inc: { n: 1000 } })\ndb.plan_t.countDocuments({ n: { $gte: 1000 } })",
        true,
        50,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.plans.len(), 4, "{:?}", out.messages);
    assert!(out.plans.iter().all(|p| p.actual));
    assert_eq!(out.results[0].rows.len(), 1);
    assert_eq!(out.plans[0].root.actual_rows, Some(1.0));
    assert!(format!("{:?}", out.plans[0].root).contains("Examina 500"));
    assert_eq!(out.results[1].rows.len(), 5);
    assert_eq!(out.results[2].rows_affected, Some(100));
    assert_eq!(out.results[3].rows[0][0], serde_json::json!(100));
    for p in &out.plans {
        eprintln!("--- {}", p.statement);
        dump(&p.root, 0);
    }

    // With an index: IXSCAN, no COLLSCAN warning.
    let mut o = QueryOutcome::default();
    s.execute("db.runCommand({ createIndexes: 'plan_t', indexes: [{ key: { n: 1 }, name: 'n_1' }] })", 10, &mut o).await.unwrap();
    let mut out = QueryOutcome::default();
    s.explain("db.plan_t.find({ n: 7 })", true, 50, &mut out).await.unwrap();
    let t = format!("{:?}", out.plans[0].root);
    assert!(t.contains("IXSCAN") && t.contains("n_1") && !t.contains("COLLSCAN"), "{t}");
    let mut out = QueryOutcome::default();
    s.explain("db.plan_t.insertOne({ _id: 9999 })", false, 50, &mut out).await.unwrap();
    assert!(out.plans.is_empty() && out.messages.len() == 1);
    let mut cnt = QueryOutcome::default();
    s.execute("db.plan_t.countDocuments({ _id: 9999 })", 10, &mut cnt).await.unwrap();
    assert_eq!(cnt.results[0].rows[0][0], serde_json::json!(0));
}

fn dump(n: &dbine_driver::PlanNode, depth: usize) {
    eprintln!("{}{} [{}] {:?} rows={:?} ms={:?} {:?}", "  ".repeat(depth), n.op, n.detail, n.object, n.actual_rows, n.actual_ms, n.warnings);
    for c in &n.children {
        dump(c, depth + 1);
    }
}

#[tokio::test]
#[ignore]
async fn designer_scripts_and_databases() {
    use dbine_driver::{ColumnDef, DdlParts, IndexDef, TableSchema};
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_mongodb::drivers()[0];
    assert!(d.capabilities().create_database && d.capabilities().drop_database && !d.capabilities().foreign_keys);
    let spec = d.designer().expect("designer");
    assert_eq!((spec.kind, spec.label), ("collection", "Nueva colección"));
    let mut s0 = d.connect(&c, None).await.expect("connect");

    // A fresh database, created and dropped through the session.
    let _ = s0.drop_database("dbine_ddl_it").await;
    s0.create_database("dbine_ddl_it").await.expect("create db");
    assert!(s0.list_databases().await.unwrap().contains(&"dbine_ddl_it".to_string()));
    assert!(s0.create_database("dbine_ddl_it").await.is_err());
    let mut s = d.connect(&c, Some("dbine_ddl_it")).await.expect("connect new db");

    // Designer → table_ddl → execute.
    let mut name = ColumnDef { name: "name".into(), data_type: "string".into(), nullable: false, comment: Some("Nombre".into()), ..Default::default() };
    name.options.insert("required".into(), "true".into());
    let t = TableSchema {
        kind: "collection".into(),
        name: "people".into(),
        columns: vec![name, ColumnDef { name: "age".into(), data_type: "int".into(), nullable: true, ..Default::default() }],
        indexes: vec![
            IndexDef { name: "name_age".into(), columns: vec!["name".into(), "age:-1".into()], unique: true, ..Default::default() },
            IndexDef { name: "bio_text".into(), columns: vec!["bio".into()], kind: Some("text".into()), ..Default::default() },
            IndexDef { name: "at_ttl".into(), columns: vec!["at".into()], kind: Some("ttl:3600".into()), filter: Some("{ age: { $gt: 18 } }".into()), ..Default::default() },
        ],
        options: [("validationLevel", "strict")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    };
    let parts = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
    let text = d.table_ddl(&t, parts).unwrap();
    eprintln!("{text}");
    let mut out = QueryOutcome::default();
    s.execute(&text, 10, &mut out).await.expect("ddl runs");
    // Again: drop tolerates, then the same create.
    s.execute(&text, 10, &mut out).await.expect("ddl runs twice");
    // ifNotExists extension.
    let guarded = d.table_ddl(&t, DdlParts { if_exists: true, create: true, ..Default::default() }).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&guarded, 10, &mut out).await.expect("guarded create");
    // (7.0 accepts an identical create; with other options it's NamespaceExists.)
    let mut out = QueryOutcome::default();
    assert!(s.execute("db.createCollection('people', { capped: true, size: 4096 })", 10, &mut out).await.is_err());
    s.execute("db.createCollection('people', { capped: true, size: 4096 }, { ifNotExists: true })", 10, &mut out).await.expect("guard");
    assert!(out.messages.iter().any(|m| m.contains("ya existía")), "{:?}", out.messages);

    // The validator applies.
    let mut out = QueryOutcome::default();
    assert!(s.execute("db.people.insertOne({ age: 3 })", 10, &mut out).await.is_err());

    // Time-series, capped/clustered, and a view via the templates.
    let mut ts = TableSchema { name: "metrics".into(), ..Default::default() };
    for (k, v) in [("timeField", "ts"), ("metaField", "sensor"), ("granularity", "minutes"), ("expireAfterSeconds", "86400")] {
        ts.options.insert(k.into(), v.into());
    }
    let mut cl = TableSchema { name: "clustered_log".into(), ..Default::default() };
    cl.options.insert("clustered".into(), "true".into());
    cl.options.insert("expireAfterSeconds".into(), "600".into());
    let mut out = QueryOutcome::default();
    for t in [&ts, &cl] {
        s.execute(&d.table_ddl(t, DdlParts { create: true, ..Default::default() }).unwrap(), 10, &mut out).await.expect("special collection");
    }
    s.execute("db.createCollection('tpl_target')", 10, &mut out).await.unwrap();
    for (i, tpl) in d.create_templates().iter().enumerate() {
        let obj = if tpl.label.contains("validador") || tpl.kind == "index" { "tpl_target".to_string() } else { format!("tpl_{i}") };
        let text = tpl.template.replace("{name}", &obj).replace("{schema}", "");
        let mut out = QueryOutcome::default();
        s.execute(&text, 10, &mut out).await.unwrap_or_else(|e| panic!("{}: {e}\n{text}", tpl.label));
    }

    // Insert script → execute → rows.
    let target = coll("people");
    let cols = vec!["_id".to_string(), "name".into(), "age".into()];
    let rows: Vec<Vec<serde_json::Value>> = (0..150)
        .map(|i| vec![serde_json::json!(format!("65a1b2c3d4e5f607182a{:04x}", i)), serde_json::json!(format!("P{i}")), if i % 3 == 0 { serde_json::Value::Null } else { serde_json::json!(i) }])
        .collect();
    let script = d.insert_script(&target, &cols, &rows).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&script, 10, &mut out).await.expect("insert script");
    assert_eq!(out.results.iter().filter_map(|r| r.rows_affected).sum::<u64>(), 150);
    let mut out = QueryOutcome::default();
    s.execute("db.people.countDocuments({ _id: { $type: 'objectId' } })", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(150));

    // database_schema reads it back and regenerates the same indexes.
    let schema = s.database_schema().await.unwrap();
    let names: Vec<_> = schema.iter().map(|t| (t.name.as_str(), t.kind.as_str())).collect();
    eprintln!("{names:?}");
    let p = schema.iter().find(|t| t.name == "people").unwrap();
    assert_eq!(p.primary_key.as_ref().unwrap().columns, vec!["_id"]);
    assert!(p.columns.iter().any(|c| c.name == "name" && c.comment.as_deref() == Some("Nombre")));
    assert!(p.checks.iter().any(|c| c.expression.contains("$jsonSchema")), "{:?}", p.checks);
    let mut want: Vec<_> = t.indexes.clone();
    want.sort_by(|a, b| a.name.cmp(&b.name));
    let mut got: Vec<_> = p.indexes.iter().filter(|i| i.name != "campo_1_fecha_-1").cloned().collect();
    got.sort_by(|a, b| a.name.cmp(&b.name));
    // The same `createIndex` (the kind `text` reads back as FULLTEXT, `ttl:` as an option).
    let ix_ddl = |i: &IndexDef| d.table_ddl(&TableSchema { name: "p".into(), indexes: vec![i.clone()], ..Default::default() }, DdlParts { indexes: true, ..Default::default() }).unwrap();
    let norm = |v: &[IndexDef]| v.iter().map(ix_ddl).collect::<Vec<_>>();
    assert_eq!(norm(&got), norm(&want));
    assert_eq!(got.iter().find(|i| i.name == "bio_text").unwrap().columns, vec!["bio"]);
    let m = schema.iter().find(|t| t.name == "metrics").unwrap();
    assert_eq!(m.options.get("timeField").map(String::as_str), Some("ts"));
    assert_eq!(m.options.get("expireAfterSeconds").map(String::as_str), Some("86400"));
    assert_eq!(schema.iter().find(|t| t.name == "clustered_log").unwrap().options.get("clustered").map(String::as_str), Some("true"));
    let v = schema.iter().find(|t| t.kind == "view").expect("a view");
    assert!(v.options.contains_key("viewOn"));

    // The whole database as a script, run into another database.
    let mut script = Vec::new();
    for t in &schema {
        script.push(d.table_ddl(t, DdlParts { drop: true, create: true, indexes: true, ..Default::default() }).unwrap());
    }
    let _ = s0.drop_database("dbine_ddl_it2").await;
    let mut s2 = d.connect(&c, Some("dbine_ddl_it2")).await.unwrap();
    let mut out = QueryOutcome::default();
    s2.execute(&script.join("\n"), 10, &mut out).await.expect("regenerated script");
    let again = s2.database_schema().await.unwrap();
    assert_eq!(again.len(), schema.len());
    let p2 = again.iter().find(|t| t.name == "people").unwrap();
    assert_eq!(norm(&p2.indexes), norm(&p.indexes));

    // Read-only sessions refuse DDL and database operations.
    let ro = ConnectionConfig { read_only: true, ..c.clone() };
    let mut r = d.connect(&ro, Some("dbine_ddl_it")).await.unwrap();
    let mut out = QueryOutcome::default();
    assert!(r.execute(&text, 10, &mut out).await.unwrap_err().to_string().contains("solo lectura"));
    assert!(r.drop_database("dbine_ddl_it").await.is_err());
    assert!(s0.drop_database("admin").await.is_err());

    s0.drop_database("dbine_ddl_it").await.expect("drop db");
    s0.drop_database("dbine_ddl_it2").await.expect("drop db2");
    assert!(!s0.list_databases().await.unwrap().contains(&"dbine_ddl_it".to_string()));
}

async fn check_monitor(driver: usize, c: ConnectionConfig) -> dbine_driver::MonitorSnapshot {
    let d = &dbine_driver_mongodb::drivers()[driver];
    assert!(d.capabilities().monitor);
    let mut s = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    s.execute("db.monitor_probe.insertOne({ a: 1 })", 10, &mut out).await.unwrap();
    let snap = s.monitor().await.expect("monitor");
    let has = |k: &str| snap.metrics.iter().any(|m| m.key == k && m.value.is_some());
    assert!(has("queries") && has("uptime") && has("storage_used"), "{:?}", snap.metrics);
    assert!(snap.tables.iter().any(|t| t.key == "databases" && !t.rows.is_empty()));
    assert!(snap.tables.iter().any(|t| t.key == "sessions"));
    assert!(snap.info.iter().any(|(k, _)| k == "Versión"));
    for m in &snap.metrics {
        eprintln!("{:<22} {:?} max={:?} counter={}", m.key, m.value, m.max, m.counter);
    }
    for t in &snap.tables {
        eprintln!("table {} rows={}", t.key, t.rows.len());
    }
    eprintln!("info {:?}\nnotes {:?}", snap.info, snap.notes);
    snap
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(c) = cfg(false) else { return };
    let snap = check_monitor(0, c).await;
    let has = |k: &str| snap.metrics.iter().any(|m| m.key == k && m.value.is_some());
    for k in ["mem_used", "mem_cache", "cache_hit", "connections", "net_in", "net_out", "rows_read"] {
        assert!(has(k), "{k}");
    }
    assert!(snap.tables.iter().any(|t| t.key == "top_objects" && !t.rows.is_empty()));
}

/// FerretDB 2 (`ghcr.io/ferretdb/ferretdb-eval:2`):
/// `docker run -d --name dbine-test-ferretdb -p 25203:27017 -e POSTGRES_USER=root -e POSTGRES_PASSWORD=secret ghcr.io/ferretdb/ferretdb-eval:2`,
/// then `DBINE_TEST_FERRETDB_URL=mongodb://root:secret@localhost:25203/`.
#[tokio::test]
#[ignore]
async fn ferretdb() {
    let Ok(url) = std::env::var("DBINE_TEST_FERRETDB_URL") else { return };
    let mut c = ConnectionConfig { driver: "ferretdb".into(), database: "dbine_it".into(), ..Default::default() };
    c.options.insert("connection_string".into(), url);
    let d = &dbine_driver_mongodb::drivers()[1];
    let mut s = d.connect(&c, None).await.expect("connect");
    assert!(s.server_version().await.unwrap().starts_with("FerretDB 2"));
    let mut out = QueryOutcome::default();
    s.execute("db.people.drop()", 10, &mut out).await.unwrap();
    s.execute("db.people.insertMany([{ _id: 1, name: 'Ana', age: 31 }, { _id: 2, name: 'Bruno', age: 25 }])", 10, &mut out)
        .await
        .unwrap();
    let mut out = QueryOutcome::default();
    s.execute("db.people.find({ age: { $gt: 30 } })", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);
    let mut out = QueryOutcome::default();
    s.explain("db.people.find({ age: { $gt: 30 } })", true, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1);
    assert!(s.list_objects().await.unwrap().iter().any(|o| o.name == "people"));
    assert!(!s.database_schema().await.unwrap().is_empty());
    let snap = check_monitor(1, c).await;
    assert!(snap.info.iter().any(|(k, _)| k == "Versión de FerretDB"));
    assert!(snap.notes.iter().any(|n| n.contains("top")));
}

/// One session profiles `dbine_it`, the other runs a slow command and a fast
/// one carrying a unique marker: each is seen exactly once, the profiler's
/// own commands never, and the profiling level is back as it was at stop.
/// `change_server: false` with profiling off samples `currentOp`.
async fn profile(change_server: bool) {
    use std::time::{Duration, Instant};
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_mongodb::drivers()[0];
    assert!(d.supports_profiler());
    let mut p = d.connect(&c, None).await.expect("connect");
    let mut w = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    w.execute("db.prof.drop()", 10, &mut out).await.unwrap();
    w.execute("db.prof.insertOne({ _id: 1 })", 10, &mut out).await.unwrap();
    let mut before = QueryOutcome::default();
    w.execute("db.runCommand({ profile: -1 })", 10, &mut before).await.unwrap();
    let opts = dbine_driver::ProfilerOptions { database: "dbine_it".into(), change_server };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{started:?}");
    let complete = started.mode == dbine_driver::ProfilerMode::Complete;
    assert_eq!(complete, change_server);
    let marker = format!("dbine_prof_{}", std::process::id());
    let slow = format!("db.prof.find({{ $where: 'sleep(600) || true', $comment: '{marker}_slow' }})");
    let fast = format!("db.prof.find({{ _id: 1, $comment: '{marker}_fast' }})");
    let work = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        w.execute(&slow, 10, &mut QueryOutcome::default()).await.expect("slow");
        tokio::time::sleep(Duration::from_millis(400)).await;
        w.execute(&fast, 10, &mut QueryOutcome::default()).await.expect("fast");
        tokio::time::sleep(Duration::from_millis(600)).await;
    };
    let watch = async {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(if complete { 6 } else { 4 });
        while Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            if complete {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{mine:#?}");
    let slow_seen: Vec<_> = mine.iter().filter(|s| s.text.contains("_slow")).collect();
    assert_eq!(slow_seen.len(), 1, "the slow command once");
    assert!(slow_seen[0].text.starts_with("db.prof.find("), "{}", slow_seen[0].text);
    assert!(slow_seen[0].duration_ms.unwrap_or(0.0) >= 300.0, "duration {:?}", slow_seen[0].duration_ms);
    if complete {
        assert_eq!(mine.iter().filter(|s| s.text.contains("_fast")).count(), 1, "the fast command once");
        assert_eq!(slow_seen[0].database.as_deref(), Some("dbine_it"));
        // The $where scans the collection's one document and writes nothing.
        assert_eq!(started.reads_unit.as_deref(), Some("documentos"));
        assert_eq!((slow_seen[0].reads, slow_seen[0].writes), (Some(1), None));
    }
    assert!(got.iter().all(|s| !s.text.contains("system.profile") && !s.text.contains("currentOp")), "own commands left out");
    let mut after = QueryOutcome::default();
    w.execute("db.runCommand({ profile: -1 })", 10, &mut after).await.unwrap();
    assert_eq!(before.results[0].rows[0][0], after.results[0].rows[0][0], "profiling level restored");
}

#[tokio::test]
#[ignore]
async fn profiler() {
    profile(true).await;
}

#[tokio::test]
#[ignore]
async fn profiler_read_only_samples() {
    profile(false).await;
}

/// Schema sync: a collection read back from `database_schema` gets a
/// validator and an index swapped, another is dropped and one is created.
#[tokio::test]
#[ignore]
async fn schema_sync() {
    use dbine_driver::{IndexDef, TableChange};
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_mongodb::drivers()[0];
    assert!(d.supports_schema_sync());
    let mut s = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    for t in ["sync_users", "sync_gone", "sync_fresh"] {
        s.execute(&format!("db.{t}.drop()"), 10, &mut out).await.ok();
    }
    s.execute("db.sync_users.insertOne({ name: 'Ana', age: 31, legacy: 1 })", 10, &mut out).await.unwrap();
    s.execute("db.sync_users.createIndex({ legacy: 1 }, { name: 'legacy_1' })", 10, &mut out).await.unwrap();
    s.execute("db.sync_gone.insertOne({ x: 1 })", 10, &mut out).await.unwrap();

    let schema = s.database_schema().await.unwrap();
    let old = schema.iter().find(|t| t.name == "sync_users").unwrap().clone();
    let gone = schema.iter().find(|t| t.name == "sync_gone").unwrap().clone();
    let mut new = old.clone();
    new.indexes = vec![IndexDef { name: "name_1".into(), columns: vec!["name".into()], unique: true, ..Default::default() }];
    new.options.insert("validator".into(), r#"{ "$jsonSchema": { "required": ["name"] } }"#.into());
    new.columns.retain(|c| c.name != "legacy");
    let mut created = old.clone();
    created.name = "sync_fresh".into();

    let script = d
        .sync_script(&[TableChange::Alter { old, new }, TableChange::Drop { table: gone }, TableChange::Create { table: created }])
        .unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        let mut out = QueryOutcome::default();
        s.execute(st, 10, &mut out).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    let after = s.database_schema().await.unwrap();
    assert!(!after.iter().any(|t| t.name == "sync_gone"));
    let fresh = after.iter().find(|t| t.name == "sync_fresh").unwrap();
    assert_eq!(fresh.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["legacy_1"]);
    let users = after.iter().find(|t| t.name == "sync_users").unwrap();
    assert_eq!(users.indexes.iter().map(|i| (i.name.as_str(), i.unique)).collect::<Vec<_>>(), vec![("name_1", true)]);
    assert!(users.checks.iter().any(|c| c.expression.contains("required")), "{:?}", users.checks);
    // The validator now rejects documents without a name.
    let mut out = QueryOutcome::default();
    assert!(s.execute("db.sync_users.insertOne({ age: 1 })", 10, &mut out).await.is_err());
    for t in ["sync_users", "sync_fresh"] {
        s.execute(&format!("db.{t}.drop()"), 10, &mut out).await.ok();
    }
}

/// The data-compare delete script removes exactly the keyed documents
/// (by `ObjectId` `_id`, and by other key fields with quotes).
#[tokio::test]
#[ignore]
async fn delete_script_runs() {
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_mongodb::drivers()[0];
    let mut s = d.connect(&c, None).await.expect("connect");
    let mut out = QueryOutcome::default();
    s.execute("db.dbine_del.drop()", 10, &mut out).await.ok();
    let mut out = QueryOutcome::default();
    s.execute(
        r#"db.dbine_del.insertMany([
             { _id: ObjectId("65a1b2c3d4e5f60718293a00"), n: 1 },
             { _id: 2, code: "O'Brien \"Bob\"", k: 1 },
             { _id: 3, code: "O'Brien \"Bob\"", k: 2 },
             { _id: 4, n: 4 },
           ])"#,
        10,
        &mut out,
    )
    .await
    .expect("insert");
    let keys = vec![
        vec![("_id".to_string(), serde_json::json!("65a1b2c3d4e5f60718293a00"))],
        vec![("code".to_string(), serde_json::json!("O'Brien \"Bob\"")), ("k".to_string(), serde_json::json!(2))],
    ];
    let script = d.delete_script(&coll("dbine_del"), &keys).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&script, 10, &mut out).await.expect(&script);
    let mut out = QueryOutcome::default();
    s.execute("db.dbine_del.find({}, { _id: 1 })", 10, &mut out).await.unwrap();
    let rows = serde_json::to_string(&out.results[0].rows).unwrap();
    assert_eq!(out.results[0].rows.len(), 2, "{rows}");
    assert!(rows.contains('2') && rows.contains('4'), "{rows}");
    let mut out = QueryOutcome::default();
    s.execute("db.dbine_del.drop()", 10, &mut out).await.ok();
}
