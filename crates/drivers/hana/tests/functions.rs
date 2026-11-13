//! The function catalog against a real SAP HANA (`DBINE_TEST_HANA_URL`; no
//! practical Docker image), skipped without it: the built-ins, a scalar and
//! a table function of the test schema with their parameters, and no
//! procedures.
//!
//! ```sh
//! DBINE_TEST_HANA_URL=hana://USER:PASSWORD@host:39041 \
//!   cargo test -p dbine-driver-hana --test functions -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome, Session};

const SCHEMA: &str = "DBINE_FN";

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_HANA_URL").ok()?;
    let rest = url.strip_prefix("hana://")?;
    let (rest, tls) = match rest.split_once('?') {
        Some((r, q)) => (r, q.contains("tls")),
        None => (rest, false),
    };
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (host, port) = addr.split_once(':')?;
    Some(ConnectionConfig {
        driver: "hana".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        encrypt: tls,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test]
#[ignore]
async fn builtins_and_the_schemas_functions() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_HANA_URL not set; skipping");
        return;
    };
    let d = dbine_driver_hana::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let _ = admin.drop_database(SCHEMA).await;
    admin.create_database(SCHEMA).await.unwrap();
    let mut s = d.connect(&cfg, Some(SCHEMA)).await.unwrap();
    run(
        &mut s,
        "CREATE FUNCTION add_tax(amount DECIMAL(18,2), rate DECIMAL(9,2) DEFAULT 0.21) \
         RETURNS result DECIMAL(18,2) AS BEGIN result := :amount * (1 + :rate); END",
    )
    .await;
    run(
        &mut s,
        "CREATE FUNCTION first_n(n INTEGER) RETURNS TABLE (id INTEGER) AS \
         BEGIN RETURN SELECT TOP 1 1 AS id FROM DUMMY; END",
    )
    .await;
    run(&mut s, "CREATE PROCEDURE cleanup AS BEGIN END").await;

    let version = s.server_version().await.unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("{} entries ({version})", c.len());
    let has = |n: &str| c.iter().any(|f| f.name == n);
    for n in [
        "ADD_DAYS",
        "STRING_AGG",
        "TO_NVARCHAR",
        "ROW_NUMBER",
        "JSON_VALUE",
    ] {
        assert!(has(n), "{n} missing ({version})");
    }
    for n in ["NVL2", "APPROX_DISTINCT", "GROUP_CONCAT", "CLEANUP"] {
        assert!(!has(n), "{n} offered");
    }
    let tax = c
        .iter()
        .find(|f| f.name == "ADD_TAX")
        .expect("scalar function");
    assert_eq!(tax.schema, SCHEMA);
    assert_eq!(tax.kind, FunctionKind::Scalar);
    assert!(
        tax.args.starts_with("(amount DECIMAL(18,2)"),
        "{}",
        tax.args
    );
    let first = c
        .iter()
        .find(|f| f.name == "FIRST_N")
        .expect("table function");
    assert_eq!(first.kind, FunctionKind::Table);
    assert_eq!(first.args, "(n INTEGER)");

    drop(s);
    admin.drop_database(SCHEMA).await.unwrap();
}
