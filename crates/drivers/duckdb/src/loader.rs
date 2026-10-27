//! DuckDB's native library is not built into DBine: the first connection
//! downloads DuckDB's official build for this platform (pinned version and
//! SHA-256, resumable) into the components folder and loads it; later ones
//! load it straight from disk. `DBINE_DUCKDB_LIB` points to a library
//! already on disk instead (machines without internet).

use dbine_driver::runtime::{components_dir, report_progress, ComponentProgress};
use dbine_driver::{Error, Result};
use duckdb::ffi;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::OnceLock;
use tokio::io::AsyncWriteExt;

include!("api_table.rs");

/// Must match the `duckdb` crate's version (1.MAJOR_MINOR_PATCH.x).
const VERSION: &str = "2.0.0";

/// A release asset of github.com/duckdb/duckdb (sizes and SHA-256 of the
/// gzipped tar the release publishes).
struct Asset {
    archive: &'static str,
    lib: &'static str,
    size: u64,
    sha256: &'static str,
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const ASSET: Option<Asset> = Some(Asset {
    archive: "duckdb-shared-libs-windows-amd64.tar.gz",
    lib: "duckdb.dll",
    size: 19_833_650,
    sha256: "2b83169128239de5f45fe21de80f99bf436e53aa4bb24020dd951a1ba8d4b95f",
});
#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
const ASSET: Option<Asset> = Some(Asset {
    archive: "duckdb-shared-libs-windows-arm64.tar.gz",
    lib: "duckdb.dll",
    size: 18_389_170,
    sha256: "159ce60eb587112b8c5f290b8beea56af5f13be4d13679de127ad9bfaafb7447",
});
#[cfg(target_os = "macos")]
const ASSET: Option<Asset> = Some(Asset {
    archive: "duckdb-shared-libs-osx-universal.tar.gz",
    lib: "libduckdb.dylib",
    size: 40_361_654,
    sha256: "14cd63180614cfd961f7959146ecd9087527ff20b67311efdc3fa1c29c18204d",
});
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET: Option<Asset> = Some(Asset {
    archive: "duckdb-shared-libs-linux-amd64.tar.gz",
    lib: "libduckdb.so",
    size: 27_841_656,
    sha256: "993ad89e7dfca66cd6da994c2cad090c2fc160ddf9f550498a098ab4bb13cfb1",
});
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const ASSET: Option<Asset> = Some(Asset {
    archive: "duckdb-shared-libs-linux-arm64.tar.gz",
    lib: "libduckdb.so",
    size: 25_735_927,
    sha256: "c60eecf91143ca6bbac55c4f4d40dc22520488b0ac257c86e0fe9a2b451cf2b7",
});
#[cfg(not(any(
    all(
        target_os = "windows",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
)))]
const ASSET: Option<Asset> = None;

const COMPONENT: &str = "DuckDB";

static READY: OnceLock<()> = OnceLock::new();
static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static TABLE: AtomicPtr<ffi::duckdb_ext_api_v1> = AtomicPtr::new(std::ptr::null_mut());

/// Make DuckDB usable: download it if needed, then load it (once per
/// process). Concurrent callers wait for the same download.
pub async fn ensure() -> Result<()> {
    if READY.get().is_some() {
        return Ok(());
    }
    let _guard = INSTALL.lock().await;
    if READY.get().is_some() {
        return Ok(());
    }
    let path = match std::env::var_os("DBINE_DUCKDB_LIB") {
        Some(p) => PathBuf::from(p),
        None => install().await?,
    };
    tokio::task::spawn_blocking(move || load(&path))
        .await
        .map_err(|e| Error::State(e.to_string()))??;
    let _ = READY.set(());
    Ok(())
}

/// The library's path, downloading and unpacking it the first time.
async fn install() -> Result<PathBuf> {
    let a = ASSET.as_ref().ok_or_else(|| {
        Error::Unsupported("DuckDB no publica su librería para esta plataforma".into())
    })?;
    let dir = components_dir().join(format!("duckdb-{VERSION}"));
    let lib = dir.join(a.lib);
    if lib.is_file() {
        return Ok(lib);
    }
    let io = |e: std::io::Error| {
        Error::Connect(format!(
            "no se pudo guardar DuckDB en {}: {e}",
            dir.display()
        ))
    };
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    let archive = download(a, &dir).await?;
    let (archive2, lib2, name) = (archive.clone(), lib.clone(), a.lib);
    tokio::task::spawn_blocking(move || unpack(&archive2, name, &lib2))
        .await
        .map_err(|e| Error::State(e.to_string()))?
        .map_err(io)?;
    let _ = tokio::fs::remove_file(&archive).await;
    Ok(lib)
}

/// Download the archive (resuming a partial one) and check its SHA-256.
async fn download(a: &Asset, dir: &Path) -> Result<PathBuf> {
    let fail = |e: String| Error::Connect(format!("no se pudo descargar DuckDB: {e}"));
    let io = |e: std::io::Error| fail(e.to_string());
    let url = format!(
        "https://github.com/duckdb/duckdb/releases/download/v{VERSION}/{}",
        a.archive
    );
    let part = dir.join(format!("{}.part", a.archive));
    let mut have = tokio::fs::metadata(&part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if have > a.size {
        let _ = tokio::fs::remove_file(&part).await;
        have = 0;
    }
    let progress = |done: u64| {
        report_progress(&ComponentProgress {
            component: COMPONENT.into(),
            drivers: vec!["duckdb".into(), "duckdb_files".into()],
            done,
            total: a.size,
        })
    };
    if have < a.size {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        let mut req = client.get(&url);
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let resp = req.send().await.map_err(|e| fail(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(fail(format!("HTTP {}", status.as_u16())));
        }
        // The server may ignore the range: start over then.
        if have > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            have = 0;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(have > 0)
            .truncate(have == 0)
            .open(&part)
            .await
            .map_err(io)?;
        let mut done = have;
        progress(done);
        let mut stream = resp.bytes_stream();
        let mut last = std::time::Instant::now();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                fail(format!(
                    "se cortó la descarga (se retoma al reintentar): {e}"
                ))
            })?;
            file.write_all(&chunk).await.map_err(io)?;
            done += chunk.len() as u64;
            if last.elapsed().as_millis() > 200 {
                progress(done);
                last = std::time::Instant::now();
            }
        }
        file.flush().await.map_err(io)?;
        if done != a.size {
            return Err(fail(format!(
                "quedó incompleta ({done} de {} bytes): reintentá para retomarla",
                a.size
            )));
        }
    }
    progress(a.size);
    let p = part.clone();
    let sum = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
        let mut h = Sha256::new();
        std::io::copy(&mut std::fs::File::open(p)?, &mut h)?;
        Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    })
    .await
    .map_err(|e| Error::State(e.to_string()))?
    .map_err(io)?;
    if sum != a.sha256 {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(fail(
            "el archivo no coincide con el original (SHA-256): se borró, reintentá".into(),
        ));
    }
    Ok(part)
}

/// Take the library out of the gzipped tar; written aside and renamed, so a
/// half-written file is never taken for the library. Headers in the same
/// archive are skipped. Only a regular file whose name is `name` is read.
fn unpack(archive: &Path, name: &str, dest: &Path) -> std::io::Result<()> {
    let file = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        let plain = path.components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
        if !plain || path.file_name().and_then(|s| s.to_str()) != Some(name) {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let mut bytes = Vec::new();
        std::io::copy(&mut entry, &mut bytes)?;
        #[cfg(target_os = "macos")]
        let bytes = thin(bytes);
        let tmp = dest.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, dest)?;
        return Ok(());
    }
    Err(std::io::Error::other(format!("el archivo no trae {name}")))
}

/// The macOS build is universal (x86_64 + arm64, ~115 MB): keep only this
/// Mac's architecture. Each slice carries its own code signature.
#[cfg(target_os = "macos")]
fn thin(bytes: Vec<u8>) -> Vec<u8> {
    let be = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    // CPU_TYPE_ARM64 / CPU_TYPE_X86_64.
    let want = if cfg!(target_arch = "aarch64") {
        0x0100_000c
    } else {
        0x0100_0007
    };
    if be(0) != Some(0xcafe_babe) {
        return bytes;
    }
    for i in 0..be(4).unwrap_or(0) as usize {
        let at = 8 + i * 20;
        if be(at) == Some(want) {
            if let (Some(off), Some(len)) = (be(at + 8), be(at + 12)) {
                if let Some(slice) = bytes.get(off as usize..off as usize + len as usize) {
                    return slice.to_vec();
                }
            }
        }
    }
    bytes
}

fn sym<T: Copy>(lib: &libloading::Library, name: &[u8]) -> Option<T> {
    unsafe { lib.get::<T>(name).ok().map(|s| *s) }
}

unsafe extern "C" fn get_api(_: ffi::duckdb_extension_info, _: *const c_char) -> *const c_void {
    TABLE.load(Ordering::Acquire) as *const c_void
}

/// Load the library and point the `duckdb` crate's C API at it. The library
/// stays loaded for the life of the process.
fn load(path: &Path) -> Result<()> {
    let fail = |e: String| {
        Error::Connect(format!(
            "no se pudo cargar DuckDB ({}): {e}",
            path.display()
        ))
    };
    let lib = unsafe { libloading::Library::new(path) }.map_err(|e| fail(e.to_string()))?;
    let lib: &'static libloading::Library = Box::leak(Box::new(lib));
    let table = api_table!(lib);
    if table.duckdb_open_ext.is_none() || table.duckdb_library_version.is_none() {
        return Err(fail("no es una librería de DuckDB".into()));
    }
    TABLE.store(Box::into_raw(Box::new(table)), Ordering::Release);
    let access = ffi::duckdb_extension_access {
        set_error: None,
        get_database: None,
        get_api: Some(get_api),
    };
    match unsafe { ffi::duckdb_rs_extension_api_init(std::ptr::null_mut(), &access, "v1.2.0") } {
        Ok(true) => {}
        _ => return Err(fail("no se pudo inicializar su API".into())),
    }
    let version = unsafe { std::ffi::CStr::from_ptr(ffi::duckdb_library_version()) }
        .to_string_lossy()
        .into_owned();
    if version.trim_start_matches('v') != VERSION {
        tracing::warn!(
            "duckdb: loaded {version}, expected v{VERSION} ({})",
            path.display()
        );
    }
    Ok(())
}

/// For tests: load DuckDB from a synchronous context.
#[cfg(test)]
pub fn ensure_blocking() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(ensure())
        .unwrap();
}
