//! "Índices" against a real Phoenix query server: a table with a primary
//! key and two global indexes (one covering a column), one of them hit by
//! a few targeted queries. Phoenix has no per-index counters (nor foreign
//! keys): the report lists the row key and the indexes with zeros and a
//! note, and the drop script "Eliminar índice" generates removes the index.
//!
//! ```sh
//! DBINE_TEST_PHOENIX_URL=http://localhost:25165 \
//!   cargo test -p dbine-driver-phoenix --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_PHOENIX_URL").ok()?).expect("URL");
    Some(ConnectionConfig { driver: "phoenix".into(), host: url.host_str()?.into(), port: url.port().unwrap_or(0), ..Default::default() })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_PHOENIX_URL not set");
        return;
    };
    let mut drivers = dbine_driver_phoenix::drivers();
    assert!(!drivers[1].supports_index_usage(), "Avatica has no index metadata");
    let d = drivers.remove(0);
    assert!(d.supports_index_usage());
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "DROP TABLE IF EXISTS DBINE.IXU_T").await;
    run(&mut s, "CREATE TABLE DBINE.IXU_T (ID INTEGER NOT NULL, A INTEGER, B VARCHAR, CONSTRAINT PK_IXU PRIMARY KEY (ID))").await;
    run(&mut s, "CREATE INDEX IXU_A ON DBINE.IXU_T (A) INCLUDE (B)").await;
    run(&mut s, "CREATE INDEX IXU_B ON DBINE.IXU_T (B)").await;
    run(&mut s, "CREATE LOCAL INDEX IXU_L ON DBINE.IXU_T (B DESC)").await;
    for i in 1..=20 {
        run(&mut s, &format!("UPSERT INTO DBINE.IXU_T VALUES ({i}, {}, 'v{i}')", i * 3)).await;
    }
    for i in 0..5 {
        run(&mut s, &format!("SELECT B FROM DBINE.IXU_T WHERE A = {}", i * 3)).await;
    }
    let t = ObjectRef { kind: "table".into(), schema: Some("DBINE".into()), name: "IXU_T".into() };
    let r = s.index_usage(&t).await.unwrap().expect("report");
    eprintln!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some() && r.foreign_keys.is_empty());
    let got: Vec<(&str, &str)> = r.indexes.iter().map(|i| (i.name.as_str(), i.kind.as_str())).collect();
    assert_eq!(got, [("PK_IXU", "ROW KEY"), ("IXU_A", "GLOBAL"), ("IXU_B", "GLOBAL"), ("IXU_L", "LOCAL")]);
    assert_eq!(r.indexes[3].key_columns, ["B DESC"], "SORT_ORDER is read");
    assert!(r.indexes[0].primary_key);
    assert_eq!((r.indexes[1].key_columns.clone(), r.indexes[1].included_columns.clone()), (vec!["A".to_string()], vec!["B".to_string()]));
    assert!(r.indexes.iter().all(|i| i.reads == 0 && !i.unused));

    let old = s.database_schema().await.unwrap().into_iter().find(|x| x.name == "IXU_T").unwrap();
    // The DESC index round-trips: dropped, then recreated from its definition.
    let mut without = old.clone();
    without.indexes.retain(|i| i.name != "IXU_L");
    for st in d.sync_script(&[TableChange::Alter { old: old.clone(), new: without.clone() }]).unwrap().statements {
        run(&mut s, &st).await;
    }
    let script = d.sync_script(&[TableChange::Alter { old: without, new: old.clone() }]).unwrap().statements;
    assert!(script.iter().any(|st| st.contains("(\"B\" DESC)")), "{script:?}");
    for st in script {
        run(&mut s, &st).await;
    }
    let again = s.database_schema().await.unwrap().into_iter().find(|x| x.name == "IXU_T").unwrap();
    assert_eq!(again.indexes, old.indexes, "recreated as it was");
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "IXU_B" && i.name != "IXU_L");
    for st in d.sync_script(&[TableChange::Alter { old, new }]).unwrap().statements {
        run(&mut s, &st).await;
    }
    let r = s.index_usage(&t).await.unwrap().unwrap();
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PK_IXU", "IXU_A"]);
    run(&mut s, "DROP TABLE DBINE.IXU_T").await;
}
