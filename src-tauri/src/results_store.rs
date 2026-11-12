//! The results store (docs/results-store.md): the query editor's results on
//! disk, in a DuckDB database encrypted with a key that never leaves this
//! machine and OS user, so they survive closing DBine or a crash.
//!
//! - One file, `<app config>/results/results.duckdb`, attached with
//!   `ENCRYPTION_KEY` on a DuckDB instance of its own (the DuckDB driver's
//!   `isolated` option, downloaded on first use like any DuckDB session).
//!   Spilled temp files are encrypted too (`temp_file_encryption`, off by
//!   DuckDB's default: verified to leak plaintext without it), extensions
//!   are off, and the configuration is locked; [`setup`] checks it all
//!   before anything is written.
//! - The key: 256 random bits in the secrets vault (`results-store-key`),
//!   whose own key is in the OS keychain. Never in the state file, a log,
//!   the history or an error: [`redact`] takes it out of every DuckDB error.
//!   A file the key can't open is deleted and the store starts again (it's
//!   a cache). Without a vault nothing is stored: results stay in memory.
//! - `runs`: one row per editor run (its metadata as the UI gave it, and the
//!   outcome without its rows). Each result set's rows go in a table of
//!   their own, `r_<id hash>_<n>`, as the grid's values in JSON text: read
//!   back, every value is exactly what the driver produced.
//! - A run counts once `complete`: one cut short (a crash mid-write) is
//!   removed the next time the store opens.

use crate::error::{CommandError, CommandResult};
use crate::local_duckdb::{
    attach_sql, exec, first_row, first_value, lit, new_key, redact, remove_paths, SetupError,
};
#[cfg(test)]
use crate::local_duckdb::{settings_sql, LOCK};
use dbine_driver::transfer::{BatchSource, Cell, LoadSpec, RowBatch, TransferColumn};
use dbine_driver::{ObjectRef, QueryOutcome, Session};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
#[cfg(test)]
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// The vault item holding the store's key.
const KEY_NAME: &str = "results-store-key";
const FILE: &str = "results.duckdb";
/// The layout's version (`store_info`): a file of another version starts over.
const VERSION: i64 = 1;
/// Space the stored results may take when the UI doesn't say.
pub const DEFAULT_MAX_BYTES: u64 = 2 << 30;
/// The file a compaction writes before it replaces the store's.
const COMPACT_FILE: &str = "results.compact.duckdb";
/// A file with at least this much free space, and more free than used, is
/// rewritten with only what's stored when it opens.
const COMPACT_MIN_FREE: u64 = 64 << 20;
/// [`COMPACT_MIN_FREE`]; tests compact past 1 MiB, to see it happen.
fn compact_min_free() -> u64 {
    if cfg!(test) {
        1 << 20
    } else {
        COMPACT_MIN_FREE
    }
}
/// Rows per batch handed to the bulk load.
const BATCH_ROWS: usize = 1_000;
/// After DuckDB couldn't open (offline before its first download…), the
/// next try waits this long instead of slowing every run.
const RETRY_AFTER: Duration = Duration::from_secs(300);
/// Longest run / tab id the UI may give.
const MAX_ID: usize = 128;

/// Why the store isn't in use.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Unavailable {
    /// No secrets vault or keychain to keep the key: nothing goes to disk.
    NoVault,
    /// DuckDB couldn't be opened (not downloaded yet and no network…).
    Duckdb,
    /// The encrypted file couldn't be set up as required.
    Setup,
}

enum Slot {
    Closed,
    Open(Store),
    Unavailable(Unavailable, Instant),
}

struct Store {
    session: Box<dyn Session>,
    dir: PathBuf,
}

static SLOT: LazyLock<Mutex<Slot>> = LazyLock::new(|| Mutex::new(Slot::Closed));
/// Goes up with every [`clear`]: a run that started before it isn't
/// stored when it ends.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Where the store lives instead of next to the vault (tests only).
#[cfg(test)]
static DIR: OnceLock<PathBuf> = OnceLock::new();
/// The key to use instead of the vault's (tests only: no keychain).
#[cfg(test)]
static TEST_KEY: OnceLock<String> = OnceLock::new();

/// A run as the editor describes it (`web/src/composables/resultRuns.ts`).
/// Whatever else the UI keeps of it travels in `extra`, untouched.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RunMeta {
    pub id: String,
    pub tab_id: String,
    pub at: i64,
    #[serde(default)]
    pub pinned: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Serialize, Debug, Default)]
pub struct Saved {
    /// `false`: the run alone doesn't fit in the space allowed.
    pub stored: bool,
}

#[derive(Serialize, Debug)]
pub struct Status {
    pub available: bool,
    pub reason: Option<Unavailable>,
    /// Space the stored results take now.
    pub bytes: u64,
}

// ---------------------------------------------------------------------------
// Opening

fn dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(d) = DIR.get() {
        return Some(d.clone());
    }
    dbine_core::secrets::vault_dir().map(|d| d.join("results"))
}

/// The store's key from the vault, created the first time. A vault that
/// can't be read gives none, never a new key over an existing one.
fn key() -> Result<String, Unavailable> {
    #[cfg(test)]
    if let Some(k) = TEST_KEY.get() {
        return Ok(k.clone());
    }
    if dbine_core::secrets::vault_dir().is_none() {
        return Err(Unavailable::NoVault);
    }
    // Only from the vault: never an older keychain item of that name.
    match dbine_core::secrets::get_vault_only(KEY_NAME) {
        Ok(Some(k)) if valid_key(&k) => Ok(k),
        Ok(_) => {
            let k = new_key().map_err(|_| Unavailable::NoVault)?;
            dbine_core::secrets::set_raw(KEY_NAME, &k).map_err(|_| Unavailable::NoVault)?;
            Ok(k)
        }
        Err(_) => Err(Unavailable::NoVault),
    }
}

fn valid_key(k: &str) -> bool {
    k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The alias the store's file is attached as.
const ALIAS: &str = "store";

/// The shared setup (`local_duckdb`), with the store's compaction between
/// attaching the file and locking the session down.
async fn setup(s: &mut dyn Session, dir: &Path, key: &str) -> Result<(), SetupError> {
    let other = |e: CommandError| SetupError::Other(redact(e, key));
    crate::local_duckdb::configure(s, dir, key).await?;
    attach(s, dir, key).await?;
    if needs_compaction(s).await.map_err(other)? {
        compact(s, dir, key).await?;
    }
    crate::local_duckdb::lock(s, ALIAS, key).await
}

/// Attach the store's file as `store`; a file this key can't open is
/// [`SetupError::Unreadable`].
async fn attach(s: &mut dyn Session, dir: &Path, key: &str) -> Result<(), SetupError> {
    crate::local_duckdb::attach(s, dir, FILE, ALIAS, key).await
}

/// Whether the attached file is mostly free space (results closed since it
/// grew): at least [`COMPACT_MIN_FREE`] free, and more free than used.
async fn needs_compaction(s: &mut dyn Session) -> CommandResult<bool> {
    let out = exec(
        s,
        "SELECT free_blocks * block_size, used_blocks * block_size \
         FROM pragma_database_size() WHERE database_name = 'store'",
    )
    .await?;
    let row = first_row(&out);
    let n = |i: usize| row.get(i).and_then(as_u64).unwrap_or(0);
    Ok(n(0) >= compact_min_free() && n(0) > n(1))
}

/// Rewrite the store with only what it holds: copied into a new file with
/// the same key, which then replaces it. The old file stays until the new
/// one is complete; a crash leaves [`COMPACT_FILE`], which the next open
/// deletes first.
async fn compact(s: &mut dyn Session, dir: &Path, key: &str) -> Result<(), SetupError> {
    let other = |e: CommandError| SetupError::Other(redact(e, key));
    let fresh = dir.join(COMPACT_FILE);
    remove_paths(&[fresh.clone(), dir.join(format!("{COMPACT_FILE}.wal"))]).await;
    exec(s, &attach_sql(dir, COMPACT_FILE, "fresh", key))
        .await
        .map_err(other)?;
    let copied = async {
        exec(s, "COPY FROM DATABASE store TO fresh").await?;
        exec(s, "CHECKPOINT fresh").await?;
        exec(s, "DETACH fresh").await?;
        exec(s, "CHECKPOINT store").await?;
        exec(s, "DETACH store").await
    }
    .await;
    if let Err(e) = copied {
        let _ = exec(s, "DETACH DATABASE IF EXISTS fresh").await;
        remove_paths(&[fresh]).await;
        // The store is still there as it was: go on with it uncompacted.
        tracing::warn!(e = %redact(e, key), "results store: couldn't compact");
        return Ok(());
    }
    if let Err(e) = std::fs::rename(&fresh, dir.join(FILE)) {
        tracing::warn!(%e, "results store: couldn't replace the compacted file");
        remove_paths(&[fresh]).await;
    }
    let _ = std::fs::remove_file(dir.join(format!("{FILE}.wal")));
    attach(s, dir, key).await
}

/// Delete the store's files: the database, its WAL, a compaction cut short
/// and spilled temp files.
async fn remove_files(dir: &Path) {
    remove_paths(&[
        dir.join(FILE),
        dir.join(format!("{FILE}.wal")),
        dir.join(COMPACT_FILE),
        dir.join(format!("{COMPACT_FILE}.wal")),
    ])
    .await;
    for _ in 0..20 {
        match std::fs::remove_dir_all(dir.join("tmp")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            _ => break,
        }
    }
}

async fn open_session() -> Result<Box<dyn Session>, Unavailable> {
    crate::local_duckdb::open_session().await.map_err(|e| {
        tracing::warn!(%e, "results store: DuckDB unavailable");
        Unavailable::Duckdb
    })
}

/// Open the store in `dir` with `key`; a file the key can't open is
/// deleted and a new one starts.
async fn open_at(dir: &Path, key: &str) -> Result<Store, Unavailable> {
    std::fs::create_dir_all(dir.join("tmp")).map_err(|e| {
        tracing::warn!(%e, "results store: folder not created");
        Unavailable::Setup
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    // A compaction cut short: the store's own file is still whole.
    remove_paths(&[
        dir.join(COMPACT_FILE),
        dir.join(format!("{COMPACT_FILE}.wal")),
    ])
    .await;
    for attempt in 0..2 {
        let mut s = open_session().await?;
        match setup(s.as_mut(), dir, key).await {
            Ok(()) => {
                let mut store = Store {
                    session: s,
                    dir: dir.to_path_buf(),
                };
                match store.prepare().await {
                    Ok(true) => return Ok(store),
                    // Another layout: start over.
                    Ok(false) if attempt == 0 => {
                        let _ = s_close(store.session).await;
                        remove_files(dir).await;
                    }
                    Ok(false) => return Err(Unavailable::Setup),
                    Err(e) => {
                        tracing::warn!(e = %redact(e, key), "results store: layout");
                        return Err(Unavailable::Setup);
                    }
                }
            }
            Err(SetupError::Unreadable) if attempt == 0 => {
                tracing::warn!(
                    "results store: the file doesn't open with this key; starting a new one"
                );
                let _ = s_close(s).await;
                remove_files(dir).await;
            }
            Err(SetupError::Unreadable) => return Err(Unavailable::Setup),
            Err(SetupError::Crypto(e)) => {
                tracing::warn!(%e, "results store: DuckDB's httpfs (encryption) unavailable");
                return Err(Unavailable::Duckdb);
            }
            Err(SetupError::Other(e)) => {
                tracing::warn!(%e, "results store: setup");
                return Err(Unavailable::Setup);
            }
        }
    }
    Err(Unavailable::Setup)
}

/// Close a session (dropping it closes the DuckDB connection).
async fn s_close(s: Box<dyn Session>) -> dbine_driver::Result<()> {
    drop(s);
    Ok(())
}

/// Whether the store's file is there (reads don't create it: a DBine that
/// never stored a result doesn't download DuckDB to list nothing).
fn exists() -> bool {
    dir().is_some_and(|d| d.join(FILE).exists())
}

/// The slot, with the store opened if it's there.
async fn lock_existing() -> tokio::sync::MutexGuard<'static, Slot> {
    if !exists() {
        let slot = SLOT.lock().await;
        if matches!(*slot, Slot::Closed) {
            return slot;
        }
        drop(slot);
    }
    lock_open().await
}

/// The slot, with the store opened (or created) if it can be.
async fn lock_open() -> tokio::sync::MutexGuard<'static, Slot> {
    let mut slot = SLOT.lock().await;
    if let Slot::Unavailable(why, since) = &*slot {
        if *why == Unavailable::NoVault || since.elapsed() < RETRY_AFTER {
            return slot;
        }
        *slot = Slot::Closed;
    }
    if matches!(*slot, Slot::Closed) {
        let opened = match (dir(), key()) {
            (None, _) => Err(Unavailable::NoVault),
            (_, Err(why)) => Err(why),
            (Some(d), Ok(k)) => open_at(&d, &k).await,
        };
        *slot = match opened {
            Ok(s) => Slot::Open(s),
            Err(why) => Slot::Unavailable(why, Instant::now()),
        };
    }
    slot
}

/// After a failed operation: a DuckDB host that went away is reopened
/// next time.
async fn after<T>(slot: &mut Slot, r: CommandResult<T>) -> CommandResult<T> {
    if r.is_err() {
        if let Slot::Open(store) = slot {
            if !store.alive().await {
                *slot = Slot::Closed;
            }
        }
    }
    r
}

/// With no run left, the store goes: its session closes and its files are
/// deleted, so nothing stored means nothing on disk.
async fn drop_if_empty(slot: &mut Slot) {
    let Slot::Open(store) = slot else {
        return;
    };
    let empty = exec(store.session.as_mut(), "SELECT count(*) FROM runs")
        .await
        .is_ok_and(|o| first_value(&o).and_then(as_u64) == Some(0));
    if !empty {
        return;
    }
    if let Slot::Open(store) = std::mem::replace(slot, Slot::Closed) {
        let dir = store.dir.clone();
        let _ = s_close(store.session).await;
        remove_files(&dir).await;
    }
}

// ---------------------------------------------------------------------------
// The store

/// What the tables of run `id` start with.
fn prefix(id: &str) -> String {
    let h = Sha256::digest(id.as_bytes());
    let hex: String = h[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("r_{hex}_")
}

/// The table of result set `n` of run `id`.
fn table(id: &str, n: usize) -> String {
    format!("{}{n}", prefix(id))
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ID && s.chars().all(|c| !c.is_control())
}

impl Store {
    /// The layout, and what a crash left half-written. `false`: the file is
    /// of another layout version.
    async fn prepare(&mut self) -> CommandResult<bool> {
        let s = self.session.as_mut();
        exec(
            s,
            "CREATE TABLE IF NOT EXISTS store_info (version BIGINT NOT NULL)",
        )
        .await?;
        let v = exec(s, "SELECT max(version) FROM store_info").await?;
        match first_value(&v).and_then(Value::as_i64) {
            None => {
                exec(s, &format!("INSERT INTO store_info VALUES ({VERSION})")).await?;
            }
            Some(VERSION) => {}
            Some(_) => return Ok(false),
        }
        exec(
            s,
            "CREATE TABLE IF NOT EXISTS runs (\
               id VARCHAR PRIMARY KEY, tab_id VARCHAR NOT NULL, ran_at BIGINT NOT NULL, \
               pinned BOOLEAN NOT NULL, meta VARCHAR NOT NULL, outcome VARCHAR, \
               results BIGINT NOT NULL DEFAULT 0, complete BOOLEAN NOT NULL DEFAULT false)",
        )
        .await?;
        let half = self.ids("SELECT id FROM runs WHERE NOT complete").await?;
        self.remove(&half).await?;
        // Tables of no run (a crash between dropping them and the row).
        let known: std::collections::HashSet<String> = self
            .ids("SELECT id FROM runs")
            .await?
            .iter()
            .map(|id| prefix(id))
            .collect();
        for t in self.tables("r\\_").await? {
            let owner = t.rfind('_').map_or(t.as_str(), |i| &t[..=i]);
            if !known.contains(owner) {
                exec(
                    self.session.as_mut(),
                    &format!("DROP TABLE IF EXISTS main.\"{t}\""),
                )
                .await?;
            }
        }
        Ok(true)
    }

    async fn alive(&mut self) -> bool {
        exec(self.session.as_mut(), "SELECT 1").await.is_ok()
    }

    /// The first column of every row of `sql`, as text.
    async fn ids(&mut self, sql: &str) -> CommandResult<Vec<String>> {
        let mut out = QueryOutcome::default();
        self.session.execute(sql, usize::MAX, &mut out).await?;
        if let Some(e) = out.error {
            return Err(CommandError::Sql(e));
        }
        Ok(out
            .results
            .iter()
            .filter(|r| !r.columns.is_empty())
            .flat_map(|r| r.rows.iter())
            .filter_map(|r| r.first().and_then(Value::as_str).map(str::to_string))
            .collect())
    }

    /// The store's tables whose name starts with `like` (a LIKE pattern
    /// with `\` as the escape).
    async fn tables(&mut self, like: &str) -> CommandResult<Vec<String>> {
        self.ids(&format!(
            "SELECT table_name FROM duckdb_tables() WHERE database_name = 'store' \
             AND table_name LIKE {} ESCAPE '\\'",
            lit(&format!("{like}%"))
        ))
        .await
    }

    /// Drop runs and their tables.
    async fn remove(&mut self, ids: &[String]) -> CommandResult<()> {
        for id in ids {
            let like = prefix(id).replace('_', "\\_");
            for t in self.tables(&like).await? {
                exec(
                    self.session.as_mut(),
                    &format!("DROP TABLE IF EXISTS main.\"{t}\""),
                )
                .await?;
            }
            exec(
                self.session.as_mut(),
                &format!("DELETE FROM runs WHERE id = {}", lit(id)),
            )
            .await?;
        }
        Ok(())
    }

    /// Space the results take (blocks in use plus the WAL).
    async fn bytes(&mut self) -> CommandResult<u64> {
        let out = exec(
            self.session.as_mut(),
            "SELECT used_blocks * block_size FROM pragma_database_size() WHERE database_name = 'store'",
        )
        .await?;
        let used = first_value(&out).and_then(Value::as_u64).unwrap_or(0);
        let wal = std::fs::metadata(self.dir.join(format!("{FILE}.wal")))
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(used + wal)
    }

    async fn save(
        &mut self,
        meta: RunMeta,
        mut out: QueryOutcome,
        max_bytes: u64,
    ) -> CommandResult<Saved> {
        if !valid_id(&meta.id) || !valid_id(&meta.tab_id) {
            return Err(CommandError::BadRequest("id de ejecución no válido".into()));
        }
        // Run again in place: the earlier rows go.
        self.remove(std::slice::from_ref(&meta.id)).await?;
        let rows: Vec<Vec<Vec<Value>>> = out
            .results
            .iter_mut()
            .map(|r| std::mem::take(&mut r.rows))
            .collect();
        let outcome =
            serde_json::to_string(&out).map_err(|e| CommandError::Internal(e.to_string()))?;
        let meta_json = serde_json::to_string(&meta.extra)
            .map_err(|e| CommandError::Internal(e.to_string()))?;
        exec(
            self.session.as_mut(),
            &format!(
                "INSERT INTO runs (id, tab_id, ran_at, pinned, meta, outcome, results, complete) \
                 VALUES ({}, {}, {}, {}, {}, {}, {}, false)",
                lit(&meta.id),
                lit(&meta.tab_id),
                meta.at,
                meta.pinned,
                lit(&meta_json),
                lit(&outcome),
                rows.len()
            ),
        )
        .await?;
        let written = async {
            for (n, (r, rows)) in out.results.iter().zip(&rows).enumerate() {
                if r.columns.is_empty() || rows.is_empty() {
                    continue;
                }
                self.write_rows(&table(&meta.id, n), r.columns.len(), rows)
                    .await?;
            }
            exec(
                self.session.as_mut(),
                &format!(
                    "UPDATE runs SET complete = true WHERE id = {}",
                    lit(&meta.id)
                ),
            )
            .await?;
            Ok::<(), CommandError>(())
        }
        .await;
        if let Err(e) = written {
            let _ = self.remove(std::slice::from_ref(&meta.id)).await;
            return Err(e);
        }
        self.fit(&meta.id, max_bytes).await
    }

    /// Past `max_bytes`, the oldest unpinned runs go (never `keep`); if
    /// `keep` alone is still too big, it goes too.
    async fn fit(&mut self, keep: &str, max_bytes: u64) -> CommandResult<Saved> {
        if self.bytes().await? <= max_bytes {
            return Ok(Saved { stored: true });
        }
        let old = self
            .ids(&format!(
                "SELECT id FROM runs WHERE NOT pinned AND id <> {} ORDER BY ran_at, id",
                lit(keep)
            ))
            .await?;
        for id in old {
            self.remove(std::slice::from_ref(&id)).await?;
            exec(self.session.as_mut(), "CHECKPOINT").await?;
            if self.bytes().await? <= max_bytes {
                return Ok(Saved { stored: true });
            }
        }
        self.remove(&[keep.to_string()]).await?;
        exec(self.session.as_mut(), "CHECKPOINT").await?;
        Ok(Saved { stored: false })
    }

    async fn write_rows(
        &mut self,
        name: &str,
        width: usize,
        rows: &[Vec<Value>],
    ) -> CommandResult<()> {
        let mut cols = vec!["_row".to_string()];
        cols.extend((0..width).map(|i| format!("c{i}")));
        let defs: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| format!("\"{c}\" {}", if i == 0 { "BIGINT" } else { "VARCHAR" }))
            .collect();
        exec(
            self.session.as_mut(),
            &format!("CREATE TABLE main.\"{name}\" ({})", defs.join(", ")),
        )
        .await?;
        let spec = LoadSpec {
            table: ObjectRef {
                kind: "table".into(),
                schema: Some("main".into()),
                name: name.to_string(),
            },
            columns: cols.clone(),
            table_lock: false,
            keep_identity: false,
            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        };
        let tcols: Vec<TransferColumn> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| TransferColumn {
                name: c.clone(),
                type_name: if i == 0 { "BIGINT" } else { "VARCHAR" }.into(),
                nullable: i != 0,
            })
            .collect();
        let mut src = JsonRows { rows, width, at: 0 };
        self.session
            .bulk_load(&spec, &tcols, &mut src, &|_| {})
            .await?;
        Ok(())
    }

    async fn runs(&mut self, tab_id: &str) -> CommandResult<Vec<RunMeta>> {
        let mut out = QueryOutcome::default();
        self.session
            .execute(
                &format!(
                    "SELECT id, tab_id, ran_at, pinned, meta FROM runs \
                     WHERE complete AND tab_id = {} ORDER BY ran_at, id",
                    lit(tab_id)
                ),
                usize::MAX,
                &mut out,
            )
            .await?;
        if let Some(e) = out.error {
            return Err(CommandError::Sql(e));
        }
        let rows = out
            .results
            .into_iter()
            .find(|r| !r.columns.is_empty())
            .map(|r| r.rows)
            .unwrap_or_default();
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let extra = r
                    .get(4)
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                Some(RunMeta {
                    id: r.first()?.as_str()?.to_string(),
                    tab_id: r.get(1)?.as_str()?.to_string(),
                    at: as_i64(r.get(2)?)?,
                    pinned: r.get(3).and_then(Value::as_bool).unwrap_or(false),
                    extra,
                })
            })
            .collect())
    }

    async fn outcome(&mut self, id: &str) -> CommandResult<Option<QueryOutcome>> {
        let o = exec(
            self.session.as_mut(),
            &format!(
                "SELECT outcome FROM runs WHERE complete AND id = {}",
                lit(id)
            ),
        )
        .await?;
        let Some(text) = first_value(&o).and_then(Value::as_str) else {
            return Ok(None);
        };
        let mut out: QueryOutcome =
            serde_json::from_str(text).map_err(|e| CommandError::Internal(e.to_string()))?;
        for n in 0..out.results.len() {
            let width = out.results[n].columns.len();
            if width == 0 {
                continue;
            }
            let name = table(id, n);
            let exists = exec(
                self.session.as_mut(),
                &format!(
                    "SELECT count(*) FROM duckdb_tables() WHERE database_name = 'store' AND table_name = {}",
                    lit(&name)
                ),
            )
            .await?;
            if first_value(&exists).and_then(Value::as_u64).unwrap_or(0) == 0 {
                continue;
            }
            let cols: Vec<String> = (0..width).map(|i| format!("\"c{i}\"")).collect();
            let mut read = QueryOutcome::default();
            self.session
                .execute(
                    &format!(
                        "SELECT {} FROM main.\"{name}\" ORDER BY _row",
                        cols.join(", ")
                    ),
                    usize::MAX,
                    &mut read,
                )
                .await?;
            if let Some(e) = read.error {
                return Err(CommandError::Sql(e));
            }
            let rows = read
                .results
                .into_iter()
                .find(|r| !r.columns.is_empty())
                .map(|r| r.rows)
                .unwrap_or_default();
            out.results[n].rows = rows
                .into_iter()
                .map(|r| r.into_iter().map(from_stored).collect())
                .collect();
        }
        Ok(Some(out))
    }

    async fn set_pinned(&mut self, id: &str, pinned: bool) -> CommandResult<()> {
        exec(
            self.session.as_mut(),
            &format!("UPDATE runs SET pinned = {pinned} WHERE id = {}", lit(id)),
        )
        .await?;
        Ok(())
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn as_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// A stored value back as the grid's: its JSON text parsed (NULL: null).
fn from_stored(v: Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        _ => Value::Null,
    }
}

/// Rows as the store's cells: the row number, then each value's JSON text.
struct JsonRows<'a> {
    rows: &'a [Vec<Value>],
    width: usize,
    at: usize,
}

#[dbine_driver::async_trait]
impl BatchSource for JsonRows<'_> {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.at >= self.rows.len() {
            return None;
        }
        let end = (self.at + BATCH_ROWS).min(self.rows.len());
        let rows: Vec<Vec<Cell>> = self.rows[self.at..end]
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mut cells = Vec::with_capacity(self.width + 1);
                cells.push(Cell::Int((self.at + i) as i64));
                cells.extend((0..self.width).map(|c| match r.get(c) {
                    None | Some(Value::Null) => Cell::Null,
                    Some(v) => Cell::Text(v.to_string()),
                }));
                cells
            })
            .collect();
        self.at = end;
        let bytes = rows.iter().flatten().map(Cell::size).sum();
        Some(RowBatch { rows, bytes })
    }
}

// ---------------------------------------------------------------------------
// The API the commands use

/// The generation a run starts in (see [`GENERATION`]).
pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// Whether a run's results stay out of the store: those of a connection an
/// organization manages (DBine Enterprise, `ent-…`), whose rows mustn't
/// outlive signing out or losing access.
fn kept_out(meta: &RunMeta) -> bool {
    meta.extra
        .get("connection_id")
        .and_then(Value::as_str)
        .is_some_and(|c| c.starts_with("ent-"))
}

/// Keep a run's outcome (rows included). `Ok(None)`: no store, a managed
/// connection's run, or one that started before the store was cleared.
pub async fn save(
    meta: RunMeta,
    out: QueryOutcome,
    max_bytes: u64,
    started: u64,
) -> CommandResult<Option<Saved>> {
    if kept_out(&meta) || started != generation() {
        return Ok(None);
    }
    let mut slot = lock_open().await;
    // Again under the lock (a clear holds it while it deletes): one that
    // came in between leaves no new file behind.
    if started != generation() {
        drop_if_empty(&mut slot).await;
        return Ok(None);
    }
    let Slot::Open(s) = &mut *slot else {
        return Ok(None);
    };
    let r = s.save(meta, out, max_bytes).await;
    after(&mut slot, r).await.map(Some)
}

/// [`save`] in the background: the run's answer doesn't wait for it, and a
/// failure only loses that run's copy on disk. `started`: the
/// [`generation`] when the run started.
pub fn save_later(meta: RunMeta, out: QueryOutcome, max_bytes: u64, started: u64) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = save(meta, out, max_bytes, started).await {
            tracing::warn!(%e, "results store: a run wasn't saved");
        }
    });
}

/// The complete runs of an editor tab, oldest first.
pub async fn runs(tab_id: String) -> CommandResult<Vec<RunMeta>> {
    let mut slot = lock_existing().await;
    let Slot::Open(s) = &mut *slot else {
        return Ok(Vec::new());
    };
    let r = s.runs(&tab_id).await;
    let r = after(&mut slot, r).await;
    drop_if_empty(&mut slot).await;
    r
}

/// A stored run's outcome with its rows.
pub async fn outcome(id: String) -> CommandResult<Option<QueryOutcome>> {
    let mut slot = lock_existing().await;
    let Slot::Open(s) = &mut *slot else {
        return Ok(None);
    };
    let r = s.outcome(&id).await;
    after(&mut slot, r).await
}

pub async fn set_pinned(id: String, pinned: bool) -> CommandResult<()> {
    let mut slot = lock_existing().await;
    let Slot::Open(s) = &mut *slot else {
        return Ok(());
    };
    let r = s.set_pinned(&id, pinned).await;
    after(&mut slot, r).await
}

pub async fn delete(ids: Vec<String>) -> CommandResult<()> {
    let mut slot = lock_existing().await;
    let Slot::Open(s) = &mut *slot else {
        return Ok(());
    };
    let r = s.remove(&ids).await;
    let r = after(&mut slot, r).await;
    drop_if_empty(&mut slot).await;
    r
}

/// Remove the runs of one editor tab (it was closed).
pub async fn delete_tab(tab_id: String) -> CommandResult<()> {
    keep_or_drop(Some(tab_id), Vec::new()).await
}

/// Remove the runs of every editor tab but `tabs` (closed while DBine
/// wasn't running, or in a window that's gone).
pub async fn keep_tabs(tabs: Vec<String>) -> CommandResult<()> {
    keep_or_drop(None, tabs).await
}

async fn keep_or_drop(drop: Option<String>, keep: Vec<String>) -> CommandResult<()> {
    let mut slot = lock_existing().await;
    let Slot::Open(s) = &mut *slot else {
        return Ok(());
    };
    let r = async {
        let all = s.ids("SELECT DISTINCT tab_id FROM runs").await?;
        let gone: Vec<&String> = match &drop {
            Some(t) => all.iter().filter(|x| *x == t).collect(),
            None => all.iter().filter(|x| !keep.contains(x)).collect(),
        };
        for t in gone {
            let ids = s
                .ids(&format!("SELECT id FROM runs WHERE tab_id = {}", lit(t)))
                .await?;
            s.remove(&ids).await?;
        }
        exec(s.session.as_mut(), "CHECKPOINT").await?;
        Ok(())
    }
    .await;
    let r = after(&mut slot, r).await;
    drop_if_empty(&mut slot).await;
    r
}

/// Delete every stored result: the files go, and so does the key, so a copy
/// left anywhere (a backup of the folder) can't be read any more. A new
/// store, with a new key, starts on next use; runs already going when this
/// is called aren't stored.
pub async fn clear() -> CommandResult<()> {
    let mut slot = SLOT.lock().await;
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Slot::Open(store) = std::mem::replace(&mut *slot, Slot::Closed) {
        let _ = s_close(store.session).await;
    }
    let Some(d) = dir() else {
        return Ok(());
    };
    remove_files(&d).await;
    #[cfg(not(test))]
    if dbine_core::secrets::vault_dir().is_some() {
        if let Err(e) = dbine_core::secrets::delete_raw(KEY_NAME) {
            tracing::warn!(%e, "results store: the key couldn't be deleted");
        }
    }
    if d.join(FILE).exists() || d.join(format!("{FILE}.wal")).exists() {
        return Err(CommandError::Internal(
            "no se pudieron borrar los resultados guardados: el archivo sigue en uso, probá de nuevo en unos segundos".into(),
        ));
    }
    Ok(())
}

pub async fn status() -> CommandResult<Status> {
    let mut slot = lock_existing().await;
    let r = match &mut *slot {
        Slot::Open(s) => s.bytes().await.map(|bytes| Status {
            available: true,
            reason: None,
            bytes,
        }),
        Slot::Unavailable(why, _) => Ok(Status {
            available: false,
            reason: Some(*why),
            bytes: 0,
        }),
        // Nothing stored yet: whether it could be (the key needs the vault,
        // which is where the store's folder comes from).
        Slot::Closed => Ok(Status {
            available: dir().is_some(),
            reason: dir().is_none().then_some(Unavailable::NoVault),
            bytes: 0,
        }),
    };
    after(&mut slot, r).await
}

#[cfg(test)]
#[path = "results_store_tests.rs"]
mod tests;
