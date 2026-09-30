//! "Comparar esquemas" against a real server: two databases whose class
//! differs in its indexes (collation, Lucene full-text with its analyzer,
//! metadata) and whose sequence only one side has. The sync script and the
//! sequence's definition are run on the target and both then read the same.
//!
//! ```sh
//! DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 \
//!   cargo test -p dbine-driver-orientdb --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};

fn cfg(url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    ConnectionConfig { driver: "orientdb".into(), host: host.into(), port: port.parse().unwrap(), username: Some(user.into()), password: Some(pass.into()), ..Default::default() }
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

async fn class(s: &mut Box<dyn Session>) -> TableSchema {
    let mut t = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "Doc").expect("Doc");
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t
}

#[tokio::test]
#[ignore]
async fn compare_and_sync() {
    let Ok(url) = std::env::var("DBINE_TEST_ORIENTDB_URL") else { return };
    let d = dbine_driver_orientdb::drivers().remove(0);
    let c = cfg(&url);
    let mut admin = d.connect(&c, None).await.expect("connect");
    for db in ["dbine_cmp_src", "dbine_cmp_dst"] {
        let _ = admin.drop_database(db).await;
        admin.create_database(db).await.unwrap();
    }
    let mut a = d.connect(&c, Some("dbine_cmp_src")).await.unwrap();
    let mut b = d.connect(&c, Some("dbine_cmp_dst")).await.unwrap();
    let base = "CREATE CLASS Doc; CREATE PROPERTY Doc.name STRING; CREATE PROPERTY Doc.bio STRING; CREATE PROPERTY Doc.age INTEGER";
    run(&mut a, base).await;
    run(&mut b, base).await;
    run(
        &mut a,
        "CREATE INDEX Doc.nc ON Doc (name COLLATE ci, age) NOTUNIQUE METADATA {ignoreNullValues: true};
         CREATE INDEX Doc.bio_ft ON Doc (bio) FULLTEXT ENGINE LUCENE METADATA {\"analyzer\": \"org.apache.lucene.analysis.en.EnglishAnalyzer\"};
         CREATE SEQUENCE folio TYPE ORDERED START 100 INCREMENT 5",
    )
    .await;
    run(&mut b, "CREATE INDEX Doc.nc ON Doc (name, age) NOTUNIQUE; CREATE INDEX Doc.bio_ft ON Doc (bio) FULLTEXT ENGINE LUCENE").await;

    let src = class(&mut a).await;
    let dst = class(&mut b).await;
    println!("{:#?}", src.indexes);
    assert_ne!(src.indexes, dst.indexes);
    let seq = ObjectRef { kind: kinds::SEQUENCE.into(), schema: None, name: "folio".into() };
    let def = a.definition(&seq).await.unwrap().unwrap();
    // Using it doesn't change its definition.
    run(&mut a, "SELECT sequence('folio').next()").await;
    assert_eq!(a.definition(&seq).await.unwrap().unwrap(), def);

    let script = d.sync_script(&[TableChange::Alter { old: dst, new: src.clone() }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut b, st).await;
    }
    run(&mut b, &def).await;
    let after = class(&mut b).await;
    assert_eq!(after.indexes, src.indexes);
    assert_eq!(b.definition(&seq).await.unwrap().unwrap(), def);

    for db in ["dbine_cmp_src", "dbine_cmp_dst"] {
        admin.drop_database(db).await.unwrap();
    }
}
