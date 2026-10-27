//! Completion across databases (`Driver::cross_database`, issue #20): which
//! engines offer it is fixed here, so a change is deliberate, and every SQL
//! engine with several databases that doesn't is listed, with its reason,
//! in docs/engine-support.md.

use dbine_driver::Language;

/// The engines whose queries read another database of the connection by
/// its name in front, each checked against the engine's documentation (and
/// against a real server where there's a test container: the drivers'
/// `tests/cross_database.rs`).
const OFFERED: &[&str] = &[
    "athena",
    "aurora-mysql",
    "azure_databricks",
    "bigquery",
    "cassandra",
    "clickhouse",
    "cloudsql-mysql",
    "cockroachdb",
    "couchbase",
    "databend",
    "databricks",
    "doris",
    "dremio",
    "duckdb",
    "fabric",
    "greptimedb",
    "hana",
    "keyspaces",
    "mariadb",
    "materialize",
    "mysql",
    "netezza",
    "oceanbase",
    "oracle",
    "oracle_adb",
    "presto",
    "redshift",
    "scylladb",
    "singlestore",
    "snowflake",
    "sqlserver",
    "starburst",
    "starrocks",
    "sybase",
    "tdengine",
    "tidb",
    "timeplus",
    "trino",
    "velodb",
    "virtuoso",
];

#[test]
fn the_engines_that_offer_it_are_the_checked_ones() {
    let mut on: Vec<&str> = dbine_drivers::all()
        .iter()
        .filter(|d| d.cross_database())
        .map(|d| d.info().id)
        .collect();
    on.sort_unstable();
    assert_eq!(on, OFFERED);
}

#[test]
fn engines_without_it_are_documented() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/engine-support.md"
    ))
    .unwrap();
    let section = doc
        .split("## Completion across databases")
        .nth(1)
        .expect("section in engine-support.md");
    let section = section.split("\n## ").next().unwrap_or(section);
    let without = section
        .split("### Engines without it")
        .nth(1)
        .expect("subsection");
    // Names wrap across lines in the doc.
    let without = without.split_whitespace().collect::<Vec<_>>().join(" ");
    let missing: Vec<&str> = dbine_drivers::all()
        .iter()
        .filter(|d| {
            let i = d.info();
            !i.databases_label.is_empty()
                && i.language != Language::Redis
                && !d.cross_database()
                && !without.contains(i.name)
        })
        .map(|d| d.info().name)
        .collect();
    assert!(
        missing.is_empty(),
        "several databases, no completion across them, and not in the doc: {missing:?}"
    );
}
