//! Forgetting a company's data: when DBine Enterprise ends a person's access
//! (signed out, revoked, a different account), nothing tied to the company's
//! connections may stay on this machine. This is the state store's part: the
//! managed connection rows (`temporary = 2`, ids `ent-…`) and every row that
//! points at one of those ids. The other stores (explorer cache, results,
//! memory) are in `src-tauri/src/enterprise/purge.rs`.

use crate::state::{db_err, StateChange, StateStore};
use crate::Result;

/// The prefix of a managed connection's id.
pub const COMPANY_PREFIX: &str = "ent-";

/// A LIKE pattern for `COMPANY_PREFIX` (no wildcard inside it).
const LIKE: &str = "ent-%";

impl StateStore {
    /// Deletes the managed connections and everything stored about them:
    /// their saved queries (and the queries' versions), migrations,
    /// contexts, history, backups' rows, lineage links and the scheduled
    /// tasks that name one. Returns the connection ids that were managed or
    /// named by one of those rows (so the caller can close their sessions).
    /// Idempotent; a normal connection's data is never touched. Not a change
    /// to sync: the revision does not move.
    pub fn purge_company_data(&self) -> Result<Vec<String>> {
        let mut ids: Vec<String> = Vec::new();
        {
            let mut c = self.lock()?;
            let tx = c.transaction().map_err(db_err)?;
            let mut collect = |sql: &str| -> Result<()> {
                let mut st = tx.prepare(sql).map_err(db_err)?;
                let rows = st
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(db_err)?;
                for r in rows {
                    ids.push(r.map_err(db_err)?);
                }
                Ok(())
            };
            collect("SELECT id FROM connections WHERE temporary = 2 OR id LIKE 'ent-%'")?;
            collect(
                "SELECT DISTINCT connection_id FROM query_history WHERE connection_id LIKE 'ent-%'",
            )?;
            collect(
                "SELECT DISTINCT connection_id FROM lineage_links WHERE connection_id LIKE 'ent-%'",
            )?;
            ids.sort();
            ids.dedup();
            // Versions of the saved queries that go.
            tx.execute(
                "DELETE FROM query_versions WHERE query_id IN
                   (SELECT id FROM queries WHERE connection_id LIKE ?1
                      OR connection_id IN (SELECT id FROM connections WHERE temporary = 2))",
                [LIKE],
            )
            .map_err(db_err)?;
            for table in [
                "queries",
                "migrations",
                "contexts",
                "backups",
                "query_history",
            ] {
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE connection_id LIKE ?1
                           OR connection_id IN (SELECT id FROM connections WHERE temporary = 2)"
                    ),
                    [LIKE],
                )
                .map_err(db_err)?;
            }
            tx.execute(
                "DELETE FROM lineage_links WHERE connection_id LIKE ?1
                   OR connection_id IN (SELECT id FROM connections WHERE temporary = 2)",
                [LIKE],
            )
            .map_err(db_err)?;
            // A task names its connections in its JSON; its runs go with it.
            tx.execute(
                "DELETE FROM task_runs WHERE task_id IN
                   (SELECT id FROM scheduled_tasks WHERE task_json LIKE '%\"connection_id\":\"ent-%')",
                [],
            )
            .map_err(db_err)?;
            tx.execute(
                "DELETE FROM scheduled_tasks WHERE task_json LIKE '%\"connection_id\":\"ent-%'",
                [],
            )
            .map_err(db_err)?;
            tx.execute(
                "DELETE FROM connections WHERE temporary = 2 OR id LIKE ?1",
                [LIKE],
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
        }
        self.notify(StateChange::new("connection", None));
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SavedConnection, SavedQuery};
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

    fn query(id: &str, conn: &str) -> SavedQuery {
        SavedQuery {
            id: id.into(),
            connection_id: conn.into(),
            database: String::new(),
            name: id.into(),
            sql: "SELECT 1".into(),
            updated_at: String::new(),
            last_run_at: None,
        }
    }

    fn count(s: &StateStore, sql: &str) -> i64 {
        s.lock().unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn the_company_goes_from_every_table_and_a_normal_connection_stays() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_connection(&conn("c1")).unwrap();
        s.set_managed_connections(&[conn("ent-x")]).unwrap();
        s.save_query(&query("q-c1", "c1")).unwrap();
        s.save_query(&query("q-ent", "ent-x")).unwrap();
        {
            let c = s.lock().unwrap();
            for id in ["q-c1", "q-ent"] {
                c.execute(
                    "INSERT INTO query_versions (query_id, saved_at, sql, hash) VALUES (?1, 'x', 's', 'h')",
                    [id],
                )
                .unwrap();
            }
            for cid in ["c1", "ent-x"] {
                c.execute(
                    "INSERT INTO query_history (connection_id, connection_name, driver, host, database, sql, started_at, duration_ms)
                     VALUES (?1, 'n', 'd', 'h', 'db', 'SELECT 1', 't', 1)",
                    [cid],
                )
                .unwrap();
                c.execute(
                    "INSERT INTO lineage_links (signature, connection_id, updated_at) VALUES (?1, ?2, 't')",
                    [format!("sig-{cid}"), cid.to_string()],
                )
                .unwrap();
                c.execute(
                    "INSERT INTO backups (id, connection_id, database, path, created_at, size, objects, rows, data, duration_ms)
                     VALUES (?1, ?2, 'db', 'p', 't', 1, 1, 1, 1, 1)",
                    [format!("b-{cid}"), cid.to_string()],
                )
                .unwrap();
                c.execute(
                    "INSERT INTO contexts (id, connection_id, database, name, body, active, created_at, updated_at)
                     VALUES (?1, ?2, '', 'n', 'b', 1, 't', 't')",
                    [format!("x-{cid}"), cid.to_string()],
                )
                .unwrap();
                c.execute(
                    "INSERT INTO scheduled_tasks (id, task_json, updated_at) VALUES (?1, ?2, 't')",
                    [
                        format!("t-{cid}"),
                        format!("{{\"steps\":[{{\"connection_id\":\"{cid}\"}}]}}"),
                    ],
                )
                .unwrap();
                c.execute(
                    "INSERT INTO task_runs (id, task_id, started_at, status, run_json) VALUES (?1, ?2, 't', 'ok', '{}')",
                    [format!("r-{cid}"), format!("t-{cid}")],
                )
                .unwrap();
            }
        }
        let rev = s.revision().unwrap();
        let ids = s.purge_company_data().unwrap();
        assert_eq!(ids, vec!["ent-x".to_string()]);
        assert_eq!(s.revision().unwrap(), rev, "not a change to sync");
        for (table, col) in [
            ("connections", "id"),
            ("queries", "connection_id"),
            ("query_history", "connection_id"),
            ("lineage_links", "connection_id"),
            ("backups", "connection_id"),
            ("contexts", "connection_id"),
        ] {
            assert_eq!(
                count(
                    &s,
                    &format!("SELECT COUNT(*) FROM {table} WHERE {col} LIKE 'ent-%'")
                ),
                0,
                "{table}"
            );
            assert_eq!(
                count(
                    &s,
                    &format!(
                        "SELECT COUNT(*) FROM {table} WHERE {col} = 'c1' OR {col} LIKE '%-c1'"
                    )
                ),
                1,
                "{table}: the normal connection's row stays"
            );
        }
        assert_eq!(count(&s, "SELECT COUNT(*) FROM query_versions"), 1);
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM query_versions WHERE query_id = 'q-c1'"
            ),
            1
        );
        assert_eq!(count(&s, "SELECT COUNT(*) FROM scheduled_tasks"), 1);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM task_runs"), 1);
        assert!(s.get_connection("c1").unwrap().is_some());
        assert!(s.list_managed_connections().unwrap().is_empty());
    }

    #[test]
    fn leftovers_with_no_managed_row_go_too_and_it_is_idempotent() {
        let s = StateStore::open_in_memory().unwrap();
        s.save_connection(&conn("c1")).unwrap();
        // History of a company connection whose row is already gone.
        s.lock()
            .unwrap()
            .execute(
                "INSERT INTO query_history (connection_id, connection_name, driver, host, database, sql, started_at, duration_ms)
                 VALUES ('ent-old', 'n', 'd', 'h', 'db', 'SELECT 1', 't', 1)",
                [],
            )
            .unwrap();
        assert_eq!(s.purge_company_data().unwrap(), vec!["ent-old".to_string()]);
        assert!(s.purge_company_data().unwrap().is_empty());
        assert_eq!(count(&s, "SELECT COUNT(*) FROM query_history"), 0);
        assert!(s.get_connection("c1").unwrap().is_some());
    }
}
