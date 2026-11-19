//! Vectors against real servers (ignored by default), each run on the
//! server its variable names:
//!
//! ```sh
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 \
//! DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//! DBINE_TEST_OPENDISTRO_URL=http://localhost:25524 \
//! DBINE_TEST_ELASTICSEARCH9_URL=http://localhost:25525 \
//!   cargo test -p dbine-driver-elasticsearch --test vectors -- --ignored --nocapture
//! ```
//!
//! Each one creates its `dbine_vec*` indices through the driver's own DDL
//! (a designer `TableSchema` with a vector column and a vector index),
//! fills them with `insert_script`, reads them back as results and as the
//! schema, searches by similarity, edits a vector with `update_script`,
//! copies the index with `read_batches` / `bulk_load`, and drops what it
//! made, first and last.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch};
use dbine_driver::{
    async_trait, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, ObjectRef, QueryOutcome,
    RowChange, Session, TableSchema, VectorElement, VectorIndex, VectorMeta,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const INDEX: &str = "dbine_vec";
const COPY: &str = "dbine_vec_copy";
const LOADED: &str = "dbine_vec_loaded";

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_elasticsearch::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{text}\n{e}"));
    out
}

async fn cleanup(s: &mut Box<dyn Session>) {
    for i in [INDEX, COPY, LOADED] {
        let mut out = QueryOutcome::default();
        let _ = s
            .execute(&format!("DELETE /{i}?ignore_unavailable=true"), 1, &mut out)
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
        kind: "index".into(),
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

/// The cells of column `name` of the first result, by `_id`.
fn cells(out: &QueryOutcome, name: &str) -> Vec<(String, Value)> {
    let r = &out.results[0];
    let at = |n: &str| r.columns.iter().position(|c| c.name == n).unwrap();
    let (id, c) = (at("_id"), at(name));
    let mut v: Vec<(String, Value)> = r
        .rows
        .iter()
        .map(|row| (row[id].as_str().unwrap().to_string(), row[c].clone()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// What each engine's vector field and index are in this test.
struct Case {
    id: &'static str,
    ty: &'static str,
    /// A second vector column: Elasticsearch's bits, OpenSearch's bytes.
    other: Option<(&'static str, VectorMeta)>,
    vector: VectorIndex,
    /// The similarity search: a request whose best hit is document 1.
    knn: &'static str,
}

fn vi(alg: &str, metric: &str, params: &[(&str, &str)]) -> VectorIndex {
    VectorIndex {
        algorithm: Some(alg.into()),
        metric: Some(metric.into()),
        params: params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

async fn exercise(case: Case, url: &str) {
    let d = driver(case.id);
    assert!(d.capabilities().vectors);
    let spec = d.designer().unwrap();
    assert!(spec.vector_indexes.is_some() && spec.indexes);
    let cfg = ConnectionConfig {
        driver: case.id.into(),
        host: url.into(),
        ..Default::default()
    };
    let mut s = d.connect(&cfg, None).await.expect("connect");
    println!("{}: {}", case.id, s.server_version().await.unwrap());
    cleanup(&mut s).await;

    // 1. The index, from the designer's model.
    let mut columns = vec![col("title", "keyword"), col("emb", case.ty)];
    if let Some((ty, _)) = &case.other {
        columns.push(col("other", ty));
    }
    let table = TableSchema {
        kind: "index".into(),
        name: INDEX.into(),
        columns,
        indexes: vec![IndexDef {
            name: "emb_idx".into(),
            columns: vec!["emb".into()],
            vector: Some(case.vector.clone()),
            ..Default::default()
        }],
        options: [("number_of_replicas".to_string(), "0".to_string())].into(),
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

    // 2. Rows with arrays (as the grid hands them over).
    let other = |i: usize| match &case.other {
        Some((_, m)) if m.element == VectorElement::Bit => json!((0..16)
            .map(|b| u8::from((b + i).is_multiple_of(3)))
            .collect::<Vec<_>>()),
        Some(_) => json!([i as i64, -(i as i64), 127]),
        None => Value::Null,
    };
    let mut names = vec!["_id".to_string(), "title".into(), "emb".into()];
    if case.other.is_some() {
        names.push("other".into());
    }
    let rows: Vec<Vec<Value>> = [
        ("1", "uno", json!([1.0, 0.0, 0.0])),
        ("2", "dos", json!([0.0, 1.0, 0.0])),
        ("3", "tres", json!([0.5, -0.25, 0.75])),
    ]
    .iter()
    .enumerate()
    .map(|(i, (id, t, e))| {
        let mut r = vec![json!(id), json!(t), e.clone()];
        if case.other.is_some() {
            r.push(other(i));
        }
        r
    })
    .collect();
    // Bit vectors take their packed bytes in a script (it has no types).
    let script_rows: Vec<Vec<Value>> = rows
        .iter()
        .map(|r| {
            let mut r = r.clone();
            if let (Some((_, m)), Some(v)) = (&case.other, r.get(3)) {
                if m.element == VectorElement::Bit {
                    let bits: Vec<f64> = v
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|b| b.as_f64().unwrap())
                        .collect();
                    let bytes = dbine_driver::vector::to_bits(&bits);
                    r[3] = json!(bytes.iter().map(|&b| b as i8).collect::<Vec<_>>());
                }
            }
            r
        })
        .collect();
    let insert = d.insert_script(&obj(INDEX), &names, &script_rows).unwrap();
    run(&mut s, &insert).await;

    // 3. Read back: vector columns marked, cells exact.
    let browse = s.browse_query(&obj(INDEX), 10);
    let out = run(&mut s, &browse).await;
    let r = &out.results[0];
    let emb = r.columns.iter().find(|c| c.name == "emb").unwrap();
    assert_eq!(
        emb.vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    assert_eq!(emb.type_name, case.ty);
    assert_eq!(
        cells(&out, "emb"),
        [
            ("1".to_string(), json!("[1,0,0]")),
            ("2".to_string(), json!("[0,1,0]")),
            ("3".to_string(), json!("[0.5,-0.25,0.75]")),
        ]
    );
    if let Some((ty, meta)) = &case.other {
        let c = r.columns.iter().find(|c| c.name == "other").unwrap();
        assert_eq!((c.type_name.as_str(), c.vector.as_ref()), (*ty, Some(meta)));
        let got = cells(&out, "other");
        for (i, (_, v)) in got.iter().enumerate() {
            assert_eq!(
                v,
                &json!(dbine_driver::vector::text(
                    other(i)
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|x| x.as_f64().unwrap())
                )),
                "other {i}"
            );
        }
    }

    // 4. Describe keeps the dimension; 5. the vector index is listed.
    let cols = s.columns(&obj(INDEX)).await.unwrap();
    assert!(
        cols.iter()
            .any(|c| c.name == "emb" && c.data_type == case.ty),
        "{cols:?}"
    );
    let schema = s.database_schema().await.unwrap();
    let got = schema.iter().find(|t| t.name == INDEX).unwrap();
    let c = got.columns.iter().find(|c| c.name == "emb").unwrap();
    assert_eq!(c.data_type, case.ty);
    let ix = got
        .indexes
        .iter()
        .find(|i| i.columns == ["emb"])
        .unwrap_or_else(|| panic!("{:?}", got.indexes));
    let read = ix.vector.clone().unwrap();
    println!("{}: read back {read:?}", case.id);
    assert_eq!(read.metric, case.vector.metric);
    assert_eq!(read.algorithm, case.vector.algorithm);
    for (k, v) in &case.vector.params {
        assert_eq!(read.params.get(k), Some(v), "{k} in {read:?}");
    }

    // 6. A similarity search.
    let out = run(&mut s, case.knn).await;
    let r = &out.results[0];
    let id = r.columns.iter().position(|c| c.name == "_id").unwrap();
    assert_eq!(r.rows[0][id], json!("1"), "{:?}", r.rows);

    // 7. An edit: the new vector reads back.
    let update = d
        .update_script(
            &obj(INDEX),
            &[RowChange {
                key: vec![("_id".into(), json!("2"))],
                set: vec![("emb".into(), json!([0.25, 0.5, -1.0]))],
                ..Default::default()
            }],
        )
        .unwrap();
    run(&mut s, &update).await;
    let out = run(&mut s, &browse).await;
    assert_eq!(cells(&out, "emb")[1].1, json!("[0.25,0.5,-1]"));

    // 8. A transfer: the index cloned from its schema, then its rows read
    // and loaded into the clone.
    let mut copy = got.clone();
    copy.name = COPY.into();
    run(&mut s, &d.table_ddl(&copy, parts).unwrap()).await;
    let sink = Arc::new(Mutex::new(Collect::default()));
    let read_spec = ReadSpec {
        table: obj(INDEX),
        columns: None,
        filter: None,
    };
    assert_eq!(s.read_batches(&read_spec, sink.clone()).await.unwrap(), 3);
    let collected = std::mem::take(&mut *sink.lock().unwrap());
    let at = collected
        .columns
        .iter()
        .position(|c| c.name == "emb")
        .unwrap();
    assert_eq!(collected.columns[at].type_name, case.ty);
    let mut emb_cells: Vec<&Cell> = collected.rows.iter().map(|r| &r[at]).collect();
    emb_cells.sort_by_key(|c| format!("{c:?}"));
    assert!(
        emb_cells.contains(&&Cell::Text("[1,0,0]".into())),
        "{emb_cells:?}"
    );
    let load = LoadSpec {
        table: obj(COPY),
        columns: collected.columns.iter().map(|c| c.name.clone()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 1000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let n = s
        .bulk_load(
            &load,
            &collected.columns,
            &mut Batches(
                vec![RowBatch {
                    rows: collected.rows.clone(),
                    bytes: 0,
                }]
                .into_iter(),
            ),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(n, 3);
    let q = s.browse_query(&obj(COPY), 10);
    let copied = run(&mut s, &q).await;
    let original = run(&mut s, &browse).await;
    assert_eq!(cells(&copied, "emb"), cells(&original, "emb"));
    if case.other.is_some() {
        assert_eq!(cells(&copied, "other"), cells(&original, "other"));
    }
    let schema = s.database_schema().await.unwrap();
    let cloned = schema.iter().find(|t| t.name == COPY).unwrap();
    assert_eq!(cloned.columns, got.columns);
    assert_eq!(
        cloned.indexes.iter().map(|i| &i.vector).collect::<Vec<_>>(),
        got.indexes.iter().map(|i| &i.vector).collect::<Vec<_>>()
    );

    // A load of vector text from another engine (pgvector sends
    // `[1,2,3]`): the field takes it as its array.
    let mut fresh = got.clone();
    fresh.name = LOADED.into();
    run(&mut s, &d.table_ddl(&fresh, parts).unwrap()).await;
    let mut row = vec![
        Cell::Text("9".into()),
        Cell::Text("from pg".into()),
        Cell::Text("[0.125,-2,3e2]".into()),
    ];
    let mut cols = vec!["_id".to_string(), "title".into(), "emb".into()];
    if let Some((_, m)) = &case.other {
        cols.push("other".into());
        row.push(Cell::Text(if m.element == VectorElement::Bit {
            "[1,0,0,0,0,0,0,1,0,1,0,0,0,0,0,0]".into()
        } else {
            "[1,2,3]".into()
        }));
    }
    let load = LoadSpec {
        table: obj(LOADED),
        columns: cols,
        ..load
    };
    s.bulk_load(
        &load,
        &[],
        &mut Batches(
            vec![RowBatch {
                rows: vec![row],
                bytes: 0,
            }]
            .into_iter(),
        ),
        &|_| {},
    )
    .await
    .unwrap();
    let q = s.browse_query(&obj(LOADED), 10);
    let out = run(&mut s, &q).await;
    assert_eq!(
        cells(&out, "emb"),
        [("9".to_string(), json!("[0.125,-2,300]"))]
    );

    cleanup(&mut s).await;
}

#[tokio::test]
#[ignore]
async fn elasticsearch_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_URL") else {
        return;
    };
    exercise(
        Case {
            id: "elasticsearch",
            ty: "dense_vector(3)",
            other: Some((
                "dense_vector(16, bit)",
                VectorMeta::dense(Some(16), VectorElement::Bit),
            )),
            vector: vi(
                "hnsw",
                "cosine",
                &[("m", "24"), ("ef_construction", "128"), ("quantization", "int8")],
            ),
            knn: "POST /dbine_vec/_search\n{\"knn\": {\"field\": \"emb\", \"query_vector\": [0.9, 0.1, 0], \"k\": 2, \"num_candidates\": 10}}",
        },
        &url,
    )
    .await;
}

/// Elasticsearch 9.2+, which leaves vectors out of `_source` unless asked.
#[tokio::test]
#[ignore]
async fn elasticsearch9_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH9_URL") else {
        return;
    };
    exercise(
        Case {
            id: "elasticsearch",
            ty: "dense_vector(3)",
            other: Some((
                "dense_vector(16, bit)",
                VectorMeta::dense(Some(16), VectorElement::Bit),
            )),
            vector: vi("hnsw", "ip", &[("m", "16"), ("ef_construction", "100")]),
            knn: "POST /dbine_vec/_search\n{\"knn\": {\"field\": \"emb\", \"query_vector\": [1, 0, 0], \"k\": 2, \"num_candidates\": 10}}",
        },
        &url,
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn opensearch_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") else {
        return;
    };
    exercise(
        Case {
            id: "opensearch",
            ty: "knn_vector(3)",
            other: Some((
                "knn_vector(3, byte)",
                VectorMeta::dense(Some(3), VectorElement::I8),
            )),
            vector: vi(
                "hnsw",
                "cosine",
                &[("engine", "lucene"), ("m", "16"), ("ef_construction", "100")],
            ),
            knn: "POST /dbine_vec/_search\n{\"query\": {\"knn\": {\"emb\": {\"vector\": [0.9, 0.1, 0], \"k\": 2}}}}",
        },
        &url,
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn opendistro_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENDISTRO_URL") else {
        return;
    };
    exercise(
        Case {
            id: "opendistro",
            ty: "knn_vector(3)",
            other: None,
            vector: vi("hnsw", "cosine", &[("m", "8"), ("ef_construction", "64")]),
            knn: "POST /dbine_vec/_search\n{\"query\": {\"knn\": {\"emb\": {\"vector\": [0.9, 0.1, 0], \"k\": 2}}}}",
        },
        &url,
    )
    .await;
}
