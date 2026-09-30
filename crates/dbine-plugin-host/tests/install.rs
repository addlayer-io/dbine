//! A driver's host downloaded like the app does it: from a (local) web
//! server, resumed after a cut, checked, and then started and used.

use dbine_driver::runtime::{set_components_dir, set_progress_sink, ComponentProgress};
use dbine_driver::{ConnectionConfig, Driver, QueryOutcome};
use dbine_plugin::install::{ensure, exe_name, hosts_dir, installed, remove_stale, Catalog, HostAsset};
use dbine_plugin::{DriverMeta, Launcher, RemoteDriver};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const EXE: &str = env!("CARGO_BIN_EXE_dbine-plugin-host");

/// Serves `body` at any path, honoring `Range: bytes=N-`; records the
/// ranges asked for.
fn serve(body: Vec<u8>) -> (String, Arc<Mutex<Vec<Option<u64>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let seen = ranges.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut s = stream.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            let text = String::from_utf8_lossy(&req).to_lowercase();
            let from = text.lines().find_map(|l| l.strip_prefix("range: bytes=")).and_then(|r| r.trim_end_matches('-').parse::<u64>().ok());
            seen.lock().unwrap().push(from);
            let (status, part) = match from {
                Some(f) => ("206 Partial Content", &body[f as usize..]),
                None => ("200 OK", &body[..]),
            };
            let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", part.len());
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(part);
        }
    });
    (format!("http://{addr}"), ranges)
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_resumes_checks_and_runs_a_host() {
    let components = tempfile::tempdir().unwrap();
    set_components_dir(components.path().to_path_buf());
    let events: Arc<Mutex<Vec<ComponentProgress>>> = Arc::default();
    let seen = events.clone();
    set_progress_sink(move |p| seen.lock().unwrap().push(p.clone()));

    // The published file: the host, gzipped.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&std::fs::read(EXE).unwrap()).unwrap();
    let gz = gz.finish().unwrap();
    let sha: String = Sha256::digest(&gz).iter().map(|b| format!("{b:02x}")).collect();
    let (base_url, ranges) = serve(gz.clone());
    let file = "dbine-driver-sqlite-1.2.0+p1.e1-test.gz".to_string();
    let version = "1.2.0+p1.e1".to_string();
    let mut catalog = Catalog { target: "test".into(), base_url, hosts: Default::default() };
    catalog.hosts.insert("sqlite".into(), HostAsset { version: version.clone(), file: file.clone(), size: gz.len() as u64, sha256: sha.clone() });

    // What an update leaves behind: another version of the driver, a folder
    // of the 0.1.x layout, a cut download of another file.
    let dir = hosts_dir();
    std::fs::create_dir_all(dir.join("0.1.0")).unwrap();
    let old = dir.join(exe_name("sqlite", "1.1.0+p1.e1"));
    std::fs::write(&old, b"old").unwrap();
    std::fs::write(dir.join("dbine-driver-postgres-0.9.0+p1.e1-test.gz.part"), b"x").unwrap();

    // A cut download: half of it is already there.
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{file}.part")), &gz[..gz.len() / 2]).unwrap();

    remove_stale(&catalog);
    assert!(!old.exists() && !dir.join("0.1.0").exists() && !dir.join("dbine-driver-postgres-0.9.0+p1.e1-test.gz.part").exists());
    assert!(dir.join(format!("{file}.part")).exists(), "the current file's cut download is kept, to resume it");

    let exe = ensure(&catalog, "sqlite", "SQLite", vec!["sqlite".into()]).await.unwrap();
    assert_eq!(exe, dir.join(exe_name("sqlite", &version)));
    assert_eq!(ranges.lock().unwrap().as_slice(), &[Some((gz.len() / 2) as u64)], "it resumed where the cut left it");
    assert!(!dir.join(format!("{file}.part")).exists());
    assert_eq!(installed(&catalog).iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), vec!["sqlite"]);
    {
        let ev = events.lock().unwrap();
        let last = ev.last().expect("progress");
        assert_eq!((last.done, last.total, last.drivers.clone()), (gz.len() as u64, gz.len() as u64, vec!["sqlite".to_string()]));
    }

    // Already there: no second download.
    ensure(&catalog, "sqlite", "SQLite", vec![]).await.unwrap();
    assert_eq!(ranges.lock().unwrap().len(), 1);

    // The downloaded host serves the driver.
    let (_, drivers) = dbine_drivers::packages().into_iter().find(|(p, _)| *p == "sqlite").unwrap();
    let meta = DriverMeta::of("sqlite", drivers[0].as_ref());
    let launcher = Launcher::at("sqlite", exe, vec!["--package".into(), "sqlite".into()], None);
    let remote = RemoteDriver::new(meta, launcher);
    let db = tempfile::tempdir().unwrap();
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: db.path().join("x.db").display().to_string(), ..Default::default() };
    let mut s = remote.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT 40 + 2 AS n", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(42));

    // A file that doesn't match its SHA-256 is refused and removed.
    let mut bad = catalog.clone();
    bad.hosts.insert("otro".into(), HostAsset { version: "1.0.0".into(), file: "otro.gz".into(), size: gz.len() as u64, sha256: "0".repeat(64) });
    let err = ensure(&bad, "otro", "Otro", vec![]).await.unwrap_err().to_string();
    assert!(err.contains("SHA-256"), "{err}");
    assert!(!dir.join("otro.gz.part").exists());
}
