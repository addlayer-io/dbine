//! "Buscar en la base" from the catalog ([`dbine_driver::Session::search_code`]),
//! per preset: each per-object definition query (the preset's `Def::Sql`
//! for views and routines, `structure::definition_sql` for sequences,
//! synonyms and types) run once for every object, with its key columns
//! added and its key conditions dropped ([`bulk`]). The rows are grouped
//! by key and built into the text `definition` returns, with the same
//! joins and builders, for the objects `list_objects` gives; then matched
//! line by line with the contract's rule, so the hits equal those of the
//! app's per-object scan.
//!
//! A kind whose source isn't such a query (`SHOW …` on Hive, Impala,
//! Spark and Teradata; `GET_DDL(?)`; Netezza's `SELECT *`; the catalog as
//! a parameter) makes the search answer `None` and the app scans; so does
//! the generic preset, which guesses INFORMATION_SCHEMA, and a source the
//! catalog can't order (several rows of one object without `ORDER BY`).
//! The sources come back whole (they may be split in rows, as
//! `syscomments`), not narrowed on the server.

use crate::presets::{Def, P};
use crate::{col, design, info, structure, OdbcSession, Rows};
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::{kinds, DbObject, Result, Session};
use std::collections::HashMap;

/// A per-object query made to read every object at once: the original
/// columns, then one column per key.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bulk {
    pub sql: String,
    /// Key columns at the end of each row.
    pub keys: usize,
    /// It keeps the original `ORDER BY`: several rows of one object come
    /// in a fixed order.
    pub ordered: bool,
}

/// Byte offset of `kw` (case-insensitive, between whitespace) outside
/// parentheses and quotes, from `from`.
fn find_kw(s: &str, kw: &str, from: usize) -> Option<usize> {
    let b = s.as_bytes();
    let (mut depth, mut quote) = (0i32, None::<u8>);
    let mut i = from;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                b'\'' | b'"' => quote = Some(c),
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ if depth == 0
                    && b[i..].len() >= kw.len()
                    && b[i..i + kw.len()].eq_ignore_ascii_case(kw.as_bytes())
                    && (i == 0 || b[i - 1].is_ascii_whitespace())
                    && b.get(i + kw.len()).is_none_or(u8::is_ascii_whitespace) =>
                {
                    return Some(i)
                }
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// `sql` split at each top-level `kw`.
fn split_kw<'a>(sql: &'a str, kw: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(i) = find_kw(sql, kw, start) {
        out.push(&sql[start..i]);
        start = i + kw.len();
    }
    out.push(&sql[start..]);
    out
}

/// `SELECT cols FROM from WHERE a = ? AND b = ? [AND …] [ORDER BY …]` (the
/// `?` conditions anywhere among the others, one per key, in order) as
/// `SELECT cols, a, b FROM from [WHERE …] [ORDER BY …]`; `None` for any
/// other shape.
pub(crate) fn bulk(sql: &str, keys: usize) -> Option<Bulk> {
    let sql = sql.trim();
    if !sql.get(..6)?.eq_ignore_ascii_case("SELECT") || !sql.as_bytes().get(6)?.is_ascii_whitespace() {
        return None;
    }
    for kw in ["UNION", "GROUP BY", "HAVING", "LIMIT", "FETCH"] {
        if find_kw(sql, kw, 0).is_some() {
            return None;
        }
    }
    let from_at = find_kw(sql, "FROM", 0)?;
    let cols = sql[6..from_at].trim();
    let upper = cols.to_ascii_uppercase();
    if cols.is_empty() || cols == "*" || upper.starts_with("DISTINCT ") || upper.starts_with("TOP ") || upper.starts_with("FIRST ") {
        return None;
    }
    let where_at = find_kw(sql, "WHERE", from_at)?;
    let from = sql[from_at + 4..where_at].trim();
    let rest = &sql[where_at + 5..];
    let (cond, order) = match find_kw(rest, "ORDER BY", 0) {
        Some(i) => (&rest[..i], Some(rest[i + 8..].trim())),
        None => (rest, None),
    };
    if cols.contains('?') || from.contains('?') || order.is_some_and(|o| o.contains('?')) {
        return None;
    }
    if find_kw(cond, "OR", 0).is_some() || find_kw(cond, "BETWEEN", 0).is_some() {
        return None;
    }
    let (mut key_exprs, mut others) = (Vec::new(), Vec::new());
    for c in split_kw(cond, "AND").into_iter().map(str::trim) {
        match c.matches('?').count() {
            0 => others.push(c),
            1 => {
                let lhs = c.strip_suffix('?')?.trim_end().strip_suffix('=')?;
                if lhs.ends_with(['<', '>', '!']) {
                    return None;
                }
                key_exprs.push(lhs.trim());
            }
            _ => return None,
        }
    }
    if key_exprs.len() != keys || key_exprs.iter().any(|k| k.is_empty()) {
        return None;
    }
    let mut out = format!("SELECT {cols}, {} FROM {from}", key_exprs.join(", "));
    if !others.is_empty() {
        out.push_str(&format!(" WHERE {}", others.join(" AND ")));
    }
    if let Some(o) = order {
        out.push_str(&format!(" ORDER BY {o}"));
    }
    Some(Bulk { sql: out, keys, ordered: order.is_some() })
}

/// Where a kind's source comes from, as `definition` reads it.
enum Plan {
    /// No source: `definition` gives `None`.
    Nothing,
    /// `structure::build` from these queries' rows.
    Built(Vec<Bulk>),
    /// The rows' first column joined with this.
    Joined(Bulk, Vec<P>, &'static str),
}

fn plan_for(s: &OdbcSession, kind: &str) -> Option<Plan> {
    let e = design::eng(s.preset);
    let own = structure::definition_sql(e, kind);
    if !own.is_empty() {
        let keys = if structure::definition_takes_schema(e) { 2 } else { 1 };
        return own.iter().map(|sql| bulk(sql, keys)).collect::<Option<Vec<_>>>().map(Plan::Built);
    }
    if structure::named_definition_sql(e, kind).is_some() {
        return None;
    }
    let defs = &s.preset.defs;
    let def = match kind {
        kinds::TABLE => defs.table,
        kinds::VIEW => defs.view,
        kinds::PROCEDURE => defs.procedure,
        kinds::FUNCTION => defs.function,
        _ => Def::None,
    };
    match def {
        Def::None => Some(Plan::Nothing),
        Def::Show(_) => None,
        Def::Sql(sql, params, join) => {
            if params.iter().any(|p| !matches!(p, P::Schema | P::Name)) {
                return None;
            }
            bulk(sql, params.len()).map(|b| Plan::Joined(b, params.to_vec(), join))
        }
    }
}

/// An object's key in a [`Bulk`]'s rows (trailing blanks don't count, as
/// in the server's `=`).
fn key_of(o: &DbObject, params: &[P]) -> Vec<String> {
    params
        .iter()
        .map(|p| match p {
            P::Schema => o.schema.as_deref().unwrap_or("").trim_end().to_string(),
            _ => o.name.trim_end().to_string(),
        })
        .collect()
}

/// A bulk query's rows by key, each without its key columns.
struct Grouped(HashMap<Vec<String>, Rows>);

impl Grouped {
    fn new(rows: Rows, keys: usize) -> Self {
        let mut map: HashMap<Vec<String>, Rows> = HashMap::new();
        for mut r in rows {
            if r.len() < keys {
                continue;
            }
            let key = r.split_off(r.len() - keys).into_iter().map(|k| k.unwrap_or_default().trim_end().to_string()).collect();
            map.entry(key).or_default().push(r);
        }
        Grouped(map)
    }

    /// The rows of `key`; `None` when only a key that differs in case has
    /// rows (the server's `=` may or may not take it).
    fn rows(&self, key: &[String]) -> Option<&[Vec<Option<String>>]> {
        if let Some(r) = self.0.get(key) {
            return Some(r);
        }
        let folded = |k: &[String]| k.iter().map(|s| s.to_lowercase()).collect::<Vec<_>>();
        let want = folded(key);
        if self.0.keys().any(|k| folded(k) == want) {
            return None;
        }
        Some(&[])
    }
}

impl OdbcSession {
    pub(crate) async fn search_code_impl(&mut self, q: &CodeSearch) -> Result<Option<CodeSearchReport>> {
        if self.preset.is_generic() || q.text.is_empty() {
            return Ok(None);
        }
        let kinds: Vec<String> = if q.kinds.is_empty() {
            info(self.preset).object_kinds.iter().filter(|k| k.has_definition).map(|k| k.id.to_string()).collect()
        } else {
            q.kinds.clone()
        };
        let mut plans = HashMap::new();
        for k in &kinds {
            let Some(p) = plan_for(self, k) else { return Ok(None) };
            plans.insert(k.clone(), p);
        }
        let objects: Vec<DbObject> = self.list_objects().await?.into_iter().filter(|o| plans.contains_key(&o.kind)).collect();

        // Each query once (Sybase reads views and routines with the same one).
        let mut results: HashMap<String, Grouped> = HashMap::new();
        for o in &objects {
            let bulks: Vec<&Bulk> = match &plans[&o.kind] {
                Plan::Nothing => continue,
                Plan::Built(b) => b.iter().collect(),
                Plan::Joined(b, ..) => vec![b],
            };
            for b in bulks {
                if !results.contains_key(&b.sql) {
                    // `definition` would fail on its own query too, or the
                    // scan reads around it: let the scan decide.
                    let Ok(rows) = self.query(b.sql.clone(), Vec::new()).await else { return Ok(None) };
                    results.insert(b.sql.clone(), Grouped::new(rows, b.keys));
                }
            }
        }

        let e = design::eng(self.preset);
        let mut report = CodeSearchReport::default();
        for o in &objects {
            let source = match &plans[&o.kind] {
                Plan::Nothing => None,
                Plan::Built(bulks) => {
                    let params: &[P] = if structure::definition_takes_schema(e) { &[P::Schema, P::Name] } else { &[P::Name] };
                    let key = key_of(o, params);
                    let mut sets = Vec::new();
                    for b in bulks {
                        let Some(rows) = results[&b.sql].rows(&key) else { return Ok(None) };
                        if !b.ordered && rows.len() > 1 {
                            return Ok(None);
                        }
                        sets.push(rows.to_vec());
                    }
                    structure::build(e, &o.kind, self.quote, o.schema.as_deref(), &o.name, &sets)
                }
                Plan::Joined(b, params, join) => {
                    let Some(rows) = results[&b.sql].rows(&key_of(o, params)) else { return Ok(None) };
                    let parts: Vec<String> = rows.iter().filter_map(|r| col(r, 0)).collect();
                    if !b.ordered && parts.len() > 1 {
                        return Ok(None);
                    }
                    let text = parts.join(join);
                    (!text.trim().is_empty()).then_some(text)
                }
            };
            report.scanned += 1;
            let Some(source) = source else { continue };
            report.hits.extend(hits_in(&o.kind, o.schema.as_deref(), &o.name, o.parent.as_deref(), &source, q));
            if q.max_hits > 0 && report.hits.len() >= q.max_hits {
                report.hits.truncate(q.max_hits);
                report.truncated = true;
                break;
            }
        }
        Ok(Some(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PRESETS;

    #[test]
    fn bulk_keeps_the_query_and_moves_the_keys() {
        let b = bulk("SELECT TEXT FROM SYSCAT.VIEWS WHERE VIEWSCHEMA = ? AND VIEWNAME = ?", 2).unwrap();
        assert_eq!(b, Bulk { sql: "SELECT TEXT, VIEWSCHEMA, VIEWNAME FROM SYSCAT.VIEWS".into(), keys: 2, ordered: false });
        let b = bulk(
            "SELECT c.text FROM syscomments c JOIN sysobjects o ON o.id = c.id WHERE user_name(o.uid) = ? AND o.name = ? ORDER BY c.number, c.colid2, c.colid",
            2,
        )
        .unwrap();
        assert_eq!(
            b.sql,
            "SELECT c.text, user_name(o.uid), o.name FROM syscomments c JOIN sysobjects o ON o.id = c.id ORDER BY c.number, c.colid2, c.colid"
        );
        assert!(b.ordered);
        // Other conditions stay, wherever the keys are; ORDER BY 1 still
        // means the first original column.
        let b = bulk(
            "SELECT TRIM(SUPER_SCHEMA), SUPER_NAME FROM SYSCAT.HIERARCHIES WHERE METATYPE = 'U' AND TRIM(SUB_SCHEMA) = ? AND SUB_NAME = ? ORDER BY 1",
            2,
        )
        .unwrap();
        assert_eq!(
            b.sql,
            "SELECT TRIM(SUPER_SCHEMA), SUPER_NAME, TRIM(SUB_SCHEMA), SUB_NAME FROM SYSCAT.HIERARCHIES WHERE METATYPE = 'U' ORDER BY 1"
        );
        // An AND inside the FROM's JOIN … ON isn't a condition.
        let b = bulk(
            "SELECT d.TYPENAME FROM SYSCAT.SEQUENCES s JOIN SYSCAT.DATATYPES d ON d.TYPEID = s.DATATYPEID AND d.TYPESCHEMA = 'SYSIBM'
             WHERE TRIM(s.SEQSCHEMA) = ? AND s.SEQNAME = ?",
            2,
        )
        .unwrap();
        assert!(b.sql.ends_with("AND d.TYPESCHEMA = 'SYSIBM'"), "{}", b.sql);
        assert_eq!(bulk("SELECT vclass_def FROM db_vclass WHERE vclass_name = ?", 1).unwrap().sql, "SELECT vclass_def, vclass_name FROM db_vclass");
    }

    #[test]
    fn bulk_refuses_other_shapes() {
        assert_eq!(bulk("SELECT GET_DDL(?)", 1), None);
        assert_eq!(bulk("SELECT * FROM _V_SEQUENCE WHERE SCHEMA = ? AND SEQNAME = ?", 2), None);
        assert_eq!(bulk("SELECT a FROM t WHERE s = ? OR n = ?", 2), None);
        assert_eq!(bulk("SELECT a FROM t WHERE s >= ? AND n = ?", 2), None);
        assert_eq!(bulk("SELECT a FROM t WHERE s = ?", 2), None, "a key missing");
        assert_eq!(bulk("SELECT DISTINCT a FROM t WHERE s = ? AND n = ?", 2), None);
        assert_eq!(bulk("SELECT a FROM t WHERE s = ? AND n = ? UNION SELECT b FROM u", 2), None);
        // Keywords inside quotes or parentheses aren't the query's.
        let b = bulk("SELECT 'x FROM y', (SELECT 1 FROM z WHERE k = 1) FROM t WHERE s = ? AND n = ?", 2).unwrap();
        assert_eq!(b.sql, "SELECT 'x FROM y', (SELECT 1 FROM z WHERE k = 1), s, n FROM t");
    }

    /// Every preset's view / routine / structure query either becomes a
    /// bulk one or is known not to (and then the search falls to the scan).
    #[test]
    fn every_preset_query_has_a_bulk_form_or_none() {
        let mut bulked = 0;
        for p in PRESETS {
            let e = design::eng(p);
            for def in [p.defs.table, p.defs.view, p.defs.procedure, p.defs.function] {
                if let Def::Sql(sql, params, _) = def {
                    if params.iter().all(|x| matches!(x, P::Schema | P::Name)) && bulk(sql, params.len()).is_some() {
                        bulked += 1;
                    } else {
                        assert!(sql.contains("GET_") || params.iter().any(|x| matches!(x, P::Catalog | P::Qualified)), "{}: {sql}", p.id);
                    }
                }
            }
            for kind in [kinds::SEQUENCE, kinds::SYNONYM, kinds::TYPE] {
                let keys = if structure::definition_takes_schema(e) { 2 } else { 1 };
                for sql in structure::definition_sql(e, kind) {
                    assert!(bulk(sql, keys).is_some(), "{} {kind}: {sql}", p.id);
                    bulked += 1;
                }
            }
        }
        assert!(bulked > 40, "{bulked}");
    }

    #[test]
    fn grouping_by_key() {
        let row = |t: &str, s: &str, n: &str| vec![Some(t.to_string()), Some(s.to_string()), Some(n.to_string())];
        let g = Grouped::new(vec![row("a", "DBO  ", "V"), row("b", "DBO", "V"), row("c", "dbo", "W")], 2);
        let k = |s: &str, n: &str| vec![s.to_string(), n.to_string()];
        assert_eq!(g.rows(&k("DBO", "V")).unwrap().len(), 2, "trailing blanks don't count");
        assert_eq!(g.rows(&k("DBO", "X")).unwrap().len(), 0, "no rows: no source");
        assert!(g.rows(&k("DBO", "W")).is_none(), "only in another case: the server decides");
    }
}
