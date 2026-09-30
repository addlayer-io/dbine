//! Bulk transfer through a driver host: batched reads come back in order,
//! bounded by the batch window, and the calls a driver doesn't have answer
//! as unsupported instead of breaking the channel.

use dbine_driver::transfer::{BatchSink, BatchSinkRef, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome};
use dbine_plugin::{DriverMeta, Launcher, RemoteDriver};
use std::sync::{Arc, Mutex};

const EXE: &str = env!("CARGO_BIN_EXE_dbine-plugin-host");

fn sqlite_remote() -> Arc<dyn Driver> {
    let (_, drivers) = dbine_drivers::packages().into_iter().find(|(p, _)| *p == "sqlite").expect("package");
    let local = drivers.into_iter().find(|d| d.info().id == "sqlite").expect("driver");
    let meta = DriverMeta::of("sqlite", local.as_ref());
    let launcher = Launcher::at("sqlite", EXE.into(), vec!["--package".into(), "sqlite".into()], None);
    Arc::new(RemoteDriver::new(meta, launcher))
}

#[derive(Default)]
struct Collect {
    columns: Vec<String>,
    batches: Vec<usize>,
    rows: usize,
    slow: bool,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.columns = columns.iter().map(|c| c.name.clone()).collect();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        if self.slow {
            // A consumer slower than the host: the window holds the host back.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.batches.push(b.len());
        self.rows += b.len();
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn batched_reads_through_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: dir.path().join("t.db").display().to_string(), ..Default::default() };
    let driver = sqlite_remote();
    let mut s = driver.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 25000)
         INSERT INTO t SELECT i, 'fila ' || i FROM n;",
        10,
        &mut out,
    )
    .await
    .unwrap();

    for slow in [false, true] {
        let collect = Arc::new(Mutex::new(Collect { slow, ..Default::default() }));
        let sink: BatchSinkRef = collect.clone();
        let spec = ReadSpec { table: ObjectRef { kind: "table".into(), schema: None, name: "t".into() }, columns: None, filter: None };
        let n = s.read_batches(&spec, sink).await.unwrap();
        let c = collect.lock().unwrap();
        assert_eq!(n, 25_000);
        assert_eq!(c.rows, 25_000);
        assert_eq!(c.columns, vec!["id", "name"]);
        assert!(c.batches.iter().all(|&b| b <= 1_000), "{:?}", c.batches);
    }

}

/// The app dies while a batched read waits for its acks (its window full,
/// the target behind): the host must still end, not stay behind holding
/// the connection and the read open on the server.
#[test]
fn the_host_ends_when_the_app_dies_mid_read() {
    use dbine_plugin::proto::{read_frame, write_frame, Call, FromHost, Hello, Ready, Reply, ToHost, BATCH_WINDOW, PROTOCOL};
    use std::io::{BufReader, BufWriter, Write};
    use std::process::{ChildStdout, Command, Stdio};
    use std::time::{Duration, Instant};

    fn call(w: &mut impl Write, id: u64, call: Call) {
        write_frame(w, &ToHost::Call { id, call }).unwrap();
    }
    fn reply(r: &mut BufReader<ChildStdout>, id: u64) -> Reply {
        loop {
            if let FromHost::Reply { id: got, result } = read_frame::<FromHost>(r).unwrap().expect("frame") {
                if got == id {
                    return result.expect("reply");
                }
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(EXE).args(["--package", "sqlite"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut input = BufWriter::new(child.stdin.take().unwrap());
    let mut output = BufReader::new(child.stdout.take().unwrap());
    write_frame(&mut input, &Hello { protocol: PROTOCOL, version: "test".into(), components_dir: None }).unwrap();
    let _: Ready = read_frame(&mut output).unwrap().expect("ready");

    let config = ConnectionConfig { driver: "sqlite".into(), host: dir.path().join("t.db").display().to_string(), ..Default::default() };
    call(&mut input, 1, Call::Connect { driver: "sqlite".into(), config, database: None });
    let Reply::Session { id: session, .. } = reply(&mut output, 1) else { panic!("no session") };
    let fill = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 25000)
         INSERT INTO t SELECT i, 'fila ' || i FROM n;";
    call(&mut input, 2, Call::Execute { session, text: fill.into(), max_rows: 10, sink: false });
    reply(&mut output, 2);

    // Take the whole window and never acknowledge it.
    let spec = ReadSpec { table: ObjectRef { kind: "table".into(), schema: None, name: "t".into() }, columns: None, filter: None };
    call(&mut input, 3, Call::ReadBatches { session, spec });
    let mut batches = 0;
    while batches < BATCH_WINDOW {
        if let FromHost::Batch { id: 3, .. } = read_frame::<FromHost>(&mut output).unwrap().expect("frame") {
            batches += 1;
        }
    }
    std::thread::sleep(Duration::from_millis(300));

    // The app dies: both pipes close.
    drop(input);
    drop(output);
    let deadline = Instant::now() + Duration::from_secs(15);
    let ended = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(ended, "the host outlived the app");
}
