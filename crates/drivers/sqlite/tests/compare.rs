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

// --- "Eliminar" in the compare ---------------------------------------------
//
// The UI drops an element by taking it out of a side's model and sending the
// difference (`changesOf` in CompareView.vue): a table that's gone is a
// `Drop`, a table that lost an item is an `Alter { old, new }`, a code object
// that's gone is dropped with `drop_other` before the tables. A table drop
// also takes out the foreign keys of that side's other tables that point to
// it. After the run the side is read again and must be its edited model.

#[derive(Clone, Debug, PartialEq)]
struct Model {
    tables: BTreeMap<String, TableSchema>,
    objects: Objects,
}

async fn model(s: &mut Box<dyn Session>) -> Model {
    let (tables, objects) = read(s).await;
    Model { tables, objects }
}

/// What the UI sends for `orig` → `work`, as statements in run order.
fn drop_script(d: &dyn dbine_driver::Driver, orig: &Model, work: &Model) -> Vec<String> {
    let mut tables = Vec::new();
    for (n, t) in &orig.tables {
        match work.tables.get(n) {
            None => tables.push(TableChange::Drop { table: t.clone() }),
            Some(w) if w != t => tables.push(TableChange::Alter { old: t.clone(), new: w.clone() }),
            Some(_) => {}
        }
    }
    // Triggers first (they read views and tables), then views.
    let mut gone: Vec<&(String, String)> = orig.objects.keys().filter(|k| !work.objects.contains_key(*k)).collect();
    gone.sort_by_key(|(k, _)| k != "trigger");
    let script = d.sync_script(&tables).unwrap();
    for w in &script.warnings {
        eprintln!("aviso: {w}");
    }
    gone.into_iter().map(|(k, n)| drop_other(k, n)).chain(script.statements).collect()
}

/// Drops on one side what `edit` takes out of its model; returns the model.
async fn drop_on(d: &dyn dbine_driver::Driver, s: &mut Box<dyn Session>, edit: impl Fn(&mut Model)) -> Model {
    let orig = model(s).await;
    let mut work = orig.clone();
    edit(&mut work);
    assert_ne!(work, orig, "the edit drops something");
    for st in drop_script(d, &orig, &work) {
        run(s, &st).await;
    }
    let after = model(s).await;
    assert_eq!(after, work, "the side reads as its edited model");
    after
}

fn table<'a>(m: &'a mut Model, t: &str) -> &'a mut TableSchema {
    m.tables.get_mut(t).unwrap()
}

/// Takes a table out, and the foreign keys that point to it.
fn drop_table(m: &mut Model, t: &str) {
    m.tables.remove(t);
    for other in m.tables.values_mut() {
        other.foreign_keys.retain(|f| f.ref_table != t);
    }
}

const BOTH: &str = "
CREATE TABLE padre (id INTEGER PRIMARY KEY, nombre TEXT);
CREATE TABLE otro (cod TEXT PRIMARY KEY);
CREATE TABLE hijo (
    id INTEGER PRIMARY KEY,
    padre_id INT REFERENCES padre (id),
    cod TEXT,
    x INT CONSTRAINT ck_x CHECK (x > 0),
    extra TEXT,
    CONSTRAINT fk_otro FOREIGN KEY (cod) REFERENCES otro (cod)
);
CREATE INDEX ix_x ON hijo (x);
CREATE INDEX ix_cod ON hijo (cod);
CREATE VIEW v_hijo AS SELECT id, x FROM hijo;
CREATE TRIGGER tg_padre AFTER INSERT ON padre BEGIN UPDATE padre SET nombre = upper(nombre) WHERE id = NEW.id; END;
INSERT INTO padre VALUES (1, 'uno');
INSERT INTO otro VALUES ('a');
INSERT INTO hijo VALUES (1, 1, 'a', 5, 'e');
";

#[tokio::test]
async fn drop_from_compare() {
    let dir = std::env::temp_dir();
    let (pa, pb) = (dir.join(format!("dbine-drop-a-{}.db", std::process::id())), dir.join(format!("dbine-drop-b-{}.db", std::process::id())));
    for p in [&pa, &pb] {
        let _ = std::fs::remove_file(p);
    }
    let d = dbine_driver_sqlite::drivers().remove(0);
    let cfg = |p: &std::path::Path| ConnectionConfig { driver: "sqlite".into(), host: p.to_string_lossy().into(), ..Default::default() };
    let mut a = d.connect(&cfg(&pa), None).await.unwrap();
    let mut b = d.connect(&cfg(&pb), None).await.unwrap();
    run(&mut a, BOTH).await;
    run(&mut b, BOTH).await;
    assert_eq!(model(&mut a).await, model(&mut b).await);
    let d = d.as_ref();

    // An index on one side: the sides differ only there.
    let ma = drop_on(d, &mut a, |m| table(m, "hijo").indexes.retain(|i| i.name != "ix_x")).await;
    let mb = model(&mut b).await;
    assert_ne!(ma.tables["hijo"].indexes, mb.tables["hijo"].indexes);
    assert_eq!(ma.tables["hijo"].columns, mb.tables["hijo"].columns);
    // The same index on the other side: equal again.
    let mb = drop_on(d, &mut b, |m| table(m, "hijo").indexes.retain(|i| i.name != "ix_x")).await;
    assert_eq!(ma, mb);

    // An index, a column, a CHECK and a foreign key, each on both sides.
    type Edit = (&'static str, fn(&mut Model));
    let edits: [Edit; 4] = [
        ("index", |m| table(m, "hijo").indexes.retain(|i| i.name != "ix_cod")),
        ("column", |m| table(m, "hijo").columns.retain(|c| c.name != "extra")),
        ("check", |m| table(m, "hijo").checks.retain(|c| c.name.as_deref() != Some("ck_x"))),
        ("foreign key", |m| table(m, "hijo").foreign_keys.retain(|f| f.ref_table != "otro")),
    ];
    for (what, edit) in edits {
        eprintln!("-- {what}");
        let ma = drop_on(d, &mut a, edit).await;
        let mb = drop_on(d, &mut b, edit).await;
        assert_eq!(ma, mb, "{what}");
    }
    // The rows survived the rebuilds.
    let mut out = QueryOutcome::default();
    a.execute("SELECT id, padre_id, x FROM hijo", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, [[serde_json::json!(1), serde_json::json!(1), serde_json::json!(5)]]);
    // The view over the rebuilt table still reads.
    let mut out = QueryOutcome::default();
    b.execute("SELECT x FROM v_hijo", 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    assert_eq!(out.results[0].rows, [[serde_json::json!(5)]]);

    // A view and a trigger.
    for s in [&mut a, &mut b] {
        drop_on(d, s, |m| {
            m.objects.remove(&("view".into(), "v_hijo".into()));
            m.objects.remove(&("trigger".into(), "tg_padre".into()));
        })
        .await;
    }

    // A table another one references (with a row pointing to it).
    for s in [&mut a, &mut b] {
        drop_on(d, s, |m| drop_table(m, "padre")).await;
    }
    let (ma, mb) = (model(&mut a).await, model(&mut b).await);
    assert_eq!(ma, mb);
    assert!(ma.tables["hijo"].foreign_keys.is_empty());
    assert!(ma.objects.is_empty());

    drop((a, b));
    for p in [&pa, &pb] {
        let _ = std::fs::remove_file(p);
    }
}
