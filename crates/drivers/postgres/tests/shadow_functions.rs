//! Catalog reads against a database where another user's functions and
//! operators sit on the `search_path` with the names of built-ins and a
//! closer match of their arguments: dependencies, references, the lineage,
//! the index report and the schema read never call one. The planted ones
//! only note that they ran and answer as the built-in would. Everything is
//! made in a database of its own, dropped at the end, even when a check
//! fails. Skipped without its URL:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//!   cargo test -p dbine-driver-postgres --test shadow_functions -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::LineageScope;
use dbine_driver::{
    ConnectionConfig, DependencyScan, DependencyTarget, Driver, ObjectRef, QueryOutcome, Session,
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

fn config(url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "postgres".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: db.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    }
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == "postgres")
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out
}

/// What ran of the planted objects since the last call.
async fn hits(s: &mut Box<dyn Session>) -> Vec<String> {
    let out = run(
        s,
        "WITH gone AS (DELETE FROM public.hits RETURNING what) SELECT DISTINCT what::text FROM gone",
    )
    .await;
    out.results
        .last()
        .map(|r| {
            r.rows
                .iter()
                .filter_map(|r| r[0].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

const OBJECTS: &str = "
CREATE TABLE public.parent (id int PRIMARY KEY);
CREATE TABLE public.child (id int PRIMARY KEY, parent_id int REFERENCES public.parent (id),
    n int CHECK (n > 0), UNIQUE (n, parent_id));
CREATE INDEX child_parent_idx ON public.child (parent_id) INCLUDE (n);
CREATE VIEW public.child_view AS SELECT c.id, c.parent_id, p.id AS pid FROM public.child c JOIN public.parent p ON p.id = c.parent_id;
CREATE FUNCTION public.reads_child() RETURNS bigint LANGUAGE plpgsql AS $$ BEGIN RETURN (SELECT count(*) FROM public.child); END $$;
CREATE TABLE public.hits (what text);
";

/// One planted object per way a catalog read could be taken over: the
/// built-in matches the same arguments only after a conversion.
const PLANTED: &str = "
CREATE FUNCTION public.generate_series(integer, smallint) RETURNS SETOF integer LANGUAGE plpgsql AS
  $$ BEGIN INSERT INTO public.hits VALUES ('generate_series'); RETURN QUERY SELECT * FROM pg_catalog.generate_series($1, $2::integer); END $$;
CREATE FUNCTION public.unnest(smallint[]) RETURNS SETOF smallint LANGUAGE plpgsql AS
  $$ BEGIN INSERT INTO public.hits VALUES ('unnest'); RETURN QUERY SELECT * FROM pg_catalog.unnest($1); END $$;
CREATE FUNCTION public.oid_ne(oid, integer) RETURNS boolean LANGUAGE plpgsql AS
  $$ BEGIN INSERT INTO public.hits VALUES ('oid <> integer'); RETURN $1 OPERATOR(pg_catalog.<>) $2::oid; END $$;
CREATE OPERATOR public.<> (LEFTARG = oid, RIGHTARG = integer, FUNCTION = public.oid_ne);
CREATE FUNCTION public.oid_is(oid, regclass) RETURNS boolean LANGUAGE plpgsql AS
  $$ BEGIN INSERT INTO public.hits VALUES ('oid = regclass'); RETURN $1 OPERATOR(pg_catalog.=) $2::oid; END $$;
CREATE OPERATOR public.= (LEFTARG = oid, RIGHTARG = regclass, FUNCTION = public.oid_is);
";

fn target(kind: &str, schema: Option<&str>, name: &str, column: Option<&str>) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: schema.map(Into::into),
            name: name.into(),
        },
        column: column.map(Into::into),
    }
}

async fn check(s: &mut Box<dyn Session>) {
    let d = driver();
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    // The planted objects do get picked by an unprotected query.
    run(
        s,
        "SELECT count(*) FROM pg_depend WHERE classid = 'pg_class'::regclass",
    )
    .await;
    assert_eq!(hits(s).await, ["oid = regclass"]);

    // With and without the schema: the latter is found by the path.
    for schema in [Some("public"), None] {
        for column in [None, Some("parent_id")] {
            let r = s
                .dependents(&target("table", schema, "child", column), &scan)
                .await
                .unwrap();
            assert!(r.note.is_none(), "{:?}", r.note);
            assert!(
                r.items.iter().any(|i| i.name == "child_view"),
                "{schema:?} {column:?}: {:?}",
                r.items
            );
            assert_eq!(hits(s).await, Vec::<String>::new(), "dependents {column:?}");
        }
        for (kind, name, used) in [
            ("table", "child", "parent"),
            ("view", "child_view", "child"),
        ] {
            let r = s
                .references(&target(kind, schema, name, None), &scan)
                .await
                .unwrap();
            assert!(
                r.items.iter().any(|i| i.name == used),
                "{schema:?} {name}: {:?}",
                r.items
            );
        }
        let ran = hits(s).await;
        // Reading a view's or a routine's source is not part of this: only
        // the dependency reads are.
        for planted in ["generate_series", "unnest", "oid <> integer"] {
            assert!(!ran.iter().any(|h| h == planted), "references: {ran:?}");
        }
        let report = s
            .index_usage(&ObjectRef {
                kind: "table".into(),
                schema: schema.map(Into::into),
                name: "child".into(),
            })
            .await
            .unwrap();
        let report = report.expect("an index report");
        assert_eq!(report.foreign_keys.len(), 1, "{:?}", report.foreign_keys);
        let ran = hits(s).await;
        for planted in ["generate_series", "unnest"] {
            assert!(!ran.iter().any(|h| h == planted), "index usage: {ran:?}");
        }
    }

    let graph = s
        .lineage_graph("", &LineageScope::default(), &scan)
        .await
        .unwrap();
    assert!(graph.edges.iter().any(|e| e.to.name == "child_view"));
    let ran = hits(s).await;
    for planted in ["generate_series", "unnest", "oid <> integer"] {
        assert!(!ran.iter().any(|h| h == planted), "lineage: {ran:?}");
    }

    let schema = s.database_schema().await.unwrap();
    let child = schema.iter().find(|t| t.name == "child").unwrap();
    assert_eq!(child.foreign_keys.len(), 1);
    let ran = hits(s).await;
    for planted in ["generate_series", "unnest"] {
        assert!(!ran.iter().any(|h| h == planted), "schema: {ran:?}");
    }

    // Outside a transaction the session keeps its path.
    let out = run(s, "SELECT current_setting('search_path')").await;
    assert_eq!(
        out.results[0].rows[0][0].as_str(),
        Some("\"$user\", public")
    );

    // Inside the user's transaction the path is put back.
    run(s, "BEGIN; SET LOCAL search_path TO public, pg_catalog").await;
    s.dependents(&target("table", Some("public"), "child", None), &scan)
        .await
        .unwrap();
    let out = run(s, "SELECT current_setting('search_path')").await;
    assert_eq!(
        out.results[0].rows[0][0].as_str(),
        Some("public, pg_catalog")
    );
    run(s, "ROLLBACK").await;
}

#[tokio::test]
#[ignore]
async fn catalog_reads_never_call_another_users_functions() {
    let Ok(url) = std::env::var("DBINE_TEST_POSTGRES_URL") else {
        return;
    };
    let d = driver();
    let mut admin = d.connect(&config(&url), None).await.unwrap();
    let db = format!("dbine_shadow_{}", std::process::id());
    run(&mut admin, &format!("CREATE DATABASE {db}")).await;
    let result = AssertUnwindSafe(async {
        let mut cfg = config(&url);
        cfg.database = db.clone();
        let mut s = d.connect(&cfg, None).await.unwrap();
        run(&mut s, OBJECTS).await;
        run(&mut s, PLANTED).await;
        run(&mut s, "DELETE FROM public.hits").await;
        check(&mut s).await;
        drop(s);
    })
    .catch_unwind()
    .await;
    run(
        &mut admin,
        &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
    )
    .await;
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
