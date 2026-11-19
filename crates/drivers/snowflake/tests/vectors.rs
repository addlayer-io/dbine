//! Vectors (embeddings) against a real Snowflake account (there's no
//! emulator): a `VECTOR(FLOAT, 3)` table made by the driver's own DDL (as
//! the designer would; Snowflake has no vector indexes), rows inserted and
//! edited with the driver's scripts, read back as vectors, described with
//! their dimension (explorer and database schema), a similarity query, and
//! a transfer (Snowflake to Snowflake, and a pgvector-shaped source: text
//! cells with a JSON array). It creates the database `DBINE_TEST_VECTORS`
//! and drops it first and at the end. Run with the variables of
//! `tests/integration.rs` (a warehouse is needed):
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=… DBINE_TEST_SNOWFLAKE_USER=… DBINE_TEST_SNOWFLAKE_TOKEN=… \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH \
//!   cargo test -p dbine-driver-snowflake --test vectors -- --ignored --nocapture
//! ```

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, ObjectRef, QueryOutcome, RowChange,
    Session, TableSchema,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

const DB: &str = "DBINE_TEST_VECTORS";

fn config() -> Option<ConnectionConfig> {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok();
    let mut options: std::collections::HashMap<String, String> = [
        ("account".to_string(), var("ACCOUNT")?),
        ("token".to_string(), var("TOKEN")?),
        ("schema".to_string(), "PUBLIC".to_string()),
    ]
    .into();
    if let Some(w) = var("WAREHOUSE") {
        options.insert("warehouse".into(), w);
    }
    Some(ConnectionConfig {
        driver: "snowflake".into(),
        username: var("USER"),
        options: options.into_iter().collect(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

struct Once(Option<Vec<Vec<Cell>>>);

#[dbine_driver::async_trait]
impl BatchSource for Once {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.take().map(|rows| RowBatch { rows, bytes: 0 })
    }
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: Some("PUBLIC".into()),
        name: name.into(),
    }
}

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        ..Default::default()
    }
}

#[tokio::test]
#[ignore]
async fn vectors_end_to_end() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_SNOWFLAKE_* not set; skipping");
        return;
    };
    let d = dbine_driver_snowflake::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    run(&mut admin, &format!("CREATE OR REPLACE DATABASE {DB}")).await;
    let s = d.connect(&cfg, Some(DB)).await.expect("connect");
    let d2 = d.clone();
    let outcome = tokio::spawn(async move {
        let mut s = s;
        scenario(d2.as_ref(), &mut s).await;
    })
    .await;
    run(&mut admin, &format!("DROP DATABASE IF EXISTS {DB}")).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn scenario(d: &dyn Driver, s: &mut Box<dyn Session>) {
    // 1. The table, from a TableSchema (the designer).
    let docs = TableSchema {
        kind: kinds::TABLE.into(),
        schema: Some("PUBLIC".into()),
        name: "DOCS".into(),
        columns: vec![col("ID", "NUMBER(38,0)"), col("EMB", "VECTOR(FLOAT, 3)")],
        ..Default::default()
    };
    let create = d
        .table_ddl(
            &docs,
            DdlParts {
                create: true,
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    run(s, &create).await;

    // 2. Rows from the insert script, vectors as JSON arrays.
    let ins = d
        .insert_script(
            &table("DOCS"),
            &["ID".into(), "EMB".into()],
            &[
                vec![json!(1), json!([1, 0, 0])],
                vec![json!(2), json!([0.1, 0.2, 0.3])],
                vec![json!(3), json!(null)],
            ],
        )
        .unwrap();
    run(s, &ins).await;

    // 3. Read back: the column is a vector, the text exact (FLOAT is 32 bits).
    let out = run(s, "SELECT ID, EMB FROM PUBLIC.DOCS ORDER BY ID").await;
    let r = out.results[0].clone();
    assert!(r.columns[1].vector.is_some(), "{:?}", r.columns[1]);
    let cells: Vec<_> = r.rows.iter().map(|row| row[1].clone()).collect();
    assert_eq!(
        cells,
        [json!("[1,0,0]"), json!("[0.1,0.2,0.3]"), json!(null)]
    );

    // 4. Describe keeps the dimension (explorer and database schema).
    let cols = s.columns(&table("DOCS")).await.unwrap();
    let emb = cols.iter().find(|c| c.name == "EMB").unwrap();
    assert_eq!(emb.data_type.replace(' ', ""), "VECTOR(FLOAT,3)");
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == "DOCS").unwrap();
    let emb = t.columns.iter().find(|c| c.name == "EMB").unwrap();
    assert_eq!(emb.data_type.replace(' ', ""), "VECTOR(FLOAT,3)");
    // So a clone of it is the same table.
    let again = d
        .table_ddl(
            &TableSchema {
                name: "DOCS_CLONE".into(),
                ..t.clone()
            },
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
    run(s, &again).await;

    // 5. Snowflake has no vector indexes.

    // 6. A similarity query.
    let out = run(
        s,
        "SELECT ID, VECTOR_COSINE_SIMILARITY(EMB, [1, 0, 0]::VECTOR(FLOAT, 3)) AS SIM FROM PUBLIC.DOCS WHERE EMB IS NOT NULL ORDER BY SIM DESC LIMIT 1",
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], json!(1));

    // 7. An edit from the grid: the vector cell as a JSON array.
    let rows = dbine_driver::vector::rows_for_scripts(
        &r.columns,
        &[vec![json!(2), json!("[0.5,0.25,-1]")]],
    );
    let up = d
        .update_script(
            &table("DOCS"),
            &[RowChange {
                key: vec![("ID".into(), json!(2))],
                set: vec![("EMB".into(), rows[0][1].clone())],
                row: Vec::new(),
            }],
        )
        .unwrap();
    run(s, &up).await;
    let out = run(s, "SELECT EMB FROM PUBLIC.DOCS WHERE ID = 2").await;
    assert_eq!(out.results[0].rows[0][0], json!("[0.5,0.25,-1]"));

    // 8. Transfer: read as vector text, load into the clone; then a
    // pgvector-shaped source (text cells with a JSON array).
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: table("DOCS"),
            columns: None,
            filter: None,
        },
        sink.clone(),
    )
    .await
    .unwrap();
    let got = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(got.columns[1].type_name.replace(' ', ""), "VECTOR(FLOAT,3)");
    let spec = LoadSpec {
        table: table("DOCS_CLONE"),
        columns: vec!["ID".into(), "EMB".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let n = s
        .bulk_load(
            &spec,
            &got.columns,
            &mut Once(Some(got.rows.clone())),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(n, 3);
    let pg_cols = vec![
        TransferColumn {
            name: "id".into(),
            type_name: "integer".into(),
            nullable: false,
        },
        TransferColumn {
            name: "emb".into(),
            type_name: "vector(3)".into(),
            nullable: true,
        },
    ];
    let pg_like = vec![vec![Cell::Int(10), Cell::Text("[0.25,-0.5,4]".into())]];
    let n = s
        .bulk_load(&spec, &pg_cols, &mut Once(Some(pg_like)), &|_| {})
        .await
        .unwrap();
    assert_eq!(n, 1);
    let out = run(s, "SELECT EMB FROM PUBLIC.DOCS_CLONE ORDER BY ID").await;
    let texts: Vec<_> = out.results[0].rows.iter().map(|r| r[0].clone()).collect();
    assert_eq!(
        texts,
        [
            json!("[1,0,0]"),
            json!("[0.5,0.25,-1]"),
            json!(null),
            json!("[0.25,-0.5,4]")
        ]
    );

    // 9. An INT vector column: the typed scripts cast to VECTOR(INT, n),
    // from the describe's types and from a result's.
    run(
        s,
        "CREATE TABLE PUBLIC.INTS (ID NUMBER(38,0), EMB VECTOR(INT, 3))",
    )
    .await;
    let described: Vec<dbine_driver::ResultColumn> = s
        .columns(&table("INTS"))
        .await
        .unwrap()
        .iter()
        .map(|c| dbine_driver::ResultColumn::new(&c.name, &c.data_type))
        .collect();
    let ins = d
        .insert_script_typed(
            &table("INTS"),
            &described,
            &[vec![json!(1), json!([1, -2, 3])]],
        )
        .unwrap();
    assert!(ins.contains("::VECTOR(INT, 3)"), "{ins}");
    run(s, &ins).await;
    let out = run(s, "SELECT ID, EMB FROM PUBLIC.INTS").await;
    let r = out.results[0].clone();
    assert_eq!(r.rows[0][1], json!("[1,-2,3]"));
    let rows =
        dbine_driver::vector::rows_for_scripts(&r.columns, &[vec![json!(1), json!("[4,5,-6]")]]);
    let up = d
        .update_script_typed(
            &table("INTS"),
            &[RowChange {
                key: vec![("ID".into(), json!(1))],
                set: vec![("EMB".into(), rows[0][1].clone())],
                row: Vec::new(),
            }],
            &r.columns,
        )
        .unwrap();
    assert!(up.contains("::VECTOR(INT, 3)"), "{up}");
    run(s, &up).await;
    let out = run(s, "SELECT EMB FROM PUBLIC.INTS").await;
    assert_eq!(out.results[0].rows[0][0], json!("[4,5,-6]"));
}
