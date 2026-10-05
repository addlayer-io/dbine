//! "Clonar tabla" on SQL Server with an IDENTITY column and 100 000 rows,
//! the size where the copy goes by bulk load in batches: IDENTITY as the
//! clustered primary key, as a unique index without a primary key, with a
//! nonclustered primary key and a clustered index elsewhere, with gaps
//! (deleted rows) and with a negative increment; each with and without
//! "Incluir índices". The clone must have the same rows and identity
//! values, the same seed and increment, and its next insert must work.
//!
//! Ignored by default. Reads `DBINE_TEST_SQLSERVER_URL` like
//! `clone_table_sqlserver.rs` (default: the `dbine-test-sqlserver` container):
//!
//! ```sh
//! cargo test -p dbine-transfer --test clone_identity_sqlserver -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use dbine_transfer::clone_table::{clone_table, CloneControl, CloneOptions, CloneRequest, ConfigEndpoints};
use std::sync::Arc;
use std::time::Instant;

const DEFAULT_URL: &str = "mssql://sa:Pw_12345!@localhost:25013";
const DB: &str = "dbine_clone_identity";
const ROWS: u32 = 100_000;

fn config() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").unwrap_or_else(|_| DEFAULT_URL.into());
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    }
}

async fn rows(s: &mut dyn Session, sql: &str) -> Vec<Vec<String>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
    if let Some(e) = out.error {
        panic!("{e}\n{sql}");
    }
    out.results
        .iter()
        .flat_map(|r| r.rows.iter())
        .map(|r| r.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect())
        .collect()
}

/// Every way the friend's table could have been made.
const VARIANTS: &[(&str, &str, &str)] = &[
    ("pk_clustered", "id int IDENTITY(1,1) CONSTRAINT PK_pk_clustered PRIMARY KEY, codigo nvarchar(40) NOT NULL, monto decimal(12,2), fecha datetime2", "CREATE INDEX IX_pk_clustered_codigo ON dbo.pk_clustered (codigo); CREATE INDEX IX_pk_clustered_fecha ON dbo.pk_clustered (fecha) INCLUDE (monto);"),
    ("unique_no_pk", "id bigint IDENTITY(1,1) NOT NULL, codigo nvarchar(40) NOT NULL, monto decimal(12,2), fecha datetime2", "CREATE UNIQUE INDEX UX_unique_no_pk_id ON dbo.unique_no_pk (id);"),
    ("pk_nonclustered", "id int IDENTITY(10,10) CONSTRAINT PK_pk_nonclustered PRIMARY KEY NONCLUSTERED, codigo nvarchar(40) NOT NULL, monto decimal(12,2), fecha datetime2", "CREATE CLUSTERED INDEX CX_pk_nonclustered_fecha ON dbo.pk_nonclustered (fecha);"),
    ("with_gaps", "id int IDENTITY(1,1) CONSTRAINT PK_with_gaps PRIMARY KEY, codigo nvarchar(40) NOT NULL, monto decimal(12,2), fecha datetime2", "DELETE FROM dbo.with_gaps WHERE id % 7 = 0;"),
    ("negative", "id int IDENTITY(-1,-1) CONSTRAINT PK_negative PRIMARY KEY, codigo nvarchar(40) NOT NULL, monto decimal(12,2), fecha datetime2", ""),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live SQL Server: see the top of the file"]
async fn sql_server_identity_100k_rows() {
    let driver: Arc<dyn Driver> = dbine_drivers::find("sqlserver").expect("sqlserver driver").clone();
    let cfg = config();
    let mut master = driver.connect(&cfg, Some("master")).await.unwrap();
    rows(&mut *master, &format!("IF DB_ID(N'{DB}') IS NOT NULL BEGIN ALTER DATABASE [{DB}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB}]; END; CREATE DATABASE [{DB}];")).await;
    let endpoints = Arc::new(ConfigEndpoints { driver: driver.clone(), config: cfg.clone(), database: Some(DB.into()) });
    let mut w = driver.connect(&cfg, Some(DB)).await.unwrap();

    let mut failures = Vec::new();
    for (table, cols, extra) in VARIANTS {
        rows(&mut *w, &format!("CREATE TABLE dbo.{table} ({cols})")).await;
        rows(
            &mut *w,
            &format!(
                "INSERT INTO dbo.{table} (codigo, monto, fecha)
                 SELECT TOP ({ROWS}) CONCAT(N'c', n), CAST(n % 10000 AS decimal(12,2)) / 7, DATEADD(minute, -n, '2026-01-01')
                   FROM (SELECT ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n FROM sys.all_objects a CROSS JOIN sys.all_objects b) x"
            ),
        )
        .await;
        if !extra.is_empty() {
            rows(&mut *w, extra).await;
        }
        for with_indexes in [true, false] {
            let name = format!("{table}_clone_{}", if with_indexes { "ix" } else { "noix" });
            let req = CloneRequest {
                source: ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: (*table).into() },
                new_name: name.clone(),
                options: CloneOptions { with_data: true, with_indexes },
            };
            let t0 = Instant::now();
            let r = clone_table(endpoints.clone() as Arc<dyn dbine_transfer::Endpoints>, req, &CloneControl::default(), |_| {}).await;
            let secs = t0.elapsed().as_secs_f64();
            match r {
                Err(e) => {
                    println!("FAIL {name} ({secs:.1}s): {e}");
                    failures.push(format!("{name}: {e}"));
                }
                Ok(rep) => {
                    let sum = |t: &str| format!("SELECT CAST(COUNT(*) AS nvarchar(20)), CAST(CHECKSUM_AGG(BINARY_CHECKSUM(id, codigo, monto, fecha)) AS nvarchar(20)), CAST(IDENT_SEED(N'dbo.{t}') AS nvarchar(20)), CAST(IDENT_INCR(N'dbo.{t}') AS nvarchar(20)), CAST(IDENT_CURRENT(N'dbo.{t}') AS nvarchar(20)) FROM dbo.{t}");
                    let a = rows(&mut *w, &sum(table)).await;
                    let b = rows(&mut *w, &sum(&name)).await;
                    // The next insert gets a new value, no duplicate.
                    let next = rows(&mut *w, &format!("INSERT INTO dbo.{name} (codigo) VALUES (N'nuevo'); SELECT CAST(SCOPE_IDENTITY() AS nvarchar(20)), CAST(COUNT(*) AS nvarchar(20)) FROM dbo.{name} WHERE id = SCOPE_IDENTITY()")).await;
                    let ok = a == b && next.first().is_some_and(|r| r.get(1).map(String::as_str) == Some("1"));
                    println!("{} {name} ({secs:.1}s, {} rows): source {:?} clone {:?} next {:?}", if ok { "ok  " } else { "DIFF" }, rep.rows, a, b, next);
                    if !ok {
                        failures.push(format!("{name}: source {a:?} clone {b:?} next {next:?}"));
                    }
                }
            }
        }
    }
    drop(w);
    rows(&mut *master, &format!("ALTER DATABASE [{DB}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{DB}];")).await;
    assert!(failures.is_empty(), "{failures:#?}");
}

/// "Comparar datos" → sync into a table with IDENTITY: the rows only on one
/// side are inserted with their identity values (`Driver::insert_script`),
/// which SQL Server refuses unless the sync wraps them in IDENTITY_INSERT
/// (`Driver::data_load_wrap`, as src-tauri/src/commands/data_compare.rs does).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live SQL Server: see the top of the file"]
async fn sql_server_data_sync_into_identity_table() {
    let driver: Arc<dyn Driver> = dbine_drivers::find("sqlserver").expect("sqlserver driver").clone();
    let cfg = config();
    let mut master = driver.connect(&cfg, Some("master")).await.unwrap();
    let db = "dbine_sync_identity";
    rows(&mut *master, &format!("IF DB_ID(N'{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END; CREATE DATABASE [{db}];")).await;
    let mut w = driver.connect(&cfg, Some(db)).await.unwrap();
    rows(&mut *w, "CREATE TABLE dbo.destino (id int IDENTITY(1,1) PRIMARY KEY, codigo nvarchar(40) NOT NULL)").await;
    let target = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "destino".into() };
    let script = driver
        .insert_script(&target, &["id".into(), "codigo".into()], &[vec![serde_json::json!(5), serde_json::json!("a")], vec![serde_json::json!(9), serde_json::json!("b")]])
        .unwrap();
    // Bare, SQL Server refuses it (error 544): what the sync sent before.
    let mut out = QueryOutcome::default();
    let bare = w.execute(&script, 10, &mut out).await;
    assert!(bare.is_err() || out.error.is_some(), "a bare insert of identity values must fail");
    // Wrapped as the sync now sends it, every row lands with its value.
    let table = dbine_driver::TableSchema {
        schema: Some("dbo".into()),
        name: "destino".into(),
        columns: vec![dbine_driver::ColumnDef { name: "id".into(), auto_increment: true, ..Default::default() }],
        ..Default::default()
    };
    let (before, after) = driver.data_load_wrap(&table);
    rows(&mut *w, &format!("{before}\n{script}\n{after}")).await;
    assert_eq!(rows(&mut *w, "SELECT CAST(id AS nvarchar(10)) FROM dbo.destino ORDER BY id").await, vec![vec!["5".to_string()], vec!["9".to_string()]]);
    drop(w);
    rows(&mut *master, &format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}];")).await;
}
