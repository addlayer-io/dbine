//! "Chequeo de salud" of an Oracle schema against a real server, as a user
//! with CREATE USER and the DBA views (`DBINE_TEST_ORACLE_ADMIN_URL`): a
//! schema with problems made on purpose shows each one, with its fix.
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test health -- --ignored --nocapture
//! ```

use dbine_driver::health::Severity;
use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

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

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn finds_what_was_broken() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut s: Box<dyn Session> = d.connect(&config(&url), None).await.unwrap();
    let _ = s.drop_database("DBINE_HEALTH").await;
    s.create_database("dbine_health").await.unwrap();
    run(&mut s, "ALTER USER dbine_health DEFAULT TABLESPACE users QUOTA UNLIMITED ON users").await;
    run(&mut s, "CREATE TABLE dbine_health.clientes (id NUMBER PRIMARY KEY, nombre VARCHAR2(50))").await;
    run(&mut s, "CREATE TABLE dbine_health.pedidos (id NUMBER, cliente_id NUMBER CONSTRAINT fk_ped_cli REFERENCES dbine_health.clientes(id))").await;
    run(&mut s, "INSERT INTO dbine_health.clientes VALUES (1, 'uno')").await;
    run(&mut s, "COMMIT").await;
    run(&mut s, "CREATE UNIQUE INDEX dbine_health.ix_nombre ON dbine_health.clientes(nombre)").await;
    run(&mut s, "ALTER INDEX dbine_health.ix_nombre UNUSABLE").await;
    run(&mut s, "CREATE FORCE VIEW dbine_health.v_rota AS SELECT * FROM dbine_health.no_existe").await;
    run(&mut s, "CREATE SEQUENCE dbine_health.sq_corta MAXVALUE 10 NOCYCLE NOCACHE").await;
    for _ in 0..9 {
        run(&mut s, "SELECT dbine_health.sq_corta.NEXTVAL FROM dual").await;
    }
    run(&mut s, "CREATE TABLE dbine_health.borrada (id NUMBER)").await;
    run(&mut s, "DROP TABLE dbine_health.borrada").await;

    let checks = s.health_checks("DBINE_HEALTH").await.unwrap();
    for c in &checks {
        eprintln!("{:?} [{}] {} {:?}\n  fix: {:?}", c.severity, c.id, c.title, c.objects, c.fix);
    }
    let get = |id: &str| checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("{id}"));
    assert_eq!(get("invalid_objects").severity, Severity::Warning);
    assert!(get("invalid_objects").objects.iter().any(|o| o.starts_with("V_ROTA")));
    assert!(get("invalid_objects").fix.as_deref().unwrap().contains("ALTER VIEW \"DBINE_HEALTH\".\"V_ROTA\" COMPILE;"));
    assert_eq!(get("unusable_indexes").severity, Severity::Critical);
    assert!(get("unusable_indexes").objects.iter().any(|o| o == "CLIENTES · IX_NOMBRE"));
    assert!(get("fk_without_index").objects.iter().any(|o| o.contains("FK_PED_CLI")));
    assert!(get("fk_without_index").fix.as_deref().unwrap().contains("ON \"DBINE_HEALTH\".\"PEDIDOS\" (\"CLIENTE_ID\")"));
    assert!(get("no_primary_key").objects.iter().any(|o| o == "PEDIDOS"));
    assert!(!get("no_primary_key").objects.iter().any(|o| o == "CLIENTES"));
    assert!(get("stale_stats").objects.iter().any(|o| o.starts_with("PEDIDOS")));
    assert_eq!(get("sequences_near_limit").severity, Severity::Critical);
    assert!(get("sequences_near_limit").objects.iter().any(|o| o.starts_with("SQ_CORTA")));
    assert!(get("recyclebin").objects.iter().any(|o| o.starts_with("BORRADA")));
    assert!(get("recyclebin").fix.as_deref().unwrap().starts_with("PURGE TABLE \"DBINE_HEALTH\".\"BIN$"));
    // The tablespace check reads DBA views the admin user has.
    assert!(checks.iter().any(|c| c.id == "tablespace_usage"));

    // Run the fixes that can work here: the findings go away.
    for id in ["unusable_indexes", "stale_stats", "fk_without_index", "recyclebin"] {
        run(&mut s, get(id).fix.clone().unwrap().as_str()).await;
    }
    let again = s.health_checks("DBINE_HEALTH").await.unwrap();
    for id in ["unusable_indexes", "stale_stats", "fk_without_index", "recyclebin"] {
        let c = again.iter().find(|c| c.id == id).unwrap();
        assert_eq!(c.severity, Severity::Ok, "{id}: {} {:?}", c.title, c.objects);
    }

    s.drop_database("DBINE_HEALTH").await.unwrap();
}
