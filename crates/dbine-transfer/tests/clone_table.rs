//! "Clonar tabla" end to end against the real SQLite driver: structure,
//! 10k rows (NULLs, blobs, identity), indexes, a self reference, and the
//! refusals.

use dbine_driver::{ConnectionConfig, ObjectRef};
use dbine_transfer::clone_table::{clone_table, CloneControl, CloneEvent, CloneOptions, CloneRequest, ConfigEndpoints};
use rusqlite::types::Value;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const ROWS: i64 = 10_000;

/// A test database, deleted (with its WAL files) when the test ends,
/// passed or not.
struct Db(PathBuf);

impl std::ops::Deref for Db {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl AsRef<std::path::Path> for Db {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        for s in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{s}", self.0.display()));
        }
    }
}

fn db(tag: &str) -> Db {
    let p = std::env::temp_dir().join(format!("dbine-clone-test-{tag}-{}.sqlite", std::process::id()));
    for s in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{s}", p.display()));
    }
    let c = Connection::open(&p).unwrap();
    c.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE clientes (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             nombre TEXT NOT NULL,
             monto REAL,
             foto BLOB,
             padre INTEGER REFERENCES clientes (id),
             CONSTRAINT ck_monto CHECK (monto IS NULL OR monto >= 0)
         );
         CREATE INDEX ix_clientes_nombre ON clientes (nombre);
         CREATE UNIQUE INDEX ux_foto ON clientes (foto) WHERE foto IS NOT NULL;",
    )
    .unwrap();
    let tx = c.unchecked_transaction().unwrap();
    {
        let mut st = tx.prepare("INSERT INTO clientes (id, nombre, monto, foto, padre) VALUES (?1, ?2, ?3, ?4, ?5)").unwrap();
        for i in 1..=ROWS {
            // Gaps in the ids (every 7th skipped) and the top one deleted
            // later: the clone must keep the ids, not renumber them.
            if i % 7 == 0 {
                continue;
            }
            let monto: Option<f64> = (i % 3 != 0).then_some(i as f64 * 1.25);
            let foto: Option<Vec<u8>> = (i % 5 == 0).then(|| (0..(i % 50) as u8 + 1).chain([0u8, 255, (i % 256) as u8, (i / 256) as u8]).collect());
            let padre: Option<i64> = (i > 2 && i % 11 == 0).then_some(if (i - 1) % 7 == 0 { i - 2 } else { i - 1 });
            st.execute(rusqlite::params![i, format!("cliente ñ {i}"), monto, foto, padre]).unwrap();
        }
    }
    tx.commit().unwrap();
    Db(p)
}

fn rows(c: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut st = c.prepare(&format!("SELECT id, nombre, monto, foto, padre FROM \"{table}\" ORDER BY id")).unwrap();
    st.query_map([], |r| (0..5).map(|i| r.get::<_, Value>(i)).collect::<rusqlite::Result<Vec<_>>>())
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn endpoints(p: &PathBuf) -> Arc<ConfigEndpoints> {
    let driver = dbine_driver_sqlite::drivers().into_iter().find(|d| d.info().id == "sqlite").unwrap();
    Arc::new(ConfigEndpoints {
        driver,
        config: ConnectionConfig { driver: "sqlite".into(), host: p.display().to_string(), ..Default::default() },
        database: None,
    })
}

fn request(name: &str, options: CloneOptions) -> CloneRequest {
    CloneRequest { source: ObjectRef { kind: "table".into(), schema: None, name: "clientes".into() }, new_name: name.into(), options }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clones_structure_and_every_row() {
    let p = db("full");
    let before = rows(&Connection::open(&p).unwrap(), "clientes");
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    let name = "clientes_20260930_101010";
    let report = clone_table(endpoints(&p), request(name, CloneOptions::default()), &CloneControl::default(), move |e| ev.lock().unwrap().push(e))
        .await
        .unwrap();
    let expected = before.len() as u64;
    assert_eq!(report.rows, expected);
    assert_eq!(report.table.name, name);

    let c = Connection::open(&p).unwrap();
    // Row for row, NULLs and blobs included; the original untouched.
    assert_eq!(rows(&c, name), before);
    assert_eq!(rows(&c, "clientes"), before);

    // Indexes renamed with the new name, the unique one partial as before.
    let ix: Vec<(String, i64)> = c
        .prepare(&format!("SELECT name, \"unique\" FROM pragma_index_list('{name}') WHERE origin = 'c' ORDER BY name"))
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(ix, vec![(format!("{name}_ux_foto"), 1), (format!("ix_{name}_nombre"), 0)]);
    let partial: String = c.query_row("SELECT sql FROM sqlite_master WHERE name = ?1", [format!("{name}_ux_foto")], |r| r.get(0)).unwrap();
    assert!(partial.contains("WHERE foto IS NOT NULL"), "{partial}");
    // The check and the self reference, the latter pointing to the clone.
    let create: String = c.query_row("SELECT sql FROM sqlite_master WHERE name = ?1", [name], |r| r.get(0)).unwrap();
    assert!(create.contains("AUTOINCREMENT"), "{create}");
    assert!(create.contains("CHECK"), "{create}");
    let fk: String = c.query_row(&format!("SELECT \"table\" FROM pragma_foreign_key_list('{name}')"), [], |r| r.get(0)).unwrap();
    assert_eq!(fk, name);
    assert!(c.execute(&format!("INSERT INTO \"{name}\" (nombre, monto) VALUES ('x', -1)"), []).is_err(), "the check came along");

    // The next insert continues after the copied ids.
    let max: i64 = c.query_row(&format!("SELECT MAX(id) FROM \"{name}\""), [], |r| r.get(0)).unwrap();
    c.execute(&format!("INSERT INTO \"{name}\" (nombre) VALUES ('nuevo')"), []).unwrap();
    assert_eq!(c.last_insert_rowid(), max + 1);

    let evs = events.lock().unwrap();
    let last = evs.iter().rev().find_map(|e| match e {
        CloneEvent::Progress { rows_done, rows_total, .. } => Some((*rows_done, *rows_total)),
        _ => None,
    });
    assert_eq!(last, Some((expected, Some(expected))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn structure_only_and_refusals() {
    let p = db("refuse");
    let ep = endpoints(&p);
    let r = clone_table(ep.clone(), request("vacia", CloneOptions { with_data: false, with_indexes: false }), &CloneControl::default(), |_| {})
        .await
        .unwrap();
    assert_eq!(r.rows, 0);
    // The explicit unique index is named as one that isn't created.
    assert!(r.notes.iter().any(|n| n.contains("índices únicos ux_foto")), "{:?}", r.notes);
    let c = Connection::open(&p).unwrap();
    let n: i64 = c.query_row("SELECT COUNT(*) FROM vacia", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
    let ix: i64 = c.query_row("SELECT COUNT(*) FROM pragma_index_list('vacia') WHERE origin = 'c'", [], |r| r.get(0)).unwrap();
    assert_eq!(ix, 0);

    // The name is taken (ignoring case): refused, nothing touched.
    let e = clone_table(ep.clone(), request("VACIA", CloneOptions::default()), &CloneControl::default(), |_| {}).await.unwrap_err();
    assert!(e.to_string().contains("ya existe"), "{e}");
    let n: i64 = c.query_row("SELECT COUNT(*) FROM vacia", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);

    // A view isn't cloned.
    c.execute_batch("CREATE VIEW v AS SELECT id FROM clientes").unwrap();
    let mut req = request("v2", CloneOptions::default());
    req.source = ObjectRef { kind: "view".into(), schema: None, name: "v".into() };
    assert!(clone_table(ep.clone(), req, &CloneControl::default(), |_| {}).await.is_err());

    // Generated columns SQLite's catalog hides: they come along from the
    // stored CREATE, recomputed, never loaded.
    c.execute_batch("CREATE TABLE calc (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) VIRTUAL); INSERT INTO calc (a) VALUES (21)").unwrap();
    let mut req = request("calc2", CloneOptions::default());
    req.source.name = "calc".into();
    let r = clone_table(ep.clone(), req, &CloneControl::default(), |_| {}).await.unwrap();
    assert!(r.notes.iter().any(|n| n.contains("calculadas")), "{:?}", r.notes);
    let b: i64 = c.query_row("SELECT b FROM calc2", [], |r| r.get(0)).unwrap();
    assert_eq!(b, 42);

    // The name an index has (tables and indexes share SQLite's namespace):
    // refused in Spanish before anything is written.
    let e = clone_table(ep.clone(), request("IX_CLIENTES_NOMBRE", CloneOptions::default()), &CloneControl::default(), |_| {}).await.unwrap_err();
    assert!(e.to_string().contains("ya existe un objeto llamado «ix_clientes_nombre»"), "{e}");
    let left: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'IX_CLIENTES_NOMBRE'", [], |r| r.get(0)).unwrap();
    assert_eq!(left, 0);

    // Cancelled before it starts: nothing left behind.
    let ctl = CloneControl::default();
    ctl.cancel();
    assert!(clone_table(ep, request("cancelada", CloneOptions::default()), &ctl, |_| {}).await.is_err());
    let left: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'cancelada'", [], |r| r.get(0)).unwrap();
    assert_eq!(left, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn index_names_taken_elsewhere_are_avoided() {
    // SQLite's index names are the schema's: another table already has the
    // name the clone's index would get. Found before writing, not after
    // the rows; the index gets another name, and the report says so.
    let p = db("ixname");
    let c = Connection::open(&p).unwrap();
    c.execute_batch("CREATE TABLE otra (v TEXT); CREATE INDEX ix_c_nombre ON otra (v);").unwrap();
    let r = clone_table(endpoints(&p), request("c", CloneOptions::default()), &CloneControl::default(), |_| {}).await.unwrap();
    let ix: Vec<String> = c
        .prepare("SELECT name FROM pragma_index_list('c') WHERE origin = 'c' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(ix, vec!["c_ux_foto".to_string(), "ix_c_nombre_2".to_string()]);
    assert!(r.renames.iter().any(|x| x.from == "ix_clientes_nombre" && x.to == "ix_c_nombre_2"), "{:?}", r.renames);
    assert!(r.notes.iter().any(|n| n.contains("ix_c_nombre_2") && n.contains("ya existe")), "{:?}", r.notes);
    // The other table's index is untouched.
    let other: String = c.query_row("SELECT tbl_name FROM sqlite_master WHERE name = 'ix_c_nombre'", [], |r| r.get(0)).unwrap();
    assert_eq!(other, "otra");
    assert_eq!(rows(&c, "c"), rows(&c, "clientes"));
}
