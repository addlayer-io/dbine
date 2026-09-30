//! What to do on each sync, and the backup's content.
//!
//! The engine compares the state's revision with the one it last uploaded
//! ("dirty") and the stored backup's revision with the one it last saw
//! ("changed elsewhere"):
//!
//! | local | remote  | action |
//! |-------|---------|--------|
//! | clean | same    | nothing |
//! | dirty | same    | upload |
//! | clean | changed | download (restore) |
//! | dirty | changed | the newest wins; the other one is kept |
//!
//! Nothing is lost silently: before a restore replaces the local state, a
//! copy of it is saved (encrypted) in the local backups folder; before an
//! upload replaces another machine's backup, that one is kept in the cloud
//! as `dbine-backup.previous.json`.

use crate::crypto::{self, Header, Kdf, Key};
use crate::provider::CloudStore;
use crate::{Result, SyncError, BACKUP_FILE, PREVIOUS_FILE};
use dbine_core::secrets::Secrets;
use dbine_core::{StateSnapshot, StateStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Reads and writes connections' secrets (the OS keychain in the app).
pub trait SecretsIo: Send + Sync {
    fn read(&self, connection_id: &str) -> dbine_core::Result<Secrets>;
    fn write(&self, connection_id: &str, secrets: &Secrets) -> dbine_core::Result<()>;
    fn remove(&self, connection_id: &str) -> dbine_core::Result<()>;
}

/// The backup's (encrypted) content.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Backup {
    pub state: StateSnapshot,
    /// Secrets of the connections that keep them, by connection id.
    #[serde(default)]
    pub secrets: BTreeMap<String, Secrets>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SyncAction {
    UpToDate,
    Uploaded {
        /// Another machine's backup was replaced and kept as the previous one.
        previous_kept: bool,
    },
    Downloaded {
        /// Where this machine's state before the restore was saved.
        local_backup: Option<String>,
        /// The machine that wrote the restored backup.
        device: String,
    },
}

/// A copy of this machine's state saved before a restore.
#[derive(Debug, Clone, Serialize)]
pub struct LocalBackup {
    pub path: String,
    pub updated_at: String,
    pub device: String,
    pub size: u64,
}

const K_SYNCED_REV: &str = "local.sync.synced_revision";
const K_REMOTE_REV: &str = "local.sync.remote_revision";
const K_LAST_SYNC: &str = "local.sync.last_sync_at";
const KEEP_LOCAL_BACKUPS: usize = 10;

struct CachedKey {
    pass: [u8; 32],
    kdf: Kdf,
    key: Key,
}

pub struct SyncEngine {
    pub store: Arc<StateStore>,
    secrets: Arc<dyn SecretsIo>,
    device: String,
    app_version: String,
    backup_dir: PathBuf,
    /// Argon2 is slow on purpose: derive once per passphrase and salt.
    key: Mutex<Option<CachedKey>>,
    #[cfg(test)]
    fast_kdf: bool,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl SyncEngine {
    pub fn new(store: Arc<StateStore>, secrets: Arc<dyn SecretsIo>, device: String, app_version: String, backup_dir: PathBuf) -> Self {
        Self {
            store,
            secrets,
            device,
            app_version,
            backup_dir,
            key: Mutex::new(None),
            #[cfg(test)]
            fast_kdf: false,
        }
    }

    fn new_kdf(&self) -> Kdf {
        #[cfg(test)]
        if self.fast_kdf {
            return Kdf::fast();
        }
        Kdf::generate()
    }

    /// Run `f` with the key for `passphrase` and `kdf` (`None`: the cached
    /// parameters if the passphrase matches, or fresh ones).
    fn with_key<T>(&self, passphrase: &str, kdf: Option<&Kdf>, f: impl FnOnce(&Kdf, &Key) -> Result<T>) -> Result<T> {
        let pass: [u8; 32] = Sha256::digest(passphrase.as_bytes()).into();
        let mut cache = self.key.lock().map_err(|_| SyncError::Local("clave en uso".into()))?;
        let reusable = cache.as_ref().is_some_and(|c| c.pass == pass && kdf.is_none_or(|k| *k == c.kdf));
        if !reusable {
            let kdf = kdf.cloned().unwrap_or_else(|| self.new_kdf());
            let key = crypto::derive(passphrase, &kdf)?;
            *cache = Some(CachedKey { pass, kdf, key });
        }
        let c = cache.as_ref().expect("key just cached");
        f(&c.kdf, &c.key)
    }

    /// Forget the cached key (passphrase changed or sync turned off).
    pub fn forget_key(&self) {
        if let Ok(mut c) = self.key.lock() {
            *c = None;
        }
    }

    fn seal(&self, passphrase: &str, fresh_salt: bool, plain: &[u8]) -> Result<Vec<u8>> {
        if fresh_salt {
            self.forget_key();
        }
        self.with_key(passphrase, None, |kdf, key| {
            let header = Header {
                format: crypto::FORMAT.into(),
                version: crypto::VERSION,
                updated_at: now(),
                device: self.device.clone(),
                app_version: self.app_version.clone(),
                kdf: kdf.clone(),
                cipher: "xchacha20poly1305".into(),
            };
            crypto::seal(key, header, plain)
        })
    }

    fn open(&self, passphrase: &str, file: &[u8]) -> Result<(Header, Backup)> {
        let header = crypto::peek(file)?;
        let plain = self.with_key(passphrase, Some(&header.kdf), |_, key| crypto::open(key, file))?;
        Ok((header, serde_json::from_slice(&plain)?))
    }

    fn build(&self) -> Result<Backup> {
        let state = self.store.snapshot()?;
        let mut secrets = BTreeMap::new();
        for c in state.connections.iter().filter(|c| c.save_password) {
            let s = self.secrets.read(&c.id).map_err(|e| {
                SyncError::Local(format!("no se pudo leer la contraseña de «{}» del llavero: {e}", c.name))
            })?;
            if !s.is_empty() {
                secrets.insert(c.id.clone(), s);
            }
        }
        Ok(Backup { state, secrets })
    }

    fn setting(&self, key: &str) -> Option<String> {
        self.store.get_setting(key).ok().flatten().and_then(|v| match v {
            serde_json::Value::String(s) => Some(s),
            serde_json::Value::Null => None,
            other => Some(other.to_string()),
        })
    }

    fn set(&self, key: &str, v: impl Into<serde_json::Value>) -> Result<()> {
        Ok(self.store.set_setting(key, Some(&v.into()))?)
    }

    /// Local changes not uploaded yet.
    pub fn is_dirty(&self) -> Result<bool> {
        let rev = self.store.revision()?;
        Ok(self.setting(K_SYNCED_REV).and_then(|s| s.parse::<u64>().ok()) != Some(rev))
    }

    pub fn last_sync_at(&self) -> Option<String> {
        self.setting(K_LAST_SYNC)
    }

    /// Forget what was synced (sync turned off, or pointed elsewhere).
    pub fn reset(&self) -> Result<()> {
        for k in [K_SYNCED_REV, K_REMOTE_REV, K_LAST_SYNC] {
            self.store.set_setting(k, None)?;
        }
        self.forget_key();
        Ok(())
    }

    /// The stored backup's header (no passphrase needed), if there's one.
    pub async fn remote_header(&self, cloud: &dyn CloudStore) -> Result<Option<Header>> {
        match cloud.download(BACKUP_FILE).await? {
            Some(b) => Ok(Some(crypto::peek(&b)?)),
            None => Ok(None),
        }
    }

    /// Check a passphrase against the stored backup.
    pub async fn verify(&self, cloud: &dyn CloudStore, passphrase: &str) -> Result<Header> {
        let file = cloud.download(BACKUP_FILE).await?.ok_or_else(|| SyncError::Remote("no hay ningún backup guardado".into()))?;
        Ok(self.open(passphrase, &file)?.0)
    }

    /// Upload this machine's state now. `fresh_salt` re-keys (a new
    /// passphrase).
    pub async fn push(&self, cloud: &dyn CloudStore, passphrase: &str, fresh_salt: bool) -> Result<SyncAction> {
        let rev = self.store.revision()?;
        let backup = self.build()?;
        let file = self.seal(passphrase, fresh_salt, &serde_json::to_vec(&backup)?)?;
        // Another machine's backup we haven't seen: keep it before replacing.
        let mut previous_kept = false;
        if let Some(meta) = cloud.stat(BACKUP_FILE).await? {
            if self.setting(K_REMOTE_REV).as_deref() != Some(meta.revision.as_str()) {
                if let Some(old) = cloud.download(BACKUP_FILE).await? {
                    cloud.upload(PREVIOUS_FILE, old).await?;
                    previous_kept = true;
                }
            }
        }
        let meta = cloud.upload(BACKUP_FILE, file).await?;
        self.set(K_SYNCED_REV, rev.to_string())?;
        self.set(K_REMOTE_REV, meta.revision)?;
        self.set(K_LAST_SYNC, now())?;
        Ok(SyncAction::Uploaded { previous_kept })
    }

    /// Replace this machine's state with the stored backup (this machine's
    /// state is saved locally first).
    pub async fn pull(&self, cloud: &dyn CloudStore, passphrase: &str) -> Result<SyncAction> {
        let meta = cloud.stat(BACKUP_FILE).await?.ok_or_else(|| SyncError::Remote("no hay ningún backup guardado".into()))?;
        let file = cloud.download(BACKUP_FILE).await?.ok_or_else(|| SyncError::Remote("no hay ningún backup guardado".into()))?;
        let (header, backup) = self.open(passphrase, &file)?;
        let local_backup = self.save_local_backup(passphrase)?;
        self.apply(backup)?;
        self.set(K_SYNCED_REV, self.store.revision()?.to_string())?;
        self.set(K_REMOTE_REV, meta.revision)?;
        self.set(K_LAST_SYNC, now())?;
        Ok(SyncAction::Downloaded { local_backup: local_backup.map(|p| p.display().to_string()), device: header.device })
    }

    fn apply(&self, backup: Backup) -> Result<()> {
        let old: Vec<String> = self.store.list_connections()?.into_iter().map(|c| c.id).collect();
        self.store.replace_all(&backup.state)?;
        for c in &backup.state.connections {
            match backup.secrets.get(&c.id) {
                Some(s) if c.save_password => self.secrets.write(&c.id, s)?,
                _ => self.secrets.remove(&c.id)?,
            }
        }
        let kept: std::collections::HashSet<&str> = backup.state.connections.iter().map(|c| c.id.as_str()).collect();
        for id in old.iter().filter(|id| !kept.contains(id.as_str())) {
            self.secrets.remove(id)?;
        }
        Ok(())
    }

    /// The automatic sync: upload, download or settle a conflict (see the
    /// module docs).
    pub async fn sync(&self, cloud: &dyn CloudStore, passphrase: &str) -> Result<SyncAction> {
        let dirty = self.is_dirty()?;
        let Some(meta) = cloud.stat(BACKUP_FILE).await? else {
            // Nothing stored (first time, or deleted from the cloud).
            return self.push(cloud, passphrase, false).await;
        };
        let changed = self.setting(K_REMOTE_REV).as_deref() != Some(meta.revision.as_str());
        match (dirty, changed) {
            (false, false) => Ok(SyncAction::UpToDate),
            (true, false) => self.push(cloud, passphrase, false).await,
            (false, true) => self.pull(cloud, passphrase).await,
            (true, true) => {
                let header = self.remote_header(cloud).await?;
                let local_at = self.setting("local.changed_at").unwrap_or_default();
                let remote_at = header.map(|h| h.updated_at).unwrap_or_default();
                if parse_ts(&remote_at) > parse_ts(&local_at) {
                    self.pull(cloud, passphrase).await
                } else {
                    self.push(cloud, passphrase, false).await
                }
            }
        }
    }

    // -- local backups ----------------------------------------------------

    /// Save this machine's state (encrypted) before it's replaced; `None`
    /// when there's nothing to save.
    fn save_local_backup(&self, passphrase: &str) -> Result<Option<PathBuf>> {
        let backup = self.build()?;
        if backup.state.connections.is_empty() && backup.state.queries.is_empty() {
            return Ok(None);
        }
        std::fs::create_dir_all(&self.backup_dir)?;
        let path = self.backup_dir.join(format!("dbine-local-{}.json", chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f")));
        std::fs::write(&path, self.seal(passphrase, false, &serde_json::to_vec(&backup)?)?)?;
        // Keep the newest few.
        let mut all = self.local_backups();
        if all.len() > KEEP_LOCAL_BACKUPS {
            for old in all.drain(KEEP_LOCAL_BACKUPS..) {
                let _ = std::fs::remove_file(old.path);
            }
        }
        Ok(Some(path))
    }

    /// Local copies saved before restores, newest first.
    pub fn local_backups(&self) -> Vec<LocalBackup> {
        let Ok(dir) = std::fs::read_dir(&self.backup_dir) else { return vec![] };
        let mut out: Vec<LocalBackup> = dir
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("dbine-local-"))
            .filter_map(|e| {
                let bytes = std::fs::read(e.path()).ok()?;
                let h = crypto::peek(&bytes).ok()?;
                Some(LocalBackup { path: e.path().display().to_string(), updated_at: h.updated_at, device: h.device, size: bytes.len() as u64 })
            })
            .collect();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    /// Put back a local copy (the current state is saved first, too). The
    /// result is a local change: the next sync uploads it.
    pub fn restore_local(&self, path: &str, passphrase: &str) -> Result<()> {
        let p = std::path::Path::new(path);
        if p.parent() != Some(self.backup_dir.as_path()) {
            return Err(SyncError::Local("esa copia no está en la carpeta de copias locales".into()));
        }
        let (_, backup) = self.open(passphrase, &std::fs::read(p)?)?;
        self.save_local_backup(passphrase)?;
        self.apply(backup)
    }
}

fn parse_ts(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s).map(|t| t.timestamp_millis()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::folder::FolderStore;
    use dbine_core::{ConnectionFolder, SavedConnection, SavedQuery};
    use std::collections::HashMap;

    #[derive(Default)]
    struct MemSecrets(Mutex<HashMap<String, Secrets>>);
    impl SecretsIo for MemSecrets {
        fn read(&self, id: &str) -> dbine_core::Result<Secrets> {
            Ok(self.0.lock().unwrap().get(id).cloned().unwrap_or_default())
        }
        fn write(&self, id: &str, s: &Secrets) -> dbine_core::Result<()> {
            self.0.lock().unwrap().insert(id.into(), s.clone());
            Ok(())
        }
        fn remove(&self, id: &str) -> dbine_core::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
    }

    struct Machine {
        engine: SyncEngine,
        secrets: Arc<MemSecrets>,
        _dir: tempfile::TempDir,
    }

    fn machine(name: &str) -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemSecrets::default());
        let store = Arc::new(StateStore::open(&dir.path().join("state.db")).unwrap());
        let mut engine = SyncEngine::new(store, secrets.clone(), name.into(), "0.1.0".into(), dir.path().join("backups"));
        engine.fast_kdf = true;
        Machine { engine, secrets, _dir: dir }
    }

    fn conn(id: &str) -> SavedConnection {
        SavedConnection {
            id: id.into(),
            name: format!("PROD {id}"),
            color: None,
            config: serde_json::from_value(serde_json::json!({ "driver": "postgres", "host": "localhost", "username": "admin" })).unwrap(),
            save_password: true,
            folder_id: Some("f1".into()),
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        }
    }

    fn query(id: &str, conn: &str, sql: &str) -> SavedQuery {
        SavedQuery { id: id.into(), connection_id: conn.into(), database: "app".into(), name: id.into(), sql: sql.into(), updated_at: String::new(), last_run_at: None }
    }

    fn pw(p: &str) -> Secrets {
        [("password".to_string(), p.to_string())].into()
    }

    #[tokio::test]
    async fn a_new_machine_gets_everything_back() {
        let cloud_dir = tempfile::tempdir().unwrap();
        let cloud = FolderStore::new(cloud_dir.path());
        let a = machine("mac-a");
        let s = &a.engine.store;
        s.save_folder(&ConnectionFolder { id: "f1".into(), name: "Cliente".into(), parent_id: None, color: Some("#f00".into()) }).unwrap();
        s.save_connection(&conn("c1")).unwrap();
        s.save_query(&query("q1", "c1", "select * from clientes")).unwrap();
        s.set_setting("copy.format", Some(&"csv".into())).unwrap();
        a.secrets.write("c1", &pw("hunter2")).unwrap();
        assert_eq!(a.engine.sync(&cloud, "frase secreta").await.unwrap(), SyncAction::Uploaded { previous_kept: false });

        // Nothing readable in the cloud file.
        let raw = std::fs::read_to_string(cloud_dir.path().join(BACKUP_FILE)).unwrap();
        for leak in ["hunter2", "clientes", "PROD", "Cliente", "localhost"] {
            assert!(!raw.contains(leak), "{leak} readable in the backup");
        }

        let b = machine("mac-b");
        assert!(matches!(b.engine.pull(&cloud, "otra frase").await, Err(SyncError::WrongPassphrase)));
        let SyncAction::Downloaded { device, local_backup } = b.engine.pull(&cloud, "frase secreta").await.unwrap() else { panic!() };
        assert_eq!(device, "mac-a");
        assert!(local_backup.is_none(), "empty machine: nothing to keep");
        let got = b.engine.store.snapshot().unwrap();
        assert_eq!(got.connections.len(), 1);
        assert_eq!(got.connections[0].folder_id.as_deref(), Some("f1"));
        assert_eq!(got.queries[0].sql, "select * from clientes");
        assert_eq!(got.settings["copy.format"], "csv");
        assert_eq!(b.secrets.read("c1").unwrap(), pw("hunter2"));
        // Just restored: nothing to do.
        assert_eq!(b.engine.sync(&cloud, "frase secreta").await.unwrap(), SyncAction::UpToDate);
    }

    #[tokio::test]
    async fn changes_travel_both_ways_and_conflicts_keep_the_loser() {
        let cloud_dir = tempfile::tempdir().unwrap();
        let cloud = FolderStore::new(cloud_dir.path());
        let a = machine("a");
        let b = machine("b");
        a.engine.store.save_connection(&conn("c1")).unwrap();
        a.engine.sync(&cloud, "p").await.unwrap();
        b.engine.pull(&cloud, "p").await.unwrap();

        // b changes, uploads; a (clean) downloads it.
        b.engine.store.save_query(&query("q1", "c1", "select 1")).unwrap();
        assert!(matches!(b.engine.sync(&cloud, "p").await.unwrap(), SyncAction::Uploaded { previous_kept: false }));
        assert!(matches!(a.engine.sync(&cloud, "p").await.unwrap(), SyncAction::Downloaded { .. }));
        assert_eq!(a.engine.store.snapshot().unwrap().queries.len(), 1);

        // Both change; b's is newer and uploads first; a's older change loses
        // but is kept in a local copy.
        a.engine.store.save_query(&query("qa", "c1", "select 'a'")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        b.engine.store.save_query(&query("qb", "c1", "select 'b'")).unwrap();
        b.engine.sync(&cloud, "p").await.unwrap();
        let SyncAction::Downloaded { local_backup: Some(_), .. } = a.engine.sync(&cloud, "p").await.unwrap() else { panic!() };
        let ids: Vec<_> = a.engine.store.snapshot().unwrap().queries.into_iter().map(|q| q.id).collect();
        assert_eq!(ids, ["q1", "qb"]);
        // One copy per restore (the earlier download saved one too), newest first.
        let copies = a.engine.local_backups();
        assert_eq!(copies.len(), 2);
        a.engine.restore_local(&copies[0].path, "p").unwrap();
        assert!(a.engine.store.get_query("qa").unwrap().is_some());

        // a now uploads over b's unseen change? No: a pulled b's last. Make b
        // change again, then a (dirty, newer) wins and b's is kept as previous.
        b.engine.store.save_query(&query("qb2", "c1", "x")).unwrap();
        b.engine.sync(&cloud, "p").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        a.engine.store.save_query(&query("qa2", "c1", "y")).unwrap();
        assert_eq!(a.engine.sync(&cloud, "p").await.unwrap(), SyncAction::Uploaded { previous_kept: true });
        assert!(cloud_dir.path().join(PREVIOUS_FILE).exists());
    }

    #[tokio::test]
    async fn restore_drops_secrets_of_connections_that_are_gone() {
        let cloud_dir = tempfile::tempdir().unwrap();
        let cloud = FolderStore::new(cloud_dir.path());
        let a = machine("a");
        a.engine.store.save_connection(&conn("c1")).unwrap();
        a.engine.push(&cloud, "p", false).await.unwrap();
        let b = machine("b");
        b.engine.store.save_connection(&conn("old")).unwrap();
        b.secrets.write("old", &pw("x")).unwrap();
        b.engine.pull(&cloud, "p").await.unwrap();
        assert!(b.secrets.read("old").unwrap().is_empty());
        assert!(b.engine.store.get_connection("old").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_new_passphrase_rekeys() {
        let cloud_dir = tempfile::tempdir().unwrap();
        let cloud = FolderStore::new(cloud_dir.path());
        let a = machine("a");
        a.engine.store.save_connection(&conn("c1")).unwrap();
        a.engine.push(&cloud, "vieja", false).await.unwrap();
        let salt1 = a.engine.remote_header(&cloud).await.unwrap().unwrap().kdf.salt;
        a.engine.push(&cloud, "nueva", true).await.unwrap();
        let salt2 = a.engine.remote_header(&cloud).await.unwrap().unwrap().kdf.salt;
        assert_ne!(salt1, salt2);
        let b = machine("b");
        assert!(b.engine.verify(&cloud, "vieja").await.is_err());
        assert!(b.engine.verify(&cloud, "nueva").await.is_ok());
    }
}
