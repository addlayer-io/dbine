//! Vectors (embeddings) against the Cloud Spanner emulator: a table with an
//! `ARRAY<FLOAT32>(vector_length=>3)` column and a vector index made by the
//! driver's own DDL (as the designer would), rows inserted and edited with
//! the driver's scripts, read back as vectors, described with their
//! dimension, the index listed with its settings, a similarity query, and a
//! transfer (Spanner to Spanner, and a pgvector-shaped source: text cells
//! with a JSON array). Everything lives in its own database, dropped first
//! and at the end.
//!
//! ```sh
//! docker start dbine-test-spanner
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 \
//!   cargo test -p dbine-driver-spanner --test vectors -- --ignored --nocapture
//! ```

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, KeyDef, ObjectRef,
    QueryOutcome, RowChange, Session, TableSchema, VectorElement, VectorIndex,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

const DB: &str = "dbine_vectors";

async fn admin(url: &str, method: reqwest::Method, path: &str, body: Option<serde_json::Value>) {
    let http = reqwest::Client::new();
    let mut r = http.request(method, format!("{url}/v1/projects/test/instances{path}"));
    if let Some(b) = body {
        r = r.json(&b);
    }
    let _ = r.send().await;
}

async fn fresh_database(url: &str) {
    admin(
        url,
        reqwest::Method::POST,
        "",
        Some(json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } })),
    )
    .await;
    drop_database(url).await;
    admin(
        url,
        reqwest::Method::POST,
        "/i1/databases",
        Some(json!({ "createStatement": format!("CREATE DATABASE `{DB}`") })),
    )
    .await;
}

async fn drop_database(url: &str) {
    admin(
        url,
        reqwest::Method::DELETE,
        &format!("/i1/databases/{DB}"),
        None,
    )
    .await;
}

async fn connect(url: &str) -> (Arc<dyn Driver>, Box<dyn Session>) {
    let mut cfg = ConnectionConfig {
        driver: "spanner".into(),
        database: DB.into(),
        ..Default::default()
    };
    for (k, v) in [
        ("project_id", "test"),
        ("instance", "i1"),
        ("endpoint_url", url),
    ] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    let s = d.connect(&cfg, None).await.expect("connect");
    (d, s)
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

fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        ..Default::default()
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

#[tokio::test]
#[ignore]
async fn vectors_end_to_end() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else {
        eprintln!("DBINE_TEST_SPANNER_URL not set; skipping");
        return;
    };
    fresh_database(&url).await;
    let u = url.clone();
    let outcome = tokio::spawn(async move { scenario(&u).await }).await;
    drop_database(&url).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn scenario(url: &str) {
    let (d, mut s) = connect(url).await;

    // 1. The table and its vector index, from a TableSchema (the designer).
    let mut vi = VectorIndex {
        metric: Some("cosine".into()),
        ..Default::default()
    };
    vi.params.insert("tree_depth".into(), "2".into());
    vi.params.insert("lists".into(), "10".into());
    let docs = TableSchema {
        kind: kinds::TABLE.into(),
        name: "docs".into(),
        columns: vec![
            col("id", "INT64", false),
            col("title", "STRING(MAX)", true),
            col("emb", "ARRAY<FLOAT32>(vector_length=>3)", true),
        ],
        primary_key: Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        }),
        indexes: vec![IndexDef {
            name: "docs_emb".into(),
            // The vector column, then a filter column (STORING).
            columns: vec!["emb".into(), "title".into()],
            vector: Some(vi),
            ..Default::default()
        }],
        ..Default::default()
    };
    let ddl = d
        .table_ddl(
            &docs,
            DdlParts {
                create: true,
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        ddl.contains("emb ARRAY<FLOAT32>(vector_length=>3)")
            || ddl.contains("`emb` ARRAY<FLOAT32>(vector_length=>3)"),
        "{ddl}"
    );
    assert!(ddl.contains("CREATE VECTOR INDEX `docs_emb` ON `docs` (`emb`) STORING (`title`) WHERE emb IS NOT NULL OPTIONS (distance_type = 'COSINE', num_leaves = 10, tree_depth = 2);"), "{ddl}");
    run(&mut s, &ddl).await;

    // 2. Rows from the insert script, vectors as JSON arrays.
    let ins = d
        .insert_script(
            &table("docs"),
            &["id".into(), "title".into(), "emb".into()],
            &[
                vec![json!(1), json!("uno"), json!([1.0, 0.0, 0.0])],
                vec![json!(2), json!("dos"), json!([0.1, 0.2, 0.3])],
                vec![json!(3), json!("tres"), json!([-1.5, 2.25, 1e-7])],
                vec![json!(4), json!("sin"), json!(null)],
            ],
        )
        .unwrap();
    run(&mut s, &ins).await;

    // 3. Read back: the column is a vector, the text exact (FLOAT32).
    let out = run(&mut s, "SELECT id, emb FROM docs ORDER BY id").await;
    let r = &out.results[0];
    let v = r.columns[1].vector.as_ref().expect("vector column");
    assert_eq!(v.element, VectorElement::F32);
    assert_eq!(r.columns[0].vector, None);
    let cells: Vec<_> = r.rows.iter().map(|row| row[1].clone()).collect();
    assert_eq!(
        cells,
        [
            json!("[1,0,0]"),
            json!("[0.1,0.2,0.3]"),
            json!("[-1.5,2.25,0.0000001]"),
            json!(null)
        ]
    );

    // 4. Describe keeps the dimension.
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == "docs").unwrap();
    let emb = t.columns.iter().find(|c| c.name == "emb").unwrap();
    assert_eq!(emb.data_type, "ARRAY<FLOAT32>(vector_length=>3)");
    let cols = s.columns(&table("docs")).await.unwrap();
    assert_eq!(
        cols.iter().find(|c| c.name == "emb").unwrap().data_type,
        "ARRAY<FLOAT32>(vector_length=>3)"
    );

    // 5. The index, with its vector settings; scripted back the same.
    let ix = t.indexes.iter().find(|i| i.name == "docs_emb").unwrap();
    let got = ix.vector.as_ref().expect("vector index");
    assert_eq!(got.metric.as_deref(), Some("cosine"));
    assert_eq!(got.params.get("lists").map(String::as_str), Some("10"));
    assert_eq!(got.params.get("tree_depth").map(String::as_str), Some("2"));
    assert_eq!(ix.columns, ["emb"]);
    assert_eq!(ix.include, ["title"]);
    let again = d
        .table_ddl(
            t,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(again.contains("CREATE VECTOR INDEX `docs_emb` ON `docs` (`emb`) STORING (`title`) WHERE emb IS NOT NULL OPTIONS (distance_type = 'COSINE', num_leaves = 10, tree_depth = 2);"), "{again}");
    // Nothing to sync against itself.
    let same = d
        .sync_script(&[dbine_driver::TableChange::Alter {
            old: t.clone(),
            new: t.clone(),
        }])
        .unwrap();
    assert!(same.statements.is_empty(), "{:?}", same.statements);

    // 6. A similarity query.
    let out = run(
        &mut s,
        "SELECT id, COSINE_DISTANCE(emb, ARRAY<FLOAT32>[1, 0, 0]) AS d FROM docs WHERE emb IS NOT NULL ORDER BY d, id LIMIT 2",
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], json!(1));
    assert_eq!(out.results[0].rows[0][1], json!(0.0));

    // 7. An edit from the grid: the vector cell as a JSON array.
    let rows = dbine_driver::vector::rows_for_scripts(
        &r.columns,
        &[vec![json!(2), json!("[0.5,0.25,-1]")]],
    );
    let up = d
        .update_script(
            &table("docs"),
            &[RowChange {
                key: vec![("id".into(), json!(2))],
                set: vec![("emb".into(), rows[0][1].clone())],
                row: Vec::new(),
            }],
        )
        .unwrap();
    run(&mut s, &up).await;
    let out = run(&mut s, "SELECT emb FROM docs WHERE id = 2").await;
    assert_eq!(out.results[0].rows[0][0], json!("[0.5,0.25,-1]"));

    // 8. Transfer: read as vector text, load into a copy; then a
    // pgvector-shaped source (text cells with a JSON array).
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: table("docs"),
            columns: Some(vec!["id".into(), "emb".into()]),
            filter: None,
        },
        sink.clone(),
    )
    .await
    .unwrap();
    let got = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(got.columns[1].type_name, "ARRAY<FLOAT32>(vector_length=>3)");
    let mut by_id = got.rows.clone();
    by_id.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    assert_eq!(by_id[0][1], Cell::Json("[1,0,0]".into()));
    assert_eq!(by_id[1][1], Cell::Json("[0.5,0.25,-1]".into()));
    let copy = TableSchema {
        name: "docs_copy".into(),
        indexes: Vec::new(),
        columns: vec![
            col("id", "INT64", false),
            col("emb", "ARRAY<FLOAT32>(vector_length=>3)", true),
        ],
        ..docs.clone()
    };
    run(
        &mut s,
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
    let spec = |name: &str| LoadSpec {
        table: table(name),
        columns: vec!["id".into(), "emb".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let n = s
        .bulk_load(
            &spec("docs_copy"),
            &got.columns,
            &mut Once(Some(got.rows.clone())),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(n, 4);
    let pg_like = vec![
        vec![Cell::Int(10), Cell::Text("[0.25,-0.5,4]".into())],
        vec![Cell::Int(11), Cell::Text("[1,2,3]".into())],
    ];
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
    let n = s
        .bulk_load(
            &spec("docs_copy"),
            &pg_cols,
            &mut Once(Some(pg_like)),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(n, 2);
    let out = run(&mut s, "SELECT id, emb FROM docs_copy ORDER BY id").await;
    let texts: Vec<_> = out.results[0].rows.iter().map(|r| r[1].clone()).collect();
    assert_eq!(
        texts,
        [
            json!("[1,0,0]"),
            json!("[0.5,0.25,-1]"),
            json!("[-1.5,2.25,0.0000001]"),
            json!(null),
            json!("[0.25,-0.5,4]"),
            json!("[1,2,3]")
        ]
    );
    eprintln!("spanner vectors: table, index, scripts, query and transfer checked");
}
