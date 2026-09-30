//! "Comparar esquemas" against real servers: two indices, one with its own
//! analysis (analyzer, filter, normalizer) and fields that use it, the other
//! plain. The sync script takes the plain one to the first and both then
//! read the same (analysis, fields and their parameters).
//!
//! ```sh
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//!   cargo test -p dbine-driver-elasticsearch --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session, TableChange, TableSchema};

async fn run(s: &mut Box<dyn Session>, text: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.map_err(|e| format!("{text}: {e}"))?;
    out.error.map_or(Ok(()), |e| Err(format!("{text}: {e}")))
}

const SOURCE: &str = r#"
PUT /dbine_cmp_src
{
  "settings": {
    "analysis": {
      "filter": { "es_stop": { "type": "stop", "stopwords": "_spanish_" } },
      "analyzer": { "es": { "type": "custom", "tokenizer": "standard", "filter": ["lowercase", "es_stop"] } },
      "normalizer": { "low": { "type": "custom", "filter": ["lowercase"] } }
    }
  },
  "mappings": { "properties": { "titulo": { "type": "text", "analyzer": "es" }, "codigo": { "type": "keyword", "normalizer": "low" } } }
}
"#;

const TARGET: &str = r#"
PUT /dbine_cmp_dst
{ "mappings": { "properties": { "otro": { "type": "keyword" } } } }
"#;

fn by_name(schema: &[TableSchema], name: &str) -> TableSchema {
    schema.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("{name}")).clone()
}

async fn compare_and_sync(id: &str, url: &str) {
    let d = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let cfg = ConnectionConfig { driver: id.into(), host: url.into(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let _ = run(&mut s, "DELETE /dbine_cmp_src,dbine_cmp_dst?ignore_unavailable=true").await;
    run(&mut s, SOURCE).await.unwrap();
    run(&mut s, TARGET).await.unwrap();
    let schema = s.database_schema().await.unwrap();
    let src = by_name(&schema, "dbine_cmp_src");
    let dst = by_name(&schema, "dbine_cmp_dst");
    println!("{:?}", src.options);
    assert!(src.options.get("analysis").is_some_and(|a| a.contains("es_stop")));
    // The target takes the source's analysis and fields (as the compare carries them).
    let mut new = dst.clone();
    new.options.insert("analysis".into(), src.options["analysis"].clone());
    new.columns.extend(src.columns.iter().cloned());
    let script = d.sync_script(&[TableChange::Alter { old: dst, new }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut s, st).await.unwrap();
    }
    let after = by_name(&s.database_schema().await.unwrap(), "dbine_cmp_dst");
    assert_eq!(after.options.get("analysis"), src.options.get("analysis"));
    for c in &src.columns {
        assert_eq!(after.columns.iter().find(|x| x.name == c.name), Some(c));
    }
    run(&mut s, "DELETE /dbine_cmp_src,dbine_cmp_dst").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn elasticsearch_compare() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_URL") else { return };
    compare_and_sync("elasticsearch", &url).await;
}

#[tokio::test]
#[ignore]
async fn opensearch_compare() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") else { return };
    compare_and_sync("opensearch", &url).await;
}
