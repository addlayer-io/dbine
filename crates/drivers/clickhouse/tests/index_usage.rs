//! "Índices" against a real ClickHouse server: a MergeTree table with a
//! sorting key and two skip indexes, one of them hit by a few targeted
//! queries. ClickHouse has no per-index counters (nor foreign keys): the
//! report lists the key and the indexes with their sizes, zeros and a note,
//! and the drop script "Eliminar índice" generates removes the index.
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Some(mut c) = cfg() else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().remove(0);
    assert!(d.supports_index_usage());
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_ixu").await;
    drop(s);
    c.database = "dbine_ixu".into();
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "DROP TABLE IF EXISTS ixu_t").await;
    run(
        &mut s,
        "CREATE TABLE ixu_t (id UInt32, a UInt32, b String,
             INDEX ixu_a a TYPE minmax GRANULARITY 1,
             INDEX ixu_b b TYPE bloom_filter GRANULARITY 2)
         ENGINE = MergeTree ORDER BY id SETTINGS index_granularity = 64",
    )
    .await;
    run(&mut s, "INSERT INTO ixu_t SELECT number, number * 3, toString(number) FROM numbers(5000)").await;
    for i in 0..5 {
        run(&mut s, &format!("SELECT count() FROM ixu_t WHERE a = {}", i * 300)).await;
    }
    let t = ObjectRef { kind: "table".into(), schema: Some("dbine_ixu".into()), name: "ixu_t".into() };
    let r = s.index_usage(&t).await.unwrap().expect("report");
    eprintln!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some() && r.foreign_keys.is_empty());
    let got: Vec<(&str, &str)> = r.indexes.iter().map(|i| (i.name.as_str(), i.kind.as_str())).collect();
    assert_eq!(got, [("PRIMARY KEY", "SPARSE"), ("ixu_a", "MINMAX"), ("ixu_b", "BLOOM_FILTER GRANULARITY 2")]);
    assert!(r.indexes[0].primary_key && r.indexes[0].key_columns == ["id"]);
    assert!(r.indexes.iter().all(|i| i.size_kb.is_some()), "sizes");
    assert!(r.indexes.iter().all(|i| i.reads == 0 && !i.unused));

    let old = s.database_schema().await.unwrap().into_iter().find(|x| x.name == "ixu_t").unwrap();
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "ixu_b");
    for st in d.sync_script(&[TableChange::Alter { old, new }]).unwrap().statements {
        run(&mut s, &st).await;
    }
    let r = s.index_usage(&t).await.unwrap().unwrap();
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PRIMARY KEY", "ixu_a"]);
    run(&mut s, "DROP TABLE ixu_t").await;
    run(&mut s, "DROP DATABASE dbine_ixu").await;
}
