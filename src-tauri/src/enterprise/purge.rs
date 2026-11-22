//! When the company's access ends, nothing of it stays on this machine.
//!
//! DBine Enterprise's connections (ids `ent-…`, managed rows) and everything
//! DBine learned through them are the company's: if the person is fired, the
//! company revokes their access and this client must disconnect from it and
//! delete the local data tied to it. [`purge_company_data`] does all of it,
//! idempotently and best effort (a store that is missing or fails never
//! stops the rest); [`Why`] says what ended the access.
//!
//! Where the data lives, and what this clears:
//!
//! | what | where | cleared by |
//! |---|---|---|
//! | the managed connection rows (`temporary = 2`), their saved queries, migrations, contexts, history, backup rows, lineage links, scheduled tasks | `state.db` | `StateStore::purge_company_data` |
//! | the explorer cache, every kind (`databases`, `objects`, `columns`, `lineage`…) | `cache.db` | `ExplorerCache::forget_prefix` |
//! | query results (rows) | the results store | `results_store::purge_company` (they are never stored: this is the second lock) |
//! | live sessions, tunnels, function catalogs, kept object lists, typed and keychain-read secrets | memory | `AppState::forget_company_memory` |
//! | the cross-lineage index and what it resolved to company connections | memory | `cross::forget_company` |
//! | the gateway's drivers and their streams | memory | `gateway::forget_drivers` |
//! | the token and the sign-in | keychain, `local.enterprise` | `Enterprise::end_access` |
//! | the UI's open tabs, drafts and AI chats about company connections | the window | the `enterprise-purged` event |

use chrono::{DateTime, Utc};
use dbine_core::{ExplorerCache, StateStore};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::state::AppState;

/// How long the company's data stays usable when DBine Enterprise cannot be
/// reached: the last SUCCESSFUL validation must be younger than this. A
/// constant now; it can become a company setting later (the catalog's
/// `settings`, next to `explorer_cache`).
pub const OFFLINE_GRACE: Duration = Duration::from_secs(24 * 3600);

/// How often the access is validated while a company session is open, and
/// while none is.
pub const CHECK_WITH_SESSIONS: Duration = Duration::from_secs(60);
pub const CHECK_IDLE: Duration = Duration::from_secs(600);

/// What ended the access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The user signed out.
    SignOut,
    /// The server refused the token.
    SessionLost,
    /// The server said so (a revocation event, a 401/403 that held).
    Revoked,
    /// Another person or server signed in on this machine.
    OtherAccount,
    /// Managed data from a session that ended without signing out.
    Leftovers,
    /// The server couldn't be reached for longer than [`OFFLINE_GRACE`].
    OfflineTooLong,
}

impl Why {
    pub fn as_str(self) -> &'static str {
        match self {
            Why::SignOut => "sign_out",
            Why::SessionLost => "session_lost",
            Why::Revoked => "revoked",
            Why::OtherAccount => "other_account",
            Why::Leftovers => "leftovers",
            Why::OfflineTooLong => "offline_too_long",
        }
    }

    /// Whether the person is told why (they did not ask for it).
    pub fn tells_the_user(self) -> bool {
        matches!(self, Why::SessionLost | Why::Revoked | Why::OfflineTooLong)
    }

    /// What the UI tells the person (Spanish, like every backend message).
    pub fn message(self) -> &'static str {
        match self {
            Why::OfflineTooLong => "no se pudo verificar tu acceso a DBine Enterprise en las últimas 24 horas: se cerraron las conexiones de la empresa y se borraron sus datos locales",
            _ => "tu acceso a DBine Enterprise terminó: se cerraron las conexiones de la empresa y se borraron sus datos locales",
        }
    }
}

/// Bumped by every purge: a background task that reads company connections
/// remembers it when it starts and stops when it changes.
static GENERATION: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)] // for the background tasks that read company connections
pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// Whether the last successful validation is too old to keep the data while
/// the server can't be reached. Never validated: expired.
pub fn grace_expired(last_ok: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match last_ok {
        None => true,
        Some(t) => {
            now.signed_duration_since(t)
                >= chrono::Duration::from_std(OFFLINE_GRACE).unwrap_or(chrono::Duration::days(1))
        }
    }
}

/// The notification kinds that end the person's whole access (not a single
/// grant: `grant.revoked` only changes the catalog).
pub fn kind_ends_access(kind: &str) -> bool {
    matches!(kind, "session.revoked" | "access.revoked")
}

/// What an HTTP status on a call to DBine Enterprise or to the gateway says
/// about the session.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The token is no good: the session is lost.
    Ended,
    /// Refused, but it can be about one thing: ask the server before acting.
    Confirm,
    /// Nothing about the session.
    Not,
}

pub fn refusal(status: u16) -> Refusal {
    match status {
        401 => Refusal::Ended,
        403 => Refusal::Confirm,
        _ => Refusal::Not,
    }
}

/// What the stores that need no app hold of the company, gone. Returns the
/// ids of the connections that were the company's.
pub fn purge_stores(store: &StateStore, cache: Option<&ExplorerCache>) -> Vec<String> {
    let mut ids = match store.purge_company_data() {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("purging the company's state: {e}");
            Vec::new()
        }
    };
    if let Some(cache) = cache {
        if let Err(e) = cache.forget_prefix(dbine_core::purge::COMPANY_PREFIX) {
            tracing::warn!("purging the company's explorer cache: {e}");
        }
        for id in &ids {
            let _ = cache.forget_connection(id);
        }
    }
    ids.sort();
    ids
}

/// Everything: sessions and memory first (so nothing reads while the rows go),
/// then the stores, the results, the index and the gateway's drivers.
/// Idempotent; without an app state (tests, early start) only the store goes.
pub async fn purge_company_data(store: &StateStore, state: Option<&AppState>) -> Vec<String> {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    // The ids before the rows go: the managed ones, to close their sessions.
    let managed: Vec<String> = store
        .list_managed_connections()
        .map(|rows| rows.into_iter().map(|c| c.id).collect())
        .unwrap_or_default();
    let mut ids = managed;
    if let Some(state) = state {
        ids.extend(state.forget_company_memory(&ids));
    }
    super::gateway::forget_drivers();
    crate::commands::cross::forget_company();
    ids.extend(purge_stores(store, state.and_then(|s| s.cache.get())));
    if let Err(e) = crate::results_store::purge_company().await {
        tracing::warn!("purging the company's results: {e}");
    }
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_core::cache::kinds;
    use dbine_core::SavedConnection;
    use dbine_driver::ConnectionConfig;

    fn conn(id: &str) -> SavedConnection {
        SavedConnection {
            id: id.into(),
            name: id.into(),
            color: None,
            config: ConnectionConfig::default(),
            save_password: false,
            folder_id: None,
            tags: Vec::new(),
            mcp_level: None,
            updated_at: String::new(),
        }
    }

    #[test]
    fn the_grace_is_counted_from_the_last_successful_validation() {
        let now = Utc::now();
        let h = |n: i64| now - chrono::Duration::hours(n);
        assert!(!grace_expired(Some(h(1)), now));
        assert!(!grace_expired(Some(h(23)), now));
        assert!(grace_expired(Some(h(24)), now));
        assert!(grace_expired(Some(h(48)), now));
        assert!(grace_expired(None, now), "never validated: no grace");
    }

    #[test]
    fn only_a_revocation_of_the_whole_access_ends_it() {
        assert!(kind_ends_access("session.revoked"));
        assert!(kind_ends_access("access.revoked"));
        assert!(!kind_ends_access("grant.revoked"));
        assert!(!kind_ends_access("session.ended"));
        assert!(!kind_ends_access("catalog.changed"));
    }

    #[test]
    fn a_401_ends_the_session_a_403_is_confirmed_and_the_rest_is_not_about_it() {
        assert_eq!(refusal(401), Refusal::Ended);
        assert_eq!(refusal(403), Refusal::Confirm);
        for s in [200, 404, 429, 500, 502, 503] {
            assert_eq!(refusal(s), Refusal::Not, "{s}");
        }
    }

    #[test]
    fn only_an_access_that_ended_on_its_own_is_explained() {
        assert!(!Why::SignOut.tells_the_user());
        assert!(!Why::OtherAccount.tells_the_user());
        assert!(!Why::Leftovers.tells_the_user());
        for w in [Why::SessionLost, Why::Revoked, Why::OfflineTooLong] {
            assert!(w.tells_the_user());
        }
    }

    #[test]
    fn the_message_says_why_and_what_was_deleted() {
        for w in [Why::SessionLost, Why::Revoked] {
            assert!(w.message().contains("se borraron sus datos locales"));
        }
        assert!(Why::OfflineTooLong.message().contains("24 horas"));
    }

    #[test]
    fn the_stores_lose_the_company_and_keep_a_normal_connection() {
        let store = StateStore::open_in_memory().unwrap();
        let cache = ExplorerCache::open_in_memory().unwrap();
        store.save_connection(&conn("c1")).unwrap();
        store.set_managed_connections(&[conn("ent-x")]).unwrap();
        for id in ["c1", "ent-x"] {
            for kind in [
                kinds::DATABASES,
                kinds::OBJECTS,
                kinds::COLUMNS,
                "lineage",
                "lineage_sources",
            ] {
                cache.put(id, "db", kind, "i", "{}").unwrap();
            }
        }
        let ids = purge_stores(&store, Some(&cache));
        assert_eq!(ids, vec!["ent-x".to_string()]);
        assert!(store.list_managed_connections().unwrap().is_empty());
        assert!(store.get_connection("c1").unwrap().is_some());
        assert_eq!(cache.forget_prefix("ent-").unwrap(), 0);
        assert!(cache.get("c1", "db", "lineage", "i").unwrap().is_some());
        // Twice, and with no cache: nothing breaks.
        assert!(purge_stores(&store, None).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_memory_loses_the_company_and_keeps_a_normal_connection() {
        let state = AppState::new(StateStore::open_in_memory().unwrap());
        state.store.save_connection(&conn("c1")).unwrap();
        state
            .store
            .set_managed_connections(&[conn("ent-x")])
            .unwrap();
        let cache = ExplorerCache::open_in_memory().unwrap();
        cache.put("ent-x", "db", "lineage", "i", "{}").unwrap();
        cache.put("c1", "db", "lineage", "i", "{}").unwrap();
        let _ = state.cache.set(cache);
        for id in ["ent-x", "ent-typed", "c1"] {
            state.typed_secrets.insert(
                id.into(),
                [("password".to_string(), "s".to_string())].into(),
            );
            state.keychain_cache.insert(
                id.into(),
                [("password".to_string(), "s".to_string())].into(),
            );
        }
        state.sessions.insert("a".into(), entry("ent-x"));
        state.sessions.insert("b".into(), entry("c1"));
        let before = generation();
        let ids = purge_company_data(&state.store, Some(&state)).await;
        assert!(generation() > before);
        assert!(ids.contains(&"ent-x".to_string()));
        assert!(ids.contains(&"ent-typed".to_string()));
        assert!(!ids.contains(&"c1".to_string()));
        assert_eq!(state.sessions.len(), 1);
        assert!(state.sessions.get("b").is_some());
        assert!(state.typed_secrets.get("ent-x").is_none());
        assert!(state.typed_secrets.get("ent-typed").is_none());
        assert!(state.typed_secrets.get("c1").is_some());
        assert!(state.keychain_cache.get("ent-typed").is_none());
        assert!(state.keychain_cache.get("c1").is_some());
        let cache = state.cache.get().unwrap();
        assert!(cache.get("ent-x", "db", "lineage", "i").unwrap().is_none());
        assert!(cache.get("c1", "db", "lineage", "i").unwrap().is_some());
        assert!(state.store.list_managed_connections().unwrap().is_empty());
        // Idempotent.
        assert!(purge_company_data(&state.store, Some(&state))
            .await
            .is_empty());
    }

    use crate::state::SessionEntry;
    use dbine_driver::{async_trait, ColumnInfo, DbObject, ObjectRef, QueryOutcome, Session};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::{Mutex, Notify};

    struct Idle;

    #[async_trait]
    impl Session for Idle {
        async fn server_version(&mut self) -> dbine_driver::Result<String> {
            unimplemented!()
        }
        async fn list_databases(&mut self) -> dbine_driver::Result<Vec<String>> {
            unimplemented!()
        }
        async fn list_objects(&mut self) -> dbine_driver::Result<Vec<DbObject>> {
            unimplemented!()
        }
        async fn columns(&mut self, _: &ObjectRef) -> dbine_driver::Result<Vec<ColumnInfo>> {
            unimplemented!()
        }
        async fn definition(&mut self, _: &ObjectRef) -> dbine_driver::Result<Option<String>> {
            unimplemented!()
        }
        fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
            unimplemented!()
        }
        async fn execute(
            &mut self,
            _: &str,
            _: usize,
            _: &mut QueryOutcome,
        ) -> dbine_driver::Result<()> {
            unimplemented!()
        }
    }

    fn entry(connection: &str) -> Arc<SessionEntry> {
        Arc::new(SessionEntry {
            connection_id: connection.into(),
            database: "d".into(),
            session: Mutex::new(Box::new(Idle)),
            cancel: Notify::new(),
            cancelled: Default::default(),
            interrupter: None,
            autocommit: AtomicBool::new(true),
            switched_to: Default::default(),
        })
    }
}
