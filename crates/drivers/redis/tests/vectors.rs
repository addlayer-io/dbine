//! Vectors (embeddings) against real servers, the way the app uses them: a
//! hash with a vector field and its search index made from the key
//! designer's `TableSchema`, rows written with the insert and update
//! scripts, read back as search results, hash fields and transfer cells,
//! described, searched and copied; and Redis 8's vector sets.
//!
//! `cargo test -p dbine-driver-redis --test vectors -- --ignored --nocapture --test-threads=1`
//! Addresses: `DBINE_TEST_REDIS8_URL` (localhost:27203, Redis 8 with its
//! query engine and vector sets), `DBINE_TEST_VALKEY_SEARCH_URL`
//! (localhost:27204, valkey-bundle with valkey-search),
//! `DBINE_TEST_DRAGONFLY_URL` (localhost:25407).
//!
//! Search indexes live in database 0 on Redis and Dragonfly, so the test
//! uses it, with keys and indexes of its own (`dbvec:*`, `dbvec_*`), which
//! it deletes at the start and at the end, also when it fails.

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, ObjectRef, QueryOutcome,
    RowChange, Session, TableSchema, VectorElement, VectorIndex,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_redis::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

/// A session of `d`: the app has one driver, whose sessions tell its
/// scripts what they read (a search index's vector types).
async fn open(d: &Arc<dyn Driver>, url: &str, read_only: bool) -> Box<dyn Session> {
    let (host, port) = url.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: d.info().id.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        read_only,
        ..Default::default()
    };
    d.connect(&cfg, Some("db0")).await.unwrap()
}

async fn try_run(s: &mut Box<dyn Session>, text: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out)
        .await
        .map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    match try_run(s, text).await {
        Ok(o) => o,
        Err(e) => panic!("{}: {e}", &text[..text.len().min(200)]),
    }
}

fn key(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::KEY.into(),
        schema: None,
        name: name.into(),
    }
}

fn index(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::INDEX.into(),
        schema: None,
        name: name.into(),
    }
}

/// A blob as an editor argument, every byte escaped.
fn blob_arg(b: &[u8]) -> String {
    let mut s = String::from("\"");
    for x in b {
        s.push_str(&format!("\\x{x:02x}"));
    }
    s.push('"');
    s
}

fn f32_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// The vector type each server's test uses: one that isn't FLOAT32 where
/// the server has it, so the blobs are decoded by the index's type.
fn vtype(id: &str) -> &'static str {
    match id {
        "redis" => "FLOAT16",
        "dragonfly" => "FLOAT64",
        _ => "FLOAT32",
    }
}

fn encode(id: &str, v: &[f64]) -> Vec<u8> {
    match vtype(id) {
        "FLOAT16" => v
            .iter()
            .flat_map(|x| dbine_driver::vector::f32_to_f16(*x as f32).to_le_bytes())
            .collect(),
        "FLOAT64" => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        _ => v.iter().flat_map(|x| (*x as f32).to_le_bytes()).collect(),
    }
}

fn col(name: &str, ty: &str, default: &str) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        default_value: Some(default.into()),
        ..Default::default()
    }
}

/// The key designer's hash, its vector field and its search index.
fn designed(id: &str) -> TableSchema {
    let mut t = TableSchema {
        kind: kinds::KEY.into(),
        name: "dbvec:doc:1".into(),
        columns: vec![
            col("id", "string", "1"),
            col("title", "TEXT", "uno"),
            col("lang", "TAG", "es"),
            col(
                "emb",
                &format!("VECTOR(3, {})", vtype(id)),
                "[0.5, 0.25, 1]",
            ),
        ],
        indexes: vec![IndexDef {
            name: "dbvec_docs".into(),
            columns: vec!["emb".into(), "lang".into()],
            vector: Some(VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some("cosine".into()),
                params: [("m".to_string(), "12".to_string())].into(),
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    t.options.insert("type".into(), "hash".into());
    t.options.insert("index_prefix".into(), "dbvec:doc:".into());
    t
}

struct Batches(std::vec::IntoIter<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
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

async fn read(s: &mut Box<dyn Session>, name: &str, columns: Option<Vec<String>>) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: key(name),
            columns,
            filter: None,
        },
        sink.clone(),
    )
    .await
    .unwrap();
    let mut c = std::mem::take(&mut *sink.lock().unwrap());
    c.rows
        .sort_by(|a, b| format!("{:?}", a[0]).cmp(&format!("{:?}", b[0])));
    c
}

async fn load(
    s: &mut Box<dyn Session>,
    name: &str,
    columns: &[TransferColumn],
    rows: Vec<Vec<Cell>>,
) -> u64 {
    s.bulk_load(
        &LoadSpec {
            table: key(name),
            columns: columns.iter().map(|c| c.name.clone()).collect(),
            table_lock: false,
            keep_identity: false,
            commit_rows: 1000,
            commit_bytes: 1 << 20,
        },
        columns,
        &mut Batches(vec![RowBatch { rows, bytes: 0 }].into_iter()),
        &|_| {},
    )
    .await
    .unwrap()
}

/// A hash field's bytes, as the server keeps them.
async fn raw(s: &mut Box<dyn Session>, k: &str, field: &str) -> Vec<u8> {
    // `HGET` shows a blob that isn't a covered vector as `0x…`, or as
    // text when its bytes happen to be UTF-8.
    let out = run(s, &format!("HGET {k} {field}")).await;
    let cell = out.results[0].rows[0][0].as_str().unwrap().to_string();
    match cell.strip_prefix("0x") {
        Some(hex) if hex.len() % 2 == 0 && hex.bytes().all(|c| c.is_ascii_hexdigit()) => (0..hex
            .len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect(),
        _ => cell.into_bytes(),
    }
}

/// The search results' `id` → `emb` cells, waiting (up to 10 s) for the
/// index to hold `n` documents.
async fn search(
    s: &mut Box<dyn Session>,
    browse: &str,
    n: usize,
) -> (QueryOutcome, Vec<(String, String)>) {
    for _ in 0..100 {
        let out = run(s, browse).await;
        let r = &out.results[0];
        if r.rows.len() >= n {
            let id = r.columns.iter().position(|c| c.name == "id").unwrap();
            let emb = r.columns.iter().position(|c| c.name == "emb").unwrap();
            let mut v: Vec<(String, String)> = r
                .rows
                .iter()
                .map(|row| {
                    (
                        row[id].as_str().unwrap().to_string(),
                        row[emb].as_str().unwrap_or("null").to_string(),
                    )
                })
                .collect();
            v.sort();
            return (out, v);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the index never held {n} documents: {browse}");
}

const CLEANUP: &str = "FT.DROPINDEX dbvec_docs\nFT.DROPINDEX dbvec_copy\nFT.DROPINDEX dbvec_pg\n\
DEL dbvec:doc:1 dbvec:doc:2 dbvec:doc:3 dbvec:copy:1 dbvec:copy:2 dbvec:copy:3 dbvec:pg:10 dbvec:pg:11 dbvec:vs";

async fn cleanup(s: &mut Box<dyn Session>) {
    // A failed command (an index that isn't there) doesn't stop the rest.
    let mut out = QueryOutcome::default();
    for line in CLEANUP.lines() {
        let _ = s.execute(line, 10, &mut out).await;
    }
}

async fn body(id: &str, url: &str) {
    let d = driver(id);
    assert!(d.capabilities().vectors);
    let mut s = open(&d, url, false).await;
    println!("{id}: {}", s.server_version().await.unwrap());

    // 1. The key, its vector field and its search index, from the designer.
    let ddl = d
        .table_ddl(
            &designed(id),
            DdlParts {
                create: true,
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    println!("{ddl}");
    run(&mut s, &ddl).await;

    // 2. More rows with the insert script, once DBine read the index
    // (its type says how to pack the vectors).
    let cols = s.columns(&index("dbvec_docs")).await.unwrap();
    let emb = cols.iter().find(|c| c.name == "emb").unwrap();
    assert_eq!(emb.data_type, format!("VECTOR(3, {})", vtype(id)));
    let names = ["id", "title", "lang", "emb"].map(String::from).to_vec();
    let rows = vec![
        vec![json!(2), json!("dos"), json!("en"), json!([1, 0, 0])],
        vec![json!(3), json!("tres"), json!("es"), json!([0.5, -2, 3.5])],
    ];
    let ins = d.insert_script(&key("dbvec:doc"), &names, &rows).unwrap();
    run(&mut s, &ins).await;

    // 3. Read back: the search results mark the vector column and show
    // the vectors' text; so do the hash's own fields.
    let browse = s.browse_query(&index("dbvec_docs"), 50);
    println!("{browse}");
    let (out, got) = search(&mut s, &browse, 3).await;
    let c = out.results[0]
        .columns
        .iter()
        .find(|c| c.name == "emb")
        .unwrap();
    let meta = c.vector.as_ref().expect("a vector column");
    assert_eq!(meta.dims, Some(3));
    assert_eq!(
        meta.element,
        match vtype(id) {
            "FLOAT16" => VectorElement::F16,
            "FLOAT64" => VectorElement::F64,
            _ => VectorElement::F32,
        }
    );
    assert_eq!(c.type_name, format!("VECTOR(3, {})", vtype(id)));
    assert_eq!(
        got,
        [
            ("dbvec:doc:1".to_string(), "[0.5,0.25,1]".to_string()),
            ("dbvec:doc:2".into(), "[1,0,0]".into()),
            ("dbvec:doc:3".into(), "[0.5,-2,3.5]".into()),
        ]
    );
    let out = run(&mut s, "HGETALL dbvec:doc:1").await;
    let row = out.results[0]
        .rows
        .iter()
        .find(|r| r[0] == json!("emb"))
        .unwrap();
    assert_eq!(row[1], json!("[0.5,0.25,1]"));
    let out = run(&mut s, "HGET dbvec:doc:3 emb").await;
    assert!(out.results[0].columns[0].vector.is_some());
    assert_eq!(out.results[0].rows[0][0], json!("[0.5,-2,3.5]"));

    // 4. Describe keeps the dimension and the type.
    let def = s.definition(&index("dbvec_docs")).await.unwrap().unwrap();
    println!("{def}");
    assert!(
        def.contains(&format!("TYPE {} DIM 3 DISTANCE_METRIC COSINE", vtype(id))),
        "{def}"
    );

    // 5. The index is listed, with its vector index's settings, and
    // scripts back as the server has it.
    let objs = s.list_objects().await.unwrap();
    assert!(objs
        .iter()
        .any(|o| o.kind == kinds::INDEX && o.name == "dbvec_docs"));
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == "dbvec_docs").unwrap();
    let ix = t.indexes.iter().find(|i| i.name == "emb").unwrap();
    let v = ix.vector.as_ref().expect("vector index");
    assert_eq!(v.algorithm.as_deref(), Some("hnsw"));
    assert_eq!(v.metric.as_deref(), Some("cosine"));
    assert_eq!(v.params["m"], "12");
    let again = d
        .table_ddl(
            t,
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(again.trim_end(), def);
    let same = d
        .sync_script(&[dbine_driver::TableChange::Alter {
            old: t.clone(),
            new: t.clone(),
        }])
        .unwrap();
    assert!(same.statements.is_empty(), "{:?}", same.statements);

    // 6. A similarity search (KNN): the nearest is the vector itself.
    let q = encode(id, &[0.5, 0.25, 1.0]);
    let knn = format!(
        "FT.SEARCH dbvec_docs \"*=>[KNN 2 @emb $q]\" PARAMS 2 q {} RETURN 1 __emb_score DIALECT 2",
        blob_arg(&q)
    );
    let out = run(&mut s, &knn).await;
    let r = &out.results[0];
    assert_eq!(r.rows.len(), 2, "{:?}", r.rows);
    let score = r
        .columns
        .iter()
        .position(|c| c.name == "__emb_score")
        .unwrap();
    let nearest = r
        .rows
        .iter()
        .min_by(|a, b| {
            let f = |v: &Value| {
                v.as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(9.0)
            };
            f(&a[score]).partial_cmp(&f(&b[score])).unwrap()
        })
        .unwrap();
    assert_eq!(nearest[0], json!("dbvec:doc:1"), "{:?}", r.rows);

    // 7. Edits: a search result's vector, and a hash field typed as text.
    let upd = d
        .update_script(
            &index("dbvec_docs"),
            &[RowChange {
                key: vec![("id".into(), json!("dbvec:doc:2"))],
                set: vec![("emb".into(), json!([0, 1, 0]))],
                ..Default::default()
            }],
        )
        .unwrap();
    println!("{upd}");
    run(&mut s, &upd).await;
    let out = run(&mut s, "HGET dbvec:doc:2 emb").await;
    assert_eq!(out.results[0].rows[0][0], json!("[0,1,0]"));
    let upd = d
        .update_script(
            &key("dbvec:doc:3"),
            &[RowChange {
                key: vec![("field".into(), json!("emb"))],
                set: vec![("value".into(), json!("[1, 1, 1]"))],
                ..Default::default()
            }],
        )
        .unwrap();
    run(&mut s, &upd).await;
    let out = run(&mut s, "HGET dbvec:doc:3 emb").await;
    assert_eq!(out.results[0].rows[0][0], json!("[1,1,1]"));

    // 8. Transfers. The read gives the vectors' text and their type…
    let got = read(&mut s, "dbvec:doc", None).await;
    let at = got.columns.iter().position(|c| c.name == "emb").unwrap();
    assert_eq!(
        got.columns[at].type_name,
        format!("VECTOR(3, {})", vtype(id))
    );
    let vectors: Vec<&Cell> = got.rows.iter().map(|r| &r[at]).collect();
    assert_eq!(
        vectors,
        [
            &Cell::Text("[0.5,0.25,1]".into()),
            &Cell::Text("[0,1,0]".into()),
            &Cell::Text("[1,1,1]".into())
        ]
    );
    // …a load into a prefix no index covers packs them by that type…
    let picked = vec!["id".to_string(), "emb".into()];
    let mut two = read(&mut s, "dbvec:doc", Some(picked)).await;
    two.columns[1].type_name = format!("VECTOR(3, {})", vtype(id));
    assert_eq!(
        load(&mut s, "dbvec:copy", &two.columns, two.rows.clone()).await,
        3
    );
    assert_eq!(
        raw(&mut s, "dbvec:copy:2", "emb").await,
        encode(id, &[0.0, 1.0, 0.0])
    );
    // …and an index over it reads them back the same.
    run(
        &mut s,
        &format!(
            "FT.CREATE dbvec_copy ON HASH PREFIX 1 dbvec:copy: SCHEMA emb VECTOR FLAT 6 TYPE {} DIM 3 DISTANCE_METRIC L2",
            vtype(id)
        ),
    )
    .await;
    assert_eq!(
        read(&mut s, "dbvec:copy", Some(vec!["id".into(), "emb".into()]))
            .await
            .rows,
        two.rows
    );
    // pgvector's read (its text, `vector(3)`): FLOAT32 blobs, what an
    // index over them takes.
    let pg_cols = vec![
        TransferColumn {
            name: "id".into(),
            type_name: "int4".into(),
            nullable: false,
        },
        TransferColumn {
            name: "emb".into(),
            type_name: "vector(3)".into(),
            nullable: true,
        },
    ];
    let pg_rows = vec![
        vec![Cell::Int(10), Cell::Text("[1,2.5,-3]".into())],
        vec![Cell::Int(11), Cell::Text("[0.33333334,0,1e-07]".into())],
    ];
    assert_eq!(load(&mut s, "dbvec:pg", &pg_cols, pg_rows).await, 2);
    assert_eq!(
        raw(&mut s, "dbvec:pg:10", "emb").await,
        f32_blob(&[1.0, 2.5, -3.0])
    );
    run(
        &mut s,
        "FT.CREATE dbvec_pg ON HASH PREFIX 1 dbvec:pg: SCHEMA emb VECTOR FLAT 6 TYPE FLOAT32 DIM 3 DISTANCE_METRIC L2",
    )
    .await;
    let back = read(&mut s, "dbvec:pg", Some(vec!["id".into(), "emb".into()])).await;
    assert_eq!(
        back.rows,
        [
            vec![Cell::Text("10".into()), Cell::Text("[1,2.5,-3]".into())],
            vec![
                Cell::Text("11".into()),
                Cell::Text("[0.33333334,0,0.0000001]".into())
            ],
        ]
    );

    // A read-only connection runs the searches, not the writes.
    let mut ro = open(&d, url, true).await;
    run(&mut ro, &knn).await;
    assert!(try_run(&mut ro, "FT.DROPINDEX dbvec_docs").await.is_err());

    if id == "redis" {
        vector_sets(&d, &mut s, &mut ro).await;
    }
}

/// Redis 8's vector sets: made from the designer, browsed with their
/// vectors and attributes, searched, edited, read and guarded.
async fn vector_sets(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, ro: &mut Box<dyn Session>) {
    let mut t = TableSchema {
        kind: kinds::KEY.into(),
        name: "dbvec:vs".into(),
        columns: vec![
            col("a", "VECTOR(3, FLOAT32)", "[0.1, 0.2, 0.3]"),
            col("b", "VECTOR(3, FLOAT32)", "[1, 0, 0]"),
        ],
        ..Default::default()
    };
    t.options.insert("type".into(), "vectorset".into());
    t.options.insert("quantization".into(), "NOQUANT".into());
    let ddl = d
        .table_ddl(
            &t,
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
    run(s, &ddl).await;
    run(s, "VSETATTR dbvec:vs a \"{\\\"lang\\\":\\\"es\\\"}\"").await;
    let page = s
        .scan_keys(&dbine_driver::KeyScan {
            pattern: "dbvec:vs".into(),
            cursor: None,
            count: 10,
            key_type: Some("vectorset".into()),
        })
        .await
        .unwrap();
    assert_eq!(page.keys[0].key_type.as_deref(), Some("vectorset"));
    let cols = s.columns(&key("dbvec:vs")).await.unwrap();
    assert_eq!(cols[1].data_type, "VECTOR(3, FLOAT32)");
    let def = s.definition(&key("dbvec:vs")).await.unwrap().unwrap();
    assert!(
        def.contains("VDIM      3") && def.contains("QUANT     f32"),
        "{def}"
    );
    let browse = s.browse_query(&key("dbvec:vs"), 50);
    assert!(browse.starts_with("VRANGE"), "{browse}");
    let out = run(s, &browse).await;
    let r = &out.results[0];
    assert_eq!(
        r.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["element", "vector", "attributes"]
    );
    assert_eq!(r.columns[1].vector.as_ref().unwrap().dims, Some(3));
    assert_eq!(
        r.rows,
        [
            vec![
                json!("a"),
                json!("[0.1,0.2,0.3]"),
                json!("{\"lang\":\"es\"}")
            ],
            vec![json!("b"), json!("[1,0,0]"), Value::Null],
        ]
    );
    let out = run(s, "VEMB dbvec:vs a").await;
    assert_eq!(out.results[0].rows[0][0], json!("[0.1,0.2,0.3]"));
    assert!(out.results[0].columns[0].vector.is_some());
    let out = run(s, "VSIM dbvec:vs VALUES 3 0.1 0.2 0.3 WITHSCORES COUNT 1").await;
    assert_eq!(out.results[0].rows[0][0], json!("a"));
    assert_eq!(out.results[0].columns[1].name, "score");
    // Edits: the vector (VADD with the set's quantization) and attributes.
    let upd = d
        .update_script(
            &key("dbvec:vs"),
            &[RowChange {
                key: vec![("element".into(), json!("a"))],
                set: vec![
                    ("vector".into(), json!([0, 1, 0])),
                    ("attributes".into(), json!("{\"lang\":\"en\"}")),
                ],
                ..Default::default()
            }],
        )
        .unwrap();
    assert!(upd.contains("NOQUANT"), "{upd}");
    run(s, &upd).await;
    let out = run(s, "VEMB dbvec:vs a").await;
    assert_eq!(out.results[0].rows[0][0], json!("[0,1,0]"));
    let out = run(s, "VGETATTR dbvec:vs a").await;
    assert_eq!(out.results[0].rows[0][0], json!("{\"lang\":\"en\"}"));
    // Read as a transfer.
    let got = read(s, "dbvec:vs", None).await;
    assert_eq!(got.columns[1].type_name, "VECTOR(3, FLOAT32)");
    assert_eq!(
        got.rows,
        [
            vec![
                Cell::Text("a".into()),
                Cell::Text("[0,1,0]".into()),
                Cell::Json("{\"lang\":\"en\"}".into())
            ],
            vec![
                Cell::Text("b".into()),
                Cell::Text("[1,0,0]".into()),
                Cell::Null
            ],
        ]
    );
    // Delete an element.
    let del = d
        .delete_script(&key("dbvec:vs"), &[vec![("element".into(), json!("b"))]])
        .unwrap();
    run(s, &del).await;
    let out = run(s, "VCARD dbvec:vs").await;
    assert_eq!(out.results[0].rows[0][0], json!(1));
    // Read-only: reads yes, writes no.
    run(ro, "VSIM dbvec:vs ELE a WITHSCORES").await;
    run(ro, "VINFO dbvec:vs").await;
    assert!(try_run(ro, "VADD dbvec:vs VALUES 3 1 1 1 c").await.is_err());
}

async fn vectors_on(id: &'static str, env: &str, default: &str) {
    let url = std::env::var(env).unwrap_or_else(|_| default.into());
    let mut admin = open(&driver(id), &url, false).await;
    cleanup(&mut admin).await;
    // In a task of its own: a failure there still lets the cleanup run.
    let u = url.clone();
    let r = tokio::spawn(async move { body(id, &u).await }).await;
    cleanup(&mut admin).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test]
#[ignore]
async fn redis8_vectors() {
    vectors_on("redis", "DBINE_TEST_REDIS8_URL", "localhost:27203").await;
}

#[tokio::test]
#[ignore]
async fn valkey_search_vectors() {
    vectors_on("valkey", "DBINE_TEST_VALKEY_SEARCH_URL", "localhost:27204").await;
}

#[tokio::test]
#[ignore]
async fn dragonfly_vectors() {
    vectors_on("dragonfly", "DBINE_TEST_DRAGONFLY_URL", "localhost:25407").await;
}
