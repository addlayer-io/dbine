//! "Renombrar…" against a real server (see `integration.rs`):
//! `DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 cargo test -p dbine-driver-orientdb --test rename -- --ignored`.
//!
//! Each script runs as the app runs it: statement by statement, the
//! request filled the way the app fills it (`table` from
//! `database_schema` for a property, `definition` for a vertex or edge
//! class, nothing for a document class).

use dbine_driver::{kinds, ConnectionConfig, Driver, ObjectRef, QueryOutcome, RenameRequest, RenameTarget, Session, TableSchema};
use dbine_driver_orientdb::{EDGE, VERTEX};
use serde_json::{json, Value};

fn cfg(url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    }
}

async fn try_run(s: &mut Box<dyn Session>, text: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.map_err(|e| e.to_string())?;
    match out.error.clone().or_else(|| out.errors.first().map(|e| e.message.clone())) {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    try_run(s, text).await.unwrap_or_else(|e| panic!("{text}: {e}"))
}

/// The rows of the last result, as JSON objects.
async fn rows(s: &mut Box<dyn Session>, text: &str) -> Vec<serde_json::Map<String, Value>> {
    let out = run(s, text).await;
    let r = out.results.last().expect("a result");
    r.rows.iter().map(|row| r.columns.iter().map(|c| c.name.clone()).zip(row.iter().cloned()).collect()).collect()
}

async fn one(s: &mut Box<dyn Session>, text: &str, col: &str) -> Value {
    rows(s, text).await.first().and_then(|r| r.get(col).cloned()).unwrap_or(Value::Null)
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

async fn table(s: &mut Box<dyn Session>, name: &str) -> TableSchema {
    s.database_schema().await.unwrap().into_iter().find(|t| t.name == name).unwrap()
}

/// Runs the script statement by statement, stopping at the first error.
async fn apply(d: &dyn Driver, s: &mut Box<dyn Session>, req: &RenameRequest) -> Result<(), String> {
    let script = d.rename_script(req).map_err(|e| e.to_string())?;
    for st in &script.statements {
        try_run(s, st).await.map_err(|e| format!("{st}: {e}"))?;
    }
    Ok(())
}

/// Index name → its fields, for one class.
async fn indexes_of(s: &mut Box<dyn Session>, class: &str) -> Vec<(String, Vec<String>)> {
    let t = table(s, class).await;
    t.indexes.into_iter().map(|ix| (ix.name, ix.columns)).collect()
}

#[tokio::test]
#[ignore]
async fn rename() {
    let url = std::env::var("DBINE_TEST_ORIENTDB_URL").expect("DBINE_TEST_ORIENTDB_URL");
    let d = dbine_driver_orientdb::drivers().remove(0);
    let c = cfg(&url);
    let mut admin = match d.connect(&c, Some("dbine_admin")).await {
        Ok(s) => s,
        Err(_) => {
            let mut tmp = d.connect(&c, None).await.expect("connect");
            let _ = tmp.create_database("dbine_admin").await;
            d.connect(&c, Some("dbine_admin")).await.unwrap()
        }
    };
    let _ = admin.drop_database("dbine_rename").await;
    admin.create_database("dbine_rename").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_rename")).await.unwrap();

    // Fixture: vertex class T (id unique, pepe indexed), edge class K
    // between T vertices, document class T2 linking to T, a document class
    // with a same-named field, and functions that use T.pepe.
    run(
        &mut s,
        "CREATE CLASS T EXTENDS V;
         CREATE PROPERTY T.id INTEGER;
         CREATE PROPERTY T.pepe STRING;
         CREATE INDEX T.id ON T (id) UNIQUE;
         CREATE INDEX T.pepe ON T (pepe) NOTUNIQUE;
         CREATE CLASS K EXTENDS E;
         CREATE CLASS T2;
         CREATE PROPERTY T2.t LINK T;
         CREATE CLASS Otra;
         CREATE PROPERTY Otra.pepe STRING;
         CREATE VERTEX T SET id = 1, pepe = 'a', apodo = 'uno';
         CREATE VERTEX T SET id = 2, pepe = 'b';
         CREATE EDGE K FROM (SELECT FROM T WHERE id = 1) TO (SELECT FROM T WHERE id = 2);
         INSERT INTO T2 SET t = (SELECT FROM T WHERE id = 1);
         INSERT INTO Otra SET pepe = 'otra';
         CREATE FUNCTION porPepe \"SELECT FROM T WHERE pepe = :p\" PARAMETERS [p] LANGUAGE SQL;
         CREATE FUNCTION dinamica \"return db.query('SELECT FROM T WHERE ' + campo + ' = ?', v);\" PARAMETERS [campo, v] LANGUAGE javascript",
    )
    .await;
    let spec = d.rename_spec().unwrap();

    // 1. Property T.pepe → juan: the value moves, the index follows.
    let req = RenameRequest {
        target: RenameTarget::Column { table: obj(VERTEX, "T"), column: "pepe".into() },
        new_name: "juan".into(),
        table: Some(table(&mut s, "T").await),
        definition: None,
    };
    assert!(spec.allows(&req.target));
    apply(d.as_ref(), &mut s, &req).await.unwrap();
    let all = rows(&mut s, "SELECT pepe, juan FROM T ORDER BY id").await;
    assert_eq!(all.iter().map(|r| (r["pepe"].clone(), r["juan"].clone())).collect::<Vec<_>>(), vec![(Value::Null, json!("a")), (Value::Null, json!("b"))]);
    assert_eq!(one(&mut s, "SELECT count(*) AS n FROM T WHERE pepe IS DEFINED", "n").await, json!(0));
    assert!(indexes_of(&mut s, "T").await.contains(&("T.pepe".to_string(), vec!["juan".to_string()])));
    assert_eq!(one(&mut s, "SELECT count(*) AS n FROM index:T.pepe WHERE key = 'b'", "n").await, json!(1));
    run(&mut s, "CREATE VERTEX T SET id = 3, juan = 'c'").await;
    assert_eq!(one(&mut s, "SELECT count(*) AS n FROM index:T.pepe WHERE key = 'c'", "n").await, json!(1));
    // The other class's same-named field is untouched.
    assert_eq!(one(&mut s, "SELECT pepe FROM Otra", "pepe").await, json!("otra"));
    // A field only in the data (not declared).
    let req = RenameRequest {
        target: RenameTarget::Column { table: obj(VERTEX, "T"), column: "apodo".into() },
        new_name: "alias".into(),
        table: Some(table(&mut s, "T").await),
        definition: None,
    };
    apply(d.as_ref(), &mut s, &req).await.unwrap();
    assert_eq!(one(&mut s, "SELECT alias, apodo FROM T WHERE id = 1", "alias").await, json!("uno"));
    assert_eq!(one(&mut s, "SELECT count(*) AS n FROM T WHERE apodo IS DEFINED", "n").await, json!(0));

    // 2. Edge class K → Conoce: vertices' out_K / in_K follow.
    let k = obj(EDGE, "K");
    let req = RenameRequest {
        target: RenameTarget::Object { object: k.clone(), parent: None },
        new_name: "Conoce".into(),
        table: None,
        definition: s.definition(&k).await.unwrap(),
    };
    apply(d.as_ref(), &mut s, &req).await.unwrap();
    assert_eq!(one(&mut s, "SELECT out('Conoce').id AS ids FROM T WHERE id = 1", "ids").await, json!("[2]"));
    assert_eq!(one(&mut s, "SELECT in('Conoce').id AS ids FROM T WHERE id = 2", "ids").await, json!("[1]"));
    assert_eq!(one(&mut s, "SELECT count(*) AS n FROM T WHERE out_K IS DEFINED OR in_K IS DEFINED", "n").await, json!(0));
    run(&mut s, "CREATE EDGE Conoce FROM (SELECT FROM T WHERE id = 2) TO (SELECT FROM T WHERE id = 3)").await;
    assert_eq!(one(&mut s, "SELECT out('Conoce').id AS ids FROM T WHERE id = 2", "ids").await, json!("[3]"));

    // 3. Vertex class T → Persona: indexes recreated on it, edges and links still there.
    let t = obj(VERTEX, "T");
    let req = RenameRequest {
        target: RenameTarget::Object { object: t.clone(), parent: None },
        new_name: "Persona".into(),
        table: None,
        definition: s.definition(&t).await.unwrap(),
    };
    apply(d.as_ref(), &mut s, &req).await.unwrap();
    let ixs = indexes_of(&mut s, "Persona").await;
    assert!(ixs.contains(&("T.id".to_string(), vec!["id".to_string()])), "{ixs:?}");
    assert!(ixs.contains(&("T.pepe".to_string(), vec!["juan".to_string()])), "{ixs:?}");
    assert!(try_run(&mut s, "CREATE VERTEX Persona SET id = 1").await.is_err(), "the unique index must hold");
    assert_eq!(one(&mut s, "SELECT out('Conoce').juan AS j FROM Persona WHERE id = 1", "j").await, json!("[\"b\"]"));
    assert_eq!(one(&mut s, "SELECT t.juan AS j FROM T2", "j").await, json!("a"));
    // The functions still name T and pepe: listed for the user, never rewritten.
    assert_eq!(one(&mut s, "SELECT code FROM OFunction WHERE name = 'porPepe'", "code").await, json!("SELECT FROM T WHERE pepe = :p"));

    // 4. Document class: renamed; with an index and nothing to read it
    // from, the engine refuses and nothing changes.
    let req = RenameRequest { target: RenameTarget::Object { object: obj(kinds::TABLE, "T2"), parent: None }, new_name: "Enlaces".into(), table: None, definition: None };
    apply(d.as_ref(), &mut s, &req).await.unwrap();
    assert_eq!(one(&mut s, "SELECT t.id AS id FROM Enlaces", "id").await, json!(1));
    run(&mut s, "CREATE INDEX Otra.pepe ON Otra (pepe) NOTUNIQUE").await;
    let req = RenameRequest { target: RenameTarget::Object { object: obj(kinds::TABLE, "Otra"), parent: None }, new_name: "Otra2".into(), table: None, definition: None };
    let err = apply(d.as_ref(), &mut s, &req).await.unwrap_err();
    assert!(err.contains("indexes"), "{err}");
    assert_eq!(one(&mut s, "SELECT pepe FROM Otra", "pepe").await, json!("otra"));
    // With the table (as the app could pass it), the index moves along.
    let req = RenameRequest { table: Some(table(&mut s, "Otra").await), ..req };
    apply(d.as_ref(), &mut s, &req).await.unwrap();
    assert_eq!(indexes_of(&mut s, "Otra2").await, vec![("Otra.pepe".to_string(), vec!["pepe".to_string()])]);

    drop(s);
    admin.drop_database("dbine_rename").await.unwrap();
}
