//! Db2 LUW's vectors (`VECTOR(n, FLOAT32 | INT8)`, Db2 12.1.2 and later)
//! against a real server, through IBM's CLI / ODBC driver and the "db2"
//! preset: query results, typed edit scripts, the designer's DDL, the
//! describe and the schema, transfer reads and loads, and the vector
//! functions.
//!
//! Each test creates its own `DBINE_T_VEC_*` table(s) and drops them
//! before asserting anything, also when a step failed (leftovers of an
//! interrupted run are dropped first).
//!
//! One variable, with the connection fields as `key=value;…` (as in
//! `tests/jobs.rs`):
//!
//! ```sh
//! # Db2 LUW 12.1.2 or later (icr.io/db2_community/db2, amd64)
//! DBINE_TEST_ODBC_DB2='host=127.0.0.1;port=50000;database=testdb;user=db2inst1;password=…;odbc_driver=IBM DB2 ODBC DRIVER' \
//!   cargo test -p dbine-driver-odbc --test db2 -- --ignored --nocapture
//! ```
//!
//! The NULLs, defaults, comments, row counts and multi-row transfers
//! (`DBINE_T_LEN_*`) come back exactly also with a driver that writes
//! 32-bit lengths under a 64-bit driver manager: IBM's macOS clidriver
//! (`libdb2.dylib`) under unixODBC, which DBine detects at connect and
//! reads without row or parameter arrays. On Linux, IBM's `libdb2o.so`
//! has 64-bit lengths.

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::vector::{VectorElement, VectorMeta};
use dbine_driver::{
    async_trait, ColumnDef, ConnectionConfig, DdlParts, Driver, FunctionKind, KeyDef, ObjectRef,
    QueryOutcome, ResultColumn, RowChange, Session, StatementResult, TableChange, TableSchema,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

const SCHEMA: &str = "DB2INST1";

/// The connection from `DBINE_TEST_ODBC_DB2`, or `None` (the test is
/// skipped).
fn cfg() -> Option<ConnectionConfig> {
    let raw = std::env::var("DBINE_TEST_ODBC_DB2").ok()?;
    let mut c = ConnectionConfig {
        driver: "db2".into(),
        ..Default::default()
    };
    for part in raw.split(';') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k.trim() {
            "host" => c.host = v.into(),
            "port" => c.port = v.parse().unwrap_or(0),
            "database" => c.database = v.into(),
            "user" => c.username = Some(v.into()),
            "password" => c.password = Some(v.into()),
            key => {
                c.options.insert(key.into(), v.into());
            }
        }
    }
    Some(c)
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_odbc::drivers()
        .into_iter()
        .find(|d| d.info().id == "db2")
        .expect("db2 preset")
}

/// A session, or `None` without the variable.
async fn open() -> Option<(Arc<dyn Driver>, Box<dyn Session>)> {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_ODBC_DB2 not set; skipping");
        return None;
    };
    let d = driver();
    let s = d.connect(&c, None).await.expect("connect");
    Some((d, s))
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out)
        .await
        .map_err(|e| format!("{e}\n---\n{sql}"))?;
    match out.error.take() {
        Some(e) => Err(format!("{e}\n---\n{sql}")),
        None => Ok(out),
    }
}

/// The last result with columns of `sql`.
async fn select(s: &mut Box<dyn Session>, sql: &str) -> Result<StatementResult, String> {
    let out = exec(s, sql).await?;
    out.results
        .into_iter()
        .rev()
        .find(|r| !r.columns.is_empty())
        .ok_or_else(|| format!("no result: {sql}"))
}

async fn drop_tables(s: &mut Box<dyn Session>, names: &[&str]) {
    for n in names {
        let _ = exec(s, &format!("DROP TABLE {SCHEMA}.{n}")).await;
    }
}

fn check(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(what.into())
    }
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: Some(SCHEMA.into()),
        name: name.into(),
    }
}

fn f32s(n: u32) -> Option<VectorMeta> {
    Some(VectorMeta::dense(Some(n), VectorElement::F32))
}

fn i8s(n: u32) -> Option<VectorMeta> {
    Some(VectorMeta::dense(Some(n), VectorElement::I8))
}

/// The column named `name` of a result, and its position.
fn column<'a>(r: &'a StatementResult, name: &str) -> Result<(usize, &'a ResultColumn), String> {
    r.columns
        .iter()
        .enumerate()
        .find(|(_, c)| c.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("no column {name} in {:?}", r.columns))
}

const SHAPE: &str = "(ID INTEGER NOT NULL PRIMARY KEY, F VECTOR(3, FLOAT32), I VECTOR(4, INT8))";

// ---------------------------------------------------------------- results

/// A SELECT's vector columns carry their meta and type, and their cells
/// come back as the canonical text; the browse query's too.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Db2 LUW 12.1.2+ server (DBINE_TEST_ODBC_DB2)"]
async fn results_mark_vector_columns() {
    let Some((_, mut s)) = open().await else {
        return;
    };
    const T: &str = "DBINE_T_VEC_RES";
    drop_tables(&mut s, &[T]).await;
    let r = async {
        exec(&mut s, &format!("CREATE TABLE {SCHEMA}.{T} {SHAPE}")).await?;
        exec(
            &mut s,
            &format!(
                "INSERT INTO {SCHEMA}.{T} VALUES \
                 (1, VECTOR('[0.1, 2.5, -3]', 3, FLOAT32), VECTOR('[1, -2, 3, 127]', 4, INT8)), \
                 (2, VECTOR('[1e-3, 0, 1.5e3]', 3, FLOAT32), VECTOR('[-128, 0, 0, 5]', 4, INT8))"
            ),
        )
        .await?;
        let browse = s.browse_query(&table(T), 100);
        for sql in [
            format!("SELECT ID, F, I FROM {SCHEMA}.{T} ORDER BY ID"),
            browse,
        ] {
            let r = select(&mut s, &sql).await?;
            let ((k, id), (fk, f), (ik, i)) =
                (column(&r, "ID")?, column(&r, "F")?, column(&r, "I")?);
            check(
                f.vector == f32s(3),
                format!("F's meta: {:?} ({sql})", f.vector),
            )?;
            check(
                i.vector == i8s(4),
                format!("I's meta: {:?} ({sql})", i.vector),
            )?;
            check(id.vector.is_none(), "ID isn't a vector")?;
            check(
                f.type_name == "VECTOR(3, FLOAT32)" && i.type_name == "VECTOR(4, INT8)",
                format!("types: {} / {} ({sql})", f.type_name, i.type_name),
            )?;
            let mut cells: Vec<(Value, Value, Value)> = r
                .rows
                .iter()
                .map(|row| (row[k].clone(), row[fk].clone(), row[ik].clone()))
                .collect();
            cells.sort_by_key(|c| c.0.as_i64());
            check(
                cells
                    == vec![
                        (json!(1), json!("[0.1,2.5,-3]"), json!("[1,-2,3,127]")),
                        (json!(2), json!("[0.001,0,1500]"), json!("[-128,0,0,5]")),
                    ],
                format!("cells: {cells:?} ({sql})"),
            )?;
        }
        // A vector built in the query has no column behind it: its text.
        let r = select(
            &mut s,
            "SELECT VECTOR('[1, 2]', 2, INT8) AS X FROM SYSIBM.SYSDUMMY1",
        )
        .await?;
        check(r.rows[0][0].is_string(), format!("X: {:?}", r.rows))?;
        Ok::<_, String>(())
    }
    .await;
    drop_tables(&mut s, &[T]).await;
    r.unwrap();
}

// ------------------------------------------------------------------ edits

/// The typed insert / update / delete scripts (what the grid's edits and
/// data compare run) write FLOAT32 and INT8 vectors that Db2 takes, with
/// the types of a result and of a describe, and the values round-trip.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Db2 LUW 12.1.2+ server (DBINE_TEST_ODBC_DB2)"]
async fn typed_edit_scripts_round_trip() {
    let Some((d, mut s)) = open().await else {
        return;
    };
    const T: &str = "DBINE_T_VEC_EDIT";
    drop_tables(&mut s, &[T]).await;
    let target = table(T);
    let r = async {
        exec(&mut s, &format!("CREATE TABLE {SCHEMA}.{T} {SHAPE}")).await?;
        let query = format!("SELECT ID, F, I FROM {SCHEMA}.{T} ORDER BY ID");
        // The result's types, as the grid has them.
        let types = select(&mut s, &query).await?.columns;
        // The describe's, as data compare and the import have them.
        let described: Vec<ResultColumn> = s
            .columns(&target)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|c| ResultColumn::new(c.name, c.data_type))
            .collect();
        for (how, cols) in [("result", &types), ("describe", &described)] {
            // Insert: numbers (what rows_for_scripts gives) and the grid's text.
            let rows = vec![
                vec![json!(1), json!([0.5, -1.25, 3.0]), json!([1, -2, 3, 4])],
                vec![json!(2), json!("[7,8,9]"), json!("[-128,127,0,1]")],
            ];
            let sql = d
                .insert_script_typed(&target, cols, &rows)
                .map_err(|e| e.to_string())?;
            exec(&mut s, &sql).await?;
            let got = select(&mut s, &query).await?.rows;
            check(
                got == vec![
                    vec![json!(1), json!("[0.5,-1.25,3]"), json!("[1,-2,3,4]")],
                    vec![json!(2), json!("[7,8,9]"), json!("[-128,127,0,1]")],
                ],
                format!("after insert ({how}): {got:?}\n{sql}"),
            )?;
            // Update both vectors of row 1.
            let change = RowChange {
                key: vec![("ID".into(), json!(1))],
                set: vec![
                    ("F".into(), json!([0.25, 0.0, -2.0])),
                    ("I".into(), json!("[9,-9,9,-9]")),
                ],
                row: Vec::new(),
            };
            let sql = d
                .update_script_typed(&target, &[change], cols)
                .map_err(|e| e.to_string())?;
            exec(&mut s, &sql).await?;
            let got = select(&mut s, &query).await?.rows;
            check(
                got.first() == Some(&vec![json!(1), json!("[0.25,0,-2]"), json!("[9,-9,9,-9]")]),
                format!("after update ({how}): {got:?}\n{sql}"),
            )?;
            // Delete both.
            let keys = vec![
                vec![("ID".to_string(), json!(1))],
                vec![("ID".to_string(), json!(2))],
            ];
            let sql = d
                .delete_script_typed(&target, &keys, cols)
                .map_err(|e| e.to_string())?;
            exec(&mut s, &sql).await?;
            let got = select(&mut s, &query).await?.rows;
            check(
                got.is_empty(),
                format!("after delete ({how}): {got:?}\n{sql}"),
            )?;
        }
        Ok::<_, String>(())
    }
    .await;
    drop_tables(&mut s, &[T]).await;
    r.unwrap();
}

// ------------------------------------------------- designer and describe

fn vector_table(name: &str) -> TableSchema {
    let col = |name: &str, ty: &str, nullable: bool| ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        ..Default::default()
    };
    TableSchema {
        kind: "table".into(),
        schema: Some(SCHEMA.into()),
        name: name.into(),
        columns: vec![
            col("ID", "INTEGER", false),
            col("F", "VECTOR(3, FLOAT32)", true),
            col("I", "VECTOR(4, INT8)", true),
            // The designer's own suggestions.
            col("E", "VECTOR(1536, FLOAT32)", true),
            col("Q", "VECTOR(1536, INT8)", true),
        ],
        primary_key: Some(KeyDef {
            name: None,
            columns: vec!["ID".into()],
        }),
        ..Default::default()
    }
}

/// The designer's CREATE TABLE and ALTER (a vector column added) run, and
/// the describe, the schema and the script generator keep the vector
/// types: the DDL generated from the read schema recreates the table.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Db2 LUW 12.1.2+ server (DBINE_TEST_ODBC_DB2)"]
async fn designer_ddl_and_describe() {
    let Some((d, mut s)) = open().await else {
        return;
    };
    const T: &str = "DBINE_T_VEC_DDL";
    const COPY: &str = "DBINE_T_VEC_DDL2";
    drop_tables(&mut s, &[T, COPY]).await;
    let r = async {
        check(d.capabilities().vectors, "db2 has vectors")?;
        let spec = d.designer().ok_or("no designer")?;
        for t in ["VECTOR(1536, FLOAT32)", "VECTOR(1536, INT8)"] {
            check(spec.data_types.contains(&t), format!("designer offers {t}"))?;
        }
        let parts = DdlParts {
            create: true,
            indexes: true,
            foreign_keys: true,
            ..Default::default()
        };
        let old = vector_table(T);
        let ddl = d.table_ddl(&old, parts).map_err(|e| e.to_string())?;
        exec(&mut s, &ddl).await?;

        // "Modificar tabla": a vector column added.
        let mut new = old.clone();
        new.columns.push(ColumnDef {
            name: "G".into(),
            data_type: "VECTOR(2, INT8)".into(),
            nullable: true,
            ..Default::default()
        });
        let sync = d
            .sync_script(&[TableChange::Alter {
                old,
                new: new.clone(),
            }])
            .map_err(|e| e.to_string())?;
        for st in &sync.statements {
            exec(&mut s, st).await?;
        }
        let types: Vec<(String, String)> = new
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.data_type.clone()))
            .collect();

        // The describe.
        let got: Vec<(String, String)> = s
            .columns(&table(T))
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|c| (c.name, c.data_type))
            .collect();
        check(got == types, format!("describe: {got:?}"))?;

        // The schema (ER diagram, schema compare, script generator).
        let schema = s.database_schema().await.map_err(|e| e.to_string())?;
        let read = schema
            .iter()
            .find(|t| t.name == T && t.schema.as_deref() == Some(SCHEMA))
            .ok_or("table not in the schema")?;
        let got: Vec<(String, String)> = read
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.data_type.clone()))
            .collect();
        check(got == types, format!("schema: {got:?}"))?;

        // The script generator's DDL from what was read recreates it.
        let mut copy = read.clone();
        copy.name = COPY.into();
        let ddl = d.table_ddl(&copy, parts).map_err(|e| e.to_string())?;
        exec(&mut s, &ddl).await?;
        let got: Vec<(String, String)> = s
            .columns(&table(COPY))
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|c| (c.name, c.data_type))
            .collect();
        check(got == types, format!("copy: {got:?}\n{ddl}"))?;
        Ok::<_, String>(())
    }
    .await;
    drop_tables(&mut s, &[T, COPY]).await;
    r.unwrap();
}

// --------------------------------------------------------------- transfer

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.batches.push(batch);
        Ok(())
    }
}

struct Source(VecDeque<RowBatch>);

#[async_trait]
impl BatchSource for Source {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.pop_front()
    }
}

/// A transfer read of the whole table, its rows by id.
async fn read(
    s: &mut Box<dyn Session>,
    name: &str,
) -> Result<(Vec<TransferColumn>, Vec<Vec<Cell>>), String> {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec {
        table: table(name),
        columns: None,
        filter: None,
    };
    s.read_batches(&spec, sink.clone())
        .await
        .map_err(|e| format!("read {name}: {e}"))?;
    let c = std::mem::take(&mut *sink.lock().unwrap());
    let mut rows: Vec<Vec<Cell>> = c.batches.into_iter().flat_map(|b| b.rows).collect();
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => i64::MIN,
    });
    Ok((c.columns, rows))
}

/// A transfer read gives the vectors' types and canonical text; a load
/// into a Db2 vector table (through `VECTOR(?, n, type)`) writes them.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Db2 LUW 12.1.2+ server (DBINE_TEST_ODBC_DB2)"]
async fn transfer_reads_and_loads_vectors() {
    let Some((_, mut s)) = open().await else {
        return;
    };
    const SRC: &str = "DBINE_T_VEC_SRC";
    const DST: &str = "DBINE_T_VEC_DST";
    drop_tables(&mut s, &[SRC, DST]).await;
    let r = async {
        for t in [SRC, DST] {
            exec(&mut s, &format!("CREATE TABLE {SCHEMA}.{t} {SHAPE}")).await?;
        }
        exec(
            &mut s,
            &format!(
                "INSERT INTO {SCHEMA}.{SRC} VALUES \
                 (1, VECTOR('[0.1, 2.5, -3]', 3, FLOAT32), VECTOR('[1, -2, 3, 127]', 4, INT8)), \
                 (2, VECTOR('[4, 5, 6]', 3, FLOAT32), VECTOR('[-128, 0, 0, 5]', 4, INT8)), \
                 (3, VECTOR('[-0.5, 0.25, 1e6]', 3, FLOAT32), VECTOR('[7, 7, 7, 7]', 4, INT8))"
            ),
        )
        .await?;
        let (cols, rows) = read(&mut s, SRC).await?;
        let types: Vec<&str> = cols.iter().map(|c| c.type_name.as_str()).collect();
        check(
            types == ["INTEGER", "VECTOR(3, FLOAT32)", "VECTOR(4, INT8)"],
            format!("read types: {types:?}"),
        )?;
        let t = |s: &str| Cell::Text(s.into());
        let want = vec![
            vec![Cell::Int(1), t("[0.1,2.5,-3]"), t("[1,-2,3,127]")],
            vec![Cell::Int(2), t("[4,5,6]"), t("[-128,0,0,5]")],
            vec![Cell::Int(3), t("[-0.5,0.25,1000000]"), t("[7,7,7,7]")],
        ];
        check(rows == want, format!("read rows: {rows:?}"))?;

        // Load what was read into the other table, in one batch.
        let spec = LoadSpec {
            table: table(DST),
            columns: cols.iter().map(|c| c.name.clone()).collect(),
            table_lock: false,
            keep_identity: false,
            commit_rows: 1_000,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        };
        let batch = RowBatch {
            rows: want.clone(),
            bytes: 0,
        };
        let progress = |_: u64| {};
        let n = s
            .bulk_load(&spec, &cols, &mut Source(vec![batch].into()), &progress)
            .await
            .map_err(|e| format!("load: {e}"))?;
        check(n == 3, format!("loaded {n}"))?;
        let (_, back) = read(&mut s, DST).await?;
        check(back == want, format!("loaded rows: {back:?}"))?;
        let same = select(
            &mut s,
            &format!(
                "SELECT COUNT(*) FROM {SCHEMA}.{SRC} A JOIN {SCHEMA}.{DST} B ON A.ID = B.ID \
                 WHERE VECTOR_DISTANCE(A.F, B.F, EUCLIDEAN) = 0 \
                 AND VECTOR_DISTANCE(A.I, B.I, EUCLIDEAN) = 0"
            ),
        )
        .await?;
        check(
            same.rows[0][0] == json!(3),
            format!("same vectors: {:?}", same.rows),
        )?;
        Ok::<_, String>(())
    }
    .await;
    drop_tables(&mut s, &[SRC, DST]).await;
    r.unwrap();
}

// -------------------------------------------------------------- functions

/// VECTOR_DISTANCE and friends in a query, and the editor's function list
/// has them for this server's version.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Db2 LUW 12.1.2+ server (DBINE_TEST_ODBC_DB2)"]
async fn vector_functions() {
    let Some((_, mut s)) = open().await else {
        return;
    };
    const T: &str = "DBINE_T_VEC_FN";
    drop_tables(&mut s, &[T]).await;
    let r = async {
        exec(&mut s, &format!("CREATE TABLE {SCHEMA}.{T} {SHAPE}")).await?;
        exec(
            &mut s,
            &format!(
                "INSERT INTO {SCHEMA}.{T} VALUES \
                 (1, VECTOR('[3, 4, 0]', 3, FLOAT32), VECTOR('[1, 0, 0, 0]', 4, INT8)), \
                 (2, VECTOR('[1, 0, 0]', 3, FLOAT32), VECTOR('[0, 1, 0, 0]', 4, INT8))"
            ),
        )
        .await?;
        // Nearest first, as a similarity search writes it.
        let r = select(
            &mut s,
            &format!(
                "SELECT ID, VECTOR_DISTANCE(F, VECTOR('[0, 0, 0]', 3, FLOAT32), EUCLIDEAN) AS D, \
                 VECTOR_NORM(F, EUCLIDEAN) AS N, VECTOR_DIMENSION_COUNT(I) AS C, \
                 VECTOR_DISTANCE(I, VECTOR('[1, 0, 0, 0]', 4, INT8), COSINE) AS COS \
                 FROM {SCHEMA}.{T} \
                 ORDER BY VECTOR_DISTANCE(F, VECTOR('[0, 0, 0]', 3, FLOAT32), EUCLIDEAN) \
                 FETCH FIRST 2 ROWS ONLY"
            ),
        )
        .await?;
        let num = |v: &Value| v.as_f64().unwrap_or(f64::NAN);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
        let got: Vec<(Value, f64, f64, Value, f64)> = r
            .rows
            .iter()
            .map(|row| {
                (
                    row[0].clone(),
                    num(&row[1]),
                    num(&row[2]),
                    row[3].clone(),
                    num(&row[4]),
                )
            })
            .collect();
        check(
            got.len() == 2
                && got[0].0 == json!(2)
                && close(got[0].1, 1.0)
                && close(got[0].2, 1.0)
                && got[0].3 == json!(4)
                && close(got[0].4, 1.0)
                && got[1].0 == json!(1)
                && close(got[1].1, 5.0)
                && close(got[1].2, 5.0)
                && close(got[1].4, 0.0),
            format!("functions: {got:?}"),
        )?;

        let catalog = s.function_catalog().await.map_err(|e| e.to_string())?;
        for name in [
            "VECTOR",
            "VECTOR_DISTANCE",
            "VECTOR_NORM",
            "VECTOR_DIMENSION_COUNT",
            "VECTOR_SERIALIZE",
        ] {
            check(
                catalog
                    .iter()
                    .any(|f| f.name.eq_ignore_ascii_case(name) && f.kind == FunctionKind::Scalar),
                format!("{name} in the function catalog"),
            )?;
        }
        Ok::<_, String>(())
    }
    .await;
    drop_tables(&mut s, &[T]).await;
    r.unwrap();
}

// ---------------------------------------------- NULLs, lengths and arrays

/// NULLs of every kind in a result, empty text that isn't NULL, row
/// counts, and the describe's defaults and comments (which a driver with
/// 32-bit lengths gave as text of NULs).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Db2 LUW server (DBINE_TEST_ODBC_DB2)"]
async fn nulls_defaults_comments_and_counts() {
    let Some((_, mut s)) = open().await else {
        return;
    };
    const T: &str = "DBINE_T_LEN_NULLS";
    drop_tables(&mut s, &[T]).await;
    let r = async {
        for sql in [
            format!(
                "CREATE TABLE {SCHEMA}.{T} (ID INTEGER NOT NULL PRIMARY KEY, \
                 S VARCHAR(20) DEFAULT 'abc', N INTEGER, D DECIMAL(10, 2), F DOUBLE, \
                 T TIMESTAMP, DT DATE, B VARBINARY(8))"
            ),
            format!("COMMENT ON TABLE {SCHEMA}.{T} IS 'tabla de prueba'"),
            format!("COMMENT ON COLUMN {SCHEMA}.{T}.S IS 'texto'"),
            format!("INSERT INTO {SCHEMA}.{T} (ID) VALUES (1)"),
            format!(
                "INSERT INTO {SCHEMA}.{T} VALUES \
                 (2, NULL, 5, 1.50, 2.5, '2026-01-02-03.04.05', '2026-01-02', BX'0102')"
            ),
            format!("INSERT INTO {SCHEMA}.{T} VALUES (3, '', NULL, NULL, NULL, NULL, NULL, NULL)"),
        ] {
            exec(&mut s, &sql).await?;
        }
        let r = select(
            &mut s,
            &format!("SELECT ID, S, N, D, F, T, DT, B FROM {SCHEMA}.{T} ORDER BY ID"),
        )
        .await?;
        let null = Value::Null;
        let want = vec![
            vec![
                json!(1),
                json!("abc"),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
            ],
            vec![
                json!(2),
                null.clone(),
                json!(5),
                json!("1.50"),
                json!(2.5),
                json!("2026-01-02 03:04:05"),
                json!("2026-01-02"),
                json!("0x0102"),
            ],
            vec![
                json!(3),
                json!(""),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null,
            ],
        ];
        check(r.rows == want, format!("rows: {:?}", r.rows))?;

        // Row counts.
        let out = exec(
            &mut s,
            &format!("UPDATE {SCHEMA}.{T} SET N = 7 WHERE ID >= 2"),
        )
        .await?;
        let counts: Vec<Option<u64>> = out.results.iter().map(|r| r.rows_affected).collect();
        check(counts == [Some(2)], format!("update count: {counts:?}"))?;
        let out = exec(&mut s, &format!("DELETE FROM {SCHEMA}.{T} WHERE ID = 999")).await?;
        let counts: Vec<Option<u64>> = out.results.iter().map(|r| r.rows_affected).collect();
        check(counts == [Some(0)], format!("delete count: {counts:?}"))?;

        // The describe's defaults.
        let cols = s.columns(&table(T)).await.map_err(|e| e.to_string())?;
        let defaults: Vec<(String, Option<String>)> = cols
            .iter()
            .map(|c| (c.name.clone(), c.default_value.clone()))
            .collect();
        let want: Vec<(String, Option<String>)> = ["ID", "S", "N", "D", "F", "T", "DT", "B"]
            .iter()
            .map(|n| (n.to_string(), (*n == "S").then(|| "'abc'".to_string())))
            .collect();
        check(defaults == want, format!("defaults: {defaults:?}"))?;

        // The schema's comments and defaults.
        let schema = s.database_schema().await.map_err(|e| e.to_string())?;
        let t = schema
            .iter()
            .find(|t| t.name == T && t.schema.as_deref() == Some(SCHEMA))
            .ok_or("table not in the schema")?;
        check(
            t.comment.as_deref() == Some("tabla de prueba"),
            format!("table comment: {:?}", t.comment),
        )?;
        let got: Vec<(String, Option<String>, Option<String>)> = t
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.default_value.clone(), c.comment.clone()))
            .collect();
        let want: Vec<(String, Option<String>, Option<String>)> =
            ["ID", "S", "N", "D", "F", "T", "DT", "B"]
                .iter()
                .map(|n| {
                    let s = *n == "S";
                    (
                        n.to_string(),
                        s.then(|| "'abc'".to_string()),
                        s.then(|| "texto".to_string()),
                    )
                })
                .collect();
        check(got == want, format!("schema columns: {got:?}"))?;
        Ok::<_, String>(())
    }
    .await;
    drop_tables(&mut s, &[T]).await;
    r.unwrap();
}

/// Rows of the multi-row transfer test: text of 0 to 299 characters (empty
/// is not NULL), and NULLs in every column but the key, each at its own
/// rhythm.
const ROWS: i64 = 2_500;

fn expected_row(k: i64) -> Vec<Cell> {
    let null_every = |n: i64, c: Cell| if k % n == 0 { Cell::Null } else { c };
    let letter = (b'A' + (k % 26) as u8) as char;
    vec![
        Cell::Int(k),
        null_every(7, Cell::Text(letter.to_string().repeat((k % 300) as usize))),
        null_every(5, Cell::Int(k * 3)),
        null_every(11, Cell::Decimal(format!("{}.{:02}", k / 4, (k % 4) * 25))),
        null_every(
            13,
            Cell::DateTime(format!("2026-01-01 00:{:02}:{:02}", k / 60, k % 60)),
        ),
        null_every(3, Cell::Bytes(k.to_string().into_bytes())),
    ]
}

const LEN_SHAPE: &str = "(ID INTEGER NOT NULL PRIMARY KEY, S VARCHAR(300), N INTEGER, \
     D DECIMAL(12, 2), T TIMESTAMP, B VARBINARY(16))";

/// A transfer read of more rows than a fetch block and a load of them
/// into another table come back exactly: lengths that change from row to
/// row, empty text and NULLs (a driver with 32-bit lengths read them out
/// of step through row arrays, and loaded `'ab', 'cdef', 'ghijkl'` as
/// `'ab', '', 'ghij'`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Db2 LUW server (DBINE_TEST_ODBC_DB2)"]
async fn multi_row_transfer_read_and_load() {
    let Some((_, mut s)) = open().await else {
        return;
    };
    const SRC: &str = "DBINE_T_LEN_SRC";
    const DST: &str = "DBINE_T_LEN_DST";
    drop_tables(&mut s, &[SRC, DST]).await;
    let r = async {
        for t in [SRC, DST] {
            exec(&mut s, &format!("CREATE TABLE {SCHEMA}.{t} {LEN_SHAPE}")).await?;
        }
        exec(
            &mut s,
            &format!(
                "INSERT INTO {SCHEMA}.{SRC} (ID, S, N, D, T, B) \
                 WITH G(K) AS (VALUES 1 UNION ALL SELECT K + 1 FROM G WHERE K < {ROWS}) \
                 SELECT K, \
                 CASE WHEN MOD(K, 7) = 0 THEN NULL ELSE REPEAT(CHR(65 + MOD(K, 26)), MOD(K, 300)) END, \
                 CASE WHEN MOD(K, 5) = 0 THEN NULL ELSE K * 3 END, \
                 CASE WHEN MOD(K, 11) = 0 THEN NULL ELSE DECIMAL(K, 12, 2) / 4 END, \
                 CASE WHEN MOD(K, 13) = 0 THEN NULL ELSE TIMESTAMP('2026-01-01-00.00.00') + K SECONDS END, \
                 CASE WHEN MOD(K, 3) = 0 THEN NULL ELSE CAST(TRIM(CHAR(K)) AS VARBINARY(16)) END \
                 FROM G"
            ),
        )
        .await?;
        let want: Vec<Vec<Cell>> = (1..=ROWS).map(expected_row).collect();
        let same = |what: &str, got: &[Vec<Cell>]| {
            let first = got.iter().zip(&want).position(|(g, w)| g != w);
            check(
                got.len() == want.len() && first.is_none(),
                format!(
                    "{what}: {} rows; first difference at {first:?}: {:?} vs {:?}",
                    got.len(),
                    first.and_then(|i| got.get(i)),
                    first.and_then(|i| want.get(i)),
                ),
            )
        };

        // The read, in batches.
        let sink = Arc::new(Mutex::new(Collect::default()));
        let spec = ReadSpec {
            table: table(SRC),
            columns: None,
            filter: None,
        };
        s.read_batches(&spec, sink.clone())
            .await
            .map_err(|e| format!("read: {e}"))?;
        let c = std::mem::take(&mut *sink.lock().unwrap());
        check(c.batches.len() > 1, format!("{} batches", c.batches.len()))?;
        let mut got: Vec<Vec<Cell>> = c.batches.iter().flat_map(|b| b.rows.clone()).collect();
        // A table's rows come in no particular order.
        got.sort_by_key(|r| match r[0] {
            Cell::Int(i) => i,
            _ => i64::MIN,
        });
        same("read", &got)?;

        // The load, batch by batch as the reader gave them.
        let load = LoadSpec {
            table: table(DST),
            columns: c.columns.iter().map(|c| c.name.clone()).collect(),
            table_lock: false,
            keep_identity: false,
            commit_rows: 1_000,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        };
        let progress = |_: u64| {};
        let n = s
            .bulk_load(
                &load,
                &c.columns,
                &mut Source(c.batches.into_iter().collect()),
                &progress,
            )
            .await
            .map_err(|e| format!("load: {e}"))?;
        check(n == ROWS as u64, format!("loaded {n}"))?;
        let (_, back) = read(&mut s, DST).await?;
        same("loaded", &back)?;
        Ok::<_, String>(())
    }
    .await;
    drop_tables(&mut s, &[SRC, DST]).await;
    r.unwrap();
}
