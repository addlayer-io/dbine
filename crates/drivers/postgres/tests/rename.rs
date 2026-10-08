//! "Renombrar…" against real servers: the dependents are found ("Ver
//! dependencias"), rewritten with `rewrite_references`, put back around the
//! driver's rename the way the app does (`rename_script` in src-tauri), and
//! the script runs as the app runs it (atomically where the spec says so).
//! Then the dependents must still work. Reads the URLs below
//! (`postgres://user:pass@host:port/db`); each test is skipped without its own:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize \
//! DBINE_TEST_RISINGWAVE_URL=postgres://root@localhost:25023/dev \
//! DBINE_TEST_CRATEDB_URL=postgres://crate@localhost:25021/doc \
//! DBINE_TEST_H2_URL=postgres://sa:sa@localhost:25025/test \
//!   cargo test -p dbine-driver-postgres --test rename -- --ignored --test-threads=1
//! ```

use dbine_driver::rename::{rewrite_references, with_create_style, RewriteOptions};
use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyScan, Driver, ObjectRef, QueryOutcome, Relation, RenameRequest, RenameTarget,
    ReplaceStyle, Session,
};
use std::sync::Arc;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(format!("{e:?}")),
        None => Ok(out),
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    try_run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The first cell of a one-row query.
async fn one(s: &mut Box<dyn Session>, sql: &str) -> String {
    let out = run(s, sql).await;
    let rs = out.results.iter().find(|r| !r.columns.is_empty()).unwrap_or_else(|| panic!("{sql}: no rows"));
    let v = &rs.rows.first().unwrap_or_else(|| panic!("{sql}: no rows"))[0];
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some(schema.into()), name: name.into() }
}

/// What happened to a dependent.
#[derive(Debug, Clone, PartialEq)]
enum Fate {
    Engine,
    Tracked,
    Manual,
    Rewritten(String),
}

/// The app's plan, as `rename_impact` + `rename_script` build it: the
/// dependents sorted, the clean rewrites kept, dropped first when the
/// engine wants it, the rename, then the rewrites put back.
async fn plan(d: &dyn Driver, s: &mut Box<dyn Session>, target: RenameTarget, new: &str) -> (Vec<String>, Vec<(String, Fate)>) {
    let spec = d.rename_spec().expect("a rename spec");
    assert!(spec.allows(&target));
    let dialect = d.script_dialect();
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&target.dependency_target(), &scan).await.expect("dependents");
    let rewrite_target = target.rewrite_target();
    let mut fates = Vec::new();
    let mut rewrites: Vec<(ObjectRef, String)> = Vec::new();
    for dep in &report.items {
        let fate = if dep.relation != Relation::Code {
            Fate::Engine
        } else if spec.tracked.contains(&dep.kind) {
            Fate::Tracked
        } else if dep.confidence == Confidence::Review {
            Fate::Manual
        } else {
            let o = ObjectRef { kind: dep.kind.clone(), schema: dep.schema.clone(), name: dep.name.clone() };
            let body = s.definition(&o).await.expect("definition").expect("a definition");
            let opts = RewriteOptions {
                dependent_schema: dep.schema.clone(),
                keep_view_columns: dep.kind == kinds::VIEW || dep.kind == kinds::MATERIALIZED_VIEW,
                ..Default::default()
            };
            let r = rewrite_references(&body, &dialect, &rewrite_target, new, &spec, &opts);
            if r.edits.is_empty() || !r.unresolved.is_empty() {
                Fate::Manual
            } else {
                rewrites.push((o, with_create_style(&r.text, &dialect, spec.replace)));
                Fate::Rewritten(r.text)
            }
        };
        fates.push((dep.name.clone(), fate));
    }
    let definition = match &target {
        RenameTarget::Object { object, .. } if object.kind != kinds::TABLE => s.definition(object).await.ok().flatten(),
        _ => None,
    };
    let req = RenameRequest { target, new_name: new.into(), table: None, definition };
    let middle = d.rename_script(&req).expect("rename script");
    let mut statements = Vec::new();
    if spec.replace == ReplaceStyle::DropCreate {
        for (o, _) in rewrites.iter().rev() {
            let what = match o.kind.as_str() {
                kinds::MATERIALIZED_VIEW => "MATERIALIZED VIEW",
                kinds::FUNCTION => "FUNCTION",
                kinds::PROCEDURE => "PROCEDURE",
                _ => "VIEW",
            };
            statements.push(format!("DROP {what} IF EXISTS {};", dbine_driver::sql::qualified_name(dbine_driver::sql::Quote::Double, o.schema(), &o.name)));
        }
    }
    statements.extend(middle.statements);
    statements.extend(rewrites.into_iter().map(|(_, def)| def));
    (statements, fates)
}

/// Runs the script as `schema_sync_run` does: in one transaction when the
/// engine allows it, rolled back on the first error.
async fn apply(d: &dyn Driver, s: &mut Box<dyn Session>, statements: &[String]) -> Result<(), String> {
    let atomic = d.rename_spec().unwrap().transactional && d.supports_manual_transactions();
    if atomic {
        s.set_autocommit(false).await.map_err(|e| e.to_string())?;
    }
    for sql in statements {
        if let Err(e) = try_run(s, sql).await {
            if atomic {
                s.rollback().await.expect("rollback");
                s.set_autocommit(true).await.expect("autocommit");
            }
            return Err(format!("{sql}: {e}"));
        }
    }
    if atomic {
        s.commit().await.map_err(|e| e.to_string())?;
        s.set_autocommit(true).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn fate<'a>(fates: &'a [(String, Fate)], name: &str) -> Option<&'a Fate> {
    fates.iter().find(|(n, _)| n == name).map(|(_, f)| f)
}

const PG_FIXTURE: &[&str] = &[
    "DROP SCHEMA IF EXISTS rn_live CASCADE",
    "DROP SCHEMA IF EXISTS rn_live2 CASCADE",
    "CREATE SCHEMA rn_live",
    "CREATE TABLE rn_live.t (id int PRIMARY KEY, pepe int CONSTRAINT ck_pepe CHECK (pepe > 0))",
    "CREATE INDEX ix_pepe ON rn_live.t (pepe)",
    "CREATE TABLE rn_live.t2 (id int PRIMARY KEY, tid int CONSTRAINT fk_t REFERENCES rn_live.t (id))",
    "CREATE VIEW rn_live.v AS SELECT id, pepe FROM rn_live.t",
    "CREATE MATERIALIZED VIEW rn_live.mv AS SELECT count(*) AS n FROM rn_live.t",
    "CREATE FUNCTION rn_live.f_pepe(p int) RETURNS bigint LANGUAGE plpgsql AS $$\nBEGIN\n  RETURN (SELECT count(*) FROM rn_live.t WHERE t.pepe = p);\nEND $$",
    "CREATE FUNCTION rn_live.f_pepe(p int, q int) RETURNS bigint LANGUAGE sql AS $$ SELECT p::bigint + q $$",
    "CREATE FUNCTION rn_live.f_dyn() RETURNS bigint LANGUAGE plpgsql AS $$\nDECLARE n bigint;\nBEGIN\n  EXECUTE 'SELECT count(*) FROM rn_live.t' INTO n;\n  RETURN n;\nEND $$",
    "CREATE TABLE rn_live.other (id int PRIMARY KEY, pepe int)",
    "CREATE FUNCTION rn_live.f_other() RETURNS bigint LANGUAGE sql AS $$ SELECT count(*) FROM rn_live.other WHERE pepe > 0 $$",
    "CREATE FUNCTION rn_live.tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    "CREATE TRIGGER tg BEFORE INSERT ON rn_live.t FOR EACH ROW EXECUTE FUNCTION rn_live.tf()",
    "CREATE SEQUENCE rn_live.sq",
    "CREATE TYPE rn_live.mood AS ENUM ('a', 'b')",
    "CREATE DOMAIN rn_live.pos AS int CHECK (VALUE > 0)",
    "CREATE PROCEDURE rn_live.p_load(n int) LANGUAGE plpgsql AS $$ BEGIN INSERT INTO rn_live.t VALUES (n, n); END $$",
    "INSERT INTO rn_live.t VALUES (1, 1), (2, 1)",
    "INSERT INTO rn_live.t2 VALUES (1, 1)",
    "INSERT INTO rn_live.other VALUES (1, 5)",
];

#[tokio::test]
#[ignore]
async fn postgres_rename_with_impact() {
    let Some(cfg) = cfg("postgres", "DBINE_TEST_POSTGRES_URL") else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let d = driver("postgres");
    let mut s = d.connect(&cfg, None).await.expect("connect");
    for sql in PG_FIXTURE {
        run(&mut s, sql).await;
    }
    let other_before = s.definition(&obj(kinds::FUNCTION, "rn_live", "f_other")).await.unwrap().unwrap();

    // 1. The table, to a name that needs quotes.
    let (script, fates) = plan(d.as_ref(), &mut s, RenameTarget::Object { object: obj(kinds::TABLE, "rn_live", "t"), parent: None }, "T_Nueva").await;
    assert_eq!(fate(&fates, "v"), Some(&Fate::Tracked), "{fates:?}");
    assert_eq!(fate(&fates, "mv"), Some(&Fate::Tracked), "{fates:?}");
    assert_eq!(fate(&fates, "f_dyn"), Some(&Fate::Manual), "{fates:?}");
    assert_eq!(fate(&fates, "t2"), Some(&Fate::Engine), "{fates:?}");
    assert!(matches!(fate(&fates, "f_pepe"), Some(Fate::Rewritten(t)) if t.contains("FROM rn_live.\"T_Nueva\"")), "{fates:?}");
    assert!(matches!(fate(&fates, "p_load"), Some(Fate::Rewritten(_))), "{fates:?}");
    assert!(fate(&fates, "f_other").is_none(), "{fates:?}");
    assert_eq!(script[0], "ALTER TABLE \"rn_live\".\"t\" RENAME TO \"T_Nueva\";");
    apply(d.as_ref(), &mut s, &script).await.expect("rename the table");
    assert_eq!(one(&mut s, "SELECT count(*)::text FROM rn_live.v").await, "2");
    assert_eq!(one(&mut s, "SELECT rn_live.f_pepe(1)::text").await, "2");
    assert_eq!(one(&mut s, "SELECT rn_live.f_pepe(1, 2)::text").await, "3");
    run(&mut s, "CALL rn_live.p_load(3)").await;
    run(&mut s, "REFRESH MATERIALIZED VIEW rn_live.mv").await;
    assert_eq!(one(&mut s, "SELECT n::text FROM rn_live.mv").await, "3");
    // The dynamic one is left as it was (and now fails).
    assert!(try_run(&mut s, "SELECT rn_live.f_dyn()").await.is_err());
    assert_eq!(s.definition(&obj(kinds::FUNCTION, "rn_live", "f_other")).await.unwrap().unwrap(), other_before);

    // 2. A column of it.
    let table = obj(kinds::TABLE, "rn_live", "T_Nueva");
    let (script, fates) = plan(d.as_ref(), &mut s, RenameTarget::Column { table: table.clone(), column: "pepe".into() }, "pepe_nueva").await;
    assert_eq!(fate(&fates, "v"), Some(&Fate::Tracked), "{fates:?}");
    assert!(matches!(fate(&fates, "f_pepe"), Some(Fate::Rewritten(t)) if t.contains("\"T_Nueva\".pepe_nueva = p")), "{fates:?}");
    assert!(fate(&fates, "f_other").is_none(), "{fates:?}");
    apply(d.as_ref(), &mut s, &script).await.expect("rename the column");
    // The view keeps its output column; the function follows.
    assert_eq!(one(&mut s, "SELECT count(pepe)::text FROM rn_live.v").await, "3");
    assert_eq!(one(&mut s, "SELECT rn_live.f_pepe(1)::text").await, "2");
    assert_eq!(s.definition(&obj(kinds::FUNCTION, "rn_live", "f_other")).await.unwrap().unwrap(), other_before);

    // 3. Atomicity: a failing statement after the rename leaves nothing done.
    let (mut script, _) = plan(d.as_ref(), &mut s, RenameTarget::Object { object: table.clone(), parent: None }, "t_tres").await;
    script.push("SELECT 1/0".into());
    assert!(apply(d.as_ref(), &mut s, &script).await.is_err());
    assert_eq!(one(&mut s, "SELECT count(*)::text FROM rn_live.\"T_Nueva\"").await, "3");
    assert_eq!(one(&mut s, "SELECT count(*)::text FROM pg_class WHERE relname = 't_tres'").await, "0");
    assert_eq!(one(&mut s, "SELECT rn_live.f_pepe(1)::text").await, "2");

    // 4. Every other kind: the rename runs and the catalog has the new name.
    let rename = |target: RenameTarget, new: &str| (target, new.to_string());
    let cases = vec![
        rename(RenameTarget::Object { object: obj(kinds::VIEW, "rn_live", "v"), parent: None }, "v2"),
        rename(RenameTarget::Object { object: obj(kinds::MATERIALIZED_VIEW, "rn_live", "mv"), parent: None }, "mv2"),
        rename(RenameTarget::Object { object: obj(kinds::SEQUENCE, "rn_live", "sq"), parent: None }, "sq2"),
        rename(RenameTarget::Object { object: obj(kinds::TYPE, "rn_live", "mood"), parent: None }, "humor"),
        rename(RenameTarget::Object { object: obj(kinds::TYPE, "rn_live", "pos"), parent: None }, "positivo"),
        rename(RenameTarget::Object { object: obj(kinds::FUNCTION, "rn_live", "f_pepe"), parent: None }, "f_Pepe2"),
        rename(RenameTarget::Object { object: obj(kinds::PROCEDURE, "rn_live", "p_load"), parent: None }, "p_carga"),
        rename(RenameTarget::Object { object: obj(kinds::TRIGGER, "rn_live", "tg"), parent: Some("T_Nueva".into()) }, "tg2"),
        rename(RenameTarget::Index { table: table.clone(), index: "ix_pepe".into() }, "ix_nuevo"),
        rename(RenameTarget::Constraint { table: table.clone(), constraint: "ck_pepe".into() }, "ck_nuevo"),
    ];
    for (target, new) in cases {
        let what = format!("{target:?}");
        let (script, _) = plan(d.as_ref(), &mut s, target, &new).await;
        apply(d.as_ref(), &mut s, &script).await.unwrap_or_else(|e| panic!("{what}: {e}"));
    }
    let names = one(
        &mut s,
        "SELECT string_agg(n, ',' ORDER BY n) FROM (
           SELECT relname::text AS n FROM pg_class WHERE relnamespace = 'rn_live'::regnamespace
           UNION ALL SELECT typname::text FROM pg_type WHERE typnamespace = 'rn_live'::regnamespace AND typtype IN ('e', 'd')
           UNION ALL SELECT proname::text FROM pg_proc WHERE pronamespace = 'rn_live'::regnamespace
           UNION ALL SELECT tgname::text FROM pg_trigger WHERE NOT tgisinternal AND tgrelid = 'rn_live.\"T_Nueva\"'::regclass
           UNION ALL SELECT conname::text FROM pg_constraint WHERE conname = 'ck_nuevo') x",
    )
    .await;
    for n in ["v2", "mv2", "sq2", "humor", "positivo", "ix_nuevo", "ck_nuevo", "tg2", "p_carga"] {
        assert!(names.split(',').any(|x| x == n), "{n} not in {names}");
    }
    // Both overloads were renamed.
    assert_eq!(names.split(',').filter(|x| *x == "f_Pepe2").count(), 2, "{names}");
    assert_eq!(one(&mut s, "SELECT rn_live.\"f_Pepe2\"(1)::text").await, "2");
    // Views still work after everything.
    assert_eq!(one(&mut s, "SELECT count(*)::text FROM rn_live.v2").await, "3");

    // 5. The schema.
    let (script, _) = plan(d.as_ref(), &mut s, RenameTarget::Schema { database: None, schema: "rn_live".into() }, "rn_live2").await;
    apply(d.as_ref(), &mut s, &script).await.expect("rename the schema");
    assert_eq!(one(&mut s, "SELECT count(*)::text FROM rn_live2.v2").await, "3");
    run(&mut s, "DROP SCHEMA rn_live2 CASCADE").await;
}

#[tokio::test]
#[ignore]
async fn cockroach_rename_with_impact() {
    let Some(cfg) = cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL") else {
        eprintln!("DBINE_TEST_COCKROACH_URL not set; skipping");
        return;
    };
    let d = driver("cockroachdb");
    let mut s = d.connect(&cfg, None).await.expect("connect");
    for sql in [
        "DROP SCHEMA IF EXISTS rn_live CASCADE",
        "DROP SCHEMA IF EXISTS rn_live2 CASCADE",
        "CREATE SCHEMA rn_live",
        "CREATE TABLE rn_live.t (id INT PRIMARY KEY, pepe INT CONSTRAINT ck_pepe CHECK (pepe > 0))",
        "CREATE INDEX ix_pepe ON rn_live.t (pepe)",
        "CREATE TABLE rn_live.t2 (id INT PRIMARY KEY, tid INT CONSTRAINT fk_t REFERENCES rn_live.t (id))",
        "CREATE VIEW rn_live.v AS SELECT id, pepe FROM rn_live.t",
        "CREATE FUNCTION rn_live.f_pepe(p INT) RETURNS INT LANGUAGE SQL AS $$ SELECT count(*)::INT FROM rn_live.t WHERE t.pepe = p $$",
        "CREATE TABLE rn_live.other (id INT PRIMARY KEY, pepe INT)",
        "CREATE FUNCTION rn_live.f_other() RETURNS INT LANGUAGE SQL AS $$ SELECT count(*)::INT FROM rn_live.other WHERE pepe > 0 $$",
        "INSERT INTO rn_live.t VALUES (1, 1), (2, 1)",
        "INSERT INTO rn_live.t2 VALUES (1, 1)",
    ] {
        run(&mut s, sql).await;
    }
    let other_before = s.definition(&obj(kinds::FUNCTION, "rn_live", "f_other")).await.unwrap().unwrap();

    // The view and the function block the rename: dropped first, created after.
    let (script, fates) = plan(d.as_ref(), &mut s, RenameTarget::Object { object: obj(kinds::TABLE, "rn_live", "t"), parent: None }, "T_Nueva").await;
    assert!(matches!(fate(&fates, "v"), Some(Fate::Rewritten(_))), "{fates:?}");
    assert!(matches!(fate(&fates, "f_pepe"), Some(Fate::Rewritten(_))), "{fates:?}");
    assert!(fate(&fates, "f_other").is_none(), "{fates:?}");
    assert!(script[0].starts_with("DROP "), "{script:?}");
    apply(d.as_ref(), &mut s, &script).await.expect("rename the table");
    assert_eq!(one(&mut s, "SELECT count(*)::STRING FROM rn_live.v").await, "2");
    assert_eq!(one(&mut s, "SELECT rn_live.f_pepe(1)::STRING").await, "2");

    let table = obj(kinds::TABLE, "rn_live", "T_Nueva");
    let (script, fates) = plan(d.as_ref(), &mut s, RenameTarget::Column { table: table.clone(), column: "pepe".into() }, "pepe_nueva").await;
    assert!(matches!(fate(&fates, "f_pepe"), Some(Fate::Rewritten(t)) if t.contains("pepe_nueva")), "{fates:?}");
    apply(d.as_ref(), &mut s, &script).await.expect("rename the column");
    assert_eq!(one(&mut s, "SELECT count(pepe)::STRING FROM rn_live.v").await, "2");
    assert_eq!(one(&mut s, "SELECT rn_live.f_pepe(1)::STRING").await, "2");
    assert_eq!(s.definition(&obj(kinds::FUNCTION, "rn_live", "f_other")).await.unwrap().unwrap(), other_before);

    // Not atomic: CockroachDB commits the open transaction before each DDL
    // statement (`autocommit_before_ddl`), so a failure keeps what ran.
    assert!(!d.rename_spec().unwrap().transactional);
    let on = one(&mut s, "SHOW autocommit_before_ddl").await;
    run(&mut s, "CREATE TABLE rn_live.solo (a INT)").await;
    run(&mut s, "BEGIN").await;
    run(&mut s, "ALTER TABLE rn_live.solo RENAME TO t_tres").await;
    assert!(try_run(&mut s, "SELECT 1/0").await.is_err());
    run(&mut s, "ROLLBACK").await;
    let kept = one(&mut s, "SELECT count(*)::STRING FROM information_schema.tables WHERE table_schema = 'rn_live' AND table_name = 't_tres'").await;
    assert_eq!(kept == "1", on == "on", "autocommit_before_ddl = {on}");

    for (target, new) in [
        (RenameTarget::Index { table: table.clone(), index: "ix_pepe".into() }, "ix_nuevo"),
        (RenameTarget::Constraint { table: table.clone(), constraint: "ck_pepe".into() }, "ck_nuevo"),
        (RenameTarget::Object { object: obj(kinds::FUNCTION, "rn_live", "f_other"), parent: None }, "f_otra"),
        (RenameTarget::Object { object: obj(kinds::VIEW, "rn_live", "v"), parent: None }, "v2"),
    ] {
        let what = format!("{target:?}");
        let (script, _) = plan(d.as_ref(), &mut s, target, new).await;
        apply(d.as_ref(), &mut s, &script).await.unwrap_or_else(|e| panic!("{what}: {e}"));
    }
    assert_eq!(one(&mut s, "SELECT rn_live.f_otra()::STRING").await, "0");
    assert_eq!(one(&mut s, "SELECT count(*)::STRING FROM rn_live.v2").await, "2");
    let create = one(&mut s, "SELECT create_statement FROM [SHOW CREATE TABLE rn_live.\"T_Nueva\"]").await;
    assert!(create.contains("ix_nuevo") && create.contains("ck_nuevo"), "{create}");
    run(&mut s, "DROP SCHEMA rn_live CASCADE").await;
}

/// Materialize and RisingWave: the engine rewrites the views and
/// materialized views that use what's renamed.
async fn streaming(id: &str, env: &str) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    for sql in [
        "DROP SCHEMA IF EXISTS rn_live CASCADE",
        "CREATE SCHEMA rn_live",
        "CREATE TABLE rn_live.t (id INT, pepe INT)",
        "CREATE VIEW rn_live.v AS SELECT id, pepe FROM rn_live.t",
        "CREATE MATERIALIZED VIEW rn_live.mv AS SELECT count(*) AS n FROM rn_live.v",
        "INSERT INTO rn_live.t VALUES (1, 1)",
    ] {
        run(&mut s, sql).await;
    }
    for (target, new) in [
        (RenameTarget::Object { object: obj(kinds::TABLE, "rn_live", "t"), parent: None }, "T_Nueva"),
        (RenameTarget::Object { object: obj(kinds::VIEW, "rn_live", "v"), parent: None }, "v2"),
    ] {
        let (script, fates) = plan(d.as_ref(), &mut s, target, new).await;
        assert!(fates.iter().all(|(_, f)| *f == Fate::Tracked || *f == Fate::Engine), "{fates:?}");
        apply(d.as_ref(), &mut s, &script).await.expect("rename");
    }
    let (script, _) = plan(d.as_ref(), &mut s, RenameTarget::Object { object: obj(kinds::MATERIALIZED_VIEW, "rn_live", "mv"), parent: None }, "mv2").await;
    apply(d.as_ref(), &mut s, &script).await.expect("rename");
    if id == "risingwave" {
        run(&mut s, "FLUSH").await;
    }
    assert_eq!(one(&mut s, "SELECT n::text FROM rn_live.mv2").await, "1");
    assert_eq!(one(&mut s, "SELECT count(*)::text FROM rn_live.\"T_Nueva\"").await, "1");
    run(&mut s, "DROP SCHEMA rn_live CASCADE").await;
}

#[tokio::test]
#[ignore]
async fn materialize_rename() {
    streaming("materialize", "DBINE_TEST_MATERIALIZE_URL").await;
}

#[tokio::test]
#[ignore]
async fn risingwave_rename() {
    streaming("risingwave", "DBINE_TEST_RISINGWAVE_URL").await;
}

/// CrateDB and H2 leave the views that use a renamed table broken: they
/// are rewritten and put back with CREATE OR REPLACE.
async fn views_rewritten(id: &str, env: &str, schema: &str) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let _ = try_run(&mut s, &format!("DROP VIEW IF EXISTS {schema}.rn_v")).await;
    for t in ["rn_t", "rn_t2"] {
        let _ = try_run(&mut s, &format!("DROP TABLE IF EXISTS {schema}.{t}")).await;
    }
    run(&mut s, &format!("CREATE TABLE {schema}.rn_t (id INT PRIMARY KEY, pepe INT)")).await;
    run(&mut s, &format!("CREATE VIEW {schema}.rn_v AS SELECT id, pepe FROM {schema}.rn_t")).await;
    run(&mut s, &format!("INSERT INTO {schema}.rn_t VALUES (1, 1)")).await;
    if id == "cratedb" {
        run(&mut s, &format!("REFRESH TABLE {schema}.rn_t")).await;
    }
    let (script, fates) = plan(d.as_ref(), &mut s, RenameTarget::Object { object: obj(kinds::TABLE, schema, "rn_t"), parent: None }, "rn_t2").await;
    assert!(matches!(fate(&fates, "rn_v"), Some(Fate::Rewritten(_))), "{fates:?}");
    apply(d.as_ref(), &mut s, &script).await.expect("rename the table");
    assert_eq!(one(&mut s, &format!("SELECT count(*) FROM {schema}.rn_v")).await, "1");
    let (script, fates) =
        plan(d.as_ref(), &mut s, RenameTarget::Column { table: obj(kinds::TABLE, schema, "rn_t2"), column: "pepe".into() }, "pepe2").await;
    assert!(matches!(fate(&fates, "rn_v"), Some(Fate::Rewritten(_))), "{fates:?}");
    apply(d.as_ref(), &mut s, &script).await.expect("rename the column");
    assert_eq!(one(&mut s, &format!("SELECT count(pepe) FROM {schema}.rn_v")).await, "1");
    run(&mut s, &format!("DROP VIEW {schema}.rn_v")).await;
    run(&mut s, &format!("DROP TABLE {schema}.rn_t2")).await;
}

#[tokio::test]
#[ignore]
async fn cratedb_rename() {
    views_rewritten("cratedb", "DBINE_TEST_CRATEDB_URL", "doc").await;
}

#[tokio::test]
#[ignore]
async fn h2_rename() {
    views_rewritten("h2", "DBINE_TEST_H2_URL", "public").await;
}
