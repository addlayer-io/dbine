//! The explorer's pages (`list_objects_page`) against a real TDengine: for
//! every filter (each kind, the database as schema, another one, "Otros",
//! searches with LIKE characters, capitals and accents, lookups) and page
//! size, each page holds what the app pages out of `list_objects`
//! (`objects::page_of`). The objects live in a database of their own,
//! dropped at the end (topics and streams first) even when a check fails;
//! skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//!   cargo test -p dbine-driver-tdengine --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::sync::Arc;

const DB: &str = "dbine_pages";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TDENGINE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    match out.error.or(out.errors.first().map(|e| e.message.clone())) {
        Some(e) => Err(format!("{sql}: {e}")),
        None => Ok(()),
    }
}

fn setup() -> Vec<String> {
    let mut v: Vec<String> = [
        "CREATE STABLE st (ts TIMESTAMP, v INT) TAGS (g INT)",
        "CREATE STABLE `StB` (ts TIMESTAMP, v INT) TAGS (g INT)",
        "CREATE STABLE `Ñ_st` (ts TIMESTAMP, v INT) TAGS (g INT)",
    ]
    .map(String::from)
    .to_vec();
    v.extend((0..25).map(|i| format!("CREATE TABLE t_{i:02} (ts TIMESTAMP, v INT)")));
    for t in [
        "`Orders`",
        "orders",
        "`a%b`",
        "axb",
        "a_b",
        "`Ñandú`",
        "`ñu`",
        "`İzmir`",
        "`Zeta`",
    ] {
        v.push(format!("CREATE TABLE {t} (ts TIMESTAMP, v INT)"));
    }
    v.extend((0..12).map(|i| format!("CREATE TABLE d_{i:02} USING st TAGS ({i})")));
    v.push("CREATE TABLE `D_x` USING `StB` TAGS (1)".into());
    v.push("CREATE TABLE `ñ_sub` USING st TAGS (2)".into());
    v
}

/// Kinds some editions lack: tried, and left out when the server refuses.
fn optional() -> Vec<String> {
    vec![
        format!("CREATE VIEW {DB}.v_a AS SELECT * FROM {DB}.t_00"),
        format!("CREATE TOPIC dbine_pages_tp AS SELECT * FROM {DB}.t_00"),
        format!(
            "CREATE STREAM dbine_pages_s INTO {DB}.s_out AS SELECT _wstart, COUNT(*) FROM {DB}.st INTERVAL(10s)"
        ),
    ]
}

fn queries(kinds: &[String]) -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
        ObjectQuery {
            schema: Some(DB.into()),
            ..Default::default()
        },
        ObjectQuery {
            schema: Some("other".into()),
            ..Default::default()
        },
        ObjectQuery {
            no_schema: true,
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: kinds.to_vec(),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec!["table".into()],
            schema: Some(DB.into()),
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("table".into()),
            pattern: Some("1".into()),
            ..Default::default()
        },
        ObjectQuery {
            pattern: Some("orders".into()),
            name: Some("ORDERS".into()),
            ..Default::default()
        },
    ];
    for k in kinds {
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            schema: Some(DB.into()),
            ..Default::default()
        });
    }
    for p in [
        "T_1", "%", "_", "a%b", "ORDERS", "ñ", "Ñ", "i", "zzz", "\\", "'",
    ] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["t_05", "orders", "ñandú", "İZMIR", "d_x", "nope", "a%b"] {
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

async fn check(d: Arc<dyn Driver>, cfg: ConnectionConfig) {
    let mut s = d.connect(&cfg, Some(DB)).await.unwrap();
    for sql in setup() {
        run(&mut s, &sql).await.unwrap();
    }
    for sql in optional() {
        if let Err(e) = run(&mut s, &sql).await {
            eprintln!("left out: {e}");
        }
    }
    let all = s.list_objects().await.unwrap();
    let kinds: Vec<String> = d
        .info()
        .object_kinds
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    let present: Vec<&String> = kinds
        .iter()
        .filter(|k| all.iter().any(|o| &o.kind == *k))
        .collect();
    eprintln!("kinds with objects: {present:?}");
    for k in ["supertable", "table", "subtable"] {
        assert!(all.iter().any(|o| o.kind == k), "no {k}: {all:?}");
    }
    let mut checked = 0;
    for q in queries(&kinds) {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let tables = ObjectQuery {
        kind: Some("table".into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &tables).await;
    assert!(pages >= 3, "a folder of more than one page: {pages}");
    eprintln!(
        "{} objects; {checked} filter/size combinations equal; the tables folder in {pages} pages of 10",
        all.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pages_equal_the_whole_list() {
    let Some(base) = cfg() else {
        eprintln!("DBINE_TEST_TDENGINE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_tdengine::drivers().remove(0);
    let mut admin = d.connect(&base, None).await.unwrap();
    eprintln!("{}", admin.server_version().await.unwrap());
    let cleanup = [
        "DROP TOPIC IF EXISTS dbine_pages_tp".to_string(),
        "DROP STREAM IF EXISTS dbine_pages_s".to_string(),
        format!("DROP DATABASE IF EXISTS {DB}"),
    ];
    for sql in &cleanup {
        let _ = run(&mut admin, sql).await;
    }
    run(&mut admin, &format!("CREATE DATABASE {DB}"))
        .await
        .unwrap();
    let outcome = tokio::spawn(check(d.clone(), base.clone())).await;
    let mut dropped = Ok(());
    for sql in &cleanup {
        let r = run(&mut admin, sql).await;
        if dropped.is_ok() {
            dropped = r;
        }
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    dropped.unwrap();
}
