//! Vectors (embeddings) through the app's own paths against a real
//! pgvector server: `dbine-test-pgvector` (`pgvector/pgvector:pg17`, on
//! 127.0.0.1:27200, postgres/pw; `DBINE_TEST_PGVECTOR_URL` replaces it).
//!
//! Ignored by default:
//!
//! ```sh
//! cargo test -p dbine --lib -- --ignored vector_live --test-threads=1 --nocapture
//! ```
//!
//! Each test works in a database of its own (`dbine_e2e_vecapp_*`, with the
//! `vector` extension), dropped before it starts and when it ends, also
//! when a check failed.

use dbine_driver::{ConnectionConfig, QueryOutcome, Session, StatementResult};
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;

pub(crate) type Res<T> = Result<T, String>;

/// `if !cond { return Err(…) }`, so the test still cleans up.
macro_rules! check {
    ($cond:expr, $($msg:tt)+) => {
        if !$cond {
            return Err(format!($($msg)+));
        }
    };
}
#[allow(unused_imports)]
pub(crate) use check;

/// The pgvector server, on database `db`.
pub(crate) fn pgvector(db: &str) -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_PGVECTOR_URL")
        .unwrap_or_else(|_| "postgres://postgres:pw@127.0.0.1:27200".into());
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let rest = rest.split(['/', '?']).next().unwrap_or(rest);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "5432"));
    ConnectionConfig {
        driver: "postgres".into(),
        host: host.into(),
        port: port.parse().unwrap_or(5432),
        username: Some(user.into()),
        password: Some(pass.into()),
        database: db.into(),
        ..Default::default()
    }
}

pub(crate) async fn open(cfg: &ConnectionConfig) -> Res<Box<dyn Session>> {
    let d = dbine_drivers::find(&cfg.driver).ok_or(format!("no driver {}", cfg.driver))?;
    let db = (!cfg.database.is_empty()).then_some(cfg.database.as_str());
    d.connect(cfg, db)
        .await
        .map_err(|e| format!("connect {}: {e}", cfg.driver))
}

/// Run `text`; an error in the outcome is an error.
pub(crate) async fn exec(s: &mut dyn Session, text: &str) -> Res<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100_000, &mut out)
        .await
        .map_err(|e| format!("{e}\n--- {text}"))?;
    if let Some(e) = out.error.take() {
        return Err(format!("{e}\n--- {text}"));
    }
    Ok(out)
}

/// The first result set with columns.
pub(crate) fn first(out: QueryOutcome) -> StatementResult {
    out.results
        .into_iter()
        .find(|r| !r.columns.is_empty())
        .unwrap_or_default()
}

/// A cell as text (`1` and `"1"` alike).
pub(crate) fn text(v: &serde_json::Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

/// `sql`'s first cell, run as the MCP's server-enforced read.
async fn count_read_only(s: &mut dyn Session, sql: &str) -> Res<String> {
    let mut out = QueryOutcome::default();
    s.run_read_only(sql, 1, &mut out)
        .await
        .map_err(|e| format!("{e}\n--- {sql}"))?;
    if let Some(e) = out.error.take() {
        return Err(format!("{e}\n--- {sql}"));
    }
    Ok(text(&first(out).rows[0][0]))
}

async fn drop_db(name: &str) -> Res<()> {
    let mut s = open(&pgvector("postgres")).await?;
    exec(
        s.as_mut(),
        &format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await?;
    drop(s);
    Ok(())
}

/// Run `body` on a fresh database `name` with pgvector, dropped before and
/// after (also when `body` fails or panics); `body`'s outcome is the test's.
pub(crate) async fn with_pgvector_db<F, Fut>(name: &str, body: F)
where
    F: FnOnce(ConnectionConfig) -> Fut,
    Fut: std::future::Future<Output = Res<()>>,
{
    drop_db(name).await.expect("clean up first");
    let setup = async {
        let mut s = open(&pgvector("postgres")).await?;
        exec(s.as_mut(), &format!("CREATE DATABASE {name}")).await?;
        drop(s);
        let mut s = open(&pgvector(name)).await?;
        exec(s.as_mut(), "CREATE EXTENSION vector").await?;
        drop(s);
        Ok::<(), String>(())
    }
    .await;
    let res = match setup {
        Ok(()) => AssertUnwindSafe(body(pgvector(name)))
            .catch_unwind()
            .await
            .unwrap_or_else(|p| {
                Err(p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "panicked".into()))
            }),
        Err(e) => Err(e),
    };
    let cleaned = drop_db(name).await;
    res.unwrap();
    cleaned.expect("clean up at the end");
}

/// C1: a similarity search on PostgreSQL with pgvector runs without the
/// user's approval (every function it calls is listed and the server
/// enforces the read), and the read-only transaction holds.
#[tokio::test]
#[ignore = "needs dbine-test-pgvector"]
async fn vector_live_similarity_search_reads_without_approval() {
    with_pgvector_db("dbine_e2e_vecapp_mcp", |cfg| async move {
        let mut s = open(&cfg).await?;
        exec(
            s.as_mut(),
            "CREATE TABLE items (id int PRIMARY KEY, embedding vector(3), b bit(3)); \
             INSERT INTO items VALUES (1, '[1,0,0]', '100'), (2, '[0,1,0]', '010'), (3, '[0.9,0.1,0]', '110')",
        )
        .await?;
        let queries = [
            "SELECT id, embedding <-> '[1,0,0]' AS d FROM items ORDER BY embedding <-> '[1,0,0]' LIMIT 2",
            "SELECT id FROM items ORDER BY embedding <=> '[1,0,0]'::vector(3) LIMIT 2",
            "SELECT id FROM items ORDER BY embedding <#> '[1,0,0]', embedding <+> '[1,0,0]' LIMIT 2",
            "SELECT id, cosine_distance(embedding, '[1,0,0]') AS c, l2_distance(embedding, '[1,0,0]'), \
             inner_product(embedding, '[1,0,0]'), l1_distance(embedding, '[1,0,0]') FROM items ORDER BY c LIMIT 2",
            "SELECT id, vector_dims(embedding), vector_norm(embedding), l2_normalize(embedding), \
             subvector(embedding, 1, 2), binary_quantize(embedding), hamming_distance(b, '100'), \
             jaccard_distance(b, '100'), b <~> '100', b <%> '100' FROM items ORDER BY id LIMIT 2",
        ];
        for q in queries {
            crate::mcp::reads::approval_free(
                dbine_driver::Language::Sql,
                "postgres",
                "postgres",
                &dbine_driver::ScriptDialect::postgres(),
                q,
            )
            .map_err(|f| format!("asks for approval because of {f}: {q}"))?;
            let mut out = QueryOutcome::default();
            s.run_read_only(q, 100, &mut out)
                .await
                .map_err(|e| format!("{e}\n--- {q}"))?;
            if let Some(e) = out.error {
                return Err(format!("{e}\n--- {q}"));
            }
            let r = first(out);
            check!(r.rows.len() == 2, "{q}: {:?}", r.rows);
            // The nearest to [1,0,0] first.
            check!(text(&r.rows[0][0]) == "1", "{q}: {:?}", r.rows);
        }
        // The read-only transaction is the server's: a function that
        // writes (one of the user's, so it asks) can't write there.
        exec(
            s.as_mut(),
            "CREATE FUNCTION bump() RETURNS int LANGUAGE sql AS 'INSERT INTO items VALUES (9, NULL, NULL) RETURNING 1'",
        )
        .await?;
        let q = "SELECT bump(), cosine_distance(embedding, '[1,0,0]') FROM items";
        check!(
            crate::mcp::reads::approval_free(
                dbine_driver::Language::Sql,
                "postgres",
                "postgres",
                &dbine_driver::ScriptDialect::postgres(),
                q,
            ) == Err("bump".into()),
            "a user's function must ask"
        );
        let mut out = QueryOutcome::default();
        let written = s.run_read_only(q, 10, &mut out).await;
        check!(
            written.is_err() || out.error.is_some(),
            "a write ran in the read-only transaction"
        );
        let n = first(exec(s.as_mut(), "SELECT count(*) FROM items").await?);
        check!(text(&n.rows[0][0]) == "3", "{:?}", n.rows);
        // pgvector's names are an extension's: the server says each one a
        // query calls is the extension's before it skips the approval (on
        // the read-only session, as the MCP read does), and a user's
        // function of the same name in another schema makes it ask.
        let q = "SELECT id FROM items ORDER BY cosine_distance(embedding, '[1,0,0]'), \
                 l1_distance(embedding, '[1,0,0]') LIMIT 2";
        let names = crate::mcp::reads::extension_calls(
            "postgres",
            &dbine_driver::ScriptDialect::postgres(),
            q,
        );
        check!(
            names == ["cosine_distance", "l1_distance"],
            "{names:?}"
        );
        let sql = crate::mcp::reads::extension_check_sql("vector", &names);
        let n = count_read_only(s.as_mut(), &sql).await?;
        check!(n == "0", "pgvector's own functions: {n}");
        exec(
            s.as_mut(),
            "CREATE SCHEMA mine; \
             CREATE FUNCTION mine.l1_distance(vector, vector) RETURNS float8 \
             LANGUAGE sql AS 'SELECT 0::float8'",
        )
        .await?;
        let n = count_read_only(s.as_mut(), &sql).await?;
        check!(n == "1", "a user's l1_distance must make it ask: {n}");
        drop(s);
        Ok(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// Data compare, sync scripts, data scripts, imports and transfers

use crate::commands::data_compare::{self, whole, ChangedRow, Counts, Diff, Side};
use crate::commands::vectors;
use dbine_driver::{ObjectRef, RowChange, VectorElement};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn table(schema: Option<&str>, name: &str) -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: schema.map(str::to_string),
        name: name.into(),
    }
}

/// A column type's vector on an engine: its dialect's answer, or (where
/// this branch's dialect doesn't classify vectors yet) pgvector's
/// `vector(n)` and DuckDB's `FLOAT[n]`, as their families' dialects do.
fn vector_of(driver: &str, ty: &str) -> Option<VectorElement> {
    if let Some(m) = vectors::of_type(driver, ty) {
        return (!m.sparse).then_some(m.element);
    }
    let t = ty.trim().to_ascii_lowercase();
    let pg = driver == "postgres" && t.starts_with("vector");
    let duck = driver == "duckdb" && t.starts_with("float[") && t.ends_with(']');
    (pg || duck).then_some(VectorElement::F32)
}

fn marks(driver: &str, cols: &[dbine_driver::ColumnInfo]) -> HashMap<String, VectorElement> {
    cols.iter()
        .filter_map(|c| vector_of(driver, &c.data_type).map(|e| (c.name.to_lowercase(), e)))
        .collect()
}

/// The numbers of a vector cell as float32s.
fn f32s(v: &Value) -> Option<Vec<f32>> {
    dbine_driver::vector::parse_value(v).map(|n| n.into_iter().map(|x| x as f32).collect())
}

async fn compare_emb(
    pg: &mut dyn Session,
    duck: &mut dyn Session,
    root: &std::path::Path,
    in_memory_rows: usize,
    marked: bool,
) -> Res<whole::Compared> {
    let (lo, ro) = (table(Some("public"), "emb"), table(Some("main"), "emb"));
    let cols = [
        pg.columns(&lo).await.map_err(|e| e.to_string())?,
        duck.columns(&ro).await.map_err(|e| e.to_string())?,
    ];
    let input = whole::Input {
        key: vec!["id".into()],
        wanted: Vec::new(),
        vectors: if marked {
            [marks("postgres", &cols[0]), marks("duckdb", &cols[1])]
        } else {
            Default::default()
        },
        cols,
    };
    let opts = whole::Options {
        in_memory_rows,
        in_memory_bytes: usize::MAX,
        max_diff_rows: 1_000,
        max_equal_rows: 0,
        scratch: Some(root.to_path_buf()),
        duckdb: None,
    };
    whole::compare(
        whole::Side {
            session: pg,
            object: &lo,
        },
        whole::Side {
            session: duck,
            object: &ro,
        },
        input,
        &opts,
        &Arc::new(whole::Ctl::default()),
        &|_| {},
    )
    .await
    .map_err(|e| e.to_string())
}

/// C3 and C2: a pgvector table against a DuckDB one with `FLOAT[3]`
/// (which reads its float32s widened to 64 bits): the same vectors are the
/// same, in memory and in the scratch DuckDB; the sync script into
/// PostgreSQL gets the vectors as arrays and leaves both tables alike.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-pgvector"]
async fn vector_live_data_compare_pgvector_against_duckdb_and_sync() {
    with_pgvector_db("dbine_e2e_vecapp_dcmp", |cfg| async move {
        let mut pg = open(&cfg).await?;
        exec(
            pg.as_mut(),
            "CREATE TABLE emb (id int PRIMARY KEY, v vector(3)); \
             INSERT INTO emb VALUES (1, '[0.1,0.2,0.3]'), (2, '[1e-07,0,1]'), (3, '[0,1,0]'), (4, '[0.5,0.5,0.5]'), (6, NULL)",
        )
        .await?;
        let mut duck = crate::local_duckdb::open_session()
            .await
            .map_err(|e| e.to_string())?;
        crate::local_duckdb::exec(
            duck.as_mut(),
            "CREATE TABLE emb (id INTEGER, v FLOAT[3]); \
             INSERT INTO emb VALUES (1, [0.1, 0.2, 0.3]), (2, [0.0000001, 0, 1]), (3, [0, 1, 1]), (5, [0, 0, 1]), (6, NULL)",
        )
        .await
        .map_err(|e| e.to_string())?;
        let root = std::env::temp_dir().join(format!("dbine-vec-dcmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let res = async {
            let memory = compare_emb(pg.as_mut(), duck.as_mut(), &root, 1_000_000, true).await?;
            let disk = compare_emb(pg.as_mut(), duck.as_mut(), &root, 1, true).await?;
            check!(disk.spilled && !memory.spilled, "both paths");
            // As text, `[1e-07,0,1]` isn't `[0.0000001,0,1]`.
            let text = compare_emb(pg.as_mut(), duck.as_mut(), &root, 1_000_000, false).await?;
            check!(text.counts.same < 3, "{:?}", text.counts);
            for c in [&memory, &disk] {
                check!(
                    (c.counts.same, c.counts.changed, c.counts.only_left, c.counts.only_right)
                        == (3, 1, 1, 1),
                    "{:?}",
                    c.counts
                );
                check!(c.changed[0].left[0] == json!(3), "{:?}", c.changed[0].left);
            }
            // The sync into PostgreSQL (left), as `data_compare_script`
            // writes it: the changed row updated, the right's row inserted.
            let p = &memory.projection;
            let lcols = pg.columns(&table(Some("public"), "emb")).await.map_err(|e| e.to_string())?;
            let rcols = duck.columns(&table(Some("main"), "emb")).await.map_err(|e| e.to_string())?;
            let type_of = |cols: &[dbine_driver::ColumnInfo], n: &str| {
                cols.iter().find(|c| c.name.eq_ignore_ascii_case(n)).map_or(String::new(), |c| c.data_type.clone())
            };
            let d = Diff {
                left: Side { connection_id: "l".into(), database: String::new(), object: table(Some("public"), "emb") },
                right: Side { connection_id: "r".into(), database: String::new(), object: table(Some("main"), "emb") },
                key: vec!["id".into()],
                columns: p.columns.clone(),
                left_identity: Vec::new(),
                right_identity: Vec::new(),
                only_left: memory.only_left.clone(),
                only_right: memory.only_right.clone(),
                changed: memory.changed.clone(),
                equal: Vec::new(),
                left_types: p.columns.iter().map(|c| type_of(&lcols, &c.0)).collect(),
                right_types: p.columns.iter().map(|c| type_of(&rcols, &c.1)).collect(),
                left_vectors: p.columns.iter().map(|c| vector_of("postgres", &type_of(&lcols, &c.0)).is_some()).collect(),
                right_vectors: p.columns.iter().map(|c| vector_of("duckdb", &type_of(&rcols, &c.1)).is_some()).collect(),
                partial: false,
                diff_truncated: false,
                counts: Counts::default(),
                last_keys: None,
            };
            let typed = |i: usize, v: &Value| data_compare::script_value(&d, false, i, v);
            let pgd = dbine_drivers::find("postgres").ok_or("postgres")?;
            let target = table(Some("public"), "emb");
            let ChangedRow { left, right, .. } = &d.changed[0];
            let set = typed(1, &right[1]);
            check!(set.is_array(), "the vector goes as an array: {set}");
            let update = pgd
                .update_script(
                    &target,
                    &[RowChange {
                        key: vec![("id".into(), typed(0, &left[0]))],
                        set: vec![("v".into(), set)],
                        row: vec![],
                    }],
                )
                .map_err(|e| e.to_string())?;
            exec(pg.as_mut(), &update).await?;
            let rows: Vec<Vec<Value>> = d.only_right.iter().map(|r| r.iter().enumerate().map(|(i, v)| typed(i, v)).collect()).collect();
            let insert = pgd
                .insert_script(&target, &["id".into(), "v".into()], &rows)
                .map_err(|e| e.to_string())?;
            exec(pg.as_mut(), &insert).await?;
            exec(pg.as_mut(), "DELETE FROM emb WHERE id = 4").await?;
            let after = compare_emb(pg.as_mut(), duck.as_mut(), &root, 1_000_000, true).await?;
            check!(
                (after.counts.same, after.counts.changed, after.counts.only_left, after.counts.only_right) == (5, 0, 0, 0),
                "{:?}",
                after.counts
            );
            let r = first(exec(pg.as_mut(), "SELECT v::text FROM emb WHERE id = 5").await?);
            check!(f32s(&r.rows[0][0]) == Some(vec![0.0, 0.0, 1.0]), "{:?}", r.rows);
            Ok::<(), String>(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&root);
        res
    })
    .await;
}

/// C2: the database data script, an import and generated rows into a
/// pgvector table: the data script recreates the rows exactly, a CSV's
/// vector text reaches the table, and every generated vector has the
/// column's dimension.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-pgvector"]
async fn vector_live_data_script_and_import() {
    with_pgvector_db("dbine_e2e_vecapp_script", |cfg| async move {
        let mut s = open(&cfg).await?;
        exec(
            s.as_mut(),
            "CREATE TABLE emb (id int PRIMARY KEY, v vector(3), h halfvec(2)); \
             INSERT INTO emb VALUES (1, '[0.1,-0.2,3e-7]', '[1,2]'), (2, NULL, NULL), (3, '[1,2,3]', '[0.5,0.25]')",
        )
        .await?;
        let before = first(exec(s.as_mut(), "SELECT id, v::text, h::text FROM emb ORDER BY id").await?).rows;
        drop(s);
        // The data script, as "Generar script" writes it.
        let driver = dbine_drivers::find("postgres").ok_or("postgres")?;
        let entry = Arc::new(crate::state::SessionEntry {
            connection_id: "c".into(),
            database: cfg.database.clone(),
            session: tokio::sync::Mutex::new(open(&cfg).await?),
            cancel: tokio::sync::Notify::new(),
            cancelled: std::sync::atomic::AtomicBool::new(false),
            interrupter: None,
            autocommit: std::sync::atomic::AtomicBool::new(true),
            switched_to: std::sync::Mutex::new(None),
        });
        let args = crate::commands::scripts::GenerateArgs {
            script_id: "vec".into(),
            connection_id: "c".into(),
            database: cfg.database.clone(),
            objects: vec![table(Some("public"), "emb")],
            options: crate::commands::scripts::ScriptOptions {
                drop: true,
                if_exists: true,
                create: true,
                data: true,
                ..Default::default()
            },
            path: None,
            target_driver: None,
            data_tables: None,
        };
        let script = crate::commands::scripts::generate(None, driver, &entry, &args)
            .await
            .map_err(|e| e.to_string())?
            .script
            .ok_or("no script")?;
        drop(entry);
        let mut s = open(&cfg).await?;
        exec(s.as_mut(), &script).await?;
        let again = first(exec(s.as_mut(), "SELECT id, v::text, h::text FROM emb ORDER BY id").await?).rows;
        check!(again == before, "{again:?} != {before:?}\n{script}");

        // An import from CSV: the vector text goes to the script as arrays.
        exec(s.as_mut(), "CREATE TABLE imp (id int, v vector(3))").await?;
        let dir = std::env::temp_dir().join(format!("dbine-vec-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let csv = dir.join("v.csv");
        std::fs::write(&csv, "id;v\n1;[1, 2.5, -3]\n2;[1e-07,0,0]\n3;\n").map_err(|e| e.to_string())?;
        let res = async {
            let reader = dbine_core::import::open(
                &csv,
                dbine_core::import::ImportFormat::CsvSemicolon,
                &dbine_core::import::ImportOptions::default(),
            )
            .map_err(|e| e.to_string())?;
            let rows: Vec<Vec<Value>> = reader
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|r| r.into_iter().map(|v| if v == json!("") { Value::Null } else { v }).collect())
                .collect();
            let cols = s.columns(&table(Some("public"), "imp")).await.map_err(|e| e.to_string())?;
            let at: Vec<bool> = ["id", "v"]
                .iter()
                .map(|n| cols.iter().any(|c| c.name == *n && vector_of("postgres", &c.data_type).is_some()))
                .collect();
            let rows = vectors::script_rows(&at, rows);
            check!(rows[0][1].is_array(), "{:?}", rows[0]);
            let sql = driver
                .insert_script(&table(Some("public"), "imp"), &["id".into(), "v".into()], &rows)
                .map_err(|e| e.to_string())?;
            exec(s.as_mut(), &sql).await?;
            let got = first(exec(s.as_mut(), "SELECT v::text FROM imp ORDER BY id").await?).rows;
            check!(f32s(&got[0][0]) == Some(vec![1.0, 2.5, -3.0]), "{got:?}");
            check!(f32s(&got[1][0]) == Some(vec![1e-7, 0.0, 0.0]), "{got:?}");
            check!(got[2][0].is_null(), "{got:?}");
            Ok::<(), String>(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        res?;

        // Generated rows: random vectors of the column's dimension.
        let rows: Vec<Vec<Value>> = (0..20)
            .map(|i| {
                vec![
                    json!(100 + i),
                    crate::commands::datagen::random_vector(
                        &mut crate::commands::datagen::Rng::new(Some(i)),
                        3,
                        VectorElement::F32,
                    ),
                    Value::Null,
                ]
            })
            .collect();
        let sql = driver
            .insert_script(&table(Some("public"), "emb"), &["id".into(), "v".into(), "h".into()], &rows)
            .map_err(|e| e.to_string())?;
        exec(s.as_mut(), &sql).await?;
        let n = first(
            exec(
                s.as_mut(),
                "SELECT count(*) FROM emb WHERE id >= 100 AND vector_dims(v) = 3 AND abs(vector_norm(v) - 1) < 1e-5",
            )
            .await?,
        );
        check!(text(&n.rows[0][0]) == "20", "{:?}", n.rows);
        drop(s);
        Ok(())
    })
    .await;
}

/// The two ends of a transfer test.
struct Ends {
    source: dbine_driver::ConnectionConfig,
    target: dbine_driver::ConnectionConfig,
}

#[dbine_driver::async_trait]
impl dbine_transfer::Endpoints for Ends {
    fn source_driver(&self) -> Arc<dyn dbine_driver::Driver> {
        dbine_drivers::find(&self.source.driver).unwrap().clone()
    }
    fn target_driver(&self) -> Arc<dyn dbine_driver::Driver> {
        dbine_drivers::find(&self.target.driver).unwrap().clone()
    }
    async fn open_source(&self) -> dbine_driver::Result<Box<dyn Session>> {
        dbine_drivers::open_session(&self.source, None).await
    }
    async fn open_target(&self) -> dbine_driver::Result<Box<dyn Session>> {
        dbine_drivers::open_session(&self.target, None).await
    }
    fn native_copy_allowed(&self) -> bool {
        false
    }
}

fn transfer_job(
    source: ObjectRef,
    target: ObjectRef,
    columns: &[&str],
) -> dbine_transfer::TransferJob {
    let columns: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
    dbine_transfer::TransferJob {
        name: format!("{} -> {}", source.name, target.name),
        source: dbine_driver::ReadSpec {
            table: source,
            columns: Some(columns.clone()),
            filter: None,
        },
        target: dbine_driver::LoadSpec {
            table: target,
            columns,
            table_lock: false,
            keep_identity: true,
            commit_rows: dbine_driver::LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: dbine_driver::LoadSpec::DEFAULT_COMMIT_BYTES,
        },
        row_estimate: None,
        truncate: None,
        empty_first: false,
        before: String::new(),
        after: String::new(),
        post: Vec::new(),
        preexisting: false,
        expected_columns: Vec::new(),
        vector_columns: vec!["v".into()],
        mode: dbine_transfer::TransferMode::Copy,
    }
}

async fn transfer(ends: Ends, job: dbine_transfer::TransferJob, tag: &str) -> Res<()> {
    let path = std::env::temp_dir().join(format!(
        "dbine-vec-transfer-{tag}-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let res = async {
        let store = Arc::new(dbine_transfer::Store::open(&path).map_err(|e| e.to_string())?);
        let report = dbine_transfer::Engine::new(store, tag)
            .run(
                vec![job],
                dbine_transfer::RunOptions::default(),
                Arc::new(ends),
                |_| {},
            )
            .await
            .map_err(|e| e.to_string())?;
        check!(
            report.summary.status == dbine_transfer::RunStatus::Finished
                && report.summary.failed == 0,
            "{:?}",
            report.summary
        );
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&path);
    res
}

/// C5: transfers keep vectors whole: pgvector to pgvector, and a DuckDB
/// file's `FLOAT[3]` into a pgvector `vector(3)` (another family).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-pgvector"]
async fn vector_live_transfers_keep_vectors() {
    with_pgvector_db("dbine_e2e_vecapp_transfer", |cfg| async move {
        let mut s = open(&cfg).await?;
        exec(
            s.as_mut(),
            "CREATE TABLE src (id int PRIMARY KEY, v vector(3)); \
             CREATE TABLE dst (id int PRIMARY KEY, v vector(3)); \
             CREATE TABLE from_duck (id int PRIMARY KEY, v vector(3)); \
             INSERT INTO src SELECT i, ('[' || i || ',0.25,-1e-07]')::vector FROM generate_series(1, 2500) i; \
             INSERT INTO src VALUES (0, NULL)",
        )
        .await?;
        transfer(
            Ends { source: cfg.clone(), target: cfg.clone() },
            transfer_job(table(Some("public"), "src"), table(Some("public"), "dst"), &["id", "v"]),
            "pg",
        )
        .await?;
        let same = first(exec(s.as_mut(), "SELECT count(*) FROM src s JOIN dst d USING (id) WHERE s.v IS NOT DISTINCT FROM d.v").await?);
        check!(text(&same.rows[0][0]) == "2501", "{:?}", same.rows);

        // DuckDB (a file of the test's own) into pgvector.
        let dir = std::env::temp_dir().join(format!("dbine-vec-duck-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = dir.join("v.duckdb");
        let duck_cfg = dbine_driver::ConnectionConfig {
            driver: "duckdb".into(),
            host: file.display().to_string(),
            ..Default::default()
        };
        let res = async {
            let mut d = dbine_drivers::open_session(&duck_cfg, None).await.map_err(|e| e.to_string())?;
            exec(
                d.as_mut(),
                "CREATE TABLE v (id INTEGER, v FLOAT[3]); \
                 INSERT INTO v SELECT i, [i::FLOAT / 10, 0.1, -0.000001]::FLOAT[3] FROM range(1, 1001) r(i)",
            )
            .await?;
            drop(d);
            transfer(
                Ends { source: duck_cfg.clone(), target: cfg.clone() },
                transfer_job(table(Some("main"), "v"), table(Some("public"), "from_duck"), &["id", "v"]),
                "duck",
            )
            .await?;
            let got = first(exec(s.as_mut(), "SELECT id, v::text FROM from_duck ORDER BY id").await?);
            check!(got.rows.len() == 1000, "{}", got.rows.len());
            for r in &got.rows {
                let i: f32 = text(&r[0]).parse().map_err(|_| "id")?;
                check!(f32s(&r[1]) == Some(vec![i / 10.0, 0.1, -0.000001]), "{r:?}");
            }
            Ok::<(), String>(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        res?;
        drop(s);
        Ok(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// Server-enforced vector reads on MySQL 9 and MariaDB

fn mysql_cfg(driver: &str, env: &str, default: &str, db: &str) -> dbine_driver::ConnectionConfig {
    let url = std::env::var(env).unwrap_or_else(|_| default.into());
    let mut c = pgvector(db);
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "3306"));
    c.driver = driver.into();
    c.host = host.into();
    c.port = port.parse().unwrap_or(3306);
    c.username = Some(user.into());
    c.password = Some(pass.into());
    c
}

/// C1 on the MySQL family: MySQL 9's and MariaDB's vector functions read
/// without approval, in the server's read-only transaction.
async fn mysql_family_vector_reads(
    driver: &str,
    env: &str,
    default: &str,
    setup: &str,
    queries: &[&str],
) -> Res<()> {
    let db = "dbine_e2e_vecapp_my";
    let mut s = open(&mysql_cfg(driver, env, default, "")).await?;
    exec(s.as_mut(), &format!("DROP DATABASE IF EXISTS {db}")).await?;
    exec(s.as_mut(), &format!("CREATE DATABASE {db}")).await?;
    let res = async {
        let mut t = open(&mysql_cfg(driver, env, default, db)).await?;
        exec(t.as_mut(), setup).await?;
        let d = dbine_drivers::find(driver).ok_or("driver")?;
        for q in queries {
            crate::mcp::reads::approval_free(
                dbine_driver::Language::Sql,
                driver,
                "mysql",
                &d.script_dialect(),
                q,
            )
            .map_err(|f| format!("asks for approval because of {f}: {q}"))?;
            let mut out = QueryOutcome::default();
            t.run_read_only(q, 100, &mut out)
                .await
                .map_err(|e| format!("{e}\n--- {q}"))?;
            if let Some(e) = out.error {
                return Err(format!("{e}\n--- {q}"));
            }
            let r = first(out);
            check!(text(&r.rows[0][0]) == "1", "{q}: {:?}", r.rows);
        }
        drop(t);
        Ok::<(), String>(())
    }
    .await;
    let cleaned = exec(s.as_mut(), &format!("DROP DATABASE IF EXISTS {db}")).await;
    res?;
    cleaned.map(|_| ())
}

#[tokio::test]
#[ignore = "needs dbine-test-mysql9"]
async fn vector_live_mysql9_vector_functions_read_without_approval() {
    mysql_family_vector_reads(
        "mysql",
        "DBINE_TEST_MYSQL9_URL",
        "mysql://root:pw@127.0.0.1:27201",
        "CREATE TABLE t (id int PRIMARY KEY, v VECTOR(3)); \
         INSERT INTO t VALUES (1, STRING_TO_VECTOR('[1,0,0]')), (2, TO_VECTOR('[0,1,0]'))",
        &[
            "SELECT id, VECTOR_TO_STRING(v), VECTOR_DIM(v), FROM_VECTOR(v) FROM t ORDER BY id",
            "SELECT id FROM t WHERE VECTOR_TO_STRING(v) = VECTOR_TO_STRING(STRING_TO_VECTOR('[1,0,0]'))",
        ],
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs dbine-test-mariadb (11.7 or later)"]
async fn vector_live_mariadb_vector_functions_read_without_approval() {
    let cfg = mysql_cfg(
        "mariadb",
        "DBINE_TEST_MARIADB_URL",
        "mysql://root:pw@127.0.0.1:25012",
        "",
    );
    let mut s = open(&cfg).await.unwrap();
    let v = first(exec(s.as_mut(), "SELECT VERSION()").await.unwrap());
    drop(s);
    let version = text(&v.rows[0][0]);
    let mut parts = version
        .split(['.', '-'])
        .map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if (major, minor) < (11, 7) {
        eprintln!("MariaDB {version} has no vectors (11.7 or later): not tried");
        return;
    }
    mysql_family_vector_reads(
        "mariadb",
        "DBINE_TEST_MARIADB_URL",
        "mysql://root:pw@127.0.0.1:25012",
        "CREATE TABLE t (id int PRIMARY KEY, v VECTOR(3) NOT NULL); \
         INSERT INTO t VALUES (1, VEC_FromText('[1,0,0]')), (2, VEC_FromText('[0,1,0]'))",
        &[
            "SELECT id FROM t ORDER BY VEC_DISTANCE_COSINE(v, VEC_FromText('[1,0.1,0]')) LIMIT 1",
            "SELECT id, VEC_ToText(v) FROM t ORDER BY VEC_DISTANCE_EUCLIDEAN(v, VEC_FromText('[1,0,0]')) LIMIT 1",
        ],
    )
    .await
    .unwrap();
}
