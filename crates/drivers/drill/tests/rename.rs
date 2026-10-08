//! "Renombrar…" against a real Drill (views in `dfs.tmp`):
//!
//! ```sh
//! docker run -d --name dbine-test-drill -p 25847:8047 apache/drill
//! DBINE_TEST_DRILL_URL=http://localhost:25847 cargo test -p dbine-driver-drill --test rename -- --ignored
//! ```
//!
//! Drill renames only views, by creating them again: the views on them are
//! rewritten with `rewrite_references` and put back with `CREATE OR
//! REPLACE VIEW` after the rename, statement by statement, as the app does.
//! Tables (files), columns and workspaces are refused. The views on the
//! renamed one name it every way Drill takes: `dfs.tmp`.rn_v, dfs.tmp.rn_v,
//! `dfs`.`tmp`.`rn_v` and bare.

use dbine_driver::rename::{rewrite_references, with_create_style, RenameTarget, RewriteOptions};
use dbine_driver::{Confidence, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, RenameRequest, Session};
use serde_json::Value;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DRILL_URL").ok()?).expect("URL");
    Some(ConnectionConfig { driver: "drill".into(), host: url.host_str()?.into(), port: url.port().unwrap_or(0), ..Default::default() })
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some("dfs.tmp".into()), name: name.into() }
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

/// The driver's rename and the rewritten dependents. Returns the
/// dependents left to the user.
async fn rename(d: &dyn Driver, s: &mut dyn Session, target: RenameTarget, new: &str) -> dbine_driver::Result<Vec<String>> {
    let spec = d.rename_spec().unwrap();
    let dialect = d.script_dialect();
    let definition = match &target {
        RenameTarget::Object { object, .. } => s.definition(object).await.unwrap(),
        _ => None,
    };
    let script = d.rename_script(&RenameRequest { target: target.clone(), new_name: new.into(), table: None, definition })?;
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&target.dependency_target(), &scan).await.unwrap();
    let mut statements = script.statements;
    let mut manual = Vec::new();
    for dep in report.items.iter().filter(|x| x.relation == Relation::Code) {
        if dep.confidence == Confidence::Review {
            manual.push(dep.name.clone());
            continue;
        }
        let def = s.definition(&ObjectRef { kind: dep.kind.clone(), schema: dep.schema.clone(), name: dep.name.clone() }).await.unwrap().unwrap();
        let opts = RewriteOptions { dependent_schema: dep.schema.clone(), keep_view_columns: dep.kind == "view", ..Default::default() };
        let r = rewrite_references(&def, &dialect, &target.rewrite_target(), new, &spec, &opts);
        assert!(r.unresolved.is_empty(), "{}: {:?}", dep.name, r.unresolved);
        assert!(!r.edits.is_empty(), "{} names the view but wasn't rewritten: {def}", dep.name);
        statements.push(with_create_style(&r.text, &dialect, spec.replace_for(&dep.kind)));
    }
    for st in &statements {
        s.execute(st, 10, &mut QueryOutcome::default()).await.map_err(|e| dbine_driver::Error::Query(format!("{st}: {e}")))?;
    }
    manual.sort();
    Ok(manual)
}

#[tokio::test]
#[ignore]
async fn drill_rename_with_impact() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_drill::drivers().remove(0);
    let mut s = d.connect(&c, Some("dfs.tmp")).await.unwrap();
    let mut out = QueryOutcome::default();
    for v in ["rn_v", "rn_w", "rn_far", "rn_dots", "rn_ticks", "rn_other", "rn v2"] {
        let _ = s.execute(&format!("DROP VIEW IF EXISTS `dfs.tmp`.`{v}`"), 10, &mut out).await;
    }
    let _ = s.execute("DROP TABLE IF EXISTS `dfs.tmp`.`rn_t`", 10, &mut out).await;
    s.execute(
        "CREATE TABLE `dfs.tmp`.`rn_t` AS SELECT 1 AS id, 'a' AS pepe FROM (VALUES(1));
         CREATE VIEW `dfs.tmp`.`rn_v` AS SELECT id, pepe FROM `dfs.tmp`.`rn_t` WHERE pepe <> 'x';
         CREATE VIEW rn_w AS SELECT pepe FROM rn_v;
         CREATE VIEW `dfs.tmp`.`rn_far` AS SELECT pepe FROM `dfs.tmp`.rn_v;
         CREATE VIEW `dfs.tmp`.`rn_dots` AS SELECT pepe FROM dfs.tmp.rn_v;
         CREATE VIEW `dfs.tmp`.`rn_ticks` AS SELECT a.pepe FROM `dfs`.`tmp`.`rn_v` a JOIN dfs.tmp.rn_t b ON a.id = b.id;
         CREATE VIEW rn_other AS SELECT pepe FROM rn_t;",
        10,
        &mut out,
    )
    .await
    .unwrap();

    // Tables and columns are refused before reaching the server.
    let table = RenameTarget::Object { object: obj("table", "rn_t"), parent: None };
    assert!(matches!(rename(d.as_ref(), s.as_mut(), table, "rn_u").await, Err(dbine_driver::Error::Unsupported(_))));
    let column = RenameTarget::Column { table: obj("table", "rn_t"), column: "pepe".into() };
    assert!(matches!(rename(d.as_ref(), s.as_mut(), column, "nuevo").await, Err(dbine_driver::Error::Unsupported(_))));

    // The view, with two views on it (one written unqualified).
    let manual = rename(d.as_ref(), s.as_mut(), RenameTarget::Object { object: obj("view", "rn_v"), parent: None }, "rn v2").await.unwrap();
    assert!(manual.is_empty(), "{manual:?}");
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM `dfs.tmp`.`rn_w`").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM `dfs.tmp`.`rn_far`").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM `dfs.tmp`.`rn_dots`").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM `dfs.tmp`.`rn_ticks`").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM `dfs.tmp`.`rn v2`").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT pepe FROM `dfs.tmp`.`rn_other`").await, "a");
    assert_eq!(one(s.as_mut(), "SELECT count(*) FROM INFORMATION_SCHEMA.VIEWS WHERE TABLE_SCHEMA = 'dfs.tmp' AND TABLE_NAME = 'rn_v'").await, "0");

    for v in ["rn_w", "rn_far", "rn_dots", "rn_ticks", "rn_other", "rn v2"] {
        s.execute(&format!("DROP VIEW `dfs.tmp`.`{v}`"), 10, &mut out).await.unwrap();
    }
    s.execute("DROP TABLE `dfs.tmp`.`rn_t`", 10, &mut out).await.unwrap();
}
