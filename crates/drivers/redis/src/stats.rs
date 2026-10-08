//! What the server already knows, for documenting a database
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: the key count of the session's logical database (`dbN`), from
//!   the `keys=` of `INFO keyspace`: a counter the server keeps, so no
//!   `SCAN` and no `DBSIZE` walk. Keys have no per-key statistics worth the
//!   name, so the estimate is per database, with the `database` kind.
//! - Comments: Redis, Valkey and Dragonfly keep none.

use crate::{shape, RedisSession};
use dbine_driver::stats::RowEstimate;
use dbine_driver::{ObjectRef, Result};

/// The object kind of a logical database's estimate.
pub(crate) const KIND_DATABASE: &str = "database";

/// `keys=` of the `dbN:` line of `INFO keyspace`. A database with no keys
/// has no line: zero.
pub(crate) fn keys_in(keyspace: &str, db: i64) -> u64 {
    let prefix = format!("db{db}:");
    keyspace
        .lines()
        .find_map(|l| l.trim().strip_prefix(prefix.as_str()))
        .and_then(|line| line.split(',').find_map(|p| p.strip_prefix("keys=")))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

impl RedisSession {
    pub(crate) async fn stats_rows(&mut self) -> Result<Vec<RowEstimate>> {
        let keyspace = shape::text_of(&self.run(&[b"INFO", b"keyspace"]).await?);
        let object = ObjectRef { kind: KIND_DATABASE.into(), schema: None, name: format!("db{}", self.db) };
        Ok(vec![RowEstimate { object, rows: keys_in(&keyspace, self.db) }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_from_info_keyspace() {
        let info = "# Keyspace\r\ndb0:keys=12,expires=3,avg_ttl=45000\r\ndb10:keys=1,expires=0,avg_ttl=0\r\n";
        assert_eq!(keys_in(info, 0), 12);
        assert_eq!(keys_in(info, 10), 1);
        // db1 isn't db10, and an empty database has no line.
        assert_eq!(keys_in(info, 1), 0);
        assert_eq!(keys_in("# Keyspace\r\n", 0), 0);
    }
}
