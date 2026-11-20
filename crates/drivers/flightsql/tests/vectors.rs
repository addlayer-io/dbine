//! Vectors (embeddings) over Flight SQL, against GizmoSQL (DuckDB behind):
//! a `FLOAT[3]` column (Arrow `FixedSizeList<Float32>(3)`) in a table made
//! by the driver's own DDL, rows inserted and edited with the driver's
//! scripts, read back as vectors, described, a similarity query, and a
//! transfer (to the same engine, and from a pgvector-shaped source: text
//! cells with a JSON array). Flight SQL has no index metadata or DDL of
//! its own; where DuckDB's vss is installed on the server, an HNSW index
//! made in its SQL is checked in the index usage report. The tables are
//! dropped first and at the end.
//!
//! ```sh
//! docker start dbine-test-flightsql
//! DBINE_TEST_FLIGHTSQL_URL=http://localhost:25337 \
//!   cargo test -p dbine-driver-flightsql --test vectors -- --ignored --nocapture
//! ```

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, ObjectRef, QueryOutcome, RowChange,
    Session, TableSchema, VectorElement,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FLIGHTSQL_URL").ok()?;
    let rest = url.split("://").nth(1)?;
    let (host, port) = rest.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "flightsql".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(
            std::env::var("DBINE_TEST_FLIGHTSQL_USER").unwrap_or_else(|_| "gizmosql_user".into()),
        ),
        password: Some(
            std::env::var("DBINE_TEST_FLIGHTSQL_PASSWORD").unwrap_or_else(|_| "secreto1".into()),
        ),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 1000, &mut out).await {
        panic!("{text}: {e}");
    }
    if let Some(e) = &out.error {
        panic!("{text}: {e}");
    }
    out
}

async fn cleanup(s: &mut Box<dyn Session>) {
    for t in ["dbine_vec_docs", "dbine_vec_copy"] {
        let mut out = QueryOutcome::default();
        let _ = s
            .execute(&format!("DROP TABLE IF EXISTS {t}"), 10, &mut out)
            .await;
    }
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
        schema: None,
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
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_FLIGHTSQL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_flightsql::drivers().pop().unwrap();
    let mut s = d.connect(&c, None).await.expect("connect");
    cleanup(&mut s).await;
    let d2 = d.clone();
    let outcome = tokio::spawn(async move {
        scenario(d2.as_ref(), &mut s).await;
    })
    .await;
    let mut s = d.connect(&c, None).await.expect("connect");
    cleanup(&mut s).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn scenario(d: &dyn Driver, s: &mut Box<dyn Session>) {
    // 1. The table. Flight SQL has no DDL of its own (the driver writes
    // none: the engine behind does), so it's the engine's SQL.
    assert!(d
        .table_ddl(
            &TableSchema {
                kind: kinds::TABLE.into(),
                name: "dbine_vec_docs".into(),
                columns: vec![col("id", "INTEGER"), col("emb", "FLOAT[3]")],
                ..Default::default()
            },
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .is_err());
    run(s, "CREATE TABLE dbine_vec_docs (id INTEGER, emb FLOAT[3])").await;

    // 2. Rows from the insert script, vectors as JSON arrays.
    let ins = d
        .insert_script(
            &table("dbine_vec_docs"),
            &["id".into(), "emb".into()],
            &[
                vec![json!(1), json!([1, 0, 0])],
                vec![json!(2), json!([0.1, 0.2, 0.3])],
                vec![json!(3), json!(null)],
            ],
        )
        .unwrap();
    run(s, &ins).await;

    // 3. Read back: the column is a vector, the text exact (FLOAT is 32 bits).
    let out = run(s, "SELECT id, emb FROM dbine_vec_docs ORDER BY id").await;
    let r = out.results[0].clone();
    let v = r.columns[1].vector.as_ref().expect("vector column");
    assert_eq!(v.dims, Some(3));
    assert_eq!(v.element, VectorElement::F32);
    // The engine's SQL type, not Arrow's FixedSizeList.
    assert_eq!(r.columns[1].type_name, "FLOAT[3]");
    assert_eq!(r.columns[0].vector, None);
    let cells: Vec<_> = r.rows.iter().map(|row| row[1].clone()).collect();
    assert_eq!(
        cells,
        [json!("[1,0,0]"), json!("[0.1,0.2,0.3]"), json!(null)]
    );

    // 4. Described: the engine's type with its dimension.
    let cols = s.columns(&table("dbine_vec_docs")).await.unwrap();
    let emb = &cols.iter().find(|c| c.name == "emb").unwrap().data_type;
    assert!(emb.contains('3'), "{emb}");

    // 5. Flight SQL has no index DDL of its own, but DuckDB behind it has
    // vss's HNSW: where the extension is installed on the server, an index
    // made in its SQL is listed by the index usage report with its
    // settings. (The test doesn't install it: an extension outlives the
    // test on the server.)
    let mut out = QueryOutcome::default();
    let vss = s.execute("LOAD vss", 10, &mut out).await.is_ok() && out.error.is_none();
    if vss {
        run(s, "SET hnsw_enable_experimental_persistence = true").await;
        run(
            s,
            "CREATE INDEX dbine_vec_hnsw ON dbine_vec_docs USING HNSW (emb) WITH (metric = 'cosine', M = 8)",
        )
        .await;
        let usage = s
            .index_usage(&table("dbine_vec_docs"))
            .await
            .unwrap()
            .unwrap();
        let ix = usage
            .indexes
            .iter()
            .find(|i| i.name == "dbine_vec_hnsw")
            .unwrap_or_else(|| panic!("{:?}", usage.indexes));
        assert_eq!(ix.kind, "HNSW");
        assert_eq!(ix.key_columns, ["emb"]);
        let v = ix.vector.as_ref().expect("vector index");
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        run(s, "DROP INDEX dbine_vec_hnsw").await;
        run(s, "RESET hnsw_enable_experimental_persistence").await;
    } else {
        eprintln!("flightsql: vss isn't installed on the server; the HNSW index step is skipped");
    }

    // 6. A similarity query.
    let out = run(
        s,
        "SELECT id, array_cosine_similarity(emb, [1, 0, 0]::FLOAT[3]) AS sim FROM dbine_vec_docs WHERE emb IS NOT NULL ORDER BY sim DESC LIMIT 1",
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
            &table("dbine_vec_docs"),
            &[RowChange {
                key: vec![("id".into(), json!(2))],
                set: vec![("emb".into(), rows[0][1].clone())],
                row: Vec::new(),
            }],
        )
        .unwrap();
    run(s, &up).await;
    let out = run(s, "SELECT emb FROM dbine_vec_docs WHERE id = 2").await;
    assert_eq!(out.results[0].rows[0][0], json!("[0.5,0.25,-1]"));

    // 8. Transfer: read as vector text, load into a copy; then a
    // pgvector-shaped source (text cells with a JSON array).
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: table("dbine_vec_docs"),
            columns: None,
            filter: None,
        },
        sink.clone(),
    )
    .await
    .unwrap();
    let mut got = std::mem::take(&mut *sink.lock().unwrap());
    got.rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    assert_eq!(got.rows[0][1], Cell::Json("[1,0,0]".into()));
    assert_eq!(got.rows[1][1], Cell::Json("[0.5,0.25,-1]".into()));
    run(s, "CREATE TABLE dbine_vec_copy (id INTEGER, emb FLOAT[3])").await;
    let spec = LoadSpec {
        table: table("dbine_vec_copy"),
        columns: vec!["id".into(), "emb".into()],
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
    let out = run(s, "SELECT emb FROM dbine_vec_copy ORDER BY id").await;
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
    eprintln!("flightsql vectors: table, scripts, query and transfer checked");
}
