//! The function catalog (issue #97) against a real kdb+ (see `live.rs` for
//! the container), skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_KDB_URL=127.0.0.1:15000:dbine:dbinepass \
//!   cargo test -p dbine-driver-kdb --test functions -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, FunctionKind, QueryOutcome, Session};

fn cfg(read_only: bool) -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_KDB_URL").ok()?;
    let mut parts = url.splitn(4, ':');
    let host = parts.next().unwrap_or("127.0.0.1").to_string();
    let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(5000);
    Some(ConnectionConfig {
        host,
        port,
        username: parts.next().map(str::to_string),
        password: parts.next().map(str::to_string),
        read_only,
        options: [("tcps".to_string(), "false".to_string())].into(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, q: &str) {
    let mut out = QueryOutcome::default();
    s.execute(q, 10, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
}

#[tokio::test]
#[ignore]
async fn kdb_catalog() {
    let Some(c) = cfg(false) else {
        eprintln!("DBINE_TEST_KDB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    println!("{}", s.server_version().await.unwrap());
    run(
        &mut s,
        ".dbine_fn.vwap:{[t;s] exec size wavg price from t where sym=s}; dbine_fn_root:{[a;b;c] a+b*c}",
    )
    .await;
    let catalog = s.function_catalog().await;
    let mut ro = d.connect(&cfg(true).unwrap(), None).await.unwrap();
    let read_only = ro.function_catalog().await;
    // A namespace lives in the root (`` ` ``), not in `` `. ``: the
    // functional delete on `` ` `` removes `.dbine_fn` itself.
    run(
        &mut s,
        "![`;();0b;enlist`dbine_fn]; delete dbine_fn_root from `.",
    )
    .await;
    // Signals (and `run` panics) if any of them is still there.
    run(
        &mut s,
        "if[(`dbine_fn in key `) or `dbine_fn_root in key `.; '`dbine_fn_left]",
    )
    .await;
    let catalog = catalog.unwrap();
    println!("{} functions", catalog.len());
    let get = |schema: &str, n: &str| catalog.iter().find(|f| f.schema == schema && f.name == n);
    // .Q.res
    assert_eq!(get("", "avg").expect("avg").kind, FunctionKind::Aggregate);
    assert!(get("", "within").is_some());
    // .q, with its parameters where it's a lambda.
    assert!(get("", "xbar").is_some());
    assert!(get("", "lj").is_some());
    // q-sql and control words are keywords, not functions.
    assert!(get("", "select").is_none());
    assert!(get("", "while").is_none());
    assert!(get("", "ww").is_none(), "wj's helper");
    // kdb+'s own namespaces stay out.
    assert!(!catalog.iter().any(|f| [".Q", ".z", ".s", ".h", ".j", ".o"]
        .contains(&f.schema.as_str())
        || f.name.starts_with(".Q")));
    // The user's.
    let f = get(".dbine_fn", "vwap").expect(".dbine_fn.vwap");
    assert_eq!((f.kind, f.args.as_str()), (FunctionKind::Scalar, "[t;s]"));
    assert_eq!(
        get("", "dbine_fn_root").expect("root function").args,
        "[a;b;c]"
    );
    // Not q.
    assert!(get("", "approx_distinct").is_none());
    // The same on a read-only connection.
    assert!(read_only.unwrap().iter().any(|f| f.name == "vwap"));
}
