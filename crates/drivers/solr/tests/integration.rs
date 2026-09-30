//! Against real servers (ignored by default):
//!
//! ```sh
//! docker run -d --name dbine-test-solr -p 25522:8983 solr:9 solr-precreate testcore
//! docker run -d --name dbine-test-solrcloud -p 25523:8983 -e SOLR_MODULES=sql solr:9 solr-foreground -c
//! DBINE_TEST_SOLR_URL=http://localhost:25522 DBINE_TEST_SOLRCLOUD_URL=http://localhost:25523 \
//!   cargo test -p dbine-driver-solr -- --ignored
//! ```
//! The standalone server needs a core named `testcore`; the SolrCloud test
//! creates (and drops) a `dbine_books` collection.
//!
//! `designer_and_scripts` creates and drops its own `ddltest_*` collections
//! on either server. A standalone server loads configsets from
//! `<SOLR_HOME>/configsets` (the Docker image leaves it empty) and its cores
//! share the configset's managed schema, so the test gives each core its
//! own copy of `_default`, made fresh before every run:
//!
//! ```sh
//! docker exec <container> sh -c 'rm -rf /var/solr/data/configsets && mkdir /var/solr/data/configsets &&
//!   for c in _default ddltest_books ddltest_copy ddltest_tpl; do
//!     cp -r /opt/solr/server/solr/configsets/_default /var/solr/data/configsets/$c; done'
//! ```
//!
//! `explain_plans` empties and fills its own core: `DBINE_TEST_SOLR_PLAN_URL`
//! (a server with `solr-precreate films`; `DBINE_TEST_SOLR_PLAN_CORE` overrides
//! the core name).

use dbine_driver::{ColumnDef, ConnectionConfig, DdlParts, Error, ObjectRef, QueryOutcome, Session, TableSchema};
use serde_json::json;

async fn session(url: &str, read_only: bool) -> Box<dyn Session> {
    let cfg = ConnectionConfig { driver: "solr".into(), host: url.into(), read_only, ..Default::default() };
    dbine_driver_solr::drivers()[0].connect(&cfg, None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, text: &str, max_rows: usize) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.execute(text, max_rows, &mut out).await.map(|_| out)
}

fn col_names(out: &QueryOutcome, i: usize) -> Vec<String> {
    out.results[i].columns.iter().map(|c| c.name.clone()).collect()
}

fn seed(core: &str) -> String {
    format!(
        r#"POST /solr/{core}/update?commit=true
{{"delete": {{"query": "*:*"}}}}

POST /solr/{core}/update?commit=true
[
  {{"id": "1", "title_s": "Dune", "genre_s": "scifi", "year_i": 1965}},
  {{"id": "2", "title_s": "Neuromancer", "genre_s": "scifi", "year_i": 1984}},
  {{"id": "3", "title_s": "Emma", "genre_s": "classic", "year_i": 1815}},
  {{"id": "4", "title_s": "Ulysses", "genre_s": "classic", "year_i": 1922}},
  {{"id": "5", "title_s": "Solaris", "genre_s": "scifi", "year_i": 1961}}
]"#
    )
}

async fn exercise(s: &mut Box<dyn Session>, url: &str, core: &str) {
    run(s, &seed(core), 10).await.expect("seed");

    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.name == core && o.kind == "collection"), "{objs:?}");
    let obj = ObjectRef { kind: "collection".into(), schema: None, name: core.into() };
    let cols = s.columns(&obj).await.unwrap();
    let id = cols.iter().find(|c| c.name == "id").expect("id field");
    assert!(id.primary_key && !id.nullable);
    assert!(cols.iter().any(|c| c.name == "year_i" && c.data_type == "pint"), "dynamic instance from luke: {cols:?}");
    let def = s.definition(&obj).await.unwrap().unwrap();
    assert!(def.contains("\"fieldTypes\"") && def.contains("\"uniqueKey\""));

    // Browse and max_rows.
    let q = s.browse_query(&obj, 100);
    let out = run(s, &q, 100).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 5);
    assert_eq!(&col_names(&out, 0)[..4], ["id", "title_s", "genre_s", "year_i"]);
    let out = run(s, &q, 2).await.unwrap();
    assert_eq!((out.results[0].rows.len(), out.results[0].total_rows, out.results[0].truncated), (2, 5, true));

    // Facets, JSON request API (POST, no /solr prefix), several requests.
    let script = format!(
        "GET /{core}/select?q=*:*&rows=0&facet=true&facet.field=genre_s&facet.query=year_i:[1900 TO *]\n\n\
         POST /{core}/query\n{{\"query\": \"genre_s:scifi\", \"limit\": 10, \"facet\": {{\"g\": {{\"type\": \"terms\", \"field\": \"genre_s\"}}, \"avg_year\": \"avg(year_i)\"}}}}"
    );
    let out = run(s, &script, 100).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 0, "rows=0 hits table");
    assert_eq!(col_names(&out, 1), ["field", "value", "count"]);
    assert_eq!(out.results[1].rows[0], vec![serde_json::json!("genre_s"), serde_json::json!("scifi"), serde_json::json!(3)]);
    assert_eq!(col_names(&out, 2), ["query", "count"]);
    assert_eq!(out.results[3].rows.len(), 3, "scifi docs");
    assert_eq!(col_names(&out, 4), ["val", "count"]);
    assert_eq!(col_names(&out, 5), ["facet", "value"]);

    // Errors keep what ran before.
    let mut out = QueryOutcome::default();
    let e = s.execute(&format!("GET /{core}/select?q=*:*\n\nGET /{core}/select?q=*:*&sort=nope asc"), 10, &mut out).await.unwrap_err();
    assert!(matches!(&e, Error::Query(m) if m.contains("nope")), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // Read-only.
    let mut ro = session(url, true).await;
    run(&mut ro, &format!("GET /{core}/select?q=id:1"), 10).await.unwrap();
    run(&mut ro, "GET /solr/admin/info/system", 10).await.unwrap();
    for w in [format!("POST /{core}/update?commit=true\n[{{\"id\":\"9\"}}]"), format!("GET /{core}/update?commit=true"), format!("GET /admin/cores?action=UNLOAD&core={core}")] {
        let e = run(&mut ro, &w, 10).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("solo lectura")), "{w}: {e:?}");
    }
}

#[tokio::test]
#[ignore]
async fn solr_standalone() {
    let url = std::env::var("DBINE_TEST_SOLR_URL").expect("DBINE_TEST_SOLR_URL");
    let mut s = session(&url, false).await;
    let v = s.server_version().await.unwrap();
    println!("{v}");
    assert!(v.starts_with("Apache Solr 9") && !v.contains("Cloud"));
    exercise(&mut s, &url, "testcore").await;
    let e = run(&mut s, "SELECT id FROM testcore", 10).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
}

#[tokio::test]
#[ignore]
async fn solr_cloud() {
    let url = std::env::var("DBINE_TEST_SOLRCLOUD_URL").expect("DBINE_TEST_SOLRCLOUD_URL");
    let mut s = session(&url, false).await;
    let v = s.server_version().await.unwrap();
    println!("{v}");
    assert!(v.contains("SolrCloud"));
    let _ = run(&mut s, "GET /admin/collections?action=DELETE&name=dbine_books", 10).await;
    run(&mut s, "GET /admin/collections?action=CREATE&name=dbine_books&numShards=1&replicationFactor=1", 10).await.unwrap();
    exercise(&mut s, &url, "dbine_books").await;

    let out = run(&mut s, "SELECT id, year_i FROM dbine_books ORDER BY year_i ASC LIMIT 10", 100).await.unwrap();
    assert_eq!(col_names(&out, 0), ["id", "year_i"]);
    assert_eq!(out.results[0].rows.len(), 5);
    assert_eq!(out.results[0].rows[0][0], "3");
    let out = run(&mut s, "SELECT genre_s, count(*) AS n FROM dbine_books GROUP BY genre_s ORDER BY n DESC", 1).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);
    assert!(out.results[0].truncated);
    let e = run(&mut s, "SELECT nope_field FROM dbine_books", 10).await.unwrap_err();
    assert!(matches!(e, Error::Query(_)), "{e:?}");

    run(&mut s, "GET /admin/collections?action=DELETE&name=dbine_books", 10).await.unwrap();
}

fn dump(n: &dbine_driver::PlanNode, depth: usize) {
    eprintln!("{}{} [{}] {:?} rows={:?} ms={:?} {:?}", "  ".repeat(depth), n.op, n.detail, n.object, n.actual_rows, n.actual_ms, n.warnings);
    for c in &n.children {
        dump(c, depth + 1);
    }
}

#[tokio::test]
#[ignore]
async fn explain_plans() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLR_PLAN_URL") else { return };
    let core = std::env::var("DBINE_TEST_SOLR_PLAN_CORE").unwrap_or_else(|_| "films".into());
    let mut s = session(&url, false).await;
    run(&mut s, &format!("POST /{core}/update?commit=true\n{{\"delete\":{{\"query\":\"*:*\"}}}}"), 5).await.unwrap();
    run(
        &mut s,
        &format!("POST /{core}/update?commit=true\n[{{\"id\":\"1\",\"title_t\":\"star wars\",\"year_i\":1977,\"genre_s\":\"scifi\"}},{{\"id\":\"2\",\"title_t\":\"star trek\",\"year_i\":1979,\"genre_s\":\"scifi\"}},{{\"id\":\"3\",\"title_t\":\"alien\",\"year_i\":1979,\"genre_s\":\"horror\"}}]"),
        5,
    )
    .await
    .unwrap();
    let script = format!(
        "GET /{core}/select?q=title_t:star%20AND%20year_i:[1970%20TO%20*]&fq=genre_s:scifi&facet=true&facet.field=genre_s\n\
         POST /{core}/update?commit=true\n[{{\"id\":\"9\",\"title_t\":\"new\"}}]"
    );

    // Estimated: no documents, the update doesn't run.
    let mut out = QueryOutcome::default();
    s.explain(&script, false, 50, &mut out).await.unwrap();
    assert!(out.results.is_empty());
    assert_eq!(out.plans.len(), 1);
    assert!(out.messages.iter().any(|m| m.contains("update")));
    dump(&out.plans[0].root, 0);
    assert_eq!(out.plans[0].root.children[0].op, "BooleanQuery");
    let n = run(&mut s, &format!("GET /{core}/select?q=id:9"), 5).await.unwrap();
    assert_eq!(n.results[0].rows.len(), 0, "the update ran");

    // Actual: results + timing.
    let mut out = QueryOutcome::default();
    s.explain(&format!("GET /{core}/select?q=title_t:star%20AND%20year_i:[1970%20TO%20*]&fq=genre_s:scifi&facet=true&facet.field=genre_s"), true, 50, &mut out)
        .await
        .unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    let p = &out.plans[0];
    assert!(p.actual);
    dump(&p.root, 0);
    assert_eq!(p.root.actual_rows, Some(2.0));
    assert_eq!(p.root.children[1].op, "Process");
}

fn col(name: &str, ty: &str, opts: &[(&str, &str)]) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable: true,
        options: opts.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    }
}

/// Designer DDL (DBine's PUT / DELETE /solr/<name> plus add-field),
/// database_schema, insert scripts and create templates, all through
/// `execute`.
async fn designer_on(url: &str) {
    let d = &dbine_driver_solr::drivers()[0];
    let mut s = session(url, false).await;
    let cloud = s.server_version().await.unwrap().contains("SolrCloud");
    // Standalone: a configset per core (see the module docs).
    let config = |core: &str| if cloud { "_default".to_string() } else { core.to_string() };
    let name = "ddltest_books";
    let mut year = col("year", "pint", &[("docValues", "true")]);
    year.nullable = false;
    year.default_value = Some("2000".into());
    let table = TableSchema {
        kind: "collection".into(),
        name: name.into(),
        columns: vec![
            col("id", "string", &[]),
            col("title", "text_general", &[("stored", "true")]),
            year,
            col("tags", "strings", &[]),
            col("rating", "pdouble", &[("indexed", "false"), ("stored", "true")]),
        ],
        options: [("configSet".to_string(), config(name))].into_iter().collect(),
        ..Default::default()
    };
    let all = DdlParts { drop: true, if_exists: true, create: true, ..Default::default() };
    let text = d.table_ddl(&table, all).unwrap();
    run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{text}\n{e}"));
    // Guards: creating again is skipped, dropping a missing one too.
    run(&mut s, &format!("PUT /solr/{name}?if_not_exists=true"), 10).await.unwrap();
    assert!(run(&mut s, &format!("PUT /solr/{name}"), 10).await.is_err());
    run(&mut s, "DELETE /solr/ddltest_nope?if_exists=true", 10).await.unwrap();
    assert!(run(&mut s, "DELETE /solr/ddltest_nope", 10).await.is_err());

    let schema = s.database_schema().await.unwrap();
    let got = schema.iter().find(|t| t.name == name).expect("collection in database_schema");
    assert_eq!(got.primary_key.as_ref().unwrap().columns, ["id"]);
    let f = |n: &str| got.columns.iter().find(|c| c.name == n).unwrap_or_else(|| panic!("{n} in {:?}", got.columns));
    assert_eq!(f("title").data_type, "text_general");
    assert!(!f("year").nullable);
    assert_eq!(f("year").default_value.as_deref(), Some("2000"));
    assert_eq!(f("year").options.get("docValues").map(String::as_str), Some("true"));
    assert_eq!(f("rating").options.get("indexed").map(String::as_str), Some("false"));

    // Insert script: lists shown as JSON text become lists again.
    let target = ObjectRef { kind: "collection".into(), schema: None, name: name.into() };
    let columns: Vec<String> = ["id", "title", "year", "tags", "_version_"].iter().map(|c| c.to_string()).collect();
    let rows = vec![
        vec![json!("1"), json!("Dune"), json!(1965), json!(r#"["scifi","classic"]"#), json!(123)],
        vec![json!("2"), json!("Emma"), json!(1815), serde_json::Value::Null, serde_json::Value::Null],
    ];
    let text = d.insert_script(&target, &columns, &rows).unwrap();
    run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{text}\n{e}"));
    let out = run(&mut s, &format!("GET /solr/{name}/select?q=tags:classic&fl=id,tags"), 10).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1, "{:?}", out.results);
    let out = run(&mut s, &format!("GET /solr/{name}/select?q=*:*&rows=0"), 10).await.unwrap();
    assert!(out.messages[0].starts_with("2 documentos"), "{:?}", out.messages);

    // Round trip: the collection read back, recreated under another name.
    let mut copy = got.clone();
    copy.name = "ddltest_copy".into();
    copy.options.insert("configSet".into(), config("ddltest_copy"));
    let text = d.table_ddl(&copy, all).unwrap();
    run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{text}\n{e}"));
    let again = s.database_schema().await.unwrap();
    let copied = again.iter().find(|t| t.name == "ddltest_copy").unwrap();
    for c in ["title", "year", "tags", "rating"] {
        let a = got.columns.iter().find(|x| x.name == c).unwrap();
        let b = copied.columns.iter().find(|x| x.name == c).unwrap();
        assert_eq!(a, b);
    }

    // Create templates against the new collection.
    for t in d.create_templates() {
        if t.kind == "collection" {
            let text = t.template.replace("{name}", "ddltest_tpl").replace("\"_default\"", &format!("\"{}\"", config("ddltest_tpl")));
            run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{}: {e}\n{text}", t.label));
            continue;
        }
        let field = if t.kind == "copy_field" { "title".to_string() } else { format!("tpl_{}", t.kind) };
        let text = t.template.replace("{name}", &field).replace("mi_coleccion", name);
        run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{}: {e}\n{text}", t.label));
    }
    let out = run(&mut s, &format!("GET /solr/{name}/schema/fieldtypes/tpl_field_type"), 10).await;
    assert!(out.is_ok());

    for c in [name, "ddltest_copy", "ddltest_tpl"] {
        run(&mut s, &format!("DELETE /solr/{c}"), 10).await.unwrap();
    }
    // A configset that isn't there: an error, and no half-created core left.
    let e = run(&mut s, "PUT /solr/ddltest_bad\n{\"configSet\": \"ddltest_missing\"}", 10).await.unwrap_err();
    println!("{e}");
    let objs = s.list_objects().await.unwrap();
    assert!(!objs.iter().any(|o| o.name.starts_with("ddltest")), "{objs:?}");
    run(&mut s, "DELETE /solr/ddltest_bad?if_exists=true", 10).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn designer_and_scripts() {
    if let Ok(url) = std::env::var("DBINE_TEST_SOLR_URL") {
        designer_on(&url).await;
    }
    if let Ok(url) = std::env::var("DBINE_TEST_SOLRCLOUD_URL") {
        designer_on(&url).await;
    }
}

async fn check_monitor(url: &str, cloud: bool) {
    assert!(dbine_driver_solr::drivers()[0].capabilities().monitor);
    let mut s = session(url, false).await;
    let snap = s.monitor().await.expect("monitor");
    for m in &snap.metrics {
        eprintln!("{:<22} {:?} max={:?} counter={}", m.key, m.value, m.max, m.counter);
    }
    for t in &snap.tables {
        eprintln!("table {} rows={}", t.key, t.rows.len());
    }
    eprintln!("info {:?}\nnotes {:?}", snap.info, snap.notes);
    let has = |k: &str| snap.metrics.iter().any(|m| m.key == k && m.value.is_some());
    for k in ["cpu_time", "mem_used", "heap_used", "uptime", "queries", "storage_used", "disk_used", "gc_time", "active_sessions"] {
        assert!(has(k), "{k}");
    }
    let table = |k: &str| snap.tables.iter().find(|t| t.key == k);
    assert!(table("databases").is_some_and(|t| !t.rows.is_empty()));
    if cloud {
        assert!(table("nodes").is_some_and(|t| !t.rows.is_empty()));
        assert!(has("live_nodes"));
    }
    assert!(!snap.notes.iter().any(|n| n.starts_with("No se pudieron")), "{:?}", snap.notes);
}

#[tokio::test]
#[ignore]
async fn monitor() {
    if let Ok(url) = std::env::var("DBINE_TEST_SOLR_URL") {
        check_monitor(&url, false).await;
    }
    if let Ok(url) = std::env::var("DBINE_TEST_SOLRCLOUD_URL") {
        check_monitor(&url, true).await;
    }
}

/// Schema sync: a core read back from `database_schema` gets a field
/// added, dropped and redefined; another core is created. On a standalone
/// server it needs the `ddltest_sync` and `ddltest_syncnew` configsets (see
/// the module docs).
async fn schema_sync_on(url: &str) {
    use dbine_driver::TableChange;
    let d = &dbine_driver_solr::drivers()[0];
    assert!(d.supports_schema_sync());
    let mut s = session(url, false).await;
    let cloud = s.server_version().await.unwrap().contains("SolrCloud");
    let config = |core: &str| if cloud { "_default".to_string() } else { core.to_string() };
    for c in ["ddltest_sync", "ddltest_syncnew"] {
        run(&mut s, &format!("DELETE /solr/{c}?if_exists=true"), 10).await.unwrap();
    }
    let base = TableSchema {
        kind: "collection".into(),
        name: "ddltest_sync".into(),
        columns: vec![col("id", "string", &[]), col("title", "text_general", &[]), col("year", "pint", &[]), col("old", "string", &[])],
        options: [("configSet".to_string(), config("ddltest_sync"))].into_iter().collect(),
        ..Default::default()
    };
    let text = d.table_ddl(&base, DdlParts { create: true, ..Default::default() }).unwrap();
    run(&mut s, &text, 10).await.unwrap_or_else(|e| panic!("{text}\n{e}"));
    let old = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "ddltest_sync").unwrap();
    let mut new = old.clone();
    new.columns.retain(|c| c.name != "old");
    new.columns.iter_mut().find(|c| c.name == "year").unwrap().data_type = "plong".into();
    new.columns.push(col("genre", "string", &[("multiValued", "true")]));
    let mut created = base.clone();
    created.name = "ddltest_syncnew".into();
    created.options.insert("configSet".into(), config("ddltest_syncnew"));
    let script = d.sync_script(&[TableChange::Alter { old, new }, TableChange::Create { table: created }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut s, st, 10).await.unwrap_or_else(|e| panic!("{st}\n{e}"));
    }
    let after = s.database_schema().await.unwrap();
    let t = after.iter().find(|t| t.name == "ddltest_sync").unwrap();
    let ty = |n: &str| t.columns.iter().find(|c| c.name == n).map(|c| c.data_type.clone());
    assert_eq!(ty("old"), None);
    assert_eq!(ty("year").as_deref(), Some("plong"));
    assert_eq!(ty("genre").as_deref(), Some("string"));
    assert!(after.iter().any(|t| t.name == "ddltest_syncnew"));
    for c in ["ddltest_sync", "ddltest_syncnew"] {
        run(&mut s, &format!("DELETE /solr/{c}"), 10).await.unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn schema_sync() {
    if let Ok(url) = std::env::var("DBINE_TEST_SOLR_URL") {
        schema_sync_on(&url).await;
    }
    if let Ok(url) = std::env::var("DBINE_TEST_SOLRCLOUD_URL") {
        schema_sync_on(&url).await;
    }
}

/// The data-compare delete script removes exactly the keyed documents of
/// `testcore` (ids with quotes and spaces).
#[tokio::test]
#[ignore]
async fn delete_script_runs() {
    let url = std::env::var("DBINE_TEST_SOLR_URL").expect("DBINE_TEST_SOLR_URL");
    let mut s = session(&url, false).await;
    let d = &dbine_driver_solr::drivers()[0];
    run(
        &mut s,
        r#"POST /solr/testcore/update?commit=true
[{"id": "del O'Brien \"Bob\""}, {"id": "del b"}, {"id": "del keep"}]"#,
        10,
    )
    .await
    .expect("seed");
    let obj = ObjectRef { kind: "collection".into(), schema: None, name: "testcore".into() };
    let keys = vec![vec![("id".to_string(), json!("del O'Brien \"Bob\""))], vec![("id".to_string(), json!("del b"))]];
    let script = d.delete_script(&obj, &keys).unwrap();
    run(&mut s, &script, 10).await.expect(&script);
    let out = run(&mut s, "GET /solr/testcore/select?q=id:del*&fl=id", 10).await.unwrap();
    let rows = serde_json::to_string(&out.results[0].rows).unwrap();
    assert!(rows.contains("del keep") && !rows.contains("Brien") && !rows.contains("del b"), "{rows}");
    run(&mut s, "POST /solr/testcore/update?commit=true\n{\"delete\": [\"del keep\"]}", 10).await.unwrap();
}
