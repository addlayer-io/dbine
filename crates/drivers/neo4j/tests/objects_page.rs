//! The explorer's pages (`list_objects_page`) against real servers: for
//! every filter (each kind, "Otros", searches with capitals, accents,
//! characters from U+E000 up and LIKE characters, lookups) and page size,
//! each page holds what the app pages out of `list_objects`
//! (`objects::page_of`). The test's labels, types, indexes, constraints
//! and trigger all start with `dbpg` and are removed at the end, even when
//! a check fails. Skipped without the variables:
//!
//! ```sh
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 \
//! DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//!   cargo test -p dbine-driver-neo4j --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, ObjectQuery, QueryOutcome, Session};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url
        .rsplit_once('@')
        .map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth
        .and_then(|a| a.split_once(':'))
        .map_or((None, None), |(u, p)| {
            (Some(u.to_string()), Some(p.to_string()))
        });
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: user,
        password: pass,
        ..Default::default()
    }
}

async fn open(id: &str, url: &str) -> Box<dyn Session> {
    dbine_driver_neo4j::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
        .connect(&cfg(id, url), None)
        .await
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out)
        .await
        .map_err(|e| format!("{text}: {e}"))?;
    match out.error.or(out.errors.first().map(|e| e.message.clone())) {
        Some(e) => Err(format!("{text}: {e}")),
        None => Ok(()),
    }
}

/// Labels: a folder of more than one page, capitals that tie ignoring
/// case, accents, characters Neo4j compares by UTF-16 units.
fn labels() -> Vec<String> {
    let mut v: Vec<String> = (0..25).map(|i| format!("dbpg_{i:02}")).collect();
    for l in [
        "dbpgOrders",
        "dbpgorders",
        "dbpg%b",
        "dbpg_b",
        "dbpgÑandú",
        "dbpgñu",
        "dbpgİzmir",
        "dbpgΑΣ",
        "dbpg\u{E000}x",
        "dbpg\u{FF21}x",
        "dbpg\u{10000}x",
    ] {
        v.push(l.into());
    }
    v
}

fn setup(memgraph: bool) -> Vec<String> {
    let mut v: Vec<String> = labels()
        .iter()
        .map(|l| format!("CREATE (:`{l}` {{x: 1, id: 1}})"))
        .collect();
    v.push("CREATE (:dbpg_00)-[:DBPG_R]->(:dbpg_01)".into());
    v.push("CREATE (:dbpg_00)-[:dbpgorders]->(:dbpg_01)".into());
    v.push("CREATE (:dbpg_00)-[:`dbpgñ_rel`]->(:dbpg_01)".into());
    if memgraph {
        v.extend(
            [
                "CREATE INDEX ON :dbpg_02(x)",
                "CREATE INDEX ON :dbpg_03",
                "CREATE CONSTRAINT ON (n:dbpg_04) ASSERT n.id IS UNIQUE",
                "CREATE CONSTRAINT ON (n:dbpg_05) ASSERT EXISTS (n.id)",
                "CREATE TRIGGER dbpg_trigger ON () CREATE AFTER COMMIT EXECUTE RETURN 1",
            ]
            .map(String::from),
        );
    } else {
        v.extend(
            [
                "CREATE INDEX dbpg_idx_a FOR (n:dbpg_02) ON (n.x)",
                "CREATE INDEX `dbpg_IDX_b` FOR ()-[r:DBPG_R]-() ON (r.x)",
                "CREATE TEXT INDEX `dbpg_ñ_idx` FOR (n:dbpg_03) ON (n.x)",
                "CREATE CONSTRAINT dbpg_c FOR (n:dbpg_04) REQUIRE n.id IS UNIQUE",
            ]
            .map(String::from),
        );
    }
    v
}

fn teardown(memgraph: bool) -> Vec<String> {
    let mut v = vec![
        "MATCH (n) WHERE any(l IN labels(n) WHERE l STARTS WITH 'dbpg') DETACH DELETE n"
            .to_string(),
    ];
    if memgraph {
        v.extend(
            [
                "DROP INDEX ON :dbpg_02(x)",
                "DROP INDEX ON :dbpg_03",
                "DROP CONSTRAINT ON (n:dbpg_04) ASSERT n.id IS UNIQUE",
                "DROP CONSTRAINT ON (n:dbpg_05) ASSERT EXISTS (n.id)",
                "DROP TRIGGER dbpg_trigger",
            ]
            .map(String::from),
        );
    } else {
        v.extend(
            [
                "DROP INDEX dbpg_idx_a IF EXISTS",
                "DROP INDEX `dbpg_IDX_b` IF EXISTS",
                "DROP INDEX `dbpg_ñ_idx` IF EXISTS",
                "DROP CONSTRAINT dbpg_c IF EXISTS",
            ]
            .map(String::from),
        );
    }
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
            not_kinds: vec!["label".into(), "procedure".into()],
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("label".into()),
            pattern: Some("1".into()),
            ..Default::default()
        },
        ObjectQuery {
            pattern: Some("orders".into()),
            name: Some("DBPGORDERS".into()),
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
        "DBPG_1",
        "%",
        "_",
        "dbpg%b",
        "ORDERS",
        "ñ",
        "Ñ",
        "i",
        "σ",
        "\u{E000}",
        "\u{FF41}",
        "\u{10000}",
        "zzz",
        "DB.",
        "await",
    ] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in [
        "dbpg_05",
        "DBPGORDERS",
        "dbpgñandú",
        "DBPGİZMIR",
        "dbpg\u{10000}X",
        "db.awaitIndex",
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
        assert!(pages < 200, "{q:?}: no last page");
        match (got.next, want.next) {
            (Some(a), Some(b)) => {
                q.after = Some(a);
                offset.after = Some(b);
            }
            _ => return pages,
        }
    }
}

async fn check(id: &'static str, url: String) {
    let memgraph = id == "memgraph";
    let mut s = open(id, &url).await;
    for c in setup(memgraph) {
        run(&mut s, &c).await.unwrap();
    }
    let all = s.list_objects().await.unwrap();
    let kinds: Vec<String> = dbine_driver_neo4j::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
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
    let folder = ObjectQuery {
        kind: Some("label".into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &folder).await;
    assert!(pages >= 4, "a folder of more than one page: {pages}");
    eprintln!(
        "{id}: {} objects; {checked} filter/size combinations equal; the labels folder in {pages} pages of 10",
        all.len()
    );
}

async fn pages_on(id: &'static str, url: String) {
    let memgraph = id == "memgraph";
    let mut admin = open(id, &url).await;
    eprintln!("{}", admin.server_version().await.unwrap());
    for c in teardown(memgraph) {
        let _ = run(&mut admin, &c).await;
    }
    let outcome = tokio::spawn(check(id, url)).await;
    let mut cleaned = Ok(());
    for c in teardown(memgraph) {
        let r = run(&mut admin, &c).await;
        if cleaned.is_ok() {
            cleaned = r;
        }
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    cleaned.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_URL") else {
        eprintln!("DBINE_TEST_NEO4J_URL not set; skipping");
        return;
    };
    pages_on("neo4j", url).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn memgraph_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_MEMGRAPH_URL") else {
        eprintln!("DBINE_TEST_MEMGRAPH_URL not set; skipping");
        return;
    };
    pages_on("memgraph", url).await;
}
