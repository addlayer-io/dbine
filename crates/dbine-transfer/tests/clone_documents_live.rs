//! "Clonar tabla" on document and search engines against a live server:
//! runs one clone, or several at once under the same name, and prints how
//! each ended. The checks (documents, `_id`s, routing, indexes, settings,
//! aliases) are the caller's, with the engine's own tools.
//!
//! Ignored by default:
//!
//! ```sh
//! DBINE_CLONE_DRIVER=opensearch \
//! DBINE_CLONE_CONFIG='{"host":"localhost","port":25521}' \
//! DBINE_CLONE_TABLE=src DBINE_CLONE_KIND=index DBINE_CLONE_NAME=src_copia \
//! DBINE_CLONE_TIMES=1 \
//! cargo test -p dbine-transfer --test clone_documents_live -- --ignored --nocapture
//! ```
//!
//! `DBINE_CLONE_TIMES=2` runs two clones with the same name at the same
//! time: exactly one must succeed, and the other must leave it alone.

use dbine_driver::{ConnectionConfig, ObjectRef};
use dbine_transfer::clone_table::{clone_table, CloneControl, CloneOptions, CloneRequest, ConfigEndpoints};
use std::sync::Arc;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live server: see the top of the file"]
async fn clone_documents_on_a_live_server() {
    let id = env("DBINE_CLONE_DRIVER").expect("DBINE_CLONE_DRIVER");
    let driver = dbine_drivers::find(&id).unwrap_or_else(|| panic!("no driver '{id}'")).clone();
    let mut json: serde_json::Value = serde_json::from_str(&env("DBINE_CLONE_CONFIG").unwrap_or_else(|| "{}".into())).expect("DBINE_CLONE_CONFIG");
    json.as_object_mut().expect("config object").entry("driver").or_insert(serde_json::json!(id));
    let config: ConnectionConfig = serde_json::from_value(json).expect("ConnectionConfig");
    let source = ObjectRef {
        kind: env("DBINE_CLONE_KIND").unwrap_or_else(|| "collection".into()),
        schema: None,
        name: env("DBINE_CLONE_TABLE").expect("DBINE_CLONE_TABLE"),
    };
    let new_name = env("DBINE_CLONE_NAME").expect("DBINE_CLONE_NAME");
    let times: usize = env("DBINE_CLONE_TIMES").and_then(|t| t.parse().ok()).unwrap_or(1);
    let endpoints = Arc::new(ConfigEndpoints { driver, config, database: env("DBINE_CLONE_DATABASE") });

    let runs = (0..times).map(|i| {
        let req = CloneRequest { source: source.clone(), new_name: new_name.clone(), options: CloneOptions::default() };
        let endpoints = endpoints.clone();
        tokio::spawn(async move {
            let r = clone_table(endpoints, req, &CloneControl::default(), move |e| println!("[{i}] {}", serde_json::to_string(&e).unwrap_or_default())).await;
            (i, r)
        })
    });
    let mut ok = 0;
    for h in runs.collect::<Vec<_>>() {
        let (i, r) = h.await.unwrap();
        match r {
            Ok(rep) => {
                ok += 1;
                println!("RESULT [{i}] ok {} rows={} notes={:?}", rep.table.name, rep.rows, rep.notes);
            }
            Err(e) => println!("RESULT [{i}] error: {e}"),
        }
    }
    assert_eq!(ok, 1, "exactly one clone ends well");
}
