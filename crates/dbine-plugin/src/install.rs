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

/// The (driver crate, version) of an installed host's file name.
fn parse_exe(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("dbine-driver-")?;
    let rest = rest.strip_suffix(std::env::consts::EXE_SUFFIX).unwrap_or(rest);
    if rest.ends_with(".part") || rest.ends_with(".tmp") || rest.ends_with(".gz") {
        return None;
    }
    let (package, version) = rest.rsplit_once('-')?;
    Some((package.to_string(), version.to_string()))
}

/// Remove what the catalog no longer uses: hosts of other versions (after
/// an update; their processes belonged to the previous run of the app),
/// cut downloads of other files, and the per-app-version folders of
/// DBine 0.1.x.
pub fn remove_stale(catalog: &Catalog) {
    let Ok(rd) = std::fs::read_dir(hosts_dir()) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let keep = match parse_exe(&name) {
            Some((package, version)) => catalog.hosts.get(&package).is_some_and(|h| h.version == version),
            None => catalog.hosts.values().any(|h| name == format!("{}.part", h.file)),
        };
        if !keep {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Installed hosts the catalog uses: (driver crate, bytes on disk).
pub fn installed(catalog: &Catalog) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = catalog
        .hosts
        .iter()
        .filter_map(|(package, h)| {
            let meta = std::fs::metadata(hosts_dir().join(exe_name(package, &h.version))).ok()?;
            meta.is_file().then(|| (package.clone(), meta.len()))
        })
        .collect();
    out.sort();
    out
}

/// Remove an installed host (it's downloaded again when needed).
pub fn remove(catalog: &Catalog, package: &str) -> std::io::Result<()> {
    let h = catalog.hosts.get(package).ok_or_else(|| std::io::Error::other(format!("no hay un driver «{package}»")))?;
    std::fs::remove_file(hosts_dir().join(exe_name(package, &h.version)))
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
    let dir = hosts_dir();
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
    let url = format!("{}/{}", catalog.base_url.trim_end_matches('/'), asset.file);
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
    fn parses_host_file_names() {
        let exe = |s: &str| format!("{s}{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(parse_exe(&exe("dbine-driver-sqlserver-1.4.0")), Some(("sqlserver".into(), "1.4.0".into())));
        assert_eq!(parse_exe(&exe("dbine-driver-my_sql-0.1.0")), Some(("my_sql".into(), "0.1.0".into())));
        assert_eq!(parse_exe("dbine-driver-sqlserver-1.4.0-p1-x86_64-pc-windows-msvc.gz.part"), None);
        assert_eq!(parse_exe("otro"), None);
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
