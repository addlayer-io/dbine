//! "Chequeo de salud" of SQL Server against a real server
//! (`DBINE_TEST_SQLSERVER_URL`): a database with problems made on purpose
//! shows each one, with its fix.
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test health -- --ignored
//! ```

use dbine_driver::health::Severity;
use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
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
async fn finds_what_was_broken() {
    let Some(cfg) = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let mut m = d.connect(&cfg, Some("master")).await.unwrap();
    let _ = m.drop_database("dbine_health").await;
    m.create_database("dbine_health").await.unwrap();
    run(&mut m, "ALTER DATABASE dbine_health SET AUTO_SHRINK ON").await;
    let mut s = d.connect(&cfg, Some("dbine_health")).await.unwrap();
    run(&mut s, "CREATE TABLE dbo.clientes (id int PRIMARY KEY)").await;
    run(&mut s, "CREATE TABLE dbo.pedidos (id int NOT NULL, cliente_id int)").await; // a heap
    run(&mut s, "ALTER TABLE dbo.pedidos WITH NOCHECK ADD CONSTRAINT fk_cli FOREIGN KEY (cliente_id) REFERENCES dbo.clientes(id)").await;
    run(&mut s, "CREATE INDEX ix_id ON dbo.pedidos(id); ALTER INDEX ix_id ON dbo.pedidos DISABLE").await;

    let checks = s.health_checks("dbine_health").await.unwrap();
    for c in &checks {
        eprintln!("{:?} [{}] {} {:?}", c.severity, c.id, c.title, c.objects);
    }
    let get = |id: &str| checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("{id}"));
    assert_eq!(get("auto_shrink").severity, Severity::Warning);
    assert!(get("auto_shrink").fix.as_deref().unwrap().contains("AUTO_SHRINK OFF"));
    assert!(get("heaps").objects.iter().any(|o| o == "dbo.pedidos"));
    assert!(get("fk_without_index").objects.iter().any(|o| o.contains("cliente_id")));
    assert!(get("untrusted_constraints").objects.iter().any(|o| o.contains("fk_cli")));
    assert!(get("untrusted_constraints").fix.as_deref().unwrap().contains("WITH CHECK CHECK CONSTRAINT [fk_cli]"));
    assert!(get("disabled_indexes").objects.iter().any(|o| o.contains("ix_id")));
    // Run the untrusted-constraint fix: the finding goes away.
    run(&mut s, get("untrusted_constraints").fix.clone().unwrap().as_str()).await;
    let again = s.health_checks("dbine_health").await.unwrap();
    assert_eq!(again.iter().find(|c| c.id == "untrusted_constraints").unwrap().severity, Severity::Ok);

    drop(s);
    m.drop_database("dbine_health").await.unwrap();
}
