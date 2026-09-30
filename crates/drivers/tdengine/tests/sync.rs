//! Schema sync against a real TDengine:
//! `DBINE_TEST_TDENGINE_URL=http://localhost:25641 cargo test -p dbine-driver-tdengine --test sync -- --ignored --nocapture`

use dbine_driver::{ColumnDef, ConnectionConfig, QueryOutcome, TableChange};

#[tokio::test]
#[ignore]
async fn tdengine_sync() {
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
    let mut s = d.connect(&c, None).await.unwrap();
    s.execute(
        "DROP DATABASE IF EXISTS dbine_sync; CREATE DATABASE dbine_sync VGROUPS 1 BUFFER 16;
         CREATE STABLE dbine_sync.st (ts TIMESTAMP, v DOUBLE, nota VARCHAR(10), gone INT) TAGS (loc VARCHAR(10), grp INT);
         CREATE TABLE dbine_sync.nt (ts TIMESTAMP, v INT, nota NCHAR(5));",
        100,
        &mut QueryOutcome::default(),
    )
    .await
    .unwrap();
    let mut s = d.connect(&c, Some("dbine_sync")).await.unwrap();
    let schema = s.database_schema().await.unwrap();
    let (st, nt) = (schema.iter().find(|t| t.name == "st").cloned().unwrap(), schema.iter().find(|t| t.name == "nt").cloned().unwrap());

    let mut nst = st.clone();
    for c in &mut nst.columns {
        match c.name.as_str() {
            "nota" => c.data_type = "VARCHAR(40)".into(),
            "loc" => c.data_type = "VARCHAR(30)".into(),
            _ => {}
        }
    }
    nst.columns.retain(|c| c.name != "gone" && c.name != "grp");
    nst.columns.push(ColumnDef { name: "extra".into(), data_type: "BIGINT".into(), ..Default::default() });
    let mut zona = ColumnDef { name: "zona".into(), data_type: "NCHAR(8)".into(), ..Default::default() };
    zona.options.insert("tag".into(), "true".into());
    nst.columns.push(zona);
    nst.comment = Some("sincronizada".into());

    let mut nnt = nt.clone();
    for c in &mut nnt.columns {
        if c.name == "nota" {
            c.data_type = "NCHAR(20)".into();
        }
    }
    nnt.columns.push(ColumnDef { name: "w".into(), data_type: "FLOAT".into(), ..Default::default() });

    let script = d.sync_script(&[TableChange::Alter { old: st, new: nst }, TableChange::Alter { old: nt, new: nnt }]).unwrap();
    println!("{script:#?}");
    for q in &script.statements {
        s.execute(q, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    let after = s.database_schema().await.unwrap();
    let t = |n: &str| after.iter().find(|t| t.name == n).unwrap();
    let ty = |t: &str, c: &str| self::ty(after.iter().find(|x| x.name == t).unwrap(), c);
    println!("{:?}", t("st").columns.iter().map(|c| (&c.name, &c.data_type, &c.options)).collect::<Vec<_>>());
    assert_eq!(ty("st", "nota").as_deref(), Some("VARCHAR(40)"));
    assert_eq!(ty("st", "loc").as_deref(), Some("VARCHAR(30)"));
    assert!(ty("st", "gone").is_none() && ty("st", "grp").is_none());
    assert_eq!(ty("st", "extra").as_deref(), Some("BIGINT"));
    assert_eq!(ty("st", "zona").as_deref(), Some("NCHAR(8)"));
    assert_eq!(ty("nt", "nota").as_deref(), Some("NCHAR(20)"));
    assert_eq!(ty("nt", "w").as_deref(), Some("FLOAT"));

    let mut other = t("nt").clone();
    other.name = "nt2".into();
    for ch in [TableChange::Create { table: other.clone() }, TableChange::Drop { table: other }] {
        for q in d.sync_script(&[ch]).unwrap().statements {
            s.execute(&q, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{q}: {e}"));
        }
    }
}

fn ty(t: &dbine_driver::TableSchema, c: &str) -> Option<String> {
    t.columns.iter().find(|x| x.name == c).map(|x| x.data_type.to_uppercase())
}
