//! Getting a driver's host onto the machine: the first connection to an
//! engine downloads its host (size and SHA-256 pinned in the catalog the app
//! carries), resumable, into the components folder; later ones start it
//! from disk. Drivers have their own versions, apart from the app's: a file
//! is named after its driver's version, so an app update that keeps a
//! driver's version keeps its file, and a new version never overwrites a
//! host that's running.
//!
//! `DBINE_DRIVERS_DIR` points to a folder that already has the hosts
//! (machines without internet): nothing is downloaded then.

use dbine_driver::runtime::{components_dir, report_progress, ComponentProgress};
use dbine_driver::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The drivers this app uses, for this platform: each crate's version and
/// where its host is published.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    /// Rust target triple they run on.
    pub target: String,
    /// Where the files are (the `drivers` release's download URL).
    pub base_url: String,
    /// Driver crate → its published host.
    pub hosts: HashMap<String, HostAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostAsset {
    /// The driver crate's version (its own, not the app's).
    pub version: String,
    /// The published file (gzip), relative to `base_url`.
    pub file: String,
    /// Its size and SHA-256.
    pub size: u64,
    pub sha256: String,
}

/// The executable's file name for a driver crate's version.
pub fn exe_name(package: &str, version: &str) -> String {
    format!("dbine-driver-{package}-{version}{}", std::env::consts::EXE_SUFFIX)
}

/// Where the hosts are.
pub fn hosts_dir() -> PathBuf {
    components_dir().join("drivers")
}

impl HostAsset {
    /// A host from the drivers index (`id` is its key there).
    pub fn from_entry(id: &str, e: &crate::index::IndexEntry) -> HostAsset {
        HostAsset { version: id.to_string(), file: e.file.clone(), size: e.size, sha256: e.sha256.clone() }
    }
}

/// Remove every host file but `keep` (executable or `.part` names), and
/// the per-app-version folders of DBine 0.1.x. Other files (the state, the
/// cached index) stay. A file in use (Windows) stays until next time.
pub fn gc(dir: &Path, keep: &std::collections::HashSet<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("dbine-driver-") && !keep.contains(&name) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Remove everything but the catalog's versions (and their cut
/// downloads). The app keeps more (the updater's [`gc`] keeps the active
/// and previous versions too); this is the floor-only cleanup.
pub fn remove_stale(catalog: &Catalog) {
    let keep = catalog.hosts.iter().flat_map(|(p, h)| [exe_name(p, &h.version), format!("{}.part", h.file)]).collect();
    gc(&hosts_dir(), &keep)
}

/// Installed hosts of the catalog's versions: (driver crate, bytes on disk).
pub fn installed(catalog: &Catalog) -> Vec<(String, u64)> {
    let dir = hosts_dir();
    let mut out: Vec<(String, u64)> = catalog.hosts.iter().filter_map(|(p, h)| installed_size(&dir, p, &h.version).map(|n| (p.clone(), n))).collect();
    out.sort();
    out
}

/// Bytes on disk of a driver crate's host version, when it's there.
pub fn installed_size(dir: &Path, package: &str, id: &str) -> Option<u64> {
    let meta = std::fs::metadata(dir.join(exe_name(package, id))).ok()?;
    meta.is_file().then_some(meta.len())
}

/// Whether `name` is one of `package`'s host files (any version, partial
/// downloads included).
fn is_package_file(name: &str, package: &str) -> bool {
    name.strip_prefix("dbine-driver-")
        .and_then(|r| r.strip_prefix(package))
        .and_then(|r| r.strip_prefix('-'))
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
}

/// Remove every version of a driver crate's host (it's downloaded again
/// when needed).
pub fn remove(dir: &Path, package: &str) -> std::io::Result<()> {
    let mut first_err = None;
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(()) };
    for e in rd.flatten() {
        if is_package_file(&e.file_name().to_string_lossy(), package) {
            if let Err(err) = std::fs::remove_file(e.path()) {
                first_err.get_or_insert(err);
            }
        }
    }
    first_err.map_or(Ok(()), Err)
}

fn install_lock(package: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(Default::default).lock().unwrap().entry(package.to_string()).or_default().clone()
}

/// The host of `package`, downloading it the first time. `label` names
/// the engine in the progress ("SQL Server"); `drivers` are the ids that
/// wait for it (the explorer shows the progress on their nodes).
pub async fn ensure(catalog: &Catalog, package: &str, label: &str, drivers: Vec<String>) -> Result<PathBuf> {
    let asset = catalog
        .hosts
        .get(package)
        .ok_or_else(|| Error::Unsupported(format!("el driver de {label} no se publica para esta plataforma ({})", catalog.target)))?;
    if let Some(dir) = std::env::var_os("DBINE_DRIVERS_DIR") {
        let exe = PathBuf::from(dir).join(exe_name(package, &asset.version));
        return if exe.is_file() {
            Ok(exe)
        } else {
            Err(Error::Connect(format!("no está el driver de {label} en DBINE_DRIVERS_DIR ({})", exe.display())))
        };
    }
    ensure_asset(&hosts_dir(), &catalog.base_url, package, asset, label, drivers).await
}

/// The host `asset` of `package` (published under `base_url`) in `dir`, downloading
/// it if it isn't on disk: resumable, checked against its SHA-256, unpacked
/// beside its final name, one download per driver crate at a time.
pub async fn ensure_asset(dir: &Path, base_url: &str, package: &str, asset: &HostAsset, label: &str, drivers: Vec<String>) -> Result<PathBuf> {
    let exe = dir.join(exe_name(package, &asset.version));
    if exe.is_file() {
        return Ok(exe);
    }
    // One download per driver: other connections wait for it.
    let lock = install_lock(package);
    let _guard = lock.lock().await;
    if exe.is_file() {
        return Ok(exe);
    }
    let io = |e: std::io::Error| Error::Connect(format!("no se pudo guardar el driver de {label} en {}: {e}", dir.display()));
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    let url = format!("{}/{}", base_url.trim_end_matches('/'), asset.file);
    let part = dir.join(format!("{}.part", asset.file));
    download(&url, &part, asset, label, &drivers).await?;
    let (part2, exe2) = (part.clone(), exe.clone());
    tokio::task::spawn_blocking(move || unpack(&part2, &exe2)).await.map_err(|e| Error::State(e.to_string()))?.map_err(io)?;
    let _ = tokio::fs::remove_file(&part).await;
    Ok(exe)
}

/// Download (resuming a partial file) and check the SHA-256.
async fn download(url: &str, part: &Path, a: &HostAsset, label: &str, drivers: &[String]) -> Result<()> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let fail = |e: String| Error::Connect(format!("no se pudo descargar el driver de {label}: {e}"));
    let io = |e: std::io::Error| fail(e.to_string());
    let component = format!("el driver de {label}");
    let progress = |done: u64| report_progress(&ComponentProgress { component: component.clone(), drivers: drivers.to_vec(), done, total: a.size });
    let mut have = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    if have > a.size {
        let _ = tokio::fs::remove_file(part).await;
        have = 0;
    }
    if have < a.size {
        let client = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(15)).build().unwrap_or_default();
        let mut req = client.get(url);
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let resp = req.send().await.map_err(|e| fail(format!("¿hay conexión a internet? ({e})")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(fail(format!("HTTP {}", status.as_u16())));
        }
        // The server may ignore the range: start over then.
        if have > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            have = 0;
        }
        let mut file = tokio::fs::OpenOptions::new().create(true).write(true).append(have > 0).truncate(have == 0).open(part).await.map_err(io)?;
        let mut done = have;
        progress(done);
        let mut stream = resp.bytes_stream();
        let mut last = std::time::Instant::now();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| fail(format!("se cortó la descarga (se retoma al reintentar): {e}")))?;
            file.write_all(&chunk).await.map_err(io)?;
            done += chunk.len() as u64;
            if last.elapsed().as_millis() > 200 {
                progress(done);
                last = std::time::Instant::now();
            }
        }
        file.flush().await.map_err(io)?;
        if done != a.size {
            return Err(fail(format!("quedó incompleta ({done} de {} bytes): reintentá para retomarla", a.size)));
        }
    }
    progress(a.size);
    let p = part.to_path_buf();
    let sum = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
        let mut h = Sha256::new();
        std::io::copy(&mut std::fs::File::open(p)?, &mut h)?;
        Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    })
    .await
    .map_err(|e| Error::State(e.to_string()))?
    .map_err(io)?;
    if sum != a.sha256 {
        let _ = tokio::fs::remove_file(part).await;
        return Err(fail("el archivo no coincide con el publicado (SHA-256): se borró, reintentá".into()));
    }
    Ok(())
}

/// Unzip the host beside its final name and rename it: a half-written file
/// is never taken for the host.
fn unpack(gz: &Path, dest: &Path) -> std::io::Result<()> {
    let tmp = dest.with_extension("tmp");
    {
        let mut input = flate2::read::GzDecoder::new(std::fs::File::open(gz)?);
        let mut out = std::fs::File::create(&tmp)?;
        std::io::copy(&mut input, &mut out)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn gc_keeps_what_it_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let names = [
            "dbine-driver-postgres-1.0.0+p1.e3",
            "dbine-driver-postgres-1.0.1+p1.e3",
            "dbine-driver-postgres-1.0.2+p1.e3",
            "dbine-driver-postgres-1.0.3+p1.e3",
            "dbine-driver-postgres-1.0.3+p1.e3-x86_64-apple-darwin.gz.part",
            "dbine-driver-mysql-2.0.0+p1.e3-x86_64-apple-darwin.gz.part",
            "state.json",
            "index-x86_64-apple-darwin.json",
            "index-x86_64-apple-darwin.json.sig",
        ];
        for n in names {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        std::fs::create_dir(dir.path().join("0.1.4")).unwrap();
        let keep: std::collections::HashSet<String> = [
            "dbine-driver-postgres-1.0.0+p1.e3",
            "dbine-driver-postgres-1.0.2+p1.e3",
            "dbine-driver-postgres-1.0.3+p1.e3",
            "dbine-driver-postgres-1.0.3+p1.e3-x86_64-apple-darwin.gz.part",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        gc(dir.path(), &keep);
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        left.sort();
        assert_eq!(
            left,
            [
                "dbine-driver-postgres-1.0.0+p1.e3",
                "dbine-driver-postgres-1.0.2+p1.e3",
                "dbine-driver-postgres-1.0.3+p1.e3",
                "dbine-driver-postgres-1.0.3+p1.e3-x86_64-apple-darwin.gz.part",
                "index-x86_64-apple-darwin.json",
                "index-x86_64-apple-darwin.json.sig",
                "state.json",
            ]
        );
    }

    #[test]
    fn package_files_by_name() {
        assert!(is_package_file("dbine-driver-postgres-1.0.0+p1.e3", "postgres"));
        assert!(is_package_file("dbine-driver-postgres-1.0.0+p1.e3-x.gz.part", "postgres"));
        assert!(!is_package_file("dbine-driver-postgres-1.0.0+p1.e3", "postgre"));
        assert!(!is_package_file("dbine-driver-my_sql-1.0.0+p1.e3", "my"));
        assert!(!is_package_file("state.json", "postgres"));
    }

    #[test]
    fn unpacks_an_executable() {
        let dir = tempfile::tempdir().unwrap();
        let gz = dir.path().join("x.gz");
        let mut enc = flate2::write::GzEncoder::new(std::fs::File::create(&gz).unwrap(), flate2::Compression::default());
        enc.write_all(b"#!/bin/sh\necho hola\n").unwrap();
        enc.finish().unwrap();
        let exe = dir.path().join(exe_name("x", "1.0.0"));
        unpack(&gz, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"#!/bin/sh\necho hola\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755);
        }
    }
}
