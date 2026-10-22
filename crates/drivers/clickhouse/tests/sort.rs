//! Column sort of the data grid against a real server: the browse query,
//! filtered (`filtered_browse`) and then sorted (`sorted_browse`) with two
//! keys, one of them descending and on a column whose name needs quoting
//! (backticks). Skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test sort -- --ignored
//! ```

use dbine_driver::{
    ColumnFilter, ConnectionConfig, Driver, FilterOp, ObjectRef, QueryOutcome, Session, SortKey,
};

fn cfg(env: &str, id: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: id.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    if let Some(e) = out.error.take() {
        panic!("{sql}: {e}");
    }
    out
}

fn ids(out: &QueryOutcome) -> Vec<String> {
    let r = out
        .results
        .iter()
        .find(|r| !r.columns.is_empty())
        .expect("a result set");
    let i = r
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("id"))
        .expect("the id column");
    r.rows
        .iter()
        .map(|row| match &row[i] {
            serde_json::Value::String(s) => s.clone(),
            v => v.to_string(),
        })
        .collect()
}

/// Rows (id, "Grupo Mixto", "Valor"): (1, b, 10), (2, a, 5), (3, b, 30),
/// (4, a, 20), (5, c, 1), (6, a, 15). Filtered on Valor >= 5 and sorted on
/// "Grupo Mixto" ASC, Valor DESC; the limit applies after the sort.
async fn check(
    d: &dyn Driver,
    s: &mut Box<dyn Session>,
    obj: &ObjectRef,
    group: &str,
    value: &str,
) {
    let filter = [ColumnFilter {
        column: value.into(),
        op: FilterOp::Ge,
        values: vec![serde_json::json!(5)],
        sql: None,
    }];
    let sort = [
        SortKey {
            column: group.into(),
            descending: false,
        },
        SortKey {
            column: value.into(),
            descending: true,
        },
    ];
    for (limit, want) in [
        (100, vec!["4", "6", "2", "3", "1"]),
        (3, vec!["4", "6", "2"]),
    ] {
        let q = d
            .filtered_browse(&s.browse_query(obj, limit), &filter)
            .unwrap();
        let q = d.sorted_browse(&q, &sort).unwrap();
        assert_eq!(ids(&run(s, &q).await), want, "{q}");
    }
    let sort = [
        SortKey {
            column: group.into(),
            descending: true,
        },
        SortKey {
            column: "id".into(),
            descending: false,
        },
    ];
    let q = d.sorted_browse(&s.browse_query(obj, 100), &sort).unwrap();
    assert_eq!(
        ids(&run(s, &q).await),
        ["5", "1", "3", "2", "4", "6"],
        "{q}"
    );
}

#[tokio::test]
#[ignore]
async fn clickhouse_sorts_the_filtered_browse() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse") else {
        return;
    };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "CREATE DATABASE IF NOT EXISTS dbine_sort").await;
    run(&mut s, "DROP TABLE IF EXISTS dbine_sort.t").await;
    run(&mut s, "CREATE TABLE dbine_sort.t (id Int32, `Grupo Mixto` String, `Valor` Int32, `a\\`b\\\\` Int32 DEFAULT -id) ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "INSERT INTO dbine_sort.t (id, `Grupo Mixto`, `Valor`) VALUES (1, 'b', 10), (2, 'a', 5), (3, 'b', 30), (4, 'a', 20), (5, 'c', 1), (6, 'a', 15)").await;
    let obj = ObjectRef {
        kind: "table".into(),
        schema: Some("dbine_sort".into()),
        name: "t".into(),
    };
    check(d.as_ref(), &mut s, &obj, "Grupo Mixto", "Valor").await;
    // A name with a backtick and a trailing backslash, escaped ClickHouse's way.
    let q = d
        .sorted_browse(
            &s.browse_query(&obj, 100),
            &[SortKey {
                column: "a`b\\".into(),
                descending: false,
            }],
        )
        .unwrap();
    assert_eq!(
        ids(&run(&mut s, &q).await),
        ["6", "5", "4", "3", "2", "1"],
        "{q}"
    );
    // A filter with a quote: the app sorts the loaded rows instead.
    let f = [ColumnFilter {
        column: "Grupo Mixto".into(),
        op: FilterOp::Eq,
        values: vec![serde_json::json!("O'Brien")],
        sql: None,
    }];
    let q = d.filtered_browse(&s.browse_query(&obj, 100), &f).unwrap();
    assert!(matches!(
        d.sorted_browse(
            &q,
            &[SortKey {
                column: "id".into(),
                descending: false
            }]
        ),
        Err(dbine_driver::Error::Unsupported(_))
    ));
    run(&mut s, "DROP DATABASE dbine_sort").await;
}
