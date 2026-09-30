//! "Comparar esquemas" against a real Couchbase Server (already initialized,
//! as `integration.rs` leaves it): two scopes whose collection differs in
//! its GSI indexes (primary, array index, DESC key, partial). The sync
//! script is run on the target and both then read the same.
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session, TableChange, TableSchema};
use std::time::Duration;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("Administrator".into()),
        password: Some("secreto1".into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

async fn wait(s: &mut Box<dyn Session>, keyspace: &str) {
    for _ in 0..40 {
        if s.execute(&format!("SELECT RAW 1 FROM {keyspace} LIMIT 1"), 1, &mut QueryOutcome::default()).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("{keyspace} no quedó lista");
}

fn docs(schema: &[TableSchema], scope: &str) -> TableSchema {
    let mut t = schema.iter().find(|t| t.name == "docs" && t.schema.as_deref() == Some(scope)).expect("docs").clone();
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t
}

#[tokio::test]
#[ignore]
async fn compare_and_sync() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_couchbase::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    if s.list_databases().await.unwrap().contains(&"dbine_cmp".to_string()) {
        s.drop_database("dbine_cmp").await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    s.create_database("dbine_cmp").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_cmp")).await.unwrap();
    for scope in ["src", "dst"] {
        run(&mut s, &format!("CREATE SCOPE dbine_cmp.{scope}; CREATE COLLECTION dbine_cmp.{scope}.docs")).await;
        wait(&mut s, &format!("dbine_cmp.{scope}.docs")).await;
        run(&mut s, &format!("INSERT INTO dbine_cmp.{scope}.docs (KEY, VALUE) VALUES ('d1', {{'tipo': 'a', 'fecha': 1, 'tags': ['x']}})")).await;
    }
    run(
        &mut s,
        "CREATE PRIMARY INDEX ON dbine_cmp.src.docs;
         CREATE INDEX ix_tags ON dbine_cmp.src.docs(DISTINCT ARRAY t FOR t IN tags END, fecha DESC) WHERE tipo = 'a';
         CREATE INDEX ix_tipo ON dbine_cmp.src.docs(tipo INCLUDE MISSING, fecha);",
    )
    .await;
    run(&mut s, "CREATE INDEX ix_tags ON dbine_cmp.dst.docs(tags); CREATE INDEX ix_viejo ON dbine_cmp.dst.docs(viejo);").await;

    let schema = s.database_schema().await.unwrap();
    let src = docs(&schema, "dbine_cmp.src");
    let dst = docs(&schema, "dbine_cmp.dst");
    println!("{:#?}", src.indexes);
    assert!(src.indexes.iter().any(|i| i.kind.as_deref() == Some("PRIMARY")));
    assert!(src.indexes.iter().any(|i| i.filter.is_some()));
    // The UI carries the source's items into the target's collection.
    let new = TableSchema { schema: dst.schema.clone(), ..src.clone() };
    let script = d.sync_script(&[TableChange::Alter { old: dst, new: new.clone() }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut s, st).await;
    }
    let after = docs(&s.database_schema().await.unwrap(), "dbine_cmp.dst");
    assert_eq!(after.indexes, src.indexes);
    assert!(d.sync_script(&[TableChange::Alter { old: after, new }]).unwrap().statements.is_empty());

    let mut s = d.connect(&c, None).await.unwrap();
    s.drop_database("dbine_cmp").await.unwrap();
}
