//! Vectors (embeddings) against bigquery-emulator: an `ARRAY<FLOAT64>`
//! table made by the driver's own DDL (as the designer would, with a vector
//! index), rows inserted and edited with the driver's scripts, read back as
//! vectors, described, a similarity query, and a transfer (BigQuery to
//! BigQuery, and a pgvector-shaped source: text cells with a JSON array).
//!
//! The emulator has no vector indexes (`CREATE VECTOR INDEX`,
//! `VECTOR_INDEXES`, `VECTOR_SEARCH` and `ML.DISTANCE` aren't there): the
//! index's script is checked as text here, and its reading by the unit
//! tests (`indexes.rs`).
//!
//! ```sh
//! docker start dbine-test-bigquery
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 \
//!   cargo test -p dbine-driver-bigquery --test vectors -- --ignored --nocapture
//! ```

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, ObjectRef, QueryOutcome,
    RowChange, Session, TableSchema, VectorElement, VectorIndex,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

async fn open() -> Option<(Arc<dyn Driver>, Box<dyn Session>)> {
    let url = std::env::var("DBINE_TEST_BIGQUERY_URL").ok()?;
    let mut c = ConnectionConfig {
        driver: "bigquery".into(),
        ..Default::default()
    };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url);
    let d = dbine_driver_bigquery::drivers().pop().unwrap();
    let s = d.connect(&c, Some("ds1")).await.unwrap();
    Some((d, s))
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: Some("ds1".into()),
        name: name.into(),
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    if let Some(e) = &out.error {
        panic!("{sql}: {e}");
    }
    out
}

async fn cleanup(s: &mut Box<dyn Session>) {
    for t in ["vec_docs", "vec_docs_copy"] {
        let mut out = QueryOutcome::default();
        let _ = s
            .execute(&format!("DROP TABLE IF EXISTS ds1.{t}"), 10, &mut out)
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
    let Some((d, mut s)) = open().await else {
        eprintln!("DBINE_TEST_BIGQUERY_URL not set; skipping");
        return;
    };
    cleanup(&mut s).await;
    let outcome = tokio::spawn(async move {
        scenario(d.as_ref(), &mut s).await;
        s
    })
    .await;
    let Some((_, mut s)) = open().await else {
        return;
    };
    cleanup(&mut s).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn scenario(d: &dyn Driver, s: &mut Box<dyn Session>) {
    // 1. The table and its vector index, from a TableSchema (the designer).
    let mut vi = VectorIndex {
        algorithm: Some("ivf".into()),
        metric: Some("cosine".into()),
        ..Default::default()
    };
    vi.params.insert("lists".into(), "100".into());
    let docs = TableSchema {
        kind: kinds::TABLE.into(),
        schema: Some("ds1".into()),
        name: "vec_docs".into(),
        columns: vec![
            col("id", "INT64"),
            col("kind", "STRING"),
            col("emb", "ARRAY<FLOAT64>"),
        ],
        indexes: vec![IndexDef {
            name: "vec_docs_emb".into(),
            columns: vec!["emb".into(), "kind".into()],
            vector: Some(vi),
            ..Default::default()
        }],
        ..Default::default()
    };
    let create = d
        .table_ddl(
            &docs,
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(create.contains("`emb` ARRAY<FLOAT64>"), "{create}");
    run(s, &create).await;
    let index = d
        .table_ddl(
            &docs,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(index, "CREATE VECTOR INDEX `vec_docs_emb` ON `ds1`.`vec_docs` (`emb`) STORING (`kind`) OPTIONS (distance_type = 'COSINE', index_type = 'IVF', ivf_options = '{\"num_lists\":100}');");

    // 2. Rows from the insert script, vectors as JSON arrays.
    let ins = d
        .insert_script(
            &table("vec_docs"),
            &["id".into(), "kind".into(), "emb".into()],
            &[
                vec![json!(1), json!("a"), json!([1, 0, 0])],
                vec![json!(2), json!("b"), json!([0.1, 0.2, 0.3])],
                vec![json!(3), json!("a"), json!([-1.5, 2.25, 1e-7])],
            ],
        )
        .unwrap();
    run(s, &ins).await;

    // 3. Read back: the column is a vector, the text exact.
    let out = run(s, "SELECT id, emb FROM ds1.vec_docs ORDER BY id").await;
    let r = out.results[0].clone();
    let v = r.columns[1].vector.as_ref().expect("vector column");
    assert_eq!(v.element, VectorElement::F64);
    assert_eq!(r.columns[0].vector, None);
    let cells: Vec<_> = r.rows.iter().map(|row| row[1].clone()).collect();
    assert_eq!(
        cells,
        [
            json!("[1,0,0]"),
            json!("[0.1,0.2,0.3]"),
            json!("[-1.5,2.25,0.0000001]")
        ]
    );

    // 4. Describe: BigQuery's vector type (it declares no dimension). The
    // explorer's columns use the API's legacy names (FLOAT); the emulator's
    // INFORMATION_SCHEMA says DOUBLE where BigQuery says FLOAT64.
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == "vec_docs").unwrap();
    let ty = &t
        .columns
        .iter()
        .find(|c| c.name == "emb")
        .unwrap()
        .data_type;
    assert!(ty == "ARRAY<FLOAT64>" || ty == "ARRAY<DOUBLE>", "{ty}");
    let cols = s.columns(&table("vec_docs")).await.unwrap();
    assert_eq!(
        cols.iter().find(|c| c.name == "emb").unwrap().data_type,
        "ARRAY<FLOAT>"
    );

    // 5. Vector indexes: the emulator has none (see the module's doc).

    // 6. A similarity query.
    let out = run(
        s,
        "SELECT id, COSINE_DISTANCE(emb, ARRAY<FLOAT64>[1, 0, 0]) AS d FROM ds1.vec_docs ORDER BY d, id LIMIT 1",
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
            &table("vec_docs"),
            &[RowChange {
                key: vec![("id".into(), json!(2))],
                set: vec![("emb".into(), rows[0][1].clone())],
                row: Vec::new(),
            }],
        )
        .unwrap();
    run(s, &up).await;
    let out = run(s, "SELECT emb FROM ds1.vec_docs WHERE id = 2").await;
    assert_eq!(out.results[0].rows[0][0], json!("[0.5,0.25,-1]"));

    // 8. Transfer: read as vector text, load into a copy; then a
    // pgvector-shaped source (text cells with a JSON array).
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: table("vec_docs"),
            columns: Some(vec!["id".into(), "emb".into()]),
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
    let copy = TableSchema {
        name: "vec_docs_copy".into(),
        indexes: Vec::new(),
        columns: vec![col("id", "INT64"), col("emb", "ARRAY<FLOAT64>")],
        ..docs.clone()
    };
    run(
        s,
        &d.table_ddl(
            &copy,
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .unwrap(),
    )
    .await;
    let spec = LoadSpec {
        table: table("vec_docs_copy"),
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
    let pg_like = vec![
        vec![Cell::Int(10), Cell::Text("[0.25,-0.5,4]".into())],
        vec![Cell::Int(11), Cell::Text("[1,2,3]".into())],
    ];
    let n = s
        .bulk_load(&spec, &pg_cols, &mut Once(Some(pg_like)), &|_| {})
        .await
        .unwrap();
    assert_eq!(n, 2);
    let out = run(s, "SELECT id, emb FROM ds1.vec_docs_copy ORDER BY id").await;
    let texts: Vec<_> = out.results[0].rows.iter().map(|r| r[1].clone()).collect();
    assert_eq!(
        texts,
        [
            json!("[1,0,0]"),
            json!("[0.5,0.25,-1]"),
            json!("[-1.5,2.25,0.0000001]"),
            json!("[0.25,-0.5,4]"),
            json!("[1,2,3]")
        ]
    );
    eprintln!("bigquery vectors: table, scripts, query and transfer checked (no vector index in the emulator)");
}
