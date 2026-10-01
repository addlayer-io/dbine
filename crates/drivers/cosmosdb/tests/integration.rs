//! Against the Linux emulator (runs on arm64 too):
//!
//! ```sh
//! docker run -d --name dbine-test-cosmosdb -p 25203:8081 \
//!   mcr.microsoft.com/cosmosdb/linux/azure-cosmos-emulator:vnext-preview --protocol https
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25203 cargo test -p dbine-driver-cosmosdb -- --ignored
//! docker rm -f dbine-test-cosmosdb
//! ```
//! The key defaults to the emulator's well-known one
//! (`DBINE_TEST_COSMOSDB_KEY` overrides it).

use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use dbine_driver_cosmosdb::auth_header;
use serde_json::{json, Value};

const EMULATOR_KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

fn setup() -> Option<(String, String)> {
    let url = std::env::var("DBINE_TEST_COSMOSDB_URL").ok()?;
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    Some((url, key))
}

fn cfg(url: &str, key: &str, db: &str) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: "cosmosdb".into(),
        host: url.into(),
        database: db.into(),
        trust_server_certificate: true,
        ..Default::default()
    };
    c.options.insert("account_key".into(), key.into());
    c
}

/// A raw signed REST call, to create what the (read-only) driver can't.
async fn rest(url: &str, key: &str, method: &str, rtype: &str, link: &str, path: &str, body: Option<Value>, pk: Option<&str>) -> u16 {
    let http = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let mut rq = http
        .request(method.parse().unwrap(), format!("{url}{path}"))
        .header("Authorization", auth_header(key, method, rtype, link, &date).unwrap())
        .header("x-ms-date", date)
        .header("x-ms-version", "2018-12-31");
    if let Some(pk) = pk {
        rq = rq.header("x-ms-documentdb-partitionkey", format!("[\"{pk}\"]"));
    }
    if let Some(b) = body {
        rq = rq.header("Content-Type", "application/json").body(b.to_string());
    }
    rq.send().await.unwrap().status().as_u16()
}

async fn run(s: &mut Box<dyn Session>, q: &str, max: usize) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, max, &mut out).await.map(|_| out)
}

#[tokio::test]
#[ignore]
async fn end_to_end() {
    let Some((url, key)) = setup() else { return };
    let url = url.trim_end_matches('/').to_string();
    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_it", "/dbs/dbine_it", None, None).await;
    assert_eq!(rest(&url, &key, "POST", "dbs", "", "/dbs", Some(json!({ "id": "dbine_it" })), None).await, 201);
    for coll in ["items", "other-coll"] {
        let body = json!({ "id": coll, "partitionKey": { "paths": ["/cat"], "kind": "Hash" } });
        assert_eq!(rest(&url, &key, "POST", "colls", "dbs/dbine_it", "/dbs/dbine_it/colls", Some(body), None).await, 201);
    }
    let docs = [
        json!({ "id": "1", "cat": "a", "name": "Ana", "price": 31, "tags": ["x", "y"] }),
        json!({ "id": "2", "cat": "b", "name": "Bruno", "price": 25.5, "addr": { "city": "Rosario" } }),
        json!({ "id": "3", "cat": "a", "name": "Carla", "price": 40 }),
    ];
    for d in docs {
        let pk = d["cat"].as_str().unwrap().to_string();
        let st = rest(&url, &key, "POST", "docs", "dbs/dbine_it/colls/items", "/dbs/dbine_it/colls/items/docs", Some(d), Some(&pk)).await;
        assert_eq!(st, 201);
    }
    let body = json!({ "id": "z", "cat": "q" });
    rest(&url, &key, "POST", "docs", "dbs/dbine_it/colls/other-coll", "/dbs/dbine_it/colls/other-coll/docs", Some(body), Some("q")).await;

    let d = &dbine_driver_cosmosdb::drivers()[0];
    let mut s = d.connect(&cfg(&url, &key, "dbine_it"), None).await.expect("connect");
    assert!(s.server_version().await.unwrap().starts_with("Azure Cosmos DB"));
    assert!(s.list_databases().await.unwrap().contains(&"dbine_it".to_string()));
    let objs: Vec<String> = s.list_objects().await.unwrap().into_iter().map(|o| o.name).collect();
    assert_eq!(objs, ["items", "other-coll"]);

    let items = ObjectRef { kind: "collection".into(), schema: None, name: "items".into() };
    let cols = s.columns(&items).await.unwrap();
    assert_eq!(cols[0].name, "id");
    assert!(cols.iter().any(|c| c.name == "price" && c.data_type.contains("integer")));
    assert!(cols.iter().any(|c| c.name == "addr" && c.nullable));
    let def = s.definition(&items).await.unwrap().unwrap();
    assert!(def.contains("partitionKey") && def.contains("/cat"), "{def}");

    // Browse (directive form) and flattening.
    let q = s.browse_query(&items, 50);
    let out = run(&mut s, &q, 100).await.unwrap();
    let r = &out.results[0];
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.columns[0].name, "id");
    let tags = r.columns.iter().position(|c| c.name == "tags").unwrap();
    assert!(r.rows.iter().any(|row| row[tags] == json!("[\"x\",\"y\"]")));
    assert!(out.messages.iter().any(|m| m.contains("RU")));

    // FROM <container> inference, USE, VALUE scalars, several statements.
    let out = run(
        &mut s,
        "SELECT p.name FROM items p WHERE p.price > 30 ORDER BY p.name;\n\
         SELECT VALUE COUNT(1) FROM c;\n\
         USE \"other-coll\";\n\
         SELECT c.id FROM c",
        10,
    )
    .await
    .unwrap();
    assert_eq!(out.results[0].rows, vec![vec![json!("Ana")], vec![json!("Carla")]]);
    assert_eq!(out.results[1].rows, vec![vec![json!(3)]]);
    assert_eq!(out.results[2].rows, vec![vec![json!("z")]]);

    // max_rows, following continuations with small pages.
    let out = run(&mut s, "-- container: items\nSELECT * FROM c", 2).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert!(out.results[0].truncated);

    // Errors: bad syntax, missing container, not a SELECT.
    let e = run(&mut s, "-- container: items\nSELECT * FORM c", 10).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(e.to_script_error().line, Some(2), "the failing statement's line");
    let e = run(&mut s, "-- container: nope\nSELECT * FROM c", 10).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    let e = run(&mut s, "DELETE FROM c", 10).await.unwrap_err();
    assert!(e.to_string().contains("SELECT"), "{e}");

    // A wrong key is AuthFailed. The vnext emulator doesn't check
    // signatures, so only against a real account (key given explicitly).
    if std::env::var("DBINE_TEST_COSMOSDB_KEY").is_ok() {
        let bad = "AAAA".repeat(22);
        let e = d.connect(&cfg(&url, &bad, "dbine_it"), None).await.err().unwrap();
        assert!(matches!(e, Error::AuthFailed(_)), "{e:?}");
    }

    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_it", "/dbs/dbine_it", None, None).await;
}

#[tokio::test]
#[ignore]
async fn explain_plans() {
    let Some((url, key)) = setup() else { return };
    let url = url.trim_end_matches('/').to_string();
    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_plan", "/dbs/dbine_plan", None, None).await;
    assert_eq!(rest(&url, &key, "POST", "dbs", "", "/dbs", Some(json!({ "id": "dbine_plan" })), None).await, 201);
    let body = json!({ "id": "items", "partitionKey": { "paths": ["/cat"], "kind": "Hash" } });
    assert_eq!(rest(&url, &key, "POST", "colls", "dbs/dbine_plan", "/dbs/dbine_plan/colls", Some(body), None).await, 201);
    for i in 0..20 {
        let d = json!({ "id": i.to_string(), "cat": if i % 2 == 0 { "a" } else { "b" }, "price": i });
        let pk = d["cat"].as_str().unwrap().to_string();
        rest(&url, &key, "POST", "docs", "dbs/dbine_plan/colls/items", "/dbs/dbine_plan/colls/items/docs", Some(d), Some(&pk)).await;
    }
    let d = &dbine_driver_cosmosdb::drivers()[0];
    assert!(d.supports_explain());
    let mut s = d.connect(&cfg(&url, &key, "dbine_plan"), None).await.expect("connect");
    let q = "-- container: items\nSELECT TOP 5 * FROM c WHERE c.price > 3 ORDER BY c.price DESC";

    let mut out = QueryOutcome::default();
    s.explain(q, false, 50, &mut out).await.unwrap();
    assert!(out.results.is_empty());
    assert_eq!(out.plans.len(), 1);
    // The vnext emulator answers the query plan request, but with an empty
    // queryInfo (Azure fills TOP / ORDER BY…): check the shape only.
    assert_eq!(out.plans[0].root.op, "SELECT");
    assert!(out.plans[0].raw.contains("queryRanges"), "{}", out.plans[0].raw);

    let mut out = QueryOutcome::default();
    s.explain(q, true, 50, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 5);
    assert!(out.plans[0].actual);
    assert!(out.plans[0].raw.contains("totalExecutionTimeInMs"), "{}", out.plans[0].raw);
    assert_eq!(out.plans[0].root.children[0].op, "Write Output");
    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_plan", "/dbs/dbine_plan", None, None).await;
}

/// Database create/drop, the designer's container through `table_ddl` +
/// `execute`, read back with `database_schema`, rows through `insert_script`.
#[tokio::test]
#[ignore]
async fn ddl_and_scripts() {
    use dbine_driver::{DdlParts, IndexDef, TableSchema};
    use std::collections::BTreeMap;
    let Some((url, key)) = setup() else { return };
    let url = url.trim_end_matches('/').to_string();
    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_ddl", "/dbs/dbine_ddl", None, None).await;
    let d = &dbine_driver_cosmosdb::drivers()[0];
    assert!(d.capabilities().create_database && d.capabilities().drop_database);
    let mut admin = d.connect(&cfg(&url, &key, ""), None).await.expect("connect");
    admin.create_database("dbine_ddl").await.unwrap();
    assert!(admin.list_databases().await.unwrap().contains(&"dbine_ddl".to_string()));

    let mut s = d.connect(&cfg(&url, &key, "dbine_ddl"), None).await.expect("connect");
    let items = TableSchema {
        kind: "collection".into(),
        name: "items".into(),
        indexes: vec![
            IndexDef { name: "uq".into(), columns: vec!["email".into()], unique: true, ..Default::default() },
            IndexDef { name: "c".into(), columns: vec!["name".into(), "price DESC".into()], kind: Some("composite".into()), ..Default::default() },
        ],
        options: BTreeMap::from([
            ("partition_key".to_string(), "/cat".to_string()),
            ("throughput_mode".to_string(), "manual".to_string()),
            ("throughput".to_string(), "400".to_string()),
            ("default_ttl".to_string(), "-1".to_string()),
        ]),
        ..Default::default()
    };
    let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, ..Default::default() };
    let ddl = d.table_ddl(&items, all).unwrap();
    let out = run(&mut s, &ddl, 10).await.unwrap();
    assert!(out.messages.iter().any(|m| m.contains("creado")), "{:?}", out.messages);

    // Rows through insert_script (numeric id becomes text; nested values stay).
    let cols = vec!["id".to_string(), "cat".into(), "name".into(), "price".into(), "email".into(), "addr".into()];
    let rows = vec![
        vec![json!(1), json!("a"), json!("Ana; \"x\""), json!(31), json!("ana@x"), json!({ "city": "Rosario" })],
        vec![json!("2"), json!("b"), json!("Bruno"), json!(25.5), json!("bruno@x"), Value::Null],
    ];
    let target = ObjectRef { kind: "collection".into(), schema: None, name: "items".into() };
    let ins = d.insert_script(&target, &cols, &rows).unwrap();
    let out = run(&mut s, &ins, 10).await.unwrap();
    assert_eq!(out.results.len(), 2);
    let out = run(&mut s, "-- container: items\nSELECT VALUE [c.id, c.name, c.addr.city] FROM c ORDER BY c.id", 10).await.unwrap();
    assert_eq!(out.results[0].rows[0], vec![json!("[\"1\",\"Ana; \\\"x\\\"\",\"Rosario\"]")]);
    assert_eq!(out.results[0].rows.len(), 2);
    // The unique key holds; UPSERT replaces.
    let e = run(&mut s, r#"INSERT INTO items {"id": "3", "cat": "a", "email": "ana@x"}"#, 10).await;
    assert!(e.unwrap_err().to_string().contains("unique key"), "unique key must reject a duplicate email");
    run(&mut s, r#"UPSERT INTO items {"id": "2", "cat": "b", "name": "Bruno II"}"#, 10).await.unwrap();
    let out = run(&mut s, "-- container: items\nSELECT VALUE c.name FROM c WHERE c.id = '2'", 10).await.unwrap();
    assert_eq!(out.results[0].rows, vec![vec![json!("Bruno II")]]);

    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == "items").expect("container in schema");
    assert_eq!(t.options["partition_key"], "/cat");
    assert_eq!(t.options.get("default_ttl").map(String::as_str), Some("-1"));
    assert_eq!(t.primary_key.as_ref().unwrap().columns, ["id"]);
    assert!(t.columns.iter().any(|c| c.name == "email"));
    assert!(t.indexes.iter().any(|i| i.unique && i.columns == ["email"]), "{:?}", t.indexes);
    // What it reads back creates the container again.
    let again = d.table_ddl(t, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap().replace("\"items\"", "\"items2\"");
    run(&mut s, &again, 10).await.unwrap();
    assert!(s.list_objects().await.unwrap().iter().any(|o| o.name == "items2"));

    // Templates run as they are.
    for tpl in d.create_templates() {
        let text = tpl.template.replace("{name}", "tpl");
        run(&mut s, &text, 10).await.unwrap();
        run(&mut s, "DROP CONTAINER tpl", 10).await.unwrap();
    }

    // Read-only refuses the extensions and database operations.
    let mut ro_cfg = cfg(&url, &key, "dbine_ddl");
    ro_cfg.read_only = true;
    let mut ro = d.connect(&ro_cfg, None).await.unwrap();
    assert!(run(&mut ro, "DROP CONTAINER items", 10).await.is_err());
    assert!(run(&mut ro, r#"INSERT INTO items {"id": "9", "cat": "z"}"#, 10).await.is_err());
    assert!(ro.drop_database("dbine_ddl").await.is_err());

    let out = run(&mut s, "DROP CONTAINER items2; DROP CONTAINER IF EXISTS items2", 10).await.unwrap();
    assert!(out.messages.iter().any(|m| m.contains("no existe")), "{:?}", out.messages);
    admin.drop_database("dbine_ddl").await.unwrap();
    assert!(!admin.list_databases().await.unwrap().contains(&"dbine_ddl".to_string()));
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some((url, key)) = setup() else { return };
    let url = url.trim_end_matches('/').to_string();
    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_mon", "/dbs/dbine_mon", None, None).await;
    assert_eq!(rest(&url, &key, "POST", "dbs", "", "/dbs", Some(json!({ "id": "dbine_mon" })), None).await, 201);
    let body = json!({ "id": "items", "partitionKey": { "paths": ["/cat"], "kind": "Hash" } });
    assert_eq!(rest(&url, &key, "POST", "colls", "dbs/dbine_mon", "/dbs/dbine_mon/colls", Some(body), None).await, 201);
    let doc = json!({ "id": "1", "cat": "a" });
    rest(&url, &key, "POST", "docs", "dbs/dbine_mon/colls/items", "/dbs/dbine_mon/colls/items/docs", Some(doc), Some("a")).await;

    let d = &dbine_driver_cosmosdb::drivers()[0];
    assert!(d.capabilities().monitor);
    let mut s = d.connect(&cfg(&url, &key, "dbine_mon"), None).await.expect("connect");
    for _ in 0..2 {
        let snap = s.monitor().await.unwrap();
        for m in &snap.metrics {
            println!("{:<16} {:?} max={:?}", m.key, m.value, m.max);
        }
        for t in &snap.tables {
            println!("[{}] {:?}", t.key, t.rows);
        }
        println!("info {:?}\nnotes {:?}", snap.info, snap.notes);
        let val = |k: &str| snap.metrics.iter().find(|m| m.key == k).unwrap().value;
        assert_eq!(val("containers"), Some(1.0));
        assert!(val("partitions").unwrap() >= 1.0);
        assert!(val("monitor_charge").is_some());
        let t = snap.tables.iter().find(|t| t.key == "top_objects").unwrap();
        assert_eq!(t.rows[0][0], json!("items"));
        assert_eq!(t.rows[0][7], json!("/cat"));
        assert!(snap.info.iter().any(|(k, _)| k == "Cuenta"));
    }
    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_mon", "/dbs/dbine_mon", None, None).await;
}

/// The data-compare delete script (`DELETE FROM … WHERE {id}`) removes
/// exactly the keyed documents, across partitions, with quotes in the id.
#[tokio::test]
#[ignore]
async fn delete_script_runs() {
    let Some((url, key)) = setup() else { return };
    let url = url.trim_end_matches('/').to_string();
    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_del", "/dbs/dbine_del", None, None).await;
    assert_eq!(rest(&url, &key, "POST", "dbs", "", "/dbs", Some(json!({ "id": "dbine_del" })), None).await, 201);
    let d = &dbine_driver_cosmosdb::drivers()[0];
    let mut s = d.connect(&cfg(&url, &key, "dbine_del"), None).await.expect("connect");
    run(
        &mut s,
        r#"CREATE CONTAINER "items" { "partitionKey": { "paths": ["/cat"], "kind": "Hash" } };
           INSERT INTO "items" { "id": "O'Brien \"Bob\"; x", "cat": "a" };
           INSERT INTO "items" { "id": "2", "cat": "b" };
           INSERT INTO "items" { "id": "keep", "cat": "a" };"#,
        10,
    )
    .await
    .expect("setup");
    let items = ObjectRef { kind: "collection".into(), schema: None, name: "items".into() };
    let keys = vec![vec![("id".to_string(), json!("O'Brien \"Bob\"; x"))], vec![("id".to_string(), json!(2))]];
    let script = d.delete_script(&items, &keys).unwrap();
    let out = run(&mut s, &script, 10).await.expect(&script);
    assert_eq!(out.results.iter().filter_map(|r| r.rows_affected).sum::<u64>(), 2, "{out:?}");
    let out = run(&mut s, "-- container: items\nSELECT c.id FROM c", 10).await.unwrap();
    let rows = serde_json::to_string(&out.results[0].rows).unwrap();
    assert_eq!(out.results[0].rows.len(), 1, "{rows}");
    assert!(rows.contains("keep"), "{rows}");
    assert!(run(&mut s, &script, 10).await.is_err(), "a second run finds nothing to delete");
    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_del", "/dbs/dbine_del", None, None).await;
}
