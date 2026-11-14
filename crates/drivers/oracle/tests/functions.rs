//! The function catalog against a real Oracle (`DBINE_TEST_ORACLE_ADMIN_URL`,
//! a user with CREATE USER), skipped without it: the documented built-ins
//! of the server's release, no undocumented internals from
//! V$SQLFN_METADATA, the schema's standalone and package functions (not its
//! procedures), and the same built-ins for a user who can't read the view.
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test functions -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome, Session};

const SCHEMA: &str = "DBINE_FN";

fn config(url: &str) -> ConnectionConfig {
    let rest = url
        .strip_prefix("oracle://")
        .expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn builtins_and_the_schemas_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut admin: Box<dyn Session> = d.connect(&config(&url), None).await.unwrap();
    let _ = admin.drop_database(SCHEMA).await;
    admin.create_database(SCHEMA).await.unwrap();
    run(&mut admin, "ALTER USER dbine_fn IDENTIFIED BY \"Fn_12345\"").await;
    run(&mut admin, "GRANT CREATE SESSION TO dbine_fn").await;
    run(
        &mut admin,
        "CREATE OR REPLACE FUNCTION dbine_fn.add_tax(amount NUMBER, rate NUMBER DEFAULT 0.21) \
         RETURN NUMBER IS BEGIN RETURN amount * (1 + rate); END;",
    )
    .await;
    run(
        &mut admin,
        "CREATE OR REPLACE PACKAGE dbine_fn.billing AS \
         FUNCTION label(code VARCHAR2) RETURN VARCHAR2; \
         PROCEDURE touch(id NUMBER); \
         END;",
    )
    .await;
    run(
        &mut admin,
        "CREATE OR REPLACE PROCEDURE dbine_fn.cleanup IS BEGIN NULL; END;",
    )
    .await;

    let version = admin.server_version().await.unwrap();
    let mut s = d.connect(&config(&url), Some(SCHEMA)).await.unwrap();
    let c = s.function_catalog().await.unwrap();
    let has = |n: &str| c.iter().any(|f| f.name == n && f.schema.is_empty());
    for n in [
        "SUBSTR",
        "TO_CHAR",
        "SYSDATE",
        "LISTAGG",
        "JSON_VALUE",
        "ROW_NUMBER",
    ] {
        assert!(has(n), "{n} missing ({version})");
    }
    if version.contains("23") || version.contains("26") {
        assert!(
            has("VECTOR_DISTANCE") && has("BOOLEAN_AND_AGG"),
            "{version}"
        );
    }
    // Undocumented internals the view lists stay out.
    for n in ["SYS_OP_MAP_NONNULL", "OPTTAD", "NVL3", "APPROX_DISTINCT"] {
        assert!(!c.iter().any(|f| f.name == n), "{n} offered");
    }
    let count = c.iter().find(|f| f.name == "COUNT").unwrap();
    assert_eq!(count.kind, FunctionKind::Aggregate);

    let tax = c
        .iter()
        .find(|f| f.name == "ADD_TAX")
        .expect("standalone function");
    assert_eq!(tax.schema, SCHEMA);
    assert_eq!(tax.args, "(amount NUMBER, [rate NUMBER])");
    assert_eq!(tax.returns, "NUMBER");
    let label = c
        .iter()
        .find(|f| f.name == "LABEL")
        .expect("package function");
    assert_eq!(label.schema, "BILLING");
    assert_eq!(label.returns, "VARCHAR2");
    assert!(!c.iter().any(|f| f.name == "TOUCH" || f.name == "CLEANUP"));

    // A user who can't read V$SQLFN_METADATA gets the same built-ins.
    let mut low_cfg = config(&url);
    low_cfg.username = Some("dbine_fn".into());
    low_cfg.password = Some("Fn_12345".into());
    let mut low = d.connect(&low_cfg, None).await.unwrap();
    let lc = low.function_catalog().await.unwrap();
    for n in ["SUBSTR", "SYSDATE", "JSON_VALUE", "ADD_TAX", "LABEL"] {
        assert!(
            lc.iter().any(|f| f.name == n),
            "{n} missing without the view"
        );
    }
    drop(low);
    drop(s);

    admin.drop_database(SCHEMA).await.unwrap();
}
