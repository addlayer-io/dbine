//! "Comparar esquemas" against a real TDengine: two databases whose
//! supertable differs in its tag indexes (the implicit one on the first tag
//! isn't reported). The sync script is run on the target and both then
//! read the same.
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 cargo test -p dbine-driver-tdengine --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session, TableChange, TableSchema};

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

async fn st(s: &mut Box<dyn Session>) -> TableSchema {
    s.database_schema().await.unwrap().into_iter().find(|t| t.name == "st").expect("st")
}

#[tokio::test]
#[ignore]
async fn tag_indexes_compare_and_sync() {
    let Ok(url) = std::env::var("DBINE_TEST_TDENGINE_URL") else { return };
    let url = reqwest::Url::parse(&url).expect("URL");
    let c = ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    };
    let d = dbine_driver_tdengine::drivers().remove(0);
    let mut admin = d.connect(&c, None).await.unwrap();
    for db in ["dbine_cmp_src", "dbine_cmp_dst"] {
        run(
            &mut admin,
            &format!(
                "DROP DATABASE IF EXISTS {db}; CREATE DATABASE {db} VGROUPS 1 BUFFER 16;
                 CREATE STABLE {db}.st (ts TIMESTAMP, v DOUBLE) TAGS (loc VARCHAR(10), grp INT, zona NCHAR(8));"
            ),
        )
        .await;
    }
    run(&mut admin, "CREATE INDEX ix_grp ON dbine_cmp_src.st (grp); CREATE INDEX ix_zona ON dbine_cmp_src.st (zona);").await;
    run(&mut admin, "CREATE INDEX ix_grp2 ON dbine_cmp_dst.st (zona);").await;
    let mut a = d.connect(&c, Some("dbine_cmp_src")).await.unwrap();
    let mut b = d.connect(&c, Some("dbine_cmp_dst")).await.unwrap();
    let src = st(&mut a).await;
    let dst = st(&mut b).await;
    println!("{:?}\n{:?}", src.indexes, dst.indexes);
    assert_eq!(src.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["ix_grp", "ix_zona"]);
    let new = TableSchema { schema: dst.schema.clone(), ..src.clone() };
    let script = d.sync_script(&[TableChange::Alter { old: dst, new: new.clone() }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut b, st).await;
    }
    let after = st(&mut b).await;
    assert_eq!(after.indexes, src.indexes);
    assert!(d.sync_script(&[TableChange::Alter { old: after, new }]).unwrap().statements.is_empty());
    run(&mut admin, "DROP DATABASE dbine_cmp_src; DROP DATABASE dbine_cmp_dst;").await;
}
