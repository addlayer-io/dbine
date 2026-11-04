//! A variant host (`dbine_drivers::VARIANTS`) serves only the connections
//! that turn its option on: SQL Server connections with `LegacyTls` open on
//! the `sqlserver_legacy` host, every other one on the regular `sqlserver`
//! host, and never the other way around.

use dbine_driver::{ConnectionConfig, Driver};
use dbine_plugin::{DriverMeta, Launcher, RemoteDriver};

const EXE: &str = env!("CARGO_BIN_EXE_dbine-plugin-host");

fn launcher(package: &str) -> std::sync::Arc<Launcher> {
    Launcher::at(
        package,
        EXE.into(),
        vec!["--package".into(), package.into()],
        None,
    )
}

#[tokio::test]
async fn legacy_tls_connections_open_on_the_variant_host() {
    let v = dbine_drivers::VARIANTS
        .iter()
        .find(|v| v.package == "sqlserver_legacy")
        .expect("sqlserver_legacy is a variant");
    assert_eq!((v.of, v.option), ("sqlserver", "LegacyTls"));
    let (_, drivers) = dbine_drivers::packages()
        .into_iter()
        .find(|(p, _)| *p == "sqlserver")
        .unwrap();
    let local = drivers
        .into_iter()
        .find(|d| d.info().id == "sqlserver")
        .unwrap();
    let (base, legacy) = (launcher("sqlserver"), launcher("sqlserver_legacy"));
    let d = RemoteDriver::new(DriverMeta::of("sqlserver", local.as_ref()), base.clone())
        .with_variant(v.option, legacy.clone());

    // Nothing listens on port 1: each connect fails fast, after its host
    // started.
    let mut cfg = ConnectionConfig {
        host: "127.0.0.1".into(),
        port: 1,
        username: Some("sa".into()),
        password: Some("x".into()),
        ..Default::default()
    };
    for off in [None, Some("false")] {
        if let Some(o) = off {
            cfg.options.insert("LegacyTls".into(), o.into());
        }
        assert!(d.connect(&cfg, None).await.is_err());
        assert!(base.pid().is_some());
        assert!(legacy.pid().is_none(), "{off:?} started the legacy host");
    }
    cfg.options.insert("LegacyTls".into(), "true".into());
    assert!(d.connect(&cfg, None).await.is_err());
    assert!(legacy.pid().is_some());
    assert_ne!(legacy.pid(), base.pid());
}

/// Through the hosts against a server limited to TLS 1.0
/// (crates/drivers/sqlserver/src/legacy_tls_live.rs says how to start it):
/// without the option the failure, with its code, crosses the pipe; with it
/// the connection opens on the legacy host and runs a query.
/// `DBINE_TEST_MSSQL_TLS10_URL='mssql://sa:…@localhost:26090' cargo test -p dbine-plugin-host --test variant -- --ignored`
#[tokio::test]
#[ignore]
async fn a_tls10_server_through_the_hosts() {
    let Ok(url) = std::env::var("DBINE_TEST_MSSQL_TLS10_URL") else {
        eprintln!("DBINE_TEST_MSSQL_TLS10_URL not set; skipping");
        return;
    };
    let rest = url.split_once("://").unwrap().1;
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap();
    let (_, drivers) = dbine_drivers::packages()
        .into_iter()
        .find(|(p, _)| *p == "sqlserver")
        .unwrap();
    let local = drivers
        .into_iter()
        .find(|d| d.info().id == "sqlserver")
        .unwrap();
    let (base, legacy) = (launcher("sqlserver"), launcher("sqlserver_legacy"));
    let d = RemoteDriver::new(DriverMeta::of("sqlserver", local.as_ref()), base.clone())
        .with_variant("LegacyTls", legacy.clone());
    let mut cfg = ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("Encrypt".into(), "Mandatory".into());
    cfg.options
        .insert("TrustServerCertificate".into(), "true".into());
    match d.connect(&cfg, None).await {
        Err(dbine_driver::Error::ConnectFailed(f)) => {
            assert_eq!(f.code.as_deref(), Some("TLS_LEGACY_ONLY"), "{}", f.message)
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert!(legacy.pid().is_none());
    cfg.options.insert("LegacyTls".into(), "true".into());
    let mut s = d.connect(&cfg, None).await.unwrap();
    assert!(legacy.pid().is_some());
    let mut out = dbine_driver::QueryOutcome::default();
    s.execute("SELECT @@VERSION", 1, &mut out).await.unwrap();
    assert!(format!("{:?}", out.results[0].rows).contains("2017"));
}
