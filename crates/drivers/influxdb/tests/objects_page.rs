//! The explorer's pages (`list_objects_page`) against real servers, one
//! test per API: for every filter (the folder, "Otros", searches with
//! regex and LIKE characters, capitals and accents, lookups) and page
//! size, each page holds what the app pages out of `list_objects`
//! (`objects::page_of`). Each test writes its measurements into a database
//! (bucket) of its own and drops it at the end, even when a check fails;
//! skipped without its variable:
//!
//! ```sh
//! DBINE_TEST_INFLUXDB_URL=http://localhost:25403 \
//! DBINE_TEST_INFLUXDB1_URL=http://localhost:25404 \
//! DBINE_TEST_INFLUXDB3_URL=http://localhost:25409 \
//!   cargo test -p dbine-driver-influxdb --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{kinds, ConnectionConfig, DbObject, ObjectQuery, Session};
use std::future::Future;

/// Measurement names as line protocol writes them (`\,` is a comma): a
/// folder of more than one page, capitals that tie ignoring case, regex
/// and LIKE characters, other alphabets.
fn names() -> Vec<String> {
    let mut v: Vec<String> = (0..25).map(|i| format!("m_{i:02}")).collect();
    for n in [
        "Orders", "orders", "ORDERS_2", "a%b", "axb", "a_b", "a.b", "a/b", "x\\,y", "Zeta",
        "Ñandú", "ñu", "zürich", "İzmir", "B(1)",
    ] {
        v.push(n.into());
    }
    v
}

fn lines() -> String {
    names()
        .iter()
        .map(|n| format!("{n} v=1i 1706708700"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn queries() -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
        ObjectQuery {
            no_schema: true,
            ..Default::default()
        },
        ObjectQuery {
            schema: Some("x".into()),
            ..Default::default()
        },
        ObjectQuery {
            kind: Some(kinds::MEASUREMENT.into()),
            ..Default::default()
        },
        ObjectQuery {
            kind: Some(kinds::TABLE.into()),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec![kinds::MEASUREMENT.into()],
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec![kinds::TABLE.into()],
            ..Default::default()
        },
        ObjectQuery {
            kind: Some(kinds::MEASUREMENT.into()),
            pattern: Some("1".into()),
            ..Default::default()
        },
        ObjectQuery {
            pattern: Some("orders".into()),
            name: Some("ORDERS".into()),
            ..Default::default()
        },
    ];
    for p in [
        "M_1", "%", "_", "a%b", ".", "/", "(", "ORDERS", "ñ", "Ñ", "Ü", "i", "zzz", ",",
    ] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in [
        "m_05", "orders", "ORDERS", "ñandú", "a.b", "x,y", "nope", "İzmir", "b(1)",
    ] {
        v.push(ObjectQuery {
            name: Some(n.into()),
            ..Default::default()
        });
    }
    v
}

/// Every page of `q` from the driver and out of the whole list: the same
/// objects in the same pages, to the last. Gives how many pages.
async fn same_pages(s: &mut Box<dyn Session>, all: &[DbObject], q: &ObjectQuery) -> usize {
    let mut q = q.clone();
    let mut offset = q.clone();
    let mut pages = 0;
    loop {
        let got = s
            .list_objects_page(&q)
            .await
            .unwrap_or_else(|e| panic!("{q:?}: {e}"));
        let want = page_of(all, &offset);
        assert_eq!(got.objects, want.objects, "{q:?}");
        assert_eq!(got.next.is_some(), want.next.is_some(), "{q:?}");
        pages += 1;
        assert!(pages < 100, "{q:?}: no last page");
        match (got.next, want.next) {
            (Some(a), Some(b)) => {
                q.after = Some(a);
                offset.after = Some(b);
            }
            _ => return pages,
        }
    }
}

async fn check(mut s: Box<dyn Session>) {
    let all = s.list_objects().await.unwrap();
    let want: Vec<String> = names().iter().map(|n| n.replace("\\,", ",")).collect();
    let mut got: Vec<String> = all.iter().map(|o| o.name.clone()).collect();
    got.sort();
    let mut sorted = want.clone();
    sorted.sort();
    assert_eq!(got, sorted);
    let mut checked = 0;
    for q in queries() {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let folder = ObjectQuery {
        kind: Some(kinds::MEASUREMENT.into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &folder).await;
    assert!(pages >= 4, "a folder of more than one page: {pages}");
    eprintln!(
        "{} measurements; {checked} filter/size combinations equal; the folder in {pages} pages of 10",
        all.len()
    );
}

/// Runs `check` on its own task and then `drop`, so the database goes
/// even when a check fails.
async fn checked_then_dropped<F: Future<Output = ()> + Send + 'static>(
    check: F,
    drop: impl Future<Output = ()>,
) {
    let outcome = tokio::spawn(check).await;
    drop.await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

fn unique(prefix: &str) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("{prefix}_{t}")
}

async fn open(driver: &str, cfg: &ConnectionConfig, db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_influxdb::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    d.connect(cfg, db).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb1_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB1_URL not set; skipping");
        return;
    };
    let cfg = ConnectionConfig {
        driver: "influxdb1".into(),
        host: url.clone(),
        ..Default::default()
    };
    let db = unique("dbine_pages");
    let mut admin = open("influxdb1", &cfg, None).await;
    admin.create_database(&db).await.unwrap();
    let r = reqwest::Client::new()
        .post(format!("{url}/write?db={db}&precision=s"))
        .body(lines())
        .send()
        .await
        .unwrap();
    let ok = r.status().is_success();
    let s = open("influxdb1", &cfg, Some(&db)).await;
    checked_then_dropped(
        async move {
            assert!(ok, "write");
            check(s).await
        },
        async {
            admin.drop_database(&db).await.unwrap();
        },
    )
    .await;
}

/// More capitalized names than are read whole: the folder is left to the
/// app, on every page; a search still narrows them and is paged here.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb1_many_capitals_go_to_the_app() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB1_URL not set; skipping");
        return;
    };
    let cfg = ConnectionConfig {
        driver: "influxdb1".into(),
        host: url.clone(),
        ..Default::default()
    };
    let db = unique("dbine_pages_big");
    let mut admin = open("influxdb1", &cfg, None).await;
    admin.create_database(&db).await.unwrap();
    let body: Vec<String> = (0..10_001)
        .map(|i| format!("M{i:05} v=1i 1706708700"))
        .collect();
    let r = reqwest::Client::new()
        .post(format!("{url}/write?db={db}&precision=s"))
        .body(body.join("\n"))
        .send()
        .await
        .unwrap();
    let ok = r.status().is_success();
    let mut s = open("influxdb1", &cfg, Some(&db)).await;
    checked_then_dropped(
        async move {
            assert!(ok, "write");
            let all = s.list_objects().await.unwrap();
            assert_eq!(all.len(), 10_001);
            for after in [None, Some("1000".to_string())] {
                let folder = ObjectQuery {
                    after,
                    ..Default::default()
                };
                assert!(matches!(
                    s.list_objects_page(&folder).await,
                    Err(dbine_driver::Error::Unsupported(_))
                ));
            }
            let search = ObjectQuery {
                pattern: Some("m0999".into()),
                limit: 4,
                ..Default::default()
            };
            let pages = same_pages(&mut s, &all, &search).await;
            assert_eq!(pages, 3, "10 names in pages of 4");
        },
        async {
            admin.drop_database(&db).await.unwrap();
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb2_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB_URL not set; skipping");
        return;
    };
    let org = std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into());
    let token = std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into());
    let mut cfg = ConnectionConfig {
        driver: "influxdb".into(),
        host: url.clone(),
        ..Default::default()
    };
    cfg.options.insert("org".into(), org.clone());
    cfg.options.insert("token".into(), token.clone());
    let bucket = unique("dbine_pages");
    let mut admin = open("influxdb", &cfg, None).await;
    admin.create_database(&bucket).await.unwrap();
    let r = reqwest::Client::new()
        .post(format!(
            "{url}/api/v2/write?org={org}&bucket={bucket}&precision=s"
        ))
        .header("Authorization", format!("Token {token}"))
        .body(lines())
        .send()
        .await
        .unwrap();
    let ok = r.status().is_success();
    let s = open("influxdb", &cfg, Some(&bucket)).await;
    checked_then_dropped(
        async move {
            assert!(ok, "write");
            check(s).await
        },
        async {
            admin.drop_database(&bucket).await.unwrap();
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb3_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB3_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB3_URL not set; skipping");
        return;
    };
    let cfg = ConnectionConfig {
        driver: "influxdb3".into(),
        host: url.clone(),
        ..Default::default()
    };
    let db = unique("dbine_pages");
    let mut admin = open("influxdb3", &cfg, None).await;
    admin.create_database(&db).await.unwrap();
    let r = reqwest::Client::new()
        .post(format!("{url}/api/v3/write_lp?db={db}&precision=second"))
        .body(lines())
        .send()
        .await
        .unwrap();
    let ok = r.status().is_success();
    let s = open("influxdb3", &cfg, Some(&db)).await;
    checked_then_dropped(
        async move {
            assert!(ok, "write");
            check(s).await
        },
        async {
            admin.drop_database(&db).await.unwrap();
        },
    )
    .await;
}
