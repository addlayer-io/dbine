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

/// Which databases of one connection a purge reaches.
#[derive(Debug, Clone, Copy)]
pub enum Databases<'a> {
    /// Exactly these.
    Named(&'a [String]),
    /// Every named database except these (what was kept after the person's
    /// access went from "all" to a list).
    AllBut(&'a [String]),
}

impl Databases<'_> {
    /// A SQL condition on `database`, with its parameters (`?2`, `?3`…:
    /// `?1` is the connection).
    fn condition(&self) -> (String, Vec<String>) {
        let (list, negate) = match self {
            Databases::Named(l) => (*l, false),
            Databases::AllBut(l) => (*l, true),
        };
        let marks = |n: usize| {
            (0..n)
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let sql = match (negate, list.is_empty()) {
            (false, true) => "0".to_string(),
            (false, false) => format!("database IN ({})", marks(list.len())),
            (true, true) => "database <> ''".to_string(),
            (true, false) => format!(
                "(database <> '' AND database NOT IN ({}))",
                marks(list.len())
            ),
        };
        (sql, list.to_vec())
    }
}

impl StateStore {
    /// Every connection id the company has left something under: managed
    /// rows and the ids of the rows that point at one (`ent-…`).
    pub fn company_connection_ids(&self) -> Result<Vec<String>> {
        let c = self.lock()?;
        let mut ids: Vec<String> = Vec::new();
        for sql in [
            "SELECT id FROM connections WHERE temporary = 2 OR id LIKE 'ent-%'",
            "SELECT DISTINCT connection_id FROM query_history WHERE connection_id LIKE 'ent-%'",
            "SELECT DISTINCT connection_id FROM lineage_links WHERE connection_id LIKE 'ent-%'",
            "SELECT DISTINCT connection_id FROM queries WHERE connection_id LIKE 'ent-%'",
            "SELECT DISTINCT connection_id FROM migrations WHERE connection_id LIKE 'ent-%'",
            "SELECT DISTINCT connection_id FROM contexts WHERE connection_id LIKE 'ent-%'",
            "SELECT DISTINCT connection_id FROM backups WHERE connection_id LIKE 'ent-%'",
        ] {
            let mut st = c.prepare(sql).map_err(db_err)?;
            let rows = st
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(db_err)?;
            for r in rows {
                ids.push(r.map_err(db_err)?);
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// Deletes one company connection as a whole: its row (when managed) and
    /// everything stored about it, as [`StateStore::purge_company_data`] does
    /// for all of them. Every other connection's data stays. Idempotent; not
    /// a change to sync.
    pub fn purge_connection_data(&self, id: &str) -> Result<()> {
        {
            let mut c = self.lock()?;
            let tx = c.transaction().map_err(db_err)?;
            tx.execute(
                "DELETE FROM query_versions WHERE query_id IN (SELECT id FROM queries WHERE connection_id = ?1)",
                [id],
            )
            .map_err(db_err)?;
            for table in [
                "queries",
                "migrations",
                "contexts",
                "backups",
                "query_history",
                "lineage_links",
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE connection_id = ?1"),
                    [id],
                )
                .map_err(db_err)?;
            }
            let needle = format!("\"connection_id\":\"{id}\"");
            tx.execute(
                "DELETE FROM task_runs WHERE task_id IN
                   (SELECT id FROM scheduled_tasks WHERE instr(task_json, ?1) > 0)",
                [&needle],
            )
            .map_err(db_err)?;
            tx.execute(
                "DELETE FROM scheduled_tasks WHERE instr(task_json, ?1) > 0",
                [&needle],
            )
            .map_err(db_err)?;
            tx.execute(
                "DELETE FROM connections WHERE id = ?1 AND (temporary = 2 OR id LIKE 'ent-%')",
                [id],
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
        }
        self.notify(StateChange::new("connection", None));
        Ok(())
    }

    /// Deletes what is stored about some databases of one company
    /// connection (saved queries and their versions, migrations, contexts,
    /// backups' rows, history, and the scheduled tasks that name the
    /// database), leaving the connection and its other databases alone.
    pub fn purge_database_data(&self, id: &str, which: Databases<'_>) -> Result<()> {
        let (cond, dbs) = which.condition();
        {
            let mut c = self.lock()?;
            let tx = c.transaction().map_err(db_err)?;
            let params = |extra: &[&str]| -> Vec<String> {
                std::iter::once(id.to_string())
                    .chain(dbs.iter().cloned())
                    .chain(extra.iter().map(|s| s.to_string()))
                    .collect()
            };
            tx.execute(
                &format!(
                    "DELETE FROM query_versions WHERE query_id IN
                       (SELECT id FROM queries WHERE connection_id = ?1 AND {cond})"
                ),
                rusqlite::params_from_iter(params(&[])),
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
                    &format!("DELETE FROM {table} WHERE connection_id = ?1 AND {cond}"),
                    rusqlite::params_from_iter(params(&[])),
                )
                .map_err(db_err)?;
            }
            // A task that names this connection and one of those databases.
            if let Databases::Named(list) = which {
                let conn_needle = format!("\"connection_id\":\"{id}\"");
                for d in list {
                    let db_needle = format!("\"database\":\"{d}\"");
                    tx.execute(
                        "DELETE FROM task_runs WHERE task_id IN
                           (SELECT id FROM scheduled_tasks
                             WHERE instr(task_json, ?1) > 0 AND instr(task_json, ?2) > 0)",
                        [&conn_needle, &db_needle],
                    )
                    .map_err(db_err)?;
                    tx.execute(
                        "DELETE FROM scheduled_tasks WHERE instr(task_json, ?1) > 0 AND instr(task_json, ?2) > 0",
                        [&conn_needle, &db_needle],
                    )
                    .map_err(db_err)?;
                }
            }
            tx.commit().map_err(db_err)?;
        }
        self.notify(StateChange::new("connection", None));
        Ok(())
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

    fn query_in(id: &str, conn: &str, db: &str) -> SavedQuery {
        SavedQuery {
            database: db.into(),
            ..query(id, conn)
        }
    }

    fn history(s: &StateStore, conn: &str, db: &str) {
        s.lock()
            .unwrap()
            .execute(
                "INSERT INTO query_history (connection_id, connection_name, driver, host, database, sql, started_at, duration_ms)
                 VALUES (?1, 'n', 'd', 'h', ?2, 'SELECT 1', 't', 1)",
                [conn, db],
            )
            .unwrap();
    }

    fn rows(s: &StateStore, table: &str, conn: &str) -> i64 {
        count(
            s,
            &format!("SELECT COUNT(*) FROM {table} WHERE connection_id = '{conn}'"),
        )
    }

    fn setup() -> StateStore {
        let s = StateStore::open_in_memory().unwrap();
        s.save_connection(&conn("c1")).unwrap();
        s.set_managed_connections(&[conn("ent-a"), conn("ent-b")])
            .unwrap();
        for c in ["c1", "ent-a", "ent-b"] {
            for d in ["d1", "d2"] {
                s.save_query(&query_in(&format!("q-{c}-{d}"), c, d))
                    .unwrap();
                history(&s, c, d);
            }
        }
        s
    }

    #[test]
    fn one_connection_goes_whole_and_the_others_stay() {
        let s = setup();
        s.purge_connection_data("ent-a").unwrap();
        assert_eq!(rows(&s, "queries", "ent-a"), 0);
        assert_eq!(rows(&s, "query_history", "ent-a"), 0);
        assert!(s.get_connection("ent-a").unwrap().is_none());
        for c in ["ent-b", "c1"] {
            assert_eq!(rows(&s, "queries", c), 2, "{c}");
            assert_eq!(rows(&s, "query_history", c), 2, "{c}");
            assert!(s.get_connection(c).unwrap().is_some(), "{c}");
        }
        s.purge_connection_data("ent-a").unwrap();
        assert_eq!(
            s.company_connection_ids().unwrap(),
            vec!["ent-b".to_string()]
        );
    }

    #[test]
    fn one_database_goes_and_its_neighbours_stay() {
        let s = setup();
        let d1 = vec!["d1".to_string()];
        s.purge_database_data("ent-a", Databases::Named(&d1))
            .unwrap();
        assert_eq!(rows(&s, "queries", "ent-a"), 1);
        assert_eq!(rows(&s, "query_history", "ent-a"), 1);
        assert!(s.get_connection("ent-a").unwrap().is_some());
        // Same database name on other connections: untouched.
        for c in ["ent-b", "c1"] {
            assert_eq!(rows(&s, "queries", c), 2, "{c}");
            assert_eq!(rows(&s, "query_history", c), 2, "{c}");
        }
        // Everything but d2 (access went from all to [d2]).
        s.purge_database_data("ent-b", Databases::AllBut(&["d2".to_string()]))
            .unwrap();
        assert_eq!(rows(&s, "queries", "ent-b"), 1);
        assert_eq!(
            count(
                &s,
                "SELECT COUNT(*) FROM queries WHERE connection_id = 'ent-b' AND database = 'd2'"
            ),
            1
        );
        assert_eq!(rows(&s, "queries", "c1"), 2);
    }
}
