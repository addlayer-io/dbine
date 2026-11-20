//! The explorer's pages (`list_objects_page`) against a real IoTDB: every
//! search and lookup (capitals, accents, quoted nodes, dots, LIKE
//! characters) and page size holds what the app pages out of
//! `list_objects` (`objects::page_of`); a folder is left to the app
//! (`Unsupported`). The devices live in a database of their own, deleted
//! at the end even when a check fails. Skipped without the variable; set
//! `DBINE_TEST_IOTDB2_URL` too for IoTDB 2:
//!
//! ```sh
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 \
//!   cargo test -p dbine-driver-iotdb --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Error, ObjectQuery, QueryOutcome, Session};

const DB: &str = "root.dbine_pages";

fn cfg(url: &str) -> ConnectionConfig {
    ConnectionConfig {
        driver: "iotdb".into(),
        host: url.into(),
        username: Some("root".into()),
        password: Some("root".into()),
        ..Default::default()
    }
}

async fn open(url: &str, db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_iotdb::drivers()
        .into_iter()
        .find(|d| d.info().id == "iotdb")
        .unwrap();
    d.connect(&cfg(url), db).await.unwrap()
}

fn devices() -> Vec<String> {
    let mut v: Vec<String> = (0..25).map(|i| format!("d_{i:02}")).collect();
    for d in [
        "Orders",
        "orders",
        "`a%b`",
        "axb",
        "a_b",
        "wf01.wt01",
        "wf01.Wt02",
        "`Ñandú`",
        "`ñu`",
        "`it's`",
        "Zeta",
    ] {
        v.push(d.into());
    }
    v
}

fn queries() -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery {
            kind: Some("device".into()),
            pattern: Some("1".into()),
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("table".into()),
            pattern: Some("1".into()),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec!["device".into()],
            pattern: Some("1".into()),
            ..Default::default()
        },
        ObjectQuery {
            schema: Some(DB.into()),
            pattern: Some("1".into()),
            ..Default::default()
        },
        ObjectQuery {
            no_schema: true,
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
        "D_", "%", "_", "a%b", "ORDERS", "ñ", "Ñ", "wf01.", ".WT", "'", "zzz", "dbine",
    ] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in [
        "d_05",
        "orders",
        "`ñandú`",
        "WF01.WT01",
        "nope",
        "`it's`",
        "`a%b`",
    ] {
        v.push(ObjectQuery {
            name: Some(n.into()),
            ..Default::default()
        });
    }
    v
}

/// Every page of `q` from the driver and out of the whole list: the same
/// page each time, cursor included, to the last. Gives how many pages.
async fn same_pages(s: &mut Box<dyn Session>, all: &[DbObject], q: &ObjectQuery) -> usize {
    let mut q = q.clone();
    let mut pages = 0;
    loop {
        let got = s
            .list_objects_page(&q)
            .await
            .unwrap_or_else(|e| panic!("{q:?}: {e}"));
        assert_eq!(got, page_of(all, &q), "{q:?}");
        pages += 1;
        assert!(pages < 100, "{q:?}: no last page");
        match got.next {
            Some(n) => q.after = Some(n),
            None => return pages,
        }
    }
}

async fn check(url: String) {
    let mut s = open(&url, Some(DB)).await;
    for d in devices() {
        let sql = format!("CREATE TIMESERIES {DB}.{d}.s1 WITH DATATYPE=INT32");
        let mut out = QueryOutcome::default();
        s.execute(&sql, 10, &mut out)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let all = s.list_objects().await.unwrap();
    assert_eq!(all.len(), devices().len(), "{all:?}");
    // A folder is the app's, and so is a search the server can't narrow.
    for q in [
        ObjectQuery::default(),
        ObjectQuery {
            kind: Some("device".into()),
            limit: 10,
            ..Default::default()
        },
        ObjectQuery {
            pattern: Some("`".into()),
            ..Default::default()
        },
    ] {
        assert!(
            matches!(s.list_objects_page(&q).await, Err(Error::Unsupported(_))),
            "{q:?}"
        );
    }
    let mut checked = 0;
    for q in queries() {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let search = ObjectQuery {
        pattern: Some("d_".into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &search).await;
    assert!(pages >= 3, "a search of more than one page: {pages}");
    eprintln!(
        "{} devices; {checked} filter/size combinations equal; a search in {pages} pages of 10",
        all.len()
    );
}

async fn pages_on(url: String) {
    let mut admin = open(&url, None).await;
    eprintln!("{}", admin.server_version().await.unwrap());
    let _ = admin.drop_database(DB).await;
    admin.create_database(DB).await.unwrap();
    let outcome = tokio::spawn(check(url)).await;
    let dropped = admin.drop_database(DB).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    dropped.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pages_equal_the_whole_list() {
    let mut any = false;
    for var in ["DBINE_TEST_IOTDB_URL", "DBINE_TEST_IOTDB2_URL"] {
        if let Ok(url) = std::env::var(var) {
            pages_on(url).await;
            any = true;
        }
    }
    if !any {
        eprintln!("DBINE_TEST_IOTDB_URL / DBINE_TEST_IOTDB2_URL not set; skipping");
    }
}
