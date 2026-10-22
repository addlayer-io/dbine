//! TLS against a real PostgreSQL with its own CA and a certificate that
//! names another host (as Cloud SQL's and AlloyDB's name the instance, not
//! the IP dialed). Skipped without the variables:
//!
//! ```sh
//! openssl req -x509 -newkey rsa:2048 -nodes -keyout ca-key.pem -out ca.pem -days 300 -subj "/CN=Test CA"
//! openssl req -newkey rsa:2048 -nodes -keyout server-key.pem -out server.csr -subj "/CN=project:instance"
//! printf "subjectAltName=DNS:db.example.test\nextendedKeyUsage=serverAuth\n" > ext.cnf
//! openssl x509 -req -in server.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -out server-cert.pem -days 300 -extfile ext.cnf
//! chmod 644 *.pem
//! docker run -d --name dbine-test-postgres-tls -e POSTGRES_PASSWORD=pw -p 25098:5432 -v "$PWD":/certs:ro \
//!   --entrypoint sh postgres:16 -c 'mkdir -p /tls && cp /certs/*.pem /tls/ && chown postgres /tls/* \
//!   && chmod 600 /tls/server-key.pem && exec docker-entrypoint.sh postgres -c ssl=on \
//!   -c ssl_cert_file=/tls/server-cert.pem -c ssl_key_file=/tls/server-key.pem'
//! DBINE_TEST_POSTGRES_TLS_URL=postgres://postgres:pw@localhost:25098/postgres DBINE_TEST_POSTGRES_TLS_CA="$PWD/ca.pem" \
//!   cargo test -p dbine-driver-postgres --test tls -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Driver};
use std::sync::Arc;

fn setup() -> Option<(ConnectionConfig, String)> {
    let url = std::env::var("DBINE_TEST_POSTGRES_TLS_URL").ok()?;
    let ca = std::env::var("DBINE_TEST_POSTGRES_TLS_CA").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig {
        host: host.into(),
        port: port.parse().unwrap(),
        database: db.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        encrypt: true,
        ..Default::default()
    };
    Some((cfg, ca))
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

/// Cloud SQL and AlloyDB with their CA check only the chain, so only that
/// CA is trusted: the server's own CA connects, any other is refused.
#[tokio::test]
#[ignore]
async fn cloud_sql_and_alloydb_trust_only_the_instance_ca() {
    let Some((mut cfg, ca)) = setup() else { return };
    let dir = std::env::temp_dir().join(format!("dbine-pg-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let other = dir.join("other.pem");
    // Another CA (Amazon's RDS bundle) didn't sign the server's certificate.
    std::fs::write(&other, include_bytes!("../src/rds-global-bundle.pem")).unwrap();
    for id in ["cloudsql_postgres", "alloydb"] {
        let d = driver(id);
        cfg.options.insert("ssl_root_cert".into(), ca.clone());
        d.connect(&cfg, None)
            .await
            .unwrap_or_else(|e| panic!("{id}: the instance CA connects: {e:?}"));
        cfg.options
            .insert("ssl_root_cert".into(), other.to_string_lossy().into_owned());
        assert!(
            d.connect(&cfg, None).await.is_err(),
            "{id}: a certificate from another CA was accepted"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Plain TLS with a CA file still checks the name: the certificate names
/// another host, so it is refused unless the user trusts any certificate.
#[tokio::test]
#[ignore]
async fn plain_postgres_checks_the_name() {
    let Some((mut cfg, ca)) = setup() else { return };
    let d = driver("postgres");
    cfg.options.insert("ssl_root_cert".into(), ca);
    assert!(
        d.connect(&cfg, None).await.is_err(),
        "a certificate for another name was accepted"
    );
    cfg.trust_server_certificate = true;
    d.connect(&cfg, None)
        .await
        .expect("trusting the server certificate connects");
}
