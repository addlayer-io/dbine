//! "Deshabilitar / Habilitar índice" against real servers: a table with a
//! primary key and a secondary index; the index is hidden from the
//! optimizer with `index_toggle_script`, `index_usage` reports it disabled,
//! the table still reads, and it's given back. The primary key is refused
//! without touching the server. Each test reads `DBINE_TEST_<ENGINE>_URL`
//! (as `index_usage.rs`) and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25014 \
//! DBINE_TEST_TIDB8_URL=mysql://root@localhost:25044 \
//! DBINE_TEST_OCEANBASE_URL=mysql://root@test:pw@localhost:25035 \
//!   cargo test -p dbine-driver-mysql --test index_toggle -- --ignored --nocapture --test-threads 1
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, IndexUsage, ObjectRef, QueryOutcome, Session};
use std::sync::Arc;

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.trim_end_matches('/').parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    if let Some(e) = out.error.take() {
        panic!("{sql}: {e}");
    }
    out
}

const DB: &str = "dbine_ixtoggle";

fn table() -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some(DB.into()), name: "pedidos".into() }
}

async fn index(s: &mut Box<dyn Session>, name: &str) -> IndexUsage {
    let r = s.index_usage(&table()).await.expect("index_usage").expect("supported");
    r.indexes.into_iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{name} not listed"))
}

async fn toggle(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, ix: &IndexUsage, enable: bool) {
    let script = d.index_toggle_script(&table(), ix, enable).expect("script");
    eprintln!("{}: {:?} {:?}", d.info().id, script.statements, script.warnings);
    assert_eq!(script.warnings.is_empty(), enable);
    for st in &script.statements {
        run(s, st).await;
    }
}

async fn disable_and_enable(id: &str, var: &str) {
    let Ok(url) = std::env::var(var) else {
        eprintln!("{var} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    assert!(d.supports_index_toggle(), "{id}");
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    eprintln!("{id}: {}", admin.server_version().await.unwrap());
    run(&mut admin, &format!("DROP DATABASE IF EXISTS {DB}")).await;
    run(&mut admin, &format!("CREATE DATABASE {DB}")).await;
    let mut s = d.connect(&cfg, Some(DB)).await.expect("connect to the test database");
    run(&mut s, "CREATE TABLE pedidos (id INT PRIMARY KEY, fecha DATE, KEY ix_fecha (fecha))").await;
    run(&mut s, "INSERT INTO pedidos VALUES (1, '2026-01-01'), (2, '2026-02-01')").await;

    let ix = index(&mut s, "ix_fecha").await;
    assert!(!ix.disabled);
    let pk = index(&mut s, "PRIMARY").await;
    assert!(matches!(d.index_toggle_script(&table(), &pk, false), Err(Error::Unsupported(_))));

    toggle(&d, &mut s, &ix, false).await;
    assert!(index(&mut s, "ix_fecha").await.disabled, "{id}: disabled after the script");
    assert!(!index(&mut s, "PRIMARY").await.disabled);
    // The table still reads; only the index is out of the optimizer's hands.
    let out = run(&mut s, "SELECT COUNT(*) FROM pedidos WHERE fecha > '2026-01-15'").await;
    assert_eq!(out.results[0].rows[0][0].to_string().trim_matches('"'), "1");

    toggle(&d, &mut s, &ix, true).await;
    assert!(!index(&mut s, "ix_fecha").await.disabled, "{id}: enabled again");
    run(&mut s, "SELECT COUNT(*) FROM pedidos FORCE INDEX (ix_fecha) WHERE fecha > '2026-01-15'").await;

    drop(s);
    run(&mut admin, &format!("DROP DATABASE {DB}")).await;
}

#[tokio::test]
#[ignore]
async fn mysql() {
    disable_and_enable("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test]
#[ignore]
async fn mariadb() {
    disable_and_enable("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test]
#[ignore]
async fn tidb() {
    disable_and_enable("tidb", "DBINE_TEST_TIDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn tidb8() {
    disable_and_enable("tidb", "DBINE_TEST_TIDB8_URL").await;
}

#[tokio::test]
#[ignore]
async fn oceanbase() {
    disable_and_enable("oceanbase", "DBINE_TEST_OCEANBASE_URL").await;
}
