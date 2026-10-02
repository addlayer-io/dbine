//! Reflections as a table's indexes, against a real Dremio OSS (the first
//! user is created if needed):
//!
//! ```sh
//! docker run -d --name dbine-test-dremio -p 25947:9047 dremio/dremio-oss
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 cargo test -p dbine-driver-dremio --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};
use serde_json::json;
use std::time::Duration;

const USER: &str = "dbine";
const PASS: &str = "secreto123";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    })
}

async fn bootstrap(c: &ConnectionConfig) {
    let http = reqwest::Client::new();
    let base = format!("http://{}:{}", c.host, c.port);
    for _ in 0..60 {
        if http.get(format!("{base}/apiv2/server_status")).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = http
        .put(format!("{base}/apiv2/bootstrap/firstuser"))
        .header("Authorization", "_dremionull")
        .json(&json!({"userName": USER, "firstName": "DB", "lastName": "Ine", "email": "dbine@example.com", "createdAt": 1700000000000u64, "password": PASS}))
        .send()
        .await;
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Some(c) = cfg() else { return };
    bootstrap(&c).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    assert!(d.supports_index_usage());
    let mut s = d.connect(&c, Some("$scratch")).await.unwrap();
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE IF EXISTS \"$scratch\".iu_t", 10, &mut out).await;
    run(&mut s, "CREATE TABLE \"$scratch\".iu_t (id INT, code VARCHAR, note VARCHAR)").await;
    run(&mut s, "INSERT INTO \"$scratch\".iu_t VALUES (1, 'x', 'a'), (2, 'y', 'b'), (3, 'x', 'c')").await;
    run(&mut s, "ALTER TABLE \"$scratch\".iu_t CREATE RAW REFLECTION r_used USING DISPLAY (id, code) LOCALSORT BY (code)").await;
    run(&mut s, "ALTER TABLE \"$scratch\".iu_t CREATE RAW REFLECTION r_idle USING DISPLAY (note)").await;

    let t = ObjectRef { kind: kinds::TABLE.into(), schema: Some("$scratch".into()), name: "iu_t".into() };
    // The planner picks a reflection once it's built; aggregate queries on
    // r_used's columns only (r_idle can't answer them).
    let mut used = 0;
    for _ in 0..30 {
        run(&mut s, "SELECT code, SUM(id) FROM \"$scratch\".iu_t GROUP BY code").await;
        let r = s.index_usage(&t).await.unwrap().unwrap();
        used = r.indexes.iter().find(|i| i.name == "r_used").map_or(0, |i| i.seeks);
        if used >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let r = s.index_usage(&t).await.unwrap().expect("report").derived();
    eprintln!("{r:#?}");
    assert!(r.stats_available && !r.seek_scan_split && !r.writes_counted && r.note.is_some());
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["r_idle", "r_used"]);
    let (idle, hit) = (&r.indexes[0], &r.indexes[1]);
    assert!(used >= 2 && hit.seeks >= 2, "r_used accelerated {used} queries");
    assert_eq!(hit.key_columns, vec!["id", "code"]);
    assert_eq!((idle.seeks, idle.read_share), (0, Some(0.0)));
    assert_eq!(hit.read_share, Some(1.0));
    assert!(r.indexes.iter().all(|i| i.seek_health.is_none() && i.size_kb.is_some()));
    assert!(r.foreign_keys.is_empty());

    // Drop r_idle through the schema sync, as "Eliminar índice…" does.
    let tables = s.database_schema().await.unwrap();
    let old = tables.into_iter().find(|x| x.name == "iu_t" && x.schema.as_deref() == Some("$scratch")).expect("iu_t in the schema");
    assert_eq!(old.indexes.len(), 2, "{:?}", old.indexes);
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "r_idle");
    let script = d.sync_script(&[TableChange::Alter { old: old.clone(), new }]).unwrap();
    eprintln!("{script:?}");
    assert_eq!(script.statements, vec!["ALTER TABLE \"$scratch\".\"iu_t\" DROP REFLECTION \"r_idle\";"]);
    for st in &script.statements {
        run(&mut s, st).await;
    }
    let r = s.index_usage(&t).await.unwrap().unwrap();
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["r_used"]);

    // And back: the sync makes it again from the old definition.
    let now = s.database_schema().await.unwrap().into_iter().find(|x| x.name == "iu_t" && x.schema.as_deref() == Some("$scratch")).unwrap();
    let script = d.sync_script(&[TableChange::Alter { old: now, new: old }]).unwrap();
    for st in &script.statements {
        run(&mut s, st).await;
    }
    let r = s.index_usage(&t).await.unwrap().unwrap();
    assert_eq!(r.indexes.len(), 2);

    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP TABLE IF EXISTS \"$scratch\".iu_t", 10, &mut out).await;
}
