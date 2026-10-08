//! "Renombrar…" against the Cloud Spanner emulator, through the app's
//! steps: the dependents ("Ver dependencias"), each one rewritten
//! (`rewrite_references`) and dropped before the rename and created after
//! it (`DropCreate`: Spanner refuses to rename a table a view uses), then
//! the dependents queried.
//!
//! Fixture: `T(id PK, pepe)` with a CHECK and an index on `pepe`, `T2` with
//! a foreign key to `T` (dropped before the table rename: the emulator
//! can't rename a table on either side of one, Spanner can), the view `V` on `T`, `V2` on `V`, and `V3` reading
//! the same-named column of `T3`. Spanner has no routines.
//!
//! ```sh
//! docker start dbine-test-spanner   # -p 25303:9020
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 cargo test -p dbine-driver-spanner --test rename -- --ignored --nocapture
//! ```

use dbine_driver::rename::{rewrite_references, with_create_style, RewriteOptions};
use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, RenameRequest, RenameTarget, ReplaceStyle, Session,
};
use serde_json::json;
use std::sync::Arc;

async fn connect(url: &str, db: &str) -> (Arc<dyn Driver>, Box<dyn Session>) {
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http.delete(format!("{url}/v1/projects/test/instances/i1/databases/{db}")).send().await;
    http.post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": format!("CREATE DATABASE `{db}`") }))
        .send()
        .await
        .unwrap();
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: db.into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url)] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    let s = d.connect(&cfg, None).await.unwrap();
    (d, s)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(format!("{e:?}")),
        None => Ok(out),
    }
}

async fn ok(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The first column of each row, as text.
async fn col(s: &mut Box<dyn Session>, sql: &str) -> Vec<String> {
    let out = ok(s, sql).await;
    let rs = out.results.iter().find(|r| !r.columns.is_empty()).unwrap_or_else(|| panic!("{sql}: no rows"));
    rs.rows.iter().map(|r| r[0].as_str().map(str::to_string).unwrap_or_else(|| r[0].to_string())).collect()
}

async fn setup(s: &mut Box<dyn Session>) {
    for st in [
        "CREATE TABLE T (id INT64 NOT NULL, pepe STRING(20), CONSTRAINT ck_pepe CHECK (pepe != '')) PRIMARY KEY (id)",
        "CREATE INDEX ix_pepe ON T (pepe)",
        "CREATE TABLE T2 (id INT64 NOT NULL, t_id INT64, CONSTRAINT fk_t FOREIGN KEY (t_id) REFERENCES T (id)) PRIMARY KEY (id)",
        "CREATE TABLE T3 (id INT64 NOT NULL, pepe STRING(20)) PRIMARY KEY (id)",
        "CREATE VIEW V SQL SECURITY INVOKER AS SELECT T.id AS id, T.pepe AS pepe FROM T",
        "CREATE VIEW V2 SQL SECURITY INVOKER AS SELECT V.pepe AS pepe FROM V",
        "CREATE VIEW V3 SQL SECURITY INVOKER AS SELECT T3.pepe AS pepe FROM T3",
        "INSERT INTO T (id, pepe) VALUES (1, 'uno'), (2, 'dos')",
        "INSERT INTO T2 (id, t_id) VALUES (10, 1)",
        "INSERT INTO T3 (id, pepe) VALUES (1, 'otra')",
    ] {
        ok(s, st).await;
    }
}

struct Plan {
    statements: Vec<String>,
    rewritten: Vec<String>,
}

/// What the app does: the dependents rewritten and dropped, the rename,
/// the rewrites created again.
async fn plan(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, mut req: RenameRequest) -> Plan {
    let spec = d.rename_spec().expect("spec");
    assert!(spec.allows(&req.target), "{:?}", req.target);
    let dialect = d.script_dialect();
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&req.target.dependency_target(), &scan).await.unwrap();
    eprintln!("dependents: {:?}", report.items.iter().map(|x| (&x.kind, &x.name, x.relation, x.confidence)).collect::<Vec<_>>());
    if let RenameTarget::Object { object, .. } = &req.target {
        if object.kind != kinds::TABLE {
            req.definition = s.definition(object).await.unwrap();
        }
    }
    let (mut rewritten, mut drops, mut creates) = (Vec::new(), Vec::new(), Vec::new());
    for dep in report.items.iter().filter(|x| x.relation == Relation::Code) {
        assert_ne!(dep.confidence, Confidence::Review, "{}", dep.name);
        let body = s.definition(&ObjectRef { kind: dep.kind.clone(), schema: dep.schema.clone(), name: dep.name.clone() }).await.unwrap().unwrap();
        let opts = RewriteOptions { dependent_schema: dep.schema.clone(), keep_view_columns: dep.kind == kinds::VIEW, ..Default::default() };
        let r = rewrite_references(&body, &dialect, &req.target.rewrite_target(), &req.new_name, &spec, &opts);
        eprintln!("{} -> {} (unresolved {:?})", dep.name, r.text, r.unresolved);
        assert!(!r.edits.is_empty(), "{} not rewritten", dep.name);
        rewritten.push(dep.name.clone());
        assert_eq!(spec.replace_for(&dep.kind), ReplaceStyle::DropCreate);
        drops.push(format!("DROP VIEW `{}`", dep.name));
        creates.push(with_create_style(&r.text, &dialect, ReplaceStyle::DropCreate).trim().to_string());
    }
    let middle = d.rename_script(&req).unwrap();
    eprintln!("warnings: {:?}", middle.warnings);
    let statements = drops.into_iter().chain(middle.statements).chain(creates).collect();
    Plan { statements, rewritten }
}

async fn apply(s: &mut Box<dyn Session>, statements: &[String]) {
    for st in statements {
        eprintln!("> {st}");
        ok(s, st.trim_end_matches(';')).await;
    }
}

fn object(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

#[tokio::test]
#[ignore]
async fn renames_a_table_and_its_views_follow() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let (d, mut s) = connect(&url, "rename_t").await;
    setup(&mut s).await;

    // The engine refuses it alone while V names T.
    assert!(run(&mut s, "ALTER TABLE T RENAME TO T_nuevo").await.is_err());
    // The emulator can't rename a table on either side of a foreign key
    // (GOOGLESQL_RET_CHECK in its foreign_key_validator); Spanner moves
    // them. So the key goes before the test here.
    ok(&mut s, "DROP VIEW `V2`").await;
    ok(&mut s, "DROP VIEW `V`").await;
    let err = run(&mut s, "ALTER TABLE T RENAME TO T_x").await.unwrap_err();
    assert!(err.contains("RET_CHECK"), "{err}");
    ok(&mut s, "ALTER TABLE T2 DROP CONSTRAINT fk_t").await;
    ok(&mut s, "CREATE VIEW V SQL SECURITY INVOKER AS SELECT T.id AS id, T.pepe AS pepe FROM T").await;
    ok(&mut s, "CREATE VIEW V2 SQL SECURITY INVOKER AS SELECT V.pepe AS pepe FROM V").await;

    let req = RenameRequest {
        target: RenameTarget::Object { object: object(kinds::TABLE, "T"), parent: None },
        new_name: "T_nuevo".into(),
        table: None,
        definition: None,
    };
    let p = plan(&d, &mut s, req).await;
    // V names T; V2 only V, and V3 the same-named column of T3.
    assert_eq!(p.rewritten, ["V"]);
    // V2 depends on V: drop it too, as the app's plan would, or V can't go.
    let mut statements = vec!["DROP VIEW `V2`".to_string()];
    statements.extend(p.statements);
    statements.push("CREATE VIEW V2 SQL SECURITY INVOKER AS SELECT V.pepe AS pepe FROM V".into());
    apply(&mut s, &statements).await;

    assert_eq!(col(&mut s, "SELECT pepe FROM V ORDER BY id").await, ["uno", "dos"]);
    assert_eq!(col(&mut s, "SELECT pepe FROM V2 ORDER BY pepe").await, ["dos", "uno"]);
    assert_eq!(col(&mut s, "SELECT pepe FROM V3").await, ["otra"]);
    // Index and CHECK followed the table.
    assert_eq!(col(&mut s, "SELECT table_name FROM INFORMATION_SCHEMA.INDEXES WHERE index_name = 'ix_pepe'").await, ["T_nuevo"]);
    assert!(run(&mut s, "INSERT INTO T_nuevo (id, pepe) VALUES (3, '')").await.is_err(), "CHECK kept");
    ok(&mut s, "INSERT INTO T_nuevo (id, pepe) VALUES (3, 'tres')").await;
    assert!(run(&mut s, "SELECT 1 FROM T").await.is_err());
}

#[tokio::test]
#[ignore]
async fn renames_a_view_by_creating_it_again() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let (d, mut s) = connect(&url, "rename_v").await;
    setup(&mut s).await;
    ok(&mut s, "CREATE VIEW VD SQL SECURITY DEFINER AS SELECT T3.pepe AS pepe FROM T3").await;

    let req = RenameRequest {
        target: RenameTarget::Object { object: object(kinds::VIEW, "V"), parent: None },
        new_name: "Vista_Nueva".into(),
        table: None,
        definition: None,
    };
    let p = plan(&d, &mut s, req).await;
    assert_eq!(p.rewritten, ["V2"]);
    apply(&mut s, &p.statements).await;
    assert_eq!(col(&mut s, "SELECT pepe FROM Vista_Nueva ORDER BY id").await, ["uno", "dos"]);
    assert_eq!(col(&mut s, "SELECT pepe FROM V2 ORDER BY pepe").await, ["dos", "uno"]);
    assert!(run(&mut s, "SELECT 1 FROM V").await.is_err());

    // A DEFINER view stays DEFINER.
    let req = RenameRequest {
        target: RenameTarget::Object { object: object(kinds::VIEW, "VD"), parent: None },
        new_name: "VD2".into(),
        table: None,
        definition: None,
    };
    let p = plan(&d, &mut s, req).await;
    apply(&mut s, &p.statements).await;
    assert_eq!(col(&mut s, "SELECT security_type FROM INFORMATION_SCHEMA.VIEWS WHERE table_name = 'VD2'").await, ["DEFINER"]);
}
