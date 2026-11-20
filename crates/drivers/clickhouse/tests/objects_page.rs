//! The explorer's pages (`list_objects_page`) against real servers: for
//! every filter (each kind, no schema, a schema, "Otros", searches,
//! lookups) and page size, each page equals the app's own paging of
//! `list_objects` (`objects::page_of`), cursor included. ClickHouse's
//! objects live in a scratch database (`dbine_pages`); Proton's user can't
//! create one, so there they are `dbine_pages_*` in `default`. The SQL
//! functions (global) are `dbine_pages_*` too. Everything is dropped at the
//! end even when a check fails. Each test reads its variable and is skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//! DBINE_TEST_CLICKHOUSE_22_URL=http://dbine:dbine@localhost:25124 \
//! DBINE_TEST_TIMEPLUS_URL=http://localhost:25119 \
//!   cargo test -p dbine-driver-clickhouse --test objects_page -- --ignored --nocapture --test-threads 1
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::sync::Arc;

const DB: &str = "dbine_pages";
/// Every object's name starts with it (Proton's share `default`).
const P: &str = "dbine_pages_";

fn cfg(env: &str, id: &str, database: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: id.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        database: database.into(),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
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

/// The names of the "tables" (streams on Proton): 25 numbered (a folder of
/// three pages of 10), names that differ only in case, LIKE's wildcards,
/// a non-ASCII letter, and one a function also has.
fn table_names() -> Vec<String> {
    let mut v: Vec<String> = (0..25).map(|i| format!("{P}t_{i:02}")).collect();
    for n in [
        "Orders",
        "orders",
        "a%b",
        "axb",
        "T_Mixed",
        "Ünïcode",
        "dup",
    ] {
        v.push(format!("{P}{n}"));
    }
    v
}

fn function_names() -> Vec<String> {
    vec![
        format!("{P}f1"),
        format!("{}F2", P.to_uppercase()),
        format!("{P}dup"),
    ]
}

/// What each flavor creates, and how it drops it.
struct Setup {
    create: Vec<String>,
    drop: Vec<String>,
    /// The kind of [`table_names`]' objects.
    tables_kind: &'static str,
}

fn clickhouse_setup() -> Setup {
    let mut create: Vec<String> = table_names()
        .iter()
        .map(|t| format!("CREATE TABLE `{t}` (id UInt64, v String) ENGINE = Memory"))
        .collect();
    create.extend((0..9).map(|i| format!("CREATE VIEW `{P}v_{i}` AS SELECT 1 AS x")));
    create.push(format!(
        "CREATE MATERIALIZED VIEW `{P}mv_a` ENGINE = Memory AS SELECT id FROM `{P}t_00`"
    ));
    create.push(format!(
        "CREATE DICTIONARY `{P}dict_a` (id UInt64, v String) PRIMARY KEY id \
         SOURCE(CLICKHOUSE(TABLE '{P}t_01')) LAYOUT(FLAT()) LIFETIME(0)"
    ));
    create.extend(
        function_names()
            .iter()
            .map(|f| format!("CREATE FUNCTION `{f}` AS (x) -> x + 1")),
    );
    let mut drop = vec![format!("DROP DATABASE IF EXISTS {DB}")];
    drop.extend(
        function_names()
            .iter()
            .map(|f| format!("DROP FUNCTION IF EXISTS `{f}`")),
    );
    Setup {
        create,
        drop,
        tables_kind: "table",
    }
}

fn timeplus_setup() -> Setup {
    let mut create: Vec<String> = table_names()
        .iter()
        .map(|t| format!("CREATE STREAM `{t}` (id int)"))
        .collect();
    create.push(format!("CREATE RANDOM STREAM `{P}r_a` (id int)"));
    create.push(format!(
        "CREATE STREAM `{P}mt_a` (id int) ENGINE = MergeTree() ORDER BY id"
    ));
    create.extend(
        (0..9).map(|i| format!("CREATE VIEW `{P}v_{i}` AS SELECT id FROM table(`{P}t_00`)")),
    );
    create.push(format!(
        "CREATE MATERIALIZED VIEW `{P}mv_a` AS SELECT id FROM `{P}t_00`"
    ));
    create.extend(
        function_names()
            .iter()
            .map(|f| format!("CREATE FUNCTION `{f}` AS (x) -> x + 1")),
    );
    let mut drop = vec![format!("DROP VIEW IF EXISTS `{P}mv_a`")];
    drop.extend((0..9).map(|i| format!("DROP VIEW IF EXISTS `{P}v_{i}`")));
    drop.extend(
        table_names()
            .into_iter()
            .chain([format!("{P}r_a"), format!("{P}mt_a")])
            .map(|t| format!("DROP STREAM IF EXISTS `{t}`")),
    );
    drop.extend(
        function_names()
            .iter()
            .map(|f| format!("DROP FUNCTION IF EXISTS `{f}`")),
    );
    Setup {
        create,
        drop,
        tables_kind: "stream",
    }
}

/// The filters the explorer sends: folders, "Otros", searches, lookups.
fn queries(kinds: &[String], declared: &[String]) -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
        ObjectQuery {
            no_schema: true,
            ..Default::default()
        },
        // Nothing has a schema here.
        ObjectQuery {
            schema: Some(DB.into()),
            ..Default::default()
        },
        ObjectQuery {
            no_schema: true,
            schema: Some(DB.into()),
            kind: Some("view".into()),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: declared.to_vec(),
            no_schema: true,
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec!["table".into(), "stream".into()],
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("function".into()),
            pattern: Some(P.into()),
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("nope".into()),
            ..Default::default()
        },
    ];
    for k in kinds {
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            no_schema: true,
            ..Default::default()
        });
    }
    for p in ["T_1", "%", "_", "a%b", "ORDERS", "dup", "ÜNÏ", "zzz", ""] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in [
        format!("{P}T_05"),
        format!("{P}orders"),
        format!("{P}DUP"),
        format!("{P}a%b"),
        "nope".into(),
    ] {
        v.push(ObjectQuery {
            name: Some(n),
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

async fn check(d: Arc<dyn Driver>, cfg: ConnectionConfig, setup: Vec<String>, tables_kind: &str) {
    let mut s = d.connect(&cfg, None).await.expect("connect");
    for sql in &setup {
        if let Err(e) = run(&mut s, sql).await {
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
    eprintln!("{}: {} objects, kinds {kinds:?}", d.info().id, all.len());
    let mut checked = 0;
    for q in queries(&kinds, &declared) {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let tables = ObjectQuery {
        kind: Some(tables_kind.into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &tables).await;
    assert!(pages >= 3, "a folder of more than one page: {pages}");
    let p = s.list_objects_page(&tables).await.unwrap();
    assert_eq!(p.objects.len(), 10);
    // A cursor that isn't the driver's (the app's own offset) goes back to
    // the app.
    let foreign = ObjectQuery {
        after: Some("10".into()),
        ..tables
    };
    assert!(matches!(
        s.list_objects_page(&foreign).await,
        Err(dbine_driver::Error::Unsupported(_))
    ));
    eprintln!(
        "{}: {checked} filter/size combinations equal; the {tables_kind} folder in {pages} pages of 10",
        d.info().id
    );
}

/// Creates the objects, checks every page and drops them, also when a check
/// fails. `own_db`: in a scratch database of its own.
async fn exercise(id: &str, env: &str, own_db: bool, setup: Setup) {
    let database = if own_db { DB } else { "default" };
    let Some(cfg) = cfg(env, id, database) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    let admin_cfg = ConnectionConfig {
        database: "default".into(),
        ..cfg.clone()
    };
    let mut admin = d.connect(&admin_cfg, None).await.expect("connect");
    eprintln!("{id}: {}", admin.server_version().await.unwrap());
    for sql in &setup.drop {
        run(&mut admin, sql).await.unwrap();
    }
    if own_db {
        run(&mut admin, &format!("CREATE DATABASE {DB}"))
            .await
            .unwrap();
    }
    let outcome = tokio::spawn(check(d.clone(), cfg, setup.create, setup.tables_kind)).await;
    let mut dropped = Ok(());
    for sql in &setup.drop {
        if let Err(e) = run(&mut admin, sql).await {
            dropped = Err(e);
        }
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    dropped.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse() {
    exercise(
        "clickhouse",
        "DBINE_TEST_CLICKHOUSE_URL",
        true,
        clickhouse_setup(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse_22() {
    exercise(
        "clickhouse",
        "DBINE_TEST_CLICKHOUSE_22_URL",
        true,
        clickhouse_setup(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timeplus() {
    exercise(
        "timeplus",
        "DBINE_TEST_TIMEPLUS_URL",
        false,
        timeplus_setup(),
    )
    .await;
}
