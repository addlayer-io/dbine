//! Against a real Flight SQL server (GizmoSQL, DuckDB behind):
//!
//! ```sh
//! docker run -d --name dbine-test-flightsql -p 25337:31337 -e TLS_ENABLED=0 \
//!   -e GIZMOSQL_PASSWORD=secreto1 gizmodata/gizmosql
//! DBINE_TEST_FLIGHTSQL_URL=http://localhost:25337 cargo test -p dbine-driver-flightsql -- --ignored
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use serde_json::json;
use std::time::{Duration, Instant};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_FLIGHTSQL_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "flightsql".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(std::env::var("DBINE_TEST_FLIGHTSQL_USER").unwrap_or_else(|_| "gizmosql_user".into())),
        password: Some(std::env::var("DBINE_TEST_FLIGHTSQL_PASSWORD").unwrap_or_else(|_| "secreto1".into())),
        ..Default::default()
    })
}

#[tokio::test]
#[ignore]
async fn flightsql() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_flightsql::drivers().remove(0);

    let mut bad = c.clone();
    bad.password = Some("nope".into());
    assert!(matches!(d.connect(&bad, None).await, Err(Error::AuthFailed(_))), "bad password");

    let mut s = d.connect(&c, None).await.unwrap();
    let v = s.server_version().await.unwrap();
    assert!(v.to_lowercase().contains("gizmo") || v.to_lowercase().contains("duckdb"), "{v}");
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"memory".to_string()), "{dbs:?}");

    let mut s = d.connect(&c, Some("memory")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP SCHEMA IF EXISTS dbine_it CASCADE; CREATE SCHEMA dbine_it;
         CREATE TABLE dbine_it.t (id BIGINT NOT NULL, nombre VARCHAR, total DECIMAL(10,2), ts TIMESTAMP, b BLOB, l INTEGER[]);
         INSERT INTO dbine_it.t VALUES (1, 'Ana', 10.5, TIMESTAMP '2024-01-31 13:45:00.123', '\\xCA\\xFE'::BLOB, [1, 2]),
                                       (2, 'O''Brien', 3, NULL, NULL, NULL), (9007199254740993, 'x', NULL, NULL, NULL, NULL);
         CREATE VIEW dbine_it.v AS SELECT id, nombre FROM dbine_it.t WHERE total > 5",
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[3].rows_affected, Some(3));

    let objs = s.list_objects().await.unwrap();
    let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n && o.schema.as_deref() == Some("dbine_it"));
    assert!(has(kinds::TABLE, "t") && has(kinds::VIEW, "v"), "{objs:?}");
    let t = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbine_it".into()), name: "t".into() };
    let cols = s.columns(&t).await.unwrap();
    assert_eq!(cols.len(), 6);
    assert!(!cols[0].nullable && cols[1].nullable, "{cols:?}");
    let def = s.definition(&ObjectRef { kind: kinds::VIEW.into(), schema: Some("dbine_it".into()), name: "v".into() }).await.unwrap().unwrap();
    assert!(def.to_uppercase().starts_with("CREATE VIEW"), "{def}");

    let q = s.browse_query(&t, 10);
    let mut out = QueryOutcome::default();
    s.execute(&format!("{q}; SELECT * FROM dbine_it.t ORDER BY id"), 2, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!((r.rows.len(), r.total_rows, r.truncated), (2, 3, true));
    let rows = &out.results[1].rows;
    assert_eq!(rows[0][0], json!(1));
    assert_eq!(rows[0][2], json!("10.50"));
    assert_eq!(rows[0][3], json!("2024-01-31 13:45:00.123"));
    assert_eq!(rows[0][4], json!("0xCAFE"));
    assert_eq!(rows[0][5], json!("[1, 2]"));
    let mut out = QueryOutcome::default();
    s.execute("SELECT id FROM dbine_it.t WHERE id > 2", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!("9007199254740993"));

    // Errors stop the script.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1; SELECT * FROM nope; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query() && e.to_string().contains("nope"), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // Plans (DuckDB's JSON).
    let mut out = QueryOutcome::default();
    s.explain("SELECT nombre, count(*) FROM dbine_it.t GROUP BY nombre; CREATE TABLE x (a INT)", false, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1);
    assert!(out.results.is_empty());
    fn find<'a>(n: &'a dbine_driver::PlanNode, op: &str) -> Option<&'a dbine_driver::PlanNode> {
        if n.op.contains(op) {
            return Some(n);
        }
        n.children.iter().find_map(|c| find(c, op))
    }
    let scan = find(&out.plans[0].root, "SCAN").unwrap_or_else(|| panic!("{:#?}", out.plans[0].root));
    assert!(scan.est_rows.is_some(), "{scan:#?}");
    let mut out = QueryOutcome::default();
    s.explain("SELECT nombre FROM dbine_it.t WHERE id < 5", true, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert!(out.plans[0].actual);
    let scan = find(&out.plans[0].root, "SCAN").unwrap_or_else(|| panic!("{:#?}", out.plans[0].root));
    assert!(scan.actual_rows.is_some(), "{scan:#?}");

    // INSERT scripts (the default SQL ones).
    let ins = d.insert_script(&t, &["id".into(), "nombre".into()], &[vec![json!(10), json!("O'Neil")]]).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&ins, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows_affected, Some(1));

    // Cancel a long query.
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        stop();
    });
    let t0 = Instant::now();
    let r = s.execute("SELECT sum(i * j) FROM range(200000000) a(i), range(1000) b(j)", 10, &mut QueryOutcome::default()).await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(t0.elapsed() < Duration::from_secs(8));
    // The session still works.
    s.execute("SELECT 1", 10, &mut QueryOutcome::default()).await.unwrap();

    // Read-only (the registry wraps SQL sessions).
    let mut ro = ReadOnlySession::new(d.connect(&c, Some("memory")).await.unwrap());
    assert!(ro.execute("DROP TABLE dbine_it.t", 10, &mut QueryOutcome::default()).await.is_err());

    s.execute("DROP SCHEMA dbine_it CASCADE", 10, &mut QueryOutcome::default()).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_flightsql::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    s.execute("CREATE OR REPLACE TABLE dbine_mon AS SELECT * FROM range(100000)", 10, &mut QueryOutcome::default()).await.unwrap();
    let snap = s.monitor().await.unwrap();
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    assert!(v("mem_used").is_some_and(|m| m > 0.0), "{:#?}", snap.metrics);
    assert!(snap.info.iter().any(|(k, _)| k == "Servidor"), "{:?}", snap.info);
    assert!(!snap.tables.iter().find(|t| t.key == "databases").unwrap().rows.is_empty());
    s.execute("DROP TABLE dbine_mon", 10, &mut QueryOutcome::default()).await.unwrap();
}

/// "Nuevo esquema…" / "Borrar esquema…" on GizmoSQL (DuckDB behind): the
/// schema shows in `information_schema.schemata`, dropping one with a table
/// needs CASCADE.
#[tokio::test]
#[ignore]
async fn create_and_drop_schema() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_flightsql::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    async fn count(s: &mut Box<dyn Session>) -> usize {
        let mut out = QueryOutcome::default();
        s.execute("SELECT schema_name FROM information_schema.schemata WHERE schema_name = 'dbine Sc'", 10, &mut out).await.unwrap();
        out.results[0].rows.len()
    }
    for cascade in [false, true] {
        let mut out = QueryOutcome::default();
        s.execute(&d.create_schema_script(None, "dbine Sc", None).unwrap(), 10, &mut out).await.unwrap();
        assert_eq!(count(&mut s).await, 1);
        if cascade {
            let mut out = QueryOutcome::default();
            s.execute("CREATE TABLE \"dbine Sc\".t (x INT)", 10, &mut out).await.unwrap();
            let mut out = QueryOutcome::default();
            assert!(s.execute(&d.drop_schema_script(None, "dbine Sc", false).unwrap(), 10, &mut out).await.is_err());
        }
        let mut out = QueryOutcome::default();
        s.execute(&d.drop_schema_script(None, "dbine Sc", cascade).unwrap(), 10, &mut out).await.unwrap();
        assert_eq!(count(&mut s).await, 0);
    }
}

/// A session opened on an attached catalog runs there: unqualified
/// scripts create and drop in it, never in the default one (where a schema
/// with the same name must survive), and it lists the new empty schema. The
/// scripts with the menu's catalog land there even from a session in the
/// default catalog.
#[tokio::test]
#[ignore]
async fn schema_in_the_open_catalog() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_flightsql::drivers().remove(0);
    async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
        let mut out = QueryOutcome::default();
        s.execute(sql, 10, &mut out).await.unwrap();
        out
    }
    let mut def = d.connect(&c, None).await.unwrap();
    run(&mut def, "ATTACH IF NOT EXISTS ':memory:' AS dbine_other").await;
    run(&mut def, "CREATE SCHEMA IF NOT EXISTS memory.\"Mi \"\"Esq\"; CREATE OR REPLACE TABLE memory.\"Mi \"\"Esq\".keep (x INT)").await;

    let mut s = d.connect(&c, Some("dbine_other")).await.unwrap();
    assert_eq!(run(&mut s, "SELECT current_database()").await.results[0].rows[0][0], json!("dbine_other"));
    run(&mut s, &d.create_schema_script(None, "Mi \"Esq", None).unwrap()).await;
    let schemas = s.list_schemas().await.unwrap().expect("GetDbSchemas");
    assert!(schemas.iter().any(|x| x.name == "Mi \"Esq" && !x.system), "{schemas:?}");
    let where_ = "SELECT catalog_name FROM information_schema.schemata WHERE schema_name = 'Mi \"Esq' ORDER BY 1";
    let cats = run(&mut def, where_).await.results[0].rows.clone();
    assert_eq!(cats, vec![vec![json!("dbine_other")], vec![json!("memory")]]);

    run(&mut s, &d.drop_schema_script(None, "Mi \"Esq", true).unwrap()).await;
    let cats = run(&mut def, where_).await.results[0].rows.clone();
    assert_eq!(cats, vec![vec![json!("memory")]], "the default catalog's schema is untouched");
    assert_eq!(run(&mut def, "SELECT count(*) FROM memory.\"Mi \"\"Esq\".keep").await.results[0].rows.len(), 1);

    // Qualified with the menu's catalog, run from the default catalog.
    run(&mut def, &d.create_schema_script(Some("dbine_other"), "Mi \"Esq", None).unwrap()).await;
    assert_eq!(run(&mut def, where_).await.results[0].rows, vec![vec![json!("dbine_other")], vec![json!("memory")]]);
    let schemas = s.list_schemas().await.unwrap().expect("GetDbSchemas");
    assert!(schemas.iter().any(|x| x.name == "Mi \"Esq" && !x.system), "{schemas:?}");
    assert!(schemas.iter().all(|x| x.system == ["information_schema", "pg_catalog"].contains(&x.name.as_str())), "{schemas:?}");
    run(&mut def, &d.drop_schema_script(Some("dbine_other"), "Mi \"Esq", false).unwrap()).await;
    assert_eq!(run(&mut def, where_).await.results[0].rows, vec![vec![json!("memory")]]);

    // USE switches the catalog: the tab and the explorer follow it.
    let mut out = QueryOutcome::default();
    s.execute("USE memory", 10, &mut out).await.unwrap();
    assert_eq!(out.database.as_deref(), Some("memory"));
    let mut out = QueryOutcome::default();
    s.execute("SELECT current_database()", 10, &mut out).await.unwrap();
    assert_eq!((out.database.as_deref(), &out.results[0].rows[0][0]), (None, &json!("memory")));
    let mut out = QueryOutcome::default();
    s.execute("USE dbine_other", 10, &mut out).await.unwrap();
    assert_eq!(out.database.as_deref(), Some("dbine_other"));

    // A catalog that doesn't exist fails to open instead of falling back.
    assert!(d.connect(&c, Some("dbine_nope")).await.is_err());
    run(&mut def, "DROP SCHEMA memory.\"Mi \"\"Esq\" CASCADE; DETACH dbine_other").await;
}

/// The editor's script contract: one command per statement, errors with
/// the engine's class and line, Flight SQL transactions when the server has
/// them.
#[tokio::test]
#[ignore]
async fn script_errors_and_transactions() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_flightsql::drivers().remove(0);
    assert_eq!(d.script_mode(), dbine_driver::sql::ScriptMode::PerStatement);
    let mut s = d.connect(&c, Some("memory")).await.unwrap();
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT $$a;b$$ AS x;\nSELECT *\n  FROM nope_nope", 10, &mut out).await.unwrap_err().to_script_error();
    eprintln!("{e:?}");
    assert_eq!(out.results[0].rows[0][0], json!("a;b"));
    assert_eq!((e.code.as_deref(), e.line), (Some("Catalog"), Some(3)), "{e:?}");

    let mut go = QueryOutcome::default();
    s.execute("CREATE OR REPLACE TABLE dbine_tx (id INTEGER)", 10, &mut go).await.unwrap();
    match s.set_autocommit(false).await {
        Err(Error::Unsupported(m)) => {
            eprintln!("no transactions: {m}");
            return;
        }
        r => r.unwrap(),
    }
    s.execute("INSERT INTO dbine_tx VALUES (1)", 10, &mut go).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Open));
    s.rollback().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Idle));
    s.execute("INSERT INTO dbine_tx VALUES (2)", 10, &mut go).await.unwrap();
    s.commit().await.unwrap();
    // An error that aborts the transaction reports it Failed; the next
    // failing query leaves no empty grid next to its error.
    s.execute("CREATE OR REPLACE TABLE dbine_tx_pk (id INTEGER PRIMARY KEY)", 10, &mut go).await.unwrap();
    s.commit().await.unwrap();
    s.execute("INSERT INTO dbine_tx_pk VALUES (1)", 10, &mut go).await.unwrap();
    let e = s.execute("INSERT INTO dbine_tx_pk VALUES (1)", 10, &mut go).await.unwrap_err();
    eprintln!("duplicate: {e}");
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Failed));
    let mut failed = QueryOutcome::default();
    assert!(s.execute("SELECT 1", 10, &mut failed).await.is_err());
    assert!(failed.results.is_empty(), "{:?}", failed.results);
    s.rollback().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Idle));
    // A binder error doesn't abort it.
    s.execute("INSERT INTO dbine_tx_pk VALUES (3)", 10, &mut go).await.unwrap();
    assert!(s.execute("SELECT nope FROM dbine_tx_pk", 10, &mut go).await.is_err());
    let st = s.transaction_state().await.unwrap();
    eprintln!("after a binder error: {st:?}");
    s.rollback().await.unwrap();
    s.execute("DROP TABLE dbine_tx_pk", 10, &mut go).await.unwrap();
    s.commit().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT list(id) FROM dbine_tx", 10, &mut out).await.unwrap();
    eprintln!("{:?}", out.results[0].rows);
    assert_eq!(out.results[0].rows[0][0].to_string().replace(' ', ""), "\"[2]\"");
    s.execute("DROP TABLE dbine_tx", 10, &mut go).await.unwrap();
}
