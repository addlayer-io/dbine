//! Vectors against a real Solr (ignored by default):
//!
//! ```sh
//! DBINE_TEST_SOLRCLOUD_URL=http://localhost:25523 \
//!   cargo test -p dbine-driver-solr --test vectors -- --ignored --nocapture
//! ```
//!
//! The test creates its `dbine_vec*` collections through the driver's own
//! DDL (a designer `TableSchema` with vector columns and a vector index),
//! fills them with `insert_script`, reads them back as results and as the
//! schema, searches with `{!knn}`, edits a vector with `update_script`,
//! copies the collection with `read_batches` / `bulk_load`, and drops what
//! it made, first and last. On a standalone server
//! (`DBINE_TEST_SOLR_URL`) the cores need their own configsets, as in
//! `integration.rs` (`dbine_vec`, `dbine_vec_copy`).

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch};
use dbine_driver::{
    async_trait, ColumnDef, ConnectionConfig, DdlParts, IndexDef, ObjectRef, QueryOutcome,
    RowChange, Session, TableSchema, VectorElement, VectorIndex, VectorMeta,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const COLL: &str = "dbine_vec";
const COPY: &str = "dbine_vec_copy";

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{text}\n{e}"));
    out
}

async fn cleanup(s: &mut Box<dyn Session>) {
    for c in [COLL, COPY] {
        let mut out = QueryOutcome::default();
        let _ = s
            .execute(&format!("DELETE /solr/{c}?if_exists=true"), 1, &mut out)
            .await;
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

fn obj(name: &str) -> ObjectRef {
    ObjectRef {
        kind: "collection".into(),
        schema: None,
        name: name.into(),
    }
}

struct Batches(std::vec::IntoIter<RowBatch>);

#[async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

#[derive(Default)]
struct Collect {
    columns: Vec<dbine_driver::transfer::TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[dbine_driver::transfer::TransferColumn]) -> std::io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        self.rows.extend(batch.rows);
        Ok(())
    }
}

/// The cells of column `name` of the first result, by `id`.
fn cells(out: &QueryOutcome, name: &str) -> Vec<(String, Value)> {
    let r = &out.results[0];
    let at = |n: &str| r.columns.iter().position(|c| c.name == n).unwrap();
    let (id, c) = (at("id"), at(name));
    let mut v: Vec<(String, Value)> = r
        .rows
        .iter()
        .map(|row| (row[id].as_str().unwrap().to_string(), row[c].clone()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

async fn exercise(url: &str) {
    let d = dbine_driver_solr::drivers().remove(0);
    assert!(d.capabilities().vectors);
    assert!(d.designer().unwrap().vector_indexes.is_some());
    let cfg = ConnectionConfig {
        driver: "solr".into(),
        host: url.into(),
        ..Default::default()
    };
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let version = s.server_version().await.unwrap();
    println!("{version}");
    // A standalone server's cores share their configset's schema: each one
    // gets its own (see the module's doc).
    let standalone = !version.contains("SolrCloud");
    let config = |name: &str| {
        standalone
            .then(|| ("configSet".to_string(), name.to_string()))
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    cleanup(&mut s).await;

    // 1. The collection, from the designer's model.
    let vector = VectorIndex {
        algorithm: Some("hnsw".into()),
        metric: Some("cosine".into()),
        params: [
            ("m".to_string(), "24".to_string()),
            ("ef_construction".to_string(), "128".to_string()),
        ]
        .into(),
    };
    let table = TableSchema {
        kind: "collection".into(),
        name: COLL.into(),
        columns: vec![
            col("id", "string"),
            col("title", "string"),
            col("emb", "knn_vector(3)"),
            col("b", "knn_vector(4, BYTE)"),
        ],
        indexes: vec![IndexDef {
            name: "emb_idx".into(),
            columns: vec!["emb".into()],
            vector: Some(vector.clone()),
            ..Default::default()
        }],
        options: config(COLL),
        ..Default::default()
    };
    let parts = DdlParts {
        drop: true,
        if_exists: true,
        create: true,
        ..Default::default()
    };
    let ddl = d.table_ddl(&table, parts).unwrap();
    println!("{ddl}");
    run(&mut s, &ddl).await;

    // 2. Rows with arrays.
    let names: Vec<String> = ["id", "title", "emb", "b"].map(String::from).to_vec();
    let rows = vec![
        vec![
            json!("1"),
            json!("uno"),
            json!([1.0, 0.0, 0.0]),
            json!([1, 2, 3, 4]),
        ],
        vec![
            json!("2"),
            json!("dos"),
            json!([0.0, 1.0, 0.0]),
            json!([-1, -2, -3, -128]),
        ],
        vec![
            json!("3"),
            json!("tres"),
            json!([0.5, -0.25, 0.1]),
            json!([0, 0, 0, 127]),
        ],
    ];
    run(&mut s, &d.insert_script(&obj(COLL), &names, &rows).unwrap()).await;

    // 3. Read back: vector columns marked, cells exact.
    let browse = s.browse_query(&obj(COLL), 10);
    let out = run(&mut s, &browse).await;
    let r = &out.results[0];
    let emb = r.columns.iter().find(|c| c.name == "emb").unwrap();
    assert_eq!(emb.type_name, "knn_vector(3)");
    assert_eq!(
        emb.vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    let b = r.columns.iter().find(|c| c.name == "b").unwrap();
    assert_eq!(
        (b.type_name.as_str(), b.vector.clone()),
        (
            "knn_vector(4, BYTE)",
            Some(VectorMeta::dense(Some(4), VectorElement::I8))
        )
    );
    assert_eq!(
        cells(&out, "emb"),
        [
            ("1".to_string(), json!("[1,0,0]")),
            ("2".to_string(), json!("[0,1,0]")),
            ("3".to_string(), json!("[0.5,-0.25,0.1]")),
        ]
    );
    assert_eq!(cells(&out, "b")[1].1, json!("[-1,-2,-3,-128]"));

    // 4. Describe keeps the dimension; 5. the vector index is listed.
    let cols = s.columns(&obj(COLL)).await.unwrap();
    assert!(
        cols.iter()
            .any(|c| c.name == "emb" && c.data_type == "knn_vector(3)"),
        "{cols:?}"
    );
    let schema = s.database_schema().await.unwrap();
    let got = schema.iter().find(|t| t.name == COLL).unwrap();
    let emb = got.columns.iter().find(|c| c.name == "emb").unwrap();
    assert_eq!(emb.data_type, "knn_vector(3)");
    assert_eq!(
        got.columns
            .iter()
            .find(|c| c.name == "b")
            .unwrap()
            .data_type,
        "knn_vector(4, BYTE)"
    );
    let ix = got
        .indexes
        .iter()
        .find(|i| i.columns == ["emb"])
        .unwrap_or_else(|| panic!("{:?}", got.indexes));
    assert_eq!(ix.vector.as_ref(), Some(&vector));
    assert!(got.indexes.iter().any(|i| i.columns == ["b"]));

    // 6. A similarity search.
    let out = run(
        &mut s,
        &format!("GET /solr/{COLL}/select?q={{!knn%20f=emb%20topK=2}}[0.9,0.1,0]&fl=id,emb,score"),
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], json!("1"), "{:?}", out.results);
    assert!(out.results[0].columns[1].vector.is_some());

    // 7. An edit.
    let update = d
        .update_script(
            &obj(COLL),
            &[RowChange {
                key: vec![("id".into(), json!("2"))],
                set: vec![("emb".into(), json!([0.25, 0.5, -1.0]))],
                ..Default::default()
            }],
        )
        .unwrap();
    run(&mut s, &update).await;
    let out = run(&mut s, &browse).await;
    assert_eq!(cells(&out, "emb")[1].1, json!("[0.25,0.5,-1]"));

    // 8. A transfer into a clone made from the schema read back.
    let mut copy = got.clone();
    copy.name = COPY.into();
    copy.options.remove("configSet");
    copy.options.extend(config(COPY));
    run(&mut s, &d.table_ddl(&copy, parts).unwrap()).await;
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec {
        table: obj(COLL),
        columns: None,
        filter: None,
    };
    assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap(), 3);
    let collected = std::mem::take(&mut *sink.lock().unwrap());
    let at = collected
        .columns
        .iter()
        .position(|c| c.name == "emb")
        .unwrap();
    assert_eq!(collected.columns[at].type_name, "knn_vector(3)");
    assert!(collected
        .rows
        .iter()
        .any(|r| r[at] == Cell::Text("[0.5,-0.25,0.1]".into())));
    let columns: Vec<String> = collected
        .columns
        .iter()
        .map(|c| c.name.clone())
        .filter(|n| n != "_version_")
        .collect();
    let keep: Vec<usize> = columns
        .iter()
        .map(|n| collected.columns.iter().position(|c| c.name == *n).unwrap())
        .collect();
    let rows: Vec<Vec<Cell>> = collected
        .rows
        .iter()
        .map(|r| keep.iter().map(|&i| r[i].clone()).collect())
        .collect();
    let load = LoadSpec {
        table: obj(COPY),
        columns: columns.clone(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 1000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let n = s
        .bulk_load(
            &load,
            &[],
            &mut Batches(vec![RowBatch { rows, bytes: 0 }].into_iter()),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(n, 3);
    let q = s.browse_query(&obj(COPY), 10);
    let copied = run(&mut s, &q).await;
    let original = run(&mut s, &browse).await;
    assert_eq!(cells(&copied, "emb"), cells(&original, "emb"));
    assert_eq!(cells(&copied, "b"), cells(&original, "b"));
    let schema = s.database_schema().await.unwrap();
    let cloned = schema.iter().find(|t| t.name == COPY).unwrap();
    assert_eq!(
        cloned.indexes.iter().map(|i| &i.vector).collect::<Vec<_>>(),
        got.indexes.iter().map(|i| &i.vector).collect::<Vec<_>>()
    );

    // Vector text from another engine (pgvector sends `[1,2,3]`), and one
    // that wouldn't fit a float32: refused.
    let one = |id: &str, emb: &str| {
        vec![
            Cell::Text(id.into()),
            Cell::Text("de pg".into()),
            Cell::Text(emb.into()),
            Cell::Text("[1,1,1,1]".into()),
        ]
    };
    let load = LoadSpec {
        columns: vec!["id".into(), "title".into(), "emb".into(), "b".into()],
        ..load
    };
    s.bulk_load(
        &load,
        &[],
        &mut Batches(
            vec![RowBatch {
                rows: vec![one("9", "[0.125,-2,300]")],
                bytes: 0,
            }]
            .into_iter(),
        ),
        &|_| {},
    )
    .await
    .unwrap();
    let e = s
        .bulk_load(
            &load,
            &[],
            &mut Batches(
                vec![RowBatch {
                    rows: vec![one("10", "[0.30000001,0,0]")],
                    bytes: 0,
                }]
                .into_iter(),
            ),
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("float de 32 bits"), "{e}");
    let q = s.browse_query(&obj(COPY), 10);
    let out = run(&mut s, &q).await;
    assert!(cells(&out, "emb").contains(&("9".to_string(), json!("[0.125,-2,300]"))));

    cleanup(&mut s).await;
}

#[tokio::test]
#[ignore]
async fn solrcloud_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLRCLOUD_URL") else {
        return;
    };
    exercise(&url).await;
}

#[tokio::test]
#[ignore]
async fn solr_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLR_URL") else {
        return;
    };
    exercise(&url).await;
}
