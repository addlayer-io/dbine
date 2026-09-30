//! Blocking chains and aborting a transaction.
//!
//! `SHOW LOCKS IN ACCOUNT` lists the running transactions with locks, each
//! `HOLDING` or `WAITING`; `resource` is the table (standard tables, lock
//! on its partitions) or, for a row lock of a hybrid table, the id of the
//! transaction waited for. A waiting transaction waits for that one, or
//! for the one holding its table. `SHOW TRANSACTIONS IN ACCOUNT` adds the
//! user and the session. Both run in the cloud services layer: no
//! warehouse is resumed. Without the rights for `IN ACCOUNT` (ACCOUNTADMIN
//! or MANAGE GRANTS) they fall back to the user's own transactions.
//!
//! Ids are transaction ids (signed 64-bit), what
//! `SYSTEM$ABORT_TRANSACTION` takes.

use crate::monitor::{Runner, Set};
use dbine_driver::{BlockedSession, Error, Result};
use std::collections::BTreeSet;

fn get<'a>(s: &'a Set, row: usize, name: &str) -> Option<&'a str> {
    let i = s.cols.iter().position(|c| c.eq_ignore_ascii_case(name))?;
    s.rows.get(row)?.get(i)?.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// Milliseconds since a timestamp as the SQL API gives it: epoch seconds
/// ("1721330303.831000000", maybe followed by a zone offset) or text.
fn ms_since(ts: &str, now_ms: i64) -> Option<u64> {
    let first = ts.split_whitespace().next()?;
    let at_ms = match first.parse::<f64>() {
        Ok(secs) => (secs * 1000.0) as i64,
        Err(_) => chrono::DateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S%.f %z")
            .or_else(|_| chrono::DateTime::parse_from_rfc3339(ts))
            .ok()?
            .timestamp_millis(),
    };
    Some((now_ms - at_ms).max(0) as u64)
}

/// A transaction id: a signed 64-bit integer.
fn txn_id(s: &str) -> Option<i64> {
    let s = s.trim();
    (!s.is_empty() && s.len() <= 20).then(|| s.parse().ok()).flatten()
}

/// The database of a fully qualified table name.
fn database_of(resource: &str) -> Option<String> {
    let first = resource.split('.').next()?.trim().trim_matches('"');
    (resource.contains('.') && !first.is_empty()).then(|| first.to_string())
}

/// The chain out of `SHOW LOCKS` and `SHOW TRANSACTIONS`.
pub(crate) fn chain(locks: &Set, txns: &Set, now_ms: i64) -> Vec<BlockedSession> {
    let n = locks.rows.len();
    let status = |r: usize| get(locks, r, "status").unwrap_or("").to_ascii_uppercase();
    let txn_row = |id: &str| (0..txns.rows.len()).find(|&r| get(txns, r, "id") == Some(id));
    let lock_row = |id: &str| (0..n).find(|&r| get(locks, r, "transaction") == Some(id));
    let base = |id: &str| {
        let t = txn_row(id);
        let l = lock_row(id);
        let session = t.and_then(|r| get(txns, r, "session")).or_else(|| l.and_then(|r| get(locks, r, "session")));
        let started = l
            .and_then(|r| get(locks, r, "transaction_started_on"))
            .or_else(|| t.and_then(|r| get(txns, r, "started_on")));
        BlockedSession {
            id: id.to_string(),
            user: t.and_then(|r| get(txns, r, "user")).map(str::to_string),
            client: session.map(|s| format!("sesión {s}")),
            waited_ms: started.and_then(|s| ms_since(s, now_ms)),
            ..Default::default()
        }
    };

    let mut out = Vec::new();
    let mut waiters = BTreeSet::new();
    let mut heads: Vec<(String, Option<String>)> = Vec::new();
    for r in 0..n {
        if status(r) != "WAITING" {
            continue;
        }
        let Some(id) = get(locks, r, "transaction").filter(|t| txn_id(t).is_some()) else { continue };
        if !waiters.insert(id.to_string()) {
            continue;
        }
        let resource = get(locks, r, "resource").unwrap_or("");
        // A hybrid-table row lock names the transaction it waits for;
        // a table lock, the table: its holder is the one waited for.
        let holder = match txn_id(resource) {
            Some(_) => Some(resource.to_string()),
            None => (0..n)
                .filter(|&h| status(h) == "HOLDING" && get(locks, h, "resource") == Some(resource))
                .filter_map(|h| get(locks, h, "transaction"))
                .find(|t| *t != id)
                .map(str::to_string),
        };
        let table = txn_id(resource).is_none().then(|| resource.to_string()).filter(|t| !t.is_empty());
        let mut s = base(id);
        s.blocked_by = holder.clone();
        s.wait = Some(format!("Esperando un bloqueo ({})", get(locks, r, "type").unwrap_or("PARTITIONS")));
        s.database = table.as_deref().and_then(database_of);
        s.object = table.clone();
        out.push(s);
        if let Some(h) = holder {
            heads.push((h, table));
        }
    }
    let mut seen = BTreeSet::new();
    for (h, table) in heads {
        if waiters.contains(&h) || !seen.insert(h.clone()) {
            continue;
        }
        let mut s = base(&h);
        s.wait = Some("Retiene el bloqueo".into());
        let held = lock_row(&h).and_then(|r| get(locks, r, "resource")).filter(|r| txn_id(r).is_none()).map(str::to_string);
        s.object = held.or(table);
        s.database = s.object.as_deref().and_then(database_of);
        out.push(s);
    }
    out
}

async fn show(r: &(dyn Runner + Sync), what: &str) -> Result<Set> {
    match r.rows(&format!("SHOW {what} IN ACCOUNT")).await {
        Ok(s) => Ok(s),
        // Without ACCOUNTADMIN / MANAGE GRANTS: the user's own.
        Err(_) => r.rows(&format!("SHOW {what}")).await,
    }
}

pub async fn blocking(r: &(dyn Runner + Sync)) -> Result<Vec<BlockedSession>> {
    let locks = show(r, "LOCKS").await?;
    if !(0..locks.rows.len()).any(|i| get(&locks, i, "status").is_some_and(|s| s.eq_ignore_ascii_case("WAITING"))) {
        return Ok(Vec::new());
    }
    let txns = show(r, "TRANSACTIONS").await.unwrap_or_default();
    Ok(chain(&locks, &txns, chrono::Utc::now().timestamp_millis()))
}

pub async fn kill(r: &(dyn Runner + Sync), id: &str) -> Result<()> {
    let n = txn_id(id).ok_or_else(|| Error::Query(format!("«{id}» no es un id de transacción de Snowflake")))?;
    r.rows(&format!("SELECT SYSTEM$ABORT_TRANSACTION({n})")).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(cols: &[&str], rows: &[&[&str]]) -> Set {
        Set {
            cols: cols.iter().map(|c| c.to_string()).collect(),
            rows: rows.iter().map(|r| r.iter().map(|v| (!v.is_empty()).then(|| v.to_string())).collect()).collect(),
        }
    }

    const LOCK_COLS: &[&str] = &["resource", "type", "transaction", "transaction_started_on", "status", "acquired_on", "query_id", "session"];

    #[test]
    fn table_locks() {
        let locks = set(
            LOCK_COLS,
            &[
                &["DB1.PUBLIC.T", "PARTITIONS", "1721330303831000000", "1721330300.000000000", "HOLDING", "1721330301.0", "q1", ""],
                &["DB1.PUBLIC.T", "PARTITIONS", "1721330310000000000", "1721330310.000000000", "WAITING", "", "q2", ""],
                &["DB1.PUBLIC.U", "PARTITIONS", "9", "1721330300.0", "HOLDING", "1721330300.0", "q3", ""],
            ],
        );
        let txns = set(
            &["id", "user", "session", "name", "started_on", "state", "scope"],
            &[&["1721330303831000000", "ANA", "123", "", "1721330300.0", "running", "0"], &["1721330310000000000", "BETO", "456", "", "", "running", "0"]],
        );
        let c = chain(&locks, &txns, 1_721_330_315_000);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].id, "1721330310000000000");
        assert_eq!(c[0].blocked_by.as_deref(), Some("1721330303831000000"));
        assert_eq!(c[0].user.as_deref(), Some("BETO"));
        assert_eq!(c[0].client.as_deref(), Some("sesión 456"));
        assert_eq!(c[0].waited_ms, Some(5000));
        assert_eq!(c[0].object.as_deref(), Some("DB1.PUBLIC.T"));
        assert_eq!(c[0].database.as_deref(), Some("DB1"));
        assert_eq!(c[1].id, "1721330303831000000");
        assert_eq!(c[1].blocked_by, None);
        assert_eq!(c[1].user.as_deref(), Some("ANA"));
        assert_eq!(c[1].waited_ms, Some(15_000));
    }

    #[test]
    fn hybrid_row_locks_name_the_holder() {
        let locks = set(
            LOCK_COLS,
            &[
                &["1111", "ROW", "2222", "1721330310.0", "WAITING", "", "q2", ""],
                &["DB.S.H", "ROW", "1111", "1721330300.0", "HOLDING", "1721330300.0", "q1", ""],
            ],
        );
        let c = chain(&locks, &Set::default(), 1_721_330_320_000);
        assert_eq!(c[0].blocked_by.as_deref(), Some("1111"));
        assert_eq!(c[1].id, "1111");
        assert_eq!(c[1].object.as_deref(), Some("DB.S.H"));
        assert!(chain(&set(LOCK_COLS, &[&["DB.S.H", "PARTITIONS", "1", "", "HOLDING", "", "", ""]]), &Set::default(), 0).is_empty());
    }

    #[test]
    fn ids_and_times() {
        assert_eq!(txn_id("1721330303831000000"), Some(1721330303831000000));
        assert_eq!(txn_id("-5"), Some(-5));
        assert_eq!(txn_id("1); DROP TABLE t"), None);
        assert_eq!(txn_id(""), None);
        assert_eq!(ms_since("1000.5 1440", 2_000_000), Some(999_500));
        assert_eq!(ms_since("2024-07-18 12:18:23.831 -0700", 1_721_330_304_831), Some(1000));
    }
}
