//! "Deshabilitar / Habilitar índice" (`ALTER INDEX … INVISIBLE / VISIBLE`,
//! rebuilding an UNUSABLE one) against a real server. Needs a user that can
//! create users (SYSTEM), like `index_usage.rs`:
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test index_toggle -- --ignored --nocapture --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, IndexUsage, ObjectRef, QueryOutcome, Session};
use std::sync::Arc;

fn config_from(url: &str) -> ConnectionConfig {
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

/// Runs `sql`; the server's error, if any.
async fn try_run(s: &mut Box<dyn Session>, sql: &str) -> Option<String> {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 100, &mut out).await {
        Err(e) => Some(e.to_string()),
        Ok(()) => out.error.take().map(|e| e.to_string()),
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    if let Some(e) = try_run(s, sql).await {
        panic!("{sql}: {e}");
    }
}

const SCHEMA: &str = "DBINE_TOGGLE";

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some(SCHEMA.into()), name: name.into() }
}

async fn index(s: &mut Box<dyn Session>, t: &ObjectRef, name: &str) -> IndexUsage {
    let r = s.index_usage(t).await.unwrap().unwrap();
    r.indexes.into_iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{name} not listed"))
}

/// Runs the script that disables (`enable` false) or enables the index as
/// `index_usage` reports it now; returns the index afterwards.
async fn toggle(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, t: &ObjectRef, name: &str, enable: bool) -> IndexUsage {
    let ix = index(s, t, name).await;
    let script = d.index_toggle_script(t, &ix, enable).unwrap();
    for st in &script.statements {
        eprintln!("{st}");
        run(s, st).await;
    }
    index(s, t, name).await
}

#[tokio::test]
#[ignore]
async fn disable_and_enable_live() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let cfg = config_from(&url);
    let d = dbine_driver_oracle::drivers().remove(0);
    assert!(d.supports_index_toggle());
    assert!(dbine_driver_oracle::drivers().iter().all(|d| d.supports_index_toggle()), "Autonomous too");
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let _ = try_run(&mut s, &format!("DROP USER {SCHEMA} CASCADE")).await;
    run(&mut s, &format!("CREATE USER {SCHEMA} IDENTIFIED BY \"Toggle_123\" QUOTA UNLIMITED ON users")).await;
    run(
        &mut s,
        "CREATE TABLE dbine_toggle.pedidos (id NUMBER CONSTRAINT pk_pedidos PRIMARY KEY, fecha DATE, total NUMBER);
CREATE INDEX dbine_toggle.ix_fecha ON dbine_toggle.pedidos (fecha);
INSERT INTO dbine_toggle.pedidos SELECT LEVEL, DATE '2026-01-01' + LEVEL, LEVEL FROM dual CONNECT BY LEVEL <= 200;
COMMIT;",
    )
    .await;
    let t = table("PEDIDOS");

    // A plain index: invisible and back.
    assert!(!index(&mut s, &t, "IX_FECHA").await.disabled);
    let off = toggle(&d, &mut s, &t, "IX_FECHA", false).await;
    assert!(off.disabled && off.kind.ends_with("INVISIBLE"), "{off:?}");
    // The table still reads, and the index is still maintained.
    run(&mut s, "SELECT COUNT(*) FROM dbine_toggle.pedidos WHERE fecha > DATE '2026-03-01'").await;
    run(&mut s, "INSERT INTO dbine_toggle.pedidos VALUES (1000, DATE '2027-01-01', 1)").await;
    let on = toggle(&d, &mut s, &t, "IX_FECHA", true).await;
    assert!(!on.disabled && on.kind == "NORMAL", "{on:?}");

    // The primary key's index: invisible, it still enforces the key.
    let pk = toggle(&d, &mut s, &t, "PK_PEDIDOS", false).await;
    assert!(pk.disabled && pk.primary_key, "{pk:?}");
    let dup = try_run(&mut s, "INSERT INTO dbine_toggle.pedidos VALUES (1, NULL, NULL)").await;
    assert!(dup.as_deref().is_some_and(|e| e.contains("ORA-00001")), "{dup:?}");
    assert!(!toggle(&d, &mut s, &t, "PK_PEDIDOS", true).await.disabled);

    // UNUSABLE and invisible: enabling rebuilds it and shows it.
    run(&mut s, "ALTER INDEX dbine_toggle.ix_fecha UNUSABLE").await;
    run(&mut s, "ALTER INDEX dbine_toggle.ix_fecha INVISIBLE").await;
    let dead = index(&mut s, &t, "IX_FECHA").await;
    assert!(dead.disabled && dead.kind == "NORMAL INVISIBLE UNUSABLE", "{dead:?}");
    let back = toggle(&d, &mut s, &t, "IX_FECHA", true).await;
    assert!(!back.disabled && back.kind == "NORMAL", "{back:?}");

    // Some partitions UNUSABLE: rebuilt one by one.
    run(
        &mut s,
        "CREATE TABLE dbine_toggle.ventas (id NUMBER, d NUMBER)
  PARTITION BY RANGE (d) (PARTITION p1 VALUES LESS THAN (10), PARTITION p2 VALUES LESS THAN (MAXVALUE));
CREATE INDEX dbine_toggle.ix_ventas ON dbine_toggle.ventas (id) LOCAL;
INSERT INTO dbine_toggle.ventas VALUES (1, 5);
COMMIT;
ALTER INDEX dbine_toggle.ix_ventas MODIFY PARTITION p1 UNUSABLE;",
    )
    .await;
    let v = table("VENTAS");
    let part = index(&mut s, &v, "IX_VENTAS").await;
    assert!(part.disabled && part.kind == "NORMAL PARTITIONS UNUSABLE", "{part:?}");
    let back = toggle(&d, &mut s, &v, "IX_VENTAS", true).await;
    assert!(!back.disabled && back.kind == "NORMAL", "{back:?}");

    // An IOT's index: refused (ORA-25176 if tried).
    run(&mut s, "CREATE TABLE dbine_toggle.iot (id NUMBER PRIMARY KEY, a NUMBER) ORGANIZATION INDEX").await;
    let iot = table("IOT");
    let ix = s.index_usage(&iot).await.unwrap().unwrap().indexes.remove(0);
    assert!(ix.kind.starts_with("IOT"), "{ix:?}");
    assert!(matches!(d.index_toggle_script(&iot, &ix, false), Err(Error::Unsupported(_))));

    run(&mut s, &format!("DROP USER {SCHEMA} CASCADE")).await;
}
