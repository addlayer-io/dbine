//! Row estimates and object comments through a real ODBC driver, with the
//! generic "odbc" preset in front of SQL Server (the engine is known by the
//! DBMS name): rows from `sys.partitions`, comments from `MS_Description`
//! on a view and a procedure, with the kinds `list_objects` gives them.
//!
//! ```sh
//! DBINE_TEST_ODBC_CONN='DRIVER={ODBC Driver 18 for SQL Server};SERVER=127.0.0.1,25013;UID=sa;PWD={Pw_12345!};TrustServerCertificate=yes' \
//!   cargo test -p dbine-driver-odbc --test stats -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

const DB: &str = "dbine_odbc_stats";

fn cfg() -> Option<ConnectionConfig> {
    let conn = std::env::var("DBINE_TEST_ODBC_CONN").ok()?;
    Some(ConnectionConfig {
        driver: "odbc".into(),
        options: [("connection_string".to_string(), conn), ("batch_mode".to_string(), "go".to_string())].into(),
        ..Default::default()
    })
}

async fn open(cfg: &ConnectionConfig, db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "odbc").unwrap();
    d.connect(cfg, db).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn estimates_and_comments() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_ODBC_CONN not set; skipping");
        return;
    };
    let mut master = open(&cfg, None).await;
    run(&mut master, &format!("IF DB_ID('{DB}') IS NOT NULL DROP DATABASE {DB}")).await;
    run(&mut master, &format!("CREATE DATABASE {DB}")).await;

    let mut s = open(&cfg, Some(DB)).await;
    run(&mut s, "CREATE TABLE dbo.clientes (id INT PRIMARY KEY, nombre NVARCHAR(40))").await;
    run(&mut s, "INSERT INTO dbo.clientes SELECT TOP 250 ROW_NUMBER() OVER (ORDER BY (SELECT 1)), 'c' FROM sys.all_columns").await;
    run(&mut s, "UPDATE STATISTICS dbo.clientes").await;
    run(&mut s, "CREATE VIEW dbo.v_clientes AS SELECT id, nombre FROM dbo.clientes").await;
    run(&mut s, "CREATE PROCEDURE dbo.p_hola AS SELECT 1").await;
    run(&mut s, "EXEC sp_addextendedproperty 'MS_Description', N'Clientes visibles', 'SCHEMA', 'dbo', 'VIEW', 'v_clientes'").await;
    run(&mut s, "EXEC sp_addextendedproperty 'MS_Description', N'Saluda', 'SCHEMA', 'dbo', 'PROCEDURE', 'p_hola'").await;

    let est = s.row_estimates().await.unwrap();
    eprintln!("{est:?}");
    let e = est.iter().find(|e| e.object.name == "clientes").expect("clientes");
    assert_eq!((e.object.kind.as_str(), e.object.schema.as_deref(), e.rows), ("table", Some("dbo"), 250));

    let comments = s.object_comments().await.unwrap();
    eprintln!("{comments:?}");
    let objects = s.list_objects().await.unwrap();
    for (name, text) in [("v_clientes", "Clientes visibles"), ("p_hola", "Saluda")] {
        let c = comments.iter().find(|c| c.object.name == name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(c.comment, text);
        assert!(
            objects.iter().any(|o| o.kind == c.object.kind && o.schema == c.object.schema && o.name == c.object.name),
            "{c:?} not in list_objects"
        );
    }
    drop(s);

    run(&mut master, &format!("ALTER DATABASE {DB} SET SINGLE_USER WITH ROLLBACK IMMEDIATE")).await;
    run(&mut master, &format!("DROP DATABASE {DB}")).await;
}
