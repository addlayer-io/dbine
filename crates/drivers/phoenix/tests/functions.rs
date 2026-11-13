//! The function catalog (issue #97) against a real Phoenix Query Server,
//! skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_PHOENIX_URL=http://localhost:25165 \
//!   cargo test -p dbine-driver-phoenix --test functions -- --ignored --nocapture
//! ```
//!
//! The same server, an Avatica server, also stands in for the generic
//! "Apache Calcite Avatica" driver.

use dbine_driver::{ConnectionConfig, Error, FunctionKind, QueryOutcome, Session};

fn cfg(driver: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_PHOENIX_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: driver.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        ..Default::default()
    };
    let serialization =
        std::env::var("DBINE_TEST_PHOENIX_SERIALIZATION").unwrap_or("protobuf".into());
    c.options.insert("serialization".into(), serialization);
    Some(c)
}

async fn open(driver: &str) -> Option<Box<dyn Session>> {
    let c = cfg(driver)?;
    let d = dbine_driver_phoenix::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    Some(d.connect(&c, None).await.unwrap())
}

/// Whether the server rejects `name` as an unknown function (ERROR 6001)
/// with every argument count from 0 to 3: Phoenix resolves a function by its
/// name and argument count, so a known one is undefined only for the counts
/// it doesn't take. An ordered-set aggregate (`ordered`) is called with its
/// `WITHIN GROUP` clause.
async fn undefined(s: &mut Box<dyn Session>, name: &str, ordered: bool) -> bool {
    let tail = if ordered {
        " WITHIN GROUP (ORDER BY 1)"
    } else {
        ""
    };
    for args in ["()", "(1)", "(1, 1)", "(1, 1, 1)"] {
        let mut out = QueryOutcome::default();
        let sql = format!("SELECT {name}{args}{tail} FROM SYSTEM.CATALOG LIMIT 1");
        let msg = match s.execute(&sql, 1, &mut out).await {
            Ok(()) => return false,
            Err(Error::Statement(e)) => e.message,
            Err(e) => e.to_string(),
        };
        if !msg.contains("Function undefined") {
            return false;
        }
    }
    true
}

#[tokio::test]
#[ignore]
async fn phoenix_catalog() {
    let Some(mut s) = open("phoenix").await else {
        eprintln!("DBINE_TEST_PHOENIX_URL not set; skipping");
        return;
    };
    println!("{}", s.server_version().await.unwrap());
    let c = s.function_catalog().await.unwrap();
    println!("{} functions", c.len());
    let get = |n: &str| c.iter().find(|f| f.name == n);
    assert_eq!(
        get("PERCENTILE_CONT").unwrap().kind,
        FunctionKind::Aggregate
    );
    assert_eq!(get("REGEXP_SPLIT").unwrap().kind, FunctionKind::Scalar);
    assert!(get("approx_distinct").is_none());
    assert!(get("JSON_VALUE").is_none());
    // Every entry is a function this server knows; an unknown name is
    // reported as one.
    assert!(undefined(&mut s, "APPROX_DISTINCT", false).await);
    for f in &c {
        let ordered = f.args.contains("WITHIN GROUP");
        assert!(
            !undefined(&mut s, &f.name, ordered).await,
            "{} unknown",
            f.name
        );
    }
}

#[tokio::test]
#[ignore]
async fn avatica_catalog() {
    let Some(mut s) = open("avatica").await else {
        eprintln!("DBINE_TEST_PHOENIX_URL not set; skipping");
        return;
    };
    println!("{}", s.server_version().await.unwrap());
    let c = s.function_catalog().await.unwrap();
    println!(
        "{} functions: {:?}",
        c.len(),
        c.iter().map(|f| f.name.as_str()).collect::<Vec<_>>()
    );
    let get = |n: &str| c.iter().find(|f| f.name.eq_ignore_ascii_case(n));
    // The SQL standard's, with their arguments.
    assert_eq!(get("COUNT").expect("COUNT").kind, FunctionKind::Aggregate);
    assert!(get("UPPER").is_some());
    assert!(get("approx_distinct").is_none());
    assert!(get("REGEXP_SPLIT").is_none(), "Phoenix's own");
    assert!(c.iter().all(|f| f.description.is_empty()));
}
