//! What depends on an object (`Session::dependents`) against a real
//! ClickHouse server: a table read by a view, a materialized view, a
//! dictionary, a default (`dictGet`), a Buffer and a Distributed table, a
//! view of another database; a column's key and the view that names it; and
//! the native answer compared with the contract's scan. Everything is
//! dropped at the end, also when a check fails.
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test dependents -- --ignored --nocapture
//! ```

use dbine_driver::{
    kinds, Confidence, DependencyReport, DependencyScan, DependencyTarget, ObjectRef, QueryOutcome,
    Relation, Session,
};

fn cfg() -> Option<dbine_driver::ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(dbine_driver::ConnectionConfig {
        driver: "clickhouse".into(),
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

fn target(kind: &str, name: &str, column: Option<&str>) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        },
        column: column.map(Into::into),
    }
}

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
async fn dependents_live() {
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
    let (a, b) = (format!("dbine_dep_{tag}"), format!("dbine_dep_x_{tag}"));
    let mut admin = d.connect(&c, None).await.unwrap();
    let setup = format!(
        "CREATE DATABASE {a};
         CREATE DATABASE {b};
         CREATE TABLE {a}.src (id UInt64, v String) ENGINE = MergeTree ORDER BY id;
         CREATE TABLE {a}.dst (id UInt64, v String) ENGINE = MergeTree ORDER BY id;
         CREATE TABLE {a}.src_old (id UInt64) ENGINE = MergeTree ORDER BY id;
         CREATE VIEW {a}.vw AS SELECT id FROM {a}.src;
         CREATE VIEW {a}.vcol AS SELECT id, v FROM {a}.src;
         CREATE VIEW {a}.vlong AS SELECT id FROM {a}.src_old;
         CREATE VIEW {b}.vfar AS SELECT id FROM {a}.src;
         CREATE MATERIALIZED VIEW {a}.mv TO {a}.dst AS SELECT id, v FROM {a}.src;
         CREATE VIEW {a}.vdst AS SELECT id FROM {a}.dst;
         CREATE DICTIONARY {a}.dict (id UInt64, v String) PRIMARY KEY id
           SOURCE(CLICKHOUSE(TABLE 'src' DB '{a}')) LAYOUT(FLAT()) LIFETIME(0);
         CREATE TABLE {a}.withdict (id UInt64, v String DEFAULT dictGet('{a}.dict', 'v', id))
           ENGINE = MergeTree ORDER BY id;
         CREATE TABLE {a}.buf AS {a}.src ENGINE = Buffer('{a}', 'src', 1, 10, 100, 10000, 1000000, 10000000, 100000000);
         CREATE TABLE {a}.dist AS {a}.src ENGINE = Distributed('default', '{a}', 'src', rand());
         CREATE TABLE {a}.unrelated (id UInt64) ENGINE = MergeTree ORDER BY id"
    );
    let mut failures: Vec<String> = Vec::new();
    match run(&mut admin, &setup).await {
        Err(e) => failures.push(format!("setup: {e}")),
        Ok(()) => {
            if let Err(e) = checks(&*d, &c, &a, &b).await {
                failures.push(e);
            }
        }
    }
    for sql in [
        format!("DROP DATABASE IF EXISTS {a} SYNC"),
        format!("DROP DATABASE IF EXISTS {b} SYNC"),
    ] {
        if let Err(e) = run(&mut admin, &sql).await {
            failures.push(format!("cleanup: {e}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

async fn checks(
    d: &dyn dbine_driver::Driver,
    c: &dbine_driver::ConnectionConfig,
    a: &str,
    b: &str,
) -> Result<(), String> {
    let mut s = d.connect(c, Some(a)).await.map_err(|e| e.to_string())?;
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let err = |n: &'static str| move |e: dbine_driver::Error| format!("{n}: {e}");

    let r = s
        .dependents(&target(kinds::TABLE, "src", None), &scan)
        .await
        .map_err(err("src"))?;
    println!("src: {r:#?}");
    let mut bad = Vec::new();
    let mut want =
        |what: &str, got: Option<(&str, Confidence)>, expect: Option<(&str, Confidence)>| {
            if got != expect {
                bad.push(format!("{what}: {got:?}, expected {expect:?}"));
            }
        };
    let how = |s: Option<&str>, n: &str| item(&r, s, n).map(|d| (d.kind.as_str(), d.confidence));
    want(
        "vw",
        how(None, "vw"),
        Some((kinds::VIEW, Confidence::Probable)),
    );
    want(
        "vcol",
        how(None, "vcol"),
        Some((kinds::VIEW, Confidence::Probable)),
    );
    want(
        "mv",
        how(None, "mv"),
        Some((kinds::MATERIALIZED_VIEW, Confidence::Confirmed)),
    );
    want(
        "vfar",
        how(Some(b), "vfar"),
        Some((kinds::VIEW, Confidence::Probable)),
    );
    want(
        "buf",
        how(None, "buf"),
        Some((kinds::TABLE, Confidence::Confirmed)),
    );
    want(
        "dist",
        how(None, "dist"),
        Some((kinds::TABLE, Confidence::Confirmed)),
    );
    // ClickHouse lists only materialized views in `dependencies_*`: a plain
    // view is read from its text (probable).
    // What doesn't read `src`: a longer name, the view's other table, an
    // unrelated table, the target itself and what only the dictionary feeds.
    for n in [
        "vlong",
        "vdst",
        "unrelated",
        "dst",
        "src",
        "src_old",
        "withdict",
    ] {
        want(n, how(None, n), None);
    }
    // `dictionary` is its own kind in this driver; a server with
    // `loading_dependencies_*` says Confirmed, an old one the text.
    if let Some(dict) = item(&r, None, "dict") {
        if dict.confidence != Confidence::Confirmed {
            bad.push(format!("dict: {:?}", dict.confidence));
        }
    } else {
        bad.push("dict missing".into());
    }
    if !item(&r, None, "vw").is_some_and(|d| !d.mentions.is_empty()) {
        bad.push("vw has no mentions".into());
    }
    if r.note.is_some() {
        bad.push(format!("note: {:?}", r.note));
    }

    // The dictionary's reader: a default's `dictGet` (the catalog, where
    // the server has it, else the text).
    let r2 = s
        .dependents(&target("dictionary", "dict", None), &scan)
        .await
        .map_err(err("dict"))?;
    if item(&r2, None, "withdict").is_none() {
        bad.push(format!("withdict doesn't read dict: {:?}", r2.items));
    }

    // The native answer names what the contract's scan finds in this
    // database (the scan reads this database only and by text).
    let generic =
        dbine_driver::dependencies::scan(&mut *s, &target(kinds::TABLE, "src", None), &scan)
            .await
            .map_err(err("generic"))?;
    for g in &generic.items {
        if item(&r, g.schema.as_deref(), &g.name).is_none() {
            bad.push(format!(
                "the scan finds {} and the native read doesn't",
                g.name
            ));
        }
    }

    // A column: the view that names it, and its key.
    let r3 = s
        .dependents(&target(kinds::TABLE, "src", Some("v")), &scan)
        .await
        .map_err(err("src.v"))?;
    println!("src.v: {r3:#?}");
    if !item(&r3, None, "vcol").is_some_and(|d| d.relation == Relation::Code) {
        bad.push("vcol should name v".into());
    }
    if item(&r3, None, "vw").is_some() {
        bad.push("vw doesn't name v".into());
    }
    let r4 = s
        .dependents(&target(kinds::TABLE, "src", Some("id")), &scan)
        .await
        .map_err(err("src.id"))?;
    if !r4
        .items
        .iter()
        .any(|d| d.relation == Relation::Index && d.detail.as_deref() == Some("ORDER BY"))
    {
        bad.push(format!("id is the sorting key: {:?}", r4.items));
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad.join("\n"))
    }
}
