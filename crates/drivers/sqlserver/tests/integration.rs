//! Against a real server. Reads `DBINE_TEST_SQLSERVER_URL`
//! (`mssql://user:pass@host:port`) and is skipped without it:
//!
//! ```sh
//! docker run -d --name dbine-test-mssql -e ACCEPT_EULA=Y -e 'MSSQL_SA_PASSWORD=Pw_12345!' \
//!   -p 25013:1433 mcr.microsoft.com/mssql/server:2022-latest
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test integration -- --ignored
//! ```

use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::time::{Duration, Instant};

fn parse_url(url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.map(|_| out)
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some("dbo".into()), name: name.into() }
}

#[tokio::test]
#[ignore]
async fn sqlserver() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url);
    let d = dbine_driver_sqlserver::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let version = admin.server_version().await.unwrap();
    eprintln!("{version}");
    assert!(version.contains("SQL Server"));
    run(
        &mut admin,
        "IF DB_ID('dbine_t') IS NOT NULL BEGIN ALTER DATABASE dbine_t SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_t; END
         GO
         CREATE DATABASE dbine_t",
    )
    .await
    .unwrap();
    assert!(admin.list_databases().await.unwrap().contains(&"dbine_t".to_string()));

    let mut s = d.connect(&cfg, Some("dbine_t")).await.unwrap();
    // GO batches: CREATE VIEW / PROCEDURE must start a batch.
    run(
        &mut s,
        "CREATE TABLE dbo.items (id int IDENTITY PRIMARY KEY, name nvarchar(40) NOT NULL DEFAULT 'x', price decimal(10,2), at datetime2(3));
         INSERT INTO dbo.items (name, price, at) VALUES ('a', 1.5, '2024-01-31 13:45:00'), ('b', 2, NULL), ('c', NULL, NULL);
         GO
         CREATE VIEW dbo.v_items AS SELECT id, name FROM dbo.items
         GO
         CREATE PROCEDURE dbo.noop AS SELECT 1
         GO
         CREATE FUNCTION dbo.add_one(@x int) RETURNS int AS BEGIN RETURN @x + 1 END
         GO
         CREATE TRIGGER dbo.items_trg ON dbo.items AFTER INSERT AS SET NOCOUNT ON",
    )
    .await
    .unwrap();

    let objs = s.list_objects().await.unwrap();
    let find = |n: &str| objs.iter().find(|o| o.name == n);
    assert_eq!(find("items").unwrap().kind, kinds::TABLE);
    assert_eq!(find("v_items").unwrap().kind, kinds::VIEW);
    assert_eq!(find("noop").unwrap().kind, kinds::PROCEDURE);
    assert_eq!(find("add_one").unwrap().kind, kinds::FUNCTION);
    let trg = find("items_trg").unwrap();
    assert_eq!((trg.kind.as_str(), trg.parent.as_deref()), (kinds::TRIGGER, Some("items")));

    let cols = s.columns(&obj(kinds::TABLE, "items")).await.unwrap();
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "price", "at"]);
    assert!(cols[0].primary_key && cols[0].auto_increment);
    assert_eq!(cols[1].data_type, "nvarchar(40)");
    assert!(!cols[1].nullable && cols[2].nullable);

    assert_eq!(s.definition(&obj(kinds::TABLE, "items")).await.unwrap(), None);
    for (k, n) in [(kinds::VIEW, "v_items"), (kinds::PROCEDURE, "noop"), (kinds::FUNCTION, "add_one"), (kinds::TRIGGER, "items_trg")] {
        let def = s.definition(&obj(k, n)).await.unwrap().unwrap();
        assert!(def.contains("CREATE"), "{def}");
    }

    let q = s.browse_query(&obj(kinds::TABLE, "items"), 2);
    let out = run(&mut s, &q).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.results[0].rows[0][2], serde_json::json!("1.50"));
    assert_eq!(out.results[0].rows[0][3], serde_json::json!("2024-01-31 13:45:00"));

    // An error in the second batch keeps the first one's result.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1 AS a\nGO\nSELECT * FROM dbo.nope\nGO\nSELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(out.results.len(), 1);
    // The driver recorded it with its code and line (the third line).
    assert_eq!(out.errors.len(), 1);
    assert_eq!((out.errors[0].code.as_deref(), out.errors[0].line), (Some("208"), Some(3)));
    run(&mut s, "SELECT 1").await.unwrap();

    let mut out = QueryOutcome::default();
    s.execute("SELECT * FROM dbo.items", 1, &mut out).await.unwrap();
    assert_eq!((out.results[0].rows.len(), out.results[0].total_rows, out.results[0].truncated), (1, 3, true));

    // Read-only intent doesn't stop writes on a standalone server; the
    // ReadOnlySession wrapper does that. It must still connect.
    let mut ro_cfg = cfg.clone();
    ro_cfg.read_only = true;
    let mut ro = d.connect(&ro_cfg, Some("dbine_t")).await.unwrap();
    run(&mut ro, "SELECT 1").await.unwrap();
    let mut wrapped: Box<dyn Session> = Box::new(dbine_driver::read_only::ReadOnlySession::new(ro));
    assert!(matches!(run(&mut wrapped, "DELETE FROM dbo.items").await, Err(Error::Query(_))));
    drop(wrapped);

    // Cancel: a TDS attention on the session's connection, which survives.
    let stop = s.interrupter().expect("interrupter");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        stop();
    });
    let t = Instant::now();
    let mut out = QueryOutcome::default();
    let e = s.execute("WAITFOR DELAY '00:00:30'", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(10));
    run(&mut s, "SELECT 1").await.expect("the session survives the cancel");
    drop(s);

    run(
        &mut admin,
        "ALTER DATABASE dbine_t SET SINGLE_USER WITH ROLLBACK IMMEDIATE\nGO\nDROP DATABASE dbine_t",
    )
    .await
    .unwrap();

    let mut bad = cfg.clone();
    bad.password = Some("definitely-wrong".into());
    match d.connect(&bad, None).await {
        Err(Error::AuthFailed(m)) => eprintln!("auth refused: {m}"),
        Err(e) => panic!("expected AuthFailed, got {e:?}"),
        Ok(_) => panic!("wrong password accepted"),
    }
}

/// Execution plans against the same server: the estimated plan runs
/// nothing; the actual one runs once and brings results plus plans.
/// Needs a `ventas` database with `pedidos`/`clientes` (see the docker
/// recipe in the module doc, plus any data).
#[tokio::test]
#[ignore]
async fn sqlserver_plans() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let mut cfg = parse_url(&url);
    cfg.trust_server_certificate = true;
    let d = dbine_driver_sqlserver::drivers().remove(0);
    let mut s = d.connect(&cfg, Some("ventas")).await.expect("connect");
    run(&mut s, "IF OBJECT_ID('dbo.plan_probe') IS NULL CREATE TABLE dbo.plan_probe(id int); DELETE dbo.plan_probe; INSERT dbo.plan_probe VALUES (1)").await.unwrap();
    let sql = "select c.ciudad, count(*) n, sum(p.total) total from pedidos p join clientes c on c.id = p.cliente_id \
               where p.estado = 'pagado' and p.fecha >= '2026-01-01' group by c.ciudad order by total desc;\n\
               delete from dbo.plan_probe;";

    let mut est = QueryOutcome::default();
    s.explain(sql, false, 100, &mut est).await.expect("estimated");
    assert_eq!(est.plans.len(), 2, "{:?}", est.messages);
    assert!(est.results.is_empty(), "nothing ran");
    assert!(!est.plans[0].actual);
    let left = run(&mut s, "select count(*) from dbo.plan_probe").await.unwrap();
    assert_eq!(left.results[0].rows[0][0], serde_json::json!(1), "the estimated DELETE didn't run");

    let mut act = QueryOutcome::default();
    s.explain(sql, true, 100, &mut act).await.expect("actual");
    assert_eq!(act.plans.len(), 2);
    assert!(act.plans.iter().all(|p| p.actual));
    assert_eq!(act.results.iter().filter(|r| !r.columns.is_empty()).count(), 1, "the SELECT's rows, not the plan XML");
    let join = &act.plans[0].root.children[0];
    fn dump(n: &dbine_driver::PlanNode, d: usize) {
        eprintln!("{}{} [{}] {:?} self={:?} est={:?} act={:?} {:?}", "  ".repeat(d), n.op, n.detail, n.object, n.self_cost, n.est_rows, n.actual_rows, n.warnings);
        n.children.iter().for_each(|c| dump(c, d + 1));
    }
    dump(&act.plans[0].root, 0);
    assert!(join.actual_rows.is_some());
    let left = run(&mut s, "select count(*) from dbo.plan_probe").await.unwrap();
    assert_eq!(left.results[0].rows[0][0], serde_json::json!(0), "the actual run executed the DELETE");

    // The session is back to normal: no showplan left switched on.
    let plain = run(&mut s, "select 1 as uno").await.unwrap();
    assert_eq!(plain.results[0].columns[0].name, "uno");
}

fn driver(id: &str) -> std::sync::Arc<dyn dbine_driver::Driver> {
    dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

/// Prints the snapshot and checks it carries real figures.
fn check_snapshot(snap: &dbine_driver::MonitorSnapshot, must: &[&str]) {
    for m in &snap.metrics {
        eprintln!("{:<22} {:<40} {:?} max={:?}{}", m.key, m.label, m.value, m.max, if m.counter { " (counter)" } else { "" });
    }
    for t in &snap.tables {
        eprintln!("[{}] {} rows; first: {:?}", t.key, t.rows.len(), t.rows.first());
    }
    eprintln!("info: {:?}\nnotes: {:?}", snap.info, snap.notes);
    for k in must {
        let m = snap.metrics.iter().find(|m| m.key == *k).unwrap_or_else(|| panic!("no metric {k}"));
        assert!(m.value.is_some(), "{k} has no value");
    }
    assert!(snap.tables.iter().any(|t| t.key == "sessions" && !t.rows.is_empty()));
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let d = driver("sqlserver");
    assert!(d.capabilities().monitor);
    let mut s = d.connect(&parse_url(&url), None).await.expect("connect");
    // Another session for the sessions table (the monitor's own is left out).
    let _other = d.connect(&parse_url(&url), None).await.unwrap();
    let snap = s.monitor().await.expect("monitor");
    check_snapshot(
        &snap,
        &["cpu_time", "mem_used", "connections", "queries", "transactions", "cache_hit", "disk_read", "storage_used", "uptime"],
    );
    for t in ["waits", "databases"] {
        assert!(snap.tables.iter().any(|x| x.key == t && !x.rows.is_empty()), "{t}");
    }
    assert!(snap.info.iter().any(|(k, _)| k == "Edición"));
    // Counters go up between snapshots.
    run(&mut s, "SELECT COUNT(*) FROM sys.objects").await.unwrap();
    let again = s.monitor().await.unwrap();
    let q = |s: &dbine_driver::MonitorSnapshot| s.metrics.iter().find(|m| m.key == "queries").and_then(|m| m.value).unwrap();
    assert!(q(&again) > q(&snap));
}

/// Babelfish for PostgreSQL through its TDS port:
///
/// ```sh
/// docker run -d --name dbine-test-babelfish -p 25714:1433 jonathanpotts/babelfishpg
/// DBINE_TEST_BABELFISH_URL='mssql://babelfish_user:12345678@localhost:25714' \
///   cargo test -p dbine-driver-sqlserver --test integration babelfish -- --ignored
/// ```
#[tokio::test]
#[ignore]
async fn babelfish() {
    let Ok(url) = std::env::var("DBINE_TEST_BABELFISH_URL") else {
        eprintln!("DBINE_TEST_BABELFISH_URL not set; skipping");
        return;
    };
    let mut cfg = parse_url(&url);
    cfg.driver = "babelfish".into();
    let d = driver("babelfish");
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let version = admin.server_version().await.unwrap();
    eprintln!("{version}");
    assert!(version.contains("Babelfish"));
    if admin.list_databases().await.unwrap().contains(&"dbine_bbf".to_string()) {
        admin.drop_database("dbine_bbf").await.unwrap();
    }
    admin.create_database("dbine_bbf").await.expect("create database");
    assert!(admin.list_databases().await.unwrap().contains(&"dbine_bbf".to_string()));

    let mut s = d.connect(&cfg, Some("dbine_bbf")).await.unwrap();
    run(
        &mut s,
        "CREATE TABLE dbo.clientes (id int IDENTITY PRIMARY KEY, nombre nvarchar(40) NOT NULL DEFAULT 'x');
         CREATE TABLE dbo.pedidos (id int IDENTITY PRIMARY KEY, cliente_id int NOT NULL REFERENCES dbo.clientes (id), total decimal(10,2));
         CREATE INDEX ix_pedidos_cliente ON dbo.pedidos (cliente_id);
         INSERT INTO dbo.clientes (nombre) VALUES ('a'), ('b');
         INSERT INTO dbo.pedidos (cliente_id, total) VALUES (1, 10.5), (2, 3);
         GO
         CREATE VIEW dbo.v_pedidos AS SELECT id, total FROM dbo.pedidos
         GO
         CREATE PROCEDURE dbo.noop AS SELECT 1",
    )
    .await
    .unwrap();
    let objs = s.list_objects().await.unwrap();
    for n in ["clientes", "pedidos", "v_pedidos", "noop"] {
        assert!(objs.iter().any(|o| o.name == n), "{n} in {objs:?}");
    }
    let cols = s.columns(&obj(kinds::TABLE, "pedidos")).await.unwrap();
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "cliente_id", "total"]);
    assert!(cols[0].primary_key);
    let def = s.definition(&obj(kinds::VIEW, "v_pedidos")).await.unwrap();
    eprintln!("view: {def:?}");
    let schema = s.database_schema().await.expect("schema");
    let pedidos = schema.iter().find(|t| t.name == "pedidos").unwrap();
    eprintln!("pedidos: {pedidos:?}");
    assert_eq!(pedidos.foreign_keys.len(), 1);
    let ddl = d.table_ddl(pedidos, dbine_driver::DdlParts { create: true, ..Default::default() }).unwrap();
    assert!(ddl.contains("CREATE TABLE [dbo].[pedidos]"), "{ddl}");

    // Plans: PostgreSQL text plans through the BABELFISH_* options.
    let sql = "SELECT c.nombre, SUM(p.total) FROM dbo.pedidos p JOIN dbo.clientes c ON c.id = p.cliente_id GROUP BY c.nombre";
    let mut est = QueryOutcome::default();
    s.explain(sql, false, 100, &mut est).await.expect("estimated");
    assert_eq!(est.plans.len(), 1, "{est:?}");
    assert!(est.results.is_empty(), "{:?}", est.results);
    eprintln!("{}", est.plans[0].raw);
    let mut act = QueryOutcome::default();
    s.explain(sql, true, 100, &mut act).await.expect("actual");
    assert_eq!(act.plans.len(), 1);
    assert!(act.plans[0].root.actual_rows.is_some(), "{:?}", act.plans[0].root);
    assert_eq!(act.results.iter().filter(|r| !r.columns.is_empty()).count(), 1);
    let plain = run(&mut s, "SELECT 1 AS uno").await.unwrap();
    assert_eq!(plain.results[0].columns[0].name, "uno");

    let snap = s.monitor().await.expect("monitor");
    check_snapshot(&snap, &["connections", "transactions", "cache_hit", "disk_read", "storage_used", "uptime"]);
    drop(s);
    admin.drop_database("dbine_bbf").await.expect("drop database");
}

/// Two sessions: one profiles, the other runs a slow statement and a fast
/// one with a marker; each is seen once and the profiler's own queries
/// are left out. `change_server` false checks the sampled fallback.
async fn profile(id: &str, env: &str, change_server: bool) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let mut cfg = parse_url(&url);
    cfg.driver = id.into();
    let d = driver(id);
    assert!(d.supports_profiler(), "{id}");
    let db = format!("dbine_prof_{id}_{}", if change_server { "xe" } else { "dmv" });
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    if !admin.list_databases().await.unwrap().contains(&db) {
        admin.create_database(&db).await.expect("create database");
    }
    let mut p = d.connect(&cfg, Some(&db)).await.expect("connect");
    let mut w = d.connect(&cfg, Some(&db)).await.expect("connect");
    let opts = dbine_driver::ProfilerOptions { database: db.clone(), change_server };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{id}: {started:?}");
    let complete = started.mode == dbine_driver::ProfilerMode::Complete;
    assert_eq!(complete, change_server && id == "sqlserver", "{id}");
    let marker = format!("dbine_prof_{}", std::process::id());
    let slow = if id == "babelfish" {
        // No WAITFOR in Babelfish: PostgreSQL's pg_sleep (void, so cast).
        format!("SELECT CAST(pg_catalog.pg_sleep(0.6) AS varchar(10)) AS {marker}_slow")
    } else {
        format!("WAITFOR DELAY '00:00:00.600'; SELECT 1 AS {marker}_slow")
    };
    let fast = format!("SELECT 1 AS {marker}_fast");
    let failed = format!("SELECT 1/0 AS {marker}_failed");
    let work = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        run(&mut w, &slow).await.expect("slow");
        tokio::time::sleep(Duration::from_millis(400)).await;
        run(&mut w, &fast).await.expect("fast");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(run(&mut w, &failed).await.is_err());
        tokio::time::sleep(Duration::from_millis(600)).await;
    };
    let watch = async {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(if complete { 10 } else { 5 });
        while Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            if got.iter().filter(|s| s.text.contains(&marker)).count() >= 3 {
                break;
            }
            if complete {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{id}: {mine:#?}");
    let slow_seen: Vec<_> = mine.iter().filter(|s| s.text.contains("_slow")).collect();
    assert_eq!(slow_seen.len(), 1, "{id}: the slow statement once");
    assert!(slow_seen[0].duration_ms.unwrap_or(0.0) >= 300.0, "{id}: duration {:?}", slow_seen[0].duration_ms);
    assert_eq!(slow_seen[0].database.as_deref(), Some(db.as_str()));
    assert_eq!(mine.iter().filter(|s| s.text.contains("_fast")).count(), 1, "{id}: the fast statement once");
    let failed_seen: Vec<_> = mine.iter().filter(|s| s.text.contains("_failed")).collect();
    assert_eq!(failed_seen.len(), 1, "{id}: the failed statement once");
    if complete {
        assert!(failed_seen[0].error.is_some(), "{id}: the error");
        // Extended Events carry CPU, logical reads and writes (in pages).
        assert_eq!(started.reads_unit.as_deref(), Some("páginas"));
        assert!(slow_seen[0].cpu_ms.is_some() && slow_seen[0].reads.is_some() && slow_seen[0].writes.is_some(), "{id}: figures");
    }
    assert!(
        got.iter().all(|s| !s.text.contains("RingBufferTarget") && !s.text.contains("dm_exec_sessions")),
        "{id}: its own statements are left out"
    );
    if complete {
        // The event session is gone.
        let left = run(&mut admin, "SELECT name FROM sys.server_event_sessions WHERE name LIKE 'dbine[_]profiler[_]%'").await.unwrap();
        assert!(left.results[0].rows.is_empty(), "{:?}", left.results[0].rows);
    }
    drop((p, w));
    admin.drop_database(&db).await.expect("drop database");
}

#[tokio::test]
#[ignore]
async fn sqlserver_profiler() {
    profile("sqlserver", "DBINE_TEST_SQLSERVER_URL", true).await;
}

#[tokio::test]
#[ignore]
async fn sqlserver_profiler_read_only() {
    profile("sqlserver", "DBINE_TEST_SQLSERVER_URL", false).await;
}

/// Babelfish in single-db mode holds one user database: run it apart from
/// `babelfish` (`--test-threads=1`).
#[tokio::test]
#[ignore]
async fn babelfish_profiler() {
    profile("babelfish", "DBINE_TEST_BABELFISH_URL", true).await;
}
