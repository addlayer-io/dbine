//! Row counts, the health check and the process's properties against a
//! real kdb+ (`dbine-test-kdb`, see `live.rs`):
//!
//! ```sh
//! DBINE_TEST_KDB_URL=127.0.0.1:15000:dbine:dbinepass \
//!   cargo test -p dbine-driver-kdb --test live_meta -- --ignored --test-threads=1
//! ```
//!
//! They create objects named `dbine_meta_*` (and the `.dbine_meta`
//! namespace) and delete them at the end. A setting they change goes back
//! to its value.

use dbine_driver::health::Severity;
use dbine_driver::{kinds, ConnectionConfig, QueryOutcome, Session};
use std::collections::BTreeMap;

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

#[tokio::test]
#[ignore]
async fn the_health_check_reads_the_process() {
    let (mut w, mut ro) = sessions().await;
    for s in [&mut w, &mut ro] {
        let c = s.health_checks("default").await.unwrap();
        let ids: Vec<&str> = c.iter().map(|x| x.id.as_str()).collect();
        for id in [
            "workspace_limit",
            "idle_heap",
            "symbols",
            "slow_handles",
            "query_timeout",
            "authentication",
        ] {
            assert!(ids.contains(&id), "{id} not in {ids:?}");
        }
        // The test server takes users from a file (-u) and has reval.
        let auth = c.iter().find(|x| x.id == "authentication").unwrap();
        assert_eq!(auth.severity, Severity::Ok, "{auth:?}");
        assert!(auth.title.contains("-u"), "{auth:?}");
        assert!(!ids.contains(&"reval"));
        let handles = c.iter().find(|x| x.id == "slow_handles").unwrap();
        assert!(handles.title.contains("conexiones abiertas"), "{handles:?}");
    }
}

#[tokio::test]
#[ignore]
async fn properties_change_and_read_back() {
    let d = dbine_driver_kdb::drivers().remove(0);
    assert!(d.capabilities().database_properties);
    let (mut w, mut ro) = sessions().await;
    let p = w.database_properties("default").await.unwrap();
    let keys: Vec<&str> = p.fields.iter().map(|f| f.key).collect();
    // Started without -w: its limit isn't offered.
    assert_eq!(keys, ["g", "T", "P", "o", "z"], "{keys:?}");
    assert!(p
        .info
        .iter()
        .any(|i| i.label == "Versión" && i.value.starts_with("kdb+ ")));
    assert!(p
        .info
        .iter()
        .any(|i| i.label == "Puerto" && !i.value.is_empty()));
    assert_eq!(
        p.values["g"],
        value(&mut w, "\\g").await.to_string(),
        "{:?}",
        p.values
    );
    // The same on a read-only connection, which can't change them.
    let pro = ro.database_properties("default").await.unwrap();
    assert_eq!(pro.values, p.values);
    let change =
        |k: &str, v: &str| -> BTreeMap<String, String> { [(k.to_string(), v.to_string())].into() };
    let e = ro
        .alter_database("default", &change("g", "1"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("solo lectura"), "{e}");

    // Each change and back, read again from the server.
    let (g, t) = (p.values["g"].clone(), p.values["T"].clone());
    let other_g = if g == "0" { "1" } else { "0" };
    let both: BTreeMap<String, String> = [
        ("g".to_string(), other_g.to_string()),
        ("T".to_string(), "3600".to_string()),
    ]
    .into();
    assert_eq!(
        d.alter_database_script("default", &both).unwrap(),
        format!("\\T 3600\n\\g {other_g}")
    );
    w.alter_database("default", &both).await.unwrap();
    let now = w.database_properties("default").await.unwrap().values;
    let back: BTreeMap<String, String> =
        [("g".to_string(), g.clone()), ("T".to_string(), t.clone())].into();
    w.alter_database("default", &back).await.unwrap();
    assert_eq!((now["g"].as_str(), now["T"].as_str()), (other_g, "3600"));
    let again = w.database_properties("default").await.unwrap().values;
    assert_eq!((again["g"].clone(), again["T"].clone()), (g, t));

    // A value out of range never reaches the server; `\w` without -w is
    // the server's refusal.
    assert!(w
        .alter_database("default", &change("P", "99"))
        .await
        .is_err());
    let e = w
        .alter_database("default", &change("w", "100000"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("-w"), "{e}");
}
