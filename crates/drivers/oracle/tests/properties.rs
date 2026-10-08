//! "Propiedades" of a database (a schema) against a real server, as a
//! user with CREATE / ALTER / DROP USER and the DBA views
//! (`DBINE_TEST_ORACLE_ADMIN_URL`), skipped without it:
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test properties -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Session};
use std::collections::BTreeMap;

fn config(url: &str) -> ConnectionConfig {
    let rest = url.strip_prefix("oracle://").expect("oracle://user:pass@host:port/service");
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

fn changes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn schema_properties() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let d = dbine_driver_oracle::drivers().remove(0);
    assert!(d.capabilities().database_properties);
    let mut s: Box<dyn Session> = d.connect(&config(&url), None).await.unwrap();
    let _ = s.drop_database("DBINE_PROPS").await;
    s.create_database("dbine_props").await.unwrap();

    let p = s.database_properties("DBINE_PROPS").await.unwrap();
    eprintln!("{:?}\n{:?}", p.values, p.info);
    let ts = p.values["default_tablespace"].clone();
    let quota = format!("quota:{ts}");
    assert_eq!(p.values.get(&quota).map(String::as_str), Some("UNLIMITED"), "the plain create's quota");
    assert_eq!(p.values["account_locked"], "");
    assert_eq!(p.values["profile"], "DEFAULT");
    assert!(p.info.iter().any(|i| i.label == "Tamaño (segmentos)"));
    assert!(p.warnings.contains_key("account_locked") && p.warnings.contains_key(&quota));
    assert!(p.choices.iter().any(|c| c.key == "profile" && c.values.contains(&"DEFAULT".to_string())));

    let ch = changes(&[
        (quota.as_str(), "10M"),
        ("quota:SYSAUX", "5M"),
        ("default_tablespace", "SYSAUX"),
        ("account_locked", "true"),
    ]);
    eprintln!("{}", d.alter_database_script("DBINE_PROPS", &ch).unwrap());
    s.alter_database("DBINE_PROPS", &ch).await.unwrap();
    let p = s.database_properties("DBINE_PROPS").await.unwrap();
    assert_eq!(p.values["default_tablespace"], "SYSAUX");
    assert_eq!(p.values["account_locked"], "true");
    assert_eq!(p.values["quota:SYSAUX"], "5M");
    assert_eq!(p.values[&quota], "10M");

    // Back: unlock (first), no quota on SYSAUX, the old default tablespace.
    s.alter_database("DBINE_PROPS", &changes(&[("account_locked", ""), ("quota:SYSAUX", ""), ("default_tablespace", &ts)])).await.unwrap();
    let p = s.database_properties("DBINE_PROPS").await.unwrap();
    assert_eq!((p.values["account_locked"].as_str(), p.values["default_tablespace"].as_str()), ("", ts.as_str()));
    assert!(!p.values.contains_key("quota:SYSAUX"), "{:?}", p.values);

    // A failing later statement says what was applied.
    let e = s.alter_database("DBINE_PROPS", &changes(&[("account_locked", ""), ("temporary_tablespace", "NO_SUCH_TS")])).await.unwrap_err();
    assert!(e.to_string().contains("se aplicaron 1 de 2"), "{e}");
    assert!(s.database_properties("DBINE_NO_SUCH_SCHEMA").await.is_err());
    s.drop_database("DBINE_PROPS").await.unwrap();
}
