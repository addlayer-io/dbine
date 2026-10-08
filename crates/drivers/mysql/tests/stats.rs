//! Row estimates and object comments of MySQL and MariaDB against real
//! servers (`DBINE_TEST_<ENGINE>_URL`, `mysql://user:pass@host:port`):
//! rows from `TABLE_ROWS` after `ANALYZE TABLE`, comments from routines
//! and MariaDB sequences, and MySQL's `VIEW` marker left out. Each test is
//! skipped without its variable:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//!   cargo test -p dbine-driver-mysql --test stats -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use std::sync::Arc;

const DB: &str = "dbine_stats";

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

async fn reads_stats_and_comments(id: &str, env: &str) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    let mut m = d.connect(&cfg, None).await.unwrap();
    let _ = m.drop_database(DB).await;
    m.create_database(DB).await.unwrap();
    let mut s = d.connect(&cfg, Some(DB)).await.unwrap();
    for sql in [
        "CREATE TABLE clientes (id int PRIMARY KEY, nombre varchar(20)) ENGINE = InnoDB",
        "INSERT INTO clientes (id, nombre) WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 500) SELECT n, 'c' FROM g",
        "CREATE VIEW v_clientes AS SELECT * FROM clientes",
        "CREATE PROCEDURE limpiar() COMMENT 'Limpia' SELECT 1",
        "CREATE FUNCTION doble(x int) RETURNS int DETERMINISTIC COMMENT 'El doble' RETURN x * 2",
        "ANALYZE TABLE clientes",
    ] {
        run(&mut s, sql).await;
    }
    if id == "mariadb" {
        run(&mut s, "CREATE SEQUENCE seq_pedidos COMMENT = 'Números de pedido'").await;
    }

    let est = s.row_estimates().await.unwrap();
    eprintln!("{est:?}");
    let clientes = est.iter().find(|e| e.object.kind == "table" && e.object.name == "clientes").expect("clientes");
    // InnoDB samples pages: an estimate, close to the 500 rows.
    assert!((400..=600).contains(&clientes.rows), "{}", clientes.rows);
    assert!(est.iter().all(|e| e.object.kind != "view"));

    let c = s.object_comments().await.unwrap();
    eprintln!("{c:?}");
    let comment = |kind: &str, name: &str| c.iter().find(|c| c.object.kind == kind && c.object.name == name).map(|c| c.comment.clone());
    assert_eq!(comment("procedure", "limpiar").as_deref(), Some("Limpia"));
    assert_eq!(comment("function", "doble").as_deref(), Some("El doble"));
    assert_eq!(comment("view", "v_clientes"), None, "MySQL's VIEW marker isn't a comment");
    if id == "mariadb" {
        assert_eq!(comment("sequence", "seq_pedidos").as_deref(), Some("Números de pedido"));
    }
    drop(s);
    m.drop_database(DB).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql_reads_stats_and_comments() {
    reads_stats_and_comments("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb_reads_stats_and_comments() {
    reads_stats_and_comments("mariadb", "DBINE_TEST_MARIADB_URL").await;
}
