//! The function catalog (issue #97) against a real TDengine (taosAdapter's
//! REST port), skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//!   cargo test -p dbine-driver-tdengine --test functions -- --ignored --nocapture
//! ```
//!
//! The UDFs it creates have a placeholder body (`/etc/hostname`, read by
//! taosAdapter in the same container): TDengine stores the body without
//! checking it, and nothing here calls them. The test drops them.

use dbine_driver::functions::version_at_least;
use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TDENGINE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn tdengine_catalog() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_TDENGINE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_tdengine::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let version = s.server_version().await.unwrap();
    println!("{version}");
    for sql in [
        "DROP FUNCTION IF EXISTS dbine_fn_scalar",
        "DROP FUNCTION IF EXISTS dbine_fn_agg",
        "CREATE FUNCTION dbine_fn_scalar AS '/etc/hostname' OUTPUTTYPE INT LANGUAGE 'Python'",
        "CREATE AGGREGATE FUNCTION dbine_fn_agg AS '/etc/hostname' OUTPUTTYPE DOUBLE \
         BUFSIZE 64 LANGUAGE 'Python'",
    ] {
        run(&mut s, sql).await;
    }
    let catalog = s.function_catalog().await;
    run(&mut s, "DROP FUNCTION dbine_fn_scalar").await;
    run(&mut s, "DROP FUNCTION dbine_fn_agg").await;
    let catalog = catalog.unwrap();
    println!("{} functions", catalog.len());
    let get = |n: &str| catalog.iter().find(|f| f.name == n);

    // Built-ins.
    assert_eq!(get("TWA").expect("TWA").kind, FunctionKind::Aggregate);
    assert!(get("CSUM").is_some());
    assert!(get("PI").is_some(), "3.3.3.0 and later");
    // By version: GROUP_CONCAT came with 3.3.8.0, ROW_NUMBER with 3.4.2.0.
    let v = version.trim_start_matches("TDengine ");
    assert_eq!(
        get("GROUP_CONCAT").is_some(),
        version_at_least(v, "3.3.8.0")
    );
    assert_eq!(get("ROW_NUMBER").is_some(), version_at_least(v, "3.4.2.0"));
    // The ones the reference gives no version for: offered exactly when
    // this server runs them.
    for f in ["MD5", "CRC32", "TO_BASE64"] {
        let mut out = QueryOutcome::default();
        let runs = s
            .execute(&format!("SELECT {f}('dbine')"), 10, &mut out)
            .await
            .is_ok();
        assert_eq!(get(f).is_some(), runs, "{f} on {version}");
    }
    // Not TDengine.
    assert!(get("approx_distinct").is_none());
    assert!(get("JSON_VALUE").is_none());

    // The UDFs.
    let f = get("dbine_fn_scalar").expect("scalar UDF");
    assert_eq!(f.kind, FunctionKind::Scalar);
    assert_eq!(f.returns, "INT");
    let f = get("dbine_fn_agg").expect("aggregate UDF");
    assert_eq!(f.kind, FunctionKind::Aggregate);
    assert_eq!(f.returns, "DOUBLE");
}
