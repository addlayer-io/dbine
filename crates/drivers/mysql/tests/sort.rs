//! Column sort of the data grid against real servers: the browse query,
//! filtered (`filtered_browse`) and then sorted (`sorted_browse`) with two
//! keys, one of them descending and on a column whose name needs quoting.
//! Each test reads `DBINE_TEST_<ENGINE>_URL` and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25014 \
//!   cargo test -p dbine-driver-mysql --test sort -- --ignored
//! ```

use dbine_driver::{ColumnFilter, ConnectionConfig, Driver, FilterOp, ObjectRef, QueryOutcome, Session, SortKey};
use std::sync::Arc;

fn cfg(env: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    out
}

fn ids(out: &QueryOutcome) -> Vec<String> {
    let r = out.results.iter().find(|r| !r.columns.is_empty()).expect("a result set");
    let i = r.columns.iter().position(|c| c.name == "id").expect("the id column");
    r.rows.iter().map(|row| match &row[i] { serde_json::Value::String(s) => s.clone(), v => v.to_string() }).collect()
}

async fn exercise(id: &str, cfg: ConnectionConfig) {
    let d = driver(id);
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_sort").await;
    run(&mut s, "DROP TABLE IF EXISTS dbine_sort.t").await;
    run(&mut s, "CREATE TABLE dbine_sort.t (id int PRIMARY KEY, `Grupo Mixto` varchar(10), `Valor` int)").await;
    run(&mut s, "INSERT INTO dbine_sort.t VALUES (1, 'b', 10), (2, 'a', 5), (3, 'b', 30), (4, 'a', 20), (5, 'c', 1), (6, 'a', 15)").await;
    let obj = ObjectRef { kind: "table".into(), schema: Some("dbine_sort".into()), name: "t".into() };
    let filter = [ColumnFilter { column: "Valor".into(), op: FilterOp::Ge, values: vec![serde_json::json!(5)], sql: None }];
    let sort = [SortKey { column: "Grupo Mixto".into(), descending: false }, SortKey { column: "Valor".into(), descending: true }];
    for (limit, want) in [(100, vec!["4", "6", "2", "3", "1"]), (3, vec!["4", "6", "2"])] {
        let q = d.filtered_browse(&s.browse_query(&obj, limit), &filter).unwrap();
        let q = d.sorted_browse(&q, &sort).unwrap();
        assert_eq!(ids(&run(&mut s, &q).await), want, "{id}: {q}");
    }
    let q = d.sorted_browse(&s.browse_query(&obj, 100), &[SortKey { column: "Grupo Mixto".into(), descending: true }, SortKey { column: "id".into(), descending: false }]).unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), ["5", "1", "3", "2", "4", "6"], "{id}: {q}");
    run(&mut s, "DROP DATABASE dbine_sort").await;
}

#[tokio::test]
#[ignore]
async fn mysql_sorts_the_filtered_browse() {
    let Some(c) = cfg("DBINE_TEST_MYSQL_URL", "mysql") else { return };
    exercise("mysql", c).await;
}

#[tokio::test]
#[ignore]
async fn mariadb_sorts_the_filtered_browse() {
    let Some(c) = cfg("DBINE_TEST_MARIADB_URL", "mariadb") else { return };
    exercise("mariadb", c).await;
}

#[tokio::test]
#[ignore]
async fn tidb_sorts_the_filtered_browse() {
    let Some(c) = cfg("DBINE_TEST_TIDB_URL", "tidb") else { return };
    exercise("tidb", c).await;
}
