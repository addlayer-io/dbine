//! Bulk transfer against real servers:
//!
//! ```sh
//! docker start dbine-test-mongodb dbine-test-ferretdb   # -p 25201 / 25203, root/secret
//! cargo test --release -p dbine-driver-mongodb -- --ignored transfer --nocapture --test-threads=1
//! ```
//!
//! `DBINE_TEST_MONGODB_URL` / `DBINE_TEST_FERRETDB_URL` override the
//! servers (defaults `mongodb://root:secret@localhost:25201/?authSource=admin`
//! and `mongodb://root:secret@localhost:25203/`).

use dbine_driver::async_trait;
use dbine_driver::transfer::{BatchSink, BatchSource, Cell, CopySpec, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, Driver, Error, ObjectRef, Session};
use mongodb::bson::{doc, oid::ObjectId, spec::BinarySubtype, Binary, Bson, DateTime, Decimal128, Document, Regex, Timestamp};
use std::collections::VecDeque;
use std::io;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DB: &str = "dbine_transfer";

struct Server {
    driver: usize,
    url: String,
    ferret: bool,
}

fn servers() -> Vec<Server> {
    vec![
        Server {
            driver: 0,
            url: std::env::var("DBINE_TEST_MONGODB_URL").unwrap_or_else(|_| "mongodb://root:secret@localhost:25201/?authSource=admin".into()),
            ferret: false,
        },
        Server {
            driver: 1,
            url: std::env::var("DBINE_TEST_FERRETDB_URL").unwrap_or_else(|_| "mongodb://root:secret@localhost:25203/".into()),
            ferret: true,
        },
    ]
}

fn driver(s: &Server) -> Arc<dyn Driver> {
    dbine_driver_mongodb::drivers().remove(s.driver)
}

async fn session(s: &Server) -> Box<dyn Session> {
    let mut c = ConnectionConfig { driver: driver(s).info().id.into(), database: DB.into(), ..Default::default() };
    c.options.insert("connection_string".into(), s.url.clone());
    let d = driver(s);
    assert!(d.supports_bulk_load());
    d.connect(&c, None).await.expect("connect")
}

async fn raw(s: &Server) -> mongodb::Database {
    mongodb::Client::with_uri_str(&s.url).await.expect("client").database(DB)
}

fn coll(name: &str) -> ObjectRef {
    ObjectRef { kind: "collection".into(), schema: None, name: name.into() }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.batches.push(batch);
        Ok(())
    }
}

struct Batches(VecDeque<RowBatch>);

#[async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.pop_front()
    }
}

async fn read(s: &mut Box<dyn Session>, name: &str, filter: Option<&str>) -> (Vec<TransferColumn>, Vec<RowBatch>, u64) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: coll(name), columns: None, filter: filter.map(str::to_string) };
    let n = s.read_batches(&spec, sink.clone()).await.expect("read_batches");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    (c.columns, c.batches, n)
}

async fn load(s: &mut Box<dyn Session>, name: &str, columns: &[TransferColumn], batches: Vec<RowBatch>, commit_rows: u64) -> (u64, Vec<u64>) {
    let spec = LoadSpec {
        table: coll(name),
        columns: columns.iter().map(|c| c.name.clone()).collect(),
        table_lock: false,
        keep_identity: true,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let seen = Mutex::new(Vec::new());
    let n = s.bulk_load(&spec, columns, &mut Batches(batches.into()), &|n| seen.lock().unwrap().push(n)).await.expect("bulk_load");
    (n, seen.into_inner().unwrap())
}

async fn all(db: &mongodb::Database, name: &str) -> Vec<Document> {
    use futures::TryStreamExt;
    db.collection::<Document>(name).find(doc! {}).sort(doc! { "_id": 1 }).await.unwrap().try_collect().await.unwrap()
}

fn sample_docs(ferret: bool) -> Vec<Document> {
    let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
    let mut full = doc! {
        "_id": oid,
        "i": 42_i32,
        "l": 1_i64 << 40,
        "small_long": 5_i64,
        "d": 3.25,
        "dec": Decimal128::from_str("12345678901234567890.1234567890").unwrap(),
        "s": "hola ñandú 😀",
        "b": true,
        "dt": DateTime::from_millis(1_706_708_700_123),
        "bin": Binary { subtype: BinarySubtype::Generic, bytes: vec![0, 1, 2, 255] },
        "uuid": Binary { subtype: BinarySubtype::Uuid, bytes: (0..16).collect() },
        "sub": { "x": 1_i32, "y": [1_i64, "a", { "z": oid }], "when": DateTime::from_millis(0) },
        "arr": [1_i32, 2.5, "tres"],
        "ref": ObjectId::parse_str("65a1b2c3d4e5f60718293a4c").unwrap(),
    };
    if !ferret {
        // Not stored by FerretDB 2.
        full.insert("ts", Timestamp { time: 1_700_000_000, increment: 3 });
        full.insert("re", Regex { pattern: "^a.*".into(), options: "i".into() });
        full.insert("mk", Bson::MinKey);
    }
    let mut big = vec![0u8; 5 * 1024 * 1024];
    for (i, b) in big.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    vec![
        full,
        doc! { "_id": ObjectId::parse_str("65a1b2c3d4e5f60718293a4d").unwrap(), "i": 7_i32, "only_here": "x" },
        doc! { "_id": ObjectId::parse_str("65a1b2c3d4e5f60718293a4e").unwrap(), "s": Bson::Null },
        doc! { "_id": ObjectId::parse_str("65a1b2c3d4e5f60718293a4f").unwrap(), "big": Binary { subtype: BinarySubtype::Generic, bytes: big } },
    ]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_all_types_round_trip() {
    for srv in servers() {
        let db = raw(&srv).await;
        for c in ["tr_src", "tr_dst"] {
            db.collection::<Document>(c).drop().await.ok();
        }
        let docs = sample_docs(srv.ferret);
        db.collection::<Document>("tr_src").insert_many(docs.clone()).await.expect("seed");
        // More documents than one cursor batch, with a field only a few have.
        let extra: Vec<Document> = (0..25_000_i64).map(|i| doc! { "_id": i, "n": i, "t": format!("t{i}") }).collect();
        db.collection::<Document>("tr_src").insert_many(extra).await.unwrap();
        db.collection::<Document>("tr_src").insert_one(doc! { "_id": 99_999_i64, "rare": 1_i32 }).await.unwrap();

        let mut s = session(&srv).await;
        let (cols, batches, n) = read(&mut s, "tr_src", None).await;
        assert_eq!(n, 25_005, "{}", srv.url);
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names[0], "_id");
        assert!(names.contains(&"rare") && names.contains(&"only_here") && names.contains(&"big"), "{names:?}");
        let rows: Vec<&Vec<Cell>> = batches.iter().flat_map(|b| &b.rows).collect();
        let at = |name: &str| names.iter().position(|n| *n == name).unwrap();
        let first = rows.iter().find(|r| r[0] == Cell::Text("65a1b2c3d4e5f60718293a4b".into())).expect("oid row");
        assert_eq!(first[at("i")], Cell::Int(42));
        assert_eq!(first[at("dec")], Cell::Decimal("12345678901234567890.1234567890".into()));
        assert_eq!(first[at("dt")], Cell::DateTimeTz("2024-01-31 13:45:00.123+00:00".into()));
        assert_eq!(first[at("uuid")], Cell::Uuid("00010203-0405-0607-0809-0a0b0c0d0e0f".into()));
        assert_eq!(first[at("bin")], Cell::Bytes(vec![0, 1, 2, 255]));
        assert!(matches!(&first[at("sub")], Cell::Json(j) if j.contains("$oid")), "{:?}", first[at("sub")]);
        // Marked as this crate's own types (see `transfer_json_from_other_engines_is_plain`).
        assert_eq!(cols[at("small_long")].type_name, "bson:long");
        let big = rows.iter().find(|r| matches!(r[at("big")], Cell::Bytes(_))).unwrap();
        assert!(matches!(&big[at("big")], Cell::Bytes(b) if b.len() == 5 * 1024 * 1024));

        // Filtered read.
        let (_, _, n) = read(&mut s, "tr_src", Some("{ n: { $gte: 24990 } }")).await;
        assert_eq!(n, 10);

        let (loaded, progress) = load(&mut s, "tr_dst", &cols, batches, 10_000).await;
        assert_eq!(loaded, 25_005);
        assert_eq!(progress.last(), Some(&25_005));
        assert!(progress.len() >= 2, "{progress:?}");

        let (src, dst) = (all(&db, "tr_src").await, all(&db, "tr_dst").await);
        assert_eq!(src.len(), dst.len());
        for (a, b) in src.iter().zip(&dst) {
            // Explicit nulls are left out (as `insert_script` does).
            let a: Document = a.iter().filter(|(_, v)| **v != Bson::Null).map(|(k, v)| (k.clone(), v.clone())).collect();
            assert_eq!(a.len(), b.len(), "{} fields of {:?}", srv.url, a.get("_id"));
            for (k, v) in &a {
                assert_eq!(Some(v), b.get(k), "{} {k} of {:?}", srv.url, a.get("_id"));
            }
        }
        // The direct copy keeps everything, explicit nulls included.
        db.collection::<Document>("tr_nat").drop().await.ok();
        let mut t = session(&srv).await;
        let copy = CopySpec { source: ReadSpec { table: coll("tr_src"), columns: None, filter: None }, target: load_spec("tr_nat", vec![], 10_000) };
        let seen = Mutex::new(Vec::new());
        let n = driver(&srv).copy_native(&mut *s, &mut *t, &copy, &|n| seen.lock().unwrap().push(n)).await.expect("copy_native");
        assert_eq!(n, 25_005);
        assert_eq!(seen.into_inner().unwrap().last(), Some(&25_005));
        assert_eq!(all(&db, "tr_src").await, all(&db, "tr_nat").await, "{}", srv.url);
        for c in ["tr_src", "tr_dst", "tr_nat"] {
            db.collection::<Document>(c).drop().await.ok();
        }
        println!("{}: round trip ok", srv.url);
    }
}

fn load_spec(name: &str, columns: Vec<String>, commit_rows: u64) -> LoadSpec {
    LoadSpec { table: coll(name), columns, table_lock: false, keep_identity: true, commit_rows, commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES }
}

async fn read_cols(s: &mut Box<dyn Session>, name: &str, columns: &[&str], filter: Option<&str>) -> dbine_driver::Result<(Vec<TransferColumn>, Vec<Vec<Cell>>)> {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: coll(name), columns: Some(columns.iter().map(|c| c.to_string()).collect()), filter: filter.map(str::to_string) };
    s.read_batches(&spec, sink.clone()).await?;
    let c = std::mem::take(&mut *sink.lock().unwrap());
    Ok((c.columns, c.batches.into_iter().flat_map(|b| b.rows).collect()))
}

async fn count(db: &mongodb::Database, name: &str) -> u64 {
    db.collection::<Document>(name).count_documents(doc! {}).await.unwrap()
}

/// Requested columns: order kept, a name asked twice fills both, and a
/// field no document has is an error. A `$where` filter works without a
/// column list too.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_read_columns_and_filters() {
    for srv in servers() {
        let db = raw(&srv).await;
        db.collection::<Document>("tr_cols").drop().await.ok();
        let docs: Vec<Document> = (1..=3_i32).map(|n| doc! { "_id": format!("k{n}"), "n": n, "t": format!("t{n}") }).collect();
        db.collection::<Document>("tr_cols").insert_many(docs).await.unwrap();
        db.collection::<Document>("tr_cols").insert_one(doc! { "_id": "rare", "only_later": true }).await.unwrap();
        let mut s = session(&srv).await;

        let (cols, rows) = read_cols(&mut s, "tr_cols", &["n", "_id", "n", "t"], Some("{ n: 1 }")).await.unwrap();
        assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["n", "_id", "n", "t"]);
        assert_eq!(rows, vec![vec![Cell::Int(1), Cell::Text("k1".into()), Cell::Int(1), Cell::Text("t1".into())]]);

        let e = read_cols(&mut s, "tr_cols", &["n", "nope"], None).await.unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("nope")), "{e:?}");
        // A field that exists somewhere (not in the filtered rows) is a column.
        let (_, rows) = read_cols(&mut s, "tr_cols", &["_id", "only_later"], Some("{ n: 2 }")).await.unwrap();
        assert_eq!(rows, vec![vec![Cell::Text("k2".into()), Cell::Null]]);

        if !srv.ferret {
            // FerretDB has no `$where`.
            let (cols, _, n) = read(&mut s, "tr_cols", Some("{ $where: 'this.n > 1' }")).await;
            assert_eq!(n, 2);
            assert!(cols.iter().any(|c| c.name == "t"), "{cols:?}");
        }
        db.collection::<Document>("tr_cols").drop().await.ok();
    }
}

/// Between MongoDB collections the direct copy keeps what rows can't: a
/// string `_id` that looks like an ObjectId, int and long in one field,
/// explicit nulls, decimals with any exponent. With a column list it
/// renames, reorders and filters.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_native_copy_is_lossless() {
    for srv in servers() {
        let db = raw(&srv).await;
        for c in ["tr_nsrc", "tr_ndst", "tr_nsub", "tr_nbulk"] {
            db.collection::<Document>(c).drop().await.ok();
        }
        let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let docs = vec![
            doc! { "_id": "65a1b2c3d4e5f60718293a4c", "ref": "65a1b2c3d4e5f60718293a4d", "num": 5_i64, "nul": Bson::Null, "dec": Decimal128::from_str("1E+100").unwrap() },
            doc! { "_id": oid, "ref": oid, "num": 5_i32, "dec": Decimal128::from_str("1.50").unwrap() },
            doc! { "_id": 7_i64, "num": 1_i64 << 40, "nested": { "b": 1_i64, "a": Bson::Null } },
        ];
        db.collection::<Document>("tr_nsrc").insert_many(docs).await.unwrap();
        let (mut s, mut t) = (session(&srv).await, session(&srv).await);
        let d = driver(&srv);
        assert!(d.supports_native_copy(d.info().id));

        let copy = CopySpec { source: ReadSpec { table: coll("tr_nsrc"), columns: None, filter: None }, target: load_spec("tr_ndst", vec![], 1) };
        assert_eq!(d.copy_native(&mut *s, &mut *t, &copy, &|_| {}).await.unwrap(), 3);
        assert_eq!(all(&db, "tr_nsrc").await, all(&db, "tr_ndst").await, "{}", srv.url);
        let string_id = db.collection::<Document>("tr_ndst").find_one(doc! { "_id": "65a1b2c3d4e5f60718293a4c" }).await.unwrap().unwrap();
        assert_eq!(string_id.get("ref"), Some(&Bson::String("65a1b2c3d4e5f60718293a4d".into())));
        assert_eq!(string_id.get("num"), Some(&Bson::Int64(5)));
        assert_eq!(string_id.get("nul"), Some(&Bson::Null));

        // Subset, renamed, reordered, filtered.
        let copy = CopySpec {
            source: ReadSpec { table: coll("tr_nsrc"), columns: Some(vec!["num".into(), "_id".into()]), filter: Some("{ num: { $gte: 5 } }".into()) },
            target: load_spec("tr_nsub", vec!["cantidad".into(), "_id".into()], 1),
        };
        assert_eq!(d.copy_native(&mut *s, &mut *t, &copy, &|_| {}).await.unwrap(), 3);
        let got = db.collection::<Document>("tr_nsub").find_one(doc! { "_id": oid }).await.unwrap().unwrap();
        assert_eq!(got, doc! { "cantidad": 5_i32, "_id": oid });
        let missing = CopySpec { source: ReadSpec { table: coll("tr_nsrc"), columns: Some(vec!["nope".into()]), filter: None }, target: load_spec("tr_nsub", vec![], 1) };
        assert!(d.copy_native(&mut *s, &mut *t, &missing, &|_| {}).await.is_err());

        // By rows, the mixed columns can't be told apart: refused, not guessed.
        let (cols, batches, _) = read(&mut s, "tr_nsrc", None).await;
        let names = cols.iter().map(|c| c.name.clone()).collect();
        let e = s.bulk_load(&load_spec("tr_nbulk", names, 1), &cols, &mut Batches(batches.into()), &|_| {}).await.unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("mezcla")), "{e:?}");
        assert_eq!(count(&db, "tr_nbulk").await, 0);
        for c in ["tr_nsrc", "tr_ndst", "tr_nsub", "tr_nbulk"] {
            db.collection::<Document>(c).drop().await.ok();
        }
    }
}

/// Rows from another engine: a string `_id` stays a string, JSON keeps its
/// `$`-keys and exact numbers, and a decimal past 34 digits fails the load
/// instead of turning into text.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_rows_from_other_engines() {
    for srv in servers() {
        let db = raw(&srv).await;
        db.collection::<Document>("tr_other").drop().await.ok();
        let mut s = session(&srv).await;
        let cols = vec![
            TransferColumn { name: "_id".into(), type_name: "varchar(24)".into(), nullable: false },
            TransferColumn { name: "doc".into(), type_name: "jsonb".into(), nullable: true },
            TransferColumn { name: "n".into(), type_name: "numeric".into(), nullable: true },
        ];
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let row = |id: &str, json: &str, n: &str| vec![Cell::Text(id.into()), Cell::Json(json.into()), Cell::Decimal(n.into())];
        let good = vec![row("65a1b2c3d4e5f60718293a4c", r#"{"a":18446744073709551615,"when":{"$date":"2024-01-01T00:00:00Z"}}"#, "1.5")];
        let n = s.bulk_load(&load_spec("tr_other", names.clone(), 1), &cols, &mut Batches(vec![RowBatch { rows: good, bytes: 0 }].into()), &|_| {}).await.unwrap();
        assert_eq!(n, 1);
        // Decoded field by field (serde would take `{ $date: … }` for a date).
        let got = db.collection::<mongodb::bson::RawDocumentBuf>("tr_other").find_one(doc! {}).await.unwrap().unwrap();
        let got = Document::try_from(got.as_ref()).unwrap();
        assert_eq!(got.get("_id"), Some(&Bson::String("65a1b2c3d4e5f60718293a4c".into())));
        let inner = got.get_document("doc").unwrap_or_else(|_| panic!("{}: {got:?}", srv.url));
        assert_eq!(inner.get("a"), Some(&Bson::Decimal128(Decimal128::from_str("18446744073709551615").unwrap())));
        assert_eq!(inner.get_document("when").unwrap().get("$date"), Some(&Bson::String("2024-01-01T00:00:00Z".into())));

        // Read back by rows, `{ $date: "…" }` would be Extended JSON for a
        // date: refused (the direct copy keeps it).
        let e = read_cols(&mut s, "tr_other", &["doc"], None).await.unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("«$date»") && m.contains("copia directa")), "{e:?}");

        db.collection::<Document>("tr_other").drop().await.ok();
        let bad = vec![row("x1", "{}", "1"), row("x2", "{}", "99999999999999999999999999999999999999")];
        let e = s.bulk_load(&load_spec("tr_other", names, 1), &cols, &mut Batches(vec![RowBatch { rows: bad, bytes: 0 }].into()), &|_| {}).await.unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("Fila 2") && m.contains("34")), "{e:?}");
        assert_eq!(count(&db, "tr_other").await, 0);
        db.collection::<Document>("tr_other").drop().await.ok();
    }
}

/// What rows can't hold as it is fails the read, naming the document,
/// instead of changing on the way: stored subdocuments whose `$`-keys
/// look like Extended JSON, and repeated keys (which the server keeps).
/// The direct copy keeps both.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_rows_refuse_what_they_cant_hold() {
    use mongodb::bson::RawDocumentBuf;
    for srv in servers() {
        let db = raw(&srv).await;
        for c in ["tr_wrap", "tr_dupsrc", "tr_dupdst"] {
            db.collection::<Document>(c).drop().await.ok();
        }
        let mut s = session(&srv).await;
        let d = driver(&srv);

        let wrap = doc! { "_id": 1, "o": { "$date": "2024-01-01T00:00:00Z" }, "p": { "$oid": "65a1b2c3d4e5f60718293a4b" }, "q": { "$numberLong": "7" } };
        match db.collection::<Document>("tr_wrap").insert_one(wrap).await {
            Ok(_) => {
                for (col, key) in [("o", "$date"), ("p", "$oid"), ("q", "$numberLong")] {
                    let e = read_cols(&mut s, "tr_wrap", &["_id", col], None).await.unwrap_err();
                    assert!(matches!(&e, Error::Unsupported(m) if m.contains("_id 1") && m.contains(&format!("«{col}»")) && m.contains(&format!("«{key}»"))), "{}: {e:?}", srv.url);
                }
                let e = read_cols(&mut s, "tr_wrap", &["_id"], None).await;
                assert!(e.is_ok(), "{}: {e:?}", srv.url);
            }
            Err(e) => eprintln!("{}: the server refuses `$`-keys ({e})", srv.url),
        }

        // { _id: 1, n: { x: 1, x: 2 }, t: 10, t: 20 }, as raw BSON.
        let mut n = RawDocumentBuf::new();
        n.append("x", 1);
        n.append("x", 2);
        let mut dup = RawDocumentBuf::new();
        dup.append("_id", 1);
        dup.append("n", n);
        dup.append("t", 10);
        dup.append("t", 20);
        match db.collection::<RawDocumentBuf>("tr_dupsrc").insert_one(dup).await {
            Ok(_) => {
                let e = read_cols(&mut s, "tr_dupsrc", &["_id", "t"], None).await.unwrap_err();
                assert!(matches!(&e, Error::Unsupported(m) if m.contains("_id 1 repite el campo «t»")), "{}: {e:?}", srv.url);
                let e = read_cols(&mut s, "tr_dupsrc", &["_id", "n"], None).await.unwrap_err();
                assert!(matches!(&e, Error::Unsupported(m) if m.contains("«n»") && m.contains("repite la clave «x»")), "{}: {e:?}", srv.url);
                // Without a column list too; nothing lands by rows.
                let sink = Arc::new(Mutex::new(Collect::default()));
                let spec = ReadSpec { table: coll("tr_dupsrc"), columns: None, filter: None };
                assert!(matches!(s.read_batches(&spec, sink).await, Err(Error::Unsupported(_))), "{}", srv.url);

                let mut t = session(&srv).await;
                let copy = CopySpec { source: ReadSpec { table: coll("tr_dupsrc"), columns: None, filter: None }, target: load_spec("tr_dupdst", vec![], 1) };
                assert_eq!(d.copy_native(&mut *s, &mut *t, &copy, &|_| {}).await.unwrap(), 1, "{}", srv.url);
                let got = db.collection::<RawDocumentBuf>("tr_dupdst").find_one(doc! {}).await.unwrap().unwrap();
                let keys: Vec<String> = got.iter().map(|f| f.unwrap().0.to_string()).collect();
                assert_eq!(keys, ["_id", "n", "t", "t"], "{}", srv.url);
                let inner: Vec<String> = got.get_document("n").unwrap().iter().map(|f| f.unwrap().0.to_string()).collect();
                assert_eq!(inner, ["x", "x"], "{}", srv.url);
            }
            Err(e) => eprintln!("{}: the server refuses repeated keys ({e})", srv.url),
        }
        for c in ["tr_wrap", "tr_dupsrc", "tr_dupdst"] {
            db.collection::<Document>(c).drop().await.ok();
        }
    }
}

/// `n` rows `{_id: i, v: "…"}` from `first`, in batches of 1000.
fn id_rows(first: i64, n: i64) -> Vec<RowBatch> {
    (first..first + n)
        .collect::<Vec<_>>()
        .chunks(1000)
        .map(|c| RowBatch { rows: c.iter().map(|&i| vec![Cell::Int(i), Cell::Text(format!("valor {i}"))]).collect(), bytes: 0 })
        .collect()
}

fn id_cols() -> Vec<TransferColumn> {
    vec![
        TransferColumn { name: "_id".into(), type_name: "long".into(), nullable: false },
        TransferColumn { name: "v".into(), type_name: "string".into(), nullable: true },
    ]
}

/// A source that hands its batches and then waits forever (a slow reader),
/// flagging when it starts waiting.
struct Stalls {
    batches: VecDeque<RowBatch>,
    waiting: Arc<AtomicBool>,
}

#[async_trait]
impl BatchSource for Stalls {
    async fn next(&mut self) -> Option<RowBatch> {
        match self.batches.pop_front() {
            Some(b) => Some(b),
            None => {
                self.waiting.store(true, Ordering::SeqCst);
                std::future::pending().await
            }
        }
    }
}

/// A failed load: when it returns, nothing more lands, and progress has
/// every document the server took (the failed request's others included).
/// The error is short.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_failure_mid_load() {
    for srv in servers() {
        let db = raw(&srv).await;
        db.collection::<Document>("tr_fail").drop().await.ok();
        // Rows 15000..25000 clash with what's there.
        let pre: Vec<Document> = (15_000..25_000_i64).map(|i| doc! { "_id": i }).collect();
        db.collection::<Document>("tr_fail").insert_many(pre).await.unwrap();
        let mut s = session(&srv).await;
        let names = vec!["_id".to_string(), "v".to_string()];
        let seen = Mutex::new(Vec::new());
        let e = s
            .bulk_load(&load_spec("tr_fail", names, 5_000), &id_cols(), &mut Batches(id_rows(0, 40_000).into()), &|n| seen.lock().unwrap().push(n))
            .await
            .unwrap_err();
        let text = e.to_string();
        assert!(text.len() < 600 && text.contains("11000") && text.contains("fila 15001"), "{} bytes: {}", text.len(), &text[..text.len().min(600)]);
        let after = count(&db, "tr_fail").await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(count(&db, "tr_fail").await, after, "{}: documents landed after the load returned", srv.url);
        let seen = seen.into_inner().unwrap();
        // The clashing requests still insert their other documents; the
        // ones not yet sent when the first failure came back never go.
        assert!((30_000..=40_000).contains(&after), "{}: {after}", srv.url);
        assert_eq!(seen.last().copied(), Some(after - 10_000), "{}: progress {seen:?}", srv.url);
        db.collection::<Document>("tr_fail").drop().await.ok();
    }
}

/// Committed documents show in progress while the source is slow, and a
/// load dropped mid-way (a cancel) leaves nothing running: a table emptied
/// right after stays empty.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_cancel_mid_load() {
    let panics = Arc::new(AtomicUsize::new(0));
    let hook_panics = panics.clone();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        hook_panics.fetch_add(1, Ordering::SeqCst);
        default_hook(info);
    }));
    for srv in servers() {
        let db = raw(&srv).await;
        let names = vec!["_id".to_string(), "v".to_string()];

        // Progress while the source waits.
        db.collection::<Document>("tr_cancel").drop().await.ok();
        let mut s = session(&srv).await;
        let waiting = Arc::new(AtomicBool::new(false));
        let reported = Arc::new(AtomicU64::new(0));
        let r = reported.clone();
        let progress = move |n: u64| r.store(n, Ordering::SeqCst);
        let mut source = Stalls { batches: id_rows(0, 40_000).into(), waiting: waiting.clone() };
        let spec = load_spec("tr_cancel", names.clone(), 10_000);
        let cols = id_cols();
        let load = s.bulk_load(&spec, &cols, &mut source, &progress);
        let r = reported.clone();
        tokio::select! {
            out = load => panic!("the load ended: {out:?}"),
            _ = async { while r.load(Ordering::SeqCst) < 40_000 { tokio::time::sleep(Duration::from_millis(10)).await } } => {}
            _ = tokio::time::sleep(Duration::from_secs(30)) => panic!("{}: progress stayed at {}", srv.url, reported.load(Ordering::SeqCst)),
        }
        assert_eq!(count(&db, "tr_cancel").await, 40_000);
        drop(s);

        // Dropped with requests in flight, then emptied: stays empty.
        for round in 0..6 {
            db.collection::<Document>("tr_cancel").drop().await.ok();
            let mut s = session(&srv).await;
            let waiting = Arc::new(AtomicBool::new(false));
            let mut source = Stalls { batches: id_rows(0, 40_000).into(), waiting: waiting.clone() };
            let load = s.bulk_load(&spec, &cols, &mut source, &|_| {});
            tokio::select! {
                out = load => panic!("the load ended: {out:?}"),
                _ = async { while !waiting.load(Ordering::SeqCst) { tokio::time::sleep(Duration::from_millis(1)).await } } => {}
            }
            // The load is dropped here.
            let at_drop = count(&db, "tr_cancel").await;
            db.collection::<Document>("tr_cancel").delete_many(doc! {}).await.unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
            let later = count(&db, "tr_cancel").await;
            println!("{}: round {round}: {at_drop} at the drop, {later} 3 s after emptying", srv.url);
            assert_eq!(later, 0, "{}: documents landed after the drop", srv.url);
        }
        db.collection::<Document>("tr_cancel").drop().await.ok();
    }
    let _ = std::panic::take_hook();
    assert_eq!(panics.load(Ordering::SeqCst), 0, "a task panicked");
}

/// Resident memory while loading 300 rows of 1 MiB each (made on demand,
/// so the source holds almost nothing).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_memory_is_bounded() {
    fn rss_kib() -> u64 {
        let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
    }
    struct Wide(usize);
    #[async_trait]
    impl BatchSource for Wide {
        async fn next(&mut self) -> Option<RowBatch> {
            if self.0 == 0 {
                return None;
            }
            self.0 -= 1;
            let rows = (0..2).map(|i| vec![Cell::Int((self.0 * 2 + i) as i64), Cell::Bytes(vec![(self.0 % 251) as u8; 1024 * 1024])]).collect();
            Some(RowBatch { rows, bytes: 2 * 1024 * 1024 })
        }
    }
    let srv = &servers()[0];
    let db = raw(srv).await;
    db.collection::<Document>("tr_mem").drop().await.ok();
    let mut s = session(srv).await;
    let cols = vec![
        TransferColumn { name: "_id".into(), type_name: "bigint".into(), nullable: false },
        TransferColumn { name: "b".into(), type_name: "bytea".into(), nullable: true },
    ];
    let names = cols.iter().map(|c| c.name.clone()).collect();
    let base = rss_kib();
    let peak = Arc::new(AtomicU64::new(base));
    let stop = Arc::new(AtomicBool::new(false));
    let (p, st) = (peak.clone(), stop.clone());
    let watcher = std::thread::spawn(move || {
        while !st.load(Ordering::SeqCst) {
            p.fetch_max(rss_kib(), Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let n = s.bulk_load(&load_spec("tr_mem", names, 100_000), &cols, &mut Wide(150), &|_| {}).await.unwrap();
    stop.store(true, Ordering::SeqCst);
    watcher.join().unwrap();
    assert_eq!(n, 300);
    let grew = (peak.load(Ordering::SeqCst) - base) / 1024;
    println!("rss {} MiB before, peak +{grew} MiB", base / 1024);
    assert!(grew < 48, "resident memory grew {grew} MiB");
    db.collection::<Document>("tr_mem").drop().await.ok();
}

fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

/// `fut`'s output and how many MiB the resident memory grew at its peak.
async fn peak_growth<F: std::future::Future>(fut: F) -> (F::Output, u64) {
    let base = rss_kib();
    let peak = Arc::new(AtomicU64::new(base));
    let stop = Arc::new(AtomicBool::new(false));
    let (p, st) = (peak.clone(), stop.clone());
    let watcher = std::thread::spawn(move || {
        while !st.load(Ordering::SeqCst) {
            p.fetch_max(rss_kib(), Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let out = fut.await;
    stop.store(true, Ordering::SeqCst);
    watcher.join().unwrap();
    (out, (peak.load(Ordering::SeqCst) - base) / 1024)
}

/// 150 documents of about 1 MiB (a string, a binary or a subdocument),
/// written one at a time so the test itself holds little.
async fn seed_wide(db: &mongodb::Database, name: &str) {
    db.collection::<Document>(name).drop().await.ok();
    for i in 0..150_i64 {
        let big = "x".repeat(1024 * 1024 - 100);
        let d = match i % 3 {
            0 => doc! { "_id": i, "s": big },
            1 => doc! { "_id": i, "b": Binary { subtype: BinarySubtype::Generic, bytes: big.into_bytes() } },
            _ => doc! { "_id": i, "o": { "inner": big } },
        };
        db.collection::<Document>(name).insert_one(d).await.unwrap();
    }
}

/// Counts rows and drops them.
#[derive(Default)]
struct Drain(u64);

impl BatchSink for Drain {
    fn begin(&mut self, _: &[TransferColumn]) -> io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.0 += batch.rows.len() as u64;
        Ok(())
    }
}

/// Reading 150 documents of 1 MiB (column types sampled from them) keeps
/// resident memory near the ~32 MiB in flight. Run alone, in its own
/// process, for a clean measure.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_read_memory_is_bounded() {
    let srv = &servers()[0];
    let db = raw(srv).await;
    seed_wide(&db, "tr_rmem").await;
    let mut s = session(srv).await;
    for columns in [None, Some(vec!["_id".to_string(), "s".into(), "b".into(), "o".into()])] {
        let sink = Arc::new(Mutex::new(Drain::default()));
        let spec = ReadSpec { table: coll("tr_rmem"), columns: columns.clone(), filter: None };
        let (n, grew) = peak_growth(s.read_batches(&spec, sink.clone())).await;
        assert_eq!(n.unwrap(), 150);
        println!("read_batches (columns {columns:?}): peak +{grew} MiB");
        assert!(grew < 48, "resident memory grew {grew} MiB");
    }
    db.collection::<Document>("tr_rmem").drop().await.ok();
}

/// The same for the direct copy, reading and writing.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_native_copy_memory_is_bounded() {
    let srv = &servers()[0];
    let db = raw(srv).await;
    seed_wide(&db, "tr_cmem").await;
    let (mut s, mut t) = (session(srv).await, session(srv).await);
    let d = driver(srv);
    for (i, columns) in [None, Some(vec!["_id".to_string(), "s".into(), "o".into()])].into_iter().enumerate() {
        let target = format!("tr_cmem_dst{i}");
        db.collection::<Document>(&target).drop().await.ok();
        let copy = CopySpec { source: ReadSpec { table: coll("tr_cmem"), columns: columns.clone(), filter: None }, target: load_spec(&target, vec![], 10_000) };
        let (n, grew) = peak_growth(d.copy_native(&mut *s, &mut *t, &copy, &|_| {})).await;
        assert_eq!(n.unwrap(), 150);
        println!("copy_native (columns {columns:?}): peak +{grew} MiB");
        assert!(grew < 48, "resident memory grew {grew} MiB");
        db.collection::<Document>(&target).drop().await.ok();
    }
    db.collection::<Document>("tr_cmem").drop().await.ok();
}

/// Columns with no type (DynamoDB's, a copy's fallback) are not MongoDB's:
/// their JSON keeps `$`-keys as plain keys and numbers exact.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_untyped_columns_load_plain_json() {
    for srv in servers() {
        let db = raw(&srv).await;
        db.collection::<Document>("tr_untyped").drop().await.ok();
        let mut s = session(&srv).await;
        let cols = vec![
            TransferColumn { name: "_id".into(), type_name: String::new(), nullable: false },
            TransferColumn { name: "doc".into(), type_name: String::new(), nullable: true },
        ];
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let rows = [
            r#"{"when":{"$date":"2024-01-01T00:00:00Z"}}"#,
            r#"{"a":18446744073709551615}"#,
            r#"{"y":12345678901234567890123}"#,
            r#"{"o":{"$oid":"65a1b2c3d4e5f60718293a4b"}}"#,
            r#"{"p":{"$numberDecimal":"x"},"k":1}"#,
        ];
        let batch = RowBatch { rows: rows.iter().enumerate().map(|(i, j)| vec![Cell::Int(i as i64), Cell::Json(j.to_string())]).collect(), bytes: 0 };
        let n = s.bulk_load(&load_spec("tr_untyped", names, 1), &cols, &mut Batches(vec![batch].into()), &|_| {}).await.unwrap();
        assert_eq!(n, 5, "{}", srv.url);
        let got = |i: i32| {
            let db = db.clone();
            async move {
                let raw = db.collection::<mongodb::bson::RawDocumentBuf>("tr_untyped").find_one(doc! { "_id": i }).await.unwrap().unwrap();
                Document::try_from(raw.as_ref()).unwrap().get_document("doc").unwrap().clone()
            }
        };
        assert_eq!(got(0).await, doc! { "when": { "$date": "2024-01-01T00:00:00Z" } }, "{}", srv.url);
        assert_eq!(got(1).await, doc! { "a": Decimal128::from_str("18446744073709551615").unwrap() }, "{}", srv.url);
        assert_eq!(got(2).await, doc! { "y": Decimal128::from_str("12345678901234567890123").unwrap() }, "{}", srv.url);
        assert_eq!(got(3).await, doc! { "o": { "$oid": "65a1b2c3d4e5f60718293a4b" } }, "{}", srv.url);
        assert_eq!(got(4).await, doc! { "p": { "$numberDecimal": "x" }, "k": 1_i32 }, "{}", srv.url);
        db.collection::<Document>("tr_untyped").drop().await.ok();
    }
}

/// A field that only documents past the type sample have still gets its
/// type, so a load keeps its Extended JSON (dates, ObjectIds inside).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_rare_fields_keep_their_type() {
    for srv in servers() {
        let db = raw(&srv).await;
        for c in ["tr_rare", "tr_rare_dst"] {
            db.collection::<Document>(c).drop().await.ok();
        }
        let docs: Vec<Document> = (0..1_500_i64).map(|i| doc! { "_id": i }).collect();
        db.collection::<Document>("tr_rare").insert_many(docs).await.unwrap();
        let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap();
        let rare = doc! { "_id": 9_999_i64, "rare": { "when": DateTime::from_millis(5), "o": oid, "n": 1_i64 }, "big": 1_i64 << 40 };
        db.collection::<Document>("tr_rare").insert_one(rare.clone()).await.unwrap();
        let mut s = session(&srv).await;

        let (cols, batches, _) = read(&mut s, "tr_rare", None).await;
        let ty = |cols: &[TransferColumn], n: &str| cols.iter().find(|c| c.name == n).unwrap().type_name.clone();
        assert_eq!((ty(&cols, "rare"), ty(&cols, "big")), ("bson:object".into(), "bson:long".into()), "{}", srv.url);
        let (explicit, _) = read_cols(&mut s, "tr_rare", &["_id", "rare", "big"], None).await.unwrap();
        assert_eq!((ty(&explicit, "rare"), ty(&explicit, "big")), ("bson:object".into(), "bson:long".into()), "{}", srv.url);

        let (n, _) = load(&mut s, "tr_rare_dst", &cols, batches, 10_000).await;
        assert_eq!(n, 1_501);
        let got = db.collection::<Document>("tr_rare_dst").find_one(doc! { "_id": 9_999_i64 }).await.unwrap().unwrap();
        assert_eq!(got, rare, "{}", srv.url);
        for c in ["tr_rare", "tr_rare_dst"] {
            db.collection::<Document>(c).drop().await.ok();
        }
    }
}

/// JSON from engines whose types share BSON's names (`object`, `array` in
/// Elasticsearch, Snowflake…) is plain JSON: `$`-keys are keys and numbers
/// stay exact. Only this crate's own reads (`bson:` types) are Extended
/// JSON.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_json_from_other_engines_is_plain() {
    for srv in servers() {
        let db = raw(&srv).await;
        db.collection::<Document>("tr_plain").drop().await.ok();
        let mut s = session(&srv).await;
        let cols = vec![
            TransferColumn { name: "_id".into(), type_name: "long".into(), nullable: false },
            TransferColumn { name: "o".into(), type_name: "object".into(), nullable: true },
            TransferColumn { name: "a".into(), type_name: "array".into(), nullable: true },
            TransferColumn { name: "v".into(), type_name: "VARIANT".into(), nullable: true },
        ];
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let o = r#"{"when":{"$date":"2024-01-01T00:00:00Z"},"p":{"$numberDecimal":"x"},"big":12345678901234567890123,"i":9007199254740993}"#;
        let a = r#"[{"$oid":"65a1b2c3d4e5f60718293a4b"},18446744073709551615,0.1]"#;
        let v = r#"{"$numberLong":"5"}"#;
        let batch = RowBatch { rows: vec![vec![Cell::Int(1), Cell::Json(o.into()), Cell::Json(a.into()), Cell::Json(v.into())]], bytes: 0 };
        let n = s.bulk_load(&load_spec("tr_plain", names, 1), &cols, &mut Batches(vec![batch].into()), &|_| {}).await.unwrap();
        assert_eq!(n, 1, "{}", srv.url);
        let raw = db.collection::<mongodb::bson::RawDocumentBuf>("tr_plain").find_one(doc! {}).await.unwrap().unwrap();
        let got = Document::try_from(raw.as_ref()).unwrap();
        let want = doc! {
            "_id": 1_i64,
            "o": {
                "when": { "$date": "2024-01-01T00:00:00Z" },
                "p": { "$numberDecimal": "x" },
                "big": Decimal128::from_str("12345678901234567890123").unwrap(),
                "i": 9_007_199_254_740_993_i64,
            },
            "a": [{ "$oid": "65a1b2c3d4e5f60718293a4b" }, Decimal128::from_str("18446744073709551615").unwrap(), 0.1],
            "v": { "$numberLong": "5" },
        };
        assert_eq!(got, want, "{}", srv.url);
        db.collection::<Document>("tr_plain").drop().await.ok();

        // PostgreSQL's `json` keeps repeated keys; a document can't, so the
        // load refuses instead of keeping only the last value.
        for ty in ["jsonb", "json", "object"] {
            db.collection::<Document>("tr_dupkey").drop().await.ok();
            let cols = vec![
                TransferColumn { name: "_id".into(), type_name: "long".into(), nullable: false },
                TransferColumn { name: "j".into(), type_name: ty.into(), nullable: true },
            ];
            let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
            let batch = RowBatch { rows: vec![vec![Cell::Int(1), Cell::Json(r#"{"a":1,"a":2}"#.into())]], bytes: 0 };
            let e = s.bulk_load(&load_spec("tr_dupkey", names, 1), &cols, &mut Batches(vec![batch].into()), &|_| {}).await.unwrap_err();
            assert!(e.to_string().contains("repite la clave «a»"), "{} {ty}: {e}", srv.url);
            assert_eq!(count(&db, "tr_dupkey").await, 0, "{} {ty}", srv.url);
        }
        db.collection::<Document>("tr_dupkey").drop().await.ok();
    }
}

/// 1000 small documents first (all the type sample sees), then 200 of
/// 1 MiB: the cursor batches follow the largest document, not the
/// sample's, so reading and copying stay bounded.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_memory_with_mixed_document_sizes() {
    let srv = &servers()[0];
    let db = raw(srv).await;
    let src = db.collection::<Document>("tr_mix");
    src.drop().await.ok();
    src.insert_many((0..1_000_i64).map(|i| doc! { "_id": i, "n": i })).await.unwrap();
    for i in 0..200_i64 {
        src.insert_one(doc! { "_id": 1_000 + i, "s": "x".repeat(1024 * 1024 - 100) }).await.unwrap();
    }
    let mut s = session(srv).await;
    for columns in [None, Some(vec!["_id".to_string(), "s".into()])] {
        let sink = Arc::new(Mutex::new(Drain::default()));
        let spec = ReadSpec { table: coll("tr_mix"), columns: columns.clone(), filter: None };
        let (n, grew) = peak_growth(s.read_batches(&spec, sink.clone())).await;
        assert_eq!(n.unwrap(), 1_200);
        println!("read_batches (columns {columns:?}): peak +{grew} MiB");
        assert!(grew < 48, "read_batches: resident memory grew {grew} MiB");
    }
    let mut t = session(srv).await;
    let d = driver(srv);
    for (i, columns) in [None, Some(vec!["_id".to_string(), "s".into()])].into_iter().enumerate() {
        let target = format!("tr_mix_dst{i}");
        db.collection::<Document>(&target).drop().await.ok();
        let copy = CopySpec { source: ReadSpec { table: coll("tr_mix"), columns: columns.clone(), filter: None }, target: load_spec(&target, vec![], 10_000) };
        let (n, grew) = peak_growth(d.copy_native(&mut *s, &mut *t, &copy, &|_| {})).await;
        assert_eq!(n.unwrap(), 1_200);
        println!("copy_native (columns {columns:?}): peak +{grew} MiB");
        assert!(grew < 48, "copy_native: resident memory grew {grew} MiB");
        db.collection::<Document>(&target).drop().await.ok();
    }
    src.drop().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_benchmark() {
    for srv in servers() {
        let rows: i64 = if srv.ferret { 200_000 } else { 1_000_000 };
        let db = raw(&srv).await;
        for c in ["tr_bench", "tr_bench_dst"] {
            db.collection::<Document>(c).drop().await.ok();
        }
        let base = DateTime::from_millis(1_700_000_000_000);
        for chunk in (0..rows).collect::<Vec<_>>().chunks(20_000) {
            let docs: Vec<Document> = chunk
                .iter()
                .map(|&i| {
                    doc! {
                        "_id": ObjectId::new(), "n": i, "amount": (i as f64) / 7.0, "name": format!("nombre {i}"),
                        "when": DateTime::from_millis(base.timestamp_millis() + i * 1000), "flag": i % 2 == 0,
                        "tags": ["a", "b"], "k": (i % 1000) as i32,
                    }
                })
                .collect();
            db.collection::<Document>("tr_bench").insert_many(docs).await.unwrap();
        }

        let mut s = session(&srv).await;
        let t0 = Instant::now();
        let (cols, batches, n) = read(&mut s, "tr_bench", None).await;
        let secs = t0.elapsed().as_secs_f64();
        assert_eq!(n, rows as u64);
        println!("{}: read {rows} docs in {secs:.2} s = {:.0} rows/s (fields found by a scan)", srv.url, rows as f64 / secs);
        let sink = Arc::new(Mutex::new(Collect::default()));
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let t0 = Instant::now();
        let spec = ReadSpec { table: coll("tr_bench"), columns: Some(names), filter: None };
        assert_eq!(s.read_batches(&spec, sink).await.unwrap(), rows as u64);
        let secs = t0.elapsed().as_secs_f64();
        println!("{}: read {rows} docs in {secs:.2} s = {:.0} rows/s (fields given)", srv.url, rows as f64 / secs);

        let t0 = Instant::now();
        let (loaded, _) = load(&mut s, "tr_bench_dst", &cols, batches, LoadSpec::DEFAULT_COMMIT_ROWS).await;
        let secs = t0.elapsed().as_secs_f64();
        assert_eq!(loaded, rows as u64);
        println!("{}: load {rows} docs in {secs:.2} s = {:.0} rows/s", srv.url, rows as f64 / secs);

        let sum = |name: &'static str| {
            let db = db.clone();
            async move {
                use futures::TryStreamExt;
                let pipeline = vec![doc! { "$group": { "_id": null, "c": { "$sum": 1 }, "n": { "$sum": "$n" }, "k": { "$sum": "$k" } } }];
                let r: Vec<Document> = db.collection::<Document>(name).aggregate(pipeline).await.unwrap().try_collect().await.unwrap();
                r[0].clone()
            }
        };
        assert_eq!(sum("tr_bench").await, sum("tr_bench_dst").await);
        let one = db.collection::<Document>("tr_bench_dst").find_one(doc! { "n": 12_345_i64 }).await.unwrap().unwrap();
        assert!(matches!(one.get("n"), Some(Bson::Int64(12_345))), "{one:?}");
        assert!(matches!(one.get("k"), Some(Bson::Int32(345))), "{one:?}");
        assert!(matches!(one.get("when"), Some(Bson::DateTime(_))), "{one:?}");
        for c in ["tr_bench", "tr_bench_dst"] {
            db.collection::<Document>(c).drop().await.ok();
        }
    }
}
