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
//! fills them with `insert_script_typed` (bit vectors one 0 / 1 per
//! dimension, as the grid hands them over, typed as a describe gives them)
//! sent straight over HTTP, as a script copied out of DBine runs, reads them
//! back as results, by id and as the schema, searches by similarity, edits
//! vectors with `update_script_typed` (over HTTP) and `update_script`
//! (through the session, which packs bits by the mapping),
//! copies the index with `read_batches` / `bulk_load`, and drops what it
//! made first, and last through a guard that also runs when it fails
//! partway. `opensearch_byte_and_binary_vectors` creates byte and binary
//! vector indexes naming no engine and adds such fields to a k-NN index by
//! schema sync.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch};
use dbine_driver::{
    async_trait, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, ObjectRef, QueryOutcome,
    ResultColumn, RowChange, Session, TableSchema, VectorElement, VectorIndex, VectorMeta,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const INDEX: &str = "dbine_vec";
const COPY: &str = "dbine_vec_copy";
const LOADED: &str = "dbine_vec_loaded";
const BYTES: &str = "dbine_vec_bytes";

/// The tests share their index names (and two of them a server): one at a
/// time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Runs a script's requests straight over HTTP, as a script copied out of
/// DBine would run: without the session's own rewriting (it packs bit
/// vectors by the mapping).
async fn raw(url: &str, script: &str) {
    use dbine_driver_elasticsearch::console::{self, Command};
    let client = reqwest::Client::new();
    for c in console::parse(script).unwrap() {
        let Command::Http(r) = c else {
            panic!("SQL in {script}")
        };
        let method = reqwest::Method::from_bytes(r.method.as_bytes()).unwrap();
        let mut req = client.request(method, format!("{}{}", url.trim_end_matches('/'), r.path));
        if let Some(b) = &r.body {
            let ct = if r.is_ndjson() {
                "application/x-ndjson"
            } else {
                "application/json"
            };
            req = req.header("content-type", ct).body(format!("{b}\n"));
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        assert!(status.is_success(), "{script}\n{status}: {text}");
        assert!(!text.contains("\"errors\":true"), "{script}\n{text}");
    }
}

/// The index's columns typed as a describe gives them (the app's typed
/// scripts from a table).
async fn described(s: &mut Box<dyn Session>, index: &str, names: &[String]) -> Vec<ResultColumn> {
    let cols = s.columns(&obj(index)).await.unwrap();
    names
        .iter()
        .map(|n| {
            let ty = cols
                .iter()
                .find(|c| &c.name == n)
                .map(|c| c.data_type.clone())
                .unwrap_or_default();
            ResultColumn::new(n.clone(), ty)
        })
        .collect()
}

async fn cleanup(s: &mut Box<dyn Session>) {
    for i in [INDEX, COPY, LOADED, BYTES] {
        let mut out = QueryOutcome::default();
        let _ = s
            .execute(&format!("DELETE /{i}?ignore_unavailable=true"), 1, &mut out)
            .await;
    }
}

fn config(id: &str, url: &str) -> ConnectionConfig {
    ConnectionConfig {
        driver: id.into(),
        host: url.into(),
        ..Default::default()
    }
}

/// Drops the test's indices when the test ends, whether it passed or
/// failed partway (in a thread of its own: the test's runtime may be
/// unwinding).
struct Guard {
    id: &'static str,
    url: String,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let (id, url) = (self.id, self.url.clone());
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                if let Ok(mut s) = driver(id).connect(&config(id, &url), None).await {
                    cleanup(&mut s).await;
                }
            });
        })
        .join();
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
    let _serial = SERIAL.lock().await;
    let d = driver(case.id);
    assert!(d.capabilities().vectors);
    let spec = d.designer().unwrap();
    assert!(spec.vector_indexes.is_some() && spec.indexes);
    let mut s = d
        .connect(&config(case.id, url), None)
        .await
        .expect("connect");
    let version = s.server_version().await.unwrap();
    println!("{}: {version}", case.id);
    cleanup(&mut s).await;
    let _guard = Guard {
        id: case.id,
        url: url.into(),
    };

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
        // Bytes as a float source gives them: the typed script writes
        // them as integers.
        Some(_) => json!([i as f64, 0.0 - i as f64, 127.0]),
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
    // Bit vectors as results show them, one 0 / 1 per dimension: the
    // typed script packs them, so it runs as it is, over plain HTTP.
    let typed = described(&mut s, INDEX, &names).await;
    let insert = d.insert_script_typed(&obj(INDEX), &typed, &rows).unwrap();
    println!("{insert}");
    raw(url, &insert).await;

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
    // The second vector has no vector index: Elasticsearch writes it
    // `index: false`, so none is read back; a k-NN index of OpenSearch
    // indexes every knn_vector, with the engine its bytes need.
    let other_ix = got.indexes.iter().find(|i| i.columns == ["other"]);
    match case.id {
        "elasticsearch" if case.other.is_some() => {
            assert!(other_ix.is_none(), "{:?}", got.indexes)
        }
        "opensearch" => assert_eq!(
            other_ix
                .and_then(|i| i.vector.as_ref())
                .and_then(|v| v.params.get("engine"))
                .map(String::as_str),
            Some("lucene"),
            "{:?}",
            got.indexes
        ),
        _ => {}
    }
    // A schema sync of the index read back against itself, or against the
    // designer's model it was made from: nothing to do.
    let mut model = got.clone();
    model.indexes = table.indexes.clone();
    for new in [got.clone(), model] {
        let same = d
            .sync_script(&[dbine_driver::TableChange::Alter {
                old: got.clone(),
                new,
            }])
            .unwrap();
        assert!(
            same.statements.is_empty() && same.warnings.is_empty(),
            "{same:?}"
        );
    }

    // A document by id: its vectors typed as in a search. Elasticsearch
    // 9.2+ leaves them out unless asked, and says so.
    let es92 =
        case.id == "elasticsearch" && dbine_driver::functions::version_at_least(&version, "9.2");
    for path in ["_doc", "_source"] {
        if es92 {
            let out = run(&mut s, &format!("GET /{INDEX}/{path}/1")).await;
            assert!(
                out.messages
                    .iter()
                    .any(|m| m.contains("_source_exclude_vectors=false")),
                "{:?}",
                out.messages
            );
        }
        let ask = if es92 {
            "?_source_exclude_vectors=false"
        } else {
            ""
        };
        let out = run(&mut s, &format!("GET /{INDEX}/{path}/1{ask}")).await;
        assert!(
            !out.messages.iter().any(|m| m.contains("exclude_vectors")),
            "{:?}",
            out.messages
        );
        let r = &out.results[0];
        let at = r.columns.iter().position(|c| c.name == "emb").unwrap();
        assert!(r.columns[at].vector.is_some(), "{:?}", r.columns);
        assert_eq!(r.rows[0][at], json!("[1,0,0]"));
        if let Some((_, m)) = &case.other {
            let at = r.columns.iter().position(|c| c.name == "other").unwrap();
            assert_eq!(r.columns[at].vector.as_ref(), Some(m));
            assert_eq!(
                r.rows[0][at],
                json!(dbine_driver::vector::text(
                    other(0)
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|x| x.as_f64().unwrap())
                ))
            );
        }
    }

    // Elasticsearch 9.2+ leaves vectors out of `_source`: a search that
    // doesn't ask for them says how; one that filters `_source` doesn't.
    if es92 {
        let out = run(
            &mut s,
            &format!("POST /{INDEX}/_search\n{{\"query\": {{\"match_all\": {{}}}}}}"),
        )
        .await;
        assert!(
            out.messages.iter().any(|m| m.contains("exclude_vectors")),
            "{:?}",
            out.messages
        );
        let out = run(
            &mut s,
            &format!(
                "POST /{INDEX}/_search\n{{\"_source\": [\"title\"], \"query\": {{\"match_all\": {{}}}}}}"
            ),
        )
        .await;
        assert!(
            !out.messages.iter().any(|m| m.contains("exclude_vectors")),
            "{:?}",
            out.messages
        );
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
    // A bit vector edited as the grid shows it: typed (packed in the
    // script, run over HTTP), then untyped (packed by the session).
    if let Some((_, m)) = &case.other {
        if m.element == VectorElement::Bit {
            let change = |bits: Value| {
                vec![RowChange {
                    key: vec![("_id".into(), json!("3"))],
                    set: vec![("other".into(), bits)],
                    ..Default::default()
                }]
            };
            let typed = described(&mut s, INDEX, &["other".to_string()]).await;
            let update = d
                .update_script_typed(
                    &obj(INDEX),
                    &change(json!([0, 1, 1, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 0, 0, 1])),
                    &typed,
                )
                .unwrap();
            assert!(update.contains("[96,-15]"), "{update}");
            raw(url, &update).await;
            let out = run(&mut s, &browse).await;
            assert_eq!(
                cells(&out, "other")[2].1,
                json!("[0,1,1,0,0,0,0,0,1,1,1,1,0,0,0,1]")
            );
            let update = d
                .update_script(
                    &obj(INDEX),
                    &change(json!([1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1])),
                )
                .unwrap();
            run(&mut s, &update).await;
            let out = run(&mut s, &browse).await;
            assert_eq!(
                cells(&out, "other")[2].1,
                json!("[1,1,0,0,0,0,0,0,0,0,0,0,0,0,1,1]")
            );
        }
    }

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
            // l2: the test's vectors aren't unit length (dot_product).
            vector: vi("hnsw", "l2", &[("m", "16"), ("ef_construction", "100")]),
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

/// OpenSearch float vector indexes that name no engine, by distance, as
/// each version's default engine takes them: L2 everywhere; cosine
/// everywhere, on 2.18 through Lucene, which the session picks there
/// (Faiss, the default from 2.18, has cosine from 2.19), up to 1536
/// dimensions; L1, which DBine gives NMSLIB (the only engine with it),
/// until 3.0, where DBine refuses it before the server does, saying what to
/// choose. Each one also reads back as made and syncs to nothing.
#[tokio::test]
#[ignore]
async fn opensearch_default_engines() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let d = driver("opensearch");
    let mut s = d
        .connect(&config("opensearch", &url), None)
        .await
        .expect("connect");
    let version = s.server_version().await.unwrap();
    println!("{version}");
    cleanup(&mut s).await;
    let _guard = Guard {
        id: "opensearch",
        url: url.clone(),
    };
    let at_least = |v: &str| dbine_driver::functions::version_at_least(&version, v);
    let on_2_18 = at_least("2.18") && !at_least("2.19");
    for (metric, dims, takes) in [
        ("l2", 3, true),
        ("cosine", 3, true),
        ("cosine", 1536, true),
        ("l1", 3, !at_least("3.0")),
    ] {
        let table = TableSchema {
            kind: "index".into(),
            name: BYTES.into(),
            columns: vec![
                col("title", "keyword"),
                col("emb", &format!("knn_vector({dims})")),
            ],
            indexes: vec![IndexDef {
                name: "emb_idx".into(),
                columns: vec!["emb".into()],
                vector: Some(VectorIndex {
                    algorithm: Some("hnsw".into()),
                    metric: Some(metric.into()),
                    ..Default::default()
                }),
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
        let mut out = QueryOutcome::default();
        let made = s.execute(&ddl, 10, &mut out).await;
        println!("{metric}({dims}): {made:?} {:?}", out.messages);
        assert_eq!(
            made.is_ok(),
            takes,
            "{metric}({dims}) on {version}: {made:?}\n{ddl}"
        );
        if let Err(e) = &made {
            // DBine says why, and what to choose, before the server refuses.
            let e = e.to_string();
            assert!(e.contains("NMSLIB") && e.contains("Faiss o Lucene"), "{e}");
        }
        let lucene = on_2_18 && metric == "cosine";
        assert_eq!(
            out.messages.iter().any(|m| m.contains("Lucene")),
            lucene,
            "{metric} on {version}: {:?}",
            out.messages
        );
        if takes {
            let schema = s.database_schema().await.unwrap();
            let got = schema.iter().find(|t| t.name == BYTES).unwrap();
            let read = got.indexes[0].vector.clone().unwrap();
            println!("{metric}({dims}): read back {read:?}");
            assert_eq!(read.metric.as_deref(), Some(metric));
            assert_eq!(
                read.params.get("engine").map(String::as_str) == Some("lucene"),
                lucene,
                "{metric} on {version}: {read:?}"
            );
            let mut model = got.clone();
            model.indexes = table.indexes.clone();
            let same = d
                .sync_script(&[dbine_driver::TableChange::Alter {
                    old: got.clone(),
                    new: model,
                }])
                .unwrap();
            assert!(
                same.statements.is_empty() && same.warnings.is_empty(),
                "{metric}: {same:?}"
            );
        }
        cleanup(&mut s).await;
    }
}

/// OpenSearch byte and binary vectors whose vector index names no engine
/// (the designer's default): created, filled as the grid hands them over,
/// searched, and more of them added to the k-NN index by schema sync.
#[tokio::test]
#[ignore]
async fn opensearch_byte_and_binary_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let d = driver("opensearch");
    let mut s = d
        .connect(&config("opensearch", &url), None)
        .await
        .expect("connect");
    println!("{}", s.server_version().await.unwrap());
    cleanup(&mut s).await;
    let _guard = Guard {
        id: "opensearch",
        url: url.clone(),
    };
    let index = |col: &str, vi: VectorIndex| IndexDef {
        name: format!("{col}_idx"),
        columns: vec![col.into()],
        vector: Some(vi),
        ..Default::default()
    };
    let table = TableSchema {
        kind: "index".into(),
        name: BYTES.into(),
        columns: vec![
            col("title", "keyword"),
            col("b", "knn_vector(4, byte)"),
            col("bits", "knn_vector(16, binary)"),
        ],
        indexes: vec![
            index("b", vi("hnsw", "cosine", &[("m", "16")])),
            index(
                "bits",
                VectorIndex {
                    algorithm: Some("hnsw".into()),
                    ..Default::default()
                },
            ),
        ],
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
    let names: Vec<String> = ["_id", "title", "b", "bits"].map(String::from).to_vec();
    let bits = |i: usize| -> Value {
        json!((0..16)
            .map(|b| u8::from((b + i).is_multiple_of(4)))
            .collect::<Vec<_>>())
    };
    let rows: Vec<Vec<Value>> = (0..3)
        .map(|i| {
            vec![
                json!(format!("{}", i + 1)),
                json!(format!("t{i}")),
                json!([1.0 + i as f64, -2.0, 3.0, 127.0]),
                bits(i),
            ]
        })
        .collect();
    // Typed as a describe gives them: bits packed and bytes as integers in
    // the script itself, which runs over plain HTTP.
    let typed = described(&mut s, BYTES, &names).await;
    let insert = d.insert_script_typed(&obj(BYTES), &typed, &rows).unwrap();
    println!("{insert}");
    assert!(insert.contains(r#""b":[1,-2,3,127]"#), "{insert}");
    raw(&url, &insert).await;
    let browse = s.browse_query(&obj(BYTES), 10);
    let out = run(&mut s, &browse).await;
    assert_eq!(cells(&out, "b")[0].1, json!("[1,-2,3,127]"));
    assert_eq!(
        cells(&out, "bits")[1].1,
        json!(dbine_driver::vector::text(
            bits(1)
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_f64().unwrap())
        ))
    );
    // Searched by each: the closest is document 1 (bits 1000 1000 1000 1000
    // packed are -120, -120).
    for q in [
        r#"{"query": {"knn": {"b": {"vector": [1, -2, 3, 127], "k": 1}}}}"#,
        r#"{"query": {"knn": {"bits": {"vector": [-120, -120], "k": 1}}}}"#,
    ] {
        let out = run(&mut s, &format!("POST /{BYTES}/_search\n{q}")).await;
        let r = &out.results[0];
        let id = r.columns.iter().position(|c| c.name == "_id").unwrap();
        assert_eq!(r.rows[0][id], json!("1"), "{q}: {:?}", r.rows);
    }
    let engine = |t: &TableSchema, c: &str| {
        t.indexes
            .iter()
            .find(|i| i.columns == [c])
            .and_then(|i| i.vector.as_ref())
            .map(|v| (v.params.get("engine").cloned(), v.metric.clone()))
    };
    let schema = s.database_schema().await.unwrap();
    let got = schema.iter().find(|t| t.name == BYTES).unwrap().clone();
    assert_eq!(
        engine(&got, "b"),
        Some((Some("lucene".into()), Some("cosine".into())))
    );
    assert_eq!(
        engine(&got, "bits"),
        Some((Some("faiss".into()), Some("hamming".into())))
    );

    // More of them by schema sync: the update gives them their engine.
    let mut more = got.clone();
    more.columns.push(col("b2", "knn_vector(4, byte)"));
    more.columns.push(col("bits2", "knn_vector(8, binary)"));
    let sync = d
        .sync_script(&[dbine_driver::TableChange::Alter {
            old: got.clone(),
            new: more,
        }])
        .unwrap();
    println!("{sync:?}");
    for st in &sync.statements {
        run(&mut s, st).await;
    }
    let names: Vec<String> = ["_id", "b2", "bits2"].map(String::from).to_vec();
    let rows = vec![vec![
        json!("4"),
        json!([0, 0, 1, 1]),
        json!([1, 0, 0, 0, 0, 0, 0, 1]),
    ]];
    run(
        &mut s,
        &d.insert_script(&obj(BYTES), &names, &rows).unwrap(),
    )
    .await;
    let out = run(&mut s, &format!("GET /{BYTES}/_doc/4")).await;
    let r = &out.results[0];
    let at = r.columns.iter().position(|c| c.name == "bits2").unwrap();
    assert_eq!(r.rows[0][at], json!("[1,0,0,0,0,0,0,1]"));
    let schema = s.database_schema().await.unwrap();
    let after = schema.iter().find(|t| t.name == BYTES).unwrap();
    assert_eq!(engine(after, "b2").and_then(|e| e.0), Some("lucene".into()));
    assert_eq!(
        engine(after, "bits2").and_then(|e| e.0),
        Some("faiss".into())
    );

    cleanup(&mut s).await;
}

/// Elasticsearch 9.x quantizations and elements DBine writes as given:
/// a BBQ HNSW index with `rescore_vector` (9.1+ for floats from 64
/// dimensions), an int8 `flat` index, and a `bfloat16` vector (9.3+): made
/// from the designer, filled by a typed script, read back with their
/// element, their indexes listed as the designer wrote them, a sync
/// against the model with nothing to do, and searched.
#[tokio::test]
#[ignore]
async fn elasticsearch9_quantized_and_bfloat16_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH9_URL") else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let d = driver("elasticsearch");
    let mut s = d
        .connect(&config("elasticsearch", &url), None)
        .await
        .expect("connect");
    let version = s.server_version().await.unwrap();
    println!("{version}");
    cleanup(&mut s).await;
    let _guard = Guard {
        id: "elasticsearch",
        url: url.clone(),
    };
    let bf16 = dbine_driver::functions::version_at_least(&version, "9.3");
    let index = |col: &str, vi: VectorIndex| IndexDef {
        name: format!("{col}_idx"),
        columns: vec![col.into()],
        vector: Some(vi),
        ..Default::default()
    };
    let mut columns = vec![
        col("title", "keyword"),
        col("q", "dense_vector(64)"),
        col("f", "dense_vector(4)"),
    ];
    let mut indexes = vec![
        index(
            "q",
            vi(
                "hnsw",
                "l2",
                &[
                    ("quantization", "bbq"),
                    ("m", "16"),
                    ("ef_construction", "100"),
                    ("rescore_vector", r#"{"oversample":3.0}"#),
                ],
            ),
        ),
        index("f", vi("flat", "l2", &[("quantization", "int8")])),
    ];
    if bf16 {
        columns.push(col("h", "dense_vector(4, bfloat16)"));
        // By L2: a cosine index keeps the normalized bfloat16 vector and its
        // length, so 9.2+ (which rebuilds vectors from the index) gives
        // back `[1.0009766, …]` for `[1, …]`.
        indexes.push(index("h", vi("hnsw", "l2", &[("quantization", "none")])));
    }
    let table = TableSchema {
        kind: "index".into(),
        name: INDEX.into(),
        columns,
        indexes,
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

    // Rows typed as a describe gives the columns; values bfloat16 keeps.
    let mut names: Vec<String> = ["_id", "title", "q", "f"].map(String::from).to_vec();
    if bf16 {
        names.push("h".into());
    }
    let q = |i: usize| -> Vec<f64> { (0..64).map(|k| ((k + i) % 5) as f64 - 2.0).collect() };
    let rows: Vec<Vec<Value>> = (0..3)
        .map(|i| {
            let mut r = vec![
                json!((i + 1).to_string()),
                json!(format!("t{i}")),
                json!(q(i)),
                json!([i as f64, 0.5, -0.25, 1.0]),
            ];
            if bf16 {
                r.push(json!([1.0, i as f64, 0.5, -2.0]));
            }
            r
        })
        .collect();
    let typed = described(&mut s, INDEX, &names).await;
    run(
        &mut s,
        &d.insert_script_typed(&obj(INDEX), &typed, &rows).unwrap(),
    )
    .await;
    let browse = s.browse_query(&obj(INDEX), 10);
    let out = run(&mut s, &browse).await;
    let r = &out.results[0];
    let meta = |n: &str| {
        r.columns
            .iter()
            .find(|c| c.name == n)
            .unwrap()
            .vector
            .clone()
    };
    assert_eq!(
        meta("q"),
        Some(VectorMeta::dense(Some(64), VectorElement::F32))
    );
    assert_eq!(cells(&out, "f")[2].1, json!("[2,0.5,-0.25,1]"));
    assert_eq!(
        cells(&out, "q")[1].1,
        json!(dbine_driver::vector::text(q(1)))
    );
    if bf16 {
        assert_eq!(
            meta("h"),
            Some(VectorMeta::dense(Some(4), VectorElement::Bf16))
        );
        assert_eq!(cells(&out, "h")[1].1, json!("[1,1,0.5,-2]"));
    }
    let schema = s.database_schema().await.unwrap();
    let got = schema.iter().find(|t| t.name == INDEX).unwrap();
    for ix in &table.indexes {
        let col = &ix.columns[0];
        let c = got.columns.iter().find(|c| &c.name == col).unwrap();
        let want_ty = &table
            .columns
            .iter()
            .find(|c| &c.name == col)
            .unwrap()
            .data_type;
        assert_eq!(&c.data_type, want_ty);
        let read = got
            .indexes
            .iter()
            .find(|i| &i.columns[0] == col)
            .and_then(|i| i.vector.clone())
            .unwrap_or_else(|| panic!("{col}: {:?}", got.indexes));
        let want = ix.vector.as_ref().unwrap();
        println!("{col}: read back {read:?}");
        assert_eq!(read.algorithm, want.algorithm, "{col}");
        assert_eq!(read.metric, want.metric, "{col}");
        for (k, v) in &want.params {
            let got_v = read.params.get(k).map(|x| x.replace(' ', ""));
            assert_eq!(got_v.as_deref(), Some(v.as_str()), "{col}: {k} in {read:?}");
        }
    }
    let mut model = got.clone();
    model.indexes = table.indexes.clone();
    let same = d
        .sync_script(&[dbine_driver::TableChange::Alter {
            old: got.clone(),
            new: model,
        }])
        .unwrap();
    assert!(
        same.statements.is_empty() && same.warnings.is_empty(),
        "{same:?}"
    );
    // Searched: the closest to document 2's vector is document 2.
    let out = run(
        &mut s,
        &format!(
            "POST /{INDEX}/_search\n{{\"knn\": {{\"field\": \"q\", \"query_vector\": {}, \"k\": 1, \"num_candidates\": 10}}}}",
            dbine_driver::vector::text(q(1))
        ),
    )
    .await;
    let r = &out.results[0];
    let id = r.columns.iter().position(|c| c.name == "_id").unwrap();
    assert_eq!(r.rows[0][id], json!("2"), "{:?}", r.rows);
    cleanup(&mut s).await;
}

/// Elasticsearch 7, whose dense_vector has no ANN index (nor `index`): a
/// vector field without a vector index is created, filled, read back and
/// described; one with a vector index is refused by the server.
#[tokio::test]
#[ignore]
async fn elasticsearch7_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH7_URL") else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let d = driver("elasticsearch");
    let mut s = d
        .connect(&config("elasticsearch", &url), None)
        .await
        .expect("connect");
    println!("{}", s.server_version().await.unwrap());
    cleanup(&mut s).await;
    let _guard = Guard {
        id: "elasticsearch",
        url: url.clone(),
    };
    let mut table = TableSchema {
        kind: "index".into(),
        name: INDEX.into(),
        columns: vec![col("title", "keyword"), col("emb", "dense_vector(3)")],
        options: [("number_of_replicas".to_string(), "0".to_string())].into(),
        ..Default::default()
    };
    let parts = DdlParts {
        drop: true,
        if_exists: true,
        create: true,
        ..Default::default()
    };
    run(&mut s, &d.table_ddl(&table, parts).unwrap()).await;
    let names: Vec<String> = ["_id", "title", "emb"].map(String::from).to_vec();
    let rows = vec![vec![json!("1"), json!("uno"), json!([0.5, -1.0, 2.0])]];
    run(
        &mut s,
        &d.insert_script(&obj(INDEX), &names, &rows).unwrap(),
    )
    .await;
    let browse = s.browse_query(&obj(INDEX), 10);
    let out = run(&mut s, &browse).await;
    let emb = out.results[0]
        .columns
        .iter()
        .find(|c| c.name == "emb")
        .unwrap();
    assert_eq!(
        emb.vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    assert_eq!(cells(&out, "emb"), [("1".to_string(), json!("[0.5,-1,2]"))]);
    let schema = s.database_schema().await.unwrap();
    let got = schema.iter().find(|t| t.name == INDEX).unwrap();
    let c = got.columns.iter().find(|c| c.name == "emb").unwrap();
    assert_eq!(
        (c.data_type.as_str(), got.indexes.is_empty()),
        ("dense_vector(3)", true)
    );
    // A vector index: Elasticsearch 7 has none. 7.17 takes the mapping
    // without effect (older 7.x refuse it): either way DBine says so, and
    // describe lists none.
    table.indexes.push(IndexDef {
        name: "emb_idx".into(),
        columns: vec!["emb".into()],
        vector: Some(vi("hnsw", "cosine", &[])),
        ..Default::default()
    });
    let mut out = QueryOutcome::default();
    let e = s
        .execute(&d.table_ddl(&table, parts).unwrap(), 10, &mut out)
        .await;
    assert!(
        out.messages
            .iter()
            .any(|m| m.contains("emb") && m.contains("ANN")),
        "{e:?} {:?}",
        out.messages
    );
    if e.is_ok() && out.error.is_none() {
        let schema = s.database_schema().await.unwrap();
        let got = schema.iter().find(|t| t.name == INDEX).unwrap();
        assert!(got.indexes.is_empty(), "{:?}", got.indexes);
    }
    cleanup(&mut s).await;
}

/// Elasticsearch 8.18+ `rank_vectors` (several vectors per document, an
/// Enterprise feature: a trial license does): typed with its dimension in
/// the designer and describe, its cells the JSON array of arrays, not a
/// vector column. Skipped on a server whose license lacks it.
#[tokio::test]
#[ignore]
async fn elasticsearch_rank_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH9_URL") else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let d = driver("elasticsearch");
    let mut s = d
        .connect(&config("elasticsearch", &url), None)
        .await
        .expect("connect");
    cleanup(&mut s).await;
    let _guard = Guard {
        id: "elasticsearch",
        url: url.clone(),
    };
    let table = TableSchema {
        kind: "index".into(),
        name: INDEX.into(),
        columns: vec![col("title", "keyword"), col("r", "rank_vectors(3)")],
        options: [("number_of_replicas".to_string(), "0".to_string())].into(),
        ..Default::default()
    };
    let parts = DdlParts {
        drop: true,
        if_exists: true,
        create: true,
        ..Default::default()
    };
    let mut out = QueryOutcome::default();
    if let Err(e) = s
        .execute(&d.table_ddl(&table, parts).unwrap(), 10, &mut out)
        .await
    {
        assert!(e.to_string().contains("license"), "{e}");
        println!("rank_vectors needs an Enterprise (or trial) license: {e}");
        return;
    }
    let names: Vec<String> = ["_id", "title", "r"].map(String::from).to_vec();
    let rows = vec![vec![
        json!("1"),
        json!("uno"),
        json!([[1.0, 0.0, 0.0], [0.5, 0.5, 0.0]]),
    ]];
    run(
        &mut s,
        &d.insert_script(&obj(INDEX), &names, &rows).unwrap(),
    )
    .await;
    let browse = s.browse_query(&obj(INDEX), 10);
    let out = run(&mut s, &browse).await;
    let r = out.results[0]
        .columns
        .iter()
        .find(|c| c.name == "r")
        .unwrap();
    assert_eq!(r.vector, None);
    assert_eq!(
        cells(&out, "r"),
        [("1".to_string(), json!("[[1.0,0.0,0.0],[0.5,0.5,0.0]]"))]
    );
    let schema = s.database_schema().await.unwrap();
    let got = schema.iter().find(|t| t.name == INDEX).unwrap();
    let c = got.columns.iter().find(|c| c.name == "r").unwrap();
    assert_eq!(
        (c.data_type.as_str(), got.indexes.is_empty()),
        ("rank_vectors(3)", true)
    );
    // Searched by MaxSim.
    let out = run(
        &mut s,
        &format!("POST /{INDEX}/_search\n{{\"query\": {{\"script_score\": {{\"query\": {{\"match_all\": {{}}}}, \"script\": {{\"source\": \"maxSimDotProduct(params.q, 'r')\", \"params\": {{\"q\": [[1, 0, 0]]}}}}}}}}}}"),
    )
    .await;
    assert_eq!(out.results[0].rows.len(), 1);
    cleanup(&mut s).await;
}
