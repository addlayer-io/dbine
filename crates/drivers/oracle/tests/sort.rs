//! Column sort of the data grid against a real server: the browse query
//! (`FETCH FIRST n ROWS ONLY`), filtered (`filtered_browse`) and then
//! sorted (`sorted_browse`) with two keys, one of them descending and on a
//! column whose name needs quoting. Skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_ORACLE_URL=oracle://dbine:Dbine123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test sort -- --ignored
//! ```

use dbine_driver::{
    ColumnFilter, ConnectionConfig, FilterOp, ObjectRef, QueryOutcome, Session, SortKey,
};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ORACLE_URL").ok()?;
    let rest = url.strip_prefix("oracle://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, service) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    Some(cfg)
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
        .position(|c| c.name == "id")
        .expect("the id column");
    r.rows
        .iter()
        .map(|row| match &row[i] {
            serde_json::Value::String(s) => s.clone(),
            v => v.to_string(),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn oracle_sorts_the_filtered_browse() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "DROP TABLE IF EXISTS DBINE_SORT").await;
    run(&mut s, "CREATE TABLE DBINE_SORT (\"id\" NUMBER(10) PRIMARY KEY, \"Grupo Mixto\" VARCHAR2(10), \"Valor\" NUMBER(10))").await;
    run(&mut s, "INSERT INTO DBINE_SORT VALUES (1, 'b', 10), (2, 'a', 5), (3, 'b', 30), (4, 'a', 20), (5, 'c', 1), (6, 'a', 15)").await;
    let obj = ObjectRef {
        kind: "table".into(),
        schema: None,
        name: "DBINE_SORT".into(),
    };
    let filter = [ColumnFilter {
        column: "Valor".into(),
        op: FilterOp::Ge,
        values: vec![serde_json::json!(5)],
        sql: None,
    }];
    let sort = [
        SortKey {
            column: "Grupo Mixto".into(),
            descending: false,
        },
        SortKey {
            column: "Valor".into(),
            descending: true,
        },
    ];
    // FETCH FIRST 3 takes the first rows of the ORDER BY.
    for (limit, want) in [
        (100, vec!["4", "6", "2", "3", "1"]),
        (3, vec!["4", "6", "2"]),
    ] {
        let q = d
            .filtered_browse(&s.browse_query(&obj, limit), &filter)
            .unwrap();
        let q = d.sorted_browse(&q, &sort).unwrap();
        assert_eq!(ids(&run(&mut s, &q).await), want, "{q}");
    }
    let q = d
        .sorted_browse(
            &s.browse_query(&obj, 100),
            &[
                SortKey {
                    column: "Grupo Mixto".into(),
                    descending: true,
                },
                SortKey {
                    column: "id".into(),
                    descending: false,
                },
            ],
        )
        .unwrap();
    assert_eq!(
        ids(&run(&mut s, &q).await),
        ["5", "1", "3", "2", "4", "6"],
        "{q}"
    );
    run(&mut s, "DROP TABLE DBINE_SORT PURGE").await;
}
