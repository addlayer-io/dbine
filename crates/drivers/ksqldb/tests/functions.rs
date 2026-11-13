//! The function catalog (issue #97) against a real ksqlDB (see
//! `integration.rs` for the containers), skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_KSQLDB_URL=http://localhost:25188 \
//!   cargo test -p dbine-driver-ksqldb --test functions -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome};
use std::collections::BTreeSet;

#[tokio::test]
#[ignore]
async fn ksqldb_catalog() {
    let Ok(url) = std::env::var("DBINE_TEST_KSQLDB_URL") else {
        eprintln!("DBINE_TEST_KSQLDB_URL not set; skipping");
        return;
    };
    let url = reqwest::Url::parse(&url).expect("URL");
    let cfg = ConnectionConfig {
        driver: "ksqldb".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        ..Default::default()
    };
    let d = dbine_driver_ksqldb::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    println!("{}", s.server_version().await.unwrap());
    let c = s.function_catalog().await.unwrap();
    println!("{} functions", c.len());
    let get = |n: &str| c.iter().find(|f| f.name == n);
    let ucase = get("UCASE").expect("UCASE");
    assert_eq!(ucase.kind, FunctionKind::Scalar);
    assert_eq!(ucase.args, "(input)");
    assert_eq!(
        get("LATEST_BY_OFFSET").unwrap().kind,
        FunctionKind::Aggregate
    );
    // A table function, called in the SELECT list.
    assert_eq!(get("EXPLODE").unwrap().kind, FunctionKind::Scalar);
    assert!(get("CAST").is_some());
    // The image's test UDFs, and what isn't ksqlDB.
    assert!(get("BAD_UDF").is_none());
    assert!(get("TEST_UDAF").is_none());
    assert!(get("approx_distinct").is_none());
    assert!(get("JSON_VALUE").is_none());

    // The catalog is the server's list: every function `SHOW FUNCTIONS`
    // names but the image's test UDFs, plus CAST (syntax the list doesn't
    // carry), and nothing else.
    let mut out = QueryOutcome::default();
    s.execute("SHOW FUNCTIONS;", 10_000, &mut out)
        .await
        .unwrap();
    let r = out.results.last().expect("SHOW FUNCTIONS");
    let name_at = r
        .columns
        .iter()
        .position(|c| c.name == "name")
        .expect("name");
    let listed: BTreeSet<String> = r
        .rows
        .iter()
        .filter_map(|row| row[name_at].as_str().map(str::to_ascii_uppercase))
        .collect();
    assert!(listed.len() > 100, "{listed:?}");
    let names: BTreeSet<String> = c.iter().map(|f| f.name.to_ascii_uppercase()).collect();
    let left_out: BTreeSet<&str> = listed.difference(&names).map(String::as_str).collect();
    assert!(
        left_out.is_subset(&FIXTURES.into_iter().collect()),
        "{left_out:?}"
    );
    let extra: Vec<&String> = names.difference(&listed).collect();
    assert_eq!(extra, ["CAST"]);
}

/// The test UDFs Confluent's image carries (left out of the catalog).
const FIXTURES: [&str; 25] = [
    "BAD_TEST_UDAF",
    "BAD_TEST_UDF",
    "BAD_TEST_UDTF",
    "BAD_UDF",
    "COMPLEXFUNCTION",
    "CONFIGURABLEUDF",
    "DECIMALSTRUCT",
    "E2ECONFIGURABLEUDF",
    "FIVE_ARG",
    "FOUR_ARG",
    "GENERIC_VAR_ARG",
    "KSQLSTRUCTUDF",
    "MID_VAR_ARG",
    "MULTI_ARG",
    "OBJ_COL_ARG",
    "RETURNDECIMAL",
    "RETURNINCOMPATIBLE",
    "SOMEFUNCTION",
    "TEST_UDAF",
    "TEST_UDF",
    "TEST_UDTF",
    "THROWING_UDTF",
    "VAR_ARG",
    "WHENCONDITION",
    "WHENRESULT",
];
