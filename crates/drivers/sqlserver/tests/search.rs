//! "Buscar en la base" from the catalog against a real server
//! (`DBINE_TEST_SQLSERVER_URL`), checked against the app's per-object scan
//! (list_objects + definition + the same line matching):
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test search -- --ignored
//! ```

use dbine_driver::search::{hits_in, CodeSearch};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn catalog_equals_scan() {
    let Some(cfg) = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let mut s = d.connect(&cfg, Some("master")).await.unwrap();
    let _ = s.drop_database("dbine_search").await;
    s.create_database("dbine_search").await.unwrap();
    let mut s = d.connect(&cfg, Some("dbine_search")).await.unwrap();
    run(&mut s, "CREATE TABLE dbo.Ventas (id int PRIMARY KEY, total money)").await;
    run(&mut s, "CREATE TABLE dbo.VentasHist (id int)").await;
    run(&mut s, "CREATE VIEW dbo.v_ventas AS\nSELECT id, total\nFROM dbo.Ventas").await;
    run(&mut s, "CREATE PROCEDURE dbo.p_total AS\nBEGIN\n  SELECT SUM(total) FROM dbo.Ventas;\n  SELECT 1 FROM dbo.VentasHist;\nEND").await;
    run(&mut s, "CREATE FUNCTION dbo.f_uno() RETURNS int AS BEGIN RETURN 1 END").await;
    run(&mut s, "CREATE SYNONYM dbo.s_ventas FOR dbo.Ventas").await;
    run(&mut s, "CREATE SEQUENCE dbo.seq_ventas START WITH 100").await;
    run(&mut s, "CREATE TRIGGER dbo.t_ventas ON dbo.Ventas AFTER INSERT AS\nUPDATE dbo.Ventas SET total = 0 WHERE 1 = 0").await;

    for (text, word, case) in [("ventas", true, false), ("Ventas", false, true), ("SUM(", false, false), ("100%_x", false, false)] {
        let kinds: Vec<String> = d.info().object_kinds.iter().filter(|k| k.has_definition && k.id != "table").map(|k| k.id.to_string()).collect();
        let q = CodeSearch { text: text.into(), whole_word: word, case_sensitive: case, kinds, ..Default::default() };
        let mut fast = s.search_code(&q).await.unwrap().expect("SQL Server answers from its catalog").hits;
        // The app's scan, as commands/search.rs does it.
        let mut scan = Vec::new();
        for o in s.list_objects().await.unwrap() {
            // As the app resolves "Código" without a kind filter: every kind
            // with a definition but tables.
            if !d.info().object_kinds.iter().any(|k| k.id == o.kind && k.has_definition) || o.kind == "table" {
                continue;
            }
            let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
            if let Some(src) = s.definition(&r).await.unwrap() {
                scan.extend(hits_in(&o.kind, o.schema.as_deref(), &o.name, o.parent.as_deref(), &src, &q));
            }
        }
        let key = |h: &dbine_driver::search::CodeHit| (h.kind.clone(), h.name.clone(), h.line);
        fast.sort_by_key(key);
        scan.sort_by_key(key);
        eprintln!("{text}: {} hits", fast.len());
        assert_eq!(fast, scan, "{text}");
    }
    let q = CodeSearch { text: "ventas".into(), whole_word: true, ..Default::default() };
    let hits = s.search_code(&q).await.unwrap().unwrap().hits;
    assert!(hits.iter().any(|h| h.kind == "view" && h.line == 3 && h.text == "FROM dbo.Ventas"), "{hits:?}");
    assert!(hits.iter().any(|h| h.kind == "trigger" && h.parent.as_deref() == Some("Ventas")), "{hits:?}");
    assert!(!hits.iter().any(|h| h.text.contains("VentasHist")), "whole word");

    drop(s);
    let mut m = d.connect(&cfg, Some("master")).await.unwrap();
    m.drop_database("dbine_search").await.unwrap();
}
