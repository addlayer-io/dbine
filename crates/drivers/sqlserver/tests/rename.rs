//! "Renombrar…" against a real server: `rename_script` plus the dependents
//! rewritten and put back the way the app does it (`rename_impact` /
//! `build_script` in src-tauri), then the dependents are used. Reads
//! `DBINE_TEST_SQLSERVER_URL` and `DBINE_TEST_BABELFISH_URL` like
//! `dependencies.rs`.

use dbine_driver::rename::{rewrite_references, with_create_style, RewriteOptions};
use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, RenameRequest, RenameTarget, ReplaceStyle, Session,
};
use std::sync::Arc;

fn parse_url(url: &str, driver: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    try_run(s, sql).await.unwrap_or_else(|e| panic!("{e}\n--- in:\n{sql}"))
}

/// The first cell of the first row, as text.
async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let out = run(s, sql).await;
    let rs = out.results.iter().find(|r| !r.columns.is_empty()).unwrap_or_else(|| panic!("no rows: {sql}"));
    match rs.rows.first().and_then(|r| r.first()) {
        Some(serde_json::Value::String(t)) => t.clone(),
        Some(v) => v.to_string(),
        None => "null".into(),
    }
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbo".into()), name: name.into() }
}

/// What the rename did with the dependents (names in lower case: Babelfish
/// reports them so).
#[derive(Debug, Default)]
struct Applied {
    rewritten: Vec<String>,
    manual: Vec<String>,
}

/// The app's flow: the dependents, each rewritten one put back around the
/// driver's rename (schemabound ones, or all with `DropCreate`, dropped
/// first), run in one transaction.
async fn rename(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, target: RenameTarget, new_name: &str) -> Result<Applied, String> {
    let spec = d.rename_spec().unwrap();
    assert!(spec.allows(&target), "{target:?}");
    let dialect = d.script_dialect();
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&target.dependency_target(), &scan).await.unwrap();
    let mut applied = Applied::default();
    let mut before = Vec::new();
    let mut after = Vec::new();
    for dep in report.items.iter().filter(|x| x.relation == Relation::Code) {
        if dep.confidence == Confidence::Review || spec.tracked.contains(&dep.kind) {
            applied.manual.push(dep.name.to_lowercase());
            continue;
        }
        let obj = ObjectRef { kind: dep.kind.clone(), schema: dep.schema.clone(), name: dep.name.clone() };
        let body = s.definition(&obj).await.unwrap().unwrap();
        let opts = RewriteOptions { dependent_schema: dep.schema.clone(), keep_view_columns: dep.kind == kinds::VIEW, ..Default::default() };
        let r = rewrite_references(&body, &dialect, &target.rewrite_target(), new_name, &spec, &opts);
        if r.edits.is_empty() || !r.unresolved.is_empty() {
            applied.manual.push(dep.name.to_lowercase());
            continue;
        }
        let bound = body.to_ascii_lowercase().contains("schemabinding");
        let style = spec.replace_for(&dep.kind);
        if style == ReplaceStyle::DropCreate || bound {
            let kind = match dep.kind.as_str() {
                kinds::PROCEDURE => "PROCEDURE",
                kinds::FUNCTION => "FUNCTION",
                kinds::TRIGGER => "TRIGGER",
                _ => "VIEW",
            };
            before.push(format!("DROP {kind} IF EXISTS [{}].[{}];", dep.schema.as_deref().unwrap_or("dbo"), dep.name));
        }
        after.push(with_create_style(&r.text, &dialect, style));
        applied.rewritten.push(dep.name.to_lowercase());
    }
    let table = match target.table() {
        Some(t) => s.database_schema().await.unwrap().into_iter().find(|x| x.name == t.name && x.schema.as_deref() == t.schema()),
        None => None,
    };
    let definition = match &target {
        RenameTarget::Object { object, .. } if object.kind != kinds::TABLE => s.definition(object).await.unwrap(),
        _ => None,
    };
    let script = d.rename_script(&RenameRequest { target, new_name: new_name.into(), table, definition }).map_err(|e| e.to_string())?;
    eprintln!("warnings: {:?}", script.warnings);
    let statements: Vec<String> = before.into_iter().chain(script.statements).chain(after).collect();
    s.set_autocommit(false).await.unwrap();
    let mut failed = None;
    for st in &statements {
        eprintln!("--> {st}");
        if let Err(e) = try_run(s, st).await {
            failed = Some(e);
            break;
        }
    }
    if let Some(e) = failed {
        s.rollback().await.unwrap();
        s.set_autocommit(true).await.unwrap();
        return Err(e);
    }
    s.commit().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    Ok(applied)
}

const FIXTURE: &str = "CREATE TABLE dbo.T (id int PRIMARY KEY, pepe int CONSTRAINT CK_pepe CHECK (pepe > 0), otra int)
GO
ALTER TABLE dbo.T ADD CONSTRAINT CK_tabla CHECK (pepe < 1000 OR otra IS NULL)
GO
CREATE INDEX IX_pepe ON dbo.T (pepe)
GO
CREATE TABLE dbo.T2 (id int PRIMARY KEY, t_id int CONSTRAINT FK_T2_T REFERENCES dbo.T (id))
GO
CREATE TABLE dbo.O (id int PRIMARY KEY, pepe int)
GO
INSERT INTO dbo.T (id, pepe, otra) VALUES (1, 10, 5), (2, 20, NULL)
GO
CREATE VIEW dbo.V AS SELECT id, pepe FROM dbo.T
GO
CREATE VIEW dbo.VB WITH SCHEMABINDING AS SELECT id, pepe FROM dbo.T
GO
CREATE PROCEDURE dbo.P AS SELECT pepe FROM dbo.T WHERE pepe > 0
GO
CREATE PROCEDURE dbo.PD AS EXEC ('SELECT pepe FROM dbo.T')
GO
CREATE PROCEDURE dbo.PO AS SELECT pepe FROM dbo.O
GO
CREATE TRIGGER dbo.TR ON dbo.T AFTER UPDATE AS SELECT 1 AS x WHERE 1 = 0";

async fn fresh(d: &Arc<dyn Driver>, cfg: &ConnectionConfig, db: &str) -> Box<dyn Session> {
    let mut admin = d.connect(cfg, None).await.expect("connect");
    if admin.list_databases().await.unwrap().iter().any(|x| x == db) {
        admin.drop_database(db).await.expect("drop database");
    }
    admin.create_database(db).await.expect("create database");
    d.connect(cfg, Some(db)).await.unwrap()
}

#[tokio::test]
#[ignore]
async fn sqlserver_rename() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url, "sqlserver");
    let d = driver("sqlserver");
    let db = "dbine_rename";
    let mut s = fresh(&d, &cfg, db).await;
    run(&mut s, FIXTURE).await;
    run(
        &mut s,
        "SET QUOTED_IDENTIFIER ON
         GO
         CREATE INDEX IX_pepe_f ON dbo.T (id) INCLUDE (pepe) WHERE pepe > 5
         GO
         ALTER TABLE dbo.T NOCHECK CONSTRAINT CK_tabla
         GO
         CREATE TABLE dbo.C (id int PRIMARY KEY, pepe int, doble AS (pepe * 2))
         GO
         CREATE USER lector WITHOUT LOGIN
         GO
         GRANT SELECT ON dbo.V TO lector
         GO
         GRANT EXECUTE ON dbo.P TO lector",
    )
    .await;
    let grants = "SELECT COUNT(*) FROM sys.database_permissions p JOIN sys.database_principals u ON u.principal_id = p.grantee_principal_id
                   WHERE u.name = 'lector' AND p.major_id IN (OBJECT_ID('dbo.V'), OBJECT_ID('dbo.P'), OBJECT_ID('dbo.V2'))";
    assert_eq!(scalar(&mut s, grants).await, "2");

    // 1. The table: the schemabound view is dropped first and created
    // after; the others go through CREATE OR ALTER and keep their grants.
    let a = rename(&d, &mut s, RenameTarget::Object { object: table("T"), parent: None }, "Tn").await.unwrap();
    eprintln!("{a:?}");
    for name in ["V", "VB", "P", "TR"] {
        assert!(a.rewritten.contains(&name.to_lowercase()), "{name}: {a:?}");
    }
    assert!(a.manual.contains(&"pd".to_string()), "{a:?}");
    assert!(!a.rewritten.contains(&"po".to_string()) && !a.manual.contains(&"po".to_string()));
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM dbo.V").await, "2");
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM dbo.VB").await, "2");
    run(&mut s, "EXEC dbo.P").await;
    assert_eq!(scalar(&mut s, grants).await, "2", "CREATE OR ALTER keeps the grants");
    assert!(scalar(&mut s, "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.VB'))").await.contains("dbo.Tn"));
    assert!(scalar(&mut s, "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.TR'))").await.contains("dbo.Tn"));
    assert_eq!(scalar(&mut s, "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.PO'))").await, "CREATE PROCEDURE dbo.PO AS SELECT pepe FROM dbo.O");
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM sys.foreign_keys WHERE referenced_object_id = OBJECT_ID('dbo.Tn')").await, "1");

    // 2. The column: CHECKs (one disabled) and the filtered index are put
    // back with the new name; the schemabound view is dropped first.
    let col = RenameTarget::Column { table: table("Tn"), column: "pepe".into() };
    let a = rename(&d, &mut s, col.clone(), "nuevo").await.unwrap();
    eprintln!("{a:?}");
    for name in ["V", "VB", "P"] {
        assert!(a.rewritten.contains(&name.to_lowercase()), "{name}: {a:?}");
    }
    // PD's dynamic SQL still names dbo.T (left to the user in step 1), so
    // it doesn't show up for Tn.pepe.
    assert!(!a.rewritten.contains(&"pd".to_string()), "{a:?}");
    assert_eq!(scalar(&mut s, "SELECT definition FROM sys.check_constraints WHERE name = 'CK_pepe'").await, "([nuevo]>(0))");
    assert_eq!(scalar(&mut s, "SELECT CONCAT(definition, is_disabled) FROM sys.check_constraints WHERE name = 'CK_tabla'").await, "([nuevo]<(1000) OR [otra] IS NULL)1");
    assert_eq!(scalar(&mut s, "SELECT filter_definition FROM sys.indexes WHERE name = 'IX_pepe_f'").await, "([nuevo]>(5))");
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM sys.indexes WHERE name = 'IX_pepe'").await, "1");
    // The view keeps its output column (`nuevo AS pepe`).
    assert_eq!(scalar(&mut s, "SELECT SUM(pepe) FROM dbo.V").await, "30");
    assert_eq!(scalar(&mut s, "SELECT SUM(pepe) FROM dbo.VB").await, "30");
    run(&mut s, "EXEC dbo.P").await;
    assert_eq!(scalar(&mut s, grants).await, "2");
    assert!(try_run(&mut s, "INSERT INTO dbo.Tn (id, nuevo) VALUES (3, -1)").await.is_err(), "the CHECK is back");

    // A failing column rename (the name is taken) undoes the batch: the
    // CHECKs dropped before sp_rename are still there.
    let taken = RenameTarget::Column { table: table("Tn"), column: "nuevo".into() };
    assert!(rename(&d, &mut s, taken, "otra").await.is_err());
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM sys.check_constraints WHERE name IN ('CK_pepe', 'CK_tabla')").await, "2");
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM sys.indexes WHERE name = 'IX_pepe_f'").await, "1");

    // A column a computed column uses: refused before anything runs.
    let e = rename(&d, &mut s, RenameTarget::Column { table: table("C"), column: "pepe".into() }, "x").await.unwrap_err();
    assert!(e.contains("columna calculada «doble»"), "{e}");

    // 3. A module: same object (grants kept), text with the new name.
    let view = ObjectRef { kind: kinds::VIEW.into(), schema: Some("dbo".into()), name: "V".into() };
    rename(&d, &mut s, RenameTarget::Object { object: view, parent: None }, "V2").await.unwrap();
    assert!(scalar(&mut s, "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.V2'))").await.contains("VIEW [dbo].[V2]") || scalar(&mut s, "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.V2'))").await.contains("VIEW dbo.V2"));
    assert_eq!(scalar(&mut s, grants).await, "2");
    let trigger = ObjectRef { kind: kinds::TRIGGER.into(), schema: Some("dbo".into()), name: "TR".into() };
    rename(&d, &mut s, RenameTarget::Object { object: trigger, parent: Some("Tn".into()) }, "TR2").await.unwrap();
    assert!(scalar(&mut s, "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.TR2'))").await.contains("dbo.TR2"));
    let proc = ObjectRef { kind: kinds::PROCEDURE.into(), schema: Some("dbo".into()), name: "P".into() };
    rename(&d, &mut s, RenameTarget::Object { object: proc, parent: None }, "P 2").await.unwrap();
    run(&mut s, "EXEC [dbo].[P 2]").await;
    assert!(scalar(&mut s, "SELECT OBJECT_DEFINITION(OBJECT_ID('dbo.[P 2]'))").await.contains("PROCEDURE dbo.[P 2]"));

    // 4. Index and constraint.
    rename(&d, &mut s, RenameTarget::Index { table: table("Tn"), index: "IX_pepe".into() }, "IX_nuevo").await.unwrap();
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM sys.indexes WHERE name = 'IX_nuevo' AND object_id = OBJECT_ID('dbo.Tn')").await, "1");
    rename(&d, &mut s, RenameTarget::Constraint { table: table("Tn"), constraint: "CK_pepe".into() }, "CK_nuevo").await.unwrap();
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM sys.check_constraints WHERE name = 'CK_nuevo'").await, "1");

    drop(s);
    d.connect(&cfg, None).await.unwrap().drop_database(db).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn babelfish_rename() {
    let Ok(url) = std::env::var("DBINE_TEST_BABELFISH_URL") else {
        eprintln!("DBINE_TEST_BABELFISH_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url, "babelfish");
    let d = driver("babelfish");
    let db = "dbine_rename_bbf";
    let mut s = fresh(&d, &cfg, db).await;
    run(&mut s, FIXTURE).await;

    // Table: every rewritten dependent dropped first and created after.
    let a = rename(&d, &mut s, RenameTarget::Object { object: table("T"), parent: None }, "Tn").await.unwrap();
    eprintln!("{a:?}");
    for name in ["V", "VB", "P"] {
        assert!(a.rewritten.contains(&name.to_lowercase()), "{name}: {a:?}");
    }
    assert!(a.manual.contains(&"pd".to_string()), "{a:?}");
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM dbo.V").await, "2");
    run(&mut s, "EXEC dbo.P").await;

    // Column: PostgreSQL moves the CHECKs by itself.
    let a = rename(&d, &mut s, RenameTarget::Column { table: table("Tn"), column: "pepe".into() }, "nuevo").await.unwrap();
    eprintln!("{a:?}");
    assert_eq!(scalar(&mut s, "SELECT SUM(pepe) FROM dbo.V").await, "30");
    run(&mut s, "EXEC dbo.P").await;
    assert!(try_run(&mut s, "INSERT INTO dbo.Tn (id, nuevo) VALUES (3, -1)").await.is_err(), "the CHECK follows");

    // Modules: a view through sp_rename, a procedure dropped and created.
    let view = ObjectRef { kind: kinds::VIEW.into(), schema: Some("dbo".into()), name: "V".into() };
    rename(&d, &mut s, RenameTarget::Object { object: view, parent: None }, "V2").await.unwrap();
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM dbo.V2").await, "2");
    let proc = ObjectRef { kind: kinds::PROCEDURE.into(), schema: Some("dbo".into()), name: "P".into() };
    rename(&d, &mut s, RenameTarget::Object { object: proc, parent: None }, "P2").await.unwrap();
    run(&mut s, "EXEC dbo.P2").await;
    // Babelfish names a renamed table's indexes after the old name and then
    // can't find them by name: indexes aren't offered.
    assert!(!d.rename_spec().unwrap().indexes);

    drop(s);
    d.connect(&cfg, None).await.unwrap().drop_database(db).await.unwrap();
}
