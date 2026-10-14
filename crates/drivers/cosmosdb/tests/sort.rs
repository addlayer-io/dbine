//! The data grid's column sort against the emulator (ignored by default),
//! the container of `integration.rs`:
//!
//! ```sh
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25203 cargo test -p dbine-driver-cosmosdb --test sort -- --ignored
//! ```

use dbine_driver::{ColumnFilter, ConnectionConfig, FilterOp, ObjectRef, QueryOutcome, Session, SortKey};
use dbine_driver_cosmosdb::auth_header;
use serde_json::{json, Value};

const EMULATOR_KEY: &str = "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";

#[allow(clippy::too_many_arguments)]
async fn rest(url: &str, key: &str, method: &str, rtype: &str, link: &str, path: &str, body: Option<Value>, pk: Option<&str>) -> u16 {
    let http = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let mut rq = http
        .request(method.parse().unwrap(), format!("{url}{path}"))
        .header("Authorization", auth_header(key, method, rtype, link, &date).unwrap())
        .header("x-ms-date", date)
        .header("x-ms-version", "2018-12-31");
    if let Some(pk) = pk {
        rq = rq.header("x-ms-documentdb-partitionkey", format!("[\"{pk}\"]"));
    }
    if let Some(b) = body {
        rq = rq.header("Content-Type", "application/json").body(b.to_string());
    }
    rq.send().await.unwrap().status().as_u16()
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{q}"));
    out
}

fn ids(out: &QueryOutcome) -> Vec<Value> {
    let r = out.results.last().unwrap();
    let i = r.columns.iter().position(|c| c.name == "id").unwrap();
    r.rows.iter().map(|row| row[i].clone()).collect()
}

#[tokio::test]
#[ignore]
async fn cosmosdb_sorts_on_the_server() {
    let Ok(url) = std::env::var("DBINE_TEST_COSMOSDB_URL") else { return };
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    let url = url.trim_end_matches('/').to_string();
    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_sort", "/dbs/dbine_sort", None, None).await;
    assert_eq!(rest(&url, &key, "POST", "dbs", "", "/dbs", Some(json!({ "id": "dbine_sort" })), None).await, 201);
    let body = json!({ "id": "items", "partitionKey": { "paths": ["/cat"], "kind": "Hash" } });
    assert_eq!(rest(&url, &key, "POST", "colls", "dbs/dbine_sort", "/dbs/dbine_sort/colls", Some(body), None).await, 201);
    // Two partitions, so the ORDER BY merges across them.
    let docs = [
        json!({ "id": "1", "cat": "x", "g": "a", "n": 5, "we\"ird": 3 }),
        json!({ "id": "2", "cat": "y", "g": "a", "n": 2, "we\"ird": 1 }),
        json!({ "id": "3", "cat": "x", "g": "b", "n": 9, "we\"ird": 2 }),
        json!({ "id": "4", "cat": "y", "g": "a", "n": 4, "we\"ird": 2.5 }),
        json!({ "id": "5", "cat": "x", "g": "a", "n": 7, "we\"ird": 0 }),
    ];
    for d in docs {
        let pk = d["cat"].as_str().unwrap().to_string();
        let st = rest(&url, &key, "POST", "docs", "dbs/dbine_sort/colls/items", "/dbs/dbine_sort/colls/items/docs", Some(d), Some(&pk)).await;
        assert_eq!(st, 201);
    }
    let d = &dbine_driver_cosmosdb::drivers()[0];
    let mut c = ConnectionConfig { driver: "cosmosdb".into(), host: url.clone(), database: "dbine_sort".into(), trust_server_certificate: true, ..Default::default() };
    c.options.insert("account_key".into(), key.clone());
    let mut s = d.connect(&c, None).await.expect("connect");
    let obj = ObjectRef { kind: "collection".into(), schema: None, name: "items".into() };
    let browse = s.browse_query(&obj, 50);
    let filtered = d.filtered_browse(&browse, &[ColumnFilter { column: "g".into(), op: FilterOp::Eq, values: vec![json!("a")], sql: None }]).unwrap();
    let k = |c: &str, desc: bool| SortKey { column: c.into(), descending: desc };
    let want = |v: &[u8]| v.iter().map(|n| json!(n.to_string())).collect::<Vec<_>>();
    let q = d.sorted_browse(&filtered, &[k("n", true)]).unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), want(&[5, 1, 4, 2]));
    let q = d.sorted_browse(&filtered, &[k("n", false)]).unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), want(&[2, 4, 1, 5]));
    let q = d.sorted_browse(&filtered, &[k("we\"ird", false)]).unwrap();
    assert_eq!(ids(&run(&mut s, &q).await), want(&[5, 2, 4, 1]));
    rest(&url, &key, "DELETE", "dbs", "dbs/dbine_sort", "/dbs/dbine_sort", None, None).await;
}
