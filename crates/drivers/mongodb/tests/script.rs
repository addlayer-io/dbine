//! Editor scripts against a real server, as in tests/integration.rs:
//! `use` lasting across runs, `show`, statements on lines or after `;`,
//! and errors with their code and place.
//!
//! `DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//!   cargo test -p dbine-driver-mongodb --test script -- --ignored`

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::json;

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_MONGODB_URL").ok()?;
    let mut c = ConnectionConfig { driver: "mongodb".into(), database: "dbine_script_a".into(), ..Default::default() };
    c.options.insert("connection_string".into(), url);
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.map(|_| out)
}

#[tokio::test]
#[ignore]
async fn use_show_and_errors() {
    let Some(c) = cfg() else { return };
    let d = &dbine_driver_mongodb::drivers()[0];
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "db.dropDatabase()\nuse dbine_script_b\ndb.dropDatabase()").await.unwrap();

    // `use` switches the database for the rest of the script and later runs.
    let out = run(&mut s, "use dbine_script_b\ndb.t.insertOne({ _id: 1, a: 'b' })\nshow collections").await.unwrap();
    assert!(out.log.iter().any(|m| m.text == "Base de datos actual: dbine_script_b"), "{:?}", out.log);
    assert_eq!(out.results.last().unwrap().rows, vec![vec![json!("t")]]);
    assert_eq!((out.results[0].statement, out.results[0].line, out.results[0].offset), (Some(1), Some(2), Some(19)));
    let out = run(&mut s, "db.t.find()").await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1, "still on dbine_script_b");
    let out = run(&mut s, "show dbs").await.unwrap();
    let names: Vec<_> = out.results[0].rows.iter().map(|r| r[0].clone()).collect();
    assert!(names.contains(&json!("dbine_script_b")), "{names:?}");
    run(&mut s, "use dbine_script_a").await.unwrap();
    assert!(run(&mut s, "db.t.find()").await.unwrap().results[0].rows.is_empty());

    // A server error: its code and the failing statement's line; the
    // statements before it ran, the ones after it didn't (mongosh stops).
    let mut out = QueryOutcome::default();
    let e = s
        .execute("db.u.insertOne({ _id: 1 })\ndb.u.insertOne({ _id: 1 })\ndb.u.insertOne({ _id: 2 })", 10, &mut out)
        .await
        .unwrap_err()
        .to_script_error();
    assert_eq!((e.code.as_deref(), e.line, e.offset), (Some("11000"), Some(2), Some(27)), "{e:?}");
    assert_eq!(out.results.len(), 1);
    let e = run(&mut s, "db.u.aggregate([{ $nope: 1 }])").await.unwrap_err().to_script_error();
    assert!(e.code.is_some() && e.message.contains('('), "{e:?}");

    // A syntax error runs nothing and points at its line.
    let mut out = QueryOutcome::default();
    let e = s.execute("db.u.insertOne({ _id: 3 })\ndb.u.find({ a: })", 10, &mut out).await.unwrap_err().to_script_error();
    assert_eq!(e.line, Some(2));
    assert!(out.results.is_empty());
    assert_eq!(run(&mut s, "db.u.countDocuments({})").await.unwrap().results[0].rows[0][0], json!(1));

    assert!(run(&mut s, "show profile").await.is_err());
    run(&mut s, "db.dropDatabase()\nuse dbine_script_b\ndb.dropDatabase()").await.unwrap();
}
