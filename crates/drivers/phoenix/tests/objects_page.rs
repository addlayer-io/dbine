//! The explorer's pages (`list_objects_page`) against a real Phoenix Query
//! Server: for every filter (each kind, a schema, the default schema,
//! "Otros", searches, lookups) and page size, each page equals the app's own
//! paging of `list_objects` (`objects::page_of`), cursor included. The
//! objects are in the schemas `DBINE_PAGES` and `DBINE_PAGES_B` and, in the
//! default schema, named `DBINE_PAGES_*`; all dropped at the end even when a
//! check fails. Skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_PHOENIX_URL=http://localhost:25165 \
//!   cargo test -p dbine-driver-phoenix --test objects_page -- --ignored --nocapture
//! ```
//! `DBINE_TEST_PHOENIX_SERIALIZATION=json` for a server set to JSON.

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::sync::Arc;

const A: &str = "DBINE_PAGES";
const B: &str = "DBINE_PAGES_B";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_PHOENIX_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "phoenix".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        ..Default::default()
    };
    if let Ok(s) = std::env::var("DBINE_TEST_PHOENIX_SERIALIZATION") {
        c.options.insert("serialization".into(), s);
    }
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    match out.error {
        Some(e) => Err(format!("{sql}: {e}")),
        None => Ok(()),
    }
}

/// The tables, as `schema.name` (quoted): in `A` 25 numbered (a folder of
/// three pages of 10), names that differ only in case, LIKE's `_` (HBase
/// takes no other wildcard nor non-ASCII letters in a table's name) and one
/// a sequence also has; in `B` names `A` has; in the default schema, two.
fn tables() -> Vec<String> {
    let mut v: Vec<String> = (0..25).map(|i| format!("{A}.\"T_{i:02}\"")).collect();
    for n in ["Orders", "ORDERS", "a_b", "aXb", "DUP"] {
        v.push(format!("{A}.\"{n}\""));
    }
    for n in ["DUP", "orders"] {
        v.push(format!("{B}.\"{n}\""));
    }
    for n in ["DBINE_PAGES_LOOSE", "dbine_pages_loose2"] {
        v.push(format!("\"{n}\""));
    }
    v
}

/// The views (metadata only: a non-ASCII letter is fine).
fn views() -> Vec<String> {
    let mut v: Vec<String> = (0..9).map(|i| format!("{A}.\"V_{i}\"")).collect();
    v.push(format!("{A}.\"Ünïcode\""));
    v.push(format!("{B}.\"V_ORDERS\""));
    v
}

fn sequences() -> Vec<String> {
    vec![
        format!("{A}.\"S_A\""),
        format!("{A}.\"DUP\""),
        format!("{B}.\"s_b\""),
        "\"DBINE_PAGES_SEQ\"".into(),
    ]
}

fn setup() -> Vec<String> {
    let mut v = vec![
        format!("CREATE SCHEMA IF NOT EXISTS {A}"),
        format!("CREATE SCHEMA IF NOT EXISTS {B}"),
    ];
    v.extend(
        tables()
            .iter()
            .map(|t| format!("CREATE TABLE {t} (ID INTEGER PRIMARY KEY, V VARCHAR)")),
    );
    v.extend(
        views()
            .iter()
            .map(|n| format!("CREATE VIEW {n} AS SELECT * FROM {A}.\"T_00\"")),
    );
    v.extend(sequences().iter().map(|n| format!("CREATE SEQUENCE {n}")));
    v
}

fn cleanup() -> Vec<String> {
    let mut v: Vec<String> = views()
        .iter()
        .map(|n| format!("DROP VIEW IF EXISTS {n}"))
        .collect();
    v.extend(tables().iter().map(|t| format!("DROP TABLE IF EXISTS {t}")));
    v.extend(
        sequences()
            .iter()
            .map(|n| format!("DROP SEQUENCE IF EXISTS {n}")),
    );
    v.push(format!("DROP SCHEMA IF EXISTS {A}"));
    v.push(format!("DROP SCHEMA IF EXISTS {B}"));
    v
}

/// The filters the explorer sends: folders, "Otros", searches, lookups.
fn queries(kinds: &[String], declared: &[String]) -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
        ObjectQuery {
            not_kinds: declared.to_vec(),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec!["table".into()],
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("nope".into()),
            ..Default::default()
        },
    ];
    let schemas = [None, Some(A), Some(B), Some("dbine_pages"), Some("SYSTEM")];
    for schema in schemas {
        let base = ObjectQuery {
            no_schema: schema.is_none(),
            schema: schema.map(Into::into),
            ..Default::default()
        };
        v.push(base.clone());
        for k in kinds {
            v.push(ObjectQuery {
                kind: Some(k.clone()),
                ..base.clone()
            });
        }
    }
    for k in kinds {
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            ..Default::default()
        });
    }
    for p in [
        "T_1", "%", "_", "a_b", "orders", "dup", "ÜNÏ", "LOOSE", "zzz", "",
    ] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["t_05", "orders", "dup", "A_B", "dbine_pages_seq", "nope"] {
        v.push(ObjectQuery {
            name: Some(n.into()),
            ..Default::default()
        });
    }
    v
}

/// Pages `q` through the driver and through `page_of` over `all`: the same
/// objects each time, to the last page. Returns how many pages.
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

async fn check(d: Arc<dyn Driver>, cfg: ConnectionConfig) {
    let mut s = d.connect(&cfg, None).await.expect("connect");
    for sql in setup() {
        if let Err(e) = run(&mut s, &sql).await {
            panic!("{e}");
        }
    }
    let all = s.list_objects().await.unwrap();
    let declared: Vec<String> = d
        .info()
        .object_kinds
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    let mut kinds = declared.clone();
    for o in &all {
        if !kinds.contains(&o.kind) {
            kinds.push(o.kind.clone());
        }
    }
    eprintln!("{} objects, kinds {kinds:?}", all.len());
    let mut checked = 0;
    for q in queries(&kinds, &declared) {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let folder = ObjectQuery {
        schema: Some(A.into()),
        kind: Some("table".into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &folder).await;
    assert!(pages >= 3, "a folder of more than one page: {pages}");
    let p = s.list_objects_page(&folder).await.unwrap();
    assert_eq!(p.objects.len(), 10);
    // A cursor that isn't the driver's (the app's own offset) goes back to
    // the app.
    let foreign = ObjectQuery {
        after: Some("10".into()),
        ..folder
    };
    assert!(matches!(
        s.list_objects_page(&foreign).await,
        Err(dbine_driver::Error::Unsupported(_))
    ));
    eprintln!("{checked} filter/size combinations equal; the tables folder in {pages} pages of 10");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn phoenix() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_PHOENIX_URL not set; skipping");
        return;
    };
    let d = dbine_driver_phoenix::drivers()
        .into_iter()
        .find(|d| d.info().id == "phoenix")
        .unwrap();
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    eprintln!("{}", admin.server_version().await.unwrap());
    for sql in cleanup() {
        let _ = run(&mut admin, &sql).await;
    }
    let outcome = tokio::spawn(check(d.clone(), cfg.clone())).await;
    let mut dropped = Ok(());
    for sql in cleanup() {
        if let Err(e) = run(&mut admin, &sql).await {
            dropped = Err(e);
        }
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    dropped.unwrap();
    // A generic Avatica server has no catalog to narrow: the app pages.
    let generic = dbine_driver_phoenix::drivers()
        .into_iter()
        .find(|d| d.info().id == "avatica")
        .unwrap();
    let mut cfg = cfg;
    cfg.options
        .entry("serialization".into())
        .or_insert_with(|| "protobuf".into());
    let mut g = generic.connect(&cfg, None).await.expect("connect");
    assert!(matches!(
        g.list_objects_page(&ObjectQuery::default()).await,
        Err(dbine_driver::Error::Unsupported(_))
    ));
}
