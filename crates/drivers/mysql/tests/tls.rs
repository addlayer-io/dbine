//! TLS against a real MySQL 8 with its own CA and a certificate that names
//! another host (as Cloud SQL's name the instance, not the IP dialed).
//! macOS refuses MySQL's auto-generated certificates (ten years valid), so
//! the server gets a short-lived one. Skipped without the variables:
//!
//! ```sh
//! openssl req -x509 -newkey rsa:2048 -nodes -keyout ca-key.pem -out ca.pem -days 300 -subj "/CN=Test CA"
//! openssl req -newkey rsa:2048 -nodes -keyout server-key.pem -out server.csr -subj "/CN=project:instance"
//! printf "subjectAltName=DNS:db.example.test\nextendedKeyUsage=serverAuth\n" > ext.cnf
//! openssl x509 -req -in server.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -out server-cert.pem -days 300 -extfile ext.cnf
//! chmod 644 *.pem
//! docker run -d --name dbine-test-mysql-tls -e MYSQL_ROOT_PASSWORD=pw -p 25099:3306 -v "$PWD":/certs:ro mysql:8 \
//!   --ssl-ca=/certs/ca.pem --ssl-cert=/certs/server-cert.pem --ssl-key=/certs/server-key.pem
//! DBINE_TEST_MYSQL_TLS_URL=mysql://root:pw@localhost:25099 DBINE_TEST_MYSQL_TLS_CA="$PWD/ca.pem" \
//!   cargo test -p dbine-driver-mysql --test tls -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Driver};
use std::sync::Arc;

fn setup() -> Option<(ConnectionConfig, String)> {
    let url = std::env::var("DBINE_TEST_MYSQL_TLS_URL").ok()?;
    let ca = std::env::var("DBINE_TEST_MYSQL_TLS_CA").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hostport
        .split('/')
        .next()
        .unwrap()
        .rsplit_once(':')
        .unwrap();
    let cfg = ConnectionConfig {
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        encrypt: true,
        ..Default::default()
    };
    Some((cfg, ca))
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

/// Cloud SQL with its CA checks only the chain, so only that CA is
/// trusted: the server's own CA connects, any other is refused.
#[tokio::test]
#[ignore]
async fn cloud_sql_trusts_only_the_instance_ca() {
    let Some((mut cfg, ca)) = setup() else { return };
    let d = driver("cloudsql-mysql");
    cfg.options.insert("ca_cert".into(), ca);
    let s = d
        .connect(&cfg, None)
        .await
        .expect("the instance CA connects");
    drop(s);

    // Another CA (Amazon's RDS bundle) didn't sign the server's certificate.
    let dir = std::env::temp_dir().join(format!("dbine-mysql-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let other = dir.join("other.pem");
    let rds = include_bytes!("../../postgres/src/rds-global-bundle.pem");
    std::fs::write(&other, rds).unwrap();
    cfg.options
        .insert("ca_cert".into(), other.to_string_lossy().into_owned());
    assert!(
        d.connect(&cfg, None).await.is_err(),
        "a certificate from another CA was accepted"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Plain TLS with a CA file still checks the name: the certificate names
/// another host, so it is refused; without TLS it connects.
#[tokio::test]
#[ignore]
async fn plain_mysql_checks_the_name_and_plain_connections_still_work() {
    let Some((mut cfg, ca)) = setup() else { return };
    let d = driver("mysql");
    cfg.options.insert("ca_cert".into(), ca);
    assert!(
        d.connect(&cfg, None).await.is_err(),
        "a certificate for another name was accepted"
    );
    cfg.trust_server_certificate = true;
    d.connect(&cfg, None)
        .await
        .expect("trusting the server certificate connects");
    cfg.trust_server_certificate = false;
    cfg.encrypt = false;
    cfg.options.clear();
    d.connect(&cfg, None)
        .await
        .expect("a connection without TLS");
}
