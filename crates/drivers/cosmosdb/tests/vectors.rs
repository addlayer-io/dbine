//! Vectors against the emulator (vnext-preview: diskANN is the only
//! vector index type it takes):
//!
//! ```sh
//! docker start dbine-test-cosmosdb
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25213 \
//!   cargo test -p dbine-driver-cosmosdb --test vectors -- --ignored --nocapture
//! ```
//!
//! The test's database is dropped first and at the end.

use dbine_driver::async_trait;
use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    vector, ColumnDef, ConnectionConfig, DdlParts, IndexDef, ObjectRef, QueryOutcome, ResultColumn,
    RowChange, Session, TableChange, TableSchema, VectorElement, VectorIndex, VectorMeta,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

const EMULATOR_KEY: &str =
    "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";
const DB: &str = "dbine_vectors";

fn coll(name: &str) -> ObjectRef {
    ObjectRef {
        kind: "collection".into(),
        schema: None,
        name: name.into(),
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"));
    out
}

fn col(name: &str, t: &str) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: t.into(),
        nullable: true,
        ..Default::default()
    }
}

/// The designer's container: a vector field and its vector index.
fn designed(name: &str, metric: &str) -> TableSchema {
    let mut t = TableSchema {
        kind: "collection".into(),
        name: name.into(),
        columns: vec![
            col("id", "string"),
            col("cat", "string"),
            col("emb", "vector(3)"),
        ],
        indexes: vec![IndexDef {
            name: "emb_vix".into(),
            columns: vec!["emb".into()],
            vector: Some(VectorIndex {
                algorithm: Some("diskANN".into()),
                metric: Some(metric.into()),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    t.options.insert("partition_key".into(), "/cat".into());
    t
}

const ALL: DdlParts = DdlParts {
    drop: false,
    if_exists: false,
    create: true,
    indexes: true,
    foreign_keys: false,
};

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.rows.extend(batch.rows);
        Ok(())
    }
}

struct Batches(VecDeque<RowBatch>);

#[async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.pop_front()
    }
}

#[tokio::test]
#[ignore]
async fn cosmos_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_COSMOSDB_URL") else {
        return;
    };
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    let d = dbine_driver_cosmosdb::drivers().remove(0);
    assert!(d.capabilities().vectors);
    let spec = d.designer().unwrap();
    assert!(spec.data_types.contains(&"vector(1536)"));
    assert_eq!(spec.vector_indexes.unwrap().algorithms[0], "diskANN");
    let mut c = ConnectionConfig {
        driver: "cosmosdb".into(),
        host: url,
        trust_server_certificate: true,
        ..Default::default()
    };
    c.options.insert("account_key".into(), key);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let _ = admin.drop_database(DB).await;
    admin.create_database(DB).await.unwrap();
    let mut s = d.connect(&c, Some(DB)).await.unwrap();

    // 1. The container with its embedding policy and vector index.
    let ddl = d.table_ddl(&designed("docs", "cosine"), ALL).unwrap();
    assert!(
        ddl.contains("vectorEmbeddingPolicy") && ddl.contains("\"/emb/*\""),
        "{ddl}"
    );
    run(&mut s, &ddl).await;

    // 2. insert_script with vectors as arrays.
    let cols = vec!["id".to_string(), "cat".into(), "emb".into()];
    let result_cols = vec![
        ResultColumn::new("id", ""),
        ResultColumn::new("cat", ""),
        ResultColumn::new("emb", "vector(3)")
            .with_vector(VectorMeta::dense(Some(3), VectorElement::F32)),
    ];
    let rows = vector::rows_for_scripts(
        &result_cols,
        &[
            vec![json!("1"), json!("a"), json!("[1,0,0]")],
            vec![json!("2"), json!("b"), json!("[0,1,0]")],
            vec![json!("3"), json!("a"), json!("[0.9,0.1,0]")],
        ],
    );
    run(
        &mut s,
        &d.insert_script(&coll("docs"), &cols, &rows).unwrap(),
    )
    .await;

    // 3. Read back: the vector column is marked, the cells exact.
    let out = run(
        &mut s,
        "-- container: docs\nSELECT c.id, c.cat, c.emb FROM c ORDER BY c.id",
    )
    .await;
    let r = &out.results[0];
    let at = r.columns.iter().position(|c| c.name == "emb").unwrap();
    assert_eq!(
        r.columns[at].vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    assert_eq!(r.columns[at].type_name, "vector(3)");
    let cells: Vec<&Value> = r.rows.iter().map(|row| &row[at]).collect();
    assert_eq!(
        cells,
        [&json!("[1,0,0]"), &json!("[0,1,0]"), &json!("[0.9,0.1,0]")]
    );

    // 4. Describe keeps the dimension; 5. the index has its vector.
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == "docs").unwrap();
    assert_eq!(
        t.columns
            .iter()
            .find(|c| c.name == "emb")
            .unwrap()
            .data_type,
        "vector(3)"
    );
    let ix = t
        .indexes
        .iter()
        .find(|i| i.vector.is_some())
        .expect("vector index");
    assert_eq!(ix.columns, ["emb"]);
    let v = ix.vector.as_ref().unwrap();
    assert_eq!(v.algorithm.as_deref(), Some("diskANN"));
    // The emulator doesn't report the embedding policy (Azure does): the
    // distance is unknown there and the dimension comes from the items.
    let reported = s
        .definition(&coll("docs"))
        .await
        .unwrap()
        .unwrap()
        .contains("vectorEmbeddingPolicy");
    assert_eq!(v.metric.as_deref(), reported.then_some("cosine"));
    // Its own DDL gives the same container; compared with itself, nothing.
    let again = d.table_ddl(t, ALL).unwrap();
    assert!(
        again.contains("\"distanceFunction\": \"cosine\"") && again.contains("\"dimensions\": 3"),
        "{again}"
    );
    let same = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: t.clone(),
        }])
        .unwrap();
    assert!(
        same.warnings.iter().all(|w| !w.contains("vector")),
        "{:?}",
        same.warnings
    );
    // Another distance is reported (Cosmos DB fixes it at creation).
    let mut other = t.clone();
    other
        .indexes
        .iter_mut()
        .find(|i| i.vector.is_some())
        .unwrap()
        .vector
        .as_mut()
        .unwrap()
        .metric = Some("l2".into());
    let w = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: other,
        }])
        .unwrap()
        .warnings;
    assert!(
        w.iter().any(|w| w.contains("vectorEmbeddingPolicy")),
        "{w:?}"
    );

    // 6. A similarity query.
    let out = run(
        &mut s,
        "-- container: docs\nSELECT TOP 2 c.id, VectorDistance(c.emb, [1, 0, 0]) AS score FROM c ORDER BY VectorDistance(c.emb, [1, 0, 0])",
    )
    .await;
    let ids: Vec<&Value> = out.results[0].rows.iter().map(|r| &r[0]).collect();
    assert_eq!(ids, [&json!("1"), &json!("3")]);

    // 7. update_script writes the array.
    let upd = d
        .update_script(
            &coll("docs"),
            &[RowChange {
                key: vec![("id".into(), json!("1"))],
                set: vec![("emb".into(), json!([0.0, 0.25, 1.0]))],
                ..Default::default()
            }],
        )
        .unwrap();
    run(&mut s, &upd).await;
    let out = run(
        &mut s,
        "-- container: docs\nSELECT c.emb FROM c WHERE c.id = \"1\"",
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], json!("[0,0.25,1]"));

    // 8. Transfers: the read gives the vectors' text; a pgvector-like
    // source loads as arrays into a container the clone would make.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let read = ReadSpec {
        table: coll("docs"),
        columns: None,
        filter: None,
    };
    s.read_batches(&read, sink.clone()).await.unwrap();
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let at = got.columns.iter().position(|c| c.name == "emb").unwrap();
    // With the embedding policy reported, its dimension too.
    let want = if reported { "vector(3)" } else { "vector" };
    assert_eq!(got.columns[at].type_name, want);
    let id = got.columns.iter().position(|c| c.name == "id").unwrap();
    let three = got
        .rows
        .iter()
        .find(|r| r[id] == Cell::Text("3".into()))
        .unwrap();
    assert_eq!(three[at], Cell::Text("[0.9,0.1,0]".into()));
    let mut target = designed("from_pg", "l2");
    target.options.insert("partition_key".into(), "/id".into());
    run(&mut s, &d.table_ddl(&target, ALL).unwrap()).await;
    let pg_cols = vec![
        TransferColumn {
            name: "id".into(),
            type_name: "text".into(),
            nullable: false,
        },
        TransferColumn {
            name: "emb".into(),
            type_name: "vector(3)".into(),
            nullable: true,
        },
    ];
    let load = LoadSpec {
        table: coll("from_pg"),
        columns: vec!["id".into(), "emb".into()],
        table_lock: false,
        keep_identity: true,
        commit_rows: 1000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let batch = RowBatch {
        rows: vec![vec![
            Cell::Text("p1".into()),
            Cell::Text("[0.1,0.2,0.3]".into()),
        ]],
        bytes: 0,
    };
    let n = s
        .bulk_load(&load, &pg_cols, &mut Batches(vec![batch].into()), &|_| {})
        .await
        .unwrap();
    assert_eq!(n, 1);
    let out = run(
        &mut s,
        "-- container: from_pg\nSELECT VALUE IS_ARRAY(c.emb) FROM c",
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], json!(true));
    let out = run(&mut s, "-- container: from_pg\nSELECT TOP 1 c.id FROM c ORDER BY VectorDistance(c.emb, [0.1, 0.2, 0.3])").await;
    assert_eq!(out.results[0].rows[0][0], json!("p1"));

    let _ = admin.drop_database(DB).await;
}
