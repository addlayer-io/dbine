//! "Deshabilitar / Habilitar índice" against a real Firebird server: a
//! secondary index goes INACTIVE and back to ACTIVE, the table still reads
//! meanwhile, and the primary key's index is refused.
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test index_toggle -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};

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

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
    out
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(sql, 10, &mut out).await;
}

async fn disabled(s: &mut Box<dyn Session>, t: &ObjectRef, name: &str) -> bool {
    let r = s.index_usage(t).await.unwrap().unwrap();
    r.indexes.iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{name} not in {r:?}")).disabled
}

#[tokio::test]
#[ignore]
async fn disable_and_enable() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    assert!(d.supports_index_toggle());
    let mut s = d.connect(&cfg, None).await.unwrap();
    try_run(&mut s, "DROP TABLE IXT_T").await;
    run(&mut s, "CREATE TABLE IXT_T (ID INTEGER NOT NULL CONSTRAINT PK_IXT_T PRIMARY KEY, FECHA DATE)").await;
    run(&mut s, "CREATE INDEX IXT_FECHA ON IXT_T (FECHA)").await;
    for i in 1..=10 {
        run(&mut s, &format!("INSERT INTO IXT_T VALUES ({i}, DATE '2026-01-01' + {i})")).await;
    }
    let t = ObjectRef { kind: "table".into(), schema: None, name: "IXT_T".into() };
    assert!(!disabled(&mut s, &t, "IXT_FECHA").await);

    let r = s.index_usage(&t).await.unwrap().unwrap();
    let ix = r.indexes.iter().find(|i| i.name == "IXT_FECHA").unwrap().clone();
    let off = d.index_toggle_script(&t, &ix, false).unwrap();
    eprintln!("disable: {off:?}");
    for st in &off.statements {
        run(&mut s, st).await;
    }
    assert!(disabled(&mut s, &t, "IXT_FECHA").await);
    // The table still reads without the index.
    let out = run(&mut s, "SELECT COUNT(*) FROM IXT_T WHERE FECHA > DATE '2026-01-05'").await;
    eprintln!("select while inactive: {out:?}");

    let on = d.index_toggle_script(&t, &ix, true).unwrap();
    eprintln!("enable: {on:?}");
    for st in &on.statements {
        run(&mut s, st).await;
    }
    assert!(!disabled(&mut s, &t, "IXT_FECHA").await);

    let pk = r.indexes.iter().find(|i| i.primary_key).unwrap();
    let refused = d.index_toggle_script(&t, pk, false);
    eprintln!("primary key: {refused:?}");
    assert!(matches!(refused, Err(Error::Unsupported(_))));

    // A named UNIQUE constraint looks like a unique index: the script warns
    // and the server refuses it.
    run(&mut s, "ALTER TABLE IXT_T ADD CONSTRAINT UQ_IXT_FECHA UNIQUE (FECHA)").await;
    let r = s.index_usage(&t).await.unwrap().unwrap();
    let uq = r.indexes.iter().find(|i| i.name == "UQ_IXT_FECHA").unwrap();
    let script = d.index_toggle_script(&t, uq, false).unwrap();
    assert!(script.warnings.iter().any(|w| w.contains("UNIQUE")), "{script:?}");
    let mut out = QueryOutcome::default();
    let err = s.execute(&script.statements[0], 10, &mut out).await.err().map(|e| e.to_string()).or(out.error.clone());
    eprintln!("unique constraint: {err:?}");
    assert!(err.is_some());
    assert!(!disabled(&mut s, &t, "UQ_IXT_FECHA").await);

    run(&mut s, "DROP TABLE IXT_T").await;
}
