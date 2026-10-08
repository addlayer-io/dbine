//! "Chequeo de salud" of MySQL, MariaDB and TiDB against real servers
//! (`DBINE_TEST_<ENGINE>_URL`, `mysql://user:pass@host:port`): a database
//! with problems made on purpose shows each one, and running a finding's
//! fix clears it. Each test is skipped without its variable:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25014 \
//!   cargo test -p dbine-driver-mysql --test health -- --ignored --test-threads=1
//! ```
//!
//! On MariaDB the test turns `userstat` on for the index counters and
//! back off at the end.

use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use std::sync::Arc;

const DB: &str = "dbine_health";

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

async fn checks(s: &mut Box<dyn Session>) -> Vec<HealthCheck> {
    let checks = s.health_checks(DB).await.unwrap();
    for c in &checks {
        eprintln!("{:?} [{}] {} {:?}", c.severity, c.id, c.title, c.objects);
    }
    checks
}

fn get<'a>(checks: &'a [HealthCheck], id: &str) -> &'a HealthCheck {
    checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no {id} check"))
}

async fn finds_what_was_broken(id: &str, env: &str) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let server = id != "tidb";
    let d = driver(id);
    let mut m = d.connect(&cfg, None).await.unwrap();
    let _ = m.drop_database(DB).await;
    m.create_database(DB).await.unwrap();
    if id == "mariadb" {
        run(&mut m, "SET GLOBAL userstat = 1").await;
    }
    let mut s = d.connect(&cfg, Some(DB)).await.unwrap();
    // ix_a repeats the start of ix_ab.
    run(&mut s, "CREATE TABLE clientes (id INT PRIMARY KEY, a INT, b INT, KEY ix_a (a), KEY ix_ab (a, b))").await;
    run(&mut s, "INSERT INTO clientes VALUES (1, 1, 1), (2, 2, 2)").await;
    run(&mut s, "CREATE TABLE sin_pk (v INT)").await;
    run(&mut s, "CREATE TABLE latina (id INT PRIMARY KEY, t VARCHAR(10)) DEFAULT CHARSET = latin1").await;
    if server {
        run(&mut s, "CREATE TABLE vieja (id INT PRIMARY KEY) ENGINE = MyISAM").await;
    } else {
        // TiDB only counts index use on tables with statistics.
        run(&mut s, "ANALYZE TABLE clientes").await;
    }
    // Open the table so the counters have rows for its indexes.
    run(&mut s, "SELECT id FROM clientes WHERE id = 1").await;

    let all = checks(&mut s).await;
    assert_eq!(get(&all, "no_primary_key").objects, vec!["sin_pk".to_string()]);
    let redundant = get(&all, "redundant_indexes");
    assert_eq!(redundant.objects, vec!["clientes · ix_a (lo cubre ix_ab)".to_string()]);
    assert_eq!(redundant.fix.as_deref(), Some("ALTER TABLE `dbine_health`.`clientes` DROP INDEX `ix_a`;"));
    let unused = get(&all, "unused_indexes");
    assert!(unused.objects.contains(&"clientes · ix_ab".to_string()), "{:?}", unused.objects);
    assert!(unused.title.starts_with("No concluyente") && unused.fix.is_none());
    assert!(get(&all, "mixed_collations").objects.iter().any(|o| o.starts_with("latina (latin1")));
    if server {
        assert!(get(&all, "mixed_collations").fix.as_deref().unwrap().contains("ALTER TABLE `dbine_health`.`latina` CONVERT TO CHARACTER SET utf8mb4 COLLATE "));
        assert_eq!(get(&all, "myisam_tables").objects, vec!["vieja".to_string()]);
        assert_eq!(get(&all, "fragmented_tables").severity, Severity::Ok);
    } else {
        assert!(all.iter().all(|c| !matches!(c.id.as_str(), "myisam_tables" | "fragmented_tables")));
    }

    // The fixes clear their findings.
    run(&mut s, redundant.fix.clone().unwrap().as_str()).await;
    if server {
        run(&mut s, get(&all, "myisam_tables").fix.clone().unwrap().as_str()).await;
        run(&mut s, get(&all, "mixed_collations").fix.clone().unwrap().as_str()).await;
    }
    let again = checks(&mut s).await;
    assert_eq!(get(&again, "redundant_indexes").severity, Severity::Ok);
    if server {
        assert_eq!(get(&again, "myisam_tables").severity, Severity::Ok);
        assert_eq!(get(&again, "mixed_collations").severity, Severity::Ok);
    }

    drop(s);
    if id == "mariadb" {
        run(&mut m, "SET GLOBAL userstat = 0").await;
    }
    m.drop_database(DB).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql() {
    finds_what_was_broken("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb() {
    finds_what_was_broken("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tidb() {
    finds_what_was_broken("tidb", "DBINE_TEST_TIDB_URL").await;
}
