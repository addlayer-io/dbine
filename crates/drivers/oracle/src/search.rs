//! "Buscar en la base" from the dictionary ([`dbine_driver::Session::search_code`]).
//!
//! The objects are the explorer's (`list_objects`), and each one's source
//! is the text `definition` gives it, read in bulk: DBMS_METADATA.GET_DDL
//! of every view, materialized view, procedure, function, package and
//! trigger in one query, and of the tables in another (each one followed
//! by its standalone indexes, as `with_indexes` does), both narrowed on
//! the server with DBMS_LOB.INSTR; types from ALL_SOURCE, sequences from
//! ALL_SEQUENCES and synonyms from ALL_SYNONYMS, put together with the
//! same builders. Lines are matched with the contract's rule, so the hits
//! equal those of the app's per-object scan. When a bulk read fails (no
//! privilege on DBMS_METADATA for an object, a materialized view it can't
//! describe yet…) the app's scan reads them one by one instead.

use crate::structure::{sequence_sql, SequenceInfo};
use crate::{err, object_kind, plsql_source, with_indexes, OracleSession, PACKAGE, SET_METADATA_TRANSFORMS};
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::{kinds, Result, Session};
use oracledb::{Connection, Row};
use std::collections::HashMap;

/// Sources by (kind, owner, name).
type Sources = HashMap<(String, String, String), String>;

/// The objects' common filter (as `LIST_OBJECTS`).
const LISTED: &str = "o.owner = :1 AND o.generated = 'N' AND o.secondary = 'N' AND o.object_name NOT LIKE 'BIN$%'";

/// Standalone indexes of the owner's tables (as `TABLE_INDEXES`).
const INDEXES: &str = "SELECT i.owner, i.index_name, i.table_name FROM all_indexes i
  WHERE i.table_owner = :1
    AND i.generated = 'N'
    AND i.index_type NOT IN ('LOB', 'IOT - TOP', 'CLUSTER')
    AND NOT EXISTS (
          SELECT 1 FROM all_constraints k
           WHERE k.owner = i.table_owner AND k.table_name = i.table_name
             AND k.index_owner = i.owner AND k.index_name = i.index_name
             AND k.constraint_type IN ('P', 'U'))";

const SEQUENCES: &str = "SELECT sequence_name, TO_CHAR(min_value), TO_CHAR(max_value), TO_CHAR(increment_by), cycle_flag,
        order_flag, cache_size, TO_CHAR(last_number)
   FROM all_sequences WHERE sequence_owner = :1";

/// 12c+ (18c for SCALE / EXTEND): errors are ignored on older servers.
const SEQUENCES_EXTRA: &str =
    "SELECT sequence_name, scale_flag, extend_flag, session_flag, keep_value FROM all_sequences WHERE sequence_owner = :1";

const SYNONYMS: &str = "SELECT owner, synonym_name, table_owner, table_name, db_link FROM all_synonyms
  WHERE owner = :1 OR (owner = 'PUBLIC' AND table_owner = :2)";

const TYPES: &str = "SELECT name, line, text FROM all_source WHERE owner = :1 AND type IN ('TYPE', 'TYPE BODY')
  ORDER BY name, CASE WHEN type LIKE '% BODY' THEN 2 ELSE 1 END, line";

/// Whether `expr` may hold `q`'s text, with DBMS_LOB.INSTR and the text
/// bound to `bind`: as is when the case matters, both in upper case when
/// not (only for ASCII text: Oracle's case folding may not be Rust's for
/// the rest, and then nothing is narrowed).
fn narrow(expr: &str, bind: &str, q: &CodeSearch) -> (String, Option<String>) {
    if q.case_sensitive {
        (format!("DBMS_LOB.INSTR({expr}, {bind}) > 0"), Some(q.text.clone()))
    } else if q.text.is_ascii() {
        (format!("DBMS_LOB.INSTR(UPPER({expr}), {bind}) > 0"), Some(q.text.to_ascii_uppercase()))
    } else {
        ("1 = 1".into(), None)
    }
}

fn text(r: &Row, i: usize) -> Result<Option<String>> {
    r.get::<Option<String>>(i).map_err(err)
}

fn yes(v: Option<String>) -> bool {
    v.as_deref() == Some("Y")
}

/// `columns` of the rows of `objects` (their last one, the DDL, named
/// `ddl`) where `matches`. The objects are filtered first (`ROWNUM`
/// keeps the view from being merged): DBMS_METADATA would otherwise be
/// asked about rows the filter drops, and fail on them.
fn described(objects: &str, columns: &str, matches: &str) -> String {
    format!("SELECT * FROM (SELECT {columns} AS ddl FROM ({objects} AND ROWNUM > 0) x) WHERE {matches}")
}

/// The rows of `sql`, its binds in order (a name used twice is bound twice).
fn rows(c: &Connection, sql: &str, binds: &[&str]) -> Result<Vec<Row>> {
    let params: Vec<&dyn oracledb::ToDbValue> = binds.iter().map(|b| b as &dyn oracledb::ToDbValue).collect();
    c.statement(sql).map_err(err)?.exclude_from_cache().build().map_err(err)?.query(&params).map_err(err)?.map(|r| r.map_err(err)).collect()
}

/// Every wanted kind's sources, read in bulk.
fn sources(c: &Connection, owner: &str, schema: &str, wanted: &[String], tables: &[String], q: &CodeSearch) -> Result<Sources> {
    let want = |k: &str| wanted.iter().any(|w| w == k);
    let mut out = Sources::new();
    let ddl_types: Vec<&str> = [
        ("view", "VIEW"),
        ("materialized_view", "MATERIALIZED VIEW"),
        ("procedure", "PROCEDURE"),
        ("function", "FUNCTION"),
        (PACKAGE, "PACKAGE"),
        ("trigger", "TRIGGER"),
    ]
    .into_iter()
    .filter(|(k, _)| want(k))
    .map(|(_, t)| t)
    .collect();
    if !ddl_types.is_empty() {
        let (matches, needle) = narrow("ddl", ":2", q);
        let list = ddl_types.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(", ");
        let sql = described(
            &format!("SELECT o.object_type, o.object_name, o.owner FROM all_objects o WHERE {LISTED} AND o.object_type IN ({list})"),
            "x.object_type, x.object_name, DBMS_METADATA.GET_DDL(REPLACE(x.object_type, ' ', '_'), x.object_name, x.owner)",
            &matches,
        );
        let binds: Vec<&str> = std::iter::once(owner).chain(needle.as_deref()).collect();
        for r in rows(c, &sql, &binds)? {
            let (Some(ty), Some(name), Some(ddl)) = (text(&r, 0)?, text(&r, 1)?, text(&r, 2)?) else { continue };
            let Some(kind) = object_kind(&ty) else { continue };
            out.insert((kind.into(), owner.into(), name), ddl.trim().to_string());
        }
    }
    if want(kinds::TABLE) {
        // Two queries: DBMS_METADATA can't describe an index while it's
        // describing a table in the same statement (ORA-01002).
        let (matches, needle) = narrow("ddl", ":2", q);
        let sql = described(
            &format!(
                "SELECT o.object_name, o.owner FROM all_objects o
                  WHERE {LISTED} AND o.object_type = 'TABLE'
                    AND NOT EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = o.owner AND m.mview_name = o.object_name)"
            ),
            "x.object_name, DBMS_METADATA.GET_DDL('TABLE', x.object_name, x.owner)",
            &matches,
        );
        let binds: Vec<&str> = std::iter::once(owner).chain(needle.as_deref()).collect();
        let mut found: Vec<(String, String)> = Vec::new();
        for r in rows(c, &sql, &binds)? {
            if let (Some(name), Some(ddl)) = (text(&r, 0)?, text(&r, 1)?) {
                found.push((name, ddl));
            }
        }
        // Tables whose text only shows up in a standalone index's DDL.
        let (matches, needle) = narrow("ddl", ":2", q);
        let sql = format!(
            "SELECT DISTINCT table_name FROM ({})",
            described(INDEXES, "x.table_name, DBMS_METADATA.GET_DDL('INDEX', x.index_name, x.owner)", &matches)
        );
        let binds: Vec<&str> = std::iter::once(owner).chain(needle.as_deref()).collect();
        for r in rows(c, &sql, &binds)? {
            let Some(name) = text(&r, 0)? else { continue };
            if found.iter().any(|(n, _)| *n == name) || !tables.contains(&name) {
                continue;
            }
            let ddl = c
                .query_row("SELECT DBMS_METADATA.GET_DDL('TABLE', :1, :2) FROM dual", &[&name.as_str(), &owner])
                .and_then(|r| r.get::<Option<String>>(0))
                .map_err(err)?;
            if let Some(ddl) = ddl {
                found.push((name, ddl));
            }
        }
        for (name, ddl) in found {
            let full = with_indexes(c, ddl.trim(), owner, &name);
            out.insert((kinds::TABLE.into(), owner.into(), name), full);
        }
    }
    if want(kinds::TYPE) {
        let mut lines: HashMap<String, Vec<(i64, String)>> = HashMap::new();
        for r in rows(c, TYPES, &[owner])? {
            let Some(name) = text(&r, 0)? else { continue };
            lines.entry(name).or_default().push((r.get::<i64>(1).map_err(err)?, text(&r, 2)?.unwrap_or_default()));
        }
        for (name, lines) in lines {
            if let Some(src) = plsql_source(&lines) {
                out.insert((kinds::TYPE.into(), owner.into(), name), src);
            }
        }
    }
    if want(kinds::SEQUENCE) {
        let mut extra: HashMap<String, Row> = HashMap::new();
        match rows(c, SEQUENCES_EXTRA, &[owner]) {
            Ok(rs) => {
                for r in rs {
                    if let Some(name) = text(&r, 0)? {
                        extra.insert(name, r);
                    }
                }
            }
            Err(e) => tracing::debug!("oracle: sequence flags: {e}"),
        }
        for r in rows(c, SEQUENCES, &[owner])? {
            let Some(name) = text(&r, 0)? else { continue };
            let mut s = SequenceInfo {
                min: text(&r, 1)?.unwrap_or_default(),
                max: text(&r, 2)?.unwrap_or_default(),
                increment: text(&r, 3)?.unwrap_or_default(),
                cycle: yes(text(&r, 4)?),
                order: yes(text(&r, 5)?),
                cache: r.get::<Option<i64>>(6).map_err(err)?.unwrap_or(0),
                start: text(&r, 7)?.unwrap_or_default(),
                ..Default::default()
            };
            if let Some(x) = extra.get(&name) {
                s.scale = yes(text(x, 1).unwrap_or(None));
                s.extend = yes(text(x, 2).unwrap_or(None));
                s.session = yes(text(x, 3).unwrap_or(None));
                s.keep = yes(text(x, 4).unwrap_or(None));
            }
            let src = sequence_sql(&name, &s);
            out.insert((kinds::SEQUENCE.into(), owner.into(), name), src);
        }
    }
    if want(kinds::SYNONYM) {
        for r in rows(c, SYNONYMS, &[owner, owner])? {
            let (Some(syn_owner), Some(name)) = (text(&r, 0)?, text(&r, 1)?) else { continue };
            let target = text(&r, 3)?.unwrap_or_default();
            let src = crate::structure::synonym_sql(syn_owner == "PUBLIC", &name, schema, text(&r, 2)?.as_deref(), &target, text(&r, 4)?.as_deref());
            out.insert((kinds::SYNONYM.into(), syn_owner, name), src);
        }
    }
    Ok(out)
}

impl OracleSession {
    pub(crate) async fn search_code_impl(&mut self, q: &CodeSearch) -> Result<Option<CodeSearchReport>> {
        if q.text.is_empty() {
            return Ok(None);
        }
        let wanted: Vec<String> = crate::info()
            .object_kinds
            .iter()
            .filter(|k| k.has_definition && (q.kinds.is_empty() || q.kinds.iter().any(|w| w == k.id)))
            .map(|k| k.id.to_string())
            .collect();
        let objects: Vec<_> = Session::list_objects(self).await?.into_iter().filter(|o| wanted.contains(&o.kind)).collect();
        let present: Vec<String> = wanted.into_iter().filter(|k| objects.iter().any(|o| &o.kind == k)).collect();
        let tables: Vec<String> = objects.iter().filter(|o| o.kind == kinds::TABLE).map(|o| o.name.clone()).collect();
        let schema = self.schema.clone();
        let prepare = !self.metadata_ready;
        self.metadata_ready = true;
        let query = q.clone();
        let read = self
            .run(move |c| {
                if prepare {
                    if let Err(e) = c.execute(SET_METADATA_TRANSFORMS, &[]) {
                        tracing::debug!("oracle: DBMS_METADATA transforms: {e}");
                    }
                }
                sources(c, &schema, &schema, &present, &tables, &query)
            })
            .await;
        let sources = match read {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("oracle: bulk sources unavailable, the app scans: {e}");
                return Ok(None);
            }
        };
        let mut report = CodeSearchReport { scanned: objects.len(), ..Default::default() };
        for o in &objects {
            let owner = o.schema.clone().unwrap_or_else(|| self.schema.clone());
            let Some(source) = sources.get(&(o.kind.clone(), owner, o.name.clone())) else { continue };
            report.hits.extend(hits_in(&o.kind, o.schema.as_deref(), &o.name, o.parent.as_deref(), source, q));
            if q.max_hits > 0 && report.hits.len() >= q.max_hits {
                report.hits.truncate(q.max_hits);
                report.truncated = true;
                break;
            }
        }
        Ok(Some(report))
    }
}
