//! "Renombrar…" against a real Amazon Athena (there is no local emulator),
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_ATHENA_DATABASE=dbine_test \
//! DBINE_TEST_ATHENA_LOCATION=s3://my-bucket/dbine-test/ \
//! DBINE_TEST_ATHENA_OUTPUT=s3://my-bucket/athena-results/ \
//! AWS_REGION=us-east-1 \
//!   cargo test -p dbine-driver-athena --test rename -- --ignored --nocapture
//! ```
//!
//! An Iceberg table, a view on it and a view on a same-named column of
//! another table; the views are rewritten with `rewrite_references` and put
//! back with `CREATE OR REPLACE VIEW` after the rename, as the app does.
//! Athena has no keys, indexes, checks or routines.

use dbine_driver::rename::{rewrite_references, with_create_style, RenameTarget, RewriteOptions};
use dbine_driver::{Confidence, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, RenameRequest, Session};
use serde_json::Value;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
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

async fn rename(d: &dyn Driver, s: &mut dyn Session, target: RenameTarget, new: &str) -> dbine_driver::Result<()> {
    let spec = d.rename_spec().unwrap();
    let dialect = d.script_dialect();
    let table = match target.table() {
        Some(t) => s.database_schema().await.unwrap().into_iter().find(|x| x.name == t.name),
        None => None,
    };
    let definition = match &target {
        RenameTarget::Object { object, .. } if object.kind == "view" => s.definition(object).await.unwrap(),
        _ => None,
    };
    let script = d.rename_script(&RenameRequest { target: target.clone(), new_name: new.into(), table, definition })?;
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&target.dependency_target(), &scan).await.unwrap();
    let mut statements = script.statements;
    for dep in report.items.iter().filter(|x| x.relation == Relation::Code && x.confidence != Confidence::Review) {
        let def = s.definition(&ObjectRef { kind: dep.kind.clone(), schema: dep.schema.clone(), name: dep.name.clone() }).await.unwrap().unwrap();
        let opts = RewriteOptions { dependent_schema: dep.schema.clone(), keep_view_columns: dep.kind == "view", ..Default::default() };
        let r = rewrite_references(&def, &dialect, &target.rewrite_target(), new, &spec, &opts);
        if r.edits.is_empty() {
            continue;
        }
        assert!(r.unresolved.is_empty(), "{}: {:?}", dep.name, r.unresolved);
        statements.push(with_create_style(&r.text, &dialect, spec.replace_for(&dep.kind)));
    }
    for st in &statements {
        s.execute(st, 10, &mut QueryOutcome::default()).await.map_err(|e| dbine_driver::Error::Query(format!("{st}: {e}")))?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn athena_rename_with_impact() {
    let (Some(db), Some(location)) = (env("DBINE_TEST_ATHENA_DATABASE"), env("DBINE_TEST_ATHENA_LOCATION")) else {
        eprintln!("DBINE_TEST_ATHENA_DATABASE / DBINE_TEST_ATHENA_LOCATION not set; skipping");
        return;
    };
    let mut cfg = ConnectionConfig { driver: "athena".into(), database: db, ..Default::default() };
    if let Some(o) = env("DBINE_TEST_ATHENA_OUTPUT") {
        cfg.options.insert("output_location".into(), o);
    }
    if let Some(r) = env("AWS_REGION") {
        cfg.options.insert("region".into(), r);
    }
    let d = dbine_driver_athena::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let location = location.trim_end_matches('/');
    let mut out = QueryOutcome::default();
    for st in ["DROP VIEW IF EXISTS rn_v", "DROP VIEW IF EXISTS rn_v2", "DROP VIEW IF EXISTS rn_w", "DROP VIEW IF EXISTS rn_other"] {
        let _ = s.execute(st, 10, &mut out).await;
    }
    for t in ["rn_t", "rn_t_nueva", "rn_t3"] {
        let _ = s.execute(&format!("DROP TABLE IF EXISTS `{t}`"), 10, &mut out).await;
    }
    for st in [
        format!("CREATE TABLE rn_t (id int, pepe string) LOCATION '{location}/rn_t/' TBLPROPERTIES ('table_type' = 'ICEBERG')"),
        format!("CREATE TABLE rn_t3 (pepe string) LOCATION '{location}/rn_t3/' TBLPROPERTIES ('table_type' = 'ICEBERG')"),
        "INSERT INTO rn_t VALUES (1, 'a')".into(),
        "INSERT INTO rn_t3 VALUES ('z')".into(),
        "CREATE VIEW rn_v AS SELECT id, pepe FROM rn_t WHERE pepe <> 'x'".into(),
        "CREATE VIEW rn_other AS SELECT pepe FROM rn_t3".into(),
    ] {
        s.execute(&st, 10, &mut out).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }

    rename(d.as_ref(), s.as_mut(), RenameTarget::Column { table: obj("table", "rn_t"), column: "pepe".into() }, "nuevo").await.unwrap();
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM rn_v").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM rn_other").await, "z");

    rename(d.as_ref(), s.as_mut(), RenameTarget::Object { object: obj("table", "rn_t"), parent: None }, "rn_t_nueva").await.unwrap();
    assert_eq!(one(s.as_mut(), "SELECT count(*) FROM rn_v").await, "1");

    s.execute("CREATE VIEW rn_w AS SELECT pepe FROM rn_v", 10, &mut out).await.unwrap();
    rename(d.as_ref(), s.as_mut(), RenameTarget::Object { object: obj("view", "rn_v"), parent: None }, "rn_v2").await.unwrap();
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM rn_w").await, "a");

    for st in ["DROP VIEW rn_w", "DROP VIEW rn_v2", "DROP VIEW rn_other", "DROP TABLE `rn_t_nueva`", "DROP TABLE `rn_t3`"] {
        s.execute(st, 10, &mut out).await.unwrap();
    }
}
