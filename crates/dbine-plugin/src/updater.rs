//! Driver updates: drivers are released apart from the app, and an
//! installed app picks up new versions of the drivers it already has
//! (docs/drivers-bajo-demanda.md).
//!
//! The catalog the app carries is each driver crate's floor: the version it
//! was released with, always available. The signed index
//! (`index-<target>.json` + `.sig`, beside the hosts) lists every version
//! published; [`index::resolve`] picks the one to run. The index is fetched
//! a little after start, every 6 hours and on demand, verified with the
//! updater's key and cached; a connection never waits for it.
//!
//! A newer version of an installed driver downloads in the background and
//! becomes active when it's complete: the next connection starts its host
//! while open sessions stay on the old one. A host that fails to start, or
//! says it's another version, is marked bad here and the previous version
//! (or the floor) takes its place.
//!
//! - `DBINE_DRIVERS_DIR`: the hosts are in that folder (no internet); the
//!   floor only, nothing is fetched.
//! - `DBINE_DRIVERS_INDEX_URL`: another index (a mirror, local tests); its
//!   files are next to it. Signed all the same.
//! - `DBINE_DRIVERS_PUBKEY` (debug builds only): another key, for tests
//!   with a throwaway one.

use crate::index::{self, Choice, DriverId, Index, IndexEntry};
use crate::install::{self, exe_name, Catalog, HostAsset};
use crate::proto::DriverMeta;
use crate::remote::{HostSource, Launcher, Target};
use crate::state::{write_atomic, LocalState};
use async_trait::async_trait;
use dbine_driver::{Error, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const INDEX_URL_ENV: &str = "DBINE_DRIVERS_INDEX_URL";
pub const PUBKEY_ENV: &str = "DBINE_DRIVERS_PUBKEY";

/// When the first check runs after start, and how often after it.
const FIRST_CHECK: Duration = Duration::from_secs(10);
const EVERY: Duration = Duration::from_secs(6 * 3600);

/// What the settings page says about a driver crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PkgStatus {
    UpToDate,
    /// A newer version is downloading in the background.
    Downloading,
    /// A newer version is ready: the next connection uses it (open ones
    /// stay on the running one).
    ReadyNextConnection,
    /// There's a newer version, for a newer app.
    NeedsApp { min_app: String },
    /// The version `from` failed (`reason`, "user" when the user went
    /// back) and the one before it runs.
    RolledBack { from: String, reason: String },
    /// The version in use has options this session didn't load: restart.
    RestartForNewOptions,
}

/// A driver crate, for the settings page.
#[derive(Debug, Clone, Serialize)]
pub struct PkgInfo {
    /// The version in use (plain semver), or the one a download would get.
    pub version: String,
    /// Bytes on disk of the version in use, when installed.
    pub installed: Option<u64>,
    /// The newest this app can run (plain semver).
    pub available: String,
    /// Download size of `available`.
    pub size: u64,
    /// The version a rollback goes back to (plain semver).
    pub previous: Option<String>,
    pub status: PkgStatus,
    /// The app version a newer driver needs.
    pub min_app_needed: Option<String>,
}

/// Plain semver of a driver id ("1.2.0+p1.e3" → "1.2.0").
pub fn plain(id: &str) -> String {
    id.split('+').next().unwrap_or_default().to_string()
}

struct Inner {
    state: LocalState,
    index: Option<Index>,
    downloading: HashSet<String>,
}

pub struct Updater {
    catalog: Catalog,
    /// What the floors' drivers say about themselves: parsed, and as
    /// published (to compare with other versions').
    metas: Vec<DriverMeta>,
    raw: Vec<serde_json::Value>,
    dir: PathBuf,
    app: semver::Version,
    pubkey: String,
    index_url: Option<String>,
    /// `DBINE_DRIVERS_DIR`: the floor only, from that folder.
    fixed_dir: Option<PathBuf>,
    inner: Mutex<Inner>,
    /// Driver crate → the manifest its drivers were loaded from at start.
    loaded: Mutex<HashMap<String, serde_json::Value>>,
    launchers: Mutex<HashMap<String, Arc<Launcher>>>,
    on_change: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl Updater {
    /// The updater of `catalog` (its floors), whose drivers' manifests are
    /// `raw` (as the app carries them), reading the environment.
    pub fn new(catalog: Catalog, raw: Vec<serde_json::Value>) -> Arc<Updater> {
        let pubkey = match std::env::var(PUBKEY_ENV) {
            Ok(k) if cfg!(debug_assertions) && !k.is_empty() => k,
            _ => index::UPDATER_PUBKEY.to_string(),
        };
        let index_url = std::env::var(INDEX_URL_ENV).ok().filter(|u| !u.is_empty()).or_else(|| {
            (!catalog.base_url.is_empty()).then(|| format!("{}/index-{}.json", catalog.base_url.trim_end_matches('/'), catalog.target))
        });
        let fixed_dir = std::env::var_os("DBINE_DRIVERS_DIR").map(PathBuf::from);
        let app = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| semver::Version::new(0, 0, 0));
        Updater::with(catalog, raw, install::hosts_dir(), app, pubkey, index_url, fixed_dir)
    }

    pub fn with(
        catalog: Catalog,
        raw: Vec<serde_json::Value>,
        dir: PathBuf,
        app: semver::Version,
        pubkey: String,
        index_url: Option<String>,
        fixed_dir: Option<PathBuf>,
    ) -> Arc<Updater> {
        let metas = raw.iter().filter_map(|v| serde_json::from_value::<DriverMeta>(v.clone()).ok()).collect();
        let state = LocalState::load(&dir.join("state.json"));
        let mut up = Updater {
            catalog,
            metas,
            raw,
            dir,
            app,
            pubkey,
            index_url,
            fixed_dir,
            inner: Mutex::new(Inner { state, index: None, downloading: HashSet::new() }),
            loaded: Mutex::new(HashMap::new()),
            launchers: Mutex::new(HashMap::new()),
            on_change: OnceLock::new(),
        };
        if up.fixed_dir.is_none() {
            let seq = up.inner.get_mut().unwrap().state.seq;
            let index = up.load_cached(seq);
            up.inner.get_mut().unwrap().index = index;
        }
        Arc::new(up)
    }

    /// Called whenever what [`Updater::info`] says may have changed.
    pub fn set_on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.on_change.set(Box::new(f));
    }

    fn changed(&self) {
        if let Some(f) = self.on_change.get() {
            f();
        }
    }

    fn cache_paths(&self) -> (PathBuf, PathBuf) {
        let name = format!("index-{}.json", self.catalog.target);
        (self.dir.join(&name), self.dir.join(format!("{name}.sig")))
    }

    /// The cached index, if it still verifies (and isn't older than `seq`).
    fn load_cached(&self, seq: u64) -> Option<Index> {
        let (path, sig) = self.cache_paths();
        let bytes = std::fs::read(path).ok()?;
        let sig = std::fs::read_to_string(sig).ok()?;
        match index::verify(&bytes, &sig, &self.pubkey, seq) {
            Ok(i) if i.target == self.catalog.target => Some(i),
            Ok(i) => {
                tracing::warn!(target: "dbine_plugin", "índice de drivers en caché de otra plataforma ({})", i.target);
                None
            }
            Err(e) => {
                tracing::warn!(target: "dbine_plugin", "índice de drivers en caché descartado: {e}");
                None
            }
        }
    }

    fn save(&self, inner: &Inner) {
        if let Err(e) = inner.state.save(&self.dir.join("state.json")) {
            tracing::warn!(target: "dbine_plugin", "no se pudo guardar el estado de los drivers: {e}");
        }
    }

    pub fn packages(&self) -> Vec<String> {
        let mut v: Vec<String> = self.catalog.hosts.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn floor(&self, package: &str) -> Option<String> {
        self.catalog.hosts.get(package).map(|h| h.version.clone())
    }

    fn exe(&self, package: &str, id: &str) -> PathBuf {
        self.dir.join(exe_name(package, id))
    }

    fn choice(&self, inner: &Inner, package: &str) -> Option<Choice> {
        let floor = DriverId::parse(&self.floor(package)?)?;
        let none = BTreeMap::new();
        let bad = inner.state.packages.get(package).map(|p| &p.bad).unwrap_or(&none);
        Some(index::resolve(&floor, inner.index.as_ref().and_then(|i| i.drivers.get(package)), &self.app, bad))
    }

    /// `id` can serve new connections: same protocol and epoch as the
    /// floor, not older, not bad, on disk.
    fn usable(&self, inner: &Inner, package: &str, id: &str) -> bool {
        let (Some(floor), Some(d)) = (self.floor(package).and_then(|f| DriverId::parse(&f)), DriverId::parse(id)) else { return false };
        let bad = inner.state.packages.get(package).is_some_and(|p| p.bad.contains_key(id));
        d.same_line(&floor) && d.version >= floor.version && !bad && self.exe(package, id).is_file()
    }

    /// The version new connections use, if one is on disk: the active one,
    /// else the floor, else the previous one.
    fn active(&self, inner: &Inner, package: &str) -> Option<String> {
        let p = inner.state.packages.get(package);
        let floor = self.floor(package);
        [p.and_then(|p| p.active.clone()), floor, p.and_then(|p| p.previous.clone())].into_iter().flatten().find(|id| self.usable(inner, package, id))
    }

    /// Any version of the driver is on disk.
    fn installed(&self, inner: &Inner, package: &str) -> bool {
        let p = inner.state.packages.get(package);
        [p.and_then(|p| p.active.clone()), self.floor(package), p.and_then(|p| p.previous.clone())]
            .into_iter()
            .flatten()
            .any(|id| self.exe(package, &id).is_file())
    }

    /// Where `id` of `package` is published: (base URL, asset).
    fn asset(&self, inner: &Inner, package: &str, id: &str) -> Option<(String, HostAsset)> {
        if self.floor(package).as_deref() == Some(id) {
            return Some((self.catalog.base_url.clone(), self.catalog.hosts.get(package)?.clone()));
        }
        let entry = inner.index.as_ref()?.drivers.get(package)?.get(id)?;
        let base = self.index_url.as_deref()?.rsplit_once('/')?.0.to_string();
        Some((base, HostAsset::from_entry(id, entry)))
    }

    /// The driver ids a crate serves and the engine it's named after.
    pub fn label(&self, package: &str) -> String {
        let metas: Vec<&DriverMeta> = self.metas.iter().filter(|m| m.package == package).collect();
        metas.iter().find(|m| m.info.id == package).or(metas.first()).map(|m| m.info.name.to_string()).unwrap_or_else(|| package.to_string())
    }

    fn driver_ids(&self, package: &str) -> Vec<String> {
        self.metas.iter().filter(|m| m.package == package).map(|m| m.info.id.to_string()).collect()
    }

    /// The manifest of `id`, as published.
    fn manifest(&self, inner: &Inner, package: &str, id: &str) -> Option<serde_json::Value> {
        if self.floor(package).as_deref() == Some(id) {
            let v: Vec<serde_json::Value> = self.raw.iter().filter(|m| m.get("package").and_then(|p| p.as_str()) == Some(package)).cloned().collect();
            return Some(serde_json::Value::Array(v));
        }
        let e: &IndexEntry = inner.index.as_ref()?.drivers.get(package)?.get(id)?;
        (!e.manifest.is_null()).then(|| e.manifest.clone())
    }

    /// What the drivers say about themselves, for this session: per crate,
    /// the version new connections will use (installed, or what a download
    /// would get), from the cached index; the floor's when it's unreadable.
    /// Called once, at start.
    pub fn startup_metas(&self) -> Vec<DriverMeta> {
        let inner = self.inner.lock().unwrap();
        let mut loaded = self.loaded.lock().unwrap();
        let mut out = Vec::new();
        for package in self.packages() {
            let floor = self.floor(&package).unwrap_or_default();
            let id = self.active(&inner, &package).or_else(|| self.choice(&inner, &package).map(|c| c.id.to_string())).unwrap_or(floor.clone());
            let theirs = (id != floor)
                .then(|| self.manifest(&inner, &package, &id))
                .flatten()
                .and_then(|v| serde_json::from_value::<Vec<DriverMeta>>(v.clone()).ok().map(|m| (v, m)))
                .filter(|(_, m)| !m.is_empty() && m.iter().all(|x| x.package == package));
            match theirs {
                Some((v, m)) => {
                    loaded.insert(package.clone(), v);
                    out.extend(m);
                }
                None => {
                    if let Some(v) = self.manifest(&inner, &package, &floor) {
                        loaded.insert(package.clone(), v);
                    }
                    out.extend(self.metas.iter().filter(|m| m.package == package).cloned());
                }
            }
        }
        out
    }

    /// Remove the hosts nothing uses: per crate, keep the floor, the active
    /// and the previous version, and partial downloads of the floor and of
    /// the version the index picks. Call it at start.
    pub fn gc(&self) {
        if self.fixed_dir.is_some() {
            return;
        }
        install::gc(&self.dir, &self.keep_set());
    }

    fn keep_set(&self) -> HashSet<String> {
        let inner = self.inner.lock().unwrap();
        let mut keep = HashSet::new();
        for package in self.packages() {
            let p = inner.state.packages.get(&package);
            let floor = self.floor(&package);
            for id in [floor.clone(), p.and_then(|p| p.active.clone()), p.and_then(|p| p.previous.clone())].into_iter().flatten() {
                keep.insert(exe_name(&package, &id));
            }
            let picked = self.choice(&inner, &package).map(|c| c.id.to_string());
            for id in [floor, picked].into_iter().flatten() {
                if let Some((_, a)) = self.asset(&inner, &package, &id) {
                    keep.insert(format!("{}.part", a.file));
                }
            }
        }
        keep
    }

    /// The launcher of `package`'s host (one per crate).
    pub fn launcher(self: &Arc<Self>, package: &str) -> Arc<Launcher> {
        self.launchers
            .lock()
            .unwrap()
            .entry(package.to_string())
            .or_insert_with(|| {
                let source = Arc::new(PkgSource { up: self.clone(), package: package.to_string() });
                Launcher::new(package, Some(dbine_driver::runtime::components_dir()), source)
            })
            .clone()
    }

    /// The host new connections to `package` use: the active version; on
    /// first use, download the one the index picks (the floor if that
    /// fails) and make it active.
    async fn resolve(&self, package: &str) -> Result<Target> {
        let label = self.label(package);
        if let Some(dir) = &self.fixed_dir {
            let floor = self.floor(package).ok_or_else(|| Error::Unsupported(format!("el driver de {label} no se publica para esta plataforma")))?;
            let exe = dir.join(exe_name(package, &floor));
            return if exe.is_file() {
                Ok(Target { exe, id: None, check_version: false })
            } else {
                Err(Error::Connect(format!("no está el driver de {label} en DBINE_DRIVERS_DIR ({})", exe.display())))
            };
        }
        let floor = self
            .floor(package)
            .ok_or_else(|| Error::Unsupported(format!("el driver de {label} no se publica para esta plataforma ({})", self.catalog.target)))?;
        let target = |id: String| Target { exe: self.exe(package, &id), check_version: id != floor, id: Some(id) };
        let (active, picked) = {
            let inner = self.inner.lock().unwrap();
            let picked = self.choice(&inner, package).map(|c| c.id.to_string()).filter(|id| *id != floor);
            (self.active(&inner, package), picked.and_then(|id| self.asset(&inner, package, &id).map(|a| (id, a))))
        };
        if let Some(id) = active {
            return Ok(target(id));
        }
        if let Some((id, (base, asset))) = picked {
            match install::ensure_asset(&self.dir, &base, package, &asset, &label, self.driver_ids(package)).await {
                Ok(_) => {
                    self.activate(package, &id);
                    return Ok(target(id));
                }
                Err(e) => tracing::warn!(target: "dbine_plugin", "driver «{package}» {id}: {e}; se usa {floor}"),
            }
        }
        let asset = self.catalog.hosts.get(package).cloned().ok_or_else(|| Error::State("sin catálogo".into()))?;
        install::ensure_asset(&self.dir, &self.catalog.base_url, package, &asset, &label, self.driver_ids(package)).await?;
        self.activate(package, &floor);
        Ok(target(floor.clone()))
    }

    fn activate(&self, package: &str, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.state.activate(package, id);
        self.save(&inner);
        drop(inner);
        self.changed();
    }

    /// `id` of `package` doesn't work here (`reason`): never pick it again
    /// and, if it was the active one, go back to the previous one.
    pub fn mark_bad(&self, package: &str, id: &str, reason: &str) {
        let floor = self.floor(package);
        let mut inner = self.inner.lock().unwrap();
        let p = inner.state.pkg(package);
        p.bad.insert(id.to_string(), reason.to_string());
        if p.active.as_deref() == Some(id) {
            p.active = p.previous.take();
            p.rolled_back_from = Some(id.to_string());
        }
        self.save(&inner);
        drop(inner);
        // The floor stays: it's the last resort. A file in use stays too
        // (Windows); the next start's cleanup takes it.
        if floor.as_deref() != Some(id) {
            let _ = std::fs::remove_file(self.exe(package, id));
        }
        self.changed();
    }

    /// The user goes back from the version in use to the one before.
    pub fn rollback(&self, package: &str) -> std::result::Result<(), String> {
        let current = {
            let inner = self.inner.lock().unwrap();
            self.active(&inner, package)
        };
        let Some(current) = current else { return Err(format!("el driver de {} no está descargado", self.label(package))) };
        if self.floor(package).as_deref() == Some(current.as_str()) {
            return Err(format!("el driver de {} ya está en la versión que trae la app", self.label(package)));
        }
        self.mark_bad(package, &current, "user");
        Ok(())
    }

    /// Download `package` now (the settings page's "descargar").
    pub async fn install(&self, package: &str) -> Result<()> {
        self.resolve(package).await.map(|_| ())
    }

    /// Remove every version of `package` on disk.
    pub fn remove(&self, package: &str) -> std::io::Result<()> {
        let r = install::remove(&self.dir, package);
        let mut inner = self.inner.lock().unwrap();
        let p = inner.state.pkg(package);
        p.active = None;
        p.previous = None;
        p.rolled_back_from = None;
        self.save(&inner);
        drop(inner);
        self.changed();
        r
    }

    /// What the settings page shows for `package`.
    pub fn info(&self, package: &str) -> Option<PkgInfo> {
        let floor = self.floor(package)?;
        let inner = self.inner.lock().unwrap();
        let choice = self.choice(&inner, package)?;
        let available = choice.id.to_string();
        let active = self.active(&inner, package);
        let p = inner.state.packages.get(package);
        let size = self.asset(&inner, package, &available).map(|(_, a)| a.size).unwrap_or(0);
        let running = self.launchers.lock().unwrap().get(package).and_then(|l| l.running_version());
        let loaded = self.loaded.lock().unwrap().get(package).cloned();
        let min_app_needed = choice.needs_app.as_ref().map(|(_, m)| m.clone());
        let status = if inner.downloading.contains(package) {
            PkgStatus::Downloading
        } else if let Some(from) = p.and_then(|p| p.rolled_back_from.clone()).filter(|f| active.as_deref() != Some(f.as_str())) {
            let reason = p.and_then(|p| p.bad.get(&from).cloned()).unwrap_or_default();
            PkgStatus::RolledBack { from: plain(&from), reason }
        } else if running.as_ref().zip(active.as_ref()).is_some_and(|(r, a)| r != a && self.fixed_dir.is_none()) {
            PkgStatus::ReadyNextConnection
        } else if active.as_ref().and_then(|a| self.manifest(&inner, package, a)).zip(loaded).is_some_and(|(now, then)| now != then) {
            PkgStatus::RestartForNewOptions
        } else if let Some(m) = &min_app_needed {
            PkgStatus::NeedsApp { min_app: m.clone() }
        } else {
            PkgStatus::UpToDate
        };
        let in_use = active.clone().unwrap_or_else(|| available.clone());
        Some(PkgInfo {
            version: plain(&in_use),
            installed: active.as_ref().and_then(|a| install::installed_size(&self.dir, package, a)),
            available: plain(&available),
            size,
            previous: active.filter(|a| *a != floor).map(|_| p.and_then(|p| p.previous.clone()).unwrap_or(floor)).map(|x| plain(&x)),
            status,
            min_app_needed,
        })
    }

    /// Check now: fetch the index, verify and cache it, and download in the
    /// background the newer versions of the installed drivers. Returns once
    /// the index is in (not the downloads).
    pub async fn check(self: &Arc<Self>) -> std::result::Result<(), String> {
        if self.fixed_dir.is_some() || self.catalog.hosts.is_empty() {
            return Ok(());
        }
        let url = self.index_url.clone().ok_or("no hay de dónde buscar actualizaciones de drivers")?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        let get = |u: String| {
            let client = client.clone();
            async move {
                let resp = client.get(&u).send().await.map_err(|e| format!("¿hay conexión a internet? ({e})"))?;
                if !resp.status().is_success() {
                    return Err(format!("HTTP {} ({u})", resp.status().as_u16()));
                }
                resp.bytes().await.map_err(|e| e.to_string())
            }
        };
        let (bytes, sig) = tokio::try_join!(get(url.clone()), get(format!("{url}.sig")))?;
        let sig = String::from_utf8_lossy(&sig).to_string();
        let seq = self.inner.lock().unwrap().state.seq;
        let index = index::verify(&bytes, &sig, &self.pubkey, seq).map_err(|e| e.to_string())?;
        if index.target != self.catalog.target {
            return Err(format!("el índice de drivers es de {}, no de {}", index.target, self.catalog.target));
        }
        let (path, sig_path) = self.cache_paths();
        let cached = std::fs::create_dir_all(&self.dir).and_then(|_| write_atomic(&path, &bytes)).and_then(|_| write_atomic(&sig_path, sig.as_bytes()));
        if let Err(e) = cached {
            tracing::warn!(target: "dbine_plugin", "no se pudo guardar el índice de drivers: {e}");
        }
        {
            let mut inner = self.inner.lock().unwrap();
            inner.state.seq = index.seq;
            inner.index = Some(index);
            self.save(&inner);
        }
        self.changed();
        let me = self.clone();
        tokio::spawn(async move { me.download_updates().await });
        Ok(())
    }

    /// Per installed driver whose picked version isn't the active one: get
    /// it (one at a time) and make it active.
    async fn download_updates(&self) {
        let todo: Vec<(String, String, String, HostAsset)> = {
            let mut inner = self.inner.lock().unwrap();
            let mut todo = Vec::new();
            for package in self.packages() {
                if inner.downloading.contains(&package) || !self.installed(&inner, &package) {
                    continue;
                }
                let Some(choice) = self.choice(&inner, &package) else { continue };
                let id = choice.id.to_string();
                if self.active(&inner, &package).as_deref() == Some(id.as_str()) {
                    continue;
                }
                if let Some((base, asset)) = self.asset(&inner, &package, &id) {
                    todo.push((package, id, base, asset));
                }
            }
            for (p, ..) in &todo {
                inner.downloading.insert(p.clone());
            }
            todo
        };
        if todo.is_empty() {
            return;
        }
        self.changed();
        for (package, id, base, asset) in todo {
            let label = self.label(&package);
            let r = install::ensure_asset(&self.dir, &base, &package, &asset, &label, Vec::new()).await;
            {
                let mut inner = self.inner.lock().unwrap();
                inner.downloading.remove(&package);
                match r {
                    // Still the pick (a newer index may have come meanwhile).
                    Ok(_) if self.choice(&inner, &package).is_some_and(|c| c.id.to_string() == id) => {
                        tracing::info!(target: "dbine_plugin", "driver «{package}» {id} descargado: lo usan las conexiones nuevas");
                        inner.state.activate(&package, &id);
                        self.save(&inner);
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(target: "dbine_plugin", "no se pudo actualizar el driver «{package}» a {id}: {e}"),
                }
            }
            self.changed();
        }
    }

    /// The periodic check: a little after start, then every 6 hours.
    pub async fn run(self: Arc<Self>) {
        if self.fixed_dir.is_some() || self.catalog.hosts.is_empty() {
            return;
        }
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            if let Err(e) = self.check().await {
                tracing::info!(target: "dbine_plugin", "no se pudieron buscar actualizaciones de drivers: {e}");
            }
            tokio::time::sleep(EVERY).await;
        }
    }
}

struct PkgSource {
    up: Arc<Updater>,
    package: String,
}

#[async_trait]
impl HostSource for PkgSource {
    async fn resolve(&self) -> Result<Target> {
        self.up.resolve(&self.package).await
    }
    fn bad(&self, id: &str, reason: &str) {
        self.up.mark_bad(&self.package, id, reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const FLOOR: &str = "1.0.0+p1.e3";

    fn catalog() -> Catalog {
        let asset = HostAsset { version: FLOOR.into(), file: "dbine-driver-pg-1.0.0+p1.e3-t.gz".into(), size: 10, sha256: "00".into() };
        Catalog { target: "t".into(), base_url: "https://x/drivers".into(), hosts: HashMap::from([("pg".to_string(), asset)]) }
    }

    fn entry(file: &str, min_app: Option<&str>) -> IndexEntry {
        IndexEntry {
            file: file.into(),
            size: 20,
            sha256: "11".into(),
            manifest: serde_json::json!([{"package": "pg", "new": file}]),
            min_app: min_app.map(str::to_string),
            yanked: None,
            published_at: None,
        }
    }

    fn updater(dir: &Path) -> Arc<Updater> {
        Updater::with(catalog(), vec![], dir.to_path_buf(), semver::Version::new(0, 2, 0), String::new(), Some("http://mirror/d/index-t.json".into()), None)
    }

    fn with_index(up: &Updater, entries: &[(&str, IndexEntry)]) {
        let mut i = Index { target: "t".into(), schema: 2, seq: 1, drivers: BTreeMap::new() };
        i.drivers.insert("pg".into(), entries.iter().map(|(k, e)| (k.to_string(), e.clone())).collect());
        up.inner.lock().unwrap().index = Some(i);
    }

    fn touch(dir: &Path, id: &str) {
        std::fs::write(dir.join(exe_name("pg", id)), b"x").unwrap();
    }

    #[test]
    fn active_falls_back_and_info_reports() {
        let dir = tempfile::tempdir().unwrap();
        let up = updater(dir.path());
        // Nothing on disk, no index: the floor, to download.
        let i = up.info("pg").unwrap();
        assert_eq!((i.version.as_str(), i.installed, i.available.as_str(), i.size), ("1.0.0", None, "1.0.0", 10));
        assert_eq!(i.status, PkgStatus::UpToDate);

        with_index(&up, &[("1.0.1+p1.e3", entry("f101", None)), ("1.0.2+p1.e3", entry("f102", Some("0.3.0")))]);
        let i = up.info("pg").unwrap();
        assert_eq!((i.available.as_str(), i.size, i.min_app_needed.as_deref()), ("1.0.1", 20, Some("0.3.0")));
        assert_eq!(i.status, PkgStatus::NeedsApp { min_app: "0.3.0".into() });
        // Its files come from beside the index.
        let inner = up.inner.lock().unwrap();
        assert_eq!(up.asset(&inner, "pg", "1.0.1+p1.e3").unwrap().0, "http://mirror/d");
        assert_eq!(up.asset(&inner, "pg", FLOOR).unwrap().0, "https://x/drivers");
        drop(inner);

        // The floor on disk, 1.0.1 downloaded and active.
        touch(dir.path(), FLOOR);
        touch(dir.path(), "1.0.1+p1.e3");
        up.inner.lock().unwrap().state.activate("pg", FLOOR);
        up.inner.lock().unwrap().state.activate("pg", "1.0.1+p1.e3");
        let i = up.info("pg").unwrap();
        assert_eq!((i.version.as_str(), i.installed, i.previous.as_deref()), ("1.0.1", Some(1), Some("1.0.0")));

        // It fails: back to the floor, and it's never picked again.
        up.mark_bad("pg", "1.0.1+p1.e3", "exited");
        assert!(!dir.path().join(exe_name("pg", "1.0.1+p1.e3")).exists());
        let i = up.info("pg").unwrap();
        assert_eq!((i.version.as_str(), i.available.as_str()), ("1.0.0", "1.0.0"));
        assert_eq!(i.status, PkgStatus::RolledBack { from: "1.0.1".into(), reason: "exited".into() });
        // The state was saved.
        let saved = LocalState::load(&dir.path().join("state.json"));
        assert_eq!(saved.packages["pg"].active.as_deref(), Some(FLOOR));
        assert_eq!(saved.packages["pg"].bad["1.0.1+p1.e3"], "exited");
        // No going back from the floor.
        assert!(up.rollback("pg").is_err());
    }

    #[test]
    fn active_is_never_older_than_the_floor_nor_of_another_line() {
        let dir = tempfile::tempdir().unwrap();
        let up = updater(dir.path());
        touch(dir.path(), "0.9.0+p1.e3");
        touch(dir.path(), "1.5.0+p1.e2");
        up.inner.lock().unwrap().state.activate("pg", "0.9.0+p1.e3");
        up.inner.lock().unwrap().state.activate("pg", "1.5.0+p1.e2");
        let inner = up.inner.lock().unwrap();
        assert_eq!(up.active(&inner, "pg"), None);
        assert!(up.installed(&inner, "pg"));
        drop(inner);
        touch(dir.path(), FLOOR);
        assert_eq!(up.active(&up.inner.lock().unwrap(), "pg").as_deref(), Some(FLOOR));
    }

    #[test]
    fn user_rollback_and_gc_keep_set() {
        let dir = tempfile::tempdir().unwrap();
        let up = updater(dir.path());
        with_index(&up, &[("1.0.1+p1.e3", entry("f101", None)), ("1.0.2+p1.e3", entry("f102", None))]);
        for id in [FLOOR, "1.0.1+p1.e3", "1.0.2+p1.e3"] {
            touch(dir.path(), id);
            up.inner.lock().unwrap().state.activate("pg", id);
        }
        let keep = up.keep_set();
        let mut keep: Vec<&str> = keep.iter().map(String::as_str).collect();
        keep.sort();
        assert_eq!(
            keep,
            [
                "dbine-driver-pg-1.0.0+p1.e3",
                "dbine-driver-pg-1.0.0+p1.e3-t.gz.part",
                "dbine-driver-pg-1.0.1+p1.e3",
                "dbine-driver-pg-1.0.2+p1.e3",
                "f102.part"
            ]
        );
        up.gc();
        assert!(dir.path().join(exe_name("pg", "1.0.1+p1.e3")).exists());

        up.rollback("pg").unwrap();
        let i = up.info("pg").unwrap();
        assert_eq!((i.version.as_str(), i.available.as_str()), ("1.0.1", "1.0.1"));
        assert_eq!(i.status, PkgStatus::RolledBack { from: "1.0.2".into(), reason: "user".into() });
    }

    #[test]
    fn startup_metas_come_from_the_version_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let up = updater(dir.path());
        with_index(&up, &[("1.0.1+p1.e3", entry("f101", None))]);
        touch(dir.path(), "1.0.1+p1.e3");
        up.inner.lock().unwrap().state.activate("pg", "1.0.1+p1.e3");
        // Not a readable manifest (the test's is made up): the floor's (none here).
        assert!(up.startup_metas().is_empty());
        assert_eq!(up.loaded.lock().unwrap()["pg"], serde_json::json!([]));
        // The active one's manifest differs from what was loaded.
        assert_eq!(up.info("pg").unwrap().status, PkgStatus::RestartForNewOptions);
    }

    /// A host that greets as `version` and then reads until the end.
    #[cfg(unix)]
    fn fake_host_gz(version: &str) -> Vec<u8> {
        use std::io::Write;
        let mut ready = Vec::new();
        crate::proto::write_frame(&mut ready, &crate::proto::Ready { protocol: crate::proto::PROTOCOL, version: version.into(), drivers: vec![] }).unwrap();
        let hex: String = ready[4..].iter().map(|b| format!("{b:02x}")).collect();
        let script = format!(
            "#!/bin/sh\nexec python3 -c '\nimport struct, sys\ni, o = sys.stdin.buffer, sys.stdout.buffer\n\
             def frame():\n    h = i.read(4)\n    if len(h) < 4: return None\n    return i.read(struct.unpack(\"<I\", h)[0])\n\
             frame()\nb = bytes.fromhex(\"{hex}\")\no.write(struct.pack(\"<I\", len(b)) + b); o.flush()\n\
             while frame() is not None: pass\n'\n"
        );
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(script.as_bytes()).unwrap();
        enc.finish().unwrap()
    }

    /// A tiny HTTP server for `files` (path → bytes); its base URL.
    fn serve(files: HashMap<String, Vec<u8>>) -> String {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let files = Arc::new(Mutex::new(files));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let files = files.clone();
                std::thread::spawn(move || {
                    let mut r = std::io::BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    r.read_line(&mut line).unwrap();
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap() == 0 || h == "\r\n" {
                            break;
                        }
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let mut w = stream;
                    match files.lock().unwrap().get(&path) {
                        Some(b) => {
                            let _ = write!(w, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len());
                            let _ = w.write_all(b);
                        }
                        None => {
                            let _ = write!(w, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        }
                    }
                });
            }
        });
        base
    }

    const TEST_PK: &str = include_str!("testdata/test.key.pub");

    /// The whole path: first use downloads the floor; the signed index
    /// brings 1.0.1 (1.0.2 needs a newer app, 1.0.3 is yanked), which
    /// downloads in the background and serves the next connection while
    /// the old host stays; restarting offline uses the cached index; an
    /// older index is refused.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_with_a_local_server() {
        use sha2::Digest;
        let dir = tempfile::tempdir().unwrap();
        let floor_gz = fake_host_gz(FLOOR);
        let sha = sha2::Sha256::digest(&floor_gz).iter().map(|b| format!("{b:02x}")).collect::<String>();
        let mut files = HashMap::from([
            ("/floor/floor.gz".to_string(), floor_gz.clone()),
            ("/d/index-t.json".to_string(), include_bytes!("testdata/e2e-index.json").to_vec()),
            ("/d/index-t.json.sig".to_string(), include_bytes!("testdata/e2e-index.json.sig").to_vec()),
            ("/d/e2e-v101.gz".to_string(), include_bytes!("testdata/e2e-v101.gz").to_vec()),
        ]);
        let base = serve(files.clone());
        let catalog = || Catalog {
            target: "t".into(),
            base_url: format!("{base}/floor"),
            hosts: HashMap::from([("pg".to_string(), HostAsset { version: FLOOR.into(), file: "floor.gz".into(), size: floor_gz.len() as u64, sha256: sha.clone() })]),
        };
        let app = semver::Version::new(0, 2, 0);
        let up = Updater::with(catalog(), vec![], dir.path().to_path_buf(), app.clone(), TEST_PK.into(), Some(format!("{base}/d/index-t.json")), None);
        let launcher = up.launcher("pg");

        // First use, no index yet: the floor.
        let old = launcher.get().await.unwrap();
        assert_eq!(old.version(), FLOOR);
        let i = up.info("pg").unwrap();
        assert_eq!((i.version.as_str(), i.status.clone()), ("1.0.0", PkgStatus::UpToDate));
        assert!(i.installed.is_some());

        // The index: 1.0.1 downloads in the background.
        up.check().await.unwrap();
        let mut waited = 0;
        while up.info("pg").unwrap().version != "1.0.1" {
            assert!(waited < 100, "no update: {:?}", up.info("pg"));
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += 1;
        }
        let i = up.info("pg").unwrap();
        assert_eq!((i.available.as_str(), i.previous.as_deref(), i.min_app_needed.as_deref()), ("1.0.1", Some("1.0.0"), Some("9.0.0")));
        assert_eq!(i.status, PkgStatus::ReadyNextConnection);

        // The next connection gets 1.0.1; the old host goes on.
        let new = launcher.get().await.unwrap();
        assert_eq!(new.version(), "1.0.1+p1.e3");
        assert!(old.is_alive() && !Arc::ptr_eq(&old, &new));
        assert_eq!(up.info("pg").unwrap().status, PkgStatus::NeedsApp { min_app: "9.0.0".into() });

        // Restart offline: the cached index (it verifies) and the state.
        let offline = Updater::with(catalog(), vec![], dir.path().to_path_buf(), app.clone(), TEST_PK.into(), Some("http://127.0.0.1:1/d/index-t.json".into()), None);
        let i = offline.info("pg").unwrap();
        assert_eq!((i.version.as_str(), i.available.as_str()), ("1.0.1", "1.0.1"));
        assert!(offline.check().await.is_err());
        // The cleanup at start keeps the floor and both versions.
        offline.gc();
        assert!(dir.path().join(exe_name("pg", FLOOR)).is_file() && dir.path().join(exe_name("pg", "1.0.1+p1.e3")).is_file());

        // A replayed older index (seq 100 < 200) is refused.
        files.insert("/d/index-t.json".into(), include_bytes!("testdata/index-seq100.json").to_vec());
        files.insert("/d/index-t.json.sig".into(), include_bytes!("testdata/index-seq100.json.sig").to_vec());
        let old_server = serve(files);
        let replay = Updater::with(catalog(), vec![], dir.path().to_path_buf(), app, TEST_PK.into(), Some(format!("{old_server}/d/index-t.json")), None);
        let err = replay.check().await.unwrap_err();
        assert!(err.contains("más viejo"), "{err}");
        assert_eq!(replay.info("pg").unwrap().available, "1.0.1");

        // A cached index that no longer verifies (another key) is ignored.
        let other_key = Updater::with(catalog(), vec![], dir.path().to_path_buf(), semver::Version::new(0, 2, 0), index::UPDATER_PUBKEY.into(), None, None);
        assert_eq!(other_key.info("pg").unwrap().available, "1.0.0");
    }
}
