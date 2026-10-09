//! "Renombrar…" on a database against real servers, the way the app does
//! it: the objects read with `list_objects` + `definition` from a session
//! on the database, the driver's script, each statement run on its own from
//! a connection without a database, then everything checked in the new one.
//!
//! Fixture (`latin1` / `latin1_spanish_ci`, to see the defaults copied):
//! `clientes` and `pedidos` with a foreign key between them and rows, the
//! view `v_pedidos` (fully qualified, as the server stores it) and
//! `v_resumen` over it, the function `f_doble`, the procedure `p_total`
//! naming the database, and the trigger `tr_pedidos` calling `f_doble`.
//! TiDB isn't offered it (its foreign keys keep naming the old database
//! after `RENAME TABLE`): its test checks that. Each test reads `DBINE_TEST_<ENGINE>_URL`
//! and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB8_URL=mysql://root@localhost:25044 \
//!   cargo test -p dbine-driver-mysql --test rename_database -- --ignored --nocapture --test-threads 1
//! ```

use dbine_driver::rename::DatabaseObject;
use dbine_driver::{kinds, ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use std::sync::Arc;

const OLD: &str = "dbine_dbren";
const NEW: &str = "dbine_dbren_nueva";

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

async fn one(s: &mut Box<dyn Session>, sql: &str) -> String {
    col(s, sql).await.into_iter().next().unwrap_or_default()
}

/// Code kinds the app reads a definition for (`MOVED_CODE`).
const MOVED_CODE: &[&str] = &[kinds::VIEW, kinds::MATERIALIZED_VIEW, kinds::PROCEDURE, kinds::FUNCTION, kinds::TRIGGER, "event"];

/// `RenameImpact::objects`, as `rename_database_impact` builds it.
async fn objects(d: &Arc<dyn Driver>, cfg: &ConnectionConfig) -> Vec<DatabaseObject> {
    let mut s = d.connect(cfg, Some(OLD)).await.unwrap();
    let mut out = Vec::new();
    for o in s.list_objects().await.unwrap() {
        let code = MOVED_CODE.contains(&o.kind.as_str());
        if !code && o.kind != kinds::TABLE && o.kind != kinds::COLLECTION {
            continue;
        }
        let definition = if code {
            let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
            Some(s.definition(&r).await.unwrap().unwrap_or_else(|| panic!("no definition for {}", o.name)))
        } else {
            None
        };
        out.push(DatabaseObject { kind: o.kind, schema: o.schema, name: o.name, definition });
    }
    out
}

async fn clean(root: &mut Box<dyn Session>) {
    ok(root, &format!("DROP DATABASE IF EXISTS {OLD}")).await;
    ok(root, &format!("DROP DATABASE IF EXISTS {NEW}")).await;
}

/// `code`: routines and a trigger.
async fn setup(root: &mut Box<dyn Session>, code: bool) {
    clean(root).await;
    let mut sqls = vec![
        if code { format!("CREATE DATABASE {OLD} CHARACTER SET latin1 COLLATE latin1_spanish_ci") } else { format!("CREATE DATABASE {OLD}") },
        format!("CREATE TABLE {OLD}.clientes (id INT PRIMARY KEY, nombre VARCHAR(20) NOT NULL)"),
        format!(
            "CREATE TABLE {OLD}.pedidos (id INT PRIMARY KEY, cliente_id INT NOT NULL, total INT NOT NULL, CONSTRAINT fk_pedidos_clientes FOREIGN KEY (cliente_id) REFERENCES {OLD}.clientes (id))"
        ),
        format!("INSERT INTO {OLD}.clientes VALUES (1, 'Ana'), (2, 'Beto')"),
        format!("INSERT INTO {OLD}.pedidos VALUES (10, 1, 100), (11, 1, 50), (12, 2, 7)"),
        format!("CREATE VIEW {OLD}.v_pedidos AS SELECT c.nombre, p.total FROM {OLD}.pedidos p JOIN {OLD}.clientes c ON c.id = p.cliente_id"),
        format!("CREATE VIEW {OLD}.v_resumen AS SELECT nombre, SUM(total) AS total FROM {OLD}.v_pedidos GROUP BY nombre"),
    ];
    if code {
        sqls.extend([
            format!("CREATE FUNCTION {OLD}.f_doble(x INT) RETURNS INT DETERMINISTIC RETURN x * 2"),
            format!("CREATE PROCEDURE {OLD}.p_total() BEGIN SELECT SUM(total) FROM {OLD}.pedidos; END"),
            format!("CREATE TRIGGER {OLD}.tr_pedidos BEFORE INSERT ON {OLD}.pedidos FOR EACH ROW SET NEW.total = f_doble(NEW.total)"),
        ]);
    }
    for sql in sqls {
        ok(root, &sql).await;
    }
}

async fn flow(id: &str, env: &str, code: bool) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipped");
        return;
    };
    let d = driver(id);
    let cfg = parse_url(id, &url);
    let mut root = d.connect(&cfg, None).await.unwrap();
    setup(&mut root, code).await;

    let spec = d.rename_spec().unwrap();
    assert!(spec.databases && spec.database_moves);
    let objects = objects(&d, &cfg).await;
    let kinds_of: Vec<&str> = objects.iter().map(|o| o.kind.as_str()).collect();
    assert_eq!(kinds_of.iter().filter(|k| **k == kinds::TABLE).count(), 2, "{kinds_of:?}");
    assert_eq!(kinds_of.iter().filter(|k| **k == kinds::VIEW).count(), 2, "{kinds_of:?}");
    if code {
        for k in [kinds::FUNCTION, kinds::PROCEDURE, kinds::TRIGGER] {
            assert!(kinds_of.contains(&k), "{k}: {kinds_of:?}");
        }
    }

    let script = d.rename_database_script(OLD, NEW, &objects).unwrap();
    for w in &script.warnings {
        eprintln!("! {w}");
    }
    // The way the app runs it: one by one, on a connection without a database.
    let mut runner = d.connect(&cfg, None).await.unwrap();
    for st in &script.statements {
        eprintln!("> {st}");
        ok(&mut runner, st).await;
    }

    let mut s = d.connect(&cfg, Some(NEW)).await.unwrap();
    // The old one is gone; the new one has the old defaults.
    let dbs = col(&mut s, "SHOW DATABASES").await;
    assert!(!dbs.iter().any(|x| x == OLD), "{dbs:?}");
    if code {
        assert_eq!(one(&mut s, &format!("SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{NEW}'")).await, "latin1_spanish_ci");
    }
    // Tables with their rows.
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM clientes").await, "2");
    assert_eq!(one(&mut s, "SELECT COUNT(*) FROM pedidos").await, "3");
    // The foreign key works.
    let fk = one(
        &mut s,
        &format!("SELECT REFERENCED_TABLE_SCHEMA FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_SCHEMA = '{NEW}' AND TABLE_NAME = 'pedidos' AND REFERENCED_TABLE_NAME = 'clientes'"),
    )
    .await;
    assert_eq!(fk, NEW);
    assert!(run(&mut s, "INSERT INTO pedidos VALUES (20, 99, 1)").await.is_err(), "the foreign key let an orphan in");
    // The views read the new tables.
    assert_eq!(one(&mut s, "SELECT total FROM v_resumen WHERE nombre = 'Ana'").await, "150");
    let views = col(&mut s, &format!("SELECT VIEW_DEFINITION FROM information_schema.VIEWS WHERE TABLE_SCHEMA = '{NEW}'")).await;
    assert!(views.iter().all(|v| !v.contains(OLD) || v.contains(NEW)), "{views:?}");
    if code {
        // The trigger runs the function, both in the new database.
        ok(&mut s, "INSERT INTO pedidos VALUES (21, 2, 4)").await;
        assert_eq!(one(&mut s, "SELECT total FROM pedidos WHERE id = 21").await, "8");
        assert_eq!(one(&mut s, "SELECT f_doble(21)").await, "42");
        // The procedure reads the new table (it named the old database).
        assert_eq!(one(&mut s, "CALL p_total()").await, "165");
        let schema = one(&mut s, "SELECT EVENT_OBJECT_SCHEMA FROM information_schema.TRIGGERS WHERE TRIGGER_NAME = 'tr_pedidos'").await;
        assert_eq!(schema, NEW);
    }
    clean(&mut root).await;
}

/// Something the app doesn't read (an event; a MariaDB sequence) refuses
/// the rename at the first statement: nothing is created and the old
/// database keeps everything.
async fn refused(id: &str, env: &str, extra: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipped");
        return;
    };
    let d = driver(id);
    let cfg = parse_url(id, &url);
    let mut root = d.connect(&cfg, None).await.unwrap();
    setup(&mut root, true).await;
    ok(&mut root, &extra.replace("{OLD}", OLD)).await;
    let objects = objects(&d, &cfg).await;
    let script = d.rename_database_script(OLD, NEW, &objects).unwrap();
    let mut runner = d.connect(&cfg, None).await.unwrap();
    let err = run(&mut runner, &script.statements[0]).await.unwrap_err();
    eprintln!("statement 1: {err}");
    assert!(err.contains("nada cambió"), "{err}");
    let dbs = col(&mut root, "SHOW DATABASES").await;
    assert!(dbs.iter().any(|x| x == OLD) && !dbs.iter().any(|x| x == NEW), "{dbs:?}");
    let count = |t: &str, c: &str| format!("SELECT COUNT(*) FROM information_schema.{t} WHERE {c} = '{OLD}'");
    assert_eq!(one(&mut root, &count("TABLES", "TABLE_SCHEMA")).await, if id == "mariadb" && extra.contains("SEQUENCE") { "5" } else { "4" });
    assert_eq!(one(&mut root, &count("ROUTINES", "ROUTINE_SCHEMA")).await, "2");
    assert_eq!(one(&mut root, &count("TRIGGERS", "TRIGGER_SCHEMA")).await, "1");
    assert_eq!(one(&mut root, &format!("SELECT COUNT(*) FROM {OLD}.pedidos")).await, "3");
    clean(&mut root).await;
}

const EVENT_SQL: &str = "CREATE EVENT {OLD}.ev_limpia ON SCHEDULE EVERY 1 DAY DISABLE DO DELETE FROM {OLD}.pedidos WHERE total < 0";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql_rename_database() {
    flow("mysql", "DBINE_TEST_MYSQL_URL", true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql_rename_database_refuses_an_event() {
    refused("mysql", "DBINE_TEST_MYSQL_URL", EVENT_SQL).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb_rename_database() {
    flow("mariadb", "DBINE_TEST_MARIADB_URL", true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb_rename_database_refuses_an_event() {
    refused("mariadb", "DBINE_TEST_MARIADB_URL", EVENT_SQL).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb_rename_database_refuses_a_sequence() {
    refused("mariadb", "DBINE_TEST_MARIADB_URL", "CREATE SEQUENCE {OLD}.s_pedidos").await;
}

/// TiDB moves the tables but leaves their foreign keys on the old
/// database: it doesn't offer the rename.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tidb_doesnt_rename_databases() {
    let d = driver("tidb");
    assert!(!d.rename_spec().unwrap().databases);
    assert!(d.rename_database_script(OLD, NEW, &[]).is_err());
}
