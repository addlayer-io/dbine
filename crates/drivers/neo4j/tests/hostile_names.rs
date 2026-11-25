//! Names that hold the characters of a unicode escape (`\u0060` is a
//! backtick to Neo4j's parser) against a real server: every query the
//! driver writes from them reads them as names. The payload only returns a
//! constant. The test removes its data and its index, even when a check
//! fails. Skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 \
//!   cargo test -p dbine-driver-neo4j --test hostile_names -- --ignored
//! ```

use dbine_driver::dependencies::LineageScope;
use dbine_driver::{ConnectionConfig, DependencyScan, ObjectRef, QueryOutcome, Session};

/// The stored name, literally: a backslash, `u0060`, and the rest.
const NAME: &str = r"dbsec\u0060) WITH count(*) AS c RETURN 12345 //";
/// The same name written by hand so that the setup doesn't depend on the
/// driver's quoting (`\u005C` is the parser's backslash).
const RAW: &str = r"`dbsec\u005Cu0060) WITH count(*) AS c RETURN 12345 //`";
/// A property whose value and name hold escapes too.
const TEXT: &str = r"a\u0027, b: \u000A c";

async fn run(s: &mut Box<dyn Session>, text: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out)
        .await
        .map_err(|e| format!("{text}: {e}"))?;
    match out
        .error
        .clone()
        .or(out.errors.first().map(|e| e.message.clone()))
    {
        Some(e) => Err(format!("{text}: {e}")),
        None => Ok(out),
    }
}

async fn check(s: &mut Box<dyn Session>) -> Result<(), String> {
    let ensure = |ok: bool, what: String| if ok { Ok(()) } else { Err(what) };
    let driver = dbine_drivers_neo4j();
    let scan = DependencyScan::new(
        driver.info(),
        driver.script_dialect(),
        driver.capabilities().foreign_keys,
    );
    let objects = s.list_objects().await.map_err(|e| e.to_string())?;
    for kind in ["label", "relationship"] {
        let found = objects.iter().any(|o| o.kind == kind && o.name == NAME);
        ensure(found, format!("{kind} not listed: {objects:?}"))?;
        let obj = ObjectRef {
            kind: kind.into(),
            schema: None,
            name: NAME.into(),
        };
        let definition = s
            .definition(&obj)
            .await
            .map_err(|e| format!("definition of the {kind}: {e}"))?
            .unwrap_or_default();
        let count = definition.lines().next().unwrap_or_default();
        let expected = if kind == "label" {
            "— 2 nodos"
        } else {
            "— 1 relaciones"
        };
        ensure(
            count.ends_with(expected),
            format!("wrong count (did the name run as Cypher?): {count}"),
        )?;
        // The comment lines can't be ended by a decoded line break.
        ensure(
            !definition
                .lines()
                .any(|l| l.starts_with("//") && l.contains(r"\u")),
            format!("a comment line holds a \\u: {definition}"),
        )?;
        let columns = s.columns(&obj).await.map_err(|e| e.to_string())?;
        ensure(
            columns.iter().any(|c| c.name == NAME),
            format!("{kind} columns: {columns:?}"),
        )?;
        let browse = s.browse_query(&obj, 10);
        let out = run(s, &browse).await?;
        let rows = out.results.first().map_or(0, |r| r.rows.len());
        ensure(rows >= 1, format!("browse {kind}: {rows} rows"))?;
    }
    s.row_estimates().await.map_err(|e| e.to_string())?;
    // The lineage's precompute reads no label's or type's definition.
    let graph = s
        .lineage_graph("", &LineageScope::default(), &scan)
        .await
        .map_err(|e| e.to_string())?;
    ensure(
        !scan
            .written_kinds
            .iter()
            .any(|k| k == "label" || k == "relationship"),
        format!("labels are scanned: {:?}", scan.written_kinds),
    )?;
    ensure(
        graph.unreadable.is_empty(),
        format!("{:?}", graph.unreadable),
    )?;
    // The server's `createStatement`, as the driver shows it, recreates the
    // index on the same names.
    let index = ObjectRef {
        kind: "index".into(),
        schema: None,
        name: "dbine_sec_idx".into(),
    };
    let create = s
        .definition(&index)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no index definition")?;
    run(s, "DROP INDEX dbine_sec_idx").await?;
    run(s, &create).await?;
    let out = run(
        s,
        "SHOW INDEXES YIELD name, labelsOrTypes WHERE name = 'dbine_sec_idx' RETURN labelsOrTypes[0] AS l",
    )
    .await?;
    let label = out.results[0].rows.first().and_then(|r| r[0].as_str());
    ensure(label == Some(NAME), format!("recreated on {label:?}"))?;
    Ok(())
}

fn dbine_drivers_neo4j() -> std::sync::Arc<dyn dbine_driver::Driver> {
    dbine_driver_neo4j::drivers()
        .into_iter()
        .find(|d| d.info().id == "neo4j")
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn names_with_unicode_escapes_stay_names() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_URL") else {
        return;
    };
    let (auth, hp) = url.rsplit_once('@').expect("user:password@host:port");
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "neo4j".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    let mut s = dbine_drivers_neo4j().connect(&cfg, None).await.unwrap();
    let clean = format!("MATCH (n:{RAW}) DETACH DELETE n");
    run(&mut s, &clean).await.unwrap();
    run(&mut s, "DROP INDEX dbine_sec_idx IF EXISTS")
        .await
        .unwrap();
    let setup =
        format!("CREATE (:{RAW} {{{RAW}: 1}})-[:{RAW} {{{RAW}: 1}}]->(:{RAW} {{{RAW}: 2}})");
    let result = async {
        run(&mut s, &setup).await?;
        run(
            &mut s,
            &format!("CREATE INDEX dbine_sec_idx FOR (n:{RAW}) ON (n.{RAW})"),
        )
        .await?;
        check(&mut s).await?;
        // A value with escapes in a script the driver writes comes back as
        // it was.
        let target = ObjectRef {
            kind: "label".into(),
            schema: None,
            name: NAME.into(),
        };
        let insert = dbine_drivers_neo4j()
            .insert_script(&target, &["v".into()], &[vec![TEXT.into()]])
            .map_err(|e| e.to_string())?;
        run(&mut s, insert.trim_end_matches([';', '\n'])).await?;
        let out = run(
            &mut s,
            &format!("MATCH (n:{RAW}) WHERE n.v IS NOT NULL RETURN n.v AS v"),
        )
        .await?;
        let back = out.results[0].rows.first().and_then(|r| r[0].as_str());
        if back != Some(TEXT) {
            return Err(format!("value came back as {back:?}"));
        }
        run(
            &mut s,
            &format!("MATCH (n:{RAW}) WHERE n.v IS NOT NULL DELETE n"),
        )
        .await?;
        Ok::<(), String>(())
    }
    .await;
    run(&mut s, "DROP INDEX dbine_sec_idx IF EXISTS")
        .await
        .unwrap();
    run(&mut s, &clean).await.unwrap();
    result.unwrap();
}
