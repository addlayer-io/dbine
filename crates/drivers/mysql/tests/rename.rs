//! "Renombrar…" against real servers, through the app's steps: the
//! dependents ("Ver dependencias"), each one rewritten
//! (`rewrite_references`) and put back the way the spec says (MySQL: dropped
//! before the rename and created after; MariaDB: `CREATE OR REPLACE` after
//! it), then the dependents queried and called.
//!
//! Fixture: `t(id PK, pepe)` with a CHECK and an index on `pepe`, `t2` with a
//! foreign key to `t`, the view `v` on `t`, `p_lee` reading `t.pepe`,
//! `p_dyn` reading it through dynamic SQL, and `p_otra` reading the
//! same-named column of `t3`. Each test reads `DBINE_TEST_<ENGINE>_URL` and
//! is skipped without it:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB8_URL=mysql://root@localhost:25044 \
//!   cargo test -p dbine-driver-mysql --test rename -- --ignored --nocapture --test-threads 1
//! ```

use dbine_driver::rename::{rewrite_references, with_create_style, RewriteOptions};
use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, RenameRequest, RenameTarget, ReplaceStyle, Session,
};
use std::sync::Arc;

const DB: &str = "dbine_rename";

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.trim_end_matches('/').parse().unwrap()));
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
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
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

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: Some(DB.into()), name: name.into() }
}

fn request(target: RenameTarget, new: &str) -> RenameRequest {
    RenameRequest { target, new_name: new.into(), table: None, definition: None }
}

fn drop_sql(kind: &str, name: &str) -> String {
    let what = match kind {
        kinds::VIEW => "VIEW",
        kinds::PROCEDURE => "PROCEDURE",
        kinds::FUNCTION => "FUNCTION",
        kinds::TRIGGER => "TRIGGER",
        other => panic!("unexpected dependent kind {other}"),
    };
    format!("DROP {what} `{DB}`.`{name}`")
}

struct Plan {
    statements: Vec<String>,
    rewritten: Vec<String>,
    manual: Vec<String>,
}

/// What the app does: the dependents rewritten, the rename, the rewrites
/// put back with the spec's style.
async fn plan(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, mut req: RenameRequest) -> Plan {
    let spec = d.rename_spec().expect("spec");
    assert!(spec.allows(&req.target), "{:?}", req.target);
    let dialect = d.script_dialect();
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&req.target.dependency_target(), &scan).await.unwrap();
    eprintln!("dependents: {:?}", report.items.iter().map(|x| (&x.kind, &x.name, x.relation, x.confidence)).collect::<Vec<_>>());
    if let Some(t) = req.target.table() {
        req.table = s.database_schema().await.unwrap().into_iter().find(|x| x.name == t.name);
    }
    let (mut rewritten, mut manual, mut drops, mut creates) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for dep in report.items.iter().filter(|x| x.relation == Relation::Code) {
        if dep.confidence == Confidence::Review {
            manual.push(dep.name.clone());
            continue;
        }
        let body = s.definition(&ObjectRef { kind: dep.kind.clone(), schema: dep.schema.clone(), name: dep.name.clone() }).await.unwrap().unwrap();
        let opts = RewriteOptions { dependent_schema: dep.schema.clone(), keep_view_columns: dep.kind == kinds::VIEW };
        let r = rewrite_references(&body, &dialect, &req.target.rewrite_target(), &req.new_name, &spec, &opts);
        eprintln!("{} -> {} (unresolved {:?})", dep.name, r.text, r.unresolved);
        if r.edits.is_empty() {
            manual.push(dep.name.clone());
            continue;
        }
        rewritten.push(dep.name.clone());
        if spec.replace == ReplaceStyle::DropCreate {
            drops.push(drop_sql(&dep.kind, &dep.name));
        }
        creates.push(with_create_style(&r.text, &dialect, spec.replace).trim().to_string());
    }
    let middle = d.rename_script(&req).unwrap();
    eprintln!("warnings: {:?}", middle.warnings);
    let statements = drops.into_iter().chain(middle.statements).chain(creates).collect();
    Plan { statements, rewritten, manual }
}

async fn apply(s: &mut Box<dyn Session>, statements: &[String]) {
    for st in statements {
        eprintln!("> {st}");
        ok(s, st).await;
    }
}

async fn setup(d: &Arc<dyn Driver>, cfg: &ConnectionConfig, routines: bool, checks: bool) -> Box<dyn Session> {
    let mut root = d.connect(cfg, None).await.unwrap();
    ok(&mut root, &format!("DROP DATABASE IF EXISTS {DB}")).await;
    ok(&mut root, &format!("CREATE DATABASE {DB}")).await;
    let mut s = d.connect(cfg, Some(DB)).await.unwrap();
    let check = if checks { ", CONSTRAINT ck_pepe CHECK (pepe <> '')" } else { "" };
    ok(&mut s, &format!("CREATE TABLE t (id INT PRIMARY KEY, pepe VARCHAR(20) NOT NULL DEFAULT 'x' COMMENT 'el nombre'{check})")).await;
    for sql in [
        "CREATE INDEX ix_pepe ON t (pepe)",
        "CREATE TABLE t2 (id INT PRIMARY KEY, t_id INT, CONSTRAINT fk_t2_t FOREIGN KEY (t_id) REFERENCES t (id))",
        "CREATE TABLE t3 (id INT PRIMARY KEY, pepe VARCHAR(20))",
        "INSERT INTO t VALUES (1, 'a')",
        "INSERT INTO t2 VALUES (1, 1)",
        "INSERT INTO t3 VALUES (1, 'otra')",
        "CREATE VIEW v AS SELECT id, pepe FROM t",
    ] {
        ok(&mut s, sql).await;
    }
    if routines {
        for sql in [
            "CREATE PROCEDURE p_lee() BEGIN SELECT pepe FROM t WHERE id = 1; END",
            "CREATE PROCEDURE p_dyn() BEGIN SET @q = 'SELECT pepe FROM t'; PREPARE st FROM @q; EXECUTE st; DEALLOCATE PREPARE st; END",
            "CREATE PROCEDURE p_otra() BEGIN SELECT pepe FROM t3 WHERE id = 1; END",
        ] {
            ok(&mut s, sql).await;
        }
    }
    s
}

/// The column's type, nullability, default and comment.
async fn column_shape(s: &mut Box<dyn Session>, table: &str, column: &str) -> Vec<String> {
    let sql = format!(
        "SELECT CONCAT_WS('|', COLUMN_TYPE, IS_NULLABLE, COALESCE(COLUMN_DEFAULT, 'NULL'), COLUMN_COMMENT) FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = '{DB}' AND TABLE_NAME = '{table}' AND COLUMN_NAME = '{column}'"
    );
    col(s, &sql).await
}

async fn rename_flow(id: &str, env: &str, routines: bool, checks: bool) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    let cfg = parse_url(id, &url);
    let mut s = setup(&d, &cfg, routines, checks).await;
    let spec = d.rename_spec().unwrap();
    let before = column_shape(&mut s, "t", "pepe").await;
    assert_eq!(before.len(), 1, "{before:?}");

    // The column: CHANGE COLUMN keeps it as it was.
    let p = plan(&d, &mut s, request(RenameTarget::Column { table: table("t"), column: "pepe".into() }, "nuevo")).await;
    assert!(p.rewritten.contains(&"v".to_string()), "{:?}", p.rewritten);
    assert!(!p.rewritten.contains(&"p_otra".to_string()), "{:?}", p.rewritten);
    if routines {
        assert!(p.rewritten.contains(&"p_lee".to_string()), "{:?}", p.rewritten);
        assert!(p.manual.contains(&"p_dyn".to_string()), "{:?}", p.manual);
        match spec.replace {
            ReplaceStyle::DropCreate => assert!(p.statements.iter().any(|x| x.starts_with("DROP PROCEDURE")), "{:?}", p.statements),
            _ => assert!(p.statements.iter().any(|x| x.starts_with("CREATE OR REPLACE") && x.contains("PROCEDURE")), "{:?}", p.statements),
        }
    }
    assert!(p.statements.iter().any(|x| x.contains("CHANGE COLUMN `pepe` nuevo")), "{:?}", p.statements);
    apply(&mut s, &p.statements).await;
    let after = column_shape(&mut s, "t", "nuevo").await;
    let normalize = |v: &[String]| v.iter().map(|x| x.replace("'x'", "x")).collect::<Vec<_>>();
    assert_eq!(normalize(&after), normalize(&before), "type, nullability, default and comment kept");
    assert!(column_shape(&mut s, "t", "pepe").await.is_empty());
    // The view keeps its output column; the index and the check follow.
    assert_eq!(col(&mut s, "SELECT pepe FROM v WHERE id = 1").await, ["a"]);
    assert_eq!(col(&mut s, &format!("SELECT COLUMN_NAME FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = '{DB}' AND INDEX_NAME = 'ix_pepe'")).await, ["nuevo"]);
    if checks {
        let clause = col(&mut s, &format!("SELECT CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = '{DB}' AND CONSTRAINT_NAME = 'ck_pepe'")).await;
        assert!(clause.len() == 1 && clause[0].contains("nuevo"), "{clause:?}");
        assert!(run(&mut s, "INSERT INTO t VALUES (9, '')").await.is_err(), "the check still holds");
    }
    if routines {
        assert_eq!(col(&mut s, "CALL p_lee()").await, ["a"]);
        assert_eq!(col(&mut s, "CALL p_otra()").await, ["otra"]);
        let body = col(&mut s, &format!("SELECT ROUTINE_DEFINITION FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = '{DB}' AND ROUTINE_NAME = 'p_otra'")).await;
        assert!(body[0].contains("pepe"), "the other table's routine is untouched: {body:?}");
        // The dynamic one was left to the user: it fails now.
        assert!(run(&mut s, "CALL p_dyn()").await.is_err());
    }

    // The table: RENAME TABLE; the foreign key follows.
    let p = plan(&d, &mut s, request(RenameTarget::Object { object: table("t"), parent: None }, "Tabla")).await;
    assert!(p.rewritten.contains(&"v".to_string()), "{:?}", p.rewritten);
    assert!(p.statements.iter().any(|x| x == &format!("RENAME TABLE `{DB}`.`t` TO `{DB}`.Tabla;")), "{:?}", p.statements);
    apply(&mut s, &p.statements).await;
    assert_eq!(col(&mut s, "SELECT pepe FROM v WHERE id = 1").await, ["a"]);
    if routines {
        assert_eq!(col(&mut s, "CALL p_lee()").await, ["a"]);
        assert_eq!(col(&mut s, "CALL p_otra()").await, ["otra"]);
    }
    assert_eq!(
        col(&mut s, &format!("SELECT REFERENCED_TABLE_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = '{DB}' AND CONSTRAINT_NAME = 'fk_t2_t'")).await,
        ["Tabla"]
    );

    // The index.
    let p = plan(&d, &mut s, request(RenameTarget::Index { table: table("Tabla"), index: "ix_pepe".into() }, "ix_nuevo")).await;
    apply(&mut s, &p.statements).await;
    assert_eq!(col(&mut s, &format!("SELECT COLUMN_NAME FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = '{DB}' AND INDEX_NAME = 'ix_nuevo'")).await, ["nuevo"]);

    // The view itself.
    let p = plan(&d, &mut s, request(RenameTarget::Object { object: ObjectRef { kind: kinds::VIEW.into(), ..table("v") }, parent: None }, "v2")).await;
    apply(&mut s, &p.statements).await;
    assert_eq!(col(&mut s, "SELECT pepe FROM v2 WHERE id = 1").await, ["a"]);

    // Constraints and databases are refused, the primary key too.
    let refused = [
        RenameTarget::Constraint { table: table("Tabla"), constraint: "ck_pepe".into() },
        RenameTarget::Schema { database: None, schema: DB.into() },
        RenameTarget::Index { table: table("Tabla"), index: "PRIMARY".into() },
    ];
    for t in refused {
        assert!(d.rename_script(&request(t, "otro")).is_err());
    }

    let mut root = d.connect(&cfg, None).await.unwrap();
    ok(&mut root, &format!("DROP DATABASE IF EXISTS {DB}")).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql_rename() {
    rename_flow("mysql", "DBINE_TEST_MYSQL_URL", true, true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb_rename() {
    rename_flow("mariadb", "DBINE_TEST_MARIADB_URL", true, true).await;
}

/// TiDB: no routines; the CHECK only exists with tidb_enable_check_constraint.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tidb_rename() {
    rename_flow("tidb", "DBINE_TEST_TIDB8_URL", false, false).await;
}
