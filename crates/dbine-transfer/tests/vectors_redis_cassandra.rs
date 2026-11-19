//! Vectors moved between pgvector (the reference engine) and Redis 8,
//! Cassandra 5 and ScyllaDB, both ways, through the transfer [`Engine`]
//! (what "Transferir datos" runs: its own connections per table, the
//! drivers' reads and bulk loads):
//!
//! - pgvector's `vector(3)` into Redis hashes that a `FLOAT16` search index
//!   covers: the index's type decides the blob, the index finds them, and
//!   the read decodes them by that type on the way back;
//! - pgvector's `vector(3)` into a CQL `vector<float, 3>` with its vector
//!   index (SAI on Cassandra, `vector_index` on ScyllaDB), and back.
//!
//! Each step is checked against the source's numbers.
//!
//! Ignored by default; each test runs when its servers' variables are set,
//! pgvector always (it's the other side):
//!
//! ```sh
//! DBINE_TEST_PGVECTOR_URL=postgres://postgres:pw@localhost:27200/postgres \
//! DBINE_TEST_REDIS8_URL=localhost:27203 \
//! DBINE_TEST_CASSANDRA_URL=localhost:25402 \
//! DBINE_TEST_SCYLLADB_URL=localhost:25413 \
//!   cargo test -p dbine-transfer --test vectors_redis_cassandra -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Every test drops what it makes first and at the end, also when it fails
//! partway.

use dbine_driver::{
    async_trait, kinds, ConnectionConfig, Driver, LoadSpec, ObjectRef, QueryOutcome, ReadSpec,
    Session,
};
use dbine_transfer::{Endpoints, Engine, RunOptions, RunStatus, Store, TransferJob, TransferMode};
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_drivers::find(id)
        .unwrap_or_else(|| panic!("{id} driver"))
        .clone()
}

/// `[scheme://][user[:pass]@]host:port[/database]`.
fn config(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, addr) = rest
        .rsplit_once('@')
        .map_or((None, rest), |(a, h)| (Some(a), h));
    let (hostport, database) = addr.split_once('/').unwrap_or((addr, ""));
    let (host, port) = hostport.rsplit_once(':').expect("host:port");
    let (user, pass) = match auth.map(|a| a.split_once(':').unwrap_or((a, ""))) {
        Some((u, p)) => (
            Some(u.to_string()),
            Some(p.to_string()).filter(|p| !p.is_empty()),
        ),
        None => (None, None),
    };
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().expect("port"),
        username: user,
        password: pass,
        database: database.into(),
        ..Default::default()
    }
}

async fn connect(cfg: &ConnectionConfig, db: Option<&str>) -> Box<dyn Session> {
    driver(&cfg.driver)
        .connect(cfg, db)
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", cfg.driver))
}

async fn run(s: &mut dyn Session, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"));
    if let Some(e) = &out.error {
        panic!("{text}: {e}");
    }
    out
}

/// A statement whose failure doesn't matter (cleanup).
async fn quiet(s: &mut dyn Session, text: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(text, 10, &mut out).await;
}

/// The last result's rows.
async fn rows(s: &mut dyn Session, text: &str) -> Vec<Vec<Value>> {
    run(s, text)
        .await
        .results
        .into_iter()
        .rev()
        .find(|r| !r.columns.is_empty())
        .map(|r| r.rows)
        .unwrap_or_default()
}

fn obj(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind: kind.into(),
        schema: schema.map(Into::into),
        name: name.into(),
    }
}

/// `(id, vector text)` of pgvector's `SELECT id, emb::text`.
async fn pg_vectors(pg: &mut dyn Session, table: &str) -> Vec<(i64, Option<String>)> {
    rows(
        pg,
        &format!("SELECT id, emb::text FROM {table} ORDER BY id"),
    )
    .await
    .into_iter()
    .map(|r| {
        let id = r[0]
            .as_i64()
            .or_else(|| r[0].as_str().and_then(|s| s.parse().ok()))
            .expect("id");
        (id, r[1].as_str().map(str::to_string))
    })
    .collect()
}

/// The two ends of a transfer, each a connection config and its database.
struct Ends {
    source: (ConnectionConfig, Option<String>),
    target: (ConnectionConfig, Option<String>),
}

#[async_trait]
impl Endpoints for Ends {
    fn source_driver(&self) -> Arc<dyn Driver> {
        driver(&self.source.0.driver)
    }
    fn target_driver(&self) -> Arc<dyn Driver> {
        driver(&self.target.0.driver)
    }
    async fn open_source(&self) -> dbine_driver::Result<Box<dyn Session>> {
        self.source_driver()
            .connect(&self.source.0, self.source.1.as_deref())
            .await
    }
    async fn open_target(&self) -> dbine_driver::Result<Box<dyn Session>> {
        self.target_driver()
            .connect(&self.target.0, self.target.1.as_deref())
            .await
    }
    fn native_copy_allowed(&self) -> bool {
        false
    }
}

/// One table, `id` and `emb`, copied by the transfer engine, which must
/// finish with no table failed. `truncate` empties the target, as the
/// app's job carries it.
async fn transfer(
    ends: Ends,
    source: ObjectRef,
    target: ObjectRef,
    truncate: Option<String>,
    tag: &str,
) {
    let columns = vec!["id".to_string(), "emb".to_string()];
    let job = TransferJob {
        name: format!("{} -> {}", source.name, target.name),
        source: ReadSpec {
            table: source,
            columns: Some(columns.clone()),
            filter: None,
        },
        target: LoadSpec {
            table: target,
            columns,
            table_lock: false,
            keep_identity: true,
            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        },
        row_estimate: None,
        truncate,
        empty_first: false,
        before: String::new(),
        after: String::new(),
        post: Vec::new(),
        preexisting: false,
        expected_columns: Vec::new(),
        vector_columns: vec!["emb".into()],
        mode: TransferMode::Copy,
    };
    let path = std::env::temp_dir().join(format!(
        "dbine-vec-redis-cql-{tag}-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = Arc::new(Store::open(&path).expect("transfer state"));
    let report = Engine::new(store, tag)
        .run(vec![job], RunOptions::default(), Arc::new(ends), |_| {})
        .await;
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
    let report = report.unwrap_or_else(|e| panic!("{tag}: {e}"));
    assert!(
        report.summary.status == RunStatus::Finished && report.summary.failed == 0,
        "{tag}: {:?}\n{:?}",
        report.summary,
        report.tables
    );
}

/// Runs `body`, then `cleanup`, also when `body` panics.
async fn with_cleanup<B, C>(body: B, cleanup: C)
where
    B: Future<Output = ()> + Send + 'static,
    C: Future<Output = ()>,
{
    let res = tokio::spawn(body).await;
    cleanup.await;
    if let Err(e) = res {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// The source table: three vectors whose numbers a half float holds
/// exactly (Redis keeps them as FLOAT16 here), and a row without one.
const PG_ROWS: &str = "(1, '[0.5,-2,1.25]'), (2, '[0,1,0.125]'), (3, '[3,-0.75,2048]'), (4, NULL)";

// ------------------------------------------------------------- Redis 8

const REDIS_PREFIX: &str = "dbvec_xfer:docs";
const REDIS_INDEX: &str = "dbvec_xfer_ix";

async fn redis_drop(r: &mut dyn Session) {
    quiet(r, &format!("FT.DROPINDEX {REDIS_INDEX}")).await;
    let keys: Vec<String> = (1..=4).map(|i| format!("{REDIS_PREFIX}:{i}")).collect();
    quiet(r, &format!("DEL {}", keys.join(" "))).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and Redis 8: see the top of the file"]
async fn pgvector_to_redis8_and_back() {
    let (Some(pg_url), Some(redis_url)) =
        (env("DBINE_TEST_PGVECTOR_URL"), env("DBINE_TEST_REDIS8_URL"))
    else {
        eprintln!("DBINE_TEST_PGVECTOR_URL or DBINE_TEST_REDIS8_URL not set; skipping");
        return;
    };
    let pgc = config("postgres", &pg_url);
    let rc = config("redis", &redis_url);
    let (src, back) = ("zz_vec_rc_redis_src", "zz_vec_rc_redis_back");
    let drop_all = {
        let (pgc, rc) = (pgc.clone(), rc.clone());
        move || {
            let (pgc, rc) = (pgc.clone(), rc.clone());
            async move {
                let mut pg = connect(&pgc, None).await;
                quiet(&mut *pg, &format!("DROP TABLE IF EXISTS {src}, {back}")).await;
                let mut r = connect(&rc, Some("db0")).await;
                redis_drop(&mut *r).await;
            }
        }
    };
    drop_all().await;
    with_cleanup(
        async move {
            let mut pg = connect(&pgc, None).await;
            run(&mut *pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
            run(
                &mut *pg,
                &format!(
                    "CREATE TABLE {src} (id int PRIMARY KEY, emb vector(3)); \
                     CREATE TABLE {back} (id int PRIMARY KEY, emb vector(3)); \
                     INSERT INTO {src} VALUES {PG_ROWS}"
                ),
            )
            .await;
            let want = pg_vectors(&mut *pg, src).await;
            let mut r = connect(&rc, Some("db0")).await;
            println!("redis: {}", r.server_version().await.unwrap());
            run(
                &mut *r,
                &format!(
                    "FT.CREATE {REDIS_INDEX} ON HASH PREFIX 1 {REDIS_PREFIX}: SCHEMA emb VECTOR HNSW 6 TYPE FLOAT16 DIM 3 DISTANCE_METRIC L2"
                ),
            )
            .await;

            // pgvector → Redis: each row a hash, the vector as the blob of
            // the index's type (FLOAT16, 2 bytes a number).
            transfer(
                Ends {
                    source: (pgc.clone(), None),
                    target: (rc.clone(), Some("db0".into())),
                },
                obj(kinds::TABLE, Some("public"), src),
                obj(kinds::KEY, None, REDIS_PREFIX),
                // The engine looks for rows already there with the key's
                // browse, a `TYPE` that answers a row (`none`) for a name
                // that isn't a key: it empties it first, as the app's job
                // would (the target's drop).
                Some(format!("DEL {REDIS_PREFIX}")),
                "pg-redis",
            )
            .await;
            let len = rows(&mut *r, &format!("HSTRLEN {REDIS_PREFIX}:1 emb")).await;
            let len = len.first().and_then(|row| row.first()).and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            });
            assert_eq!(len, Some(6));
            // The index holds them: the nearest to the first is itself.
            let q = dbine_driver::vector::to_f16_le(&[0.5, -2.0, 1.25]);
            let knn = format!(
                "FT.SEARCH {REDIS_INDEX} \"*=>[KNN 1 @emb $q]\" PARAMS 2 q {} RETURN 1 __emb_score DIALECT 2",
                blob_arg(&q)
            );
            let mut nearest = None;
            for _ in 0..100 {
                let out = run(&mut *r, &knn).await;
                let res = &out.results[0];
                if let Some(row) = res.rows.first() {
                    let id = res.columns.iter().position(|c| c.name == "id").unwrap();
                    nearest = row[id].as_str().map(str::to_string);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            assert_eq!(nearest.as_deref(), Some("dbvec_xfer:docs:1"));

            // Redis → pgvector: the read decodes the blobs by the index's
            // type; the row without a vector has none.
            transfer(
                Ends {
                    source: (rc.clone(), Some("db0".into())),
                    target: (pgc.clone(), None),
                },
                obj(kinds::KEY, None, REDIS_PREFIX),
                obj(kinds::TABLE, Some("public"), back),
                None,
                "redis-pg",
            )
            .await;
            assert_eq!(pg_vectors(&mut *pg, back).await, want, "back in pgvector");
        },
        drop_all(),
    )
    .await;
}

// ------------------------------------------------- Cassandra / ScyllaDB

const KS: &str = "dbine_vec_xfer";

async fn cql_round_trip(id: &'static str, url_var: &str) {
    let (Some(pg_url), Some(cql_url)) = (env("DBINE_TEST_PGVECTOR_URL"), env(url_var)) else {
        eprintln!("DBINE_TEST_PGVECTOR_URL or {url_var} not set; skipping");
        return;
    };
    let pgc = config("postgres", &pg_url);
    let cc = config(id, &cql_url);
    let src = format!("zz_vec_rc_{id}_src");
    let back = format!("zz_vec_rc_{id}_back");
    let drop_all = {
        let (pgc, cc, src, back) = (pgc.clone(), cc.clone(), src.clone(), back.clone());
        move || {
            let (pgc, cc, src, back) = (pgc.clone(), cc.clone(), src.clone(), back.clone());
            async move {
                let mut pg = connect(&pgc, None).await;
                quiet(&mut *pg, &format!("DROP TABLE IF EXISTS {src}, {back}")).await;
                let mut c = connect(&cc, None).await;
                quiet(&mut *c, &format!("DROP KEYSPACE IF EXISTS {KS}")).await;
            }
        }
    };
    drop_all().await;
    with_cleanup(
        async move {
            let mut pg = connect(&pgc, None).await;
            run(&mut *pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
            run(
                &mut *pg,
                &format!(
                    "CREATE TABLE {src} (id int PRIMARY KEY, emb vector(3)); \
                     CREATE TABLE {back} (id int PRIMARY KEY, emb vector(3)); \
                     INSERT INTO {src} VALUES {PG_ROWS}, (5, '[0.1,0.2,0.3]'), (6, '[1,-2.5,3e-7]')"
                ),
            )
            .await;
            let want = pg_vectors(&mut *pg, &src).await;
            let mut c = connect(&cc, None).await;
            println!("{id}: {}", c.server_version().await.unwrap());
            run(
                &mut *c,
                &format!("CREATE KEYSPACE {KS} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"),
            )
            .await;
            run(
                &mut *c,
                &format!("CREATE TABLE {KS}.docs (id int PRIMARY KEY, emb vector<float, 3>)"),
            )
            .await;
            let index = if id == "scylladb" {
                format!("CREATE CUSTOM INDEX docs_emb ON {KS}.docs (emb) USING 'vector_index' WITH OPTIONS = {{'similarity_function': 'EUCLIDEAN'}}")
            } else {
                format!("CREATE INDEX docs_emb ON {KS}.docs (emb) USING 'sai' WITH OPTIONS = {{'similarity_function': 'EUCLIDEAN'}}")
            };
            run(&mut *c, &index).await;

            // pgvector → CQL.
            transfer(
                Ends {
                    source: (pgc.clone(), None),
                    target: (cc.clone(), Some(KS.into())),
                },
                obj(kinds::TABLE, Some("public"), &src),
                obj(kinds::TABLE, Some(KS), "docs"),
                None,
                &format!("pg-{id}"),
            )
            .await;
            let got = rows(&mut *c, &format!("SELECT id, emb FROM {KS}.docs")).await;
            assert_eq!(got.len(), want.len(), "{got:?}");
            // Cassandra's SAI answers a similarity search at once;
            // ScyllaDB's needs its separate Vector Store service.
            if id == "cassandra" {
                let nearest = rows(
                    &mut *c,
                    &format!("SELECT id FROM {KS}.docs ORDER BY emb ANN OF [0.1, 0.2, 0.3] LIMIT 1"),
                )
                .await;
                assert_eq!(nearest[0][0], Value::from(5), "{nearest:?}");
            }

            // CQL → pgvector.
            transfer(
                Ends {
                    source: (cc.clone(), Some(KS.into())),
                    target: (pgc.clone(), None),
                },
                obj(kinds::TABLE, Some(KS), "docs"),
                obj(kinds::TABLE, Some("public"), &back),
                None,
                &format!("{id}-pg"),
            )
            .await;
            assert_eq!(pg_vectors(&mut *pg, &back).await, want, "back in pgvector");
        },
        drop_all(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and Cassandra 5: see the top of the file"]
async fn pgvector_to_cassandra_and_back() {
    cql_round_trip("cassandra", "DBINE_TEST_CASSANDRA_URL").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and ScyllaDB: see the top of the file"]
async fn pgvector_to_scylladb_and_back() {
    cql_round_trip("scylladb", "DBINE_TEST_SCYLLADB_URL").await;
}
