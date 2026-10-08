//! "Renombrar con impacto" against a real server: the driver's rename plus
//! the dependents rewritten the way the app does it (`rename_impact` →
//! `rename_script`), then the dependents checked (VALID in ALL_OBJECTS, and
//! they still run). Needs a user that can create users (SYSTEM):
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test rename -- --ignored --nocapture --test-threads=1
//! ```

use dbine_driver::dependencies::{Confidence, DependencyScan, Relation};
use dbine_driver::rename::{rewrite_references, with_create_style, RewriteOptions};
use dbine_driver::{kinds, ConnectionConfig, Driver, ObjectRef, QueryOutcome, RenameRequest, RenameTarget, Session};
use serde_json::Value;
use std::sync::Arc;

fn config_from(url: &str) -> ConnectionConfig {
    let rest = url.strip_prefix("oracle://").expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
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

const SCHEMA: &str = "DBINE_RENAME";
const PASSWORD: &str = "Rename_123";

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some(SCHEMA.into()), name: name.into() }
}

async fn status(s: &mut Box<dyn Session>, name: &str, ty: &str) -> String {
    let v = scalar(s, &format!("SELECT status FROM all_objects WHERE owner = '{SCHEMA}' AND object_name = '{name}' AND object_type = '{ty}'")).await;
    v.as_str().unwrap_or("MISSING").to_string()
}

/// What the app's dialog would show and run.
struct Applied {
    rewritten: Vec<String>,
    manual: Vec<String>,
    /// Status of the view `V…` right after the rename, before the rewrites.
    middle_done: bool,
}

/// `rename_impact` + `rename_script` + run, as the app does: the scan,
/// each Confirmed/Probable code dependent rewritten (only the clean ones are
/// kept, as the dialog preselects), the driver's rename, then each kept
/// dependent put back with `CREATE OR REPLACE`. `between` runs after the
/// rename and before the rewrites.
async fn apply(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, target: RenameTarget, new: &str, between: Option<(&str, &str)>) -> Applied {
    let spec = d.rename_spec().unwrap();
    assert!(spec.allows(&target));
    let dialect = d.script_dialect();
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&target.dependency_target(), &scan).await.unwrap();
    let rewrite_target = target.rewrite_target();
    let mut rewritten = Vec::new();
    let mut manual = Vec::new();
    let mut creates = Vec::new();
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
        } else {
            rewritten.push(dep.name.clone());
            creates.push(with_create_style(&r.text, &dialect, spec.replace_for(&dep.kind)));
        }
    }
    let definition = match &target {
        RenameTarget::Object { object, .. } if object.kind != kinds::TABLE => s.definition(object).await.unwrap(),
        _ => None,
    };
    // After the rewritten dependents: the schema compiled again.
    creates.extend(spec.epilogue_for(&target));
    let script = d.rename_script(&RenameRequest { target, new_name: new.into(), table: None, definition }).unwrap();
    for st in &script.statements {
        eprintln!("--\n{st}");
        run(s, st).await;
    }
    let mut middle_done = true;
    if let Some((name, ty)) = between {
        middle_done = status(s, name, ty).await == "INVALID";
    }
    for c in &creates {
        eprintln!("--\n{c}");
        run(s, c).await;
    }
    rewritten.sort();
    manual.sort();
    Applied { rewritten, manual, middle_done }
}

/// What isn't VALID after a step (dependents of a rewritten object that
/// weren't rewritten themselves are invalidated in cascade), then the same
/// after recompiling them: nothing may stay INVALID.
async fn settle(s: &mut Box<dyn Session>) -> Vec<String> {
    let before = invalid(s).await;
    run(s, &format!("BEGIN DBMS_UTILITY.COMPILE_SCHEMA('{SCHEMA}', FALSE); END;")).await;
    assert_eq!(invalid(s).await, Vec::<String>::new(), "recompiled");
    before
}

async fn invalid(s: &mut Box<dyn Session>) -> Vec<String> {
    let mut out = QueryOutcome::default();
    s.execute(&format!("SELECT object_name FROM all_objects WHERE owner = '{SCHEMA}' AND status <> 'VALID' ORDER BY 1"), 100, &mut out).await.unwrap();
    out.results[0].rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

const FIXTURE: &[&str] = &[
    "CREATE TABLE t (id NUMBER PRIMARY KEY, pepe NUMBER CONSTRAINT ck_t_pepe CHECK (pepe >= 0))",
    "CREATE TABLE t2 (id NUMBER PRIMARY KEY, t_id NUMBER CONSTRAINT fk_t2_t REFERENCES t (id))",
    "CREATE INDEX ix_t_pepe ON t (pepe)",
    "CREATE TABLE otra (id NUMBER PRIMARY KEY, pepe NUMBER)",
    "INSERT INTO t VALUES (1, 10)",
    "INSERT INTO otra VALUES (1, 5)",
    "CREATE VIEW v AS SELECT id, pepe FROM t",
    "CREATE OR REPLACE PROCEDURE p_usa AS\nBEGIN\n  UPDATE t SET pepe = pepe + 1;\nEND p_usa;",
    "CREATE OR REPLACE PROCEDURE p_din AS\nBEGIN\n  EXECUTE IMMEDIATE 'UPDATE t SET pepe = 0';\nEND p_din;",
    "CREATE OR REPLACE PROCEDURE p_otra AS\nBEGIN\n  UPDATE otra SET pepe = pepe + 1;\nEND p_otra;",
    "CREATE OR REPLACE PROCEDURE p_llama AS\nBEGIN\n  p_usa;\nEND p_llama;",
    "CREATE OR REPLACE PACKAGE pk AS\n  FUNCTION total RETURN NUMBER;\nEND pk;",
    "CREATE OR REPLACE PACKAGE BODY pk AS\n  FUNCTION total RETURN NUMBER IS n NUMBER;\n  BEGIN\n    SELECT SUM(pepe) INTO n FROM t;\n    RETURN n;\n  END total;\nEND pk;",
    "CREATE OR REPLACE TRIGGER trg_t BEFORE INSERT ON t FOR EACH ROW\nBEGIN\n  IF :new.pepe IS NULL THEN :new.pepe := 0; END IF;\nEND;",
    "CREATE SEQUENCE sq",
    "CREATE SYNONYM sy FOR t",
];

#[tokio::test]
#[ignore]
async fn rename_with_impact_live() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let admin_cfg = config_from(&url);
    let d = dbine_driver_oracle::drivers().remove(0);
    assert!(dbine_driver_oracle::drivers().iter().all(|d| d.rename_spec().is_some()), "Autonomous too");
    let mut admin = d.connect(&admin_cfg, None).await.expect("connect");
    let _ = try_run(&mut admin, &format!("DROP USER {SCHEMA} CASCADE")).await;
    run(&mut admin, &format!("CREATE USER {SCHEMA} IDENTIFIED BY \"{PASSWORD}\" QUOTA UNLIMITED ON users")).await;
    run(&mut admin, &format!("GRANT CREATE SESSION, CREATE TABLE, CREATE VIEW, CREATE PROCEDURE, CREATE TRIGGER, CREATE SEQUENCE, CREATE SYNONYM TO {SCHEMA}")).await;

    // Connected as the owner: RENAME works only there.
    let mut cfg = admin_cfg.clone();
    cfg.username = Some(SCHEMA.into());
    cfg.password = Some(PASSWORD.into());
    let mut s = d.connect(&cfg, Some(SCHEMA)).await.expect("connect as owner");
    for st in FIXTURE {
        run(&mut s, st).await;
    }
    assert_eq!(invalid(&mut s).await, Vec::<String>::new(), "fixture compiles");

    // 1. The table: views and code go INVALID, the rewrites make them VALID.
    let a = apply(&d, &mut s, RenameTarget::Object { object: obj("table", "T"), parent: None }, "CLIENTES", Some(("V", "VIEW"))).await;
    assert!(a.middle_done, "the view is INVALID after the rename");
    assert_eq!(a.manual, ["P_DIN"], "dynamic SQL is left to the user");
    assert_eq!(a.rewritten, ["PK", "P_USA", "SY", "TRG_T", "V"]);
    assert_eq!(status(&mut s, "V", "VIEW").await, "VALID");
    assert_eq!(scalar(&mut s, "SELECT pepe FROM v WHERE id = 1").await, Value::from(10));
    run(&mut s, "BEGIN p_usa; p_otra; END;").await;
    assert_eq!(scalar(&mut s, "SELECT pk.total FROM dual").await, Value::from(11));
    run(&mut s, "INSERT INTO clientes (id) VALUES (2)").await;
    assert_eq!(scalar(&mut s, "SELECT pepe FROM clientes WHERE id = 2").await, Value::from(0), "the trigger still fires");
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM user_constraints WHERE constraint_name = 'FK_T2_T' AND table_name = 'T2'").await, Value::from(1));
    eprintln!("invalid after the table rename: {:?}", settle(&mut s).await);
    // The dynamic one now fails at run time, as the dialog warned.
    assert!(try_run(&mut s, "BEGIN p_din; END;").await.is_some());

    // 2. The column: a lower-case name is stored quoted.
    let a = apply(
        &d,
        &mut s,
        RenameTarget::Column { table: obj("table", "CLIENTES"), column: "PEPE".into() },
        "IMPORTE",
        Some(("V", "VIEW")),
    )
    .await;
    assert!(a.middle_done);
    assert!(a.rewritten.contains(&"V".to_string()) && a.rewritten.contains(&"P_USA".to_string()), "{:?}", a.rewritten);
    assert!(!a.rewritten.iter().any(|n| n == "P_OTRA"));
    eprintln!("invalid after the column rename: {:?}", settle(&mut s).await);
    assert_eq!(scalar(&mut s, "SELECT pepe FROM v WHERE id = 1").await, Value::from(11), "the view keeps its output column");
    run(&mut s, "BEGIN p_usa; p_otra; END;").await;
    assert_eq!(scalar(&mut s, "SELECT importe FROM clientes WHERE id = 1").await, Value::from(12));
    assert_eq!(scalar(&mut s, "SELECT pepe FROM otra WHERE id = 1").await, Value::from(7));
    assert!(try_run(&mut s, "INSERT INTO clientes VALUES (3, -1)").await.is_some(), "the CHECK follows the column");
    assert_eq!(scalar(&mut s, "SELECT column_name FROM user_ind_columns WHERE index_name = 'IX_T_PEPE'").await, Value::from("IMPORTE"));

    // 3. Index, constraint, trigger.
    apply(&d, &mut s, RenameTarget::Index { table: obj("table", "CLIENTES"), index: "IX_T_PEPE".into() }, "IX_CLIENTES_IMPORTE", None).await;
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM user_indexes WHERE index_name = 'IX_CLIENTES_IMPORTE'").await, Value::from(1));
    apply(&d, &mut s, RenameTarget::Constraint { table: obj("table", "CLIENTES"), constraint: "CK_T_PEPE".into() }, "CK_Importe", None).await;
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM user_constraints WHERE constraint_name = 'CK_Importe'").await, Value::from(1));
    apply(&d, &mut s, RenameTarget::Object { object: obj("trigger", "TRG_T"), parent: Some("CLIENTES".into()) }, "TRG_CLIENTES", None).await;
    assert_eq!(status(&mut s, "TRG_CLIENTES", "TRIGGER").await, "VALID");

    // 4. View, sequence, synonym with RENAME (own schema).
    run(&mut s, "CREATE VIEW v_sobre_v AS SELECT id FROM v").await;
    let a = apply(&d, &mut s, RenameTarget::Object { object: obj("view", "V"), parent: None }, "V_CLIENTES", Some(("V_SOBRE_V", "VIEW"))).await;
    assert!(a.middle_done);
    assert_eq!(a.rewritten, ["V_SOBRE_V"]);
    assert_eq!(status(&mut s, "V_SOBRE_V", "VIEW").await, "VALID");
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM v_sobre_v").await, Value::from(2));
    assert_eq!(scalar(&mut s, "SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual").await, Value::from(SCHEMA), "current schema kept");
    apply(&d, &mut s, RenameTarget::Object { object: obj("sequence", "SQ"), parent: None }, "SQ_CLIENTES", None).await;
    assert_eq!(status(&mut s, "SQ_CLIENTES", "SEQUENCE").await, "VALID");
    apply(&d, &mut s, RenameTarget::Object { object: obj("synonym", "SY"), parent: None }, "SY_CLIENTES", None).await;
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM sy_clientes").await, Value::from(2));

    // Another user: the block refuses with its message and changes nothing.
    let mut other = d.connect(&admin_cfg, Some(SCHEMA)).await.expect("connect as admin");
    let script = d
        .rename_script(&RenameRequest {
            target: RenameTarget::Object { object: obj("view", "V_CLIENTES"), parent: None },
            new_name: "V_X".into(),
            table: None,
            definition: None,
        })
        .unwrap();
    let e = try_run(&mut other, &script.statements[0]).await.expect("refused");
    assert!(e.contains("propio esquema"), "{e}");
    assert_eq!(status(&mut s, "V_CLIENTES", "VIEW").await, "VALID");

    // 5. Procedure and package: created again, callers rewritten.
    let a = apply(&d, &mut s, RenameTarget::Object { object: obj("procedure", "P_USA"), parent: None }, "SUMAR_IMPORTE", None).await;
    assert_eq!(a.rewritten, ["P_LLAMA"]);
    assert_eq!(status(&mut s, "P_USA", "PROCEDURE").await, "MISSING");
    assert_eq!(status(&mut s, "SUMAR_IMPORTE", "PROCEDURE").await, "VALID");
    run(&mut s, "BEGIN p_llama; END;").await;
    assert_eq!(scalar(&mut s, "SELECT importe FROM clientes WHERE id = 1").await, Value::from(13));
    run(&mut s, "CREATE OR REPLACE FUNCTION usa_pk RETURN NUMBER AS BEGIN RETURN pk.total; END usa_pk;").await;
    let a = apply(&d, &mut s, RenameTarget::Object { object: obj("package", "PK"), parent: None }, "PK_CLIENTES", None).await;
    assert_eq!(a.rewritten, ["USA_PK"]);
    assert_eq!(status(&mut s, "PK_CLIENTES", "PACKAGE").await, "VALID");
    assert_eq!(status(&mut s, "PK_CLIENTES", "PACKAGE BODY").await, "VALID");
    assert_eq!(scalar(&mut s, "SELECT usa_pk FROM dual").await, Value::from(15), "13 + 2: p_usa updates both rows");

    eprintln!("invalid at the end: {:?}", settle(&mut s).await);

    drop(s);
    drop(other);
    run(&mut admin, &format!("DROP USER {SCHEMA} CASCADE")).await;
}
