//! "Índices" against a real Firebird server: a table with a primary key, a
//! foreign key and two indexes, one of them read a few times. Firebird has
//! no per-index counters: the report lists everything with zeros and a
//! note, and the drop script "Eliminar índice" generates removes the index.
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url.strip_prefix("firebird://").expect("firebird://user:pass@host:port/path");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, path) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(sql, 10, &mut out).await;
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    assert!(d.supports_index_usage());
    let mut s = d.connect(&cfg, None).await.unwrap();
    try_run(&mut s, "DROP TABLE IXU_T").await;
    try_run(&mut s, "DROP TABLE IXU_P").await;
    run(&mut s, "CREATE TABLE IXU_P (ID INTEGER NOT NULL PRIMARY KEY)").await;
    run(
        &mut s,
        "CREATE TABLE IXU_T (ID INTEGER NOT NULL CONSTRAINT PK_IXU_T PRIMARY KEY, P_ID INTEGER CONSTRAINT FK_IXU_P REFERENCES IXU_P (ID), A INTEGER, B INTEGER)",
    )
    .await;
    run(&mut s, "CREATE INDEX IXU_A ON IXU_T (A)").await;
    run(&mut s, "CREATE DESCENDING INDEX IXU_B ON IXU_T (B)").await;
    run(&mut s, "INSERT INTO IXU_P VALUES (1)").await;
    for i in 1..=20 {
        run(&mut s, &format!("INSERT INTO IXU_T VALUES ({i}, 1, {i}, {})", i * 2)).await;
    }
    for i in 0..5 {
        run(&mut s, &format!("SELECT * FROM IXU_T WHERE A = {i}")).await;
    }
    let t = ObjectRef { kind: "table".into(), schema: None, name: "IXU_T".into() };
    let r = s.index_usage(&t).await.unwrap().expect("report");
    eprintln!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some());
    let got: Vec<(&str, &str)> = r.indexes.iter().map(|i| (i.name.as_str(), i.kind.as_str())).collect();
    assert_eq!(got, [("PK_IXU_T", "ASC"), ("IXU_A", "ASC"), ("IXU_B", "DESC")]);
    assert!(r.indexes[0].primary_key);
    assert!(r.indexes.iter().all(|i| i.reads == 0 && !i.unused));
    assert_eq!(r.foreign_keys.len(), 1);
    assert_eq!((r.foreign_keys[0].name.as_deref(), r.foreign_keys[0].ref_table.as_str()), (Some("FK_IXU_P"), "IXU_P"));

    let old = s.database_schema().await.unwrap().into_iter().find(|x| x.name == "IXU_T").unwrap();
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "IXU_B");
    for st in d.sync_script(&[TableChange::Alter { old, new }]).unwrap().statements {
        run(&mut s, &st).await;
    }
    let r = s.index_usage(&t).await.unwrap().unwrap();
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PK_IXU_T", "IXU_A"]);
    run(&mut s, "DROP TABLE IXU_T").await;
    run(&mut s, "DROP TABLE IXU_P").await;
}
