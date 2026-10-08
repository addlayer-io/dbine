//! "Chequeo de salud" of a Firebird database against a real server
//! (`DBINE_TEST_FIREBIRD_URL`): a database file made next to the connected
//! one, with problems made on purpose, shows each one with its fix.
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test health -- --ignored --nocapture
//! ```

use dbine_driver::health::Severity;
use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

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
async fn finds_what_was_broken() {
    let Some(base) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    let mut m = d.connect(&base, None).await.unwrap();
    let folder = base.database.rsplit_once('/').map(|(dir, _)| dir.to_string()).unwrap();
    let file = format!("{folder}/dbine_health.fdb");
    let _ = m.drop_database(&file).await;
    m.create_database(&file).await.unwrap();

    let mut cfg = base.clone();
    cfg.database = file.clone();
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "CREATE TABLE clientes (id INTEGER NOT NULL PRIMARY KEY, nombre VARCHAR(40))").await;
    run(&mut s, "CREATE TABLE eventos (id INTEGER, cliente_id INTEGER)").await;
    // Created on the empty table: selectivity 0, never updated.
    run(&mut s, "CREATE INDEX ix_eventos_cliente ON eventos (cliente_id)").await;
    run(&mut s, "CREATE INDEX ix_clientes_nombre ON clientes (nombre)").await; // stays empty: not a finding
    run(&mut s, "CREATE INDEX ix_eventos_id ON eventos (id)").await;
    run(&mut s, "ALTER INDEX ix_eventos_id INACTIVE").await;
    run(&mut s, "INSERT INTO eventos VALUES (1, 10)").await;
    run(&mut s, "INSERT INTO eventos VALUES (2, 20)").await;
    run(&mut s, "COMMIT").await;

    let checks = s.health_checks(&file).await.unwrap();
    for c in &checks {
        eprintln!("{:?} [{}] {} {:?}\n  {}\n  fix: {:?}", c.severity, c.id, c.title, c.objects, c.detail, c.fix);
    }
    let get = |id: &str| checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("{id}"));
    assert_eq!(get("transaction_gap").severity, Severity::Ok);
    assert!(checks.iter().any(|c| c.id == "forced_writes"));
    assert_eq!(get("index_statistics").severity, Severity::Warning);
    assert_eq!(get("index_statistics").objects, ["EVENTOS · IX_EVENTOS_CLIENTE"]);
    assert_eq!(get("index_statistics").fix.as_deref(), Some("SET STATISTICS INDEX \"IX_EVENTOS_CLIENTE\";"));
    assert!(get("inactive_indexes").objects.iter().any(|o| o == "EVENTOS · IX_EVENTOS_ID"));
    assert_eq!(get("no_primary_key").objects, ["EVENTOS"]);

    // Run the fixes: the findings go away.
    for id in ["index_statistics", "inactive_indexes"] {
        run(&mut s, get(id).fix.clone().unwrap().as_str()).await;
    }
    run(&mut s, "COMMIT").await;
    let again = s.health_checks(&file).await.unwrap();
    assert_eq!(again.iter().find(|c| c.id == "index_statistics").unwrap().severity, Severity::Ok);
    assert!(!again.iter().any(|c| c.id == "inactive_indexes"));

    drop(s);
    m.drop_database(&file).await.unwrap();
}
