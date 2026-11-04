//! Row counts against a real kdb+ (`dbine-test-kdb`, see `live.rs`):
//!
//! ```sh
//! DBINE_TEST_KDB_URL=127.0.0.1:15000:dbine:dbinepass \
//!   cargo test -p dbine-driver-kdb --test live_meta -- --ignored --test-threads=1
//! ```
//!
//! They create objects named `dbine_meta_*` (and the `.dbine_meta`
//! namespace) and delete them at the end. A setting they change goes back
//! to its value.

use dbine_driver::{kinds, ConnectionConfig, QueryOutcome, Session};

/// `host:port[:user:password]` from `DBINE_TEST_KDB_URL`.
fn cfg(read_only: bool) -> ConnectionConfig {
    let url =
        std::env::var("DBINE_TEST_KDB_URL").expect("DBINE_TEST_KDB_URL=host:port[:user:password]");
    let mut parts = url.splitn(4, ':');
    let host = parts.next().unwrap_or("127.0.0.1").to_string();
    let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(5000);
    ConnectionConfig {
        host,
        port,
        username: parts.next().map(str::to_string),
        password: parts.next().map(str::to_string),
        read_only,
        options: [("tcps".to_string(), "false".to_string())].into(),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(q, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    out
}

/// The first cell of `q`'s result.
async fn value(s: &mut Box<dyn Session>, q: &str) -> serde_json::Value {
    run(s, q).await.results[0].rows[0][0].clone()
}

async fn sessions() -> (Box<dyn Session>, Box<dyn Session>) {
    let d = dbine_driver_kdb::drivers().remove(0);
    (
        d.connect(&cfg(false), None).await.unwrap(),
        d.connect(&cfg(true), None).await.unwrap(),
    )
}

#[tokio::test]
#[ignore]
async fn row_counts_without_scanning() {
    let (mut w, mut ro) = sessions().await;
    run(
        &mut w,
        "dbine_meta_t:([] a:til 7); .dbine_meta.k:([id:1 2 3] v:`a`b`c)",
    )
    .await;
    // A partitioned table counted already (`.Q.pn` filled) has its count.
    let objects = w.list_objects().await.unwrap();
    let hdb = objects
        .iter()
        .find(|o| o.kind == dbine_driver_kdb::PARTITIONED)
        .map(|o| o.name.clone());
    let counted = match &hdb {
        Some(t) => value(&mut w, &format!("count {t}")).await.as_u64(),
        None => None,
    };
    for s in [&mut w, &mut ro] {
        let e = s.row_estimates().await.unwrap();
        let rows = |n: &str| e.iter().find(|x| x.object.name == n).map(|x| x.rows);
        assert_eq!(rows("dbine_meta_t"), Some(7), "{e:?}");
        assert_eq!(rows(".dbine_meta.k"), Some(3), "{e:?}");
        let k = e.iter().find(|x| x.object.name == ".dbine_meta.k").unwrap();
        assert_eq!(k.object.kind, kinds::TABLE);
        if let (Some(t), Some(n)) = (&hdb, counted) {
            assert_eq!(rows(t), Some(n), "{e:?}");
        }
        // Views and variables aren't counted.
        assert!(e.iter().all(|x| x.object.kind != kinds::VIEW));
    }
    run(
        &mut w,
        "delete dbine_meta_t from `.; ![`;();0b;enlist `dbine_meta]",
    )
    .await;
}
