//! The explorer's pages (`list_objects_page`) against a real server: for
//! every filter (each kind, a schema, no schema, "Otros", searches with
//! wildcards, lookups) and page size, each page equals the app's own paging
//! of `list_objects` (`objects::page_of`), cursor included. The objects
//! live in a database file of their own next to `DBINE_TEST_FIREBIRD_URL`'s,
//! dropped at the end even when a check fails; skipped without it:
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::collections::BTreeMap;
use std::sync::Arc;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url.strip_prefix("firebird://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, path) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(
        out.error.is_none() && out.errors.is_empty(),
        "{sql}: {:?} {:?}",
        out.error,
        out.errors
    );
}

fn setup() -> Vec<String> {
    let mut v: Vec<String> = (0..25)
        .map(|i| format!("CREATE TABLE t_{i:02} (id INTEGER NOT NULL PRIMARY KEY)"))
        .collect();
    for t in ["\"Orders\"", "orders", "\"a%b\"", "axb", "\"t_lower\""] {
        v.push(format!("CREATE TABLE {t} (id INTEGER)"));
    }
    v.extend((0..9).map(|i| format!("CREATE VIEW v_{i} AS SELECT 1 AS x FROM RDB$DATABASE")));
    v.extend(
        [
            "CREATE PROCEDURE p_a AS BEGIN END",
            "CREATE PROCEDURE \"P_b\" AS BEGIN END",
            "CREATE FUNCTION f_a RETURNS INTEGER AS BEGIN RETURN 1; END",
            "CREATE PACKAGE pk_a AS BEGIN PROCEDURE x; END",
            "CREATE TRIGGER tr_a FOR t_00 BEFORE INSERT AS BEGIN END",
            "CREATE TRIGGER tr_conn ON CONNECT AS BEGIN END",
            "CREATE SEQUENCE s_a",
            "CREATE SEQUENCE \"S_b\"",
            "CREATE DOMAIN d_a AS INTEGER",
        ]
        .map(String::from),
    );
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
            schema: Some("PUBLIC".into()),
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
    for p in ["T_1", "%", "_", "a%b", "!", "ORDERS", "zzz"] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["t_05", "orders", "s_b", "nope", "a%b"] {
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
        let want = page_of(all, &q);
        assert_eq!(got, want, "{q:?}");
        pages += 1;
        assert!(pages < 100, "{q:?}: no last page");
        match got.next {
            Some(n) => q.after = Some(n),
            None => return pages,
        }
    }
}

async fn check(d: Arc<dyn Driver>, cfg: ConnectionConfig) {
    let mut s = d.connect(&cfg, None).await.unwrap();
    for sql in setup() {
        run(&mut s, &sql).await;
    }
    let all = s.list_objects().await.unwrap();
    let kinds: Vec<String> = d
        .info()
        .object_kinds
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    for k in &kinds {
        assert!(all.iter().any(|o| &o.kind == k), "no {k}: {all:?}");
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
    let p = s.list_objects_page(&tables).await.unwrap();
    assert_eq!(p.objects.len(), 10);
    assert_eq!(p.objects[0].name, "a%b");
    eprintln!(
        "{} objects; {checked} filter/size combinations equal; the tables folder in {pages} pages of 10",
        all.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pages_equal_the_whole_list() {
    let Some(base) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    let mut admin = d.connect(&base, None).await.unwrap();
    eprintln!("{}", admin.server_version().await.unwrap());
    let dir = base.database.rsplit_once('/').unwrap().0.to_string();
    let path = format!("{dir}/dbine_pages.fdb");
    let _ = admin.drop_database(&path).await;
    let options: BTreeMap<String, String> = [("folder", dir.as_str()), ("charset", "utf8")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    admin
        .create_database_with("dbine_pages", &options)
        .await
        .unwrap();
    let cfg = ConnectionConfig {
        database: path.clone(),
        ..base.clone()
    };
    let outcome = tokio::spawn(check(d.clone(), cfg)).await;
    let dropped = admin.drop_database(&path).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    dropped.unwrap();
}
