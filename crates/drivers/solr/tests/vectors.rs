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
//! schema (a vector of a dynamic field too), searches with `{!knn}`, edits
//! a vector with `update_script`, copies the collection with
//! `read_batches` / `bulk_load`, drops a vector index by schema sync, and
//! drops what it made first, and last through a guard that also runs when
//! it fails partway. On Solr 9.5+ a column has 1536 dimensions.
//!
//! On a standalone server (`DBINE_TEST_SOLR_URL`) the cores need their own
//! configsets, as in `integration.rs` (`dbine_vec`, `dbine_vec_copy`), and
//! share their managed schema with them: the cleanup takes the test's
//! fields and field types out of it before dropping each core (creating
//! the core first when a run before left only the configset).

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

/// Fields the test adds to its collections' schemas.
const FIELDS: &[&str] = &["title", "emb", "b", "big", "x_v"];

async fn cleanup(s: &mut Box<dyn Session>, standalone: bool) {
    for c in [COLL, COPY] {
        let mut out = QueryOutcome::default();
        if standalone {
            // The core's configset keeps the test's fields and field types
            // when the core goes: take them out first (its core made again
            // if a run before dropped it already).
            let _ = s
                .execute(
                    &format!("PUT /solr/{c}?if_not_exists=true\n{{\"configSet\": \"{c}\"}}"),
                    1,
                    &mut out,
                )
                .await;
            let _ = s
                .execute(
                    &format!(
                        "POST /solr/{c}/schema\n{{\"delete-dynamic-field\": {{\"name\": \"*_v\"}}}}"
                    ),
                    1,
                    &mut out,
                )
                .await;
            for f in FIELDS {
                let _ = s
                    .execute(
                        &format!(
                            "POST /solr/{c}/schema\n{{\"delete-field\": {{\"name\": \"{f}\"}}}}"
                        ),
                        1,
                        &mut out,
                    )
                    .await;
            }
            let mut types = QueryOutcome::default();
            let _ = s
                .execute(
                    &format!("GET /solr/{c}/schema/fieldtypes"),
                    1000,
                    &mut types,
                )
                .await;
            let names: Vec<String> = types
                .results
                .iter()
                .flat_map(|r| {
                    let at = r.columns.iter().position(|x| x.name == "name");
                    r.rows
                        .iter()
                        .filter_map(move |row| row.get(at?)?.as_str().map(str::to_string))
                })
                .filter(|n| n.starts_with("knn_vector_"))
                .collect();
            for t in names {
                let _ = s
                    .execute(
                        &format!(
                            "POST /solr/{c}/schema\n{{\"delete-field-type\": {{\"name\": \"{t}\"}}}}"
                        ),
                        1,
                        &mut out,
                    )
                    .await;
            }
        }
        let _ = s
            .execute(&format!("DELETE /solr/{c}?if_exists=true"), 1, &mut out)
            .await;
    }
}

fn config(url: &str) -> ConnectionConfig {
    ConnectionConfig {
        driver: "solr".into(),
        host: url.into(),
        ..Default::default()
    }
}

/// Drops what the test made when it ends, whether it passed or failed
/// partway (in a thread of its own: the test's runtime may be unwinding).
struct Guard {
    url: String,
    standalone: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let (url, standalone) = (self.url.clone(), self.standalone);
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let d = dbine_driver_solr::drivers().remove(0);
                if let Ok(mut s) = d.connect(&config(&url), None).await {
                    cleanup(&mut s, standalone).await;
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
    let mut s = d.connect(&config(url), None).await.expect("connect");
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
    cleanup(&mut s, standalone).await;
    let _guard = Guard {
        url: url.into(),
        standalone,
    };
    // More than 1024 dimensions: Solr 9.5+ (SchemaCodecFactory by default).
    let big = dbine_driver::functions::version_at_least(&version, "9.5");

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
        columns: [
            col("id", "string"),
            col("title", "string"),
            col("emb", "knn_vector(3)"),
            col("b", "knn_vector(4, BYTE)"),
        ]
        .into_iter()
        .chain(big.then(|| col("big", "knn_vector(1536)")))
        .collect(),
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
    // 1536 dimensions, set on document 1 as an edit.
    let big_vector: Vec<f64> = (0..1536).map(|i| f64::from(i % 7) - 3.0).collect();
    if big {
        let set = d
            .update_script(
                &obj(COLL),
                &[RowChange {
                    key: vec![("id".into(), json!("1"))],
                    set: vec![("big".into(), json!(big_vector))],
                    ..Default::default()
                }],
            )
            .unwrap();
        run(&mut s, &set).await;
    }

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
    // A vector of a dynamic field (`*_v`, of emb's field type).
    let emb_type = emb.options.get("fieldType").unwrap().clone();
    run(
        &mut s,
        &format!(
            "POST /solr/{COLL}/schema\n{{\"add-dynamic-field\": {{\"name\": \"*_v\", \"type\": \"{emb_type}\", \"indexed\": false}}}}"
        ),
    )
    .await;
    run(
        &mut s,
        &format!(
            "POST /solr/{COLL}/update?commit=true\n[{{\"id\": \"7\", \"x_v\": [0.5, 0.25, 2]}}]"
        ),
    )
    .await;
    let out = run(&mut s, &format!("GET /solr/{COLL}/select?q=id:7")).await;
    let r = &out.results[0];
    let x = r.columns.iter().find(|c| c.name == "x_v").unwrap();
    assert_eq!(
        x.vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32)),
        "{:?}",
        r.columns
    );
    assert_eq!(
        cells(&out, "x_v"),
        [("7".to_string(), json!("[0.5,0.25,2]"))]
    );
    run(
        &mut s,
        &format!("POST /solr/{COLL}/update?commit=true\n{{\"delete\": [\"7\"]}}"),
    )
    .await;
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
    // b has no vector index: indexed: false, so none is read back.
    assert!(
        !got.indexes.iter().any(|i| i.columns == ["b"]),
        "{:?}",
        got.indexes
    );
    // A schema sync against itself or the designer's model: nothing to do.
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
    if big {
        let out = run(&mut s, &format!("GET /solr/{COLL}/select?q=id:1&fl=id,big")).await;
        assert_eq!(
            cells(&out, "big")[0].1,
            json!(dbine_driver::vector::text(big_vector.iter().copied()))
        );
    }

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

    // The copy's vector index dropped by schema sync: emb isn't indexed
    // any more, and its field type stays as it was.
    let mut dropped = cloned.clone();
    dropped.indexes.retain(|i| i.columns != ["emb"]);
    let sync = d
        .sync_script(&[dbine_driver::TableChange::Alter {
            old: cloned.clone(),
            new: dropped,
        }])
        .unwrap();
    println!("{sync:?}");
    assert!(!sync.statements.iter().any(|x| x.contains("field-type")));
    for st in &sync.statements {
        run(&mut s, st).await;
    }
    let schema = s.database_schema().await.unwrap();
    let after = schema.iter().find(|t| t.name == COPY).unwrap();
    assert!(
        !after.indexes.iter().any(|i| i.columns == ["emb"]),
        "{:?}",
        after.indexes
    );

    cleanup(&mut s, standalone).await;
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
