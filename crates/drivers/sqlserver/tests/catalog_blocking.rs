//! Issue #123: another session's open transaction with DDL (an ETL creating
//! or altering tables, a deployment script) must not hold up the explorer's
//! structure reads. Before, they waited on its schema locks until the
//! app's 60 s limit cut them. Against a real server
//! (`DBINE_TEST_SQLSERVER_URL`, see tests/integration.rs):
//! `cargo test -p dbine-driver-sqlserver --test catalog_blocking -- --ignored`

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::time::Duration;

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind: kind.into(),
        schema: Some("dbo".into()),
        name: name.into(),
    }
}

const DB: &str = "dbine_test_catalog_blocking";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn structure_reads_dont_wait_for_another_sessions_open_ddl() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == "sqlserver")
        .unwrap();
    let mut admin = d.connect(&cfg, Some("master")).await.unwrap();
    let drop_db = format!(
        "IF DB_ID('{DB}') IS NOT NULL BEGIN ALTER DATABASE [{DB}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB}]; END"
    );
    run(&mut admin, &drop_db).await.unwrap();
    run(&mut admin, &format!("CREATE DATABASE [{DB}]"))
        .await
        .unwrap();
    let mut setup = d.connect(&cfg, Some(DB)).await.unwrap();
    for sql in [
        "CREATE TABLE dbo.orders (id int PRIMARY KEY, total decimal(10,2))",
        "CREATE TABLE dbo.child (id int)",
        "CREATE PROCEDURE dbo.p1 AS SELECT 1",
        "CREATE PROCEDURE dbo.p3 AS SELECT 3",
        "CREATE SEQUENCE dbo.s1",
        "CREATE SEQUENCE dbo.s2",
        "CREATE SYNONYM dbo.o1 FOR dbo.orders",
    ] {
        run(&mut setup, sql).await.unwrap();
    }
    drop(setup);

    // The ETL: a transaction left open that creates a table, alters another,
    // creates and alters procedures and alters a sequence and a synonym,
    // holding their schema locks. (Reading the source of the very object
    // it alters still waits for it: that text is changing.)
    let mut etl = d.connect(&cfg, Some(DB)).await.unwrap();
    run(
        &mut etl,
        "BEGIN TRAN; CREATE TABLE dbo.etl_tmp (id int); ALTER TABLE dbo.child ADD x int; \
         EXEC('CREATE PROCEDURE dbo.p2 AS SELECT 2'); EXEC('ALTER PROCEDURE dbo.p3 AS SELECT 33'); \
         ALTER SEQUENCE dbo.s2 RESTART WITH 5; DROP SYNONYM dbo.o1; CREATE SYNONYM dbo.o1 FOR dbo.child;",
    )
    .await
    .unwrap();

    // The explorer's session, in an isolation level of its own.
    let mut s = d.connect(&cfg, Some(DB)).await.unwrap();
    run(&mut s, "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    let limit = Duration::from_secs(15);
    let objects = tokio::time::timeout(limit, s.list_objects())
        .await
        .expect("list_objects waited for the open transaction")
        .unwrap();
    let has = |kind: &str, name: &str| objects.iter().any(|o| o.kind == kind && o.name == name);
    assert!(has(kinds::TABLE, "orders") && has(kinds::TABLE, "child"));
    assert!(has(kinds::PROCEDURE, "p1"));
    assert!(
        has(kinds::SEQUENCE, "s1") && has(kinds::SEQUENCE, "s2") && has(kinds::SYNONYM, "o1"),
        "the other objects too"
    );
    tokio::time::timeout(limit, s.list_schemas())
        .await
        .expect("list_schemas waited")
        .unwrap();
    for table in ["orders", "child"] {
        let cols = tokio::time::timeout(limit, s.columns(&obj(kinds::TABLE, table)))
            .await
            .unwrap_or_else(|_| panic!("columns of {table} waited"))
            .unwrap();
        assert_eq!(cols[0].name, "id", "{table}");
    }
    let def = tokio::time::timeout(limit, s.definition(&obj(kinds::PROCEDURE, "p1")))
        .await
        .expect("definition waited")
        .unwrap()
        .unwrap_or_default();
    assert!(def.contains("SELECT 1"), "{def}");
    for (kind, name) in [(kinds::SEQUENCE, "s1"), (kinds::SYNONYM, "o1")] {
        let def = tokio::time::timeout(limit, s.definition(&obj(kind, name)))
            .await
            .unwrap_or_else(|_| panic!("definition of {name} waited"))
            .unwrap();
        assert!(def.is_some(), "{name}");
    }
    let schema = tokio::time::timeout(limit, s.database_schema())
        .await
        .expect("database_schema waited")
        .unwrap();
    assert!(schema.iter().any(|t| t.name == "orders"));

    // The session keeps its own isolation level (3: REPEATABLE READ).
    let o = run(
        &mut s,
        "SELECT CAST(transaction_isolation_level AS int) FROM sys.dm_exec_sessions WHERE session_id = @@SPID",
    )
    .await
    .unwrap();
    assert_eq!(o.results[0].rows[0][0].as_i64(), Some(3));

    run(&mut etl, "ROLLBACK").await.unwrap();
    drop((s, etl));
    run(&mut admin, &drop_db).await.unwrap();
}
