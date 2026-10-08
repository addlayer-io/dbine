//! "Renombrar…" against a real Dremio OSS; run `integration.rs` once first
//! so the user exists:
//!
//! ```sh
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 cargo test -p dbine-driver-dremio --test rename -- --ignored
//! ```
//!
//! Dremio doesn't follow views: they are rewritten with
//! `rewrite_references` and put back with `CREATE OR REPLACE VIEW` after
//! the rename, statement by statement, as the app does. The tables are
//! Iceberg tables in `$scratch`; the views live in a space of their own.
//! Dremio has no keys, indexes, checks or routines.

use dbine_driver::rename::{rewrite_references, with_create_style, RenameTarget, RewriteOptions};
use dbine_driver::{Confidence, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, RenameRequest, Session};
use serde_json::Value;

const SPACE: &str = "dbine_rename_sp";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        password: Some("secreto123".into()),
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

fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some(schema.into()), name: name.into() }
}

/// The driver's rename and the rewritten dependents found in the space.
/// Returns the dependents left to the user.
async fn rename(d: &dyn Driver, c: &ConnectionConfig, target: RenameTarget, new: &str) -> dbine_driver::Result<Vec<String>> {
    let spec = d.rename_spec().unwrap();
    let dialect = d.script_dialect();
    // The table's own container, as the app reads it.
    let table = match target.table() {
        Some(t) => {
            let mut s = d.connect(c, t.schema()).await.unwrap();
            s.database_schema().await.unwrap().into_iter().find(|x| x.name == t.name)
        }
        None => None,
    };
    let mut s = d.connect(c, Some(SPACE)).await.unwrap();
    let definition = match &target {
        RenameTarget::Object { object, .. } => s.definition(object).await.unwrap(),
        _ => None,
    };
    let script = d.rename_script(&RenameRequest { target: target.clone(), new_name: new.into(), table, definition })?;
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
        if r.edits.is_empty() {
            continue;
        }
        assert!(r.unresolved.is_empty(), "{}: {:?}", dep.name, r.unresolved);
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
async fn dremio_rename_with_impact() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_dremio::drivers().remove(0);
    let mut s = d.connect(&c, Some("$scratch")).await.unwrap();
    let _ = s.drop_database(SPACE).await;
    s.create_database(SPACE).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        &format!(
            "DROP TABLE IF EXISTS \"$scratch\".rn_t; DROP TABLE IF EXISTS \"$scratch\".rn_t3;
             CREATE TABLE \"$scratch\".rn_t (id INT, pepe VARCHAR);
             CREATE TABLE \"$scratch\".rn_t3 (pepe VARCHAR);
             INSERT INTO \"$scratch\".rn_t VALUES (1, 'a');
             INSERT INTO \"$scratch\".rn_t3 VALUES ('z');
             CREATE VIEW {SPACE}.v AS SELECT id, pepe FROM \"$scratch\".rn_t WHERE pepe <> 'x';
             CREATE VIEW {SPACE}.other AS SELECT pepe FROM \"$scratch\".rn_t3;"
        ),
        10,
        &mut out,
    )
    .await
    .unwrap();

    // Column: the view keeps its output name, the same-named column of
    // rn_t3 is left alone.
    let column = RenameTarget::Column { table: obj("table", "$scratch", "rn_t"), column: "pepe".into() };
    let manual = rename(d.as_ref(), &c, column, "Nuevo Pepe").await.unwrap();
    assert!(manual.is_empty(), "{manual:?}");
    assert_eq!(one(s.as_mut(), &format!("SELECT pepe FROM {SPACE}.v")).await, "a");
    assert_eq!(one(s.as_mut(), &format!("SELECT pepe FROM {SPACE}.other")).await, "z");
    assert_eq!(one(s.as_mut(), "SELECT \"Nuevo Pepe\" FROM \"$scratch\".rn_t").await, "a");

    // A table is refused before reaching the server.
    let table = RenameTarget::Object { object: obj("table", "$scratch", "rn_t"), parent: None };
    assert!(matches!(rename(d.as_ref(), &c, table, "rn_u").await, Err(dbine_driver::Error::Unsupported(_))));

    // View, with a view on it: created again with the new name.
    s.execute(&format!("CREATE VIEW {SPACE}.w AS SELECT pepe FROM {SPACE}.v"), 10, &mut out).await.unwrap();
    rename(d.as_ref(), &c, RenameTarget::Object { object: obj("view", SPACE, "v"), parent: None }, "Ventas v2").await.unwrap();
    assert_eq!(one(s.as_mut(), &format!("SELECT pepe FROM {SPACE}.w")).await, "a");
    assert_eq!(one(s.as_mut(), &format!("SELECT count(*) FROM {SPACE}.\"Ventas v2\"")).await, "1");
    assert_eq!(
        one(s.as_mut(), &format!("SELECT count(*) FROM INFORMATION_SCHEMA.\"TABLES\" WHERE TABLE_SCHEMA = '{SPACE}' AND TABLE_NAME = 'v'")).await,
        "0"
    );

    s.execute("DROP TABLE \"$scratch\".rn_t; DROP TABLE \"$scratch\".rn_t3", 10, &mut out).await.unwrap();
    s.drop_database(SPACE).await.unwrap();
}
