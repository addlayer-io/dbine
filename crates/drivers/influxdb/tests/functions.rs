//! The function catalog (issue #97) against real servers, each skipped
//! without its variable:
//!
//! ```sh
//! DBINE_TEST_INFLUXDB_URL=http://localhost:25403 \
//! DBINE_TEST_INFLUXDB1_URL=http://localhost:25404 \
//! DBINE_TEST_INFLUXDB3_URL=http://localhost:25409 \
//!   cargo test -p dbine-driver-influxdb --test functions -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, FunctionInfo, FunctionKind, Session};

async fn open(driver: &str, cfg: &ConnectionConfig, db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_influxdb::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    d.connect(cfg, db).await.unwrap()
}

fn find<'a>(c: &'a [FunctionInfo], name: &str, schema: &str) -> Option<&'a FunctionInfo> {
    c.iter().find(|f| f.name == name && f.schema == schema)
}

#[tokio::test]
#[ignore]
async fn influxql_catalog() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB1_URL not set; skipping");
        return;
    };
    let cfg = ConnectionConfig {
        driver: "influxdb1".into(),
        host: url,
        ..Default::default()
    };
    let mut s = open("influxdb1", &cfg, None).await;
    println!("{}", s.server_version().await.unwrap());
    let c = s.function_catalog().await.unwrap();
    println!("{} functions", c.len());
    let mean = find(&c, "MEAN", "").expect("MEAN");
    assert_eq!(mean.kind, FunctionKind::Aggregate);
    // 1.6 and later.
    assert!(find(&c, "ABS", "").is_some());
    assert!(find(&c, "RELATIVE_STRENGTH_INDEX", "").is_some());
    // Not InfluxQL.
    assert!(find(&c, "approx_distinct", "").is_none());
    assert!(find(&c, "HISTOGRAM", "").is_none());
}

#[tokio::test]
#[ignore]
async fn flux_catalog() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB_URL not set; skipping");
        return;
    };
    let org = std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into());
    let token = std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into());
    let mut cfg = ConnectionConfig {
        driver: "influxdb".into(),
        host: url,
        ..Default::default()
    };
    cfg.options.insert("org".into(), org);
    cfg.options.insert("token".into(), token.clone());
    let mut s = open("influxdb", &cfg, None).await;
    println!("{}", s.server_version().await.unwrap());
    let c = s.function_catalog().await.unwrap();
    println!("{} functions", c.len());
    // The prelude is the server's suggestions, name for name and in its
    // order (without the `_` internals). On a server whose Flux the static
    // catalog knows, the static prelude has the same names: this proves the
    // catalog matches the server, and the unit tests prove the parse of the
    // answer's real shape.
    let answer = reqwest::Client::new()
        .get(format!(
            "{}/api/v2/query/suggestions",
            cfg.host.trim_end_matches('/')
        ))
        .header("Authorization", format!("Token {token}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
    let suggested: Vec<&str> = answer["funcs"]
        .as_array()
        .expect("funcs")
        .iter()
        .filter_map(|f| f["name"].as_str())
        .filter(|n| !n.starts_with('_'))
        .collect();
    let prelude: Vec<&str> = c
        .iter()
        .filter(|f| f.schema.is_empty())
        .map(|f| f.name.as_str())
        .collect();
    assert!(suggested.len() > 50, "{suggested:?}");
    assert_eq!(prelude, suggested);
    // Deprecated in Flux 0.172 but still run by the server: offered.
    assert!(find(&c, "join", "").is_some());
    assert!(find(&c, "join", "experimental").is_some());
    // date/boundaries only fails from Flux 0.177.1 on (InfluxDB 2.5+).
    assert!(!c.iter().any(|f| f.schema == "date/boundaries"));
    assert!(find(&c, "monday", "experimental/date/boundaries").is_some());
    let aw = find(&c, "aggregateWindow", "").expect("aggregateWindow");
    assert!(aw.args.contains("every"), "{}", aw.args);
    assert!(find(&c, "from", "").is_some());
    assert!(find(&c, "_fillEmpty", "").is_none(), "internal");
    // Packages, with their import path.
    let upper = find(&c, "toUpper", "strings").expect("strings.toUpper");
    assert_eq!(upper.args, "(v)");
    assert!(find(&c, "measurements", "influxdata/influxdb/schema").is_some());
    // Flux 0.173 and later (InfluxDB 2.4+).
    assert!(find(&c, "sand", "bitwise").is_some());
    assert!(!c.iter().any(|f| f.schema.starts_with("internal")
        || f.schema.starts_with("testing")
        || f.schema.starts_with("contrib")));
    assert!(c.iter().all(|f| f.description.is_empty()));
}

#[tokio::test]
#[ignore]
async fn influxdb3_sql_catalog() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB3_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB3_URL not set; skipping");
        return;
    };
    let cfg = ConnectionConfig {
        driver: "influxdb3".into(),
        host: url,
        ..Default::default()
    };
    let db = "dbine_functions";
    let mut admin = open("influxdb3", &cfg, None).await;
    let _ = admin.drop_database(db).await;
    admin.create_database(db).await.unwrap();
    let mut s = open("influxdb3", &cfg, Some(db)).await;
    println!("{}", s.server_version().await.unwrap());
    let c = s.function_catalog().await.unwrap();
    println!("{} functions", c.len());
    // From information_schema.routines: with the server's descriptions.
    let ad = find(&c, "approx_distinct", "").expect("approx_distinct");
    assert_eq!(ad.kind, FunctionKind::Aggregate);
    assert!(!ad.description.is_empty());
    let rn = find(&c, "row_number", "").expect("row_number");
    assert_eq!(rn.kind, FunctionKind::Window);
    // InfluxDB's own.
    assert!(find(&c, "date_bin_gapfill", "").is_some());
    assert!(find(&c, "selector_first", "").is_some());
    // Internal, and not DataFusion.
    assert!(find(&c, "influx_regex_match", "").is_none());
    assert!(find(&c, "json_value", "").is_none());
    // Without a database of its own, the session reads through the first.
    let c = admin.function_catalog().await.unwrap();
    assert!(!find(&c, "approx_distinct", "")
        .unwrap()
        .description
        .is_empty());
    drop(s);
    admin.drop_database(db).await.unwrap();
}
