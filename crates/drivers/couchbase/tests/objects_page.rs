//! The explorer's pages (`list_objects_page`) against real servers: for
//! every filter (each kind, each scope, no schema, "Otros", searches,
//! lookups) each page equals what the app pages out of `list_objects`
//! (`objects::page_of`), and the 25 collections of a scope come in pages
//! of 10. The objects live in a scratch bucket (`dbine_pages`), dropped at
//! the end even when a check fails. Each test is skipped without its
//! variable:
//!
//! ```sh
//! docker start dbine-test-couchbase      # or dbine-test-couchbase-ee
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//! DBINE_TEST_COUCHBASE_EE_URL=http://localhost:25993 DBINE_TEST_COUCHBASE_EE_MGMT_PORT=25991 \
//!   cargo test -p dbine-driver-couchbase --test objects_page -- --ignored --nocapture --test-threads=1
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{kinds, ConnectionConfig, DbObject, ObjectQuery, QueryOutcome, Session};
use std::time::Duration;

const USER: &str = "Administrator";
const PASS: &str = "secreto1";
const BUCKET: &str = "dbine_pages";

fn cfg(url: &str, mgmt: &str) -> ConnectionConfig {
    let url = reqwest::Url::parse(url).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), mgmt.into());
    c
}

/// Provision the node (data, query and index services) if it isn't yet.
async fn init(c: &ConnectionConfig) {
    let mgmt = format!("http://{}:{}", c.host, c.options["mgmt_port"]);
    let http = reqwest::Client::new();
    let ok = http
        .get(format!("{mgmt}/pools/default"))
        .basic_auth(USER, Some(PASS))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if !ok {
        let r = http
            .post(format!("{mgmt}/clusterInit"))
            .form(&[
                ("hostname", "127.0.0.1"),
                ("services", "kv,n1ql,index"),
                ("memoryQuota", "512"),
                ("indexMemoryQuota", "256"),
                ("username", USER),
                ("password", PASS),
                ("port", "SAME"),
                ("indexerStorageMode", "forestdb"),
            ])
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "clusterInit");
    }
    for _ in 0..60 {
        let q = http
            .post(format!("http://{}:{}/query/service", c.host, c.port))
            .basic_auth(USER, Some(PASS))
            .form(&[("statement", "SELECT RAW 1")])
            .send()
            .await;
        if q.map(|r| r.status().is_success()).unwrap_or(false) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("the query service didn't come up");
}

async fn run(s: &mut Box<dyn Session>, n1ql: &str) -> dbine_driver::Result<()> {
    let mut out = QueryOutcome::default();
    s.execute(n1ql, 10, &mut out).await
}

/// Pages `q` both ways, `size` at a time: the same objects on each page,
/// and the same pages. Gives how many there were.
async fn same_pages(
    s: &mut Box<dyn Session>,
    all: &[DbObject],
    mut q: ObjectQuery,
    size: u32,
) -> usize {
    q.limit = size;
    let (mut got_q, mut want_q) = (q.clone(), q.clone());
    let mut pages = 0;
    loop {
        let want = page_of(all, &want_q);
        let got = s
            .list_objects_page(&got_q)
            .await
            .unwrap_or_else(|e| panic!("{got_q:?}: {e}"));
        assert_eq!(got.objects, want.objects, "page {pages} of {q:?}");
        assert_eq!(
            got.next.is_some(),
            want.next.is_some(),
            "page {pages} of {q:?}"
        );
        pages += 1;
        match (got.next, want.next) {
            (Some(g), Some(w)) => {
                got_q.after = Some(g);
                want_q.after = Some(w);
            }
            _ => return pages,
        }
    }
}

fn q(f: impl FnOnce(&mut ObjectQuery)) -> ObjectQuery {
    let mut q = ObjectQuery::default();
    f(&mut q);
    q
}

/// The scratch bucket's objects, then every filter in pages of 7 and the
/// big scope's collections in pages of 10.
async fn check(c: ConnectionConfig) {
    let d = dbine_driver_couchbase::drivers().remove(0);
    let mut s = d.connect(&c, Some(BUCKET)).await.expect("connect");
    // 25 collections in mixed case in `Big`; in `Ventas`, two that differ
    // only in case, each with a primary index (one name, two collections).
    let mut ddl: Vec<String> = vec![
        format!("CREATE SCOPE {BUCKET}.Big"),
        format!("CREATE SCOPE {BUCKET}.Ventas"),
    ];
    for i in 0..25 {
        let name = if i % 2 == 0 {
            format!("Pedidos_{i:02}")
        } else {
            format!("pedidos_{i:02}_x")
        };
        ddl.push(format!("CREATE COLLECTION {BUCKET}.Big.{name}"));
    }
    for name in ["Dup", "dup", "notas"] {
        ddl.push(format!("CREATE COLLECTION {BUCKET}.Ventas.`{name}`"));
    }
    run(&mut s, &ddl.join(";\n"))
        .await
        .expect("scopes and collections");
    // Wait for the query service to know them.
    for _ in 0..60 {
        let p = s
            .list_objects_page(&q(|q| q.kind = Some(kinds::COLLECTION.into())))
            .await
            .unwrap();
        if p.objects.len() >= 29 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    run(
        &mut s,
        &format!(
            "CREATE PRIMARY INDEX ON {BUCKET}.Ventas.`Dup`;\n\
             CREATE PRIMARY INDEX ON {BUCKET}.Ventas.`dup`;\n\
             CREATE INDEX ix_a ON {BUCKET}.Ventas.notas(a);\n\
             CREATE INDEX Ix_A ON {BUCKET}.Big.Pedidos_00(a)"
        ),
    )
    .await
    .expect("indexes");
    let functions = run(
        &mut s,
        &format!(
            "CREATE FUNCTION {BUCKET}.Ventas.f_ventas(x) {{ x }};\n\
             CREATE FUNCTION dbine_pg_global(x) {{ x }}"
        ),
    )
    .await;
    eprintln!(
        "functions: {}",
        functions.map_or_else(|e| e.to_string(), |_| "made".into())
    );

    let all = s.list_objects().await.expect("list_objects");
    let count = |k: &str| all.iter().filter(|o| o.kind == k).count();
    assert_eq!(count(kinds::COLLECTION), 29, "{all:?}");
    assert_eq!(count(kinds::INDEX), 4, "{all:?}");
    eprintln!(
        "{} objects, {} functions",
        all.len(),
        count(kinds::FUNCTION)
    );

    let big = q(|q| {
        q.schema = Some(format!("{BUCKET}.Big"));
        q.kind = Some(kinds::COLLECTION.into());
    });
    assert_eq!(same_pages(&mut s, &all, big, 10).await, 3, "25 by 10");
    let declared: Vec<String> = [kinds::COLLECTION, kinds::INDEX, kinds::FUNCTION]
        .map(String::from)
        .to_vec();
    let mut queries = vec![
        ObjectQuery::default(),
        q(|q| q.not_kinds = declared.clone()),
        q(|q| q.not_kinds = vec![kinds::COLLECTION.into()]),
        q(|q| q.not_kinds = vec![kinds::INDEX.into()]),
        q(|q| q.no_schema = true),
        q(|q| {
            q.no_schema = true;
            q.schema = Some(format!("{BUCKET}.Big"));
        }),
        q(|q| q.schema = Some(format!("{BUCKET}.big"))),
        q(|q| q.schema = Some("nope".into())),
        q(|q| q.kind = Some(kinds::TABLE.into())),
        q(|q| {
            q.schema = Some(format!("{BUCKET}.Ventas"));
            q.pattern = Some("D".into());
        }),
        q(|q| {
            q.kind = Some(kinds::INDEX.into());
            q.name = Some("IX_A".into());
        }),
    ];
    for k in &declared {
        queries.push(q(|q| q.kind = Some(k.clone())));
    }
    for s in ["_default", "Big", "Ventas"] {
        queries.push(q(|q| q.schema = Some(format!("{BUCKET}.{s}"))));
        for k in &declared {
            queries.push(q(|q| {
                q.schema = Some(format!("{BUCKET}.{s}"));
                q.kind = Some(k.clone());
            }));
        }
    }
    for p in [
        "pedidos",
        "PEDIDOS_1",
        "_",
        "%",
        "#",
        "dup",
        "ix",
        "f_",
        "é",
        "",
        " ",
        "zzz",
        "\"",
        "\\",
    ] {
        queries.push(q(|q| q.pattern = Some(p.into())));
    }
    for n in [
        "dup",
        "DUP",
        "#primary",
        "ix_a",
        "pedidos_01_x",
        "_default",
        "f_ventas",
        "DBINE_PG_GLOBAL",
        "nope",
        "",
    ] {
        queries.push(q(|q| q.name = Some(n.into())));
    }
    let mut pages = 0;
    for q in &queries {
        pages += same_pages(&mut s, &all, q.clone(), 7).await;
    }
    eprintln!(
        "{} queries, {pages} pages of 7: same as page_of",
        queries.len()
    );
}

async fn on_scratch_bucket(url: &str, mgmt: &str) {
    let c = cfg(url, mgmt);
    init(&c).await;
    let d = dbine_driver_couchbase::drivers().remove(0);
    let mut admin = d.connect(&c, None).await.expect("connect");
    if admin
        .list_databases()
        .await
        .unwrap()
        .contains(&BUCKET.to_string())
    {
        admin.drop_database(BUCKET).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = run(&mut admin, "DROP FUNCTION dbine_pg_global").await;
    admin.create_database(BUCKET).await.unwrap();
    let outcome = tokio::spawn(check(c.clone())).await;
    let _ = run(&mut admin, "DROP FUNCTION dbine_pg_global").await;
    admin.drop_database(BUCKET).await.unwrap();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test]
#[ignore]
async fn couchbase_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_COUCHBASE_URL") else {
        return;
    };
    let mgmt = std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into());
    on_scratch_bucket(&url, &mgmt).await;
}

#[tokio::test]
#[ignore]
async fn couchbase_ee_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_COUCHBASE_EE_URL") else {
        return;
    };
    let mgmt = std::env::var("DBINE_TEST_COUCHBASE_EE_MGMT_PORT").unwrap_or_else(|_| "8091".into());
    on_scratch_bucket(&url, &mgmt).await;
}
