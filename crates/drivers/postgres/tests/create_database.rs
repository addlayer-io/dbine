//! "Nueva base de datos" with options against real servers: read the
//! server's choices, create a database with options, read them back from
//! the catalog and drop it; then the plain create. Each test reads
//! `DBINE_TEST_<ENGINE>_URL` (`postgres://user:pass@host:port/db`) and is
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_OPENGAUSS_URL='postgres://gaussdb:Dbine@1234@localhost:25020/postgres' \
//! DBINE_TEST_TIMESCALEDB_URL=postgres://postgres:pw@localhost:25015/postgres \
//! DBINE_TEST_YUGABYTE_URL=postgres://yugabyte@localhost:25016/yugabyte \
//! DBINE_TEST_GREENGAGE_URL=postgres://gpadmin:pw@localhost:25018/postgres \
//! DBINE_TEST_RISINGWAVE_URL=postgres://root@localhost:25023/dev \
//! DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize \
//!   cargo test -p dbine-driver-postgres --test create_database -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use std::collections::BTreeMap;
use std::sync::Arc;

const DB: &str = "dbine_create_opts";
const PLAIN: &str = "dbine_create_plain";
const OWNER: &str = "dbine_create_owner";

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

/// Runs `sql`, ignoring errors (cleanup).
async fn quiet(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(sql, 10, &mut out).await;
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

/// The first cell of the first row, as text.
async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    let c = &out.results.iter().rfind(|r| !r.rows.is_empty()).unwrap_or_else(|| panic!("{sql}: no rows")).rows[0][0];
    c.as_str().map(str::to_string).unwrap_or_else(|| c.to_string())
}

fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// Choices, the create with `options` (checked by `verify` returning
/// `expect`), the drop, and the plain create. `owner_sql` creates the
/// role the options name, `before_drop` undoes what blocks a drop.
#[allow(clippy::too_many_arguments)]
async fn exercise(
    id: &str,
    env: &str,
    owner_sql: Option<&str>,
    options: &[(&str, &str)],
    verify: &str,
    expect: &str,
    before_drop: Option<&str>,
    choice_keys: &[&str],
) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    let mut s = d.connect(&cfg, None).await.unwrap();
    if let Some(b) = before_drop {
        quiet(&mut s, b).await;
    }
    let _ = s.drop_database(DB).await;
    let _ = s.drop_database(PLAIN).await;
    if let Some(sql) = owner_sql {
        quiet(&mut s, &format!("DROP USER {OWNER}")).await;
        run(&mut s, sql).await;
    }

    let choices = s.create_database_choices().await.unwrap();
    for k in choice_keys {
        let c = choices.iter().find(|c| c.key == *k).unwrap_or_else(|| panic!("{id}: no choices for {k}: {choices:?}"));
        assert!(c.default.is_some() || !c.values.is_empty(), "{id}: empty choices for {k}");
    }
    eprintln!("{id} choices: {:?}", choices.iter().map(|c| (&c.key, &c.default, c.values.len())).collect::<Vec<_>>());

    let options = opts(options);
    let fields = d.create_database_fields();
    for k in options.keys() {
        assert!(fields.iter().any(|f| f.key == k), "{id}: {k} isn't one of its fields");
    }
    let script = d.create_database_script(DB, &options).unwrap();
    eprintln!("{script}");
    s.create_database_with(DB, &options).await.unwrap_or_else(|e| panic!("{id}: {e}\n{script}"));
    let got = scalar(&mut s, verify).await;
    assert_eq!(got, expect, "{id}");

    if let Some(b) = before_drop {
        run(&mut s, b).await;
    }
    s.drop_database(DB).await.unwrap();
    // Without options it's the plain create.
    s.create_database_with(PLAIN, &BTreeMap::new()).await.unwrap();
    s.drop_database(PLAIN).await.unwrap();
    if owner_sql.is_some() {
        quiet(&mut s, &format!("DROP USER {OWNER}")).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres() {
    // PostgreSQL 16 in the container: ICU from template0.
    exercise(
        "postgres",
        "DBINE_TEST_POSTGRES_URL",
        Some(&format!("CREATE ROLE {OWNER}")),
        &[
            ("owner", OWNER),
            ("template", "template0"),
            ("encoding", "UTF8"),
            ("locale_provider", "icu"),
            ("icu_locale", "es-AR"),
            ("lc_collate", "C"),
            ("lc_ctype", "C"),
            ("tablespace", "pg_default"),
            ("connection_limit", "7"),
            ("is_template", "true"),
        ],
        &format!(
            "SELECT concat_ws('|', pg_get_userbyid(datdba), pg_encoding_to_char(encoding), datlocprovider, daticulocale, datcollate, datconnlimit, datistemplate)
             FROM pg_database WHERE datname = '{DB}'"
        ),
        &format!("{OWNER}|UTF8|i|es-AR|C|7|t"),
        Some(&format!("ALTER DATABASE {DB} IS_TEMPLATE false")),
        &["owner", "template", "encoding", "lc_collate", "icu_locale", "locale_provider", "tablespace"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescale() {
    exercise(
        "timescaledb",
        "DBINE_TEST_TIMESCALEDB_URL",
        None,
        &[("template", "template0"), ("encoding", "LATIN1"), ("lc_collate", "C"), ("lc_ctype", "C"), ("connection_limit", "3")],
        &format!("SELECT concat_ws('|', pg_encoding_to_char(encoding), datcollate, datconnlimit) FROM pg_database WHERE datname = '{DB}'"),
        "LATIN1|C|3",
        None,
        &["owner", "encoding", "lc_collate"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss() {
    exercise(
        "opengauss",
        "DBINE_TEST_OPENGAUSS_URL",
        Some(&format!("CREATE USER {OWNER} PASSWORD 'Dbine@1234'")),
        &[("owner", OWNER), ("encoding", "UTF8"), ("lc_collate", "C"), ("lc_ctype", "C"), ("dbcompatibility", "B"), ("connection_limit", "5")],
        &format!(
            "SELECT concat_ws('|', pg_get_userbyid(datdba), pg_encoding_to_char(encoding), datcollate, datcompatibility, datconnlimit)
             FROM pg_database WHERE datname = '{DB}'"
        ),
        &format!("{OWNER}|UTF8|C|B|5"),
        None,
        &["owner", "template", "encoding", "lc_collate", "dbcompatibility"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn yugabyte() {
    exercise(
        "yugabytedb",
        "DBINE_TEST_YUGABYTE_URL",
        Some(&format!("CREATE ROLE {OWNER}")),
        &[("owner", OWNER), ("colocation", "true"), ("connection_limit", "9")],
        &format!(
            "SELECT concat_ws('|', pg_get_userbyid(datdba), datconnlimit) FROM pg_database WHERE datname = '{DB}'"
        ),
        &format!("{OWNER}|9"),
        None,
        &["owner", "template", "encoding"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn greengage() {
    exercise(
        "greengage",
        "DBINE_TEST_GREENGAGE_URL",
        Some(&format!("CREATE ROLE {OWNER}")),
        &[("owner", OWNER), ("template", "template0"), ("encoding", "UTF8"), ("connection_limit", "4")],
        &format!("SELECT concat_ws('|', pg_get_userbyid(datdba), pg_encoding_to_char(encoding), datconnlimit) FROM pg_database WHERE datname = '{DB}'"),
        &format!("{OWNER}|UTF8|4"),
        None,
        &["owner", "template", "encoding", "tablespace"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroach() {
    // A single-node cluster has no regions: only the owner here (the
    // multi-region clauses are checked against a `cockroach demo` with
    // localities, see the report).
    exercise(
        "cockroachdb",
        "DBINE_TEST_COCKROACH_URL",
        Some(&format!("CREATE USER {OWNER}")),
        &[("owner", OWNER)],
        &format!("SELECT owner FROM [SHOW DATABASES] WHERE database_name = '{DB}'"),
        OWNER,
        None,
        &["owner"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn risingwave() {
    exercise(
        "risingwave",
        "DBINE_TEST_RISINGWAVE_URL",
        Some(&format!("CREATE USER {OWNER}")),
        // barrier_interval_ms / checkpoint_frequency need a license the
        // container doesn't have.
        &[("owner", OWNER), ("resource_group", "default")],
        &format!(
            "SELECT concat_ws('|', u.name, d.resource_group)
             FROM rw_catalog.rw_databases d JOIN rw_catalog.rw_users u ON u.id = d.owner WHERE d.name = '{DB}'"
        ),
        &format!("{OWNER}|default"),
        None,
        &["owner", "resource_group"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn materialize_plain_only() {
    let Some(cfg) = cfg("materialize", "DBINE_TEST_MATERIALIZE_URL") else {
        eprintln!("DBINE_TEST_MATERIALIZE_URL not set; skipping");
        return;
    };
    let d = driver("materialize");
    assert!(d.create_database_fields().is_empty());
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database(PLAIN).await;
    s.create_database_with(PLAIN, &BTreeMap::new()).await.unwrap();
    s.drop_database(PLAIN).await.unwrap();
}
