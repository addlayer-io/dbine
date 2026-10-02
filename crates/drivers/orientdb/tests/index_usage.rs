//! Index usage against a real server: a class with a LINK to another (its
//! foreign key), a unique index and two more, listed without counters
//! (OrientDB counts none per index). Then "Eliminar índice…": the schema
//! sync script without one of them, run.
//!
//! ```sh
//! DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 \
//!   cargo test -p dbine-driver-orientdb --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

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

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Ok(url) = std::env::var("DBINE_TEST_ORIENTDB_URL") else { return };
    let d = dbine_driver_orientdb::drivers().remove(0);
    assert!(d.supports_index_usage());
    let c = cfg(&url);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let _ = admin.drop_database("dbine_ixu").await;
    admin.create_database("dbine_ixu").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_ixu")).await.unwrap();
    run(
        &mut s,
        "CREATE CLASS Cliente; CREATE PROPERTY Cliente.id INTEGER; CREATE INDEX Cliente.id ON Cliente (id) UNIQUE;
         CREATE CLASS Pedido; CREATE PROPERTY Pedido.id INTEGER; CREATE PROPERTY Pedido.cliente LINK Cliente; CREATE PROPERTY Pedido.fecha INTEGER;
         CREATE INDEX Pedido.id ON Pedido (id) UNIQUE; CREATE INDEX Pedido.cliente ON Pedido (cliente) NOTUNIQUE; CREATE INDEX Pedido.fecha ON Pedido (fecha) NOTUNIQUE",
    )
    .await;
    let obj = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "Pedido".into() };
    let r = s.index_usage(&obj).await.unwrap().expect("report").derived();
    println!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some());
    let mut names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["Pedido.cliente", "Pedido.fecha", "Pedido.id"]);
    assert!(r.indexes.iter().find(|i| i.name == "Pedido.id").unwrap().unique);
    assert_eq!(r.foreign_keys.len(), 1);
    assert_eq!((r.foreign_keys[0].columns[0].as_str(), r.foreign_keys[0].ref_table.as_str()), ("cliente", "Cliente"));

    let table = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "Pedido").unwrap();
    let mut without = table.clone();
    without.indexes.retain(|i| i.name != "Pedido.fecha");
    let script = d.sync_script(&[TableChange::Alter { old: table, new: without }]).unwrap();
    println!("{script:#?}");
    assert_eq!(script.statements.len(), 1);
    run(&mut s, &script.statements[0]).await;
    let r = s.index_usage(&obj).await.unwrap().unwrap();
    assert!(r.indexes.iter().all(|i| i.name != "Pedido.fecha"));
    drop(s);
    admin.drop_database("dbine_ixu").await.unwrap();
}
