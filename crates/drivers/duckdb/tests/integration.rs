//! End to end through the `Driver` API on a temporary database file (DuckDB
//! is embedded, so no server or env var is needed).

use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome};
use std::time::Duration;

fn cfg(path: &str, read_only: bool) -> ConnectionConfig {
    ConnectionConfig { driver: "duckdb".into(), host: path.into(), read_only, ..Default::default() }
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some("main".into()), name: name.into() }
}

#[tokio::test]
async fn full_session() {
    let path = std::env::temp_dir().join(format!("dbine-duck-{}.duckdb", std::process::id()));
    let path_s = path.to_string_lossy().to_string();
    let _ = std::fs::remove_file(&path);
    let driver = dbine_driver_duckdb::drivers().remove(0);
    {
        let mut s = driver.connect(&cfg(&path_s, false), None).await.unwrap();
        assert!(s.server_version().await.unwrap().starts_with("DuckDB v"));
        let mut out = QueryOutcome::default();
        s.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR NOT NULL DEFAULT 'x');
             INSERT INTO t SELECT i, 'n' || i FROM range(10) r(i);
             CREATE VIEW v AS SELECT id FROM t;
             CREATE MACRO twice(a) AS a * 2;
             CREATE SEQUENCE seq;",
            100,
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(out.results[1].rows_affected, Some(10));

        // A second session on the same file shares the instance.
        let mut s2 = driver.connect(&cfg(&path_s, false), None).await.unwrap();
        let dbs = s2.list_databases().await.unwrap();
        assert_eq!(dbs.len(), 1, "{dbs:?}");
        let objs = s2.list_objects().await.unwrap();
        let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n);
        assert!(has(kinds::TABLE, "t") && has(kinds::VIEW, "v") && has(kinds::FUNCTION, "twice") && has(kinds::SEQUENCE, "seq"), "{objs:?}");

        let cols = s2.columns(&obj(kinds::TABLE, "t")).await.unwrap();
        assert_eq!(cols.len(), 2);
        assert!(cols[0].primary_key && !cols[1].nullable);
        assert_eq!(cols[1].default_value.as_deref(), Some("'x'"));
        for (k, n) in [(kinds::TABLE, "t"), (kinds::VIEW, "v"), (kinds::FUNCTION, "twice"), (kinds::SEQUENCE, "seq")] {
            let def = s2.definition(&obj(k, n)).await.unwrap().unwrap();
            assert!(def.to_uppercase().starts_with("CREATE"), "{def}");
        }

        let q = s2.browse_query(&obj(kinds::TABLE, "t"), 5);
        let mut out = QueryOutcome::default();
        s2.execute(&q, 3, &mut out).await.unwrap();
        assert_eq!(out.results[0].rows.len(), 3);
        assert_eq!(out.results[0].total_rows, 5);
        assert!(out.results[0].truncated);

        let mut out = QueryOutcome::default();
        let err = s2.execute("SELECT twice(21); SELECT * FROM nope; SELECT 3", 10, &mut out).await.unwrap_err();
        assert!(err.is_query(), "{err:?}");
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].rows[0][0], serde_json::json!(42));

        // Cancel a long query from another task.
        let stop = s2.interrupter().unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop();
        });
        let mut out = QueryOutcome::default();
        let started = std::time::Instant::now();
        let r = s2.execute("SELECT count(*) FROM range(10000000000) a", 10, &mut out).await;
        assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(out.results.is_empty(), "{:?}", out.results);
    }

    // Read-only: the file is reopened in READ_ONLY access mode.
    let mut ro = driver.connect(&cfg(&path_s, true), None).await.unwrap();
    let mut out = QueryOutcome::default();
    assert!(ro.execute("INSERT INTO t VALUES (99, 'z')", 10, &mut out).await.is_err());
    ro.execute("SELECT count(*) FROM t", 10, &mut out).await.unwrap();
    // Nor attaches or detaches databases (detaching deletes the file).
    let sibling = path.with_file_name(format!("dbine-duck-ro-{}.duckdb", std::process::id()));
    std::fs::write(&sibling, b"").unwrap();
    let stem = sibling.file_stem().unwrap().to_string_lossy().to_string();
    assert!(matches!(ro.create_database("nueva").await, Err(Error::Query(m)) if m.contains("solo lectura")));
    assert!(matches!(ro.drop_database(&stem).await, Err(Error::Query(m)) if m.contains("solo lectura")));
    assert!(sibling.exists());
    let _ = std::fs::remove_file(&sibling);
    drop(ro);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn in_memory_and_attached_catalogs() {
    let driver = dbine_driver_duckdb::drivers().remove(0);
    let mut s = driver.connect(&cfg("", false), None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("ATTACH ':memory:' AS other; CREATE TABLE other.main.x (a INT);", 10, &mut out).await.unwrap();
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"other".to_string()) && dbs.contains(&"memory".to_string()), "{dbs:?}");
    let mut o = driver.connect(&cfg(":memory:", false), Some("other")).await.unwrap();
    let objs = o.list_objects().await.unwrap();
    assert!(objs.iter().any(|x| x.name == "x"), "{objs:?}");
}

#[tokio::test]
async fn plans() {
    let driver = dbine_driver_duckdb::drivers().remove(0);
    let mut s = driver.connect(&cfg(":memory:", false), None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE a AS SELECT range id, range % 10 g FROM range(20000);
         CREATE TABLE b AS SELECT range id, range % 1000 a_id FROM range(5000);",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let count = |out: &QueryOutcome, i: usize| out.results[i].rows[0][0].clone();

    // Estimated: nothing runs.
    let mut out = QueryOutcome::default();
    s.explain(
        "SELECT a.g, count(*) FROM a JOIN b ON b.a_id = a.id GROUP BY a.g; DELETE FROM b WHERE id < 100; CREATE TABLE c (x INT)",
        false,
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.plans.len(), 2);
    assert!(out.results.is_empty());
    assert_eq!(out.messages.len(), 1);
    assert!(out.plans.iter().all(|p| !p.actual && p.raw_format == "json"));
    assert_eq!(out.plans[1].root.op, "DELETE");
    let mut check = QueryOutcome::default();
    s.execute("SELECT count(*) FROM b", 10, &mut check).await.unwrap();
    assert_eq!(count(&check, 0), serde_json::json!(5000));

    // Actual: the script runs once, reads carry measured figures.
    let mut out = QueryOutcome::default();
    s.explain("SELECT count(*) FROM a WHERE g = 3; DELETE FROM b WHERE id < 100; SELECT count(*) FROM b", true, 10, &mut out)
        .await
        .unwrap();
    assert_eq!(out.plans.len(), 3);
    assert_eq!(out.results.len(), 3);
    assert_eq!(count(&out, 0), serde_json::json!(2000));
    assert_eq!(count(&out, 2), serde_json::json!(4900));
    assert!(out.plans[0].actual && !out.plans[1].actual && out.plans[2].actual);
    fn any_actual(n: &dbine_driver::PlanNode) -> bool {
        n.actual_rows.is_some() || n.children.iter().any(any_actual)
    }
    assert!(any_actual(&out.plans[0].root), "{:#?}", out.plans[0].root);
    assert_ne!(out.plans[0].root.op, "EXPLAIN_ANALYZE");

    let mut out = QueryOutcome::default();
    assert!(s.explain("SELECT 1; SELECT * FROM missing", true, 10, &mut out).await.is_err());
    assert_eq!(out.results.len(), 1);
}

/// Catalog read, DDL round trip into a new attached database, inserts and
/// create / drop database.
#[tokio::test]
async fn schema_ddl_round_trip() {
    use dbine_driver::{DdlParts, TableSchema};
    let dir = std::env::temp_dir().join(format!("dbine-duck-ddl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.duckdb").to_string_lossy().to_string();
    let driver = dbine_driver_duckdb::drivers().remove(0);
    let mut s = driver.connect(&cfg(&path, false), None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE SEQUENCE seq_c;
         CREATE TABLE clientes (id INTEGER DEFAULT nextval('seq_c') PRIMARY KEY, email VARCHAR NOT NULL UNIQUE, nombre VARCHAR);
         CREATE TABLE pedidos (id INTEGER PRIMARY KEY, cliente_id INTEGER REFERENCES clientes(id), total DECIMAL(10,2) DEFAULT 0, tags VARCHAR[]);
         CREATE TABLE lineas (pedido_id INTEGER, n INTEGER, producto VARCHAR, PRIMARY KEY (pedido_id, n), FOREIGN KEY (pedido_id) REFERENCES pedidos(id));
         CREATE INDEX ix_nombre ON clientes(nombre);
         COMMENT ON TABLE clientes IS 'Clientes';
         COMMENT ON COLUMN clientes.nombre IS 'Nombre y apellido';",
        10,
        &mut out,
    )
    .await
    .unwrap();

    let schema = s.database_schema().await.unwrap();
    let names: Vec<&str> = schema.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["clientes", "pedidos", "lineas"], "dependency order");
    let cli = &schema[0];
    assert_eq!(cli.comment.as_deref(), Some("Clientes"));
    assert_eq!(cli.columns[2].comment.as_deref(), Some("Nombre y apellido"));
    assert!(cli.columns[0].auto_increment, "{cli:?}");
    assert!(cli.indexes.iter().any(|i| i.unique && i.columns == vec!["email"]), "{:?}", cli.indexes);
    assert!(cli.indexes.iter().any(|i| i.name == "ix_nombre" && i.columns == vec!["nombre"]), "{:?}", cli.indexes);
    let lin = &schema[2];
    assert_eq!(lin.primary_key.as_ref().unwrap().columns, vec!["pedido_id", "n"]);
    assert_eq!(lin.foreign_keys[0].ref_table, "pedidos");
    assert_eq!(schema[1].columns[3].data_type, "VARCHAR[]");

    // Round trip into a new attached database.
    let caps = driver.capabilities();
    assert!(caps.create_database && caps.drop_database && caps.foreign_keys);
    s.create_database("copia").await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"copia".to_string()));
    let all = DdlParts { drop: false, if_exists: true, create: true, indexes: true, foreign_keys: true };
    let script: Vec<String> = schema.iter().map(|t| driver.table_ddl(t, all).unwrap()).collect();
    let mut s2 = driver.connect(&cfg(&path, false), Some("copia")).await.unwrap();
    let mut out = QueryOutcome::default();
    s2.execute(&script.join("\n"), 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{}", script.join("\n")));
    let copy = s2.database_schema().await.unwrap();
    assert_eq!(copy, schema);

    // Inserts from the generic script run.
    let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("main".into()), name: "clientes".into() };
    let ins = driver
        .insert_script(
            &target,
            &["email".into(), "nombre".into()],
            &[vec!["a@x".into(), "O'Hara".into()], vec!["b@x".into(), serde_json::Value::Null]],
        )
        .unwrap();
    let mut out = QueryOutcome::default();
    s2.execute(&format!("{ins}\nSELECT count(*) FROM clientes WHERE id IS NOT NULL;"), 10, &mut out).await.unwrap();
    assert_eq!(out.results.last().unwrap().rows[0][0], serde_json::json!(2));

    // A designer-built table with auto-increment, comments and drop.
    let designed: TableSchema = serde_json::from_value(serde_json::json!({
        "schema": "main", "name": "nueva", "comment": "hecha en el diseñador",
        "columns": [
            {"name": "id", "data_type": "BIGINT", "nullable": false, "auto_increment": true},
            {"name": "cliente_id", "data_type": "INTEGER", "comment": "ref"}
        ],
        "primary_key": {"columns": ["id"]},
        "foreign_keys": [{"columns": ["cliente_id"], "ref_table": "clientes", "ref_columns": ["id"]}],
        "indexes": [{"name": "ix_nueva_c", "columns": ["cliente_id"], "unique": false}]
    }))
    .unwrap();
    let ddl = driver.table_ddl(&designed, DdlParts { drop: true, if_exists: true, ..all }).unwrap();
    let mut out = QueryOutcome::default();
    s2.execute(&format!("{ddl}\nINSERT INTO nueva (cliente_id) VALUES (1), (2);"), 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{ddl}"));
    drop(s2);

    assert!(s.drop_database("copia").await.is_ok());
    assert!(!dir.join("copia.duckdb").exists());
    assert!(s.drop_database("main").await.is_err());
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn monitor() {
    let driver = dbine_driver_duckdb::drivers().remove(0);
    assert!(driver.capabilities().monitor);
    let path = std::env::temp_dir().join(format!("dbine-duck-mon-{}.duckdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut s = driver.connect(&cfg(&path.to_string_lossy(), false), None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("CREATE TABLE big AS SELECT range AS i, md5(range::VARCHAR) AS h FROM range(200000); CHECKPOINT;", 10, &mut out).await.unwrap();
    let snap = s.monitor().await.unwrap();
    for m in &snap.metrics {
        eprintln!("{:<14} {:<40} {:?} max={:?}", m.key, m.label, m.value, m.max);
    }
    eprintln!("{:?}\n{:?}", snap.info, snap.notes);
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    assert!(v("mem_used").unwrap() > 0.0);
    assert!(v("storage_used").unwrap() > 1_000_000.0);
    assert_eq!(v("connections"), Some(1.0));
    let top = snap.tables.iter().find(|t| t.key == "top_objects").unwrap();
    assert!(top.rows[0][0].as_str().unwrap().ends_with(".main.big"));
    drop(s);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.wal", path.display()));
}

/// The "Archivos CSV / Parquet / JSON" preset on a folder.
#[tokio::test]
async fn files_preset() {
    let dir = std::env::temp_dir().join(format!("dbine-duck-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("2024")).unwrap();
    std::fs::write(dir.join("clientes.csv"), "id,nombre\n1,Ana\n2,Beto\n").unwrap();
    std::fs::write(dir.join("eventos.jsonl"), "{\"id\": 1, \"tipo\": \"alta\"}\n{\"id\": 2, \"tipo\": \"baja\"}\n").unwrap();
    std::fs::write(dir.join("2024/tabulado.tsv"), "a\tb\n1\t2\n").unwrap();
    std::fs::write(dir.join("roto.parquet"), "no es parquet").unwrap();
    std::fs::write(dir.join("leeme.txt"), "x").unwrap();
    // A parquet file written by DuckDB itself.
    {
        let driver = dbine_driver_duckdb::drivers().remove(0);
        let mut s = driver.connect(&cfg(":memory:", false), None).await.unwrap();
        let target = dir.join("ventas.parquet");
        let sql = format!("COPY (SELECT range AS id, range * 1.5 AS total FROM range(5)) TO '{}' (FORMAT PARQUET)", target.display());
        let mut out = QueryOutcome::default();
        s.execute(&sql, 10, &mut out).await.unwrap();
    }

    let files = dbine_driver_duckdb::drivers().into_iter().find(|d| d.info().id == "duckdb_files").unwrap();
    let mut c = ConnectionConfig { driver: "duckdb_files".into(), host: dir.join("clientes.csv").to_string_lossy().into(), ..Default::default() };
    c.options.insert("recursive".into(), "true".into());
    let mut s = files.connect(&c, None).await.unwrap();
    let names: Vec<String> = s.list_objects().await.unwrap().into_iter().map(|o| o.name).collect();
    assert_eq!(names, ["2024/tabulado", "clientes", "eventos", "ventas"], "the broken parquet is skipped");
    let v = ObjectRef { kind: kinds::VIEW.into(), schema: Some("main".into()), name: "clientes".into() };
    let cols = s.columns(&v).await.unwrap();
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "nombre"]);
    let mut out = QueryOutcome::default();
    s.execute(
        "SELECT c.nombre, e.tipo FROM clientes c JOIN eventos e USING (id) ORDER BY 1;
         SELECT SUM(total) FROM ventas;
         SELECT * FROM \"2024/tabulado\";
         SELECT COUNT(*) FROM 'clientes.csv';",
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[0].rows, vec![vec![serde_json::json!("Ana"), serde_json::json!("alta")], vec![serde_json::json!("Beto"), serde_json::json!("baja")]]);
    assert_eq!(out.results[1].rows[0][0], serde_json::json!("15.0"), "decimals come as exact text");
    assert_eq!(out.results[2].rows[0].len(), 2);
    assert_eq!(out.results[3].rows[0][0], serde_json::json!(2), "file_search_path is the folder");

    // New files show up on the next connection; the data is read live.
    std::fs::write(dir.join("clientes.csv"), "id,nombre\n1,Ana\n2,Beto\n3,Caro\n").unwrap();
    std::fs::write(dir.join("nuevo.csv"), "x\n1\n").unwrap();
    let mut s2 = files.connect(&c, None).await.unwrap();
    assert!(s2.list_objects().await.unwrap().iter().any(|o| o.name == "nuevo"));
    let mut out = QueryOutcome::default();
    s2.execute("SELECT COUNT(*) FROM clientes", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(3));
    let snap = s2.monitor().await.unwrap();
    assert!(snap.metrics.iter().any(|m| m.key == "mem_used" && m.value.is_some()));
    assert!(files.connect(&ConnectionConfig { host: dir.join("nope").to_string_lossy().into(), ..c.clone() }, None).await.is_err());
    drop((s, s2));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Schema sync on a real database: each generated script turns the table
/// into its new version (with and without a rebuild), and a second
/// comparison finds nothing left to change.
#[tokio::test]
async fn schema_sync_applies() {
    use dbine_driver::{ColumnDef, ForeignKeyDef, IndexDef, KeyDef, TableChange, TableSchema};
    let driver = dbine_driver_duckdb::drivers().remove(0);
    assert!(driver.supports_schema_sync());
    let mut s = driver.connect(&cfg(":memory:", false), None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE sync_padre (id INTEGER PRIMARY KEY, codigo INTEGER);
         CREATE TABLE sync_hijo (id INTEGER PRIMARY KEY, padre_id INTEGER REFERENCES sync_padre (id), nombre VARCHAR NOT NULL, codigo INTEGER, baja DATE);
         CREATE INDEX ix_sync_hijo_nombre ON sync_hijo (nombre);
         INSERT INTO sync_padre VALUES (1, 10), (5, 50);
         INSERT INTO sync_hijo VALUES (1, 1, 'uno', 5, NULL);",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let find = |schema: &[TableSchema], n: &str| schema.iter().find(|t| t.name == n).unwrap().clone();
    async fn apply(s: &mut Box<dyn dbine_driver::Session>, script: &dbine_driver::SyncScript) {
        eprintln!("{}\n{:?}", script.statements.join("\n"), script.warnings);
        for st in &script.statements {
            let mut out = QueryOutcome::default();
            s.execute(st, 10, &mut out).await.unwrap_or_else(|e| panic!("{st}: {e}"));
        }
    }
    let settled = |driver: &std::sync::Arc<dyn dbine_driver::Driver>, now: TableSchema, want: &TableSchema| {
        let left = driver.sync_script(&[TableChange::Alter { old: now, new: want.clone() }]).unwrap();
        assert!(left.statements.is_empty(), "{:?}", left.statements);
    };

    // In place: a wider type, nullability, a default, a column in and one
    // out (the indexes move out of the way and come back).
    let old = find(&s.database_schema().await.unwrap(), "sync_hijo");
    let mut new = old.clone();
    let nombre = new.columns.iter_mut().find(|c| c.name == "nombre").unwrap();
    nombre.data_type = "VARCHAR(40)".into();
    nombre.nullable = true;
    nombre.default_value = Some("'x'".into());
    new.columns.retain(|c| c.name != "baja");
    new.columns.push(ColumnDef { name: "email".into(), data_type: "VARCHAR".into(), nullable: false, default_value: Some("'-'".into()), ..Default::default() });
    new.indexes.retain(|i| i.name != "ix_sync_hijo_nombre");
    new.indexes.push(IndexDef { name: "ix_hijo_email".into(), columns: vec!["email".into()], unique: false, kind: Some("ART".into()), filter: None, ..Default::default() });
    let script = driver.sync_script(&[TableChange::Alter { old, new: new.clone() }]).unwrap();
    assert!(!script.statements.iter().any(|x| x.contains("__dbine_old")), "{:?}", script.statements);
    apply(&mut s, &script).await;
    let now = find(&s.database_schema().await.unwrap(), "sync_hijo");
    // VARCHAR(40) comes back as VARCHAR.
    new.columns.iter_mut().find(|c| c.name == "nombre").unwrap().data_type = "VARCHAR".into();
    settled(&driver, now, &new);

    // Keys: a new foreign key and a different primary key: rebuilt.
    let mut last = new.clone();
    last.foreign_keys[0].on_delete = None;
    last.foreign_keys.push(ForeignKeyDef { columns: vec!["codigo".into()], ref_schema: Some("main".into()), ref_table: "sync_padre".into(), ref_columns: vec!["id".into()], ..Default::default() });
    last.primary_key = Some(KeyDef { name: None, columns: vec!["id".into(), "email".into()] });
    last.indexes.push(IndexDef { name: "ux_hijo_codigo".into(), columns: vec!["codigo".into()], unique: true, kind: Some("ART".into()), filter: None, ..Default::default() });
    let script = driver.sync_script(&[TableChange::Alter { old: new, new: last.clone() }]).unwrap();
    assert!(script.statements[0].contains("__dbine_old"));
    apply(&mut s, &script).await;
    let now = find(&s.database_schema().await.unwrap(), "sync_hijo");
    settled(&driver, now, &last);
    let rows = s.execute("SELECT count(*) FROM sync_hijo WHERE codigo = 5", 10, &mut QueryOutcome::default()).await;
    assert!(rows.is_ok());

    let sync_padre = find(&s.database_schema().await.unwrap(), "sync_padre");
    let script = driver.sync_script(&[TableChange::Drop { table: last }, TableChange::Drop { table: sync_padre }]).unwrap();
    apply(&mut s, &script).await;
    assert!(!s.database_schema().await.unwrap().iter().any(|t| t.name.starts_with("sync_")));
}

/// "Nuevo esquema…" / "Borrar esquema…": the scripts run, the catalog shows
/// the schema, and dropping a schema with objects needs CASCADE.
#[tokio::test]
async fn create_and_drop_schema() {
    let path = std::env::temp_dir().join(format!("dbine-duck-schema-{}.duckdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let driver = dbine_driver_duckdb::drivers().remove(0);
    let mut s = driver.connect(&cfg(&path.to_string_lossy(), false), None).await.unwrap();
    let exists = |out: &QueryOutcome| out.results[0].rows.len();
    for cascade in [false, true] {
        let mut out = QueryOutcome::default();
        s.execute(&driver.create_schema_script(None, "Ventas \"2\"", None).unwrap(), 10, &mut out).await.unwrap();
        let mut out = QueryOutcome::default();
        s.execute("SELECT schema_name FROM duckdb_schemas() WHERE schema_name = 'Ventas \"2\"'", 10, &mut out).await.unwrap();
        assert_eq!(exists(&out), 1);
        // Empty, it still shows in the explorer.
        let schemas = s.list_schemas().await.unwrap().expect("DuckDB lists schemas");
        let find = |n: &str| schemas.iter().find(|x| x.name == n).map(|x| x.system);
        assert_eq!((find("Ventas \"2\""), find("main"), find("information_schema")), (Some(false), Some(false), None), "{schemas:?}");
        if cascade {
            let mut out = QueryOutcome::default();
            s.execute("CREATE TABLE \"Ventas \"\"2\"\"\".t (x INT)", 10, &mut out).await.unwrap();
            let mut out = QueryOutcome::default();
            assert!(s.execute(&driver.drop_schema_script(None, "Ventas \"2\"", false).unwrap(), 10, &mut out).await.is_err());
        }
        let mut out = QueryOutcome::default();
        s.execute(&driver.drop_schema_script(None, "Ventas \"2\"", cascade).unwrap(), 10, &mut out).await.unwrap();
        let mut out = QueryOutcome::default();
        s.execute("SELECT schema_name FROM duckdb_schemas() WHERE schema_name = 'Ventas \"2\"'", 10, &mut out).await.unwrap();
        assert_eq!(exists(&out), 0);
    }
    drop(s);
    let _ = std::fs::remove_file(&path);
}

/// The scripts take the catalog the menu was opened on: run from a session
/// that `USE`s another one, the schema still lands in (and leaves) that
/// catalog, and a session opened there lists it while it's empty.
#[tokio::test]
async fn schema_in_the_menus_catalog() {
    let dir = std::env::temp_dir();
    let main = dir.join(format!("dbine-duck-cat-{}.duckdb", std::process::id()));
    let other = dir.join(format!("dbine-duck-cat-{}-b.duckdb", std::process::id()));
    for p in [&main, &other] {
        let _ = std::fs::remove_file(p);
    }
    let driver = dbine_driver_duckdb::drivers().remove(0);
    let mut s = driver.connect(&cfg(&main.to_string_lossy(), false), None).await.unwrap();
    async fn run(s: &mut Box<dyn dbine_driver::Session>, sql: &str) -> QueryOutcome {
        let mut out = QueryOutcome::default();
        s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        out
    }
    run(&mut s, &format!("ATTACH '{}' AS dbine_other", other.to_string_lossy())).await;
    let where_ = "SELECT database_name FROM duckdb_schemas() WHERE schema_name = 'Esq 2' ORDER BY 1";
    run(&mut s, &driver.create_schema_script(Some("dbine_other"), "Esq 2", None).unwrap()).await;
    assert_eq!(run(&mut s, where_).await.results[0].rows, vec![vec![serde_json::json!("dbine_other")]]);

    let mut there = driver.connect(&cfg(&main.to_string_lossy(), false), Some("dbine_other")).await.unwrap();
    let listed = there.list_schemas().await.unwrap().expect("DuckDB lists schemas");
    let find = |n: &str| listed.iter().find(|x| x.name == n).map(|x| x.system);
    assert_eq!((find("Esq 2"), find("main"), find("information_schema"), find("pg_catalog")), (Some(false), Some(false), None, None), "{listed:?}");
    drop(there);

    run(&mut s, &driver.drop_schema_script(Some("dbine_other"), "Esq 2", false).unwrap()).await;
    assert!(run(&mut s, where_).await.results[0].rows.is_empty());
    run(&mut s, "DETACH dbine_other").await;
    drop(s);
    for p in [&main, &other] {
        let _ = std::fs::remove_file(p);
    }
}
