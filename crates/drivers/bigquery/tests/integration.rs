//! Against bigquery-emulator:
//!   docker run -d --name dbine-test-bigquery -p 25302:9050 ghcr.io/goccy/bigquery-emulator --project=test --dataset=ds1
//!   DBINE_TEST_BIGQUERY_URL=http://localhost:25302 cargo test -p dbine-driver-bigquery -- --ignored

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use serde_json::json;

async fn open(dataset: Option<&str>) -> Option<Box<dyn Session>> {
    let url = std::env::var("DBINE_TEST_BIGQUERY_URL").ok()?;
    let mut c = ConnectionConfig { driver: "bigquery".into(), ..Default::default() };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url);
    Some(dbine_driver_bigquery::drivers().pop().unwrap().connect(&c, dataset).await.unwrap())
}

#[tokio::test]
#[ignore]
async fn round_trip() {
    let Some(mut s) = open(Some("ds1")).await else { return };
    assert!(s.server_version().await.unwrap().contains("test"));
    assert!(s.list_databases().await.unwrap().contains(&"ds1".to_string()));

    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE IF EXISTS ds1.people", 10, &mut out).await;
    s.execute(
        "CREATE TABLE ds1.people (id INT64 NOT NULL, name STRING, score FLOAT64, tags ARRAY<STRING>, \
         addr STRUCT<city STRING, zip INT64>, ts TIMESTAMP)",
        100,
        &mut out,
    )
    .await
    .unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "INSERT INTO ds1.people (id, name, score, tags, addr, ts) VALUES \
         (1, 'ana', 1.5, ['a', 'b'], STRUCT('Rosario', 2000), TIMESTAMP '2024-01-31 13:45:00 UTC'), \
         (2, 'bob', NULL, [], NULL, NULL), (3, 'cy', 2.0, NULL, NULL, NULL)",
        100,
        &mut out,
    )
    .await
    .unwrap();

    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == kinds::TABLE && o.name == "people"), "{objs:?}");

    let t = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "people".into() };
    let cols = s.columns(&t).await.unwrap();
    let names: Vec<_> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "name", "score", "tags", "addr", "addr.city", "addr.zip", "ts"]);
    // (the emulator drops NOT NULL, so nullability is covered by unit tests)

    let mut out = QueryOutcome::default();
    s.execute(&(s.browse_query(&t, 100).replace("LIMIT 100", "ORDER BY id LIMIT 100")), 2, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!(r.rows.len(), 2);
    assert!(r.truncated);
    assert_eq!(
        r.rows[0],
        vec![json!(1), json!("ana"), json!(1.5), json!("[\"a\",\"b\"]"), json!("{\"city\":\"Rosario\",\"zip\":2000}"),
             json!("2024-01-31 13:45:00 UTC")]
    );

    let mut out = QueryOutcome::default();
    assert!(s.execute("SELECT nope FROM ds1.people", 10, &mut out).await.is_err());
}

/// The emulator answers dry runs with the statement type only (no bytes,
/// no referenced tables) and gives no queryPlan; the parsing of both is
/// covered by recorded fixtures in the unit tests. What's checked here:
/// the estimated plan doesn't run anything, the actual one runs once.
#[tokio::test]
#[ignore]
async fn plans() {
    let Some(mut s) = open(Some("ds1")).await else { return };
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE IF EXISTS ds1.plans_t", 10, &mut out).await;
    s.execute("CREATE TABLE ds1.plans_t (id INT64, g INT64)", 10, &mut out).await.unwrap();

    let mut out = QueryOutcome::default();
    s.explain("SELECT g, COUNT(*) FROM ds1.plans_t GROUP BY g; INSERT INTO ds1.plans_t VALUES (1, 1); CREATE TABLE ds1.nope (a INT64)", false, 10, &mut out)
        .await
        .unwrap();
    assert!(out.results.is_empty());
    assert_eq!(out.plans.len(), 2, "{out:?}");
    assert_eq!(out.plans[0].root.op, "SELECT");
    assert!(!out.plans[0].actual);
    assert_eq!(out.messages.len(), 1, "{:?}", out.messages);
    let mut out = QueryOutcome::default();
    s.execute("SELECT COUNT(*) FROM ds1.plans_t", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0], vec![json!(0)], "the dry run must not insert");

    let mut out = QueryOutcome::default();
    s.explain("INSERT INTO ds1.plans_t VALUES (1, 1), (2, 1)", true, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1);
    assert!(out.plans[0].actual);
    let mut out = QueryOutcome::default();
    s.explain("SELECT g, COUNT(*) AS n FROM ds1.plans_t GROUP BY g", true, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, vec![vec![json!(1), json!(2)]]);
    assert!(out.plans[0].actual);
}

/// Datasets, catalog and DDL round trip. The emulator keeps no
/// descriptions, NOT NULL or keys and rejects foreign keys, so those are
/// covered by the unit tests over recorded INFORMATION_SCHEMA rows; here:
/// create / drop dataset, the schema read from INFORMATION_SCHEMA, the
/// generated DDL (partitioning, clustering, PK, defaults, descriptions)
/// running on a fresh dataset, and the INSERT script's literals.
#[tokio::test]
#[ignore]
async fn ddl_round_trip() {
    use dbine_driver::DdlParts;
    let Some(mut admin) = open(Some("ds1")).await else { return };
    let driver = dbine_driver_bigquery::drivers().pop().unwrap();
    for d in ["ddl_src", "ddl_dst"] {
        let _ = admin.drop_database(d).await;
        admin.create_database(d).await.unwrap();
    }
    assert!(admin.list_databases().await.unwrap().contains(&"ddl_src".to_string()));
    assert!(admin.drop_database("ds1").await.is_err(), "the session's dataset can't be dropped");

    let mut src = open(Some("ddl_src")).await.unwrap();
    let mut out = QueryOutcome::default();
    src.execute(
        "CREATE TABLE clientes (id INT64 NOT NULL OPTIONS(description='clave'), nombre STRING(80), PRIMARY KEY (id) NOT ENFORCED) \
         OPTIONS(description='Clientes');
         CREATE TABLE pedidos (id INT64, cliente_id INT64, creado DATE, estado STRING DEFAULT 'nuevo', PRIMARY KEY (id) NOT ENFORCED) \
         PARTITION BY creado CLUSTER BY cliente_id;
         CREATE TABLE lineas (pedido_id INT64, n INT64, precio NUMERIC, tags ARRAY<STRING>, extra STRUCT<a INT64, b STRING>);
         CREATE VIEW v AS SELECT * FROM clientes",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let schema = src.database_schema().await.unwrap();
    assert_eq!(schema.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["clientes", "lineas", "pedidos"]);
    let lineas = &schema[1];
    assert_eq!(
        lineas.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect::<Vec<_>>(),
        vec![("pedido_id", "INT64"), ("n", "INT64"), ("precio", "NUMERIC"), ("tags", "ARRAY<STRING>"), ("extra", "STRUCT<a INT64, b STRING>")]
    );

    // Round trip with what the designer adds on top of the catalog.
    let mut tables = schema.clone();
    tables[0].comment = Some("Clientes 'VIP'".into());
    tables[0].columns[1].comment = Some("nombre\ncompleto".into());
    tables[2].options.insert("partition_by".into(), "creado".into());
    tables[2].options.insert("cluster_by".into(), "cliente_id".into());
    tables[2].columns[3].default_value = Some("'nuevo'".into());
    tables[2].primary_key = Some(dbine_driver::KeyDef { name: None, columns: vec!["id".into()] });
    let mut script: Vec<String> = tables
        .iter()
        .map(|t| driver.table_ddl(t, DdlParts { create: true, drop: true, if_exists: true, ..Default::default() }).unwrap())
        .collect();
    script.extend(tables.iter().map(|t| driver.table_ddl(t, DdlParts { indexes: true, foreign_keys: true, ..Default::default() }).unwrap()));
    let script = script.join("\n");
    assert!(script.contains("PARTITION BY creado\nCLUSTER BY `cliente_id`"), "{script}");
    let mut dst = open(Some("ddl_dst")).await.unwrap();
    for stmt in script.split(";\n").filter(|s| !s.trim().is_empty()) {
        dst.execute(stmt, 10, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
    let copy = dst.database_schema().await.unwrap();
    let shape = |s: &[dbine_driver::TableSchema]| {
        s.iter().map(|t| (t.name.clone(), t.columns.iter().map(|c| (c.name.clone(), c.data_type.clone())).collect::<Vec<_>>())).collect::<Vec<_>>()
    };
    assert_eq!(shape(&copy), shape(&schema));

    // INSERT script literals.
    let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("ddl_dst".into()), name: "clientes".into() };
    let ins = driver
        .insert_script(&target, &["id".into(), "nombre".into()], &[vec![json!(1), json!("O'Brien \\ \"x\"\ny")], vec![json!(2), json!(null)]])
        .unwrap();
    dst.execute(&ins, 10, &mut QueryOutcome::default()).await.unwrap();
    let mut out = QueryOutcome::default();
    dst.execute("SELECT nombre FROM ddl_dst.clientes ORDER BY id", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, vec![vec![json!("O'Brien \\ \"x\"\ny")], vec![json!(null)]]);

    drop((src, dst));
    for d in ["ddl_src", "ddl_dst"] {
        admin.drop_database(d).await.unwrap();
    }
    assert!(!admin.list_databases().await.unwrap().contains(&"ddl_src".to_string()));
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(mut s) = open(Some("ds1")).await else { return };
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE mon_t", 10, &mut out).await;
    s.execute("CREATE TABLE mon_t (id INT64); SELECT 1", 10, &mut out).await.unwrap();
    let snap = s.monitor().await.unwrap();
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    eprintln!(
        "{:?}\ninfo {:?}\nnotes {:?}\ntables {:?}",
        snap.metrics.iter().map(|m| (m.key.as_str(), m.value)).collect::<Vec<_>>(),
        snap.info,
        snap.notes,
        snap.tables.iter().map(|t| (t.key.as_str(), t.rows.len())).collect::<Vec<_>>()
    );
    assert!(v("active_sessions").is_some());
    assert!(v("recent_jobs").unwrap() >= 1.0, "the SELECT above is a recent job");
    let recent = snap.tables.iter().find(|t| t.key == "recent_queries").unwrap();
    assert!(!recent.rows.is_empty());
    assert!(snap.tables.iter().any(|t| t.key == "top_objects" && !t.rows.is_empty()));
    // Cached: a second snapshot doesn't re-read storage.
    s.monitor().await.unwrap();
}

/// The profiler sees a job run from another session, once. The emulator
/// keeps job times in whole seconds (so the work starts a second after the
/// profiler) and lists jobs without their configuration (read one by one).
#[tokio::test]
#[ignore]
async fn profile() {
    let Some(mut p) = open(Some("ds1")).await else { return };
    let Some(mut w) = open(Some("ds1")).await else { return };
    assert!(dbine_driver_bigquery::drivers().pop().unwrap().supports_profiler());
    let opts = dbine_driver::ProfilerOptions { database: "ds1".into(), change_server: false };
    let started = p.profiler_start(&opts).await.unwrap();
    eprintln!("{started:?}");
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let marker = format!("dbine_prof_{}", std::process::id());
    let mut out = QueryOutcome::default();
    w.execute(&format!("SELECT 1 AS {marker}"), 10, &mut out).await.unwrap();
    let mut got = Vec::new();
    for _ in 0..8 {
        got.extend(p.profiler_poll().await.unwrap());
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
    p.profiler_stop().await.unwrap();
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{mine:#?}");
    assert_eq!(mine.len(), 1, "seen once: {got:#?}");
    assert!(mine[0].duration_ms.is_some() && mine[0].time.len() == 23);
}

/// Schema sync: each statement of the script against the emulator.
/// bigquery-emulator (goccy) implements only part of ALTER TABLE, so the
/// ones it refuses are printed and counted, not failed on.
#[tokio::test]
#[ignore]
async fn schema_sync() {
    use dbine_driver::{ColumnDef, TableChange};
    let Some(mut s) = open(Some("ds1")).await else { return };
    let d = dbine_driver_bigquery::drivers().pop().unwrap();
    assert!(d.supports_schema_sync());
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE IF EXISTS ds1.sync_t", 10, &mut out).await;
    s.execute("CREATE TABLE ds1.sync_t (id INT64 NOT NULL, nombre STRING(10) NOT NULL, baja DATE, monto INT64)", 10, &mut out).await.unwrap();
    s.execute("INSERT INTO ds1.sync_t (id, nombre, baja, monto) VALUES (1, 'uno', DATE '2024-01-01', 5)", 10, &mut out).await.unwrap();
    let old = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "sync_t").unwrap();
    let mut new = old.clone();
    new.columns.retain(|c| c.name != "baja");
    let nombre = new.columns.iter_mut().find(|c| c.name == "nombre").unwrap();
    nombre.data_type = "STRING(20)".into();
    nombre.nullable = true;
    new.columns.iter_mut().find(|c| c.name == "monto").unwrap().data_type = "NUMERIC".into();
    new.columns.push(ColumnDef { name: "email".into(), data_type: "STRING".into(), nullable: true, default_value: Some("'x@y'".into()), ..Default::default() });
    new.primary_key = Some(dbine_driver::KeyDef { name: None, columns: vec!["id".into()] });
    let script = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
    let mut refused = Vec::new();
    for stmt in &script.statements {
        if let Err(e) = s.execute(stmt, 10, &mut out).await {
            refused.push(format!("{stmt}\n  → {e}"));
        }
    }
    eprintln!("{} sentencias, {} rechazadas por el emulador:\n{}", script.statements.len(), refused.len(), refused.join("\n"));
    let now = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "sync_t").unwrap();
    eprintln!("{:#?}", now.columns.iter().map(|c| (&c.name, &c.data_type, c.nullable)).collect::<Vec<_>>());
}
