//! "Comparar esquemas" against a real libSQL server: the model is a local
//! SQLite file (the same catalog reader), the server's tables differ from
//! it in each thing the compare reads (CHECKs, descending and collated
//! index keys, expression and partial indexes, FTS5 virtual tables). The
//! libSQL driver's sync script runs on the server and both must then read
//! the same. Only objects named `cmpx_*` are looked at.
//!
//! ```sh
//! DBINE_TEST_LIBSQL_URL=http://localhost:25880 cargo test -p dbine-driver-libsql --test compare -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};
use std::collections::BTreeMap;

const VIRTUAL_TABLE: &str = "virtual_table";
const CODE_KINDS: &[&str] = &["view", "trigger", VIRTUAL_TABLE];

type Objects = BTreeMap<(String, String), String>;

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

async fn read(s: &mut Box<dyn Session>) -> (BTreeMap<String, TableSchema>, Objects) {
    let mine = |n: &str| n.starts_with("cmpx_");
    let tables = s.database_schema().await.unwrap().into_iter().filter(|t| mine(&t.name)).map(|t| (t.name.clone(), t)).collect();
    let mut objects = Objects::new();
    for o in s.list_objects().await.unwrap() {
        if CODE_KINDS.contains(&o.kind.as_str()) && mine(&o.name) {
            let r = ObjectRef { kind: o.kind.clone(), schema: None, name: o.name.clone() };
            objects.insert((o.kind, o.name), s.definition(&r).await.unwrap().expect("definition"));
        }
    }
    (tables, objects)
}

/// What src-tauri's `drop_other` should write for these kinds.
fn drop_other(kind: &str, name: &str) -> String {
    let what = if kind == VIRTUAL_TABLE { "TABLE" } else { &kind.to_uppercase() };
    format!("DROP {what} IF EXISTS \"{name}\";")
}

const MODEL: &str = "
CREATE TABLE cmpx_docs (
    id INTEGER PRIMARY KEY,
    titulo TEXT NOT NULL CHECK (length(titulo) > 0),
    n INT CONSTRAINT ck_n CHECK (n >= 0),
    CONSTRAINT ck_t CHECK (n < 1000 OR titulo <> '')
);
CREATE INDEX cmpx_ix_titulo ON cmpx_docs (titulo COLLATE NOCASE DESC, n);
CREATE INDEX cmpx_ix_lower ON cmpx_docs (lower(titulo)) WHERE n > 0;
CREATE VIRTUAL TABLE cmpx_fts USING fts5(titulo, cuerpo, tokenize = 'porter');
CREATE VIEW cmpx_v AS SELECT id, titulo FROM cmpx_docs;
";

const SERVER: &str = "
CREATE TABLE cmpx_docs (
    id INTEGER PRIMARY KEY,
    titulo TEXT NOT NULL,
    n INT CONSTRAINT ck_n CHECK (n >= 10)
);
INSERT INTO cmpx_docs VALUES (1, 'uno', 20);
CREATE INDEX cmpx_ix_titulo ON cmpx_docs (titulo, n);
CREATE VIRTUAL TABLE cmpx_fts USING fts5(titulo);
CREATE VIRTUAL TABLE cmpx_sobra USING fts5(x);
";

const CLEAN: &str = "
DROP VIEW IF EXISTS cmpx_v;
DROP TABLE IF EXISTS cmpx_fts;
DROP TABLE IF EXISTS cmpx_sobra;
DROP TABLE IF EXISTS cmpx_docs;
DROP TABLE IF EXISTS cmpx_docs__dbine_new;
";

#[tokio::test]
#[ignore]
async fn compare_and_sync_everything() {
    let Ok(url) = std::env::var("DBINE_TEST_LIBSQL_URL") else {
        eprintln!("DBINE_TEST_LIBSQL_URL not set; skipping");
        return;
    };
    let mut cfg = ConnectionConfig { driver: "libsql".into(), host: url, ..Default::default() };
    if let Ok(t) = std::env::var("DBINE_TEST_LIBSQL_TOKEN") {
        cfg.options.insert("auth_token".into(), t);
    }
    let d = dbine_driver_libsql::drivers().remove(0);
    assert!(d.info().object_kinds.iter().any(|k| k.id == VIRTUAL_TABLE));
    let mut srv = d.connect(&cfg, None).await.expect("connect");
    run(&mut srv, CLEAN).await;
    run(&mut srv, SERVER).await;

    let path = std::env::temp_dir().join(format!("dbine-libsql-cmp-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let local = ConnectionConfig { driver: "sqlite".into(), host: path.to_string_lossy().into(), ..Default::default() };
    let mut model = dbine_driver_sqlite::drivers().remove(0).connect(&local, None).await.unwrap();
    run(&mut model, MODEL).await;

    let (ta, oa) = read(&mut model).await;
    let (tb, ob) = read(&mut srv).await;
    assert_eq!(tb.keys().collect::<Vec<_>>(), ["cmpx_docs"], "no virtual nor shadow tables");
    assert_eq!(ob.keys().filter(|(k, _)| k == VIRTUAL_TABLE).count(), 2, "{ob:?}");
    assert_eq!(tb["cmpx_docs"].checks.len(), 1);

    let tables: Vec<TableChange> = ta
        .iter()
        .filter_map(|(n, t)| match tb.get(n) {
            None => Some(TableChange::Create { table: t.clone() }),
            Some(o) if o != t => Some(TableChange::Alter { old: o.clone(), new: t.clone() }),
            Some(_) => None,
        })
        .collect();
    let mut before = Vec::new();
    let mut after = Vec::new();
    for ((k, n), def) in &oa {
        match ob.get(&(k.clone(), n.clone())) {
            Some(o) if o == def => {}
            Some(_) => {
                before.push(drop_other(k, n));
                after.push(format!("{def};"));
            }
            None => after.push(format!("{def};")),
        }
    }
    for (k, n) in ob.keys() {
        if !oa.contains_key(&(k.clone(), n.clone())) {
            before.push(drop_other(k, n));
        }
    }
    let script = d.sync_script(&tables).unwrap();
    for s in before.iter().chain(&script.statements).chain(&after) {
        run(&mut srv, s).await;
    }

    let (tb2, ob2) = read(&mut srv).await;
    assert_eq!(tb2, ta);
    assert_eq!(ob2, oa);
    let mut out = QueryOutcome::default();
    srv.execute("SELECT titulo FROM cmpx_docs", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, [[serde_json::json!("uno")]]);

    run(&mut srv, CLEAN).await;
    drop(model);
    let _ = std::fs::remove_file(&path);
}
