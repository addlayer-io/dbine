//! A driver through its host behaves like the same driver in the app: the
//! same answers, the same errors, rows streamed to a sink, cancel, and a
//! host that dies.
//!
//! SQLite runs everywhere; PostgreSQL needs its test container:
//! `DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres cargo test -p dbine-plugin-host -- --ignored`

use dbine_driver::{ConnectionConfig, Driver, Error, ObjectRef, QueryOutcome, ResultColumn, RowSink, RowSinkRef, Session};
use dbine_plugin::{DriverMeta, Launcher, RemoteDriver};
use serde_json::Value;
use std::sync::{Arc, Mutex};

const EXE: &str = env!("CARGO_BIN_EXE_dbine-plugin-host");

fn remote(package: &str, driver_id: &str) -> (Arc<dyn Driver>, Arc<dyn Driver>) {
    let (local, remote, _) = remote_with_launcher(package, driver_id);
    (local, remote)
}

fn remote_with_launcher(package: &str, driver_id: &str) -> (Arc<dyn Driver>, Arc<dyn Driver>, Arc<Launcher>) {
    let (_, drivers) = dbine_drivers::packages().into_iter().find(|(p, _)| *p == package).expect("package");
    let local = drivers.into_iter().find(|d| d.info().id == driver_id).expect("driver");
    let meta = DriverMeta::of(package, local.as_ref());
    let launcher = Launcher::at(package, EXE.into(), vec!["--package".into(), package.into()], None);
    (local, Arc::new(RemoteDriver::new(meta, launcher.clone())), launcher)
}

/// What a session answers, as JSON (so both sides compare as data).
async fn survey(s: &mut Box<dyn Session>, table: &str) -> Value {
    let obj = ObjectRef { kind: "table".into(), schema: None, name: table.into() };
    let mut out = QueryOutcome::default();
    let run = s.execute(&format!("SELECT * FROM {table} ORDER BY id"), 100, &mut out).await;
    let mut bad = QueryOutcome::default();
    let err = s.execute("SELECT * FROM no_existe", 10, &mut bad).await.err().map(|e| e.to_string());
    serde_json::json!({
        "databases": s.list_databases().await.map_err(|e| e.to_string()),
        "objects": serde_json::to_value(s.list_objects().await.map_err(|e| e.to_string()).ok()).unwrap(),
        "columns": serde_json::to_value(s.columns(&obj).await.ok()).unwrap(),
        "browse": s.browse_query(&obj, 50),
        "rows": out.results.iter().map(|r| &r.rows).collect::<Vec<_>>(),
        "columns_of_result": out.results.iter().map(|r| r.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "run_ok": run.is_ok(),
        "error": err,
        "schema": serde_json::to_value(s.database_schema().await.ok()).unwrap(),
    })
}

#[derive(Default)]
struct Collect {
    begins: Vec<(usize, Vec<String>)>,
    rows: Vec<(usize, Vec<Value>)>,
}
impl RowSink for Collect {
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> std::io::Result<()> {
        self.begins.push((index, columns.iter().map(|c| c.name.clone()).collect()));
        Ok(())
    }
    fn row(&mut self, index: usize, row: &[Value]) -> std::io::Result<()> {
        self.rows.push((index, row.to_vec()));
        Ok(())
    }
}

async fn streamed(s: &mut Box<dyn Session>, sql: &str) -> (Value, QueryOutcome) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let mut out = QueryOutcome { sink: Some(RowSinkRef(sink.clone())), sink_base: 2, ..Default::default() };
    s.execute(sql, 5, &mut out).await.unwrap();
    let c = sink.lock().unwrap();
    (serde_json::json!({ "begins": c.begins, "rows": c.rows }), out)
}

fn sqlite_file() -> (tempfile::TempDir, ConnectionConfig) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.db");
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.display().to_string(), ..Default::default() };
    (dir, cfg)
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_through_the_host_answers_the_same() {
    let (local, remote) = remote("sqlite", "sqlite");
    let (_dir, cfg) = sqlite_file();
    let mut setup = local.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    setup
        .execute(
            "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nombre TEXT NOT NULL, alta TEXT);
             INSERT INTO clientes (nombre, alta) VALUES ('Ana', '2024-01-02'), ('Beto', NULL), ('Carla', '2025-05-06');
             CREATE INDEX ix_nombre ON clientes (nombre);",
            10,
            &mut out,
        )
        .await
        .unwrap();
    drop(setup);

    let mut a = local.connect(&cfg, None).await.unwrap();
    let mut b = remote.connect(&cfg, None).await.unwrap();
    assert_eq!(survey(&mut a, "clientes").await, survey(&mut b, "clientes").await);
    assert_eq!(a.server_version().await.unwrap(), b.server_version().await.unwrap());

    // Rows to a sink: the same result-set numbers (base 2) and rows; the
    // row limit doesn't apply to a sink.
    let sql = "SELECT id, nombre FROM clientes ORDER BY id; SELECT count(*) AS n FROM clientes;";
    let (sa, oa) = streamed(&mut a, sql).await;
    let (sb, ob) = streamed(&mut b, sql).await;
    assert_eq!(sa, sb);
    assert_eq!(oa.results.len(), ob.results.len());
    assert!(ob.results.iter().all(|r| r.rows.is_empty()), "streamed rows don't stay in the outcome");

    // Driver-level methods go to the host too.
    let table = a.database_schema().await.unwrap().into_iter().find(|t| t.name == "clientes").unwrap();
    let parts = dbine_driver::DdlParts { create: true, indexes: true, ..Default::default() };
    assert_eq!(local.table_ddl(&table, parts).unwrap(), remote.table_ddl(&table, parts).unwrap());

    // A failing statement keeps what ran before it, on both sides.
    let (mut oa, mut ob) = (QueryOutcome::default(), QueryOutcome::default());
    let ea = a.execute("SELECT 1 AS uno; SELECT * FROM no_existe;", 10, &mut oa).await.unwrap_err();
    let eb = b.execute("SELECT 1 AS uno; SELECT * FROM no_existe;", 10, &mut ob).await.unwrap_err();
    assert_eq!(ea.to_string(), eb.to_string());
    assert_eq!(std::mem::discriminant(&ea), std::mem::discriminant(&eb));
    assert_eq!(oa.results.len(), ob.results.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_host_is_an_error_then_starts_again() {
    let (_local, remote, launcher) = remote_with_launcher("sqlite", "sqlite");
    let (_dir, cfg) = sqlite_file();
    let mut s = remote.connect(&cfg, None).await.unwrap();
    assert!(s.server_version().await.is_ok());
    // Kill this driver's host, as a crash would.
    let pid = launcher.pid().expect("running");
    let _ = std::process::Command::new("kill").arg("-9").arg(pid.to_string()).status();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let err = s.server_version().await.unwrap_err();
    assert!(matches!(err, Error::Connect(_)), "{err}");
    // The next connect starts a new host.
    let mut again = remote.connect(&cfg, None).await.unwrap();
    assert!(again.server_version().await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres_through_the_host_answers_the_same() {
    let url = std::env::var("DBINE_TEST_POSTGRES_URL").expect("DBINE_TEST_POSTGRES_URL");
    let u = url.trim_start_matches("postgres://");
    let (auth, rest) = u.split_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (hostport, db) = rest.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "postgres".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: db.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    let (local, remote) = remote("postgres", "postgres");
    let mut setup = local.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    setup
        .execute(
            "DROP TABLE IF EXISTS plugin_parity; CREATE TABLE plugin_parity (id serial PRIMARY KEY, nombre varchar(20) NOT NULL, monto numeric(10,2), alta timestamptz DEFAULT now());
             INSERT INTO plugin_parity (nombre, monto) SELECT 'n' || g, g * 1.5 FROM generate_series(1, 50) g;",
            10,
            &mut out,
        )
        .await
        .unwrap();
    let mut a = local.connect(&cfg, None).await.unwrap();
    let mut b = remote.connect(&cfg, None).await.unwrap();
    let (mut sa, mut sb) = (survey(&mut a, "plugin_parity").await, survey(&mut b, "plugin_parity").await);
    // `alta` is now(): the same, but not worth comparing to the microsecond.
    for s in [&mut sa, &mut sb] {
        s["rows"] = Value::Null;
    }
    assert_eq!(sa, sb);
    let (xa, _) = streamed(&mut a, "SELECT id, nombre, monto FROM plugin_parity ORDER BY id").await;
    let (xb, _) = streamed(&mut b, "SELECT id, nombre, monto FROM plugin_parity ORDER BY id").await;
    assert_eq!(xa, xb);
    assert_eq!(xb["rows"].as_array().unwrap().len(), 50);

    // Cancel: a long statement stops through the interrupter.
    let stop = b.interrupter().expect("postgres can cancel");
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        stop();
    });
    let started = std::time::Instant::now();
    let mut o = QueryOutcome::default();
    let r = b.execute("SELECT pg_sleep(20)", 10, &mut o).await;
    assert!(started.elapsed().as_secs() < 10, "cancel took {:?}", started.elapsed());
    assert!(r.is_err() || o.error.is_some());

    // Monitor through the host.
    let (ma, mb) = (a.monitor().await.map(|m| m.metrics.len()), b.monitor().await.map(|m| m.metrics.len()));
    assert_eq!(ma.is_ok(), mb.is_ok());
    let mut clean = QueryOutcome::default();
    setup.execute("DROP TABLE plugin_parity", 10, &mut clean).await.unwrap();
}

#[test]
fn a_driver_s_own_split_comes_through_the_host() {
    // Oracle cuts SQL*Plus lines itself, which the dialect alone can't.
    let (local, remote) = remote("oracle", "oracle");
    let sql = "SET SERVEROUTPUT ON\nBEGIN NULL; END;\n/\nPROMPT a;\nEXEC p(1);\nSELECT 1 FROM dual;\n";
    let units = remote.split_script(sql);
    assert_eq!(units, local.split_script(sql));
    assert_eq!(units.len(), 5, "{units:?}");
    assert_ne!(units, dbine_driver::sql::split_script(sql, &remote.script_dialect()));
}
