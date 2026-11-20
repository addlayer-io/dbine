//! pgvector's binary vectors (`bit(16)` searched by an HNSW
//! `bit_hamming_ops` index) moved by the transfer [`Engine`] into the
//! other engines' binary vectors, as a migration does: the target table is
//! the one the schema conversion makes (libSQL `F1BIT_BLOB(16)` with a
//! cosine index, which on 1-bit vectors measures Hamming; MongoDB
//! `vector(16, bit)` with a euclidean `vectorSearch`; Oracle
//! `VECTOR(16, BINARY)` with an HNSW `HAMMING` index, see
//! `dbine-schema/tests/convert.rs`), created by the target driver's DDL;
//! the job marks the column as a vector, as the migration does from the
//! target's types. pgvector reads `bit(n)` as the bit string `1011…`; the
//! engine hands the target's loader the contract's binary vector (a JSON
//! array of 0 and 1). Each target's rows are read back against the
//! source's bits and searched by its index, nearest first.
//!
//! Ignored by default; each test runs when its servers' variables are set,
//! pgvector always:
//!
//! ```sh
//! DBINE_TEST_PGVECTOR_URL=postgres://postgres:pw@localhost:27200/postgres \
//! DBINE_TEST_LIBSQL_URL=http://localhost:25880 \
//! DBINE_TEST_ATLAS_URL='mongodb://localhost:27205/?directConnection=true' \
//! DBINE_TEST_ORACLE_URL=oracle://dbine:Dbine123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-transfer --test vectors_bits_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Every test drops what it makes first and at the end, also when it fails
//! partway.

use dbine_driver::{
    async_trait, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, LoadSpec, ObjectRef,
    QueryOutcome, ReadSpec, Session, TableSchema, TransferColumn, VectorIndex,
};
use dbine_transfer::{Endpoints, Engine, RunOptions, RunStatus, Store, TransferJob, TransferMode};
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SRC: &str = "zz_vec_bits_src";

/// The source's rows: `id`, then the bits (`None`: NULL).
const ROWS: [(i64, Option<&str>); 5] = [
    (1, Some("1000000000000000")),
    (2, Some("1110000000000000")),
    (3, Some("0111111111111111")),
    (4, Some("1100000000000000")),
    (5, None),
];

/// The query: the first row's bits. Nearest first by Hamming: 1 (0 bits
/// apart), 4 (1), 2 (2), 3 (16).
const NEAREST: [i64; 3] = [1, 4, 2];

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_drivers::find(id)
        .unwrap_or_else(|| panic!("{id} driver"))
        .clone()
}

fn pgvector() -> Option<ConnectionConfig> {
    let url = env("DBINE_TEST_PGVECTOR_URL")?;
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

fn libsql() -> Option<ConnectionConfig> {
    Some(ConnectionConfig {
        driver: "libsql".into(),
        host: env("DBINE_TEST_LIBSQL_URL")?,
        ..Default::default()
    })
}

const MONGO_DB: &str = "dbine_xbits";

fn atlas() -> Option<ConnectionConfig> {
    let mut c = ConnectionConfig {
        driver: "mongodb".into(),
        database: MONGO_DB.into(),
        ..Default::default()
    };
    c.options
        .insert("connection_string".into(), env("DBINE_TEST_ATLAS_URL")?);
    Some(c)
}

fn oracle() -> Option<ConnectionConfig> {
    let url = env("DBINE_TEST_ORACLE_URL")?;
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

async fn connect(cfg: &ConnectionConfig, db: Option<&str>) -> Box<dyn Session> {
    driver(&cfg.driver)
        .connect(cfg, db)
        .await
        .unwrap_or_else(|e| panic!("connect {}: {e}", cfg.driver))
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

/// A binary vector as its bit string, whatever the form a result gives
/// it: a JSON array (or its text) of 0 and 1, or of libSQL's signs
/// (`vector_extract` writes a 1-bit vector as 1 and -1).
fn bits_of(v: &Value) -> Option<String> {
    let nums = match v {
        Value::Null => return None,
        Value::String(s) => dbine_driver::vector::parse(s)?,
        other => dbine_driver::vector::parse_value(other)?,
    };
    Some(
        nums.into_iter()
            .map(|x| if x > 0.0 { '1' } else { '0' })
            .collect(),
    )
}

fn id_of(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("id {v}"))
}

/// `(id, bits)` rows, sorted by id.
fn id_and_bits(rows: Vec<Vec<Value>>) -> Vec<(i64, Option<String>)> {
    let mut out: Vec<_> = rows
        .iter()
        .map(|r| (id_of(&r[0]), bits_of(r.get(1).unwrap_or(&Value::Null))))
        .collect();
    out.sort_by_key(|r| r.0);
    out
}

fn want() -> Vec<(i64, Option<String>)> {
    ROWS.iter()
        .map(|(i, b)| (*i, b.map(str::to_string)))
        .collect()
}

/// The source in pgvector: `bit(16)` with an HNSW Hamming index, as a
/// table of binary vectors is made there.
async fn pg_source(pg: &mut dyn Session) {
    run(pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
    let values: Vec<String> = ROWS
        .iter()
        .map(|(i, b)| match b {
            Some(b) => format!("({i}, B'{b}')"),
            None => format!("({i}, NULL)"),
        })
        .collect();
    run(
        pg,
        &format!(
            "CREATE TABLE {SRC} (id int PRIMARY KEY, emb bit(16)); \
             CREATE INDEX {SRC}_ix ON {SRC} USING hnsw (emb bit_hamming_ops); \
             INSERT INTO {SRC} VALUES {}",
            values.join(", ")
        ),
    )
    .await;
    // `bit` reads as its bit string.
    let got: Vec<(i64, Option<String>)> =
        rows(pg, &format!("SELECT id, emb::text FROM {SRC} ORDER BY id"))
            .await
            .iter()
            .map(|r| (id_of(&r[0]), r[1].as_str().map(str::to_string)))
            .collect();
    assert_eq!(got, want(), "the source");
}

async fn pg_drop(pgc: &ConnectionConfig) {
    let mut pg = connect(pgc, None).await;
    let _ = try_run(&mut *pg, &format!("DROP TABLE IF EXISTS {SRC}")).await;
}

/// The two ends of a transfer.
struct Ends {
    source: ConnectionConfig,
    target: (ConnectionConfig, Option<String>),
}

#[async_trait]
impl Endpoints for Ends {
    fn source_driver(&self) -> Arc<dyn Driver> {
        driver(&self.source.driver)
    }
    fn target_driver(&self) -> Arc<dyn Driver> {
        driver(&self.target.0.driver)
    }
    async fn open_source(&self) -> dbine_driver::Result<Box<dyn Session>> {
        self.source_driver().connect(&self.source, None).await
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

/// The source's `id` and `emb` into `target`'s `columns`, by the transfer
/// engine, as a migration's job: the target's describe as the expected
/// columns and `columns[1]` marked as a vector (what the migration gives
/// from the type the conversion made). The run must finish with no table
/// failed.
async fn transfer(
    pgc: &ConnectionConfig,
    target: (ConnectionConfig, Option<String>),
    table: ObjectRef,
    columns: [&str; 2],
    tag: &str,
) {
    let mut t = connect(&target.0, target.1.as_deref()).await;
    let expected: Vec<TransferColumn> = t
        .columns(&table)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|c| TransferColumn {
            name: c.name,
            type_name: c.data_type,
            nullable: c.nullable,
        })
        .collect();
    println!("{tag}: the target describes {expected:?}");
    let job = TransferJob {
        name: format!("{SRC} -> {}", table.name),
        source: ReadSpec {
            table: ObjectRef {
                kind: "table".into(),
                schema: Some("public".into()),
                name: SRC.into(),
            },
            columns: Some(vec!["id".into(), "emb".into()]),
            filter: None,
        },
        target: LoadSpec {
            table,
            columns: columns.iter().map(|c| c.to_string()).collect(),
            table_lock: false,
            keep_identity: true,
            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        },
        row_estimate: None,
        truncate: None,
        empty_first: false,
        before: String::new(),
        after: String::new(),
        post: Vec::new(),
        preexisting: false,
        expected_columns: expected,
        vector_columns: vec![columns[1].into()],
        mode: TransferMode::Copy,
    };
    let path = std::env::temp_dir().join(format!(
        "dbine-vec-bits-{tag}-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let store = Arc::new(Store::open(&path).expect("transfer state"));
    let ends = Ends {
        source: pgc.clone(),
        target,
    };
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

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        ..Default::default()
    }
}

/// The converted table: `id` and the binary vector `emb`, with the vector
/// index the conversion gives the source's Hamming one.
fn converted(
    kind: &str,
    name: &str,
    columns: [(&str, &str); 2],
    index: (&str, Option<&str>, VectorIndex),
) -> TableSchema {
    let mut id = col(columns[0].0, columns[0].1);
    id.nullable = false;
    TableSchema {
        kind: kind.into(),
        name: name.into(),
        columns: vec![id, col(columns[1].0, columns[1].1)],
        primary_key: (kind == "table").then(|| dbine_driver::KeyDef {
            name: None,
            columns: vec![columns[0].0.into()],
        }),
        indexes: vec![IndexDef {
            name: index.0.into(),
            columns: vec![columns[1].0.into()],
            kind: index.1.map(str::to_string),
            vector: Some(index.2),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn metric(m: &str) -> VectorIndex {
    VectorIndex {
        metric: Some(m.into()),
        ..Default::default()
    }
}

const TABLE: DdlParts = DdlParts {
    drop: false,
    if_exists: false,
    create: true,
    indexes: false,
    foreign_keys: false,
};

const INDEXES: DdlParts = DdlParts {
    drop: false,
    if_exists: false,
    create: false,
    indexes: true,
    foreign_keys: false,
};

// ---------------------------------------------------------------- libSQL

const LIB_T: &str = "zz_vec_bits";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and libSQL (sqld): see the top of the file"]
async fn pgvector_bits_into_libsql() {
    let (Some(pgc), Some(lc)) = (pgvector(), libsql()) else {
        eprintln!("DBINE_TEST_PGVECTOR_URL or DBINE_TEST_LIBSQL_URL not set; skipping");
        return;
    };
    let drop_all = |pgc: ConnectionConfig, lc: ConnectionConfig| async move {
        pg_drop(&pgc).await;
        let mut l = connect(&lc, None).await;
        let _ = try_run(&mut *l, &format!("DROP TABLE IF EXISTS {LIB_T}")).await;
    };
    drop_all(pgc.clone(), lc.clone()).await;
    let (p, l) = (pgc.clone(), lc.clone());
    with_cleanup(
        async move {
            let mut pg = connect(&p, None).await;
            pg_source(&mut *pg).await;
            let d = driver("libsql");
            let t = converted(
                "table",
                LIB_T,
                [("id", "INTEGER"), ("emb", "F1BIT_BLOB(16)")],
                ("zz_vec_bits_ix", None, metric("cosine")),
            );
            let mut lib = connect(&l, None).await;
            run(&mut *lib, &d.table_ddl(&t, TABLE).unwrap()).await;
            let obj = ObjectRef {
                kind: "table".into(),
                schema: None,
                name: LIB_T.into(),
            };
            transfer(&p, (l.clone(), None), obj, ["id", "emb"], "libsql").await;
            // The index after the copy, as a migration makes it.
            let ix = d.table_ddl(&t, INDEXES).unwrap();
            assert!(ix.contains("'metric=cosine'"), "{ix}");
            run(&mut *lib, &ix).await;

            let got = id_and_bits(
                rows(
                    &mut *lib,
                    &format!(
                        "SELECT id, CASE WHEN emb IS NULL THEN NULL ELSE vector_extract(emb) END \
                         FROM {LIB_T} ORDER BY id"
                    ),
                )
                .await,
            );
            assert_eq!(got, want(), "libSQL");
            let blob_len = rows(
                &mut *lib,
                &format!("SELECT length(emb) FROM {LIB_T} WHERE id = 1"),
            )
            .await;
            println!("libsql: a 16-bit vector is {} bytes", blob_len[0][0]);

            // Searched by its index: cosine over 1-bit vectors counts the
            // bits that differ.
            let q = "vector1bit('[1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]')";
            let found = rows(
                &mut *lib,
                &format!(
                    "SELECT t.id, vector_distance_cos(z.emb, {q}) \
                     FROM vector_top_k('zz_vec_bits_ix', {q}, 3) t \
                     JOIN {LIB_T} z ON z.rowid = t.id"
                ),
            )
            .await;
            let ids: Vec<i64> = found.iter().map(|r| id_of(&r[0])).collect();
            let dist: Vec<f64> = found.iter().filter_map(|r| r[1].as_f64()).collect();
            assert_eq!(ids, NEAREST, "{found:?}");
            assert_eq!(dist, [0.0, 1.0, 2.0], "Hamming distances: {found:?}");
        },
        drop_all(pgc, lc),
    )
    .await;
}

// ---------------------------------------------------------------- MongoDB

const MONGO_C: &str = "zz_vec_bits";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and Atlas Local: see the top of the file"]
async fn pgvector_bits_into_atlas() {
    let (Some(pgc), Some(mc)) = (pgvector(), atlas()) else {
        eprintln!("DBINE_TEST_PGVECTOR_URL or DBINE_TEST_ATLAS_URL not set; skipping");
        return;
    };
    let drop_all = |pgc: ConnectionConfig, mc: ConnectionConfig| async move {
        pg_drop(&pgc).await;
        let mut m = connect(&mc, Some(MONGO_DB)).await;
        let _ = try_run(&mut *m, "db.dropDatabase()").await;
    };
    drop_all(pgc.clone(), mc.clone()).await;
    let (p, m) = (pgc.clone(), mc.clone());
    with_cleanup(
        async move {
            let mut pg = connect(&p, None).await;
            pg_source(&mut *pg).await;
            let d = driver("mongodb");
            let t = converted(
                "collection",
                MONGO_C,
                [("id", "int"), ("emb", "vector(16, bit)")],
                ("zz_vec_bits_ix", None, metric("l2")),
            );
            let mut mongo = connect(&m, Some(MONGO_DB)).await;
            run(&mut *mongo, &d.table_ddl(&t, TABLE).unwrap()).await;
            let obj = ObjectRef {
                kind: "collection".into(),
                schema: None,
                name: MONGO_C.into(),
            };
            transfer(
                &p,
                (m.clone(), Some(MONGO_DB.into())),
                obj,
                ["id", "emb"],
                "mongodb",
            )
            .await;
            let ix = d.table_ddl(&t, INDEXES).unwrap();
            assert!(ix.contains("createSearchIndex"), "{ix}");
            run(&mut *mongo, &ix).await;

            let coll = format!("db.getCollection(\"{MONGO_C}\")");
            let r = rows(
                &mut *mongo,
                &format!("{coll}.find({{}}, {{ _id: 0, id: 1, emb: 1 }}).sort({{ id: 1 }})"),
            )
            .await;
            println!("mongodb: {r:?}");
            // Rows without the field leave it out: the NULL one has none.
            let got = id_and_bits(r);
            assert_eq!(got, want(), "MongoDB");
            let strings = rows(
                &mut *mongo,
                &format!(
                    "{coll}.aggregate([{{ $match: {{ emb: {{ $type: \"string\" }} }} }}, {{ $count: \"n\" }}])"
                ),
            )
            .await;
            assert!(strings.is_empty(), "no bit string arrived as text: {strings:?}");

            // Searched there (euclidean over 0 and 1 ranks as Hamming),
            // once mongot has built the index.
            let q = "[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]";
            let search = format!(
                "{coll}.aggregate([{{ $vectorSearch: {{ index: \"zz_vec_bits_ix\", path: \"emb\", queryVector: {q}, numCandidates: 10, limit: 3 }} }}, {{ $project: {{ _id: 0, id: 1 }} }}])"
            );
            let start = Instant::now();
            let found = loop {
                match try_run(&mut *mongo, &search).await {
                    Ok(o) if o.results.first().is_some_and(|r| r.rows.len() == 3) => {
                        break o.results[0].rows.clone()
                    }
                    _ if start.elapsed() < Duration::from_secs(120) => {
                        tokio::time::sleep(Duration::from_secs(1)).await
                    }
                    other => panic!("the vector search never answered: {other:?}"),
                }
            };
            let ids: Vec<i64> = found.iter().map(|r| id_of(&r[0])).collect();
            assert_eq!(ids, NEAREST, "{found:?}");
        },
        drop_all(pgc, mc),
    )
    .await;
}

// ---------------------------------------------------------------- Oracle

const ORA_T: &str = "ZZ_VEC_BITS";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs pgvector and Oracle 23ai: see the top of the file"]
async fn pgvector_bits_into_oracle() {
    let (Some(pgc), Some(oc)) = (pgvector(), oracle()) else {
        eprintln!("DBINE_TEST_PGVECTOR_URL or DBINE_TEST_ORACLE_URL not set; skipping");
        return;
    };
    let drop_all = |pgc: ConnectionConfig, oc: ConnectionConfig| async move {
        pg_drop(&pgc).await;
        let mut o = connect(&oc, None).await;
        let _ = try_run(&mut *o, &format!("DROP TABLE {ORA_T} PURGE")).await;
    };
    drop_all(pgc.clone(), oc.clone()).await;
    let (p, o) = (pgc.clone(), oc.clone());
    with_cleanup(
        async move {
            let mut pg = connect(&p, None).await;
            pg_source(&mut *pg).await;
            let d = driver("oracle");
            let mut hnsw = metric("hamming");
            hnsw.algorithm = Some("hnsw".into());
            let mut t = converted(
                "table",
                ORA_T,
                [("ID", "NUMBER(10)"), ("EMB", "VECTOR(16, BINARY)")],
                ("ZZ_VEC_BITS_IX", None, hnsw),
            );
            let mut ora = connect(&o, None).await;
            run(&mut *ora, &d.table_ddl(&t, TABLE).unwrap()).await;
            let obj = ObjectRef {
                kind: "table".into(),
                schema: None,
                name: ORA_T.into(),
            };
            transfer(&p, (o.clone(), None), obj, ["ID", "EMB"], "oracle").await;
            // HNSW needs the vector memory pool; IVF where the instance
            // has none.
            let ix = d.table_ddl(&t, INDEXES).unwrap();
            assert!(ix.contains("DISTANCE HAMMING"), "{ix}");
            if let Err(e) = try_run(&mut *ora, &ix).await {
                assert!(e.contains("ORA-51962"), "{e}\n{ix}");
                eprintln!("NOTE: no vector memory pool (ORA-51962): an IVF index instead");
                t.indexes[0].vector.as_mut().unwrap().algorithm = Some("ivf".into());
                run(&mut *ora, &d.table_ddl(&t, INDEXES).unwrap()).await;
            }

            let got = id_and_bits(
                rows(
                    &mut *ora,
                    &format!("SELECT ID, EMB FROM {ORA_T} ORDER BY ID"),
                )
                .await,
            );
            assert_eq!(got, want(), "Oracle");
            let packed = rows(
                &mut *ora,
                &format!("SELECT FROM_VECTOR(EMB) FROM {ORA_T} WHERE ID = 2"),
            )
            .await;
            assert_eq!(packed[0][0], Value::from("[224,0]"), "packed bits");

            // Searched by Hamming, approximately (through the index).
            let found = rows(
                &mut *ora,
                &format!(
                    "SELECT ID, VECTOR_DISTANCE(EMB, TO_VECTOR('[128,0]', 16, BINARY), HAMMING) D \
                     FROM {ORA_T} WHERE EMB IS NOT NULL \
                     ORDER BY VECTOR_DISTANCE(EMB, TO_VECTOR('[128,0]', 16, BINARY), HAMMING) \
                     FETCH APPROX FIRST 3 ROWS ONLY"
                ),
            )
            .await;
            let ids: Vec<i64> = found.iter().map(|r| id_of(&r[0])).collect();
            assert_eq!(ids, NEAREST, "{found:?}");
            let dist: Vec<f64> = found
                .iter()
                .filter_map(|r| r[1].as_f64().or_else(|| r[1].as_str()?.parse().ok()))
                .collect();
            assert_eq!(dist, [0.0, 1.0, 2.0], "Hamming distances: {found:?}");
        },
        drop_all(pgc, oc),
    )
    .await;
}
