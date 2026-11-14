//! The function catalog (#97) against real servers (see `tests/integration.rs`):
//! - `DBINE_TEST_SOLR_URL=http://localhost:25522`: a standalone Solr with a
//!   `testcore` core: the function queries, and no SQL.
//! - `DBINE_TEST_SOLRCLOUD_URL=http://localhost:25523`: SolrCloud with the
//!   `sql` module: the function queries and Parallel SQL's functions (the
//!   catalog asks a collection's `/sql`). The test creates the collection
//!   `dbine_fn_probe` and deletes it.
//! - `DBINE_TEST_SOLRCLOUD_NOSQL_URL`: SolrCloud 9+ without the `sql` module
//!   (its default; `docker run --rm -d --name dbine-test-solrcloud-nosql -p
//!   25530:8983 solr:9 solr-foreground -c`): the function queries and no SQL,
//!   though every collection declares `/sql`. Same collection, deleted too.
//!
//! `cargo test -p dbine-driver-solr --test functions -- --ignored`. Each
//! function is also sent to the server, which must know it.

use dbine_driver::functions::version_at_least;
use dbine_driver::{ConnectionConfig, FunctionInfo, FunctionKind};

const COLLECTION: &str = "dbine_fn_probe";

async fn catalog(url: &str) -> (String, Vec<FunctionInfo>) {
    let cfg = ConnectionConfig {
        driver: "solr".into(),
        host: url.into(),
        read_only: true,
        ..Default::default()
    };
    let mut s = dbine_driver_solr::drivers()[0]
        .connect(&cfg, None)
        .await
        .expect("connect");
    let v = s.server_version().await.unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("{v}: {} functions", c.len());
    (v, c)
}

/// The function queries the server's version has, and nothing else.
fn check_function_queries(v: &str, c: &[&FunctionInfo]) {
    assert!(c.iter().all(|f| f.kind == FunctionKind::Scalar));
    let has = |n: &str| c.iter().any(|f| f.name == n);
    for n in ["recip", "termfreq", "if", "exists", "gt"] {
        assert!(has(n), "{n}");
    }
    let at = |m| version_at_least(v, m);
    assert_eq!(has("vectorSimilarity"), at("9.3"), "{v}");
    assert_eq!(has("isnan"), at("9.2"), "{v}");
    assert_eq!(has("lateVector"), at("10.1"), "{v}");
    assert!(!has("upper") && !has("$match") && !has("COUNT"));
}

/// The function queries `core` doesn't know (`{!func}name()`).
async fn unknown_function_queries(url: &str, c: &[&FunctionInfo], core: &str) -> Vec<String> {
    let http = reqwest::Client::new();
    let mut names: Vec<&str> = c.iter().map(|f| f.name.as_str()).collect();
    names.dedup();
    let mut unknown = Vec::new();
    for n in names {
        let body = http
            .get(format!("{url}/solr/{core}/select"))
            .query(&[("q", format!("{{!func}}{n}()")), ("rows", "0".into())])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if body.contains("Unknown function") {
            unknown.push(format!("{n}: {body}"));
        }
    }
    unknown
}

#[tokio::test]
#[ignore]
async fn solr_function_queries() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLR_URL") else {
        return;
    };
    let (v, c) = catalog(&url).await;
    // A standalone server runs no SQL: nothing for the console's SQL lines.
    assert!(c.iter().all(|f| f.language.is_empty()), "{v}");
    let all: Vec<&FunctionInfo> = c.iter().collect();
    check_function_queries(&v, &all);
    let unknown = unknown_function_queries(&url, &all, "testcore").await;
    assert!(unknown.is_empty(), "{unknown:#?}");
}

#[tokio::test]
#[ignore]
async fn solrcloud_sql_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLRCLOUD_URL") else {
        return;
    };
    // A collection to run them on, made first: the catalog asks a
    // collection's `/sql` whether Parallel SQL runs.
    let http = reqwest::Client::new();
    let admin = format!("{url}/solr/admin/collections");
    let drop = || {
        http.get(&admin)
            .query(&[("action", "DELETE"), ("name", COLLECTION)])
            .send()
    };
    let _ = drop().await;
    let created = http
        .get(&admin)
        .query(&[
            ("action", "CREATE"),
            ("name", COLLECTION),
            ("numShards", "1"),
            ("replicationFactor", "1"),
            ("wt", "json"),
        ])
        .send()
        .await
        .unwrap();
    assert!(
        created.status().is_success(),
        "{}",
        created.text().await.unwrap()
    );
    let added = http
        .post(format!("{url}/solr/{COLLECTION}/update?commit=true"))
        .json(&serde_json::json!([
            { "id": "1", "n_i": 2, "s_s": "a" },
            { "id": "2", "n_i": 5, "s_s": "b" }
        ]))
        .send()
        .await
        .unwrap();
    let added = added.status().is_success();

    let (v, c) = catalog(&url).await;
    let (sql, json): (Vec<&FunctionInfo>, Vec<&FunctionInfo>) =
        c.iter().partition(|f| f.language == "sql");

    // Each one runs (the checks of the list itself come after the cleanup).
    let mut failed = Vec::new();
    for f in &sql {
        let args = match f.args.as_str() {
            "(*)" => "(*)".to_string(),
            a => a.replace("field", if a.contains("DISTINCT") { "s_s" } else { "n_i" }),
        };
        let stmt = format!("SELECT {}{args} FROM {COLLECTION}", f.name);
        let body = http
            .post(format!("{url}/solr/{COLLECTION}/sql"))
            .form(&[("stmt", stmt.as_str())])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if body.contains("EXCEPTION") || !body.contains("result-set") {
            failed.push(format!("{stmt}: {body}"));
        }
    }
    failed.extend(unknown_function_queries(&url, &json, COLLECTION).await);
    let _ = drop().await;
    assert!(added);
    assert!(failed.is_empty(), "{failed:#?}");

    check_function_queries(&v, &json);
    let has = |n: &str| sql.iter().any(|f| f.name == n);
    for n in ["COUNT", "MIN", "MAX", "SUM", "AVG"] {
        assert!(has(n), "{n}");
    }
    assert!(sql.iter().all(|f| f.kind == FunctionKind::Aggregate));
    assert_eq!(
        has("APPROX_COUNT_DISTINCT"),
        version_at_least(&v, "8.10"),
        "{v}"
    );
    // Calcite's scalar functions aren't documented (nor pushed down).
    assert!(!has("UPPER") && !has("ROW_NUMBER") && !has("recip"));
}

#[tokio::test]
#[ignore]
async fn solrcloud_without_the_sql_module() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLRCLOUD_NOSQL_URL") else {
        return;
    };
    let http = reqwest::Client::new();
    let admin = format!("{url}/solr/admin/collections");
    let drop = || {
        http.get(&admin)
            .query(&[("action", "DELETE"), ("name", COLLECTION)])
            .send()
    };
    let _ = drop().await;
    let created = http
        .get(&admin)
        .query(&[
            ("action", "CREATE"),
            ("name", COLLECTION),
            ("numShards", "1"),
            ("replicationFactor", "1"),
            ("wt", "json"),
        ])
        .send()
        .await
        .unwrap();
    let created = created.status().is_success();
    // The handler is declared, and fails to load.
    let probe = http
        .get(format!("{url}/solr/{COLLECTION}/sql"))
        .send()
        .await
        .unwrap();
    let probe = (probe.status().as_u16(), probe.text().await.unwrap());
    let (v, c) = catalog(&url).await;
    let _ = drop().await;

    assert!(created);
    assert!(v.contains("SolrCloud"), "{v}");
    assert!(
        probe.0 >= 500 && probe.1.contains("SQLHandler"),
        "{probe:?}"
    );
    assert!(!c.iter().any(|f| f.language == "sql"), "{v}");
    let all: Vec<&FunctionInfo> = c.iter().collect();
    check_function_queries(&v, &all);
}
