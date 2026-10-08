//! "Renombrar…" against a real TDengine (taosAdapter's REST port),
//! skipped without `DBINE_TEST_TDENGINE_URL`:
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//!   cargo test -p dbine-driver-tdengine --test rename -- --ignored --nocapture
//! ```
//!
//! TDengine has no keys, checks, procedures or (in the community edition)
//! views, so the fixture is a normal table with a stream on its column, a
//! second table with a same-named column, and a supertable with a tag
//! index, a subtable and a stream on another tag. The streams are
//! rewritten, dropped before the rename and created again after it, as the
//! app plans it (`DropCreate`).

use dbine_driver::dependencies::DependencyScan;
use dbine_driver::rename::{rewrite_references, RenameRequest, RenameTarget, RewriteOptions};
use dbine_driver::{Confidence, ConnectionConfig, Driver, ObjectRef, QueryOutcome, Relation, Session};

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn request(s: &mut Box<dyn Session>, kind: &str, table: &str, column: &str, new: &str) -> RenameRequest {
    let t = s.database_schema().await.unwrap().into_iter().find(|t| t.name == table);
    RenameRequest {
        target: RenameTarget::Column { table: ObjectRef { kind: kind.into(), schema: Some("dbine_rename".into()), name: table.into() }, column: column.into() },
        new_name: new.into(),
        table: t,
        definition: None,
    }
}

/// The rename as the app runs it: the streams that name the target
/// rewritten, dropped first and created after. Returns the rewritten ones.
async fn apply(d: &dyn Driver, s: &mut Box<dyn Session>, req: &RenameRequest) -> dbine_driver::Result<Vec<String>> {
    let spec = d.rename_spec().unwrap();
    let dialect = d.script_dialect();
    let scan = DependencyScan::new(d.info(), dialect, d.capabilities().foreign_keys);
    let report = s.dependents(&req.target.dependency_target(), &scan).await.unwrap();
    let (mut drops, mut creates, mut names) = (Vec::new(), Vec::new(), Vec::new());
    for dep in report.items.iter().filter(|x| x.relation == Relation::Code && x.confidence != Confidence::Review) {
        let obj = ObjectRef { kind: dep.kind.clone(), schema: dep.schema.clone(), name: dep.name.clone() };
        let def = s.definition(&obj).await.unwrap().unwrap();
        let r = rewrite_references(&def, &dialect, &req.target.rewrite_target(), &req.new_name, &spec, &RewriteOptions { dependent_schema: dep.schema.clone(), ..Default::default() });
        assert!(r.unresolved.is_empty() && !r.edits.is_empty(), "{}: {def} -> {r:?}", dep.name);
        assert_eq!(dep.kind, "stream");
        drops.push(format!("DROP STREAM IF EXISTS `{}`", dep.name));
        creates.push(r.text);
        names.push(dep.name.clone());
    }
    let statements: Vec<String> = drops.into_iter().chain(d.rename_script(req)?.statements).chain(creates).collect();
    for st in &statements {
        eprintln!("> {st}");
        run(s, st).await?;
    }
    Ok(names)
}

async fn stream_sql(s: &mut Box<dyn Session>, name: &str) -> String {
    let out = run(s, &format!("SELECT sql FROM information_schema.ins_streams WHERE stream_name = '{name}'")).await.unwrap();
    out.results[0].rows.first().and_then(|r| r[0].as_str()).unwrap_or_default().to_string()
}

async fn columns(s: &mut Box<dyn Session>, table: &str) -> Vec<String> {
    s.columns(&ObjectRef { kind: "table".into(), schema: Some("dbine_rename".into()), name: table.into() }).await.unwrap().into_iter().map(|c| c.name).collect()
}

#[tokio::test]
#[ignore]
async fn tdengine_rename() {
    let Some(url) = std::env::var("DBINE_TEST_TDENGINE_URL").ok() else {
        eprintln!("DBINE_TEST_TDENGINE_URL not set; skipping");
        return;
    };
    let url = reqwest::Url::parse(&url).expect("URL");
    let cfg = ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    };
    let d = dbine_driver_tdengine::drivers().into_iter().next().unwrap();
    let db = "dbine_rename";
    {
        let mut s = d.connect(&cfg, None).await.unwrap();
        let _ = run(&mut s, "DROP STREAM IF EXISTS dbine_rename_s1; DROP STREAM IF EXISTS dbine_rename_s2").await;
        let _ = s.drop_database(db).await;
        s.create_database(db).await.unwrap();
    }
    let mut s = d.connect(&cfg, Some(db)).await.unwrap();
    run(
        &mut s,
        "CREATE TABLE t (ts TIMESTAMP, pepe INT);
         CREATE TABLE t2 (ts TIMESTAMP, pepe INT);
         CREATE STABLE m (ts TIMESTAMP, v INT) TAGS (d INT, loc VARCHAR(10), zona INT);
         CREATE INDEX ix_loc ON m (loc);
         CREATE TABLE c1 USING m TAGS (1, 'a', 7);
         INSERT INTO t VALUES (now, 5);
         INSERT INTO t2 VALUES (now, 6);
         INSERT INTO c1 VALUES (now, 1);
         CREATE STREAM dbine_rename_s1 INTO out1 AS SELECT _wstart, sum(pepe) AS total FROM t INTERVAL(10s);
         CREATE STREAM dbine_rename_s2 INTO out2 AS SELECT _wstart, count(zona) AS c FROM m INTERVAL(10s);",
    )
    .await
    .unwrap();

    let spec = d.rename_spec().unwrap();

    // Column of a normal table: the stream that names it is rewritten.
    let req = request(&mut s, "table", "t", "pepe", "PepA").await;
    assert!(spec.allows(&req.target));
    assert_eq!(apply(d.as_ref(), &mut s, &req).await.unwrap(), ["dbine_rename_s1"]);
    assert!(stream_sql(&mut s, "dbine_rename_s1").await.contains("sum(`PepA`)"), "{}", stream_sql(&mut s, "dbine_rename_s1").await);
    assert_eq!(columns(&mut s, "t").await, vec!["ts", "PepA"]);
    assert_eq!(columns(&mut s, "t2").await, vec!["ts", "pepe"]);
    let out = run(&mut s, "SELECT `PepA` FROM t").await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);

    // The timestamp column of a normal table renames too.
    let req = request(&mut s, "table", "t2", "ts", "momento").await;
    assert!(apply(d.as_ref(), &mut s, &req).await.unwrap().is_empty());
    assert_eq!(columns(&mut s, "t2").await, vec!["momento", "pepe"]);

    // A supertable tag: its index and the subtable follow it.
    let req = request(&mut s, "supertable", "m", "loc", "lugar").await;
    assert!(apply(d.as_ref(), &mut s, &req).await.unwrap().is_empty());
    let out = run(&mut s, "SELECT column_name FROM information_schema.ins_indexes WHERE db_name = 'dbine_rename' AND index_name = 'ix_loc'").await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!("lugar"));
    let out = run(&mut s, "SELECT lugar FROM c1").await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!("a"));

    // A tag a stream uses: the server refuses it alone, so the stream is
    // dropped first and created again on the new name.
    let req = request(&mut s, "supertable", "m", "zona", "region").await;
    let st = d.rename_script(&req).unwrap().statements.remove(0);
    let err = run(&mut s, &st).await.unwrap_err();
    eprintln!("tag used by a stream: {err}");
    assert_eq!(apply(d.as_ref(), &mut s, &req).await.unwrap(), ["dbine_rename_s2"]);
    assert!(stream_sql(&mut s, "dbine_rename_s2").await.contains("count(region)"), "{}", stream_sql(&mut s, "dbine_rename_s2").await);
    // The stream writes into its old output table again.
    run(&mut s, "INSERT INTO c1 VALUES (now, 2)").await.unwrap();

    // Supertable columns and subtables are refused before reaching the server.
    let req = request(&mut s, "supertable", "m", "v", "w").await;
    assert!(d.rename_script(&req).is_err());
    let req = request(&mut s, "subtable", "c1", "lugar", "x").await;
    assert!(d.rename_script(&req).is_err());

    run(&mut s, "DROP STREAM IF EXISTS dbine_rename_s1; DROP STREAM IF EXISTS dbine_rename_s2").await.unwrap();
    s.drop_database(db).await.unwrap();
}
