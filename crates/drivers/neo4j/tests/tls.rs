//! TLS options against the test servers of `scripts/tls-test-servers.sh`
//! (certificates signed by `tests/tls/ca.pem` for localhost / 127.0.0.1 /
//! host.docker.internal): `dbine-test-tls-neo4j` (Bolt, TLS optional) and
//! `dbine-test-tls-memgraph` (Bolt over TLS only, no auth):
//!
//! ```sh
//! DBINE_TEST_TLS_NEO4J=localhost:26050 DBINE_TEST_TLS_MEMGRAPH=localhost:26055 \
//! DBINE_TEST_TLS_OTHER_IP=::1 \
//!   cargo test -p dbine-driver-neo4j --test tls -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! `DBINE_TEST_TLS_OTHER_IP`: an address of this machine that reaches the
//! servers but isn't in the certificate (the host-name checks must fail
//! through it).

use dbine_driver::{ConnectionConfig, Error, TUNNEL_ORIGIN_HOST};

fn tls_file(name: &str) -> String {
    format!("{}/../../../tests/tls/{name}", env!("CARGO_MANIFEST_DIR"))
}

struct Server {
    driver: &'static str,
    host: String,
    port: u16,
    auth: bool,
}

impl Server {
    fn from_env(driver: &'static str, var: &str, auth: bool) -> Option<Self> {
        let url = std::env::var(var).ok()?;
        let (host, port) = url.rsplit_once(':').unwrap();
        Some(Self {
            driver,
            host: host.into(),
            port: port.parse().unwrap(),
            auth,
        })
    }

    fn cfg(
        &self,
        host: &str,
        encrypt: bool,
        trust: bool,
        opts: &[(&str, &str)],
    ) -> ConnectionConfig {
        ConnectionConfig {
            driver: self.driver.into(),
            host: host.into(),
            port: self.port,
            username: self.auth.then(|| "neo4j".into()),
            password: self.auth.then(|| "Dbine-Tls-2026".into()),
            encrypt,
            trust_server_certificate: trust,
            options: opts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn at(&self, encrypt: bool, trust: bool, opts: &[(&str, &str)]) -> ConnectionConfig {
        self.cfg(&self.host, encrypt, trust, opts)
    }

    async fn connect(&self, c: &ConnectionConfig) -> Result<(), String> {
        let d = dbine_driver_neo4j::drivers()
            .into_iter()
            .find(|d| d.info().id == self.driver)
            .unwrap();
        match d.connect(c, None).await {
            Ok(mut s) => {
                let mut out = Default::default();
                s.execute("RETURN 1 AS one", 10, &mut out)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
            Err(Error::Connect(m)) => Err(m),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn ok(&self, what: &str, c: ConnectionConfig) {
        match self.connect(&c).await {
            Ok(()) => println!("PASS {} {what}: connected", self.driver),
            Err(e) => panic!("{} {what}: should connect: {e}", self.driver),
        }
    }

    async fn fails(&self, what: &str, c: ConnectionConfig, needle: &str) {
        match self.connect(&c).await {
            Ok(()) => panic!("{} {what}: should have failed", self.driver),
            Err(e) => {
                println!("PASS {} {what}: refused: {e}", self.driver);
                assert!(e.contains(needle), "{what}: {e} doesn't mention {needle}");
            }
        }
    }

    /// The verified and unverified modes (the server's certificate is
    /// signed by the test CA, which no system store has).
    async fn tls_modes(&self) {
        let ca = tls_file("ca.pem");
        let wrong_ca = tls_file("selfsigned.pem");
        self.ok(
            "scheme=bolt+s + trustedCertificates=ca.pem",
            self.at(
                false,
                false,
                &[("scheme", "bolt+s"), ("trustedCertificates", &ca)],
            ),
        )
        .await;
        self.fails(
            "scheme=bolt+s without trustedCertificates",
            self.at(false, false, &[("scheme", "bolt+s")]),
            "trustedCertificates",
        )
        .await;
        self.fails(
            "scheme=bolt+s + trustedCertificates=wrong CA",
            self.at(
                false,
                false,
                &[("scheme", "bolt+s"), ("trustedCertificates", &wrong_ca)],
            ),
            "trustedCertificates",
        )
        .await;
        self.ok(
            "scheme=bolt+ssc (not verified)",
            self.at(false, false, &[("scheme", "bolt+ssc")]),
        )
        .await;
        self.ok("legacy tt (bolt+ssc)", self.at(true, true, &[]))
            .await;
        self.fails(
            "legacy tf (bolt+s, system roots)",
            self.at(true, false, &[]),
            "trustedCertificates",
        )
        .await;
        self.ok(
            "a bolt+ssc:// host wins over scheme=bolt",
            self.at(false, false, &[("scheme", "bolt")])
                .tap_host(format!("bolt+ssc://{}", self.host)),
        )
        .await;
        if let Ok(ip) = std::env::var("DBINE_TEST_TLS_OTHER_IP") {
            self.fails(
                "bolt+s + CA, address not in the certificate",
                self.cfg(
                    &ip,
                    false,
                    false,
                    &[("scheme", "bolt+s"), ("trustedCertificates", &ca)],
                ),
                "trustedCertificates",
            )
            .await;
            self.ok(
                "bolt+s + CA, checked against the tunnel's origin name",
                self.cfg(
                    &ip,
                    false,
                    false,
                    &[
                        ("scheme", "bolt+s"),
                        ("trustedCertificates", &ca),
                        (TUNNEL_ORIGIN_HOST, "localhost"),
                    ],
                ),
            )
            .await;
        }
    }
}

trait TapHost {
    fn tap_host(self, host: String) -> Self;
}

impl TapHost for ConnectionConfig {
    fn tap_host(mut self, host: String) -> Self {
        self.host = host;
        self
    }
}

#[tokio::test]
#[ignore]
async fn neo4j_tls_options() {
    let Some(s) = Server::from_env("neo4j", "DBINE_TEST_TLS_NEO4J", true) else {
        return;
    };
    // Plaintext on the same port (TLS optional there).
    s.ok("legacy ff (bolt)", s.at(false, false, &[])).await;
    s.ok("legacy ft (bolt)", s.at(false, true, &[])).await;
    s.ok("scheme=bolt", s.at(true, false, &[("scheme", "bolt")]))
        .await;
    s.tls_modes().await;
}

#[tokio::test]
#[ignore]
async fn memgraph_tls_options() {
    let Some(s) = Server::from_env("memgraph", "DBINE_TEST_TLS_MEMGRAPH", false) else {
        return;
    };
    s.tls_modes().await;
    // TLS only: plaintext fails.
    s.fails(
        "scheme=bolt on a TLS-only server",
        s.at(false, false, &[("scheme", "bolt")]),
        "",
    )
    .await;
}
