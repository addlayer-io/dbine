//! Row estimates and object comments of SQL Server against a real server
//! (`DBINE_TEST_SQLSERVER_URL`): rows from the partition statistics, and
//! from `sys.partitions` for a user without `VIEW DATABASE STATE`;
//! comments from `MS_Description`.
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test stats -- --ignored
//! ```

use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

const DB: &str = "dbine_stats";

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

fn rows_of(est: &[RowEstimate], kind: &str, name: &str) -> Option<u64> {
    est.iter().find(|e| e.object.kind == kind && e.object.name == name && e.object.schema.as_deref() == Some("dbo")).map(|e| e.rows)
}

fn comment_of<'a>(c: &'a [ObjectComment], kind: &str, name: &str) -> Option<&'a str> {
    c.iter()
        .find(|c| c.object.kind == kind && c.object.name == name && c.object.schema.as_deref() == Some("dbo"))
        .map(|c| c.comment.as_str())
}

fn describe(kind: &str, name: &str, text: &str) -> String {
    format!("EXEC sys.sp_addextendedproperty N'MS_Description', N'{text}', N'SCHEMA', N'dbo', N'{kind}', N'{name}'")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn reads_partition_stats_and_descriptions() {
    let Some(cfg) = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let mut m = d.connect(&cfg, Some("master")).await.unwrap();
    let _ = m.drop_database(DB).await;
    m.create_database(DB).await.unwrap();
    let mut s = d.connect(&cfg, Some(DB)).await.unwrap();
    for sql in [
        "CREATE TABLE dbo.clientes (id int PRIMARY KEY, nombre nvarchar(20))",
        "INSERT INTO dbo.clientes SELECT TOP (1000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)), N'c' FROM sys.all_columns a CROSS JOIN sys.all_columns b",
        "CREATE TABLE dbo.montones (id int)",
        "INSERT INTO dbo.montones SELECT TOP (250) 1 FROM sys.all_columns",
        "CREATE VIEW dbo.v_clientes WITH SCHEMABINDING AS SELECT id, nombre FROM dbo.clientes WHERE id <= 300",
        "CREATE UNIQUE CLUSTERED INDEX ix_v ON dbo.v_clientes (id)",
        "CREATE VIEW dbo.v_simple AS SELECT id FROM dbo.clientes",
        "CREATE PROCEDURE dbo.limpiar AS SELECT 1",
        "CREATE FUNCTION dbo.doble(@x int) RETURNS int AS BEGIN RETURN @x * 2 END",
        "CREATE TRIGGER dbo.tg_clientes ON dbo.clientes AFTER INSERT AS SELECT 1 WHERE 1 = 0",
        "CREATE SEQUENCE dbo.seq_pedidos",
        "CREATE SYNONYM dbo.sin_clientes FOR dbo.clientes",
        "CREATE TYPE dbo.codigo FROM nvarchar(10)",
        "UPDATE STATISTICS dbo.clientes",
        "UPDATE STATISTICS dbo.montones",
    ] {
        run(&mut s, sql).await;
    }
    for sql in [
        describe("VIEW", "v_simple", "Vista simple"),
        describe("PROCEDURE", "limpiar", "Limpia"),
        describe("FUNCTION", "doble", "El doble"),
        "EXEC sys.sp_addextendedproperty N'MS_Description', N'Al insertar', N'SCHEMA', N'dbo', N'TABLE', N'clientes', N'TRIGGER', N'tg_clientes'".into(),
        describe("SEQUENCE", "seq_pedidos", "Números de pedido"),
        describe("SYNONYM", "sin_clientes", "Alias"),
        "EXEC sys.sp_addextendedproperty N'MS_Description', N'Código', N'SCHEMA', N'dbo', N'TYPE', N'codigo'".into(),
        describe("TABLE", "clientes", "Viene con el esquema"),
    ] {
        run(&mut s, &sql).await;
    }

    let est = s.row_estimates().await.unwrap();
    eprintln!("{est:?}");
    assert_eq!(rows_of(&est, "table", "clientes"), Some(1000));
    assert_eq!(rows_of(&est, "table", "montones"), Some(250), "a heap counts too");
    assert_eq!(rows_of(&est, "view", "v_clientes"), Some(300), "an indexed view keeps its rows");
    assert_eq!(rows_of(&est, "view", "v_simple"), None);

    let c = s.object_comments().await.unwrap();
    eprintln!("{c:?}");
    assert_eq!(comment_of(&c, "view", "v_simple"), Some("Vista simple"));
    assert_eq!(comment_of(&c, "procedure", "limpiar"), Some("Limpia"));
    assert_eq!(comment_of(&c, "function", "doble"), Some("El doble"));
    assert_eq!(comment_of(&c, "trigger", "tg_clientes"), Some("Al insertar"));
    assert_eq!(comment_of(&c, "sequence", "seq_pedidos"), Some("Números de pedido"));
    assert_eq!(comment_of(&c, "synonym", "sin_clientes"), Some("Alias"));
    assert_eq!(comment_of(&c, "type", "codigo"), Some("Código"));
    assert!(c.iter().all(|c| c.object.kind != "table"), "tables come with the schema");
    let listed = s.list_objects().await.unwrap();
    for c in &c {
        assert!(
            listed.iter().any(|o| o.kind == c.object.kind && o.schema == c.object.schema && o.name == c.object.name),
            "{:?} not in list_objects",
            c.object
        );
    }

    // Without VIEW DATABASE STATE: sys.partitions.
    run(&mut s, "CREATE USER lector WITHOUT LOGIN").await;
    run(&mut s, "GRANT SELECT ON SCHEMA::dbo TO lector").await;
    run(&mut s, "EXECUTE AS USER = 'lector'").await;
    let est = s.row_estimates().await.unwrap();
    run(&mut s, "REVERT").await;
    eprintln!("lector: {est:?}");
    assert_eq!(rows_of(&est, "table", "clientes"), Some(1000));
    assert_eq!(rows_of(&est, "table", "montones"), Some(250));

    drop(s);
    m.drop_database(DB).await.unwrap();
}
