//! "Renombrar…" against a real server, through the app's steps: the
//! dependents ("Ver dependencias"), each one rewritten
//! (`rewrite_references`) and put back with `CREATE OR REPLACE` after the
//! driver's rename, then the dependents queried.
//!
//! Targets come as the explorer gives them, without a schema: ClickHouse
//! writes every name in a stored definition qualified with the database
//! (`db.t`), which the rewrite takes as the target's
//! (`RewriteOptions::database`), as the app does.
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//! DBINE_TEST_TIMEPLUS_URL=http://localhost:25119 \
//!   cargo test -p dbine-driver-clickhouse --test rename -- --ignored --nocapture
//! ```

use dbine_driver::rename::{rewrite_references, with_create_style, RewriteOptions};
use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, RenameRequest, RenameTarget, ReplaceStyle, Session,
};
use std::sync::Arc;

const DB: &str = "dbine_rename";

fn cfg(env: &str, id: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: id.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
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

fn first(out: &QueryOutcome) -> String {
    let rs = out.results.first().expect("rows");
    let v = &rs.rows[0][0];
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

/// What the app does: the dependents rewritten, the rename, the rewrites
/// put back with the spec's style. Returns the statements and the names of
/// what was rewritten.
async fn plan(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, db: &str, mut req: RenameRequest) -> (Vec<String>, Vec<String>) {
    let spec = d.rename_spec().expect("spec");
    assert!(spec.allows(&req.target));
    let dialect = d.script_dialect();
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&req.target.dependency_target(), &scan).await.unwrap();
    eprintln!("dependents: {:?}", report.items.iter().map(|x| (&x.kind, &x.name, x.relation, x.confidence)).collect::<Vec<_>>());
    if let Some(t) = req.target.table() {
        req.table = s.database_schema().await.unwrap().into_iter().find(|x| x.name == t.name);
    }
    let mut rewritten = Vec::new();
    let mut drops = Vec::new();
    let mut creates = Vec::new();
    for dep in report.items.iter().filter(|x| x.relation == Relation::Code && x.confidence != Confidence::Review) {
        let body = s.definition(&ObjectRef { kind: dep.kind.clone(), schema: dep.schema.clone(), name: dep.name.clone() }).await.unwrap().unwrap();
        let opts = RewriteOptions { dependent_schema: dep.schema.clone(), keep_view_columns: true, database: Some(db.into()), ..Default::default() };
        let r = rewrite_references(&body, &dialect, &req.target.rewrite_target(), &req.new_name, &spec, &opts);
        eprintln!("{} -> {} (unresolved {:?})", dep.name, r.text, r.unresolved);
        if !r.edits.is_empty() {
            rewritten.push(dep.name.clone());
            let style = spec.replace_for(&dep.kind);
            if style == ReplaceStyle::DropCreate {
                let name = match &dep.schema {
                    Some(db) => format!("`{db}`.`{}`", dep.name),
                    None => format!("`{}`", dep.name),
                };
                drops.push(format!("DROP VIEW {name};"));
            }
            creates.push(format!("{};", with_create_style(&r.text, &dialect, style).trim().trim_end_matches(';')));
        }
    }
    let middle = d.rename_script(&req).map_err(|e| e.to_string());
    let mut statements = match middle {
        Ok(m) => drops.into_iter().chain(m.statements).collect::<Vec<_>>(),
        Err(e) => return (vec![format!("ERR {e}")], rewritten),
    };
    statements.extend(creates);
    (statements, rewritten)
}

async fn apply(s: &mut Box<dyn Session>, statements: &[String]) -> Result<(), String> {
    for st in statements {
        eprintln!("> {st}");
        run(s, st).await.map_err(|e| format!("{st}: {e}"))?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse_rename() {
    let Some(cfg) = cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse") else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap();
    let mut root = d.connect(&cfg, None).await.unwrap();
    for db in ["dbine_rename", "dbine_rename2"] {
        let _ = run(&mut root, &format!("DROP DATABASE IF EXISTS {db} SYNC")).await;
    }
    ok(&mut root, "CREATE DATABASE dbine_rename ENGINE = Atomic").await;
    let mut s = d.connect(&cfg, Some("dbine_rename")).await.unwrap();
    for sql in [
        "CREATE TABLE t (id UInt64, pepe String, fecha Date, INDEX ix_pepe pepe TYPE bloom_filter GRANULARITY 1, \
         CONSTRAINT ck_pepe CHECK length(pepe) < 100) ENGINE = MergeTree PARTITION BY toYYYYMM(fecha) ORDER BY id",
        "CREATE TABLE t3 (id UInt64, pepe String) ENGINE = MergeTree ORDER BY id",
        "CREATE TABLE dest (id UInt64, pepe String) ENGINE = MergeTree ORDER BY id",
        "CREATE VIEW v AS SELECT id, pepe FROM t",
        "CREATE VIEW v_otra AS SELECT id, pepe FROM t3",
        "CREATE MATERIALIZED VIEW mv TO dest AS SELECT id, pepe FROM t",
        "INSERT INTO t VALUES (1, 'a', '2024-01-01')",
    ] {
        ok(&mut s, sql).await;
    }
    let col = |c: &str| RenameRequest {
        target: RenameTarget::Column { table: ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "t".into() }, column: c.into() },
        new_name: "nuevo".into(),
        table: None,
        definition: None,
    };

    // A key column: refused before anything runs.
    let (st, _) = plan(&d, &mut s, DB, col("id")).await;
    assert!(st[0].starts_with("ERR") && st[0].contains("«id»") && st[0].contains("clave"), "{st:?}");
    // Without the table (the app couldn't read it), the guard stops it on the server.
    let e = apply(&mut s, &d.rename_script(&col("fecha")).unwrap().statements).await.unwrap_err();
    assert!(e.contains("«fecha»") && e.contains("clave"), "{e}");
    // A column a materialized view reads: the guard stops it before the rename.
    let (st, _) = plan(&d, &mut s, DB, col("pepe")).await;
    let e = apply(&mut s, &st).await.unwrap_err();
    assert!(e.contains("vista materializada"), "{e}");
    assert_eq!(first(&ok(&mut s, "SELECT pepe FROM t").await), "a");

    // The table: the plain view is rewritten with CREATE OR REPLACE, the
    // materialized view with TO recreated; the other table's view untouched.
    let req = RenameRequest {
        target: RenameTarget::Object { object: ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "t".into() }, parent: None },
        new_name: "Tabla".into(),
        table: None,
        definition: None,
    };
    let (st, rewritten) = plan(&d, &mut s, DB, req).await;
    assert!(rewritten.contains(&"v".to_string()) && rewritten.contains(&"mv".to_string()), "{rewritten:?}");
    assert!(!rewritten.contains(&"v_otra".to_string()));
    assert!(st.iter().any(|x| x.starts_with("CREATE OR REPLACE VIEW")) && st.iter().any(|x| x.starts_with("CREATE OR REPLACE MATERIALIZED VIEW")), "{st:?}");
    apply(&mut s, &st).await.unwrap();
    assert_eq!(first(&ok(&mut s, "SELECT pepe FROM v").await), "a");
    ok(&mut s, "INSERT INTO Tabla VALUES (2, 'b', '2024-01-01')").await;
    assert_eq!(first(&ok(&mut s, "SELECT pepe FROM dest WHERE id = 2").await), "b");
    let mv = first(&ok(&mut s, "SELECT create_table_query FROM system.tables WHERE database = 'dbine_rename' AND name = 'mv'").await);
    assert!(mv.contains("Tabla") && mv.contains(" TO dbine_rename.dest"), "{mv}");
    assert_eq!(first(&ok(&mut s, "SELECT count() FROM v_otra").await), "0");

    // A column only a plain view reads: renamed, the view keeps its output name.
    ok(&mut s, "DROP VIEW mv").await;
    let mut c = col("pepe");
    if let RenameTarget::Column { table, .. } = &mut c.target {
        table.name = "Tabla".into();
    }
    let (st, rewritten) = plan(&d, &mut s, DB, c).await;
    assert_eq!(rewritten, ["v"]);
    apply(&mut s, &st).await.unwrap();
    // ClickHouse stores the view with its structure, matched by name: the
    // rewrite keeps the output name (`nuevo AS pepe`).
    assert_eq!(first(&ok(&mut s, "SELECT pepe FROM v ORDER BY id").await), "a");
    assert_eq!(first(&ok(&mut s, "SELECT count() FROM v").await), "2");
    assert_eq!(first(&ok(&mut s, "SELECT count() FROM v_otra").await), "0");
    let t = first(&ok(&mut s, "SELECT create_table_query FROM system.tables WHERE database = 'dbine_rename' AND name = 'Tabla'").await);
    assert!(t.contains("INDEX ix_pepe nuevo") && t.contains("CHECK length(nuevo)"), "{t}");

    // Dictionary and database.
    ok(&mut s, "CREATE DICTIONARY dic (id UInt64, nuevo String) PRIMARY KEY id SOURCE(CLICKHOUSE(TABLE 'Tabla' DB 'dbine_rename')) LAYOUT(FLAT()) LIFETIME(0)").await;
    let dic = RenameRequest {
        target: RenameTarget::Object { object: ObjectRef { kind: "dictionary".into(), schema: None, name: "dic".into() }, parent: None },
        new_name: "dic2".into(),
        table: None,
        definition: None,
    };
    apply(&mut s, &d.rename_script(&dic).unwrap().statements).await.unwrap();
    assert_eq!(first(&ok(&mut s, "SELECT count() FROM system.dictionaries WHERE database = 'dbine_rename' AND name = 'dic2'").await), "1");
    // A dictionary reading one of its tables stops the database rename.
    ok(&mut s, "DROP DICTIONARY dic2").await;
    let db = RenameRequest {
        target: RenameTarget::Schema { database: Some("dbine_rename".into()), schema: "dbine_rename".into() },
        new_name: "dbine_rename2".into(),
        table: None,
        definition: None,
    };
    let (st, rewritten) = plan(&d, &mut s, DB, db).await;
    eprintln!("db rewrites: {rewritten:?}");
    apply(&mut s, &st).await.unwrap();
    // The session followed the new name.
    assert_eq!(first(&ok(&mut s, "SELECT currentDatabase()").await), "dbine_rename2");
    assert_eq!(first(&ok(&mut s, "SELECT count() FROM v").await), "2");

    let _ = run(&mut root, "DROP DATABASE IF EXISTS dbine_rename2 SYNC").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timeplus_rename() {
    let Some(cfg) = cfg("DBINE_TEST_TIMEPLUS_URL", "timeplus") else {
        eprintln!("DBINE_TEST_TIMEPLUS_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "timeplus").unwrap();
    let mut s = d.connect(&cfg, Some("default")).await.unwrap();
    for sql in ["DROP VIEW IF EXISTS rn_v", "DROP STREAM IF EXISTS rn_s", "DROP STREAM IF EXISTS rn_s2"] {
        let _ = run(&mut s, sql).await;
    }
    ok(&mut s, "CREATE STREAM rn_s (id uint64, pepe string)").await;
    ok(&mut s, "CREATE VIEW rn_v AS SELECT id, pepe FROM rn_s").await;
    let stream = ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "rn_s".into() };
    let req = RenameRequest { target: RenameTarget::Object { object: stream, parent: None }, new_name: "rn_s2".into(), table: None, definition: None };
    let (st, rewritten) = plan(&d, &mut s, "default", req).await;
    assert_eq!(rewritten, ["rn_v"]);
    // Proton refuses the rename while the view exists: it goes first.
    assert!(st[0].starts_with("DROP VIEW") && st[1].starts_with("RENAME STREAM"), "{st:?}");
    apply(&mut s, &st).await.unwrap();
    let view = "SELECT create_table_query FROM system.tables WHERE database = 'default' AND name = 'rn_v'";
    assert!(first(&ok(&mut s, view).await).contains("default.rn_s2"));
    let col = RenameRequest {
        target: RenameTarget::Column { table: ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "rn_s2".into() }, column: "pepe".into() },
        new_name: "nuevo".into(),
        table: None,
        definition: None,
    };
    // Checked after the cleanup: Proton refuses the rename while the view
    // reads the column, so the view (`FROM default.rn_s2`) must be rewritten.
    let (st, rewritten) = plan(&d, &mut s, "default", col).await;
    let column = if rewritten == ["rn_v"] { apply(&mut s, &st).await.map(|_| ()) } else { Err(format!("not rewritten: {rewritten:?}")) };
    let def = if column.is_ok() { first(&ok(&mut s, view).await) } else { String::new() };
    for sql in ["DROP VIEW IF EXISTS rn_v", "DROP STREAM IF EXISTS rn_s2"] {
        let _ = run(&mut s, sql).await;
    }
    column.unwrap();
    assert!(def.contains("nuevo AS pepe"), "{def}");
}
