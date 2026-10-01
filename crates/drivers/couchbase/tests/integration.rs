//! Against a real Couchbase Server (the test initializes a fresh node):
//!
//! ```sh
//! docker run -d --name dbine-test-couchbase -p 25891:8091 -p 25893:8093 couchbase/server:community
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase -- --ignored --test-threads=1
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{kinds, ConnectionConfig, DdlParts, Error, IndexDef, ObjectRef, QueryOutcome, Session, TableSchema};
use serde_json::json;
use std::time::{Duration, Instant};

const USER: &str = "Administrator";
const PASS: &str = "secreto1";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

/// Provision the node (data, query and index services) if it isn't yet.
async fn init(c: &ConnectionConfig) {
    let mgmt = format!("http://{}:{}", c.host, c.options["mgmt_port"]);
    let http = reqwest::Client::new();
    let ok = http.get(format!("{mgmt}/pools/default")).basic_auth(USER, Some(PASS)).send().await.map(|r| r.status().is_success()).unwrap_or(false);
    if !ok {
        let r = http
            .post(format!("{mgmt}/clusterInit"))
            .form(&[
                ("hostname", "127.0.0.1"),
                ("services", "kv,n1ql,index"),
                ("memoryQuota", "512"),
                ("indexMemoryQuota", "256"),
                ("username", USER),
                ("password", PASS),
                ("port", "SAME"),
                ("indexerStorageMode", "forestdb"),
            ])
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "clusterInit: {}", r.text().await.unwrap_or_default());
    }
    for _ in 0..60 {
        let q = http
            .post(format!("http://{}:{}/query/service", c.host, c.port))
            .basic_auth(USER, Some(PASS))
            .form(&[("statement", "SELECT RAW 1")])
            .send()
            .await;
        if q.map(|r| r.status().is_success()).unwrap_or(false) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("the query service didn't come up");
}

fn coll(schema: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::COLLECTION.into(), schema: Some(schema.into()), name: name.into() }
}

#[tokio::test]
#[ignore]
async fn couchbase() {
    let Some(c) = cfg() else { return };
    init(&c).await;
    let d = dbine_driver_couchbase::drivers().remove(0);

    let mut bad = c.clone();
    bad.password = Some("nope".into());
    assert!(matches!(d.connect(&bad, None).await, Err(Error::AuthFailed(_))), "bad password");

    let mut s = d.connect(&c, None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("Couchbase Server "));
    if s.list_databases().await.unwrap().contains(&"dbine_it".to_string()) {
        s.drop_database("dbine_it").await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    s.create_database("dbine_it").await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"dbine_it".to_string()));

    let mut s = d.connect(&c, Some("dbine_it")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("CREATE SCOPE dbine_it.ventas; CREATE COLLECTION dbine_it.ventas.pedidos", 10, &mut out).await.unwrap();
    // The new collection takes a moment to be usable.
    let mut ready = false;
    for _ in 0..30 {
        if s.execute("SELECT RAW 1 FROM dbine_it.ventas.pedidos LIMIT 1", 10, &mut QueryOutcome::default()).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ready);
    let mut out = QueryOutcome::default();
    s.execute(
        "INSERT INTO dbine_it.ventas.pedidos (KEY, VALUE) VALUES ('p1', {'cliente': 'Ana', 'total': 10.5, 'items': [1, 2]}),
           ('p2', {'cliente': 'O''Brien', 'total': 3, 'nota': {'x': true}, 'big': 9007199254740993});
         INSERT INTO pedidos_default (KEY, VALUE) VALUES ('d1', {'a': 1})",
        10,
        &mut out,
    )
    .await
    .unwrap_err(); // the default-scope collection doesn't exist yet: stops after the first
    assert_eq!(out.results[0].rows_affected, Some(2));
    let mut out = QueryOutcome::default();
    s.execute("CREATE INDEX ix_cliente ON dbine_it.ventas.pedidos(cliente); UPDATE dbine_it.ventas.pedidos SET visto = true WHERE total > 5", 10, &mut out).await.unwrap();
    assert_eq!(out.results[1].rows_affected, Some(1));

    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == kinds::COLLECTION && o.name == "pedidos" && o.schema.as_deref() == Some("dbine_it.ventas")), "{objs:?}");
    assert!(objs.iter().any(|o| o.kind == kinds::COLLECTION && o.name == "_default" && o.schema.as_deref() == Some("dbine_it._default")));
    assert!(objs.iter().any(|o| o.kind == kinds::INDEX && o.name == "ix_cliente" && o.parent.as_deref() == Some("pedidos")));
    let cols = s.columns(&coll("dbine_it.ventas", "pedidos")).await.unwrap();
    assert_eq!(cols[0].name, "_id");
    let total = cols.iter().find(|c| c.name == "total").unwrap();
    assert!(total.data_type.contains("number") && total.data_type.contains("integer"), "{}", total.data_type);
    assert!(cols.iter().find(|c| c.name == "nota").unwrap().nullable);
    let def = s.definition(&coll("dbine_it.ventas", "pedidos")).await.unwrap().unwrap();
    assert!(def.starts_with("CREATE COLLECTION `dbine_it`.`ventas`.`pedidos`;") && def.contains("CREATE INDEX `ix_cliente`"), "{def}");
    let ixdef = s.definition(&ObjectRef { kind: kinds::INDEX.into(), schema: Some("dbine_it.ventas".into()), name: "ix_cliente".into() }).await.unwrap().unwrap();
    assert!(ixdef.starts_with("CREATE INDEX `ix_cliente` ON `dbine_it`.`ventas`.`pedidos`"), "{ixdef}");

    let q = s.browse_query(&coll("dbine_it.ventas", "pedidos"), 10);
    let mut out = QueryOutcome::default();
    s.execute(&format!("{q}; SELECT RAW cliente FROM dbine_it.ventas.pedidos ORDER BY cliente"), 1, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!((r.rows.len(), r.total_rows, r.truncated), (1, 2, true));
    assert_eq!(r.columns[0].name, "_id");
    assert_eq!(out.results[1].columns[0].name, "value");
    let mut out = QueryOutcome::default();
    s.execute("SELECT p.big, p.items, p.nota FROM dbine_it.ventas.pedidos p USE KEYS 'p2'", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0], vec![json!("9007199254740993"), serde_json::Value::Null, json!("{\"x\":true}")]);

    // Errors stop the script.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1; SELECT * FROM nope_nada; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    // With its code and place in the script (the keyspace is at byte 24).
    let se = e.to_script_error();
    assert_eq!((se.code.as_deref(), se.line, se.offset), (Some("12003"), Some(1), Some(24)), "{se:?}");
    assert_eq!(out.results.len(), 1);

    // Plans.
    let mut out = QueryOutcome::default();
    s.explain("SELECT * FROM dbine_it.ventas.pedidos WHERE cliente = 'Ana'; CREATE INDEX nada ON dbine_it.ventas.pedidos(x)", false, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1);
    assert!(out.results.is_empty());
    fn find<'a>(n: &'a dbine_driver::PlanNode, op: &str) -> Option<&'a dbine_driver::PlanNode> {
        if n.op.starts_with(op) {
            return Some(n);
        }
        n.children.iter().find_map(|c| find(c, op))
    }
    let scan = find(&out.plans[0].root, "IndexScan").unwrap_or_else(|| panic!("{:#?}", out.plans[0].root));
    assert_eq!(scan.object.as_deref(), Some("dbine_it.ventas.pedidos"));
    let mut out = QueryOutcome::default();
    s.explain("SELECT cliente FROM dbine_it.ventas.pedidos WHERE cliente >= 'A'", true, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.plans.len(), 1);

    // Designer, scripts and templates.
    let t = TableSchema {
        kind: kinds::COLLECTION.into(),
        schema: Some("dbine_it.ventas".into()),
        name: "copia".into(),
        indexes: vec![IndexDef { name: "ix_copia".into(), columns: vec!["cliente".into()], ..Default::default() }],
        // max_ttl is Enterprise-only; Community refuses it.
        options: [("primary_index".to_string(), "true".to_string())].into(),
        ..Default::default()
    };
    let ddl = d.table_ddl(&t, DdlParts { create: true, if_exists: true, indexes: true, ..Default::default() }).unwrap();
    s.execute(&ddl, 10, &mut QueryOutcome::default()).await.unwrap();
    let mut browse = QueryOutcome::default();
    s.execute(&s.browse_query(&coll("dbine_it.ventas", "pedidos"), 100), 100, &mut browse).await.unwrap();
    let cols: Vec<String> = browse.results[0].columns.iter().map(|c| c.name.clone()).collect();
    let ins = d.insert_script(&coll("dbine_it.ventas", "copia"), &cols, &browse.results[0].rows).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&ins, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows_affected, Some(2));
    let mut out = QueryOutcome::default();
    s.execute("SELECT RAW c.nota.x FROM dbine_it.ventas.copia c USE KEYS 'p2'", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!(true), "nested values survive the copy");
    let def = s.definition(&coll("dbine_it.ventas", "copia")).await.unwrap().unwrap();
    assert!(def.contains("ix_copia") && def.contains("CREATE PRIMARY INDEX"), "{def}");
    for (i, tpl) in d.create_templates().iter().filter(|t| t.kind == kinds::INDEX || t.kind == kinds::FUNCTION).enumerate() {
        let name = if tpl.kind == kinds::FUNCTION { "tpl_function".to_string() } else { format!("tpl_index_{i}") };
        let sql = tpl.template.replace("{schema}", "`dbine_it`.`ventas`").replace("`coleccion`", "`pedidos`").replace("{name}", &name);
        s.execute(&sql, 10, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{}: {e}", tpl.label));
    }
    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == kinds::FUNCTION && o.name == "tpl_function"), "{objs:?}");
    let fdef = s.definition(&ObjectRef { kind: kinds::FUNCTION.into(), schema: None, name: "tpl_function".into() }).await.unwrap().unwrap();
    assert!(fdef.contains("CREATE OR REPLACE FUNCTION"), "{fdef}");
    s.execute("DROP FUNCTION tpl_function", 10, &mut QueryOutcome::default()).await.unwrap();

    // Read-only: the registry guard and the server's `readonly`.
    let mut roc = c.clone();
    roc.read_only = true;
    let mut ro = d.connect(&roc, Some("dbine_it")).await.unwrap();
    let e = ro.execute("UPDATE dbine_it.ventas.pedidos SET x = 1", 10, &mut QueryOutcome::default()).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    let mut ro = ReadOnlySession::new(ro);
    assert!(ro.execute("DELETE FROM dbine_it.ventas.pedidos", 10, &mut QueryOutcome::default()).await.is_err());

    // Cancel: the request stops and the server drops it.
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        stop();
    });
    let t = Instant::now();
    let r = s.execute("SELECT COUNT(1) AS n FROM ARRAY_RANGE(0, 5000) AS a UNNEST ARRAY_RANGE(0, 5000) AS b", 10, &mut QueryOutcome::default()).await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(8));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let snap = s.monitor().await.unwrap();
    assert!(snap.tables.iter().find(|t| t.key == "queries").unwrap().rows.is_empty(), "the server stopped it");

    let mut s = d.connect(&c, None).await.unwrap();
    s.drop_database("dbine_it").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(c) = cfg() else { return };
    init(&c).await;
    let d = dbine_driver_couchbase::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    // Bucket figures (data RAM, disk, items) need a bucket.
    if !s.list_databases().await.unwrap().contains(&"dbine_mon".to_string()) {
        s.create_database("dbine_mon").await.unwrap();
    }
    s.execute("SELECT RAW 1", 10, &mut QueryOutcome::default()).await.unwrap();
    // A new bucket's stats take a few seconds to show up.
    let mut snap = s.monitor().await.unwrap();
    for _ in 0..30 {
        if snap.metrics.iter().any(|m| m.key == "mem_cache" && m.value.is_some()) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        snap = s.monitor().await.unwrap();
    }
    s.drop_database("dbine_mon").await.unwrap();
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    for k in ["cpu", "mem_used", "mem_cache", "connections", "queries", "storage_used", "uptime", "cpu_query"] {
        assert!(v(k).is_some(), "{k}: {:#?}", snap.metrics);
    }
    assert!(v("mem_used").unwrap() > 1e6 && v("queries").unwrap() >= 1.0);
    let nodes = snap.tables.iter().find(|t| t.key == "nodes").unwrap();
    assert_eq!(nodes.rows.len(), 1);
    assert!(snap.info.iter().any(|(k, _)| k == "Versión"));
}

/// One session profiles the `dbine_prof` scope, the other runs a slow and a
/// fast statement that name it (with a unique marker) and one that doesn't:
/// the first two are seen once, the last and the profiler's own never, and
/// the completed-requests threshold is back as it was at stop.
#[tokio::test]
#[ignore]
async fn profiler() {
    let Some(c) = cfg() else { return };
    init(&c).await;
    let d = dbine_driver_couchbase::drivers().remove(0);
    assert!(d.supports_profiler());
    let threshold = || async {
        let url = format!("http://{}:{}/settings/querySettings", c.host, c.options["mgmt_port"]);
        let v: serde_json::Value =
            reqwest::Client::new().get(url).basic_auth(USER, Some(PASS)).send().await.unwrap().json().await.unwrap();
        v["queryCompletedThreshold"].as_i64().unwrap()
    };
    let before = threshold().await;
    let mut p = d.connect(&c, None).await.unwrap();
    let mut w = d.connect(&c, None).await.unwrap();
    let opts = dbine_driver::ProfilerOptions { database: "dbine_prof".into(), change_server: true };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{started:?}");
    assert_eq!(threshold().await, 0);
    // Not containing the scope's name itself.
    let marker = format!("cbmark_{}", std::process::id());
    let slow = format!("SELECT COUNT(*) AS n FROM ARRAY_RANGE(0, 2000000) AS x WHERE \"dbine_prof {marker}_slow\" IS NOT NULL");
    let fast = format!("SELECT RAW \"dbine_prof {marker}_fast\"");
    let other = format!("SELECT RAW \"{marker}_other\"");
    for stmt in [&slow, &fast, &other] {
        w.execute(stmt, 10, &mut QueryOutcome::default()).await.expect(stmt);
    }
    let mut got = Vec::new();
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        got.extend(p.profiler_poll().await.expect("profiler_poll"));
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    p.profiler_stop().await.expect("profiler_stop");
    assert_eq!(threshold().await, before, "threshold restored");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{mine:#?}");
    assert_eq!(mine.len(), 2, "the slow and the fast statement, once each; not the other scope's");
    assert!(mine[0].text.contains("_slow") && mine[1].text.contains("_fast"));
    assert!(mine[0].duration_ms.unwrap_or(0.0) > mine[1].duration_ms.unwrap_or(0.0));
    assert_eq!(mine[1].rows, Some(1));
    assert_eq!(started.reads_unit.as_deref(), Some("documentos"));
    assert!(mine[0].cpu_ms.unwrap_or(0.0) > 0.0, "CPU time {:?}", mine[0].cpu_ms);
    assert!(got.iter().all(|s| !s.text.contains("completed_requests") && !s.text.contains("NOW_MILLIS")), "own statements left out");
}

/// Schema sync: an index swapped and a primary index added on one
/// collection, another collection dropped and one created with an index.
#[tokio::test]
#[ignore]
async fn schema_sync() {
    use dbine_driver::{ColumnDef, TableChange};
    let Some(c) = cfg() else { return };
    init(&c).await;
    let d = dbine_driver_couchbase::drivers().remove(0);
    assert!(d.supports_schema_sync());
    let mut s = d.connect(&c, None).await.unwrap();
    if s.list_databases().await.unwrap().contains(&"dbine_sync".to_string()) {
        s.drop_database("dbine_sync").await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    s.create_database("dbine_sync").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_sync")).await.unwrap();
    let run = |q: &'static str| q;
    let mut out = QueryOutcome::default();
    s.execute(run("CREATE SCOPE dbine_sync.app; CREATE COLLECTION dbine_sync.app.items; CREATE COLLECTION dbine_sync.app.gone"), 10, &mut out).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while s.execute("CREATE INDEX ix_old ON dbine_sync.app.items(qty)", 10, &mut QueryOutcome::default()).await.is_err() {
        assert!(Instant::now() < deadline, "the collection didn't become usable");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let col = |n: &str| ColumnDef { name: n.into(), data_type: "string".into(), nullable: true, ..Default::default() };
    let ix = |n: &str, f: &str| IndexDef { name: n.into(), columns: vec![f.into()], ..Default::default() };
    let t = |name: &str, indexes: Vec<IndexDef>| TableSchema {
        kind: kinds::COLLECTION.into(),
        schema: Some("dbine_sync.app".into()),
        name: name.into(),
        columns: vec![col("sku"), col("qty")],
        indexes,
        ..Default::default()
    };
    let old = t("items", vec![ix("ix_old", "qty")]);
    let mut new = t("items", vec![ix("ix_sku", "sku")]);
    new.options.insert("primary_index".into(), "true".into());
    let script = d
        .sync_script(&[
            TableChange::Alter { old, new },
            TableChange::Drop { table: t("gone", vec![]) },
            TableChange::Create { table: t("fresh", vec![ix("ix_fresh", "sku")]) },
        ])
        .unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        let mut out = QueryOutcome::default();
        s.execute(st, 10, &mut out).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    let objs = s.list_objects().await.unwrap();
    let has = |kind: &str, name: &str| objs.iter().any(|o| o.kind == kind && o.name == name);
    assert!(has(kinds::COLLECTION, "fresh") && !has(kinds::COLLECTION, "gone"), "{objs:?}");
    assert!(has(kinds::INDEX, "ix_sku") && has(kinds::INDEX, "ix_fresh") && !has(kinds::INDEX, "ix_old"), "{objs:?}");
    let mut s = d.connect(&c, None).await.unwrap();
    s.drop_database("dbine_sync").await.unwrap();
}

/// The data-compare delete script removes exactly the keyed documents:
/// by document key (`USE KEYS`, quotes included) and by other fields.
#[tokio::test]
#[ignore]
async fn delete_script_runs() {
    let Some(c) = cfg() else { return };
    init(&c).await;
    let d = dbine_driver_couchbase::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    if !s.list_databases().await.unwrap().contains(&"dbine_del".to_string()) {
        s.create_database("dbine_del").await.unwrap();
    }
    let mut s = d.connect(&c, Some("dbine_del")).await.unwrap();
    let mut last = None;
    for _ in 0..60 {
        let r = async {
            s.execute("CREATE PRIMARY INDEX IF NOT EXISTS ON dbine_del._default._default", 10, &mut QueryOutcome::default()).await?;
            s.execute("DELETE FROM dbine_del._default._default WHERE true", 10, &mut QueryOutcome::default()).await
        }
        .await;
        last = r.err();
        if last.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(last.is_none(), "{last:?}");
    let mut out = QueryOutcome::default();
    s.execute(
        "INSERT INTO dbine_del._default._default (KEY, VALUE) VALUES ('O''Brien \"Bob\"', {'n': 1}), ('b', {'code': 'x''y', 'k': 2}), ('keep', {'code': 'x''y', 'k': 3})",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let obj = coll("dbine_del._default", "_default");
    let keys = vec![vec![("_id".to_string(), json!("O'Brien \"Bob\""))], vec![("code".to_string(), json!("x'y")), ("k".to_string(), json!(2))]];
    let script = d.delete_script(&obj, &keys).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&script, 10, &mut out).await.expect(&script);
    let mut out = QueryOutcome::default();
    s.execute("SELECT RAW META(d).id FROM dbine_del._default._default AS d", 10, &mut out).await.unwrap();
    let rows = serde_json::to_string(&out.results[0].rows).unwrap();
    assert_eq!(out.results[0].rows.len(), 1, "{rows}");
    assert!(rows.contains("keep"), "{rows}");
    d.connect(&c, None).await.unwrap().drop_database("dbine_del").await.unwrap();
}
