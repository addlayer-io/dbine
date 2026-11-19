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
use dbine_driver::{ObjectRef, RowChange, VectorElement, VectorMeta};
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
pub(crate) fn f32s(v: &Value) -> Option<Vec<f32>> {
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
            // As text, `[1e-07,0,1]` isn't `[0.0000001,0,1]`: by value is
            // never less equal than as text. (Where both drivers already
            // send the canonical text, as the merged build's do, the two
            // agree.)
            let text = compare_emb(pg.as_mut(), duck.as_mut(), &root, 1_000_000, false).await?;
            check!(text.counts.same <= 3, "{:?}", text.counts);
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
                left_vectors: p.columns.iter().map(|c| vector_of("postgres", &type_of(&lcols, &c.0)).map(|e| VectorMeta::dense(None, e))).collect(),
                right_vectors: p.columns.iter().map(|c| vector_of("duckdb", &type_of(&rcols, &c.1)).map(|e| VectorMeta::dense(None, e))).collect(),
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
    fn column_types(
        &self,
        columns: &[dbine_driver::TransferColumn],
    ) -> Vec<dbine_driver::ResultColumn> {
        crate::commands::typed_scripts::of_describe(
            &self.target.driver,
            columns
                .iter()
                .map(|c| (c.name.as_str(), c.type_name.as_str())),
        )
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

// ---------------------------------------------------------------------------
// The app's own commands through a saved connection, on pgvector, MySQL 9
// and MariaDB: test data, an import, the data script, a data compare with
// its sync script. Every path learns which columns are vectors from the
// saved connection's driver (its dialect), and MySQL and MariaDB reject a
// vector written as plain text: these pass only where each path sends the
// vectors as the engine's literal.

/// One engine of [`app_paths`].
#[derive(Clone, Copy)]
struct Engine {
    driver: &'static str,
    cfg: fn(&str) -> ConnectionConfig,
    schema: Option<&'static str>,
    /// The vector column's type, and its value as text in a query.
    vector_type: &'static str,
    as_text: &'static str,
    text_type: &'static str,
}

pub(crate) fn mysql9(db: &str) -> ConnectionConfig {
    mysql_cfg(
        "mysql",
        "DBINE_TEST_MYSQL9_URL",
        "mysql://root:pw@127.0.0.1:27201",
        db,
    )
}

pub(crate) fn mariadb(db: &str) -> ConnectionConfig {
    mysql_cfg(
        "mariadb",
        "DBINE_TEST_MARIADB_URL",
        "mysql://root:pw@127.0.0.1:25012",
        db,
    )
}

/// A state of its own (its folder goes when it's dropped) with the
/// connection `c` saved for `cfg`.
struct TestState {
    state: crate::state::AppState,
    dir: std::path::PathBuf,
}

impl Drop for TestState {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn test_state(tag: &str, cfg: &ConnectionConfig) -> Res<TestState> {
    let dir = std::env::temp_dir().join(format!(
        "dbine-vecapp-{tag}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let t = TestState {
        state: crate::state::AppState::new(
            dbine_core::StateStore::open(&dir.join("state.sqlite")).map_err(|e| e.to_string())?,
        ),
        dir,
    };
    let conn = dbine_core::SavedConnection {
        id: "c".into(),
        name: "c".into(),
        color: None,
        config: cfg.clone(),
        save_password: false,
        folder_id: None,
        tags: Vec::new(),
        mcp_level: None,
        updated_at: String::new(),
    };
    t.state
        .store
        .save_connection(&conn)
        .map_err(|e| e.to_string())?;
    t.state.typed_secrets.insert(
        "c".into(),
        [(
            "password".to_string(),
            cfg.password.clone().unwrap_or_default(),
        )]
        .into_iter()
        .collect(),
    );
    Ok(t)
}

/// The vectors in `table`'s `v`, by `id`, as float32s.
async fn vectors_by_id(
    e: &Engine,
    s: &mut dyn Session,
    table: &str,
) -> Res<Vec<(i64, Option<Vec<f32>>)>> {
    let r = first(
        exec(
            s,
            &format!("SELECT id, {} FROM {table} ORDER BY id", e.as_text),
        )
        .await?,
    );
    Ok(r.rows
        .iter()
        .map(|row| {
            let id = text(&row[0]).parse().unwrap_or(-1);
            (id, f32s(&row[1]))
        })
        .collect())
}

/// Run `body` on a fresh database `name` of engine `e` (pgvector's with
/// the extension), dropped before and after, also when `body` fails or
/// panics.
async fn with_engine_db<F, Fut>(e: &Engine, name: &str, body: F)
where
    F: FnOnce(ConnectionConfig) -> Fut,
    Fut: std::future::Future<Output = Res<()>>,
{
    if e.driver == "postgres" {
        return with_pgvector_db(name, body).await;
    }
    let admin = (e.cfg)("");
    let run = |sql: String| {
        let admin = admin.clone();
        async move {
            let mut s = open(&admin).await?;
            exec(s.as_mut(), &sql).await.map(|_| ())
        }
    };
    run(format!("DROP DATABASE IF EXISTS {name}"))
        .await
        .expect("clean up first");
    let res = match run(format!("CREATE DATABASE {name}")).await {
        Ok(()) => AssertUnwindSafe(body((e.cfg)(name)))
            .catch_unwind()
            .await
            .unwrap_or_else(|p| {
                Err(p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "panicked".into()))
            }),
        Err(err) => Err(err),
    };
    let cleaned = run(format!("DROP DATABASE IF EXISTS {name}")).await;
    res.unwrap();
    cleaned.expect("clean up at the end");
}

/// C2, C3, C4 through the app's commands on engine `e`.
async fn app_paths(e: Engine) {
    let db = format!("dbine_e2e_vecapp_paths_{}", e.driver);
    with_engine_db(&e, &db.clone(), |cfg| async move {
        // The saved connection's dialect says `v` is a vector: without it
        // (a build whose driver family doesn't classify vectors) every path
        // below would write text.
        check!(
            vectors::of_type(e.driver, e.vector_type).is_some_and(|m| !m.sparse),
            "{}'s dialect doesn't take {} as a vector in this build",
            e.driver,
            e.vector_type
        );
        let t = test_state(e.driver, &cfg)?;
        let state = &t.state;
        let tbl = |name: &str| table(e.schema, name);
        let mut s = open(&cfg).await?;
        exec(
            s.as_mut(),
            &format!("CREATE TABLE emb (id int PRIMARY KEY, v {})", e.vector_type),
        )
        .await?;
        exec(
            s.as_mut(),
            &format!("CREATE TABLE emb_t (id int PRIMARY KEY, v {})", e.text_type),
        )
        .await?;

        // Test data: `auto` gives `v` random vectors of its dimension.
        let generated = crate::commands::datagen::run_generation(
            state,
            crate::commands::datagen::DataGenArgs {
                connection_id: "c".into(),
                database: cfg.database.clone(),
                table: tbl("emb"),
                columns: Vec::new(),
                rows: 20,
                seed: Some(7),
                gen_id: "vec".into(),
                batch: 8,
            },
            |_| {},
        )
        .await
        .map_err(|err| format!("test data: {err}"))?;
        check!(generated.rows == 20, "{} rows", generated.rows);
        let got = vectors_by_id(&e, s.as_mut(), "emb").await?;
        check!(got.len() == 20, "{got:?}");
        for (id, v) in &got {
            let v = v.as_ref().ok_or(format!("row {id}: no vector"))?;
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            check!(v.len() == 3 && (norm - 1.0).abs() < 1e-4, "row {id}: {v:?}");
        }
        exec(s.as_mut(), "DELETE FROM emb").await?;

        // An import from CSV: the file's vector text reaches the table.
        let dir = std::env::temp_dir().join(format!(
            "dbine-vecapp-import-{}-{}",
            e.driver,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        let csv = dir.join("v.csv");
        let imported = async {
            std::fs::write(&csv, "id;v\n1;[0.5, -0.25, 1]\n2;[1,2,3]\n4;[1e-07,0,0]\n")
                .map_err(|err| err.to_string())?;
            let map = |n: &str| crate::commands::imports::Mapping {
                source: n.into(),
                target: n.into(),
            };
            crate::commands::imports::import_with(
                state,
                crate::commands::imports::ImportArgs {
                    import_id: "vec".into(),
                    connection_id: "c".into(),
                    database: cfg.database.clone(),
                    path: csv.display().to_string(),
                    format: dbine_core::import::ImportFormat::CsvSemicolon,
                    options: dbine_core::import::ImportOptions::default(),
                    target: tbl("emb"),
                    create_table: None,
                    mapping: vec![map("id"), map("v")],
                    batch: 2,
                },
                Arc::new(|_| {}),
            )
            .await
            .map_err(|err| format!("import: {err}"))
        }
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        check!(imported?.rows == 3, "imported");
        let want = vec![
            (1, Some(vec![0.5, -0.25, 1.0])),
            (2, Some(vec![1.0, 2.0, 3.0])),
            (4, Some(vec![1e-7, 0.0, 0.0])),
        ];
        let got = vectors_by_id(&e, s.as_mut(), "emb").await?;
        check!(got == want, "{got:?}");

        // The database data script recreates the rows exactly.
        let driver = dbine_drivers::find(e.driver).ok_or("driver")?;
        let entry = state
            .dedicated_session("vecapp-script", "c", &cfg.database, false)
            .await
            .map_err(|err| err.to_string())?;
        let script = crate::commands::scripts::generate(
            None,
            driver,
            &entry,
            &crate::commands::scripts::GenerateArgs {
                script_id: "vec".into(),
                connection_id: "c".into(),
                database: cfg.database.clone(),
                objects: vec![tbl("emb")],
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
            },
        )
        .await
        .map_err(|err| format!("data script: {err}"))?
        .script
        .ok_or("no script")?;
        state.sessions.remove("vecapp-script");
        drop(entry);
        exec(s.as_mut(), &script).await?;
        let again = vectors_by_id(&e, s.as_mut(), "emb").await?;
        check!(again == want, "{again:?}\n{script}");

        // A data compare against the vectors as text: equal by value,
        // whatever their spelling, and its sync script writes the
        // vector side.
        exec(
            s.as_mut(),
            "INSERT INTO emb_t VALUES (1, '[0.5, -0.25, 1.0]'), (2, '[1,2,4]'), \
             (3, '[3,2,1]'), (4, '[0.0000001, 0, 0]')",
        )
        .await?;
        let side = |name: &str| crate::commands::data_compare::Side {
            connection_id: "c".into(),
            database: cfg.database.clone(),
            object: tbl(name),
        };
        let compare = || async {
            let opts = whole::Options {
                in_memory_rows: 1_000,
                in_memory_bytes: 1 << 20,
                max_diff_rows: 1_000,
                max_equal_rows: 0,
                scratch: None,
                duckdb: None,
            };
            data_compare::compare_sides(
                state,
                data_compare::DataCompareArgs {
                    left: side("emb"),
                    right: side("emb_t"),
                    key: Vec::new(),
                    columns: Vec::new(),
                    run: String::new(),
                    equal: false,
                },
                "vecapp",
                &Arc::new(whole::Ctl::default()),
                &opts,
                &|_| {},
            )
            .await
            .map_err(|err| format!("data compare: {err}"))
        };
        let r = compare().await?;
        check!(
            (r.counts.same, r.counts.changed, r.counts.only_right) == (2, 1, 1),
            "{:?}",
            r.counts
        );
        let scripts = data_compare::scripts_for(
            state,
            &data_compare::DataScriptArgs {
                id: r.id.clone(),
                choices: data_compare::Choices {
                    changed: data_compare::Pick {
                        all: data_compare::Dir::Left,
                        rows: Vec::new(),
                    },
                    only_right: data_compare::Pick {
                        all: data_compare::Dir::Left,
                        rows: Vec::new(),
                    },
                    only_left: Default::default(),
                },
            },
        )
        .map_err(|err| format!("sync script: {err}"))?;
        check!(scripts.len() == 1, "{} scripts", scripts.len());
        exec(s.as_mut(), &scripts[0].script).await?;
        let r = compare().await?;
        check!(
            (r.counts.same, r.counts.changed, r.counts.only_right) == (4, 0, 0),
            "after the sync: {:?}\n{}",
            r.counts,
            scripts[0].script
        );
        let got = vectors_by_id(&e, s.as_mut(), "emb").await?;
        check!(got[1] == (2, Some(vec![1.0, 2.0, 4.0])), "{got:?}");
        check!(got[2] == (3, Some(vec![3.0, 2.0, 1.0])), "{got:?}");
        drop(s);
        Ok(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// Typed scripts (round 2): the grid's copy and save send the result's
// columns with their types, as the web does, and a data compare's sync
// types its target's columns from the describe. pgvector's `halfvec`,
// `sparsevec` and `vector` columns get their values exactly.

/// The rows of `table` with every vector column as its text.
async fn typed_rows(s: &mut dyn Session, table: &str) -> Res<Vec<Vec<String>>> {
    let r = first(
        exec(
            s,
            &format!("SELECT id, h::text, sp::text, v::text FROM {table} ORDER BY id"),
        )
        .await?,
    );
    Ok(r.rows
        .iter()
        .map(|row| row.iter().map(text).collect())
        .collect())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-pgvector"]
async fn vector_live_typed_scripts_pgvector() {
    use crate::commands::schema::{
        insert_script_with, update_script_with, InsertScriptArgs, UpdateScriptArgs,
    };
    with_pgvector_db("dbine_e2e_vecapp_typed", |cfg| async move {
        let t = test_state("typed", &cfg)?;
        let state = &t.state;
        let mut s = open(&cfg).await?;
        for name in ["typed", "typed2"] {
            exec(
                s.as_mut(),
                &format!(
                    "CREATE TABLE {name} (id int PRIMARY KEY, h halfvec(3), sp sparsevec(5), v vector(3))"
                ),
            )
            .await?;
        }
        // The result's columns as the grid has them: the driver's marks.
        let cols = first(exec(s.as_mut(), "SELECT * FROM typed").await?).columns;
        let marked: Vec<(&str, bool)> = cols
            .iter()
            .map(|c| (c.name.as_str(), c.vector.as_ref().is_some_and(|m| !m.sparse)))
            .collect();
        check!(
            marked == [("id", false), ("h", true), ("sp", false), ("v", true)],
            "{cols:?}"
        );
        let types = serde_json::to_value(&cols).map_err(|e| e.to_string())?;
        let target = json!({ "kind": "table", "schema": "public", "name": "typed" });

        // "Copiar como SQL INSERTs", as ResultsPane sends it: dense vectors
        // as arrays, the sparse one as its text, and the columns' types.
        let args: InsertScriptArgs = serde_json::from_value(json!({
            "connection_id": "c",
            "target": target,
            "columns": ["id", "h", "sp", "v"],
            "rows": [
                [1, [0.5, -1, 2], "{1:0.5,3:2}/5", [0.1, 0.2, 0.3]],
                [2, null, null, [1, 2, 3]],
            ],
            "types": types,
        }))
        .map_err(|e| e.to_string())?;
        let sql = insert_script_with(state, &args).map_err(|e| e.to_string())?;
        exec(s.as_mut(), &sql).await?;
        let got = typed_rows(s.as_mut(), "typed").await?;
        check!(
            got == [
                ["1", "[0.5,-1,2]", "{1:0.5,3:2}/5", "[0.1,0.2,0.3]"],
                ["2", "null", "null", "[1,2,3]"],
            ],
            "{got:?}\n{sql}"
        );

        // "Guardar" in the grid: an edit, a deleted row and a new row in one
        // script, typed by the result's columns.
        let args: UpdateScriptArgs = serde_json::from_value(json!({
            "connection_id": "c",
            "target": target,
            "changes": [{
                "key": [["id", 1]],
                "set": [["h", [1, 2, 3]], ["v", [0.25, 0.5, 1]]],
                "row": [],
            }],
            "deletes": [[["id", 2]]],
            "inserts": [[["id", 3], ["h", [3, 2, 1]], ["sp", "{2:1}/5"], ["v", [3, 2, 1]]]],
            "identity": [],
            "types": types,
        }))
        .map_err(|e| e.to_string())?;
        let sql = update_script_with(state, &args).map_err(|e| e.to_string())?;
        exec(s.as_mut(), &sql).await?;
        let got = typed_rows(s.as_mut(), "typed").await?;
        let want = [
            ["1", "[1,2,3]", "{1:0.5,3:2}/5", "[0.25,0.5,1]"],
            ["3", "[3,2,1]", "{2:1}/5", "[3,2,1]"],
        ];
        check!(got == want, "{got:?}\n{sql}");

        // A data compare into another table: its sync script is typed by
        // the target's describe, and the screen gets each column's vectors.
        exec(
            s.as_mut(),
            "INSERT INTO typed2 VALUES (1, '[1,2,3]', '{1:0.5,3:2}/5', '[0.25,0.5,1]'), \
             (3, '[3,2,0]', '{2:1}/5', '[3,2,1]'), (4, '[1,1,1]', NULL, '[4,4,4]')",
        )
        .await?;
        let side = |name: &str| data_compare::Side {
            connection_id: "c".into(),
            database: cfg.database.clone(),
            object: table(Some("public"), name),
        };
        let compare = || async {
            let opts = whole::Options {
                in_memory_rows: 1_000,
                in_memory_bytes: 1 << 20,
                max_diff_rows: 1_000,
                max_equal_rows: 0,
                scratch: None,
                duckdb: None,
            };
            data_compare::compare_sides(
                state,
                data_compare::DataCompareArgs {
                    left: side("typed"),
                    right: side("typed2"),
                    key: Vec::new(),
                    columns: Vec::new(),
                    run: String::new(),
                    equal: false,
                },
                "vecapp-typed",
                &Arc::new(whole::Ctl::default()),
                &opts,
                &|_| {},
            )
            .await
            .map_err(|err| format!("data compare: {err}"))
        };
        let r = compare().await?;
        check!(
            (r.counts.same, r.counts.changed, r.counts.only_right) == (1, 1, 1),
            "{:?}",
            r.counts
        );
        let shown: Vec<Option<(Option<u32>, bool)>> = r
            .vectors
            .iter()
            .map(|m| m.as_ref().map(|m| (m.dims, m.sparse)))
            .collect();
        check!(
            shown == [None, Some((Some(3), false)), Some((Some(5), true)), Some((Some(3), false))],
            "{shown:?}"
        );
        let to_right = || data_compare::Pick {
            all: data_compare::Dir::Right,
            rows: Vec::new(),
        };
        let scripts = data_compare::scripts_for(
            state,
            &data_compare::DataScriptArgs {
                id: r.id.clone(),
                choices: data_compare::Choices {
                    changed: to_right(),
                    only_right: to_right(),
                    only_left: Default::default(),
                },
            },
        )
        .map_err(|err| format!("sync script: {err}"))?;
        check!(scripts.len() == 1, "{} scripts", scripts.len());
        exec(s.as_mut(), &scripts[0].script).await?;
        let r = compare().await?;
        check!(
            (r.counts.same, r.counts.changed, r.counts.only_right) == (2, 0, 0),
            "after the sync: {:?}\n{}",
            r.counts,
            scripts[0].script
        );
        let got = typed_rows(s.as_mut(), "typed2").await?;
        check!(got == want, "{got:?}");
        drop(s);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-pgvector"]
async fn vector_live_app_paths_pgvector() {
    app_paths(Engine {
        driver: "postgres",
        cfg: pgvector,
        schema: Some("public"),
        vector_type: "vector(3)",
        as_text: "v::text",
        text_type: "text",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-mysql9"]
async fn vector_live_app_paths_mysql9() {
    app_paths(Engine {
        driver: "mysql",
        cfg: mysql9,
        schema: None,
        vector_type: "VECTOR(3)",
        as_text: "VECTOR_TO_STRING(v)",
        text_type: "TEXT",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-mariadb (11.7 or later)"]
async fn vector_live_app_paths_mariadb() {
    app_paths(Engine {
        driver: "mariadb",
        cfg: mariadb,
        schema: None,
        vector_type: "VECTOR(3)",
        as_text: "VEC_ToText(v)",
        text_type: "TEXT",
    })
    .await;
}

// ---------------------------------------------------------------------------
// A clone on an engine without a bulk load

/// The clone's database, its vector columns told as the app tells them.
struct CloneEnds(ConnectionConfig);

#[dbine_driver::async_trait]
impl dbine_transfer::Endpoints for CloneEnds {
    fn source_driver(&self) -> Arc<dyn dbine_driver::Driver> {
        dbine_drivers::find(&self.0.driver).unwrap().clone()
    }
    fn target_driver(&self) -> Arc<dyn dbine_driver::Driver> {
        dbine_drivers::find(&self.0.driver).unwrap().clone()
    }
    async fn open_source(&self) -> dbine_driver::Result<Box<dyn Session>> {
        dbine_drivers::open_session(&self.0, None).await
    }
    async fn open_target(&self) -> dbine_driver::Result<Box<dyn Session>> {
        dbine_drivers::open_session(&self.0, None).await
    }
    fn vector_columns(&self, table: &dbine_driver::TableSchema) -> Vec<String> {
        crate::commands::clone_table::vector_columns(table, |t| vectors::of_type(&self.0.driver, t))
    }
    fn column_types(
        &self,
        columns: &[dbine_driver::TransferColumn],
    ) -> Vec<dbine_driver::ResultColumn> {
        crate::commands::typed_scripts::of_describe(
            &self.0.driver,
            columns
                .iter()
                .map(|c| (c.name.as_str(), c.type_name.as_str())),
        )
    }
}

/// C5: "Clonar tabla" of a CrateDB table with a `FLOAT_VECTOR` (CrateDB has
/// no bulk load and rejects a vector written as text): the clone's rows go
/// through the insert script with the vectors as arrays, and arrive whole.
/// `dbine-test-cratedb` (`DBINE_TEST_CRATEDB_URL`, default
/// `postgres://crate@127.0.0.1:25021/doc`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs dbine-test-cratedb"]
async fn vector_live_clone_cratedb_sends_vectors_as_arrays() {
    let url = std::env::var("DBINE_TEST_CRATEDB_URL")
        .unwrap_or_else(|_| "postgres://crate@127.0.0.1:25021/doc".into());
    let mut cfg = pgvector("doc");
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("crate", rest));
    let hostport = hostport.split('/').next().unwrap_or(hostport);
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "5432"));
    cfg.driver = "cratedb".into();
    cfg.host = host.into();
    cfg.port = port.parse().unwrap_or(5432);
    cfg.username = Some(auth.split(':').next().unwrap_or("crate").into());
    cfg.password = None;
    let clean = || async {
        let mut s = open(&cfg).await?;
        for t in ["vecapp_items", "vecapp_items_c"] {
            exec(s.as_mut(), &format!("DROP TABLE IF EXISTS doc.{t}")).await?;
        }
        Ok::<(), String>(())
    };
    clean().await.expect("clean up first");
    let body = AssertUnwindSafe(async {
        check!(
            vectors::of_type("cratedb", "float_vector(3)").is_some(),
            "CrateDB's dialect doesn't take float_vector(3) as a vector in this build"
        );
        let mut s = open(&cfg).await?;
        exec(
            s.as_mut(),
            "CREATE TABLE doc.vecapp_items (id int PRIMARY KEY, v FLOAT_VECTOR(3))",
        )
        .await?;
        exec(
            s.as_mut(),
            "INSERT INTO doc.vecapp_items (id, v) VALUES (1, [0.5, -0.25, 1]), (2, [1, 2, 3]), (3, NULL)",
        )
        .await?;
        exec(s.as_mut(), "REFRESH TABLE doc.vecapp_items").await?;
        let read = |t: &str| format!("SELECT id, v FROM doc.{t} ORDER BY id");
        let want = first(exec(s.as_mut(), &read("vecapp_items")).await?).rows;
        let report = dbine_transfer::clone_table::clone_table(
            Arc::new(CloneEnds(cfg.clone())),
            dbine_transfer::clone_table::CloneRequest {
                source: table(Some("doc"), "vecapp_items"),
                new_name: "vecapp_items_c".into(),
                options: Default::default(),
            },
            &dbine_transfer::clone_table::CloneControl::default(),
            |_| {},
        )
        .await
        .map_err(|e| format!("clone: {e}"))?;
        check!(report.rows == 3, "{} rows", report.rows);
        exec(s.as_mut(), "REFRESH TABLE doc.vecapp_items_c").await?;
        let got = first(exec(s.as_mut(), &read("vecapp_items_c")).await?).rows;
        let as_f32 = |rows: &[Vec<Value>]| -> Vec<Option<Vec<f32>>> {
            rows.iter().map(|r| f32s(&r[1])).collect()
        };
        check!(as_f32(&got) == as_f32(&want), "{got:?} != {want:?}");
        check!(
            as_f32(&got)[1] == Some(vec![1.0, 2.0, 3.0]),
            "{got:?}"
        );
        Ok::<(), String>(())
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|p| {
        Err(p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "panicked".into()))
    });
    let cleaned = clean().await;
    body.unwrap();
    cleaned.expect("clean up at the end");
}
