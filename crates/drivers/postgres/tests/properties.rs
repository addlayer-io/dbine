//! "Propiedades" of a database against real servers: create a temp
//! database, read its properties, apply several changes, read them back,
//! undo what blocks a drop and drop it. Each test reads
//! `DBINE_TEST_<ENGINE>_URL` (`postgres://user:pass@host:port/db`) and is
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_TIMESCALEDB_URL=postgres://postgres:pw@localhost:25015/postgres \
//! DBINE_TEST_YUGABYTE_URL=postgres://yugabyte@localhost:25016/yugabyte \
//! DBINE_TEST_OPENGAUSS_URL='postgres://gaussdb:Dbine@1234@localhost:25020/postgres' \
//! DBINE_TEST_GREENGAGE_URL=postgres://gpadmin:pw@localhost:25018/postgres \
//! DBINE_TEST_RISINGWAVE_URL=postgres://root@localhost:25023/dev \
//! DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize \
//!   cargo test -p dbine-driver-postgres --test properties -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use std::collections::BTreeMap;
use std::sync::Arc;

const DB: &str = "dbine_props";
const OWNER: &str = "dbine_props_owner";

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn quiet(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(sql, 10, &mut out).await;
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// One engine: `setup` before the create (the owner role), `first` and
/// `second` rounds of changes, each checked by reading the properties
/// again (`expect` overrides what the read must give where it differs
/// from what was sent), `before_drop` to undo what blocks the drop.
#[allow(clippy::too_many_arguments)]
async fn exercise(
    id: &str,
    env: &str,
    simple: bool,
    setup: &[&str],
    first: &[(&str, &str)],
    second: &[(&str, &str)],
    expect: &[(&str, &str)],
    before_drop: &[&str],
    cleanup: &[&str],
) {
    let Some(mut cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    if simple {
        cfg.options.insert("query_protocol".into(), "simple".into());
    }
    let d = driver(id);
    assert!(d.capabilities().database_properties, "{id}");
    let mut s = d.connect(&cfg, None).await.unwrap();
    for sql in before_drop {
        quiet(&mut s, sql).await;
    }
    let _ = s.drop_database(DB).await;
    for sql in cleanup {
        quiet(&mut s, sql).await;
    }
    for sql in setup {
        run(&mut s, sql).await;
    }
    s.create_database(DB).await.unwrap();

    let p = s.database_properties(DB).await.unwrap_or_else(|e| panic!("{id}: {e}"));
    eprintln!(
        "{id}: tabs {:?}\n  info {:?}\n  values {:?}",
        p.fields.iter().map(|f| f.group).collect::<std::collections::BTreeSet<_>>(),
        p.info.iter().map(|i| format!("{}/{}={}", i.group, i.label, i.value)).collect::<Vec<_>>(),
        p.values
    );
    for f in &p.fields {
        assert!(p.values.contains_key(f.key), "{id}: no current value for {}", f.key);
    }
    for round in [first, second] {
        if round.is_empty() {
            continue;
        }
        // REFRESH COLLATION VERSION is offered only with a recorded version.
        let changes: BTreeMap<String, String> =
            map(round).into_iter().filter(|(k, _)| k != "refresh_collation_version" || p.fields.iter().any(|f| f.key == k)).collect();
        for k in changes.keys() {
            assert!(p.fields.iter().any(|f| f.key == k), "{id}: {k} isn't one of its fields");
        }
        let script = d.alter_database_script(DB, &changes).unwrap();
        eprintln!("{script}");
        s.alter_database(DB, &changes).await.unwrap_or_else(|e| panic!("{id}: {e}\n{script}"));
        let after = s.database_properties(DB).await.unwrap();
        for (k, v) in &changes {
            let want = expect.iter().find(|(ek, _)| ek == k && round == first).map_or(v.as_str(), |(_, ev)| *ev);
            if k == "refresh_collation_version" {
                continue;
            }
            assert_eq!(after.values.get(k).map(String::as_str).unwrap_or(""), want, "{id}: {k}");
        }
    }
    for sql in before_drop {
        run(&mut s, sql).await;
    }
    s.drop_database(DB).await.unwrap();
    for sql in cleanup {
        quiet(&mut s, sql).await;
    }
}

const PG_FIRST: &[(&str, &str)] = &[
    ("owner", OWNER),
    ("connection_limit", "7"),
    ("allow_connections", ""),
    ("is_template", "true"),
    ("set:work_mem", "64MB"),
    ("set:search_path", "\"$user\", public, \"a b\""),
    ("set:statement_timeout", "5s"),
    ("set:default_transaction_isolation", "repeatable read"),
    ("comment", "Base de prueba de o'DBine"),
    ("refresh_collation_version", "true"),
];
const PG_SECOND: &[(&str, &str)] = &[("set:work_mem", ""), ("is_template", ""), ("allow_connections", "true"), ("comment", ""), ("connection_limit", "-1")];

/// The PostgreSQL tests share one server and the same database and role
/// names: one at a time.
static SAME_SERVER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn postgres_like(id: &str, env: &str, simple: bool) {
    let _one_at_a_time = SAME_SERVER.lock().await;
    exercise(
        id,
        env,
        simple,
        &[&format!("CREATE ROLE {OWNER}")],
        PG_FIRST,
        PG_SECOND,
        &[],
        &[&format!("ALTER DATABASE {DB} IS_TEMPLATE false")],
        &[&format!("DROP ROLE {OWNER}")],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres() {
    postgres_like("postgres", "DBINE_TEST_POSTGRES_URL", false).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres_simple_protocol() {
    postgres_like("postgres", "DBINE_TEST_POSTGRES_URL", true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescale() {
    postgres_like("timescaledb", "DBINE_TEST_TIMESCALEDB_URL", false).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn greengage() {
    // Greengage 7 (PostgreSQL 12): no REFRESH COLLATION VERSION.
    let first: Vec<_> = PG_FIRST.iter().copied().filter(|(k, _)| *k != "refresh_collation_version").collect();
    exercise(
        "greengage",
        "DBINE_TEST_GREENGAGE_URL",
        false,
        &[&format!("CREATE ROLE {OWNER}")],
        &first,
        PG_SECOND,
        &[],
        &[&format!("ALTER DATABASE {DB} IS_TEMPLATE false")],
        &[&format!("DROP ROLE {OWNER}")],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn yugabyte() {
    exercise(
        "yugabytedb",
        "DBINE_TEST_YUGABYTE_URL",
        false,
        &[&format!("CREATE ROLE {OWNER}")],
        &[
            ("owner", OWNER),
            ("connection_limit", "9"),
            ("allow_connections", ""),
            ("set:work_mem", "32MB"),
            ("comment", "yb"),
            ("refresh_collation_version", "true"),
        ],
        &[("allow_connections", "true"), ("set:work_mem", "")],
        &[],
        &[],
        &[&format!("DROP ROLE {OWNER}")],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss() {
    exercise(
        "opengauss",
        "DBINE_TEST_OPENGAUSS_URL",
        false,
        &[&format!("CREATE USER {OWNER} PASSWORD 'Dbine@1234'")],
        &[("owner", OWNER), ("connection_limit", "5"), ("set:work_mem", "64MB"), ("set:search_path", "public, x"), ("comment", "og")],
        &[("set:work_mem", ""), ("comment", "")],
        &[],
        &[],
        &[&format!("DROP USER {OWNER}")],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroach() {
    exercise(
        "cockroachdb",
        "DBINE_TEST_COCKROACH_URL",
        false,
        &[&format!("CREATE USER {OWNER}")],
        &[
            ("owner", OWNER),
            ("set:statement_timeout", "10s"),
            ("set:search_path", "public, x"),
            ("set:sql_safe_updates", "on"),
            ("set:default_transaction_isolation", "serializable"),
            ("comment", "crdb"),
        ],
        &[("set:statement_timeout", ""), ("comment", "")],
        &[],
        &[],
        &[&format!("DROP USER {OWNER}")],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn risingwave() {
    exercise(
        "risingwave",
        "DBINE_TEST_RISINGWAVE_URL",
        false,
        &[&format!("CREATE USER {OWNER}")],
        &[("owner", OWNER)],
        &[("owner", "root")],
        &[],
        &[],
        &[&format!("DROP USER {OWNER}")],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn materialize() {
    exercise(
        "materialize",
        "DBINE_TEST_MATERIALIZE_URL",
        false,
        &[&format!("CREATE ROLE {OWNER}")],
        &[("owner", OWNER), ("comment", "mz")],
        &[("owner", "materialize"), ("comment", "")],
        &[],
        &[],
        &[&format!("DROP ROLE {OWNER}")],
    )
    .await;
}
