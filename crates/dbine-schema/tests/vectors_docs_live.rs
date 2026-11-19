//! Vectors from pgvector into the document engines through the schema
//! conversion, against live servers: a pgvector table with vector indexes
//! is read with its driver, converted for MongoDB (Atlas Local), Cosmos DB
//! (the emulator) and Couchbase (8.0), and created there by the target
//! driver's own DDL, as the IDE's "convert and copy" does. Each test checks
//! what the conversion decided (vector types, each vector index's mapping
//! with the column it searches, the one the target can't hold dropped with
//! its reason), that the target took it (described back with the vector
//! type and index settings), then copies the rows through the drivers'
//! typed reads and loads and searches them with the converted index. Each
//! test drops what it made, first and at the end.
//!
//! Ignored by default; each test runs when its servers' variables are set:
//!
//! ```sh
//! DBINE_TEST_PGVECTOR_URL=postgres://postgres:pw@localhost:27200/postgres \
//! DBINE_TEST_ATLAS_URL='mongodb://localhost:27205/?directConnection=true' \
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25213 \
//! DBINE_TEST_COUCHBASE_EE_URL=http://localhost:25993 DBINE_TEST_COUCHBASE_EE_MGMT_PORT=25991 \
//!   cargo test -p dbine-schema --test vectors_docs_live -- --ignored --nocapture --test-threads=1
//! ```

use dbine_driver::transfer::{
    BatchSink, BatchSource, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    async_trait, ConnectionConfig, DdlParts, Driver, IndexDef, ObjectRef, QueryOutcome, Session,
    TableSchema,
};
use dbine_schema::{convert, Conversion, IssueCode, Options};
use serde_json::Value;
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DB: &str = "dbine_xconv";
const SRC: &str = "xconv_src";

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_drivers::find(id)
        .unwrap_or_else(|| panic!("{id} driver"))
        .clone()
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

fn atlas() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ATLAS_URL").ok()?;
    let mut c = ConnectionConfig {
        driver: "mongodb".into(),
        database: DB.into(),
        ..Default::default()
    };
    c.options.insert("connection_string".into(), url);
    Some(c)
}

fn cosmos() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_COSMOSDB_URL").ok()?;
    let mut c = ConnectionConfig {
        driver: "cosmosdb".into(),
        host: url,
        trust_server_certificate: true,
        ..Default::default()
    };
    // The emulator's well-known key.
    c.options.insert(
        "account_key".into(),
        std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| {
            "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==".into()
        }),
    );
    Some(c)
}

fn couchbase(database: &str) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_COUCHBASE_EE_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (host, port) = rest.trim_end_matches('/').rsplit_once(':')?;
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some("Administrator".into()),
        password: Some("secreto1".into()),
        database: database.into(),
        ..Default::default()
    };
    c.options.insert(
        "mgmt_port".into(),
        std::env::var("DBINE_TEST_COUCHBASE_EE_MGMT_PORT").unwrap_or_else(|_| "8091".into()),
    );
    Some(c)
}

async fn try_run(s: &mut dyn Session, text: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(text, 10_000, &mut out)
        .await
        .map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

async fn run(s: &mut dyn Session, text: &str) -> QueryOutcome {
    try_run(s, text)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{text}"))
}

/// Retries `text` for up to 90 s (a new bucket, collection or index takes
/// a moment to be usable).
async fn run_settled(s: &mut dyn Session, text: &str) -> QueryOutcome {
    let start = Instant::now();
    loop {
        match try_run(s, text).await {
            Ok(out) => return out,
            Err(e) if start.elapsed() < Duration::from_secs(90) => {
                eprintln!("retrying: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => panic!("{e}\n{text}"),
        }
    }
}

/// The last result with columns.
async fn result(s: &mut dyn Session, text: &str) -> dbine_driver::StatementResult {
    run(s, text)
        .await
        .results
        .into_iter()
        .rev()
        .find(|r| !r.columns.is_empty())
        .expect("a result")
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

const COLUMNS: [&str; 4] = ["id", "cat", "emb", "h"];

/// The source's rows, read with its driver's typed read and loaded into
/// `target` (as the copy after a conversion does).
async fn copy_rows(pg: &mut dyn Session, into: &mut dyn Session, target: ObjectRef) -> u64 {
    let sink = Arc::new(Mutex::new(Collect::default()));
    pg.read_batches(
        &ReadSpec {
            table: ObjectRef {
                kind: "table".into(),
                schema: Some("public".into()),
                name: SRC.into(),
            },
            columns: Some(COLUMNS.iter().map(|c| c.to_string()).collect()),
            filter: None,
        },
        sink.clone(),
    )
    .await
    .expect("read");
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let spec = LoadSpec {
        table: target,
        columns: COLUMNS.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: true,
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

/// The source table in pgvector: a `vector(3)` searched by an l2 HNSW
/// index (with its parameters) and by an l1 one (which no document engine
/// has), and a `halfvec(3)` searched by cosine. Read back by its driver,
/// as the conversion gets it.
async fn pg_source(pg: &mut dyn Session) -> TableSchema {
    run(pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
    run(pg, &format!("DROP TABLE IF EXISTS {SRC}")).await;
    run(
        pg,
        &format!(
            "CREATE TABLE {SRC} (id int PRIMARY KEY, cat text, emb vector(3), h halfvec(3));
             CREATE INDEX xconv_emb_l2 ON {SRC} USING hnsw (emb vector_l2_ops) WITH (m = 16, ef_construction = 64);
             CREATE INDEX xconv_emb_l1 ON {SRC} USING hnsw (emb vector_l1_ops);
             CREATE INDEX xconv_h_cos ON {SRC} USING hnsw (h halfvec_cosine_ops);
             INSERT INTO {SRC} VALUES
               (1, 'a', '[0.1,0.2,0.3]', '[1,0,0]'),
               (2, 'b', '[1,-2.5,3]', '[0,1,0]'),
               (3, 'a', '[0.9,0.1,0]', '[0.5,0.5,0]')"
        ),
    )
    .await;
    pg.database_schema()
        .await
        .expect("pgvector schema")
        .into_iter()
        .find(|t| t.name == SRC)
        .expect("the source table")
}

/// What the conversion must decide for every document engine: both vector
/// columns stay vectors with their dimension, the l2 and cosine indexes
/// carry over (each mapped with the column it searches), and the l1 one is
/// dropped with its reason.
fn check_conversion(c: &Conversion, target: &str) -> TableSchema {
    let t = c.tables[0].clone();
    let ty = |n: &str| {
        t.columns
            .iter()
            .find(|c| c.name == n)
            .unwrap_or_else(|| panic!("{target}: column {n}"))
            .data_type
            .clone()
    };
    assert_eq!(ty("emb"), "vector(3)", "{target}");
    assert_eq!(ty("h"), "vector(3)", "{target}");
    let vix = |n: &str| t.indexes.iter().find(|i| i.name.ends_with(n)).cloned();
    let l2 = vix("emb_l2").unwrap_or_else(|| panic!("{target}: {:?}", c.issues));
    assert_eq!(l2.columns, ["emb"], "{target}");
    assert_eq!(
        l2.vector.as_ref().unwrap().metric.as_deref(),
        Some("l2"),
        "{target}"
    );
    let cos = vix("h_cos").unwrap_or_else(|| panic!("{target}: {:?}", c.issues));
    assert_eq!(
        cos.vector.as_ref().unwrap().metric.as_deref(),
        Some("cosine"),
        "{target}"
    );
    assert!(vix("emb_l1").is_none(), "{target}: {:?}", t.indexes);
    assert!(
        c.issues.iter().any(|i| i.code == IssueCode::IndexDropped
            && i.object.as_deref().is_some_and(|o| o.ends_with("emb_l1"))
            && i.message.contains("«l1»")),
        "{target}: {:?}",
        c.issues
    );
    t
}

/// The converted table as the target describes it back: the vector types
/// with their dimension, and both vector indexes with their metric.
fn check_described(back: &TableSchema, converted: &TableSchema, target: &str) {
    for col in ["emb", "h"] {
        let c = back
            .columns
            .iter()
            .find(|c| c.name == col)
            .unwrap_or_else(|| panic!("{target}: {col} in {:?}", back.columns));
        assert!(c.data_type.contains("vector(3"), "{target}: {c:?}");
    }
    for ix in converted.indexes.iter().filter(|i| i.vector.is_some()) {
        let got = back
            .indexes
            .iter()
            .find(|i| {
                i.name == ix.name || i.columns.first() == ix.columns.first() && i.vector.is_some()
            })
            .unwrap_or_else(|| panic!("{target}: {} in {:?}", ix.name, back.indexes));
        assert_eq!(
            got.vector.as_ref().and_then(|v| v.metric.clone()),
            ix.vector.as_ref().and_then(|v| v.metric.clone()),
            "{target}: {}",
            ix.name
        );
    }
}

const CREATE: DdlParts = DdlParts {
    drop: false,
    if_exists: false,
    create: true,
    indexes: true,
    foreign_keys: false,
};

/// Runs `body`, then `cleanup` whether it failed or not.
async fn guarded<F, C>(body: F, cleanup: C)
where
    F: std::future::Future<Output = ()> + Send + 'static,
    C: std::future::Future<Output = ()>,
{
    let res = tokio::spawn(body).await;
    cleanup.await;
    if let Err(e) = res {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn pg_drop(pgc: &ConnectionConfig) {
    let mut pg = driver("postgres").connect(pgc, None).await.expect("pg");
    let _ = try_run(&mut *pg, &format!("DROP TABLE IF EXISTS {SRC}")).await;
}

fn index_named<'a>(t: &'a TableSchema, suffix: &str) -> &'a IndexDef {
    t.indexes
        .iter()
        .find(|i| i.name.ends_with(suffix))
        .unwrap_or_else(|| panic!("{suffix} in {:?}", t.indexes))
}

// ---------------------------------------------------------------- MongoDB

async fn mongo_drop(pgc: &ConnectionConfig, mc: &ConnectionConfig) {
    pg_drop(pgc).await;
    let mut m = driver("mongodb")
        .connect(mc, Some(DB))
        .await
        .expect("mongodb");
    let _ = try_run(&mut *m, "db.dropDatabase()").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and Atlas Local: see the top of the file"]
async fn pgvector_converted_into_atlas() {
    let (Some(pgc), Some(mc)) = (pgvector(), atlas()) else {
        eprintln!("DBINE_TEST_PGVECTOR_URL or DBINE_TEST_ATLAS_URL not set; skipping");
        return;
    };
    mongo_drop(&pgc, &mc).await;
    let (p, m) = (pgc.clone(), mc.clone());
    guarded(async move { mongo(&p, &m).await }, mongo_drop(&pgc, &mc)).await;
}

async fn mongo(pgc: &ConnectionConfig, mc: &ConnectionConfig) {
    let mut pg = driver("postgres").connect(pgc, None).await.expect("pg");
    let src = pg_source(&mut *pg).await;
    let c = convert(
        std::slice::from_ref(&src),
        "postgres",
        "mongodb",
        &Options::default(),
    )
    .expect("conversion");
    let t = check_conversion(&c, "MongoDB");
    // The HNSW parameters Atlas takes carry over.
    let l2 = index_named(&t, "emb_l2").vector.clone().unwrap();
    assert_eq!(l2.params.get("m").map(String::as_str), Some("16"));
    let d = driver("mongodb");
    let ddl = d.table_ddl(&t, CREATE).unwrap();
    assert!(ddl.contains("createSearchIndex"), "{ddl}");
    // pgvector's ef_construction (64) is under Atlas's least
    // numEdgeCandidates: the driver writes 100 and says so.
    assert!(
        ddl.contains("\"numEdgeCandidates\":100") && ddl.contains("ef_construction = 64 queda 100"),
        "{ddl}"
    );
    let mut m = d.connect(mc, Some(DB)).await.expect("mongodb");
    run(&mut *m, &ddl).await;
    let coll = ObjectRef {
        kind: "collection".into(),
        schema: None,
        name: t.name.clone(),
    };
    assert_eq!(copy_rows(&mut *pg, &mut *m, coll).await, 3);

    // Described back with the vector types and both indexes.
    let back = m
        .database_schema()
        .await
        .unwrap()
        .into_iter()
        .find(|x| x.name == t.name)
        .expect("the collection");
    check_described(&back, &t, "MongoDB");

    // Searched with the converted l2 index, once mongot has built it.
    let l2_name = index_named(&t, "emb_l2").name.clone();
    let query = format!(
        "db.getCollection(\"{}\").aggregate([{{ $vectorSearch: {{ index: \"{l2_name}\", path: \"emb\", queryVector: [0.9, 0.1, 0], numCandidates: 10, limit: 1 }} }}, {{ $project: {{ _id: 0, id: 1 }} }}])",
        t.name
    );
    let start = Instant::now();
    let ids = loop {
        match try_run(&mut *m, &query).await {
            Ok(o) if o.results.first().is_some_and(|r| !r.rows.is_empty()) => {
                break o.results[0].rows.clone()
            }
            _ if start.elapsed() < Duration::from_secs(180) => {
                tokio::time::sleep(Duration::from_secs(1)).await
            }
            other => panic!("the vector search never answered: {other:?}"),
        }
    };
    assert_eq!(ids[0][0], Value::from(3));
}

// ---------------------------------------------------------------- Cosmos DB

async fn cosmos_drop(pgc: &ConnectionConfig, cc: &ConnectionConfig) {
    pg_drop(pgc).await;
    let mut admin = driver("cosmosdb")
        .connect(cc, None)
        .await
        .expect("cosmosdb");
    let _ = admin.drop_database(DB).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and the Cosmos DB emulator: see the top of the file"]
async fn pgvector_converted_into_cosmosdb() {
    let (Some(pgc), Some(cc)) = (pgvector(), cosmos()) else {
        eprintln!("DBINE_TEST_PGVECTOR_URL or DBINE_TEST_COSMOSDB_URL not set; skipping");
        return;
    };
    cosmos_drop(&pgc, &cc).await;
    let (p, c) = (pgc.clone(), cc.clone());
    guarded(
        async move { cosmos_db(&p, &c).await },
        cosmos_drop(&pgc, &cc),
    )
    .await;
}

async fn cosmos_db(pgc: &ConnectionConfig, cc: &ConnectionConfig) {
    let mut pg = driver("postgres").connect(pgc, None).await.expect("pg");
    let src = pg_source(&mut *pg).await;
    let c = convert(
        std::slice::from_ref(&src),
        "postgres",
        "cosmosdb",
        &Options::default(),
    )
    .expect("conversion");
    let mut t = check_conversion(&c, "Cosmos DB");
    // The emulator takes diskANN only (pgvector's hnsw isn't Cosmos DB's:
    // the driver's default).
    for ix in t.indexes.iter_mut().filter(|i| i.vector.is_some()) {
        assert_eq!(ix.vector.as_ref().unwrap().algorithm, None, "{ix:?}");
    }
    t.options
        .entry("partition_key".into())
        .or_insert_with(|| "/id".into());
    let d = driver("cosmosdb");
    let ddl = d.table_ddl(&t, CREATE).unwrap();
    assert!(
        ddl.contains("vectorEmbeddingPolicy") && ddl.contains("diskANN"),
        "{ddl}"
    );
    let mut admin = d.connect(cc, None).await.expect("cosmosdb");
    admin.create_database(DB).await.expect("database");
    let mut s = d.connect(cc, Some(DB)).await.expect("cosmosdb");
    run(&mut *s, &ddl).await;
    let coll = ObjectRef {
        kind: "collection".into(),
        schema: None,
        name: t.name.clone(),
    };
    assert_eq!(copy_rows(&mut *pg, &mut *s, coll).await, 3);

    let back = s
        .database_schema()
        .await
        .unwrap()
        .into_iter()
        .find(|x| x.name == t.name)
        .expect("the container");
    check_described(&back, &t, "Cosmos DB");

    // Searched with the embedding's distance (l2: euclidean). (The
    // emulator's backend aborts on TOP with a WHERE and VectorDistance:
    // no filter.)
    let near = result(
        &mut *s,
        &format!(
            "-- container: {}\nSELECT TOP 1 c.id FROM c ORDER BY VectorDistance(c.emb, [0.9, 0.1, 0])",
            t.name
        ),
    )
    .await;
    assert_eq!(near.rows[0][0].to_string().trim_matches('"'), "3");
}

// ---------------------------------------------------------------- Couchbase

async fn couchbase_drop(pgc: &ConnectionConfig, cb: &ConnectionConfig) {
    pg_drop(pgc).await;
    let mut admin = driver("couchbase")
        .connect(cb, None)
        .await
        .expect("couchbase");
    let _ = admin.drop_database(DB).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and Couchbase Server EE 8.0: see the top of the file"]
async fn pgvector_converted_into_couchbase() {
    let (Some(pgc), Some(cb)) = (pgvector(), couchbase("")) else {
        eprintln!("DBINE_TEST_PGVECTOR_URL or DBINE_TEST_COUCHBASE_EE_URL not set; skipping");
        return;
    };
    couchbase_drop(&pgc, &cb).await;
    let (p, c) = (pgc.clone(), cb.clone());
    guarded(
        async move { couchbase_db(&p, &c).await },
        couchbase_drop(&pgc, &cb),
    )
    .await;
}

async fn couchbase_db(pgc: &ConnectionConfig, cb: &ConnectionConfig) {
    let mut pg = driver("postgres").connect(pgc, None).await.expect("pg");
    let src = pg_source(&mut *pg).await;
    let scope = format!("{DB}._default");
    let opts = Options {
        target_schema: Some(scope.clone()),
        ..Default::default()
    };
    let c =
        convert(std::slice::from_ref(&src), "postgres", "couchbase", &opts).expect("conversion");
    let t = check_conversion(&c, "Couchbase");
    let d = driver("couchbase");
    let ddl = d.table_ddl(&t, CREATE).unwrap();
    assert!(ddl.contains("CREATE VECTOR INDEX"), "{ddl}");
    let mut admin = d.connect(cb, None).await.expect("couchbase");
    admin.create_database(DB).await.expect("bucket");
    let mut s = d
        .connect(&couchbase(DB).unwrap(), Some(DB))
        .await
        .expect("couchbase");
    for st in d.split_script(&ddl) {
        run_settled(&mut *s, &st.text).await;
    }
    let coll = ObjectRef {
        kind: "collection".into(),
        schema: Some(scope.clone()),
        name: t.name.clone(),
    };
    assert_eq!(copy_rows(&mut *pg, &mut *s, coll).await, 3);

    // The vector indexes were made deferred (a new collection): built now.
    let ks = format!("`{DB}`.`_default`.`{}`", t.name);
    let names: Vec<String> = t
        .indexes
        .iter()
        .filter(|i| i.vector.is_some())
        .map(|i| format!("`{}`", i.name))
        .collect();
    run_settled(
        &mut *s,
        &format!("BUILD INDEX ON {ks}({})", names.join(", ")),
    )
    .await;
    let start = Instant::now();
    loop {
        let out = run(
            &mut *s,
            &format!("SELECT RAW i.state FROM system:indexes AS i WHERE i.bucket_id = \"{DB}\" AND i.name IN [{}]", names.join(", ").replace('`', "\"")),
        )
        .await;
        let rows = &out.results[0].rows;
        if rows.len() == names.len() && rows.iter().all(|r| r[0] == "online") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "the vector indexes never got online: {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let back = s
        .database_schema()
        .await
        .unwrap()
        .into_iter()
        .find(|x| x.name == t.name)
        .expect("the collection");
    check_described(&back, &t, "Couchbase");

    let near = run_settled(
        &mut *s,
        &format!("SELECT RAW d.id FROM {ks} AS d ORDER BY APPROX_VECTOR_DISTANCE(d.emb, [0.9, 0.1, 0], \"L2\") LIMIT 1"),
    )
    .await;
    assert_eq!(near.results[0].rows[0][0], Value::from(3));
}
