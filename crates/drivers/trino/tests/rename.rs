//! "Renombrar…" against a real Trino (memory catalog):
//!
//! ```sh
//! docker run -d --name dbine-test-trino -p 25180:8080 trinodb/trino
//! DBINE_TEST_TRINO_URL=http://localhost:25180 cargo test -p dbine-driver-trino --test rename -- --ignored
//! ```
//!
//! The same runs on Presto with `DBINE_TEST_PRESTO_URL` (`prestodb/presto`,
//! memory catalog).
//!
//! Trino doesn't follow views, and DBine doesn't put them back either
//! (re-created, they would belong to the renaming user): the views that
//! name the target are listed for the user, as the app does, and keep
//! naming the old object. Statements run one by one: the memory catalog
//! takes no DDL in a transaction. The memory catalog has no keys, indexes,
//! checks or routines; the rest of the fixture is there.

use dbine_driver::rename::{rewrite_references, with_create_style, ReferenceStyle, RenameTarget, RewriteOptions};
use dbine_driver::{Confidence, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, RenameRequest, Session};
use serde_json::Value;

fn cfg(var: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(var).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: driver.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    })
}

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
    ObjectRef { kind: kind.into(), schema: Some("rn_it".into()), name: name.into() }
}

/// The driver's rename and the rewritten dependents, one by one in a fresh
/// session (the app's run gets its own). Returns the dependents left to
/// the user.
async fn rename(d: &dyn Driver, c: &ConnectionConfig, target: RenameTarget, new: &str) -> dbine_driver::Result<Vec<String>> {
    let spec = d.rename_spec().unwrap();
    let dialect = d.script_dialect();
    let mut s = d.connect(c, None).await.unwrap();
    let table = match target.table() {
        Some(t) => s.database_schema().await.unwrap().into_iter().find(|x| x.name == t.name && x.schema == t.schema),
        None => None,
    };
    let script = d.rename_script(&RenameRequest { target: target.clone(), new_name: new.into(), table, definition: None })?;
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&target.dependency_target(), &scan).await.unwrap();
    let mut statements = script.statements;
    let mut manual = Vec::new();
    for dep in report.items.iter().filter(|x| x.relation == Relation::Code) {
        if dep.confidence == Confidence::Review || spec.references == ReferenceStyle::None {
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
    let mut run = d.connect(c, None).await.unwrap();
    for st in &statements {
        run.execute(st, 10, &mut QueryOutcome::default()).await.map_err(|e| dbine_driver::Error::Query(format!("{st}: {e}")))?;
    }
    manual.sort();
    Ok(manual)
}

#[tokio::test]
#[ignore]
async fn trino_rename_with_impact() {
    let Some(c) = cfg("DBINE_TEST_TRINO_URL", "trino") else { return };
    run(dbine_driver_trino::drivers().remove(0).as_ref(), &c).await;
}

#[tokio::test]
#[ignore]
async fn presto_rename_with_impact() {
    let Some(c) = cfg("DBINE_TEST_PRESTO_URL", "presto") else { return };
    run(dbine_driver_trino::drivers().remove(1).as_ref(), &c).await;
}

async fn run(d: &dyn Driver, c: &ConnectionConfig) {
    let c = c.clone();
    let mut s = d.connect(&c, None).await.unwrap();
    clean(s.as_mut()).await;
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE SCHEMA rn_it; CREATE SCHEMA rn_other; USE memory.rn_it;
         CREATE TABLE t (id bigint, pepe varchar);
         CREATE TABLE t3 (pepe varchar);
         INSERT INTO t VALUES (1, 'a');
         INSERT INTO t3 VALUES ('z');
         CREATE VIEW v AS SELECT id, pepe FROM t WHERE pepe <> 'x';
         CREATE VIEW other AS SELECT pepe FROM t3;
         CREATE VIEW rn_other.far AS SELECT pepe FROM rn_it.t;",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let column = || RenameTarget::Column { table: obj("table", "t"), column: "pepe".into() };
    let table = || RenameTarget::Object { object: obj("table", "t"), parent: None };

    // Upper case is refused before reaching the server.
    assert!(matches!(rename(d, &c, column(), "Nuevo").await, Err(dbine_driver::Error::Unsupported(_))));

    let names = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    if d.info().id == "presto" {
        // Presto's memory connector renames no columns: the server says so
        // at the first statement.
        let err = rename(d, &c, column(), "nuevo pepe").await.unwrap_err().to_string();
        assert!(err.contains("RENAME COLUMN") && err.contains("not support"), "{err}");
        assert_eq!(one(s.as_mut(), "SELECT pepe FROM rn_it.v").await, "a");
    } else {
        // Column: the views that read it are listed, not put back, and
        // still name the old column; the same-named column of t3 is
        // nobody's business.
        let manual = rename(d, &c, column(), "nuevo pepe").await.unwrap();
        assert_eq!(manual, names(&["far", "v"]));
        assert_eq!(one(s.as_mut(), "SELECT \"nuevo pepe\" FROM rn_it.t").await, "a");
        assert_eq!(one(s.as_mut(), "SELECT pepe FROM rn_it.other").await, "z");
        fails(d, &c, "SELECT pepe FROM rn_it.v").await;
        fails(d, &c, "SELECT pepe FROM rn_other.far").await;
        // Back, for what follows: nothing names "nuevo pepe".
        let back = RenameTarget::Column { table: obj("table", "t"), column: "nuevo pepe".into() };
        assert!(rename(d, &c, back, "pepe").await.unwrap().is_empty());
        assert_eq!(one(s.as_mut(), "SELECT pepe FROM rn_it.v").await, "a");
    }

    // Table: the same.
    assert_eq!(rename(d, &c, table(), "t_nueva").await.unwrap(), names(&["far", "v"]));
    assert_eq!(one(s.as_mut(), "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'rn_it' AND table_name = 't'").await, "0");
    fails(d, &c, "SELECT count(*) FROM rn_it.v").await;
    fails(d, &c, "SELECT count(*) FROM rn_other.far").await;
    let back = RenameTarget::Object { object: obj("table", "t_nueva"), parent: None };
    assert!(rename(d, &c, back, "t").await.unwrap().is_empty());
    assert_eq!(one(s.as_mut(), "SELECT count(*) FROM rn_it.v").await, "1");

    // View, with a view on it: renamed through ALTER (it keeps its owner),
    // the one on it listed.
    s.execute("CREATE VIEW rn_it.w AS SELECT pepe FROM rn_it.v", 10, &mut out).await.unwrap();
    assert_eq!(rename(d, &c, RenameTarget::Object { object: obj("view", "v"), parent: None }, "v2").await.unwrap(), names(&["w"]));
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM rn_it.v2").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT count(*) FROM information_schema.views WHERE table_schema = 'rn_it' AND table_name = 'v'").await, "0");
    fails(d, &c, "SELECT pepe FROM rn_it.w").await;

    // Schema: the views that name it are listed; the ones inside it that
    // name tables unqualified are what the warning is about.
    let schema = RenameTarget::Schema { database: Some("memory".into()), schema: "rn_it".into() };
    let w = d.rename_script(&RenameRequest { target: schema.clone(), new_name: "rn_it_b".into(), table: None, definition: None }).unwrap().warnings;
    assert_eq!(w.len(), 1);
    if d.info().id == "presto" {
        let err = rename(d, &c, schema, "rn_it_b").await.unwrap_err().to_string();
        assert!(err.contains("not support"), "{err}");
        clean(s.as_mut()).await;
        return;
    }
    let manual = rename(d, &c, schema, "rn_it_b").await.unwrap();
    assert!(manual.contains(&"far".to_string()), "{manual:?}");
    assert_eq!(one(s.as_mut(), "SELECT count(*) FROM rn_it_b.t").await, "1");
    fails(d, &c, "SELECT count(*) FROM rn_other.far").await;

    clean(s.as_mut()).await;
}

/// A view left naming what was renamed no longer runs.
async fn fails(d: &dyn Driver, c: &ConnectionConfig, sql: &str) {
    let mut s = d.connect(c, None).await.unwrap();
    assert!(s.execute(sql, 10, &mut QueryOutcome::default()).await.is_err(), "{sql} still runs");
}

/// One by one: Presto has no `DROP SCHEMA … CASCADE`.
async fn clean(s: &mut dyn Session) {
    for schema in ["rn_it", "rn_it_b", "rn_other"] {
        for v in ["v", "v2", "w", "other", "far"] {
            let _ = s.execute(&format!("DROP VIEW IF EXISTS memory.{schema}.{v}"), 10, &mut QueryOutcome::default()).await;
        }
        for t in ["t", "t3", "t_nueva"] {
            let _ = s.execute(&format!("DROP TABLE IF EXISTS memory.{schema}.{t}"), 10, &mut QueryOutcome::default()).await;
        }
        let _ = s.execute(&format!("DROP SCHEMA IF EXISTS memory.{schema}"), 10, &mut QueryOutcome::default()).await;
    }
}
