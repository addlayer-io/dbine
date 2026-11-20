//! The explorer's pages (`list_objects_page`) through a real ODBC driver:
//! a schema's tables and views (SQLTables for that schema) equal the app's
//! own paging of `list_objects` (`objects::page_of`) for every filter and
//! page size, cursor included, and what SQLTables can't narrow (routines,
//! sequences, every schema at once) is left to the app (`Unsupported`).
//! Two schemas whose names differ where the first has a `_` check that the
//! schema's pattern is escaped (or what it matched beyond is dropped).
//!
//! ```sh
//! # Db2 LUW (icr.io/db2_community/db2) through IBM's CLI driver, with the
//! # Db2 preset and with the generic one.
//! DBINE_TEST_ODBC_PAGES_DB2='host=127.0.0.1;port=50000;database=testdb;user=db2inst1;password=…;odbc_driver=IBM DB2 ODBC DRIVER' \
//!   cargo test -p dbine-driver-odbc --test objects_page -- --ignored --nocapture
//! ```
//!
//! The objects live in schemas named `DBINE_PAGE_*`, dropped at the end
//! even when a check fails.

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Error, ObjectQuery, QueryOutcome, Session};

/// The connection from `var` (`key=value;…`), or `None` (skipped).
fn cfg(var: &str, preset: &str) -> Option<ConnectionConfig> {
    let raw = std::env::var(var).ok()?;
    let mut c = ConnectionConfig {
        driver: preset.into(),
        ..Default::default()
    };
    for part in raw.split(';') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k.trim() {
            "host" => c.host = v.into(),
            "port" => c.port = v.parse().unwrap_or(0),
            "database" => c.database = v.into(),
            "user" => c.username = Some(v.into()),
            "password" => c.password = Some(v.into()),
            key => {
                c.options.insert(key.into(), v.into());
            }
        }
    }
    Some(c)
}

/// The same server through the generic preset: its connection string.
fn generic(c: &ConnectionConfig) -> ConnectionConfig {
    let conn = format!(
        "DRIVER={{{}}};DATABASE={};HOSTNAME={};PORT={};PROTOCOL=TCPIP;UID={};PWD={}",
        c.options["odbc_driver"],
        c.database,
        c.host,
        c.port,
        c.username.as_deref().unwrap_or_default(),
        c.password.as_deref().unwrap_or_default()
    );
    ConnectionConfig {
        driver: "odbc".into(),
        options: [("connection_string".to_string(), conn)].into(),
        ..Default::default()
    }
}

async fn open(c: &ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_driver_odbc::drivers()
        .into_iter()
        .find(|d| d.info().id == c.driver)
        .expect("preset");
    d.connect(c, None).await.expect("connect")
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    match out.error {
        Some(e) => Err(format!("{sql}: {e}")),
        None => Ok(()),
    }
}

/// The schemas made here: `A_B` and `AXB` (which `A_B` matches as a
/// pattern) and one in lower case.
const SCHEMAS: &[&str] = &["DBINE_PAGE_A_B", "DBINE_PAGE_AXB", "DBINE_PAGE_low"];

fn setup() -> Vec<String> {
    let mut v: Vec<String> = SCHEMAS
        .iter()
        .map(|s| format!("CREATE SCHEMA \"{s}\""))
        .collect();
    let s = SCHEMAS[0];
    v.extend((0..25).map(|i| format!("CREATE TABLE \"{s}\".T_{i:02} (ID INT)")));
    for t in ["Orders", "ORDERS", "a%b", "AXB"] {
        v.push(format!("CREATE TABLE \"{s}\".\"{t}\" (ID INT)"));
    }
    v.extend(
        (0..9).map(|i| format!("CREATE VIEW \"{s}\".V_{i} AS SELECT 1 AS X FROM SYSIBM.SYSDUMMY1")),
    );
    v.push(format!("CREATE SEQUENCE \"{s}\".S_A"));
    v.extend([
        format!("CREATE TABLE \"{}\".T_00 (ID INT)", SCHEMAS[1]),
        format!(
            "CREATE VIEW \"{}\".V_AXB AS SELECT 1 AS X FROM SYSIBM.SYSDUMMY1",
            SCHEMAS[1]
        ),
        format!("CREATE TABLE \"{}\".\"t_low\" (ID INT)", SCHEMAS[2]),
    ]);
    v
}

/// Drops every object of [`SCHEMAS`] and the schemas (what a run that died
/// before its cleanup left too). Nothing else is touched.
async fn cleanup(s: &mut Box<dyn Session>) -> Result<(), String> {
    let all = s.list_objects().await.map_err(|e| e.to_string())?;
    let mut failed = Vec::new();
    // Views first: a table goes after the views over it.
    for what in ["view", "table", "sequence"] {
        for o in all.iter().filter(|o| o.kind == what) {
            let Some(schema) = o.schema.as_deref().filter(|x| SCHEMAS.contains(x)) else {
                continue;
            };
            let sql = format!("DROP {} \"{schema}\".\"{}\"", what.to_uppercase(), o.name);
            if let Err(e) = exec(s, &sql).await {
                failed.push(e);
            }
        }
    }
    for schema in SCHEMAS {
        // Absent when this run never made it: not a failure.
        let _ = exec(s, &format!("DROP SCHEMA \"{schema}\" RESTRICT")).await;
    }
    // None of them is left.
    let left: Vec<String> = s
        .list_schemas()
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or_default()
        .into_iter()
        .map(|i| i.name)
        .filter(|n| SCHEMAS.contains(&n.as_str()))
        .collect();
    if !left.is_empty() {
        failed.push(format!("schemas left: {left:?}"));
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("\n"))
    }
}

/// The filters SQLTables narrows: a schema's tables, views or both, with
/// searches and lookups inside.
fn narrow() -> Vec<ObjectQuery> {
    let mut v = Vec::new();
    let both = || -> Vec<String> {
        ["procedure", "function", "sequence", "synonym", "type"]
            .map(String::from)
            .to_vec()
    };
    for s in SCHEMAS
        .iter()
        .chain(&["DBINE_PAGE_LOW", "DBINE_PAGE_NONE", "SYSCAT", ""])
    {
        for k in ["table", "view"] {
            v.push(ObjectQuery {
                schema: Some(s.to_string()),
                kind: Some(k.into()),
                ..Default::default()
            });
        }
        v.push(ObjectQuery {
            schema: Some(s.to_string()),
            not_kinds: both(),
            ..Default::default()
        });
    }
    let s = Some(SCHEMAS[0].to_string());
    for p in ["T_1", "%", "_", "a%b", "orders", "zzz", "x", ""] {
        v.push(ObjectQuery {
            schema: s.clone(),
            not_kinds: both(),
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["t_05", "orders", "a%b", "V_3", "nope", "S_A"] {
        v.push(ObjectQuery {
            schema: s.clone(),
            kind: Some("table".into()),
            name: Some(n.into()),
            ..Default::default()
        });
        v.push(ObjectQuery {
            schema: s.clone(),
            not_kinds: both(),
            name: Some(n.into()),
            ..Default::default()
        });
    }
    v
}

/// What SQLTables can't narrow: left to the app.
fn wide() -> Vec<ObjectQuery> {
    let s = Some(SCHEMAS[0].to_string());
    vec![
        ObjectQuery::default(),
        ObjectQuery {
            schema: s.clone(),
            ..Default::default()
        },
        ObjectQuery {
            schema: s.clone(),
            kind: Some("sequence".into()),
            ..Default::default()
        },
        ObjectQuery {
            schema: s,
            kind: Some("procedure".into()),
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("table".into()),
            ..Default::default()
        },
        ObjectQuery {
            pattern: Some("orders".into()),
            ..Default::default()
        },
        ObjectQuery {
            no_schema: true,
            kind: Some("table".into()),
            ..Default::default()
        },
    ]
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

async fn check(c: ConnectionConfig) {
    let mut s = open(&c).await;
    for sql in setup() {
        exec(&mut s, &sql).await.unwrap();
    }
    let all = s.list_objects().await.unwrap();
    let mine = all
        .iter()
        .filter(|o| o.schema.as_deref().is_some_and(|x| SCHEMAS.contains(&x)))
        .count();
    eprintln!("{}: {} objects, {mine} of them here", c.driver, all.len());
    let mut checked = 0;
    for q in narrow() {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    for q in wide() {
        match s.list_objects_page(&q).await {
            Err(Error::Unsupported(_)) => {}
            other => panic!("{q:?}: {other:?}"),
        }
    }
    let tables = ObjectQuery {
        schema: Some(SCHEMAS[0].into()),
        kind: Some("table".into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &tables).await;
    assert!(pages >= 3, "a folder of more than one page: {pages}");
    let p = s.list_objects_page(&tables).await.unwrap();
    assert_eq!(p.objects.len(), 10);
    // `A_B` is a pattern that matches `AXB` too: none of its objects.
    let p = s
        .list_objects_page(&ObjectQuery {
            schema: Some(SCHEMAS[0].into()),
            kind: Some("view".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(p.objects.len(), 9, "{p:?}");
    eprintln!(
        "{}: {checked} filter/size combinations equal, {} left to the app; the tables folder in {pages} pages of 10",
        c.driver,
        wide().len()
    );
}

async fn exercise(c: ConnectionConfig) {
    let mut admin = open(&c).await;
    cleanup(&mut admin).await.unwrap();
    let outcome = tokio::spawn(check(c)).await;
    let cleaned = cleanup(&mut admin).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    cleaned.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn db2() {
    let Some(c) = cfg("DBINE_TEST_ODBC_PAGES_DB2", "db2") else {
        eprintln!("DBINE_TEST_ODBC_PAGES_DB2 not set; skipping");
        return;
    };
    exercise(generic(&c)).await;
    exercise(c).await;
}
