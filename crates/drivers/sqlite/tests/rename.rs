//! "Renombrar…" on a real file (no server needed, so it runs by default).
//! SQLite follows views, triggers, indexes, checks and foreign keys by
//! itself, so nothing is rewritten (see `support/rename_flow.rs`).

#[path = "support/rename_flow.rs"]
mod rename_flow;

use dbine_driver::{ConnectionConfig, QueryOutcome};

#[tokio::test]
async fn sqlite_rename_with_impact() {
    let path = std::env::temp_dir().join(format!("dbine-sqlite-rename-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.display().to_string(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    // The script turns it off itself, whatever the connection had.
    let mut out = QueryOutcome::default();
    s.execute("PRAGMA legacy_alter_table = ON", 10, &mut out).await.unwrap();
    rename_flow::rename_flow(d.as_ref(), s.as_mut()).await;
    drop(s);
    let _ = std::fs::remove_file(&path);
}
