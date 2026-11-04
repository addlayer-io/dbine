//! Legacy TLS against a real server that only speaks TLS 1.0: SQL Server
//! 2017 on Linux with `network.tlsprotocols = 1.0` and the repository's
//! self-signed test certificate (tests/tls/selfsigned.pem, for `localhost`).
//! Skipped without `DBINE_TEST_MSSQL_TLS10_URL`:
//!
//! ```sh
//! docker run -d --name dbine-test-mssql-tls10 --platform linux/amd64 -m 2g \
//!   -p 127.0.0.1:26090:1433 -v "$PWD/tests/tls:/tls:ro" \
//!   -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD=… --entrypoint /bin/bash \
//!   mcr.microsoft.com/mssql/server:2017-latest -c 'mkdir -p /var/opt/mssql
//!     cp /tls/selfsigned.pem /var/opt/mssql/c.pem; cp /tls/selfsigned.key /var/opt/mssql/c.key
//!     printf "[network]\ntlscert = /var/opt/mssql/c.pem\ntlskey = /var/opt/mssql/c.key\ntlsprotocols = 1.0\n" > /var/opt/mssql/mssql.conf
//!     exec /opt/mssql/bin/sqlservr'
//! DBINE_TEST_MSSQL_TLS10_URL='mssql://sa:…@localhost:26090' \
//!   cargo test -p dbine-driver-sqlserver --features legacy-tls --lib legacy_tls_live -- --ignored --test-threads=1
//! ```
//!
//! Without the `legacy-tls` feature only the refusal is checked (the regular
//! host never negotiates TLS 1.0).

use super::*;

fn config(options: &[(&str, &str)]) -> Option<ConnectionConfig> {
    let Ok(url) = std::env::var("DBINE_TEST_MSSQL_TLS10_URL") else {
        eprintln!("DBINE_TEST_MSSQL_TLS10_URL not set; skipping");
        return None;
    };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap();
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        options: options
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    })
}

fn driver() -> SqlServerDriver {
    SqlServerDriver {
        info: variant::info(Variant::SqlServer),
        variant: Variant::SqlServer,
    }
}

const SELF_SIGNED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../tests/tls/selfsigned.pem"
);

/// Without the user's yes, a TLS 1.0-only server is refused with the code
/// the app asks on, naming the server, whatever the encryption mode: the
/// login is always encrypted, and nothing falls back to TLS 1.0.
#[tokio::test]
#[ignore]
async fn refused_without_consent() {
    for options in [
        &[("Encrypt", "Mandatory"), ("TrustServerCertificate", "true")][..],
        &[("Encrypt", "Mandatory")][..],
        &[("Encrypt", "Optional")][..],
        &[
            ("Encrypt", "Mandatory"),
            ("TrustServerCertificate", "true"),
            ("LegacyTls", "false"),
        ][..],
    ] {
        let Some(cfg) = config(options) else { return };
        let e = driver()
            .open(&cfg, None)
            .await
            .err()
            .expect("must not connect");
        let Error::ConnectFailed(f) = &e else {
            panic!("{options:?}: {e:?}")
        };
        assert_eq!(
            f.code.as_deref(),
            Some(LEGACY_TLS_ONLY),
            "{options:?}: {}",
            f.message
        );
        assert!(
            f.message.contains("SQL Server 2017, 14.0."),
            "{}",
            f.message
        );
        eprintln!("{options:?}: {}", f.message);
    }
}

/// With the yes stored on the connection, it connects and runs a query;
/// the certificate is still verified unless the connection trusts it.
#[tokio::test]
#[ignore]
async fn connects_with_consent() {
    let on = [("Encrypt", "Mandatory"), ("LegacyTls", "true")];
    if config(&[]).is_none() {
        return;
    }
    if !LEGACY_TLS_BUILT_IN {
        let cfg = config(&on).unwrap();
        let e = driver()
            .open(&cfg, None)
            .await
            .err()
            .expect("no legacy backend");
        assert!(e.to_string().contains("TLS antiguo"), "{e}");
        return;
    }
    let query = |cfg: ConnectionConfig| async move {
        let mut s = driver().open(&cfg, None).await?;
        let mut out = QueryOutcome::default();
        s.execute(
            "SELECT encrypt_option, @@VERSION FROM sys.dm_exec_connections WHERE session_id = @@SPID",
            10,
            &mut out,
        )
        .await?;
        Ok::<_, Error>(format!("{:?}", out.results[0].rows[0]))
    };
    // Trusted: connects.
    let mut trusted = on.to_vec();
    trusted.push(("TrustServerCertificate", "true"));
    let row = query(config(&trusted).unwrap()).await.unwrap();
    assert!(row.contains("TRUE"), "{row}");
    assert!(row.contains("2017"), "{row}");
    eprintln!("trusted: {row}");
    // Verified against the self-signed certificate given as the CA.
    let mut verified = on.to_vec();
    verified.push(("ServerCertificate", SELF_SIGNED));
    let row = query(config(&verified).unwrap()).await.unwrap();
    assert!(row.contains("TRUE"), "{row}");
    // Verified against the system's roots only: refused (self-signed).
    let e = query(config(&on).unwrap())
        .await
        .expect_err("unverified certificate");
    assert!(e.to_string().to_lowercase().contains("certificate"), "{e}");
    eprintln!("not trusted: {e}");
    // Optional (TLS only around the login) works too.
    let row = query(config(&[("Encrypt", "Optional"), ("LegacyTls", "true")]).unwrap())
        .await
        .unwrap();
    assert!(row.contains("FALSE"), "{row}");
}
