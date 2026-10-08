//! Row estimates and object comments of an Oracle schema against a real
//! server, as a user with CREATE USER (`DBINE_TEST_ORACLE_ADMIN_URL`):
//! after DBMS_STATS, analyzed tables and materialized views carry their
//! rows, a never-analyzed table none; views and materialized views carry
//! their comments.
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test stats -- --ignored --nocapture
//! ```

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
async fn estimates_and_comments() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut s: Box<dyn Session> = d.connect(&config(&url), None).await.unwrap();
    let _ = s.drop_database("DBINE_STATS").await;
    s.create_database("dbine_stats").await.unwrap();
    run(&mut s, "ALTER USER dbine_stats DEFAULT TABLESPACE users QUOTA UNLIMITED ON users").await;
    // The materialized view's owner needs these for its container table.
    run(&mut s, "GRANT CREATE TABLE, CREATE MATERIALIZED VIEW TO dbine_stats").await;
    run(&mut s, "CREATE TABLE dbine_stats.clientes (id NUMBER PRIMARY KEY, nombre VARCHAR2(50))").await;
    run(&mut s, "INSERT INTO dbine_stats.clientes SELECT level, 'c' || level FROM dual CONNECT BY level <= 250").await;
    run(&mut s, "COMMIT").await;
    run(&mut s, "CREATE VIEW dbine_stats.v_clientes AS SELECT id, nombre FROM dbine_stats.clientes").await;
    run(&mut s, "COMMENT ON TABLE dbine_stats.v_clientes IS 'Clientes visibles'").await;
    run(&mut s, "CREATE MATERIALIZED VIEW dbine_stats.mv_clientes AS SELECT id FROM dbine_stats.clientes WHERE id <= 10").await;
    run(&mut s, "COMMENT ON MATERIALIZED VIEW dbine_stats.mv_clientes IS 'Los primeros diez'").await;
    run(&mut s, "BEGIN DBMS_STATS.GATHER_SCHEMA_STATS('DBINE_STATS'); END;").await;
    // Created after the statistics: never analyzed, so no estimate.
    run(&mut s, "CREATE TABLE dbine_stats.nueva (id NUMBER)").await;
    drop(s);

    let mut s: Box<dyn Session> = d.connect(&config(&url), Some("DBINE_STATS")).await.unwrap();
    let est = s.row_estimates().await.unwrap();
    eprintln!("{est:?}");
    let rows = |kind: &str, name: &str| est.iter().find(|e| e.object.kind == kind && e.object.name == name).map(|e| e.rows);
    assert_eq!(rows("table", "CLIENTES"), Some(250));
    assert_eq!(rows("materialized_view", "MV_CLIENTES"), Some(10));
    assert!(est.iter().all(|e| e.object.name != "NUEVA"));
    assert!(est.iter().all(|e| e.object.schema.is_none()));

    let comments = s.object_comments().await.unwrap();
    eprintln!("{comments:?}");
    let comment = |kind: &str, name: &str| {
        comments.iter().find(|c| c.object.kind == kind && c.object.name == name).map(|c| c.comment.clone())
    };
    assert_eq!(comment("view", "V_CLIENTES").as_deref(), Some("Clientes visibles"));
    assert_eq!(comment("materialized_view", "MV_CLIENTES").as_deref(), Some("Los primeros diez"));
    drop(s);

    let mut s: Box<dyn Session> = d.connect(&config(&url), None).await.unwrap();
    s.drop_database("DBINE_STATS").await.unwrap();
}
