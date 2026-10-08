//! "Buscar en la base" from the catalog against a real server, checked
//! against the app's per-object scan (list_objects + definition + the same
//! line matching), in a database file of its own next to
//! `DBINE_TEST_FIREBIRD_URL`'s; skipped without it:
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test search -- --ignored --nocapture
//! ```

use dbine_driver::search::{hits_in, CodeHit, CodeSearch};
use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use std::collections::BTreeMap;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url.strip_prefix("firebird://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, path) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
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
    h.sort_by(|a, b| (&a.kind, &a.name, a.line).cmp(&(&b.kind, &b.name, b.line)));
    h
}

const SCRIPT: &str = r#"
CREATE DOMAIN d_monto_ventas AS NUMERIC(18,2) DEFAULT 0 CHECK (VALUE >= 0);
CREATE TABLE ventas (id INTEGER NOT NULL PRIMARY KEY, total d_monto_ventas);
CREATE TABLE ventas_hist (id INTEGER);
CREATE VIEW v_ventas AS
SELECT id, total
FROM ventas
WHERE total > 0;
CREATE VIEW v_anio AS SELECT 'AÑO 100%_x' AS etiqueta FROM RDB$DATABASE;
CREATE SEQUENCE seq_ventas START WITH 5 INCREMENT BY 2;
SET TERM ^ ;
CREATE PROCEDURE p_total (desde INTEGER) RETURNS (t NUMERIC(18,2)) AS
BEGIN
  SELECT SUM(total) FROM ventas WHERE id > :desde INTO :t;
  SUSPEND;
END^
SET TERM ; ^
create function f_uno (x integer) returns varchar(20) as
begin
  return '100%_x';
end;
create package pk_ventas as begin procedure cerrar; end;
create package body pk_ventas as begin
  procedure cerrar as begin update ventas set total = 0; end
end;
CREATE TRIGGER t_ventas FOR ventas BEFORE INSERT OR UPDATE AS
BEGIN
  NEW.total = COALESCE(NEW.total, 0);
END;
CREATE TRIGGER t_conexion ON CONNECT AS
BEGIN
  /* ventas */
END;
"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn catalog_equals_scan() {
    let Some(base) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    let mut admin = d.connect(&base, None).await.unwrap();
    let dir = base.database.rsplit_once('/').unwrap().0.to_string();
    let path = format!("{dir}/dbine_search.fdb");
    let _ = admin.drop_database(&path).await;
    let options: BTreeMap<String, String> =
        [("folder", dir.as_str()), ("charset", "utf8")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    admin.create_database_with("dbine_search", &options).await.unwrap();
    let mut s = d.connect(&ConnectionConfig { database: path.clone(), ..base.clone() }, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(SCRIPT, 10, &mut out).await.unwrap();
    assert!(out.error.is_none() && out.errors.is_empty(), "{:?} {:?}", out.error, out.errors);

    let cases: &[(&str, bool, bool)] = &[
        ("ventas", true, false),
        ("VENTAS", false, true),
        ("SUM(", false, false),
        ("100%_x", false, false),
        ("create or alter", false, false),
        ("año", false, false),
        ("before insert", false, false),
        ("\"desde\"", false, false),
    ];
    let mut all = Vec::new();
    for kinds in [vec![], vec!["trigger".to_string(), "type".to_string()]] {
        for &(text, word, case) in cases {
            let q = CodeSearch { text: text.into(), whole_word: word, case_sensitive: case, kinds: kinds.clone(), ..Default::default() };
            let fast = sorted(s.search_code(&q).await.unwrap().expect("Firebird answers from its catalog").hits);
            let slow = sorted(scan(&mut s, d.as_ref(), &q).await);
            eprintln!("{text:?} {kinds:?}: {} hits", fast.len());
            assert_eq!(fast, slow, "{text:?} {kinds:?}");
            all.push(fast);
        }
    }
    let found: std::collections::BTreeSet<String> = [all[0].clone(), all[1].clone(), all[4].clone()].concat().into_iter().map(|h| h.kind).collect();
    for k in ["view", "procedure", "function", "package", "trigger", "sequence", "type"] {
        assert!(found.contains(k), "{k}: {found:?}");
    }
    assert!(all[0].iter().any(|h| h.kind == "trigger" && h.parent.as_deref() == Some("VENTAS")), "{:?}", all[0]);
    assert!(all[7].iter().any(|h| h.kind == "procedure"), "a parameter the builder adds: {:?}", all[7]);
    assert!(!all[5].is_empty(), "non-ASCII");

    let q = CodeSearch { text: "ventas".into(), max_hits: 2, ..Default::default() };
    let capped = s.search_code(&q).await.unwrap().unwrap();
    assert!(capped.truncated && capped.hits.len() == 2, "{capped:?}");

    drop(s);
    admin.drop_database(&path).await.unwrap();
}
