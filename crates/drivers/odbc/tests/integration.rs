//! Against a real server through a real ODBC driver, with the generic
//! "odbc" preset. Run with, for example:
//!
//! ```sh
//! docker run -d --name dbine-test-odbc-mssql -e ACCEPT_EULA=Y -e 'MSSQL_SA_PASSWORD=Dbine_Odbc#2026' \
//!   -p 25741:1433 mcr.microsoft.com/mssql/server:2022-latest
//! DBINE_TEST_ODBC_CONN='DRIVER={ODBC Driver 18 for SQL Server};SERVER=127.0.0.1,25741;UID=sa;PWD={Dbine_Odbc#2026};TrustServerCertificate=yes' \
//!   cargo test -p dbine-driver-odbc --test integration -- --ignored --nocapture
//! docker rm -f dbine-test-odbc-mssql
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use serde_json::json;
use std::time::{Duration, Instant};

fn cfg(batch: &str) -> ConnectionConfig {
    let conn = std::env::var("DBINE_TEST_ODBC_CONN").expect("DBINE_TEST_ODBC_CONN");
    ConnectionConfig {
        driver: "odbc".into(),
        options: [("connection_string".to_string(), conn), ("batch_mode".to_string(), batch.to_string())].into(),
        ..Default::default()
    }
}

async fn open(cfg: &ConnectionConfig, db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "odbc").unwrap();
    d.connect(cfg, db).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str, max_rows: usize) -> (QueryOutcome, Result<(), Error>) {
    let mut out = QueryOutcome::default();
    let r = s.execute(sql, max_rows, &mut out).await;
    (out, r)
}

fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some(schema.into()), name: name.into() }
}

#[tokio::test]
#[ignore]
async fn sql_server_through_odbc() {
    let drivers = dbine_driver_odbc::installed_odbc_drivers().unwrap();
    println!("installed ODBC drivers: {drivers:?}");
    assert!(drivers.iter().any(|d| d.contains("SQL Server")));

    // Setup in master, GO batches.
    let mut s = open(&cfg("go"), None).await;
    let v = s.server_version().await.unwrap();
    println!("version: {v}");
    assert!(v.contains("SQL Server"));
    let (_, r) = run(&mut s, "IF DB_ID('dbine_odbc') IS NULL CREATE DATABASE dbine_odbc", 10).await;
    r.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"dbine_odbc".to_string()));
    drop(s);

    // The catalog the explorer picks becomes the current one.
    let mut s = open(&cfg("go"), Some("dbine_odbc")).await;
    let setup = "
DROP VIEW IF EXISTS dbo.v_items
DROP PROCEDURE IF EXISTS dbo.p_items
DROP TABLE IF EXISTS dbo.items
GO
CREATE TABLE dbo.items (
    id int IDENTITY PRIMARY KEY,
    name nvarchar(50) NOT NULL,
    price decimal(10,2),
    created datetime2,
    day date,
    data varbinary(10),
    flag bit,
    big bigint,
    ratio float
)
GO
CREATE VIEW dbo.v_items AS SELECT id, name FROM dbo.items
GO
CREATE PROCEDURE dbo.p_items AS BEGIN SELECT 1 AS one; SELECT 2 AS two; END
GO
INSERT INTO dbo.items (name, price, created, day, data, flag, big, ratio) VALUES
  (N'ñandú', 0.5, '2024-01-31 13:45:00.12', '2024-01-31', 0xDEAD, 1, 9007199254740993, 1.5),
  (N'b', NULL, NULL, NULL, NULL, 0, 2, NULL)
";
    let (out, r) = run(&mut s, setup, 10).await;
    r.unwrap();
    assert_eq!(out.results.last().unwrap().rows_affected, Some(2));

    let objs = s.list_objects().await.unwrap();
    let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n && o.schema.as_deref() == Some("dbo"));
    assert!(has("table", "items"), "{objs:?}");
    assert!(has("view", "v_items"), "{objs:?}");
    assert!(has("procedure", "p_items"), "{objs:?}");
    assert!(!objs.iter().any(|o| o.schema.as_deref() == Some("sys")));

    let cols = s.columns(&obj("table", "dbo", "items")).await.unwrap();
    println!("{cols:?}");
    assert_eq!(cols.len(), 9);
    assert_eq!(cols[0].name, "id");
    assert!(cols[0].primary_key && cols[0].auto_increment && !cols[0].nullable);
    assert_eq!(cols[1].data_type, "nvarchar(50)");
    assert!(!cols[1].nullable);
    assert_eq!(cols[2].data_type, "decimal(10,2)");

    let def = s.definition(&obj("view", "dbo", "v_items")).await.unwrap().unwrap();
    assert!(def.contains("CREATE VIEW"), "{def}");
    let def = s.definition(&obj("procedure", "dbo", "p_items")).await.unwrap().unwrap();
    assert!(def.contains("CREATE PROCEDURE"), "{def}");

    // Browse + value conversion.
    let q = s.browse_query(&obj("table", "dbo", "items"), 10);
    let (out, r) = run(&mut s, &(q + " ORDER BY id"), 100).await;
    r.unwrap();
    let row = &out.results[0].rows[0];
    println!("{row:?}");
    assert_eq!(row[0], json!(1));
    assert_eq!(row[1], json!("ñandú"));
    assert_eq!(row[2], json!("0.50"));
    assert_eq!(row[3], json!("2024-01-31 13:45:00.12"));
    assert_eq!(row[4], json!("2024-01-31"));
    assert_eq!(row[5], json!("0xDEAD"));
    assert_eq!(row[6], json!(true));
    assert_eq!(row[7], json!("9007199254740993"));
    assert_eq!(row[8], json!(1.5));
    assert_eq!(out.results[0].rows[1][2], json!(null));

    // Several result sets, a count and a PRINT from one batch; a procedure
    // with two result sets.
    let (out, r) = run(
        &mut s,
        "SELECT 1 AS a; SELECT 'x' AS b; UPDATE dbo.items SET name = name WHERE id = 1; PRINT 'hola'\nGO\nEXEC dbo.p_items",
        10,
    )
    .await;
    r.unwrap();
    println!("{:?} / {:?}", out.results, out.messages);
    assert_eq!(out.results[0].rows, vec![vec![json!(1)]]);
    assert_eq!(out.results[1].rows, vec![vec![json!("x")]]);
    assert_eq!(out.results[2].rows_affected, Some(1));
    assert!(out.messages.iter().any(|m| m == "hola"), "{:?}", out.messages);
    let n = out.results.len();
    assert_eq!(out.results[n - 2].columns[0].name, "one");
    assert_eq!(out.results[n - 1].columns[0].name, "two");

    // Error mid-script: what ran before stays.
    let (out, r) = run(&mut s, "SELECT 1\nGO\nSELECT * FROM dbo.nope\nGO\nSELECT 2", 10).await;
    let e = r.unwrap_err();
    println!("error: {e}");
    assert!(e.is_query() && e.to_string().contains("nope"), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // max_rows keeps the first rows and counts the rest.
    let (out, r) = run(&mut s, "SELECT name FROM sys.all_objects", 3).await;
    r.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    assert!(out.results[0].truncated && out.results[0].total_rows > 3);

    // Cancel from another thread.
    let stop = s.interrupter().unwrap();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        stop();
    });
    let start = Instant::now();
    let (_, r) = run(&mut s, "WAITFOR DELAY '00:00:30'; SELECT 1", 10).await;
    t.join().unwrap();
    println!("cancel after {:?}: {r:?}", start.elapsed());
    assert!(matches!(r, Err(Error::Cancelled)));
    assert!(start.elapsed() < Duration::from_secs(10));
    // The session is still usable.
    let (out, r) = run(&mut s, "SELECT 3", 10).await;
    r.unwrap();
    assert_eq!(out.results[0].rows, vec![vec![json!(3)]]);
    drop(s);

    // Statement mode (split on ;): error mid-script.
    let mut s = open(&cfg("statements"), Some("dbine_odbc")).await;
    let (out, r) = run(&mut s, "SELECT 1; SELECT * FROM dbo.nope; SELECT 2", 10).await;
    assert!(r.is_err());
    assert_eq!(out.results.len(), 1);
    drop(s);

    // Read-only: the registry wraps SQL sessions like this.
    let mut ro = cfg("statements");
    ro.read_only = true;
    let mut s: Box<dyn Session> = Box::new(ReadOnlySession::new(open(&ro, Some("dbine_odbc")).await));
    let (_, r) = run(&mut s, "DELETE FROM dbo.items", 10).await;
    assert!(r.is_err());
    let (out, r) = run(&mut s, "SELECT COUNT(*) FROM dbo.items", 10).await;
    r.unwrap();
    assert_eq!(out.results[0].rows, vec![vec![json!(2)]]);
}

#[tokio::test]
#[ignore]
async fn bad_driver_name_lists_installed_ones() {
    let d = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "db2").unwrap();
    let cfg = ConnectionConfig {
        driver: "db2".into(),
        host: "127.0.0.1".into(),
        database: "SAMPLE".into(),
        options: [("odbc_driver".to_string(), "No Such Driver".to_string())].into(),
        ..Default::default()
    };
    let e = d.connect(&cfg, None).await.err().expect("must fail");
    println!("{e}");
    assert!(matches!(e, Error::Connect(ref m) if m.contains("ODBC Driver 18 for SQL Server")));
}

/// Plans through the generic preset on SQL Server: SHOWPLAN_ALL (estimated)
/// and STATISTICS PROFILE (actual).
#[tokio::test]
#[ignore]
async fn sql_server_plans_through_odbc() {
    let d = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "odbc").unwrap();
    assert!(d.supports_explain());
    let mut s = open(&cfg("statements"), None).await;
    let (_, r) = run(
        &mut s,
        "DROP TABLE IF EXISTS dbo.plan_items; \
         CREATE TABLE dbo.plan_items (id int PRIMARY KEY, grp int, name nvarchar(20)); \
         INSERT INTO dbo.plan_items VALUES (1, 1, N'a'), (2, 1, N'b'), (3, 2, N'c')",
        10,
    )
    .await;
    r.unwrap();
    let q = "SELECT grp, COUNT(*) AS n FROM dbo.plan_items WHERE id > 1 GROUP BY grp";

    let mut out = QueryOutcome::default();
    s.explain(&format!("{q}; DELETE FROM dbo.plan_items"), false, 10, &mut out).await.unwrap();
    assert!(out.results.is_empty(), "nothing ran: {:?}", out.results);
    assert_eq!(out.plans.len(), 2);
    let p = &out.plans[0];
    println!("{}\n{:#?}", p.raw, p.root);
    assert!(!p.actual);
    assert_eq!(p.root.op, "SELECT");
    assert!(p.root.total_cost.is_some());
    assert!(!p.root.children.is_empty());
    assert_eq!(out.plans[1].root.op, "DELETE");
    let (o, _) = run(&mut s, "SELECT COUNT(*) FROM dbo.plan_items", 10).await;
    assert_eq!(o.results[0].rows[0][0], json!(3), "the DELETE didn't run");

    let mut out = QueryOutcome::default();
    s.explain(q, true, 10, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 1, "only the query's rows: {:?}", out.results);
    assert_eq!(out.results[0].rows.len(), 2);
    let p = &out.plans[0];
    assert!(p.actual);
    let first = &p.root.children[0];
    assert!(first.actual_rows.is_some() && first.executions.is_some(), "{:#?}", p.root);

    // The session is back to normal.
    let (o, r) = run(&mut s, "SELECT 1 AS one", 10).await;
    r.unwrap();
    assert_eq!(o.results[0].rows[0][0], json!(1));
    run(&mut s, "DROP TABLE dbo.plan_items", 10).await.1.unwrap();
}

#[tokio::test]
#[ignore]
async fn sql_server_schema_ddl_and_inserts_through_odbc() {
    use dbine_driver::DdlParts;
    let driver = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "odbc").unwrap();
    assert!(driver.capabilities().foreign_keys && driver.capabilities().create_database);
    const DB: &str = "dbine_odbc_ddl";

    let mut master = open(&cfg("go"), None).await;
    let (_, r) = run(&mut master, &format!("IF DB_ID('{DB}') IS NOT NULL DROP DATABASE {DB}"), 10).await;
    r.unwrap();
    master.create_database(DB).await.unwrap();
    assert!(master.list_databases().await.unwrap().contains(&DB.to_string()));

    let mut s = open(&cfg("go"), Some(DB)).await;
    let setup = "
        CREATE TABLE dbo.clientes (id int IDENTITY(1,1) CONSTRAINT pk_clientes PRIMARY KEY, nombre nvarchar(50) NOT NULL, activo bit NULL);
        CREATE TABLE dbo.pedidos (
            id int IDENTITY(1,1) NOT NULL,
            linea int NOT NULL,
            cliente_id int NULL,
            estado varchar(20) NOT NULL CONSTRAINT df_estado DEFAULT 'nuevo',
            total decimal(18,2) NULL,
            CONSTRAINT pk_pedidos PRIMARY KEY (id, linea),
            CONSTRAINT fk_pedidos_cliente FOREIGN KEY (cliente_id) REFERENCES dbo.clientes (id) ON DELETE CASCADE
        );
        CREATE INDEX ix_estado ON dbo.pedidos (estado, total);
        CREATE UNIQUE INDEX ux_cliente_linea ON dbo.pedidos (cliente_id, linea) WHERE cliente_id IS NOT NULL;
    ";
    let (_, r) = run(&mut s, setup, 10).await;
    r.unwrap();

    let schema = s.database_schema().await.unwrap();
    let pedidos = schema.iter().find(|t| t.name == "pedidos").expect("pedidos");
    println!("{pedidos:#?}");
    assert_eq!(pedidos.schema.as_deref(), Some("dbo"));
    let pk = pedidos.primary_key.as_ref().unwrap();
    assert_eq!(pk.columns, vec!["id", "linea"]);
    assert_eq!(pk.name.as_deref(), Some("pk_pedidos"));
    let names: Vec<&str> = pedidos.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "linea", "cliente_id", "estado", "total"]);
    assert!(pedidos.columns[0].auto_increment);
    assert_eq!(pedidos.columns[4].data_type, "decimal(18,2)");
    assert!(pedidos.columns[3].default_value.as_deref().unwrap().contains("nuevo"));
    let fk = &pedidos.foreign_keys[0];
    assert_eq!(pedidos.foreign_keys.len(), 1);
    assert_eq!((fk.name.as_deref(), fk.ref_table.as_str()), (Some("fk_pedidos_cliente"), "clientes"));
    assert_eq!((fk.columns.clone(), fk.ref_columns.clone()), (vec!["cliente_id".to_string()], vec!["id".to_string()]));
    assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));
    let ix: Vec<(&str, bool)> = pedidos.indexes.iter().map(|i| (i.name.as_str(), i.unique)).collect();
    assert_eq!(ix, vec![("ix_estado", false), ("ux_cliente_linea", true)]);
    assert_eq!(pedidos.indexes[0].columns, vec!["estado", "total"]);
    let clientes = schema.iter().find(|t| t.name == "clientes").unwrap();
    assert!(clientes.foreign_keys.is_empty() && clientes.indexes.is_empty());

    // DDL round trip: the same table under another name.
    let mut copy = pedidos.clone();
    copy.name = "pedidos2".into();
    copy.primary_key.as_mut().unwrap().name = None;
    copy.foreign_keys[0].name = None;
    for i in &mut copy.indexes {
        i.name = format!("{}_2", i.name);
    }
    let ddl = driver
        .table_ddl(&copy, DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: true })
        .unwrap();
    println!("{ddl}");
    let (_, r) = run(&mut s, &ddl, 10).await;
    r.unwrap();
    let schema = s.database_schema().await.unwrap();
    let p2 = schema.iter().find(|t| t.name == "pedidos2").unwrap();
    assert_eq!(p2.columns, copy.columns);
    assert_eq!(p2.primary_key.as_ref().unwrap().columns, vec!["id", "linea"]);
    assert_eq!(p2.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    assert_eq!(p2.indexes.len(), 2);
    assert_eq!(p2.indexes[1].columns, vec!["cliente_id", "linea"]);

    // Inserts from the driver's script.
    let target = obj("table", "dbo", "clientes");
    let rows = vec![vec![json!("O'Brien"), json!(true)], vec![json!("Ana"), json!(false)], vec![json!("Luis"), serde_json::Value::Null]];
    let script = driver.insert_script(&target, &["nombre".into(), "activo".into()], &rows).unwrap();
    println!("{script}");
    let (_, r) = run(&mut s, &script, 10).await;
    r.unwrap();
    let (out, r) = run(&mut s, "SELECT COUNT(*) FROM dbo.clientes WHERE activo = 1 AND nombre = 'O''Brien'", 10).await;
    r.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!(1));
    drop(s);

    master.drop_database(DB).await.unwrap();
    assert!(!master.list_databases().await.unwrap().contains(&DB.to_string()));
}

/// The monitor of the generic preset against SQL Server: it goes by the
/// DBMS name to the SQL Server views.
#[tokio::test]
#[ignore]
async fn monitor() {
    let driver = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "odbc").unwrap();
    assert!(driver.capabilities().monitor);
    let mut s = open(&cfg("go"), None).await;
    let snap = s.monitor().await.expect("monitor");
    for m in &snap.metrics {
        println!("{:<16} {:<28} {:?} max {:?}{}", m.key, m.label, m.value, m.max, if m.counter { " (contador)" } else { "" });
    }
    for t in &snap.tables {
        println!("tabla {} ({} filas): {:?}", t.key, t.rows.len(), t.columns);
    }
    println!("info: {:?}\nnotas: {:?}", snap.info, snap.notes);
    let value = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    assert!(value("uptime").is_some_and(|v| v > 0.0));
    assert!(value("connections").is_some_and(|v| v >= 1.0));
    assert!(value("mem_used").is_some_and(|v| v > 0.0));
    assert!(value("queries").is_some());
    assert!(value("storage_used").is_some_and(|v| v > 0.0));
    let sessions = snap.tables.iter().find(|t| t.key == "sessions").expect("sessions");
    assert!(!sessions.rows.is_empty());
    assert!(snap.tables.iter().any(|t| t.key == "databases" && t.rows.iter().any(|r| r[0] == json!("master"))));
    // A second snapshot moves the counters.
    let again = s.monitor().await.unwrap();
    let q = |s: &dbine_driver::MonitorSnapshot| s.metrics.iter().find(|m| m.key == "queries").and_then(|m| m.value).unwrap();
    assert!(q(&again) >= q(&snap));

    // Presets without a monitor say why.
    let spark = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "spark").unwrap();
    assert!(!spark.capabilities().monitor);
}

/// The editor's script contract over ODBC: errors with SQLSTATE and native
/// code, server messages with theirs, manual transactions.
#[tokio::test]
#[ignore]
async fn script_errors_messages_and_transactions() {
    let c = cfg("statements");
    let mut s = open(&c, None).await;
    let mut other = open(&c, None).await;
    let _ = run(&mut s, "DROP TABLE dbine_tx", 10).await;
    run(&mut s, "CREATE TABLE dbine_tx (id INT PRIMARY KEY)", 10).await.1.unwrap();
    let (_, r) = run(&mut s, "SELECT * FROM nope_nope", 10).await;
    let e = r.unwrap_err().to_script_error();
    eprintln!("{e:?}");
    assert!(e.sqlstate.is_some() && e.code.is_some(), "{e:?}");

    s.set_autocommit(false).await.unwrap();
    run(&mut s, "INSERT INTO dbine_tx VALUES (1)", 10).await.1.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Open));
    s.rollback().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Idle));
    run(&mut s, "INSERT INTO dbine_tx VALUES (2)", 10).await.1.unwrap();
    s.commit().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    let (out, r) = run(&mut other, "SELECT COUNT(*) FROM dbine_tx", 10).await;
    r.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!(1));
    run(&mut s, "DROP TABLE dbine_tx", 10).await.1.unwrap();
}
