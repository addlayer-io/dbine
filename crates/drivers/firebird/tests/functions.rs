//! The function catalog against a real Firebird (`DBINE_TEST_FIREBIRD_URL`),
//! skipped without it: the built-ins of the engine version, the database's
//! own functions (standalone and in a package, with their `COMMENT ON`) and
//! the system packages' functions, but no procedures and no package's
//! private function (declared only in its body).
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test functions -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome, Session};

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url.strip_prefix("firebird://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, path) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn builtins_and_the_databases_functions() {
    let Some(base) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    let mut m = d.connect(&base, None).await.unwrap();
    let folder = base
        .database
        .rsplit_once('/')
        .map(|(dir, _)| dir.to_string())
        .unwrap();
    let file = format!("{folder}/dbine_functions.fdb");
    let _ = m.drop_database(&file).await;
    m.create_database(&file).await.unwrap();

    let mut cfg = base.clone();
    cfg.database = file.clone();
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(
        &mut s,
        "CREATE FUNCTION add_tax(amount NUMERIC(18,2), rate NUMERIC(9,2) = 0.21) \
         RETURNS NUMERIC(18,2) AS BEGIN RETURN amount * (1 + rate); END",
    )
    .await;
    run(&mut s, "COMMENT ON FUNCTION add_tax IS 'Price with tax'").await;
    run(
        &mut s,
        "CREATE PACKAGE billing AS BEGIN FUNCTION label(code VARCHAR(10)) RETURNS VARCHAR(40); END",
    )
    .await;
    run(
        &mut s,
        "CREATE PACKAGE BODY billing AS BEGIN \
         FUNCTION prefix RETURNS VARCHAR(2) AS BEGIN RETURN 'L-'; END \
         FUNCTION label(code VARCHAR(10)) RETURNS VARCHAR(40) AS BEGIN RETURN prefix() || code; END \
         END",
    )
    .await;
    run(&mut s, "CREATE PROCEDURE cleanup AS BEGIN END").await;

    let version = s.server_version().await.unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("{} entries ({version})", c.len());
    let has = |n: &str| c.iter().any(|f| f.name == n);
    for n in [
        "LIST",
        "DATEADD",
        "GEN_UUID",
        "ROW_NUMBER",
        "CURRENT_TIMESTAMP",
    ] {
        assert!(has(n), "{n} missing ({version})");
    }
    for n in [
        "NVL",
        "APPROX_DISTINCT",
        "JSON_VALUE",
        "GROUP_CONCAT",
        "CLEANUP",
        "PREFIX",
    ] {
        assert!(!has(n), "{n} offered");
    }
    if version.contains(" 5.") {
        assert!(has("BLOB_APPEND") && has("UNICODE_CHAR"));
        let blob = c
            .iter()
            .find(|f| f.name == "NEW_BLOB")
            .expect("RDB$BLOB_UTIL");
        assert_eq!(blob.schema, "RDB$BLOB_UTIL");
    }
    if version.contains(" 4.") || version.contains(" 5.") {
        assert!(has("CRYPT_HASH") && has("NTILE"));
    }

    let tax = c
        .iter()
        .find(|f| f.name == "ADD_TAX")
        .expect("standalone function");
    assert_eq!(tax.kind, FunctionKind::Scalar);
    assert_eq!(tax.args, "(amount NUMERIC(18,2), [rate NUMERIC(9,2)])");
    assert_eq!(tax.returns, "NUMERIC(18,2)");
    assert_eq!(tax.description, "Price with tax");
    assert_eq!(tax.schema, "");
    let label = c
        .iter()
        .find(|f| f.name == "LABEL")
        .expect("package function");
    assert_eq!(label.schema, "BILLING");
    assert_eq!(label.args, "(code VARCHAR(10))");
    assert_eq!(label.returns, "VARCHAR(40)");

    drop(s);
    m.drop_database(&file).await.unwrap();
}
