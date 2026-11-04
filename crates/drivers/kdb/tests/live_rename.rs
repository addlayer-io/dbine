//! "Renombrar…" against a real kdb+ (`dbine-test-kdb`, see `live.rs`):
//! views, functions, variables and namespaces, each rename one statement
//! that checks on the server before it changes anything.
//!
//! ```sh
//! DBINE_TEST_KDB_URL=127.0.0.1:15000:dbine:dbinepass \
//!   cargo test -p dbine-driver-kdb --test live_rename -- --ignored --test-threads=1
//! ```
//!
//! They create objects named `dbine_rn_*` (and `.dbine_rn_*` namespaces,
//! a splayed table in `dbine_rn_s/`) and delete them at the end.

use dbine_driver::{
    kinds, ConnectionConfig, Driver, ObjectRef, QueryOutcome, RenameRequest, RenameTarget, Session,
};

/// `host:port[:user:password]` from `DBINE_TEST_KDB_URL`.
fn cfg(read_only: bool) -> ConnectionConfig {
    let url =
        std::env::var("DBINE_TEST_KDB_URL").expect("DBINE_TEST_KDB_URL=host:port[:user:password]");
    let mut parts = url.splitn(4, ':');
    let host = parts.next().unwrap_or("127.0.0.1").to_string();
    let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(5000);
    ConnectionConfig {
        host,
        port,
        username: parts.next().map(str::to_string),
        password: parts.next().map(str::to_string),
        read_only,
        options: [("tcps".to_string(), "false".to_string())].into(),
        ..Default::default()
    }
}

/// `q`'s outcome, or its error as the run shows it.
async fn try_run(s: &mut Box<dyn Session>, q: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(q, 1000, &mut out)
        .await
        .map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

async fn run(s: &mut Box<dyn Session>, q: &str) {
    try_run(s, q).await.unwrap_or_else(|e| panic!("{q}: {e}"));
}

/// Whether q expression `q` is `1b`.
async fn yes(s: &mut Box<dyn Session>, q: &str) -> bool {
    let out = try_run(s, q).await.unwrap_or_else(|e| panic!("{q}: {e}"));
    out.results[0].rows[0][0] == serde_json::json!(true)
}

fn object(kind: &str, schema: Option<&str>, name: &str) -> RenameTarget {
    RenameTarget::Object {
        object: ObjectRef {
            kind: kind.into(),
            schema: schema.map(String::from),
            name: name.into(),
        },
        parent: None,
    }
}

fn namespace(schema: &str) -> RenameTarget {
    RenameTarget::Schema {
        database: None,
        schema: schema.into(),
    }
}

/// The rename's script, run as the app runs it (each statement on its own).
async fn rename(
    d: &dyn Driver,
    s: &mut Box<dyn Session>,
    target: RenameTarget,
    new: &str,
) -> Result<(), String> {
    let script = d
        .rename_script(&RenameRequest {
            target,
            new_name: new.into(),
            table: None,
            definition: None,
        })
        .map_err(|e| e.to_string())?;
    assert_eq!(script.statements.len(), 1, "{:?}", script.statements);
    for st in &script.statements {
        try_run(s, st).await?;
    }
    Ok(())
}

const CLEANUP: &str = "{![`.;();0b;] (key `.) where (key `.) like \"dbine_rn_*\"; n:(key `) where (key `) like \"dbine_rn_*\"; if[count n; ![`;();0b;n]]; f:`$\":dbine_rn_s/\",/:string key `:dbine_rn_s; if[count f; hdel each f; hdel `:dbine_rn_s]}[]";

#[tokio::test]
#[ignore]
async fn renames_views_functions_variables_and_namespaces() {
    let d = dbine_driver_kdb::drivers().remove(0);
    let d = d.as_ref();
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    let mut ro = d.connect(&cfg(true), None).await.unwrap();
    run(&mut s, CLEANUP).await;

    // A view: defined again from its definition on the server, the old one gone.
    run(&mut s, "dbine_rn_a:1 2 3").await;
    run(&mut s, "dbine_rn_v::dbine_rn_a+1").await;
    rename(
        d,
        &mut s,
        object(kinds::VIEW, None, "dbine_rn_v"),
        "dbine_rn_w",
    )
    .await
    .unwrap();
    assert!(
        yes(
            &mut s,
            "(`dbine_rn_w in system \"b .\") and not `dbine_rn_v in key `."
        )
        .await
    );
    assert!(yes(&mut s, "(view `dbine_rn_w)~\"dbine_rn_a+1\"").await);
    run(&mut s, "dbine_rn_a:10 20").await;
    assert!(yes(&mut s, "dbine_rn_w~11 21").await);
    // A name taken: refused on the server, nothing changes.
    let e = rename(
        d,
        &mut s,
        object(kinds::VIEW, None, "dbine_rn_w"),
        "dbine_rn_a",
    )
    .await
    .unwrap_err();
    assert!(e.contains("Ya existe «dbine_rn_a»"), "{e}");
    assert!(
        yes(
            &mut s,
            "(`dbine_rn_w in system \"b .\") and dbine_rn_a~10 20"
        )
        .await
    );
    // A view outside the root: refused before anything is sent.
    let e = rename(
        d,
        &mut s,
        object(kinds::VIEW, None, "dbine_rn_w"),
        ".dbine_rn_x.w",
    )
    .await
    .unwrap_err();
    assert!(e.contains("solo en el espacio de nombres raíz"), "{e}");

    // A function bound to its namespace (defined there, as a script's
    // `\d` does) still resolves its globals there under the new name.
    run(
        &mut s,
        "{[n;c;t] system \"d \",c; p:parse t; system \"d .\"; n set p}[`.dbine_rn_f.f;\".dbine_rn_f\";\"{x+k}\"]",
    )
    .await;
    run(&mut s, ".dbine_rn_f.k:10").await;
    rename(
        d,
        &mut s,
        object(kinds::FUNCTION, Some(".dbine_rn_f"), "f"),
        "g",
    )
    .await
    .unwrap();
    assert!(
        yes(
            &mut s,
            "(11~.dbine_rn_f.g 1) and not `f in key `.dbine_rn_f"
        )
        .await
    );
    run(&mut s, ".dbine_rn_f.k:20").await;
    assert!(yes(&mut s, "21~.dbine_rn_f.g 1").await);
    // A root one, and never to another namespace.
    run(&mut s, "dbine_rn_fn:{x*2}").await;
    rename(
        d,
        &mut s,
        object(kinds::FUNCTION, None, "dbine_rn_fn"),
        "dbine_rn_fn2",
    )
    .await
    .unwrap();
    assert!(yes(&mut s, "(6~dbine_rn_fn2 3) and not `dbine_rn_fn in key `.").await);
    let e = rename(
        d,
        &mut s,
        object(kinds::FUNCTION, Some(".dbine_rn_f"), "g"),
        ".dbine_rn_x.g",
    )
    .await
    .unwrap_err();
    assert!(e.contains("otro espacio de nombres"), "{e}");

    // A variable; a name taken leaves both as they were.
    run(&mut s, "dbine_rn_x:`a`b!1 2").await;
    rename(
        d,
        &mut s,
        object(dbine_driver_kdb::VARIABLE, None, "dbine_rn_x"),
        "dbine_rn_y",
    )
    .await
    .unwrap();
    assert!(
        yes(
            &mut s,
            "((`a`b!1 2)~dbine_rn_y) and not `dbine_rn_x in key `."
        )
        .await
    );
    let e = rename(
        d,
        &mut s,
        object(dbine_driver_kdb::VARIABLE, None, "dbine_rn_y"),
        "dbine_rn_a",
    )
    .await
    .unwrap_err();
    assert!(e.contains("Ya existe «dbine_rn_a»"), "{e}");
    assert!(yes(&mut s, "((`a`b!1 2)~dbine_rn_y) and dbine_rn_a~10 20").await);
    // Gone since the explorer listed it: refused, nothing created.
    let e = rename(
        d,
        &mut s,
        object(dbine_driver_kdb::VARIABLE, None, "dbine_rn_gone"),
        "dbine_rn_z",
    )
    .await
    .unwrap_err();
    assert!(e.contains("no existe o cambió"), "{e}");
    assert!(yes(&mut s, "not `dbine_rn_z in key `.").await);

    // A namespace without functions: its objects under the new name, the
    // old one gone.
    run(
        &mut s,
        ".dbine_rn_ns.t:([] a:1 2 3); .dbine_rn_ns.k:([id:1 2] v:`a`b); .dbine_rn_ns.x:5; .dbine_rn_ns.sub.y:`p`q",
    )
    .await;
    rename(d, &mut s, namespace(".dbine_rn_ns"), "dbine_rn_ns2")
        .await
        .unwrap();
    assert!(yes(&mut s, "(3=count .dbine_rn_ns2.t) and (5~.dbine_rn_ns2.x) and (`p`q~.dbine_rn_ns2.sub.y) and (`b~first exec v from .dbine_rn_ns2.k where id=2) and not `dbine_rn_ns in key `").await);
    let schemas = s.list_schemas().await.unwrap().unwrap();
    assert!(schemas.iter().any(|n| n.name == ".dbine_rn_ns2"));
    assert!(!schemas.iter().any(|n| n.name == ".dbine_rn_ns"));
    // Its new name taken.
    run(&mut s, ".dbine_rn_t1.x:1").await;
    let e = rename(d, &mut s, namespace(".dbine_rn_t1"), ".dbine_rn_ns2")
        .await
        .unwrap_err();
    assert!(e.contains("Ya existe «.dbine_rn_ns2»"), "{e}");
    assert!(yes(&mut s, "(1~.dbine_rn_t1.x) and 3=count .dbine_rn_ns2.t").await);

    // With functions (one in a dictionary): refused, naming them, and
    // nothing changes.
    run(
        &mut s,
        ".dbine_rn_nf.f:{x}; .dbine_rn_nf.h:`a`b!({x};1); .dbine_rn_nf.y:1 2",
    )
    .await;
    let e = rename(d, &mut s, namespace(".dbine_rn_nf"), "dbine_rn_nf2")
        .await
        .unwrap_err();
    assert!(e.contains("sus funciones y enlaces"), "{e}");
    assert!(
        e.contains(".dbine_rn_nf.f, .dbine_rn_nf.h") && !e.contains(".dbine_rn_nf.y"),
        "{e}"
    );
    assert!(
        yes(
            &mut s,
            "(`f`h`y~(key `.dbine_rn_nf) except `) and not `dbine_rn_nf2 in key `"
        )
        .await
    );
    // Columns linked to its tables, inside it and from the root.
    run(
        &mut s,
        ".dbine_rn_nl.k:([id:1 2] v:`a`b); .dbine_rn_nl.t:([] r:`.dbine_rn_nl.k$1 2 1)",
    )
    .await;
    let e = rename(d, &mut s, namespace(".dbine_rn_nl"), "dbine_rn_nl2")
        .await
        .unwrap_err();
    assert!(e.contains("Son: .dbine_rn_nl.t"), "{e}");
    run(&mut s, "![`.dbine_rn_nl;();0b;enlist `t]").await;
    run(&mut s, "dbine_rn_ext:([] r:`.dbine_rn_nl.k$2 1)").await;
    let e = rename(d, &mut s, namespace(".dbine_rn_nl"), "dbine_rn_nl2")
        .await
        .unwrap_err();
    assert!(e.contains("Son: dbine_rn_ext"), "{e}");
    assert!(
        yes(
            &mut s,
            "(`dbine_rn_nl in key `) and not `dbine_rn_nl2 in key `"
        )
        .await
    );
    // A table on disk in it.
    run(&mut s, "`:dbine_rn_s/ set ([] n:1 2)").await;
    run(&mut s, ".dbine_rn_nd.s:get `:dbine_rn_s/; .dbine_rn_nd.x:1").await;
    let e = rename(d, &mut s, namespace(".dbine_rn_nd"), "dbine_rn_nd2")
        .await
        .unwrap_err();
    assert!(
        e.contains("tablas en disco") && e.contains("Son: .dbine_rn_nd.s"),
        "{e}"
    );
    assert!(yes(&mut s, "not `dbine_rn_nd2 in key `").await);
    // kdb+'s own and DBine's: refused before anything is sent.
    for (old, new) in [
        (".q", ".dbine_rn_q"),
        (".dbine_rn_ns2", ".z"),
        (".dbine_stage", ".dbine_rn_st"),
    ] {
        let e = rename(d, &mut s, namespace(old), new).await.unwrap_err();
        assert!(e.contains("propio de kdb+"), "{old} {new}: {e}");
    }

    // Read-only: the same statements are refused (the guard, then reval
    // on the server), and nothing changes.
    for (target, new) in [
        (object(kinds::VIEW, None, "dbine_rn_w"), "dbine_rn_v"),
        (object(kinds::FUNCTION, None, "dbine_rn_fn2"), "dbine_rn_fn"),
        (
            object(dbine_driver_kdb::VARIABLE, None, "dbine_rn_y"),
            "dbine_rn_x",
        ),
        (namespace(".dbine_rn_ns2"), "dbine_rn_ns"),
    ] {
        let e = rename(d, &mut ro, target, new).await.unwrap_err();
        assert!(e.contains("solo lectura"), "{e}");
    }
    assert!(
        yes(
            &mut s,
            "(`dbine_rn_w in system \"b .\") and (`dbine_rn_fn2`dbine_rn_y in key `.)~11b"
        )
        .await
    );
    assert!(
        yes(
            &mut s,
            "not any `dbine_rn_v`dbine_rn_fn`dbine_rn_x in key `."
        )
        .await
    );
    assert!(
        yes(
            &mut s,
            "(`dbine_rn_ns2 in key `) and not `dbine_rn_ns in key `"
        )
        .await
    );

    run(&mut s, CLEANUP).await;
    assert!(yes(&mut s, "not any ((key `.),key `) like \"dbine_rn_*\"").await);
}
