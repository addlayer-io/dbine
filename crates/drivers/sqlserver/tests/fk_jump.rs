//! Jumping by foreign key in the data grid against real servers: the UI
//! pairs a composite foreign key's columns from `database_schema` and opens
//! the other table through `filtered_browse` with one `=` filter per column.
//! This checks both directions ("Ir a la fila referenciada" and "Filas que
//! la referencian") with the key's columns in a different order than the
//! table's. Each test reads `DBINE_TEST_<ENGINE>_URL` and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test fk_jump -- --ignored
//! ```

use dbine_driver::{
    ColumnFilter, ConnectionConfig, Driver, FilterOp, ObjectRef, QueryOutcome, Session,
};
use serde_json::Value;
use std::sync::Arc;

fn cfg(env: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        trust_server_certificate: true,
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out
}

/// A value as text (engines return some numbers as strings).
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// The rows of `obj` where each column equals its value, as the grid's jump
/// filters them (with the values as the engine returned them); the values
/// of `columns` in each row, as text.
async fn filtered(
    d: &Arc<dyn Driver>,
    s: &mut Box<dyn Session>,
    obj: &ObjectRef,
    key: &[(String, Value)],
    columns: &[&str],
) -> Vec<Vec<String>> {
    let filters: Vec<ColumnFilter> = key
        .iter()
        .map(|(c, v)| ColumnFilter {
            column: c.clone(),
            op: FilterOp::Eq,
            values: vec![v.clone()],
            sql: None,
        })
        .collect();
    let q = d
        .filtered_browse(&s.browse_query(obj, 100), &filters)
        .unwrap();
    let out = run(s, &q).await;
    let r = out
        .results
        .iter()
        .find(|r| !r.columns.is_empty())
        .expect("a result set");
    let at: Vec<usize> = columns
        .iter()
        .map(|n| {
            r.columns
                .iter()
                .position(|c| c.name == *n)
                .unwrap_or_else(|| panic!("{n} in {q}"))
        })
        .collect();
    let mut rows: Vec<Vec<String>> = r
        .rows
        .iter()
        .map(|row| at.iter().map(|&i| text(&row[i])).collect())
        .collect();
    rows.sort();
    rows
}

/// Row `id` of `obj`: the values of `columns` as the engine returns them
/// (what the grid has in its cells).
async fn raw_row(
    d: &Arc<dyn Driver>,
    s: &mut Box<dyn Session>,
    obj: &ObjectRef,
    id: i64,
    columns: &[String],
) -> Vec<Value> {
    let f = [ColumnFilter {
        column: "id".into(),
        op: FilterOp::Eq,
        values: vec![serde_json::json!(id)],
        sql: None,
    }];
    let q = d.filtered_browse(&s.browse_query(obj, 100), &f).unwrap();
    let out = run(s, &q).await;
    let r = out
        .results
        .iter()
        .find(|r| !r.columns.is_empty())
        .expect("a result set");
    let row = r.rows.first().unwrap_or_else(|| panic!("row {id} in {q}"));
    columns
        .iter()
        .map(|n| row[r.columns.iter().position(|c| &c.name == n).unwrap()].clone())
        .collect()
}

async fn exercise(id: &str, mut c: ConnectionConfig) {
    let d = driver(id);
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "IF DB_ID('dbine_fk') IS NOT NULL BEGIN ALTER DATABASE dbine_fk SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_fk; END").await;
    run(&mut s, "CREATE DATABASE dbine_fk").await;
    c.database = "dbine_fk".into();
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "CREATE TABLE dbo.[Pedidos] (region nvarchar(10), num int, cliente nvarchar(20), PRIMARY KEY (region, num))").await;
    run(&mut s, "CREATE TABLE dbo.lineas (id int PRIMARY KEY, p_num int, p_region nvarchar(10), FOREIGN KEY (p_region, p_num) REFERENCES dbo.[Pedidos] (region, num))").await;
    run(&mut s, "INSERT INTO dbo.[Pedidos] VALUES ('sur', 1, 'Ana'), ('sur', 2, 'Beto'), ('norte', 1, 'Carla')").await;
    run(&mut s, "INSERT INTO dbo.lineas VALUES (10, 1, 'sur'), (11, 1, 'sur'), (12, 1, 'norte'), (13, NULL, NULL)").await;

    let tables = s.database_schema().await.unwrap();
    let lineas = tables.iter().find(|t| t.name == "lineas").expect("lineas");
    let fk = lineas
        .foreign_keys
        .iter()
        .find(|f| f.ref_table == "Pedidos")
        .expect("the composite foreign key");
    assert_eq!(fk.columns, ["p_region", "p_num"], "{id}");
    assert_eq!(fk.ref_columns, ["region", "num"], "{id}");

    let pedidos = ObjectRef {
        kind: "table".into(),
        schema: Some("dbo".into()),
        name: "Pedidos".into(),
    };
    let lineas_ref = ObjectRef {
        kind: "table".into(),
        schema: Some("dbo".into()),
        name: "lineas".into(),
    };
    // Child row 12 → its parent ('norte', 1).
    let row = raw_row(&d, &mut s, &lineas_ref, 12, &fk.columns).await;
    let key: Vec<(String, Value)> = fk.ref_columns.iter().cloned().zip(row).collect();
    assert_eq!(
        filtered(&d, &mut s, &pedidos, &key, &["cliente"]).await,
        [["Carla"]],
        "{id}"
    );
    // Parent ('sur', 1) → the child rows that reference it.
    let key: Vec<(String, Value)> = fk
        .columns
        .iter()
        .cloned()
        .zip([serde_json::json!("sur"), serde_json::json!(1)])
        .collect();
    assert_eq!(
        filtered(&d, &mut s, &lineas_ref, &key, &["id"]).await,
        [["10"], ["11"]],
        "{id}"
    );
    drop(s);
    c.database = String::new();
    run(
        &mut d.connect(&c, None).await.unwrap(),
        "ALTER DATABASE dbine_fk SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_fk",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn sqlserver_jumps_by_a_composite_foreign_key() {
    let Some(c) = cfg("DBINE_TEST_SQLSERVER_URL", "sqlserver") else {
        return;
    };
    exercise("sqlserver", c).await;
}
