//! The explorer's pages (`list_objects_page`) against a real OrientDB: for
//! every filter (each kind, "Otros", searches with capitals, accents and
//! characters from U+E000 up, lookups) and page size, each page holds what
//! the app pages out of `list_objects` (`objects::page_of`), on a writable
//! session and on a read-only one (which reads by GET). The objects live in
//! a database of their own, dropped at the end even when a check fails;
//! skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 \
//!   cargo test -p dbine-driver-orientdb --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::sync::Arc;

const DB: &str = "dbine_pages";

fn cfg(url: &str, read_only: bool) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        read_only,
        ..Default::default()
    }
}

fn setup() -> Vec<String> {
    let mut v: Vec<String> = (0..25)
        .map(|i| format!("CREATE CLASS dbpg_v_{i:02} EXTENDS V"))
        .collect();
    v.extend(
        [
            "CREATE CLASS Orders EXTENDS V",
            "CREATE CLASS `Émployé` EXTENDS Orders",
            "CREATE CLASS knows EXTENDS E",
            "CREATE CLASS `KNOWS_2` EXTENDS knows",
            "CREATE CLASS Doc",
            "CREATE CLASS `a%b`",
            "CREATE CLASS a_b",
            "CREATE CLASS `\u{FF21}x`",
            "CREATE PROPERTY Doc.x STRING",
            "CREATE INDEX Doc.x ON Doc (x) NOTUNIQUE",
            "CREATE INDEX `Ñidx` ON Doc (x) NOTUNIQUE",
            "CREATE INDEX `\u{FF21}x.i` ON Doc (x) NOTUNIQUE",
            "CREATE INDEX dbpg_manual DICTIONARY STRING",
            "CREATE FUNCTION fAbc 'return 1' LANGUAGE javascript",
            "CREATE FUNCTION Fb 'return 1' LANGUAGE javascript",
            "CREATE SEQUENCE seqA TYPE ORDERED",
            "CREATE SEQUENCE `ñseq` TYPE ORDERED",
            "CREATE SEQUENCE `\u{FF21}xseq` TYPE ORDERED",
        ]
        .map(String::from),
    );
    v
}

fn queries(kinds: &[String]) -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
        ObjectQuery {
            no_schema: true,
            ..Default::default()
        },
        ObjectQuery {
            schema: Some("x".into()),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: kinds.to_vec(),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec!["vertex".into(), "index".into()],
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("vertex".into()),
            pattern: Some("1".into()),
            ..Default::default()
        },
        ObjectQuery {
            pattern: Some("orders".into()),
            name: Some("ORDERS".into()),
            ..Default::default()
        },
    ];
    for k in kinds {
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            ..Default::default()
        });
    }
    for p in [
        "DBPG_V_1", "%", "_", "a%b", "ORDERS", "é", "É", "ñ", "\u{FF41}", ".", "'", "\\", "zzz",
        "o",
    ] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in [
        "dbpg_v_05",
        "orders",
        "ÉMPLOYÉ",
        "doc.x",
        "\u{FF41}X",
        "FABC",
        "Seqa",
        "nope",
    ] {
        v.push(ObjectQuery {
            name: Some(n.into()),
            ..Default::default()
        });
    }
    v
}

/// Every page of `q` from the driver and out of the whole list: the same
/// objects in the same pages, to the last. Gives how many pages.
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

async fn check(d: Arc<dyn Driver>, url: String) {
    let mut s = d.connect(&cfg(&url, false), Some(DB)).await.unwrap();
    for sql in setup() {
        let mut out = QueryOutcome::default();
        s.execute(&sql, 10, &mut out)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
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
    let read_only = d.connect(&cfg(&url, true), Some(DB)).await.unwrap();
    for (mode, mut s) in [("writable", s), ("read-only", read_only)] {
        let mut checked = 0;
        for q in queries(&kinds) {
            for limit in [7, 10, 0] {
                same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
                checked += 1;
            }
        }
        let folder = ObjectQuery {
            kind: Some("vertex".into()),
            limit: 10,
            ..Default::default()
        };
        let pages = same_pages(&mut s, &all, &folder).await;
        assert!(pages >= 3, "a folder of more than one page: {pages}");
        eprintln!(
            "{mode}: {} objects; {checked} filter/size combinations equal; the vertices folder in {pages} pages of 10",
            all.len()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pages_equal_the_whole_list() {
    let Ok(url) = std::env::var("DBINE_TEST_ORIENTDB_URL") else {
        eprintln!("DBINE_TEST_ORIENTDB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_orientdb::drivers().remove(0);
    let mut admin = d.connect(&cfg(&url, false), None).await.unwrap();
    eprintln!("{}", admin.server_version().await.unwrap());
    let _ = admin.drop_database(DB).await;
    admin.create_database(DB).await.unwrap();
    let outcome = tokio::spawn(check(d.clone(), url)).await;
    let dropped = admin.drop_database(DB).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    dropped.unwrap();
}
