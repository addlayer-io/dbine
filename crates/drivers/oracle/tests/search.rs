//! "Buscar en la base" from the dictionary against a real server, checked
//! against the app's per-object scan (list_objects + definition + the same
//! line matching), as a user that can create schemas
//! (`DBINE_TEST_ORACLE_ADMIN_URL`), skipped without it:
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test search -- --ignored --nocapture
//! ```

use dbine_driver::search::{hits_in, CodeHit, CodeSearch};
use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};

const SCHEMA: &str = "DBINE_SEARCH";

fn config(url: &str) -> ConnectionConfig {
    let rest = url.strip_prefix("oracle://").expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none() && out.errors.is_empty(), "{sql}: {:?} {:?}", out.error, out.errors);
}

/// The app's scan, as commands/search.rs does it.
async fn scan(s: &mut Box<dyn Session>, d: &dyn Driver, q: &CodeSearch) -> Vec<CodeHit> {
    let with_source: Vec<&str> = d.info().object_kinds.iter().filter(|k| k.has_definition).map(|k| k.id).collect();
    let mut hits = Vec::new();
    for o in s.list_objects().await.unwrap() {
        if !with_source.contains(&o.kind.as_str()) || !(q.kinds.is_empty() || q.kinds.contains(&o.kind)) {
            continue;
        }
        let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        if let Ok(Some(src)) = s.definition(&r).await {
            hits.extend(hits_in(&o.kind, o.schema.as_deref(), &o.name, o.parent.as_deref(), &src, q));
        }
    }
    hits
}

fn sorted(mut h: Vec<CodeHit>) -> Vec<CodeHit> {
    h.sort_by(|a, b| (&a.kind, &a.schema, &a.name, a.line).cmp(&(&b.kind, &b.schema, &b.name, b.line)));
    h
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn catalog_equals_scan() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut admin: Box<dyn Session> = d.connect(&config(&url), None).await.unwrap();
    let mut quiet = QueryOutcome::default();
    let _ = admin.execute("DROP PUBLIC SYNONYM s_ventas_pub", 10, &mut quiet).await;
    let _ = admin.drop_database(SCHEMA).await;
    admin.create_database(SCHEMA).await.unwrap();
    // A materialized view's container table belongs to the schema.
    run(&mut admin, "GRANT CREATE TABLE, CREATE MATERIALIZED VIEW TO dbine_search").await;
    let mut s = d.connect(&config(&url), Some(SCHEMA)).await.unwrap();
    for sql in [
        "CREATE TABLE ventas (id NUMBER PRIMARY KEY, total NUMBER)",
        "CREATE INDEX ix_ventas_total ON ventas (total)",
        "CREATE TABLE ventas_hist (id NUMBER)",
        "CREATE VIEW v_ventas AS\nSELECT id, total\nFROM ventas\nWHERE total > 0",
        "CREATE VIEW v_anio AS SELECT 'AÑO 100%_x' AS etiqueta FROM dual",
        "CREATE MATERIALIZED VIEW mv_ventas AS SELECT SUM(total) AS t FROM ventas",
        "CREATE PROCEDURE p_total AS\n  n NUMBER;\nBEGIN\n  SELECT SUM(total) INTO n FROM ventas;\n  SELECT COUNT(*) INTO n FROM ventas_hist;\nEND;",
        "CREATE FUNCTION f_uno RETURN VARCHAR2 AS\nBEGIN\n  RETURN '100%_x';\nEND;",
        "CREATE PACKAGE pk_ventas AS\n  PROCEDURE cerrar;\nEND pk_ventas;",
        "CREATE PACKAGE BODY pk_ventas AS\n  PROCEDURE cerrar IS\n  BEGIN\n    UPDATE ventas SET total = 0;\n  END;\nEND pk_ventas;",
        "CREATE TRIGGER t_ventas BEFORE INSERT ON ventas FOR EACH ROW\nBEGIN\n  :NEW.total := NVL(:NEW.total, 0);\nEND;",
        "CREATE TYPE t_monto AS OBJECT (valor NUMBER, MEMBER FUNCTION doble RETURN NUMBER);",
        "CREATE TYPE BODY t_monto AS\n  MEMBER FUNCTION doble RETURN NUMBER IS\n  BEGIN\n    RETURN valor * 2; -- ventas\n  END;\nEND;",
        "CREATE SEQUENCE seq_ventas START WITH 10",
        "CREATE SYNONYM s_ventas FOR ventas",
        "CREATE PUBLIC SYNONYM s_ventas_pub FOR dbine_search.ventas",
    ] {
        run(&mut s, sql).await;
    }

    let cases: &[(&str, bool, bool)] = &[
        ("ventas", true, false),
        ("VENTAS", false, true),
        ("SUM(", false, false),
        ("100%_x", false, false),
        ("ix_ventas", false, false),
        ("editionable", false, false),
        ("año", false, false),
        ("dbine_search", true, false),
    ];
    let mut all = Vec::new();
    for kinds in [vec![], vec!["trigger".to_string(), "type".to_string(), "synonym".to_string()]] {
        for &(text, word, case) in cases {
            let q = CodeSearch { text: text.into(), whole_word: word, case_sensitive: case, kinds: kinds.clone(), ..Default::default() };
            let fast = sorted(s.search_code(&q).await.unwrap().expect("Oracle answers from its dictionary").hits);
            let slow = sorted(scan(&mut s, d.as_ref(), &q).await);
            eprintln!("{text:?} {kinds:?}: {} hits", fast.len());
            assert_eq!(fast, slow, "{text:?} {kinds:?}");
            all.push(fast);
        }
    }
    let kinds_of = |h: &[CodeHit]| h.iter().map(|h| h.kind.clone()).collect::<std::collections::BTreeSet<_>>();
    let found = kinds_of(&[all[0].clone(), all[1].clone()].concat());
    for k in ["table", "view", "materialized_view", "procedure", "package", "trigger", "type", "sequence", "synonym"] {
        assert!(found.contains(k), "{k}: {found:?}");
    }
    assert!(all[0].iter().any(|h| h.kind == "trigger" && h.parent.as_deref() == Some("VENTAS")), "{:?}", all[0]);
    assert!(all[0].iter().any(|h| h.schema.as_deref() == Some("PUBLIC")), "public synonym: {:?}", all[0]);
    assert!(all[4].iter().any(|h| h.kind == "table" && h.name == "VENTAS"), "the index's DDL: {:?}", all[4]);
    assert!(!all[6].is_empty(), "non-ASCII without narrowing");

    let q = CodeSearch { text: "ventas".into(), max_hits: 2, ..Default::default() };
    let capped = s.search_code(&q).await.unwrap().unwrap();
    assert!(capped.truncated && capped.hits.len() == 2, "{capped:?}");

    drop(s);
    admin.execute("DROP PUBLIC SYNONYM s_ventas_pub", 10, &mut quiet).await.unwrap();
    admin.drop_database(SCHEMA).await.unwrap();
}
