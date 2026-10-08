//! Row estimates from `SYSTEM.STATS` against a real Phoenix Query Server:
//!
//! ```sh
//! DBINE_TEST_PHOENIX_URL=http://localhost:25165 \
//!   cargo test -p dbine-driver-phoenix --test stats -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_PHOENIX_URL").ok()?).expect("URL");
    Some(ConnectionConfig { driver: "phoenix".into(), host: url.host_str()?.into(), port: url.port().unwrap_or(0), ..Default::default() })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn phoenix_stats() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_PHOENIX_URL not set; skipping");
        return;
    };
    let d = dbine_driver_phoenix::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE IF EXISTS DBINE_ST.VENTAS", 10, &mut out).await;
    run(&mut s, "CREATE SCHEMA IF NOT EXISTS DBINE_ST").await;
    run(&mut s, "CREATE TABLE DBINE_ST.VENTAS (ID INTEGER PRIMARY KEY, V VARCHAR)").await;
    for i in 1..=25 {
        run(&mut s, &format!("UPSERT INTO DBINE_ST.VENTAS VALUES ({i}, 'x')")).await;
    }
    // A guide post every 100 bytes: with the default width (300 MB) a table
    // this small keeps only an empty guide post, with no count.
    run(&mut s, "UPDATE STATISTICS DBINE_ST.VENTAS SET \"phoenix.stats.guidepost.width\" = 100").await;

    let rows = s.row_estimates().await.unwrap();
    eprintln!("{rows:?}");
    let t = rows.iter().find(|r| r.object.name == "VENTAS" && r.object.schema.as_deref() == Some("DBINE_ST")).expect("VENTAS");
    // The rows after the region's last guide post aren't counted.
    assert_eq!(t.object.kind, kinds::TABLE);
    assert!((20..=25).contains(&t.rows), "{t:?}");
    assert!(s.object_comments().await.unwrap().is_empty());
    run(&mut s, "DROP TABLE DBINE_ST.VENTAS").await;
    run(&mut s, "DROP SCHEMA DBINE_ST").await;
}
