//! The data grid's column sort against real servers (same containers and
//! variables as `integration.rs`):
//! `DBINE_TEST_INFLUXDB_URL=http://localhost:25403 DBINE_TEST_INFLUXDB1_URL=http://localhost:25404 DBINE_TEST_INFLUXDB3_URL=http://localhost:25409 cargo test -p dbine-driver-influxdb --test sort -- --ignored`.

use dbine_driver::{
    kinds, ColumnFilter, ConnectionConfig, Driver, Error, FilterOp, ObjectRef, QueryOutcome,
    Session, SortKey,
};
use serde_json::{json, Value};
use std::sync::Arc;

/// Host `a` has values 3, 1, 2 in time order; host `b` one point.
const LINES: &str = "dbine_sort,host=a value=3 1706708700\n\
dbine_sort,host=a value=1 1706708760\n\
dbine_sort,host=b value=9 1706708820\n\
dbine_sort,host=a value=2 1706708880";

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_influxdb::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

fn k(c: &str, descending: bool) -> SortKey {
    SortKey {
        column: c.into(),
        descending,
    }
}

/// The browse query of `dbine_sort` filtered to host `a` and sorted, run:
/// the `value` column in the order it came back.
async fn values(
    d: &Arc<dyn Driver>,
    s: &mut Box<dyn Session>,
    sort: &[SortKey],
) -> Result<Vec<Value>, Error> {
    let obj = ObjectRef {
        kind: kinds::MEASUREMENT.into(),
        schema: None,
        name: "dbine_sort".into(),
    };
    let browse = s.browse_query(&obj, 200);
    let filtered = d.filtered_browse(
        &browse,
        &[ColumnFilter {
            column: "host".into(),
            op: FilterOp::Eq,
            values: vec![json!("a")],
            sql: None,
        }],
    )?;
    let q = d.sorted_browse(&filtered, sort)?;
    println!("{q}");
    let mut out = QueryOutcome::default();
    s.execute(&q, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    let r = out.results.last().unwrap();
    let i = r.columns.iter().position(|c| c.name == "value").unwrap();
    Ok(r.rows.iter().map(|row| row[i].clone()).collect())
}

fn num(v: &[Value]) -> Vec<f64> {
    v.iter()
        .map(|x| {
            x.as_f64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
                .unwrap()
        })
        .collect()
}

#[tokio::test]
#[ignore]
async fn influxdb1_sorts_by_time_only() {
    let url = std::env::var("DBINE_TEST_INFLUXDB1_URL").unwrap();
    let cfg = ConnectionConfig {
        driver: "influxdb1".into(),
        host: url.clone(),
        ..Default::default()
    };
    let d = driver("influxdb1");
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP DATABASE dbine_sort; CREATE DATABASE dbine_sort",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let r = reqwest::Client::new()
        .post(format!("{url}/write?db=dbine_sort&precision=s"))
        .body(LINES)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let mut s = d.connect(&cfg, Some("dbine_sort")).await.unwrap();
    assert_eq!(
        num(&values(&d, &mut s, &[k("time", false)]).await.unwrap()),
        [3.0, 1.0, 2.0]
    );
    assert_eq!(
        num(&values(&d, &mut s, &[k("time", true)]).await.unwrap()),
        [2.0, 1.0, 3.0]
    );
    assert!(matches!(
        values(&d, &mut s, &[k("value", true)]).await,
        Err(Error::Unsupported(_))
    ));
}

#[tokio::test]
#[ignore]
async fn influxdb2_sorts_with_flux() {
    let url = std::env::var("DBINE_TEST_INFLUXDB_URL").unwrap();
    let org = std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into());
    let token = std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into());
    let r = reqwest::Client::new()
        .post(format!(
            "{url}/api/v2/write?org={org}&bucket=test&precision=s"
        ))
        .header("Authorization", format!("Token {token}"))
        .body(LINES)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let mut cfg = ConnectionConfig {
        driver: "influxdb".into(),
        host: url,
        ..Default::default()
    };
    cfg.options.insert("org".into(), org);
    cfg.options.insert("token".into(), token);
    let d = driver("influxdb");
    let mut s = d.connect(&cfg, Some("test")).await.unwrap();
    assert_eq!(
        num(&values(&d, &mut s, &[k("value", true)]).await.unwrap()),
        [3.0, 2.0, 1.0]
    );
    assert_eq!(
        num(&values(&d, &mut s, &[k("value", false)]).await.unwrap()),
        [1.0, 2.0, 3.0]
    );
    assert_eq!(
        num(&values(&d, &mut s, &[k("_time", true)]).await.unwrap()),
        [2.0, 1.0, 3.0]
    );
    assert!(matches!(
        values(&d, &mut s, &[k("host", false), k("value", true)]).await,
        Err(Error::Unsupported(_))
    ));
}

#[tokio::test]
#[ignore]
async fn influxdb3_sorts_with_sql() {
    let url = std::env::var("DBINE_TEST_INFLUXDB3_URL").unwrap();
    let r = reqwest::Client::new()
        .post(format!(
            "{url}/api/v3/write_lp?db=dbine_sort&precision=second"
        ))
        .body(LINES)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{:?}", r.text().await);
    let cfg = ConnectionConfig {
        driver: "influxdb3".into(),
        host: url,
        ..Default::default()
    };
    let d = driver("influxdb3");
    let mut s = d.connect(&cfg, Some("dbine_sort")).await.unwrap();
    assert_eq!(
        num(&values(&d, &mut s, &[k("value", true)]).await.unwrap()),
        [3.0, 2.0, 1.0]
    );
    assert_eq!(
        num(&values(&d, &mut s, &[k("host", false), k("time", false)])
            .await
            .unwrap()),
        [3.0, 1.0, 2.0]
    );
}
