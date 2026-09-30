//! "Clonar tabla" on Oracle, against a live server: character length
//! semantics, identity options, a table named with its schema, names taken
//! by objects the explorer doesn't list, long constraint names, global
//! temporary tables, table compression and LOCAL index partition names.
//!
//! Ignored by default:
//!
//! ```sh
//! DBINE_TEST_ORACLE_URL=oracle://dbine:Dbine123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-transfer --test clone_table_oracle -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use dbine_transfer::clone_table::{clone_table, CloneControl, CloneOptions, CloneReport, CloneRequest, ConfigEndpoints};
use dbine_transfer::Endpoints;
use serde_json::Value;
use std::sync::Arc;

const SRC: &str = "CLON_ORA_SRC";

fn endpoints() -> Arc<ConfigEndpoints> {
    let url = std::env::var("DBINE_TEST_ORACLE_URL").expect("DBINE_TEST_ORACLE_URL");
    let rest = url.strip_prefix("oracle://").expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut config = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    config.options.insert("service".into(), service.into());
    let driver = dbine_drivers::find("oracle").expect("oracle driver").clone();
    Arc::new(ConfigEndpoints { driver, config, database: None })
}

async fn query(s: &mut dyn Session, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.map_err(|e| e.to_string())?;
    if let Some(e) = out.error {
        return Err(e);
    }
    Ok(out.results.into_iter().rev().find(|r| !r.columns.is_empty()).map(|r| r.rows).unwrap_or_default())
}

async fn run(s: &mut dyn Session, sql: &str) {
    query(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn text(s: &mut dyn Session, sql: &str) -> String {
    let rows = query(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    match rows.first().and_then(|r| r.first()) {
        Some(Value::String(v)) => v.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

fn drop_sql(name: &str) -> String {
    format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE \"{name}\" CASCADE CONSTRAINTS PURGE'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;\n/")
}

async fn clone(schema: Option<&str>, new_name: &str, with_data: bool) -> Result<CloneReport, String> {
    let req = CloneRequest {
        source: ObjectRef { kind: "table".into(), schema: schema.map(Into::into), name: SRC.into() },
        new_name: new_name.into(),
        options: CloneOptions { with_data, with_indexes: true },
    };
    clone_table(endpoints(), req, &CloneControl::default(), |e| println!("{e:?}")).await.map_err(|e| e.to_string())
}

async fn identity(s: &mut dyn Session, table: &str) -> (String, String) {
    let rows = query(
        s,
        &format!("SELECT generation_type, identity_options FROM user_tab_identity_cols WHERE table_name = '{table}' AND column_name = 'Id'"),
    )
    .await
    .unwrap();
    let r = &rows[0];
    (r[0].as_str().unwrap().to_string(), r[1].as_str().unwrap().to_string())
}

/// `START WITH: n, ` left out: where each identity is now.
fn options_but_start(o: &str) -> String {
    o.split(", ").filter(|p| !p.starts_with("START WITH")).collect::<Vec<_>>().join(", ")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs DBINE_TEST_ORACLE_URL (dbine-test-oracle)"]
async fn oracle_clones_exactly() {
    let e = endpoints();
    let mut w = e.open_target().await.unwrap();
    let names = ["CLON_ORA_A", "CLON_ORA_B", "CLON_ORA_C", "Clon Ñandú mixto"];
    for n in [SRC].iter().chain(names.iter()) {
        run(&mut *w, &drop_sql(n)).await;
    }
    run(
        &mut *w,
        &format!(
            "CREATE TABLE {SRC} (
               \"Id\" NUMBER GENERATED ALWAYS AS IDENTITY (START WITH 100 INCREMENT BY 5 CACHE 10) PRIMARY KEY,
               NOMBRE VARCHAR2(10 CHAR) NOT NULL,
               CODIGO VARCHAR2(20) CONSTRAINT UQ_CLON_ORA_SRC_CODIGO UNIQUE,
               INICIAL CHAR(2 CHAR),
               NACIONAL NVARCHAR2(10),
               MONTO NUMBER(10,2) CONSTRAINT CK_CLON_ORA_SRC_MONTO CHECK (MONTO >= 0),
               DOBLE NUMBER GENERATED ALWAYS AS (MONTO * 2) VIRTUAL,
               CREADO TIMESTAMP DEFAULT SYSTIMESTAMP
             )"
        ),
    )
    .await;
    run(&mut *w, &format!("CREATE INDEX IX_CLON_ORA_SRC_CREADO ON {SRC} (CREADO)")).await;
    // 10 characters, 20 bytes: only fits in VARCHAR2(10 CHAR).
    run(&mut *w, &format!("INSERT INTO {SRC} (NOMBRE, CODIGO, INICIAL, NACIONAL, MONTO) VALUES ('ññññññññññ', 'A-1', 'ñá', 'ñññ', 10.5)")).await;
    run(&mut *w, &format!("INSERT INTO {SRC} (NOMBRE, CODIGO, INICIAL, NACIONAL, MONTO) VALUES ('Pérez', 'A-2', 'ab', NULL, 3)")).await;
    run(&mut *w, "COMMIT").await;
    let original_cols = w.columns(&ObjectRef { kind: "table".into(), schema: None, name: SRC.into() }).await.unwrap();
    assert_eq!(original_cols[1].data_type, "VARCHAR2(10 CHAR)");
    assert_eq!(original_cols[3].data_type, "CHAR(2 CHAR)");
    assert_eq!(original_cols[4].data_type, "NVARCHAR2(10)");
    let current = text(&mut *w, "SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual").await;

    // 1 + 2 + 3: named with its schema, data, identity ALWAYS kept.
    let r = clone(Some(&current), "CLON_ORA_A", true).await.expect("clone A");
    println!("A: {:?} {:?}", r.notes, r.renames);
    assert_eq!(r.rows, 2);
    assert!(!r.notes.iter().any(|n| n.contains("solo informa las columnas")), "{:?}", r.notes);
    let a_ref = ObjectRef { kind: "table".into(), schema: None, name: "CLON_ORA_A".into() };
    let a_cols = w.columns(&a_ref).await.unwrap();
    assert_eq!(
        a_cols.iter().map(|c| (&c.name, &c.data_type, c.nullable)).collect::<Vec<_>>(),
        original_cols.iter().map(|c| (&c.name, &c.data_type, c.nullable)).collect::<Vec<_>>()
    );
    // Lengths in bytes too (DATA_LENGTH 40 = 10 characters of 4 bytes).
    let lengths = |t: &str| format!("SELECT column_name || ':' || char_used || ':' || data_length FROM user_tab_columns WHERE table_name = '{t}' AND data_type LIKE '%CHAR%' ORDER BY column_id");
    assert_eq!(query(&mut *w, &lengths("CLON_ORA_A")).await.unwrap(), query(&mut *w, &lengths(SRC)).await.unwrap());
    // Virtual column, check, unique and index came along.
    assert_eq!(text(&mut *w, "SELECT virtual_column FROM user_tab_cols WHERE table_name = 'CLON_ORA_A' AND column_name = 'DOBLE'").await, "YES");
    let named = |t: &str| format!("SELECT COUNT(*) FROM user_constraints WHERE table_name = '{t}' AND constraint_type IN ('C', 'U', 'P') AND generated = 'USER NAME'");
    assert_eq!(text(&mut *w, &named("CLON_ORA_A")).await, "2");
    assert_eq!(text(&mut *w, "SELECT COUNT(*) FROM user_constraints WHERE table_name = 'CLON_ORA_A' AND constraint_type = 'P'").await, "1");
    assert_eq!(text(&mut *w, "SELECT COUNT(*) FROM user_indexes WHERE table_name = 'CLON_ORA_A' AND index_name LIKE 'IX_%'").await, "1");
    // 6: made-up names unshortened when the server takes 128 bytes.
    if text(&mut *w, "SELECT value FROM v$parameter WHERE name = 'compatible'").await.starts_with("23") {
        assert!(r.renames.iter().all(|x| !x.shortened), "{:?}", r.renames);
    }
    let (gen_src, opt_src) = identity(&mut *w, SRC).await;
    let (gen_a, opt_a) = identity(&mut *w, "CLON_ORA_A").await;
    assert_eq!((gen_a.as_str(), options_but_start(&opt_a)), (gen_src.as_str(), options_but_start(&opt_src)));
    assert_eq!(gen_a, "ALWAYS");
    // ALWAYS: an explicit Id is refused, a new row goes past the copied ones by 5.
    assert!(query(&mut *w, "INSERT INTO CLON_ORA_A (\"Id\", NOMBRE) VALUES (1, 'x')").await.is_err());
    run(&mut *w, "INSERT INTO CLON_ORA_A (NOMBRE, CODIGO) VALUES ('ÑÑÑÑÑÑÑÑÑÑ', 'A-3')").await;
    assert_eq!(text(&mut *w, "SELECT MAX(\"Id\") FROM CLON_ORA_A").await, "110");
    run(&mut *w, "ROLLBACK").await;

    // Empty clone: starts where the original's definition does.
    let r = clone(None, "CLON_ORA_B", false).await.expect("clone B");
    assert_eq!(r.rows, 0);
    let (gen_b, opt_b) = identity(&mut *w, "CLON_ORA_B").await;
    assert_eq!(gen_b, "ALWAYS");
    assert!(opt_b.starts_with("START WITH: 100, INCREMENT BY: 5"), "{opt_b}");

    // Quoted mixed-case name with ñ.
    clone(None, "Clon Ñandú mixto", true).await.expect("clone mixed case");

    // 5: a name taken by the identity's sequence (the explorer doesn't list it).
    let seq = text(&mut *w, &format!("SELECT sequence_name FROM user_tab_identity_cols WHERE table_name = '{SRC}'")).await;
    let err = clone(None, &seq, true).await.expect_err("taken by a sequence");
    assert!(err.contains("ya existe") && !err.contains("ORA-00955"), "{err}");
    assert_eq!(text(&mut *w, &format!("SELECT COUNT(*) FROM user_sequences WHERE sequence_name = '{seq}'")).await, "1");

    // 4: the limit is in bytes: 64 ñ = 128 bytes fits, 65 doesn't.
    let err = clone(None, &"ñ".repeat(65), false).await.expect_err("too long");
    assert!(err.contains("128 bytes") && err.contains("130"), "{err}");

    // 3 (refusal): another schema's table with the same name is not taken for this one.
    let err = clone(Some("NO_EXISTE_CLON"), "CLON_ORA_C", true).await.expect_err("other schema");
    println!("otro esquema: {err}");
    assert_eq!(text(&mut *w, "SELECT COUNT(*) FROM user_tables WHERE table_name = 'CLON_ORA_C'").await, "0");

    for n in [SRC].iter().chain(names.iter()) {
        run(&mut *w, &drop_sql(n)).await;
    }
}

/// A descending identity (negative ids) clones with its rows and goes on
/// downwards; a table with an INVISIBLE column is refused, naming it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs DBINE_TEST_ORACLE_URL (dbine-test-oracle)"]
async fn oracle_descending_identity_and_invisible_column() {
    let e = endpoints();
    let mut w = e.open_target().await.unwrap();
    let names = ["CLONV_NEG", "CLONV_NEG_C", "CLONV_NEG_E", "CLONV_INV", "CLONV_INV_C"];
    for n in names {
        run(&mut *w, &drop_sql(n)).await;
    }
    run(
        &mut *w,
        "CREATE TABLE CLONV_NEG (\"Id\" NUMBER GENERATED BY DEFAULT ON NULL AS IDENTITY \
         (START WITH -1 INCREMENT BY -1 MAXVALUE -1 MINVALUE -1000000 NOCACHE ORDER) PRIMARY KEY, NOMBRE VARCHAR2(20))",
    )
    .await;
    for n in ["a", "b", "c", "d"] {
        run(&mut *w, &format!("INSERT INTO CLONV_NEG (NOMBRE) VALUES ('{n}')")).await;
    }
    run(&mut *w, "COMMIT").await;
    assert_eq!(text(&mut *w, "SELECT MIN(\"Id\") FROM CLONV_NEG").await, "-4");
    let req = |name: &str, table: &str, with_data: bool| CloneRequest {
        source: ObjectRef { kind: "table".into(), schema: None, name: table.into() },
        new_name: name.into(),
        options: CloneOptions { with_data, with_indexes: true },
    };
    let r = clone_table(e.clone(), req("CLONV_NEG_C", "CLONV_NEG", true), &CloneControl::default(), |e| println!("{e:?}"))
        .await
        .expect("descending identity with data");
    assert_eq!(r.rows, 4);
    let (g1, o1) = identity(&mut *w, "CLONV_NEG").await;
    let (g2, o2) = identity(&mut *w, "CLONV_NEG_C").await;
    assert_eq!((g2, options_but_start(&o2)), (g1, options_but_start(&o1)));
    run(&mut *w, "INSERT INTO CLONV_NEG_C (NOMBRE) VALUES ('e')").await;
    assert_eq!(text(&mut *w, "SELECT MIN(\"Id\") FROM CLONV_NEG_C").await, "-5");
    run(&mut *w, "ROLLBACK").await;
    // Empty: starts where the original's definition does.
    clone_table(e.clone(), req("CLONV_NEG_E", "CLONV_NEG", false), &CloneControl::default(), |_| {}).await.expect("descending identity, no data");
    run(&mut *w, "INSERT INTO CLONV_NEG_E (NOMBRE) VALUES ('x')").await;
    assert_eq!(text(&mut *w, "SELECT MAX(\"Id\") FROM CLONV_NEG_E").await, "-1");
    run(&mut *w, "ROLLBACK").await;

    run(&mut *w, "CREATE TABLE CLONV_INV (ID NUMBER PRIMARY KEY, SECRETO VARCHAR2(20) INVISIBLE DEFAULT 'oculto', NOMBRE VARCHAR2(20))").await;
    let err = clone_table(e.clone(), req("CLONV_INV_C", "CLONV_INV", true), &CloneControl::default(), |_| {})
        .await
        .expect_err("invisible column")
        .to_string();
    assert!(err.contains("«SECRETO»") && err.contains("INVISIBLE"), "{err}");
    assert_eq!(text(&mut *w, "SELECT COUNT(*) FROM user_tables WHERE table_name = 'CLONV_INV_C'").await, "0");

    for n in names {
        run(&mut *w, &drop_sql(n)).await;
    }
}

/// Constraint and index names are schema-wide: the ones the clone's would
/// take that another table already has are avoided before anything is
/// written (renamed, with a note), not found out by the CREATE (ORA-02264)
/// or after the rows (ORA-00955). The CHECK comes along, renamed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs DBINE_TEST_ORACLE_URL (dbine-test-oracle)"]
async fn oracle_names_taken_by_other_tables_are_avoided() {
    let e = endpoints();
    let mut w = e.open_target().await.unwrap();
    let names = ["CLONV_COL_X", "CLONV_COL_Y", "CLONV_OTH", "CLONV_OTH2", "CLONV_COL"];
    for n in names {
        run(&mut *w, &drop_sql(n)).await;
    }
    run(&mut *w, "CREATE TABLE CLONV_COL (ID NUMBER CONSTRAINT PK_CLONV_COL PRIMARY KEY, V NUMBER CONSTRAINT CK_CLONV_COL_V CHECK (V >= 0))").await;
    run(&mut *w, "CREATE INDEX IX_CLONV_COL_V ON CLONV_COL (V)").await;
    run(&mut *w, "INSERT INTO CLONV_COL (ID, V) SELECT LEVEL, LEVEL FROM dual CONNECT BY LEVEL <= 50").await;
    run(&mut *w, "COMMIT").await;
    // Another table owns the clone's would-be PK, CHECK and index names.
    run(
        &mut *w,
        "CREATE TABLE CLONV_OTH (A NUMBER CONSTRAINT PK_CLONV_COL_X PRIMARY KEY, B NUMBER CONSTRAINT CK_CLONV_COL_X_V CHECK (B > 0))",
    )
    .await;
    run(&mut *w, "CREATE INDEX IX_CLONV_COL_X_V ON CLONV_OTH (B)").await;
    // An index (no constraint) with the name of the clone's primary key.
    run(&mut *w, "CREATE TABLE CLONV_OTH2 (A NUMBER, B NUMBER)").await;
    run(&mut *w, "CREATE INDEX PK_CLONV_COL_Y ON CLONV_OTH2 (A)").await;

    let req = |name: &str| CloneRequest {
        source: ObjectRef { kind: "table".into(), schema: None, name: "CLONV_COL".into() },
        new_name: name.into(),
        options: CloneOptions { with_data: true, with_indexes: true },
    };
    let r = clone_table(e.clone(), req("CLONV_COL_X"), &CloneControl::default(), |e| println!("{e:?}")).await.expect("clone X");
    println!("X: {:?} {:?}", r.notes, r.renames);
    assert_eq!(r.rows, 50);
    assert!(r.notes.iter().any(|n| n.contains("ya usaba otro objeto")), "{:?}", r.notes);
    for taken in ["PK_CLONV_COL_X", "CK_CLONV_COL_X_V", "IX_CLONV_COL_X_V"] {
        assert!(r.renames.iter().all(|x| x.to != taken), "{taken}: {:?}", r.renames);
    }
    assert_eq!(text(&mut *w, "SELECT COUNT(*) FROM user_constraints WHERE table_name = 'CLONV_COL_X' AND constraint_type = 'P'").await, "1");
    assert_eq!(
        text(&mut *w, "SELECT COUNT(*) FROM user_constraints WHERE table_name = 'CLONV_COL_X' AND constraint_type = 'C' AND generated = 'USER NAME'").await,
        "1"
    );
    assert_eq!(text(&mut *w, "SELECT COUNT(*) FROM user_indexes WHERE table_name = 'CLONV_COL_X' AND index_name LIKE 'IX_%'").await, "1");
    // The CHECK holds on the clone.
    assert!(query(&mut *w, "INSERT INTO CLONV_COL_X (ID, V) VALUES (1000, -1)").await.is_err());

    let r = clone_table(e.clone(), req("CLONV_COL_Y"), &CloneControl::default(), |_| {}).await.expect("clone Y");
    println!("Y: {:?} {:?}", r.notes, r.renames);
    assert!(r.renames.iter().all(|x| x.to != "PK_CLONV_COL_Y"), "{:?}", r.renames);
    assert_eq!(text(&mut *w, "SELECT COUNT(*) FROM user_constraints WHERE table_name = 'CLONV_COL_Y' AND constraint_type = 'P'").await, "1");

    for n in names {
        run(&mut *w, &drop_sql(n)).await;
    }
}

/// What the reported structure doesn't carry about the storage: a global
/// temporary table stays one (same ON COMMIT, created empty: its rows are
/// each session's), the table's compression (a partitioned table's
/// default and a partition's own), and a LOCAL index's partition names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs DBINE_TEST_ORACLE_URL (dbine-test-oracle)"]
async fn oracle_temporary_compressed_and_local_partitions() {
    let e = endpoints();
    let mut w = e.open_target().await.unwrap();
    let names = ["CLONV_GTT", "CLONV_GTT_C", "CLONV_GTD", "CLONV_GTD_C", "CLONV_CMP", "CLONV_CMP_C", "CLONV_ADV", "CLONV_ADV_C", "CLONV_PTC", "CLONV_PTC_C"];
    for n in names {
        run(&mut *w, &drop_sql(n)).await;
    }
    let req = |name: &str, table: &str| CloneRequest {
        source: ObjectRef { kind: "table".into(), schema: None, name: table.into() },
        new_name: name.into(),
        options: CloneOptions { with_data: true, with_indexes: true },
    };
    let storage = |t: &str| {
        format!("SELECT temporary || ':' || duration || ':' || compression || ':' || compress_for FROM user_tables WHERE table_name = '{t}'")
    };

    // Global temporary, ON COMMIT PRESERVE ROWS (with rows in this session).
    run(&mut *w, "CREATE GLOBAL TEMPORARY TABLE CLONV_GTT (ID NUMBER PRIMARY KEY, V VARCHAR2(10)) ON COMMIT PRESERVE ROWS").await;
    run(&mut *w, "CREATE INDEX IX_CLONV_GTT_V ON CLONV_GTT (V)").await;
    run(&mut *w, "INSERT INTO CLONV_GTT VALUES (1, 'a')").await;
    run(&mut *w, "COMMIT").await;
    let r = clone_table(e.clone(), req("CLONV_GTT_C", "CLONV_GTT"), &CloneControl::default(), |e| println!("{e:?}")).await.expect("GTT");
    println!("GTT: {:?}", r.notes);
    assert_eq!(r.rows, 0);
    assert!(r.notes.iter().any(|n| n.contains("temporal global (ON COMMIT PRESERVE ROWS)")), "{:?}", r.notes);
    assert_eq!(text(&mut *w, &storage("CLONV_GTT_C")).await, text(&mut *w, &storage("CLONV_GTT")).await);
    assert!(text(&mut *w, &storage("CLONV_GTT_C")).await.starts_with("Y:SYS$SESSION"));
    assert_eq!(text(&mut *w, "SELECT COUNT(*) FROM user_indexes WHERE table_name = 'CLONV_GTT_C'").await, "2");
    // This session's rows go (the table can't be dropped while it has them).
    run(&mut *w, "TRUNCATE TABLE CLONV_GTT").await;
    // ON COMMIT DELETE ROWS.
    run(&mut *w, "CREATE GLOBAL TEMPORARY TABLE CLONV_GTD (ID NUMBER, V NUMBER CONSTRAINT CK_CLONV_GTD_V CHECK (V > 0)) ON COMMIT DELETE ROWS").await;
    clone_table(e.clone(), req("CLONV_GTD_C", "CLONV_GTD"), &CloneControl::default(), |_| {}).await.expect("GTT delete rows");
    assert!(text(&mut *w, &storage("CLONV_GTD_C")).await.starts_with("Y:SYS$TRANSACTION"));

    // Basic compression, and advanced.
    run(&mut *w, "CREATE TABLE CLONV_CMP (ID NUMBER CONSTRAINT PK_CLONV_CMP PRIMARY KEY, V VARCHAR2(20)) COMPRESS").await;
    run(&mut *w, "INSERT INTO CLONV_CMP SELECT LEVEL, 'v' || LEVEL FROM dual CONNECT BY LEVEL <= 20").await;
    run(&mut *w, "COMMIT").await;
    let r = clone_table(e.clone(), req("CLONV_CMP_C", "CLONV_CMP"), &CloneControl::default(), |_| {}).await.expect("COMPRESS");
    assert_eq!(r.rows, 20);
    assert_eq!(text(&mut *w, &storage("CLONV_CMP_C")).await, text(&mut *w, &storage("CLONV_CMP")).await);
    assert!(text(&mut *w, &storage("CLONV_CMP_C")).await.ends_with(":ENABLED:BASIC"));
    if query(&mut *w, "CREATE TABLE CLONV_ADV (ID NUMBER PRIMARY KEY) ROW STORE COMPRESS ADVANCED").await.is_ok() {
        clone_table(e.clone(), req("CLONV_ADV_C", "CLONV_ADV"), &CloneControl::default(), |_| {}).await.expect("COMPRESS ADVANCED");
        assert_eq!(text(&mut *w, &storage("CLONV_ADV_C")).await, text(&mut *w, &storage("CLONV_ADV")).await);
    }

    // Partitioned: compressed by default, one partition not; a LOCAL index
    // with partition names of its own.
    run(
        &mut *w,
        "CREATE TABLE CLONV_PTC (ID NUMBER CONSTRAINT PK_CLONV_PTC PRIMARY KEY, Y NUMBER) COMPRESS PARTITION BY RANGE (Y) \
         (PARTITION P2023 VALUES LESS THAN (2024), PARTITION VALUES LESS THAN (2025) NOCOMPRESS, PARTITION PMAX VALUES LESS THAN (MAXVALUE))",
    )
    .await;
    run(&mut *w, "CREATE INDEX IX_CLONV_PTC_L ON CLONV_PTC (Y) LOCAL (PARTITION LP1, PARTITION LP2, PARTITION LP3)").await;
    run(&mut *w, "INSERT INTO CLONV_PTC VALUES (1, 2023)").await;
    run(&mut *w, "INSERT INTO CLONV_PTC VALUES (2, 2024)").await;
    run(&mut *w, "COMMIT").await;
    let r = clone_table(e.clone(), req("CLONV_PTC_C", "CLONV_PTC"), &CloneControl::default(), |_| {}).await.expect("partitioned");
    assert_eq!(r.rows, 2);
    let parts = |t: &str| format!("SELECT LISTAGG(compression, ',') WITHIN GROUP (ORDER BY partition_position) FROM user_tab_partitions WHERE table_name = '{t}'");
    assert_eq!(text(&mut *w, &parts("CLONV_PTC_C")).await, "ENABLED,DISABLED,ENABLED");
    let def = |t: &str| format!("SELECT def_compression FROM user_part_tables WHERE table_name = '{t}'");
    assert_eq!(text(&mut *w, &def("CLONV_PTC_C")).await, text(&mut *w, &def("CLONV_PTC")).await);
    let ix = r.renames.iter().find(|x| x.from == "IX_CLONV_PTC_L").map(|x| x.to.clone()).expect("local index renamed");
    let ix_parts = |i: &str| format!("SELECT LISTAGG(partition_name, ',') WITHIN GROUP (ORDER BY partition_position) FROM user_ind_partitions WHERE index_name = '{i}'");
    assert_eq!(text(&mut *w, &ix_parts(&ix)).await, "LP1,LP2,LP3");

    for n in names {
        run(&mut *w, &drop_sql(n)).await;
    }
}
