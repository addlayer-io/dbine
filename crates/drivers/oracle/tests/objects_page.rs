//! The explorer's pages (`list_objects_page`) against a real server: for
//! every filter (each kind, the schema's own objects, PUBLIC's synonyms,
//! "Otros", searches with wildcards, lookups) and page size, each page
//! equals the app's own paging of `list_objects` (`objects::page_of`),
//! cursor included. The objects live in a scratch schema (and two public
//! synonyms), dropped at the end even when a check fails. As a user that
//! can create schemas (`DBINE_TEST_ORACLE_ADMIN_URL`), skipped without it:
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::sync::Arc;

const SCHEMA: &str = "DBINE_PAGES";
const PUBLIC_SYNONYMS: [&str; 2] = ["DBINE_PAGES_PUB1", "dbine_pages_pub2"];

fn config(url: &str) -> ConnectionConfig {
    let rest = url
        .strip_prefix("oracle://")
        .expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
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
        .map(|i| format!("CREATE TABLE t_{i:02} (id NUMBER PRIMARY KEY)"))
        .collect();
    for t in ["\"Orders\"", "orders", "\"a%b\"", "axb", "\"t_lower\""] {
        v.push(format!("CREATE TABLE {t} (id NUMBER)"));
    }
    v.extend((0..9).map(|i| format!("CREATE VIEW v_{i} AS SELECT 1 AS x FROM dual")));
    v.extend(
        [
            "CREATE MATERIALIZED VIEW mv_a AS SELECT COUNT(*) AS n FROM t_00",
            "CREATE PROCEDURE p_a AS BEGIN NULL; END;",
            "CREATE FUNCTION f_a RETURN NUMBER AS BEGIN RETURN 1; END;",
            "CREATE PACKAGE pk_a AS PROCEDURE x; END pk_a;",
            "CREATE TRIGGER tr_a BEFORE INSERT ON t_00 FOR EACH ROW BEGIN NULL; END;",
            "CREATE SEQUENCE s_a",
            "CREATE SEQUENCE \"S_b\"",
            "CREATE SYNONYM syn_a FOR t_01",
            "CREATE TYPE ty_a AS OBJECT (x NUMBER);",
        ]
        .map(String::from),
    );
    v.extend(
        PUBLIC_SYNONYMS
            .iter()
            .map(|p| format!("CREATE PUBLIC SYNONYM \"{p}\" FOR {SCHEMA}.t_02")),
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
            schema: Some("PUBLIC".into()),
            kind: Some("synonym".into()),
            ..Default::default()
        },
        ObjectQuery {
            schema: Some(SCHEMA.into()),
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
    for p in ["T_1", "%", "_", "a%b", "!", "ORDERS", "pub", "zzz"] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["t_05", "orders", "Dbine_Pages_Pub2", "nope", "a%b"] {
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
    let mut s = d.connect(&cfg, Some(SCHEMA)).await.unwrap();
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
    let public = all
        .iter()
        .filter(|o| o.schema.as_deref() == Some("PUBLIC"))
        .count();
    assert_eq!(public, 2, "{all:?}");
    let mut checked = 0;
    for q in queries(&kinds) {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let tables = ObjectQuery {
        kind: Some("table".into()),
        no_schema: true,
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

async fn drop_public_synonyms(admin: &mut Box<dyn Session>) {
    for p in PUBLIC_SYNONYMS {
        let mut quiet = QueryOutcome::default();
        let _ = admin
            .execute(&format!("DROP PUBLIC SYNONYM \"{p}\""), 10, &mut quiet)
            .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pages_equal_the_whole_list() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let cfg = config(&url);
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut admin: Box<dyn Session> = d.connect(&cfg, None).await.unwrap();
    eprintln!("{}", admin.server_version().await.unwrap());
    drop_public_synonyms(&mut admin).await;
    let _ = admin.drop_database(SCHEMA).await;
    admin.create_database(SCHEMA).await.unwrap();
    run(
        &mut admin,
        &format!("GRANT CREATE TABLE, CREATE MATERIALIZED VIEW TO {SCHEMA}"),
    )
    .await;
    let outcome = tokio::spawn(check(d.clone(), cfg)).await;
    drop_public_synonyms(&mut admin).await;
    let dropped = admin.drop_database(SCHEMA).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    dropped.unwrap();
}
