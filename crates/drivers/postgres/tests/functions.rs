//! The function catalog (issue #97) against real servers. Each test reads
//! `DBINE_TEST_<ENGINE>_URL` (`postgres://user:pass@host:port/db`) and is
//! skipped without it; what it creates it drops.
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//!   cargo test -p dbine-driver-postgres --test functions -- --ignored
//! ```
//!
//! The others: `DBINE_TEST_COCKROACH_URL` (root@…:26014/defaultdb),
//! `DBINE_TEST_TIMESCALEDB_URL`, `DBINE_TEST_YUGABYTE_URL`,
//! `DBINE_TEST_GREENGAGE_URL`, `DBINE_TEST_GREENPLUM_URL` (Greengage 6),
//! `DBINE_TEST_CLOUDBERRY_URL`, `DBINE_TEST_OPENGAUSS_URL`,
//! `DBINE_TEST_MATERIALIZE_URL`, `DBINE_TEST_CRATEDB_URL`,
//! `DBINE_TEST_RISINGWAVE_URL` and `DBINE_TEST_H2_URL`.

use dbine_driver::{ConnectionConfig, FunctionInfo, FunctionKind, QueryOutcome, Session};

/// `scheme://user:pass@host:port/db` into a config (no URL escapes).
fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport
        .rsplit_once(':')
        .map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    let mut cfg = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    };
    cfg.options.insert("sslmode".into(), "disable".into());
    cfg
}

async fn connect(id: &str, env: &str) -> Option<Box<dyn Session>> {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return None;
    };
    let d = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap();
    Some(
        d.connect(&parse_url(id, &url), None)
            .await
            .expect("connect"),
    )
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 100, &mut out).await {
        panic!("{sql}: {e}");
    }
}

/// What a catalog must and mustn't have.
struct Expect<'a> {
    present: &'a [(&'a str, FunctionKind)],
    absent: &'a [&'a str],
    /// A user's function and the schema it must carry.
    user: Option<(&'a str, &'a str)>,
}

fn check(id: &str, c: &[FunctionInfo], e: &Expect) {
    eprintln!("{id}: {} entries", c.len());
    for (name, kind) in e.present {
        assert!(
            c.iter().any(|f| f.name == *name && f.kind == *kind),
            "{id}: {name} ({kind:?}) missing"
        );
    }
    for name in e.absent {
        assert!(!c.iter().any(|f| f.name == *name), "{id}: {name} listed");
    }
    if let Some((name, schema)) = e.user {
        let f = c
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("{id}: {name} missing"));
        assert_eq!(f.schema, schema, "{id}: {f:?}");
    }
    // Built-ins are reached by their bare name.
    for (name, _) in e.present {
        assert!(
            c.iter().any(|f| f.name == *name && f.schema.is_empty()),
            "{id}: {name} has a schema"
        );
    }
}

fn has(c: &[FunctionInfo], name: &str) -> bool {
    c.iter().any(|f| f.name == name)
}

/// PostgreSQL's catalog on any variant that keeps it: the built-ins with
/// their comments, gated by version, without internals, and a user's
/// function with its schema.
async fn pg_catalog(id: &str, env: &str, extra: &[(&str, FunctionKind)]) {
    let Some(mut s) = connect(id, env).await else {
        return;
    };
    run(&mut s, "DROP SCHEMA IF EXISTS dbine_fn CASCADE").await;
    run(
        &mut s,
        "CREATE SCHEMA dbine_fn;
         CREATE FUNCTION dbine_fn.net_price(x numeric) RETURNS numeric LANGUAGE sql AS 'SELECT x * 2';",
    )
    .await;
    // Dropped before anything can fail.
    let c = s.function_catalog().await;
    run(&mut s, "DROP SCHEMA dbine_fn CASCADE").await;
    let c = c.unwrap();

    let mut present = vec![
        ("count", FunctionKind::Aggregate),
        ("string_agg", FunctionKind::Aggregate),
        ("generate_series", FunctionKind::Table),
        ("row_number", FunctionKind::Window),
        ("lower", FunctionKind::Scalar),
        ("now", FunctionKind::Scalar),
    ];
    present.extend_from_slice(extra);
    check(
        id,
        &c,
        &Expect {
            present: &present,
            absent: &[
                "approx_distinct",
                "int4pl",
                "textcat",
                "boolin",
                "int4send",
                "pg_stat_get_numscans",
                "pg_show_all_settings",
                "btint4cmp",
                "int4_avg_combine",
                "int8dec",
                "int4range_canonical",
            ],
            user: Some(("net_price", "dbine_fn")),
        },
    );
    let count = c.iter().find(|f| f.name == "count").unwrap();
    assert!(!count.args.is_empty());
    // The server's own comment, where it keeps them.
    if !matches!(id, "yugabytedb" | "cockroachdb") {
        let f = c.iter().find(|f| f.name == "string_agg").unwrap();
        assert!(!f.description.is_empty(), "{id}: {f:?}");
    }
    // Follows the server's version.
    let mut version = s.server_version().await.unwrap();
    if id == "cockroachdb" {
        // It names its own version; its SQL is PostgreSQL 13's.
        version = "13".into();
    }
    let v = dbine_driver::functions::version_numbers(&version);
    let at_least = |major: u32| v.first().is_some_and(|m| *m >= major);
    if !version.contains("openGauss") {
        assert_eq!(has(&c, "jsonb_path_query"), at_least(12), "{id}: {version}");
    }
    if id == "postgres" {
        assert_eq!(has(&c, "any_value"), at_least(16), "{version}");
        assert_eq!(has(&c, "json_value"), at_least(17), "{version}");
    }
}

#[tokio::test]
#[ignore]
async fn postgres() {
    pg_catalog(
        "postgres",
        "DBINE_TEST_POSTGRES_URL",
        &[
            ("starts_with", FunctionKind::Scalar),
            ("pg_size_pretty", FunctionKind::Scalar),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn cockroachdb() {
    pg_catalog(
        "cockroachdb",
        "DBINE_TEST_COCKROACH_URL",
        &[("unique_rowid", FunctionKind::Scalar)],
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn timescaledb() {
    let Some(mut s) = connect("timescaledb", "DBINE_TEST_TIMESCALEDB_URL").await else {
        return;
    };
    let c = s.function_catalog().await.unwrap();
    // The extension's functions (the image installs it in `postgres`),
    // with the schema they were installed in.
    let bucket = c.iter().find(|f| f.name == "time_bucket").unwrap();
    assert!(!bucket.schema.is_empty(), "{bucket:?}");
    assert!(!c.iter().any(|f| f.schema.starts_with("_timescaledb")));
    drop(s);
    pg_catalog("timescaledb", "DBINE_TEST_TIMESCALEDB_URL", &[]).await;
}

#[tokio::test]
#[ignore]
async fn yugabytedb() {
    pg_catalog(
        "yugabytedb",
        "DBINE_TEST_YUGABYTE_URL",
        &[("yb_hash_code", FunctionKind::Scalar)],
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn greengage() {
    pg_catalog("greengage", "DBINE_TEST_GREENGAGE_URL", &[]).await;
}

/// Greengage 6 (PostgreSQL 9.4): no `prokind`.
#[tokio::test]
#[ignore]
async fn greenplum() {
    pg_catalog("greenplum", "DBINE_TEST_GREENPLUM_URL", &[]).await;
}

#[tokio::test]
#[ignore]
async fn cloudberry() {
    pg_catalog("cloudberry", "DBINE_TEST_CLOUDBERRY_URL", &[]).await;
}

#[tokio::test]
#[ignore]
async fn opengauss() {
    pg_catalog(
        "opengauss",
        "DBINE_TEST_OPENGAUSS_URL",
        &[
            ("add_months", FunctionKind::Scalar),
            ("listagg", FunctionKind::Aggregate),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn materialize() {
    let Some(mut s) = connect("materialize", "DBINE_TEST_MATERIALIZE_URL").await else {
        return;
    };
    let c = s.function_catalog().await.unwrap();
    check(
        "materialize",
        &c,
        &Expect {
            present: &[
                ("mz_now", FunctionKind::Scalar),
                ("count", FunctionKind::Aggregate),
                ("list_agg", FunctionKind::Aggregate),
                ("generate_series", FunctionKind::Table),
                ("lag", FunctionKind::Window),
                ("date_bin", FunctionKind::Scalar),
            ],
            absent: &["approx_distinct", "mz_error_if_null", "+"],
            user: None,
        },
    );
    let concat = c.iter().find(|f| f.name == "concat").unwrap();
    assert!(concat.args.contains("VARIADIC"), "{concat:?}");
}

#[tokio::test]
#[ignore]
async fn cratedb() {
    let Some(mut s) = connect("cratedb", "DBINE_TEST_CRATEDB_URL").await else {
        return;
    };
    run(&mut s, "DROP FUNCTION IF EXISTS doc.dbine_fn_next(integer)").await;
    run(
        &mut s,
        "CREATE FUNCTION doc.dbine_fn_next(integer) RETURNS integer LANGUAGE JAVASCRIPT \
         AS 'function dbine_fn_next(a) { return a + 1; }'",
    )
    .await;
    let c = s.function_catalog().await;
    run(&mut s, "DROP FUNCTION doc.dbine_fn_next(integer)").await;
    let c = c.unwrap();
    check(
        "cratedb",
        &c,
        &Expect {
            present: &[
                ("hyperloglog_distinct", FunctionKind::Aggregate),
                ("knn_match", FunctionKind::Scalar),
                ("date_trunc", FunctionKind::Scalar),
                ("generate_series", FunctionKind::Table),
                ("row_number", FunctionKind::Window),
            ],
            absent: &[
                "approx_distinct",
                "op_and",
                "int4recv",
                "_cast",
                "subscript",
            ],
            user: Some(("dbine_fn_next", "doc")),
        },
    );
    let f = c.iter().find(|f| f.name == "date_trunc").unwrap();
    assert!(f.args.contains("text"), "{f:?}");
}

#[tokio::test]
#[ignore]
async fn risingwave() {
    let Some(mut s) = connect("risingwave", "DBINE_TEST_RISINGWAVE_URL").await else {
        return;
    };
    run(&mut s, "DROP FUNCTION IF EXISTS dbine_fn_add").await;
    run(
        &mut s,
        "CREATE FUNCTION dbine_fn_add(a int, b varchar, c double precision) RETURNS int \
         LANGUAGE sql AS 'select a + length(b)'",
    )
    .await;
    let c = s.function_catalog().await;
    run(&mut s, "DROP FUNCTION dbine_fn_add").await;
    let c = c.unwrap();
    check(
        "risingwave",
        &c,
        &Expect {
            present: &[
                ("tumble", FunctionKind::Table),
                ("approx_percentile", FunctionKind::Aggregate),
                ("jsonb_agg", FunctionKind::Aggregate),
                ("row_number", FunctionKind::Window),
                ("proctime", FunctionKind::Scalar),
            ],
            absent: &["approx_distinct", "json_value"],
            user: Some(("dbine_fn_add", "public")),
        },
    );
    // The arguments in their order.
    let f = c.iter().find(|f| f.name == "dbine_fn_add").unwrap();
    assert_eq!(f.args, "(integer, character varying, double precision)");
    // The static catalog follows the server's version (the image is 2.2+).
    assert!(has(&c, "mysql_query") && has(&c, "map_keys"));
}

#[tokio::test]
#[ignore]
async fn h2() {
    let Some(mut s) = connect("h2", "DBINE_TEST_H2_URL").await else {
        return;
    };
    run(&mut s, "DROP ALIAS IF EXISTS dbine_fn_abs").await;
    run(
        &mut s,
        "CREATE ALIAS dbine_fn_abs FOR 'java.lang.Math.abs(int)'",
    )
    .await;
    let c = s.function_catalog().await;
    run(&mut s, "DROP ALIAS dbine_fn_abs").await;
    let c = c.unwrap();
    check(
        "h2",
        &c,
        &Expect {
            present: &[
                ("listagg", FunctionKind::Aggregate),
                ("formatdatetime", FunctionKind::Scalar),
                ("csvread", FunctionKind::Table),
                ("ratio_to_report", FunctionKind::Window),
            ],
            absent: &["approx_distinct", "jsonb_path_query"],
            user: Some(("dbine_fn_abs", "public")),
        },
    );
    // H2 2.2 added BTRIM.
    let mut out = QueryOutcome::default();
    let runs = s.execute("SELECT BTRIM(' a ')", 10, &mut out).await.is_ok();
    assert_eq!(has(&c, "btrim"), runs);
    let f = c.iter().find(|f| f.name == "dbine_fn_abs").unwrap();
    assert!(f.args.contains("integer"), "{f:?}");
}
