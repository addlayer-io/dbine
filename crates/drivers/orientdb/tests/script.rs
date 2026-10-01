//! Editor scripts against a real server, as in tests/integration.rs:
//! statement by statement (the app splits with the driver's dialect), each
//! statement's place, errors placed on their line.
//!
//! `DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 cargo test -p dbine-driver-orientdb --test script -- --ignored`

use dbine_driver::{ConnectionConfig, QueryOutcome, ScriptMode, Session};
use serde_json::json;

fn cfg(url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.map(|_| out)
}

#[tokio::test]
#[ignore]
async fn statements_one_by_one() {
    let Ok(url) = std::env::var("DBINE_TEST_ORIENTDB_URL") else { return };
    let d = dbine_driver_orientdb::drivers().remove(0);
    assert_eq!(d.script_mode(), ScriptMode::PerStatement);
    let c = cfg(&url);
    let mut admin = d.connect(&c, None).await.unwrap();
    let _ = admin.drop_database("dbine_script").await;
    admin.create_database("dbine_script").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_script")).await.unwrap();

    // The units the app sends one at a time: a `;` inside an escaped
    // string doesn't split.
    let script = "CREATE CLASS Nota;\nINSERT INTO Nota SET t = 'it\\'s; ok';\nSELECT t FROM Nota";
    let units = d.split_script(script);
    assert_eq!(units.len(), 3, "{units:?}");
    for u in &units {
        run(&mut s, &u.text).await.unwrap();
    }
    let out = run(&mut s, "SELECT t FROM Nota").await.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!("it's; ok"));

    // The whole text at once (Users and permissions, Backups…): each
    // statement placed, the failing one on its line.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT count(*) FROM Nota;\nSELECT FROM Nope", 10, &mut out).await.unwrap_err().to_script_error();
    assert_eq!(e.line, Some(2), "{e:?}");
    assert_eq!((out.results[0].statement, out.results[0].line), (Some(0), Some(1)));
    admin.drop_database("dbine_script").await.unwrap();
}
