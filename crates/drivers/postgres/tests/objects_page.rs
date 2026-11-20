//! The explorer's pages (`list_objects_page`) against real servers: for
//! every filter combination, the pages, read with a small limit, must be
//! exactly the app's own pages of the whole list (`objects::page_of` over
//! `list_objects`). A scratch schema adds a folder of more than one page
//! (25 views in pages of 10), names that differ only in case, a table and a
//! function with the same name, one trigger name on two tables and a name
//! with LIKE wildcards; it is dropped at the end, also when a check fails.
//! Each test reads `DBINE_TEST_<ENGINE>_URL` and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_TIMESCALEDB_URL=postgres://postgres:pw@localhost:25015/postgres \
//! DBINE_TEST_YUGABYTE_URL=postgres://yugabyte@localhost:25016/yugabyte \
//! DBINE_TEST_CLOUDBERRY_URL=postgres://gpadmin:pw@localhost:25017/postgres \
//! DBINE_TEST_GREENGAGE_URL=postgres://gpadmin:pw@localhost:25018/postgres \
//! DBINE_TEST_GREENPLUM_URL=postgres://gpadmin:pw@localhost:25019/postgres \
//! DBINE_TEST_OPENGAUSS_URL='postgres://gaussdb:Dbine@1234@localhost:25020/postgres' \
//! DBINE_TEST_CRATEDB_URL=postgres://crate@localhost:25021/doc \
//! DBINE_TEST_RISINGWAVE_URL=postgres://root@localhost:25023/dev \
//! DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize \
//! DBINE_TEST_H2_URL=postgres://sa:sa@localhost:25025/test \
//!   cargo test -p dbine-driver-postgres --test objects_page -- --ignored --test-threads=1
//! ```
//!
//! (Greengage 6 runs as the `greenplum` variant.)

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use futures::FutureExt;
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

const SCHEMA: &str = "dbine_pages";

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 10, &mut out).await {
        Ok(_) if out.error.is_none() => Ok(()),
        Ok(_) => Err(format!("{:?}", out.error)),
        Err(e) => Err(e.to_string()),
    }
}

/// What the scratch schema holds: the 25 views must be made; the rest is
/// what each engine takes.
fn scratch(id: &str) -> Vec<String> {
    let s = SCHEMA;
    let mut v = vec![format!("CREATE SCHEMA {s}")];
    v.extend((0..25).map(|i| format!("CREATE VIEW {s}.v{i:02} AS SELECT 1 AS a")));
    v.extend([
        format!("CREATE TABLE {s}.\"Orders\" (a int)"),
        format!("CREATE VIEW {s}.orders AS SELECT 1 AS a"),
        format!("CREATE TABLE {s}.\"pct%_x\" (a int)"),
        format!("CREATE TABLE {s}.pctax (a int)"),
        format!("CREATE TABLE {s}.t_same (a int)"),
        format!("CREATE FUNCTION {s}.t_same() RETURNS int LANGUAGE sql AS 'SELECT 1'"),
        format!("CREATE FUNCTION {s}.t_same(x int) RETURNS int LANGUAGE sql AS 'SELECT 2'"),
        format!("CREATE SEQUENCE {s}.s_one"),
        format!("CREATE TYPE {s}.mood AS ENUM ('a', 'b')"),
        format!("CREATE MATERIALIZED VIEW {s}.mv_one AS SELECT 1 AS a"),
        format!(
            "CREATE FUNCTION {s}.trg_fn() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$"
        ),
        format!(
            "CREATE TRIGGER trg_same BEFORE INSERT ON {s}.t_same FOR EACH ROW EXECUTE PROCEDURE {s}.trg_fn()"
        ),
        format!(
            "CREATE TRIGGER trg_same BEFORE INSERT ON {s}.\"Orders\" FOR EACH ROW EXECUTE PROCEDURE {s}.trg_fn()"
        ),
        format!("CREATE SYNONYM {s}.syn_one FOR {s}.t_same"),
    ]);
    if id == "h2" {
        v.push(format!("CREATE DOMAIN {s}.d_one AS INT"));
    }
    v
}

/// Drops the scratch schema (CrateDB has no `DROP SCHEMA`: its objects).
async fn drop_scratch(s: &mut Box<dyn Session>) {
    if run(s, &format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"))
        .await
        .is_ok()
    {
        return;
    }
    let Ok(all) = s.list_objects().await else {
        return;
    };
    for o in all.iter().filter(|o| {
        o.schema
            .as_deref()
            .is_some_and(|x| x.eq_ignore_ascii_case(SCHEMA))
    }) {
        let what = if o.kind == "view" { "VIEW" } else { "TABLE" };
        let _ = run(
            s,
            &format!(
                "DROP {what} IF EXISTS {SCHEMA}.\"{}\"",
                o.name.replace('"', "\"\"")
            ),
        )
        .await;
    }
}

/// Every page of `q`, by `list_objects_page`.
async fn driver_pages(s: &mut Box<dyn Session>, q: &ObjectQuery) -> Vec<Vec<DbObject>> {
    let mut q = q.clone();
    let mut out = Vec::new();
    loop {
        let p = s
            .list_objects_page(&q)
            .await
            .unwrap_or_else(|e| panic!("{q:?}: {e}"));
        assert!(p.objects.len() <= q.page_size(), "{q:?}: page too big");
        out.push(p.objects);
        assert!(out.len() < 5000, "{q:?}: pages without end");
        match p.next {
            Some(n) => q.after = Some(n),
            None => return out,
        }
    }
}

/// Every page of `q`, as the app makes them out of the whole list.
fn app_pages(all: &[DbObject], q: &ObjectQuery) -> Vec<Vec<DbObject>> {
    let mut q = q.clone();
    let mut out = Vec::new();
    loop {
        let p = page_of(all, &q);
        out.push(p.objects);
        match p.next {
            Some(n) => q.after = Some(n),
            None => return out,
        }
    }
}

/// The pages of `q` with a small limit (7, or enough for ~200 pages on a
/// big catalog) are the app's; their objects.
async fn same(s: &mut Box<dyn Session>, all: &[DbObject], q: ObjectQuery) -> Vec<DbObject> {
    let total: usize = app_pages(all, &q).iter().map(Vec::len).sum();
    let q = ObjectQuery {
        limit: if q.limit > 0 {
            q.limit
        } else {
            (total as u32 / 200 + 1).max(7)
        },
        ..q
    };
    let want = app_pages(all, &q);
    let got = driver_pages(s, &q).await;
    assert_eq!(got, want, "{q:?}");
    want.concat()
}

fn query(f: impl FnOnce(&mut ObjectQuery)) -> ObjectQuery {
    let mut q = ObjectQuery::default();
    f(&mut q);
    q
}

async fn exercise(s: &mut Box<dyn Session>, d: &dyn Driver, id: &str) {
    let mut made = Vec::new();
    for (i, sql) in scratch(id).iter().enumerate() {
        match run(s, sql).await {
            Ok(()) => made.push(sql.clone()),
            // The schema (CrateDB makes it with its first object) and the
            // 25 views must be there; the rest, where the engine has it.
            Err(e) if (1..=25).contains(&i) => panic!("{id}: {sql}: {e}"),
            Err(_) => {}
        }
    }
    eprintln!("{id}: made {} scratch objects", made.len());
    let all = s.list_objects().await.unwrap();
    let schema = all
        .iter()
        .filter_map(|o| o.schema.clone())
        .find(|x| x.eq_ignore_ascii_case(SCHEMA))
        .unwrap_or_else(|| panic!("{id}: {SCHEMA} isn't listed"));

    // A folder of more than one page.
    let folder = query(|q| {
        q.schema = Some(schema.clone());
        q.kind = Some("view".into());
        q.limit = 10;
    });
    let views = same(s, &all, folder.clone()).await;
    let pages = driver_pages(s, &folder).await;
    assert!(pages.len() >= 3, "{id}: {} pages of views", pages.len());
    assert!(views.iter().any(|o| o.name == "v00") && views.iter().any(|o| o.name == "v24"));

    let declared: Vec<String> = d
        .info()
        .object_kinds
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    let kinds: BTreeSet<String> = declared
        .iter()
        .cloned()
        .chain(all.iter().map(|o| o.kind.clone()))
        .collect();
    let mut queries = Vec::new();
    for k in &kinds {
        queries.push(query(|q| q.kind = Some(k.clone())));
        queries.push(query(|q| {
            q.kind = Some(k.clone());
            q.schema = Some(schema.clone());
        }));
    }
    queries.extend([
        query(|_| {}),
        query(|q| q.schema = Some(schema.clone())),
        query(|q| q.schema = Some("no_such_schema".into())),
        query(|q| q.no_schema = true),
        query(|q| q.not_kinds = declared.clone()),
        query(|q| {
            q.not_kinds = vec!["view".into(), "table".into()];
            q.schema = Some(schema.clone());
        }),
        query(|q| q.pattern = Some("ORDER".into())),
        query(|q| q.pattern = Some("%_".into())),
        query(|q| q.pattern = Some("_".into())),
        query(|q| q.pattern = Some("o'b\\!".into())),
        query(|q| {
            q.pattern = Some("V1".into());
            q.kind = Some("view".into());
            q.schema = Some(schema.clone());
        }),
        query(|q| q.name = Some("ORDERS".into())),
        query(|q| q.name = Some("t_same".into())),
        query(|q| q.name = Some("TRG_SAME".into())),
        query(|q| {
            q.name = Some("t_same".into());
            q.kind = Some("function".into());
        }),
        query(|q| q.name = Some("nothing_called_this".into())),
    ]);
    let mut checked = 0;
    for q in queries {
        checked += same(s, &all, q).await.len();
    }
    // The scratch schema's tricky names came out as the app sees them.
    let mine = same(s, &all, query(|q| q.schema = Some(schema.clone()))).await;
    let names: BTreeSet<&str> = mine.iter().map(|o| o.name.as_str()).collect();
    assert!(
        names.contains("Orders") && names.contains("orders"),
        "{id}: {names:?}"
    );
    let pct = same(s, &all, query(|q| q.pattern = Some("%_".into()))).await;
    assert!(
        pct.iter().all(|o| o.name.contains("%_")),
        "{id}: a wildcard matched {pct:?}"
    );
    eprintln!(
        "{id}: {} objects in the database, {} kinds, {checked} objects checked page by page",
        all.len(),
        kinds.len()
    );
}

async fn check(id: &str, env: &str) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    let mut s = d.connect(&cfg, None).await.unwrap();
    drop_scratch(&mut s).await;
    let r = AssertUnwindSafe(exercise(&mut s, &*d, id))
        .catch_unwind()
        .await;
    drop_scratch(&mut s).await;
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

#[tokio::test]
#[ignore]
async fn postgres() {
    check("postgres", "DBINE_TEST_POSTGRES_URL").await;
}

/// A database with 20,000 views: a folder's first page, a search and the
/// other schema's table come without reading the whole list, and still
/// match it. Prints the times.
#[tokio::test]
#[ignore]
async fn postgres_many_views() {
    let Some(cfg) = cfg("postgres", "DBINE_TEST_POSTGRES_URL") else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    // A thousand at a time: one transaction can't lock them all.
    let batches = |verb: &'static str| {
        (0..20).map(move |b| {
            format!(
                "DO $$BEGIN FOR i IN {}..{} LOOP EXECUTE format('{verb}', i); END LOOP; END$$",
                b * 1000 + 1,
                b * 1000 + 1000
            )
        })
    };
    let create = batches("CREATE VIEW dbine_pages.v%s AS SELECT 1 AS a");
    let drop: Vec<String> = batches("DROP VIEW IF EXISTS dbine_pages.v%s").collect();
    let mut s = driver("postgres").connect(&cfg, None).await.unwrap();
    for sql in &drop {
        let _ = run(&mut s, sql).await;
    }
    drop_scratch(&mut s).await;
    let r = AssertUnwindSafe(async {
        run(&mut s, &format!("CREATE SCHEMA {SCHEMA}"))
            .await
            .unwrap();
        run(&mut s, &format!("CREATE TABLE {SCHEMA}.the_table (a int)"))
            .await
            .unwrap();
        for sql in create {
            run(&mut s, &sql).await.unwrap();
        }
        let t = std::time::Instant::now();
        let all = s.list_objects().await.unwrap();
        let whole = t.elapsed();
        for q in [
            query(|q| {
                q.schema = Some(SCHEMA.into());
                q.kind = Some("view".into());
            }),
            query(|q| {
                q.schema = Some(SCHEMA.into());
                q.kind = Some("table".into());
            }),
            query(|q| q.pattern = Some("v1999".into())),
        ] {
            let t = std::time::Instant::now();
            let p = s.list_objects_page(&q).await.unwrap();
            let one = t.elapsed();
            let want = page_of(&all, &q);
            assert_eq!(p.objects, want.objects, "{q:?}");
            assert_eq!(p.next.is_some(), want.next.is_some(), "{q:?}");
            eprintln!(
                "{} objects: the whole list in {whole:?}; {} of them ({q:?}) in {one:?}",
                all.len(),
                p.objects.len()
            );
        }
    })
    .catch_unwind()
    .await;
    for sql in &drop {
        let _ = run(&mut s, sql).await;
    }
    drop_scratch(&mut s).await;
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

/// The simple query protocol (a gateway that refuses the extended one):
/// the values go inlined as literals, with the same pages.
#[tokio::test]
#[ignore]
async fn postgres_simple_protocol() {
    let Some(mut cfg) = cfg("postgres", "DBINE_TEST_POSTGRES_URL") else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    cfg.options.insert("query_protocol".into(), "simple".into());
    let d = driver("postgres");
    let mut s = d.connect(&cfg, None).await.unwrap();
    drop_scratch(&mut s).await;
    let r = AssertUnwindSafe(exercise(&mut s, &*d, "postgres"))
        .catch_unwind()
        .await;
    drop_scratch(&mut s).await;
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

#[tokio::test]
#[ignore]
async fn cockroachdb() {
    check("cockroachdb", "DBINE_TEST_COCKROACH_URL").await;
}

#[tokio::test]
#[ignore]
async fn timescaledb() {
    check("timescaledb", "DBINE_TEST_TIMESCALEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn yugabytedb() {
    check("yugabytedb", "DBINE_TEST_YUGABYTE_URL").await;
}

#[tokio::test]
#[ignore]
async fn cloudberry() {
    check("cloudberry", "DBINE_TEST_CLOUDBERRY_URL").await;
}

#[tokio::test]
#[ignore]
async fn greengage() {
    check("greengage", "DBINE_TEST_GREENGAGE_URL").await;
}

#[tokio::test]
#[ignore]
async fn greenplum() {
    check("greenplum", "DBINE_TEST_GREENPLUM_URL").await;
}

#[tokio::test]
#[ignore]
async fn opengauss() {
    check("opengauss", "DBINE_TEST_OPENGAUSS_URL").await;
}

#[tokio::test]
#[ignore]
async fn cratedb() {
    check("cratedb", "DBINE_TEST_CRATEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn risingwave() {
    check("risingwave", "DBINE_TEST_RISINGWAVE_URL").await;
}

#[tokio::test]
#[ignore]
async fn materialize() {
    check("materialize", "DBINE_TEST_MATERIALIZE_URL").await;
}

#[tokio::test]
#[ignore]
async fn h2() {
    check("h2", "DBINE_TEST_H2_URL").await;
}

#[tokio::test]
#[ignore]
async fn edb() {
    check("edb", "DBINE_TEST_EDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn kingbase() {
    check("kingbase", "DBINE_TEST_KINGBASE_URL").await;
}

#[tokio::test]
#[ignore]
async fn redshift() {
    check("redshift", "DBINE_TEST_REDSHIFT_URL").await;
}
