//! Against a real Trino:
//!
//! ```sh
//! docker run -d --name dbine-test-trino -p 25180:8080 trinodb/trino
//! DBINE_TEST_TRINO_URL=http://localhost:25180 cargo test -p dbine-driver-trino -- --ignored
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::time::{Duration, Instant};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TRINO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "trino".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    })
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some("dbine_it".into()), name: name.into() }
}

#[tokio::test]
#[ignore]
async fn trino() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_trino::drivers().remove(0);

    let mut bad = c.clone();
    bad.database = "nope".into();
    assert!(d.connect(&bad, None).await.is_err());

    let mut s = d.connect(&c, None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("Trino "));
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"memory".to_string()) && dbs.contains(&"tpch".to_string()), "{dbs:?}");

    let mut out = QueryOutcome::default();
    s.execute(
        "DROP SCHEMA IF EXISTS dbine_it CASCADE; CREATE SCHEMA dbine_it; USE memory.dbine_it;
         CREATE TABLE t (id bigint NOT NULL, name varchar, d decimal(10,2), b varbinary, a array(integer));
         INSERT INTO t SELECT n, 'n' || cast(n AS varchar), cast(n AS decimal(10,2)) / 4, X'CAFE', ARRAY[1,2]
           FROM UNNEST(sequence(1, 20)) AS x(n);
         CREATE VIEW v AS SELECT id FROM t;",
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[4].rows_affected, Some(20));

    // USE changed the session schema: an unqualified name resolves.
    let mut out = QueryOutcome::default();
    s.execute("SELECT count(*) FROM t", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(20));

    let objs = s.list_objects().await.unwrap();
    let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n && o.schema.as_deref() == Some("dbine_it"));
    assert!(has(kinds::TABLE, "t") && has(kinds::VIEW, "v"), "{objs:?}");
    let cols = s.columns(&obj(kinds::TABLE, "t")).await.unwrap();
    assert_eq!(cols.len(), 5);
    assert!(!cols[0].nullable && cols[1].nullable && cols[0].data_type == "bigint");
    for (k, n) in [(kinds::TABLE, "t"), (kinds::VIEW, "v")] {
        let def = s.definition(&obj(k, n)).await.unwrap().unwrap();
        assert!(def.starts_with("CREATE"), "{def}");
    }

    let q = s.browse_query(&obj(kinds::TABLE, "t"), 10);
    let mut out = QueryOutcome::default();
    s.execute(&format!("{q}"), 4, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!((r.rows.len(), r.total_rows, r.truncated), (4, 10, true));
    let row = &r.rows[0];
    assert_eq!(row[3], serde_json::json!("0xCAFE"));
    assert_eq!(row[4], serde_json::json!("[1,2]"));

    // SET SESSION and PREPARE persist between runs.
    let mut out = QueryOutcome::default();
    s.execute("SET SESSION query_max_run_time = '1h'; PREPARE p FROM SELECT ? + 1", 10, &mut out).await.unwrap();
    s.execute("EXECUTE p USING 41", 10, &mut out).await.unwrap();
    assert_eq!(out.results.last().unwrap().rows[0][0], serde_json::json!(42));

    // Error mid-script.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1; SELECT * FROM nope; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // Cancel.
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        stop();
    });
    let t = Instant::now();
    let mut out = QueryOutcome::default();
    let r = s.execute("SELECT sum(quantity) FROM tpch.sf1000.lineitem", 10, &mut out).await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(20));

    // Read-only (the registry wraps SQL sessions).
    let mut ro = ReadOnlySession::new(d.connect(&c, None).await.unwrap());
    let mut out = QueryOutcome::default();
    assert!(ro.execute("DROP SCHEMA dbine_it CASCADE", 10, &mut out).await.is_err());

    let mut out = QueryOutcome::default();
    s.execute("DROP SCHEMA memory.dbine_it CASCADE", 10, &mut out).await.unwrap();
}

/// `docker run -d --name dbine-test-presto -p 25181:8080 prestodb/presto`,
/// then `DBINE_TEST_PRESTO_URL=http://localhost:25181`.
#[tokio::test]
#[ignore]
async fn presto() {
    let Ok(url) = std::env::var("DBINE_TEST_PRESTO_URL") else { return };
    let url = reqwest::Url::parse(&url).unwrap();
    let c = ConnectionConfig {
        driver: "presto".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        ..Default::default()
    };
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == "presto").unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("Presto "));
    let dbs = s.list_databases().await.unwrap();
    assert!(!dbs.is_empty());
    let mut s = d.connect(&c, Some(&dbs[0])).await.unwrap();
    let objs = s.list_objects().await.unwrap();
    if let Some(o) = objs.first() {
        let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        assert!(!s.columns(&r).await.unwrap().is_empty());
    }
    let mut out = QueryOutcome::default();
    s.execute("SELECT x FROM UNNEST(sequence(1, 10)) AS t(x); SET SESSION query_max_run_time = '1h'", 3, &mut out).await.unwrap();
    assert_eq!((out.results[0].rows.len(), out.results[0].total_rows), (3, 10));
    let e = s.execute("SELECT * FROM nope", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
}

#[tokio::test]
#[ignore]
async fn trino_plans() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_trino::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP SCHEMA IF EXISTS dbine_plan CASCADE; CREATE SCHEMA dbine_plan; USE memory.dbine_plan;
         CREATE TABLE t (id bigint, g bigint);
         INSERT INTO t SELECT n, n % 10 FROM UNNEST(sequence(1, 100)) AS x(n);",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let count = |out: &QueryOutcome, i: usize| out.results[i].rows[0][0].to_string().trim_matches('"').to_string();

    // Estimated: nothing runs.
    let q = "SELECT n.name, count(*) FROM tpch.tiny.nation n JOIN tpch.tiny.customer c ON c.nationkey = n.nationkey GROUP BY n.name";
    let mut out = QueryOutcome::default();
    s.explain(&format!("{q}; INSERT INTO t VALUES (1000, 0)"), false, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 2);
    assert!(out.results.is_empty());
    assert_eq!(out.plans[0].root.op, "Output");
    fn find<'a>(n: &'a dbine_driver::PlanNode, op: &str) -> Option<&'a dbine_driver::PlanNode> {
        if n.op == op {
            return Some(n);
        }
        n.children.iter().find_map(|c| find(c, op))
    }
    // The fragments are stitched into one tree down to the scans.
    assert!(find(&out.plans[0].root, "TableScan").or_else(|| find(&out.plans[0].root, "ScanProject")).is_some(), "{:#?}", out.plans[0].root);
    assert!(out.plans[0].root.total_cost.is_some());
    let mut check = QueryOutcome::default();
    s.execute("SELECT count(*) FROM t", 10, &mut check).await.unwrap();
    assert_eq!(count(&check, 0), "100");

    // Actual: runs once; reads carry measured rows.
    let mut out = QueryOutcome::default();
    s.explain("SELECT count(*) FROM t WHERE g = 3; INSERT INTO t VALUES (1000, 0); SELECT count(*) FROM t", true, 10, &mut out)
        .await
        .unwrap();
    assert_eq!(out.results.len(), 3);
    assert_eq!(count(&out, 0), "10");
    assert_eq!(count(&out, 2), "101");
    assert!(out.plans[0].actual && !out.plans[1].actual && out.plans[2].actual);
    fn any_actual(n: &dbine_driver::PlanNode) -> bool {
        n.actual_rows.is_some() || n.children.iter().any(any_actual)
    }
    assert!(any_actual(&out.plans[0].root), "{:#?}", out.plans[0].root);

    let mut out = QueryOutcome::default();
    assert!(s.explain("SELECT 1; SELECT * FROM missing_table", true, 10, &mut out).await.is_err());
    assert_eq!(out.results.len(), 1);
    s.execute("DROP SCHEMA dbine_plan CASCADE", 10, &mut QueryOutcome::default()).await.unwrap();
}

/// Designer DDL, `database_schema` and INSERT scripts round trip on the
/// memory connector (Trino has no keys or indexes: columns, NOT NULL,
/// defaults and comments).
#[tokio::test]
#[ignore]
async fn trino_ddl() {
    use dbine_driver::{DdlParts, TableSchema};
    let Some(c) = cfg() else { return };
    let d = dbine_driver_trino::drivers().remove(0);
    assert!(!d.capabilities().create_database && !d.capabilities().foreign_keys);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP SCHEMA IF EXISTS dbine_ddl CASCADE; DROP SCHEMA IF EXISTS dbine_ddl2 CASCADE;
         CREATE SCHEMA dbine_ddl; CREATE SCHEMA dbine_ddl2;
         CREATE TABLE dbine_ddl.clientes (id bigint NOT NULL COMMENT 'clave', nombre varchar(50) NOT NULL, alta date, estado varchar(10) DEFAULT 'nuevo')
           COMMENT 'Clientes de o''k';
         CREATE TABLE dbine_ddl.pedidos (id bigint NOT NULL, cliente_id bigint COMMENT 'cliente', total decimal(10,2), ts timestamp(3), tags array(varchar));
         CREATE TABLE dbine_ddl.lineas (pedido_id bigint, n integer DEFAULT 1, bin varbinary, ok boolean);
         CREATE VIEW dbine_ddl.v AS SELECT id FROM dbine_ddl.clientes;",
        10,
        &mut out,
    )
    .await
    .unwrap();

    let only = |all: Vec<TableSchema>, schema: &str| -> Vec<TableSchema> {
        all.into_iter().filter(|t| t.schema.as_deref() == Some(schema)).collect()
    };
    let src = only(s.database_schema().await.unwrap(), "dbine_ddl");
    assert_eq!(src.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["clientes", "lineas", "pedidos"]);
    let cl = &src[0];
    assert_eq!(cl.comment.as_deref(), Some("Clientes de o'k"));
    assert_eq!((cl.columns[0].data_type.as_str(), cl.columns[0].nullable, cl.columns[0].comment.as_deref()), ("bigint", false, Some("clave")));
    assert_eq!(cl.columns[1].data_type, "varchar(50)");
    assert_eq!(cl.columns[3].default_value.as_deref(), Some("'nuevo'"));
    assert_eq!(src[2].columns[1].comment.as_deref(), Some("cliente"));
    assert_eq!(src[2].columns[4].data_type, "array(varchar)");

    // Round trip into another schema: every create, then (no-op) indexes / keys.
    let moved: Vec<TableSchema> = src.iter().cloned().map(|mut t| {
        t.schema = Some("dbine_ddl2".into());
        t
    }).collect();
    let mut script: Vec<String> = moved.iter().map(|t| d.table_ddl(t, DdlParts { create: true, ..Default::default() }).unwrap()).collect();
    script.extend(moved.iter().map(|t| d.table_ddl(t, DdlParts { indexes: true, foreign_keys: true, ..Default::default() }).unwrap()));
    s.execute(&script.join("\n"), 10, &mut QueryOutcome::default()).await.unwrap();
    let copy = only(s.database_schema().await.unwrap(), "dbine_ddl2");
    assert_eq!(copy, moved);

    // drop + if_exists re-creates.
    let again = d.table_ddl(&moved[0], DdlParts { drop: true, if_exists: true, create: true, ..Default::default() }).unwrap();
    s.execute(&again, 10, &mut QueryOutcome::default()).await.unwrap();

    // INSERT scripts with dates, timestamps, binaries, decimals, arrays as text are not coerced.
    let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbine_ddl2".into()), name: "pedidos".into() };
    let rows = vec![
        vec![serde_json::json!(1), serde_json::json!(7), serde_json::json!(12.5), serde_json::json!("2024-01-31 10:00:00.123"), serde_json::Value::Null],
        vec![serde_json::json!(2), serde_json::Value::Null, serde_json::Value::Null, serde_json::Value::Null, serde_json::Value::Null],
    ];
    let cols: Vec<String> = ["id", "cliente_id", "total", "ts", "tags"].map(String::from).to_vec();
    let ins = d.insert_script(&target, &cols, &rows).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&ins, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows_affected, Some(2));
    let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbine_ddl2".into()), name: "lineas".into() };
    let cols: Vec<String> = ["pedido_id", "n", "bin", "ok"].map(String::from).to_vec();
    let ins = d.insert_script(&target, &cols, &[vec![serde_json::json!(1), serde_json::json!(2), serde_json::json!("0xCAFE"), serde_json::json!(true)]]).unwrap();
    s.execute(&ins, 10, &mut QueryOutcome::default()).await.unwrap();
    let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbine_ddl2".into()), name: "clientes".into() };
    let cols: Vec<String> = ["id", "nombre", "alta"].map(String::from).to_vec();
    let ins = d.insert_script(&target, &cols, &[vec![serde_json::json!(1), serde_json::json!("O'Brien"), serde_json::json!("2024-01-31")]]).unwrap();
    s.execute(&ins, 10, &mut QueryOutcome::default()).await.unwrap();

    // Templates run once their placeholders are filled (view and function; no MVs in memory).
    for t in d.create_templates().iter().filter(|t| t.kind == kinds::VIEW) {
        let sql = t.template.replace("{schema}", "dbine_ddl2").replace("{name}", "v_tpl").replace("\"tabla\" t\nWHERE t.activo = true", "\"clientes\" t");
        s.execute(&sql, 10, &mut QueryOutcome::default()).await.unwrap();
    }

    assert!(matches!(s.create_database("x").await, Err(Error::Unsupported(_))));
    s.execute("DROP SCHEMA dbine_ddl CASCADE; DROP SCHEMA dbine_ddl2 CASCADE", 10, &mut QueryOutcome::default()).await.unwrap();
}

/// Presto's memory connector keeps no comments and takes no NOT NULL:
/// columns, round trip and INSERT scripts.
#[tokio::test]
#[ignore]
async fn presto_ddl() {
    use dbine_driver::{DdlParts, TableSchema};
    let Ok(url) = std::env::var("DBINE_TEST_PRESTO_URL") else { return };
    let url = reqwest::Url::parse(&url).unwrap();
    let c = ConnectionConfig {
        driver: "presto".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    };
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == "presto").unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let cleanup = "DROP TABLE dbine_ddl.a; DROP TABLE dbine_ddl2.a; DROP TABLE dbine_ddl.b; DROP TABLE dbine_ddl2.b; DROP SCHEMA dbine_ddl; DROP SCHEMA dbine_ddl2";
    for stmt in cleanup.split(';') {
        let _ = s.execute(stmt, 10, &mut QueryOutcome::default()).await;
    }
    s.execute(
        "CREATE SCHEMA dbine_ddl; CREATE SCHEMA dbine_ddl2;
         CREATE TABLE dbine_ddl.a (id bigint, nombre varchar(50), alta date, ts timestamp, total decimal(10,2));
         CREATE TABLE dbine_ddl.b (x integer, bin varbinary, ok boolean, tags array(varchar));",
        10,
        &mut QueryOutcome::default(),
    )
    .await
    .unwrap();
    let only = |all: Vec<TableSchema>, schema: &str| -> Vec<TableSchema> {
        all.into_iter().filter(|t| t.schema.as_deref() == Some(schema)).collect()
    };
    let src = only(s.database_schema().await.unwrap(), "dbine_ddl");
    assert_eq!(src.len(), 2);
    assert_eq!(src[0].columns[1].data_type, "varchar(50)");
    let moved: Vec<TableSchema> = src.iter().cloned().map(|mut t| {
        t.schema = Some("dbine_ddl2".into());
        t
    }).collect();
    let script: Vec<String> = moved.iter().map(|t| d.table_ddl(t, DdlParts { create: true, if_exists: true, ..Default::default() }).unwrap()).collect();
    s.execute(&script.join("\n"), 10, &mut QueryOutcome::default()).await.unwrap();
    assert_eq!(only(s.database_schema().await.unwrap(), "dbine_ddl2"), moved);
    let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbine_ddl2".into()), name: "a".into() };
    let cols: Vec<String> = ["id", "nombre", "alta", "ts"].map(String::from).to_vec();
    let rows = vec![vec![serde_json::json!(1), serde_json::json!("O'Brien"), serde_json::json!("2024-01-31"), serde_json::json!("2024-01-31 10:00:00.123")]];
    let mut out = QueryOutcome::default();
    s.execute(&d.insert_script(&target, &cols, &rows).unwrap(), 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows_affected, Some(1));
    let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbine_ddl2".into()), name: "b".into() };
    let cols: Vec<String> = ["x", "bin", "ok"].map(String::from).to_vec();
    s.execute(&d.insert_script(&target, &cols, &[vec![serde_json::json!(1), serde_json::json!("0xCAFE"), serde_json::json!(false)]]).unwrap(), 10, &mut QueryOutcome::default())
        .await
        .unwrap();
    s.execute(cleanup, 10, &mut QueryOutcome::default()).await.unwrap();
}

/// The monitor snapshot: coordinator figures, JMX totals, nodes, queries.
async fn check_monitor(driver: &str, c: &ConnectionConfig) {
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().monitor);
    let mut s = d.connect(c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT count(*) FROM tpch.tiny.lineitem", 10, &mut out).await.unwrap();
    let snap = s.monitor().await.unwrap();
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    eprintln!(
        "{driver}: {:?}\ninfo {:?}\nnotes {:?}\ntables {:?}",
        snap.metrics.iter().map(|m| (m.key.as_str(), m.value, m.max)).collect::<Vec<_>>(),
        snap.info,
        snap.notes,
        snap.tables.iter().map(|t| (t.key.as_str(), t.rows.len())).collect::<Vec<_>>()
    );
    assert!(v("cpu").is_some() && v("mem_used").is_some() && v("uptime").is_some());
    assert!(v("queries").unwrap_or(0.0) >= 1.0 || v("active_sessions").is_some());
    assert!(v("nodes").unwrap() >= 1.0);
    let nodes = snap.tables.iter().find(|t| t.key == "nodes").unwrap();
    assert!(!nodes.rows.is_empty());
    let recent = snap.tables.iter().find(|t| t.key == "recent_queries").unwrap();
    assert!(!recent.rows.is_empty());
    // The monitor's own statements don't list themselves.
    assert!(recent.rows.iter().all(|r| !r[7].as_str().unwrap_or("").contains("dbine-monitor")));
}

#[tokio::test]
#[ignore]
async fn trino_monitor() {
    let Some(c) = cfg() else { return };
    check_monitor("trino", &c).await;
}

#[tokio::test]
#[ignore]
async fn presto_monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_PRESTO_URL") else { return };
    let url = reqwest::Url::parse(&url).unwrap();
    let c = ConnectionConfig {
        driver: "presto".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        ..Default::default()
    };
    check_monitor("presto", &c).await;
}

async fn run(s: &mut Box<dyn Session>, sql: String) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.execute(&sql, 100, &mut out).await.map(|_| out)
}

/// The profiler: one session profiles a catalog while another runs a slow
/// statement and a fast one there, and a third one elsewhere.
async fn profile(driver: &str, url: &str, catalog: &str, other: &str) {
    let url = reqwest::Url::parse(url).unwrap();
    let c = ConnectionConfig {
        driver: driver.into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        ..Default::default()
    };
    let d = dbine_driver_trino::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.supports_profiler());
    let mut p = d.connect(&c, Some(catalog)).await.unwrap();
    let mut w = d.connect(&c, Some(catalog)).await.unwrap();
    let mut elsewhere = d.connect(&c, Some(other)).await.unwrap();
    let opts = dbine_driver::ProfilerOptions { database: catalog.into(), change_server: true };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{driver}: {started:?}");
    let marker = format!("dbine_prof_{}", std::process::id());
    let work = async {
        run(&mut w, format!("SELECT count(*) AS {marker}_slow FROM tpch.tiny.lineitem")).await.expect("slow");
        run(&mut w, format!("SELECT 1 AS {marker}_fast")).await.expect("fast");
        run(&mut elsewhere, format!("SELECT 2 AS {marker}_other")).await.expect("other");
        assert!(run(&mut w, format!("SELECT {marker}_bad FROM nope")).await.is_err());
    };
    let watch = async {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            if got.iter().filter(|s| s.text.contains(&marker)).count() >= 3 && Instant::now() + Duration::from_secs(5) > until {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{driver}: {mine:#?}");
    let slow: Vec<_> = mine.iter().filter(|s| s.text.contains("_slow")).collect();
    assert_eq!(slow.len(), 1, "{driver}: the slow statement once");
    assert!(slow[0].duration_ms.unwrap_or(0.0) > 0.0, "{driver}: {:?}", slow[0].duration_ms);
    assert_eq!(mine.iter().filter(|s| s.text.contains("_fast")).count(), 1, "{driver}: the fast statement once");
    assert!(!mine.iter().any(|s| s.text.contains("_other")), "{driver}: other catalogs are left out");
    let bad: Vec<_> = mine.iter().filter(|s| s.text.contains("_bad")).collect();
    assert!(bad.len() == 1 && bad[0].error.is_some(), "{driver}: the failed statement, with its error");
}

#[tokio::test]
#[ignore]
async fn trino_profiler() {
    let Ok(url) = std::env::var("DBINE_TEST_TRINO_URL") else { return };
    profile("trino", &url, "memory", "system").await;
}

#[tokio::test]
#[ignore]
async fn presto_profiler() {
    let Ok(url) = std::env::var("DBINE_TEST_PRESTO_URL") else { return };
    profile("presto", &url, "tpch", "system").await;
}
