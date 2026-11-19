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
        drop(s);
        Ok(())
    })
    .await;
}
