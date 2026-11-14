//! The functions of the console's SQL lines (#97) against real servers (the
//! containers of `tests/integration.rs`; Open Distro as in `tests/jobs.rs`):
//!
//! ```sh
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//! DBINE_TEST_OPENDISTRO_URL=http://localhost:25524 \
//!   cargo test -p dbine-driver-elasticsearch --test functions -- --ignored
//! ```
//!
//! Each function offered is sent to the server, which must know it. The
//! tests create one index, `dbine_fn_probe`, and delete it.

use dbine_driver::functions::{parse_catalog, version_at_least};
use dbine_driver::{ConnectionConfig, FunctionInfo, FunctionKind};
use serde_json::{json, Value};
use std::collections::BTreeSet;

const INDEX: &str = "dbine_fn_probe";

async fn catalog(id: &str, url: &str) -> (String, Vec<FunctionInfo>) {
    let cfg = ConnectionConfig {
        driver: id.into(),
        host: url.into(),
        read_only: true,
        ..Default::default()
    };
    let mut s = dbine_driver_elasticsearch::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
        .connect(&cfg, None)
        .await
        .expect("connect");
    let v = s.server_version().await.unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("{v}: {} functions", c.len());
    (v, c)
}

fn names(c: &[FunctionInfo]) -> BTreeSet<String> {
    c.iter().map(|f| f.name.clone()).collect()
}

/// A raw call: the status and the JSON body.
async fn call(method: reqwest::Method, url: &str, body: Option<Value>) -> (u16, Value) {
    let mut rb = reqwest::Client::new().request(method, url);
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    let resp = rb.send().await.unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// A probe index with a text, a keyword, an integer, a double and a nested
/// object.
async fn create_probe(url: &str) {
    let base = format!("{url}/{INDEX}");
    let _ = call(reqwest::Method::DELETE, &base, None).await;
    let mapping = json!({ "mappings": { "properties": {
        "t": { "type": "text" }, "k": { "type": "keyword" },
        "n": { "type": "integer" }, "d": { "type": "double" },
        "m": { "type": "nested", "properties": { "x": { "type": "keyword" } } } } } });
    let (status, body) = call(reqwest::Method::PUT, &base, Some(mapping)).await;
    assert!(status < 300, "{body}");
    let doc = json!({ "t": "hello world", "k": "hello", "n": 2, "d": 1.5, "m": [{ "x": "a" }] });
    let (status, body) = call(
        reqwest::Method::POST,
        &format!("{base}/_doc?refresh=true"),
        Some(doc),
    )
    .await;
    assert!(status < 300, "{body}");
}

async fn drop_probe(url: &str) {
    let _ = call(reqwest::Method::DELETE, &format!("{url}/{INDEX}"), None).await;
}

/// Runs `stmt` on the SQL endpoint: the error text, or None when it ran.
async fn sql(endpoint: &str, stmt: &str) -> Option<String> {
    let (status, body) = call(
        reqwest::Method::POST,
        endpoint,
        Some(json!({ "query": stmt })),
    )
    .await;
    (status >= 400 || body.get("error").is_some()).then(|| body.to_string())
}

/// How each function the plugins document is written where `NAME()` alone
/// isn't how it's called: special syntax, a predicate, or a function the
/// new engine doesn't parse without a FROM (Open Distro's legacy ones).
const SAMPLES: &[(&str, &str)] = &[
    ("CAST", "SELECT CAST(n AS STRING) FROM dbine_fn_probe"),
    ("EXTRACT", "SELECT EXTRACT(YEAR FROM '2020-01-01 00:00:00')"),
    ("POSITION", "SELECT POSITION('a' IN 'abc')"),
    ("GET_FORMAT", "SELECT GET_FORMAT(DATE, 'USA')"),
    (
        "TIMESTAMPADD",
        "SELECT TIMESTAMPADD(DAY, 17, '2000-01-01 00:00:00')",
    ),
    (
        "TIMESTAMPDIFF",
        "SELECT TIMESTAMPDIFF(YEAR, '1997-01-01 00:00:00', '2001-03-06 00:00:00')",
    ),
    ("TYPEOF", "SELECT TYPEOF(1)"),
    ("NESTED", "SELECT NESTED(m.x, m) FROM dbine_fn_probe"),
    ("PERCENTILE", "SELECT PERCENTILE(n, 50) FROM dbine_fn_probe"),
    (
        "PERCENTILE_APPROX",
        "SELECT PERCENTILE_APPROX(n, 50) FROM dbine_fn_probe",
    ),
    (
        "MATCH",
        "SELECT n FROM dbine_fn_probe WHERE MATCH(t, 'hello')",
    ),
    (
        "MATCHQUERY",
        "SELECT n FROM dbine_fn_probe WHERE MATCHQUERY(t, 'hello')",
    ),
    (
        "MATCH_QUERY",
        "SELECT n FROM dbine_fn_probe WHERE MATCH_QUERY(t, 'hello')",
    ),
    (
        "MATCH_PHRASE",
        "SELECT n FROM dbine_fn_probe WHERE MATCH_PHRASE(t, 'hello world')",
    ),
    (
        "MATCH_BOOL_PREFIX",
        "SELECT n FROM dbine_fn_probe WHERE MATCH_BOOL_PREFIX(t, 'hel')",
    ),
    (
        "MATCH_PHRASE_PREFIX",
        "SELECT n FROM dbine_fn_probe WHERE MATCH_PHRASE_PREFIX(t, 'hello wor')",
    ),
    (
        "MULTI_MATCH",
        "SELECT n FROM dbine_fn_probe WHERE MULTI_MATCH('query'='hello', 'fields'='t')",
    ),
    (
        "SIMPLE_QUERY_STRING",
        "SELECT n FROM dbine_fn_probe WHERE SIMPLE_QUERY_STRING(['t'], 'hello')",
    ),
    (
        "QUERY_STRING",
        "SELECT n FROM dbine_fn_probe WHERE QUERY_STRING(['t'], 'hello')",
    ),
    (
        "QUERY",
        "SELECT n FROM dbine_fn_probe WHERE QUERY('t:hello')",
    ),
    (
        "WILDCARD_QUERY",
        "SELECT n FROM dbine_fn_probe WHERE WILDCARD_QUERY(k, 'hel*')",
    ),
    (
        "SCORE",
        "SELECT n FROM dbine_fn_probe WHERE SCORE(MATCH_QUERY(t, 'hello'), 2.0)",
    ),
    (
        "HIGHLIGHT",
        "SELECT n, HIGHLIGHT(t) FROM dbine_fn_probe WHERE MATCH(t, 'hello')",
    ),
    ("ADD", "SELECT ADD(n, 1) FROM dbine_fn_probe"),
    ("SUBTRACT", "SELECT SUBTRACT(n, 1) FROM dbine_fn_probe"),
    ("MULTIPLY", "SELECT MULTIPLY(n, 2) FROM dbine_fn_probe"),
    ("DIVIDE", "SELECT DIVIDE(n, 1) FROM dbine_fn_probe"),
    ("MODULUS", "SELECT MODULUS(n, 2) FROM dbine_fn_probe"),
    ("ASCII", "SELECT ASCII(k) FROM dbine_fn_probe"),
    ("CBRT", "SELECT CBRT(n) FROM dbine_fn_probe"),
    ("COSH", "SELECT COSH(n) FROM dbine_fn_probe"),
    ("SINH", "SELECT SINH(n) FROM dbine_fn_probe"),
    ("EXPM1", "SELECT EXPM1(n) FROM dbine_fn_probe"),
    ("RINT", "SELECT RINT(d) FROM dbine_fn_probe"),
    ("SIGNUM", "SELECT SIGNUM(n) FROM dbine_fn_probe"),
    ("CURDATE", "SELECT CURDATE() FROM dbine_fn_probe"),
    ("NOW", "SELECT NOW() FROM dbine_fn_probe"),
    ("LEFT", "SELECT LEFT(k, 2) FROM dbine_fn_probe"),
    ("LOCATE", "SELECT LOCATE('l', k) FROM dbine_fn_probe"),
    ("REPLACE", "SELECT REPLACE(k, 'l', 'x') FROM dbine_fn_probe"),
    ("MAKETIME", "SELECT MAKETIME(1, 2, 3) FROM dbine_fn_probe"),
];

/// Whether the SQL plugin knows `name`: `NAME()` runs or fails only on
/// its arguments, or its sample runs.
async fn plugin_knows(endpoint: &str, f: &FunctionInfo) -> Result<(), String> {
    let name = f.name.as_str();
    if let Some((_, stmt)) = SAMPLES.iter().find(|(n, _)| *n == name) {
        return match sql(endpoint, stmt).await {
            None => Ok(()),
            Some(e) => Err(format!("{stmt}: {e}")),
        };
    }
    let stmt = match f.kind {
        FunctionKind::Aggregate => format!("SELECT {name}(n) FROM {INDEX}"),
        FunctionKind::Window => format!("SELECT {name}() OVER (ORDER BY n) FROM {INDEX}"),
        _ => format!("SELECT {name}()"),
    };
    match sql(endpoint, &stmt).await {
        None => Ok(()),
        Some(e) if e.contains("function expected") => Ok(()),
        Some(e) => Err(format!("{stmt}: {e}")),
    }
}

#[tokio::test]
#[ignore]
async fn elasticsearch_sql_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_URL") else {
        return;
    };
    let (v, c) = catalog("elasticsearch", &url).await;
    let at = |m| version_at_least(&v, m);
    assert!(c.iter().all(|f| f.language == "sql"));
    let have = names(&c);
    // The server's own list, whole, except CASE (syntax, not a call).
    let (_, listed) = call(
        reqwest::Method::POST,
        &format!("{url}/_sql?format=json"),
        Some(json!({ "query": "SHOW FUNCTIONS" })),
    )
    .await;
    let listed: BTreeSet<String> = listed["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .filter(|n| n != "CASE")
        .collect();
    assert!(listed.len() > 100);
    let unlisted: BTreeSet<String> = ["EXTRACT", "MATCH", "QUERY"].map(String::from).into();
    assert_eq!(
        have,
        listed.union(&unlisted).cloned().collect::<BTreeSet<_>>()
    );
    // The static fallback says the same for this version.
    let fallback = parse_catalog(include_str!("../src/functions/elasticsearch.txt"), Some(&v));
    assert_eq!(names(&fallback), have, "{v}");
    // Arguments and kinds.
    let avg = c.iter().find(|f| f.name == "AVG").unwrap();
    assert_eq!(avg.kind, FunctionKind::Aggregate);
    assert_eq!(avg.args, "(numeric_field)");
    assert_eq!(c.iter().filter(|f| f.name == "COUNT").count(), 3);
    assert_eq!(have.contains("DATE_FORMAT"), at("8.5"), "{v}");
    assert_eq!(have.contains("TO_CHAR"), at("7.12"), "{v}");
    for n in [
        "CASE",
        "ROW_NUMBER",
        "PERCENTILE_APPROX",
        "TYPEOF",
        "recip",
        "$match",
    ] {
        assert!(!have.contains(n), "{n}");
    }

    // The server knows every one: none is an unknown function.
    create_probe(&url).await;
    let endpoint = format!("{url}/_sql?format=json");
    let mut unknown = Vec::new();
    for n in &have {
        let stmt = match n.as_str() {
            "EXTRACT" => "SELECT EXTRACT(YEAR FROM NOW())".to_string(),
            "MATCH" => format!("SELECT n FROM {INDEX} WHERE MATCH(t, 'hello')"),
            "QUERY" => format!("SELECT n FROM {INDEX} WHERE QUERY('t:hello')"),
            _ => format!("SELECT {n}()"),
        };
        if let Some(e) = sql(&endpoint, &stmt).await {
            if e.contains("Unknown function") || matches!(n.as_str(), "EXTRACT" | "MATCH" | "QUERY")
            {
                unknown.push(format!("{stmt}: {e}"));
            }
        }
    }
    drop_probe(&url).await;
    assert!(unknown.is_empty(), "{unknown:#?}");
}

#[tokio::test]
#[ignore]
async fn opensearch_sql_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") else {
        return;
    };
    let (v, c) = catalog("opensearch", &url).await;
    let at = |m| version_at_least(&v, m);
    assert!(c.iter().all(|f| f.language == "sql"));
    let have = names(&c);
    assert!(have.len() > 100);
    for n in ["ABS", "COUNT", "MATCH_QUERY", "ROW_NUMBER", "DATE_FORMAT"] {
        assert!(have.contains(n), "{n}");
    }
    assert_eq!(have.contains("MATCH"), at("1.2"), "{v}");
    assert_eq!(have.contains("TYPEOF"), at("2.4"), "{v}");
    assert_eq!(have.contains("PERCENTILE"), at("2.15"), "{v}");
    for n in ["CASE", "HISTOGRAM", "TO_CHAR", "recip", "$match"] {
        assert!(!have.contains(n), "{n}");
    }
    assert_eq!(
        c.iter().find(|f| f.name == "DENSE_RANK").unwrap().kind,
        FunctionKind::Window
    );

    create_probe(&url).await;
    let endpoint = format!("{url}/_plugins/_sql");
    let mut unknown = Vec::new();
    for f in &c {
        if let Err(e) = plugin_knows(&endpoint, f).await {
            unknown.push(e);
        }
    }
    drop_probe(&url).await;
    assert!(unknown.is_empty(), "{unknown:#?}");
}

#[tokio::test]
#[ignore]
async fn opendistro_sql_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENDISTRO_URL") else {
        return;
    };
    let (_, c) = catalog("opendistro", &url).await;
    assert!(c.iter().all(|f| f.language == "sql"));
    let have = names(&c);
    // The plugin's own version (1.13.x in the container), not Elasticsearch's.
    let (_, plugins) = call(
        reqwest::Method::GET,
        &format!("{url}/_cat/plugins?format=json&h=component,version"),
        None,
    )
    .await;
    let plugin = plugins
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["component"].as_str().unwrap().ends_with("sql"))
        .map(|p| p["version"].as_str().unwrap().to_string())
        .unwrap();
    let at = |m| version_at_least(&plugin, m);
    for n in ["ABS", "COUNT", "MATCH_QUERY", "SUBSTRING"] {
        assert!(have.contains(n), "{n}");
    }
    assert_eq!(have.contains("DAYNAME"), at("1.11"), "{plugin}");
    assert_eq!(have.contains("NULLIF"), at("1.13"), "{plugin}");
    // OpenSearch's later functions, and Elasticsearch's.
    for n in [
        "MATCH",
        "TYPEOF",
        "PERCENTILE",
        "HISTOGRAM",
        "CASE",
        "recip",
    ] {
        assert!(!have.contains(n), "{n}");
    }

    create_probe(&url).await;
    let endpoint = format!("{url}/_opendistro/_sql");
    let mut unknown = Vec::new();
    for f in &c {
        if let Err(e) = plugin_knows(&endpoint, f).await {
            unknown.push(e);
        }
    }
    drop_probe(&url).await;
    assert!(unknown.is_empty(), "{unknown:#?}");
}
