//! The "Renombrar…" flow shared by the SQLite and libSQL tests (same SQL):
//! the driver's statements run in one transaction, as the app runs them,
//! and what depends on the target keeps working.

use dbine_driver::rename::RenameTarget;
use dbine_driver::{DependencyScan, Driver, ObjectRef, QueryOutcome, RenameRequest, Session};
use serde_json::Value;

async fn run(s: &mut dyn Session, statements: &[String]) -> dbine_driver::Result<()> {
    let mut out = QueryOutcome::default();
    s.execute("BEGIN", 10, &mut out).await?;
    for st in statements {
        if let Err(e) = s.execute(st, 10, &mut out).await {
            s.execute("ROLLBACK", 10, &mut out).await.unwrap();
            return Err(e);
        }
    }
    s.execute("COMMIT", 10, &mut out).await
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

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: None, name: name.into() }
}

fn req(target: RenameTarget, new: &str) -> RenameRequest {
    RenameRequest { target, new_name: new.into(), table: None, definition: None }
}

/// The fixture and the renames, shared with the libSQL test (same SQL).
pub async fn rename_flow(d: &dyn Driver, s: &mut dyn Session) {
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP VIEW IF EXISTS rn_v; DROP TABLE IF EXISTS rn_t2; DROP TABLE IF EXISTS rn_log;
         DROP TABLE IF EXISTS rn_t; DROP TABLE IF EXISTS \"RN T\";
         CREATE TABLE rn_t (id INTEGER PRIMARY KEY, pepe TEXT CHECK (pepe <> ''));
         CREATE TABLE rn_t2 (id INTEGER PRIMARY KEY, tid INT REFERENCES rn_t (id), pepe TEXT);
         CREATE TABLE rn_log (msg TEXT);
         CREATE INDEX rn_ix_pepe ON rn_t (pepe COLLATE NOCASE DESC) WHERE pepe IS NOT NULL;
         CREATE VIEW rn_v AS SELECT id, pepe FROM rn_t;
         CREATE TRIGGER rn_tr AFTER INSERT ON rn_t BEGIN INSERT INTO rn_log VALUES (NEW.pepe || (SELECT count(*) FROM rn_t WHERE pepe IS NOT NULL)); END;
         CREATE TRIGGER rn_tr_dyn AFTER INSERT ON rn_t BEGIN INSERT INTO rn_log VALUES ('SELECT pepe FROM rn_t'); END;
         CREATE TRIGGER rn_tr_t2 AFTER INSERT ON rn_t2 BEGIN INSERT INTO rn_log VALUES (NEW.pepe); END;",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let spec = d.rename_spec().unwrap();

    // What "Ver dependencias" finds on the column: the view and the trigger, both tracked.
    let column = RenameTarget::Column { table: table("rn_t"), column: "pepe".into() };
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let report = s.dependents(&column.dependency_target(), &scan).await.unwrap();
    for name in ["rn_v", "rn_tr"] {
        let dep = report.items.iter().find(|x| x.name == name).unwrap_or_else(|| panic!("{name} not found: {:?}", report.items));
        assert!(spec.tracked.contains(&dep.kind), "{dep:?}");
    }

    // Column: the view, the trigger, the index and the CHECK follow it.
    let script = d.rename_script(&req(column, "Nuevo Pepe")).unwrap();
    run(s, &script.statements).await.unwrap();
    s.execute("INSERT INTO rn_t (id, \"Nuevo Pepe\") VALUES (1, 'a')", 10, &mut out).await.unwrap();
    assert_eq!(one(s, "SELECT \"Nuevo Pepe\" FROM rn_v").await, "a");
    assert_eq!(one(s, "SELECT count(*) FROM rn_log").await, "2");
    assert!(s.execute("INSERT INTO rn_t (id, \"Nuevo Pepe\") VALUES (2, '')", 10, &mut out).await.is_err(), "the CHECK follows the column");
    assert!(one(s, "SELECT sql FROM sqlite_master WHERE name = 'rn_ix_pepe'").await.contains("\"Nuevo Pepe\" COLLATE NOCASE DESC"));
    // The other table's same-named column and the dynamic SQL are left alone.
    assert!(one(s, "SELECT sql FROM sqlite_master WHERE name = 'rn_tr_t2'").await.contains("NEW.pepe"));
    assert!(one(s, "SELECT sql FROM sqlite_master WHERE name = 'rn_tr_dyn'").await.contains("'SELECT pepe FROM rn_t'"));

    // Table: the view, the triggers and the foreign key follow it.
    let script = d.rename_script(&req(RenameTarget::Object { object: table("rn_t"), parent: None }, "RN T")).unwrap();
    run(s, &script.statements).await.unwrap();
    assert_eq!(one(s, "SELECT count(*) FROM rn_v").await, "1");
    s.execute("INSERT INTO \"RN T\" (id, \"Nuevo Pepe\") VALUES (3, 'b')", 10, &mut out).await.unwrap();
    assert_eq!(one(s, "SELECT count(*) FROM rn_log").await, "4");
    s.execute("INSERT INTO rn_t2 (id, tid, pepe) VALUES (1, 3, 'x')", 10, &mut out).await.unwrap();
    assert!(s.execute("INSERT INTO rn_t2 (id, tid) VALUES (2, 99)", 10, &mut out).await.is_err(), "the foreign key follows the table");
    assert!(one(s, "SELECT sql FROM sqlite_master WHERE name = 'rn_t2'").await.contains("REFERENCES \"RN T\""));

    // Trigger: created again from its definition, the old one dropped.
    let trigger = ObjectRef { kind: "trigger".into(), schema: None, name: "rn_tr".into() };
    let mut r = req(RenameTarget::Object { object: trigger.clone(), parent: Some("RN T".into()) }, "rn tr nuevo");
    r.definition = s.definition(&trigger).await.unwrap();
    let script = d.rename_script(&r).unwrap();
    run(s, &script.statements).await.unwrap();
    assert_eq!(one(s, "SELECT group_concat(name) FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'rn tr%'").await, "rn tr nuevo");
    s.execute("INSERT INTO \"RN T\" (id, \"Nuevo Pepe\") VALUES (4, 'c')", 10, &mut out).await.unwrap();
    assert_eq!(one(s, "SELECT count(*) FROM rn_log").await, "7");

    // Index: created again from the catalog (order, collation, WHERE).
    let mut r = req(RenameTarget::Index { table: table("RN T"), index: "rn_ix_pepe".into() }, "RN Ix");
    r.table = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "RN T");
    let script = d.rename_script(&r).unwrap();
    run(s, &script.statements).await.unwrap();
    assert_eq!(one(s, "SELECT count(*) FROM sqlite_master WHERE name = 'rn_ix_pepe'").await, "0");
    let ix = one(s, "SELECT sql FROM sqlite_master WHERE name = 'RN Ix'").await;
    assert!(ix.contains("\"Nuevo Pepe\" COLLATE NOCASE DESC") && ix.contains("WHERE"), "{ix}");

    // A view already broken makes SQLite refuse the rename; nothing changes.
    s.execute("CREATE TABLE rn_z (a); CREATE VIEW rn_broken AS SELECT a FROM rn_z; DROP TABLE rn_z;", 10, &mut out).await.unwrap();
    let script = d.rename_script(&req(RenameTarget::Object { object: table("RN T"), parent: None }, "rn_t")).unwrap();
    let err = run(s, &script.statements).await.unwrap_err().to_string();
    assert!(err.contains("rn_broken"), "{err}");
    assert_eq!(one(s, "SELECT count(*) FROM \"RN T\"").await, "3");

    s.execute(
        "DROP VIEW rn_broken; DROP VIEW rn_v; DROP TABLE rn_t2; DROP TABLE rn_log; DROP TABLE \"RN T\";",
        10,
        &mut out,
    )
    .await
    .unwrap();
}
