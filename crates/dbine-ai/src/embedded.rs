//! The model built into DBine: a small code model run by llama.cpp, both
//! downloaded once into the app's data folder. Nothing to install and
//! nothing leaves the machine.
//!
//! llama.cpp is not built into the app. Its official build for the platform
//! (pinned release, SHA-256) comes down with the first model, or with the
//! first chat when a model is already on disk; its `llama-server` runs as a
//! child process on 127.0.0.1 with a random API key, and chats go through
//! its OpenAI-compatible API. Whoever never uses the built-in model
//! downloads nothing.

use crate::{AiError, Cancel, ChatRequest, OnDelta, Result};
use futures::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// A model DBine can download and run.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogModel {
    pub id: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    /// RAM it's comfortable with (GB).
    pub min_ram_gb: u32,
}

/// Qwen2.5-Coder Instruct, Q4_K_M, from Qwen's own repositories (sizes and
/// SHA-256 as Hugging Face reports them).
pub const CATALOG: &[CatalogModel] = &[
    CatalogModel {
        id: "qwen2.5-coder-3b",
        label: "Qwen2.5-Coder 3B",
        detail: "rápido y liviano · 2,1 GB",
        file: "qwen2.5-coder-3b-instruct-q4_k_m.gguf",
        url: "https://huggingface.co/Qwen/Qwen2.5-Coder-3B-Instruct-GGUF/resolve/main/qwen2.5-coder-3b-instruct-q4_k_m.gguf",
        size: 2_104_932_800,
        sha256: "724fb256bec1ff062b2f65e4569e871ad2e95ab2a3989723d1769c54294730b7",
        min_ram_gb: 8,
    },
    CatalogModel {
        id: "qwen2.5-coder-7b",
        label: "Qwen2.5-Coder 7B",
        detail: "mejores respuestas en SQL · 4,7 GB",
        file: "qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        url: "https://huggingface.co/Qwen/Qwen2.5-Coder-7B-Instruct-GGUF/resolve/main/qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        size: 4_683_073_536,
        sha256: "509287f78cb4d4cf6b3843734733b914b2c158e43e22a7f4bf5e963800894d3c",
        min_ram_gb: 16,
    },
    // The smallest that gets cross-database SQL Server right (brackets,
    // three-part names): 3B, 7B and 14B didn't in the refusal/quality bench
    // of 2026-10-03, and Qwen3-30B-A3B ran out of tokens thinking.
    CatalogModel {
        id: "qwen2.5-coder-32b",
        label: "Qwen2.5-Coder 32B",
        detail: "el más preciso en SQL · 19,9 GB",
        file: "qwen2.5-coder-32b-instruct-q4_k_m.gguf",
        url: "https://huggingface.co/Qwen/Qwen2.5-Coder-32B-Instruct-GGUF/resolve/main/qwen2.5-coder-32b-instruct-q4_k_m.gguf",
        size: 19_851_335_872,
        sha256: "4d64b316b5e6319d9613e0d97935d9ebd631fc7e334da400d00085eca749d085",
        min_ram_gb: 48,
    },
];

pub fn catalog_model(id: &str) -> Option<&'static CatalogModel> {
    CATALOG.iter().find(|m| m.id == id)
}

// -- engine (llama.cpp) -------------------------------------------------------------

/// The llama.cpp release the engine comes from.
const ENGINE_TAG: &str = "b11213";

/// llama.cpp's official build for this platform (github.com/ggml-org/
/// llama.cpp/releases; sizes and SHA-256 as the release lists them). CPU on
/// Windows and Linux, Metal on macOS, as before.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ENGINE: Option<CatalogModel> = Some(engine(
    "llama-b11213-bin-macos-arm64.tar.gz",
    "https://github.com/ggml-org/llama.cpp/releases/download/b11213/llama-b11213-bin-macos-arm64.tar.gz",
    11_755_479,
    "ab4774bfbfdf55fa62e1b5190c6468a571e3eb266f23b9e07b8ac9235022a025",
));
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const ENGINE: Option<CatalogModel> = Some(engine(
    "llama-b11213-bin-macos-x64.tar.gz",
    "https://github.com/ggml-org/llama.cpp/releases/download/b11213/llama-b11213-bin-macos-x64.tar.gz",
    11_309_055,
    "03cd4c9fd9f0afe6dc5a68383a5002739c34ebb8a3d1d20a11804137d6906466",
));
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
const ENGINE: Option<CatalogModel> = Some(engine(
    "llama-b11213-bin-win-cpu-x64.zip",
    "https://github.com/ggml-org/llama.cpp/releases/download/b11213/llama-b11213-bin-win-cpu-x64.zip",
    19_154_670,
    "2e2c98edd541ed420b504c54fb6271d113bd2edbf20e388e59957f85d2af4df2",
));
#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
const ENGINE: Option<CatalogModel> = Some(engine(
    "llama-b11213-bin-win-cpu-arm64.zip",
    "https://github.com/ggml-org/llama.cpp/releases/download/b11213/llama-b11213-bin-win-cpu-arm64.zip",
    12_040_642,
    "8772df4b4d1af6e2367c604b9670fd5f5afea61d4fbe30f6ee3e96a58e7e3c1b",
));
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ENGINE: Option<CatalogModel> = Some(engine(
    "llama-b11213-bin-ubuntu-x64.tar.gz",
    "https://github.com/ggml-org/llama.cpp/releases/download/b11213/llama-b11213-bin-ubuntu-x64.tar.gz",
    17_403_410,
    "f7a999e8946974f33c803b053b1aab9633605f4d1460839a711e863624ccd2e2",
));
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const ENGINE: Option<CatalogModel> = Some(engine(
    "llama-b11213-bin-ubuntu-arm64.tar.gz",
    "https://github.com/ggml-org/llama.cpp/releases/download/b11213/llama-b11213-bin-ubuntu-arm64.tar.gz",
    13_499_607,
    "980073eb27c3a9525bd529c0f03521b4155fdc56f901a4e4efe9982b6a9eebde",
));
#[cfg(not(any(
    all(any(target_os = "macos", target_os = "windows", target_os = "linux"), any(target_arch = "x86_64", target_arch = "aarch64")),
)))]
const ENGINE: Option<CatalogModel> = None;

#[allow(dead_code)]
const fn engine(file: &'static str, url: &'static str, size: u64, sha256: &'static str) -> CatalogModel {
    CatalogModel { id: "llama.cpp", label: "el motor de IA", detail: "", file, url, size, sha256, min_ram_gb: 0 }
}

/// Whether the built-in model can run on this platform.
pub const ENABLED: bool = ENGINE.is_some();

const SERVER_EXE: &str = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };

fn engine_dir(dir: &Path) -> PathBuf {
    dir.join(format!("llama.cpp-{ENGINE_TAG}"))
}

pub fn engine_installed(dir: &Path) -> bool {
    engine_dir(dir).join(SERVER_EXE).is_file()
}

/// Bytes the engine still has to download (0 once installed).
pub fn engine_pending(dir: &Path) -> u64 {
    if engine_installed(dir) {
        0
    } else {
        ENGINE.as_ref().map_or(0, |e| e.size)
    }
}

/// Download and unpack the engine unless it's there. `progress(done, total)`
/// over the archive's bytes.
pub async fn ensure_engine(dir: &Path, progress: &(dyn Fn(u64, u64) + Send + Sync), cancel: &Cancel) -> Result<()> {
    let e = ENGINE.as_ref().ok_or_else(|| AiError::NotAvailable("el modelo integrado no está disponible en esta plataforma".into()))?;
    static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = INSTALL.lock().await;
    if engine_installed(dir) {
        return Ok(());
    }
    let archive = download_from(dir, e, e.url, progress, cancel).await?;
    let dest = engine_dir(dir);
    let a = archive.clone();
    tokio::task::spawn_blocking(move || unpack(&a, &dest))
        .await
        .map_err(|e| AiError::Provider(e.to_string()))?
        .map_err(|e| AiError::Provider(format!("no se pudo instalar el motor de IA: {e}")))?;
    let _ = tokio::fs::remove_file(&archive).await;
    Ok(())
}

/// Unpack the release archive (tar.gz or zip) into `dest`, without its top
/// `llama-<tag>/` folder. Unpacked aside and renamed, so a half-unpacked
/// engine is never taken for an installed one.
fn unpack(archive: &Path, dest: &Path) -> std::io::Result<()> {
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dest.with_file_name(format!("{name}.tmp"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let file = std::fs::File::open(archive)?;
    if archive.extension().is_some_and(|x| x == "zip") {
        let mut zip = zip::ZipArchive::new(file).map_err(std::io::Error::other)?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(std::io::Error::other)?;
            let Some(rel) = entry.enclosed_name().and_then(|p| inner_path(&p)) else { continue };
            let out = tmp.join(rel);
            if entry.is_dir() {
                std::fs::create_dir_all(&out)?;
                continue;
            }
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::io::copy(&mut entry, &mut std::fs::File::create(&out)?)?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode))?;
            }
        }
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        for entry in tar.entries()? {
            let mut entry = entry?;
            let Some(rel) = inner_path(&entry.path()?) else { continue };
            let out = tmp.join(rel);
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            // Keeps the executable bits and the libraries' symlinks.
            entry.unpack(&out)?;
        }
    }
    if !tmp.join(SERVER_EXE).is_file() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(std::io::Error::other(format!("el archivo no trae {SERVER_EXE}")));
    }
    let _ = std::fs::remove_dir_all(dest);
    std::fs::rename(&tmp, dest)
}

/// A path inside the archive without the top `llama-<tag>/` folder; `None`
/// for that folder itself and for anything that isn't a plain relative path.
fn inner_path(p: &Path) -> Option<PathBuf> {
    let mut parts = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => parts.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.first().is_some_and(|f| *f == format!("llama-{ENGINE_TAG}").as_str()) {
        parts.remove(0);
    }
    (!parts.is_empty()).then(|| parts.iter().collect())
}

// -- models -------------------------------------------------------------------------

pub fn model_path(dir: &Path, m: &CatalogModel) -> PathBuf {
    dir.join(m.file)
}

/// Downloaded models (complete files only).
pub fn installed(dir: &Path) -> Vec<&'static CatalogModel> {
    CATALOG
        .iter()
        .filter(|m| std::fs::metadata(model_path(dir, m)).map(|md| md.len() == m.size).unwrap_or(false))
        .collect()
}

/// Download a model into `dir`, resuming a partial download, and check its
/// SHA-256 before putting it in place. `progress(done, total)`.
pub async fn download(dir: &Path, m: &CatalogModel, progress: &(dyn Fn(u64, u64) + Send + Sync), cancel: &Cancel) -> Result<PathBuf> {
    download_from(dir, m, m.url, progress, cancel).await
}

pub async fn download_from(
    dir: &Path,
    m: &CatalogModel,
    url: &str,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    cancel: &Cancel,
) -> Result<PathBuf> {
    let io = |e: std::io::Error| AiError::Provider(format!("no se pudo guardar {}: {e}", m.label));
    tokio::fs::create_dir_all(dir).await.map_err(io)?;
    let dest = model_path(dir, m);
    if std::fs::metadata(&dest).map(|md| md.len() == m.size).unwrap_or(false) {
        return Ok(dest);
    }
    let part = dir.join(format!("{}.part", m.file));
    let mut have = tokio::fs::metadata(&part).await.map(|md| md.len()).unwrap_or(0);
    if have > m.size {
        let _ = tokio::fs::remove_file(&part).await;
        have = 0;
    }
    let client = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(15)).build().unwrap_or_default();
    let mut req = client.get(url);
    if have > 0 {
        req = req.header("Range", format!("bytes={have}-"));
    }
    let resp = req.send().await.map_err(|e| AiError::Provider(format!("no se pudo descargar {}: {e}", m.label)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(AiError::Provider(format!("no se pudo descargar {}: HTTP {}", m.label, status.as_u16())));
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
    progress(done, m.size);
    let mut stream = resp.bytes_stream();
    let mut last = std::time::Instant::now();
    loop {
        let chunk = tokio::select! {
            c = stream.next() => c,
            _ = cancel.cancelled() => {
                file.flush().await.map_err(io)?;
                return Err(AiError::Cancelled);
            }
        };
        let Some(chunk) = chunk else { break };
        let chunk = chunk.map_err(|e| AiError::Provider(format!("se cortó la descarga (se retoma al reintentar): {e}")))?;
        file.write_all(&chunk).await.map_err(io)?;
        done += chunk.len() as u64;
        if last.elapsed().as_millis() > 200 {
            progress(done, m.size);
            last = std::time::Instant::now();
        }
    }
    file.flush().await.map_err(io)?;
    drop(file);
    progress(done, m.size);
    if done != m.size {
        return Err(AiError::Provider(format!("la descarga quedó incompleta ({done} de {} bytes): reintentá para retomarla", m.size)));
    }
    // Check the whole file (off the async threads: GBs of hashing).
    let p = part.clone();
    let sum = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
        let mut f = std::fs::File::open(p)?;
        let mut h = Sha256::new();
        std::io::copy(&mut f, &mut h)?;
        Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    })
    .await
    .map_err(|e| AiError::Provider(e.to_string()))?
    .map_err(io)?;
    if sum != m.sha256 {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(AiError::Provider(format!("{} descargado no coincide con el original (SHA-256): se borró, reintentá", m.label)));
    }
    tokio::fs::rename(&part, &dest).await.map_err(io)?;
    Ok(dest)
}

pub fn delete(dir: &Path, m: &CatalogModel) -> Result<()> {
    server::stop_if(&model_path(dir, m));
    for p in [model_path(dir, m), dir.join(format!("{}.part", m.file))] {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(AiError::Provider(format!("no se pudo borrar el modelo: {e}"))),
        }
    }
    Ok(())
}

/// Stop the engine. Call it before the process exits, so no `llama-server`
/// outlives the app.
pub fn shutdown() {
    server::stop();
}

// -- chat ---------------------------------------------------------------------------

/// Longest answer, in tokens.
const MAX_NEW_TOKENS: u32 = 3072;

/// Run a chat on a downloaded model (the engine must be installed:
/// `ensure_engine`).
pub async fn chat(dir: &Path, req: &ChatRequest, on_delta: OnDelta<'_>, cancel: &Cancel) -> Result<String> {
    let id = req.model.clone().unwrap_or_default();
    let m = catalog_model(&id).ok_or_else(|| AiError::NotAvailable("elegí un modelo integrado".into()))?;
    if !installed(dir).iter().any(|x| x.id == m.id) {
        return Err(AiError::NotAvailable(format!("el modelo {} no está descargado", m.label)));
    }
    if !engine_installed(dir) {
        return Err(AiError::NotAvailable("falta el motor de IA: volvé a intentar para descargarlo".into()));
    }
    let (base, key) = server::ready(&engine_dir(dir), &model_path(dir, m), cancel).await?;
    // Low temperature: SQL wants the likely answer, not a creative one.
    let body = serde_json::json!({
        "messages": crate::openai::messages(req),
        "stream": true,
        "temperature": 0.2,
        "top_p": 0.95,
        "min_p": 0.05,
        "max_tokens": MAX_NEW_TOKENS,
        "seed": 42,
    });
    let request = reqwest::Client::new().post(format!("{base}/v1/chat/completions")).bearer_auth(key).json(&body);
    match crate::openai::stream(request, "El modelo integrado", "el modelo integrado no responde".into(), on_delta).await {
        Err(AiError::Provider(msg)) if msg.contains("exceed") => Err(AiError::Provider(
            "la pregunta con la estructura de la base no entra en el modelo: desactivá «Incluir estructura» o usá un modelo más grande".into(),
        )),
        r => r,
    }
}

/// The running `llama-server`: one at a time, with the model of the last
/// chat loaded (loading takes a few seconds, so it stays up between
/// answers).
mod server {
    use super::SERVER_EXE;
    use crate::{AiError, Cancel, Result};
    use std::collections::VecDeque;
    use std::io::BufRead;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Context window (tokens): a question with the database structure
    /// fits with room for the answer.
    const CONTEXT: &str = "16384";

    struct Running {
        child: Child,
        engine: PathBuf,
        model: PathBuf,
        base: String,
        key: String,
        /// The server's last log lines, to explain a failed start.
        log: Arc<Mutex<VecDeque<String>>>,
    }

    static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
    static STARTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub fn stop() {
        let taken = RUNNING.lock().ok().and_then(|mut r| r.take());
        if let Some(mut s) = taken {
            let _ = s.child.kill();
            let _ = s.child.wait();
            let _ = std::fs::remove_file(s.engine.join("server.pid"));
        }
    }

    pub fn stop_if(model: &Path) {
        let running = RUNNING.lock().ok().is_some_and(|r| r.as_ref().is_some_and(|s| s.model == model));
        if running {
            stop();
        }
    }

    /// The server's address and key, with `model` loaded; starts it (or
    /// restarts it with another model) when needed.
    pub async fn ready(engine: &Path, model: &Path, cancel: &Cancel) -> Result<(String, String)> {
        let _guard = STARTING.lock().await;
        if let Ok(mut r) = RUNNING.lock() {
            if let Some(s) = r.as_mut() {
                if s.model == model && matches!(s.child.try_wait(), Ok(None)) {
                    return Ok((s.base.clone(), s.key.clone()));
                }
            }
        }
        stop();
        kill_stale(engine);
        let fail = |e: std::io::Error| AiError::Provider(format!("no se pudo iniciar el motor de IA: {e}"));
        let port = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map_err(fail)?.port();
        let key = random_key();
        let mut cmd = Command::new(engine.join(SERVER_EXE));
        cmd.current_dir(engine)
            .arg("--model")
            .arg(model)
            .args(["--host", "127.0.0.1", "--port", &port.to_string(), "--api-key", &key])
            // Every layer on the GPU (Metal) when there is one; one chat at a time.
            .args(["--n-gpu-layers", "999", "--ctx-size", CONTEXT, "--parallel", "1", "--no-webui", "--offline"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW: no console window next to the app.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(fail)?;
        let _ = std::fs::write(engine.join("server.pid"), child.id().to_string());
        let log = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(err) = child.stderr.take() {
            let log = log.clone();
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(err).lines().map_while(std::result::Result::ok) {
                    tracing::trace!(target: "llama", "{line}");
                    if let Ok(mut l) = log.lock() {
                        if l.len() == 40 {
                            l.pop_front();
                        }
                        l.push_back(line);
                    }
                }
            });
        }
        let base = format!("http://127.0.0.1:{port}");
        if let Ok(mut r) = RUNNING.lock() {
            *r = Some(Running { child, engine: engine.to_path_buf(), model: model.to_path_buf(), base: base.clone(), key: key.clone(), log });
        }
        if let Err(e) = wait_ready(&base, cancel).await {
            stop();
            return Err(e);
        }
        Ok((base, key))
    }

    /// Wait for `/health` (the model loads first); fails if the server exits.
    async fn wait_ready(base: &str, cancel: &Cancel) -> Result<()> {
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if cancel.is_cancelled() {
                return Err(AiError::Cancelled);
            }
            if let Some(e) = exited() {
                return Err(e);
            }
            if let Ok(r) = client.get(format!("{base}/health")).timeout(Duration::from_secs(2)).send().await {
                if r.status().is_success() {
                    return Ok(());
                }
            }
            if Instant::now() > deadline {
                return Err(AiError::Provider("el motor de IA no terminó de cargar el modelo en 3 minutos".into()));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Why the server stopped, if it did.
    fn exited() -> Option<AiError> {
        let mut r = RUNNING.lock().ok()?;
        let s = r.as_mut()?;
        let status = s.child.try_wait().ok()??;
        // STATUS_DLL_NOT_FOUND: the Visual C++ runtime is missing.
        if cfg!(windows) && status.code() == Some(0xC000_0135_u32 as i32) {
            return Some(AiError::Provider(
                "al motor de IA le falta el runtime de Visual C++ de Microsoft: instalá «Microsoft Visual C++ Redistributable» y volvé a intentar".into(),
            ));
        }
        let tail: Vec<String> = s.log.lock().map(|l| l.iter().rev().take(3).rev().cloned().collect()).unwrap_or_default();
        Some(AiError::Provider(format!("el motor de IA se cerró al cargar el modelo ({status}): {}", tail.join(" · "))))
    }

    /// A server left behind by a previous run that didn't close cleanly.
    fn kill_stale(engine: &Path) {
        let file = engine.join("server.pid");
        let Ok(pid) = std::fs::read_to_string(&file) else { return };
        let _ = std::fs::remove_file(&file);
        let Ok(pid) = pid.trim().parse::<u32>() else { return };
        let pid = pid.to_string();
        #[cfg(unix)]
        {
            let out = Command::new("ps").args(["-p", &pid, "-o", "comm="]).output();
            if out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("llama-server")) {
                let _ = Command::new("kill").arg(&pid).status();
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let out = Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]).creation_flags(0x0800_0000).output();
            if out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("llama-server")) {
                let _ = Command::new("taskkill").args(["/PID", &pid, "/F"]).creation_flags(0x0800_0000).status();
            }
        }
    }

    /// 128 random bits (std's hasher keys come from the OS).
    fn random_key() -> String {
        use std::hash::{BuildHasher, Hasher};
        (0..2).map(|_| format!("{:016x}", std::collections::hash_map::RandomState::new().build_hasher().finish())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent() {
        for m in CATALOG {
            assert!(m.url.ends_with(m.file));
            assert_eq!(m.sha256.len(), 64);
        }
        assert!(catalog_model("qwen2.5-coder-3b").is_some());
        if let Some(e) = ENGINE {
            assert!(e.url.ends_with(e.file));
            assert!(e.url.contains(&format!("/download/{ENGINE_TAG}/")));
            assert_eq!(e.sha256.len(), 64);
        }
    }

    #[test]
    fn archive_paths_lose_the_top_folder() {
        let p = |s: &str| inner_path(Path::new(s));
        assert_eq!(p("llama-b11213/llama-server"), Some(PathBuf::from("llama-server")));
        assert_eq!(p("llama-b11213/"), None);
        assert_eq!(p("llama-batched-bench-impl.dll"), Some(PathBuf::from("llama-batched-bench-impl.dll")));
        assert_eq!(p("llama-b11213/../evil"), None);
        assert_eq!(p("/etc/passwd"), None);
    }

    /// Resumable download with SHA-256 check, against a local HTTP server
    /// that honours `Range` (a fake 1 KB "model").
    #[tokio::test]
    async fn downloads_resume_and_verify() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let content: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        let sha: String = Sha256::digest(&content).iter().map(|b| format!("{b:02x}")).collect();
        let sha: &'static str = Box::leak(sha.into_boxed_str());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let body = content.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap();
                    let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                    let from: usize = req.split("range: bytes=").nth(1).and_then(|r| r.split('-').next()).and_then(|x| x.parse().ok()).unwrap_or(0);
                    let part = &body[from..];
                    let status = if from > 0 { "206 Partial Content" } else { "200 OK" };
                    let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", part.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(part).await;
                });
            }
        });
        let d = tempfile::tempdir().unwrap();
        let m = CatalogModel { id: "t", label: "t", detail: "", file: "t.gguf", url: "", size: 1024, sha256: sha, min_ram_gb: 0 };
        // A previous, interrupted download.
        std::fs::write(d.path().join("t.gguf.part"), &content[..300]).unwrap();
        let seen = std::sync::Mutex::new(Vec::new());
        let p = download_from(d.path(), &m, &format!("http://{addr}/t.gguf"), &|a, _| seen.lock().unwrap().push(a), &Cancel::new())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), content);
        assert_eq!(seen.lock().unwrap()[0], 300, "resumed from the partial file");
        assert_eq!(installed(d.path()).len(), 0, "not in the catalog");

        // A corrupt download is refused and removed.
        let bad = CatalogModel { sha256: "00", file: "u.gguf", ..m };
        let e = download_from(d.path(), &bad, &format!("http://{addr}/u.gguf"), &|_, _| {}, &Cancel::new()).await.unwrap_err();
        assert!(e.to_string().contains("SHA-256"));
        assert!(!d.path().join("u.gguf.part").exists());
    }

    /// End to end with the real engine and a real model: downloads llama.cpp
    /// into the folder and chats. `DBINE_TEST_MODELS=<folder with a catalog
    /// model> cargo test -p dbine-ai -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn real_engine_chat() {
        let dir = PathBuf::from(std::env::var("DBINE_TEST_MODELS").expect("DBINE_TEST_MODELS"));
        let m = installed(&dir).first().copied().expect("a catalog model in DBINE_TEST_MODELS");
        let cancel = Cancel::new();
        ensure_engine(&dir, &|_, _| {}, &cancel).await.unwrap();
        assert!(engine_installed(&dir));
        let req = ChatRequest {
            kind: crate::ProviderKind::Embedded,
            model: Some(m.id.into()),
            system: "Respondé solo con SQL.".into(),
            messages: vec![crate::ChatMessage { role: "user".into(), content: "Contá las filas de la tabla clientes.".into() }],
        };
        let pieces = std::sync::Mutex::new(0);
        let text = chat(&dir, &req, &|_| *pieces.lock().unwrap() += 1, &cancel).await.unwrap();
        assert!(text.to_lowercase().contains("count"), "{text}");
        assert!(*pieces.lock().unwrap() > 1, "streamed");
        // The second chat reuses the running server.
        chat(&dir, &req, &|_| {}, &cancel).await.unwrap();
        shutdown();
    }
}
