//! The explorer's pages (`list_objects_page`) against real servers: for
//! every filter (each kind, a schema, no schema, "Otros", searches with
//! wildcards, lookups) and page size, each page equals the app's own paging
//! of `list_objects` (`objects::page_of`), cursor included. The objects
//! live in a scratch database (`dbine_pages`), dropped at the end even when
//! a check fails. Each test reads `DBINE_TEST_<ENGINE>_URL` and is skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MYSQL9_URL=mysql://root:pw@localhost:27201 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB8_URL=mysql://root@localhost:25044 \
//! DBINE_TEST_OCEANBASE_URL=mysql://root@test:pw@localhost:25035 \
//! DBINE_TEST_STARROCKS_URL=mysql://root@localhost:25030 \
//! DBINE_TEST_DORIS_URL=mysql://root@localhost:25031 \
//! DBINE_TEST_GREPTIMEDB_URL=mysql://localhost:25017 \
//!   cargo test -p dbine-driver-mysql --test objects_page -- --ignored --nocapture --test-threads 1
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::sync::Arc;

const DB: &str = "dbine_pages";

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| {
        (h, p.trim_end_matches('/').parse().unwrap())
    });
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
    dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    match out.error {
        Some(e) => Err(format!("{sql}: {e}")),
        None => Ok(()),
    }
}

/// MySQL's tables.
fn mysql_table(name: &str) -> String {
    format!("CREATE TABLE `{name}` (id INT PRIMARY KEY)")
}

/// StarRocks' and Doris' tables.
fn olap_table(name: &str) -> String {
    format!(
        "CREATE TABLE `{name}` (id INT) DUPLICATE KEY(id) DISTRIBUTED BY HASH(id) BUCKETS 1 \
         PROPERTIES ('replication_num' = '1')"
    )
}

fn greptime_table(name: &str) -> String {
    format!("CREATE TABLE `{name}` (ts TIMESTAMP TIME INDEX, v INT)")
}

/// What every engine here has: 25 tables (a folder of three pages of 10),
/// `extra` tables (names that differ only in case, LIKE's wildcards) and
/// views.
fn common(table: fn(&str) -> String, extra: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = (0..25).map(|i| table(&format!("t_{i:02}"))).collect();
    v.extend(extra.iter().map(|t| table(t)));
    v.extend((0..9).map(|i| format!("CREATE VIEW v_{i} AS SELECT 1 AS x")));
    v
}

/// The extra tables of the MySQL servers.
const NAMES: &[&str] = &["Orders", "a%b", "axb", "dup", "T_Mixed"];

fn routines_and_triggers() -> Vec<String> {
    [
        "CREATE PROCEDURE p_a() SELECT 1",
        "CREATE PROCEDURE P_b() SELECT 2",
        "CREATE PROCEDURE dup() SELECT 3",
        "CREATE FUNCTION f_a() RETURNS INT DETERMINISTIC RETURN 1",
        "CREATE FUNCTION f_b() RETURNS INT DETERMINISTIC RETURN 2",
        "CREATE TRIGGER tr_a BEFORE INSERT ON t_00 FOR EACH ROW SET NEW.id = NEW.id",
        // A trigger named as a table: list_objects' order (tables first).
        "CREATE TRIGGER dup BEFORE INSERT ON t_01 FOR EACH ROW SET NEW.id = NEW.id",
    ]
    .map(String::from)
    .to_vec()
}

fn sequences() -> Vec<String> {
    ["CREATE SEQUENCE s_a", "CREATE SEQUENCE S_b"]
        .map(String::from)
        .to_vec()
}

/// The filters the explorer sends: folders, "Otros", searches, lookups.
fn queries(kinds: &[String]) -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
        ObjectQuery {
            no_schema: true,
            ..Default::default()
        },
        ObjectQuery {
            schema: Some(DB.into()),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: kinds.to_vec(),
            no_schema: true,
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec!["table".into()],
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("table".into()),
            pattern: Some("1".into()),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec!["table".into()],
            pattern: Some("_".into()),
            ..Default::default()
        },
    ];
    for k in kinds {
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            ..Default::default()
        });
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            no_schema: true,
            ..Default::default()
        });
    }
    for p in ["T_1", "%", "_", "a%b", "!", "ORDERS", "dup", "zzz"] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["T_05", "orders", "dup", "nope", "a%b"] {
        v.push(ObjectQuery {
            name: Some(n.into()),
            ..Default::default()
        });
    }
    v
}

/// Pages `q` through the driver and through `page_of` over `all`: the same
/// page each time, to the last one. Returns how many pages.
async fn same_pages(s: &mut Box<dyn Session>, all: &[DbObject], q: &ObjectQuery) -> usize {
    let mut q = q.clone();
    let mut pages = 0;
    loop {
        let got = s
            .list_objects_page(&q)
            .await
            .unwrap_or_else(|e| panic!("{q:?}: {e}"));
        let want = page_of(all, &q);
        assert_eq!(got, want, "{q:?}");
        pages += 1;
        assert!(pages < 100, "{q:?}: no last page");
        match got.next {
            Some(n) => q.after = Some(n),
            None => return pages,
        }
    }
}

async fn check(
    d: Arc<dyn Driver>,
    cfg: ConnectionConfig,
    table: fn(&str) -> String,
    setup: Vec<String>,
) {
    let mut s = d.connect(&cfg, None).await.expect("connect");
    for sql in &setup {
        if let Err(e) = run(&mut s, sql).await {
            panic!("{e}");
        }
    }
    // A name that differs only in case is refused where table names ignore
    // it (TiDB); everything else must be there.
    if run(&mut s, &table("orders")).await.is_err() {
        eprintln!("{}: names ignore case here", d.info().id);
    }
    let all = s.list_objects().await.unwrap();
    let mut kinds: Vec<String> = d
        .info()
        .object_kinds
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    for o in &all {
        if !kinds.contains(&o.kind) {
            kinds.push(o.kind.clone());
        }
    }
    eprintln!("{}: {} objects, kinds {kinds:?}", d.info().id, all.len());
    let mut checked = 0;
    for q in queries(&kinds) {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let tables = ObjectQuery {
        kind: Some("table".into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &tables).await;
    assert!(pages >= 3, "a folder of more than one page: {pages}");
    let p = s.list_objects_page(&tables).await.unwrap();
    assert_eq!(p.objects.len(), 10);
    eprintln!(
        "{}: {checked} filter/size combinations equal; the tables folder in {pages} pages of 10",
        d.info().id
    );
}

async fn exercise(id: &str, env: &str, table: fn(&str) -> String, setup: Vec<String>) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    eprintln!("{id}: {}", admin.server_version().await.unwrap());
    run(&mut admin, &format!("DROP DATABASE IF EXISTS {DB}"))
        .await
        .unwrap();
    run(&mut admin, &format!("CREATE DATABASE {DB}"))
        .await
        .unwrap();
    let in_db = ConnectionConfig {
        database: DB.into(),
        ..cfg
    };
    let outcome = tokio::spawn(check(d.clone(), in_db, table, setup)).await;
    let dropped = run(&mut admin, &format!("DROP DATABASE {DB}")).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    dropped.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql() {
    let setup = [common(mysql_table, NAMES), routines_and_triggers()].concat();
    exercise("mysql", "DBINE_TEST_MYSQL_URL", mysql_table, setup.clone()).await;
    // The managed products read their catalog as MySQL does.
    exercise(
        "aurora-mysql",
        "DBINE_TEST_MYSQL_URL",
        mysql_table,
        setup.clone(),
    )
    .await;
    exercise("cloudsql-mysql", "DBINE_TEST_MYSQL_URL", mysql_table, setup).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql9() {
    let setup = [common(mysql_table, NAMES), routines_and_triggers()].concat();
    exercise("mysql", "DBINE_TEST_MYSQL9_URL", mysql_table, setup).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb() {
    let setup = [
        common(mysql_table, NAMES),
        routines_and_triggers(),
        sequences(),
    ]
    .concat();
    exercise("mariadb", "DBINE_TEST_MARIADB_URL", mysql_table, setup).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tidb() {
    let setup = [common(mysql_table, NAMES), sequences()].concat();
    exercise("tidb", "DBINE_TEST_TIDB8_URL", mysql_table, setup).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn oceanbase() {
    let setup = [
        common(mysql_table, NAMES),
        routines_and_triggers(),
        sequences(),
    ]
    .concat();
    exercise("oceanbase", "DBINE_TEST_OCEANBASE_URL", mysql_table, setup).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn starrocks() {
    let mut setup = common(olap_table, &["Orders", "axb", "T_Mixed"]);
    setup.push(
        "CREATE MATERIALIZED VIEW mv_a DISTRIBUTED BY HASH(id) BUCKETS 1 REFRESH MANUAL \
         PROPERTIES ('replication_num' = '1') AS SELECT id FROM t_00"
            .into(),
    );
    exercise("starrocks", "DBINE_TEST_STARROCKS_URL", olap_table, setup).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn doris() {
    let setup = common(olap_table, &["Orders", "axb", "T_Mixed"]);
    exercise("doris", "DBINE_TEST_DORIS_URL", olap_table, setup.clone()).await;
    // VeloDB is managed Doris.
    exercise("velodb", "DBINE_TEST_DORIS_URL", olap_table, setup).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn greptimedb() {
    let setup = common(greptime_table, &["Orders", "axb", "T_Mixed"]);
    exercise(
        "greptimedb",
        "DBINE_TEST_GREPTIMEDB_URL",
        greptime_table,
        setup,
    )
    .await;
}
