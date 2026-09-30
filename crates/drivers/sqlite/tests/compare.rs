//! "Comparar esquemas" on two SQLite files that differ in each thing the
//! compare reads (CHECKs, descending and collated index keys, expression
//! and partial indexes, FTS5 / FTS4 virtual tables): the sync script is run
//! on one side and both must then read the same.

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
    let tables = s.database_schema().await.unwrap().into_iter().map(|t| (t.name.clone(), t)).collect();
    let mut objects = Objects::new();
    for o in s.list_objects().await.unwrap() {
        if CODE_KINDS.contains(&o.kind.as_str()) {
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

const A: &str = "
CREATE TABLE docs (
    id INTEGER PRIMARY KEY,
    titulo TEXT NOT NULL CHECK (length(titulo) > 0),
    n INT CONSTRAINT ck_n CHECK (n >= 0),
    CONSTRAINT ck_t CHECK (n < 1000 OR titulo <> '')
);
CREATE INDEX ix_titulo ON docs (titulo COLLATE NOCASE DESC, n);
CREATE INDEX ix_lower ON docs (lower(titulo)) WHERE n > 0;
CREATE VIRTUAL TABLE docs_fts USING fts5(titulo, cuerpo, tokenize = 'porter');
CREATE VIRTUAL TABLE otro_fts USING fts4(a);
CREATE VIEW v_docs AS SELECT id, titulo FROM docs;
";

const B: &str = "
CREATE TABLE docs (
    id INTEGER PRIMARY KEY,
    titulo TEXT NOT NULL,
    n INT CONSTRAINT ck_n CHECK (n >= 10)
);
INSERT INTO docs VALUES (1, 'uno', 20);
CREATE INDEX ix_titulo ON docs (titulo, n);
CREATE VIRTUAL TABLE docs_fts USING fts5(titulo);
CREATE VIRTUAL TABLE sobra_fts USING fts5(x);
";

#[tokio::test]
async fn compare_and_sync_everything() {
    let dir = std::env::temp_dir();
    let (pa, pb) = (dir.join(format!("dbine-cmp-a-{}.db", std::process::id())), dir.join(format!("dbine-cmp-b-{}.db", std::process::id())));
    for p in [&pa, &pb] {
        let _ = std::fs::remove_file(p);
    }
    let d = dbine_driver_sqlite::drivers().remove(0);
    let cfg = |p: &std::path::Path| ConnectionConfig { driver: "sqlite".into(), host: p.to_string_lossy().into(), ..Default::default() };
    let mut a = d.connect(&cfg(&pa), None).await.unwrap();
    let mut b = d.connect(&cfg(&pb), None).await.unwrap();
    run(&mut a, A).await;
    run(&mut b, B).await;

    let (ta, oa) = read(&mut a).await;
    let (tb, ob) = read(&mut b).await;
    assert_eq!(ta.keys().collect::<Vec<_>>(), ["docs"], "no virtual nor shadow tables");
    let docs = &ta["docs"];
    assert_eq!(docs.checks.len(), 3, "{:?}", docs.checks);
    let ix = docs.indexes.iter().find(|i| i.name == "ix_titulo").unwrap();
    assert_eq!(ix.options.get("desc").map(String::as_str), Some("titulo"));
    assert_eq!(ix.options.get("collate:titulo").map(String::as_str), Some("NOCASE"));
    assert_eq!(oa[&(VIRTUAL_TABLE.into(), "docs_fts".into())], "CREATE VIRTUAL TABLE docs_fts USING fts5(titulo, cuerpo, tokenize = 'porter')");

    // Tables by name, objects by kind and name.
    let tables: Vec<TableChange> = ta
        .iter()
        .filter_map(|(n, t)| match tb.get(n) {
            None => Some(TableChange::Create { table: t.clone() }),
            Some(o) if o != t => Some(TableChange::Alter { old: o.clone(), new: t.clone() }),
            Some(_) => None,
        })
        .collect();
    assert_eq!(tables.len(), 1);
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
    for w in &script.warnings {
        eprintln!("aviso: {w}");
    }
    for s in before.iter().chain(&script.statements).chain(&after) {
        run(&mut b, s).await;
    }

    let (tb2, ob2) = read(&mut b).await;
    assert_eq!(tb2, ta);
    assert_eq!(ob2, oa);
    // The rows survived the rebuild.
    let mut out = QueryOutcome::default();
    b.execute("SELECT titulo FROM docs", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, [[serde_json::json!("uno")]]);
    drop((a, b));
    for p in [&pa, &pb] {
        let _ = std::fs::remove_file(p);
    }
}
