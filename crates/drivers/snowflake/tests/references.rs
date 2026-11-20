//! "Ver dependencias…" › what an object uses (`Session::references`)
//! against a real Snowflake account (no emulator; the same variables as
//! tests/jobs.rs, a role that can create databases, and a warehouse):
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=… DBINE_TEST_SNOWFLAKE_USER=… DBINE_TEST_SNOWFLAKE_TOKEN=… \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH \
//!   cargo test -p dbine-driver-snowflake --test references -- --ignored --nocapture
//! ```
//!
//! Two databases of its own (`DBINE_TEST_REFS_DB`, `DBINE_TEST_REFS_X`): a
//! table, a view on it and on the other database's table (a source
//! outside), a SQL function and a procedure that read both, and a table
//! with a foreign key. `OBJECT_DEPENDENCIES` lags up to three hours: a new
//! object's items are Probable (from its definition) and the report says
//! so; without access to ACCOUNT_USAGE the note says that instead. Both
//! databases are dropped at the end, also when a check fails.

use dbine_driver::{
    Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget, ObjectRef,
    QueryOutcome, Relation, Session,
};

const DB: &str = "DBINE_TEST_REFS_DB";
const X: &str = "DBINE_TEST_REFS_X";

fn config() -> Option<ConnectionConfig> {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok();
    let mut options = std::collections::HashMap::from([
        ("account".to_string(), var("ACCOUNT")?),
        ("token".to_string(), var("TOKEN")?),
    ]);
    options.insert("warehouse".into(), var("WAREHOUSE")?);
    Some(ConnectionConfig {
        driver: "snowflake".into(),
        username: var("USER"),
        options: options.into_iter().collect(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out)
        .await
        .map_err(|e| format!("{e}\n---\n{q}"))
}

async fn refs(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    kind: &str,
    name: &str,
) -> Result<DependencyReport, String> {
    let t = DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: Some("PUBLIC".into()),
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

fn check(f: &mut Vec<String>, ok: bool, what: String) {
    if !ok {
        f.push(what);
    }
}

#[tokio::test]
#[ignore]
async fn references_live() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_SNOWFLAKE_* not set: skipped");
        return;
    };
    let d = dbine_driver_snowflake::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let setup = format!(
        "CREATE OR REPLACE DATABASE {X};
         CREATE TABLE {X}.PUBLIC.LEADS (ID INT);
         CREATE OR REPLACE DATABASE {DB};
         CREATE TABLE {DB}.PUBLIC.ORDERS (ID INT PRIMARY KEY, TOTAL NUMBER(10, 2));
         CREATE TABLE {DB}.PUBLIC.LINES (ID INT, ORDER_ID INT REFERENCES {DB}.PUBLIC.ORDERS (ID));
         CREATE VIEW {DB}.PUBLIC.V AS
           SELECT o.ID, l.ID AS LEAD_ID
           FROM {DB}.PUBLIC.ORDERS o
           JOIN {X}.PUBLIC.LEADS l ON l.ID = o.ID;
         CREATE FUNCTION {DB}.PUBLIC.ORDER_COUNT() RETURNS NUMBER AS
           'SELECT COUNT(*) FROM {DB}.PUBLIC.ORDERS';
         CREATE PROCEDURE {DB}.PUBLIC.LOAD() RETURNS VARCHAR LANGUAGE SQL AS
           $$ BEGIN INSERT INTO {DB}.PUBLIC.LINES SELECT ID, ID FROM {DB}.PUBLIC.V; RETURN 'ok'; END $$;"
    );
    let mut f: Vec<String> = Vec::new();
    match run(&mut admin, &setup).await {
        Err(e) => f.push(format!("setup: {e}")),
        Ok(()) => {
            if let Err(e) = checks(&*d, &cfg, &mut f).await {
                f.push(e);
            }
        }
    }
    for db in [DB, X] {
        if let Err(e) = run(&mut admin, &format!("DROP DATABASE IF EXISTS {db}")).await {
            f.push(format!("cleanup: {e}"));
        }
    }
    assert!(f.is_empty(), "{}", f.join("\n"));
}

async fn checks(
    d: &dyn dbine_driver::Driver,
    cfg: &ConnectionConfig,
    f: &mut Vec<String>,
) -> Result<(), String> {
    let mut s = d
        .connect(cfg, Some(DB))
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let item = |r: &DependencyReport, name: &str| {
        r.items
            .iter()
            .find(|x| x.schema.as_deref() == Some("PUBLIC") && x.name == name)
            .cloned()
    };
    // Confirmed once OBJECT_DEPENDENCIES has it; Probable (from the
    // definition) before, with a note.
    let known = |r: &DependencyReport, x: &Option<dbine_driver::Dependent>| {
        x.as_ref().is_some_and(|x| {
            x.confidence == Confidence::Confirmed
                || (x.confidence == Confidence::Probable
                    && r.note.is_some()
                    && !x.mentions.is_empty())
        })
    };

    let r = refs(&mut s, &scan, "view", "V").await?;
    let orders = item(&r, "ORDERS");
    check(f, known(&r, &orders), format!("V: ORDERS {orders:?}"));
    let leads = r.external.iter().find(|x| {
        x.database
            .as_deref()
            .is_some_and(|d| d.eq_ignore_ascii_case(X))
            && x.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case("LEADS"))
    });
    check(
        f,
        leads.is_some(),
        format!("V: {X}.PUBLIC.LEADS outside {:?}", r.external),
    );
    check(f, r.items.len() == 1, format!("V: {:?}", r.items));

    let r = refs(&mut s, &scan, "function", "ORDER_COUNT").await?;
    let orders = item(&r, "ORDERS");
    check(
        f,
        known(&r, &orders),
        format!("ORDER_COUNT: ORDERS {orders:?}"),
    );

    // Procedures aren't in OBJECT_DEPENDENCIES: their body.
    let r = refs(&mut s, &scan, "procedure", "LOAD").await?;
    for name in ["LINES", "V"] {
        let x = item(&r, name);
        check(
            f,
            x.as_ref().is_some_and(|x| !x.mentions.is_empty()),
            format!("LOAD: {name} {x:?}"),
        );
    }

    // The table's foreign key.
    let r = refs(&mut s, &scan, "table", "LINES").await?;
    let orders = item(&r, "ORDERS");
    check(
        f,
        orders.as_ref().is_some_and(|x| {
            x.relation == Relation::ForeignKey && x.confidence == Confidence::Confirmed
        }),
        format!("LINES: ORDERS {orders:?}"),
    );
    Ok(())
}
