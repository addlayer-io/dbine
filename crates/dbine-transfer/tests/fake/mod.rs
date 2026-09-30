//! An in-memory driver for the engine's tests: tables as rows of cells,
//! failure injection, optional bulk load and native copy, and sync by rows
//! over the in-memory tables (a deterministic FNV row hash).

#![allow(dead_code)]

use dbine_driver::transfer::{BatchSinkRef, Progress};
use dbine_driver::{
    async_trait, BatchSource, BucketSum, Buckets, Cell, ColumnInfo, ConnectionConfig, CopySpec, DbObject, DeltaDepth, DeltaResult, DeltaSpec, Driver,
    DriverInfo, Error, Family, Language, LoadSpec, ObjectRef, QueryOutcome, ReadSpec, Result, RowBatch, Session, StatementResult, TransferColumn,
};
use dbine_transfer::{Endpoints, Event, TransferJob, TransferMode};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

pub type Table = Vec<Vec<Cell>>;

/// A target write fails once the table holds `after_rows` rows.
pub struct Fault {
    pub table: String,
    pub after_rows: usize,
    pub error: fn() -> Error,
    pub times: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Native {
    Off,
    On,
    /// Declared, but the call answers `Unsupported`.
    Unsupported,
}

/// Both databases and every knob.
pub struct Fake {
    pub source: Mutex<HashMap<String, Table>>,
    pub target: Mutex<HashMap<String, Table>>,
    /// Target columns (for the pre-copy check).
    pub target_columns: Mutex<HashMap<String, Vec<ColumnInfo>>>,
    /// Target statements run, in order (`TRUNCATE t`, `POST t`, `BEFORE t`…).
    pub log: Mutex<Vec<String>>,
    pub bulk: bool,
    pub native: Native,
    pub native_allowed: bool,
    pub read_delay: Duration,
    pub write_delay: Duration,
    /// Reads wait for a permit before starting (tables held "running").
    pub gate: Option<Arc<Semaphore>>,
    pub faults: Mutex<Vec<Fault>>,
    /// The target's bulk load panics on this table.
    pub panic_on: Option<String>,
    /// `SLEEP` statements hang (a `post` cut mid-way).
    pub sleep_post: AtomicBool,
    pub reading: AtomicUsize,
    pub max_reading: AtomicUsize,
    pub started: Mutex<Vec<String>>,
    pub produced: AtomicUsize,
    pub finished: AtomicUsize,
    pub max_window: AtomicUsize,
    pub bulk_calls: AtomicUsize,
    pub native_calls: AtomicUsize,
    /// Both ends declare sync by rows.
    pub delta: bool,
    /// The target driver's id (another one: no sync by rows).
    pub target_id: &'static str,
    /// The buckets of every summary (both sides).
    pub summaries: Mutex<Vec<Buckets>>,
    /// The buckets of every `delta_apply` (empty: the whole table).
    pub delta_applies: Mutex<Vec<Vec<i64>>>,
    /// The filters the source was read with.
    pub filters: Mutex<Vec<Option<String>>>,
}

impl Default for Fake {
    fn default() -> Self {
        Fake {
            source: Mutex::default(),
            target: Mutex::default(),
            target_columns: Mutex::default(),
            log: Mutex::default(),
            bulk: true,
            native: Native::Off,
            native_allowed: false,
            read_delay: Duration::ZERO,
            write_delay: Duration::ZERO,
            gate: None,
            faults: Mutex::default(),
            panic_on: None,
            sleep_post: AtomicBool::new(false),
            reading: AtomicUsize::new(0),
            max_reading: AtomicUsize::new(0),
            started: Mutex::default(),
            produced: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            max_window: AtomicUsize::new(0),
            bulk_calls: AtomicUsize::new(0),
            native_calls: AtomicUsize::new(0),
            delta: true,
            target_id: "fake",
            summaries: Mutex::default(),
            delta_applies: Mutex::default(),
            filters: Mutex::default(),
        }
    }
}

impl Fake {
    /// A source table of `n` rows `(i, "row i")`, and an empty target one.
    pub fn table(&self, name: &str, n: usize) {
        let rows = (0..n).map(|i| vec![Cell::Int(i as i64), Cell::Text(format!("row {i}"))]).collect();
        self.source.lock().unwrap().insert(name.into(), rows);
        self.target.lock().unwrap().insert(name.into(), Vec::new());
        self.target_columns.lock().unwrap().insert(name.into(), vec![col("id", "int"), col("name", "text")]);
    }

    /// The target table as a copy of the source one.
    pub fn same_on_target(&self, name: &str) {
        let rows = self.source.lock().unwrap()[name].clone();
        self.target.lock().unwrap().insert(name.into(), rows);
    }

    /// Both tables sorted by id, for comparing.
    pub fn in_sync(&self, name: &str) -> bool {
        let sorted = |mut t: Table| {
            t.sort_by_key(|r| serde_json::to_string(r).unwrap());
            t
        };
        sorted(self.source.lock().unwrap()[name].clone()) == sorted(self.target_rows(name))
    }

    pub fn target_rows(&self, name: &str) -> Table {
        self.target.lock().unwrap().get(name).cloned().unwrap_or_default()
    }

    pub fn count(&self, stmt: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|s| *s == stmt).count()
    }

    fn fault(&self, table: &str, rows: usize) -> Result<()> {
        let mut faults = self.faults.lock().unwrap();
        if let Some(f) = faults.iter_mut().find(|f| f.table == table && f.times > 0 && rows >= f.after_rows) {
            f.times -= 1;
            return Err((f.error)());
        }
        Ok(())
    }
}

// -- sync by rows over the in-memory tables ------------------------------------------------------

/// Every fake table has these columns.
const COLUMNS: [&str; 2] = ["id", "name"];

fn index(column: &str) -> usize {
    COLUMNS.iter().position(|c| *c == column).unwrap_or_else(|| panic!("no column {column}"))
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3))
}

fn key_text(key: &[String], row: &[Cell]) -> String {
    let cells: Vec<&Cell> = key.iter().map(|k| &row[index(k)]).collect();
    serde_json::to_string(&cells).unwrap()
}

fn bucket_of(b: &Buckets, key: &[String], row: &[Cell]) -> i64 {
    match b {
        Buckets::Range { column, lo, hi, width, n } => {
            let Cell::Int(v) = row[index(column)] else { panic!("not an integer key") };
            if v < *lo {
                -1
            } else if v > *hi {
                *n as i64
            } else {
                ((v as i128 - *lo as i128) / *width as i128) as i64
            }
        }
        Buckets::Hash { n } => (fnv(key_text(key, row).as_bytes()) % n) as i64,
    }
}

fn row_hash(depth: DeltaDepth, key: &[String], row: &[Cell]) -> u64 {
    match depth {
        DeltaDepth::Keys => fnv(key_text(key, row).as_bytes()),
        _ => fnv(serde_json::to_string(row).unwrap().as_bytes()),
    }
}

/// The fake's filter: the buckets, as JSON.
type Filter = (Vec<String>, Buckets, Vec<i64>);

pub fn col(name: &str, t: &str) -> ColumnInfo {
    ColumnInfo { name: name.into(), data_type: t.into(), nullable: true, primary_key: false, auto_increment: false, default_value: None }
}

pub struct FakeDriver {
    info: DriverInfo,
    fake: Arc<Fake>,
    source: bool,
}

pub struct FakeSession {
    fake: Arc<Fake>,
    source: bool,
}

fn info(id: &'static str) -> DriverInfo {
    DriverInfo {
        id,
        name: "Fake",
        family: Family::Relational,
        language: Language::Sql,
        dialect: "standard",
        default_port: 0,
        fields: vec![],
        databases_label: "",
        has_schemas: false,
        object_kinds: vec![],
    }
}

#[async_trait]
impl Driver for FakeDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    async fn connect(&self, _: &ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
        Ok(Box::new(FakeSession { fake: self.fake.clone(), source: self.source }))
    }

    fn insert_script(&self, target: &ObjectRef, _columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        Ok(format!("INSERT {}\n{}", target.name, serde_json::to_string(rows)?))
    }

    fn supports_bulk_load(&self) -> bool {
        self.fake.bulk
    }

    fn supports_native_copy(&self, target: &str) -> bool {
        target == "fake" && self.fake.native != Native::Off
    }

    async fn copy_native(&self, source: &mut dyn Session, target: &mut dyn Session, spec: &CopySpec, progress: Progress<'_>) -> Result<u64> {
        if self.fake.native == Native::Unsupported {
            return Err(Error::Unsupported("no con estas versiones".into()));
        }
        // The driver finds its own sessions.
        let src = source.as_any().and_then(|a| a.downcast_mut::<FakeSession>()).map(|s| s.source);
        let tgt = target.as_any().and_then(|a| a.downcast_mut::<FakeSession>()).map(|s| s.source);
        assert_eq!((src, tgt), (Some(true), Some(false)));
        self.fake.native_calls.fetch_add(1, Ordering::SeqCst);
        let rows = self.fake.source.lock().unwrap().get(&spec.source.table.name).cloned().unwrap_or_default();
        let n = rows.len() as u64;
        self.fake.target.lock().unwrap().entry(spec.target.table.name.clone()).or_default().extend(rows);
        progress(n);
        Ok(n)
    }

    fn supports_delta(&self) -> bool {
        self.fake.delta
    }

    fn delta_filter(&self, spec: &DeltaSpec, buckets: &[i64]) -> Result<String> {
        let f: Filter = (spec.key.clone(), spec.buckets.clone(), buckets.to_vec());
        Ok(serde_json::to_string(&f)?)
    }
}

struct Reading<'a>(&'a Fake);

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        self.0.reading.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Session for FakeSession {
    async fn server_version(&mut self) -> Result<String> {
        Ok("fake".into())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec!["main".into()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        Ok(vec![])
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        Ok(self.fake.target_columns.lock().unwrap().get(&obj.name).cloned().unwrap_or_default())
    }

    async fn definition(&mut self, _: &ObjectRef) -> Result<Option<String>> {
        Ok(None)
    }

    fn browse_query(&self, obj: &ObjectRef, _limit: u32) -> String {
        format!("PROBE {}", obj.name)
    }

    async fn execute(&mut self, text: &str, _max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        assert!(!self.source, "the source is only read: {text}");
        let f = &self.fake;
        let (head, body) = text.split_once('\n').unwrap_or((text, ""));
        let (verb, table) = head.split_once(' ').unwrap_or((head, ""));
        match verb {
            "TRUNCATE" => f.target.lock().unwrap().entry(table.into()).or_default().clear(),
            "PROBE" => {
                let n = f.target.lock().unwrap().get(table).map_or(0, |t| t.len());
                out.results.push(StatementResult { total_rows: n as u64, ..Default::default() });
            }
            "INSERT" => {
                let have = f.target.lock().unwrap().get(table).map_or(0, |t| t.len());
                f.fault(table, have)?;
                let rows: Vec<Vec<serde_json::Value>> = serde_json::from_str(body)?;
                let rows = rows.iter().map(|r| r.iter().map(Cell::from_json).collect()).collect::<Vec<_>>();
                f.target.lock().unwrap().entry(table.into()).or_default().extend(rows);
                return Ok(());
            }
            "SLEEP" => {
                if f.sleep_post.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_secs(600)).await;
                }
            }
            "FAIL" => {
                out.error = Some("syntax error".into());
                return Ok(());
            }
            _ => {}
        }
        f.log.lock().unwrap().push(head.to_string());
        Ok(())
    }

    async fn read_batches(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        assert!(self.source);
        let f = self.fake.clone();
        let name = spec.table.name.clone();
        f.started.lock().unwrap().push(name.clone());
        let now = f.reading.fetch_add(1, Ordering::SeqCst) + 1;
        f.max_reading.fetch_max(now, Ordering::SeqCst);
        let _reading = Reading(&f);
        if let Some(g) = &f.gate {
            g.acquire().await.unwrap().forget();
        }
        f.filters.lock().unwrap().push(spec.filter.clone());
        let mut rows = f.source.lock().unwrap().get(&name).cloned().unwrap_or_default();
        if let Some(filter) = &spec.filter {
            let (key, buckets, only): Filter = serde_json::from_str(filter)?;
            rows.retain(|r| only.contains(&bucket_of(&buckets, &key, r)));
        }
        let columns = vec![
            TransferColumn { name: "id".into(), type_name: "int".into(), nullable: true },
            TransferColumn { name: "name".into(), type_name: "text".into(), nullable: true },
        ];
        sink.lock().unwrap().begin(&columns)?;
        for chunk in rows.chunks(1000) {
            let batch = RowBatch { rows: chunk.to_vec(), bytes: chunk.iter().flatten().map(Cell::size).sum() };
            sink.lock().unwrap().batch(batch)?;
            let produced = f.produced.fetch_add(1, Ordering::SeqCst) + 1;
            let window = produced - f.finished.load(Ordering::SeqCst);
            f.max_window.fetch_max(window, Ordering::SeqCst);
            if !f.read_delay.is_zero() {
                tokio::time::sleep(f.read_delay).await;
            }
        }
        Ok(rows.len() as u64)
    }

    async fn bulk_load(&mut self, spec: &LoadSpec, columns: &[TransferColumn], source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
        assert!(!self.source);
        assert!(!columns.is_empty());
        let f = self.fake.clone();
        let table = spec.table.name.clone();
        f.bulk_calls.fetch_add(1, Ordering::SeqCst);
        if f.panic_on.as_deref() == Some(table.as_str()) {
            panic!("boom en {table}");
        }
        // Rows since the last commit: a dropped load rolls them back.
        let mut staged: Table = Vec::new();
        let mut committed = 0u64;
        while let Some(batch) = source.next().await {
            if !f.write_delay.is_zero() {
                tokio::time::sleep(f.write_delay).await;
            }
            let have = f.target.lock().unwrap().get(&table).map_or(0, |t| t.len()) + staged.len();
            f.fault(&table, have)?;
            staged.extend(batch.rows);
            f.finished.fetch_add(1, Ordering::SeqCst);
            if staged.len() as u64 >= spec.commit_rows {
                committed += staged.len() as u64;
                f.target.lock().unwrap().entry(table.clone()).or_default().append(&mut staged);
                progress(committed);
            }
        }
        committed += staged.len() as u64;
        f.target.lock().unwrap().entry(table.clone()).or_default().append(&mut staged);
        progress(committed);
        Ok(committed)
    }

    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        Some(self)
    }

    async fn key_range(&mut self, table: &ObjectRef, column: &str) -> Result<Option<(i64, i64, u64)>> {
        let side = if self.source { &self.fake.source } else { &self.fake.target };
        let rows = side.lock().unwrap().get(&table.name).cloned().unwrap_or_default();
        let i = index(column);
        let mut values = Vec::with_capacity(rows.len());
        for r in &rows {
            match r[i] {
                Cell::Int(v) => values.push(v),
                _ => return Err(Error::Query(format!("{column} no es entera"))),
            }
        }
        Ok(values.iter().min().map(|lo| (*lo, *values.iter().max().unwrap(), rows.len() as u64)))
    }

    async fn delta_summary(&mut self, spec: &DeltaSpec) -> Result<Vec<BucketSum>> {
        let side = if self.source { &self.fake.source } else { &self.fake.target };
        let rows = side.lock().unwrap().get(&spec.table.name).cloned().unwrap_or_default();
        self.fake.summaries.lock().unwrap().push(spec.buckets.clone());
        let mut sums: BTreeMap<i64, (u64, u128)> = BTreeMap::new();
        for r in &rows {
            let e = sums.entry(bucket_of(&spec.buckets, &spec.key, r)).or_default();
            e.0 += 1;
            e.1 += row_hash(spec.depth, &spec.key, r) as u128;
        }
        Ok(sums.into_iter().map(|(bucket, (rows, sum))| BucketSum { bucket, rows, sum: sum.to_string() }).collect())
    }

    async fn delta_apply(
        &mut self,
        spec: &DeltaSpec,
        buckets: &[i64],
        columns: &[TransferColumn],
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
    ) -> Result<DeltaResult> {
        assert!(!self.source, "the source is only read");
        assert!(!columns.is_empty());
        let f = self.fake.clone();
        let table = spec.table.name.clone();
        f.delta_applies.lock().unwrap().push(buckets.to_vec());
        let mut incoming: Table = Vec::new();
        while let Some(batch) = source.next().await {
            incoming.extend(batch.rows);
            progress(incoming.len() as u64);
        }
        // One transaction: a failure leaves the table as it was.
        let have = f.target.lock().unwrap().get(&table).map_or(0, |t| t.len());
        f.fault(&table, have)?;
        let mut target = f.target.lock().unwrap();
        let rows = target.entry(table).or_default();
        let (scope, mut keep): (Table, Table) =
            std::mem::take(rows).into_iter().partition(|r| buckets.is_empty() || buckets.contains(&bucket_of(&spec.buckets, &spec.key, r)));
        let mut old: HashMap<String, Vec<Cell>> = scope.into_iter().map(|r| (key_text(&spec.key, &r), r)).collect();
        let mut result = DeltaResult::default();
        for r in &incoming {
            match old.remove(&key_text(&spec.key, r)) {
                None => result.inserted += 1,
                Some(o) if &o != r => result.updated += 1,
                Some(_) => {}
            }
        }
        result.deleted = old.len() as u64;
        keep.extend(incoming);
        *rows = keep;
        Ok(result)
    }
}

/// The app's side: both drivers over one [`Fake`].
pub struct FakeEndpoints {
    pub fake: Arc<Fake>,
    source: Arc<dyn Driver>,
    target: Arc<dyn Driver>,
}

impl FakeEndpoints {
    pub fn new(fake: Arc<Fake>) -> Arc<Self> {
        let source: Arc<dyn Driver> = Arc::new(FakeDriver { info: info("fake"), fake: fake.clone(), source: true });
        let target: Arc<dyn Driver> = Arc::new(FakeDriver { info: info(fake.target_id), fake: fake.clone(), source: false });
        Arc::new(FakeEndpoints { fake, source, target })
    }
}

#[async_trait]
impl Endpoints for FakeEndpoints {
    fn source_driver(&self) -> Arc<dyn Driver> {
        self.source.clone()
    }
    fn target_driver(&self) -> Arc<dyn Driver> {
        self.target.clone()
    }
    async fn open_source(&self) -> Result<Box<dyn Session>> {
        Ok(Box::new(FakeSession { fake: self.fake.clone(), source: true }))
    }
    async fn open_target(&self) -> Result<Box<dyn Session>> {
        Ok(Box::new(FakeSession { fake: self.fake.clone(), source: false }))
    }
    fn native_copy_allowed(&self) -> bool {
        self.fake.native_allowed && self.source.supports_native_copy(self.target.info().id)
    }
}

/// A job copying table `name` into the same name.
pub fn job(name: &str, estimate: Option<u64>) -> TransferJob {
    let table = ObjectRef { kind: "table".into(), schema: None, name: name.into() };
    TransferJob {
        name: name.into(),
        source: ReadSpec { table: table.clone(), columns: None, filter: None },
        target: LoadSpec {
            table,
            columns: vec!["id".into(), "name".into()],
            table_lock: false,
            keep_identity: false,
            commit_rows: 0,
            commit_bytes: 0,
        },
        row_estimate: estimate,
        truncate: Some(format!("TRUNCATE {name}")),
        empty_first: false,
        before: String::new(),
        after: String::new(),
        post: vec![],
        preexisting: false,
        expected_columns: vec![],
        mode: TransferMode::Copy,
    }
}

/// A job syncing table `name` by rows on `key`.
pub fn delta_job(name: &str, key: &str, estimate: Option<u64>) -> TransferJob {
    TransferJob { mode: TransferMode::Delta { key: vec![key.into()], depth: DeltaDepth::Full, max_cores: 0 }, ..job(name, estimate) }
}

/// Collects a run's events.
#[derive(Clone, Default)]
pub struct Events(pub Arc<Mutex<Vec<Event>>>);

impl Events {
    pub fn sink(&self) -> impl Fn(Event) + Send + Sync + 'static {
        let v = self.0.clone();
        move |e| v.lock().unwrap().push(e)
    }
    pub fn all(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
}

/// A fresh state file.
pub fn state_path(tag: &str) -> std::path::PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let p = std::env::temp_dir().join(format!(
        "dbine-transfer-{tag}-{}-{}.sqlite",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
    }
    p
}

/// Wait (up to 10 s) until `cond` holds.
pub async fn until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..1000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}
