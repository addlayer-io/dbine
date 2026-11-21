//! "Linaje" of a whole database (`Session::lineage_graph`) against a real
//! ClickHouse server: a materialized view with its `TO` table and its source
//! (`dependencies_*`, confirmed), a view over a table of another database
//! (external), a dictionary on a table, a table whose default reads the
//! dictionary, a Buffer table and a table nobody touches. Everything is
//! dropped at the end, also when a check fails.
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test lineage -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::{Confidence, LineageScope};
use dbine_driver::{DependencyScan, QueryOutcome, Session};

fn cfg() -> Option<dbine_driver::ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(dbine_driver::ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .map_err(|e| format!("{e}\n---\n{sql}"))
}

#[tokio::test]
#[ignore]
async fn lineage_live() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set: skipped");
        return;
    };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap();
    let tag = format!("{}", std::process::id());
    let (a, b) = (format!("dbine_lin_{tag}"), format!("dbine_lin_x_{tag}"));
    let mut admin = d.connect(&c, None).await.unwrap();
    let setup = format!(
        "CREATE DATABASE {a};
         CREATE DATABASE {b};
         CREATE TABLE {a}.src (id UInt64, v String) ENGINE = MergeTree ORDER BY id;
         CREATE TABLE {a}.dst (id UInt64, v String) ENGINE = MergeTree ORDER BY id;
         CREATE TABLE {a}.loose (id UInt64) ENGINE = MergeTree ORDER BY id;
         CREATE TABLE {b}.other (id UInt64, w String) ENGINE = MergeTree ORDER BY id;
         CREATE VIEW {a}.v AS SELECT s.id, o.w FROM {a}.src AS s JOIN {b}.other AS o ON o.id = s.id;
         CREATE MATERIALIZED VIEW {a}.mv TO {a}.dst AS SELECT id, v FROM {a}.src;
         CREATE DICTIONARY {a}.dict (id UInt64, v String) PRIMARY KEY id
           SOURCE(CLICKHOUSE(TABLE 'src' DB '{a}')) LAYOUT(FLAT()) LIFETIME(0);
         CREATE TABLE {a}.withdict (id UInt64, v String DEFAULT dictGet('{a}.dict', 'v', id))
           ENGINE = MergeTree ORDER BY id;
         CREATE TABLE {a}.buf AS {a}.src ENGINE = Buffer('{a}', 'src', 1, 10, 100, 10000, 1000000, 10000000, 100000000)"
    );
    let mut failures: Vec<String> = Vec::new();
    match run(&mut admin, &setup).await {
        Err(e) => failures.push(format!("setup: {e}")),
        Ok(()) => {
            let mut s = d.connect(&c, Some(&a)).await.unwrap();
            let scan =
                DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
            let g = s
                .lineage_graph(&a, &LineageScope::default(), &scan)
                .await
                .unwrap();
            let edge = |from: &str, to: &str| {
                g.edges
                    .iter()
                    .find(|e| e.from.name == from && e.to.name == to)
                    .map(|e| e.confidence)
            };
            println!(
                "{:#?}",
                g.edges
                    .iter()
                    .map(|e| format!("{}>{} {:?}", e.from.name, e.to.name, e.confidence))
                    .collect::<Vec<_>>()
            );
            let mut check = |ok: bool, what: &str| {
                if !ok {
                    failures.push(what.to_string());
                }
            };
            check(
                g.objects.iter().any(|o| o.name == "loose"),
                "the isolated table is a node",
            );
            check(
                edge("src", "mv") == Some(Confidence::Confirmed),
                "src > mv confirmed",
            );
            check(
                edge("dst", "mv") == Some(Confidence::Confirmed),
                "dst > mv (TO) confirmed",
            );
            check(
                edge("src", "v") == Some(Confidence::Probable),
                "src > v probable",
            );
            check(
                g.external
                    .iter()
                    .any(|x| x.to.name == "v" && x.source.name.as_deref() == Some("other")),
                "the table of another database is external",
            );
            check(edge("src", "dict").is_some(), "src > dict");
            check(edge("dict", "withdict").is_some(), "dict > withdict");
            check(edge("src", "buf").is_some(), "src > buf (Buffer)");
        }
    }
    for sql in [
        format!("DROP DATABASE IF EXISTS {a} SYNC"),
        format!("DROP DATABASE IF EXISTS {b} SYNC"),
    ] {
        if let Err(e) = run(&mut admin, &sql).await {
            failures.push(format!("cleanup: {e}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
