//! "Renombrar…" against real servers (same containers as `integration.rs`):
//! `DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521
//!  DBINE_TEST_OPENDISTRO_URL=http://localhost:25524 cargo test -p dbine-driver-elasticsearch --test rename -- --ignored --nocapture`
//!
//! An index with documents and two aliases (one filtered and routed, one
//! the write index) is renamed by copy: the documents, the aliases and
//! writes through the write alias carry over, and the old index is gone.
//! Then an alias pointing to two indices is renamed.

use dbine_driver::rename::{RenameRequest, RenameTarget};
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};

const SEED: &str = r#"
DELETE /dbine_rn,dbine_rn2,dbine_rn_b?ignore_unavailable=true

PUT /dbine_rn
{"settings": {"number_of_shards": 1, "number_of_replicas": 1}, "mappings": {"properties": {"x": {"type": "keyword"}}}, "aliases": {"dbine_rn_w": {"is_write_index": true}, "dbine_rn_f": {"filter": {"term": {"x": "1"}}, "routing": "1"}}}

PUT /dbine_rn_b
{"aliases": {"dbine_rn_all": {}}}

POST /dbine_rn/_doc?refresh=true
{"x": "1"}

POST /dbine_rn/_doc?refresh=true
{"x": "2"}

POST /_aliases
{"actions": [{"add": {"index": "dbine_rn", "alias": "dbine_rn_all", "is_write_index": true}}]}
"#;

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    out
}

/// `GET /<target>/_count`'s count.
async fn count(s: &mut Box<dyn Session>, target: &str) -> String {
    let out = run(s, &format!("GET /{target}/_count")).await;
    let cell = &out.results[0].rows[0][0];
    let j: serde_json::Value = match cell {
        serde_json::Value::String(t) => serde_json::from_str(t).unwrap(),
        v => v.clone(),
    };
    j["count"].to_string()
}

fn request(kind: &str, name: &str, new: &str, definition: Option<String>) -> RenameRequest {
    RenameRequest {
        target: RenameTarget::Object { object: ObjectRef { kind: kind.into(), schema: None, name: name.into() }, parent: None },
        new_name: new.into(),
        table: None,
        definition,
    }
}

async fn rename(d: &dyn dbine_driver::Driver, s: &mut Box<dyn Session>, kind: &str, name: &str, new: &str) {
    let def = s.definition(&ObjectRef { kind: kind.into(), schema: None, name: name.into() }).await.unwrap();
    let script = d.rename_script(&request(kind, name, new, def)).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(s, st).await;
    }
}

async fn check(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    let cfg = ConnectionConfig { driver: id.into(), host: url, ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, SEED).await;

    rename(d.as_ref(), &mut s, kinds::INDEX, "dbine_rn", "dbine_rn2").await;
    let names: Vec<String> = s.list_objects().await.unwrap().into_iter().filter(|o| o.kind == kinds::INDEX).map(|o| o.name).collect();
    assert!(names.contains(&"dbine_rn2".to_string()) && !names.contains(&"dbine_rn".to_string()), "{names:?}");
    assert_eq!(count(&mut s, "dbine_rn2").await, "2");
    // The filtered alias still filters; the write alias still takes writes.
    assert_eq!(count(&mut s, "dbine_rn_f").await, "1");
    run(&mut s, "POST /dbine_rn_w/_doc?refresh=true\n{\"x\": \"3\"}").await;
    assert_eq!(count(&mut s, "dbine_rn2").await, "3");
    let def = s.definition(&ObjectRef { kind: kinds::INDEX.into(), schema: None, name: "dbine_rn2".into() }).await.unwrap().unwrap();
    assert!(def.contains("\"is_write_index\" : true") || def.contains("\"is_write_index\": true"), "{def}");

    // An alias on two indices, the write flag kept on the right one.
    rename(d.as_ref(), &mut s, dbine_driver_elasticsearch::KIND_ALIAS, "dbine_rn_all", "dbine_rn_todo").await;
    assert_eq!(count(&mut s, "dbine_rn_todo").await, "3");
    run(&mut s, "POST /dbine_rn_todo/_doc?refresh=true\n{\"x\": \"4\"}").await;
    assert_eq!(count(&mut s, "dbine_rn2").await, "4");
    let mut out = QueryOutcome::default();
    assert!(s.execute("GET /dbine_rn_all/_count", 100, &mut out).await.is_err());

    // Fields aren't renamed.
    let col = RenameRequest {
        target: RenameTarget::Column { table: ObjectRef { kind: kinds::INDEX.into(), schema: None, name: "dbine_rn2".into() }, column: "x".into() },
        new_name: "y".into(),
        table: None,
        definition: None,
    };
    assert!(d.rename_script(&col).unwrap_err().to_string().contains("reindexar"));

    run(&mut s, "DELETE /dbine_rn,dbine_rn2,dbine_rn_b?ignore_unavailable=true").await;
}

#[tokio::test]
#[ignore]
async fn elasticsearch() {
    check("elasticsearch", "DBINE_TEST_ELASTICSEARCH_URL").await;
}

#[tokio::test]
#[ignore]
async fn opensearch() {
    check("opensearch", "DBINE_TEST_OPENSEARCH_URL").await;
}

#[tokio::test]
#[ignore]
async fn opendistro() {
    check("opendistro", "DBINE_TEST_OPENDISTRO_URL").await;
}
