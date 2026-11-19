//! Vectors through "Clonar tabla" and across engines, against live servers:
//! a SQL Server 2025 table with a DiskANN index and an Oracle 23ai table
//! with an HNSW (or IVF) index and a BINARY vector are cloned with their
//! rows and indexes; and a pgvector table goes to SQL Server, from there to
//! Oracle and back to pgvector through the drivers' typed reads and loads,
//! each step checked against the source's values. Each test drops what it
//! made, first and at the end.
//!
//! Ignored by default; each test runs when its servers' variables are set:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER2025_URL='mssql://sa:Pw_12345!@localhost:27202' \
//! DBINE_TEST_ORACLE_URL=oracle://dbine:Dbine123@localhost:25601/FREEPDB1 \
//! DBINE_TEST_PGVECTOR_URL=postgres://postgres:pw@localhost:27200/postgres \
//!   cargo test -p dbine-transfer --test vectors_mssql_oracle -- --ignored --nocapture --test-threads=1
//! ```

use dbine_driver::transfer::{
    BatchSink, BatchSource, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{async_trait, ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use dbine_transfer::clone_table::{
    clone_table, CloneControl, CloneOptions, CloneReport, CloneRequest, ConfigEndpoints,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

const SS_DB: &str = "dbine_vec_clone";

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_drivers::find(id)
        .unwrap_or_else(|| panic!("{id} driver"))
        .clone()
}

fn sqlserver() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_SQLSERVER2025_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

fn oracle() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ORACLE_URL").ok()?;
    let rest = url.strip_prefix("oracle://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, service) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    Some(cfg)
}

fn pgvector() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_PGVECTOR_URL").ok()?;
    let rest = url.strip_prefix("postgres://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, db) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    Some(ConnectionConfig {
        driver: "postgres".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        database: db.into(),
        ..Default::default()
    })
}

async fn try_run(s: &mut dyn Session, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out)
        .await
        .map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

async fn run(s: &mut dyn Session, sql: &str) -> QueryOutcome {
    try_run(s, sql)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
}

/// The last result's rows.
async fn rows(s: &mut dyn Session, sql: &str) -> Vec<Vec<Value>> {
    run(s, sql)
        .await
        .results
        .into_iter()
        .rev()
        .find(|r| !r.columns.is_empty())
        .map(|r| r.rows)
        .unwrap_or_default()
}

/// A vector's text in the canonical form (float32 values, shortest).
fn canonical(v: &Value) -> Value {
    match v.as_str().and_then(dbine_driver::vector::parse) {
        Some(x) => dbine_driver::vector::text(x.into_iter().map(|f| f as f32)).into(),
        None => v.clone(),
    }
}

/// `(id, vector)` rows of any engine: the id as a number, the vector in
/// the canonical form.
fn id_and_vector(rows: Vec<Vec<Value>>) -> Vec<(i64, Value)> {
    rows.into_iter()
        .map(|r| {
            let id = r[0]
                .as_i64()
                .or_else(|| r[0].as_str().and_then(|s| s.parse().ok()))
                .expect("id");
            (id, canonical(&r[1]))
        })
        .collect()
}

async fn clone(
    endpoints: Arc<ConfigEndpoints>,
    source: ObjectRef,
    name: &str,
) -> Result<CloneReport, String> {
    clone_table(
        endpoints as Arc<dyn dbine_transfer::Endpoints>,
        CloneRequest {
            source,
            new_name: name.into(),
            options: CloneOptions {
                with_data: true,
                with_indexes: true,
            },
        },
        &CloneControl::default(),
        |e| println!("{e:?}"),
    )
    .await
    .map_err(|e| e.to_string())
}

fn table(schema: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: schema.map(Into::into),
        name: name.into(),
    }
}

// ------------------------------------------------------------ SQL Server

async fn sqlserver_db(cfg: &ConnectionConfig, create: bool) {
    let mut m = driver("sqlserver")
        .connect(cfg, Some("master"))
        .await
        .expect("connect");
    run(&mut *m, &format!("IF DB_ID('{SS_DB}') IS NOT NULL BEGIN ALTER DATABASE [{SS_DB}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{SS_DB}]; END")).await;
    if create {
        run(&mut *m, &format!("CREATE DATABASE [{SS_DB}]")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs SQL Server 2025: see the top of the file"]
async fn sqlserver_clone_keeps_vectors_and_their_index() {
    let Some(cfg) = sqlserver() else {
        eprintln!("DBINE_TEST_SQLSERVER2025_URL not set; skipping");
        return;
    };
    sqlserver_db(&cfg, true).await;
    let c = cfg.clone();
    let res = tokio::spawn(async move { sqlserver_clone(&c).await }).await;
    sqlserver_db(&cfg, false).await;
    if let Err(e) = res {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn sqlserver_clone(cfg: &ConnectionConfig) {
    let d = driver("sqlserver");
    let mut s = d.connect(cfg, Some(SS_DB)).await.unwrap();
    run(
        &mut *s,
        "ALTER DATABASE SCOPED CONFIGURATION SET PREVIEW_FEATURES = ON",
    )
    .await;
    run(
        &mut *s,
        "CREATE TABLE dbo.docs (id int NOT NULL CONSTRAINT pk_docs PRIMARY KEY, emb vector(3) NULL, note nvarchar(20) NULL)",
    )
    .await;
    let values: Vec<String> = (1..=40)
        .map(|i| format!("({i}, '[{}, {}, 0.125]', N'n{i}')", i as f64 * 0.5, -i))
        .chain(std::iter::once("(41, NULL, NULL)".to_string()))
        .collect();
    run(
        &mut *s,
        &format!("INSERT INTO dbo.docs VALUES {}", values.join(", ")),
    )
    .await;
    run(
        &mut *s,
        "CREATE VECTOR INDEX ix_docs_emb ON dbo.docs (emb) WITH (METRIC = 'dot', TYPE = 'DiskANN')",
    )
    .await;
    let endpoints = Arc::new(ConfigEndpoints {
        driver: d.clone(),
        config: cfg.clone(),
        database: Some(SS_DB.into()),
    });
    let report = clone(endpoints, table(Some("dbo"), "docs"), "docs_clone")
        .await
        .unwrap_or_else(|e| panic!("clone failed: {e}"));
    println!("{report:?}");
    assert_eq!(report.rows, 41);
    let mut s = d.connect(cfg, Some(SS_DB)).await.unwrap();
    let cols = s.columns(&table(Some("dbo"), "docs_clone")).await.unwrap();
    assert_eq!(cols[1].data_type, "vector(3)");
    let all = |t: &str| format!("SELECT id, emb, note FROM dbo.{t} ORDER BY id");
    let a = rows(&mut *s, &all("docs")).await;
    let b = rows(&mut *s, &all("docs_clone")).await;
    assert_eq!(a.len(), 41);
    assert_eq!(a, b);
    assert_eq!(b[0][1], Value::from("[0.5,-1,0.125]"));
    let tables = s.database_schema().await.unwrap();
    let t = tables.iter().find(|t| t.name == "docs_clone").unwrap();
    let ix = t
        .indexes
        .iter()
        .find(|i| i.vector.is_some())
        .expect("the clone has the vector index");
    let v = ix.vector.clone().unwrap();
    assert_eq!(
        (v.algorithm.as_deref(), v.metric.as_deref()),
        (Some("diskann"), Some("ip"))
    );
    assert_eq!(ix.columns, ["emb"]);
}

// ---------------------------------------------------------------- Oracle

const ORA_TABLES: [&str; 2] = ["VEC_CLON", "VEC_CLON_COPY"];

async fn oracle_drop(cfg: &ConnectionConfig) {
    let mut s = driver("oracle").connect(cfg, None).await.expect("connect");
    for t in ORA_TABLES {
        let _ = try_run(&mut *s, &format!("DROP TABLE {t} PURGE")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Oracle 23ai: see the top of the file"]
async fn oracle_clone_keeps_vectors_and_their_index() {
    let Some(cfg) = oracle() else {
        eprintln!("DBINE_TEST_ORACLE_URL not set; skipping");
        return;
    };
    oracle_drop(&cfg).await;
    let c = cfg.clone();
    let res = tokio::spawn(async move { oracle_clone(&c).await }).await;
    oracle_drop(&cfg).await;
    if let Err(e) = res {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn oracle_clone(cfg: &ConnectionConfig) {
    let d = driver("oracle");
    let mut s = d.connect(cfg, None).await.unwrap();
    run(
        &mut *s,
        "CREATE TABLE VEC_CLON (ID NUMBER(10) PRIMARY KEY, EMB VECTOR(3, FLOAT32), B VECTOR(16, BINARY), NOTE VARCHAR2(20))",
    )
    .await;
    for i in 1..=30 {
        run(
            &mut *s,
            &format!(
                "INSERT INTO VEC_CLON VALUES ({i}, '[{}, {}, 0.125]', '[{}, 15]', 'n{i}')",
                i as f64 * 0.5,
                -i,
                i * 7 % 256
            ),
        )
        .await;
    }
    run(
        &mut *s,
        "INSERT INTO VEC_CLON VALUES (31, NULL, NULL, NULL)",
    )
    .await;
    // HNSW needs the vector memory pool; IVF where the instance has none.
    let hnsw = "CREATE VECTOR INDEX IX_VEC_CLON ON VEC_CLON (EMB) ORGANIZATION INMEMORY NEIGHBOR GRAPH DISTANCE DOT WITH TARGET ACCURACY 92";
    let algorithm = match try_run(&mut *s, hnsw).await {
        Ok(_) => "hnsw",
        Err(e) if e.contains("ORA-51962") => {
            eprintln!("NOTE: no vector memory pool (ORA-51962): an IVF index instead");
            run(&mut *s, "CREATE VECTOR INDEX IX_VEC_CLON ON VEC_CLON (EMB) ORGANIZATION NEIGHBOR PARTITIONS DISTANCE DOT WITH TARGET ACCURACY 92").await;
            "ivf"
        }
        Err(e) => panic!("{e}"),
    };
    let endpoints = Arc::new(ConfigEndpoints {
        driver: d.clone(),
        config: cfg.clone(),
        database: None,
    });
    let report = clone(endpoints, table(None, "VEC_CLON"), "VEC_CLON_COPY")
        .await
        .unwrap_or_else(|e| panic!("clone failed: {e}"));
    println!("{report:?}");
    assert_eq!(report.rows, 31);
    let mut s = d.connect(cfg, None).await.unwrap();
    let cols = s.columns(&table(None, "VEC_CLON_COPY")).await.unwrap();
    let types: Vec<&str> = cols.iter().map(|c| c.data_type.as_str()).collect();
    assert_eq!(
        types,
        [
            "NUMBER(10)",
            "VECTOR(3, FLOAT32)",
            "VECTOR(16, BINARY)",
            "VARCHAR2(20)"
        ]
    );
    let all = |t: &str| format!("SELECT ID, EMB, B, FROM_VECTOR(B), NOTE FROM {t} ORDER BY ID");
    let a = rows(&mut *s, &all("VEC_CLON")).await;
    let b = rows(&mut *s, &all("VEC_CLON_COPY")).await;
    assert_eq!(a.len(), 31);
    assert_eq!(a, b);
    assert_eq!(b[0][1], Value::from("[0.5,-1,0.125]"));
    assert_eq!(b[0][3], Value::from("[7,15]"));
    let tables = s.database_schema().await.unwrap();
    assert!(
        !tables.iter().any(|t| t.name.starts_with("VECTOR$")),
        "internal tables listed"
    );
    let t = tables.iter().find(|t| t.name == "VEC_CLON_COPY").unwrap();
    let ix = t
        .indexes
        .iter()
        .find(|i| i.vector.is_some())
        .expect("the clone has the vector index");
    let v = ix.vector.clone().unwrap();
    assert_eq!(
        (v.algorithm.as_deref(), v.metric.as_deref()),
        (Some(algorithm), Some("ip"))
    );
    assert_eq!(
        v.params.get("target_accuracy").map(String::as_str),
        Some("92")
    );
}

// ------------------------------------------------- across engines

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

/// `t` read with the driver's typed read, then loaded into `into`.
async fn move_rows(
    from: &mut dyn Session,
    t: ObjectRef,
    into: &mut dyn Session,
    target: ObjectRef,
    columns: &[&str],
) -> u64 {
    let sink = Arc::new(Mutex::new(Collect::default()));
    from.read_batches(
        &ReadSpec {
            table: t,
            columns: None,
            filter: None,
        },
        sink.clone(),
    )
    .await
    .expect("read");
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let spec = LoadSpec {
        table: target,
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    into.bulk_load(
        &spec,
        &got.columns,
        &mut Batches(got.batches.into()),
        &|_| {},
    )
    .await
    .expect("load")
}

async fn cross_drop(pg: &mut dyn Session, ss: &ConnectionConfig, ora: &mut dyn Session) {
    let _ = try_run(pg, "DROP TABLE IF EXISTS xfam_src, xfam_back").await;
    sqlserver_db(ss, false).await;
    let _ = try_run(ora, "DROP TABLE XFAM_V PURGE").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector, SQL Server 2025 and Oracle 23ai: see the top of the file"]
async fn pgvector_to_sqlserver_to_oracle_and_back() {
    let (Some(pgc), Some(ssc), Some(orac)) = (pgvector(), sqlserver(), oracle()) else {
        eprintln!("DBINE_TEST_PGVECTOR_URL, DBINE_TEST_SQLSERVER2025_URL or DBINE_TEST_ORACLE_URL not set; skipping");
        return;
    };
    let mut pg = driver("postgres").connect(&pgc, None).await.expect("pg");
    let mut ora = driver("oracle").connect(&orac, None).await.expect("oracle");
    cross_drop(&mut *pg, &ssc, &mut *ora).await;
    sqlserver_db(&ssc, true).await;
    let ss = ssc.clone();
    let res = tokio::spawn(async move {
        let mut pg = driver("postgres").connect(&pgc, None).await.expect("pg");
        let mut ora = driver("oracle").connect(&orac, None).await.expect("oracle");
        let mut sql = driver("sqlserver")
            .connect(&ss, Some(SS_DB))
            .await
            .expect("sqlserver");
        cross(&mut *pg, &mut *sql, &mut *ora).await;
    })
    .await;
    cross_drop(&mut *pg, &ssc, &mut *ora).await;
    if let Err(e) = res {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn cross(pg: &mut dyn Session, ss: &mut dyn Session, ora: &mut dyn Session) {
    run(pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
    run(
        pg,
        "CREATE TABLE xfam_src (id int PRIMARY KEY, emb vector(3)); CREATE TABLE xfam_back (id int PRIMARY KEY, emb vector(3))",
    )
    .await;
    run(
        pg,
        "INSERT INTO xfam_src VALUES (1, '[0.1,0.2,0.3]'), (2, '[1,-2.5,3e-7]'), (3, NULL), (4, '[-0.0001,123456.7,0]')",
    )
    .await;
    run(
        ss,
        "CREATE TABLE dbo.xfam_v (id int PRIMARY KEY, emb vector(3))",
    )
    .await;
    run(
        ora,
        "CREATE TABLE XFAM_V (ID NUMBER(10) PRIMARY KEY, EMB VECTOR(3, FLOAT32))",
    )
    .await;
    let want = id_and_vector(rows(pg, "SELECT id, emb::text FROM xfam_src ORDER BY id").await);
    assert_eq!(want[1].1, Value::from("[1,-2.5,0.0000003]"));

    // pgvector → SQL Server
    let n = move_rows(
        pg,
        table(Some("public"), "xfam_src"),
        ss,
        table(Some("dbo"), "xfam_v"),
        &["id", "emb"],
    )
    .await;
    assert_eq!(n, 4);
    let got = rows(ss, "SELECT id, emb FROM dbo.xfam_v ORDER BY id").await;
    assert_eq!(id_and_vector(got), want, "SQL Server");

    // SQL Server → Oracle
    let n = move_rows(
        ss,
        table(Some("dbo"), "xfam_v"),
        ora,
        table(None, "XFAM_V"),
        &["ID", "EMB"],
    )
    .await;
    assert_eq!(n, 4);
    let got = rows(ora, "SELECT ID, EMB FROM XFAM_V ORDER BY ID").await;
    assert_eq!(id_and_vector(got), want, "Oracle");

    // Oracle → pgvector
    let n = move_rows(
        ora,
        table(None, "XFAM_V"),
        pg,
        table(Some("public"), "xfam_back"),
        &["id", "emb"],
    )
    .await;
    assert_eq!(n, 4);
    let back = id_and_vector(rows(pg, "SELECT id, emb::text FROM xfam_back ORDER BY id").await);
    assert_eq!(back, want, "back in pgvector");
}

// ------------------------------------------- sparse and bit vectors, pgvector

/// pgvector's `sparsevec` and `bit` columns into Oracle's SPARSE and BINARY
/// vectors: Oracle's loads take pgvector's sparse text and bit strings, its
/// reads give its sparse text and one 0 / 1 per dimension, which pgvector's
/// `bit` load takes back. (Its `sparsevec` load doesn't read Oracle's
/// sparse text yet, so the sparse column doesn't make the way back.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and Oracle 23ai: see the top of the file"]
async fn pgvector_sparse_and_bit_vectors_to_oracle_and_back() {
    let (Some(pgc), Some(orac)) = (pgvector(), oracle()) else {
        eprintln!("DBINE_TEST_PGVECTOR_URL or DBINE_TEST_ORACLE_URL not set; skipping");
        return;
    };
    let mut pg = driver("postgres").connect(&pgc, None).await.expect("pg");
    let mut ora = driver("oracle").connect(&orac, None).await.expect("oracle");
    sparse_and_bits_drop(&mut *pg, &mut *ora).await;
    let res = tokio::spawn(async move {
        let mut pg = driver("postgres").connect(&pgc, None).await.expect("pg");
        let mut ora = driver("oracle").connect(&orac, None).await.expect("oracle");
        sparse_and_bits(&mut *pg, &mut *ora).await;
    })
    .await;
    sparse_and_bits_drop(&mut *pg, &mut *ora).await;
    if let Err(e) = res {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn sparse_and_bits_drop(pg: &mut dyn Session, ora: &mut dyn Session) {
    let _ = try_run(pg, "DROP TABLE IF EXISTS xfam_sb, xfam_sb_back").await;
    let _ = try_run(ora, "DROP TABLE XFAM_SB PURGE").await;
}

async fn sparse_and_bits(pg: &mut dyn Session, ora: &mut dyn Session) {
    run(pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
    run(
        pg,
        "CREATE TABLE xfam_sb (id int PRIMARY KEY, sp sparsevec(5), b bit(16)); \
         CREATE TABLE xfam_sb_back (id int PRIMARY KEY, sp sparsevec(5), b bit(16))",
    )
    .await;
    run(
        pg,
        "INSERT INTO xfam_sb VALUES (1, '{1:1.5,4:-2}/5', B'1100100100001111'), (2, '{}/5', B'0000000000000001'), (3, NULL, NULL)",
    )
    .await;
    run(
        ora,
        "CREATE TABLE XFAM_SB (ID NUMBER(10) PRIMARY KEY, SP VECTOR(5, FLOAT32, SPARSE), B VECTOR(16, BINARY))",
    )
    .await;

    // pgvector → Oracle
    let n = move_rows(
        pg,
        table(Some("public"), "xfam_sb"),
        ora,
        table(None, "XFAM_SB"),
        &["ID", "SP", "B"],
    )
    .await;
    assert_eq!(n, 3);
    let got = rows(ora, "SELECT ID, SP, B FROM XFAM_SB ORDER BY ID").await;
    assert_eq!(got[0][1], Value::from("[5,[0,3],[1.5,-2]]"));
    assert_eq!(got[0][2], Value::from("[1,1,0,0,1,0,0,1,0,0,0,0,1,1,1,1]"));
    assert_eq!(got[1][1], Value::from("[5,[],[]]"));
    assert_eq!(got[2][1], Value::Null);
    let packed = rows(ora, "SELECT FROM_VECTOR(B) FROM XFAM_SB WHERE ID = 1").await;
    assert_eq!(packed[0][0], Value::from("[201,15]"));

    // Oracle → pgvector: the BINARY vectors.
    run(ora, "ALTER TABLE XFAM_SB DROP COLUMN SP").await;
    let n = move_rows(
        ora,
        table(None, "XFAM_SB"),
        pg,
        table(Some("public"), "xfam_sb_back"),
        &["id", "b"],
    )
    .await;
    assert_eq!(n, 3);
    let back = rows(pg, "SELECT id, b::text FROM xfam_sb_back ORDER BY id").await;
    let src = rows(pg, "SELECT id, b::text FROM xfam_sb ORDER BY id").await;
    assert_eq!(back, src, "back in pgvector");
}
