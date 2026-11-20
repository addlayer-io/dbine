//! The explorer's pages (`list_objects_page`) against a real Apache Drill:
//! for every filter (each kind, the schema, no schema, "Otros", searches,
//! lookups) and page size, each page equals the app's own paging of
//! `list_objects` (`objects::page_of`), cursor included. The objects are
//! `dbine_pages_*` in `dfs.tmp`: views and directories written with CTAS
//! (the workspace's files), one of them hidden; all dropped at the end even
//! when a check fails. Drill refuses a view named as a file, so with
//! `DBINE_TEST_DRILL_CONTAINER` the test also writes one into the
//! container's `/tmp` (`dfs.tmp`) with `docker exec`. Skipped without the
//! URL:
//!
//! ```sh
//! DBINE_TEST_DRILL_URL=http://localhost:25847 DBINE_TEST_DRILL_CONTAINER=dbine-test-drill \
//!   cargo test -p dbine-driver-drill --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::sync::Arc;

const WS: &str = "dfs.tmp";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DRILL_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "drill".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        database: WS.into(),
        ..Default::default()
    })
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

/// The directories (CTAS): numbered, names that differ only in case, LIKE's
/// wildcards, a non-ASCII letter and a hidden one.
fn dirs() -> Vec<String> {
    let mut v: Vec<String> = (0..14).map(|i| format!("dbine_pages_t{i:02}")).collect();
    for n in [
        "dbine_pages_Orders",
        "dbine_pages_orders",
        "dbine_pages_a%b",
        "dbine_pages_Ünïcode",
        "_dbine_pages_hidden",
    ] {
        v.push(n.into());
    }
    v
}

/// The views: 12 (a folder of three pages of 5), and one named as the
/// file [`docker_file`] writes.
fn views() -> Vec<String> {
    let mut v: Vec<String> = (0..12).map(|i| format!("dbine_pages_v{i:02}")).collect();
    v.push("dbine_pages_dup".into());
    v
}

/// A file named as a view, written in the container's `dfs.tmp` (`/tmp`):
/// `touch` or `rm` it, when the container's name is given.
fn docker_file(touch: bool) -> bool {
    let Ok(c) = std::env::var("DBINE_TEST_DRILL_CONTAINER") else {
        return false;
    };
    let cmd: &[&str] = if touch { &["touch"] } else { &["rm", "-f"] };
    std::process::Command::new("docker")
        .args(["exec", c.as_str()])
        .args(cmd)
        .arg("/tmp/dbine_pages_dup")
        .status()
        .is_ok_and(|s| s.success())
}

fn setup() -> Vec<String> {
    let mut v: Vec<String> = dirs()
        .iter()
        .map(|d| format!("CREATE TABLE {WS}.`{d}` AS SELECT 1 AS x FROM (VALUES(1))"))
        .collect();
    v.extend(
        views()
            .iter()
            .map(|n| format!("CREATE VIEW {WS}.`{n}` AS SELECT 1 AS x FROM (VALUES(1))")),
    );
    v
}

fn cleanup() -> Vec<String> {
    let mut v: Vec<String> = views()
        .iter()
        .map(|n| format!("DROP VIEW IF EXISTS {WS}.`{n}`"))
        .collect();
    v.extend(
        dirs()
            .iter()
            .map(|d| format!("DROP TABLE IF EXISTS {WS}.`{d}`")),
    );
    v
}

/// The filters the explorer sends: folders, "Otros", searches, lookups.
fn queries(kinds: &[String], declared: &[String]) -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
        ObjectQuery {
            no_schema: true,
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: declared.to_vec(),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec!["file".into()],
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("nope".into()),
            ..Default::default()
        },
    ];
    for schema in [WS, "DFS.TMP", "dfs.root"] {
        v.push(ObjectQuery {
            schema: Some(schema.into()),
            ..Default::default()
        });
        for k in kinds {
            v.push(ObjectQuery {
                schema: Some(schema.into()),
                kind: Some(k.clone()),
                ..Default::default()
            });
        }
    }
    for p in ["PAGES_T1", "_", "a%b", "ORDERS", "ÜNÏ", "hidden"] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["DBINE_PAGES_T05", "dbine_pages_orders", "dbine_pages_DUP"] {
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
    let named_as_view = docker_file(true);
    let all = s.list_objects().await.unwrap();
    assert!(
        all.iter()
            .any(|o| o.kind == "file" && o.name == "dbine_pages_Orders"),
        "the directories are the workspace's files"
    );
    assert!(!all.iter().any(|o| o.name == "_dbine_pages_hidden"));
    if named_as_view {
        let files = s
            .list_objects_page(&ObjectQuery {
                kind: Some("file".into()),
                name: Some("dbine_pages_dup".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            files.objects.is_empty(),
            "a file named as a view is the view"
        );
    } else {
        eprintln!("DBINE_TEST_DRILL_CONTAINER not set: no file named as a view");
    }
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
    // Each page of files lists the workspace (as `SHOW FILES` does), which
    // Drill takes seconds to do: pages of 50 and of 1,000.
    for q in queries(&kinds, &declared) {
        for limit in [50, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let views = ObjectQuery {
        schema: Some(WS.into()),
        kind: Some("view".into()),
        limit: 5,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &views).await;
    assert!(pages >= 3, "a folder of more than one page: {pages}");
    let p = s.list_objects_page(&views).await.unwrap();
    assert_eq!(p.objects.len(), 5);
    let files = ObjectQuery {
        kind: Some("file".into()),
        limit: 50,
        ..Default::default()
    };
    let file_pages = same_pages(&mut s, &all, &files).await;
    assert!(file_pages >= 2, "{file_pages}");
    // A cursor that isn't the driver's (the app's own offset) goes back to
    // the app.
    let foreign = ObjectQuery {
        after: Some("10".into()),
        ..files
    };
    assert!(matches!(
        s.list_objects_page(&foreign).await,
        Err(dbine_driver::Error::Unsupported(_))
    ));
    eprintln!(
        "{checked} filter/size combinations equal; the views folder in {pages} pages of 5, the files in {file_pages} of 50"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn drill() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_DRILL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_drill::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    eprintln!("{}", admin.server_version().await.unwrap());
    for sql in cleanup() {
        let _ = run(&mut admin, &sql).await;
    }
    let outcome = tokio::spawn(check(d.clone(), cfg)).await;
    docker_file(false);
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
}
