//! The function catalog for the editor's completion (issue #97) against
//! real servers: SQL Server's built-ins for its release, Babelfish's own
//! subset, and the database's functions with their schema, arguments,
//! result and `MS_Description`. Each test is skipped without its variable
//! and drops what it creates:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//! DBINE_TEST_BABELFISH_URL='mssql://babelfish_user:12345678@localhost:25714' \
//!   cargo test -p dbine-driver-sqlserver --test functions -- --ignored --test-threads=1
//! ```

use dbine_driver::functions::version_numbers;
use dbine_driver::{ConnectionConfig, Driver, FunctionInfo, FunctionKind, QueryOutcome, Session};
use std::sync::Arc;

const DB: &str = "dbine_fn_catalog";

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

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

fn find<'a>(cat: &'a [FunctionInfo], name: &str) -> Option<&'a FunctionInfo> {
    cat.iter().find(|f| f.name.eq_ignore_ascii_case(name))
}

#[track_caller]
fn has(cat: &[FunctionInfo], name: &str, kind: FunctionKind) -> FunctionInfo {
    let f = find(cat, name).unwrap_or_else(|| panic!("{name} missing"));
    assert_eq!(f.kind, kind, "{name}");
    f.clone()
}

#[track_caller]
fn lacks(cat: &[FunctionInfo], names: &[&str]) {
    for n in names {
        assert!(find(cat, n).is_none(), "{n} listed: {:?}", find(cat, n));
    }
}

/// A database with a documented scalar function and an inline
/// table-valued one: the catalog of a session in it, and the server's
/// `ProductVersion`.
async fn catalog_with_user_functions(id: &str, env: &str) -> Option<(Vec<FunctionInfo>, String)> {
    let Some(c) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return None;
    };
    let d = driver(id);
    let mut m = d.connect(&c, None).await.unwrap();
    if m.list_databases().await.unwrap().iter().any(|d| d == DB) {
        m.drop_database(DB).await.unwrap();
    }
    m.create_database(DB).await.unwrap();
    let mut s = d.connect(&c, Some(DB)).await.unwrap();
    run(
        &mut s,
        "CREATE FUNCTION dbo.con_iva(@neto decimal(18,2), @pais nchar(2)) RETURNS decimal(18,2) AS BEGIN RETURN @neto * 1.21 END",
    )
    .await;
    run(
        &mut s,
        "CREATE FUNCTION dbo.numeros(@hasta int) RETURNS TABLE AS RETURN (SELECT @hasta AS n)",
    )
    .await;
    if id == "sqlserver" {
        run(
            &mut s,
            "EXEC sys.sp_addextendedproperty N'MS_Description', N'El neto con IVA', N'SCHEMA', N'dbo', N'FUNCTION', N'con_iva'",
        )
        .await;
    }
    let mut out = QueryOutcome::default();
    s.execute(
        "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128))",
        1,
        &mut out,
    )
    .await
    .unwrap();
    let version = out.results[0].rows[0][0]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let cat = s.function_catalog().await.unwrap();
    drop(s);
    m.drop_database(DB).await.unwrap();
    eprintln!("{id} {version}: {} entries", cat.len());

    let con_iva = cat
        .iter()
        .find(|f| f.name == "con_iva")
        .expect("the scalar function");
    assert_eq!(con_iva.schema, "dbo");
    assert_eq!(con_iva.kind, FunctionKind::Scalar);
    assert_eq!(con_iva.args, "(@neto decimal(18,2), @pais nchar(2))");
    assert_eq!(con_iva.returns, "decimal(18,2)");
    if id == "sqlserver" {
        assert_eq!(con_iva.description, "El neto con IVA");
    }
    let numeros = cat
        .iter()
        .find(|f| f.name == "numeros")
        .expect("the table-valued function");
    assert_eq!(numeros.kind, FunctionKind::Table);
    assert_eq!(numeros.args, "(@hasta int)");
    assert_eq!(numeros.returns, "TABLE");
    Some((cat, version))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn sqlserver_lists_the_builtins_of_its_release_and_the_databases_functions() {
    let Some((cat, version)) =
        catalog_with_user_functions("sqlserver", "DBINE_TEST_SQLSERVER_URL").await
    else {
        return;
    };
    has(&cat, "ROW_NUMBER", FunctionKind::Window);
    has(&cat, "COUNT_BIG", FunctionKind::Aggregate);
    has(&cat, "OPENJSON", FunctionKind::Table);
    let major = version_numbers(&version)[0];
    // 2017 and 2022.
    if major >= 14 {
        has(&cat, "STRING_AGG", FunctionKind::Aggregate);
    }
    if major >= 16 {
        has(&cat, "GREATEST", FunctionKind::Scalar);
        has(&cat, "GENERATE_SERIES", FunctionKind::Table);
        has(&cat, "CURRENT_TIMEZONE", FunctionKind::Scalar);
        has(&cat, "CURRENT_TIMEZONE_ID", FunctionKind::Scalar);
    }
    // 2025, and other engines'.
    if major < 17 {
        lacks(&cat, &["REGEXP_LIKE", "JSON_OBJECTAGG", "VECTOR_DISTANCE"]);
    }
    lacks(
        &cat,
        &["approx_distinct", "GROUP_CONCAT", "JSON_TABLE", "NVL"],
    );
    // Niladic keywords: the editor would insert them as `name()`.
    lacks(
        &cat,
        &["CURRENT_TIMESTAMP", "CURRENT_DATE", "CURRENT_USER", "USER"],
    );
    assert!(cat
        .iter()
        .all(|f| f.description.is_empty() || f.schema == "dbo"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn babelfish_lists_what_it_implements() {
    let Some((cat, _)) = catalog_with_user_functions("babelfish", "DBINE_TEST_BABELFISH_URL").await
    else {
        return;
    };
    has(&cat, "ROW_NUMBER", FunctionKind::Window);
    has(&cat, "COUNT", FunctionKind::Aggregate);
    has(&cat, "STRING_AGG", FunctionKind::Aggregate);
    has(&cat, "CAST", FunctionKind::Scalar);
    has(&cat, "IIF", FunctionKind::Scalar);
    has(&cat, "DATEADD", FunctionKind::Scalar);
    has(&cat, "ACOS", FunctionKind::Scalar);
    has(&cat, "OPENJSON", FunctionKind::Table);
    // SQL Server's that Babelfish refuses ("not currently supported").
    lacks(&cat, &["CHECKSUM_AGG", "PARSE", "SOUNDEX", "STATS_DATE"]);
    lacks(&cat, &["approx_distinct", "GROUP_CONCAT", "REGEXP_LIKE"]);
    lacks(&cat, &["CURRENT_TIMESTAMP", "SYSTEM_USER"]);
}
