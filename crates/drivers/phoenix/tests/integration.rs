//! Against a real Phoenix Query Server:
//!
//! ```sh
//! docker run -d --name dbine-test-phoenix -p 25165:8765 boostport/hbase-phoenix-all-in-one:2.0-5.0
//! DBINE_TEST_PHOENIX_URL=http://localhost:25165 cargo test -p dbine-driver-phoenix -- --ignored
//! ```
//! `DBINE_TEST_PHOENIX_SERIALIZATION=json` for a server set to JSON.

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{kinds, ConnectionConfig, DdlParts, Error, ObjectRef, QueryOutcome, Session, TableSchema};

fn cfg(read_only: bool) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_PHOENIX_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "phoenix".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        read_only,
        ..Default::default()
    };
    if let Ok(s) = std::env::var("DBINE_TEST_PHOENIX_SERIALIZATION") {
        c.options.insert("serialization".into(), s);
    }
    Some(c)
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some("DBINE".into()), name: name.into() }
}

#[tokio::test]
#[ignore]
async fn phoenix() {
    let Some(c) = cfg(false) else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let v = s.server_version().await.unwrap();
    assert!(v.starts_with("Apache Phoenix"), "{v}");

    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE SCHEMA IF NOT EXISTS DBINE; DROP VIEW IF EXISTS DBINE.V; DROP TABLE IF EXISTS DBINE.T; DROP SEQUENCE IF EXISTS DBINE.SEQ;
         CREATE TABLE DBINE.T (ID BIGINT NOT NULL PRIMARY KEY, NAME VARCHAR(20), D DECIMAL(10,2), TS TIMESTAMP, DT DATE);
         CREATE VIEW DBINE.V AS SELECT * FROM DBINE.T WHERE ID > 5;
         CREATE SEQUENCE DBINE.SEQ START WITH 10 INCREMENT BY 2;",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let mut out = QueryOutcome::default();
    for i in 1..=12 {
        s.execute(
            &format!("UPSERT INTO DBINE.T VALUES ({i}, 'n{i}', {i}.25, TO_TIMESTAMP('2024-01-31 13:45:00'), TO_DATE('2024-01-31', 'yyyy-MM-dd'))"),
            10,
            &mut out,
        )
        .await
        .unwrap();
    }
    assert_eq!(out.results[0].rows_affected, Some(1));

    let objs = s.list_objects().await.unwrap();
    let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n && o.schema.as_deref() == Some("DBINE"));
    assert!(has(kinds::TABLE, "T") && has(kinds::VIEW, "V") && has(kinds::SEQUENCE, "SEQ"), "{objs:?}");
    assert!(!objs.iter().any(|o| o.schema.as_deref() == Some("SYSTEM")));

    let cols = s.columns(&obj(kinds::TABLE, "T")).await.unwrap();
    assert_eq!(cols.len(), 5, "{cols:?}");
    assert!(cols[0].primary_key && !cols[0].nullable && cols[1].nullable, "{cols:?}");
    assert_eq!(cols[1].data_type, "VARCHAR(20)");
    assert!(s.definition(&obj(kinds::VIEW, "V")).await.unwrap().unwrap().contains("ID > 5"));
    assert!(s.definition(&obj(kinds::SEQUENCE, "SEQ")).await.unwrap().unwrap().contains("START WITH 10"));

    let q = s.browse_query(&obj(kinds::TABLE, "T"), 10);
    let mut out = QueryOutcome::default();
    s.execute(&q, 4, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!((r.rows.len(), r.total_rows, r.truncated), (4, 10, true));
    assert_eq!(r.rows[0][0], serde_json::json!(1));
    assert_eq!(r.rows[0][2], serde_json::json!("1.25"));
    assert!(r.rows[0][3].as_str().unwrap().starts_with("2024-01-31 13:45:00"), "{:?}", r.rows[0]);
    assert!(r.rows[0][4].as_str().unwrap().starts_with("2024-01-31"), "{:?}", r.rows[0]);

    // More rows than one frame (500).
    let mut out = QueryOutcome::default();
    s.execute("SELECT a.ID FROM DBINE.T a, DBINE.T b, DBINE.T c, DBINE.T d", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].total_rows, 12 * 12 * 12 * 12);

    // Error mid-script.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1 FROM DBINE.T LIMIT 1; SELECT * FROM NOPE; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // Read-only: the connection is read-only on the server too.
    let mut ro = d.connect(&cfg(true).unwrap(), None).await.unwrap();
    let mut out = QueryOutcome::default();
    let e = ro.execute("UPSERT INTO DBINE.T (ID) VALUES (99)", 10, &mut out).await;
    assert!(e.is_err(), "{e:?}");
    ro.execute("SELECT COUNT(*) FROM DBINE.T", 10, &mut out).await.unwrap();
    let mut wrapped = ReadOnlySession::new(d.connect(&c, None).await.unwrap());
    assert!(wrapped.execute("DROP TABLE DBINE.T", 10, &mut out).await.is_err());

    let mut out = QueryOutcome::default();
    s.execute("DROP VIEW DBINE.V; DROP TABLE DBINE.T; DROP SEQUENCE DBINE.SEQ", 10, &mut out).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn plans() {
    let Some(c) = cfg(false) else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    assert!(d.supports_explain());
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE SCHEMA IF NOT EXISTS DBINE; CREATE TABLE IF NOT EXISTS DBINE.PLAN_T (ID INTEGER PRIMARY KEY, X INTEGER, Y VARCHAR); \
         UPSERT INTO DBINE.PLAN_T VALUES (1, 1, 'a'); UPSERT INTO DBINE.PLAN_T VALUES (2, 1, 'b')",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let q = "SELECT Y, COUNT(*) FROM DBINE.PLAN_T WHERE X = 1 GROUP BY Y";
    let mut out = QueryOutcome::default();
    s.explain(&format!("{q}; UPSERT INTO DBINE.PLAN_T VALUES (3, 3, 'c')"), false, 10, &mut out).await.unwrap();
    assert!(out.results.is_empty());
    assert_eq!(out.plans.len(), 2);
    println!("{}\n{:#?}", out.plans[0].raw, out.plans[0].root);
    let scan = {
        let mut n = &out.plans[0].root;
        while let Some(c) = n.children.first() {
            n = c;
        }
        n
    };
    assert_eq!(scan.object.as_deref(), Some("DBINE.PLAN_T"));
    let mut out = QueryOutcome::default();
    s.execute("SELECT COUNT(*) FROM DBINE.PLAN_T", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(2), "the UPSERT didn't run");

    let mut out = QueryOutcome::default();
    s.explain(q, true, 10, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 1);
    assert_eq!(out.plans.len(), 1);
    s.execute("DROP TABLE DBINE.PLAN_T", 10, &mut QueryOutcome::default()).await.unwrap();
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{e:?}\n{sql}"));
}

async fn schema_of(s: &mut Box<dyn Session>, schema: &str) -> Vec<TableSchema> {
    s.database_schema().await.unwrap().into_iter().filter(|t| t.schema.as_deref() == Some(schema)).collect()
}

/// Slow on the all-in-one image (about a minute: the salted table's regions
/// open one by one). Stopping the container while it runs can leave the
/// HBase table `DBINE_DDL:CLIENTES` stuck in ENABLING, and every later run
/// then fails with ERROR 1102 (XCL02) "Cannot get all table regions". To
/// recover, in `hbase shell`: `put 'hbase:meta', 'DBINE_DDL:CLIENTES',
/// 'table:state', "\x08\x01"` (DISABLED) and `drop 'DBINE_DDL:CLIENTES'`.
#[tokio::test]
#[ignore]
async fn schema_and_ddl() {
    let Some(c) = cfg(false) else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    for sc in ["DBINE_DDL", "DBINE_DDL2"] {
        run(&mut s, &format!("CREATE SCHEMA IF NOT EXISTS {sc}; DROP TABLE IF EXISTS {sc}.ITEMS; DROP TABLE IF EXISTS {sc}.PEDIDOS; DROP TABLE IF EXISTS {sc}.CLIENTES")).await;
    }
    run(
        &mut s,
        "CREATE TABLE DBINE_DDL.CLIENTES (ID BIGINT NOT NULL, NOMBRE VARCHAR(50), CF1.EMAIL VARCHAR, SALDO DECIMAL(10,2) DEFAULT 0,
            CONSTRAINT PK_CLI PRIMARY KEY (ID)) SALT_BUCKETS=2;
         CREATE TABLE DBINE_DDL.PEDIDOS (CLIENTE_ID BIGINT NOT NULL, PEDIDO_ID INTEGER NOT NULL, ESTADO VARCHAR(20) NOT NULL,
            TOTAL DECIMAL(12,2), CONSTRAINT PK_PED PRIMARY KEY (CLIENTE_ID, PEDIDO_ID)) IMMUTABLE_ROWS=true;
         CREATE TABLE DBINE_DDL.ITEMS (ID INTEGER NOT NULL PRIMARY KEY, TAGS VARCHAR ARRAY, CANT UNSIGNED_INT, ALTA TIMESTAMP);
         CREATE INDEX IX_NOMBRE ON DBINE_DDL.CLIENTES (NOMBRE) INCLUDE (SALDO);
         CREATE LOCAL INDEX IX_ESTADO ON DBINE_DDL.PEDIDOS (ESTADO, TOTAL);
         CREATE INDEX IX_PED ON DBINE_DDL.PEDIDOS (PEDIDO_ID);",
    )
    .await;

    let src = schema_of(&mut s, "DBINE_DDL").await;
    assert_eq!(src.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["CLIENTES", "ITEMS", "PEDIDOS"], "{src:#?}");
    let cli = &src[0];
    assert_eq!(cli.primary_key.as_ref().unwrap().name.as_deref(), Some("PK_CLI"));
    assert_eq!(cli.options.get("SALT_BUCKETS").map(String::as_str), Some("2"));
    let email = cli.columns.iter().find(|c| c.name == "EMAIL").unwrap();
    assert_eq!(email.options.get("family").map(String::as_str), Some("CF1"));
    assert_eq!(cli.columns[3].default_value.as_deref(), Some("0"), "{:?}", cli.columns);
    assert_eq!(cli.indexes.len(), 1);
    assert_eq!((cli.indexes[0].columns.clone(), cli.indexes[0].kind.as_deref()), (vec!["NOMBRE".to_string()], Some("global")));
    let ped = &src[2];
    assert_eq!(ped.primary_key.as_ref().unwrap().columns, ["CLIENTE_ID", "PEDIDO_ID"]);
    assert_eq!(ped.options.get("IMMUTABLE_ROWS").map(String::as_str), Some("true"));
    assert!(!ped.columns[2].nullable && ped.columns[3].data_type == "DECIMAL(12, 2)", "{:?}", ped.columns);
    let ix: Vec<_> = ped.indexes.iter().map(|i| (i.name.as_str(), i.columns.clone(), i.kind.as_deref().unwrap())).collect();
    assert_eq!(
        ix,
        [("IX_ESTADO", vec!["ESTADO".to_string(), "TOTAL".to_string()], "local"), ("IX_PED", vec!["PEDIDO_ID".to_string()], "global")]
    );
    assert_eq!(src[1].columns[1].data_type, "VARCHAR ARRAY");
    assert!(src.iter().all(|t| t.foreign_keys.is_empty()));

    // Round trip into another schema: every table first, then the indexes.
    let moved: Vec<_> = src.iter().map(|t| TableSchema { schema: Some("DBINE_DDL2".into()), ..t.clone() }).collect();
    let mut script = Vec::new();
    for t in &moved {
        script.push(d.table_ddl(t, DdlParts { create: true, if_exists: true, ..Default::default() }).unwrap());
    }
    for t in &moved {
        script.push(d.table_ddl(t, DdlParts { indexes: true, foreign_keys: true, ..Default::default() }).unwrap());
    }
    let script = script.join("\n");
    println!("{script}");
    run(&mut s, &script).await;
    assert_eq!(schema_of(&mut s, "DBINE_DDL2").await, moved);

    // Inserts.
    let t = ObjectRef { kind: kinds::TABLE.into(), schema: Some("DBINE_DDL2".into()), name: "CLIENTES".into() };
    let ins = d
        .insert_script(
            &t,
            &["ID".into(), "NOMBRE".into(), "EMAIL".into(), "SALDO".into()],
            &[vec![1.into(), "O'Brien".into(), serde_json::Value::Null, 1.5.into()], vec![2.into(), "b".into(), "b@x".into(), 0.into()]],
        )
        .unwrap();
    assert!(ins.starts_with("UPSERT INTO"), "{ins}");
    run(&mut s, &ins).await;
    let mut out = QueryOutcome::default();
    s.execute("SELECT NOMBRE FROM DBINE_DDL2.CLIENTES WHERE ID = 1", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!("O'Brien"));

    // Drop with the generated DDL; databases aren't created from DBine.
    for t in &moved {
        run(&mut s, &d.table_ddl(t, DdlParts { drop: true, if_exists: true, ..Default::default() }).unwrap()).await;
    }
    assert!(schema_of(&mut s, "DBINE_DDL2").await.is_empty());
    assert!(!d.capabilities().create_database);
    assert!(matches!(s.create_database("X").await, Err(Error::Unsupported(_))));
    run(&mut s, "DROP TABLE DBINE_DDL.ITEMS; DROP TABLE DBINE_DDL.PEDIDOS; DROP TABLE DBINE_DDL.CLIENTES").await;
}

/// With the HBase UIs published too:
/// `-p 25166:16010 -p 25167:16030`, then
/// `DBINE_TEST_PHOENIX_HBASE_MASTER=http://localhost:25166` and
/// `DBINE_TEST_PHOENIX_HBASE_RS=http://localhost:25167`.
#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(mut c) = cfg(false) else { return };
    let hbase = std::env::var("DBINE_TEST_PHOENIX_HBASE_MASTER").ok();
    if let Some(m) = &hbase {
        c.options.insert("hbase_master".into(), m.clone());
    }
    if let Ok(rs) = std::env::var("DBINE_TEST_PHOENIX_HBASE_RS") {
        c.options.insert("hbase_regionservers".into(), rs);
    }
    let d = dbine_driver_phoenix::drivers().remove(0);
    assert!(d.capabilities().monitor);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE IF NOT EXISTS DBINE_MON (ID BIGINT NOT NULL PRIMARY KEY, V VARCHAR);
         UPSERT INTO DBINE_MON VALUES (1, 'a'); UPSERT INTO DBINE_MON VALUES (2, 'b');
         UPDATE STATISTICS DBINE_MON",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let snap = s.monitor().await.unwrap();
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    eprintln!(
        "{:?}\ninfo {:?}\nnotes {:?}\ntables {:?}",
        snap.metrics.iter().map(|m| (m.key.as_str(), m.value, m.max)).collect::<Vec<_>>(),
        snap.info,
        snap.notes,
        snap.tables.iter().map(|t| (t.key.as_str(), t.rows.len())).collect::<Vec<_>>()
    );
    assert!(v("tables").unwrap() >= 1.0);
    if hbase.is_some() {
        assert!(snap.tables.iter().any(|t| t.key == "top_objects" && t.rows.iter().any(|r| r[0] == "DBINE_MON")));
        assert!(v("cpu").is_some() && v("mem_used").is_some() && v("regions").unwrap() >= 1.0);
        assert!(v("queries").unwrap() > 0.0 && v("region_servers") == Some(1.0) && v("uptime").is_some());
        assert!(snap.tables.iter().any(|t| t.key == "nodes" && !t.rows.is_empty()));
    }
}

/// The generic "Apache Calcite Avatica" preset against the Query Server
/// (an Avatica server): catalog through Avatica's metadata calls.
#[tokio::test]
#[ignore]
async fn avatica_generic() {
    let Some(mut c) = cfg(false) else { return };
    c.driver = "avatica".into();
    // The Query Server speaks protobuf unless configured otherwise.
    let ser = c.options.get("serialization").cloned().unwrap_or_else(|| "protobuf".into());
    c.options.insert("serialization".into(), ser);
    let d = dbine_driver_phoenix::drivers().into_iter().find(|d| d.info().id == "avatica").unwrap();
    assert!(!d.capabilities().monitor && d.designer().is_none());
    let mut s = d.connect(&c, None).await.unwrap();
    let v = s.server_version().await.unwrap();
    assert!(v.starts_with("Avatica: Phoenix"), "{v}");
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE IF NOT EXISTS DBINE_AV (ID BIGINT NOT NULL PRIMARY KEY, NAME VARCHAR(20), D DECIMAL(10,2));
         CREATE VIEW IF NOT EXISTS DBINE_AV_V AS SELECT * FROM DBINE_AV;
         UPSERT INTO DBINE_AV VALUES (1, 'a', 1.5)",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.name == "DBINE_AV" && o.kind == kinds::TABLE), "{objs:?}");
    assert!(objs.iter().any(|o| o.name == "DBINE_AV_V" && o.kind == kinds::VIEW));
    assert!(!objs.iter().any(|o| o.schema.as_deref() == Some("SYSTEM")), "system tables are left out");
    let t = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "DBINE_AV".into() };
    let cols = s.columns(&t).await.unwrap();
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["ID", "NAME", "D"]);
    assert_eq!(cols[1].data_type, "VARCHAR(20)");
    assert_eq!(cols[2].data_type, "DECIMAL(10,2)");
    assert!(!cols[0].nullable && cols[1].nullable);
    let schema = s.database_schema().await.unwrap();
    assert!(schema.iter().any(|t| t.name == "DBINE_AV" && t.columns.len() == 3));
    let mut out = QueryOutcome::default();
    s.execute(&s.browse_query(&t, 10), 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][1], serde_json::json!("a"));
    assert!(matches!(s.monitor().await, Err(Error::Unsupported(_))));
    let mut out = QueryOutcome::default();
    s.execute("DROP VIEW DBINE_AV_V; DROP TABLE DBINE_AV", 10, &mut out).await.unwrap();
}

/// Schema sync: columns added (in a column family, with a default) and
/// dropped, an index made again on another column, a table dropped and one
/// created; syncing again from what Phoenix now has gives nothing.
#[tokio::test]
#[ignore]
async fn schema_sync() {
    use dbine_driver::{ColumnDef, IndexDef, KeyDef, TableChange};
    let Some(c) = cfg(false) else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    assert!(d.supports_schema_sync());
    let mut s = d.connect(&c, None).await.unwrap();
    run(
        &mut s,
        "CREATE SCHEMA IF NOT EXISTS DBINE_SYNC; DROP TABLE IF EXISTS DBINE_SYNC.T; DROP TABLE IF EXISTS DBINE_SYNC.VIEJA; DROP TABLE IF EXISTS DBINE_SYNC.NUEVA",
    )
    .await;
    run(
        &mut s,
        "CREATE TABLE DBINE_SYNC.T (ID BIGINT NOT NULL, NOMBRE VARCHAR(10), BAJA DATE, CONSTRAINT PK_T PRIMARY KEY (ID));
         CREATE INDEX IX_NOMBRE ON DBINE_SYNC.T (NOMBRE);
         CREATE INDEX IX_BAJA ON DBINE_SYNC.T (BAJA);
         CREATE TABLE DBINE_SYNC.VIEJA (ID INTEGER NOT NULL PRIMARY KEY);
         UPSERT INTO DBINE_SYNC.T (ID, NOMBRE, BAJA) VALUES (1, 'uno', TO_DATE('2024-01-01'));",
    )
    .await;
    let all = schema_of(&mut s, "DBINE_SYNC").await;
    let find = |n: &str| all.iter().find(|t| t.name == n).cloned().unwrap();
    let old = find("T");
    let mut new = old.clone();
    new.columns.retain(|c| c.name != "BAJA");
    let mut email = ColumnDef { name: "EMAIL".into(), data_type: "VARCHAR".into(), nullable: true, default_value: Some("'-'".into()), ..Default::default() };
    email.options.insert("family".into(), "CF1".into());
    new.columns.push(email);
    new.indexes = vec![IndexDef { name: "IX_NOMBRE".into(), columns: vec!["EMAIL".into()], unique: false, kind: Some("global".into()), filter: None, ..Default::default() }];
    let nueva = dbine_driver::TableSchema {
        kind: "table".into(),
        schema: Some("DBINE_SYNC".into()),
        name: "NUEVA".into(),
        columns: vec![ColumnDef { name: "ID".into(), data_type: "INTEGER".into(), nullable: false, ..Default::default() }],
        primary_key: Some(KeyDef { name: Some("PK_N".into()), columns: vec!["ID".into()] }),
        ..Default::default()
    };
    let script = d.sync_script(&[TableChange::Alter { old, new: new.clone() }, TableChange::Drop { table: find("VIEJA") }, TableChange::Create { table: nueva }]).unwrap();
    println!("{script:#?}");
    for stmt in &script.statements {
        run(&mut s, stmt).await;
    }
    let all = schema_of(&mut s, "DBINE_SYNC").await;
    assert!(all.iter().all(|t| t.name != "VIEJA") && all.iter().any(|t| t.name == "NUEVA"));
    let now = all.into_iter().find(|t| t.name == "T").unwrap();
    let again = d.sync_script(&[TableChange::Alter { old: now.clone(), new }]).unwrap();
    assert!(again.statements.is_empty(), "{:?}\n{now:#?}", again.statements);
    run(&mut s, "UPSERT INTO DBINE_SYNC.T (ID) VALUES (2)").await;
    run(&mut s, "DROP TABLE DBINE_SYNC.T; DROP TABLE DBINE_SYNC.NUEVA").await;
}

/// "Nuevo esquema…" / "Borrar esquema…": the schema shows in
/// `SYSTEM.CATALOG`, a schema with a table can't be dropped (no CASCADE),
/// and a grant is refused unless the server has HBase ACLs on.
#[tokio::test]
#[ignore]
async fn create_and_drop_schema() {
    let Some(c) = cfg(false) else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let spec = d.schema_spec().unwrap();
    assert!(!spec.owner && !spec.cascade && spec.privileges.contains(&"R"));
    assert!(dbine_driver_phoenix::drivers().remove(1).schema_spec().is_none());
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE IF EXISTS \"DBINE_SC\".T", 10, &mut out).await;
    let _ = s.execute(&d.drop_schema_script(None, "DBINE_SC", false).unwrap(), 10, &mut out).await;

    let mut out = QueryOutcome::default();
    s.execute(&d.create_schema_script(None, "DBINE_SC", None).unwrap(), 10, &mut out).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT TABLE_SCHEM FROM SYSTEM.CATALOG WHERE TABLE_SCHEM = 'DBINE_SC' AND TENANT_ID IS NULL", 10, &mut out).await.unwrap();
    assert!(!out.results[0].rows.is_empty(), "the schema isn't in SYSTEM.CATALOG");
    // Empty, it's listed (the tree shows it and can drop it).
    let listed = s.list_schemas().await.unwrap().expect("Phoenix lists schemas");
    assert!(listed.iter().any(|x| x.name == "DBINE_SC" && !x.system), "{listed:?}");
    assert!(listed.iter().any(|x| x.name == "SYSTEM" && x.system), "{listed:?}");
    // Names HBase would refuse (slowly) are refused before running.
    assert!(d.create_schema_script(None, "Mi Esquema", None).is_err() && d.create_schema_script(None, "q\"x", None).is_err());

    // What "Nuevo esquema…" writes for a grant (the default: `security_script` on the schema).
    let grant = d.schema_grant_script(Some("ignored"), "DBINE_SC", &["R".into(), "W".into()], "dbine_ana", false).unwrap();
    assert_eq!(grant, "GRANT 'RW' ON SCHEMA \"DBINE_SC\" TO 'dbine_ana'");
    assert!(d.schema_owner_script(None, "DBINE_SC", "ana").unwrap().is_none());
    let mut out = QueryOutcome::default();
    match s.execute(&grant, 10, &mut out).await {
        Ok(()) => eprintln!("the server has ACLs: {grant} ran"),
        Err(e) => eprintln!("no HBase ACLs on this server, {grant}: {e}"),
    }

    let mut out = QueryOutcome::default();
    s.execute("CREATE TABLE \"DBINE_SC\".T (ID BIGINT NOT NULL PRIMARY KEY)", 10, &mut out).await.unwrap();
    let mut out = QueryOutcome::default();
    assert!(s.execute(&d.drop_schema_script(None, "DBINE_SC", false).unwrap(), 10, &mut out).await.is_err(), "dropped a schema with a table");
    let mut out = QueryOutcome::default();
    s.execute("DROP TABLE \"DBINE_SC\".T", 10, &mut out).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&d.drop_schema_script(None, "DBINE_SC", false).unwrap(), 10, &mut out).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT TABLE_SCHEM FROM SYSTEM.CATALOG WHERE TABLE_SCHEM = 'DBINE_SC' AND TENANT_ID IS NULL", 10, &mut out).await.unwrap();
    assert!(out.results[0].rows.is_empty(), "the schema is still there");
    assert!(!s.list_schemas().await.unwrap().unwrap().iter().any(|x| x.name == "DBINE_SC"));
}

/// The editor's script contract: Phoenix's code and SQLSTATE with the
/// position in the script, manual transactions (UPSERTs kept on the server
/// connection until COMMIT).
#[tokio::test]
#[ignore]
async fn script_errors_and_transactions() {
    let Some(c) = cfg(false) else { return };
    let d = dbine_driver_phoenix::drivers().remove(0);
    assert_eq!(d.script_mode(), dbine_driver::sql::ScriptMode::PerStatement);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut other = d.connect(&c, None).await.unwrap();
    let script = "SELECT 1 FROM SYSTEM.CATALOG LIMIT 1;\nSELECT 2 FROM\n  SYSTEM.CATALOG WHERE\n  nope = 1;";
    let mut out = QueryOutcome::default();
    let e = s.execute(script, 10, &mut out).await.unwrap_err().to_script_error();
    assert_eq!((e.code.as_deref(), e.sqlstate.as_deref()), (Some("504"), Some("42703")), "{e:?}");
    assert_eq!(out.results.len(), 1);
    let e = s.execute("SELECT 1;\nSELECT 2 FROM SYSTEM.CATALOG\n  WHER x", 10, &mut QueryOutcome::default()).await.unwrap_err().to_script_error();
    eprintln!("{e:?}");
    assert_eq!(e.code.as_deref(), Some("603"), "{e:?}");
    assert_eq!(e.line, Some(3));
    assert_eq!(e.offset, Some("SELECT 1;\nSELECT 2 FROM SYSTEM.CATALOG\n  WHER ".len()));

    let run = |sql: &'static str| sql;
    let mut go = QueryOutcome::default();
    let _ = s.execute("DROP TABLE IF EXISTS DBINE.TX", 10, &mut go).await;
    s.execute(run("CREATE TABLE DBINE.TX (ID INTEGER PRIMARY KEY)"), 10, &mut go).await.unwrap();
    s.set_autocommit(false).await.unwrap();
    s.execute("UPSERT INTO DBINE.TX VALUES (1)", 10, &mut go).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Open));
    s.rollback().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Idle));
    s.execute("UPSERT INTO DBINE.TX VALUES (2)", 10, &mut go).await.unwrap();
    let mut out = QueryOutcome::default();
    other.execute("SELECT COUNT(*) FROM DBINE.TX", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(0), "not visible before the commit");
    s.commit().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    let mut out = QueryOutcome::default();
    other.execute("SELECT ID FROM DBINE.TX", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, vec![vec![serde_json::json!(2)]]);
    s.execute("DROP TABLE DBINE.TX", 10, &mut go).await.unwrap();
}
