//! "Buscar en la base" from the catalog against a real server, checked
//! against the app's per-object scan (list_objects + definition + the same
//! line matching), skipped without `DBINE_TEST_CLICKHOUSE_URL`:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test search -- --ignored
//! ```

use dbine_driver::search::{hits_in, CodeHit, CodeSearch};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

/// The app's scan, as commands/search.rs does it.
async fn scan(s: &mut Box<dyn Session>, with_source: &[&str], q: &CodeSearch) -> Vec<CodeHit> {
    let mut hits = Vec::new();
    for o in s.list_objects().await.unwrap() {
        if !with_source.contains(&o.kind.as_str()) || !(q.kinds.is_empty() || q.kinds.contains(&o.kind)) {
            continue;
        }
        let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        if let Some(src) = s.definition(&r).await.unwrap() {
            hits.extend(hits_in(&o.kind, o.schema.as_deref(), &o.name, o.parent.as_deref(), &src, q));
        }
    }
    hits
}

fn sorted(mut h: Vec<CodeHit>) -> Vec<CodeHit> {
    h.sort_by(|a, b| (&a.kind, &a.name, a.line).cmp(&(&b.kind, &b.name, b.line)));
    h
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn catalog_equals_scan() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap();
    let with_source: Vec<&str> = d.info().object_kinds.iter().filter(|k| k.has_definition).map(|k| k.id).collect();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_search").await;
    run(&mut s, "DROP FUNCTION IF EXISTS dbine_search_total").await;
    s.create_database("dbine_search").await.unwrap();
    let mut s = d.connect(&cfg, Some("dbine_search")).await.unwrap();
    run(&mut s, "CREATE TABLE ventas (id UInt64, total Float64) ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "CREATE TABLE ventas_hist (id UInt64, nota String DEFAULT '100%_x') ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "CREATE VIEW v_ventas AS\nSELECT id, total\nFROM ventas").await;
    run(&mut s, "CREATE MATERIALIZED VIEW mv_ventas ENGINE = MergeTree ORDER BY id AS SELECT id, sum(total) AS t FROM ventas GROUP BY id").await;
    run(
        &mut s,
        "CREATE DICTIONARY d_ventas (id UInt64, total Float64) PRIMARY KEY id \
         SOURCE(CLICKHOUSE(TABLE 'ventas' DB 'dbine_search')) LAYOUT(FLAT()) LIFETIME(0)",
    )
    .await;
    run(&mut s, "CREATE FUNCTION dbine_search_total AS (ventas) -> ventas * 100").await;

    let cases = [
        ("ventas", true, false, vec![]),
        ("VENTAS", false, true, vec![]),
        ("Ventas", false, false, vec![]),
        ("sum(", false, false, vec![]),
        ("100%_x", false, false, vec![]),
        ("ventas", false, false, vec!["function".to_string()]),
        ("ventas", false, false, vec!["view".to_string(), "dictionary".to_string()]),
        ("Año", false, false, vec![]),
    ];
    for (text, word, case, kinds) in cases {
        let q = CodeSearch { text: text.into(), whole_word: word, case_sensitive: case, kinds: kinds.clone(), ..Default::default() };
        let fast = s.search_code(&q).await.unwrap().expect("ClickHouse answers from its catalog").hits;
        let slow = scan(&mut s, &with_source, &q).await;
        eprintln!("{text} {kinds:?}: {} hits", fast.len());
        assert_eq!(sorted(fast), sorted(slow), "{text} {kinds:?}");
    }

    let q = CodeSearch { text: "ventas".into(), whole_word: true, ..Default::default() };
    let hits = s.search_code(&q).await.unwrap().unwrap().hits;
    for kind in ["table", "view", "materialized_view", "dictionary", "function"] {
        assert!(hits.iter().any(|h| h.kind == kind), "{kind}: {hits:?}");
    }
    assert!(!hits.iter().any(|h| h.text.contains("ventas_hist")), "whole word");
    let capped = s.search_code(&CodeSearch { max_hits: 2, ..q }).await.unwrap().unwrap();
    assert_eq!((capped.hits.len(), capped.truncated), (2, true));

    run(&mut s, "DROP FUNCTION dbine_search_total").await;
    drop(s);
    let mut m = d.connect(&cfg, None).await.unwrap();
    m.drop_database("dbine_search").await.unwrap();
}
