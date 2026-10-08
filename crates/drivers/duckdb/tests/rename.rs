//! "Renombrar…" on a temporary file (DuckDB is embedded, so no server or
//! env var is needed). DuckDB doesn't follow views or macros: they are
//! rewritten with `rewrite_references` and put back with `CREATE OR
//! REPLACE` after the rename, in one transaction, as the app does. It does
//! refuse what an index or a foreign key uses.

use dbine_driver::rename::{rewrite_references, with_create_style, RenameTarget, RewriteOptions};
use dbine_driver::{Confidence, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, RenameRequest, Session};
use serde_json::Value;

async fn one(s: &mut dyn Session, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    match out.results.last().and_then(|r| r.rows.first()).and_then(|r| r.first()) {
        Some(Value::String(v)) => v.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some("main".into()), name: name.into() }
}

/// The driver's rename and the rewritten dependents, in one transaction.
/// Returns the dependents left to the user (dynamic SQL).
async fn rename(d: &dyn Driver, s: &mut dyn Session, target: RenameTarget, new: &str) -> dbine_driver::Result<Vec<String>> {
    let spec = d.rename_spec().unwrap();
    let dialect = d.script_dialect();
    let table = match target.table() {
        Some(t) => s.database_schema().await.unwrap().into_iter().find(|x| x.name == t.name),
        None => None,
    };
    let script = d.rename_script(&RenameRequest { target: target.clone(), new_name: new.into(), table, definition: None })?;
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&target.dependency_target(), &scan).await.unwrap();
    let mut statements = script.statements;
    let mut manual = Vec::new();
    for dep in report.items.iter().filter(|x| x.relation == Relation::Code) {
        assert!(!spec.tracked.contains(&dep.kind));
        if dep.confidence == Confidence::Review {
            manual.push(dep.name.clone());
            continue;
        }
        let def = s.definition(&ObjectRef { kind: dep.kind.clone(), schema: dep.schema.clone(), name: dep.name.clone() }).await.unwrap().unwrap();
        let opts = RewriteOptions { dependent_schema: dep.schema.clone(), keep_view_columns: dep.kind == "view", ..Default::default() };
        let r = rewrite_references(&def, &dialect, &target.rewrite_target(), new, &spec, &opts);
        if r.edits.is_empty() {
            continue;
        }
        assert!(r.unresolved.is_empty(), "{}: {:?}", dep.name, r.unresolved);
        statements.push(with_create_style(&r.text, &dialect, spec.replace_for(&dep.kind)));
    }
    let mut out = QueryOutcome::default();
    s.execute("BEGIN", 10, &mut out).await?;
    for st in &statements {
        if let Err(e) = s.execute(st, 10, &mut out).await {
            s.execute("ROLLBACK", 10, &mut out).await.unwrap();
            return Err(e);
        }
    }
    s.execute("COMMIT", 10, &mut out).await?;
    manual.sort();
    Ok(manual)
}

#[tokio::test]
async fn duckdb_rename_with_impact() {
    let path = std::env::temp_dir().join(format!("dbine-duck-rename-{}.duckdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d = dbine_driver_duckdb::drivers().remove(0);
    let cfg = ConnectionConfig { driver: "duckdb".into(), host: path.to_string_lossy().into(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, pepe VARCHAR CHECK (pepe <> ''));
         CREATE TABLE t2 (id INTEGER, tid INTEGER REFERENCES t (id));
         CREATE TABLE t3 (pepe VARCHAR);
         CREATE INDEX ix_pepe ON t (pepe);
         CREATE VIEW v AS SELECT id, pepe FROM t;
         CREATE MACRO m() AS TABLE SELECT pepe FROM t WHERE pepe IS NOT NULL;
         CREATE MACRO dyn() AS TABLE SELECT * FROM query('SELECT pepe FROM t');
         CREATE MACRO other() AS TABLE SELECT pepe FROM t3;
         INSERT INTO t VALUES (1, 'a');
         INSERT INTO t3 VALUES ('z');",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let column = || RenameTarget::Column { table: obj("table", "t"), column: "pepe".into() };
    let table = || RenameTarget::Object { object: obj("table", "t"), parent: None };

    // The index and the foreign key block both renames; nothing changes.
    let err = rename(d.as_ref(), s.as_mut(), column(), "nuevo").await.unwrap_err().to_string();
    assert!(err.contains("Dependency"), "{err}");
    let err = rename(d.as_ref(), s.as_mut(), table(), "t_nueva").await.unwrap_err().to_string();
    assert!(err.contains("Dependency"), "{err}");
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM v").await, "a");
    // A foreign key column is refused before reaching the engine.
    let fk = RenameTarget::Column { table: obj("table", "t2"), column: "tid".into() };
    assert!(matches!(rename(d.as_ref(), s.as_mut(), fk, "x").await, Err(dbine_driver::Error::Unsupported(_))));

    s.execute("DROP INDEX ix_pepe; DROP TABLE t2;", 10, &mut out).await.unwrap();

    // Column: the view keeps its output name, the macro follows, the
    // same-named column of t3 and the dynamic SQL are left alone.
    let manual = rename(d.as_ref(), s.as_mut(), column(), "Nuevo Pepe").await.unwrap();
    assert_eq!(manual, ["dyn"]);
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM v").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT \"Nuevo Pepe\" FROM m()").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM other()").await, "z");
    assert!(s.execute("INSERT INTO t VALUES (2, '')", 10, &mut out).await.is_err(), "the CHECK follows the column");

    // Table.
    let manual = rename(d.as_ref(), s.as_mut(), table(), "T Nueva").await.unwrap();
    assert_eq!(manual, ["dyn"]);
    assert_eq!(one(s.as_mut(), "SELECT count(*) FROM v").await, "1");
    assert_eq!(one(s.as_mut(), "SELECT count(*) FROM m()").await, "1");

    // View, with a view on it.
    s.execute("CREATE VIEW w AS SELECT pepe FROM v;", 10, &mut out).await.unwrap();
    rename(d.as_ref(), s.as_mut(), RenameTarget::Object { object: obj("view", "v"), parent: None }, "v2").await.unwrap();
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM w").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT count(*) FROM duckdb_views() WHERE view_name = 'v'").await, "0");

    drop(s);
    let _ = std::fs::remove_file(&path);
}
