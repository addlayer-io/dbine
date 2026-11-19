//! `VECTOR` against a real Oracle Database 23ai / 26ai: tables and HNSW /
//! IVF indexes made from the designer's `TableSchema`, rows written with
//! the edit scripts (every format, sparse, a 1536-dimension one past the
//! literal limit), read back marked as vectors, the declared type kept, the
//! indexes listed, a similarity search, a sync that changes an index, and
//! transfers (another engine's batches in, a typed read out). Its tables
//! are dropped first and at the end.
//!
//! ```sh
//! docker start dbine-test-oracle   # -p 25601:1521, user dbine/Dbine123, FREEPDB1
//! cargo test -p dbine-driver-oracle --test vectors -- --ignored --test-threads=1
//! ```
//!
//! HNSW indexes need the vector memory pool (`VECTOR_MEMORY_SIZE` > 0,
//! set at the CDB). `DBINE_TEST_ORACLE_URL` overrides the server (default
//! `oracle://dbine:Dbine123@localhost:25601/FREEPDB1`).

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    async_trait, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, KeyDef, ObjectRef,
    QueryOutcome, RowChange, Session, TableChange, TableSchema, VectorElement, VectorIndex,
    VectorMeta,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

const TABLES: [&str; 5] = [
    "VEC_DOCS",
    "VEC_DOCS_COPY",
    "VEC_KINDS",
    "VEC_KINDS_COPY",
    "VEC_IVF",
];

fn config() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_ORACLE_URL")
        .unwrap_or_else(|_| "oracle://dbine:Dbine123@localhost:25601/FREEPDB1".into());
    let rest = url
        .strip_prefix("oracle://")
        .expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_oracle::drivers().remove(0)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 1000, &mut out).await {
        panic!("{e}\n{sql}");
    }
    if let Some(e) = &out.error {
        panic!("{e}\n{sql}");
    }
    out
}

async fn drop_tables(s: &mut Box<dyn Session>) {
    for t in TABLES {
        let _ = s
            .execute(
                &format!("DROP TABLE {t} PURGE"),
                1,
                &mut QueryOutcome::default(),
            )
            .await;
    }
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: None,
        name: name.into(),
    }
}

fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        ..Default::default()
    }
}

fn schema(name: &str, columns: Vec<ColumnDef>, indexes: Vec<IndexDef>) -> TableSchema {
    TableSchema {
        kind: "table".into(),
        name: name.into(),
        columns,
        primary_key: Some(KeyDef {
            name: None,
            columns: vec!["ID".into()],
        }),
        indexes,
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

fn hnsw(metric: &str) -> VectorIndex {
    VectorIndex {
        algorithm: Some("hnsw".into()),
        metric: Some(metric.into()),
        params: [
            ("m".to_string(), "16".to_string()),
            ("ef_construction".to_string(), "100".to_string()),
            ("target_accuracy".to_string(), "95".to_string()),
        ]
        .into(),
    }
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
    batches: Vec<RowBatch>,
}
impl BatchSink for Collect {
    fn begin(&mut self, c: &[TransferColumn]) -> io::Result<()> {
        self.columns = c.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.batches.push(b);
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

async fn read(s: &mut Box<dyn Session>, name: &str) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: table(name),
            columns: None,
            filter: None,
        },
        sink.clone(),
    )
    .await
    .expect("read_batches");
    let got = std::mem::take(&mut *sink.lock().unwrap());
    got
}

/// Every row of a table as its cells' JSON, ordered by ID.
async fn rows_of(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    run(s, sql).await.results.remove(0).rows
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn vectors_end_to_end() {
    let mut s = driver().connect(&config(), None).await.expect("connect");
    drop_tables(&mut s).await;
    // In its own task, so the tables go even when it fails partway.
    let res = tokio::spawn(async move { body(&mut s).await }).await;
    let mut s = driver().connect(&config(), None).await.expect("connect");
    drop_tables(&mut s).await;
    if let Err(e) = res {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn body(s: &mut Box<dyn Session>) {
    let d = driver();
    assert!(d.capabilities().vectors);
    let spec = d
        .designer()
        .unwrap()
        .vector_indexes
        .expect("vector indexes");
    assert_eq!(spec.algorithms, ["hnsw", "ivf"]);

    // 1. Tables and indexes as the designer makes them.
    let docs = schema(
        "VEC_DOCS",
        vec![
            col("ID", "NUMBER(10)", false),
            col("EMB", "VECTOR(3, FLOAT32)", true),
            col("NOTE", "VARCHAR2(20)", true),
        ],
        vec![vector_index("IX_VEC_DOCS_EMB", &["EMB"], hnsw("cosine"))],
    );
    let ddl = d.table_ddl(&docs, ALL).unwrap();
    assert!(ddl.contains("\"EMB\" VECTOR(3, FLOAT32)"), "{ddl}");
    assert!(
        ddl.contains("CREATE VECTOR INDEX \"IX_VEC_DOCS_EMB\" ON \"VEC_DOCS\" (\"EMB\") ORGANIZATION INMEMORY NEIGHBOR GRAPH DISTANCE COSINE WITH TARGET ACCURACY 95 PARAMETERS (TYPE HNSW, NEIGHBORS 16, EFCONSTRUCTION 100);"),
        "{ddl}"
    );
    run(s, &ddl).await;
    let ivf = schema(
        "VEC_IVF",
        vec![
            col("ID", "NUMBER(10)", false),
            col("EMB", "VECTOR(3, FLOAT32)", true),
            col("CAT", "NUMBER(5)", true),
        ],
        vec![vector_index(
            "IX_VEC_IVF_EMB",
            &["EMB", "CAT"],
            VectorIndex {
                algorithm: Some("ivf".into()),
                metric: Some("l2".into()),
                params: [("lists".to_string(), "2".to_string())].into(),
            },
        )],
    );
    run(s, &d.table_ddl(&ivf, ALL).unwrap()).await;
    let kinds = schema(
        "VEC_KINDS",
        vec![
            col("ID", "NUMBER(10)", false),
            col("F", "VECTOR(*, *)", true),
            col("B", "VECTOR(16, BINARY)", true),
            col("S", "VECTOR(5, FLOAT32, SPARSE)", true),
            col("I", "VECTOR(4, INT8)", true),
            col("D", "VECTOR(2, FLOAT64)", true),
            col("L", "VECTOR(1536, FLOAT32)", true),
        ],
        vec![],
    );
    run(s, &d.table_ddl(&kinds, ALL).unwrap()).await;

    // 2. Rows from the insert script.
    let doc_rows: Vec<Vec<Value>> = (1..=40)
        .map(|i| match i {
            1 => vec![json!(1), json!([0.1, 0.2, 0.3]), json!("a")],
            2 => vec![json!(2), json!([1.0, 0.0, 0.0]), json!("b")],
            _ => vec![
                json!(i),
                json!([i as f64, (i % 7) as f64, 1.0]),
                Value::Null,
            ],
        })
        .collect();
    let cols3 = |a: &str, b: &str, c: &str| vec![a.to_string(), b.to_string(), c.to_string()];
    let insert = d
        .insert_script(&table("VEC_DOCS"), &cols3("ID", "EMB", "NOTE"), &doc_rows)
        .unwrap();
    assert!(insert.contains("'[0.1,0.2,0.3]'"), "{insert}");
    run(s, &insert).await;
    let ivf_rows: Vec<Vec<Value>> = (1..=30)
        .map(|i| {
            vec![
                json!(i),
                json!([i as f64, 1.0, (i % 3) as f64]),
                json!(i % 3),
            ]
        })
        .collect();
    run(
        s,
        &d.insert_script(&table("VEC_IVF"), &cols3("ID", "EMB", "CAT"), &ivf_rows)
            .unwrap(),
    )
    .await;
    let long: Vec<f64> = (0..1536).map(|i| (i as f64) * 0.001 - 0.5).collect();
    let kind_rows = vec![
        vec![
            json!(1),
            json!([1.5, 2.0]),
            json!([255, 0]),
            json!("[5,[1,3],[0.5,2.5]]"),
            json!([1, -2, 3, 4]),
            json!([1.5, 2.25]),
            json!(long),
        ],
        vec![
            json!(2),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
    ];
    let kind_cols: Vec<String> = ["ID", "F", "B", "S", "I", "D", "L"]
        .iter()
        .map(|c| c.to_string())
        .collect();
    let insert = d
        .insert_script(&table("VEC_KINDS"), &kind_cols, &kind_rows)
        .unwrap();
    assert!(
        insert.contains("TO_CLOB('[-0.5,"),
        "the long vector goes as a CLOB chain"
    );
    run(s, &insert).await;

    // 3. Read back: columns say VECTOR(…), texts are exact.
    let out = run(
        s,
        "SELECT ID, EMB, NOTE FROM VEC_DOCS WHERE ID <= 2 ORDER BY ID",
    )
    .await;
    let r = &out.results[0];
    assert_eq!(r.columns[1].type_name, "VECTOR(3, FLOAT32)");
    assert_eq!(
        r.columns[1].vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    assert_eq!(r.columns[0].vector, None);
    assert_eq!(r.rows[0][1], json!("[0.1,0.2,0.3]"));
    assert_eq!(r.rows[1][1], json!("[1,0,0]"));
    let out = run(s, "SELECT F, B, S, I, D, L FROM VEC_KINDS ORDER BY ID").await;
    let r = &out.results[0];
    let meta: Vec<Option<VectorMeta>> = r.columns.iter().map(|c| c.vector.clone()).collect();
    assert_eq!(meta[0], Some(VectorMeta::dense(None, VectorElement::F32)));
    assert_eq!(
        meta[1],
        Some(VectorMeta::dense(Some(16), VectorElement::Bit))
    );
    assert_eq!(
        meta[2],
        Some(VectorMeta::sparse(Some(5), VectorElement::F32))
    );
    assert_eq!(meta[3], Some(VectorMeta::dense(Some(4), VectorElement::I8)));
    assert_eq!(
        meta[4],
        Some(VectorMeta::dense(Some(2), VectorElement::F64))
    );
    assert_eq!(r.columns[2].type_name, "VECTOR(5, FLOAT32, SPARSE)");
    assert_eq!(
        r.rows[0][..5],
        [
            json!("[1.5,2]"),
            json!("[255,0]"),
            json!("[5,[1,3],[0.5,2.5]]"),
            json!("[1,-2,3,4]"),
            json!("[1.5,2.25]"),
        ]
    );
    let want_long = dbine_driver::vector::text(long.iter().map(|&x| x as f32));
    assert_eq!(r.rows[0][5], json!(want_long), "the long vector, whole");
    assert_eq!(r.rows[1][0], Value::Null);
    // Table browsing too.
    let browse = s.browse_query(&table("VEC_DOCS"), 5);
    let out = run(s, &browse).await;
    let r = &out.results[0];
    let at = r.columns.iter().position(|c| c.name == "EMB").unwrap();
    assert!(r.columns[at].vector.is_some(), "{browse}");

    // 4. Describe keeps the declared type.
    let cols = s.columns(&table("VEC_KINDS")).await.unwrap();
    let types: Vec<&str> = cols.iter().map(|c| c.data_type.as_str()).collect();
    assert_eq!(
        types,
        [
            "NUMBER(10)",
            "VECTOR(*, *)",
            "VECTOR(16, BINARY)",
            "VECTOR(5, FLOAT32, SPARSE)",
            "VECTOR(4, INT8)",
            "VECTOR(2, FLOAT64)",
            "VECTOR(1536, FLOAT32)",
        ]
    );

    // 5. The indexes are listed with their settings.
    let tables = s.database_schema().await.unwrap();
    let t = |n: &str| tables.iter().find(|t| t.name == n).unwrap().clone();
    let docs_now = t("VEC_DOCS");
    assert_eq!(docs_now.columns[1].data_type, "VECTOR(3, FLOAT32)");
    let ix = docs_now
        .indexes
        .iter()
        .find(|i| i.name == "IX_VEC_DOCS_EMB")
        .expect("HNSW index listed");
    assert_eq!(ix.kind.as_deref(), Some("VECTOR"));
    assert_eq!(ix.columns, ["EMB"]);
    let v = ix.vector.clone().unwrap();
    assert_eq!(v.algorithm.as_deref(), Some("hnsw"));
    assert_eq!(v.metric.as_deref(), Some("cosine"));
    assert_eq!(
        v.params.get("target_accuracy").map(String::as_str),
        Some("95")
    );
    // NEIGHBORS / EFCONSTRUCTION are in VECSYS.VECTOR$INDEX, readable by
    // DBAs only; a login that has it gets them too.
    if let Some(m) = v.params.get("m") {
        assert_eq!(m, "16");
        assert_eq!(
            v.params.get("ef_construction").map(String::as_str),
            Some("100")
        );
    }
    let ivf_now = t("VEC_IVF");
    let ix = ivf_now
        .indexes
        .iter()
        .find(|i| i.name == "IX_VEC_IVF_EMB")
        .expect("IVF index listed");
    assert_eq!(
        ix.columns,
        ["EMB", "CAT"],
        "INCLUDE columns after the vector"
    );
    let v = ix.vector.clone().unwrap();
    assert_eq!(v.algorithm.as_deref(), Some("ivf"));
    assert_eq!(v.metric.as_deref(), Some("l2"));
    // Scripted again from the catalog, it's the same index.
    let again = d
        .table_ddl(
            &ivf_now,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        again.contains("CREATE VECTOR INDEX \"IX_VEC_IVF_EMB\" ON \"VEC_IVF\" (\"EMB\") INCLUDE (\"CAT\") ORGANIZATION NEIGHBOR PARTITIONS DISTANCE EUCLIDEAN"),
        "{again}"
    );

    // 6. Similarity, exact and approximate.
    let rows = rows_of(
        s,
        "SELECT ID FROM VEC_DOCS ORDER BY VECTOR_DISTANCE(EMB, TO_VECTOR('[0.1,0.2,0.3]'), COSINE) FETCH FIRST 2 ROWS ONLY",
    )
    .await;
    assert_eq!(rows[0][0], json!(1));
    let rows = rows_of(
        s,
        "SELECT ID FROM VEC_DOCS ORDER BY VECTOR_DISTANCE(EMB, TO_VECTOR('[0.1,0.2,0.3]'), COSINE) FETCH APPROX FIRST 2 ROWS ONLY WITH TARGET ACCURACY 90",
    )
    .await;
    assert_eq!(rows[0][0], json!(1));

    // 7. The update script.
    let update = d
        .update_script(
            &table("VEC_DOCS"),
            &[RowChange {
                key: vec![("ID".into(), json!(2))],
                set: vec![("EMB".into(), json!([0.0, 1.0, 0.5]))],
                ..Default::default()
            }],
        )
        .unwrap();
    run(s, &update).await;
    let rows = rows_of(s, "SELECT EMB FROM VEC_DOCS WHERE ID = 2").await;
    assert_eq!(rows[0][0], json!("[0,1,0.5]"));

    // 8. A sync that changes the distance remakes the index.
    let mut new = docs_now.clone();
    new.indexes
        .iter_mut()
        .find(|i| i.name == "IX_VEC_DOCS_EMB")
        .unwrap()
        .vector = Some(hnsw("l2"));
    let sync = d
        .sync_script(&[TableChange::Alter {
            old: docs_now.clone(),
            new,
        }])
        .unwrap();
    let text = sync.statements.join("\n");
    assert!(text.contains("DROP INDEX \"IX_VEC_DOCS_EMB\""), "{text}");
    assert!(text.contains("DISTANCE EUCLIDEAN"), "{text}");
    for st in &sync.statements {
        run(s, st).await;
    }
    let tables = s.database_schema().await.unwrap();
    let ix = tables
        .iter()
        .find(|t| t.name == "VEC_DOCS")
        .unwrap()
        .indexes
        .iter()
        .find(|i| i.name == "IX_VEC_DOCS_EMB")
        .unwrap()
        .clone();
    assert_eq!(ix.vector.unwrap().metric.as_deref(), Some("l2"));

    // 9. Transfers: a typed read gives the canonical text…
    let read_docs = read(s, "VEC_DOCS").await;
    let first = &read_docs.batches[0].rows;
    assert!(
        first
            .iter()
            .any(|r| r[1] == Cell::Text("[0.1,0.2,0.3]".into())),
        "{first:?}"
    );
    // …which loads into a copy, with another engine's (pgvector's) text after it.
    run(
        s,
        &d.table_ddl(&schema("VEC_DOCS_COPY", docs.columns.clone(), vec![]), ALL)
            .unwrap(),
    )
    .await;
    let mut batches: VecDeque<RowBatch> = read_docs.batches.clone().into();
    batches.push_back(RowBatch {
        rows: vec![vec![
            Cell::Int(100),
            Cell::Text("[1, 2.5, -3]".into()),
            Cell::Text("pgvector".into()),
        ]],
        bytes: 64,
    });
    let spec = LoadSpec {
        table: table("VEC_DOCS_COPY"),
        columns: cols3("ID", "EMB", "NOTE"),
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let n = s
        .bulk_load(&spec, &read_docs.columns, &mut Batches(batches), &|_| {})
        .await
        .expect("bulk_load");
    assert_eq!(n, 41);
    let mut want = rows_of(s, "SELECT ID, EMB FROM VEC_DOCS ORDER BY ID").await;
    want.push(vec![json!(100), json!("[1,2.5,-3]")]);
    assert_eq!(
        rows_of(s, "SELECT ID, EMB FROM VEC_DOCS_COPY ORDER BY ID").await,
        want
    );
    // Every format, sparse and the long one, through a read and a load.
    let read_kinds = read(s, "VEC_KINDS").await;
    run(
        s,
        &d.table_ddl(
            &schema("VEC_KINDS_COPY", kinds.columns.clone(), vec![]),
            ALL,
        )
        .unwrap(),
    )
    .await;
    let spec = LoadSpec {
        table: table("VEC_KINDS_COPY"),
        columns: kind_cols.clone(),
        ..spec
    };
    s.bulk_load(
        &spec,
        &read_kinds.columns,
        &mut Batches(read_kinds.batches.into()),
        &|_| {},
    )
    .await
    .expect("bulk_load kinds");
    let all = "F, B, S, I, D, L";
    assert_eq!(
        rows_of(
            s,
            &format!("SELECT ID, {all} FROM VEC_KINDS_COPY ORDER BY ID")
        )
        .await,
        rows_of(s, &format!("SELECT ID, {all} FROM VEC_KINDS ORDER BY ID")).await
    );
}
