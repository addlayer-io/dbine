//! Against the Cloud Spanner emulator's REST gateway:
//!   docker run -d --name dbine-test-spanner -p 25303:9020 gcr.io/cloud-spanner-emulator/emulator
//!   DBINE_TEST_SPANNER_URL=http://localhost:25303 cargo test -p dbine-driver-spanner -- --ignored
//! The test creates instance `i1` and database `db1` in project `test` if missing.

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome};
use serde_json::json;

#[tokio::test]
#[ignore]
async fn round_trip() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http
        .post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": "CREATE DATABASE `db1`" }))
        .send()
        .await;

    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "db1".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    let mut s = dbine_driver_spanner::drivers().pop().unwrap().connect(&cfg, None).await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"db1".to_string()));

    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP VIEW v_people; DROP TABLE people", 10, &mut out).await;
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE people (id INT64 NOT NULL, name STRING(20), score FLOAT64, raw BYTES(10), tags ARRAY<STRING(5)>) PRIMARY KEY (id);
         CREATE VIEW v_people SQL SECURITY INVOKER AS SELECT people.id, people.name FROM people;
         INSERT INTO people (id, name, score, raw, tags) VALUES (1, 'ana', 1.5, b'ab', ['x']), (2, 'bob', NULL, NULL, NULL)",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results.last().unwrap().rows_affected, Some(2));

    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == kinds::TABLE && o.name == "people"), "{objs:?}");
    assert!(objs.iter().any(|o| o.kind == kinds::VIEW && o.name == "v_people"));

    let t = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "people".into() };
    let cols = s.columns(&t).await.unwrap();
    assert_eq!(cols[0].name, "id");
    assert!(cols[0].primary_key && !cols[0].nullable);
    assert_eq!(cols[1].data_type, "STRING(20)");
    assert!(s.definition(&t).await.unwrap().unwrap().starts_with("CREATE TABLE people"));

    let mut out = QueryOutcome::default();
    s.execute(&s.browse_query(&t, 10).replace("LIMIT", "ORDER BY id LIMIT"), 10, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!(r.rows[0], vec![json!(1), json!("ana"), json!(1.5), json!("0x6162"), json!("[\"x\"]")]);
    assert_eq!(r.rows[1][2], json!(null));

    let mut out = QueryOutcome::default();
    assert!(s.execute("SELECT nope FROM people", 10, &mut out).await.is_err());

    // Plans. The emulator answers PLAN / PROFILE with a "No query plan"
    // node; what's checked is that nothing runs for PLAN and that PROFILE
    // runs once and returns rows, counts and stats.
    let mut out = QueryOutcome::default();
    s.explain("SELECT id FROM people WHERE id > 0; INSERT INTO people (id) VALUES (50); CREATE TABLE nope (a INT64) PRIMARY KEY (a)", false, 10, &mut out)
        .await
        .unwrap();
    assert_eq!(out.plans.len(), 2, "{out:?}");
    assert!(out.results.is_empty() && !out.plans[0].actual);
    assert_eq!(out.messages.len(), 1);
    let mut out = QueryOutcome::default();
    s.execute("SELECT COUNT(*) FROM people WHERE id = 50", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0], vec![json!(0)], "PLAN must not run the INSERT");

    let mut out = QueryOutcome::default();
    s.explain("SELECT id FROM people ORDER BY id; INSERT INTO people (id) VALUES (50)", true, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 2);
    assert!(out.plans[0].actual);
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.results[1].rows_affected, Some(1));
    assert!(out.plans[0].root.actual_ms.is_some());
    let mut out = QueryOutcome::default();
    s.execute("SELECT COUNT(*) FROM people WHERE id = 50", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0], vec![json!(1)], "PROFILE runs the INSERT once");
}

/// Catalog → GoogleSQL DDL → a fresh database, plus create / drop database.
#[tokio::test]
#[ignore]
async fn schema_ddl_round_trip() {
    use dbine_driver::DdlParts;
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http
        .post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": "CREATE DATABASE `db1`" }))
        .send()
        .await;
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "db1".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    let driver = dbine_driver_spanner::drivers().pop().unwrap();
    let mut admin = driver.connect(&cfg, None).await.unwrap();
    for db in ["ddl_src", "ddl_dst"] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.unwrap();
    }
    assert!(admin.list_databases().await.unwrap().contains(&"ddl_dst".to_string()));
    assert!(admin.drop_database("db1").await.is_err(), "the session's own database");

    let mut src = driver.connect(&cfg, Some("ddl_src")).await.unwrap();
    let mut out = QueryOutcome::default();
    src.execute(
        "CREATE TABLE clientes (id INT64 NOT NULL GENERATED BY DEFAULT AS IDENTITY (BIT_REVERSED_POSITIVE), email STRING(120) NOT NULL,
                                alta TIMESTAMP DEFAULT (CURRENT_TIMESTAMP())) PRIMARY KEY (id);
         CREATE TABLE pedidos (id INT64 NOT NULL, cliente_id INT64, total NUMERIC, doble INT64 AS (id * 2) STORED,
                               CONSTRAINT fk_ped_cli FOREIGN KEY (cliente_id) REFERENCES clientes (id) ON DELETE CASCADE) PRIMARY KEY (id);
         CREATE TABLE pedidos_lineas (id INT64 NOT NULL, n INT64 NOT NULL, cliente_id INT64, producto STRING(MAX)) PRIMARY KEY (id, n),
           INTERLEAVE IN PARENT pedidos ON DELETE CASCADE;
         ALTER TABLE pedidos_lineas ADD CONSTRAINT fk_lin_cli FOREIGN KEY (cliente_id) REFERENCES clientes (id);
         CREATE UNIQUE INDEX ux_email ON clientes (email);
         CREATE NULL_FILTERED INDEX ix_total ON pedidos (total, cliente_id);
         CREATE SCHEMA ventas;
         CREATE TABLE ventas.notas (id INT64 NOT NULL, texto STRING(50)) PRIMARY KEY (id);
         CREATE INDEX ventas.ix_texto ON ventas.notas (texto);
         CREATE TABLE ventas.notas_det (id INT64 NOT NULL, k INT64 NOT NULL) PRIMARY KEY (id, k), INTERLEAVE IN PARENT ventas.notas",
        10,
        &mut out,
    )
    .await
    .unwrap();

    let schema = src.database_schema().await.unwrap();
    let names: Vec<(Option<&str>, &str)> = schema.iter().map(|t| (t.schema.as_deref(), t.name.as_str())).collect();
    assert_eq!(names, [(None, "clientes"), (None, "pedidos"), (None, "pedidos_lineas"), (Some("ventas"), "notas"), (Some("ventas"), "notas_det")]);
    assert_eq!(schema[4].options.get("interleave_in_parent").map(String::as_str), Some("ventas.notas"));
    let clientes = &schema[0];
    assert!(clientes.columns[0].auto_increment && !clientes.columns[0].nullable);
    assert_eq!(clientes.columns[1].data_type, "STRING(120)");
    assert_eq!(clientes.columns[2].default_value.as_deref(), Some("CURRENT_TIMESTAMP()"));
    assert_eq!(clientes.indexes.len(), 1);
    assert!(clientes.indexes[0].unique && clientes.indexes[0].columns == ["email"]);
    let pedidos = &schema[1];
    assert_eq!(pedidos.columns[3].options.get("generated_as").map(String::as_str), Some("id * 2"));
    assert_eq!(pedidos.indexes.len(), 1, "FK backing indexes are left out: {:?}", pedidos.indexes);
    assert_eq!(pedidos.indexes[0].kind.as_deref(), Some("NULL_FILTERED"));
    assert_eq!(pedidos.indexes[0].columns, ["total", "cliente_id"]);
    let fk = &pedidos.foreign_keys[0];
    assert_eq!((fk.name.as_deref(), fk.ref_table.as_str(), fk.on_delete.as_deref()), (Some("fk_ped_cli"), "clientes", Some("CASCADE")));
    let lineas = &schema[2];
    assert_eq!(lineas.primary_key.as_ref().unwrap().columns, ["id", "n"]);
    assert_eq!(lineas.options.get("interleave_in_parent").map(String::as_str), Some("pedidos"));
    assert_eq!(lineas.options.get("on_delete").map(String::as_str), Some("CASCADE"));
    assert_eq!(lineas.foreign_keys[0].on_delete, None);
    assert_eq!(schema[3].indexes[0].name, "ix_texto");

    // Round trip: every CREATE first, then indexes and foreign keys.
    let mut script: Vec<String> = vec!["CREATE SCHEMA ventas;".into()];
    script.extend(schema.iter().map(|t| driver.table_ddl(t, DdlParts { create: true, if_exists: true, ..Default::default() }).unwrap()));
    script.extend(schema.iter().map(|t| driver.table_ddl(t, DdlParts { indexes: true, foreign_keys: true, ..Default::default() }).unwrap()));
    let script = script.join("\n");
    let mut dst = driver.connect(&cfg, Some("ddl_dst")).await.unwrap();
    let mut out = QueryOutcome::default();
    dst.execute(&script, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{script}"));
    let copied = dst.database_schema().await.unwrap();
    assert_eq!(copied, schema);
    // DROP (indexes first; interleaved children before) + CREATE runs again over the copy.
    let redo = driver.table_ddl(&schema[4], DdlParts { drop: true, ..Default::default() }).unwrap()
        + "\n"
        + &driver.table_ddl(&schema[3], DdlParts { drop: true, if_exists: true, create: true, indexes: true, ..Default::default() }).unwrap();
    let mut out = QueryOutcome::default();
    dst.execute(&redo, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{redo}"));

    let target = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "clientes".into() };
    let ins = driver
        .insert_script(&target, &["email".into(), "alta".into()], &[vec![json!("o'brien\\x@a.com"), json!(null)], vec![json!("b@a.com"), json!("2024-01-31T10:00:00Z")]])
        .unwrap();
    let mut out = QueryOutcome::default();
    dst.execute(&ins, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{ins}"));
    assert_eq!(out.results[0].rows_affected, Some(2));
    let mut out = QueryOutcome::default();
    dst.execute("SELECT email FROM clientes WHERE email LIKE 'o%'", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0], vec![json!("o'brien\\x@a.com")]);

    drop((src, dst));
    for db in ["ddl_src", "ddl_dst"] {
        admin.drop_database(db).await.unwrap();
    }
    assert!(!admin.list_databases().await.unwrap().contains(&"ddl_src".to_string()));
}

/// The emulator has no SPANNER_SYS nor Cloud Monitoring: the snapshot still
/// comes back, with the instance facts and notes saying what's missing.
#[tokio::test]
#[ignore]
async fn monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http
        .post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": "CREATE DATABASE `db1`" }))
        .send()
        .await;
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "db1".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    assert!(d.capabilities().monitor);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let snap = s.monitor().await.unwrap();
    eprintln!(
        "{:?}\ninfo {:?}\nnotes {:?}\ntables {:?}",
        snap.metrics.iter().map(|m| (m.key.as_str(), m.value)).collect::<Vec<_>>(),
        snap.info,
        snap.notes,
        snap.tables.iter().map(|t| (t.key.as_str(), t.rows.len())).collect::<Vec<_>>()
    );
    assert!(snap.info.iter().any(|(k, v)| k == "Unidades de procesamiento" && v == "1000"));
    assert!(snap.info.iter().any(|(k, v)| k == "Dialecto" && v == "GoogleSQL"));
    assert!(snap.notes.iter().any(|n| n.contains("SPANNER_SYS")));
    // The session still works after the failed system queries.
    let mut out = QueryOutcome::default();
    s.execute("SELECT 1", 10, &mut out).await.unwrap();
}

/// The emulator has no SPANNER_SYS: the profiler says so at start (its
/// sampling is covered by unit tests) and the session keeps working.
#[tokio::test]
#[ignore]
async fn profile() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "db1".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    assert!(d.supports_profiler());
    let mut s = d.connect(&cfg, None).await.unwrap();
    let opts = dbine_driver::ProfilerOptions { database: "db1".into(), change_server: false };
    match s.profiler_start(&opts).await {
        Err(e) => assert!(e.to_string().contains("SPANNER_SYS"), "{e}"),
        Ok(started) => {
            eprintln!("{started:?}");
            s.profiler_poll().await.unwrap();
        }
    }
    s.profiler_stop().await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT 1", 10, &mut out).await.unwrap();
}

/// Schema sync against the emulator: the script runs, and syncing again
/// from what the database now has gives nothing.
#[tokio::test]
#[ignore]
async fn schema_sync() {
    use dbine_driver::{ColumnDef, ForeignKeyDef, IndexDef, TableChange, TableSchema};
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http.post(format!("{url}/v1/projects/test/instances/i1/databases")).json(&json!({ "createStatement": "CREATE DATABASE `dbsync`" })).send().await;
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "dbsync".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    assert!(d.supports_schema_sync());
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    for stmt in ["ALTER TABLE sync_t DROP CONSTRAINT fk_nueva", "DROP INDEX ix_baja", "DROP INDEX ix_email", "DROP TABLE sync_t", "DROP TABLE sref", "DROP TABLE sync_new"] {
        let _ = s.execute(stmt, 10, &mut out).await;
    }
    s.execute(
        "CREATE TABLE sref (id INT64 NOT NULL) PRIMARY KEY (id);
         CREATE TABLE sync_t (id INT64 NOT NULL, nombre STRING(10), baja DATE, estado STRING(5) DEFAULT ('a'), ref INT64,
           CONSTRAINT fk_ref FOREIGN KEY (ref) REFERENCES sref (id)) PRIMARY KEY (id);
         CREATE INDEX ix_baja ON sync_t (baja)",
        10,
        &mut out,
    )
    .await
    .unwrap();
    s.execute("INSERT INTO sync_t (id, nombre, baja) VALUES (1, 'uno', DATE '2024-01-01')", 10, &mut out).await.unwrap();

    let find = |all: &[TableSchema], n: &str| all.iter().find(|t| t.name == n).cloned().unwrap();
    let all = s.database_schema().await.unwrap();
    let old = find(&all, "sync_t");
    let mut new = old.clone();
    let c = |n: &str, ty: &str, nullable: bool| ColumnDef { name: n.into(), data_type: ty.into(), nullable, ..Default::default() };
    new.columns.retain(|c| c.name != "baja");
    let nombre = new.columns.iter_mut().find(|c| c.name == "nombre").unwrap();
    nombre.data_type = "STRING(20)".into();
    nombre.nullable = false;
    nombre.default_value = Some("'s/n'".into());
    new.columns.iter_mut().find(|c| c.name == "estado").unwrap().default_value = None;
    new.columns.push(c("email", "STRING(MAX)", true));
    new.foreign_keys.clear();
    new.indexes = vec![IndexDef { name: "ix_email".into(), columns: vec!["email".into()], unique: true, kind: None, filter: None, ..Default::default() }];
    let created = TableSchema { kind: "table".into(), name: "sync_new".into(), columns: vec![c("id", "INT64", false), c("v", "STRING(10)", true)], primary_key: Some(dbine_driver::KeyDef { name: None, columns: vec!["id".into()] }), ..Default::default() };
    let changes = vec![
        TableChange::Alter { old, new: new.clone() },
        TableChange::Drop { table: find(&all, "sref") },
        TableChange::Create { table: created },
    ];
    let script = d.sync_script(&changes).unwrap();
    eprintln!("{script:#?}");
    for stmt in &script.statements {
        s.execute(stmt, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{stmt}"));
    }
    let all = s.database_schema().await.unwrap();
    let now = find(&all, "sync_t");
    assert!(all.iter().all(|t| t.name != "sref"));
    assert!(all.iter().any(|t| t.name == "sync_new"));
    let again = d.sync_script(&[TableChange::Alter { old: now.clone(), new: new.clone() }]).unwrap();
    assert!(again.statements.is_empty(), "{:?}\n{now:#?}", again.statements);

    // A foreign key back, and a NOT NULL dropped.
    let mut new2 = now.clone();
    new2.foreign_keys.push(ForeignKeyDef { name: Some("fk_nueva".into()), columns: vec!["ref".into()], ref_schema: None, ref_table: "sync_new".into(), ref_columns: vec!["id".into()], on_delete: None, on_update: None });
    new2.columns.iter_mut().find(|c| c.name == "nombre").unwrap().nullable = true;
    let script = d.sync_script(&[TableChange::Alter { old: now, new: new2.clone() }]).unwrap();
    for stmt in &script.statements {
        s.execute(stmt, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{stmt}"));
    }
    let now = find(&s.database_schema().await.unwrap(), "sync_t");
    let again = d.sync_script(&[TableChange::Alter { old: now.clone(), new: new2 }]).unwrap();
    assert!(again.statements.is_empty(), "{:?}\n{now:#?}", again.statements);
    s.execute("SELECT nombre, estado, email FROM sync_t WHERE id = 1", 10, &mut out).await.unwrap();
}
