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
const VERSION: &str = "1.5.5";

/// A release asset of github.com/duckdb/duckdb (sizes and SHA-256 as the
/// release lists them).
struct Asset {
    archive: &'static str,
    lib: &'static str,
    size: u64,
    sha256: &'static str,
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const ASSET: Option<Asset> = Some(Asset {
    archive: "libduckdb-windows-amd64.zip",
    lib: "duckdb.dll",
    size: 13_412_060,
    sha256: "8375eb1fcf2212e8a0817950354815d4dde9dd383c2d9fa7b8975b71e278c1bd",
});
#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
const ASSET: Option<Asset> = Some(Asset {
    archive: "libduckdb-windows-arm64.zip",
    lib: "duckdb.dll",
    size: 14_355_813,
    sha256: "006f8df62957f640a100d673432a5b6f9a7002662822a4567ed06a436ee1d801",
});
#[cfg(target_os = "macos")]
const ASSET: Option<Asset> = Some(Asset {
    archive: "libduckdb-osx-universal.zip",
    lib: "libduckdb.dylib",
    size: 36_178_910,
    sha256: "7b5b8915cc382d0708636fe6385c0cdad5a61c9ff8ba2638b3e2141640783155",
});
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ASSET: Option<Asset> = Some(Asset {
    archive: "libduckdb-linux-amd64.zip",
    lib: "libduckdb.so",
    size: 41_300_590,
    sha256: "1fb8ce388157d84a25abe685a8a2520bf00c00321821968e4bb398fd766e7abb",
});
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const ASSET: Option<Asset> = Some(Asset {
    archive: "libduckdb-linux-arm64.zip",
    lib: "libduckdb.so",
    size: 37_694_966,
    sha256: "abe4f6f005ee0b448a058322f4263584b4bd1b6faf7ab4637b79eeaf978f8e9c",
});
#[cfg(not(any(
    all(target_os = "windows", any(target_arch = "x86_64", target_arch = "aarch64")),
    target_os = "macos",
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
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
    tokio::task::spawn_blocking(move || load(&path)).await.map_err(|e| Error::State(e.to_string()))??;
    let _ = READY.set(());
    Ok(())
}

/// The library's path, downloading and unpacking it the first time.
async fn install() -> Result<PathBuf> {
    let a = ASSET.as_ref().ok_or_else(|| Error::Unsupported("DuckDB no publica su librería para esta plataforma".into()))?;
    let dir = components_dir().join(format!("duckdb-{VERSION}"));
    let lib = dir.join(a.lib);
    if lib.is_file() {
        return Ok(lib);
    }
    let io = |e: std::io::Error| Error::Connect(format!("no se pudo guardar DuckDB en {}: {e}", dir.display()));
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    let zip = download(a, &dir).await?;
    let (zip2, lib2, name) = (zip.clone(), lib.clone(), a.lib);
    tokio::task::spawn_blocking(move || unpack(&zip2, name, &lib2)).await.map_err(|e| Error::State(e.to_string()))?.map_err(io)?;
    let _ = tokio::fs::remove_file(&zip).await;
    Ok(lib)
}

/// Download the archive (resuming a partial one) and check its SHA-256.
async fn download(a: &Asset, dir: &Path) -> Result<PathBuf> {
    let fail = |e: String| Error::Connect(format!("no se pudo descargar DuckDB: {e}"));
    let io = |e: std::io::Error| fail(e.to_string());
    let url = format!("https://github.com/duckdb/duckdb/releases/download/v{VERSION}/{}", a.archive);
    let part = dir.join(format!("{}.part", a.archive));
    let mut have = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
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
        let client = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(15)).build().unwrap_or_default();
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
        let mut file =
            tokio::fs::OpenOptions::new().create(true).write(true).append(have > 0).truncate(have == 0).open(&part).await.map_err(io)?;
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
        return Err(fail("el archivo no coincide con el original (SHA-256): se borró, reintentá".into()));
    }
    Ok(part)
}

/// Take the library out of the archive; written aside and renamed, so a
/// half-written file is never taken for the library.
fn unpack(zip: &Path, name: &str, dest: &Path) -> std::io::Result<()> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip)?).map_err(std::io::Error::other)?;
    let mut entry = archive.by_name(name).map_err(std::io::Error::other)?;
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    std::io::copy(&mut entry, &mut bytes)?;
    #[cfg(target_os = "macos")]
    let bytes = thin(bytes);
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dest)
}

/// The macOS build is universal (x86_64 + arm64, ~115 MB): keep only this
/// Mac's architecture. Each slice carries its own code signature.
#[cfg(target_os = "macos")]
fn thin(bytes: Vec<u8>) -> Vec<u8> {
    let be = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    // CPU_TYPE_ARM64 / CPU_TYPE_X86_64.
    let want = if cfg!(target_arch = "aarch64") { 0x0100_000c } else { 0x0100_0007 };
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
    let fail = |e: String| Error::Connect(format!("no se pudo cargar DuckDB ({}): {e}", path.display()));
    let lib = unsafe { libloading::Library::new(path) }.map_err(|e| fail(e.to_string()))?;
    let lib: &'static libloading::Library = Box::leak(Box::new(lib));
    let table = api_table!(lib);
    if table.duckdb_open_ext.is_none() || table.duckdb_library_version.is_none() {
        return Err(fail("no es una librería de DuckDB".into()));
    }
    TABLE.store(Box::into_raw(Box::new(table)), Ordering::Release);
    let access = ffi::duckdb_extension_access { set_error: None, get_database: None, get_api: Some(get_api) };
    match unsafe { ffi::duckdb_rs_extension_api_init(std::ptr::null_mut(), &access, "v1.2.0") } {
        Ok(true) => {}
        _ => return Err(fail("no se pudo inicializar su API".into())),
    }
    let version = unsafe { std::ffi::CStr::from_ptr(ffi::duckdb_library_version()) }.to_string_lossy().into_owned();
    if version.trim_start_matches('v') != VERSION {
        tracing::warn!("duckdb: loaded {version}, expected v{VERSION} ({})", path.display());
    }
    Ok(())
}

/// For tests: load DuckDB from a synchronous context.
#[cfg(test)]
pub fn ensure_blocking() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(ensure()).unwrap();
}
