//! Bulk transfer against a real server:
//! `docker run -d --name dbine-test-orientdb -p 22480:2480 -e ORIENTDB_ROOT_PASSWORD=dbine-test-pass orientdb:3.2`
//! then
//! `DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 cargo test -p dbine-driver-orientdb -- --ignored transfer`.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use dbine_driver_orientdb::{EDGE, VERTEX};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn cfg(url: &str, read_only: bool) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        read_only,
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 100, &mut out).await {
        panic!("{text}: {e}");
    }
    out
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

#[derive(Default)]
struct Collect {
    cols: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
    batches: usize,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.cols = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        self.batches += 1;
        self.rows.extend(batch.rows);
        Ok(())
    }
}

struct Source(std::vec::IntoIter<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Source {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

fn source(rows: Vec<Vec<Cell>>) -> Source {
    let batches: Vec<RowBatch> = rows.chunks(1000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    Source(batches.into_iter())
}

fn load_spec(table: ObjectRef, columns: &[&str]) -> LoadSpec {
    LoadSpec {
        table,
        columns: columns.iter().map(|s| s.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

fn cols(names: &[&str]) -> Vec<TransferColumn> {
    names.iter().map(|n| TransferColumn { name: n.to_string(), type_name: String::new(), nullable: true }).collect()
}

async fn read(s: &mut Box<dyn Session>, spec: ReadSpec) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(&spec, sink.clone()).await.expect("read_batches");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.rows.len());
    c
}

/// JSON cells compared as values (key order aside).
fn same(a: &Cell, b: &Cell) -> bool {
    match (a, b) {
        (Cell::Json(x), Cell::Json(y)) => serde_json::from_str::<Value>(x).unwrap() == serde_json::from_str::<Value>(y).unwrap(),
        _ => a == b,
    }
}

#[tokio::test]
#[ignore]
async fn transfer_round_trip() {
    let url = std::env::var("DBINE_TEST_ORIENTDB_URL").expect("DBINE_TEST_ORIENTDB_URL");
    let d = dbine_driver_orientdb::drivers().remove(0);
    assert!(d.supports_bulk_load());
    let c = cfg(&url, false);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let _ = admin.drop_database("dbine_transfer").await;
    admin.create_database("dbine_transfer").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_transfer")).await.unwrap();

    run(
        &mut s,
        "CREATE CLASS Target; CREATE CLASS Types; \
         CREATE PROPERTY Types.id INTEGER; CREATE PROPERTY Types.b BOOLEAN; CREATE PROPERTY Types.sh SHORT; \
         CREATE PROPERTY Types.l LONG; CREATE PROPERTY Types.f FLOAT; CREATE PROPERTY Types.d DOUBLE; \
         CREATE PROPERTY Types.dec DECIMAL; CREATE PROPERTY Types.t STRING; CREATE PROPERTY Types.bin BINARY; \
         CREATE PROPERTY Types.dt DATE; CREATE PROPERTY Types.ts DATETIME; CREATE PROPERTY Types.tz DATETIME; \
         CREATE PROPERTY Types.e EMBEDDED; CREATE PROPERTY Types.el EMBEDDEDLIST; CREATE PROPERTY Types.em EMBEDDEDMAP; \
         CREATE PROPERTY Types.lk LINK; CREATE PROPERTY Types.u STRING; CREATE PROPERTY Types.n STRING",
    )
    .await;
    let target = run(&mut s, "INSERT INTO Target SET name = 'x'").await;
    let target_rid = target.results[0].rows[0][0].as_str().unwrap().to_string();

    // Every type, a row of NULLs and a large binary.
    let names = ["id", "b", "sh", "l", "f", "d", "dec", "t", "bin", "dt", "ts", "tz", "e", "el", "em", "lk", "u", "n", "free"];
    let big: Vec<u8> = (0..600_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let full = vec![
        Cell::Int(1),
        Cell::Bool(true),
        Cell::Int(-32768),
        Cell::Int(9_007_199_254_740_993),
        Cell::Float(1.5),
        Cell::Float(f64::INFINITY),
        Cell::Decimal("12345678901234567890.123456789".into()),
        Cell::Text("línea 1\nlínea \"2\" \\ ' 😀 \u{1}".into()),
        Cell::Bytes(big.clone()),
        Cell::Date("2024-02-29".into()),
        Cell::DateTime("2024-02-03 04:05:06.789".into()),
        Cell::DateTimeTz("2024-02-03 04:05:06.789+02:00".into()),
        Cell::Json(r#"{"x": 1, "y": [1, {"z": "a"}]}"#.into()),
        Cell::Json(r#"[1, "a", 2.5]"#.into()),
        Cell::Json(r#"{"k": "v"}"#.into()),
        Cell::Text(target_rid.clone()),
        Cell::Uuid("6f1c2c9e-8a1b-4c3d-9e8f-0a1b2c3d4e5f".into()),
        Cell::Int(42),
        Cell::Int(7),
    ];
    let mut nulls = vec![Cell::Int(2)];
    nulls.extend(std::iter::repeat_n(Cell::Null, names.len() - 1));
    let t = obj(kinds::TABLE, "Types");
    let mut committed = Vec::new();
    let p = Mutex::new(&mut committed);
    let n = s
        .bulk_load(&load_spec(t.clone(), &names), &cols(&names), &mut source(vec![full.clone(), nulls.clone()]), &|n| p.lock().unwrap().push(n))
        .await
        .expect("bulk_load");
    assert_eq!(n, 2);
    assert_eq!(committed.last(), Some(&2));

    // Read back in another order than the class's.
    let mut order = names.to_vec();
    order.reverse();
    let got = read(&mut s, ReadSpec { table: t.clone(), columns: Some(order.iter().map(|s| s.to_string()).collect()), filter: None }).await;
    assert_eq!(got.cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), order);
    assert_eq!(got.cols.iter().find(|c| c.name == "ts").unwrap().type_name, "DATETIME");
    let mut rows = got.rows.clone();
    rows.sort_by_key(|r| match r.last() {
        Some(Cell::Int(i)) => *i,
        _ => 0,
    });
    let expect = |i: usize, c: &Cell| -> Cell {
        match (names[i], c) {
            // Date-times come back with their offset, in the database's zone (UTC here).
            ("tz", Cell::DateTimeTz(_)) => Cell::DateTimeTz("2024-02-03 02:05:06.789+00:00".into()),
            ("ts", Cell::DateTime(t)) => Cell::DateTimeTz(format!("{t}+00:00")),
            ("u", Cell::Uuid(u)) => Cell::Text(u.clone()),
            ("n", Cell::Int(i)) => Cell::Text(i.to_string()),
            _ => c.clone(),
        }
    };
    for (row, src) in rows.iter().zip([&full, &nulls]) {
        for (j, name) in order.iter().enumerate() {
            let i = names.iter().position(|n| n == name).unwrap();
            let want = expect(i, &src[i]);
            assert!(same(&row[j], &want), "{name}: {:?} != {:?}", row[j], want);
        }
    }

    // A filter in OrientDB SQL; no columns: the class's.
    let got = read(&mut s, ReadSpec { table: t.clone(), columns: None, filter: Some("id = 2".into()) }).await;
    assert_eq!(got.rows.len(), 1);
    assert!(got.cols.iter().any(|c| c.name == "dec" && c.type_name == "DECIMAL"));
    // A bad filter fails, it isn't ignored.
    assert!(s.read_batches(&ReadSpec { table: t.clone(), columns: None, filter: Some("id = = 2".into()) }, Arc::new(Mutex::new(Collect::default()))).await.is_err());

    // 50k vertices: load, read back, compare.
    run(&mut s, "CREATE CLASS Big EXTENDS V; CREATE PROPERTY Big.id LONG; CREATE PROPERTY Big.v DOUBLE; CREATE PROPERTY Big.ts DATETIME").await;
    let big_names = ["id", "name", "v", "ts"];
    let rows: Vec<Vec<Cell>> = (0..50_000i64)
        .map(|i| {
            vec![
                Cell::Int(i),
                if i % 10 == 0 { Cell::Null } else { Cell::Text(format!("fila {i}")) },
                Cell::Float(i as f64 / 4.0),
                Cell::DateTime(format!("2024-01-01 00:{:02}:{:02}.{:03}", i / 60 % 60, i % 60, i % 1000)),
            ]
        })
        .collect();
    let b = obj(VERTEX, "Big");
    let windows = Mutex::new(0u32);
    let t0 = Instant::now();
    let n = s
        .bulk_load(&load_spec(b.clone(), &big_names), &cols(&big_names), &mut source(rows.clone()), &|_| *windows.lock().unwrap() += 1)
        .await
        .expect("bulk_load 50k");
    let load = t0.elapsed();
    assert_eq!(n, 50_000);
    assert!(*windows.lock().unwrap() >= 50);
    let t0 = Instant::now();
    let got = read(&mut s, ReadSpec { table: b.clone(), columns: Some(big_names.iter().map(|s| s.to_string()).collect()), filter: None }).await;
    let rd = t0.elapsed();
    println!(
        "OrientDB 50k: carga {:.0} filas/s ({load:?}), lectura {:.0} filas/s ({rd:?}, {} lotes)",
        50_000.0 / load.as_secs_f64(),
        50_000.0 / rd.as_secs_f64(),
        got.batches
    );
    let mut back = got.rows;
    back.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(back.len(), rows.len());
    // DATETIME comes back with its offset (UTC here).
    let rows: Vec<Vec<Cell>> = rows
        .into_iter()
        .map(|mut r| {
            if let Cell::DateTime(t) = &r[3] {
                r[3] = Cell::DateTimeTz(format!("{t}+00:00"));
            }
            r
        })
        .collect();
    assert!(back == rows, "the 50k rows read back differ");

    // Edges: out / in are record ids of the target's vertices.
    run(&mut s, "CREATE CLASS Knows EXTENDS E").await;
    let v = run(&mut s, "SELECT @rid FROM Big WHERE id < 2 ORDER BY id").await;
    let (a, z) = (v.results[0].rows[0][0].as_str().unwrap().to_string(), v.results[0].rows[1][0].as_str().unwrap().to_string());
    let k = obj(EDGE, "Knows");
    let n = s
        .bulk_load(&load_spec(k.clone(), &["out", "in", "since"]), &cols(&["out", "in", "since"]), &mut source(vec![vec![Cell::Text(a.clone()), Cell::Text(z.clone()), Cell::Int(2020)]]), &|_| {})
        .await
        .unwrap();
    assert_eq!(n, 1);
    let got = read(&mut s, ReadSpec { table: k.clone(), columns: None, filter: None }).await;
    assert_eq!(got.cols[0].name, "out");
    assert_eq!(got.rows[0][..2], [Cell::Text(a), Cell::Text(z)]);
    // An edge without its ends can't be loaded.
    assert!(s.bulk_load(&load_spec(k, &["since"]), &cols(&["since"]), &mut source(vec![vec![Cell::Int(1)]]), &|_| {}).await.is_err());

    // A failed chunk is rolled back whole.
    let before = run(&mut s, "SELECT count(*) AS n FROM Types").await.results[0].rows[0][0].clone();
    let bad = vec![vec![Cell::Int(10), Cell::Int(1)], vec![Cell::Int(11), Cell::Text("not a number".into())]];
    assert!(s.bulk_load(&load_spec(t.clone(), &["id", "sh"]), &cols(&["id", "sh"]), &mut source(bad), &|_| {}).await.is_err());
    // Nothing to load is not an error.
    assert_eq!(s.bulk_load(&load_spec(t.clone(), &["id"]), &cols(&["id"]), &mut source(vec![]), &|_| {}).await.unwrap(), 0);
    let after = run(&mut s, "SELECT count(*) AS n FROM Types").await.results[0].rows[0][0].clone();
    assert_eq!(before, after);

    // Read-only connections read, and refuse loading.
    let mut ro = d.connect(&cfg(&url, true), Some("dbine_transfer")).await.unwrap();
    assert_eq!(read(&mut ro, ReadSpec { table: t.clone(), columns: Some(vec!["id".into()]), filter: None }).await.rows.len(), 2);
    assert!(ro.bulk_load(&load_spec(t.clone(), &["id"]), &cols(&["id"]), &mut source(vec![vec![Cell::Int(3)]]), &|_| {}).await.is_err());

    // A document class has no graph bookkeeping: `in_stock` / `out_date` are data,
    // declared or not, on load and on read.
    run(&mut s, "CREATE CLASS Stock; CREATE PROPERTY Stock.in_stock BOOLEAN").await;
    let st = obj(kinds::TABLE, "Stock");
    let n = s
        .bulk_load(
            &load_spec(st.clone(), &["id", "in_stock", "out_date"]),
            &cols(&["id", "in_stock", "out_date"]),
            &mut source(vec![vec![Cell::Int(1), Cell::Bool(true), Cell::Text("mañana".into())]]),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(n, 1);
    let got = read(&mut s, ReadSpec { table: st, columns: None, filter: None }).await;
    let names: Vec<&str> = got.cols.iter().map(|c| c.name.as_str()).collect();
    let at = |n: &str| got.rows[0][names.iter().position(|x| *x == n).unwrap_or_else(|| panic!("{n} missing in {names:?}"))].clone();
    assert_eq!(at("in_stock"), Cell::Bool(true));
    assert_eq!(at("out_date"), Cell::Text("mañana".into()));

    // A NULL into a property with a DEFAULT stays NULL.
    run(&mut s, "CREATE CLASS Dflt; CREATE PROPERTY Dflt.id INTEGER; CREATE PROPERTY Dflt.x STRING (DEFAULT 'dflt')").await;
    let df = obj(kinds::TABLE, "Dflt");
    s.bulk_load(&load_spec(df.clone(), &["id", "x"]), &cols(&["id", "x"]), &mut source(vec![vec![Cell::Int(1), Cell::Null]]), &|_| {}).await.unwrap();
    let got = read(&mut s, ReadSpec { table: df, columns: Some(vec!["id".into(), "x".into()]), filter: None }).await;
    assert_eq!(got.rows, vec![vec![Cell::Int(1), Cell::Null]]);

    // An unbalanced filter can't slip out of the class / @rid bounds.
    let e = s.read_batches(&ReadSpec { table: t.clone(), columns: None, filter: Some("id=1) OR (id=1".into()) }, Arc::new(Mutex::new(Collect::default()))).await;
    assert!(e.is_err());
    // A long filter goes through the read-only /query endpoint (in the URL).
    let long = format!("id IN [{}]", (0..3000).map(|i| i.to_string()).collect::<Vec<_>>().join(", "));
    assert_eq!(read(&mut s, ReadSpec { table: t.clone(), columns: Some(vec!["id".into()]), filter: Some(long) }).await.rows.len(), 2);

    // Wide records: pages sized by bytes, everything read back.
    run(&mut s, "CREATE CLASS Wide; CREATE PROPERTY Wide.id INTEGER; CREATE PROPERTY Wide.bin BINARY").await;
    let w = obj(kinds::TABLE, "Wide");
    let wide: Vec<Vec<Cell>> = (0..40).map(|i| vec![Cell::Int(i), Cell::Bytes(vec![i as u8; 300_000])]).collect();
    s.bulk_load(&load_spec(w.clone(), &["id", "bin"]), &cols(&["id", "bin"]), &mut source(wide.clone()), &|_| {}).await.unwrap();
    let mut got = read(&mut s, ReadSpec { table: w, columns: Some(vec!["id".into(), "bin".into()]), filter: None }).await.rows;
    got.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert!(got == wide, "wide rows differ");

    // A DST fall-back hour: two instants with the same wall-clock time stay apart.
    run(&mut s, "ALTER DATABASE TIMEZONE 'Europe/Berlin'").await;
    run(&mut s, "CREATE CLASS Dst; CREATE PROPERTY Dst.id INTEGER; CREATE PROPERTY Dst.ts DATETIME").await;
    let dst = obj(kinds::TABLE, "Dst");
    let two = vec![
        vec![Cell::Int(1), Cell::DateTimeTz("2024-10-27 02:30:00.000+02:00".into())],
        vec![Cell::Int(2), Cell::DateTimeTz("2024-10-27 02:30:00.000+01:00".into())],
    ];
    s.bulk_load(&load_spec(dst.clone(), &["id", "ts"]), &cols(&["id", "ts"]), &mut source(two.clone()), &|_| {}).await.unwrap();
    let mut got = read(&mut s, ReadSpec { table: dst, columns: Some(vec!["id".into(), "ts".into()]), filter: None }).await.rows;
    got.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(got, two);

    drop(s);
    drop(ro);
    admin.drop_database("dbine_transfer").await.unwrap();
}

/// Reads whose memory must stay bounded, and filters that must not slip
/// out of the class: a class whose record sizes jump from 1 byte to
/// 120 KB, one read without columns (its fields sampled by name and type
/// only), and a filter hiding parentheses in comments.
#[tokio::test]
#[ignore]
async fn transfer_bounds() {
    let url = std::env::var("DBINE_TEST_ORIENTDB_URL").expect("DBINE_TEST_ORIENTDB_URL");
    let d = dbine_driver_orientdb::drivers().remove(0);
    let c = cfg(&url, false);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let _ = admin.drop_database("dbine_transfer_bounds").await;
    admin.create_database("dbine_transfer_bounds").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_transfer_bounds")).await.unwrap();

    // One cluster, so record ids follow the insert order: the small
    // records come first and the large ones after them.
    run(&mut s, "CREATE CLASS Mixed CLUSTERS 1").await;
    run(&mut s, "CREATE PROPERTY Mixed.id INTEGER").await;
    let m = obj(kinds::TABLE, "Mixed");
    let rows: Vec<Vec<Cell>> = (0..390i64)
        .map(|i| {
            let len = if i < 90 { 1 } else { 120_000 };
            vec![Cell::Int(i), Cell::Text(char::from(b'a' + (i % 26) as u8).to_string().repeat(len))]
        })
        .collect();
    // Two loads: the small records get the lower ids.
    for part in [&rows[..90], &rows[90..]] {
        s.bulk_load(&load_spec(m.clone(), &["id", "s"]), &cols(&["id", "s"]), &mut source(part.to_vec()), &|_| {}).await.unwrap();
    }
    let by_id = |mut r: Vec<Vec<Cell>>| {
        r.sort_by_key(|r| match r[0] {
            Cell::Int(i) => i,
            _ => -1,
        });
        r
    };
    let got = read(&mut s, ReadSpec { table: m.clone(), columns: Some(vec!["id".into(), "s".into()]), filter: None }).await;
    assert!(by_id(got.rows) == rows, "mixed-size rows differ");

    // Without columns: the declared id and the sampled schemaless `s`.
    let got = read(&mut s, ReadSpec { table: m.clone(), columns: None, filter: None }).await;
    let names: Vec<&str> = got.cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "s"]);
    assert_eq!(got.cols[1].type_name, "STRING");
    assert!(!got.cols[1].nullable);
    assert!(by_id(got.rows) == rows, "mixed-size rows (no columns) differ");

    // A filter with comments: refused, not read with duplicates and rows
    // of a subclass.
    run(&mut s, "CREATE CLASS D; CREATE CLASS D2 EXTENDS D").await;
    run(&mut s, "INSERT INTO D SET id = 1; INSERT INTO D2 SET id = 1").await;
    let dd = obj(kinds::TABLE, "D");
    for f in ["id=1 /* ( */ ) OR (id=1 /* ) */", "id=1 -- x", "id=1 // x"] {
        let e = s.read_batches(&ReadSpec { table: dd.clone(), columns: Some(vec!["id".into()]), filter: Some(f.into()) }, Arc::new(Mutex::new(Collect::default()))).await;
        assert!(e.is_err(), "{f}");
    }
    let got = read(&mut s, ReadSpec { table: dd, columns: Some(vec!["id".into()]), filter: Some("id = 1".into()) }).await;
    assert_eq!(got.rows, vec![vec![Cell::Int(1)]]);

    drop(s);
    admin.drop_database("dbine_transfer_bounds").await.unwrap();
}

/// A sink that takes its time with each row, so writers get in while a
/// read is under way.
#[derive(Default)]
struct Slow(Collect);

impl BatchSink for Slow {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.0.begin(columns)
    }
    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        std::thread::sleep(std::time::Duration::from_millis(batch.rows.len() as u64));
        self.0.batch(batch)
    }
}

/// Records inserted into a class's clusters while it's read (inside
/// windows not read yet) don't push out records that were there, and
/// a class whose records turn from plain text into control characters
/// (6 reply bytes each) is read whole, its oversized pages cut again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn transfer_concurrent_inserts_and_escaped_text() {
    let url = std::env::var("DBINE_TEST_ORIENTDB_URL").expect("DBINE_TEST_ORIENTDB_URL");
    let d = dbine_driver_orientdb::drivers().remove(0);
    let c = cfg(&url, false);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let _ = admin.drop_database("dbine_transfer_conc").await;
    admin.create_database("dbine_transfer_conc").await.unwrap();
    let mut s = d.connect(&c, Some("dbine_transfer_conc")).await.unwrap();

    const N: i64 = 1500;
    run(&mut s, "CREATE CLASS Conc CLUSTERS 8").await;
    run(&mut s, "CREATE PROPERTY Conc.id INTEGER").await;
    let conc = obj(kinds::TABLE, "Conc");
    let rows: Vec<Vec<Cell>> = (0..N).map(|i| vec![Cell::Int(i), Cell::Text("x".repeat(10_000))]).collect();
    s.bulk_load(&load_spec(conc.clone(), &["id", "s"]), &cols(&["id", "s"]), &mut source(rows), &|_| {}).await.unwrap();

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut w = d.connect(&c, Some("dbine_transfer_conc")).await.unwrap();
    let writer = {
        let stop = stop.clone();
        let pad = "y".repeat(10_000);
        tokio::spawn(async move {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let script: Vec<String> = (0..8).map(|_| format!("INSERT INTO Conc SET id = -1, s = '{pad}'")).collect();
                let mut out = QueryOutcome::default();
                w.execute(&script.join(";\n"), 10, &mut out).await.unwrap();
                n += 8;
            }
            n
        })
    };
    let sink = Arc::new(Mutex::new(Slow::default()));
    let spec = ReadSpec { table: conc.clone(), columns: Some(vec!["id".into()]), filter: None };
    let res = s.read_batches(&spec, sink.clone()).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let inserted = writer.await.unwrap();
    let n = res.expect("read under inserts");
    let got = std::mem::take(&mut sink.lock().unwrap().0.rows);
    assert_eq!(n as usize, got.len());
    let mut ids: Vec<i64> = got.iter().filter_map(|r| if let Cell::Int(i) = r[0] { (i >= 0).then_some(i) } else { None }).collect();
    ids.sort();
    let missing = (0..N).filter(|i| ids.binary_search(i).is_err()).count();
    assert!(inserted > 0, "no concurrent inserts");
    assert_eq!(missing, 0, "{missing} pre-existing records missing ({inserted} inserted meanwhile)");
    ids.dedup();
    assert_eq!(ids.len() as i64, N, "duplicates");

    // Plain text first (the factor calibrates at ~1), then control
    // characters that reply 6 bytes per stored byte.
    run(&mut s, "CREATE CLASS Esc CLUSTERS 1").await;
    run(&mut s, "CREATE PROPERTY Esc.id INTEGER").await;
    run(&mut s, "CREATE PROPERTY Esc.s STRING").await;
    let esc = obj(kinds::TABLE, "Esc");
    let rows: Vec<Vec<Cell>> =
        (0..61i64).map(|i| vec![Cell::Int(i), Cell::Text(if i == 0 { "a".repeat(100_000) } else { "\u{1}".repeat(100_000) })]).collect();
    // Two loads: the plain record gets the lowest id, so it's the first page.
    for part in [&rows[..1], &rows[1..]] {
        s.bulk_load(&load_spec(esc.clone(), &["id", "s"]), &cols(&["id", "s"]), &mut source(part.to_vec()), &|_| {}).await.unwrap();
    }
    let t = Instant::now();
    let mut got = read(&mut s, ReadSpec { table: esc, columns: Some(vec!["id".into(), "s".into()]), filter: None }).await.rows;
    got.sort_by_key(|r| if let Cell::Int(i) = r[0] { i } else { -1 });
    assert!(got == rows, "escaped rows differ");
    eprintln!("escaped read: {:?}", t.elapsed());

    drop(s);
    admin.drop_database("dbine_transfer_conc").await.unwrap();
}
