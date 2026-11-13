//! The function catalog through a real ODBC driver, with the generic "odbc"
//! preset in front of SQL Server (the only ODBC driver the test machines
//! carry): the engine is known by the DBMS name, so the built-ins are SQL
//! Server's shared catalog for the server's version (none of the ODBC
//! escape-only names such as `UCASE`, which T-SQL lacks), and the user's
//! functions come from `sys.objects` with their arguments from
//! SQLProcedureColumns. The test creates two functions and drops them.
//!
//! ```sh
//! DBINE_TEST_ODBC_CONN='DRIVER={ODBC Driver 18 for SQL Server};SERVER=127.0.0.1,25013;UID=sa;PWD={Pw_12345!};TrustServerCertificate=yes' \
//!   cargo test -p dbine-driver-odbc --test functions -- --ignored --nocapture
//! ```
//!
//! On macOS, where Homebrew's `openssl` is 4 and the SQL Server driver
//! loads only 1.x or 3, run the test binary itself (cargo drops `DYLD_`
//! variables) with `DYLD_LIBRARY_PATH=/opt/homebrew/opt/openssl@3/lib`.

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome, Session};

const DROP: &str = "DROP FUNCTION IF EXISTS dbo.dbine_fn_add_tax; \
                    DROP FUNCTION IF EXISTS dbo.dbine_fn_top";

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn the_generic_preset_names_the_engines_functions() {
    let Ok(conn) = std::env::var("DBINE_TEST_ODBC_CONN") else {
        eprintln!("DBINE_TEST_ODBC_CONN not set; skipping");
        return;
    };
    let cfg = ConnectionConfig {
        driver: "odbc".into(),
        options: [("connection_string".to_string(), conn)].into(),
        ..Default::default()
    };
    let d = dbine_driver_odbc::drivers()
        .into_iter()
        .find(|d| d.info().id == "odbc")
        .unwrap();
    let mut s = d.connect(&cfg, None).await.expect("connect");
    run(&mut s, DROP).await;
    run(
        &mut s,
        "CREATE FUNCTION dbo.dbine_fn_add_tax(@amount decimal(18,2), @rate decimal(9,2) = 0.21) \
         RETURNS decimal(18,2) AS BEGIN RETURN @amount * (1 + @rate); END",
    )
    .await;
    run(
        &mut s,
        "CREATE FUNCTION dbo.dbine_fn_top(@n int) RETURNS TABLE AS RETURN SELECT @n AS id",
    )
    .await;

    let c = s.function_catalog().await.unwrap();
    let version = s.server_version().await.unwrap();
    eprintln!("{} entries ({version})", c.len());
    run(&mut s, DROP).await;

    let has = |n: &str| c.iter().any(|f| f.name.eq_ignore_ascii_case(n));
    for n in ["COUNT", "UPPER", "COALESCE", "DATEADD", "ISNULL"] {
        assert!(has(n), "{n} missing");
    }
    // Not T-SQL: other engines' names and the ODBC escape-only ones.
    for n in [
        "APPROX_DISTINCT",
        "NVL",
        "TO_CHAR",
        "GROUP_CONCAT",
        "UCASE",
        "IFNULL",
        "DAYOFWEEK",
        "CURDATE",
    ] {
        assert!(!has(n), "{n} offered");
    }
    let count = c
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case("COUNT"))
        .unwrap();
    assert_eq!(count.kind, FunctionKind::Aggregate);
    // The test servers are SQL Server 2017 or later.
    assert!(has("STRING_AGG"), "SQL Server's catalog for its version");

    let tax = c
        .iter()
        .find(|f| f.name == "dbine_fn_add_tax")
        .expect("the user's scalar function");
    assert_eq!(
        (tax.kind, tax.schema.as_str(), tax.returns.as_str()),
        (FunctionKind::Scalar, "dbo", "decimal")
    );
    assert_eq!(tax.args, "(@amount decimal, @rate decimal)");
    let top = c
        .iter()
        .find(|f| f.name == "dbine_fn_top")
        .expect("the user's table function");
    assert_eq!(
        (top.kind, top.args.as_str(), top.returns.as_str()),
        (FunctionKind::Table, "(@n int)", "TABLE")
    );
}
