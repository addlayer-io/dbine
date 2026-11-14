//! The function catalog (issue #97) against real servers (same containers
//! as `integration.rs`), each skipped without its variable:
//!
//! ```sh
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 DBINE_TEST_IOTDB2_URL=http://localhost:27150 \
//!   cargo test -p dbine-driver-iotdb --test functions -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome, Session};
use std::collections::BTreeSet;

async fn check(var: &str) {
    let Ok(url) = std::env::var(var) else {
        eprintln!("{var} not set; skipping");
        return;
    };
    for id in ["iotdb", "timechodb"] {
        let d = dbine_driver_iotdb::drivers()
            .into_iter()
            .find(|d| d.info().id == id)
            .unwrap();
        let cfg = ConnectionConfig {
            driver: id.into(),
            host: url.clone(),
            username: Some("root".into()),
            password: Some("root".into()),
            ..Default::default()
        };
        let mut s = d.connect(&cfg, None).await.unwrap();
        println!("{}", s.server_version().await.unwrap());
        let c = s.function_catalog().await.unwrap();
        println!("{} functions", c.len());
        let get = |n: &str| c.iter().find(|f| f.name == n);
        let avg = get("AVG").expect("AVG");
        assert_eq!(avg.kind, FunctionKind::Aggregate);
        assert_eq!(avg.args, "(ts)");
        let top = get("TOP_K").expect("TOP_K");
        assert_eq!(top.kind, FunctionKind::Scalar);
        assert!(top.args.contains("'k'"), "{}", top.args);
        // 1.3.2 and later.
        assert!(get("MAX_BY").is_some());
        // Not IoTDB's tree model.
        assert!(get("approx_distinct").is_none());
        assert!(get("GROUP_CONCAT").is_none());
        // The catalog is the server's list: every function `SHOW FUNCTIONS`
        // names that can be called, and nothing else.
        let listed = show_functions(&mut s).await;
        let names: BTreeSet<String> = c.iter().map(|f| f.name.to_ascii_uppercase()).collect();
        assert_eq!(names, listed);
    }
}

/// The names `SHOW FUNCTIONS` lists, upper-cased, without a UDF whose
/// `State` (2.x) isn't `AVAILABLE`.
async fn show_functions(s: &mut Box<dyn Session>) -> BTreeSet<String> {
    let mut out = QueryOutcome::default();
    s.execute("SHOW FUNCTIONS", 10_000, &mut out).await.unwrap();
    let r = out.results.last().expect("SHOW FUNCTIONS");
    let col = |n: &str| {
        r.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(n))
    };
    let name_at = col("FunctionName").expect("FunctionName");
    let state_at = col("State");
    let listed: BTreeSet<String> = r
        .rows
        .iter()
        .filter(|row| {
            state_at
                .and_then(|i| row[i].as_str())
                .is_none_or(|s| s.is_empty() || s.eq_ignore_ascii_case("AVAILABLE"))
        })
        .filter_map(|row| row[name_at].as_str().map(str::to_ascii_uppercase))
        .collect();
    assert!(listed.len() > 50, "{listed:?}");
    listed
}

#[tokio::test]
#[ignore]
async fn iotdb1_catalog() {
    check("DBINE_TEST_IOTDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn iotdb2_catalog() {
    check("DBINE_TEST_IOTDB2_URL").await;
}
