//! A local DuckDB database on disk, encrypted, on a DuckDB instance of its
//! own: what the results store (`results_store.rs`) and the data compare's
//! scratch tables (`commands/data_compare.rs`) are built on. What was
//! verified is in docs/results-store.md.
//!
//! The setup, in this order and checked before anything is written:
//! 1. [`configure`]: no extension loads or installs by itself
//!    ([`NO_EXTENSIONS`]); `httpfs` loaded (DuckDB writes encrypted files
//!    securely only with its OpenSSL); temp files in the caller's folder and
//!    encrypted (`temp_file_encryption`: verified to leak plaintext without
//!    it); a memory limit.
//! 2. [`attach`]: the file, with `ENCRYPTION_KEY`, under the caller's alias.
//! 3. [`lock`]: `USE` that alias, no external access, configuration locked;
//!    then every setting is read back and the session is refused if one is
//!    off.
//!
//! The key never goes to a log or an error: [`redact`] takes it out of every
//! DuckDB error.

use crate::error::{CommandError, CommandResult};
use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 256 bits from the OS generator, as hex.
pub(crate) fn new_key() -> Result<String, getrandom::Error> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// `e` without the key (a DuckDB error may quote the statement).
pub(crate) fn redact(e: impl ToString, key: &str) -> String {
    e.to_string().replace(key, "…")
}

/// A SQL string literal.
pub(crate) fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// No extension loads or installs by itself (only `httpfs`, explicitly).
pub(crate) const NO_EXTENSIONS: &str = "SET autoinstall_known_extensions = false; \
SET autoload_known_extensions = false; \
SET allow_community_extensions = false; \
SET allow_unsigned_extensions = false";

/// [`settings_sql_with`] at the default memory limit (what the results
/// store's tests check).
#[cfg(test)]
pub(crate) fn settings_sql(dir: &Path) -> Vec<String> {
    settings_sql_with(dir, MEMORY_LIMIT)
}

/// The memory DuckDB may take when the caller doesn't say (the results
/// store's); past it, it spills to the encrypted temp files.
pub(crate) const MEMORY_LIMIT: &str = "1GB";

/// The settings that must hold before anything is written. `httpfs` must
/// be loaded first: DuckDB's built-in crypto only reads encrypted files;
/// writing them securely takes the OpenSSL one `httpfs` brings (verified:
/// without it, `ATTACH … ENCRYPTION_KEY` is refused unless
/// `force_mbedtls_unsafe`, which is never set here and is checked).
pub(crate) fn settings_sql_with(dir: &Path, memory_limit: &str) -> Vec<String> {
    vec![
        format!(
            "SET temp_directory = {}",
            lit(&dir.join("tmp").to_string_lossy())
        ),
        "SET temp_file_encryption = true".into(),
        format!("SET memory_limit = {}", lit(memory_limit)),
    ]
}

/// Attach `file` of `dir` encrypted with `key`, as `alias`.
pub(crate) fn attach_sql(dir: &Path, file: &str, alias: &str, key: &str) -> String {
    format!(
        "ATTACH {} AS {alias} (ENCRYPTION_KEY {})",
        lit(&dir.join(file).to_string_lossy()),
        lit(key)
    )
}

/// After `USE <alias>` (see [`lock`]): nothing else can be attached, read
/// or written, and no setting can change.
pub(crate) const LOCK: &[&str] = &[
    "SET enable_external_access = false",
    "SET lock_configuration = true",
];

/// What [`lock`] checks: temp files encrypted, configuration locked, the
/// file encrypted and in use, the unsafe crypto and unsigned extensions off.
fn check_sql(alias: &str) -> String {
    format!(
        "SELECT current_setting('temp_file_encryption')::VARCHAR, \
current_setting('lock_configuration')::VARCHAR, \
current_setting('enable_external_access')::VARCHAR, \
(SELECT encrypted::VARCHAR FROM duckdb_databases() WHERE database_name = {}), \
current_database(), \
current_setting('force_mbedtls_unsafe')::VARCHAR, \
current_setting('allow_unsigned_extensions')::VARCHAR",
        lit(alias)
    )
}

/// The values [`check_sql`] must give, in its order (lowercase).
fn expected(alias: &str) -> [String; 7] {
    [
        "true".into(),
        "true".into(),
        "false".into(),
        "true".into(),
        alias.to_ascii_lowercase(),
        "false".into(),
        "false".into(),
    ]
}

pub(crate) enum SetupError {
    /// The file is there but this key doesn't open it (or it's damaged).
    Unreadable,
    /// `httpfs` couldn't be loaded (not downloaded yet and no network).
    Crypto(String),
    Other(String),
}

impl SetupError {
    /// What went wrong, without the key (for a log or an error message).
    pub(crate) fn reason(&self) -> String {
        match self {
            SetupError::Unreadable => "the file doesn't open with this key".into(),
            SetupError::Crypto(e) | SetupError::Other(e) => e.clone(),
        }
    }
}

/// Step 1: extensions off, `httpfs` loaded (installed if it isn't yet),
/// temp files in `dir/tmp` and encrypted, the memory limit.
pub(crate) async fn configure(
    s: &mut dyn Session,
    dir: &Path,
    key: &str,
) -> Result<(), SetupError> {
    configure_with(s, dir, key, MEMORY_LIMIT).await
}

/// [`configure`] with another memory limit.
pub(crate) async fn configure_with(
    s: &mut dyn Session,
    dir: &Path,
    key: &str,
    memory_limit: &str,
) -> Result<(), SetupError> {
    let other = |e: CommandError| SetupError::Other(redact(e, key));
    exec(s, NO_EXTENSIONS).await.map_err(other)?;
    if exec(s, "LOAD httpfs").await.is_err() {
        exec(s, "INSTALL httpfs; LOAD httpfs")
            .await
            .map_err(|e| SetupError::Crypto(redact(e, key)))?;
    }
    for sql in settings_sql_with(dir, memory_limit) {
        exec(s, &sql).await.map_err(other)?;
    }
    Ok(())
}

/// An alias goes into `ATTACH … AS` and `USE` unquoted: only lowercase
/// letters and `_` (the callers' constants), so no caller can turn it into
/// SQL.
fn valid_alias(alias: &str) -> Result<(), SetupError> {
    if !alias.is_empty() && alias.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        Ok(())
    } else {
        Err(SetupError::Other(format!(
            "alias de base local inválido: {alias:?}"
        )))
    }
}

/// Step 2: attach `file` of `dir` as `alias`; a file this key can't open is
/// [`SetupError::Unreadable`].
pub(crate) async fn attach(
    s: &mut dyn Session,
    dir: &Path,
    file: &str,
    alias: &str,
    key: &str,
) -> Result<(), SetupError> {
    valid_alias(alias)?;
    exec(s, &attach_sql(dir, file, alias, key))
        .await
        .map_err(|e| {
            let e = redact(e, key);
            let lower = e.to_lowercase();
            if lower.contains("encryption key")
                || lower.contains("encrypted database")
                || lower.contains("not a valid duckdb database")
                || lower.contains("corrupt")
            {
                SetupError::Unreadable
            } else {
                SetupError::Other(e)
            }
        })?;
    Ok(())
}

/// Step 3: `USE alias`, lock everything down, then read the settings back:
/// any of them off and the session is refused.
pub(crate) async fn lock(s: &mut dyn Session, alias: &str, key: &str) -> Result<(), SetupError> {
    valid_alias(alias)?;
    let other = |e: CommandError| SetupError::Other(redact(e, key));
    exec(s, &format!("USE {alias}")).await.map_err(other)?;
    for sql in LOCK {
        exec(s, sql).await.map_err(other)?;
    }
    let out = exec(s, &check_sql(alias)).await.map_err(other)?;
    let row: Vec<String> = first_row(&out)
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_ascii_lowercase())
        .collect();
    if row != expected(alias) {
        return Err(SetupError::Other(format!(
            "la base local «{alias}» no quedó aislada: {row:?}"
        )));
    }
    Ok(())
}

/// A DuckDB instance of its own: in memory, the driver's `isolated` option.
pub(crate) fn config() -> ConnectionConfig {
    let mut cfg = ConnectionConfig {
        driver: "duckdb".into(),
        host: ":memory:".into(),
        ..Default::default()
    };
    cfg.options.insert("isolated".into(), "true".into());
    cfg
}

/// A session on a DuckDB instance of its own ([`config`]), downloaded on
/// first use like any DuckDB session.
pub(crate) async fn open_session() -> dbine_driver::Result<Box<dyn Session>> {
    dbine_drivers::open_session(&config(), None).await
}

/// Delete files, trying again for a moment while one is still open (on
/// Windows a file DuckDB hasn't let go of yet can't be deleted).
pub(crate) async fn remove_paths(paths: &[PathBuf]) {
    for _ in 0..20 {
        let left: Vec<&PathBuf> = paths
            .iter()
            .filter(|p| match std::fs::remove_file(p) {
                Ok(()) => false,
                Err(e) => e.kind() != std::io::ErrorKind::NotFound,
            })
            .collect();
        if left.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Delete a folder and all it holds, trying again for a moment as
/// [`remove_paths`] does. `false`: something is still there.
pub(crate) async fn remove_dir(dir: &Path) -> bool {
    for _ in 0..20 {
        match std::fs::remove_dir_all(dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            _ => return true,
        }
    }
    !dir.exists()
}

/// Run `sql`; an error in the outcome is an error.
pub(crate) async fn exec(s: &mut dyn Session, sql: &str) -> CommandResult<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1_000, &mut out).await?;
    match out.error.take() {
        Some(e) => Err(CommandError::Sql(e)),
        None => Ok(out),
    }
}

pub(crate) fn first_row(out: &QueryOutcome) -> Vec<Value> {
    out.results
        .iter()
        .find(|r| !r.columns.is_empty())
        .and_then(|r| r.rows.first())
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn first_value(out: &QueryOutcome) -> Option<&Value> {
    out.results
        .iter()
        .find(|r| !r.columns.is_empty())
        .and_then(|r| r.rows.first())
        .and_then(|r| r.first())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_plain_aliases_are_accepted() {
        assert!(super::valid_alias("store").is_ok());
        assert!(super::valid_alias("cmp").is_ok());
        for bad in ["", "Store", "x; DROP", "a b", "a.b", "\"a\"", "a1"] {
            assert!(super::valid_alias(bad).is_err(), "{bad:?}");
        }
    }

    use super::*;

    #[test]
    fn the_check_follows_the_alias() {
        let sql = check_sql("cmp");
        assert!(sql.contains("database_name = 'cmp'"), "{sql}");
        assert_eq!(expected("cmp")[4], "cmp");
        assert_eq!(expected("store")[4], "store");
        // Every value the store checked before, in its order.
        assert_eq!(
            expected("store"),
            ["true", "true", "false", "true", "store", "false", "false"].map(String::from)
        );
        let e = SetupError::Other("x".into());
        assert_eq!(e.reason(), "x");
    }
}
