//! The explorer's pages (`list_objects_page`) against a real libSQL server:
//! for every filter (each kind, a schema, no schema, "Otros", searches with
//! wildcards, lookups) and page size, each page equals the app's own paging
//! of `list_objects` (`objects::page_of`), cursor included. The objects are
//! named `dbp_*` and dropped at the end, even when a check fails:
//!
//! ```sh
//! DBINE_TEST_LIBSQL_URL=http://localhost:25880 \
//!   cargo test -p dbine-driver-libsql --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, ObjectQuery, QueryOutcome, Session};

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_LIBSQL_URL").ok()?;
    let mut cfg = ConnectionConfig {
        driver: "libsql".into(),
        host: url,
        ..Default::default()
    };
    if let Ok(t) = std::env::var("DBINE_TEST_LIBSQL_TOKEN") {
        cfg.options.insert("auth_token".into(), t);
    }
    Some(cfg)
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

/// The tables made here (with an FTS5 table's shadow tables, which
/// `list_objects` lists as tables, when the server has FTS5).
fn tables() -> Vec<String> {
    let mut v: Vec<String> = (0..25).map(|i| format!("dbp_t_{i:02}")).collect();
    v.extend(["dbp_Orders", "dbp_a%b", "dbp_axb", "dbp_dup", "dbp_T_Mixed"].map(String::from));
    v
}

/// 25 tables (a folder of three pages of 10), names that differ only in
/// case, LIKE's wildcards, views and triggers (one named as a table, one as
/// another table in other case).
fn setup() -> Vec<String> {
    let mut v: Vec<String> = tables()
        .iter()
        .map(|t| format!("CREATE TABLE \"{t}\" (id INTEGER PRIMARY KEY)"))
        .collect();
    v.extend((0..9).map(|i| format!("CREATE VIEW dbp_v_{i} AS SELECT 1 AS x")));
    v.extend([
        "CREATE TRIGGER dbp_tr_a BEFORE INSERT ON dbp_t_00 BEGIN SELECT 1; END".into(),
        "CREATE TRIGGER dbp_dup BEFORE INSERT ON dbp_t_01 BEGIN SELECT 1; END".into(),
        "CREATE TRIGGER dbp_orders BEFORE INSERT ON dbp_t_02 BEGIN SELECT 1; END".into(),
    ]);
    v
}

/// Drops what [`setup`] makes (the triggers go with their tables).
async fn cleanup(s: &mut Box<dyn Session>) -> Result<(), String> {
    for i in 0..9 {
        run(s, &format!("DROP VIEW IF EXISTS dbp_v_{i}")).await?;
    }
    run(s, "DROP TABLE IF EXISTS dbp_fts").await?;
    for t in tables() {
        run(s, &format!("DROP TABLE IF EXISTS \"{t}\"")).await?;
    }
    Ok(())
}

/// The filters the explorer sends: folders, "Otros", searches, lookups.
fn queries(kinds: &[String]) -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
        ObjectQuery {
            no_schema: true,
            ..Default::default()
        },
        ObjectQuery {
            schema: Some("main".into()),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: kinds.to_vec(),
            no_schema: true,
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
    }
    for p in [
        "T_1", "%", "_", "a%b", "!", "ORDERS", "dup", "zzz", "dbp_", "fts_",
    ] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in [
        "DBP_T_05",
        "dbp_orders",
        "dbp_dup",
        "nope",
        "dbp_a%b",
        "dbp_fts",
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
    let d = dbine_driver_libsql::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    for sql in setup() {
        run(&mut s, &sql).await.unwrap();
    }
    if let Err(e) = run(&mut s, "CREATE VIRTUAL TABLE dbp_fts USING fts5(body)").await {
        eprintln!("libsql: no FTS5 here ({e})");
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
    eprintln!("libsql: {} objects, kinds {kinds:?}", all.len());
    let mut checked = 0;
    for q in queries(&kinds) {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let tables = ObjectQuery {
        kind: Some("table".into()),
        pattern: Some("dbp_".into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &tables).await;
    assert!(pages >= 3, "a folder of more than one page: {pages}");
    // A table and a trigger of the same name, in the tree's order.
    let p = s
        .list_objects_page(&ObjectQuery {
            name: Some("DBP_DUP".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let got: Vec<(&str, &str)> = p
        .objects
        .iter()
        .map(|o| (o.kind.as_str(), o.name.as_str()))
        .collect();
    assert_eq!(got, [("table", "dbp_dup"), ("trigger", "dbp_dup")]);
    eprintln!(
        "libsql: {checked} filter/size combinations equal; the dbp_ tables in {pages} pages of 10"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pages_equal_the_whole_list() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_LIBSQL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_libsql::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    // What a run that died before its cleanup left.
    cleanup(&mut admin).await.unwrap();
    let outcome = tokio::spawn(check(cfg)).await;
    let cleaned = cleanup(&mut admin).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    cleaned.unwrap();
}
