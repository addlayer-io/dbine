//! The function catalog for the editor's completion (issue #97) against
//! real servers (`DBINE_TEST_<ENGINE>_URL`, `mysql://[user[:pass]@]host:port`):
//! functions each engine has and some it doesn't, by version where that
//! counts, and the user's own functions with their schema. Each test is
//! skipped without its variable and drops what it creates:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25044 \
//! DBINE_TEST_STARROCKS_URL=mysql://root@localhost:25030 \
//! DBINE_TEST_DORIS_URL=mysql://root@localhost:25031 \
//! DBINE_TEST_GREPTIMEDB_URL=mysql://localhost:25017 \
//!   cargo test -p dbine-driver-mysql --test functions -- --ignored --test-threads=1
//! ```

use dbine_driver::functions::version_at_least;
use dbine_driver::{ConnectionConfig, Driver, FunctionInfo, FunctionKind, QueryOutcome, Session};
use std::sync::Arc;

const DB: &str = "dbine_fn_catalog";

fn cfg(driver: &str, url: &str) -> Option<ConnectionConfig> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
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
    dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

/// The catalog of a session on `env`'s server (in `database` when given),
/// with the server's version; `None` without the variable.
async fn catalog(
    id: &str,
    env: &str,
    database: Option<&str>,
) -> Option<(Vec<FunctionInfo>, String)> {
    let Some(c) = std::env::var(env).ok().and_then(|u| cfg(id, &u)) else {
        eprintln!("{env} not set; skipping");
        return None;
    };
    let mut s = driver(id).connect(&c, database).await.unwrap();
    let version = s.server_version().await.unwrap();
    let cat = s.function_catalog().await.unwrap();
    eprintln!("{id} {version}: {} entries", cat.len());
    Some((cat, version))
}

fn find<'a>(cat: &'a [FunctionInfo], name: &str) -> Option<&'a FunctionInfo> {
    cat.iter().find(|f| f.name.eq_ignore_ascii_case(name))
}

#[track_caller]
fn has(cat: &[FunctionInfo], name: &str, kind: FunctionKind) -> FunctionInfo {
    let f = find(cat, name).unwrap_or_else(|| panic!("{name} missing"));
    assert!(
        cat.iter()
            .any(|f| f.name.eq_ignore_ascii_case(name) && f.kind == kind),
        "{name}: {:?}, not {kind:?}",
        f.kind
    );
    f.clone()
}

#[track_caller]
fn lacks(cat: &[FunctionInfo], names: &[&str]) {
    for n in names {
        assert!(find(cat, n).is_none(), "{n} listed: {:?}", find(cat, n));
    }
}

/// A stored function in a database of its own: listed with its schema,
/// arguments, result type and comment.
async fn with_user_function(id: &str, env: &str, check: impl Fn(&[FunctionInfo], &str)) {
    let Some(c) = std::env::var(env).ok().and_then(|u| cfg(id, &u)) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    let mut m = d.connect(&c, None).await.unwrap();
    let _ = m.drop_database(DB).await;
    m.create_database(DB).await.unwrap();
    let mut s = d.connect(&c, Some(DB)).await.unwrap();
    run(
        &mut s,
        "CREATE FUNCTION doble(x int) RETURNS int DETERMINISTIC COMMENT 'El doble' RETURN x * 2",
    )
    .await;
    let version = s.server_version().await.unwrap();
    let cat = s.function_catalog().await.unwrap();
    drop(s);
    m.drop_database(DB).await.unwrap();
    eprintln!("{id} {version}: {} entries", cat.len());
    let doble = cat
        .iter()
        .find(|f| f.name == "doble")
        .expect("the user's function");
    assert_eq!(doble.schema, DB);
    // MariaDB writes the display width: `int(11)`.
    assert!(doble.args.starts_with("(x int"), "{}", doble.args);
    assert!(doble.returns.starts_with("int"), "{}", doble.returns);
    assert_eq!(doble.description, "El doble");
    assert_eq!(doble.kind, FunctionKind::Scalar);
    check(&cat, &version);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql_lists_its_help_tables_and_the_users_functions() {
    with_user_function("mysql", "DBINE_TEST_MYSQL_URL", |cat, version| {
        has(cat, "ROW_NUMBER", FunctionKind::Window);
        has(cat, "COUNT", FunctionKind::Aggregate);
        has(cat, "JSON_ARRAYAGG", FunctionKind::Aggregate);
        has(cat, "JSON_TABLE", FunctionKind::Table);
        let upper = has(cat, "UPPER", FunctionKind::Scalar);
        assert_eq!(upper.args, "(str)");
        assert!(
            upper.description.starts_with("Returns the string str"),
            "{}",
            upper.description
        );
        let round: Vec<&str> = cat
            .iter()
            .filter(|f| f.name == "ROUND")
            .map(|f| f.args.as_str())
            .collect();
        assert_eq!(round, ["(X)", "(X,D)"]);
        if version_at_least(version, "8.0.21") {
            has(cat, "JSON_VALUE", FunctionKind::Scalar);
        } else {
            lacks(cat, &["JSON_VALUE"]);
        }
        if version_at_least(version, "8.0.11") {
            lacks(cat, &["PASSWORD"]);
        }
        if version_at_least(version, "8.0.24") {
            has(cat, "ST_Collect", FunctionKind::Aggregate);
        }
        // Other engines' functions, operators and statements.
        lacks(
            cat,
            &[
                "approx_distinct",
                "TIDB_VERSION",
                "VEC_DISTANCE_EUCLIDEAN",
                "NVL2",
                "LIKE",
                "SELECT",
                "CAN_ACCESS_TABLE",
            ],
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb_lists_its_help_tables_and_the_users_functions() {
    with_user_function("mariadb", "DBINE_TEST_MARIADB_URL", |cat, version| {
        has(cat, "ROW_NUMBER", FunctionKind::Window);
        has(cat, "GROUP_CONCAT", FunctionKind::Aggregate);
        has(cat, "JSON_TABLE", FunctionKind::Table);
        has(cat, "NVL2", FunctionKind::Scalar);
        let coalesce = has(cat, "COALESCE", FunctionKind::Scalar);
        assert!(
            coalesce
                .description
                .starts_with("Returns the first non-NULL value"),
            "{}",
            coalesce.description
        );
        // Not in its help tables: from the static catalog, by version.
        if version_at_least(version, "11.7") {
            has(cat, "VEC_DISTANCE_EUCLIDEAN", FunctionKind::Scalar);
        } else {
            lacks(cat, &["VEC_DISTANCE_EUCLIDEAN"]);
        }
        if version_at_least(version, "10.7") {
            has(cat, "NATURAL_SORT_KEY", FunctionKind::Scalar);
        }
        if version_at_least(version, "11.0") {
            has(cat, "JSON_PRETTY", FunctionKind::Scalar);
        }
        // Spider's exist only once its install script creates them.
        lacks(cat, &["SPIDER_DIRECT_SQL", "SPIDER_COPY_TABLES"]);
        // MySQL's only, placeholders and other engines'.
        lacks(
            cat,
            &[
                "REGEXP_LIKE",
                "JSON_SCHEMA_VALIDATION_REPORT",
                "ROLES_GRAPHML",
                "approx_distinct",
                "function_name",
                "TIDB_VERSION",
            ],
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tidb_lists_its_builtins() {
    let Some((cat, version)) = catalog("tidb", "DBINE_TEST_TIDB_URL", None).await else {
        return;
    };
    has(&cat, "TIDB_VERSION", FunctionKind::Scalar);
    has(&cat, "APPROX_PERCENTILE", FunctionKind::Aggregate);
    has(&cat, "ROW_NUMBER", FunctionKind::Window);
    // Listed as `char_func` and `insert_func`.
    assert_eq!(
        has(&cat, "CHAR", FunctionKind::Scalar).args,
        "(N,... [USING charset_name])"
    );
    has(&cat, "INSERT", FunctionKind::Scalar);
    let own = version.split_once("TiDB-").map_or("", |(_, v)| v);
    if version_at_least(own, "8.4") {
        has(&cat, "VEC_DIMS", FunctionKind::Scalar);
    }
    // TiDB has neither; operators aren't functions.
    lacks(
        &cat,
        &[
            "JSON_TABLE",
            "JSON_VALUE",
            "approx_distinct",
            "EQ",
            "AND",
            "char_func",
        ],
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn starrocks_lists_its_builtins_by_signature() {
    let Some((cat, _)) = catalog("starrocks", "DBINE_TEST_STARROCKS_URL", None).await else {
        return;
    };
    let acd = has(&cat, "approx_count_distinct", FunctionKind::Aggregate);
    assert!(acd.args.starts_with('('), "{acd:?}");
    has(&cat, "row_number", FunctionKind::Window);
    has(&cat, "lag", FunctionKind::Window);
    has(&cat, "unnest", FunctionKind::Table);
    // One signature per argument count, not per type.
    assert_eq!(cat.iter().filter(|f| f.name == "abs").count(), 1);
    assert!(cat.iter().all(|f| !f.name.starts_with('_')));
    lacks(&cat, &["JSON_TABLE", "TIDB_VERSION"]);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn doris_lists_its_builtins_and_the_users_functions() {
    let Some(c) = std::env::var("DBINE_TEST_DORIS_URL")
        .ok()
        .and_then(|u| cfg("doris", &u))
    else {
        eprintln!("DBINE_TEST_DORIS_URL not set; skipping");
        return;
    };
    let d = driver("doris");
    let mut m = d.connect(&c, None).await.unwrap();
    let _ = m.drop_database(DB).await;
    m.create_database(DB).await.unwrap();
    let mut s = d.connect(&c, Some(DB)).await.unwrap();
    run(
        &mut s,
        "CREATE ALIAS FUNCTION enmascarar(INT) WITH PARAMETER(id) AS CONCAT(LEFT(id, 3), '****')",
    )
    .await;
    let cat = s.function_catalog().await.unwrap();
    drop(s);
    m.drop_database(DB).await.unwrap();
    eprintln!("doris: {} entries", cat.len());
    let alias = cat
        .iter()
        .find(|f| f.name == "enmascarar")
        .expect("the user's function");
    assert_eq!(alias.schema, DB);
    // Doris 4 lists built-ins by name: the static catalog gives the kind.
    has(&cat, "approx_count_distinct", FunctionKind::Aggregate);
    has(&cat, "row_number", FunctionKind::Window);
    has(&cat, "explode_split", FunctionKind::Table);
    assert_eq!(
        has(&cat, "date_format", FunctionKind::Scalar).args,
        "(date,format)"
    );
    assert!(cat.iter().all(|f| !f.name.starts_with('_')));
    lacks(&cat, &["JSON_TABLE", "TIDB_VERSION"]);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn greptimedb_lists_its_static_catalog() {
    let Some((cat, _)) = catalog("greptimedb", "DBINE_TEST_GREPTIMEDB_URL", None).await else {
        return;
    };
    has(&cat, "date_bin", FunctionKind::Scalar);
    has(&cat, "json_get_int", FunctionKind::Scalar);
    has(&cat, "approx_distinct", FunctionKind::Aggregate);
    has(&cat, "row_number", FunctionKind::Window);
    lacks(&cat, &["JSON_TABLE", "GROUP_CONCAT", "TIDB_VERSION"]);
}
