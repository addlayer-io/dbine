//! Index listing and drop through the schema sync, against the emulator:
//!   docker run -d --name dbine-test-spanner -p 25303:9020 gcr.io/cloud-spanner-emulator/emulator
//!   DBINE_TEST_SPANNER_URL=http://localhost:25303 cargo test -p dbine-driver-spanner --test index_usage -- --ignored --nocapture

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};
use serde_json::json;

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else { return };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let _ = http.post(format!("{url}/v1/projects/test/instances/i1/databases")).json(&json!({ "createStatement": "CREATE DATABASE `db1`" })).send().await;
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "db1".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_spanner::drivers().pop().unwrap();
    assert!(d.supports_index_usage());
    let mut s = d.connect(&cfg, None).await.unwrap();

    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP INDEX iu_child_used; DROP INDEX iu_child_idle; DROP TABLE iu_child; DROP TABLE iu_parent", 10, &mut out).await;
    run(
        &mut s,
        "CREATE TABLE iu_parent (id INT64 NOT NULL, name STRING(20)) PRIMARY KEY (id);
         CREATE TABLE iu_child (id INT64 NOT NULL, parent_id INT64, code STRING(10), note STRING(50),
           CONSTRAINT fk_iu_parent FOREIGN KEY (parent_id) REFERENCES iu_parent (id)) PRIMARY KEY (id);
         CREATE INDEX iu_child_used ON iu_child (code DESC) STORING (note);
         CREATE NULL_FILTERED INDEX iu_child_idle ON iu_child (note)",
    )
    .await;
    run(&mut s, "INSERT INTO iu_parent (id, name) VALUES (1, 'a'), (2, 'b')").await;
    run(&mut s, "INSERT INTO iu_child (id, parent_id, code, note) VALUES (1, 1, 'x', 'n1'), (2, 2, 'y', 'n2'), (3, 1, 'z', NULL)").await;
    for code in ["x", "y", "z", "x", "y"] {
        run(&mut s, &format!("SELECT id FROM iu_child@{{FORCE_INDEX=iu_child_used}} WHERE code = '{code}'")).await;
    }

    let t = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "iu_child".into() };
    let r = s.index_usage(&t).await.unwrap().expect("report").derived();
    eprintln!("{r:#?}");
    // The emulator has no SPANNER_SYS: no counters, and the note names the permission a real instance needs.
    assert!(!r.stats_available && r.note.as_deref().is_some_and(|n| n.contains("spanner.databases.select")), "{:?}", r.note);
    let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["PRIMARY_KEY", "iu_child_idle", "iu_child_used"], "the FK's managed index is left out");
    assert!(r.indexes[0].primary_key && r.indexes[0].key_columns == vec!["id"]);
    let used = &r.indexes[2];
    assert_eq!((used.key_columns.clone(), used.included_columns.clone()), (vec!["code DESC".to_string()], vec!["note".to_string()]));
    assert_eq!(r.indexes[1].kind, "NULL_FILTERED INDEX");
    // No counters: nothing is "unused" nor has a share.
    assert!(r.indexes.iter().all(|i| i.reads == 0 && !i.unused && i.read_share.is_none()));
    assert_eq!(r.foreign_keys.len(), 1);
    assert_eq!((r.foreign_keys[0].columns.clone(), r.foreign_keys[0].ref_table.as_str()), (vec!["parent_id".to_string()], "iu_parent"));

    // Drop one through the schema sync, as "Eliminar índice…" does.
    let tables = s.database_schema().await.unwrap();
    let old = tables.into_iter().find(|x| x.name == "iu_child").unwrap();
    assert!(old.indexes.iter().any(|i| i.name == "iu_child_idle"));
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "iu_child_idle");
    let script = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
    eprintln!("{script:?}");
    assert_eq!(script.statements.len(), 1);
    for st in &script.statements {
        run(&mut s, st).await;
    }
    let r = s.index_usage(&t).await.unwrap().unwrap();
    assert!(!r.indexes.iter().any(|i| i.name == "iu_child_idle"));

    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP INDEX iu_child_used; DROP TABLE iu_child; DROP TABLE iu_parent", 10, &mut out).await;
}
