//! "Comparar esquemas" on two DuckDB files that differ in each thing the
//! compare reads (CHECKs, expression and unique indexes, sequences, ENUM /
//! STRUCT / alias types, in two schemas): the sync script and the object
//! definitions run on one side and both must then read the same. DuckDB is
//! embedded, so this needs no server.

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};
use std::collections::BTreeMap;

const CODE_KINDS: &[&str] = &["view", "sequence", "type"];
const PREREQS: &[&str] = &["type", "sequence"];

type Objects = BTreeMap<(String, Option<String>, String), String>;

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

async fn read(s: &mut Box<dyn Session>) -> (BTreeMap<String, TableSchema>, Objects) {
    let tables = s
        .database_schema()
        .await
        .unwrap()
        .into_iter()
        .map(|mut t| {
            t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            (format!("{}.{}", t.schema.clone().unwrap_or_default(), t.name), t)
        })
        .collect();
    let mut objects = Objects::new();
    for o in s.list_objects().await.unwrap() {
        if CODE_KINDS.contains(&o.kind.as_str()) {
            let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
            objects.insert((o.kind, o.schema, o.name), s.definition(&r).await.unwrap().expect("definition"));
        }
    }
    (tables, objects)
}

/// What src-tauri's `drop_other` writes.
fn drop_other(kind: &str, schema: Option<&str>, name: &str) -> String {
    let full = match schema {
        Some(s) => format!("\"{s}\".\"{name}\""),
        None => format!("\"{name}\""),
    };
    format!("DROP {} IF EXISTS {full};", kind.to_uppercase())
}

const A: &str = "
CREATE SCHEMA ventas;
CREATE TYPE estado AS ENUM ('nuevo', 'pagado', 'it''s');
CREATE TYPE ventas.punto AS STRUCT(x INTEGER, y VARCHAR);
CREATE TYPE importe AS DECIMAL(18,2);
CREATE SEQUENCE ventas.folio START WITH 100 INCREMENT BY 5 MAXVALUE 100000 CYCLE;
CREATE TABLE ventas.pedidos (
    id INTEGER PRIMARY KEY,
    cliente VARCHAR NOT NULL CHECK (length(cliente) > 1),
    total DECIMAL(18,2),
    estado ENUM('nuevo', 'pagado', 'it''s'),
    CHECK (total >= 0 AND total < 1000000)
);
CREATE INDEX ix_cliente ON ventas.pedidos (lower(cliente), id);
CREATE INDEX ix_mas ON ventas.pedidos ((id + 1));
CREATE UNIQUE INDEX ux_cliente ON ventas.pedidos (cliente);
CREATE VIEW ventas.v_pedidos AS SELECT id, cliente FROM ventas.pedidos;
";

const B: &str = "
CREATE SCHEMA ventas;
CREATE TYPE estado AS ENUM ('nuevo', 'pagado');
CREATE TYPE sobra AS INTEGER[];
CREATE SEQUENCE ventas.folio START WITH 1;
CREATE SEQUENCE ventas.extra;
CREATE TABLE ventas.pedidos (
    id INTEGER PRIMARY KEY,
    cliente VARCHAR NOT NULL,
    total DECIMAL(18,2),
    estado ENUM('nuevo', 'pagado', 'it''s'),
    CHECK (total >= 0)
);
INSERT INTO ventas.pedidos VALUES (1, 'uno', 10, 'nuevo');
CREATE INDEX ix_cliente ON ventas.pedidos (cliente);
";

#[tokio::test]
async fn compare_and_sync_everything() {
    let dir = std::env::temp_dir();
    let (pa, pb) = (dir.join(format!("dbine-cmp-a-{}.duckdb", std::process::id())), dir.join(format!("dbine-cmp-b-{}.duckdb", std::process::id())));
    for p in [&pa, &pb] {
        let _ = std::fs::remove_file(p);
    }
    let d = dbine_driver_duckdb::drivers().remove(0);
    let kinds: Vec<&str> = d.info().object_kinds.iter().map(|k| k.id).collect();
    assert!(kinds.contains(&"type") && kinds.contains(&"sequence"), "{kinds:?}");
    let cfg = |p: &std::path::Path| ConnectionConfig { driver: "duckdb".into(), host: p.to_string_lossy().into(), ..Default::default() };
    let mut a = d.connect(&cfg(&pa), None).await.unwrap();
    let mut b = d.connect(&cfg(&pb), None).await.unwrap();
    run(&mut a, A).await;
    run(&mut b, B).await;

    let (ta, oa) = read(&mut a).await;
    let (tb, ob) = read(&mut b).await;
    for ((k, s, n), def) in &oa {
        eprintln!("-- {k} {s:?}.{n}\n{def}");
    }
    let p = &ta["ventas.pedidos"];
    assert_eq!(p.checks.len(), 2, "{:?}", p.checks);
    let ix = |n: &str| p.indexes.iter().find(|i| i.name == n).unwrap().columns.clone();
    assert_eq!(ix("ix_cliente"), ["(lower(cliente))", "id"]);
    assert_eq!(ix("ix_mas"), ["(id + 1)"]);
    let obj = |o: &Objects, k: &str, s: &str, n: &str| o.get(&(k.to_string(), Some(s.to_string()), n.to_string())).cloned();
    assert_eq!(obj(&oa, "type", "main", "estado").as_deref(), Some("CREATE TYPE \"main\".\"estado\" AS ENUM('nuevo', 'pagado', 'it''s');"));
    assert_eq!(obj(&oa, "type", "ventas", "punto").as_deref(), Some("CREATE TYPE \"ventas\".\"punto\" AS STRUCT(x INTEGER, y VARCHAR);"));
    assert_eq!(obj(&oa, "type", "main", "importe").as_deref(), Some("CREATE TYPE \"main\".\"importe\" AS DECIMAL(18,2);"));
    let seq = obj(&oa, "sequence", "ventas", "folio").unwrap();
    assert!(seq.starts_with("CREATE SEQUENCE \"ventas\".\"folio\" INCREMENT BY 5") && seq.contains("START WITH 100 CYCLE"), "{seq}");

    // The compare: tables by schema and name, objects by kind, schema and name.
    let mut tables = Vec::new();
    for (name, t) in &ta {
        match tb.get(name) {
            None => tables.push(TableChange::Create { table: t.clone() }),
            Some(o) if o != t => tables.push(TableChange::Alter { old: o.clone(), new: t.clone() }),
            Some(_) => {}
        }
    }
    assert_eq!(tables.len(), 1);
    let (mut before, mut early, mut after, mut late) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ((k, s, n), def) in &oa {
        let prereq = PREREQS.contains(&k.as_str());
        match ob.get(&(k.clone(), s.clone(), n.clone())) {
            Some(o) if o == def => continue,
            Some(_) => before.push(drop_other(k, s.as_deref(), n)),
            None => {}
        }
        if prereq { early.push(def.clone()) } else { after.push(def.clone()) }
    }
    for (k, s, n) in ob.keys() {
        if !oa.contains_key(&(k.clone(), s.clone(), n.clone())) {
            let drop = drop_other(k, s.as_deref(), n);
            if PREREQS.contains(&k.as_str()) { late.push(drop) } else { before.push(drop) }
        }
    }
    let script = d.sync_script(&tables).unwrap();
    for w in &script.warnings {
        eprintln!("aviso: {w}");
    }
    let all: Vec<String> = before.into_iter().chain(early).chain(script.statements).chain(after).chain(late).collect();
    for s in &all {
        run(&mut b, s).await;
    }

    let (tb2, ob2) = read(&mut b).await;
    assert_eq!(tb2, ta);
    assert_eq!(ob2, oa);
    let mut out = QueryOutcome::default();
    b.execute("SELECT cliente FROM ventas.pedidos", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, [[serde_json::json!("uno")]]);
    drop((a, b));
    for p in [&pa, &pb] {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(p.with_extension("duckdb.wal"));
    }
}
