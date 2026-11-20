//! "Ver dependencias…" › what an object uses (`Session::references`)
//! against a real Databricks SQL warehouse with Unity Catalog (there's no
//! container for it), skipped without one. The user needs CREATE SCHEMA on
//! the catalog:
//!
//! ```sh
//! DBINE_TEST_DATABRICKS_HOST=dbc-….cloud.databricks.com \
//! DBINE_TEST_DATABRICKS_WAREHOUSE=abcdef1234567890 \
//! DBINE_TEST_DATABRICKS_TOKEN=dapi… DBINE_TEST_DATABRICKS_CATALOG=main \
//!   cargo test -p dbine-driver-databricks --test references -- --ignored --nocapture
//! ```
//!
//! A schema of its own: a table, a view on it, a SQL function that reads
//! it, a table with a foreign key to it, and a table filled from the first
//! one (`INSERT … SELECT`, which Unity Catalog's lineage records). Lineage
//! arrives a few minutes after the query: what it hasn't seen yet comes
//! from the definitions (Probable), so the checks take either. The schema
//! is dropped at the end, also when a check fails.

use dbine_driver::{
    Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget, ObjectRef,
    QueryOutcome, Relation, Session,
};

fn env(k: &str) -> Option<String> {
    std::env::var(format!("DBINE_TEST_DATABRICKS_{k}"))
        .ok()
        .filter(|v| !v.is_empty())
}

fn cfg() -> Option<ConnectionConfig> {
    let mut c = ConnectionConfig {
        driver: "databricks".into(),
        host: env("HOST")?,
        database: env("CATALOG")?,
        ..Default::default()
    };
    c.options.insert("warehouse".into(), env("WAREHOUSE")?);
    c.options.insert("token".into(), env("TOKEN")?);
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out)
        .await
        .map_err(|e| format!("{e}\n---\n{q}"))
}

fn check(f: &mut Vec<String>, ok: bool, what: String) {
    if !ok {
        f.push(what);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn references_live() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_DATABRICKS_* not set; skipping");
        return;
    };
    let catalog = cfg.database.clone();
    let schema = format!("dbine_refs_{}", std::process::id());
    let d = dbine_driver_databricks::drivers().remove(0);
    let mut s = d.connect(&cfg, Some(&catalog)).await.unwrap();
    let setup = format!(
        "CREATE SCHEMA {schema};
         CREATE TABLE {schema}.orders (id INT NOT NULL, total DECIMAL(10, 2), CONSTRAINT orders_pk PRIMARY KEY (id));
         CREATE TABLE {schema}.lines (id INT, order_id INT, CONSTRAINT lines_fk FOREIGN KEY (order_id) REFERENCES {schema}.orders (id));
         CREATE VIEW {schema}.v AS SELECT o.id, o.total FROM {catalog}.{schema}.orders o;
         CREATE FUNCTION {schema}.order_count() RETURNS INT RETURN (SELECT COUNT(*) FROM {catalog}.{schema}.orders);
         CREATE TABLE {schema}.copy AS SELECT * FROM {catalog}.{schema}.orders"
    );
    let mut f: Vec<String> = Vec::new();
    match run(&mut s, &setup).await {
        Err(e) => f.push(format!("setup: {e}")),
        Ok(()) => {
            if let Err(e) = checks(&mut s, &d, &schema, &mut f).await {
                f.push(e);
            }
        }
    }
    if let Err(e) = run(&mut s, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).await {
        f.push(format!("cleanup: {e}"));
    }
    assert!(f.is_empty(), "{}", f.join("\n"));
}

async fn refs(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    schema: &str,
    kind: &str,
    name: &str,
) -> Result<DependencyReport, String> {
    let t = DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
        },
        column: None,
    };
    let r = s
        .references(&t, scan)
        .await
        .map_err(|e| format!("{name}: {e}"))?;
    println!("{name}: {r:#?}");
    Ok(r)
}

async fn checks(
    s: &mut Box<dyn Session>,
    d: &std::sync::Arc<dyn dbine_driver::Driver>,
    schema: &str,
    f: &mut Vec<String>,
) -> Result<(), String> {
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let orders = |r: &DependencyReport, relation: Relation| {
        r.items
            .iter()
            .find(|x| {
                x.schema.as_deref() == Some(schema) && x.name == "orders" && x.relation == relation
            })
            .cloned()
    };

    // The view: from its definition (with its line), confirmed once
    // lineage has it.
    let r = refs(s, &scan, schema, "view", "v").await?;
    let x = orders(&r, Relation::Code);
    check(
        f,
        x.as_ref().is_some_and(|x| !x.mentions.is_empty()),
        format!("v: {x:?}"),
    );

    // The function's body.
    let r = refs(s, &scan, schema, "function", "order_count").await?;
    let x = orders(&r, Relation::Code);
    check(f, x.is_some(), format!("order_count: {x:?}"));

    // The foreign key (informational in Unity Catalog).
    let r = refs(s, &scan, schema, "table", "lines").await?;
    let x = orders(&r, Relation::ForeignKey);
    check(
        f,
        x.as_ref()
            .is_some_and(|x| x.confidence == Confidence::Confirmed),
        format!("lines: {x:?}"),
    );

    // The table filled from `orders`: lineage only (a table has no code),
    // once Unity Catalog has recorded it; the call works either way.
    let r = refs(s, &scan, schema, "table", "copy").await?;
    let x = orders(&r, Relation::Code);
    check(
        f,
        r.note.is_some() || x.is_none() || x.is_some_and(|x| x.confidence == Confidence::Confirmed),
        format!("copy: {r:?}"),
    );
    Ok(())
}
