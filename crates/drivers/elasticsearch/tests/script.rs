//! Console scripts against real servers, as in tests/integration.rs: each
//! request's place, errors with their type and line, `_msearch` item
//! errors as warnings.
//!
//! `DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//!   cargo test -p dbine-driver-elasticsearch --test script -- --ignored`

use dbine_driver::{ConnectionConfig, MessageLevel, QueryOutcome, Session};

async fn session(id: &str, url: &str) -> Box<dyn Session> {
    let d = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    d.connect(&ConnectionConfig { driver: id.into(), host: url.into(), ..Default::default() }, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.map(|_| out)
}

async fn check(id: &str, url: &str) {
    let mut s = session(id, url).await;
    let _ = run(&mut s, "DELETE /dbine_script").await;
    let out = run(
        &mut s,
        "# setup\nPUT /dbine_script/_doc/1?refresh=true\n{\"a\": 1}\n\nGET /dbine_script/_count\nSELECT a FROM dbine_script",
    )
    .await
    .unwrap();
    let place: Vec<_> = out.results.iter().map(|r| (r.statement, r.line, r.offset)).collect();
    assert_eq!(place[0], (Some(0), Some(2), Some(8)));
    assert_eq!(place[1], (Some(1), Some(5), Some(56)));
    assert_eq!(place.last().unwrap().1, Some(6));

    // _msearch: a failing search is a warning, the others' hits stay.
    let out = run(&mut s, "POST /_msearch\n{\"index\": \"dbine_script\"}\n{\"query\": {\"match_all\": {}}}\n{\"index\": \"dbine_nope\"}\n{\"query\": {\"match_all\": {}}}").await.unwrap();
    assert!(out.log.iter().any(|m| m.level == MessageLevel::Warning && m.text.starts_with("Búsqueda 2")), "{:?}", out.log);

    // The failing request: its type as the code, its line; the script stops.
    let mut out = QueryOutcome::default();
    let e = s.execute("GET /dbine_script/_count\n\nPOST /dbine_script/_search\n{\"query\": {\"nope\": {}}}\n\nGET /dbine_script/_count", 10, &mut out).await.unwrap_err().to_script_error();
    assert_eq!(e.line, Some(3), "{e:?}");
    assert!(e.code.as_deref().is_some_and(|c| c.ends_with("_exception")), "{e:?}");
    assert_eq!(out.results.len(), 1);
    // A line that's neither a request nor SQL runs nothing.
    let mut out = QueryOutcome::default();
    let e = s.execute("GET /dbine_script/_count\n\nhola", 10, &mut out).await.unwrap_err().to_script_error();
    assert_eq!((e.line, e.offset), (Some(3), Some(26)), "{e:?}");
    assert!(out.results.is_empty());
    run(&mut s, "DELETE /dbine_script").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn elasticsearch_scripts() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_URL") else { return };
    check("elasticsearch", &url).await;
}

#[tokio::test]
#[ignore]
async fn opensearch_scripts() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") else { return };
    check("opensearch", &url).await;
}
