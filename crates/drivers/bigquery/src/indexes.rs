//! BigQuery's only indexes: search indexes (`CREATE SEARCH INDEX`, the
//! full-text kind) and vector indexes, read from INFORMATION_SCHEMA's
//! `SEARCH_INDEXES` / `VECTOR_INDEXES` (their `ddl` column) and written
//! back as GoogleSQL.
//!
//! The index's columns are the entries of `ON t (…)` as BigQuery writes them
//! (`ALL COLUMNS`, `a`, `b OPTIONS(…)`); `STORING (…)` goes into
//! [`IndexDef::include`], and `PARTITION BY` plus each `OPTIONS (…)` entry
//! into [`IndexDef::options`] under their own names.

use crate::ddl::{ident, table_name};
use dbine_driver::{IndexDef, SyncScript, TableChange, TableSchema};

pub const SEARCH: &str = "SEARCH";
pub const VECTOR: &str = "VECTOR";
pub const PARTITION_BY: &str = "partition_by";

pub fn supported(ix: &IndexDef) -> bool {
    kind(ix).is_some()
}

fn kind(ix: &IndexDef) -> Option<&'static str> {
    match ix.kind.as_deref().map(str::trim) {
        Some(k) if k.eq_ignore_ascii_case(SEARCH) => Some("SEARCH INDEX"),
        Some(k) if k.eq_ignore_ascii_case(VECTOR) => Some("VECTOR INDEX"),
        _ => None,
    }
}

/// The text inside the parentheses that open at byte `open`.
fn balanced(s: &str, open: usize) -> Option<(&str, usize)> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (i, ch) in s[open..].char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return Some((&s[open + 1..open + i], open + i + 1));
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits at commas outside parentheses, brackets and quotes.
fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth, mut quote) = (Vec::new(), String::new(), 0i32, None::<char>);
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(ch),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, ',') if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Where `word` starts in `s` outside quotes and parentheses, from `from`.
fn find_word(s: &str, word: &str, from: usize) -> Option<usize> {
    let up = s.to_ascii_uppercase();
    let (mut depth, mut quote) = (0i32, None::<char>);
    for (i, ch) in s.char_indices().skip_while(|(i, _)| *i < from) {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(ch),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, _) if depth == 0 && up[i..].starts_with(word) => {
                let before = s[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric() && c != '_');
                let after = s[i + word.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '_');
                if before && after {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `` `a` `` → `a`; anything else stays as written.
fn entry(e: &str) -> String {
    let e = e.trim();
    match e.strip_prefix('`').and_then(|x| x.strip_suffix('`')) {
        Some(inner) if !inner.contains('`') => inner.to_string(),
        _ => e.to_string(),
    }
}

/// An index from its `ddl` (INFORMATION_SCHEMA), `None` if it can't be read.
pub fn parse(name: &str, kind: &str, ddl: &str) -> Option<IndexDef> {
    let on = find_word(ddl, "ON", 0)?;
    let open = on + ddl[on..].find('(')?;
    let (cols, mut rest) = balanced(ddl, open)?;
    let mut ix = IndexDef { name: name.into(), kind: Some(kind.into()), columns: split_top(cols).iter().map(|c| entry(c)).collect(), ..Default::default() };
    if let Some(at) = find_word(ddl, "STORING", rest) {
        let open = at + ddl[at..].find('(')?;
        let (inc, end) = balanced(ddl, open)?;
        ix.include = split_top(inc).iter().map(|c| entry(c)).collect();
        rest = end;
    }
    let opts = find_word(ddl, "OPTIONS", rest);
    if let Some(at) = find_word(ddl, "PARTITION BY", rest) {
        let end = opts.filter(|o| *o > at).unwrap_or(ddl.len());
        let p = ddl[at + "PARTITION BY".len()..end].trim().trim_end_matches(';').trim();
        if !p.is_empty() {
            ix.options.insert(PARTITION_BY.into(), p.to_string());
        }
    }
    if let Some(at) = opts {
        let open = at + ddl[at..].find('(')?;
        let (inner, _) = balanced(ddl, open)?;
        for kv in split_top(inner) {
            if let Some((k, v)) = kv.split_once('=') {
                ix.options.insert(k.trim().to_lowercase(), v.trim().to_string());
            }
        }
    }
    Some(ix)
}

/// Reads the rows of `SEARCH_INDEXES` / `VECTOR_INDEXES` (`table_name`,
/// `index_name`, `ddl`) into the tables.
pub fn attach(tables: &mut [TableSchema], kind: &str, rows: &[crate::ddl::Row]) {
    for r in rows {
        let (Some(t), Some(n), Some(d)) = (r.get("table_name"), r.get("index_name"), r.get("ddl")) else { continue };
        let Some(ix) = parse(n, kind, d) else { continue };
        if let Some(t) = tables.iter_mut().find(|x| &x.name == t) {
            t.indexes.push(ix);
        }
    }
}

fn column_entry(c: &str) -> String {
    if !c.is_empty() && c.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
        ident(c)
    } else {
        c.to_string()
    }
}

/// One index's CREATE statement.
pub fn create(t: &TableSchema, ix: &IndexDef, if_not_exists: bool) -> Option<String> {
    let word = kind(ix)?;
    let mut s = format!(
        "CREATE {word} {}{} ON {} ({})",
        if if_not_exists { "IF NOT EXISTS " } else { "" },
        ident(&ix.name),
        table_name(t),
        ix.columns.iter().map(|c| column_entry(c)).collect::<Vec<_>>().join(", ")
    );
    if !ix.include.is_empty() {
        s.push_str(&format!(" STORING ({})", ix.include.iter().map(|c| column_entry(c)).collect::<Vec<_>>().join(", ")));
    }
    if let Some(p) = ix.options.get(PARTITION_BY).filter(|p| !p.trim().is_empty()) {
        s.push_str(&format!(" PARTITION BY {}", p.trim()));
    }
    let opts: Vec<String> = ix.options.iter().filter(|(k, _)| k.as_str() != PARTITION_BY).map(|(k, v)| format!("{k} = {v}")).collect();
    if !opts.is_empty() {
        s.push_str(&format!(" OPTIONS ({})", opts.join(", ")));
    }
    s.push(';');
    Some(s)
}

/// One index's DROP statement (it's dropped with its table too).
pub fn drop(t: &TableSchema, ix: &IndexDef) -> Option<String> {
    Some(format!("DROP {} IF EXISTS {} ON {};", kind(ix)?, ident(&ix.name), table_name(t)))
}

/// The index statements of a table sync: a changed index is dropped and
/// made again. They go after the columns change.
pub fn sync(old: &TableSchema, new: &TableSchema) -> (Vec<String>, Vec<String>) {
    let same = |a: &IndexDef, b: &IndexDef| {
        let low = |v: &[String]| v.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>();
        let mut ia = low(&a.include);
        let mut ib = low(&b.include);
        ia.sort();
        ib.sort();
        low(&a.columns) == low(&b.columns) && a.kind.as_deref().map(str::to_uppercase) == b.kind.as_deref().map(str::to_uppercase) && ia == ib && a.options == b.options
    };
    let (mut drops, mut creates) = (Vec::new(), Vec::new());
    for o in old.indexes.iter().filter(|i| supported(i)) {
        if !new.indexes.iter().any(|n| n.name.eq_ignore_ascii_case(&o.name) && same(o, n)) {
            drops.extend(drop(new, o));
        }
    }
    for n in new.indexes.iter().filter(|i| supported(i)) {
        if !old.indexes.iter().any(|o| o.name.eq_ignore_ascii_case(&n.name) && same(o, n)) {
            creates.extend(create(new, n, false));
        }
    }
    (drops, creates)
}

/// The indexes of `Alter` changes, planned apart from the generic planner
/// (which has no DROP … INDEX … ON): drops first, creates last.
pub fn plan(changes: &[TableChange], script: &mut SyncScript) {
    let (mut drops, mut creates) = (Vec::new(), Vec::new());
    for ch in changes {
        if let TableChange::Alter { old, new } = ch {
            let (d, c) = sync(old, new);
            drops.extend(d);
            creates.extend(c);
        }
    }
    drops.append(&mut script.statements);
    drops.append(&mut creates);
    script.statements = drops;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn opts(v: &[(&str, &str)]) -> BTreeMap<String, String> {
        v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn search_and_vector_index_ddl_is_read() {
        let ix = parse("ix", SEARCH, "CREATE SEARCH INDEX ix ON `p.ds.t`(ALL COLUMNS) OPTIONS(analyzer='LOG_ANALYZER', data_types=['STRING', 'INT64'])").unwrap();
        assert_eq!(ix.columns, vec!["ALL COLUMNS"]);
        assert_eq!(ix.options, opts(&[("analyzer", "'LOG_ANALYZER'"), ("data_types", "['STRING', 'INT64']")]));
        let ix = parse("ix", SEARCH, "CREATE SEARCH INDEX ix ON ds.t(a, `b` OPTIONS(index_granularity='COLUMN'))").unwrap();
        assert_eq!(ix.columns, vec!["a", "`b` OPTIONS(index_granularity='COLUMN')"]);
        assert!(ix.options.is_empty());
        let ix = parse("vx", VECTOR, "CREATE VECTOR INDEX vx ON ds.t(emb) STORING(a, b) PARTITION BY DATE(ts) OPTIONS(index_type = 'IVF', distance_type = 'COSINE')").unwrap();
        assert_eq!(ix.columns, vec!["emb"]);
        assert_eq!(ix.include, vec!["a", "b"]);
        assert_eq!(ix.options, opts(&[("distance_type", "'COSINE'"), ("index_type", "'IVF'"), (PARTITION_BY, "DATE(ts)")]));
    }

    #[test]
    fn index_statements() {
        let t = TableSchema { schema: Some("ds".into()), name: "t".into(), ..Default::default() };
        let ix = IndexDef { name: "vx".into(), kind: Some(VECTOR.into()), columns: vec!["emb".into()], include: vec!["a".into()], options: opts(&[("index_type", "'IVF'"), (PARTITION_BY, "DATE(ts)")]), ..Default::default() };
        assert_eq!(create(&t, &ix, false).unwrap(), "CREATE VECTOR INDEX `vx` ON `ds`.`t` (`emb`) STORING (`a`) PARTITION BY DATE(ts) OPTIONS (index_type = 'IVF');");
        assert_eq!(drop(&t, &ix).unwrap(), "DROP VECTOR INDEX IF EXISTS `vx` ON `ds`.`t`;");
        let s = IndexDef { name: "sx".into(), kind: Some(SEARCH.into()), columns: vec!["ALL COLUMNS".into()], ..Default::default() };
        assert_eq!(create(&t, &s, true).unwrap(), "CREATE SEARCH INDEX IF NOT EXISTS `sx` ON `ds`.`t` (ALL COLUMNS);");
        assert!(create(&t, &IndexDef { name: "b".into(), ..Default::default() }, false).is_none());
    }

    #[test]
    fn changed_indexes_are_remade() {
        let s = IndexDef { name: "sx".into(), kind: Some(SEARCH.into()), columns: vec!["ALL COLUMNS".into()], ..Default::default() };
        let old = TableSchema { name: "t".into(), indexes: vec![s.clone()], ..Default::default() };
        let mut new = old.clone();
        new.indexes[0].options.insert("analyzer".into(), "'NO_OP_ANALYZER'".into());
        let (d, c) = sync(&old, &new);
        assert_eq!(d, vec!["DROP SEARCH INDEX IF EXISTS `sx` ON `t`;"]);
        assert_eq!(c, vec!["CREATE SEARCH INDEX `sx` ON `t` (ALL COLUMNS) OPTIONS (analyzer = 'NO_OP_ANALYZER');"]);
        assert_eq!(sync(&old, &old), (vec![], vec![]));
    }
}
