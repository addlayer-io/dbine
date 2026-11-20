//! Vectors through the migration's own code (`plan` and `run_migration`, what "Migrar" runs:
//! the conversion with the target's capabilities, the target driver's DDL, the transfer engine
//! with the jobs the migration builds) against live servers:
//!
//! - pgvector: `dbine-test-pgvector` (`DBINE_TEST_PGVECTOR_URL`, default
//!   `postgres://postgres:pw@127.0.0.1:27200`);
//! - libSQL: `dbine-test-libsql` (`DBINE_TEST_LIBSQL_URL`, default `http://127.0.0.1:25880`);
//! - MongoDB Atlas Local: `dbine-test-atlas` (`DBINE_TEST_ATLAS_URL`, default
//!   `mongodb://127.0.0.1:27205/?directConnection=true`);
//! - CockroachDB: `dbine-test-cockroach` (`DBINE_TEST_COCKROACH_URL`, default
//!   `postgres://root@127.0.0.1:26014/defaultdb`), optional;
//! - CrateDB: `dbine-test-cratedb` (`DBINE_TEST_CRATEDB_URL`, default
//!   `postgres://crate@127.0.0.1:25021/doc`).
//!
//! Ignored by default:
//!
//! ```sh
//! cargo test -p dbine --lib -- --ignored vector_live_migration --test-threads=1 --nocapture
//! ```
//!
//! Each test drops what it makes before it starts and when it ends, also when a check failed.

use super::*;
use dbine_core::{SavedConnection, StateStore};
use dbine_driver::{ConnectionConfig, QueryOutcome};
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;

type Res<T> = Result<T, String>;

macro_rules! check {
    ($cond:expr, $($msg:tt)+) => {
        if !$cond {
            return Err(format!($($msg)+));
        }
    };
}

fn env_or(k: &str, default: &str) -> String {
    std::env::var(k)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.into())
}

/// A `postgres://user[:pw]@host:port[/db]` URL as a config of `driver` on `database`.
fn pg_like(driver: &str, url: &str, database: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split(['/', '?']).next().unwrap_or(rest);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "5432"));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap_or(5432),
        username: Some(user.into()),
        password: (!pass.is_empty()).then(|| pass.to_string()),
        database: database.into(),
        ..Default::default()
    }
}

fn pgvector(database: &str) -> ConnectionConfig {
    pg_like(
        "postgres",
        &env_or(
            "DBINE_TEST_PGVECTOR_URL",
            "postgres://postgres:pw@127.0.0.1:27200",
        ),
        database,
    )
}

fn cockroach(database: &str) -> ConnectionConfig {
    pg_like(
        "cockroachdb",
        &env_or(
            "DBINE_TEST_COCKROACH_URL",
            "postgres://root@127.0.0.1:26014/defaultdb",
        ),
        database,
    )
}

fn cratedb() -> ConnectionConfig {
    pg_like(
        "cratedb",
        &env_or(
            "DBINE_TEST_CRATEDB_URL",
            "postgres://crate@127.0.0.1:25021/doc",
        ),
        "doc",
    )
}

fn libsql() -> ConnectionConfig {
    ConnectionConfig {
        driver: "libsql".into(),
        host: env_or("DBINE_TEST_LIBSQL_URL", "http://127.0.0.1:25880"),
        ..Default::default()
    }
}

fn atlas(database: &str) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: "mongodb".into(),
        database: database.into(),
        ..Default::default()
    };
    c.options.insert(
        "connection_string".into(),
        env_or(
            "DBINE_TEST_ATLAS_URL",
            "mongodb://127.0.0.1:27205/?directConnection=true",
        ),
    );
    c
}

async fn open(cfg: &ConnectionConfig) -> Res<Box<dyn Session>> {
    let d = dbine_drivers::find(&cfg.driver).ok_or(format!("no driver {}", cfg.driver))?;
    let db = (!cfg.database.is_empty()).then_some(cfg.database.as_str());
    d.connect(cfg, db)
        .await
        .map_err(|e| format!("connect {}: {e}", cfg.driver))
}

async fn sql(cfg: &ConnectionConfig, text: &str) -> Res<QueryOutcome> {
    let mut s = open(cfg).await?;
    let mut out = QueryOutcome::default();
    s.execute(text, 100_000, &mut out)
        .await
        .map_err(|e| format!("{e}\n--- {text}"))?;
    match out.error.take() {
        Some(e) => Err(format!("{e}\n--- {text}")),
        None => Ok(out),
    }
}

/// The last result's rows.
async fn rows(cfg: &ConnectionConfig, text: &str) -> Res<Vec<Vec<Value>>> {
    Ok(sql(cfg, text)
        .await?
        .results
        .into_iter()
        .rev()
        .find(|r| !r.columns.is_empty())
        .map(|r| r.rows)
        .unwrap_or_default())
}

fn int(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str()?.parse().ok())
        .unwrap_or(-1)
}

/// A binary vector, as a result gives it (a JSON array or its text, of 0 / 1 or of libSQL's
/// 1 / -1), as its bit string.
fn bits(v: &Value) -> Option<String> {
    let n = match v {
        Value::String(s) => dbine_driver::vector::parse(s)?,
        other => dbine_driver::vector::parse_value(other)?,
    };
    Some(
        n.into_iter()
            .map(|x| if x > 0.0 { '1' } else { '0' })
            .collect(),
    )
}

/// A state of its own, in its own folder (removed at the end), with `conns` saved.
struct Env {
    state: AppState,
    runs: MigrationRuns,
    dir: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn env(tag: &str, conns: &[(&str, &ConnectionConfig)]) -> Env {
    let dir = std::env::temp_dir().join(format!(
        "dbine-migvec-{tag}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let state = AppState::new(StateStore::open(&dir.join("state.sqlite")).unwrap());
    for (id, cfg) in conns {
        let conn = SavedConnection {
            id: id.to_string(),
            name: id.to_string(),
            color: None,
            config: (*cfg).clone(),
            save_password: false,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        };
        state.store.save_connection(&conn).unwrap();
        state.typed_secrets.insert(
            id.to_string(),
            [(
                "password".to_string(),
                cfg.password.clone().unwrap_or_default(),
            )]
            .into_iter()
            .collect(),
        );
    }
    let runs = MigrationRuns::open(&dir);
    Env { state, runs, dir }
}

fn plan_args(database: &str, table: &str, target_driver: &str) -> PlanArgs {
    PlanArgs {
        connection_id: "src".into(),
        database: database.into(),
        tables: vec![TableRef {
            schema: None,
            name: table.into(),
        }],
        target_driver: target_driver.into(),
        options: PlanOptions {
            fold_case: true,
            target_schema: None,
            drop: false,
            if_exists: true,
            indexes: true,
            foreign_keys: true,
            data: true,
            keep_schemas: true,
        },
        mode: MigrationMode::Convert,
        sync: SyncOptions::default(),
        target: None,
    }
}

/// Runs the migration of `plan` into connection `tgt`; it must end `done`.
async fn migrate(e: &Env, id: &str, plan: PlanArgs, target_database: &str) -> Res<RunResult> {
    let args = RunArgs {
        migration_id: id.into(),
        plan,
        target_connection_id: "tgt".into(),
        target_database: target_database.into(),
        transfer: TransferOptions {
            parallel: Some(2),
            order: None,
            commit_rows: None,
        },
    };
    let r = run_migration(&e.state, &e.runs, args, Arc::new(|_| {}))
        .await
        .map_err(|e| e.to_string())?;
    check!(
        r.status == "done",
        "{id}: {} {:?} {:?}",
        r.status,
        r.tables
            .iter()
            .map(|t| (&t.name, &t.status, &t.error))
            .collect::<Vec<_>>(),
        r.notes
    );
    Ok(r)
}

fn issues_of(p: &MigrationPlan, code: IssueCode) -> Vec<&Issue> {
    p.issues.iter().filter(|i| i.code == code).collect()
}

/// Runs `body`, then `cleanup`, also when `body` failed or panicked; both must succeed.
async fn guarded<B, C>(body: B, cleanup: C)
where
    B: std::future::Future<Output = Res<()>>,
    C: std::future::Future<Output = Res<()>>,
{
    let res = AssertUnwindSafe(body)
        .catch_unwind()
        .await
        .unwrap_or_else(|p| {
            Err(p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panicked".into()))
        });
    let cleaned = cleanup.await;
    res.unwrap();
    cleaned.expect("clean up at the end");
}

/// A fresh pgvector database `db`, with the extension.
async fn pg_db(db: &str) -> Res<()> {
    pg_db_drop(db).await?;
    sql(&pgvector("postgres"), &format!("CREATE DATABASE {db}")).await?;
    sql(&pgvector(db), "CREATE EXTENSION IF NOT EXISTS vector")
        .await
        .map(|_| ())
}

async fn pg_db_drop(db: &str) -> Res<()> {
    sql(
        &pgvector("postgres"),
        &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
    )
    .await
    .map(|_| ())
}

/// A vector of `n` dimensions, for a libSQL `vector32('…')`.
fn wide(n: usize, seed: usize) -> String {
    let v: Vec<String> = (0..n)
        .map(|i| format!("{}", ((i * 7 + seed) % 13) as f32 / 8.0 - 0.5))
        .collect();
    format!("[{}]", v.join(","))
}

// -- A4: pgvector's dimension limit per target ------------------------------------------------------

const WIDE: &str = "zz_migvec_wide";
const WIDE_DB: &str = "dbine_e2e_migvec_wide";

/// A 3072-dimension vector with a vector index (libSQL's) migrated into PostgreSQL: pgvector's
/// HNSW indexes up to 2000 dimensions of `vector`, so the plan leaves the index out with the
/// reason and still builds, and the run creates the table and copies the rows. Into CockroachDB
/// (its own vector indexes take more), the plan keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs dbine-test-libsql and dbine-test-pgvector (CockroachDB optional)"]
async fn vector_live_migration_leaves_out_a_pgvector_index_past_its_dimensions() {
    let lib = libsql();
    let clean = || async {
        sql(&libsql(), &format!("DROP TABLE IF EXISTS {WIDE}")).await?;
        pg_db_drop(WIDE_DB).await?;
        if open(&cockroach("defaultdb")).await.is_ok() {
            sql(
                &cockroach("defaultdb"),
                &format!("DROP DATABASE IF EXISTS {WIDE_DB} CASCADE"),
            )
            .await?;
        }
        Ok::<(), String>(())
    };
    clean().await.expect("clean up first");
    let body = async {
        sql(
            &lib,
            &format!(
                "CREATE TABLE {WIDE} (id INTEGER PRIMARY KEY, emb F32_BLOB(3072)); \
                 CREATE INDEX {WIDE}_ix ON {WIDE} (libsql_vector_idx(emb, 'metric=cosine')); \
                 INSERT INTO {WIDE} VALUES (1, vector32('{}')), (2, vector32('{}')), (3, NULL)",
                wide(3072, 1),
                wide(3072, 5)
            ),
        )
        .await?;
        pg_db(WIDE_DB).await?;
        let e = env("wide", &[("src", &lib), ("tgt", &pgvector(WIDE_DB))]);

        // The plan: the index is left out, with the reason; the script builds.
        let p = plan(&e.state, &plan_args("", WIDE, "postgres"))
            .await
            .map_err(|e| e.to_string())?;
        let dropped = issues_of(&p, IssueCode::IndexDropped);
        println!("postgres: {dropped:?}");
        check!(
            dropped
                .iter()
                .any(|i| i.message.contains("2000") && i.message.contains("3072")),
            "no dimension reason: {:?}",
            p.issues
        );
        check!(p.script.contains("vector(3072)"), "{}", p.script);
        check!(
            !p.script.to_ascii_lowercase().contains("using hnsw"),
            "{}",
            p.script
        );

        // The run: the table and its rows, no index.
        let r = migrate(&e, "wide-pg", plan_args("", WIDE, "postgres"), WIDE_DB).await?;
        println!(
            "postgres run: {:?}",
            r.tables
                .iter()
                .map(|t| (&t.status, t.rows_done))
                .collect::<Vec<_>>()
        );
        let n = rows(
            &pgvector(WIDE_DB),
            &format!("SELECT count(*), count(emb) FROM {WIDE}"),
        )
        .await?;
        check!((int(&n[0][0]), int(&n[0][1])) == (3, 2), "{n:?}");
        let ix = rows(
            &pgvector(WIDE_DB),
            &format!("SELECT indexdef FROM pg_indexes WHERE tablename = '{WIDE}'"),
        )
        .await?;
        check!(
            !ix.iter()
                .any(|r| r[0].as_str().unwrap_or("").contains("hnsw")),
            "{ix:?}"
        );
        let dims = rows(
            &pgvector(WIDE_DB),
            &format!("SELECT vector_dims(emb) FROM {WIDE} WHERE id = 1"),
        )
        .await?;
        check!(int(&dims[0][0]) == 3072, "{dims:?}");

        // Into CockroachDB the index stays.
        let p = plan(&e.state, &plan_args("", WIDE, "cockroachdb"))
            .await
            .map_err(|e| e.to_string())?;
        check!(
            !issues_of(&p, IssueCode::IndexDropped)
                .iter()
                .any(|i| i.object.as_deref() == Some(&format!("{WIDE}_ix"))),
            "CockroachDB: {:?}",
            p.issues
        );
        check!(
            p.script.contains("CREATE VECTOR INDEX"),
            "CockroachDB:\n{}",
            p.script
        );
        if open(&cockroach("defaultdb")).await.is_ok() {
            sql(
                &cockroach("defaultdb"),
                &format!("CREATE DATABASE {WIDE_DB}"),
            )
            .await?;
            let e = env("wide-crdb", &[("src", &lib), ("tgt", &cockroach(WIDE_DB))]);
            let r = migrate(&e, "wide-crdb", plan_args("", WIDE, "cockroachdb"), WIDE_DB).await?;
            println!(
                "cockroachdb run: {:?}",
                r.tables
                    .iter()
                    .map(|t| (&t.status, t.rows_done))
                    .collect::<Vec<_>>()
            );
            let ix = rows(
                &cockroach(WIDE_DB),
                &format!("SELECT index_name FROM [SHOW INDEXES FROM {WIDE}]"),
            )
            .await?;
            println!("cockroachdb indexes: {ix:?}");
            check!(
                ix.iter()
                    .any(|r| r[0].as_str() == Some(&format!("{WIDE}_ix"))),
                "{ix:?}"
            );
        } else {
            println!("NOTE: CockroachDB not reachable: only its plan was checked");
        }
        Ok(())
    };
    guarded(body, clean()).await;
}

// -- A4 notes, C2: what the conversion report says about vector indexes -------------------------------

const BITS: &str = "zz_migvec_bits";
const BITS_DB: &str = "dbine_e2e_migvec_bits";

async fn pg_bits_source() -> Res<()> {
    pg_db(BITS_DB).await?;
    sql(
        &pgvector(BITS_DB),
        &format!(
            "CREATE TABLE {BITS} (id int PRIMARY KEY, emb bit(16)); \
             CREATE INDEX {BITS}_ix ON {BITS} USING hnsw (emb bit_hamming_ops); \
             INSERT INTO {BITS} VALUES (1, B'1000000000000000'), (2, B'1110000000000000'), \
             (3, B'0111111111111111'), (4, B'1100000000000000'), (5, NULL)"
        ),
    )
    .await
    .map(|_| ())
}

/// The bits every target must hold, by id.
fn want_bits() -> Vec<(i64, Option<String>)> {
    vec![
        (1, Some("1000000000000000".into())),
        (2, Some("1110000000000000".into())),
        (3, Some("0111111111111111".into())),
        (4, Some("1100000000000000".into())),
        (5, None),
    ]
}

fn id_bits(rows: Vec<Vec<Value>>) -> Vec<(i64, Option<String>)> {
    let mut out: Vec<_> = rows
        .iter()
        .map(|r| (int(&r[0]), r.get(1).and_then(bits)))
        .collect();
    out.sort_by_key(|r| r.0);
    out
}

/// pgvector's `bit(16)` with a Hamming index: into MongoDB it's searched by euclidean and into
/// libSQL by cosine (both rank bits as Hamming), each said in the plan; libSQL's 1-bit vectors
/// with an L2 index become pgvector's `bit(16)` searched by Hamming, said too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs dbine-test-pgvector and dbine-test-libsql"]
async fn vector_live_migration_plan_says_how_vector_indexes_change() {
    const LBITS: &str = "zz_migvec_lbits";
    let clean = || async {
        pg_db_drop(BITS_DB).await?;
        sql(&libsql(), &format!("DROP TABLE IF EXISTS {LBITS}"))
            .await
            .map(|_| ())
    };
    clean().await.expect("clean up first");
    let body = async {
        pg_bits_source().await?;
        let e = env("notes", &[("src", &pgvector(BITS_DB))]);
        let changed = |p: &MigrationPlan| {
            issues_of(p, IssueCode::IndexChanged)
                .iter()
                .map(|i| i.message.clone())
                .collect::<Vec<_>>()
        };
        let p = plan(&e.state, &plan_args(BITS_DB, BITS, "mongodb"))
            .await
            .map_err(|e| e.to_string())?;
        println!("mongodb: {:?}", changed(&p));
        check!(
            changed(&p)
                .iter()
                .any(|m| m.contains("hamming") && m.contains("euclidean")),
            "MongoDB: {:?}",
            p.issues
        );
        check!(
            p.script.contains("vector(16, bit)") || p.script.contains("createSearchIndex"),
            "{}",
            p.script
        );
        let p = plan(&e.state, &plan_args(BITS_DB, BITS, "libsql"))
            .await
            .map_err(|e| e.to_string())?;
        println!("libsql: {:?}", changed(&p));
        check!(
            changed(&p)
                .iter()
                .any(|m| m.contains("coseno") && m.contains("Hamming")),
            "libSQL: {:?}",
            p.issues
        );
        check!(
            issues_of(&p, IssueCode::IndexDropped).is_empty(),
            "libSQL: {:?}",
            p.issues
        );
        check!(
            p.script.contains("F1BIT_BLOB(16)") && p.script.contains("metric=cosine"),
            "{}",
            p.script
        );

        // libSQL → pgvector: L2 over 1-bit vectors is Hamming there.
        sql(
            &libsql(),
            &format!(
                "CREATE TABLE {LBITS} (id INTEGER PRIMARY KEY, emb F1BIT_BLOB(16)); \
                 CREATE INDEX {LBITS}_ix ON {LBITS} (libsql_vector_idx(emb, 'metric=l2'))"
            ),
        )
        .await?;
        let e = env("notes-lib", &[("src", &libsql())]);
        let p = plan(&e.state, &plan_args("", LBITS, "postgres"))
            .await
            .map_err(|e| e.to_string())?;
        println!("postgres: {:?}", changed(&p));
        check!(
            changed(&p).iter().any(|m| m.contains("l2 queda hamming")),
            "pgvector: {:?}",
            p.issues
        );
        check!(
            p.script.contains("bit(16)") && p.script.contains("bit_hamming_ops"),
            "{}",
            p.script
        );
        Ok(())
    };
    guarded(body, clean()).await;
}

// -- A5 + C1: pgvector's binary vectors migrated into libSQL and MongoDB ------------------------------

/// pgvector's `bit(16)` with a Hamming index migrated into libSQL: an `F1BIT_BLOB(16)` holding
/// the same bits (pgvector reads them as bit strings; the run hands libSQL its binary vectors),
/// with a cosine index that finds the nearest by Hamming.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs dbine-test-pgvector and dbine-test-libsql"]
async fn vector_live_migration_moves_pgvector_bits_into_libsql() {
    let clean = || async {
        pg_db_drop(BITS_DB).await?;
        sql(&libsql(), &format!("DROP TABLE IF EXISTS {BITS}"))
            .await
            .map(|_| ())
    };
    clean().await.expect("clean up first");
    let body = async {
        pg_bits_source().await?;
        let e = env(
            "bits-lib",
            &[("src", &pgvector(BITS_DB)), ("tgt", &libsql())],
        );
        migrate(&e, "bits-lib", plan_args(BITS_DB, BITS, "libsql"), "").await?;
        let got = id_bits(
            rows(
                &libsql(),
                &format!("SELECT id, CASE WHEN emb IS NULL THEN NULL ELSE vector_extract(emb) END FROM {BITS}"),
            )
            .await?,
        );
        check!(got == want_bits(), "libSQL: {got:?}");
        let ix = rows(
            &libsql(),
            &format!("SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = '{BITS}'"),
        )
        .await?;
        println!("libsql index: {ix:?}");
        check!(
            ix.iter()
                .any(|r| r[0].as_str().unwrap_or("").contains("metric=cosine")),
            "{ix:?}"
        );
        let q = "vector1bit('[1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]')";
        let found = rows(
            &libsql(),
            &format!(
                "SELECT t.id, vector_distance_cos(z.emb, {q}) FROM vector_top_k('{BITS}_ix', {q}, 3) t \
                 JOIN {BITS} z ON z.rowid = t.id"
            ),
        )
        .await?;
        let ids: Vec<i64> = found.iter().map(|r| int(&r[0])).collect();
        check!(ids == [1, 4, 2], "nearest by Hamming: {found:?}");
        Ok(())
    };
    guarded(body, clean()).await;
}

/// The same into MongoDB (Atlas Local): MongoDB describes a new, empty collection by no field,
/// so the run marks the vector from the type the conversion made, and the bits arrive as the
/// vector `vectorSearch` finds (never as the bit string's text).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs dbine-test-pgvector and dbine-test-atlas"]
async fn vector_live_migration_moves_pgvector_bits_into_atlas() {
    let clean = || async {
        pg_db_drop(BITS_DB).await?;
        sql(&atlas(BITS_DB), "db.dropDatabase()").await.map(|_| ())
    };
    clean().await.expect("clean up first");
    let body = async {
        pg_bits_source().await?;
        let e = env(
            "bits-atlas",
            &[("src", &pgvector(BITS_DB)), ("tgt", &atlas(BITS_DB))],
        );
        migrate(
            &e,
            "bits-atlas",
            plan_args(BITS_DB, BITS, "mongodb"),
            BITS_DB,
        )
        .await?;
        let coll = format!("db.getCollection(\"{BITS}\")");
        let got = id_bits(
            rows(
                &atlas(BITS_DB),
                &format!("{coll}.find({{}}, {{ _id: 0, id: 1, emb: 1 }})"),
            )
            .await?,
        );
        check!(got == want_bits(), "MongoDB: {got:?}");
        let strings = rows(
            &atlas(BITS_DB),
            &format!("{coll}.aggregate([{{ $match: {{ emb: {{ $type: \"string\" }} }} }}, {{ $count: \"n\" }}])"),
        )
        .await?;
        check!(
            strings.is_empty(),
            "bit strings arrived as text: {strings:?}"
        );
        let search = format!(
            "{coll}.aggregate([{{ $vectorSearch: {{ index: \"{BITS}_ix\", path: \"emb\", \
             queryVector: [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], numCandidates: 10, limit: 3 }} }}, \
             {{ $project: {{ _id: 0, id: 1 }} }}])"
        );
        let start = Instant::now();
        let found = loop {
            match rows(&atlas(BITS_DB), &search).await {
                Ok(r) if r.len() == 3 => break r,
                _ if start.elapsed() < std::time::Duration::from_secs(120) => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await
                }
                other => return Err(format!("the vector search never answered: {other:?}")),
            }
        };
        let ids: Vec<i64> = found.iter().map(|r| int(&r[0])).collect();
        check!(ids == [1, 4, 2], "nearest by Hamming: {found:?}");
        Ok(())
    };
    guarded(body, clean()).await;
}

// -- A1 spot check: PostgreSQL into CrateDB -----------------------------------------------------------

const CR_DB: &str = "dbine_e2e_migvec_crate";

/// A parent / child pair with a foreign key and a secondary index migrated from PostgreSQL into
/// CrateDB, which has neither: the plan leaves them out with their reasons and the run creates
/// the tables and copies the rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs dbine-test-pgvector and dbine-test-cratedb"]
async fn vector_live_migration_into_cratedb_leaves_out_keys_and_indexes() {
    let clean = || async {
        pg_db_drop(CR_DB).await?;
        // The tables keep their source schema (`public`) there.
        for t in [
            "public.zz_migcr_child",
            "public.zz_migcr_parent",
            "doc.zz_migcr_child",
            "doc.zz_migcr_parent",
        ] {
            sql(&cratedb(), &format!("DROP TABLE IF EXISTS {t}")).await?;
        }
        Ok::<(), String>(())
    };
    clean().await.expect("clean up first");
    let body = async {
        pg_db(CR_DB).await?;
        sql(
            &pgvector(CR_DB),
            "CREATE TABLE zz_migcr_parent (id int PRIMARY KEY, name text NOT NULL); \
             CREATE TABLE zz_migcr_child (id int PRIMARY KEY, parent_id int NOT NULL REFERENCES zz_migcr_parent(id), \
             note text, emb vector(3)); \
             CREATE INDEX zz_migcr_child_parent ON zz_migcr_child (parent_id); \
             INSERT INTO zz_migcr_parent VALUES (1, 'a'), (2, 'b'); \
             INSERT INTO zz_migcr_child VALUES (10, 1, 'x', '[1,2,3]'), (11, 2, 'y', NULL), (12, 1, NULL, '[0.5,0,-1]')",
        )
        .await?;
        let e = env("crate", &[("src", &pgvector(CR_DB)), ("tgt", &cratedb())]);
        let mut args = plan_args(CR_DB, "zz_migcr_parent", "cratedb");
        args.tables.push(TableRef {
            schema: None,
            name: "zz_migcr_child".into(),
        });
        let p = plan(&e.state, &args).await.map_err(|e| e.to_string())?;
        let fks = issues_of(&p, IssueCode::ForeignKeyDropped);
        let ixs = issues_of(&p, IssueCode::IndexDropped);
        println!("cratedb fks: {fks:?}\ncratedb indexes: {ixs:?}");
        check!(
            !fks.is_empty() && fks.iter().all(|i| !i.message.is_empty()),
            "{:?}",
            p.issues
        );
        check!(
            ixs.iter()
                .any(|i| i.object.as_deref() == Some("zz_migcr_child_parent")),
            "{:?}",
            p.issues
        );
        check!(
            !p.script.contains("REFERENCES") && !p.script.contains("CREATE INDEX"),
            "{}",
            p.script
        );
        migrate(&e, "crate", args, "doc").await?;
        sql(
            &cratedb(),
            "REFRESH TABLE public.zz_migcr_parent, public.zz_migcr_child",
        )
        .await?;
        let n = rows(&cratedb(), "SELECT count(*) FROM public.zz_migcr_child").await?;
        check!(int(&n[0][0]) == 3, "{n:?}");
        let n = rows(&cratedb(), "SELECT count(*) FROM public.zz_migcr_parent").await?;
        check!(int(&n[0][0]) == 2, "{n:?}");
        let v = rows(
            &cratedb(),
            "SELECT emb FROM public.zz_migcr_child WHERE id = 10",
        )
        .await?;
        println!("cratedb emb: {v:?}");
        Ok(())
    };
    guarded(body, clean()).await;
}
