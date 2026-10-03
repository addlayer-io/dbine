//! "Deshabilitar / Habilitar índice" against a real CockroachDB (22.2+):
//! `ALTER INDEX … NOT VISIBLE` / `VISIBLE`. Reads `DBINE_TEST_COCKROACH_URL`
//! (`postgres://user:pass@host:port/db`) and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//!   cargo test -p dbine-driver-postgres --test index_toggle -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, ObjectRef, QueryOutcome, Session};
use std::sync::Arc;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

async fn disabled(s: &mut Box<dyn Session>, t: &ObjectRef, name: &str) -> bool {
    let r = s.index_usage(t).await.unwrap().expect("a report");
    r.indexes.iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{name} not listed")).disabled
}

#[tokio::test]
#[ignore]
async fn cockroach_disable_and_enable() {
    let Some(cfg) = cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL") else {
        eprintln!("DBINE_TEST_COCKROACH_URL not set; skipping");
        return;
    };
    let d = driver("cockroachdb");
    assert!(d.supports_index_toggle());
    assert!(!driver("postgres").supports_index_toggle());
    let mut s = d.connect(&cfg, None).await.expect("connect");
    run(&mut s, "DROP SCHEMA IF EXISTS dbine_tg CASCADE").await;
    run(&mut s, "CREATE SCHEMA dbine_tg").await;
    run(&mut s, "CREATE TABLE dbine_tg.\"Pedidos\" (id int PRIMARY KEY, fecha date, INDEX ix_fecha (fecha))").await;
    run(&mut s, "INSERT INTO dbine_tg.\"Pedidos\" VALUES (1, '2026-01-02'), (2, '2026-03-04')").await;
    let t = ObjectRef { kind: "table".into(), schema: Some("dbine_tg".into()), name: "Pedidos".into() };
    assert!(!disabled(&mut s, &t, "ix_fecha").await);

    let report = s.index_usage(&t).await.unwrap().unwrap();
    let ix = report.indexes.iter().find(|i| i.name == "ix_fecha").unwrap().clone();
    let off = d.index_toggle_script(&t, &ix, false).unwrap();
    assert_eq!(off.warnings, ["El índice se sigue manteniendo en cada escritura; el optimizador deja de usarlo."]);
    for st in &off.statements {
        run(&mut s, st).await;
    }
    assert!(disabled(&mut s, &t, "ix_fecha").await);
    assert!(!disabled(&mut s, &t, "Pedidos_pkey").await);
    run(&mut s, "SELECT count(*) FROM dbine_tg.\"Pedidos\" WHERE fecha > '2026-02-01'").await;
    run(&mut s, "INSERT INTO dbine_tg.\"Pedidos\" VALUES (3, '2026-05-06')").await;

    for st in d.index_toggle_script(&t, &ix, true).unwrap().statements {
        run(&mut s, &st).await;
    }
    assert!(!disabled(&mut s, &t, "ix_fecha").await);

    let pk = report.indexes.iter().find(|i| i.primary_key).unwrap();
    assert!(matches!(d.index_toggle_script(&t, pk, false), Err(Error::Unsupported(_))));

    run(&mut s, "DROP SCHEMA dbine_tg CASCADE").await;
}
