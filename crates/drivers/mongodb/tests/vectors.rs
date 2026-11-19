//! Vectors against real servers:
//!
//! ```sh
//! docker start dbine-test-atlas      # mongodb-atlas-local, 27205 (Search + Vector Search)
//! docker start dbine-test-mongodb    # mongo:7, 25201, root/secret (no search)
//! docker start dbine-test-ferretdb   # FerretDB 2, 25203, root/secret
//! DBINE_TEST_ATLAS_URL='mongodb://localhost:27205/?directConnection=true' \
//! DBINE_TEST_MONGODB_URL='mongodb://root:secret@localhost:25201/?authSource=admin' \
//! DBINE_TEST_FERRETDB_URL='mongodb://root:secret@localhost:25203/' \
//!   cargo test -p dbine-driver-mongodb --test vectors -- --ignored --test-threads=1
//! ```
//!
//! Each test drops what it creates first and at the end.

use dbine_driver::async_trait;
use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    vector, ColumnDef, ConnectionConfig, DdlParts, IndexDef, ObjectRef, QueryOutcome, RowChange,
    Session, TableChange, TableSchema, VectorElement, VectorIndex, VectorMeta,
};
use mongodb::bson::{doc, spec::BinarySubtype, Bson, Document};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

const DB: &str = "dbine_vectors";

fn cfg(var: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(var).ok()?;
    let mut c = ConnectionConfig {
        driver: driver.into(),
        database: DB.into(),
        ..Default::default()
    };
    c.options.insert("connection_string".into(), url);
    Some(c)
}

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

async fn raw(url: &str) -> mongodb::Database {
    mongodb::Client::with_uri_str(url)
        .await
        .expect("client")
        .database(DB)
}

async fn docs(db: &mongodb::Database, name: &str) -> Vec<Document> {
    use futures::TryStreamExt;
    db.collection::<Document>(name)
        .find(doc! {})
        .sort(doc! { "_id": 1 })
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap()
}

/// The designer's collection: a category, a vector field and a vector
/// index on it with the category as filter.
fn designed(name: &str, index: IndexDef) -> TableSchema {
    let col = |n: &str, t: &str| ColumnDef {
        name: n.into(),
        data_type: t.into(),
        nullable: true,
        ..Default::default()
    };
    TableSchema {
        kind: "collection".into(),
        name: name.into(),
        columns: vec![
            col("_id", "int"),
            col("cat", "string"),
            col("emb", "vector(3)"),
        ],
        indexes: vec![index],
        ..Default::default()
    }
}

fn vector_index(name: &str, columns: &[&str], v: VectorIndex) -> IndexDef {
    IndexDef {
        name: name.into(),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        vector: Some(v),
        ..Default::default()
    }
}

const ALL: DdlParts = DdlParts {
    drop: false,
    if_exists: false,
    create: true,
    indexes: true,
    foreign_keys: false,
};

/// Rows as the grid's copy gives them to `insert_script`: vectors as
/// arrays of numbers.
fn rows() -> (Vec<String>, Vec<Vec<Value>>) {
    let cols = vec!["_id".to_string(), "cat".into(), "emb".into()];
    let result_cols = vec![
        dbine_driver::ResultColumn::new("_id", "int"),
        dbine_driver::ResultColumn::new("cat", "string"),
        dbine_driver::ResultColumn::new("emb", "vector(3)")
            .with_vector(VectorMeta::dense(Some(3), VectorElement::F32)),
    ];
    let rows = vec![
        vec![json!(1), json!("a"), json!("[1,0,0]")],
        vec![json!(2), json!("b"), json!("[0,1,0]")],
        vec![json!(3), json!("a"), json!("[0.9,0.1,0]")],
    ];
    (cols, vector::rows_for_scripts(&result_cols, &rows))
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.batches.push(batch);
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

async fn read_all(s: &mut Box<dyn Session>, name: &str) -> (Vec<TransferColumn>, Vec<Vec<Cell>>) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec {
        table: coll(name),
        columns: None,
        filter: None,
    };
    s.read_batches(&spec, sink.clone())
        .await
        .expect("read_batches");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    (
        c.columns,
        c.batches.into_iter().flat_map(|b| b.rows).collect(),
    )
}

async fn load(
    s: &mut Box<dyn Session>,
    name: &str,
    columns: &[TransferColumn],
    rows: Vec<Vec<Cell>>,
) -> u64 {
    let spec = LoadSpec {
        table: coll(name),
        columns: columns.iter().map(|c| c.name.clone()).collect(),
        table_lock: false,
        keep_identity: true,
        commit_rows: 1000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let batch = RowBatch { rows, bytes: 0 };
    s.bulk_load(&spec, columns, &mut Batches(vec![batch].into()), &|_| {})
        .await
        .expect("bulk_load")
}

/// Waits until the collection's search index `name` is queryable.
async fn ready(s: &mut Box<dyn Session>, c: &str, name: &str) {
    for _ in 0..120 {
        let out = run(
            s,
            &format!("db.getCollection(\"{c}\").getSearchIndexes(\"{name}\")"),
        )
        .await;
        let r = &out.results[0];
        let q = r.columns.iter().position(|c| c.name == "queryable");
        if let (Some(q), Some(row)) = (q, r.rows.first()) {
            if row[q] == json!(true) {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("the search index {name} never got ready (mongot pauses index builds when the disk is over 85% full)");
}

async fn cleanup(s: &mut Box<dyn Session>, names: &[&str]) {
    for n in names {
        run(s, &format!("db.getCollection(\"{n}\").drop()")).await;
    }
}

#[tokio::test]
#[ignore]
async fn atlas_vector_search() {
    let Some(c) = cfg("DBINE_TEST_ATLAS_URL", "mongodb") else {
        return;
    };
    let url = c.options["connection_string"].clone();
    let d = dbine_driver_mongodb::drivers().remove(0);
    assert!(d.capabilities().vectors);
    let spec = d
        .designer()
        .unwrap()
        .vector_indexes
        .expect("vector indexes");
    assert!(spec.filter_columns && spec.metrics.contains(&"cosine"));
    let mut s = d.connect(&c, None).await.expect("connect");
    let (t1, t2, t3) = ("vec_docs", "vec_from_pg", "vec_copy");
    cleanup(&mut s, &[t1, t2, t3]).await;

    // 1. The designer's collection and its vector index, by the driver's DDL.
    let ix = vector_index(
        "emb_vix",
        &["emb", "cat"],
        VectorIndex {
            metric: Some("cosine".into()),
            params: [("quantization", "scalar"), ("m", "32")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        },
    );
    let ddl = d.table_ddl(&designed(t1, ix), ALL).unwrap();
    assert!(
        ddl.contains("createSearchIndex") && ddl.contains("\"numDimensions\":3"),
        "{ddl}"
    );
    run(&mut s, &ddl).await;

    // 2. Rows by insert_script (vectors as arrays), plus a BinData vector.
    let (cols, rows) = rows();
    let ins = d.insert_script(&coll(t1), &cols, &rows).unwrap();
    run(&mut s, &ins).await;
    run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").insertOne({{ _id: 4, cat: \"b\", emb: BinData(9, \"JwAAAAA/AAAAPwAAAAA=\") }})"),
    )
    .await;

    // 3. Read back: the vector column is marked, the cells are exact.
    let out = run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").find({{}}).sort({{ _id: 1 }})"),
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
        [
            &json!("[1,0,0]"),
            &json!("[0,1,0]"),
            &json!("[0.9,0.1,0]"),
            &json!("[0.5,0.5,0]")
        ]
    );
    // A BinData vector alone is a vector column too.
    let out = run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").find({{ _id: 4 }}, {{ emb: 1 }})"),
    )
    .await;
    let r = &out.results[0];
    let at = r.columns.iter().position(|c| c.name == "emb").unwrap();
    assert!(r.columns[at].vector.is_some());
    assert_eq!(r.rows[0][at], json!("[0.5,0.5,0]"));
    // A grid insert after a result of BinData vectors writes one.
    let ins = d
        .insert_script(
            &coll(t1),
            &["_id".into(), "cat".into(), "emb".into()],
            &[vec![json!(5), json!("c"), json!([0.0, 0.0, 1.0])]],
        )
        .unwrap();
    assert!(ins.contains("BinData(9,"), "{ins}");
    run(&mut s, &ins).await;
    let five = docs(&raw(&url).await, t1).await;
    assert!(
        matches!(five[4].get("emb"), Some(Bson::Binary(b)) if b.subtype == BinarySubtype::Vector),
        "{five:?}"
    );
    run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").deleteOne({{ _id: 5 }})"),
    )
    .await;

    // 4. Describe keeps the dimension; 5. the index is listed with its vector.
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == t1).expect("table");
    let emb = t.columns.iter().find(|c| c.name == "emb").unwrap();
    assert_eq!(emb.data_type, "vector(3)");
    let ix = t
        .indexes
        .iter()
        .find(|i| i.name == "emb_vix")
        .expect("vector index");
    assert_eq!(ix.kind.as_deref(), Some("vectorSearch"));
    assert_eq!(ix.columns, ["emb", "cat"]);
    let v = ix.vector.as_ref().unwrap();
    assert_eq!(v.metric.as_deref(), Some("cosine"));
    assert_eq!(
        v.params.get("quantization").map(String::as_str),
        Some("scalar")
    );
    assert_eq!(v.params.get("m").map(String::as_str), Some("32"));
    let usage = s.index_usage(&coll(t1)).await.unwrap().unwrap();
    let u = usage
        .indexes
        .iter()
        .find(|i| i.name == "emb_vix")
        .expect("listed");
    assert!(u.kind.starts_with("VECTOR SEARCH"), "{}", u.kind);
    // The explorer's index carries the schema's settings.
    assert_eq!(u.vector.as_ref(), ix.vector.as_ref());
    // The table's own DDL gives the same index back.
    let again = d
        .table_ddl(
            t,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        again.contains("\"similarity\":\"cosine\"") && again.contains("\"path\":\"cat\""),
        "{again}"
    );
    // Compared with itself: nothing to do.
    let same = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: t.clone(),
        }])
        .unwrap();
    assert!(same.statements.is_empty(), "{:?}", same.statements);

    // 7. update_script: the array stays an array, the BinData a BinData.
    let upd = d
        .update_script(
            &coll(t1),
            &[
                RowChange {
                    key: vec![("_id".into(), json!(1))],
                    set: vec![
                        ("emb".into(), json!([0.0, 0.0, 1.0])),
                        ("cat".into(), json!("$c")),
                    ],
                    // The grid's row: the vector's text.
                    row: vec![
                        ("_id".into(), json!(1)),
                        ("cat".into(), json!("a")),
                        ("emb".into(), json!("[1,0,0]")),
                    ],
                },
                RowChange {
                    key: vec![("_id".into(), json!(4))],
                    set: vec![("emb".into(), json!([0.25, 0.0, 1.5]))],
                    row: vec![("emb".into(), json!("[0.5,0.5,0]"))],
                },
            ],
        )
        .unwrap();
    run(&mut s, &upd).await;
    let db = raw(&url).await;
    let all = docs(&db, t1).await;
    assert_eq!(
        all[0].get("emb"),
        Some(&Bson::Array(vec![
            Bson::Double(0.0),
            Bson::Double(0.0),
            Bson::Double(1.0)
        ]))
    );
    assert_eq!(all[0].get_str("cat"), Ok("$c"), "a $-string is a literal");
    match all[3].get("emb") {
        Some(Bson::Binary(b)) => {
            assert_eq!(b.subtype, BinarySubtype::Vector);
            assert_eq!(
                vector::from_f32_le(&b.bytes[2..]).unwrap(),
                [0.25, 0.0, 1.5]
            );
        }
        other => panic!("{other:?}"),
    }

    // The index changes in place (updateSearchIndex) and back.
    let mut changed = t.clone();
    changed
        .indexes
        .iter_mut()
        .find(|i| i.name == "emb_vix")
        .unwrap()
        .vector
        .as_mut()
        .unwrap()
        .metric = Some("l2".into());
    let sync = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: changed,
        }])
        .unwrap();
    assert_eq!(sync.statements.len(), 1, "{:?}", sync.statements);
    assert!(sync.statements[0].contains("updateSearchIndex"));
    run(&mut s, &sync.statements[0]).await;
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == t1).unwrap();
    let ix = t.indexes.iter().find(|i| i.name == "emb_vix").unwrap();
    assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("l2"));

    // 8. Transfers: the read gives the vectors' text; a pgvector-like
    // source (`vector(3)`, text cells) loads as arrays of doubles.
    let (tcols, trows) = read_all(&mut s, t1).await;
    let at = tcols.iter().position(|c| c.name == "emb").unwrap();
    assert!(
        tcols[at].type_name.starts_with("bson:vector(3"),
        "{}",
        tcols[at].type_name
    );
    let by_id = |id: i64| trows.iter().find(|r| r[0] == Cell::Int(id)).expect("row");
    // An array vector as JSON, a BinData one as text: a load by rows
    // tells which form each was.
    assert_eq!(by_id(2)[at], Cell::Json("[0,1,0]".into()));
    assert_eq!(by_id(4)[at], Cell::Text("[0.25,0,1.5]".into()));
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
        vec![Cell::Int(1), Cell::Text("[0.1,0.2,0.3]".into())],
        vec![Cell::Int(2), Cell::Text("[-1,0,1e-7]".into())],
    ];
    // The target as a clone from pgvector makes it: the field's type and
    // its vector index, by the driver's DDL.
    let mut target = designed(
        t2,
        vector_index(
            "emb_l2",
            &["emb"],
            VectorIndex {
                metric: Some("l2".into()),
                ..Default::default()
            },
        ),
    );
    target.columns.retain(|c| c.name == "emb");
    run(&mut s, &d.table_ddl(&target, ALL).unwrap()).await;
    assert_eq!(load(&mut s, t2, &pg_cols, pg_rows).await, 2);
    let got = docs_by(&db, t2, "id").await;
    assert_eq!(
        got[0].get("emb"),
        Some(&Bson::Array(vec![
            Bson::Double(0.1),
            Bson::Double(0.2),
            Bson::Double(0.3)
        ]))
    );
    // And back out as pgvector's text.
    let (bcols, brows) = read_all(&mut s, t2).await;
    let at = bcols.iter().position(|c| c.name == "emb").unwrap();
    let texts: Vec<&Cell> = brows.iter().map(|r| &r[at]).collect();
    assert!(
        texts.contains(&&Cell::Json("[-1,0,0.0000001]".into())),
        "{texts:?}"
    );
    // Same engine by rows: a field with BinData vectors loads as BinData
    // (the direct copy keeps every value as it is).
    let n = load(&mut s, t3, &tcols, trows).await;
    assert_eq!(n, 4);
    let copy = docs(&db, t3).await;
    assert!(
        matches!(copy[3].get("emb"), Some(Bson::Binary(b)) if b.subtype == BinarySubtype::Vector)
    );
    // …and its arrays stay arrays.
    assert!(
        matches!(copy[1].get("emb"), Some(Bson::Array(_))),
        "{:?}",
        copy[1]
    );

    // Definitions DBine can say whole, though written otherwise (an
    // explicit "none", the filter first): no whole definition kept, and an
    // edit changes the index in place.
    run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").createSearchIndex({{ name: \"vix2\", type: \"vectorSearch\", definition: {{ fields: [{{ type: \"filter\", path: \"cat\" }}, {{ type: \"vector\", path: \"emb\", numDimensions: 3, similarity: \"dotProduct\", quantization: \"none\" }}] }} }})"),
    )
    .await;
    // And an Atlas Search index with a knnVector field.
    run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").createSearchIndex({{ name: \"knn\", type: \"search\", definition: {{ mappings: {{ dynamic: false, fields: {{ emb: {{ type: \"knnVector\", dimensions: 3, similarity: \"cosine\" }}, cat: {{ type: \"token\" }} }} }} }} }})"),
    )
    .await;
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == t1).unwrap().clone();
    assert!(t.warnings.is_empty(), "{:?}", t.warnings);
    let vix2 = t.indexes.iter().find(|i| i.name == "vix2").expect("vix2");
    assert!(
        !vix2.options.contains_key("definition"),
        "{:?}",
        vix2.options
    );
    assert_eq!(vix2.columns, ["emb", "cat"]);
    let knn = t.indexes.iter().find(|i| i.name == "knn").expect("knn");
    assert_eq!(knn.kind.as_deref(), Some("search"));
    assert_eq!(knn.columns, ["emb"]);
    assert_eq!(
        knn.vector.as_ref().unwrap().metric.as_deref(),
        Some("cosine")
    );
    let usage = s.index_usage(&coll(t1)).await.unwrap().unwrap();
    let u = usage
        .indexes
        .iter()
        .find(|i| i.name == "knn")
        .expect("knn listed");
    assert!(u.kind.starts_with("SEARCH"), "{}", u.kind);
    // Compared with itself: nothing.
    let same = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: t.clone(),
        }])
        .unwrap();
    assert!(same.statements.is_empty(), "{:?}", same.statements);
    let mut edited = t.clone();
    for i in edited.indexes.iter_mut() {
        if i.name == "vix2" || i.name == "knn" {
            i.vector.as_mut().unwrap().metric = Some("l2".into());
        }
    }
    let sync = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: edited,
        }])
        .unwrap();
    assert_eq!(sync.statements.len(), 2, "{:?}", sync.statements);
    for st in &sync.statements {
        assert!(st.contains("updateSearchIndex"), "{st}");
        run(&mut s, st).await;
    }
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == t1).unwrap();
    for name in ["vix2", "knn"] {
        let i = t.indexes.iter().find(|i| i.name == name).unwrap();
        assert_eq!(
            i.vector.as_ref().unwrap().metric.as_deref(),
            Some("l2"),
            "{name}"
        );
    }
    run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").dropSearchIndex(\"vix2\")"),
    )
    .await;
    run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").dropSearchIndex(\"knn\")"),
    )
    .await;

    // 6. A similarity query (euclidean since the update) with the filter,
    // once mongot has built the index.
    ready(&mut s, t1, "emb_vix").await;
    let out = run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").aggregate([{{ $vectorSearch: {{ index: \"emb_vix\", path: \"emb\", queryVector: [0, 0, 1], numCandidates: 10, limit: 2, filter: {{ cat: \"b\" }} }} }}, {{ $project: {{ _id: 1, score: {{ $meta: \"vectorSearchScore\" }} }} }}])"),
    )
    .await;
    let ids: Vec<&Value> = out.results[0].rows.iter().map(|r| &r[0]).collect();
    assert_eq!(ids, [&json!(4), &json!(2)]);

    // The explorer's "Eliminar índice": dropSearchIndex.
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == t1).unwrap().clone();
    let mut without = t.clone();
    without.indexes.retain(|i| i.name != "emb_vix");
    let drop = d
        .sync_script(&[TableChange::Alter {
            old: t,
            new: without,
        }])
        .unwrap();
    assert_eq!(
        drop.statements,
        [format!(
            "db.getCollection(\"{t1}\").dropSearchIndex(\"emb_vix\")"
        )]
    );
    run(&mut s, &drop.statements[0]).await;

    cleanup(&mut s, &[t1, t2, t3]).await;
}

/// Atlas: a vector index on bits (the designer asked cosine) is made with
/// the euclidean distance and found by a bit query; the explorer lists it
/// with its settings; typed scripts keep each field's form.
#[tokio::test]
#[ignore]
async fn atlas_bits_and_typed_scripts() {
    let Some(c) = cfg("DBINE_TEST_ATLAS_URL", "mongodb") else {
        return;
    };
    let url = c.options["connection_string"].clone();
    let d = dbine_driver_mongodb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.expect("connect");
    let t = "vec_bits";
    cleanup(&mut s, &[t]).await;

    let mut table = designed(
        t,
        vector_index(
            "bix",
            &["emb"],
            VectorIndex {
                metric: Some("cosine".into()),
                ..Default::default()
            },
        ),
    );
    table.columns[2].data_type = "vector(16, bit)".into();
    let ddl = d.table_ddl(&table, ALL).unwrap();
    assert!(
        ddl.contains("en lugar de cosine") && ddl.contains("\"similarity\":\"euclidean\""),
        "{ddl}"
    );
    run(&mut s, &ddl).await;
    let bits = |b: &[u8]| -> Value {
        json!(b
            .iter()
            .flat_map(|x| (0..8).rev().map(move |i| f64::from((x >> i) & 1)))
            .collect::<Vec<f64>>())
    };
    let cols = [
        dbine_driver::ResultColumn::new("_id", "int"),
        dbine_driver::ResultColumn::new("emb", "vector(16, bit)"),
    ];
    let rows = vec![
        vec![json!(1), bits(&[0b1010_0000, 0b1111_0000])],
        vec![json!(2), bits(&[0b1111_0000, 0b0000_1111])],
        vec![json!(3), bits(&[0, 0])],
    ];
    run(
        &mut s,
        &d.insert_script_typed(&coll(t), &cols, &rows).unwrap(),
    )
    .await;
    let got = docs(&raw(&url).await, t).await;
    assert_eq!(stored(&got[0], "emb").0, Some(0x10));

    ready(&mut s, t, "bix").await;
    // The query: the first document's bits, packed (0x10 0x00 0xA0 0xF0).
    let out = run(
        &mut s,
        &format!("db.getCollection(\"{t}\").aggregate([{{ $vectorSearch: {{ index: \"bix\", path: \"emb\", queryVector: BinData(9, \"EACg8A==\"), numCandidates: 10, limit: 3 }} }}, {{ $project: {{ _id: 1 }} }}])"),
    )
    .await;
    let ids: Vec<&Value> = out.results[0].rows.iter().map(|r| &r[0]).collect();
    // Hamming 0, 6 and 10.
    assert_eq!(ids, [&json!(1), &json!(3), &json!(2)]);

    let schema = s.database_schema().await.unwrap();
    let tb = schema.iter().find(|x| x.name == t).unwrap();
    let ix = tb.indexes.iter().find(|i| i.name == "bix").unwrap();
    assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("l2"));
    let usage = s.index_usage(&coll(t)).await.unwrap().unwrap();
    let u = usage.indexes.iter().find(|i| i.name == "bix").unwrap();
    assert_eq!(u.vector, ix.vector);
    cleanup(&mut s, &[t]).await;

    typed_scripts(&*d, &mut s, &url, "vec_typed_atlas").await;
}

async fn docs_by(db: &mongodb::Database, name: &str, key: &str) -> Vec<Document> {
    use futures::TryStreamExt;
    db.collection::<Document>(name)
        .find(doc! {})
        .sort(doc! { key: 1 })
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap()
}

/// MongoDB 7 without mongot: no search indexes, the rest works and a
/// vector search index is the server's error, not a crash.
#[tokio::test]
#[ignore]
async fn mongo7_without_search() {
    let Some(c) = cfg("DBINE_TEST_MONGODB_URL", "mongodb") else {
        return;
    };
    let d = dbine_driver_mongodb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.expect("connect");
    let t = "vec_nosearch";
    cleanup(&mut s, &[t]).await;
    run(&mut s, &format!("db.getCollection(\"{t}\").insertOne({{ _id: 1, emb: BinData(9, \"JwAAAIA/AAAAAAAAAAA=\") }})")).await;
    let out = run(&mut s, &format!("db.getCollection(\"{t}\").find({{}})")).await;
    let r = &out.results[0];
    assert!(r.columns[1].vector.is_some());
    assert_eq!(r.rows[0][1], json!("[1,0,0]"));
    let schema = s.database_schema().await.unwrap();
    let tb = schema.iter().find(|x| x.name == t).unwrap();
    assert!(tb.indexes.is_empty());
    // No search here: nothing to warn about.
    assert!(tb.warnings.is_empty(), "{:?}", tb.warnings);
    // BinData float32 vectors say their element (`vector(3)` is an array).
    assert_eq!(
        tb.columns
            .iter()
            .find(|c| c.name == "emb")
            .unwrap()
            .data_type,
        "vector(3, float32)"
    );
    assert_eq!(r.columns[1].type_name, "vector(3, float32)");
    let usage = s.index_usage(&coll(t)).await.unwrap().unwrap();
    assert_eq!(usage.indexes.len(), 1, "only _id_");
    assert!(usage.indexes[0].vector.is_none());
    let ddl = d
        .table_ddl(
            &designed(t, vector_index("v", &["emb"], VectorIndex::default())),
            DdlParts {
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut out = QueryOutcome::default();
    let e = s.execute(&ddl, 10, &mut out).await.unwrap_err();
    assert!(!e.to_string().is_empty());
    cleanup(&mut s, &[t]).await;
    typed_scripts(&*d, &mut s, &c.options["connection_string"], "vec_typed7").await;
}

/// The stored vector of `field`: its BinData element byte and numbers, or
/// `None` and the array's numbers.
fn stored(d: &Document, field: &str) -> (Option<u8>, Vec<f64>) {
    match d.get(field) {
        Some(Bson::Binary(b)) => {
            assert_eq!(b.subtype, BinarySubtype::Vector, "{field}");
            let nums: Vec<f32> = match b.bytes[0] {
                0x03 => vector::from_i8(&b.bytes[2..]),
                0x10 => {
                    let n = (b.bytes.len() - 2) * 8 - b.bytes[1] as usize;
                    vector::from_bits(&b.bytes[2..], Some(n))
                        .into_iter()
                        .map(f32::from)
                        .collect()
                }
                _ => vector::from_f32_le(&b.bytes[2..]).unwrap(),
            };
            let nums = nums.into_iter().map(f64::from).collect();
            (Some(b.bytes[0]), nums)
        }
        Some(Bson::Array(a)) => (
            None,
            a.iter()
                .map(|x| match x {
                    Bson::Double(f) => *f,
                    Bson::Int32(i) => *i as f64,
                    Bson::Int64(i) => *i as f64,
                    other => panic!("{other:?}"),
                })
                .collect(),
        ),
        other => panic!("{field}: {other:?}"),
    }
}

/// Typed scripts (the app knows the columns' types): an insert writes
/// each vector in the form its type names, a result's and a describe's
/// types say that form back, and an update without the grid's row keeps
/// the element its type names.
async fn typed_scripts(d: &dyn dbine_driver::Driver, s: &mut Box<dyn Session>, url: &str, t: &str) {
    use dbine_driver::ResultColumn;
    cleanup(s, &[t]).await;
    let cols = [
        ResultColumn::new("_id", "int"),
        ResultColumn::new("q", "vector(3, int8)"),
        ResultColumn::new("b", "vector(9, bit)"),
        ResultColumn::new("f", "vector(3, float32)"),
        ResultColumn::new("a", "vector(3)"),
    ];
    let rows = vec![vec![
        json!(1),
        json!([1.0, -2.0, 3.0]),
        json!([1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0, 1.0]),
        json!([0.5, 0.25, 1.0]),
        json!([1.0, 2.0, 3.0]),
    ]];
    let ins = d
        .insert_script_typed(&coll(t), &cols, &rows)
        .expect("insert_script_typed");
    run(s, &ins).await;
    let db = raw(url).await;
    let got = docs(&db, t).await;
    assert_eq!(stored(&got[0], "q"), (Some(0x03), vec![1.0, -2.0, 3.0]));
    assert_eq!(
        stored(&got[0], "b"),
        (
            Some(0x10),
            vec![1.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0, 1.0]
        )
    );
    assert_eq!(stored(&got[0], "f"), (Some(0x27), vec![0.5, 0.25, 1.0]));
    assert_eq!(stored(&got[0], "a"), (None, vec![1.0, 2.0, 3.0]));

    // A result names each BinData field's form.
    let out = run(s, &format!("db.getCollection(\"{t}\").find({{}})")).await;
    let r = &out.results[0];
    let col = |n: &str| r.columns.iter().find(|c| c.name == n).unwrap();
    assert_eq!(col("q").type_name, "vector(3, int8)");
    assert_eq!(col("b").type_name, "vector(9, bit)");
    assert_eq!(col("f").type_name, "vector(3, float32)");
    assert_eq!(
        col("f").vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    let at = r.columns.iter().position(|c| c.name == "b").unwrap();
    assert_eq!(r.rows[0][at], json!("[1,0,1,1,0,0,0,1,1]"));

    // An update with the result's types and no grid row (a data sync):
    // each field keeps its element.
    let upd = d
        .update_script_typed(
            &coll(t),
            &[RowChange {
                key: vec![("_id".into(), json!(1))],
                set: vec![
                    ("q".into(), json!([4.0, 5.0, 6.0])),
                    ("f".into(), json!([0.125, 2.0, 3.0])),
                    (
                        "b".into(),
                        json!([0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0]),
                    ),
                    ("a".into(), json!([7.0, 8.0, 9.0])),
                ],
                ..Default::default()
            }],
            &r.columns,
        )
        .expect("update_script_typed");
    run(s, &upd).await;
    let got = docs(&db, t).await;
    assert_eq!(stored(&got[0], "q"), (Some(0x03), vec![4.0, 5.0, 6.0]));
    assert_eq!(stored(&got[0], "f"), (Some(0x27), vec![0.125, 2.0, 3.0]));
    assert_eq!(
        stored(&got[0], "b"),
        (
            Some(0x10),
            vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0]
        )
    );
    assert_eq!(stored(&got[0], "a"), (None, vec![7.0, 8.0, 9.0]));

    // A describe's types say the same; an insert with them writes the
    // same forms.
    let schema = s.database_schema().await.unwrap();
    let tb = schema.iter().find(|x| x.name == t).expect("described");
    let ty = |n: &str| {
        tb.columns
            .iter()
            .find(|c| c.name == n)
            .unwrap()
            .data_type
            .clone()
    };
    assert_eq!(ty("q"), "vector(3, int8)");
    assert_eq!(ty("b"), "vector(9, bit)");
    assert_eq!(ty("f"), "vector(3, float32)");
    let described: Vec<ResultColumn> = ["_id", "q", "b", "f", "a"]
        .iter()
        .map(|n| match tb.columns.iter().find(|c| c.name == *n) {
            Some(c) => ResultColumn::new(c.name.clone(), c.data_type.clone()),
            None => ResultColumn::new(*n, ""),
        })
        .collect();
    let mut row = rows[0].clone();
    row[0] = json!(2);
    let ins = d.insert_script_typed(&coll(t), &described, &[row]).unwrap();
    run(s, &ins).await;
    let got = docs(&db, t).await;
    assert_eq!(stored(&got[1], "q").0, Some(0x03));
    assert_eq!(stored(&got[1], "b").0, Some(0x10));
    assert_eq!(stored(&got[1], "f").0, Some(0x27));
    assert_eq!(stored(&got[1], "a").0, None);
    cleanup(s, &[t]).await;
}

/// FerretDB 2: `cosmosSearch` vector indexes (hnsw / ivf), `$search` with
/// `cosmosSearch` and `$vectorSearch`.
#[tokio::test]
#[ignore]
async fn ferretdb_vectors() {
    let Some(c) = cfg("DBINE_TEST_FERRETDB_URL", "ferretdb") else {
        return;
    };
    let url = c.options["connection_string"].clone();
    let d = dbine_driver_mongodb::drivers().remove(1);
    assert!(d.capabilities().vectors);
    let mut s = d.connect(&c, None).await.expect("connect");
    let (t1, t2) = ("vec_docs", "vec_from_pg");
    cleanup(&mut s, &[t1, t2]).await;

    // 1. The designer's collection and its vector index (cosmosSearch).
    let ix = vector_index(
        "emb_vix",
        &["emb"],
        VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("cosine".into()),
            params: [("m", "16"), ("ef_construction", "64")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        },
    );
    let mut table = designed(t1, ix);
    // FerretDB doesn't apply validators.
    table
        .options
        .insert("validate_fields".into(), "false".into());
    let ddl = d.table_ddl(&table, ALL).unwrap();
    assert!(
        ddl.contains("cosmosSearch") && ddl.contains("vector-hnsw"),
        "{ddl}"
    );
    run(&mut s, &ddl).await;

    // 2. Rows by insert_script, plus a BinData vector.
    let (cols, rows) = rows();
    run(&mut s, &d.insert_script(&coll(t1), &cols, &rows).unwrap()).await;
    run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").insertOne({{ _id: 4, cat: \"b\", emb: BinData(9, \"JwAAAAA/AAAAPwAAAAA=\") }})"),
    )
    .await;

    // 3. Read back.
    let out = run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").find({{}}).sort({{ _id: 1 }})"),
    )
    .await;
    let r = &out.results[0];
    let at = r.columns.iter().position(|c| c.name == "emb").unwrap();
    assert_eq!(
        r.columns[at].vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    assert_eq!(r.rows[2][at], json!("[0.9,0.1,0]"));
    assert_eq!(r.rows[3][at], json!("[0.5,0.5,0]"));

    // 4 / 5. Describe and the index.
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == t1).unwrap();
    assert_eq!(
        t.columns
            .iter()
            .find(|c| c.name == "emb")
            .unwrap()
            .data_type,
        "vector(3)"
    );
    let ix = t.indexes.iter().find(|i| i.name == "emb_vix").unwrap();
    assert_eq!(ix.kind.as_deref(), Some("cosmosSearch"));
    let v = ix.vector.as_ref().unwrap();
    assert_eq!(v.algorithm.as_deref(), Some("hnsw"));
    assert_eq!(v.metric.as_deref(), Some("cosine"));
    let same = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: t.clone(),
        }])
        .unwrap();
    assert!(same.statements.is_empty(), "{:?}", same.statements);

    // 6. Similarity queries: $search cosmosSearch and $vectorSearch.
    let out = run(
        &mut s,
        &format!("db.getCollection(\"{t1}\").aggregate([{{ $search: {{ cosmosSearch: {{ vector: [1, 0, 0], path: \"emb\", k: 2 }} }} }}, {{ $project: {{ _id: 1 }} }}])"),
    )
    .await;
    let ids: Vec<&Value> = out.results[0].rows.iter().map(|r| &r[0]).collect();
    assert_eq!(ids, [&json!(1), &json!(3)]);

    // 7. update_script keeps each form.
    let upd = d
        .update_script(
            &coll(t1),
            &[
                RowChange {
                    key: vec![("_id".into(), json!(1))],
                    set: vec![("emb".into(), json!([0.0, 0.0, 1.0]))],
                    row: vec![("emb".into(), json!("[1,0,0]"))],
                },
                RowChange {
                    key: vec![("_id".into(), json!(4))],
                    set: vec![("emb".into(), json!([0.25, 0.0, 1.5]))],
                    row: vec![("emb".into(), json!("[0.5,0.5,0]"))],
                },
            ],
        )
        .unwrap();
    run(&mut s, &upd).await;
    let db = raw(&url).await;
    let all = docs(&db, t1).await;
    assert!(matches!(all[0].get("emb"), Some(Bson::Array(_))));
    match all[3].get("emb") {
        Some(Bson::Binary(b)) => {
            assert_eq!(b.subtype, BinarySubtype::Vector);
            assert_eq!(
                vector::from_f32_le(&b.bytes[2..]).unwrap(),
                [0.25, 0.0, 1.5]
            );
        }
        other => panic!("{other:?}"),
    }
    // Without the grid's row: by each element's byte range (FerretDB
    // compares BinData as MongoDB does).
    let upd = d
        .update_script(
            &coll(t1),
            &[RowChange {
                key: vec![("_id".into(), json!(4))],
                set: vec![("emb".into(), json!([0.5, 0.5, 0.0]))],
                ..Default::default()
            }],
        )
        .unwrap();
    run(&mut s, &upd).await;
    let all = docs(&db, t1).await;
    match all[3].get("emb") {
        Some(Bson::Binary(b)) => {
            assert_eq!(b.bytes[0], 0x27, "float32");
            assert_eq!(vector::from_f32_le(&b.bytes[2..]).unwrap(), [0.5, 0.5, 0.0]);
        }
        other => panic!("{other:?}"),
    }

    // The index changes: dropped and made again (ivf).
    let mut changed = t.clone();
    let ix = changed
        .indexes
        .iter_mut()
        .find(|i| i.name == "emb_vix")
        .unwrap();
    ix.vector = Some(VectorIndex {
        algorithm: Some("ivf".into()),
        metric: Some("l2".into()),
        params: [("lists".to_string(), "1".to_string())].into(),
    });
    let sync = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: changed,
        }])
        .unwrap();
    for st in &sync.statements {
        run(&mut s, st).await;
    }
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == t1).unwrap();
    let v = t
        .indexes
        .iter()
        .find(|i| i.name == "emb_vix")
        .unwrap()
        .vector
        .clone()
        .unwrap();
    assert_eq!(
        (v.algorithm.as_deref(), v.metric.as_deref()),
        (Some("ivf"), Some("l2"))
    );

    // 8. Transfers.
    let (tcols, trows) = read_all(&mut s, t1).await;
    let at = tcols.iter().position(|c| c.name == "emb").unwrap();
    let id3 = trows.iter().find(|r| r[0] == Cell::Int(3)).expect("row 3");
    // An array vector as JSON (a BinData one as text).
    assert_eq!(id3[at], Cell::Json("[0.9,0.1,0]".into()));
    let id4 = trows.iter().find(|r| r[0] == Cell::Int(4)).expect("row 4");
    assert_eq!(id4[at], Cell::Text("[0.5,0.5,0]".into()));
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
    let pg_rows = vec![vec![Cell::Int(1), Cell::Text("[0.1,0.2,0.3]".into())]];
    assert_eq!(load(&mut s, t2, &pg_cols, pg_rows).await, 1);
    let got = docs(&db, t2).await;
    assert_eq!(
        got[0].get("emb"),
        Some(&Bson::Array(vec![
            Bson::Double(0.1),
            Bson::Double(0.2),
            Bson::Double(0.3)
        ]))
    );

    cleanup(&mut s, &[t1, t2]).await;
    typed_scripts(&*d, &mut s, &url, "vec_typed_ferret").await;
}

/// Amazon DocumentDB has no container: its scripts, checked offline.
#[test]
fn documentdb_scripts() {
    let d = dbine_driver_mongodb::drivers().remove(2);
    assert!(d.capabilities().vectors);
    let spec = d.designer().unwrap().vector_indexes.unwrap();
    assert_eq!(spec.algorithms, ["hnsw", "ivfflat"]);
    let ix = vector_index(
        "emb_vix",
        &["emb"],
        VectorIndex {
            algorithm: Some("ivfflat".into()),
            metric: Some("l2".into()),
            params: [("lists".to_string(), "10".to_string())].into(),
        },
    );
    let ddl = d.table_ddl(&designed("c", ix), ALL).unwrap();
    assert!(
        ddl.ends_with(r#"db.getCollection("c").createIndex({"emb":"vector"}, {"name":"emb_vix","vectorOptions":{"type":"ivfflat","similarity":"euclidean","dimensions":3,"lists":10}})"#),
        "{ddl}"
    );
    // An Atlas index from a clone becomes DocumentDB's, with a warning.
    let atlas = IndexDef {
        kind: Some("vectorSearch".into()),
        ..vector_index(
            "v",
            &["emb", "cat"],
            VectorIndex {
                metric: Some("ip".into()),
                ..Default::default()
            },
        )
    };
    let s = d
        .sync_script(&[TableChange::Create {
            table: designed("c", atlas),
        }])
        .unwrap();
    assert!(
        s.statements[0].contains(r#"{"emb":"vector"}"#) && s.statements[0].contains("dotProduct"),
        "{:?}",
        s.statements
    );
    assert!(
        s.warnings.iter().any(|w| w.contains("los filtros (cat)")),
        "{:?}",
        s.warnings
    );
    // Edits write arrays (DocumentDB indexes arrays only).
    let upd = d
        .update_script(
            &coll("c"),
            &[RowChange {
                key: vec![("_id".into(), json!(1))],
                set: vec![("emb".into(), json!([1.0, 2.0, 3.0]))],
                ..Default::default()
            }],
        )
        .unwrap();
    assert_eq!(
        upd,
        r#"db.getCollection("c").updateOne({ "_id": 1 }, { $set: { "emb": [1.0,2.0,3.0] } })"#
    );
}
