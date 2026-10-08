//! "Renombrar con impacto" against a real Firebird server: a column renamed
//! the way the app does it (`rename_impact` → `rename_script`, run
//! statement by statement), then the dependents checked. Firebird refuses the rename
//! while anything uses the column, so the rewritten code is dropped first
//! and created after it with its `CREATE OR ALTER` definition.
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test rename -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::{Confidence, DependencyScan, Relation};
use dbine_driver::rename::{rewrite_references, with_create_style, RewriteOptions};
use dbine_driver::{kinds, ConnectionConfig, Driver, Error, ObjectRef, QueryOutcome, RenameRequest, RenameTarget, Session, TableSchema};
use serde_json::Value;
use std::sync::Arc;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url.strip_prefix("firebird://").expect("firebird://user:pass@host:port/path");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, path) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) -> Option<String> {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 100, &mut out).await {
        Err(e) => Some(e.to_string()),
        Ok(()) => out.error.take().map(|e| e.to_string()),
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    if let Some(e) = try_run(s, sql).await {
        panic!("{sql}: {e}");
    }
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> Value {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results.first().and_then(|r| r.rows.first()).and_then(|r| r.first()).cloned().unwrap_or(Value::Null)
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: None, name: name.into() }
}

async fn schema_of(s: &mut Box<dyn Session>, name: &str) -> TableSchema {
    s.database_schema().await.unwrap().into_iter().find(|t| t.name == name).unwrap()
}

async fn columns(s: &mut Box<dyn Session>, name: &str) -> Vec<String> {
    schema_of(s, name).await.columns.into_iter().map(|c| c.name).collect()
}

const CLEANUP: &[&str] = &[
    "DROP TRIGGER RN_TRG",
    "DROP PROCEDURE RN_P_USA",
    "DROP PROCEDURE RN_P_DIN",
    "DROP PROCEDURE RN_P_OTRA",
    "DROP VIEW RN_V",
    "DROP TABLE RN_T2",
    "DROP TABLE RN_T",
    "DROP TABLE RN_OTRA",
];

const FIXTURE: &[&str] = &[
    "CREATE TABLE RN_T (ID INTEGER NOT NULL PRIMARY KEY, PEPE INTEGER CONSTRAINT RN_CK_PEPE CHECK (PEPE >= 0))",
    "CREATE TABLE RN_T2 (ID INTEGER NOT NULL PRIMARY KEY, T_ID INTEGER CONSTRAINT RN_FK_T2_T REFERENCES RN_T (ID))",
    "CREATE INDEX RN_IX_PEPE ON RN_T (PEPE)",
    "CREATE TABLE RN_OTRA (ID INTEGER NOT NULL PRIMARY KEY, PEPE INTEGER)",
    "INSERT INTO RN_T VALUES (1, 10)",
    "INSERT INTO RN_OTRA VALUES (1, 5)",
    "CREATE VIEW RN_V AS SELECT ID, PEPE FROM RN_T",
    "CREATE PROCEDURE RN_P_USA AS\nBEGIN\n  UPDATE RN_T SET PEPE = PEPE + 1;\nEND",
    "CREATE PROCEDURE RN_P_DIN AS\nBEGIN\n  EXECUTE STATEMENT 'UPDATE RN_T SET PEPE = 0';\nEND",
    "CREATE PROCEDURE RN_P_OTRA AS\nBEGIN\n  UPDATE RN_OTRA SET PEPE = PEPE + 1;\nEND",
    "CREATE TRIGGER RN_TRG FOR RN_T BEFORE INSERT AS\nBEGIN\n  IF (NEW.PEPE IS NULL) THEN NEW.PEPE = 0;\nEND",
];

/// What the dialog would show and run.
struct Plan {
    statements: Vec<String>,
    rewritten: Vec<String>,
    manual: Vec<String>,
}

/// `rename_impact` + `rename_script`: the scan, each Confirmed/Probable
/// code dependent rewritten (the clean ones kept, as the dialog
/// preselects), dropped before the driver's rename and created after it
/// (`DropCreate`), triggers and procedures dropped before the views.
async fn plan(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, target: RenameTarget, new: &str) -> Plan {
    let spec = d.rename_spec().unwrap();
    assert!(spec.allows(&target));
    let dialect = d.script_dialect();
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&target.dependency_target(), &scan).await.unwrap();
    let rewrite_target = target.rewrite_target();
    let (mut rewritten, mut manual, mut drops, mut creates) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for dep in &report.items {
        if dep.relation != Relation::Code {
            continue;
        }
        if dep.confidence == Confidence::Review {
            manual.push(dep.name.clone());
            continue;
        }
        let def = s.definition(&ObjectRef { kind: dep.kind.clone(), schema: dep.schema.clone(), name: dep.name.clone() }).await.unwrap().unwrap();
        let opts = RewriteOptions { dependent_schema: dep.schema.clone(), keep_view_columns: dep.kind == kinds::VIEW, ..Default::default() };
        let r = rewrite_references(&def, &dialect, &rewrite_target, new, &spec, &opts);
        if r.edits.is_empty() || !r.unresolved.is_empty() {
            eprintln!("manual {}: {:?}", dep.name, r.unresolved);
            manual.push(dep.name.clone());
            continue;
        }
        rewritten.push(dep.name.clone());
        let drop = format!("DROP {} \"{}\"", dep.kind.to_uppercase(), dep.name);
        if dep.kind == kinds::VIEW {
            drops.push(drop);
            creates.insert(0, with_create_style(&r.text, &dialect, spec.replace));
        } else {
            drops.insert(0, drop);
            creates.push(with_create_style(&r.text, &dialect, spec.replace));
        }
    }
    let table = match target.table() {
        Some(t) => Some(schema_of(s, &t.name).await),
        None => None,
    };
    let middle = d.rename_script(&RenameRequest { target, new_name: new.into(), table, definition: None }).unwrap();
    let statements = drops.into_iter().chain(middle.statements).chain(creates).collect();
    rewritten.sort();
    manual.sort();
    Plan { statements, rewritten, manual }
}

/// Runs the statements in one transaction, as the app's atomic run would:
/// committed when all pass, rolled back at the first error, which it returns.
async fn run_atomic(s: &mut Box<dyn Session>, statements: &[String]) -> Option<String> {
    s.set_autocommit(false).await.unwrap();
    let mut failed = None;
    for st in statements {
        eprintln!("--\n{st}");
        if let Some(e) = try_run(s, st).await {
            failed = Some(format!("{st}: {e}"));
            break;
        }
    }
    match failed {
        None => s.commit().await.unwrap(),
        Some(_) => s.rollback().await.unwrap(),
    }
    s.set_autocommit(true).await.unwrap();
    failed
}

#[tokio::test]
#[ignore]
async fn rename_column_with_impact_live() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    for st in CLEANUP {
        let _ = try_run(&mut s, st).await;
    }
    for st in FIXTURE {
        run(&mut s, st).await;
    }
    let pepe = || RenameTarget::Column { table: table("RN_T"), column: "PEPE".into() };

    // 1. Only columns: no table rename.
    assert!(matches!(d.rename_script(&RenameRequest { target: RenameTarget::Object { object: table("RN_T"), parent: None }, new_name: "X".into(), table: None, definition: None }), Err(Error::Unsupported(_))));

    // 2. A CHECK uses the column: the driver refuses before the server would.
    let t = schema_of(&mut s, "RN_T").await;
    let refused = d.rename_script(&RenameRequest { target: pepe(), new_name: "NOMBRE".into(), table: Some(t.clone()), definition: None });
    assert!(matches!(&refused, Err(Error::Unsupported(m)) if m.contains("CHECK")), "{refused:?}");
    // The server's own answer (it names the first user it finds).
    let e = try_run(&mut s, "ALTER TABLE RN_T ALTER COLUMN PEPE TO NOMBRE").await.expect("refused");
    eprintln!("server, CHECK: {e}");
    assert!(e.contains("is referenced in"), "{e}");
    // A primary key column: refused as well.
    let id = RenameTarget::Column { table: table("RN_T"), column: "ID".into() };
    assert!(matches!(d.rename_script(&RenameRequest { target: id, new_name: "X".into(), table: Some(t), definition: None }), Err(Error::Unsupported(m)) if m.contains("clave")));
    // A foreign key column nothing else uses: the key alone blocks it.
    let e = try_run(&mut s, "ALTER TABLE RN_T2 ALTER COLUMN T_ID TO T_ID2").await.expect("refused");
    eprintln!("server, key: {e}");
    assert!(e.contains("Integrity Constraint"), "{e}");
    run(&mut s, "ALTER TABLE RN_T DROP CONSTRAINT RN_CK_PEPE").await;

    // 3. With the CHECK gone, the view, the procedure and the trigger still block it.
    let e = try_run(&mut s, "ALTER TABLE RN_T ALTER COLUMN PEPE TO NOMBRE").await.expect("refused");
    eprintln!("server, code: {e}");
    assert!(e.contains("referenced in RN_"), "{e}");

    // 4. In one transaction it can't work: the drops take effect at commit
    //    and the rename, checked at once, still sees the view.
    let p = plan(&d, &mut s, pepe(), "NOMBRE").await;
    let e = run_atomic(&mut s, &p.statements).await.expect("refused in one transaction");
    assert!(e.contains("is referenced in"), "{e}");
    assert!(columns(&mut s, "RN_T").await.contains(&"PEPE".to_string()), "rolled back");
    assert_eq!(scalar(&mut s, "SELECT PEPE FROM RN_V WHERE ID = 1").await, Value::from(10));
    assert!(!d.rename_spec().unwrap().transactional);

    // 5. The real run, statement by statement: the dependents dropped, the
    //    column renamed, the dependents created again with the new name.
    //    The trigger on the table names it as NEW.PEPE: rewritten too.
    assert_eq!(p.manual, ["RN_P_DIN"], "dynamic SQL is left to the user");
    assert_eq!(p.rewritten, ["RN_P_USA", "RN_TRG", "RN_V"]);
    for st in &p.statements {
        eprintln!("--\n{st}");
        run(&mut s, st).await;
    }
    let cols = columns(&mut s, "RN_T").await;
    assert!(cols.contains(&"NOMBRE".to_string()) && !cols.contains(&"PEPE".to_string()), "{cols:?}");
    // The view keeps its output column (`NOMBRE AS PEPE`).
    assert_eq!(scalar(&mut s, "SELECT PEPE FROM RN_V WHERE ID = 1").await, Value::from(10));
    run(&mut s, "EXECUTE PROCEDURE RN_P_USA").await;
    run(&mut s, "EXECUTE PROCEDURE RN_P_OTRA").await;
    run(&mut s, "INSERT INTO RN_T (ID) VALUES (2)").await;
    assert_eq!(scalar(&mut s, "SELECT NOMBRE FROM RN_T WHERE ID = 1").await, Value::from(11));
    assert_eq!(scalar(&mut s, "SELECT NOMBRE FROM RN_T WHERE ID = 2").await, Value::from(0), "the trigger runs");
    // The other table's procedure wasn't touched.
    assert_eq!(scalar(&mut s, "SELECT PEPE FROM RN_OTRA WHERE ID = 1").await, Value::from(6));
    // The plain index followed by itself.
    assert_eq!(
        scalar(&mut s, "SELECT TRIM(RDB$FIELD_NAME) FROM RDB$INDEX_SEGMENTS WHERE RDB$INDEX_NAME = 'RN_IX_PEPE'").await,
        Value::from("NOMBRE")
    );
    // The dynamic one now fails: it's the user's to fix.
    assert!(try_run(&mut s, "EXECUTE PROCEDURE RN_P_DIN").await.is_some());

    for st in CLEANUP {
        let _ = try_run(&mut s, st).await;
    }
}
