//! Against a real server:
//!
//! ```sh
//! docker run -d --name dbine-test-couchdb -p 25202:5984 \
//!   -e COUCHDB_USER=admin -e COUCHDB_PASSWORD=secret couchdb:3
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb -- --ignored
//! docker rm -f dbine-test-couchdb
//! ```

use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome};
use serde_json::json;

/// `http://user:pass@host:port` → config.
fn cfg(read_only: bool) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_COUCHDB_URL").ok()?;
    let rest = url.strip_prefix("http://")?;
    let (auth, host) = rest.split_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (h, p) = host.trim_end_matches('/').split_once(':')?;
    Some(ConnectionConfig {
        driver: "couchdb".into(),
        host: h.into(),
        port: p.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        read_only,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn dbine_driver::Session>, q: &str, max: usize) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, max, &mut out).await.map(|_| out)
}

#[tokio::test]
#[ignore]
async fn end_to_end() {
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_couchdb::drivers()[0];
    let mut s = d.connect(&c, None).await.expect("connect");
    assert!(s.server_version().await.unwrap().starts_with("CouchDB 3"));

    run(&mut s, "DELETE /dbine_it", 10).await.ok();
    run(
        &mut s,
        r#"PUT /dbine_it
           POST /dbine_it/_bulk_docs
           {"docs": [
             {"_id": "a", "name": "Ana", "age": 31, "tags": ["x"]},
             {"_id": "b", "name": "Bruno", "age": 25, "addr": {"city": "Rosario"}},
             {"_id": "c", "name": "Carla", "age": 40}
           ]}
           PUT /dbine_it/_design/app
           {"views": {"by_age": {"map": "function (doc) { if (doc.age) emit(doc.age, doc.name); }", "reduce": "_count"}}}"#,
        10,
    )
    .await
    .expect("setup");

    let mut s = d.connect(&c, Some("dbine_it")).await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"dbine_it".to_string()));
    let objs = s.list_objects().await.unwrap();
    let names: Vec<_> = objs.iter().map(|o| (o.kind.as_str(), o.name.as_str())).collect();
    assert_eq!(names, [("collection", "_all_docs"), ("view", "app/by_age")]);

    let all = ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() };
    let view = ObjectRef { kind: "view".into(), schema: None, name: "app/by_age".into() };
    let cols = s.columns(&all).await.unwrap();
    assert_eq!(cols[0].name, "_id");
    assert!(cols.iter().any(|c| c.name == "age" && c.data_type == "integer" && !c.nullable));
    let vcols = s.columns(&view).await.unwrap();
    assert!(vcols.iter().any(|c| c.name == "key"), "{vcols:?}");
    assert!(s.definition(&view).await.unwrap().unwrap().contains("emit(doc.age"));
    assert!(s.definition(&all).await.unwrap().unwrap().contains("\"db_name\""));

    // Browse both objects.
    let q = s.browse_query(&all, 50);
    let out = run(&mut s, &q, 100).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    assert_eq!(out.results[0].columns[0].name, "_id");
    let q = s.browse_query(&view, 50);
    let out = run(&mut s, &(q + "&reduce=false"), 100).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);

    // Mango + console lines in one script; max_rows.
    let out = run(
        &mut s,
        "{\"selector\": {\"age\": {\"$gt\": 26}}, \"fields\": [\"name\"]}\n\
         GET _all_docs?include_docs=true\n\
         {\"selector\": {}}\n\
         GET /_all_dbs",
        2,
    )
    .await
    .unwrap();
    assert_eq!(out.results[0].rows, vec![vec![json!("Ana")], vec![json!("Carla")]]);
    assert!(out.results[1].truncated && out.results[1].rows.len() == 2);
    assert!(out.results[1].columns.iter().any(|c| c.name == "_rev"));
    assert!(out.results[2].truncated);
    assert_eq!(out.results[3].columns[0].name, "value");

    // Server errors.
    let e = run(&mut s, "{\"selector\": {\"age\": {\"$nope\": 1}}}", 10).await.unwrap_err();
    assert!(matches!(e, Error::Query(_)), "{e:?}");
    let e = run(&mut s, "GET /dbine_it/missing", 10).await.unwrap_err();
    assert!(e.to_string().contains("not_found"), "{e}");

    // Read-only.
    let mut ro = d.connect(&cfg(true).unwrap(), Some("dbine_it")).await.unwrap();
    run(&mut ro, "{\"selector\": {}}\nPOST _all_docs {\"keys\": [\"a\"]}", 10).await.unwrap();
    for w in ["PUT /x", "POST /dbine_it {\"a\": 1}", "DELETE /dbine_it", "POST _bulk_docs {\"docs\": []}"] {
        let e = run(&mut ro, w, 10).await.unwrap_err();
        assert!(e.to_string().contains("solo lectura"), "{w}: {e}");
    }

    // Wrong password.
    let bad = ConnectionConfig { password: Some("nope".into()), ..c.clone() };
    assert!(matches!(d.connect(&bad, None).await.err().unwrap(), Error::AuthFailed(_)));

    run(&mut s, "DELETE /dbine_it", 10).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn explain_plans() {
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_couchdb::drivers()[0];
    assert!(d.supports_explain());
    let mut s = d.connect(&c, None).await.expect("connect");
    run(&mut s, "DELETE /dbine_plan", 5).await.ok();
    run(&mut s, "PUT /dbine_plan", 5).await.unwrap();
    let docs: Vec<_> = (0..300).map(|i| json!({ "_id": format!("d{i:03}"), "n": i, "g": i % 3 })).collect();
    run(&mut s, &format!("POST /dbine_plan/_bulk_docs {}", json!({ "docs": docs })), 5).await.unwrap();
    let mut s = d.connect(&c, Some("dbine_plan")).await.unwrap();

    // Estimated: no documents come back, the DELETE doesn't run.
    let mut out = QueryOutcome::default();
    s.explain("{\"selector\": {\"n\": {\"$gt\": 295}}}\nDELETE /dbine_plan/d000", false, 50, &mut out).await.unwrap();
    assert!(out.results.is_empty());
    assert_eq!(out.plans.len(), 1);
    assert_eq!(out.messages.len(), 1);
    let scan = &out.plans[0].root.children[0].children[0];
    assert_eq!(scan.op, "Full Scan");
    assert!(scan.warnings[0].contains("_all_docs"));
    assert!(run(&mut s, "GET d000", 5).await.is_ok());

    // Actual: results + execution stats.
    let mut out = QueryOutcome::default();
    s.explain("{\"selector\": {\"n\": {\"$gt\": 295}}}", true, 50, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 4);
    let p = &out.plans[0];
    assert!(p.actual);
    assert_eq!(p.root.actual_rows, Some(4.0));
    assert_eq!(p.root.children[0].children[0].actual_rows, Some(300.0));
    assert!(p.root.warnings.is_empty(), "{:?}", p.root.warnings);

    // With a JSON index.
    run(&mut s, "POST _index {\"index\": {\"fields\": [\"n\"]}, \"name\": \"n-idx\", \"ddoc\": \"by-n\"}", 5).await.unwrap();
    let mut out = QueryOutcome::default();
    s.explain("POST _find {\"selector\": {\"n\": {\"$gt\": 295}}}", true, 50, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 4);
    let p = &out.plans[0];
    let fetch = &p.root.children[0].children[0];
    assert_eq!(fetch.op, "Fetch");
    assert_eq!(fetch.children[0].op, "Index Scan");
    assert_eq!(fetch.children[0].object.as_deref(), Some("by-n/n-idx"));
    assert!(fetch.children[0].actual_rows.is_some_and(|n| (4.0..=5.0).contains(&n)));
    run(&mut s, "DELETE /dbine_plan", 5).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn scripts_schema_and_databases() {
    use dbine_driver::DdlParts;
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_couchdb::drivers()[0];
    let caps = d.capabilities();
    assert!(caps.create_database && caps.drop_database && !caps.foreign_keys);
    assert!(d.designer().is_none());
    let mut s0 = d.connect(&c, None).await.expect("connect");
    for db in ["dbine_ddl_it", "dbine_ddl_it2"] {
        let _ = s0.drop_database(db).await;
    }
    assert!(s0.create_database("Bad Name").await.is_err());
    s0.create_database("dbine_ddl_it").await.expect("create db");
    assert!(s0.list_databases().await.unwrap().contains(&"dbine_ddl_it".to_string()));
    assert!(s0.create_database("dbine_ddl_it").await.is_err());
    let mut s = d.connect(&c, Some("dbine_ddl_it")).await.unwrap();

    // Templates run through execute on the session's database.
    for t in d.create_templates() {
        let text = t.template.replace("{name}", &format!("t_{}", t.kind));
        run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{}: {e}\n{text}", t.label));
    }
    // Insert script → execute → documents.
    let cols = vec!["_id".to_string(), "_rev".into(), "tipo".into(), "fecha".into(), "n".into()];
    let rows: Vec<Vec<serde_json::Value>> = (0..620)
        .map(|i| vec![json!(format!("doc{i:04}")), json!("9-zzz"), json!(if i % 2 == 0 { "a" } else { "b" }), json!(format!("2024-01-{:02}", i % 28 + 1)), if i % 5 == 0 { serde_json::Value::Null } else { json!(i) }])
        .collect();
    let target = ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() };
    let script = d.insert_script(&target, &cols, &rows).unwrap();
    let out = run(&mut s, &script, 10).await.expect("insert script");
    assert!(out.messages.iter().all(|m| !m.contains("fallaron")), "{:?}", out.messages);
    let out = run(&mut s, "GET /dbine_ddl_it", 10).await.unwrap();
    let total = out.results[0].columns.iter().position(|c| c.name == "doc_count").unwrap();
    // 620 documents + 2 design docs from templates + the Mango index's.
    assert_eq!(out.results[0].rows[0][total], json!(623));
    // The validation template works.
    assert!(run(&mut s, r#"POST _bulk_docs {"docs": [{"_id": "sin_tipo"}]}"#, 10).await.unwrap().messages.iter().any(|m| m.contains("forbidden")));
    // A partial, descending index through table_ddl.
    let extra = dbine_driver::TableSchema {
        name: "_all_docs".into(),
        indexes: vec![dbine_driver::IndexDef {
            name: "n_desc".into(),
            columns: vec!["n:desc".into()],
            filter: Some(r#"{"tipo": "a"}"#.into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    run(&mut s, &d.table_ddl(&extra, DdlParts { indexes: true, ..Default::default() }).unwrap(), 10).await.expect("index ddl");

    // database_schema: _all_docs with fields, indexes, design docs.
    let schema = s.database_schema().await.unwrap();
    assert_eq!(schema.len(), 1);
    let t = &schema[0];
    assert_eq!(t.primary_key.as_ref().unwrap().columns, vec!["_id"]);
    assert!(t.columns.iter().any(|c| c.name == "tipo"));
    let mut ix: Vec<_> = t.indexes.iter().map(|i| (i.name.clone(), i.columns.clone(), i.filter.clone())).collect();
    ix.sort();
    assert_eq!(
        ix,
        vec![
            ("n_desc".to_string(), vec!["n:desc".to_string()], Some(r#"{"tipo":{"$eq":"a"}}"#.to_string())), // CouchDB normalizes it
            ("t_index".to_string(), vec!["tipo".to_string(), "fecha".to_string()], None),
        ]
    );
    let ddocs = t.options.get("design_docs").expect("design docs");
    assert!(ddocs.contains("t_view") && ddocs.contains("t_validation") && !ddocs.contains("\"_rev\""));

    // Regenerate the database elsewhere: DDL + data.
    s0.create_database("dbine_ddl_it2").await.unwrap();
    let mut s2 = d.connect(&c, Some("dbine_ddl_it2")).await.unwrap();
    let ddl = d.table_ddl(t, DdlParts { drop: true, create: true, indexes: true, ..Default::default() }).unwrap();
    run(&mut s2, &ddl, 10).await.expect("regenerated ddl");
    let again = s2.database_schema().await.unwrap();
    let mut ix2: Vec<_> = again[0].indexes.iter().map(|i| (i.name.clone(), i.columns.clone(), i.filter.clone())).collect();
    ix2.sort();
    assert_eq!(ix2, ix);
    run(&mut s2, "GET _design/t_view/_view/por_tipo", 10).await.expect("the view was recreated");

    // Read-only: no writes, no database operations.
    let ro = ConnectionConfig { read_only: true, ..c.clone() };
    let mut r = d.connect(&ro, Some("dbine_ddl_it")).await.unwrap();
    assert!(run(&mut r, &script, 10).await.unwrap_err().to_string().contains("solo lectura"));
    assert!(r.drop_database("dbine_ddl_it").await.is_err());
    assert!(run(&mut r, "{\"selector\": {\"tipo\": \"a\"}, \"limit\": 5}", 10).await.is_ok());

    s0.drop_database("dbine_ddl_it").await.expect("drop");
    s0.drop_database("dbine_ddl_it2").await.expect("drop 2");
    assert!(!s0.list_databases().await.unwrap().iter().any(|n| n.starts_with("dbine_ddl_it")));
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_couchdb::drivers()[0];
    assert!(d.capabilities().monitor);
    let mut s = d.connect(&c, None).await.expect("connect");
    run(&mut s, "PUT /dbine_monitor", 10).await.ok();
    run(&mut s, "POST /dbine_monitor {\"a\": 1}", 10).await.unwrap();
    let snap = s.monitor().await.expect("monitor");
    for m in &snap.metrics {
        eprintln!("{:<22} {:?} max={:?} counter={}", m.key, m.value, m.max, m.counter);
    }
    for t in &snap.tables {
        eprintln!("table {} rows={}", t.key, t.rows.len());
    }
    eprintln!("info {:?}\nnotes {:?}", snap.info, snap.notes);
    let has = |k: &str| snap.metrics.iter().any(|m| m.key == k && m.value.is_some());
    for k in ["mem_used", "queries", "rows_written", "uptime", "storage_used", "run_queue"] {
        assert!(has(k), "{k}");
    }
    assert!(snap.tables.iter().any(|t| t.key == "databases" && !t.rows.is_empty()));
    assert!(snap.tables.iter().any(|t| t.key == "nodes" && !t.rows.is_empty()));
    run(&mut s, "DELETE /dbine_monitor", 10).await.ok();
}

/// Schema sync on `_all_docs`: a new Mango index and a new design document
/// are created; the ones that can't be replaced come as warnings.
#[tokio::test]
#[ignore]
async fn schema_sync() {
    use dbine_driver::{IndexDef, TableChange};
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_couchdb::drivers()[0];
    assert!(d.supports_schema_sync());
    let mut s = d.connect(&c, None).await.expect("connect");
    run(&mut s, "DELETE /dbine_sync", 10).await.ok();
    run(&mut s, "PUT /dbine_sync\nPOST /dbine_sync/_bulk_docs\n{\"docs\": [{\"_id\": \"a\", \"tipo\": \"x\"}]}", 10).await.expect("setup");
    let c2 = ConnectionConfig { database: "dbine_sync".into(), ..c.clone() };
    let mut s = d.connect(&c2, Some("dbine_sync")).await.expect("connect");
    let old = s.database_schema().await.unwrap().remove(0);
    let mut new = old.clone();
    new.indexes.push(IndexDef { name: "por_tipo".into(), columns: vec!["tipo".into()], ..Default::default() });
    new.options.insert("design_docs".into(), json!([{ "_id": "_design/app", "views": { "v": { "map": "function (doc) { emit(doc.tipo, 1); }" } } }]).to_string());
    let script = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
    println!("{script:#?}");
    assert_eq!(script.statements.len(), 2);
    for st in &script.statements {
        run(&mut s, st, 10).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    let after = s.database_schema().await.unwrap().remove(0);
    assert!(after.indexes.iter().any(|i| i.name == "por_tipo"));
    assert!(after.options.get("design_docs").is_some_and(|d| d.contains("_design/app")));
    run(&mut s, "DELETE /dbine_sync", 10).await.ok();
}

/// The data-compare delete script (update handler, no `_rev`) removes
/// exactly the keyed documents, including an id with quotes and a slash.
#[tokio::test]
#[ignore]
async fn delete_script_runs() {
    let Some(c) = cfg(false) else { return };
    let d = &dbine_driver_couchdb::drivers()[0];
    let mut s = d.connect(&c, None).await.expect("connect");
    run(&mut s, "DELETE /dbine_del", 10).await.ok();
    run(
        &mut s,
        r#"PUT /dbine_del
           POST /dbine_del/_bulk_docs
           {"docs": [{"_id": "O'Brien \"Bob\"/1"}, {"_id": "b"}, {"_id": "keep"}]}"#,
        10,
    )
    .await
    .expect("setup");
    let mut s = d.connect(&c, Some("dbine_del")).await.unwrap();
    let all = ObjectRef { kind: "collection".into(), schema: None, name: "_all_docs".into() };
    let keys = vec![vec![("_id".to_string(), json!("O'Brien \"Bob\"/1"))], vec![("_id".to_string(), json!("b"))]];
    let script = d.delete_script(&all, &keys).unwrap();
    run(&mut s, &script, 10).await.expect(&script);
    let out = run(&mut s, "GET _all_docs", 10).await.unwrap();
    let rows = serde_json::to_string(&out.results[0].rows).unwrap();
    assert!(rows.contains("keep"), "{rows}");
    assert!(!rows.contains("Brien") && !rows.contains("\"b\""), "{rows}");
    run(&mut s, "DELETE /dbine_del", 10).await.ok();
}
