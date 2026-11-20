//! "Ver dependencias…" › what an object uses (`Session::references`)
//! against a real ClickHouse server: two databases of its own, a view that
//! reads a table of each, a materialized view with its `TO` table, a
//! dictionary on a local table and a table whose default reads it,
//! Distributed, Merge and Buffer tables, a MySQL engine table and a view on
//! `remote()` (sources outside). ClickHouse has no foreign keys nor stored
//! procedures; a SQL function goes through the contract's default.
//! Everything is dropped at the end, also when a check fails.
//!
//! Timeplus (`references_timeplus`): a view and a materialized view `INTO`
//! a stream, whose sources Timeplus records for both.
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//! DBINE_TEST_TIMEPLUS_URL=http://localhost:25119 \
//!   cargo test -p dbine-driver-clickhouse --test references -- --ignored --nocapture
//! ```

use dbine_driver::{
    Confidence, DependencyReport, DependencyScan, DependencyTarget, ObjectRef, QueryOutcome,
    Relation, Session,
};

fn cfg() -> Option<dbine_driver::ConnectionConfig> {
    cfg_from("DBINE_TEST_CLICKHOUSE_URL", "clickhouse")
}

fn cfg_from(var: &str, driver: &str) -> Option<dbine_driver::ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(var).ok()?).expect("URL");
    Some(dbine_driver::ConnectionConfig {
        driver: driver.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .map_err(|e| format!("{e}\n---\n{sql}"))
}

fn target(kind: &str, name: &str) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        },
        column: None,
    }
}

/// The item named so (schema, name), if the report has it.
fn item<'a>(
    r: &'a DependencyReport,
    schema: Option<&str>,
    name: &str,
) -> Option<&'a dbine_driver::Dependent> {
    r.items
        .iter()
        .find(|d| d.schema.as_deref() == schema && d.name == name)
}

#[tokio::test]
#[ignore]
async fn references_live() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set: skipped");
        return;
    };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap();
    let tag = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            % 1_000_000
    );
    let (a, b) = (format!("dbine_refs_{tag}"), format!("dbine_refs_x_{tag}"));
    let func = format!("dbine_refs_f_{tag}");
    let mut admin = d.connect(&c, None).await.unwrap();
    // A cluster that has this server (`default` on recent images).
    let cluster = first(
        &mut admin,
        "SELECT cluster FROM system.clusters WHERE is_local ORDER BY cluster = 'default' DESC, cluster LIMIT 1",
    )
    .await
    .expect("a local cluster");
    // `loading_dependencies_*` (not on old servers such as 22.8).
    let loading = first(
        &mut admin,
        "SELECT count() FROM system.columns WHERE database = 'system' AND table = 'tables' AND name = 'loading_dependencies_table'",
    )
    .await
    .as_deref()
        == Some("1");
    let setup = format!(
        "CREATE DATABASE {a};
         CREATE DATABASE {b};
         CREATE TABLE {a}.src (id UInt64, v String) ENGINE = MergeTree ORDER BY id;
         CREATE TABLE {a}.dst (id UInt64, v String) ENGINE = MergeTree ORDER BY id;
         CREATE TABLE {b}.other (id UInt64, w String) ENGINE = MergeTree ORDER BY id;
         CREATE VIEW {a}.v AS SELECT s.id, o.w FROM {a}.src AS s JOIN {b}.other AS o ON o.id = s.id;
         CREATE MATERIALIZED VIEW {a}.mv TO {a}.dst AS SELECT id, v FROM {a}.src;
         CREATE DICTIONARY {a}.dict (id UInt64, v String) PRIMARY KEY id
           SOURCE(CLICKHOUSE(TABLE 'src' DB '{a}')) LAYOUT(FLAT()) LIFETIME(0);
         CREATE TABLE {a}.withdict (id UInt64, v String DEFAULT dictGet('{a}.dict', 'v', id))
           ENGINE = MergeTree ORDER BY id;
         CREATE TABLE {a}.dist AS {a}.src ENGINE = Distributed('{cluster}', '{a}', 'src', rand());
         CREATE TABLE {a}.mrg AS {a}.src ENGINE = Merge('{a}', '^src$');
         CREATE TABLE {a}.buf AS {a}.src ENGINE = Buffer('{a}', 'src', 1, 10, 100, 10000, 1000000, 10000000, 100000000);
         CREATE TABLE {a}.my (id UInt64, v String)
           ENGINE = MySQL('mysql.example:3306', 'erp', 'clients', 'u', 'not-a-real-password');
         CREATE VIEW {a}.vremote AS SELECT * FROM remote('127.0.0.1:9000', '{b}', 'other', '{user}', '{pw}');
         CREATE FUNCTION {func} AS (x) -> x * 2",
        user = c.username.as_deref().unwrap_or("default"),
        pw = c.password.as_deref().unwrap_or(""),
    );
    let mut failures: Vec<String> = Vec::new();
    match run(&mut admin, &setup).await {
        Err(e) => failures.push(format!("setup: {e}")),
        Ok(()) => checks(&*d, &c, &[&a, &b, &func, &cluster], loading, &mut failures).await,
    }
    // Clean up whatever happened above.
    for sql in [
        format!("DROP DATABASE IF EXISTS {a} SYNC"),
        format!("DROP DATABASE IF EXISTS {b} SYNC"),
        format!("DROP FUNCTION IF EXISTS {func}"),
    ] {
        if let Err(e) = run(&mut admin, &sql).await {
            failures.push(format!("cleanup: {e}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

async fn refs(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    kind: &str,
    name: &str,
) -> Result<DependencyReport, String> {
    let r = s
        .references(&target(kind, name), scan)
        .await
        .map_err(|e| format!("{name}: {e}"))?;
    println!("{name}: {r:#?}");
    Ok(r)
}

fn check(failures: &mut Vec<String>, ok: bool, what: String) {
    if !ok {
        failures.push(what);
    }
}

/// The first value of a query's first row, as text.
async fn first(s: &mut Box<dyn Session>, sql: &str) -> Option<String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.ok()?;
    let v = out.results.last()?.rows.first()?.first()?.clone();
    Some(match v {
        serde_json::Value::String(s) => s,
        v => v.to_string(),
    })
}

/// `names`: the database, the other database, the function, the cluster.
async fn checks(
    d: &dyn dbine_driver::Driver,
    c: &dbine_driver::ConnectionConfig,
    names: &[&str; 4],
    loading: bool,
    failures: &mut Vec<String>,
) {
    if let Err(e) = checks_in(d, c, names, loading, failures).await {
        failures.push(e);
    }
}

async fn checks_in(
    d: &dyn dbine_driver::Driver,
    c: &dbine_driver::ConnectionConfig,
    &[a, b, func, cluster]: &[&str; 4],
    loading: bool,
    f: &mut Vec<String>,
) -> Result<(), String> {
    let mut s = d
        .connect(c, Some(a))
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);

    // A view: one table here, one in the other database (its schema).
    let r = refs(&mut s, &scan, "view", "v").await?;
    let src = item(&r, None, "src");
    check(
        f,
        src.is_some_and(|x| {
            x.kind == "table" && x.confidence == Confidence::Probable && !x.mentions.is_empty()
        }),
        format!("v: src {src:?}"),
    );
    let other = item(&r, Some(b), "other");
    check(
        f,
        other.is_some_and(|x| x.kind == "table"),
        format!("v: {b}.other {other:?}"),
    );
    check(
        f,
        r.items.len() == 2 && r.external.is_empty(),
        format!("v: {r:?}"),
    );

    // A materialized view: its source (dependencies_table) and its TO table.
    let r = refs(&mut s, &scan, "materialized_view", "mv").await?;
    let src = item(&r, None, "src");
    check(
        f,
        src.is_some_and(|x| x.confidence == Confidence::Confirmed),
        format!("mv: src {src:?}"),
    );
    let dst = item(&r, None, "dst");
    check(
        f,
        dst.is_some_and(|x| {
            x.confidence == Confidence::Confirmed && x.detail.as_deref() == Some("TO")
        }),
        format!("mv: dst {dst:?}"),
    );

    // A dictionary's ClickHouse source, and the dictionary a default reads.
    let r = refs(&mut s, &scan, "dictionary", "dict").await?;
    let src = item(&r, None, "src");
    check(
        f,
        src.is_some_and(|x| x.confidence == Confidence::Confirmed),
        format!("dict: src {src:?}"),
    );
    let r = refs(&mut s, &scan, "table", "withdict").await?;
    let dict = item(&r, None, "dict");
    check(
        f,
        !loading
            || dict
                .is_some_and(|x| x.kind == "dictionary" && x.confidence == Confidence::Confirmed),
        format!("withdict: dict {dict:?}"),
    );

    // Engines over other tables of the server.
    let distributed = format!("Distributed('{cluster}')");
    for (name, detail) in [
        ("dist", distributed.as_str()),
        ("mrg", "Merge('^src$')"),
        ("buf", "Buffer"),
    ] {
        let r = refs(&mut s, &scan, "table", name).await?;
        let src = item(&r, None, "src");
        check(
            f,
            src.is_some_and(|x| {
                x.confidence == Confidence::Confirmed
                    && x.relation == Relation::Code
                    && x.detail.as_deref() == Some(detail)
            }),
            format!("{name}: src {src:?}"),
        );
        check(f, r.items.len() == 1, format!("{name}: {r:?}"));
    }

    // Sources outside: a MySQL table, a view on remote().
    let r = refs(&mut s, &scan, "table", "my").await?;
    let x = r.external.first();
    check(
        f,
        r.items.is_empty()
            && x.is_some_and(|x| {
                x.system.as_deref() == Some("MySQL")
                    && x.server.as_deref() == Some("mysql.example:3306")
                    && x.database.as_deref() == Some("erp")
                    && x.name.as_deref() == Some("clients")
            }),
        format!("my: {r:?}"),
    );
    check(
        f,
        !format!("{r:?}").contains("not-a-real-password"),
        "my: the password shows".into(),
    );
    let r = refs(&mut s, &scan, "view", "vremote").await?;
    let x = r.external.first();
    check(
        f,
        x.is_some_and(|x| {
            x.system.as_deref() == Some("ClickHouse")
                && x.server.as_deref() == Some("127.0.0.1:9000")
                && x.database.as_deref() == Some(b)
                && x.name.as_deref() == Some("other")
                && !x.mentions.is_empty()
        }),
        format!("vremote: {r:?}"),
    );
    check(
        f,
        !format!("{r:?}").contains("[HIDDEN]') "),
        "vremote".into(),
    );

    // A table that uses nothing; a SQL function (the default: its body).
    let r = refs(&mut s, &scan, "table", "src").await?;
    check(
        f,
        r.items.is_empty() && r.external.is_empty(),
        format!("src: {r:?}"),
    );
    refs(&mut s, &scan, "function", func).await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn references_timeplus() {
    let Some(c) = cfg_from("DBINE_TEST_TIMEPLUS_URL", "timeplus") else {
        eprintln!("DBINE_TEST_TIMEPLUS_URL not set: skipped");
        return;
    };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "timeplus")
        .unwrap();
    // The stock image's `default` user may only use the `default` database.
    let mut s = d.connect(&c, Some("default")).await.unwrap();
    let p = format!("dbine_refs_{}", std::process::id());
    let mut f: Vec<String> = Vec::new();
    let setup = format!(
        "CREATE STREAM {p}_s (id uint64);
         CREATE STREAM {p}_t (id uint64);
         CREATE VIEW {p}_v AS SELECT id FROM {p}_s;
         CREATE MATERIALIZED VIEW {p}_mv INTO {p}_t AS SELECT id FROM {p}_s"
    );
    if let Err(e) = run(&mut s, &setup).await {
        f.push(e);
    } else {
        let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
        let src = format!("{p}_s");
        let dst = format!("{p}_t");
        let v = format!("{p}_v");
        let mv = format!("{p}_mv");
        match refs(&mut s, &scan, "view", &v).await {
            Ok(r) => {
                let x = item(&r, None, &src);
                check(
                    &mut f,
                    x.is_some_and(|x| {
                        x.kind == "stream"
                            && x.confidence == Confidence::Confirmed
                            && !x.mentions.is_empty()
                    }),
                    format!("{v}: {r:?}"),
                );
            }
            Err(e) => f.push(e),
        }
        match refs(&mut s, &scan, "materialized_view", &mv).await {
            Ok(r) => {
                let a = item(&r, None, &src);
                let b = item(&r, None, &dst);
                check(
                    &mut f,
                    a.is_some_and(|x| x.confidence == Confidence::Confirmed)
                        && b.is_some_and(|x| {
                            x.confidence == Confidence::Confirmed
                                && x.detail.as_deref() == Some("INTO")
                        }),
                    format!("{mv}: {r:?}"),
                );
            }
            Err(e) => f.push(e),
        }
    }
    for sql in [
        format!("DROP VIEW IF EXISTS {p}_mv"),
        format!("DROP VIEW IF EXISTS {p}_v"),
        format!("DROP STREAM IF EXISTS {p}_t"),
        format!("DROP STREAM IF EXISTS {p}_s"),
    ] {
        if let Err(e) = run(&mut s, &sql).await {
            f.push(format!("cleanup: {e}"));
        }
    }
    assert!(f.is_empty(), "{}", f.join("\n"));
}
