//! Against real servers (ignored by default):
//!
//! ```sh
//! docker run -d --name dbine-test-elasticsearch -p 25520:9200 -e discovery.type=single-node \
//!   -e xpack.security.enabled=false -e "ES_JAVA_OPTS=-Xms512m -Xmx512m" \
//!   docker.elastic.co/elasticsearch/elasticsearch:8.15.3
//! docker run -d --name dbine-test-opensearch -p 25521:9200 -e discovery.type=single-node \
//!   -e DISABLE_SECURITY_PLUGIN=true -e DISABLE_INSTALL_DEMO_CONFIG=true \
//!   -e "OPENSEARCH_JAVA_OPTS=-Xms512m -Xmx512m" opensearchproject/opensearch:2.17.1
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//!   cargo test -p dbine-driver-elasticsearch -- --ignored
//! ```

use dbine_driver::{ColumnDef, ConnectionConfig, DdlParts, Driver, Error, ObjectRef, QueryOutcome, Session, TableSchema};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

const INDEX: &str = "dbine_books";

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn session(id: &str, url: &str, read_only: bool) -> Box<dyn Session> {
    let cfg = ConnectionConfig { driver: id.into(), host: url.into(), read_only, ..Default::default() };
    driver(id).connect(&cfg, None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, text: &str, max_rows: usize) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.execute(text, max_rows, &mut out).await.map(|_| out)
}

fn col_names(out: &QueryOutcome, i: usize) -> Vec<String> {
    out.results[i].columns.iter().map(|c| c.name.clone()).collect()
}

const SEED: &str = r#"
# fresh index with a mapping, five books and an alias
PUT /dbine_books
{
  "mappings": {
    "properties": {
      "title":  { "type": "text", "fields": { "keyword": { "type": "keyword" } } },
      "genre":  { "type": "keyword" },
      "year":   { "type": "integer" },
      "author": { "properties": { "name": { "type": "keyword" } } }
    }
  }
}

POST /_bulk?refresh=true
{"index":{"_index":"dbine_books","_id":"1"}}
{"title":"Dune","genre":"scifi","year":1965,"author":{"name":"Herbert"}}
{"index":{"_index":"dbine_books","_id":"2"}}
{"title":"Neuromancer","genre":"scifi","year":1984,"author":{"name":"Gibson"}}
{"index":{"_index":"dbine_books","_id":"3"}}
{"title":"Emma","genre":"classic","year":1815,"author":{"name":"Austen"}}
{"index":{"_index":"dbine_books","_id":"4"}}
{"title":"Ulysses","genre":"classic","year":1922,"author":{"name":"Joyce"}}
{"index":{"_index":"dbine_books","_id":"5"}}
{"title":"Solaris","genre":"scifi","year":1961,"author":{"name":"Lem"}}

POST /_aliases
{"actions":[{"add":{"index":"dbine_books","alias":"dbine_books_alias"}}]}
"#;

async fn exercise(id: &str, url: &str) {
    let mut s = session(id, url, false).await;
    let version = s.server_version().await.unwrap();
    println!("{id}: {version}");
    let _ = run(&mut s, &format!("DELETE /{INDEX}"), 10).await;
    run(&mut s, SEED, 10).await.expect("seed");

    // Explorer.
    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.name == INDEX && o.kind == "index"), "{objs:?}");
    assert!(objs.iter().any(|o| o.name == "dbine_books_alias" && o.kind == "alias"), "{objs:?}");
    assert!(!objs.iter().any(|o| o.name.starts_with('.')), "system indices hidden");
    let obj = ObjectRef { kind: "index".into(), schema: None, name: INDEX.into() };
    let cols: Vec<String> = s.columns(&obj).await.unwrap().into_iter().map(|c| format!("{}:{}", c.name, c.data_type)).collect();
    for want in ["title:text", "title.keyword:keyword", "author:object", "author.name:keyword", "year:integer"] {
        assert!(cols.contains(&want.to_string()), "{want} in {cols:?}");
    }
    let def = s.definition(&obj).await.unwrap().unwrap();
    assert!(def.contains("\"mappings\"") && def.contains("\"settings\""), "{def}");

    // Browse.
    let q = s.browse_query(&obj, 100);
    let out = run(&mut s, &q, 100).await.unwrap();
    assert_eq!(out.results.len(), 1);
    assert_eq!(out.results[0].rows.len(), 5);
    let names = col_names(&out, 0);
    assert_eq!(&names[..6], ["_index", "_id", "_score", "title", "genre", "year"]);
    assert!(out.results[0].rows[0][6].as_str().unwrap().starts_with("{\"name\""), "nested as JSON string");

    // max_rows.
    let out = run(&mut s, &q, 2).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.results[0].total_rows, 5);
    assert!(out.results[0].truncated);

    // Aggregations + several requests in one script.
    let out = run(
        &mut s,
        "GET /dbine_books/_search\n{\"size\":0,\"aggs\":{\"g\":{\"terms\":{\"field\":\"genre\"},\"aggs\":{\"y\":{\"avg\":{\"field\":\"year\"}}}},\"maxy\":{\"max\":{\"field\":\"year\"}}}}\n\nGET /_cat/indices/dbine_*\n\nGET /dbine_books/_count",
        100,
    )
    .await
    .unwrap();
    assert_eq!(col_names(&out, 0), ["key", "doc_count", "y"]);
    assert_eq!(out.results[0].rows[0][0], "scifi");
    assert_eq!(out.results[0].rows[0][1], 3);
    assert_eq!(col_names(&out, 1), ["aggregation", "value"]);
    assert!(col_names(&out, 2).contains(&"index".to_string()), "_cat as rows");
    assert_eq!(out.results[2].rows.len(), 1);
    assert_eq!(col_names(&out, 3), ["response"]);

    // _msearch (NDJSON) and _mget.
    let out = run(
        &mut s,
        "GET /dbine_books/_msearch\n{}\n{\"query\":{\"term\":{\"genre\":\"classic\"}}}\n{}\n{\"query\":{\"match_all\":{}},\"size\":1}\n\nGET /dbine_books/_mget\n{\"ids\":[\"1\",\"3\"]}",
        100,
    )
    .await
    .unwrap();
    assert_eq!(out.results.len(), 3);
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.results[1].rows.len(), 1);
    assert_eq!(out.results[2].rows.len(), 2);

    // SQL, with the cursor closed after max_rows.
    let out = run(&mut s, "SELECT title, year FROM dbine_books ORDER BY year", 2).await.unwrap();
    let r = &out.results[0];
    assert_eq!(col_names(&out, 0), ["title", "year"]);
    assert_eq!(r.rows[0][0], "Emma");
    assert_eq!(r.rows.len(), 2);
    assert!(r.truncated, "{r:?}");
    let out = run(&mut s, "SELECT genre, COUNT(*) AS n FROM dbine_books GROUP BY genre;\nSHOW TABLES", 100).await.unwrap();
    assert_eq!(out.results.len(), 2);
    assert_eq!(out.results[0].rows.len(), 2);
    assert!(!out.results[1].rows.is_empty());

    // Errors: the earlier results stay, the message is the server's.
    let mut out = QueryOutcome::default();
    let e = s.execute("GET /dbine_books/_count\n\nGET /no_such_index/_search", 10, &mut out).await.unwrap_err();
    assert!(e.is_query() && e.to_string().contains("index_not_found"), "{e:?}");
    // With its code and place: the second request, on line 3.
    let se = e.to_script_error();
    assert_eq!((se.code.as_deref(), se.line, se.offset), (Some("index_not_found_exception"), Some(3), Some(25)), "{se:?}");
    assert_eq!(out.results.len(), 1);
    let e = run(&mut s, "SELECT nope FROM dbine_books", 10).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");

    // Interrupter: nothing running, must not panic.
    (s.interrupter().unwrap())();

    // Read-only.
    let mut ro = session(id, url, true).await;
    let out = run(&mut ro, "POST /dbine_books/_search\n{\"size\":1}", 10).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);
    run(&mut ro, "SELECT title FROM dbine_books", 10).await.unwrap();
    for w in ["DELETE /dbine_books", "POST /dbine_books/_doc\n{\"a\":1}", "PUT /x", "POST /dbine_books/_delete_by_query\n{}"] {
        let e = run(&mut ro, w, 10).await.unwrap_err();
        assert!(e.is_query() && e.to_string().contains("solo lectura"), "{w}: {e:?}");
    }

    run(&mut s, "DELETE /dbine_books", 10).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn elasticsearch() {
    let url = std::env::var("DBINE_TEST_ELASTICSEARCH_URL").expect("DBINE_TEST_ELASTICSEARCH_URL");
    exercise("elasticsearch", &url).await;
}

#[tokio::test]
#[ignore]
async fn opensearch() {
    let url = std::env::var("DBINE_TEST_OPENSEARCH_URL").expect("DBINE_TEST_OPENSEARCH_URL");
    exercise("opensearch", &url).await;
}

fn col(name: &str, ty: &str, opts: &[(&str, &str)]) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        options: opts.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    }
}

/// Designer DDL, database_schema, insert scripts and create templates,
/// everything through `execute`.
async fn designer_on(id: &str, url: &str) {
    let d = driver(id);
    let os = id == "opensearch";
    let mut s = session(id, url, false).await;
    for req in cleanup(os).split("\n\n") {
        let _ = run(&mut s, req, 10).await;
    }
    let caps = d.capabilities();
    assert!(!caps.create_database && !caps.drop_database);
    let spec = d.designer().unwrap();
    assert_eq!((spec.kind, spec.label), ("index", "Nuevo índice"));

    let vec_type = if os { "knn_vector" } else { "dense_vector" };
    let mut table = TableSchema {
        kind: "index".into(),
        name: "ddltest_books".into(),
        comment: Some("Libros".into()),
        columns: vec![
            col("title", "text", &[("analyzer", "english"), ("extra", r#"{"fields":{"raw":{"type":"keyword"}}}"#)]),
            col("author.name", "keyword", &[("doc_values", "false")]),
            col("year", "integer", &[]),
            col("published", "date", &[("format", "yyyy-MM-dd")]),
            col("price", "scaled_float", &[("scaling_factor", "100")]),
            col("tags", "nested", &[]),
            col("tags.label", "keyword", &[]),
            col("vec", vec_type, &[("dims", "3")]),
        ],
        options: [("number_of_shards", "1"), ("number_of_replicas", "0"), ("aliases", "ddltest_alias"), ("dynamic", "strict")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    };
    table.columns[2].comment = Some("Año".into());
    if os {
        table.options.insert("knn".into(), "true".into());
    }
    let all = DdlParts { drop: true, if_exists: true, create: true, ..Default::default() };
    let text = d.table_ddl(&table, all).unwrap();
    run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{text}\n{e}"));
    // Guarded drop of an index that isn't there.
    run(&mut s, "DELETE /ddltest_nope?ignore_unavailable=true", 10).await.unwrap();

    let schema = s.database_schema().await.unwrap();
    let got = schema.iter().find(|t| t.name == "ddltest_books").expect("index in database_schema");
    let cols: Vec<String> = got.columns.iter().map(|c| format!("{}:{}", c.name, c.data_type)).collect();
    for c in ["title:text", "author:object", "author.name:keyword", "year:integer", "tags:nested", "tags.label:keyword"] {
        assert!(cols.contains(&c.to_string()), "{c} in {cols:?}");
    }
    assert!(cols.contains(&format!("vec:{vec_type}")), "{cols:?}");
    let o = |k: &str| got.options.get(k).cloned().unwrap_or_default();
    assert_eq!((o("number_of_shards"), o("number_of_replicas"), o("aliases"), o("dynamic")), ("1".into(), "0".into(), "ddltest_alias".into(), "strict".into()));
    assert_eq!(got.comment.as_deref(), Some("Libros"));
    assert_eq!(got.columns.iter().find(|c| c.name == "year").unwrap().comment.as_deref(), Some("Año"));
    let title = got.columns.iter().find(|c| c.name == "title").unwrap();
    assert_eq!(title.options.get("analyzer").map(String::as_str), Some("english"));
    assert!(title.options.get("extra").unwrap().contains("raw"));

    // Round trip: the index read back, recreated under another name.
    let mut copy = got.clone();
    copy.name = "ddltest_copy".into();
    copy.options.remove("aliases");
    let text = d.table_ddl(&copy, all).unwrap();
    run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{text}\n{e}"));
    let again = s.database_schema().await.unwrap();
    let copied = again.iter().find(|t| t.name == "ddltest_copy").unwrap();
    assert_eq!(copied.columns, got.columns);

    // Insert script: nested values as JSON text, `_id` to the action line.
    let target = ObjectRef { kind: "index".into(), schema: None, name: "ddltest_books".into() };
    let columns: Vec<String> = ["_id", "title", "author", "year", "tags", "vec"].iter().map(|c| c.to_string()).collect();
    let rows = vec![
        vec![json!("a"), json!("Dune"), json!(r#"{"name":"Herbert"}"#), json!(1965), json!(r#"[{"label":"scifi"}]"#), json!("[1,0,0]")],
        vec![json!("b"), json!("Emma"), json!(r#"{"name":"Austen"}"#), json!(1815), serde_json::Value::Null, json!([0, 1, 0])],
    ];
    let text = d.insert_script(&target, &columns, &rows).unwrap();
    run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{text}\n{e}"));
    let out = run(&mut s, "GET /ddltest_books/_search\n{\"query\": {\"term\": {\"author.name\": \"Herbert\"}}}", 10).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);
    let out = run(&mut s, "GET /ddltest_books/_doc/b", 10).await.unwrap();
    assert!(format!("{:?}", out.results).contains("Austen"));
    // Strict mapping: a bad row makes the script fail.
    let bad = d.insert_script(&target, &["nope".to_string()], &[vec![json!(1)]]).unwrap();
    let e = run(&mut s, &bad, 10).await.unwrap_err();
    assert!(e.is_query() && e.to_string().starts_with("_bulk: fallaron 1 de 1"), "{e:?}");

    // Create templates, each run as is.
    for t in d.create_templates() {
        let text = t.template.replace("{name}", &format!("ddltest-{}", t.kind.replace('_', "-")));
        let text = text.replace("mi_indice", "ddltest_books");
        run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{}: {e}\n{text}", t.label));
    }
    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == "stream" && o.name == "ddltest-stream"), "{objs:?}");
    assert!(objs.iter().any(|o| o.kind == "alias" && o.name == "ddltest-alias"), "{objs:?}");
    let ds = ObjectRef { kind: "stream".into(), schema: None, name: "ddltest-stream".into() };
    let text = d.insert_script(&ds, &["@timestamp".to_string(), "message".to_string()], &[vec![json!("2024-05-01T00:00:00Z"), json!("hola")]]).unwrap();
    run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{text}\n{e}"));
    let out = run(&mut s, "GET /ddltest-stream/_count", 10).await.unwrap();
    assert!(format!("{:?}", out.results).contains(r#"\"count\": 1"#), "{:?}", out.results);

    run(&mut s, &cleanup(os), 10).await.unwrap();
}

fn cleanup(os: bool) -> String {
    let policy = if os { "DELETE /_plugins/_ism/policies/ddltest-policy" } else { "DELETE /_ilm/policy/ddltest-policy" };
    format!(
        "DELETE /_data_stream/ddltest-stream\n\nDELETE /_index_template/ddltest-stream-template\n\nDELETE /_index_template/ddltest-index-template\n\nDELETE /_ingest/pipeline/ddltest-pipeline\n\n{policy}\n\nDELETE /ddltest_books\n\nDELETE /ddltest_copy"
    )
}

#[tokio::test]
#[ignore]
async fn designer_and_scripts() {
    if let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_URL") {
        designer_on("elasticsearch", &url).await;
    }
    if let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") {
        designer_on("opensearch", &url).await;
    }
}

#[tokio::test]
#[ignore]
async fn bad_host_is_a_connect_error() {
    let cfg = ConnectionConfig { driver: "elasticsearch".into(), host: "127.0.0.1".into(), port: 1, ..Default::default() };
    let e = driver("elasticsearch").connect(&cfg, None).await.err().unwrap();
    assert!(matches!(e, Error::Connect(_)), "{e:?}");
}

fn dump(n: &dbine_driver::PlanNode, depth: usize) {
    eprintln!("{}{} [{}] {:?} rows={:?} ms={:?} {:?}", "  ".repeat(depth), n.op, n.detail, n.object, n.actual_rows, n.actual_ms, n.warnings);
    for c in &n.children {
        dump(c, depth + 1);
    }
}

async fn plans_on(id: &str, url: &str) {
    assert!(driver(id).supports_explain());
    let mut s = session(id, url, false).await;
    run(&mut s, "DELETE /dbine_plan", 5).await.ok();
    run(&mut s, "PUT /dbine_plan\n{\"mappings\":{\"properties\":{\"title\":{\"type\":\"text\"},\"year\":{\"type\":\"integer\"},\"genre\":{\"type\":\"keyword\"}}}}", 5)
        .await
        .unwrap();
    run(
        &mut s,
        "POST /dbine_plan/_bulk?refresh=true\n{\"index\":{}}\n{\"title\":\"star wars\",\"year\":1977,\"genre\":\"scifi\"}\n{\"index\":{}}\n{\"title\":\"star trek\",\"year\":1979,\"genre\":\"scifi\"}\n{\"index\":{}}\n{\"title\":\"alien\",\"year\":1979,\"genre\":\"horror\"}",
        5,
    )
    .await
    .unwrap();
    let script = "GET /dbine_plan/_search\n{\"query\":{\"bool\":{\"must\":[{\"match\":{\"title\":\"star\"}}],\"filter\":[{\"range\":{\"year\":{\"gte\":1970}}}],\"must_not\":[{\"wildcard\":{\"title\":\"*x\"}}]}},\"aggs\":{\"g\":{\"terms\":{\"field\":\"genre\"}}}}\n\n\
                  PUT /dbine_plan/_doc/x1\n{\"title\":\"new\"}\n\n\
                  SELECT title, year FROM dbine_plan WHERE year > 1978 ORDER BY year";

    // Estimated: nothing runs (the DELETE neither).
    let mut out = QueryOutcome::default();
    s.explain(script, false, 50, &mut out).await.unwrap();
    assert!(out.results.is_empty(), "{:?}", out.results);
    assert_eq!(out.plans.len(), 2, "{:?}", out.messages);
    assert!(out.messages.iter().any(|m| m.contains("PUT")));
    assert!(run(&mut s, "GET /dbine_plan/_doc/x1", 5).await.is_err(), "the PUT ran");
    for p in &out.plans {
        eprintln!("--- {id} estimated: {}", p.statement);
        dump(&p.root, 0);
    }
    let q = &out.plans[0].root.children[0];
    assert_eq!(q.op, "BooleanQuery");
    assert!(format!("{q:?}").contains("Comodín inicial"));

    // Actual: results + profile.
    let mut out = QueryOutcome::default();
    s.explain(script, true, 50, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.plans.len(), 2);
    assert!(out.plans.iter().all(|p| p.actual));
    for p in &out.plans {
        eprintln!("--- {id} actual: {}", p.statement);
        dump(&p.root, 0);
    }
    let shard = &out.plans[0].root.children[0];
    assert_eq!(shard.op, "Shard");
    assert!(format!("{shard:?}").contains("TermQuery"));
    assert_eq!(out.plans[0].root.actual_rows, Some(2.0));
    run(&mut s, "DELETE /dbine_plan", 5).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn explain_plans() {
    if let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_URL") {
        plans_on("elasticsearch", &url).await;
    }
    if let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") {
        plans_on("opensearch", &url).await;
    }
}

/// Open Distro for Elasticsearch 1.13 (ES OSS 7.10.2, amd64 only):
/// `docker run -d --name dbine-test-opendistro -p 25524:9200 -e discovery.type=single-node
///   -e opendistro_security.disabled=true -e "ES_JAVA_OPTS=-Xms512m -Xmx512m" amazon/opendistro-for-elasticsearch:1.13.3`,
/// then `DBINE_TEST_OPENDISTRO_URL=http://localhost:25524`.
#[tokio::test]
#[ignore]
async fn opendistro() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENDISTRO_URL") else { return };
    exercise("opendistro", &url).await;
    let mut s = session("opendistro", &url, false).await;
    assert!(s.server_version().await.unwrap().starts_with("Open Distro"));
    let mut out = QueryOutcome::default();
    s.explain("SELECT 1", false, 10, &mut out).await.ok();
}

async fn check_monitor(id: &str, url: &str) {
    assert!(driver(id).capabilities().monitor);
    let mut s = session(id, url, false).await;
    let snap = s.monitor().await.expect("monitor");
    for m in &snap.metrics {
        eprintln!("{:<22} {:?} max={:?} counter={}", m.key, m.value, m.max, m.counter);
    }
    for t in &snap.tables {
        eprintln!("table {} rows={}", t.key, t.rows.len());
    }
    eprintln!("info {:?}\nnotes {:?}", snap.info, snap.notes);
    let has = |k: &str| snap.metrics.iter().any(|m| m.key == k && m.value.is_some());
    for k in ["cpu", "cpu_time", "mem_used", "heap_used", "connections", "queries", "storage_used", "uptime", "nodes"] {
        assert!(has(k), "{id}: {k}");
    }
    let table = |k: &str| snap.tables.iter().find(|t| t.key == k);
    assert!(table("nodes").is_some_and(|t| !t.rows.is_empty()), "{id}: nodes");
    assert!(table("queries").is_some(), "{id}: tasks");
    assert!(table("databases").is_some(), "{id}: indices");
    assert!(snap.notes.iter().all(|n| !n.contains("No se pudieron leer")), "{:?}", snap.notes);
}

#[tokio::test]
#[ignore]
async fn monitor() {
    for (id, var) in [
        ("elasticsearch", "DBINE_TEST_ELASTICSEARCH_URL"),
        ("opensearch", "DBINE_TEST_OPENSEARCH_URL"),
        ("opendistro", "DBINE_TEST_OPENDISTRO_URL"),
    ] {
        if let Ok(url) = std::env::var(var) {
            check_monitor(id, &url).await;
        }
    }
}

// -- profiler -----------------------------------------------------------------------------

/// The profiler sees another session's slow search (with its duration) once,
/// and leaves out its own requests.
async fn profile(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    assert!(driver(id).supports_profiler(), "{id}");
    let mut p = session(id, &url, true).await;
    let mut w = session(id, &url, false).await;
    let mut seed = String::from("POST /_bulk?refresh=true\n");
    for n in 0..2000 {
        seed.push_str(&format!("{{\"index\":{{\"_index\":\"dbine_prof\"}}}}\n{{\"n\":{n}}}\n"));
    }
    let _ = run(&mut w, "DELETE /dbine_prof", 10).await;
    run(&mut w, &seed, 10).await.expect("seed");

    let opts = dbine_driver::ProfilerOptions { database: "default".into(), change_server: false };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{id}: {started:?}");
    let marker = format!("dbine_prof_{}", std::process::id());
    // A script query that takes a while on 2000 documents.
    let slow = format!(
        "GET /dbine_prof/_search?size=0\n{{\"query\":{{\"script\":{{\"script\":{{\"source\":\"double x=0; for (int i=0;i<20000;i++){{x+=Math.sin(i+x);}} return x>0;\",\"params\":{{\"m\":\"{marker}\"}}}}}}}}}}"
    );
    let work = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let t = Instant::now();
        run(&mut w, &slow, 10).await.expect("slow");
        eprintln!("{id}: slow search took {:?}", t.elapsed());
    };
    let watch = async {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(30);
        while Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            if got.iter().any(|s| s.text.contains(&marker)) {
                break;
            }
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{id}: {mine:#?}");
    assert_eq!(mine.len(), 1, "{id}: the slow search once");
    assert!(mine[0].text.starts_with("GET /dbine_prof/_search"), "{id}: {}", mine[0].text);
    assert_eq!(mine[0].database.as_deref(), Some("dbine_prof"));
    assert!(mine[0].duration_ms.unwrap_or(0.0) >= 200.0, "{id}: duration {:?}", mine[0].duration_ms);
    // Only OpenSearch reports a task's CPU time.
    assert_eq!(mine[0].cpu_ms.is_some_and(|ms| ms > 0.0), id == "opensearch", "{id}: CPU {:?}", mine[0].cpu_ms);
    assert!(got.iter().all(|s| !s.text.contains("_tasks")), "{id}: its own requests are left out");
    run(&mut w, "DELETE /dbine_prof", 10).await.expect("cleanup");
}

#[tokio::test]
#[ignore]
async fn elasticsearch_profiler() {
    profile("elasticsearch", "DBINE_TEST_ELASTICSEARCH_URL").await;
}

#[tokio::test]
#[ignore]
async fn opensearch_profiler() {
    profile("opensearch", "DBINE_TEST_OPENSEARCH_URL").await;
}

#[tokio::test]
#[ignore]
async fn opendistro_profiler() {
    profile("opendistro", "DBINE_TEST_OPENDISTRO_URL").await;
}

/// The data-compare delete script removes exactly the keyed documents
/// (ids with quotes, spaces and a slash).
#[tokio::test]
#[ignore]
async fn delete_script_runs() {
    let url = std::env::var("DBINE_TEST_ELASTICSEARCH_URL").expect("DBINE_TEST_ELASTICSEARCH_URL");
    let mut s = session("elasticsearch", &url, false).await;
    run(&mut s, "DELETE /dbine_del", 10).await.ok();
    let obj = ObjectRef { kind: dbine_driver::kinds::INDEX.into(), schema: None, name: "dbine_del".into() };
    let d = driver("elasticsearch");
    let ins = d
        .insert_script(&obj, &["_id".to_string(), "n".into()], &[vec![json!("O'Brien \"Bob\" a/b"), json!(1)], vec![json!("b"), json!(2)], vec![json!("keep"), json!(3)]])
        .unwrap();
    run(&mut s, &ins, 10).await.expect(&ins);
    let keys = vec![vec![("_id".to_string(), json!("O'Brien \"Bob\" a/b"))], vec![("_id".to_string(), json!("b"))]];
    let script = d.delete_script(&obj, &keys).unwrap();
    run(&mut s, &script, 10).await.expect(&script);
    let out = run(&mut s, "GET /dbine_del/_search\n{\"query\": {\"match_all\": {}}}", 10).await.unwrap();
    let rows = serde_json::to_string(&out.results[0].rows).unwrap();
    assert_eq!(out.results[0].rows.len(), 1, "{rows}");
    assert!(rows.contains("keep"), "{rows}");
    run(&mut s, "DELETE /dbine_del", 10).await.unwrap();
}
