//! `VECTOR` against a real Oracle Database 23ai / 26ai: tables and HNSW /
//! IVF indexes made from the designer's `TableSchema`, rows written with
//! the edit scripts (every format, BINARY as bits, sparse, a
//! 1536-dimension one past the literal limit), read back marked as
//! vectors, the declared type kept, the indexes listed with their settings
//! (also for a login that owns them without any role, and with a note for
//! one that can't read them), the indexes' internal tables left out, a
//! similarity search, a sync that changes an index, and transfers (another
//! engine's batches in, a typed read out). Its tables and its user are
//! dropped first and at the end.
//!
//! ```sh
//! docker start dbine-test-oracle   # -p 25601:1521, user dbine/Dbine123, FREEPDB1
//! cargo test -p dbine-driver-oracle --test vectors -- --ignored --test-threads=1
//! ```
//!
//! HNSW indexes need the vector memory pool, `VECTOR_MEMORY_SIZE` > 0. It
//! can't be raised from 0 while the instance runs, and the container
//! starts with 0: set it in the spfile and restart, then put it back the
//! same way when done:
//!
//! ```sh
//! docker exec -i dbine-test-oracle sqlplus -s / as sysdba <<'SQL'
//! ALTER SYSTEM SET vector_memory_size = 128M SCOPE = SPFILE;
//! SHUTDOWN IMMEDIATE
//! STARTUP
//! ALTER PLUGGABLE DATABASE ALL OPEN;
//! SQL
//! ```
//!
//! Without it the test says so and makes IVF indexes where it would make
//! HNSW ones (ORA-51962), so everything else is still checked.
//!
//! The login needs CREATE USER, DROP USER and to grant CREATE SESSION /
//! CREATE TABLE (the `dbine` user of the container has them) for the
//! part that runs as a user without roles. `DBINE_TEST_ORACLE_URL`
//! overrides the server (default
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

const TABLES: [&str; 6] = [
    "VEC_DOCS",
    "VEC_DOCS_COPY",
    "VEC_KINDS",
    "VEC_KINDS_COPY",
    "VEC_IVF",
    "VEC_BITS",
];

/// A user without roles, made by the test: owns a vector index, and reads
/// the test's own one through a grant.
const PLAIN_USER: &str = "DBINE_VEC_PLAIN";
const PLAIN_PASSWORD: &str = "Plain_123";

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

async fn try_run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 1000, &mut out).await {
        return Err(e.to_string());
    }
    match out.error.take() {
        Some(e) => Err(e.to_string()),
        None => Ok(out),
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    try_run(s, sql)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
}

async fn drop_tables(s: &mut Box<dyn Session>) {
    for sql in TABLES
        .iter()
        .map(|t| format!("DROP TABLE {t} PURGE"))
        .chain([format!("DROP USER {PLAIN_USER} CASCADE")])
    {
        let _ = s.execute(&sql, 1, &mut QueryOutcome::default()).await;
    }
}

/// The test's login as another user.
fn config_as(user: &str, password: &str) -> ConnectionConfig {
    ConnectionConfig {
        username: Some(user.into()),
        password: Some(password.into()),
        ..config()
    }
}

/// An index of `name` as the designer makes it: HNSW where the instance
/// has the vector memory pool, IVF where it doesn't.
fn designed(hnsw_ok: bool, metric: &str) -> VectorIndex {
    if hnsw_ok {
        hnsw(metric)
    } else {
        VectorIndex {
            algorithm: Some("ivf".into()),
            metric: Some(metric.into()),
            params: [
                ("lists".to_string(), "2".to_string()),
                ("target_accuracy".to_string(), "95".to_string()),
            ]
            .into(),
        }
    }
}

/// Whether a name is one of a vector index's internal tables.
fn internal(name: &str) -> bool {
    name.starts_with("VECTOR$")
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
    let mut docs = schema(
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
    let hnsw_ok = match try_run(s, &ddl).await {
        Ok(_) => true,
        Err(e) if e.contains("ORA-51962") => {
            eprintln!(
                "NOTE: this instance has no vector memory pool (VECTOR_MEMORY_SIZE = 0, ORA-51962), so the \
                 test makes IVF indexes instead of HNSW ones. See this file's header to turn the pool on."
            );
            let _ = try_run(s, "DROP TABLE VEC_DOCS PURGE").await;
            docs.indexes[0].vector = Some(designed(false, "cosine"));
            run(s, &d.table_ddl(&docs, ALL).unwrap()).await;
            false
        }
        Err(e) => panic!("{e}\n{ddl}"),
    };
    let algorithm = if hnsw_ok { "hnsw" } else { "ivf" };
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
            col("Z", "VECTOR(8, FLOAT32)", true),
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
    // BINARY: one 0 / 1 per dimension, Oracle's own example (bytes 201, 15).
    let bits = json!([1, 1, 0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 1, 1, 1, 1]);
    let kind_rows = vec![
        vec![
            json!(1),
            json!([1.5, 2.0]),
            bits.clone(),
            json!("[5,[1,3],[0.5,2.5]]"),
            json!([1, -2, 3, 4]),
            json!([1.5, 2.25]),
            json!(long),
            json!([1, 0, 1, 0, 1, 0, 1, 1]),
        ],
        vec![
            json!(2),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
    ];
    let kind_cols: Vec<String> = ["ID", "F", "B", "S", "I", "D", "L", "Z"]
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
    // Stored as Oracle's bytes; a FLOAT32 column with 0s and 1s keeps them.
    let stored = rows_of(
        s,
        "SELECT FROM_VECTOR(B), VECTOR_DIMENSION_FORMAT(Z) FROM VEC_KINDS WHERE ID = 1",
    )
    .await;
    assert_eq!(stored[0], [json!("[201,15]"), json!("FLOAT32")]);

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
    let out = run(s, "SELECT F, B, S, I, D, L, Z FROM VEC_KINDS ORDER BY ID").await;
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
            json!("[1,1,0,0,1,0,0,1,0,0,0,0,1,1,1,1]"),
            json!("[5,[1,3],[0.5,2.5]]"),
            json!("[1,-2,3,4]"),
            json!("[1.5,2.25]"),
        ]
    );
    let want_long = dbine_driver::vector::text(long.iter().map(|&x| x as f32));
    assert_eq!(r.rows[0][5], json!(want_long), "the long vector, whole");
    assert_eq!(r.rows[0][6], json!("[1,0,1,0,1,0,1,1]"));
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
            "VECTOR(8, FLOAT32)",
        ]
    );

    // 5. The indexes are listed with their settings, and their internal
    // tables (VECTOR$…$HNSW_…, …$IVF_FLAT_…) aren't tables of the schema.
    let tables = s.database_schema().await.unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert!(!names.iter().any(|n| internal(n)), "{names:?}");
    let objects = s.list_objects().await.unwrap();
    assert!(
        !objects.iter().any(|o| internal(&o.name)),
        "{:?}",
        objects.iter().map(|o| &o.name).collect::<Vec<_>>()
    );
    let n: i64 = rows_of(
        s,
        "SELECT COUNT(*) FROM user_tables WHERE table_name LIKE 'VECTOR$IX_VEC_%'",
    )
    .await[0][0]
        .as_i64()
        .unwrap();
    assert!(n >= 2, "the indexes do have internal tables: {n}");
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
    assert_eq!(v.algorithm.as_deref(), Some(algorithm));
    assert_eq!(v.metric.as_deref(), Some("cosine"));
    assert_eq!(
        v.params.get("target_accuracy").map(String::as_str),
        Some("95")
    );
    // Every setting, from Data Pump's view (the login has no DBA role).
    if hnsw_ok {
        assert_eq!(v.params.get("m").map(String::as_str), Some("16"));
        assert_eq!(
            v.params.get("ef_construction").map(String::as_str),
            Some("100")
        );
    } else {
        assert_eq!(v.params.get("lists").map(String::as_str), Some("2"));
    }
    // The table's DDL (DBMS_METADATA) has the vector index as one.
    let def = s.definition(&table("VEC_DOCS")).await.unwrap().unwrap();
    assert!(
        def.contains("CREATE VECTOR INDEX \"") && def.contains("\"IX_VEC_DOCS_EMB\""),
        "{def}"
    );
    assert!(
        !def.contains("CREATE INDEX \""),
        "a B-tree on the vector: {def}"
    );
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

    // Index usage lists them with the same settings as the schema.
    for (table_name, index) in [
        ("VEC_DOCS", "IX_VEC_DOCS_EMB"),
        ("VEC_IVF", "IX_VEC_IVF_EMB"),
    ] {
        let usage = s
            .index_usage(&table(table_name))
            .await
            .unwrap()
            .expect("index usage");
        let u = usage
            .indexes
            .iter()
            .find(|i| i.name == index)
            .unwrap_or_else(|| panic!("{index} in index usage"));
        assert_eq!(u.kind, "VECTOR");
        assert_eq!(u.key_columns[0], "EMB");
        let listed = t(table_name)
            .indexes
            .iter()
            .find(|i| i.name == index)
            .unwrap()
            .vector
            .clone();
        assert!(listed.is_some());
        assert_eq!(u.vector, listed, "{index}");
    }
    // A BINARY column's index without a distance: HAMMING, which Oracle
    // takes there (its default, COSINE, it refuses).
    let bits_table = schema(
        "VEC_BITS",
        vec![
            col("ID", "NUMBER(10)", false),
            col("B", "VECTOR(16, BINARY)", true),
        ],
        vec![vector_index(
            "IX_VEC_BITS_B",
            &["B"],
            VectorIndex {
                algorithm: Some("ivf".into()),
                ..Default::default()
            },
        )],
    );
    let ddl = d.table_ddl(&bits_table, ALL).unwrap();
    assert!(ddl.contains("DISTANCE HAMMING"), "{ddl}");
    run(s, &ddl).await;
    let tables = s.database_schema().await.unwrap();
    let bits_ix = tables
        .iter()
        .find(|t| t.name == "VEC_BITS")
        .unwrap()
        .indexes
        .iter()
        .find(|i| i.name == "IX_VEC_BITS_B")
        .expect("BINARY vector index listed")
        .clone();
    assert_eq!(
        bits_ix.vector.and_then(|v| v.metric).as_deref(),
        Some("hamming")
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
    assert!(!rows.is_empty());

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
    // A BINARY one, by its bits.
    let update = d
        .update_script(
            &table("VEC_KINDS"),
            &[RowChange {
                key: vec![("ID".into(), json!(1))],
                set: vec![(
                    "B".into(),
                    json!([0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0]),
                )],
                ..Default::default()
            }],
        )
        .unwrap();
    run(s, &update).await;
    let rows = rows_of(s, "SELECT B, FROM_VECTOR(B) FROM VEC_KINDS WHERE ID = 1").await;
    assert_eq!(
        rows[0],
        [json!("[0,0,0,0,0,0,0,1,1,0,0,0,0,0,0,0]"), json!("[1,128]")]
    );

    // Typed, with the result's columns: BINARY bits as the grid's text are
    // packed without asking the dictionary, a dense value into the SPARSE
    // column is written in Oracle's sparse form, a sparse one into a dense
    // column spread out (Oracle converts neither).
    let out = run(s, "SELECT ID, B, S, I FROM VEC_KINDS WHERE ID = 1").await;
    let types = out.results[0].columns.clone();
    let typed = d
        .update_script_typed(
            &table("VEC_KINDS"),
            &[RowChange {
                key: vec![("ID".into(), json!(1))],
                set: vec![
                    ("B".into(), json!("[1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1]")),
                    ("S".into(), json!([0, 0, 2.5, 0, -1])),
                    ("I".into(), json!("[4,[0,3],[7,-8]]")),
                ],
                ..Default::default()
            }],
            &types,
        )
        .unwrap();
    assert!(!typed.contains("all_tab_cols"), "{typed}");
    run(s, &typed).await;
    let rows = rows_of(
        s,
        "SELECT B, FROM_VECTOR(B), S, I FROM VEC_KINDS WHERE ID = 1",
    )
    .await;
    assert_eq!(
        rows[0],
        [
            json!("[1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1]"),
            json!("[128,1]"),
            json!("[5,[2,4],[2.5,-1]]"),
            json!("[7,0,0,-8]")
        ]
    );
    // A typed insert from the describe's types (pgvector's sparse text
    // into the SPARSE column), and its delete.
    let described: Vec<dbine_driver::ResultColumn> = s
        .columns(&table("VEC_KINDS"))
        .await
        .unwrap()
        .iter()
        .filter(|c| ["ID", "B", "S"].contains(&c.name.as_str()))
        .map(|c| dbine_driver::ResultColumn::new(c.name.clone(), c.data_type.clone()))
        .collect();
    let insert = d
        .insert_script_typed(
            &table("VEC_KINDS"),
            &described,
            &[vec![json!(99), bits.clone(), json!("{1:0.5,5:2}/5")]],
        )
        .unwrap();
    run(s, &insert).await;
    let rows = rows_of(s, "SELECT B, S FROM VEC_KINDS WHERE ID = 99").await;
    assert_eq!(
        rows[0],
        [
            json!("[1,1,0,0,1,0,0,1,0,0,0,0,1,1,1,1]"),
            json!("[5,[0,4],[0.5,2]]")
        ]
    );
    run(
        s,
        &d.delete_script_typed(
            &table("VEC_KINDS"),
            &[vec![("ID".into(), json!(99))]],
            &described,
        )
        .unwrap(),
    )
    .await;

    // 8. A sync that changes the distance remakes the index.
    let mut new = docs_now.clone();
    new.indexes
        .iter_mut()
        .find(|i| i.name == "IX_VEC_DOCS_EMB")
        .unwrap()
        .vector = Some(designed(hnsw_ok, "l2"));
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
    assert_eq!(read_docs.columns[1].type_name, "VECTOR(3, FLOAT32)");
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
    let all = "F, B, FROM_VECTOR(B), S, I, D, L, Z";
    assert_eq!(
        rows_of(
            s,
            &format!("SELECT ID, {all} FROM VEC_KINDS_COPY ORDER BY ID")
        )
        .await,
        rows_of(s, &format!("SELECT ID, {all} FROM VEC_KINDS ORDER BY ID")).await
    );
    // pgvector's bit text loads into BINARY too.
    let spec = LoadSpec {
        columns: vec!["ID".into(), "B".into()],
        ..spec
    };
    let pg = vec![
        TransferColumn {
            name: "id".into(),
            type_name: "integer".into(),
            nullable: false,
        },
        TransferColumn {
            name: "b".into(),
            type_name: "bit(16)".into(),
            nullable: true,
        },
    ];
    s.bulk_load(
        &spec,
        &pg,
        &mut Batches(
            vec![RowBatch {
                rows: vec![
                    vec![Cell::Int(10), Cell::Text("1100100100001111".into())],
                    vec![
                        Cell::Int(11),
                        Cell::Text("[0,0,0,0,0,0,0,1,0,0,0,0,0,0,1,0]".into()),
                    ],
                ],
                bytes: 64,
            }]
            .into(),
        ),
        &|_| {},
    )
    .await
    .expect("bulk_load bits");
    assert_eq!(
        rows_of(
            s,
            "SELECT FROM_VECTOR(B) FROM VEC_KINDS_COPY WHERE ID >= 10 ORDER BY ID"
        )
        .await,
        [[json!("[201,15]")], [json!("[1,2]")]]
    );

    // 10. A user without roles reads its own index's settings…
    let me = config()
        .username
        .clone()
        .unwrap_or_default()
        .to_ascii_uppercase();
    run(
        s,
        &format!("CREATE USER {PLAIN_USER} IDENTIFIED BY \"{PLAIN_PASSWORD}\" QUOTA 20M ON USERS"),
    )
    .await;
    run(
        s,
        &format!("GRANT CREATE SESSION, CREATE TABLE TO {PLAIN_USER}"),
    )
    .await;
    run(s, &format!("GRANT SELECT ON VEC_DOCS TO {PLAIN_USER}")).await;
    let mut p = d
        .connect(&config_as(PLAIN_USER, PLAIN_PASSWORD), None)
        .await
        .expect("connect as the plain user");
    let own = schema(
        "VEC_OWN",
        vec![
            col("ID", "NUMBER(10)", false),
            col("EMB", "VECTOR(3, FLOAT32)", true),
        ],
        vec![vector_index(
            "IX_VEC_OWN",
            &["EMB"],
            designed(hnsw_ok, "ip"),
        )],
    );
    run(&mut p, &d.table_ddl(&own, ALL).unwrap()).await;
    let mine = p.database_schema().await.unwrap();
    assert_eq!(
        mine.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["VEC_OWN"],
        "no internal tables"
    );
    let v = mine[0].indexes[0].vector.clone().unwrap();
    assert_eq!(v.algorithm.as_deref(), Some(algorithm));
    assert_eq!(v.metric.as_deref(), Some("ip"), "DOT, not COSINE");
    assert_eq!(
        v.params.get("target_accuracy").map(String::as_str),
        Some("95")
    );
    // …and can't read someone else's: the DDL says so instead of making a
    // COSINE index silently.
    drop(p);
    let mut p = d
        .connect(&config_as(PLAIN_USER, PLAIN_PASSWORD), Some(&me))
        .await
        .expect("connect as the plain user, in the test's schema");
    let theirs = p.database_schema().await.unwrap();
    let docs_there = theirs
        .iter()
        .find(|t| t.name == "VEC_DOCS")
        .expect("granted table");
    let ix = docs_there
        .indexes
        .iter()
        .find(|i| i.name == "IX_VEC_DOCS_EMB")
        .unwrap();
    assert_eq!(ix.vector.as_ref().unwrap().metric, None);
    let ddl = d
        .table_ddl(
            docs_there,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        ddl.contains("-- El índice vectorial IX_VEC_DOCS_EMB no indica la distancia"),
        "{ddl}"
    );
}
