//! The explorer's pages (`list_objects_page`) on real DuckDB databases (a
//! database file, and a folder of CSV files): for every filter (each kind,
//! a schema, no schema, "Otros", searches with wildcards and letters beyond
//! ASCII, lookups) and page size, each page equals the app's own paging of
//! `list_objects` (`objects::page_of`), cursor included. The files live in
//! a folder of the build's temp folder, deleted at the end even when a
//! check fails.

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A folder of its own, deleted (with what's in it) when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dbine-duck-pages-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_duckdb::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

/// The filters the explorer sends: folders, "Otros", searches, lookups.
fn queries(kinds: &[String], schemas: &[&str]) -> Vec<ObjectQuery> {
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
    for s in schemas.iter().chain(&["nope", "MAIN"]) {
        v.push(ObjectQuery {
            schema: Some(s.to_string()),
            ..Default::default()
        });
        for k in kinds {
            v.push(ObjectQuery {
                schema: Some(s.to_string()),
                kind: Some(k.clone()),
                ..Default::default()
            });
        }
        v.push(ObjectQuery {
            schema: Some(s.to_string()),
            not_kinds: kinds.to_vec(),
            ..Default::default()
        });
    }
    for k in kinds {
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            ..Default::default()
        });
    }
    for p in [
        "T_1", "%", "_", "a%b", "!", "ORDERS", "dup", "zzz", "é", "ÉL", "csv",
    ] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["T_05", "orders", "dup", "nope", "a%b", "élan", "F_10"] {
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

/// Every query in every page size, then `folder` in pages of 10 (at least
/// three of them).
async fn check(s: &mut Box<dyn Session>, d: &Arc<dyn Driver>, schemas: &[&str], folder: &str) {
    let all = s.list_objects().await.unwrap();
    let mut kinds: Vec<String> = d
        .info()
        .object_kinds
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    for o in &all {
        if !kinds.contains(&o.kind) {
            kinds.push(o.kind.clone());
        }
    }
    let id = d.info().id;
    eprintln!("{id}: {} objects, kinds {kinds:?}", all.len());
    let mut checked = 0;
    for q in queries(&kinds, schemas) {
        for limit in [7, 10, 0] {
            same_pages(s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let q = ObjectQuery {
        schema: Some("main".into()),
        kind: Some(folder.into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(s, &all, &q).await;
    assert!(pages >= 3, "a folder of more than one page: {pages}");
    eprintln!(
        "{id}: {checked} filter/size combinations equal; main's {folder} folder in {pages} pages of 10"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_database_file() {
    let dir = TempDir::new("db");
    let d = driver("duckdb");
    let cfg = ConnectionConfig {
        driver: "duckdb".into(),
        host: dir.0.join("pages.duckdb").display().to_string(),
        ..Default::default()
    };
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut setup: Vec<String> = (0..25)
        .map(|i| format!("CREATE TABLE t_{i:02} (id INTEGER)"))
        .collect();
    for t in ["Orders", "a%b", "axb", "dup", "T_Mixed", "Élan"] {
        setup.push(format!("CREATE TABLE \"{t}\" (id INTEGER)"));
    }
    setup.extend((0..9).map(|i| format!("CREATE VIEW v_{i} AS SELECT 1 AS x")));
    setup.extend(
        [
            "CREATE SCHEMA s2",
            "CREATE SCHEMA \"Ünï\"",
            "CREATE TABLE s2.orders (id INTEGER)",
            "CREATE TABLE s2.\"élan\" (id INTEGER)",
            "CREATE VIEW \"Ünï\".v_x AS SELECT 1 AS x",
            "CREATE MACRO f_10(a) AS a, (a, b) AS a + b",
            "CREATE MACRO s2.f_b(a) AS a * 2",
            "CREATE MACRO main.tm() AS TABLE SELECT 1 AS x",
            // A sequence, a macro and a type named as a table: one of each.
            "CREATE SEQUENCE dup",
            "CREATE MACRO dup() AS 1",
            "CREATE TYPE dup AS ENUM ('a', 'b')",
            "CREATE SEQUENCE s2.seq_a",
            "CREATE TYPE s2.mood AS ENUM ('x')",
        ]
        .map(String::from),
    );
    for sql in &setup {
        run(&mut s, sql).await;
    }
    check(&mut s, &d, &["main", "s2", "Ünï"], "table").await;
    // A table, a macro, a sequence and a type of the same name, in the
    // whole list's order.
    let p = s
        .list_objects_page(&ObjectQuery {
            name: Some("DUP".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let got: Vec<&str> = p.objects.iter().map(|o| o.kind.as_str()).collect();
    assert_eq!(got, ["function", "sequence", "table", "type"]);
    drop(s);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_of_files() {
    let dir = TempDir::new("files");
    for i in 0..23 {
        std::fs::write(dir.0.join(format!("f_{i:02}.csv")), "a,b\n1,2\n").unwrap();
    }
    for f in ["Orders.csv", "a%b.csv", "Élan.csv"] {
        std::fs::write(dir.0.join(f), "a\n1\n").unwrap();
    }
    let d = driver("duckdb_files");
    let cfg = ConnectionConfig {
        driver: "duckdb_files".into(),
        host: dir.0.display().to_string(),
        ..Default::default()
    };
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "CREATE TABLE scratch (id INTEGER)").await;
    run(&mut s, "CREATE MACRO twice(a) AS a * 2").await;
    check(&mut s, &d, &["main"], "view").await;
    drop(s);
}
