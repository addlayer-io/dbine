//! The function query catalog (#97) against a real Solr with a `testcore`
//! core (see `tests/integration.rs`):
//! `DBINE_TEST_SOLR_URL=http://localhost:25522 cargo test -p dbine-driver-solr --test functions -- --ignored`.
//! Each function is also sent to the server, which must know it.

use dbine_driver::{ConnectionConfig, FunctionKind};

#[tokio::test]
#[ignore]
async fn solr_function_queries() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLR_URL") else {
        return;
    };
    let cfg = ConnectionConfig {
        driver: "solr".into(),
        host: url.clone(),
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
    assert!(c.iter().all(|f| f.kind == FunctionKind::Scalar));
    let has = |n: &str| c.iter().any(|f| f.name == n);
    for n in ["recip", "termfreq", "if", "exists", "gt"] {
        assert!(has(n), "{n}");
    }
    let at = |m| dbine_driver::functions::version_at_least(&v, m);
    assert_eq!(has("vectorSimilarity"), at("9.3"), "{v}");
    assert_eq!(has("isnan"), at("9.2"), "{v}");
    assert_eq!(has("lateVector"), at("10.1"), "{v}");
    assert!(!has("upper") && !has("$match"));

    // The server knows every one it is offered.
    let http = reqwest::Client::new();
    let mut names: Vec<&str> = c.iter().map(|f| f.name.as_str()).collect();
    names.dedup();
    for n in names {
        let body = http
            .get(format!("{url}/solr/testcore/select"))
            .query(&[("q", format!("{{!func}}{n}()")), ("rows", "0".into())])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!body.contains("Unknown function"), "{n}: {body}");
    }
}
