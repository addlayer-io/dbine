//! The explorer's pages (`list_objects_page`) against a real Dremio OSS: for
//! every filter (each kind, a schema, no schema, "Otros", searches, lookups)
//! and page size, each page equals the app's own paging of `list_objects`
//! (`objects::page_of`), cursor included. In a scratch space
//! (`dbine_pages`, views in it and in a folder of it) and in `$scratch`
//! (tables, `dbine_pages_*`), all dropped at the end even when a check
//! fails. Skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 \
//!   cargo test -p dbine-driver-dremio --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const USER: &str = "dbine";
const PASS: &str = "secreto123";
const SPACE: &str = "dbine_pages";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    })
}

/// A fresh Dremio has no users: create the first one (ignored if it exists).
async fn bootstrap(c: &ConnectionConfig) {
    let http = reqwest::Client::new();
    let base = format!("http://{}:{}", c.host, c.port);
    for _ in 0..60 {
        if http
            .get(format!("{base}/apiv2/server_status"))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = http
        .put(format!("{base}/apiv2/bootstrap/firstuser"))
        .header("Authorization", "_dremionull")
        .json(&json!({"userName": USER, "firstName": "DB", "lastName": "Ine", "email": "dbine@example.com", "createdAt": 1700000000000u64, "password": PASS}))
        .send()
        .await;
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

/// The space's views: 25 numbered (a folder of three pages of 10), LIKE's
/// wildcards, a non-ASCII letter, and names its folder and `$scratch`
/// also have.
fn space_views() -> Vec<String> {
    let mut v: Vec<String> = (0..25).map(|i| format!("v_{i:02}")).collect();
    for n in ["a%b", "axb", "dup", "Ünïcode", "Orders_old"] {
        v.push(n.into());
    }
    v
}

fn folder_views() -> Vec<String> {
    ["dup", "orders", "v_x"].map(String::from).to_vec()
}

fn scratch_tables() -> Vec<String> {
    let mut v: Vec<String> = (0..12).map(|i| format!("dbine_pages_t{i:02}")).collect();
    v.push("dbine_pages_dup".into());
    v.push("dbine_pages_Orders".into());
    v
}

fn setup() -> Vec<String> {
    let mut v: Vec<String> = space_views()
        .iter()
        .map(|n| format!("CREATE VIEW {SPACE}.\"{n}\" AS SELECT 1 AS x"))
        .collect();
    v.extend(
        folder_views()
            .iter()
            .map(|n| format!("CREATE VIEW {SPACE}.sub.\"{n}\" AS SELECT 1 AS x")),
    );
    v.extend(
        scratch_tables()
            .iter()
            .map(|t| format!("CREATE TABLE \"$scratch\".\"{t}\" (id INT)")),
    );
    v.push(format!(
        "CREATE VIEW {SPACE}.\"dbine_pages_dup\" AS SELECT 1 AS x"
    ));
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
            not_kinds: vec!["table".into()],
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("nope".into()),
            ..Default::default()
        },
    ];
    for schema in [SPACE, "dbine_pages.sub", "$scratch", "DBINE_PAGES", "nope"] {
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
    for k in kinds {
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            ..Default::default()
        });
    }
    for p in ["V_1", "%", "_", "a%b", "ORDERS", "dup", "ÜNÏ", "zzz", ""] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["V_05", "orders", "DUP", "a%b", "dbine_pages_orders", "nope"] {
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

/// Every filter and size in the container `context`; how many combinations.
async fn check_in(d: &Arc<dyn Driver>, c: &ConnectionConfig, context: &str) -> usize {
    let mut s = d.connect(c, Some(context)).await.expect("connect");
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
    eprintln!("{context}: {} objects, kinds {kinds:?}", all.len());
    let mut checked = 0;
    for q in queries(&kinds, &declared) {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    checked
}

/// The space's folder `sub`, through the REST API (Dremio OSS has no SQL
/// for a space's folders).
async fn create_folder(c: &ConnectionConfig) {
    let http = reqwest::Client::new();
    let base = format!("http://{}:{}", c.host, c.port);
    let login: serde_json::Value = http
        .post(format!("{base}/apiv2/login"))
        .json(&json!({"userName": USER, "password": PASS}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = login["token"].as_str().unwrap();
    let r = http
        .post(format!("{base}/api/v3/catalog"))
        .header("Authorization", format!("_dremio{token}"))
        .json(&json!({"entityType": "folder", "path": [SPACE, "sub"]}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{:?}", r.text().await);
}

async fn check(d: Arc<dyn Driver>, c: ConnectionConfig) {
    create_folder(&c).await;
    let mut s = d.connect(&c, Some("$scratch")).await.expect("connect");
    for sql in setup() {
        if let Err(e) = run(&mut s, &sql).await {
            panic!("{e}");
        }
    }
    let checked = check_in(&d, &c, SPACE).await + check_in(&d, &c, "$scratch").await;
    let mut s = d.connect(&c, Some(SPACE)).await.expect("connect");
    let all = s.list_objects().await.unwrap();
    let folder = ObjectQuery {
        schema: Some(SPACE.into()),
        kind: Some("view".into()),
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
    eprintln!("{checked} filter/size combinations equal; the views folder in {pages} pages of 10");
}

/// The space (with what it holds) and the `$scratch` tables.
async fn cleanup(s: &mut Box<dyn Session>) -> Result<(), String> {
    let mut r = Ok(());
    for t in scratch_tables() {
        if let Err(e) = run(s, &format!("DROP TABLE IF EXISTS \"$scratch\".\"{t}\"")).await {
            r = Err(e);
        }
    }
    if s.list_databases()
        .await
        .map_err(|e| e.to_string())?
        .iter()
        .any(|d| d == SPACE)
    {
        s.drop_database(SPACE).await.map_err(|e| e.to_string())?;
    }
    r
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn dremio() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_DREMIO_URL not set; skipping");
        return;
    };
    bootstrap(&c).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    let mut admin = d.connect(&c, None).await.expect("connect");
    eprintln!("{}", admin.server_version().await.unwrap());
    let _ = cleanup(&mut admin).await;
    admin.create_database(SPACE).await.unwrap();
    let outcome = tokio::spawn(check(d.clone(), c)).await;
    let dropped = cleanup(&mut admin).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    dropped.unwrap();
}
