//! "Renombrar…" against real servers (ignored by default), the same
//! containers as `integration.rs`:
//!
//! ```sh
//! DBINE_TEST_SOLR_URL=http://localhost:25522 DBINE_TEST_SOLRCLOUD_URL=http://localhost:25523 \
//!   cargo test -p dbine-driver-solr --test rename -- --ignored --nocapture
//! ```
//! Standalone: a core with a document is renamed, listed and searched by
//! the new name. SolrCloud: the request is refused with the explanation.

use dbine_driver::rename::{RenameRequest, RenameTarget};
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};

async fn session(url: &str) -> Box<dyn Session> {
    let cfg = ConnectionConfig { driver: "solr".into(), host: url.into(), ..Default::default() };
    dbine_driver_solr::drivers()[0].connect(&cfg, None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.map(|_| out)
}

fn request(name: &str, new: &str) -> RenameRequest {
    RenameRequest {
        target: RenameTarget::Object { object: ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: name.into() }, parent: None },
        new_name: new.into(),
        table: None,
        definition: None,
    }
}

async fn names(s: &mut Box<dyn Session>) -> Vec<String> {
    s.list_objects().await.unwrap().into_iter().map(|o| o.name).collect()
}

#[tokio::test]
#[ignore]
async fn standalone() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLR_URL") else {
        eprintln!("DBINE_TEST_SOLR_URL not set; skipping");
        return;
    };
    let d = &dbine_driver_solr::drivers()[0];
    let mut s = session(&url).await;
    for n in ["dbine_rn", "dbine_rn2"] {
        run(&mut s, &format!("DELETE /solr/{n}?if_exists=true")).await.unwrap();
    }
    run(&mut s, "PUT /solr/dbine_rn").await.unwrap();
    run(&mut s, "POST /solr/dbine_rn/update?commit=true\n[{\"id\": \"1\"}]").await.unwrap();

    let script = d.rename_script(&request("dbine_rn", "dbine_rn2")).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut s, st).await.unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    let list = names(&mut s).await;
    assert!(list.contains(&"dbine_rn2".to_string()) && !list.contains(&"dbine_rn".to_string()), "{list:?}");
    let out = run(&mut s, "GET /solr/dbine_rn2/select?q=*:*").await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);
    run(&mut s, "DELETE /solr/dbine_rn2").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn solrcloud_is_refused() {
    let Ok(url) = std::env::var("DBINE_TEST_SOLRCLOUD_URL") else {
        eprintln!("DBINE_TEST_SOLRCLOUD_URL not set; skipping");
        return;
    };
    let d = &dbine_driver_solr::drivers()[0];
    let mut s = session(&url).await;
    run(&mut s, "DELETE /solr/dbine_rn?if_exists=true").await.unwrap();
    run(&mut s, "PUT /solr/dbine_rn").await.unwrap();
    let script = d.rename_script(&request("dbine_rn", "dbine_rn2")).unwrap();
    let e = run(&mut s, &script.statements[0]).await.unwrap_err().to_string();
    assert!(e.contains("En SolrCloud las colecciones no se renombran"), "{e}");
    assert!(names(&mut s).await.contains(&"dbine_rn".to_string()));
    run(&mut s, "DELETE /solr/dbine_rn").await.unwrap();
}
