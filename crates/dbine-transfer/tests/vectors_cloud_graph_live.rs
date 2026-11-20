//! Vectors moved between pgvector (the reference engine) and the cloud and
//! graph engines, both ways, through the drivers' own `read_batches` and
//! `bulk_load` (what a transfer and "Clonar tabla" run): pgvector's
//! `vector(3)` into Spanner's `ARRAY<FLOAT32>(vector_length=>3)`, BigQuery's
//! `ARRAY<FLOAT64>`, Neo4j's and Memgraph's float lists and a Flight SQL
//! (GizmoSQL / DuckDB) `FLOAT[3]`, then each of those back into a pgvector
//! table, comparing every number.
//!
//! Ignored by default; each engine runs when its URL is set, pgvector
//! always (it's the other side):
//!
//! ```sh
//! DBINE_TEST_PGVECTOR_URL=postgres:pw@127.0.0.1:27200 \
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 \
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 \
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 \
//! DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//! DBINE_TEST_FLIGHTSQL_URL=http://localhost:25337 \
//!   cargo test -p dbine-transfer --test vectors_cloud_graph_live -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Every test drops what it creates first and at the end, also when it
//! fails partway.

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::future::Future;
use std::sync::{Arc, Mutex};

const PG_SRC: &str = "zz_vec_cg_src";
const PG_BACK: &str = "zz_vec_cg_back";

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

/// `user:pass@host:port` (the user and password optional).
fn split_url(url: &str) -> (Option<String>, Option<String>, String, u16) {
    let url = url.split("://").last().unwrap_or(url).trim_end_matches('/');
    let (auth, hp) = url
        .rsplit_once('@')
        .map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').expect("host:port");
    let (user, pass) = auth
        .and_then(|a| a.split_once(':'))
        .map_or((auth.map(str::to_string), None), |(u, p)| {
            (Some(u.to_string()), Some(p.to_string()))
        });
    (user, pass, host.to_string(), port.parse().expect("port"))
}

async fn connect(c: ConnectionConfig, db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_drivers::find(&c.driver).expect("driver in the registry");
    d.connect(&c, db)
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", c.driver))
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(q, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    if let Some(e) = &out.error {
        panic!("{q}: {e}");
    }
    out
}

/// A statement whose failure doesn't matter (cleanup).
async fn quiet(s: &mut Box<dyn Session>, q: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(q, 10, &mut out).await;
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, c: &[TransferColumn]) -> std::io::Result<()> {
        self.columns = c.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

struct Once(Option<Vec<Vec<Cell>>>);

#[dbine_driver::async_trait]
impl BatchSource for Once {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.take().map(|rows| RowBatch { rows, bytes: 0 })
    }
}

fn obj(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind: kind.into(),
        schema: schema.map(str::to_string),
        name: name.into(),
    }
}

/// `id` and `emb` of a table, ordered by id.
async fn read(s: &mut Box<dyn Session>, t: ObjectRef) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: t,
            columns: Some(vec!["id".into(), "emb".into()]),
            filter: None,
        },
        sink.clone(),
    )
    .await
    .expect("read_batches");
    let mut c = std::mem::take(&mut *sink.lock().unwrap());
    c.rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        Cell::Text(ref t) => t.parse().unwrap_or(0),
        _ => 0,
    });
    c
}

async fn load(s: &mut Box<dyn Session>, t: ObjectRef, c: &Collect) -> u64 {
    let spec = LoadSpec {
        table: t,
        columns: vec!["id".into(), "emb".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    s.bulk_load(&spec, &c.columns, &mut Once(Some(c.rows.clone())), &|_| {})
        .await
        .expect("bulk_load")
}

/// Each row's vector as 32-bit floats (pgvector's precision), `None` for NULL.
fn vectors(c: &Collect) -> Vec<Option<Vec<f32>>> {
    c.rows
        .iter()
        .map(|r| match &r[1] {
            Cell::Text(s) | Cell::Json(s) => Some(
                dbine_driver::vector::parse(s)
                    .unwrap_or_else(|| panic!("not a vector: {s}"))
                    .into_iter()
                    .map(|x| x as f32)
                    .collect(),
            ),
            Cell::Null => None,
            o => panic!("vector cell {o:?}"),
        })
        .collect()
}

fn pg_config() -> ConnectionConfig {
    let (user, pass, host, port) = split_url(
        &env("DBINE_TEST_PGVECTOR_URL").unwrap_or_else(|| "postgres:pw@127.0.0.1:27200".into()),
    );
    ConnectionConfig {
        driver: "postgres".into(),
        host,
        port,
        username: user,
        password: pass,
        database: "postgres".into(),
        ..Default::default()
    }
}

/// The pgvector source and the empty table the vectors come back to.
async fn pg_tables() -> (Box<dyn Session>, Collect) {
    let mut pg = connect(pg_config(), None).await;
    run(&mut pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
    quiet(
        &mut pg,
        &format!("DROP TABLE IF EXISTS {PG_SRC}, {PG_BACK}"),
    )
    .await;
    run(
        &mut pg,
        &format!(
            "CREATE TABLE {PG_SRC} (id int PRIMARY KEY, emb vector(3)); \
             CREATE TABLE {PG_BACK} (id int PRIMARY KEY, emb vector(3))"
        ),
    )
    .await;
    run(
        &mut pg,
        &format!(
            "INSERT INTO {PG_SRC} VALUES (1, '[1,0,0]'), (2, '[0.1,0.2,0.3]'), \
             (3, '[-1.5,2.25,1e-7]'), (4, NULL)"
        ),
    )
    .await;
    let src = read(&mut pg, obj("table", Some("public"), PG_SRC)).await;
    assert_eq!(src.rows.len(), 4);
    (pg, src)
}

async fn pg_drop() {
    let mut pg = connect(pg_config(), None).await;
    quiet(
        &mut pg,
        &format!("DROP TABLE IF EXISTS {PG_SRC}, {PG_BACK}"),
    )
    .await;
}

/// pgvector → `target` → pgvector, comparing every number (a NULL vector
/// included: graph engines leave the property out, which reads back as
/// NULL too).
async fn both_ways(engine: &str, target: &mut Box<dyn Session>, t: ObjectRef) {
    let (mut pg, src) = pg_tables().await;
    let want = vectors(&src);
    assert_eq!(load(target, t.clone(), &src).await, 4, "{engine}: load");
    let got = read(target, t).await;
    eprintln!(
        "{engine}: read {:?} as {:?}",
        got.columns.iter().map(|c| &c.type_name).collect::<Vec<_>>(),
        got.rows
    );
    assert_eq!(vectors(&got), want, "{engine}: pgvector → {engine}");
    assert_eq!(
        load(&mut pg, obj("table", Some("public"), PG_BACK), &got).await,
        4,
        "{engine}: back into pgvector"
    );
    let back = read(&mut pg, obj("table", Some("public"), PG_BACK)).await;
    assert_eq!(vectors(&back), want, "{engine}: {engine} → pgvector");
}

/// Runs `body`, then `cleanup` whatever happened, then re-raises a failure.
async fn guarded<F, C>(body: F, cleanup: C)
where
    F: Future<Output = ()> + Send + 'static,
    C: Future<Output = ()>,
{
    let outcome = tokio::spawn(body).await;
    cleanup.await;
    pg_drop().await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

// ---- Spanner ----------------------------------------------------------------

const SPANNER_DB: &str = "zzveccg";

async fn spanner_admin(url: &str, method: &str, path: &str, body: &str) {
    use std::io::{Read, Write};
    let hp = url
        .split("://")
        .last()
        .unwrap()
        .trim_end_matches('/')
        .to_string();
    let mut c = std::net::TcpStream::connect(&hp).expect("emulator");
    let req = format!(
        "{method} /v1/projects/test/instances{path} HTTP/1.1\r\nHost: {hp}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    c.write_all(req.as_bytes()).unwrap();
    let mut s = String::new();
    let _ = c.read_to_string(&mut s);
}

async fn spanner_session(url: &str) -> Box<dyn Session> {
    let mut c = ConnectionConfig {
        driver: "spanner".into(),
        database: SPANNER_DB.into(),
        ..Default::default()
    };
    for (k, v) in [
        ("project_id", "test"),
        ("instance", "i1"),
        ("endpoint_url", url),
    ] {
        c.options.insert(k.into(), v.into());
    }
    connect(c, None).await
}

#[tokio::test]
#[ignore]
async fn pgvector_and_spanner() {
    let Some(url) = env("DBINE_TEST_SPANNER_URL") else {
        eprintln!("DBINE_TEST_SPANNER_URL not set; skipping");
        return;
    };
    spanner_admin(&url, "POST", "", r#"{"instanceId":"i1","instance":{"config":"projects/test/instanceConfigs/emulator-config","displayName":"i1","nodeCount":1}}"#).await;
    spanner_admin(&url, "DELETE", &format!("/i1/databases/{SPANNER_DB}"), "").await;
    spanner_admin(
        &url,
        "POST",
        "/i1/databases",
        &format!(
            r#"{{"createStatement":"CREATE DATABASE `{SPANNER_DB}`","extraStatements":["CREATE TABLE zz_cg (id INT64 NOT NULL, emb ARRAY<FLOAT32>(vector_length=>3)) PRIMARY KEY (id)"]}}"#
        ),
    )
    .await;
    let u = url.clone();
    guarded(
        async move {
            let mut s = spanner_session(&u).await;
            both_ways("spanner", &mut s, obj("table", None, "zz_cg")).await;
        },
        spanner_admin(&url, "DELETE", &format!("/i1/databases/{SPANNER_DB}"), ""),
    )
    .await;
}

// ---- BigQuery ---------------------------------------------------------------

async fn bigquery_session(url: &str) -> Box<dyn Session> {
    let mut c = ConnectionConfig {
        driver: "bigquery".into(),
        ..Default::default()
    };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url.into());
    connect(c, Some("ds1")).await
}

#[tokio::test]
#[ignore]
async fn pgvector_and_bigquery() {
    let Some(url) = env("DBINE_TEST_BIGQUERY_URL") else {
        eprintln!("DBINE_TEST_BIGQUERY_URL not set; skipping");
        return;
    };
    let mut s = bigquery_session(&url).await;
    quiet(&mut s, "DROP TABLE IF EXISTS ds1.zz_vec_cg").await;
    run(
        &mut s,
        "CREATE TABLE ds1.zz_vec_cg (id INT64, emb ARRAY<FLOAT64>)",
    )
    .await;
    let u = url.clone();
    guarded(
        async move {
            let mut s = bigquery_session(&u).await;
            both_ways("bigquery", &mut s, obj("table", Some("ds1"), "zz_vec_cg")).await;
        },
        async move {
            quiet(&mut s, "DROP TABLE IF EXISTS ds1.zz_vec_cg").await;
        },
    )
    .await;
}

// ---- Neo4j and Memgraph -----------------------------------------------------

async fn graph_session(driver: &str, url: &str) -> Box<dyn Session> {
    let (username, password, host, port) = split_url(url);
    connect(
        ConnectionConfig {
            driver: driver.into(),
            host,
            port,
            username,
            password,
            ..Default::default()
        },
        None,
    )
    .await
}

async fn graph(driver: &'static str, var: &str) {
    let Some(url) = env(var) else {
        eprintln!("{var} not set; skipping");
        return;
    };
    let mut s = graph_session(driver, &url).await;
    quiet(&mut s, "MATCH (n:ZzVecCg) DETACH DELETE n").await;
    let u = url.clone();
    guarded(
        async move {
            let mut s = graph_session(driver, &u).await;
            both_ways(driver, &mut s, obj("label", None, "ZzVecCg")).await;
            // In the graph they're float lists, what its vector index and
            // functions take, read back as vectors.
            let out = run(&mut s, "MATCH (n:ZzVecCg) WHERE n.id = 2 RETURN n.emb AS e").await;
            let c = &out.results[0].columns[0];
            assert!(c.vector.is_some(), "{driver}: {c:?}");
            let out = run(
                &mut s,
                "MATCH (n:ZzVecCg) WHERE n.id = 3 RETURN all(x IN n.emb WHERE x = toFloat(x)) AS floats",
            )
            .await;
            assert_eq!(out.results[0].rows[0][0], serde_json::json!(true));
        },
        async move {
            quiet(&mut s, "MATCH (n:ZzVecCg) DETACH DELETE n").await;
        },
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn pgvector_and_neo4j() {
    graph("neo4j", "DBINE_TEST_NEO4J_URL").await;
}

#[tokio::test]
#[ignore]
async fn pgvector_and_memgraph() {
    graph("memgraph", "DBINE_TEST_MEMGRAPH_URL").await;
}

// ---- Flight SQL (GizmoSQL / DuckDB) -----------------------------------------

async fn flightsql_session(url: &str) -> Box<dyn Session> {
    let (_, _, host, port) = split_url(url);
    connect(
        ConnectionConfig {
            driver: "flightsql".into(),
            host,
            port,
            username: Some(
                env("DBINE_TEST_FLIGHTSQL_USER").unwrap_or_else(|| "gizmosql_user".into()),
            ),
            password: Some(
                env("DBINE_TEST_FLIGHTSQL_PASSWORD").unwrap_or_else(|| "secreto1".into()),
            ),
            ..Default::default()
        },
        None,
    )
    .await
}

#[tokio::test]
#[ignore]
async fn pgvector_and_flightsql() {
    let Some(url) = env("DBINE_TEST_FLIGHTSQL_URL") else {
        eprintln!("DBINE_TEST_FLIGHTSQL_URL not set; skipping");
        return;
    };
    let mut s = flightsql_session(&url).await;
    quiet(&mut s, "DROP TABLE IF EXISTS zz_vec_cg").await;
    run(&mut s, "CREATE TABLE zz_vec_cg (id INTEGER, emb FLOAT[3])").await;
    let u = url.clone();
    guarded(
        async move {
            let mut s = flightsql_session(&u).await;
            both_ways("flightsql", &mut s, obj("table", None, "zz_vec_cg")).await;
        },
        async move {
            quiet(&mut s, "DROP TABLE IF EXISTS zz_vec_cg").await;
        },
    )
    .await;
}
