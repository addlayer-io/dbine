//! "Buscar en la base" from the catalog against real servers, checked
//! against the app's per-object scan (list_objects + definition + the same
//! line matching). Each test reads `DBINE_TEST_<ENGINE>_URL` and is skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//!   cargo test -p dbine-driver-postgres --test search -- --ignored --test-threads=1
//! ```

use dbine_driver::search::{hits_in, CodeHit, CodeSearch};
use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use std::sync::Arc;

const DB: &str = "dbine_search";

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

/// The app's scan, as commands/search.rs does it.
async fn scan(s: &mut Box<dyn Session>, d: &dyn Driver, q: &CodeSearch) -> Vec<CodeHit> {
    let with_source: Vec<&str> = d.info().object_kinds.iter().filter(|k| k.has_definition).map(|k| k.id).collect();
    let mut hits = Vec::new();
    for o in s.list_objects().await.unwrap() {
        if !with_source.contains(&o.kind.as_str()) || !(q.kinds.is_empty() || q.kinds.contains(&o.kind)) {
            continue;
        }
        let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        if let Ok(Some(src)) = s.definition(&r).await {
            hits.extend(hits_in(&o.kind, o.schema.as_deref(), &o.name, o.parent.as_deref(), &src, q));
        }
    }
    hits
}

fn sorted(mut h: Vec<CodeHit>) -> Vec<CodeHit> {
    h.sort_by(|a, b| (&a.kind, &a.schema, &a.name, a.line).cmp(&(&b.kind, &b.schema, &b.name, b.line)));
    h
}

/// Catalog == scan for each (text, whole word, case sensitive).
async fn same_as_scan(s: &mut Box<dyn Session>, d: &dyn Driver, kinds: &[&str], cases: &[(&str, bool, bool)]) -> Vec<Vec<CodeHit>> {
    let mut all = Vec::new();
    for &(text, word, case) in cases {
        let q = CodeSearch {
            text: text.into(),
            whole_word: word,
            case_sensitive: case,
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
            ..Default::default()
        };
        let fast = s.search_code(&q).await.unwrap().expect("answers from its catalog");
        let fast = sorted(fast.hits);
        let scan = sorted(scan(s, d, &q).await);
        eprintln!("{} {text:?}: {} hits", d.info().id, fast.len());
        assert_eq!(fast, scan, "{text:?}");
        all.push(fast);
    }
    all
}

const CASES: &[(&str, bool, bool)] = &[
    ("ventas", true, false),
    ("Ventas", false, true),
    ("SUM(", false, false),
    ("100%_x", false, false),
    ("\"v_ventas\"", false, false),
    ("view \"public\".", false, false),
    ("año", false, false),
    ("public", true, false),
];

async fn fresh(d: &dyn Driver, cfg: &ConnectionConfig) -> Box<dyn Session> {
    let mut admin = d.connect(cfg, None).await.unwrap();
    let _ = admin.drop_database(DB).await;
    admin.create_database(DB).await.unwrap();
    d.connect(cfg, Some(DB)).await.unwrap()
}

async fn cleanup(d: &dyn Driver, cfg: &ConnectionConfig, s: Box<dyn Session>) {
    drop(s);
    let mut admin = d.connect(cfg, None).await.unwrap();
    admin.drop_database(DB).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres_catalog_equals_scan() {
    let Some(cfg) = cfg("postgres", "DBINE_TEST_POSTGRES_URL") else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let d = driver("postgres");
    let mut s = fresh(d.as_ref(), &cfg).await;
    for sql in [
        "CREATE TABLE \"Ventas\" (id int PRIMARY KEY, total numeric)",
        "CREATE TABLE ventas_hist (id int)",
        "CREATE VIEW v_ventas AS\nSELECT id, total\nFROM \"Ventas\"\nWHERE total > 0",
        "CREATE VIEW v_anio AS SELECT 'AÑO 100%_x' AS etiqueta",
        "CREATE MATERIALIZED VIEW mv_ventas AS SELECT sum(total) AS t FROM \"Ventas\"",
        "CREATE FUNCTION f_total(a int) RETURNS numeric LANGUAGE sql AS $$\n  SELECT SUM(total) FROM \"Ventas\" WHERE id > a\n$$",
        "CREATE FUNCTION f_total(a text) RETURNS text LANGUAGE sql AS $$\n  SELECT a || '100%_x'\n$$",
        "CREATE PROCEDURE p_total() LANGUAGE plpgsql AS $$\nBEGIN\n  PERFORM 1 FROM ventas_hist;\n  PERFORM SUM(total) FROM \"Ventas\";\nEND\n$$",
        "CREATE FUNCTION t_audit() RETURNS trigger LANGUAGE plpgsql AS $$\nBEGIN\n  RETURN NEW;\nEND\n$$",
        "CREATE TRIGGER t_ventas BEFORE INSERT ON \"Ventas\" FOR EACH ROW EXECUTE FUNCTION t_audit()",
        "CREATE TRIGGER t_ventas BEFORE INSERT ON ventas_hist FOR EACH ROW EXECUTE FUNCTION t_audit()",
        "CREATE TYPE estado_ventas AS ENUM ('abierta', 'cerrada')",
        "CREATE DOMAIN monto_ventas AS numeric CHECK (VALUE >= 0)",
        "CREATE SEQUENCE seq_ventas",
    ] {
        run(&mut s, sql).await;
    }

    let hits = same_as_scan(&mut s, d.as_ref(), &[], CASES).await;
    assert!(hits[0].iter().any(|h| h.kind == "view" && h.name == "v_ventas" && h.line > 1 && h.text == "FROM \"Ventas\""), "{:?}", hits[0]);
    assert!(hits[0].iter().any(|h| h.kind == "trigger" && h.name == "t_ventas"), "{:?}", hits[0]);
    assert!(hits[7].iter().any(|h| h.kind == "type") && hits[7].iter().any(|h| h.kind == "sequence"), "{:?}", hits[7]);
    assert!(!hits[0].iter().any(|h| h.text.contains("ventas_hist") && !h.text.contains("\"Ventas\"")), "whole word");
    assert!(hits[3].iter().any(|h| h.kind == "function"), "a later overload: {:?}", hits[3]);
    assert!(hits[5].iter().any(|h| h.kind == "materialized_view") && hits[5].iter().any(|h| h.kind == "view"), "the head counts: {:?}", hits[5]);
    assert!(!hits[6].is_empty(), "non-ASCII without narrowing");
    same_as_scan(&mut s, d.as_ref(), &["trigger", "procedure"], CASES).await;

    let q = CodeSearch { text: "ventas".into(), max_hits: 2, ..Default::default() };
    let capped = s.search_code(&q).await.unwrap().unwrap();
    assert!(capped.truncated && capped.hits.len() == 2, "{capped:?}");

    cleanup(d.as_ref(), &cfg, s).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroach_catalog_equals_scan() {
    let Some(cfg) = cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL") else {
        eprintln!("DBINE_TEST_COCKROACH_URL not set; skipping");
        return;
    };
    let d = driver("cockroachdb");
    let mut s = fresh(d.as_ref(), &cfg).await;
    for sql in [
        "CREATE TABLE \"Ventas\" (id INT PRIMARY KEY, total DECIMAL)",
        "CREATE TABLE ventas_hist (id INT PRIMARY KEY)",
        "CREATE VIEW v_ventas AS SELECT id, total FROM \"Ventas\"",
        "CREATE FUNCTION f_total(a INT) RETURNS DECIMAL LANGUAGE SQL AS $$ SELECT SUM(total) FROM \"Ventas\" WHERE id > a $$",
        "CREATE FUNCTION f_total(a STRING) RETURNS STRING LANGUAGE SQL AS $$ SELECT a || '100%_x año' $$",
        "CREATE PROCEDURE p_total() LANGUAGE SQL AS $$ SELECT 1 FROM ventas_hist; SELECT SUM(total) FROM \"Ventas\" $$",
        "CREATE FUNCTION t_audit() RETURNS TRIGGER LANGUAGE PLpgSQL AS $$ BEGIN RETURN NEW; END $$",
        "CREATE TRIGGER t_ventas BEFORE INSERT ON \"Ventas\" FOR EACH ROW EXECUTE FUNCTION t_audit()",
        "CREATE TRIGGER t_ventas BEFORE INSERT ON ventas_hist FOR EACH ROW EXECUTE FUNCTION t_audit()",
        "CREATE TYPE estado_ventas AS ENUM ('abierta', 'cerrada')",
        "CREATE SEQUENCE seq_ventas",
    ] {
        run(&mut s, sql).await;
    }

    // Tables and views come from SHOW CREATE, one by one: the app scans.
    let q = CodeSearch { text: "ventas".into(), ..Default::default() };
    assert!(s.search_code(&q).await.unwrap().is_none());
    let kinds = ["function", "procedure", "trigger", "sequence", "type"];
    let hits = same_as_scan(&mut s, d.as_ref(), &kinds, CASES).await;
    assert!(hits[0].iter().any(|h| h.kind == "trigger") && hits[0].iter().any(|h| h.kind == "procedure"), "{:?}", hits[0]);
    assert!(hits[7].iter().any(|h| h.kind == "type") && hits[7].iter().any(|h| h.kind == "sequence"), "{:?}", hits[7]);

    cleanup(d.as_ref(), &cfg, s).await;
}
