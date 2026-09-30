//! "Comparar esquemas" against the Cloud Spanner emulator: two databases
//! that differ in every kind of thing the compare reads (CHECKs, named and
//! generated; STORING, descending, filtered, NULL_FILTERED and interleaved
//! indexes; search and vector indexes with their OPTIONS; hidden TOKENLIST
//! columns; sequences). The sync script is run on one side and both must
//! then read the same.
//!
//! ```sh
//! docker run -d --name dbine-test-spanner -p 25303:9020 gcr.io/cloud-spanner-emulator/emulator
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 \
//!   cargo test -p dbine-driver-spanner --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};
use serde_json::json;
use std::collections::BTreeMap;

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

const SOURCE: &str = "
CREATE SEQUENCE seq_folio OPTIONS (sequence_kind = 'bit_reversed_positive', skip_range_min = 1, skip_range_max = 1000);
CREATE TABLE docs (
  id INT64 NOT NULL,
  codigo STRING(20) NOT NULL,
  titulo STRING(200),
  precio INT64,
  fecha DATE,
  titulo_tokens TOKENLIST AS (TOKENIZE_FULLTEXT(titulo)) HIDDEN,
  CONSTRAINT ck_precio CHECK (precio > 0),
  CHECK (fecha >= DATE '2000-01-01'),
) PRIMARY KEY (id);
CREATE INDEX ix_fecha ON docs (fecha DESC, codigo) STORING (titulo);
CREATE UNIQUE NULL_FILTERED INDEX ux_codigo ON docs (codigo);
CREATE INDEX ix_precio ON docs (precio) WHERE precio IS NOT NULL;
CREATE SEARCH INDEX sx_titulo ON docs (titulo_tokens) OPTIONS (sort_order_sharding = true);
CREATE TABLE lineas (id INT64 NOT NULL, n INT64 NOT NULL, cant INT64) PRIMARY KEY (id, n), INTERLEAVE IN PARENT docs ON DELETE CASCADE;
CREATE INDEX ix_lineas_cant ON lineas (id, cant), INTERLEAVE IN docs;
CREATE TABLE vecs (id INT64 NOT NULL, e ARRAY<FLOAT32>(vector_length=>3)) PRIMARY KEY (id);
CREATE VECTOR INDEX vx_e ON vecs (e) WHERE e IS NOT NULL OPTIONS (distance_type = 'COSINE');
";

const TARGET: &str = "
CREATE SEQUENCE seq_folio OPTIONS (sequence_kind = 'bit_reversed_positive');
CREATE SEQUENCE seq_extra OPTIONS (sequence_kind = 'bit_reversed_positive');
CREATE TABLE docs (
  id INT64 NOT NULL,
  codigo STRING(20) NOT NULL,
  titulo STRING(200),
  precio INT64,
  fecha DATE,
  titulo_tokens TOKENLIST AS (TOKENIZE_FULLTEXT(titulo)) HIDDEN,
  CONSTRAINT ck_precio CHECK (precio >= 0),
  CONSTRAINT ck_vieja CHECK (id > 0),
  CHECK (fecha >= DATE '1990-01-01'),
) PRIMARY KEY (id);
CREATE INDEX ix_fecha ON docs (fecha, codigo);
CREATE UNIQUE INDEX ux_codigo ON docs (codigo);
CREATE SEARCH INDEX sx_titulo ON docs (titulo_tokens);
CREATE TABLE lineas (id INT64 NOT NULL, n INT64 NOT NULL, cant INT64) PRIMARY KEY (id, n), INTERLEAVE IN PARENT docs ON DELETE CASCADE;
CREATE INDEX ix_lineas_cant ON lineas (id, cant);
CREATE TABLE vecs (id INT64 NOT NULL, e ARRAY<FLOAT32>(vector_length=>3)) PRIMARY KEY (id);
CREATE VECTOR INDEX vx_e ON vecs (e) WHERE e IS NOT NULL OPTIONS (distance_type = 'EUCLIDEAN');
";

type Objects = BTreeMap<(String, Option<String>, String), String>;

fn normalized(mut t: TableSchema) -> TableSchema {
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t.checks.sort_by(|a, b| a.expression.cmp(&b.expression));
    t
}

/// Generated CHECK names differ between databases: compare them by condition.
fn comparable(mut t: TableSchema) -> TableSchema {
    for c in t.checks.iter_mut() {
        if c.name.as_deref().is_some_and(|n| n.starts_with("CK_")) {
            c.name = None;
        }
    }
    t
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(';').to_string()
}

async fn read(s: &mut Box<dyn Session>) -> (BTreeMap<String, TableSchema>, Objects) {
    let tables = s.database_schema().await.expect("database_schema").into_iter().map(|t| (t.name.clone(), normalized(t))).collect();
    let mut objects = Objects::new();
    for o in s.list_objects().await.expect("list_objects") {
        if o.kind != "sequence" {
            continue;
        }
        let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        let def = s.definition(&r).await.expect("definition").unwrap_or_else(|| panic!("no definition for {r:?}"));
        objects.insert((o.kind, o.schema, o.name), def);
    }
    (tables, objects)
}

async fn connect(url: &str, db: &str) -> Box<dyn Session> {
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: db.into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url)] {
        cfg.options.insert(k.into(), v.into());
    }
    dbine_driver_spanner::drivers().pop().unwrap().connect(&cfg, None).await.expect("connect")
}

#[tokio::test]
#[ignore]
async fn compare_and_sync_everything() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else {
        eprintln!("DBINE_TEST_SPANNER_URL not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http.post(format!("{url}/v1/projects/test/instances/i1/databases")).json(&json!({ "createStatement": "CREATE DATABASE `db1`" })).send().await;
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    assert!(d.info().object_kinds.iter().any(|k| k.id == "sequence"));
    let mut admin = connect(&url, "db1").await;
    let (a_db, b_db) = ("cmp_a", "cmp_b");
    for db in [a_db, b_db] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.expect("create_database");
    }
    let mut a = connect(&url, a_db).await;
    let mut b = connect(&url, b_db).await;
    run(&mut a, SOURCE).await.unwrap();
    run(&mut b, TARGET).await.unwrap();

    let (ta, oa) = read(&mut a).await;
    let (tb, ob) = read(&mut b).await;
    for ((k, s, n), def) in &oa {
        eprintln!("-- {k} {s:?}.{n}\n{def}");
    }

    // What the catalog says.
    let docs = &ta["docs"];
    let ix = |t: &TableSchema, n: &str| t.indexes.iter().find(|i| i.name == n).cloned().unwrap_or_else(|| panic!("{n} in {:#?}", t.indexes));
    let fecha = ix(docs, "ix_fecha");
    assert_eq!((fecha.columns.as_slice(), fecha.include.as_slice()), (&["fecha".to_string(), "codigo".into()][..], &["titulo".to_string()][..]));
    assert_eq!(fecha.options.get("desc").map(String::as_str), Some("fecha"));
    assert_eq!(ix(docs, "ux_codigo").kind.as_deref(), Some("NULL_FILTERED"));
    assert_eq!(ix(docs, "ix_precio").filter.as_deref(), Some("precio IS NOT NULL"));
    let sx = ix(docs, "sx_titulo");
    assert_eq!(sx.kind.as_deref(), Some("SEARCH"));
    assert_eq!(sx.options.get("sort_order_sharding").map(String::as_str), Some("true"), "{sx:?}");
    assert!(docs.columns.iter().any(|c| c.name == "titulo_tokens" && c.options.get("hidden").is_some_and(|h| h == "true")));
    assert_eq!(ix(&ta["lineas"], "ix_lineas_cant").options.get("interleave_in").map(String::as_str), Some("docs"));
    let vx = ix(&ta["vecs"], "vx_e");
    assert_eq!((vx.kind.as_deref(), vx.options.get("distance_type").map(String::as_str)), (Some("VECTOR"), Some("'COSINE'")));
    assert_eq!(docs.checks.len(), 2, "{:?}", docs.checks);
    assert!(docs.checks.iter().any(|c| c.name.as_deref() == Some("ck_precio") && c.expression == "precio > 0"));
    assert!(docs.checks.iter().any(|c| c.name.as_deref().is_some_and(|n| n.starts_with("CK_docs_"))), "{:?}", docs.checks);
    let seq = &oa[&("sequence".into(), None, "seq_folio".into())];
    assert!(seq.starts_with("CREATE SEQUENCE seq_folio") && seq.contains("skip_range_max = 1000"), "{seq}");

    // The compare: tables by name (generated CHECK names by condition), sequences by name.
    let mut tables = Vec::new();
    for (name, t) in &ta {
        match tb.get(name) {
            None => tables.push(TableChange::Create { table: t.clone() }),
            Some(o) if comparable(o.clone()) != comparable(t.clone()) => tables.push(TableChange::Alter { old: o.clone(), new: t.clone() }),
            Some(_) => {}
        }
    }
    let changed: Vec<&str> = tables.iter().map(|c| match c {
        TableChange::Alter { new, .. } | TableChange::Create { table: new } | TableChange::Drop { table: new } => new.name.as_str(),
    }).collect();
    assert_eq!(changed, ["docs", "lineas", "vecs"]);

    // Sequences: drop what's gone or changed (src-tauri's drop_other, with
    // GoogleSQL's backticks), create before the tables.
    let (mut before, mut early) = (Vec::new(), Vec::new());
    for ((_, _, n), def) in &oa {
        match ob.get(&("sequence".to_string(), None, n.clone())) {
            None => early.push(def.clone()),
            Some(o) if squash(o) != squash(def) => {
                before.push(format!("DROP SEQUENCE IF EXISTS `{n}`;"));
                early.push(def.clone());
            }
            Some(_) => {}
        }
    }
    let mut late = Vec::new();
    for (_, _, n) in ob.keys() {
        if !oa.contains_key(&("sequence".to_string(), None, n.clone())) {
            late.push(format!("DROP SEQUENCE IF EXISTS `{n}`;"));
        }
    }
    let script = d.sync_script(&tables).expect("sync_script");
    for w in &script.warnings {
        eprintln!("aviso: {w}");
    }
    let statements: Vec<String> = before.into_iter().chain(early).chain(script.statements).chain(late).collect();
    for (i, s) in statements.iter().enumerate() {
        if let Err(e) = run(&mut b, s).await {
            panic!("statement {i} failed: {e}\n---\n{s}\n---\nwhole script:\n{}", statements.join("\n"));
        }
    }
    let (tb2, ob2) = read(&mut b).await;
    for (name, t) in &ta {
        assert_eq!(tb2.get(name).cloned().map(comparable), Some(comparable(t.clone())), "table {name} after the sync");
    }
    assert_eq!(tb2.len(), ta.len());
    let squashed = |o: &Objects| o.iter().map(|(k, v)| (k.clone(), squash(v))).collect::<Vec<_>>();
    assert_eq!(squashed(&ob2), squashed(&oa));

    drop(a);
    drop(b);
    for db in [a_db, b_db] {
        admin.drop_database(db).await.expect("drop_database");
    }
}
