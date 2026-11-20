//! Vectors (embeddings) against real servers, the way the app uses them:
//! a table with a `vector<float, 3>` column and its vector index made from
//! a `TableSchema` (as the designer does), rows written with the insert and
//! update scripts, read back as results and as transfer cells, described,
//! searched and copied.
//!
//! `DBINE_TEST_CASSANDRA_URL=localhost:25402 DBINE_TEST_SCYLLADB_URL=localhost:25413 \
//!  cargo test -p dbine-driver-cassandra --test vectors -- --ignored --nocapture`
//!
//! Each test makes its own keyspace and drops it at the start and at the
//! end, also when it fails.

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, ObjectRef, QueryOutcome,
    RowChange, Session, TableSchema, VectorElement, VectorIndex,
};
use futures::FutureExt;
use serde_json::{json, Value};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

const KS: &str = "dbine_vec";

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_cassandra::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn open(id: &str, url: &str, ks: Option<&str>) -> Box<dyn Session> {
    let (host, port) = url.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: id.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        ..Default::default()
    };
    driver(id).connect(&cfg, ks).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 100, &mut out).await {
        panic!("{text}: {e}");
    }
    if let Some(e) = &out.error {
        panic!("{text}: {e}");
    }
    out
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: Some(KS.into()),
        name: name.into(),
    }
}

fn col(name: &str, ty: &str, opts: &[(&str, &str)]) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        options: opts
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

/// The designer's table: a key, a text and a vector, and its vector index.
fn docs(id: &str, name: &str) -> TableSchema {
    let mut v = VectorIndex {
        metric: Some("cosine".into()),
        ..Default::default()
    };
    let mut columns = vec!["emb".to_string()];
    if id == "scylladb" {
        v.params.insert("m".into(), "24".into());
        v.params.insert("ef_construction".into(), "200".into());
        v.params.insert("quantization".into(), "f16".into());
        // ScyllaDB's vector_index takes filter columns.
        columns.push("body".into());
    }
    TableSchema {
        kind: kinds::TABLE.into(),
        schema: Some(KS.into()),
        name: name.into(),
        columns: vec![
            col("id", "int", &[("partition_key", "true")]),
            col("body", "text", &[]),
            col("emb", "vector<float, 3>", &[]),
        ],
        indexes: vec![IndexDef {
            name: format!("{name}_emb"),
            columns,
            kind: Some(
                if id == "scylladb" {
                    "vector_index"
                } else {
                    "sai"
                }
                .into(),
            ),
            vector: Some(v),
            ..Default::default()
        }],
        ..Default::default()
    }
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

async fn read(s: &mut Box<dyn Session>, name: &str) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: table(name),
            columns: Some(vec!["id".into(), "emb".into()]),
            filter: None,
        },
        sink.clone(),
    )
    .await
    .unwrap();
    let mut c = std::mem::take(&mut *sink.lock().unwrap());
    c.rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    c
}

async fn load(s: &mut Box<dyn Session>, name: &str, rows: Vec<Vec<Cell>>) -> u64 {
    let mut src = Batches(vec![RowBatch { rows, bytes: 0 }].into_iter());
    s.bulk_load(
        &LoadSpec {
            table: table(name),
            columns: vec!["id".into(), "emb".into()],
            table_lock: false,
            keep_identity: false,
            commit_rows: 1000,
            commit_bytes: 1 << 20,
        },
        &[],
        &mut src,
        &|_| {},
    )
    .await
    .unwrap()
}

/// The `emb` cell of each row of a query, by id.
fn vectors(out: &QueryOutcome) -> Vec<(i64, String)> {
    let r = out.results.iter().find(|r| !r.columns.is_empty()).unwrap();
    let (id, emb) = (
        r.columns.iter().position(|c| c.name == "id").unwrap(),
        r.columns.iter().position(|c| c.name == "emb").unwrap(),
    );
    let mut v: Vec<(i64, String)> = r
        .rows
        .iter()
        .map(|row| {
            (
                row[id].as_i64().unwrap(),
                row[emb].as_str().unwrap_or("null").to_string(),
            )
        })
        .collect();
    v.sort();
    v
}

async fn body(id: &str, url: &str) {
    let d = driver(id);
    assert!(d.capabilities().vectors);
    let mut s = open(id, url, Some(KS)).await;
    println!("{id}: {}", s.server_version().await.unwrap());

    // 1. The table and its index, from the designer's TableSchema.
    let all = DdlParts {
        create: true,
        indexes: true,
        ..Default::default()
    };
    let ddl = d.table_ddl(&docs(id, "docs"), all).unwrap();
    println!("{ddl}");
    run(&mut s, &ddl).await;

    // 2. Rows with the insert script (vectors as JSON arrays).
    let cols = ["id".to_string(), "body".into(), "emb".into()];
    let rows = vec![
        vec![json!(1), json!("uno"), json!([0.1, 0.2, 0.3])],
        vec![json!(2), json!("dos"), json!([1, 0, 0])],
        vec![json!(3), json!("tres"), json!([0.5, -2, 3.5])],
    ];
    let ins = d.insert_script(&table("docs"), &cols, &rows).unwrap();
    run(&mut s, &ins).await;

    // 3. Read back: the column says it holds vectors, cells are the text.
    let out = run(&mut s, "SELECT id, emb FROM docs;").await;
    let r = &out.results[0];
    let emb = r.columns.iter().find(|c| c.name == "emb").unwrap();
    assert_eq!(emb.type_name, "vector<float, 3>");
    let meta = emb.vector.as_ref().expect("vector column");
    assert_eq!((meta.dims, meta.element), (Some(3), VectorElement::F32));
    assert_eq!(
        vectors(&out),
        [
            (1, "[0.1,0.2,0.3]".to_string()),
            (2, "[1,0,0]".to_string()),
            (3, "[0.5,-2,3.5]".to_string())
        ]
    );
    // Browsing the table gives the same.
    let browse = s.browse_query(&table("docs"), 10);
    let out = run(&mut s, &browse).await;
    assert!(out.results[0].columns.iter().any(|c| c.vector.is_some()));
    assert_eq!(vectors(&out)[0].1, "[0.1,0.2,0.3]");

    // 4. Describe keeps the dimension.
    let c = s.columns(&table("docs")).await.unwrap();
    assert_eq!(
        c.iter().find(|c| c.name == "emb").unwrap().data_type,
        "vector<float, 3>"
    );
    let def = s.definition(&table("docs")).await.unwrap().unwrap();
    assert!(def.contains("vector<float, 3>"), "{def}");

    // 5. The index is listed with its search settings, and scripts back
    // as it was made.
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == "docs").unwrap();
    assert_eq!(
        t.columns
            .iter()
            .find(|c| c.name == "emb")
            .unwrap()
            .data_type,
        "vector<float, 3>"
    );
    let ix = t
        .indexes
        .iter()
        .find(|i| i.name == "docs_emb")
        .unwrap_or_else(|| panic!("{:?}", t.indexes));
    println!("{ix:?}");
    let v = ix.vector.as_ref().expect("vector index");
    assert_eq!(v.metric.as_deref(), Some("cosine"));
    let want = docs(id, "docs");
    assert_eq!(ix.columns, want.indexes[0].columns);
    if id == "scylladb" {
        assert_eq!(v.params["m"], "24");
        assert_eq!(v.params["ef_construction"], "200");
        assert_eq!(v.params["quantization"], "f16");
        assert!(
            !ix.options.contains_key("index_version"),
            "{:?}",
            ix.options
        );
    }
    let only = DdlParts {
        indexes: true,
        ..Default::default()
    };
    let again = d.table_ddl(t, only).unwrap();
    let first = d.table_ddl(&want, only).unwrap();
    let norm = |s: &str| {
        s.replace("'similarity_function': 'COSINE', ", "")
            .replace(" WITH OPTIONS = {'similarity_function': 'COSINE'}", "")
    };
    assert_eq!(norm(&again), norm(&first), "{again}\n{first}");
    // An equal schema syncs to nothing.
    let sync = d
        .sync_script(&[dbine_driver::TableChange::Alter {
            old: t.clone(),
            new: t.clone(),
        }])
        .unwrap();
    assert!(sync.statements.is_empty(), "{:?}", sync.statements);
    // The index usage report carries the same vector settings.
    let usage = s
        .index_usage(&table("docs"))
        .await
        .unwrap()
        .expect("report");
    let listed = usage
        .indexes
        .iter()
        .find(|i| i.name == "docs_emb")
        .unwrap_or_else(|| panic!("{:?}", usage.indexes));
    assert_eq!(listed.vector, ix.vector, "{listed:?}");

    // 6. Similarity: ANN on Cassandra; ScyllaDB answers ANN only with its
    // Vector Store service, so there the similarity functions.
    let out = run(
        &mut s,
        "SELECT id, similarity_cosine(emb, [0.1, 0.2, 0.3]) AS score FROM docs;",
    )
    .await;
    let best = out.results[0]
        .rows
        .iter()
        .max_by(|a, b| a[1].as_f64().partial_cmp(&b[1].as_f64()).unwrap())
        .unwrap();
    assert_eq!(best[0], json!(1));
    if id == "cassandra" {
        let out = run(
            &mut s,
            "SELECT id FROM docs ORDER BY emb ANN OF [0.1, 0.2, 0.3] LIMIT 1;",
        )
        .await;
        assert_eq!(out.results[0].rows, vec![vec![json!(1)]]);
    }

    // 7. An edit with the update script.
    let upd = d
        .update_script(
            &table("docs"),
            &[RowChange {
                key: vec![("id".into(), json!(2))],
                set: vec![("emb".into(), json!([0, 1, 0.25]))],
                ..Default::default()
            }],
        )
        .unwrap();
    run(&mut s, &upd).await;
    let out = run(&mut s, "SELECT id, emb FROM docs WHERE id = 2;").await;
    assert_eq!(vectors(&out), [(2, "[0,1,0.25]".to_string())]);

    // 7b. Scripts with the result's column types: text that reads as JSON
    // stays text, a vector's text becomes its array.
    let typed = run(&mut s, "SELECT id, body, emb FROM docs WHERE id = 1;")
        .await
        .results[0]
        .columns
        .clone();
    let row = vec![json!(4), json!("[1]"), json!("[0, 0, 1]")];
    let ins = d
        .insert_script_typed(&table("docs"), &typed, std::slice::from_ref(&row))
        .unwrap();
    run(&mut s, &ins).await;
    let check = "SELECT id, body, emb FROM docs WHERE id = 4;";
    let out = run(&mut s, check).await;
    assert_eq!(out.results[0].rows[0][1], json!("[1]"));
    assert_eq!(vectors(&out), [(4, "[0,0,1]".to_string())]);
    // Untyped, that text is a JSON list, which a text column refuses.
    let names = ["id".to_string(), "body".into(), "emb".into()];
    let untyped = d.insert_script(&table("docs"), &names, &[row]).unwrap();
    let mut o = QueryOutcome::default();
    let refused = s.execute(&untyped, 10, &mut o).await.is_err() || o.error.is_some();
    assert!(refused, "{untyped}");
    let upd = d
        .update_script_typed(
            &table("docs"),
            &[RowChange {
                key: vec![("id".into(), json!(4))],
                set: vec![
                    ("body".into(), json!("{\"a\": 1}")),
                    ("emb".into(), json!("[1,1,0]")),
                ],
                ..Default::default()
            }],
            &typed,
        )
        .unwrap();
    run(&mut s, &upd).await;
    let out = run(&mut s, check).await;
    assert_eq!(out.results[0].rows[0][1], json!("{\"a\": 1}"));
    assert_eq!(vectors(&out), [(4, "[1,1,0]".to_string())]);
    // Another engine's JSON value (a `jsonb` synced or imported) into the
    // text column: its JSON text, not a map or a list the server refuses.
    let doc = vec![json!(5), json!({"a": [1, 2]}), json!([0, 1, 0])];
    let ins = d
        .insert_script_typed(&table("docs"), &typed, std::slice::from_ref(&doc))
        .unwrap();
    run(&mut s, &ins).await;
    let upd = d
        .update_script_typed(
            &table("docs"),
            &[RowChange {
                key: vec![("id".into(), json!(4))],
                set: vec![("body".into(), json!([true, null]))],
                ..Default::default()
            }],
            &typed,
        )
        .unwrap();
    run(&mut s, &upd).await;
    let mut rows = run(&mut s, "SELECT id, body FROM docs WHERE id IN (4, 5);")
        .await
        .results[0]
        .rows
        .clone();
    rows.sort_by_key(|r| r[0].as_i64());
    assert_eq!(
        rows,
        vec![
            vec![json!(4), json!("[true,null]")],
            vec![json!(5), json!("{\"a\":[1,2]}")]
        ]
    );
    run(&mut s, "DELETE FROM docs WHERE id = 5;").await;
    let del = d
        .delete_script_typed(&table("docs"), &[vec![("id".into(), json!(4))]], &typed)
        .unwrap();
    run(&mut s, &del).await;
    assert!(run(&mut s, check).await.results[0].rows.is_empty());

    // 8. Transfers: the read gives the text, the load takes it back (the
    // same engine), and takes what pgvector's read gives (another one).
    let got = read(&mut s, "docs").await;
    assert_eq!(got.columns[1].type_name, "vector<float, 3>");
    assert_eq!(
        got.rows,
        vec![
            vec![Cell::Int(1), Cell::Text("[0.1,0.2,0.3]".into())],
            vec![Cell::Int(2), Cell::Text("[0,1,0.25]".into())],
            vec![Cell::Int(3), Cell::Text("[0.5,-2,3.5]".into())],
        ]
    );
    let copy = DdlParts {
        create: true,
        ..Default::default()
    };
    run(&mut s, &d.table_ddl(&docs(id, "docs_copy"), copy).unwrap()).await;
    assert_eq!(load(&mut s, "docs_copy", got.rows.clone()).await, 3);
    assert_eq!(read(&mut s, "docs_copy").await.rows, got.rows);
    // pgvector's text output (`vector::text` of float4s).
    let pg = vec![
        vec![Cell::Int(10), Cell::Text("[1,2.5,-3]".into())],
        vec![Cell::Int(11), Cell::Text("[0.33333334,0,1e-07]".into())],
    ];
    assert_eq!(load(&mut s, "docs_copy", pg).await, 2);
    let out = run(
        &mut s,
        "SELECT id, emb FROM docs_copy WHERE id IN (10, 11);",
    )
    .await;
    assert_eq!(
        vectors(&out),
        [
            (10, "[1,2.5,-3]".to_string()),
            (11, "[0.33333334,0,0.0000001]".to_string())
        ]
    );
    // A vector of another length is refused, naming it.
    let bad = s
        .bulk_load(
            &LoadSpec {
                table: table("docs_copy"),
                columns: vec!["id".into(), "emb".into()],
                table_lock: false,
                keep_identity: false,
                commit_rows: 1000,
                commit_bytes: 1 << 20,
            },
            &[],
            &mut Batches(
                vec![RowBatch {
                    rows: vec![vec![Cell::Int(12), Cell::Text("[1,2]".into())]],
                    bytes: 0,
                }]
                .into_iter(),
            ),
            &|_| {},
        )
        .await;
    assert!(
        bad.as_ref()
            .is_err_and(|e| e.to_string().contains("dimensiones")),
        "{bad:?}"
    );

    // 9. A vector index without its settings (the designer's "Vectorial"
    // switched off on it) is still one: SAI on Cassandra, a custom
    // vector_index on ScyllaDB, with the server's defaults.
    let mut bare = docs(id, "docs_copy");
    bare.indexes[0].name = "docs_copy_emb".into();
    bare.indexes[0].columns.truncate(1);
    bare.indexes[0].vector = None;
    let ddl = d.table_ddl(&bare, only).unwrap();
    println!("{ddl}");
    run(&mut s, &ddl).await;
    let schema = s.database_schema().await.unwrap();
    let t = schema.iter().find(|t| t.name == "docs_copy").unwrap();
    let ix = t
        .indexes
        .iter()
        .find(|i| i.name == "docs_copy_emb")
        .unwrap();
    assert_eq!(
        ix.kind.as_deref(),
        Some(if id == "scylladb" {
            "vector_index"
        } else {
            "sai"
        })
    );
    assert_eq!(
        ix.vector.as_ref().unwrap().metric.as_deref(),
        Some("cosine")
    );
}

async fn vectors_on(id: &str, url: &str) {
    let mut admin = open(id, url, None).await;
    let fresh = format!(
        "DROP KEYSPACE IF EXISTS {KS};\nCREATE KEYSPACE {KS} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}};"
    );
    run(&mut admin, &fresh).await;
    let r = AssertUnwindSafe(body(id, url)).catch_unwind().await;
    run(&mut admin, &format!("DROP KEYSPACE IF EXISTS {KS};")).await;
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

#[tokio::test]
#[ignore]
async fn cassandra_vectors() {
    let url =
        std::env::var("DBINE_TEST_CASSANDRA_URL").unwrap_or_else(|_| "localhost:25402".into());
    vectors_on("cassandra", &url).await;
}

#[tokio::test]
#[ignore]
async fn scylladb_vectors() {
    let url = std::env::var("DBINE_TEST_SCYLLADB_URL").unwrap_or_else(|_| "localhost:25413".into());
    vectors_on("scylladb", &url).await;
}

/// ScyllaDB creates vector indexes only in keyspaces with tablets: the
/// designer's index in a keyspace without them is refused, and the error
/// says why and what to do.
#[tokio::test]
#[ignore]
async fn scylladb_vector_index_needs_tablets() {
    let url = std::env::var("DBINE_TEST_SCYLLADB_URL").unwrap_or_else(|_| "localhost:25413".into());
    let mut admin = open("scylladb", &url, None).await;
    let drop = format!("DROP KEYSPACE IF EXISTS {KS};");
    run(&mut admin, &drop).await;
    run(
        &mut admin,
        &format!("CREATE KEYSPACE {KS} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}} AND tablets = {{'enabled': false}};"),
    )
    .await;
    let r = AssertUnwindSafe(async {
        let d = driver("scylladb");
        let mut s = open("scylladb", &url, Some(KS)).await;
        let mut t = docs("scylladb", "docs");
        let tables = DdlParts {
            create: true,
            ..Default::default()
        };
        let indexes = t.indexes.clone();
        t.indexes.clear();
        run(&mut s, &d.table_ddl(&t, tables).unwrap()).await;
        t.indexes = indexes;
        let only = DdlParts {
            indexes: true,
            ..Default::default()
        };
        let ddl = d.table_ddl(&t, only).unwrap();
        let mut out = QueryOutcome::default();
        let e = match s.execute(&ddl, 10, &mut out).await {
            Err(e) => e.to_string(),
            Ok(()) => out.error.clone().unwrap_or_default(),
        };
        println!("{e}");
        assert!(
            e.contains("tablets") && e.contains("keyspace nuevo con tablets"),
            "{ddl}: {e}"
        );
    })
    .catch_unwind()
    .await;
    run(&mut admin, &drop).await;
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

/// The value the grid sends for an edited vector is a JSON array; a text
/// that holds one works too (a script typed by hand).
#[test]
fn update_takes_text_vectors_too() {
    let d = driver("cassandra");
    let s = d
        .update_script(
            &table("docs"),
            &[RowChange {
                key: vec![("id".into(), json!(1))],
                set: vec![("emb".into(), Value::from("[1,2,3]"))],
                ..Default::default()
            }],
        )
        .unwrap();
    assert!(s.contains("emb = fromJson('[1,2,3]')"), "{s}");
}
