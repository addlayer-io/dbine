//! The explorer's pages (`Session::list_objects_page`) for SQLite and
//! libSQL: a folder of the tree, a name search or a lookup as one query on
//! `sqlite_master`, narrowed to the kinds and names asked for, sorted as the
//! tree shows them (name ignoring case, then as written) and cut to the page
//! with `LIMIT … OFFSET` (the cursor is the offset, as `objects::page_of`'s).
//! Names and patterns go as bound values; the SQL holds only catalog text
//! and numbers.
//!
//! Every object here is at the database level (no schema).

use crate::schema;
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery};

/// The function the local SQLite connection registers ([`register_fold`])
/// to compare names ignoring case as the app does (Unicode lower case);
/// SQLite's `lower()` only folds ASCII. libSQL's server can't take one, so
/// it folds with `lower()`.
pub const FOLD: &str = "dbine_fold";

/// Registers [`FOLD`] on `c` (replacing it if it's there).
pub fn register_fold(c: &rusqlite::Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    c.create_scalar_function(
        FOLD,
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            Ok(match ctx.get_raw(0) {
                rusqlite::types::ValueRef::Text(t) => {
                    Some(String::from_utf8_lossy(t).to_lowercase())
                }
                _ => None,
            })
        },
    )
}

/// What `list_objects` reads from `sqlite_master m`, in this order: the
/// type, the name, the table (a trigger's) and the `CREATE` text.
pub const COLUMNS: &str = "m.type, m.name, m.tbl_name, COALESCE(m.sql, '')";

/// The objects `list_objects` leaves out, as a condition on `m`: SQLite's
/// own and the vector features' ([`schema::NOT_VECTOR_SHADOW`]). `extra`:
/// the engine's own on top (libSQL's), or `""`.
pub fn not_internal(extra: &str) -> String {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!("{extra} AND ")
    };
    format!(
        "m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND {extra}{}",
        schema::NOT_VECTOR_SHADOW
    )
}

/// The kinds `list_objects` returns, each with the condition that picks
/// it out of `sqlite_master` (a virtual table's `CREATE` text as
/// [`schema::is_virtual`] reads it).
const KINDS: &[(&str, &str)] = &[
    (
        kinds::TABLE,
        "m.type = 'table' AND NOT (upper(substr(ltrim(COALESCE(m.sql, '')), 1, 14)) = 'CREATE VIRTUAL')",
    ),
    (
        schema::VIRTUAL_TABLE,
        "m.type = 'table' AND (upper(substr(ltrim(COALESCE(m.sql, '')), 1, 14)) = 'CREATE VIRTUAL')",
    ),
    (kinds::VIEW, "m.type = 'view'"),
    (kinds::TRIGGER, "m.type = 'trigger'"),
];

/// A row of [`COLUMNS`] as the explorer's object (what `list_objects`
/// returns for it).
pub fn object(ty: &str, name: String, tbl: Option<String>, sql: &str) -> DbObject {
    let (kind, parent) = match ty {
        "table" if schema::is_virtual(sql) => (schema::VIRTUAL_TABLE, None),
        "table" => (kinds::TABLE, None),
        "view" => (kinds::VIEW, None),
        _ => (kinds::TRIGGER, tbl),
    };
    DbObject {
        kind: kind.into(),
        schema: None,
        name,
        parent,
    }
}

/// Whether `q` keeps objects of `kind`.
fn wants(q: &ObjectQuery, kind: &str) -> bool {
    q.kind.as_deref().is_none_or(|k| k == kind) && !q.not_kinds.iter().any(|k| k == kind)
}

/// A page's query: the SQL, its bound values in order, and the slice of
/// the sorted list it asks for.
#[derive(Debug)]
pub struct PageQuery {
    pub sql: String,
    pub params: Vec<String>,
    offset: usize,
    size: usize,
}

/// The query for page `q`, or `None` when no object can match (a schema
/// asked for: nothing has one here, or no kind left). `fold` lower-cases a
/// name ([`FOLD`] or `lower`); `extra` is what [`not_internal`] takes.
pub fn page_query(q: &ObjectQuery, fold: &str, extra: &str) -> Option<PageQuery> {
    if !q.no_schema && q.schema.is_some() {
        return None;
    }
    let picked: Vec<String> = KINDS
        .iter()
        .filter(|(k, _)| wants(q, k))
        .map(|(_, c)| format!("({c})"))
        .collect();
    if picked.is_empty() {
        return None;
    }
    let mut conds = vec![
        if picked.len() == KINDS.len() {
            "m.type IN ('table', 'view', 'trigger')".to_string()
        } else {
            format!("({})", picked.join(" OR "))
        },
        not_internal(extra),
    ];
    let mut params = Vec::new();
    if let Some(n) = &q.name {
        conds.push(format!("{fold}(m.name) = {fold}(?)"));
        params.push(n.clone());
    }
    if let Some(p) = &q.pattern {
        conds.push(format!("instr({fold}(m.name), {fold}(?)) > 0"));
        params.push(p.clone());
    }
    let offset = q
        .after
        .as_deref()
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(0);
    let size = q.page_size();
    // The type last: a table and a trigger may share a name (`list_objects`
    // breaks the tie the same way).
    let sql = format!(
        "SELECT {COLUMNS} FROM sqlite_master m WHERE {} ORDER BY {fold}(m.name), m.name, m.type LIMIT {} OFFSET {offset}",
        conds.join(" AND "),
        size.saturating_add(1)
    );
    Some(PageQuery {
        sql,
        params,
        offset,
        size,
    })
}

impl PageQuery {
    /// The page out of the query's rows of [`COLUMNS`] (one more than the
    /// page when more remain).
    pub fn page(&self, rows: Vec<(String, String, Option<String>, String)>) -> ObjectPage {
        let more = rows.len() > self.size;
        let objects = rows
            .into_iter()
            .take(self.size)
            .map(|(ty, name, tbl, sql)| object(&ty, name, tbl, &sql))
            .collect();
        ObjectPage {
            objects,
            next: more.then(|| (self.offset + self.size).to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_reads_only_its_kind() {
        let q = ObjectQuery {
            no_schema: true,
            kind: Some("view".into()),
            limit: 10,
            ..Default::default()
        };
        let p = page_query(&q, FOLD, "").unwrap();
        assert!(p.params.is_empty());
        assert!(p
            .sql
            .contains("WHERE ((m.type = 'view')) AND m.name NOT LIKE 'sqlite\\_%'"));
        assert!(p.sql.contains("libsql_vector_meta_shadow"), "{}", p.sql);
        assert!(p
            .sql
            .ends_with("ORDER BY dbine_fold(m.name), m.name, m.type LIMIT 11 OFFSET 0"));

        // Plain tables leave the virtual ones out; triggers keep their
        // table as the parent.
        let q = ObjectQuery {
            kind: Some("table".into()),
            after: Some("20".into()),
            limit: 10,
            ..Default::default()
        };
        let p = page_query(&q, "lower", "").unwrap();
        assert!(p.sql.contains(
            "((m.type = 'table' AND NOT (upper(substr(ltrim(COALESCE(m.sql, '')), 1, 14)) = 'CREATE VIRTUAL')))"
        ));
        assert!(p
            .sql
            .ends_with("ORDER BY lower(m.name), m.name, m.type LIMIT 11 OFFSET 20"));
        let rows = vec![
            (
                "table".into(),
                "fts".into(),
                Some("fts".into()),
                "CREATE VIRTUAL TABLE fts USING fts5(a)".into(),
            ),
            (
                "trigger".into(),
                "tr".into(),
                Some("t".into()),
                "CREATE TRIGGER tr …".into(),
            ),
        ];
        let page = p.page(rows);
        assert_eq!(page.objects[0].kind, "virtual_table");
        assert_eq!(page.objects[0].parent, None);
        assert_eq!(page.objects[1].kind, "trigger");
        assert_eq!(page.objects[1].parent.as_deref(), Some("t"));
        assert_eq!(page.next, None);
    }

    #[test]
    fn user_text_is_bound() {
        let q = ObjectQuery {
            pattern: Some("50%_off'".into()),
            name: Some("x' OR 1=1 --".into()),
            ..Default::default()
        };
        let p = page_query(&q, FOLD, "m.name NOT LIKE 'libsql\\_%' ESCAPE '\\'").unwrap();
        assert!(
            !p.sql.contains("50%") && !p.sql.contains("OR 1=1"),
            "{}",
            p.sql
        );
        assert_eq!(p.params, ["x' OR 1=1 --", "50%_off'"]);
        assert!(p.sql.contains("m.type IN ('table', 'view', 'trigger')"));
        assert!(p.sql.contains("dbine_fold(m.name) = dbine_fold(?)"));
        assert!(p
            .sql
            .contains("instr(dbine_fold(m.name), dbine_fold(?)) > 0"));
        assert!(p
            .sql
            .contains("ESCAPE '\\' AND m.name NOT LIKE 'libsql\\_%' ESCAPE '\\' AND m.name <> 'libsql_vector_meta_shadow'"));
        assert!(p.sql.ends_with("LIMIT 1001 OFFSET 0"), "the default page");
    }

    #[test]
    fn kinds_left_out_and_schemas_asked_for() {
        // "Otros": every kind here is declared.
        let q = ObjectQuery {
            not_kinds: ["table", "virtual_table", "view", "trigger"]
                .map(String::from)
                .to_vec(),
            ..Default::default()
        };
        assert!(page_query(&q, FOLD, "").is_none());
        // Everything but tables: the other three.
        let q = ObjectQuery {
            not_kinds: vec!["table".into()],
            ..Default::default()
        };
        let p = page_query(&q, FOLD, "").unwrap();
        assert_eq!(p.sql.matches("m.type = ").count(), 3, "{}", p.sql);
        // No object has a schema.
        let q = ObjectQuery {
            schema: Some("main".into()),
            ..Default::default()
        };
        assert!(page_query(&q, FOLD, "").is_none());
        let q = ObjectQuery {
            schema: Some("main".into()),
            no_schema: true,
            ..Default::default()
        };
        assert!(page_query(&q, FOLD, "").is_some());
    }

    #[test]
    fn a_bad_cursor_starts_over_and_more_rows_give_the_next() {
        let q = ObjectQuery {
            after: Some("x".into()),
            limit: 2,
            ..Default::default()
        };
        let p = page_query(&q, FOLD, "").unwrap();
        assert!(p.sql.ends_with("LIMIT 3 OFFSET 0"));
        assert_eq!(p.page(Vec::new()), ObjectPage::default());
        let row = |n: &str| {
            (
                "view".to_string(),
                n.to_string(),
                Some(n.to_string()),
                String::new(),
            )
        };
        let page = p.page(vec![row("a"), row("b"), row("c")]);
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.next.as_deref(), Some("2"));
    }

    #[test]
    fn the_fold_lowers_beyond_ascii() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        register_fold(&c).unwrap();
        let (f, l): (String, String) = c
            .query_row(
                "SELECT dbine_fold('AÑOS Ü'), lower('AÑOS Ü')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(f, "años ü");
        assert_eq!(l, "aÑos Ü", "SQLite's own folds only ASCII");
        let n: Option<String> = c
            .query_row("SELECT dbine_fold(NULL)", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, None);
    }
}
