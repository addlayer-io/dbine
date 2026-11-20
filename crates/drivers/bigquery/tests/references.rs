//! "Ver dependencias…" › what an object uses (`Session::references`)
//! against bigquery-emulator, from the REST API only (no query job):
//! two datasets of its own, a table in each, a view that reads both
//! (and a CTE, which drops out), and a table with foreign keys to both.
//! The emulator keeps a view's query as written only when it comes through
//! `tables.insert`, checks that every table it names exists (so no other
//! project), and has no routines nor external tables: those are covered by
//! the unit tests. Both datasets are deleted at the
//! end, also when a check fails.
//!
//! ```sh
//! docker start dbine-test-bigquery   # -p 25302:9050 --project=test --dataset=ds1
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302 \
//!   cargo test -p dbine-driver-bigquery --test references -- --ignored --nocapture
//! ```

use dbine_driver::{
    Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget, ObjectRef,
    Relation, Session,
};
use serde_json::{json, Value};

async fn open(url: &str, dataset: &str) -> Result<Box<dyn Session>, String> {
    let mut c = ConnectionConfig {
        driver: "bigquery".into(),
        ..Default::default()
    };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url.into());
    dbine_driver_bigquery::drivers()
        .pop()
        .unwrap()
        .connect(&c, Some(dataset))
        .await
        .map_err(|e| format!("connect: {e}"))
}

/// A REST call to the emulator; an error answer is an error.
async fn rest(method: reqwest::Method, url: String, body: Option<Value>) -> Result<Value, String> {
    let mut req = reqwest::Client::new().request(method, &url);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.map_err(|e| format!("{url}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("{url}: {status} {text}"));
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn table(ds: &str, name: &str, extra: Value) -> Value {
    let mut t = json!({
        "tableReference": { "projectId": "test", "datasetId": ds, "tableId": name },
        "schema": { "fields": [
            { "name": "id", "type": "INTEGER" },
            { "name": "parent_id", "type": "INTEGER" },
            { "name": "src_id", "type": "INTEGER" }
        ] }
    });
    if let (Some(t), Some(extra)) = (t.as_object_mut(), extra.as_object()) {
        t.extend(extra.clone());
    }
    t
}

fn target(kind: &str, name: &str) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        },
        column: None,
    }
}

fn check(failures: &mut Vec<String>, ok: bool, what: String) {
    if !ok {
        failures.push(what);
    }
}

#[tokio::test]
#[ignore]
async fn references_live() {
    let Ok(url) = std::env::var("DBINE_TEST_BIGQUERY_URL") else {
        eprintln!("DBINE_TEST_BIGQUERY_URL not set: skipped");
        return;
    };
    let tag = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            % 1_000_000
    );
    let (a, b) = (format!("dbine_refs_{tag}"), format!("dbine_refs_x_{tag}"));
    let base = format!("{url}/bigquery/v2/projects/test");
    let mut f: Vec<String> = Vec::new();
    if let Err(e) = setup(&base, &a, &b).await {
        f.push(format!("setup: {e}"));
    } else if let Err(e) = checks(&url, &a, &b, &mut f).await {
        f.push(e);
    }
    for ds in [&a, &b] {
        if let Err(e) = rest(
            reqwest::Method::DELETE,
            format!("{base}/datasets/{ds}?deleteContents=true"),
            None,
        )
        .await
        {
            f.push(format!("cleanup: {e}"));
        }
    }
    assert!(f.is_empty(), "{}", f.join("\n"));
}

async fn setup(base: &str, a: &str, b: &str) -> Result<(), String> {
    use reqwest::Method;
    for ds in [a, b] {
        rest(
            Method::POST,
            format!("{base}/datasets"),
            Some(json!({ "datasetReference": { "projectId": "test", "datasetId": ds } })),
        )
        .await?;
    }
    rest(
        Method::POST,
        format!("{base}/datasets/{a}/tables"),
        Some(table(a, "src", json!({}))),
    )
    .await?;
    rest(
        Method::POST,
        format!("{base}/datasets/{b}/tables"),
        Some(table(b, "parent", json!({}))),
    )
    .await?;
    let fks = json!({ "tableConstraints": {
        "primaryKey": { "columns": ["id"] },
        "foreignKeys": [
            { "name": "fk_parent", "referencedTable": { "projectId": "test", "datasetId": b, "tableId": "parent" },
              "columnReferences": [{ "referencingColumn": "parent_id", "referencedColumn": "id" }] },
            { "name": "fk_src", "referencedTable": { "projectId": "test", "datasetId": a, "tableId": "src" },
              "columnReferences": [{ "referencingColumn": "src_id", "referencedColumn": "id" }] }
        ] } });
    rest(
        Method::POST,
        format!("{base}/datasets/{a}/tables"),
        Some(table(a, "child", fks)),
    )
    .await?;
    let view = json!({
        "tableReference": { "projectId": "test", "datasetId": a, "tableId": "v" },
        "view": { "useLegacySql": false, "query": format!(
            "WITH recent AS (SELECT id FROM `test`.{a}.src)\nSELECT s.id, p.id AS pid\nFROM `test.{a}.src` AS s\n\
             JOIN {b}.parent p ON p.id = s.id\nWHERE s.id IN (SELECT id FROM recent)"
        ) }
    });
    rest(
        Method::POST,
        format!("{base}/datasets/{a}/tables"),
        Some(view),
    )
    .await?;
    Ok(())
}

async fn refs(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    kind: &str,
    name: &str,
) -> Result<DependencyReport, String> {
    let r = s
        .references(&target(kind, name), scan)
        .await
        .map_err(|e| format!("{name}: {e}"))?;
    println!("{name}: {r:#?}");
    Ok(r)
}

async fn checks(url: &str, a: &str, b: &str, f: &mut Vec<String>) -> Result<(), String> {
    let mut s = open(url, a).await?;
    let d = dbine_driver_bigquery::drivers().pop().unwrap();
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);

    // The view: a table in its dataset (no schema, named twice), one in
    // the other dataset (its schema); the CTE drops out.
    let r = refs(&mut s, &scan, "view", "v").await?;
    let got: Vec<_> = r
        .items
        .iter()
        .map(|d| {
            (
                d.kind.as_str(),
                d.schema.as_deref(),
                d.name.as_str(),
                d.confidence,
            )
        })
        .collect();
    check(
        f,
        got == [
            ("table", None, "src", Confidence::Probable),
            ("table", Some(b), "parent", Confidence::Probable),
        ],
        format!("v: {got:?}"),
    );
    let lines = |i: usize| {
        r.items
            .get(i)
            .map(|d| d.mentions.iter().map(|m| m.line).collect::<Vec<_>>())
    };
    check(
        f,
        lines(0) == Some(vec![2, 4]) && lines(1) == Some(vec![5]),
        format!("v mentions: {:?}", r.items),
    );
    check(
        f,
        r.external.is_empty(),
        format!("v external: {:?}", r.external),
    );
    check(f, r.scanned == 1, format!("v scanned: {}", r.scanned));

    // The table's foreign keys: confirmed, each with its key.
    let r = refs(&mut s, &scan, "table", "child").await?;
    let got: Vec<_> = r
        .items
        .iter()
        .map(|d| {
            (
                d.schema.as_deref(),
                d.name.as_str(),
                d.relation,
                d.confidence,
            )
        })
        .collect();
    check(
        f,
        got == [
            (None, "src", Relation::ForeignKey, Confidence::Confirmed),
            (
                Some(b),
                "parent",
                Relation::ForeignKey,
                Confidence::Confirmed,
            ),
        ],
        format!("child: {got:?}"),
    );
    check(
        f,
        r.items[0]
            .detail
            .as_deref()
            .is_some_and(|x| x.starts_with("fk_src (src_id)")),
        format!("child detail: {:?}", r.items[0].detail),
    );

    // A table that uses nothing.
    let r = refs(&mut s, &scan, "table", "src").await?;
    check(
        f,
        r.items.is_empty() && r.external.is_empty(),
        format!("src: {r:?}"),
    );

    // A routine the emulator doesn't have: unreadable, not an error.
    let r = refs(&mut s, &scan, "function", "nope").await?;
    check(
        f,
        r.unreadable == [format!("{a}.nope")],
        format!("nope: {r:?}"),
    );
    Ok(())
}
