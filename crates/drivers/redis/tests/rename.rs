//! "Renombrar…" against a real server (see `integration.rs`):
//! `DBINE_TEST_REDIS_URL=redis://localhost:25400 cargo test -p dbine-driver-redis --test rename -- --ignored`.
//! `DBINE_TEST_VALKEY_URL` / `DBINE_TEST_DRAGONFLY_URL` run it against
//! those servers too, when set.

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, RenameRequest, RenameTarget, Session};
use serde_json::{json, Value};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.trim_start_matches("redis://");
    let (host, port) = rest.split_once(':').unwrap_or((rest, "6379"));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.trim_end_matches('/').parse().unwrap(), ..Default::default() }
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 100, &mut out).await {
        out.error = Some(e.to_string());
    }
    out
}

fn failed(out: &QueryOutcome) -> Option<String> {
    out.error.clone().or_else(|| out.errors.first().map(|e| e.message.clone()))
}

/// The first cell of the last result.
async fn cell(s: &mut Box<dyn Session>, text: &str) -> Value {
    let out = run(s, text).await;
    assert!(failed(&out).is_none(), "{text}: {:?}", failed(&out));
    out.results.last().and_then(|r| r.rows.first()).and_then(|r| r.first()).cloned().unwrap_or(Value::Null)
}

fn req(old: &str, new: &str) -> RenameRequest {
    RenameRequest {
        target: RenameTarget::Object { object: ObjectRef { kind: kinds::KEY.into(), schema: None, name: old.into() }, parent: None },
        new_name: new.into(),
        table: None,
        definition: None,
    }
}

async fn rename(driver: &str, url: &str) {
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let mut s = d.connect(&cfg(driver, url), Some("db6")).await.unwrap();
    assert!(d.rename_spec().is_some_and(|sp| sp.allows(&req("a", "b").target)));
    let out = run(&mut s, "DEL \"rn:T\" \"rn:T nuevo\" rn:otra rn:falta rn:x").await;
    assert!(failed(&out).is_none(), "{:?}", failed(&out));
    run(&mut s, "HSET \"rn:T\" id 1 pepe hola\nEXPIRE \"rn:T\" 3600\nSET rn:otra ocupada").await;

    // The rename: value and TTL kept, old key gone.
    let script = d.rename_script(&req("rn:T", "rn:T nuevo")).unwrap();
    for st in &script.statements {
        let out = run(&mut s, st).await;
        assert!(failed(&out).is_none(), "{st}: {:?}", failed(&out));
    }
    assert_eq!(cell(&mut s, "EXISTS \"rn:T\"").await, json!(0));
    assert_eq!(cell(&mut s, "HGET \"rn:T nuevo\" pepe").await, json!("hola"));
    let ttl = cell(&mut s, "TTL \"rn:T nuevo\"").await.as_i64().unwrap();
    assert!(ttl > 3000, "TTL {ttl}");

    // An existing key with the new name: refused, nothing overwritten.
    let script = d.rename_script(&req("rn:T nuevo", "rn:otra")).unwrap();
    let out = run(&mut s, &script.statements[0]).await;
    let err = failed(&out).expect("must fail");
    assert!(err.contains("ya existe"), "{err}");
    assert_eq!(cell(&mut s, "GET rn:otra").await, json!("ocupada"));
    assert_eq!(cell(&mut s, "HGET \"rn:T nuevo\" pepe").await, json!("hola"));

    // A key that no longer exists: the server's error.
    let script = d.rename_script(&req("rn:falta", "rn:x")).unwrap();
    let out = run(&mut s, &script.statements[0]).await;
    assert!(failed(&out).is_some());
    assert_eq!(cell(&mut s, "EXISTS rn:x").await, json!(0));

    run(&mut s, "DEL \"rn:T nuevo\" rn:otra").await;
}

#[tokio::test]
#[ignore]
async fn redis() {
    rename("redis", &std::env::var("DBINE_TEST_REDIS_URL").expect("DBINE_TEST_REDIS_URL")).await;
}

#[tokio::test]
#[ignore]
async fn valkey() {
    let Ok(url) = std::env::var("DBINE_TEST_VALKEY_URL") else { return };
    rename("valkey", &url).await;
}

#[tokio::test]
#[ignore]
async fn dragonfly() {
    let Ok(url) = std::env::var("DBINE_TEST_DRAGONFLY_URL") else { return };
    rename("dragonfly", &url).await;
}
