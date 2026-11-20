//! The explorer's pages (`list_objects_page`) against a real kdb+
//! (`dbine-test-kdb`, see `live.rs`): for every filter (each kind, the root,
//! each namespace, "Otros", searches with `like`'s wildcards, lookups) and
//! page size, each page equals the app's own paging of `list_objects`
//! (`objects::page_of`), cursor included.
//!
//! ```sh
//! DBINE_TEST_KDB_URL=127.0.0.1:15000:dbine:dbinepass \
//!   cargo test -p dbine-driver-kdb --test objects_page -- --ignored --nocapture
//! ```
//!
//! The objects are named `dbine_pg*` (with the `.dbine_pg` and `.dbine_PG`
//! namespaces and a splayed table under `dbine_pg_hdb/`) and deleted at the
//! end, even when a check fails.

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, ObjectQuery, QueryOutcome, Session};

/// `host:port[:user:password]` from `DBINE_TEST_KDB_URL`.
fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_KDB_URL").ok()?;
    let mut parts = url.splitn(4, ':');
    let host = parts.next().unwrap_or("127.0.0.1").to_string();
    let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(5000);
    Some(ConnectionConfig {
        host,
        port,
        username: parts.next().map(str::to_string),
        password: parts.next().map(str::to_string),
        options: [("tcps".to_string(), "false".to_string())].into(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(q, 10, &mut out)
        .await
        .map_err(|e| format!("{q}: {e}"))?;
    match out.error {
        Some(e) => Err(format!("{q}: {e}")),
        None => Ok(()),
    }
}

/// 25 tables at the root (a folder of three pages of 10), names that
/// differ only in case, `like`'s wildcards in names, views, functions and
/// variables at the root and in two namespaces whose names differ only in
/// case, and a splayed table.
const SETUP: &[&str] = &[
    "{(`$\"dbine_pg_t_\",-2#\"0\",string x) set ([] a:1 2)} each til 25",
    "dbine_pg_Orders:([] a:1 2); dbine_pg_orders:{x+1}; dbine_pg_f:{x*2}",
    "(`$\"dbine_pg_a*b\") set 1; (`$\"dbine_pg_a?b\") set 2; dbine_pg_axb:3; (`$\"dbine_pg_a[b\") set 4",
    "dbine_pg_v::1+1",
    "dbine_pg_w::count dbine_pg_t_00",
    ".dbine_pg.t:([] a:1 2); .dbine_pg.T:([] b:1 2); .dbine_pg.f:{x}; .dbine_pg.x:42",
    ".dbine_PG.orders:([] a:1 2); .dbine_PG.Orders:7",
    "`:dbine_pg_hdb/dbine_pg_s/ set ([] n:1 2; x:1.5 2.5)",
    "dbine_pg_s:get `:dbine_pg_hdb/dbine_pg_s/",
];

/// Deletes what [`SETUP`] makes (on disk too).
const CLEANUP: &[&str] = &[
    "{![`.;();0b;enlist x]} each {x where x like \"dbine_pg*\"} key `.",
    "![`;();0b;`dbine_pg`dbine_PG]",
    // The files before their folders; nothing when the folder isn't there.
    "@[{hdel each desc {$[11h=type d:key x;raze x,.z.s each ` sv/:x,/:d;enlist x]} x};`:dbine_pg_hdb;{}]",
];

/// The filters the explorer sends: folders, "Otros", searches, lookups.
fn queries(kinds: &[String]) -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
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
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("table".into()),
            pattern: Some("1".into()),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec!["table".into()],
            pattern: Some("_".into()),
            ..Default::default()
        },
    ];
    for s in [".dbine_pg", ".dbine_PG", ".DBINE_PG", ".nope", ".", ""] {
        v.push(ObjectQuery {
            schema: Some(s.into()),
            ..Default::default()
        });
        v.push(ObjectQuery {
            schema: Some(s.into()),
            kind: Some("table".into()),
            ..Default::default()
        });
    }
    for k in kinds {
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            ..Default::default()
        });
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            no_schema: true,
            ..Default::default()
        });
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            schema: Some(".dbine_pg".into()),
            ..Default::default()
        });
    }
    for p in [
        "T_1", "*", "?", "[", "a*b", "a?b", "a[b", "ORDERS", "dbine_pg", "zzz", "_", "",
    ] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in [
        "DBINE_PG_T_05",
        "dbine_pg_orders",
        "orders",
        "nope",
        "dbine_pg_a*b",
        "dbine_pg_a?b",
        "x",
        "",
    ] {
        v.push(ObjectQuery {
            name: Some(n.into()),
            ..Default::default()
        });
    }
    v
}

/// Pages `q` through the driver and through `page_of` over `all`: the same
/// page each time, to the last one. Returns how many pages.
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

async fn check(cfg: ConnectionConfig) {
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    for q in SETUP {
        run(&mut s, q).await.unwrap();
    }
    let all = s.list_objects().await.unwrap();
    let mut kinds: Vec<String> = d
        .info()
        .object_kinds
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    for o in &all {
        assert!(kinds.contains(&o.kind), "{o:?}");
    }
    kinds.sort();
    let kind_of = |n: &str| all.iter().find(|o| o.name == n).map(|o| o.kind.as_str());
    assert_eq!(kind_of("dbine_pg_s"), Some(dbine_driver_kdb::SPLAYED));
    assert_eq!(kind_of("dbine_pg_v"), Some("view"));
    eprintln!("kdb: {} objects, kinds {kinds:?}", all.len());
    let mut checked = 0;
    for q in queries(&kinds) {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let tables = ObjectQuery {
        no_schema: true,
        kind: Some("table".into()),
        pattern: Some("dbine_pg".into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &tables).await;
    assert!(pages >= 3, "a folder of more than one page: {pages}");
    eprintln!(
        "kdb: {checked} filter/size combinations equal; the root's dbine_pg tables in {pages} pages of 10"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pages_equal_the_whole_list() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_KDB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let outcome = tokio::spawn(check(cfg)).await;
    let mut cleaned = Ok(());
    for q in CLEANUP {
        if let Err(e) = run(&mut admin, q).await {
            cleaned = Err(e);
        }
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    cleaned.unwrap();
}
