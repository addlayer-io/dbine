//! DSQL has no emulator: this drives the wire and catalog code through the
//! password test hook against a plain PostgreSQL.
//!   docker run -d --name dbine-test-dsqlpg -e POSTGRES_PASSWORD=dbine -p 25301:5432 postgres:16
//!   DBINE_TEST_DSQL_URL=localhost:25301 cargo test -p dbine-driver-dsql -- --ignored
//! (user postgres, password dbine).

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome};
use serde_json::json;

#[tokio::test]
#[ignore]
async fn wire_and_catalog() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else { return };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "dsql".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some("postgres".into()),
        ..Default::default()
    };
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    assert!(s.server_version().await.unwrap().contains("PostgreSQL"));
    assert_eq!(s.list_databases().await.unwrap(), vec!["postgres"]);

    let mut out = QueryOutcome::default();
    s.execute(
        "DROP VIEW IF EXISTS dq_v; DROP TABLE IF EXISTS dq_t; DROP SEQUENCE IF EXISTS dq_s;
         CREATE TABLE dq_t (id int PRIMARY KEY, name varchar(20) NOT NULL DEFAULT 'x', amount numeric(10,2),
                            ok boolean, raw bytea, at timestamptz, big bigint, f double precision);
         CREATE VIEW dq_v AS SELECT id, name FROM dq_t;
         CREATE SEQUENCE dq_s START WITH 5;
         INSERT INTO dq_t VALUES (1, 'a', 1.50, true, '\\xcafe', '2024-01-31 13:45:00+00', 9007199254740993, 0.5),
                                 (2, 'b', NULL, false, NULL, NULL, 3, NULL);",
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results.last().unwrap().rows_affected, Some(2));

    let mut out = QueryOutcome::default();
    s.execute("SELECT * FROM dq_t ORDER BY id; SELECT 1 AS one", 1, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!(r.columns[2].type_name, "numeric");
    assert_eq!(r.rows.len(), 1);
    assert!(r.truncated);
    assert_eq!(
        r.rows[0],
        vec![json!(1), json!("a"), json!("1.50"), json!(true), json!("0xCAFE"), json!("2024-01-31 13:45:00+00"),
             json!("9007199254740993"), json!(0.5)]
    );
    assert_eq!(out.results[1].rows[0], vec![json!(1)]);

    let objs = s.list_objects().await.unwrap();
    let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n && o.schema.as_deref() == Some("public"));
    assert!(has(kinds::TABLE, "dq_t") && has(kinds::VIEW, "dq_v") && has(kinds::SEQUENCE, "dq_s"));

    let t = ObjectRef { kind: kinds::TABLE.into(), schema: Some("public".into()), name: "dq_t".into() };
    let cols = s.columns(&t).await.unwrap();
    assert_eq!(cols[0].name, "id");
    assert!(cols[0].primary_key && !cols[0].nullable);
    assert_eq!(cols[1].data_type, "character varying(20)");
    let v = ObjectRef { kind: kinds::VIEW.into(), schema: Some("public".into()), name: "dq_v".into() };
    assert!(s.definition(&v).await.unwrap().unwrap().starts_with("CREATE OR REPLACE VIEW \"public\".\"dq_v\" AS"));
    let q = ObjectRef { kind: kinds::SEQUENCE.into(), schema: Some("public".into()), name: "dq_s".into() };
    assert!(s.definition(&q).await.unwrap().unwrap().contains("START WITH 5"));

    let mut out = QueryOutcome::default();
    assert!(s.execute("SELECT 1; SELECT nope", 10, &mut out).await.is_err());
    assert_eq!(out.results.len(), 1);

    // Cancel a running statement from another task.
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        stop();
    });
    let mut out = QueryOutcome::default();
    assert!(matches!(s.execute("SELECT pg_sleep(10)", 10, &mut out).await, Err(dbine_driver::Error::Cancelled)));

    let mut out = QueryOutcome::default();
    s.execute("DROP VIEW dq_v; DROP TABLE dq_t; DROP SEQUENCE dq_s", 10, &mut out).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn plans() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else { return };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "dsql".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some("postgres".into()),
        ..Default::default()
    };
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("DROP TABLE IF EXISTS dq_p; CREATE TABLE dq_p (id int PRIMARY KEY, g int); INSERT INTO dq_p SELECT i, i % 3 FROM generate_series(1, 50) i", 10, &mut out)
        .await
        .unwrap();

    // Estimated: nothing runs, not even the INSERT.
    let mut out = QueryOutcome::default();
    s.explain("SELECT g, count(*) FROM dq_p GROUP BY g; INSERT INTO dq_p VALUES (100, 1); CREATE TABLE dq_x (a int)", false, 10, &mut out)
        .await
        .unwrap();
    assert_eq!(out.plans.len(), 2);
    assert!(out.results.is_empty());
    assert!(!out.plans[0].actual && out.plans[0].raw_format == "json");
    assert!(out.plans[0].root.total_cost.is_some());
    assert_eq!(out.plans[1].root.op, "Insert");
    assert_eq!(out.messages.len(), 1);

    // Actual: the read runs (results + measured plan), the write runs once.
    let mut out = QueryOutcome::default();
    s.explain("SELECT * FROM dq_p WHERE id = 7; INSERT INTO dq_p VALUES (100, 1)", true, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 2);
    assert!(out.plans[0].actual);
    assert_eq!(out.plans[0].root.actual_rows, Some(1.0));
    assert!(!out.plans[1].actual);
    assert_eq!(out.results[0].rows.len(), 1);
    assert_eq!(out.results[1].rows_affected, Some(1));
    let mut out = QueryOutcome::default();
    s.execute("SELECT count(*) FROM dq_p WHERE id = 100; DROP TABLE dq_p", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0], vec![json!(1)]);
}

/// Catalog → DDL → fresh schema, with DSQL's subset (no foreign keys, no
/// comments). `CREATE INDEX ASYNC` is DSQL-only, so it runs here as a plain
/// CREATE INDEX; that and the other DSQL restrictions are unit-tested only.
#[tokio::test]
#[ignore]
async fn schema_ddl_round_trip() {
    use dbine_driver::{DdlParts, TableSchema};
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else { return };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "dsql".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some("postgres".into()),
        ..Default::default()
    };
    let driver = dbine_driver_dsql::drivers().pop().unwrap();
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP SCHEMA IF EXISTS dq_src CASCADE; DROP SCHEMA IF EXISTS dq_dst CASCADE; CREATE SCHEMA dq_src; CREATE SCHEMA dq_dst;
         CREATE TABLE dq_src.clientes (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), email varchar(120) NOT NULL, alta timestamptz DEFAULT now());
         CREATE TABLE dq_src.pedidos (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, cliente_id uuid NOT NULL, total numeric(12,2), ok boolean);
         CREATE TABLE dq_src.lineas (pedido_id bigint, n int, producto text, CONSTRAINT lineas_pk PRIMARY KEY (pedido_id, n));
         CREATE UNIQUE INDEX ux_email ON dq_src.clientes (email);
         CREATE INDEX ix_cliente ON dq_src.pedidos (cliente_id, ok);",
        10,
        &mut out,
    )
    .await
    .unwrap();

    let of = |all: &[TableSchema], schema: &str| -> Vec<TableSchema> {
        all.iter().filter(|t| t.schema.as_deref() == Some(schema)).cloned().collect()
    };
    let src = of(&s.database_schema().await.unwrap(), "dq_src");
    assert_eq!(src.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["clientes", "lineas", "pedidos"]);
    let clientes = &src[0];
    assert_eq!(clientes.columns[1].data_type, "character varying(120)");
    assert!(!clientes.columns[1].nullable);
    assert_eq!(clientes.columns[0].default_value.as_deref(), Some("gen_random_uuid()"));
    assert_eq!(clientes.primary_key.as_ref().unwrap().name.as_deref(), Some("clientes_pkey"));
    assert!(clientes.indexes.iter().any(|i| i.name == "ux_email" && i.unique && i.columns == ["email"]));
    assert_eq!(src[1].primary_key.as_ref().unwrap().columns, ["pedido_id", "n"]);
    let pedidos = &src[2];
    assert!(pedidos.columns[0].auto_increment && pedidos.columns[0].default_value.is_none());
    assert!(pedidos.indexes.iter().any(|i| i.name == "ix_cliente" && !i.unique && i.columns == ["cliente_id", "ok"]));
    assert!(src.iter().all(|t| t.foreign_keys.is_empty()));

    // Round trip into dq_dst: every create first, then indexes.
    let moved: Vec<TableSchema> = src.iter().cloned().map(|mut t| {
        t.schema = Some("dq_dst".into());
        t
    }).collect();
    let mut script: Vec<String> = moved
        .iter()
        .map(|t| driver.table_ddl(t, DdlParts { create: true, drop: true, if_exists: true, ..Default::default() }).unwrap())
        .collect();
    script.extend(moved.iter().map(|t| driver.table_ddl(t, DdlParts { indexes: true, foreign_keys: true, ..Default::default() }).unwrap()));
    let script = script.join("\n");
    assert!(script.contains("CREATE UNIQUE INDEX ASYNC \"ux_email\" ON \"dq_dst\".\"clientes\" (\"email\");"), "{script}");
    let mut out = QueryOutcome::default();
    s.execute(&script.replace(" INDEX ASYNC ", " INDEX "), 10, &mut out).await.unwrap();
    let dst = of(&s.database_schema().await.unwrap(), "dq_dst");
    assert_eq!(dst, moved);

    // insert_script output runs.
    let target = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dq_dst".into()), name: "pedidos".into() };
    let ins = driver
        .insert_script(
            &target,
            &["cliente_id".into(), "total".into(), "ok".into()],
            &[vec![json!("7b4c1a3e-0000-4000-8000-000000000001"), json!("1.50"), json!(true)], vec![json!("7b4c1a3e-0000-4000-8000-000000000002"), json!(null), json!(false)]],
        )
        .unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&ins, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows_affected, Some(2));

    assert!(matches!(s.create_database("x").await, Err(dbine_driver::Error::Unsupported(_))));
    let mut out = QueryOutcome::default();
    s.execute("DROP SCHEMA dq_src CASCADE; DROP SCHEMA dq_dst CASCADE", 10, &mut out).await.unwrap();
}

/// The monitor over the same wire (sessions, running statements, notes).
#[tokio::test]
#[ignore]
async fn monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else { return };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "dsql".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some("postgres".into()),
        ..Default::default()
    };
    let driver = dbine_driver_dsql::drivers().remove(0);
    assert!(driver.capabilities().monitor);
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    let snap = s.monitor().await.unwrap();
    for m in &snap.metrics {
        eprintln!("dsql: {} = {:?}", m.key, m.value);
    }
    for t in &snap.tables {
        eprintln!("dsql: table {} {} rows", t.key, t.rows.len());
        assert!(t.rows.iter().all(|r| r.len() == t.columns.len()));
    }
    for n in &snap.notes {
        eprintln!("dsql: note {n}");
    }
    assert!(snap.metrics.iter().filter(|m| m.value.is_some()).count() >= 3);
    assert!(snap.info.iter().any(|(k, _)| k == "Versión"));
    assert!(snap.tables.iter().any(|t| t.key == "sessions"));
}

/// Schema sync over the PostgreSQL stand-in: the script runs (with
/// `CREATE INDEX ASYNC` as a plain CREATE INDEX) and syncing again from
/// what the database now has gives nothing.
#[tokio::test]
#[ignore]
async fn schema_sync() {
    use dbine_driver::{ColumnDef, IndexDef, KeyDef, TableChange, TableSchema};
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else { return };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig { driver: "dsql".into(), host: host.into(), port: port.parse().unwrap(), username: Some("postgres".into()), ..Default::default() };
    let d = dbine_driver_dsql::drivers().pop().unwrap();
    assert!(d.supports_schema_sync());
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP SCHEMA IF EXISTS dq_sync CASCADE; CREATE SCHEMA dq_sync;
         CREATE TABLE dq_sync.t (id integer PRIMARY KEY, nombre varchar(10) NOT NULL, baja date, nota text DEFAULT 'x');
         CREATE INDEX ix_baja ON dq_sync.t (baja);
         CREATE TABLE dq_sync.vieja (id integer PRIMARY KEY);
         INSERT INTO dq_sync.t (id, nombre, baja) VALUES (1, 'uno', '2024-01-01');",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let of = |all: Vec<TableSchema>, n: &str| all.into_iter().find(|t| t.schema.as_deref() == Some("dq_sync") && t.name == n);
    let all = s.database_schema().await.unwrap();
    let old = of(all.clone(), "t").unwrap();
    let vieja = of(all, "vieja").unwrap();
    let mut new = old.clone();
    new.columns.retain(|c| c.name != "baja");
    let nombre = new.columns.iter_mut().find(|c| c.name == "nombre").unwrap();
    nombre.nullable = true;
    new.columns.iter_mut().find(|c| c.name == "nota").unwrap().default_value = Some("'y'::text".into());
    new.columns.push(ColumnDef { name: "email".into(), data_type: "text".into(), nullable: true, default_value: Some("'s/n'::text".into()), ..Default::default() });
    new.indexes = vec![IndexDef { name: "ix_email".into(), columns: vec!["email".into()], unique: true, kind: None, filter: None, ..Default::default() }];
    let nueva = TableSchema {
        kind: "table".into(),
        schema: Some("dq_sync".into()),
        name: "nueva".into(),
        columns: vec![ColumnDef { name: "id".into(), data_type: "integer".into(), nullable: false, ..Default::default() }],
        primary_key: Some(KeyDef { name: Some("nueva_pkey".into()), columns: vec!["id".into()] }),
        ..Default::default()
    };
    let script = d
        .sync_script(&[TableChange::Alter { old, new: new.clone() }, TableChange::Drop { table: vieja }, TableChange::Create { table: nueva }])
        .unwrap();
    eprintln!("{script:#?}");
    for stmt in &script.statements {
        let stmt = stmt.replace(" INDEX ASYNC ", " INDEX ");
        s.execute(&stmt, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{stmt}"));
    }
    let all = s.database_schema().await.unwrap();
    assert!(of(all.clone(), "vieja").is_none());
    assert!(of(all.clone(), "nueva").is_some());
    let now = of(all, "t").unwrap();
    let again = d.sync_script(&[TableChange::Alter { old: now.clone(), new }]).unwrap();
    assert!(again.statements.is_empty(), "{:?}\n{now:#?}", again.statements);
    s.execute("INSERT INTO dq_sync.t (id) VALUES (2); SELECT email, nota FROM dq_sync.t WHERE id = 2", 10, &mut out).await.unwrap();
    s.execute("DROP SCHEMA dq_sync CASCADE", 10, &mut out).await.unwrap();
}

/// The editor's script contract: SQLSTATE and position in the script,
/// notices, manual transactions (a failed one until rolled back).
#[tokio::test]
#[ignore]
async fn script_errors_notices_and_transactions() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else { return };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig { driver: "dsql".into(), host: host.into(), port: port.parse().unwrap(), username: Some("postgres".into()), ..Default::default() };
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    let mut other = dbine_driver_dsql::connect_with_password(&cfg, "dbine").await.unwrap();
    let script = "SELECT $$a;b$$;\nSELECT 1 FROM\n  nope_nope;";
    let mut out = QueryOutcome::default();
    let e = s.execute(script, 10, &mut out).await.unwrap_err().to_script_error();
    assert_eq!((e.sqlstate.as_deref(), e.line), (Some("42P01"), Some(3)), "{e:?}");
    assert_eq!(e.offset, Some(script.find("nope_nope").unwrap()));
    assert_eq!(out.results[0].rows[0][0], json!("a;b"));
    let mut out = QueryOutcome::default();
    s.execute("DO $$ BEGIN RAISE WARNING 'cuidado'; END $$", 10, &mut out).await.unwrap();
    assert!(out.log.iter().any(|m| m.text.contains("cuidado") && m.level == dbine_driver::MessageLevel::Warning), "{:?}", out.log);

    let mut go = QueryOutcome::default();
    s.execute("DROP TABLE IF EXISTS dq_tx; CREATE TABLE dq_tx (id int PRIMARY KEY)", 10, &mut go).await.unwrap();
    s.set_autocommit(false).await.unwrap();
    s.execute("SELECT 1", 10, &mut go).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Idle));
    s.execute("INSERT INTO dq_tx VALUES (1)", 10, &mut go).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Open));
    assert!(s.execute("INSERT INTO dq_tx VALUES (1)", 10, &mut go).await.is_err());
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Failed));
    s.rollback().await.unwrap();
    s.execute("INSERT INTO dq_tx VALUES (2)", 10, &mut go).await.unwrap();
    let mut out = QueryOutcome::default();
    other.execute("SELECT count(*) FROM dq_tx", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!(0));
    s.commit().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    let mut out = QueryOutcome::default();
    other.execute("SELECT id FROM dq_tx", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, vec![vec![json!(2)]]);
    s.execute("DROP TABLE dq_tx", 10, &mut go).await.unwrap();
}
