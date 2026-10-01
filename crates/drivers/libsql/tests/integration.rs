//! Against a real libSQL server (sqld):
//!
//! ```sh
//! docker run -d --name dbine-test-libsql -p 25880:8080 ghcr.io/tursodatabase/libsql-server:latest
//! DBINE_TEST_LIBSQL_URL=http://localhost:25880 cargo test -p dbine-driver-libsql -- --ignored
//! ```
//!
//! For Turso, `DBINE_TEST_LIBSQL_URL=libsql://…` and `DBINE_TEST_LIBSQL_TOKEN`.

use dbine_driver::{kinds, ConnectionConfig, DdlParts, Error, ObjectRef, QueryOutcome, Session};
use std::sync::Arc;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_LIBSQL_URL").ok()?;
    let mut cfg = ConnectionConfig { driver: "libsql".into(), host: url, ..Default::default() };
    if let Ok(t) = std::env::var("DBINE_TEST_LIBSQL_TOKEN") {
        cfg.options.insert("auth_token".into(), t);
    }
    Some(cfg)
}

fn driver() -> Arc<dyn dbine_driver::Driver> {
    dbine_driver_libsql::drivers().remove(0)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 1000, &mut out).await {
        panic!("{sql}: {e}");
    }
    out
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

#[tokio::test]
#[ignore]
async fn full_session() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_LIBSQL_URL not set; skipping");
        return;
    };
    let d = driver();
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let v = s.server_version().await.unwrap();
    eprintln!("{v}");
    assert!(v.starts_with("libSQL"));
    run(
        &mut s,
        "DROP TABLE IF EXISTS pedidos; DROP TABLE IF EXISTS clientes; DROP VIEW IF EXISTS v_pedidos;
         CREATE TABLE clientes (id INTEGER PRIMARY KEY AUTOINCREMENT, nombre TEXT NOT NULL DEFAULT 'x', foto BLOB);
         CREATE TABLE pedidos (id INTEGER PRIMARY KEY, cliente_id INTEGER NOT NULL REFERENCES clientes (id) ON DELETE CASCADE,
                               total REAL, nota TEXT DEFAULT ';');
         CREATE INDEX ix_pedidos_cliente ON pedidos (cliente_id);
         CREATE VIEW v_pedidos AS SELECT id, total FROM pedidos;
         CREATE TRIGGER tr_pedidos AFTER INSERT ON pedidos BEGIN UPDATE pedidos SET nota = 'a;b' WHERE id = NEW.id; END;
         INSERT INTO clientes (nombre, foto) VALUES ('Ana', x'CAFE'), ('Beto', NULL);
         INSERT INTO pedidos (cliente_id, total) VALUES (1, 10.5), (2, 3), (1, 7);",
    )
    .await;

    let objs = s.list_objects().await.unwrap();
    let names: Vec<(&str, &str)> = objs.iter().map(|o| (o.kind.as_str(), o.name.as_str())).collect();
    for want in [(kinds::TABLE, "clientes"), (kinds::TABLE, "pedidos"), (kinds::VIEW, "v_pedidos"), (kinds::TRIGGER, "tr_pedidos")] {
        assert!(names.contains(&want), "{want:?} in {names:?}");
    }
    let cols = s.columns(&obj(kinds::TABLE, "clientes")).await.unwrap();
    assert!(cols[0].primary_key && cols[0].auto_increment);
    assert_eq!(cols[1].default_value.as_deref(), Some("'x'"));
    assert!(s.definition(&obj(kinds::TRIGGER, "tr_pedidos")).await.unwrap().unwrap().contains("'a;b'"));

    let out = run(&mut s, "SELECT c.nombre, c.foto, SUM(p.total) AS total, MIN(p.nota) FROM clientes c JOIN pedidos p ON p.cliente_id = c.id GROUP BY c.id ORDER BY c.id").await;
    let r = &out.results[0];
    assert_eq!(r.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["nombre", "foto", "total", "MIN(p.nota)"]);
    assert_eq!(r.rows[0], vec![serde_json::json!("Ana"), serde_json::json!("0xCAFE"), serde_json::json!(17.5), serde_json::json!("a;b")]);
    let q = s.browse_query(&obj(kinds::TABLE, "pedidos"), 2);
    let out = run(&mut s, &q).await;
    assert_eq!(out.results[0].rows.len(), 2);

    // A failing statement stops the script and keeps what ran.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1; SELECT * FROM nope; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query() && e.to_string().contains("no such table"), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // The stream keeps the session: a transaction spans executes.
    run(&mut s, "BEGIN; DELETE FROM pedidos WHERE id = 3").await;
    let out = run(&mut s, "SELECT COUNT(*) FROM pedidos").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(2));
    run(&mut s, "ROLLBACK").await;
    let out = run(&mut s, "SELECT COUNT(*) FROM pedidos").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(3));

    // Structure, DDL round trip.
    let schema = s.database_schema().await.unwrap();
    let pedidos = schema.iter().find(|t| t.name == "pedidos").unwrap();
    assert_eq!(pedidos.foreign_keys[0].ref_table, "clientes");
    assert_eq!(pedidos.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    assert_eq!(pedidos.indexes[0].name, "ix_pedidos_cliente");
    let ddl = d.table_ddl(pedidos, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
    assert!(ddl.contains("REFERENCES \"clientes\""), "{ddl}");

    // Plans.
    let mut est = QueryOutcome::default();
    s.explain("SELECT * FROM pedidos WHERE cliente_id = 1; DELETE FROM pedidos WHERE id = 99", false, 10, &mut est).await.unwrap();
    assert_eq!(est.plans.len(), 2);
    assert!(est.plans[0].raw.contains("ix_pedidos_cliente"), "{}", est.plans[0].raw);

    // Monitor.
    assert!(d.capabilities().monitor);
    let snap = s.monitor().await.unwrap();
    for m in &snap.metrics {
        eprintln!("{:<14} {:<44} {:?}", m.key, m.label, m.value);
    }
    eprintln!("{:?}\n{:?}\n{:?}", snap.info, snap.tables, snap.notes);
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    assert!(v("storage_used").unwrap() > 0.0 && v("pages").unwrap() > 1.0);
    assert!(snap.tables.iter().any(|t| t.key == "top_objects" && t.rows.iter().any(|r| r[0] == "pedidos")));
    assert!(snap.info.iter().any(|(k, _)| k == "URL"));

    // Read-only: the server refuses writes too.
    let mut ro_cfg = cfg.clone();
    ro_cfg.read_only = true;
    let mut ro = d.connect(&ro_cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    assert!(ro.execute("DELETE FROM pedidos", 10, &mut out).await.is_err());
    run(&mut s, "DROP VIEW v_pedidos; DROP TABLE pedidos; DROP TABLE clientes").await;
}

#[tokio::test]
#[ignore]
async fn bad_token_is_auth_failed_or_ignored() {
    let Some(mut cfg) = config() else { return };
    cfg.host = "http://127.0.0.1:9".into();
    assert!(matches!(driver().connect(&cfg, None).await.err(), Some(Error::Connect(_))));
}

/// An idle stream expires on the server; the next statement runs on a new
/// one (with the session's PRAGMAs again) and the user is told.
#[tokio::test]
#[ignore]
async fn expired_stream_is_renewed() {
    let Some(cfg) = config() else { return };
    let mut s = driver().connect(&cfg, None).await.unwrap();
    run(&mut s, "SELECT 1").await;
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    let out = run(&mut s, "PRAGMA foreign_keys").await;
    eprintln!("{:?}", out.messages);
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(1));
    assert!(out.messages.iter().any(|m| m.contains("venció")), "{:?}", out.messages);
}

/// Schema sync against sqld: columns added in place, the rest rebuilt, and
/// a second comparison finds nothing left to change.
#[tokio::test]
#[ignore]
async fn schema_sync_applies() {
    use dbine_driver::{ColumnDef, ForeignKeyDef, IndexDef, TableChange, TableSchema};
    let Some(cfg) = config() else { return };
    let d = driver();
    assert!(d.supports_schema_sync());
    let mut s = d.connect(&cfg, None).await.unwrap();
    for q in ["DROP TABLE IF EXISTS sync_hijo", "DROP TABLE IF EXISTS sync_padre"] {
        run(&mut s, q).await;
    }
    run(
        &mut s,
        "CREATE TABLE sync_padre (id INTEGER PRIMARY KEY, codigo INTEGER);
         CREATE TABLE sync_hijo (id INTEGER PRIMARY KEY, padre_id INTEGER REFERENCES sync_padre (id), nombre VARCHAR(20) NOT NULL, baja DATE);
         CREATE INDEX ix_sync_hijo_nombre ON sync_hijo (nombre);
         INSERT INTO sync_padre VALUES (1, 10), (5, 50);
         INSERT INTO sync_hijo VALUES (1, 1, 'uno', NULL);",
    )
    .await;
    let find = |schema: &[TableSchema], n: &str| schema.iter().find(|t| t.name == n).unwrap().clone();
    let old = find(&s.database_schema().await.unwrap(), "sync_hijo");
    let mut new = old.clone();
    new.columns.push(ColumnDef { name: "email".into(), data_type: "TEXT".into(), nullable: true, ..Default::default() });
    let script = d.sync_script(&[TableChange::Alter { old, new: new.clone() }]).unwrap();
    assert_eq!(script.statements.len(), 1, "{:?}", script.statements);
    for st in &script.statements {
        run(&mut s, st).await;
    }
    let mut last = find(&s.database_schema().await.unwrap(), "sync_hijo");
    let nombre = last.columns.iter_mut().find(|c| c.name == "nombre").unwrap();
    nombre.data_type = "VARCHAR(40)".into();
    nombre.nullable = true;
    last.columns.retain(|c| c.name != "baja");
    last.indexes = vec![IndexDef { name: "ix_sync_hijo_email".into(), columns: vec!["email".into()], unique: false, kind: None, filter: None, ..Default::default() }];
    last.foreign_keys = vec![ForeignKeyDef { columns: vec!["padre_id".into()], ref_table: "sync_padre".into(), ref_columns: vec!["id".into()], on_delete: Some("CASCADE".into()), ..Default::default() }];
    let now = find(&s.database_schema().await.unwrap(), "sync_hijo");
    let script = d.sync_script(&[TableChange::Alter { old: now, new: last.clone() }]).unwrap();
    eprintln!("{}\n{:?}", script.statements.join("\n"), script.warnings);
    for st in &script.statements {
        run(&mut s, st).await;
    }
    let now = find(&s.database_schema().await.unwrap(), "sync_hijo");
    let left = d.sync_script(&[TableChange::Alter { old: now, new: last.clone() }]).unwrap();
    assert!(left.statements.is_empty(), "{:?}", left.statements);
    let out = run(&mut s, "SELECT nombre FROM sync_hijo").await;
    assert_eq!(out.results[0].rows.len(), 1);
    let padre = find(&s.database_schema().await.unwrap(), "sync_padre");
    let script = d.sync_script(&[TableChange::Drop { table: last }, TableChange::Drop { table: padre }]).unwrap();
    for st in &script.statements {
        run(&mut s, st).await;
    }
}

/// The editor's script contract: one statement per call, errors with their
/// code and line, manual transactions over Hrana 3.
#[tokio::test]
#[ignore]
async fn script_statements_errors_and_transactions() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_LIBSQL_URL not set; skipping");
        return;
    };
    let d = driver();
    assert_eq!(d.script_mode(), dbine_driver::sql::ScriptMode::PerStatement);
    assert!(d.script_defaults().continue_on_error);
    let units = d.split_script("CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END;\nSELECT [a;b] FROM t;");
    assert_eq!(units.len(), 2, "{units:?}");
    let mut s = d.connect(&cfg, None).await.expect("connect");
    run(&mut s, "DROP TABLE IF EXISTS tx_t").await;
    run(&mut s, "CREATE TABLE tx_t (id INTEGER PRIMARY KEY)").await;
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1 FROM\n  nope_nope", 10, &mut out).await.unwrap_err().to_script_error();
    eprintln!("{e:?}");
    assert!(e.code.is_some(), "{e:?}");
    let mut out = QueryOutcome::default();
    let e = s.execute("SELEC 1", 10, &mut out).await.unwrap_err().to_script_error();
    eprintln!("{e:?}");
    assert!(e.code.is_some(), "{e:?}");

    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Idle));
    s.set_autocommit(false).await.expect("manual");
    run(&mut s, "SELECT 1").await;
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Idle), "a read opens nothing");
    run(&mut s, "INSERT INTO tx_t VALUES (1)").await;
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Open));
    s.rollback().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Idle));
    run(&mut s, "INSERT INTO tx_t VALUES (2)").await;
    s.commit().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    let out = run(&mut s, "SELECT group_concat(id) FROM tx_t").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!("2"));
    run(&mut s, "DROP TABLE tx_t").await;
}
