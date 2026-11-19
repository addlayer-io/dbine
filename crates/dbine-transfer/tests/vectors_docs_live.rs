//! Vectors between pgvector and the document engines, against live
//! servers: a pgvector table goes to MongoDB (Atlas Local), Cosmos DB (the
//! emulator) and Couchbase (8.0) through the drivers' typed reads and
//! loads, into a collection the target driver's own DDL makes with a
//! vector index (as a clone or the designer would), is searched there, and
//! comes back to pgvector; each step is checked against the source's
//! values. Each test drops what it made, first and at the end.
//!
//! Ignored by default; each test runs when its servers' variables are set:
//!
//! ```sh
//! DBINE_TEST_PGVECTOR_URL=postgres://postgres:pw@localhost:27200/postgres \
//! DBINE_TEST_ATLAS_URL='mongodb://localhost:27205/?directConnection=true' \
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25213 \
//! DBINE_TEST_COUCHBASE_EE_URL=http://localhost:25993 DBINE_TEST_COUCHBASE_EE_MGMT_PORT=25991 \
//!   cargo test -p dbine-transfer --test vectors_docs_live -- --ignored --nocapture --test-threads=1
//! ```

use dbine_driver::transfer::{
    BatchSink, BatchSource, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    async_trait, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, ObjectRef, QueryOutcome,
    Session, TableSchema, VectorIndex,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DB: &str = "dbine_xdoc";

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

/// The last result: its columns and rows.
async fn result(s: &mut dyn Session, text: &str) -> dbine_driver::StatementResult {
    run(s, text)
        .await
        .results
        .into_iter()
        .rev()
        .find(|r| !r.columns.is_empty())
        .expect("a result")
}

/// A vector's text in the canonical form (float32 values, shortest);
/// anything else as it is.
fn canonical(v: &Value) -> Value {
    match v.as_str().and_then(dbine_driver::vector::parse) {
        Some(x) => dbine_driver::vector::text(x.into_iter().map(|f| f as f32)).into(),
        None => v.clone(),
    }
}

/// `(id, vector)` rows of any engine, sorted by id: the id as a number,
/// the vector in the canonical form (a missing one as null).
fn id_and_vector(rows: Vec<Vec<Value>>) -> Vec<(i64, Value)> {
    let mut out: Vec<(i64, Value)> = rows
        .into_iter()
        .map(|r| {
            let id = r[0]
                .as_i64()
                .or_else(|| r[0].as_f64().map(|f| f as i64))
                .or_else(|| r[0].as_str().and_then(|s| s.parse().ok()))
                .expect("id");
            (id, canonical(r.get(1).unwrap_or(&Value::Null)))
        })
        .collect();
    out.sort_by_key(|r| r.0);
    out
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

/// `id` and `emb` of `t` read with the driver's typed read, then loaded
/// into `target`'s `id` and `emb`.
async fn move_rows(
    from: &mut dyn Session,
    t: ObjectRef,
    into: &mut dyn Session,
    target: ObjectRef,
) -> u64 {
    let sink = Arc::new(Mutex::new(Collect::default()));
    from.read_batches(
        &ReadSpec {
            table: t,
            columns: Some(vec!["id".into(), "emb".into()]),
            filter: None,
        },
        sink.clone(),
    )
    .await
    .expect("read");
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let spec = LoadSpec {
        table: target,
        columns: vec!["id".into(), "emb".into()],
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

fn table(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind: kind.into(),
        schema: schema.map(Into::into),
        name: name.into(),
    }
}

/// The source and the table it comes back to, in pgvector, with its rows;
/// the values every step must give.
async fn pg_source(pg: &mut dyn Session, src: &str, back: &str) -> Vec<(i64, Value)> {
    run(pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
    run(pg, &format!("DROP TABLE IF EXISTS {src}, {back}")).await;
    run(
        pg,
        &format!("CREATE TABLE {src} (id int PRIMARY KEY, emb vector(3)); CREATE TABLE {back} (id int PRIMARY KEY, emb vector(3))"),
    )
    .await;
    run(
        pg,
        &format!("INSERT INTO {src} VALUES (1, '[0.1,0.2,0.3]'), (2, '[1,-2.5,3e-7]'), (3, NULL), (4, '[-0.0001,123456.7,0]')"),
    )
    .await;
    let want = id_and_vector(
        result(pg, &format!("SELECT id, emb::text FROM {src} ORDER BY id"))
            .await
            .rows,
    );
    assert_eq!(want[1].1, Value::from("[1,-2.5,0.0000003]"));
    want
}

/// The designer's collection: `id` and a `vector(3)` field with a vector
/// index (l2) on it.
fn designed(
    schema: Option<&str>,
    name: &str,
    index: IndexDef,
    options: &[(&str, &str)],
) -> TableSchema {
    let col = |n: &str, t: &str| ColumnDef {
        name: n.into(),
        data_type: t.into(),
        nullable: true,
        ..Default::default()
    };
    TableSchema {
        kind: "collection".into(),
        schema: schema.map(Into::into),
        name: name.into(),
        columns: vec![col("id", ""), col("emb", "vector(3)")],
        indexes: vec![index],
        options: options
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

fn vector_index(name: &str, kind: Option<&str>, params: &[(&str, &str)]) -> IndexDef {
    IndexDef {
        name: name.into(),
        columns: vec!["emb".into()],
        kind: kind.map(str::to_string),
        vector: Some(VectorIndex {
            metric: Some("l2".into()),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }),
        ..Default::default()
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

// ---------------------------------------------------------------- MongoDB

async fn mongo_drop(pgc: &ConnectionConfig, mc: &ConnectionConfig) {
    let mut pg = driver("postgres").connect(pgc, None).await.expect("pg");
    let _ = try_run(&mut *pg, "DROP TABLE IF EXISTS xdoc_m_src, xdoc_m_back").await;
    let mut m = driver("mongodb")
        .connect(mc, Some(DB))
        .await
        .expect("mongodb");
    let _ = try_run(&mut *m, "db.dropDatabase()").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and Atlas Local: see the top of the file"]
async fn pgvector_to_atlas_and_back() {
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
    let want = pg_source(&mut *pg, "xdoc_m_src", "xdoc_m_back").await;
    let d = driver("mongodb");
    let mut m = d.connect(mc, Some(DB)).await.expect("mongodb");
    let ddl = d
        .table_ddl(
            &designed(None, "xdoc_v", vector_index("emb_vix", None, &[]), &[]),
            CREATE,
        )
        .unwrap();
    assert!(ddl.contains("createSearchIndex"), "{ddl}");
    run(&mut *m, &ddl).await;
    let coll = table("collection", None, "xdoc_v");

    // pgvector → MongoDB: arrays of doubles the vector index searches.
    let n = move_rows(
        &mut *pg,
        table("table", Some("public"), "xdoc_m_src"),
        &mut *m,
        coll.clone(),
    )
    .await;
    assert_eq!(n, 4);
    let arrays = result(
        &mut *m,
        "db.getCollection(\"xdoc_v\").aggregate([{ $match: { emb: { $type: \"array\" } } }, { $count: \"n\" }])",
    )
    .await;
    assert_eq!(arrays.rows[0][0], Value::from(3));
    let r = result(
        &mut *m,
        "db.getCollection(\"xdoc_v\").find({}, { _id: 0, id: 1, emb: 1 }).sort({ id: 1 })",
    )
    .await;
    let at = r.columns.iter().position(|c| c.name == "emb").unwrap();
    assert!(r.columns[at].vector.is_some(), "{:?}", r.columns);
    let id = r.columns.iter().position(|c| c.name == "id").unwrap();
    let got: Vec<Vec<Value>> = r
        .rows
        .iter()
        .map(|row| vec![row[id].clone(), row[at].clone()])
        .collect();
    assert_eq!(id_and_vector(got), want, "MongoDB");

    // Searched there, once mongot has built the index.
    let start = Instant::now();
    let ids = loop {
        let out = try_run(
            &mut *m,
            "db.getCollection(\"xdoc_v\").aggregate([{ $vectorSearch: { index: \"emb_vix\", path: \"emb\", queryVector: [0.1, 0.2, 0.3], numCandidates: 10, limit: 1 } }, { $project: { _id: 0, id: 1 } }])",
        )
        .await;
        match out {
            Ok(o) if o.results.first().is_some_and(|r| !r.rows.is_empty()) => {
                break o.results[0].rows.clone()
            }
            _ if start.elapsed() < Duration::from_secs(120) => {
                tokio::time::sleep(Duration::from_secs(1)).await
            }
            other => panic!("the vector search never answered: {other:?}"),
        }
    };
    assert_eq!(ids[0][0], Value::from(1));

    // MongoDB → pgvector.
    let n = move_rows(
        &mut *m,
        coll,
        &mut *pg,
        table("table", Some("public"), "xdoc_m_back"),
    )
    .await;
    assert_eq!(n, 4);
    let back = id_and_vector(
        result(
            &mut *pg,
            "SELECT id, emb::text FROM xdoc_m_back ORDER BY id",
        )
        .await
        .rows,
    );
    assert_eq!(back, want, "back in pgvector");
}

// ---------------------------------------------------------------- Cosmos DB

async fn cosmos_drop(pgc: &ConnectionConfig, cc: &ConnectionConfig) {
    let mut pg = driver("postgres").connect(pgc, None).await.expect("pg");
    let _ = try_run(&mut *pg, "DROP TABLE IF EXISTS xdoc_c_src, xdoc_c_back").await;
    let mut admin = driver("cosmosdb")
        .connect(cc, None)
        .await
        .expect("cosmosdb");
    let _ = admin.drop_database(DB).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and the Cosmos DB emulator: see the top of the file"]
async fn pgvector_to_cosmosdb_and_back() {
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
    let want = pg_source(&mut *pg, "xdoc_c_src", "xdoc_c_back").await;
    let d = driver("cosmosdb");
    let mut admin = d.connect(cc, None).await.expect("cosmosdb");
    admin.create_database(DB).await.expect("database");
    let mut c = d.connect(cc, Some(DB)).await.expect("cosmosdb");
    // diskANN: the only vector index the emulator takes.
    let t = designed(
        None,
        "xdoc_v",
        vector_index("emb_vix", None, &[]),
        &[("partition_key", "/id")],
    );
    let ddl = d.table_ddl(&t, CREATE).unwrap();
    assert!(
        ddl.contains("vectorEmbeddingPolicy") && ddl.contains("diskANN"),
        "{ddl}"
    );
    run(&mut *c, &ddl).await;
    let coll = table("collection", None, "xdoc_v");

    // pgvector → Cosmos DB: arrays (the ids as text, as Cosmos DB keeps them).
    let n = move_rows(
        &mut *pg,
        table("table", Some("public"), "xdoc_c_src"),
        &mut *c,
        coll.clone(),
    )
    .await;
    assert_eq!(n, 4);
    let arrays = result(
        &mut *c,
        "-- container: xdoc_v\nSELECT VALUE COUNT(1) FROM c WHERE IS_ARRAY(c.emb)",
    )
    .await;
    assert_eq!(arrays.rows[0][0], Value::from(3));
    let r = result(&mut *c, "-- container: xdoc_v\nSELECT c.id, c.emb FROM c").await;
    let at = r.columns.iter().position(|x| x.name == "emb").unwrap();
    assert!(r.columns[at].vector.is_some(), "{:?}", r.columns);
    assert_eq!(id_and_vector(r.rows.clone()), want, "Cosmos DB");
    // (The emulator's backend aborts on TOP with a WHERE and
    // VectorDistance: no filter.)
    let near = result(
        &mut *c,
        "-- container: xdoc_v\nSELECT TOP 1 c.id FROM c ORDER BY VectorDistance(c.emb, [0.1, 0.2, 0.3])",
    )
    .await;
    assert_eq!(near.rows[0][0], Value::from("1"));

    // Cosmos DB → pgvector.
    let n = move_rows(
        &mut *c,
        coll,
        &mut *pg,
        table("table", Some("public"), "xdoc_c_back"),
    )
    .await;
    assert_eq!(n, 4);
    let back = id_and_vector(
        result(
            &mut *pg,
            "SELECT id, emb::text FROM xdoc_c_back ORDER BY id",
        )
        .await
        .rows,
    );
    assert_eq!(back, want, "back in pgvector");
}

// ---------------------------------------------------------------- Couchbase

async fn couchbase_drop(pgc: &ConnectionConfig, cb: &ConnectionConfig) {
    let mut pg = driver("postgres").connect(pgc, None).await.expect("pg");
    let _ = try_run(&mut *pg, "DROP TABLE IF EXISTS xdoc_b_src, xdoc_b_back").await;
    let mut admin = driver("couchbase")
        .connect(cb, None)
        .await
        .expect("couchbase");
    let _ = admin.drop_database(DB).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and Couchbase Server EE 8.0: see the top of the file"]
async fn pgvector_to_couchbase_and_back() {
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
    let want = pg_source(&mut *pg, "xdoc_b_src", "xdoc_b_back").await;
    let d = driver("couchbase");
    let mut admin = d.connect(cb, None).await.expect("couchbase");
    admin.create_database(DB).await.expect("bucket");
    let mut s = d
        .connect(&couchbase(DB).unwrap(), Some(DB))
        .await
        .expect("couchbase");
    let scope = format!("{DB}._default");
    let ks = format!("`{DB}`.`_default`.`xdoc_v`");
    // A hyperscale index over the empty collection: made deferred, built
    // once the rows are in.
    let t = designed(
        Some(&scope),
        "xdoc_v",
        vector_index("emb_vix", None, &[("lists", "1")]),
        &[("primary_index", "true")],
    );
    let ddl = d.table_ddl(&t, CREATE).unwrap();
    assert!(ddl.contains("CREATE VECTOR INDEX `emb_vix`"), "{ddl}");
    for st in d.split_script(&ddl) {
        run_settled(&mut *s, &st.text).await;
    }
    let coll = table("collection", Some(&scope), "xdoc_v");

    // pgvector → Couchbase: arrays.
    let n = move_rows(
        &mut *pg,
        table("table", Some("public"), "xdoc_b_src"),
        &mut *s,
        coll.clone(),
    )
    .await;
    assert_eq!(n, 4);
    let arrays = run_settled(
        &mut *s,
        &format!("SELECT RAW COUNT(1) FROM {ks} AS d WHERE ISARRAY(d.emb)"),
    )
    .await;
    assert_eq!(arrays.results[0].rows[0][0], Value::from(3));
    let r = result(&mut *s, &format!("SELECT d.id, d.emb FROM {ks} AS d")).await;
    let at = r.columns.iter().position(|x| x.name == "emb").unwrap();
    assert!(r.columns[at].vector.is_some(), "{:?}", r.columns);
    assert_eq!(id_and_vector(r.rows.clone()), want, "Couchbase");

    // Searched there, once the index is built.
    run_settled(&mut *s, &format!("BUILD INDEX ON {ks}(`emb_vix`)")).await;
    let start = Instant::now();
    loop {
        let out = run(
            &mut *s,
            &format!("SELECT RAW i.state FROM system:indexes AS i WHERE i.bucket_id = \"{DB}\" AND i.name = \"emb_vix\""),
        )
        .await;
        if out.results[0]
            .rows
            .first()
            .is_some_and(|r| r[0] == "online")
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "the vector index never got online"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let near = run_settled(
        &mut *s,
        &format!("SELECT RAW d.id FROM {ks} AS d ORDER BY APPROX_VECTOR_DISTANCE(d.emb, [0.1, 0.2, 0.3], \"L2\") LIMIT 1"),
    )
    .await;
    assert_eq!(near.results[0].rows[0][0], Value::from(1));

    // Couchbase → pgvector.
    let n = move_rows(
        &mut *s,
        coll,
        &mut *pg,
        table("table", Some("public"), "xdoc_b_back"),
    )
    .await;
    assert_eq!(n, 4);
    let back = id_and_vector(
        result(
            &mut *pg,
            "SELECT id, emb::text FROM xdoc_b_back ORDER BY id",
        )
        .await
        .rows,
    );
    assert_eq!(back, want, "back in pgvector");
}
