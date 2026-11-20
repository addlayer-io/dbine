//! The explorer's pages (`list_objects_page`) on a real database file: for
//! every filter (each kind, a schema, no schema, "Otros", searches with
//! wildcards and letters beyond ASCII, lookups) and page size, each page
//! equals the app's own paging of `list_objects` (`objects::page_of`),
//! cursor included. The file lives in the build's temp folder and is
//! deleted at the end, even when a check fails.

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, ObjectQuery, QueryOutcome, Session};

/// The database file and its journals, deleted when dropped.
struct TempDb(std::path::PathBuf);

impl Drop for TempDb {
    fn drop(&mut self) {
        for ext in ["", "-journal", "-wal", "-shm"] {
            let mut p = self.0.clone().into_os_string();
            p.push(ext);
            let _ = std::fs::remove_file(p);
        }
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

/// The objects: 25 tables (a folder of three pages of 10), names that
/// differ only in case (also beyond ASCII, which SQLite tells apart), LIKE's
/// wildcards, views, triggers (one named as a table, one as another table
/// in other case), virtual tables (FTS5, whose shadow tables
/// `list_objects` lists as tables, and R*Tree), and what `list_objects`
/// leaves out: an index, `sqlite_sequence` and a vector index's metadata.
fn setup() -> Vec<String> {
    let mut v: Vec<String> = (0..25)
        .map(|i| format!("CREATE TABLE t_{i:02} (id INTEGER PRIMARY KEY)"))
        .collect();
    for t in [
        "Orders", "a%b", "axb", "dup", "T_Mixed", "AÑOS", "años", "Élan",
    ] {
        v.push(format!("CREATE TABLE \"{t}\" (id INTEGER PRIMARY KEY)"));
    }
    v.extend((0..9).map(|i| format!("CREATE VIEW v_{i} AS SELECT 1 AS x")));
    v.extend([
        "CREATE TRIGGER tr_a BEFORE INSERT ON t_00 BEGIN SELECT 1; END".into(),
        "CREATE TRIGGER dup BEFORE INSERT ON t_01 BEGIN SELECT 1; END".into(),
        "CREATE TRIGGER orders BEFORE INSERT ON t_02 BEGIN SELECT 1; END".into(),
        "CREATE VIRTUAL TABLE fts USING fts5(body)".into(),
        "CREATE VIRTUAL TABLE rt USING rtree(id, x0, x1)".into(),
        "CREATE INDEX ix_t03 ON t_03 (id)".into(),
        "CREATE TABLE seq (id INTEGER PRIMARY KEY AUTOINCREMENT)".into(),
        "INSERT INTO seq DEFAULT VALUES".into(),
        "CREATE TABLE libsql_vector_meta_shadow (name TEXT)".into(),
    ]);
    v
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
            schema: Some("main".into()),
            no_schema: true,
            kind: Some("view".into()),
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
        "T_1", "%", "_", "a%b", "!", "ORDERS", "dup", "zzz", "ñ", "AÑ", "élan", "fts_",
    ] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["T_05", "orders", "dup", "nope", "a%b", "años", "ÉLAN", ""] {
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

#[tokio::test]
async fn pages_equal_the_whole_list() {
    let db = TempDb(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dbine-sqlite-pages-{}.db", std::process::id())),
    );
    drop(TempDb(db.0.clone()));
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let cfg = ConnectionConfig {
        driver: "sqlite".into(),
        host: db.0.display().to_string(),
        ..Default::default()
    };
    let mut s = d.connect(&cfg, None).await.unwrap();
    for sql in setup() {
        run(&mut s, &sql).await;
    }
    let all = s.list_objects().await.unwrap();
    let names: Vec<&str> = all.iter().map(|o| o.name.as_str()).collect();
    for left_out in ["ix_t03", "sqlite_sequence", "libsql_vector_meta_shadow"] {
        assert!(!names.contains(&left_out), "{left_out}: {names:?}");
    }
    assert!(names.contains(&"fts_data"), "{names:?}");
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
    eprintln!("sqlite: {} objects, kinds {kinds:?}", all.len());
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
    // Two objects of the same name (a table and a trigger) and two that
    // differ only in case, in the tree's order.
    let p = s
        .list_objects_page(&ObjectQuery {
            pattern: Some("dup".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let got: Vec<(&str, &str)> = p
        .objects
        .iter()
        .map(|o| (o.kind.as_str(), o.name.as_str()))
        .collect();
    assert_eq!(got, [("table", "dup"), ("trigger", "dup")]);
    let p = s
        .list_objects_page(&ObjectQuery {
            name: Some("AÑOS".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let got: Vec<&str> = p.objects.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(got, ["AÑOS", "años"]);
    eprintln!(
        "sqlite: {checked} filter/size combinations equal; the tables folder in {pages} pages of 10"
    );
    drop(s);
}
