//! The function catalog (#97) against real servers:
//! `DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687` (neo4j:5) and
//! `DBINE_TEST_MEMGRAPH_URL=localhost:27687`, then
//! `cargo test -p dbine-driver-neo4j --test functions -- --ignored`.
//! URLs: `[user:password@]host:port`.

use dbine_driver::{ConnectionConfig, FunctionInfo, FunctionKind, QueryOutcome, Session};

async fn open(id: &str, url: &str) -> Box<dyn Session> {
    let (auth, hp) = url
        .rsplit_once('@')
        .map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth
        .and_then(|a| a.split_once(':'))
        .map_or((None, None), |(u, p)| {
            (Some(u.to_string()), Some(p.to_string()))
        });
    let cfg = ConnectionConfig {
        driver: id.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: user,
        password: pass,
        read_only: true,
        ..Default::default()
    };
    dbine_driver_neo4j::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
        .connect(&cfg, None)
        .await
        .unwrap()
}

fn find<'a>(c: &'a [FunctionInfo], schema: &str, name: &str) -> Option<&'a FunctionInfo> {
    c.iter().find(|f| f.name == name && f.schema == schema)
}

/// What the server answers to `RETURN <call>`: `None` when it runs, its
/// error otherwise.
async fn answer(s: &mut Box<dyn Session>, call: &str) -> Option<String> {
    let mut out = QueryOutcome::default();
    match s.execute(&format!("RETURN {call}"), 1, &mut out).await {
        Err(e) => Some(e.to_string()),
        Ok(()) => out
            .error
            .or_else(|| out.errors.first().map(|e| format!("{e:?}"))),
    }
}

/// Whether Memgraph has a function `name`: one of the call shapes runs, or
/// fails for something other than an unknown name (wrong arguments). The
/// list predicates (`all`, `reduce`…) and aggregates are only known with
/// their own shapes.
async fn memgraph_knows(s: &mut Box<dyn Session>, name: &str) -> bool {
    for args in [
        "()",
        "(1)",
        "(x IN [1] WHERE true)",
        "(x IN [1] | x)",
        "(a = 0, x IN [1] | a)",
    ] {
        match answer(s, &format!("{name}{args}")).await {
            None => return true,
            Some(e) if !e.contains("doesn't exist") && !e.contains("parsing error") => return true,
            Some(_) => {}
        }
    }
    false
}

#[tokio::test]
#[ignore]
async fn neo4j_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_URL") else {
        return;
    };
    let mut s = open("neo4j", &url).await;
    let v = s.server_version().await.unwrap();
    let c = s.function_catalog().await.unwrap();
    eprintln!("{v}: {} functions", c.len());
    // From SHOW FUNCTIONS: signatures and the server's descriptions.
    let abs = find(&c, "", "abs").unwrap();
    assert_eq!(abs.args, "(input :: INTEGER | FLOAT)");
    assert!(!abs.description.is_empty());
    assert_eq!(find(&c, "", "count").unwrap().kind, FunctionKind::Aggregate);
    assert!(find(&c, "date", "truncate").is_some());
    // (The 4 in "Neo4j" is not the version.)
    let vectors = dbine_driver::functions::version_at_least(v.trim_start_matches("Neo4j"), "5.18");
    assert_eq!(
        find(&c, "vector.similarity", "cosine").is_some(),
        vectors,
        "{v}"
    );
    // Not Neo4j's: Memgraph's and Neptune's own.
    assert!(find(&c, "", "uniformSample").is_none());
    assert!(find(&c, "", "collToSet").is_none());

    // Inside an explicit transaction nothing is sent (a failure would end
    // it): the documented catalog, and the transaction stays open.
    let mut out = QueryOutcome::default();
    s.execute(":begin", 10, &mut out).await.unwrap();
    let c = s.function_catalog().await.unwrap();
    assert!(find(&c, "", "abs").is_some_and(|f| f.description.is_empty()));
    let mut out = QueryOutcome::default();
    s.execute("RETURN 1 AS one", 10, &mut out).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(":rollback", 10, &mut out).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn memgraph_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_MEMGRAPH_URL") else {
        return;
    };
    let mut s = open("memgraph", &url).await;
    let v = s.server_version().await.unwrap();
    let c = s.function_catalog().await.unwrap();
    let modules = c
        .iter()
        .filter(|f| !f.schema.is_empty() && f.schema != "point" && f.schema != "convert")
        .count();
    eprintln!("{v}: {} functions, {modules} from query modules", c.len());
    assert!(find(&c, "", "uniformSample").is_some());
    assert!(find(&c, "", "toUpper").is_some());
    assert_eq!(find(&c, "", "avg").unwrap().kind, FunctionKind::Aggregate);
    assert!(find(&c, "point", "distance").is_some());
    // A module function the documentation also lists comes once.
    assert_eq!(
        c.iter()
            .filter(|f| f.schema == "convert" && f.name == "str2object")
            .count(),
        1
    );
    // Neo4j's, not Memgraph's.
    assert!(find(&c, "vector.similarity", "cosine").is_none());
    assert!(find(&c, "", "char_length").is_none());
    // project(path) aggregates paths.
    assert!(c
        .iter()
        .any(|f| f.name == "project" && f.kind == FunctionKind::Aggregate));

    // Every name offered is the server's, and the ones its version doesn't
    // have yet aren't offered: nullIf is 3.14's.
    assert!(!memgraph_knows(&mut s, "dbineNoSuchFunction").await);
    let v314 = dbine_driver::functions::version_at_least(&v, "3.14");
    assert_eq!(find(&c, "", "nullIf").is_some(), v314, "{v}");
    assert_eq!(memgraph_knows(&mut s, "nullIf").await, v314, "{v}");
    let mut names: Vec<String> = c
        .iter()
        .map(|f| match f.schema.as_str() {
            "" => f.name.clone(),
            schema => format!("{schema}.{}", f.name),
        })
        .collect();
    names.sort();
    names.dedup();
    let mut unknown = Vec::new();
    for n in &names {
        if !memgraph_knows(&mut s, n).await {
            unknown.push(n.clone());
        }
    }
    assert!(unknown.is_empty(), "{v} doesn't have {unknown:?}");
}
