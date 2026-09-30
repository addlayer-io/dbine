//! What "Comparar esquemas" needs beyond columns and keys: CHECK
//! constraints, every index kind (secondary with `STORING`, descending keys,
//! `WHERE … IS NOT NULL` and `INTERLEAVE IN`; search and vector indexes with
//! their `OPTIONS`) and sequences, read from INFORMATION_SCHEMA plus the
//! database DDL (the index `OPTIONS` are only there) and written back as
//! GoogleSQL.
//!
//! Index settings go into [`IndexDef::options`]: `desc` (the descending key
//! columns), `interleave_in`, `partition_by` / `order_by` (search indexes)
//! and each `OPTIONS (…)` entry under its own name.

use crate::{bq, bq_qualified};
use dbine_driver::alter::check_expr;
use dbine_driver::{CheckDef, IndexDef, SyncScript, TableChange};
#[cfg(test)]
use dbine_driver::TableSchema;
use std::collections::BTreeMap;

pub const OPT_DESC: &str = "desc";
pub const OPT_INTERLEAVE: &str = "interleave_in";
pub const OPT_PARTITION_BY: &str = "partition_by";
pub const OPT_ORDER_BY: &str = "order_by";
/// Index options that aren't `OPTIONS (…)` entries.
const STRUCTURAL: &[&str] = &[OPT_DESC, OPT_INTERLEAVE, OPT_PARTITION_BY, OPT_ORDER_BY];

pub const SEARCH: &str = "SEARCH";
pub const VECTOR: &str = "VECTOR";
pub const NULL_FILTERED: &str = "NULL_FILTERED";

fn is(kind: &Option<String>, k: &str) -> bool {
    kind.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(k))
}

/// `CREATE [SEARCH |VECTOR ]INDEX` / `DROP … INDEX`: the keyword for the kind.
fn index_word(ix: &IndexDef) -> &'static str {
    if is(&ix.kind, SEARCH) {
        "SEARCH INDEX"
    } else if is(&ix.kind, VECTOR) {
        "VECTOR INDEX"
    } else {
        "INDEX"
    }
}

/// Spanner names an unnamed CHECK `CK_<table>_<16 hex>_<n>`; the name
/// doesn't carry over to another database.
pub fn generated_check_name(table: &str, name: &str) -> bool {
    let Some(rest) = name.strip_prefix("CK_").and_then(|r| r.strip_prefix(table)).and_then(|r| r.strip_prefix('_')) else {
        return false;
    };
    let mut parts = rest.split('_');
    matches!((parts.next(), parts.next(), parts.next()), (Some(h), Some(n), None)
        if h.len() == 16 && h.chars().all(|c| c.is_ascii_hexdigit()) && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// NOT NULL columns show up in CHECK_CONSTRAINTS as `CK_IS_NOT_NULL_…`.
pub fn not_null_check(name: &str) -> bool {
    name.starts_with("CK_IS_NOT_NULL_")
}

/// The name a DDL statement creates, for `CREATE <what> [IF NOT EXISTS] name`
/// (`what` like `TABLE`, `SEQUENCE`, `SEARCH INDEX`), unquoted and
/// schema-qualified as written.
pub fn created_name(ddl: &str, what: &str) -> Option<String> {
    let rest = ddl.trim_start().strip_prefix("CREATE ")?;
    let rest = rest.strip_prefix(what)?.strip_prefix(' ')?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("IF NOT EXISTS ").unwrap_or(rest);
    let name: String = rest.chars().take_while(|c| !c.is_whitespace() && *c != '(').collect();
    (!name.is_empty()).then(|| name.replace('`', ""))
}

/// The text inside the parentheses that open at byte `open` (which must be `(`).
fn balanced(s: &str, open: usize) -> Option<&str> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (i, ch) in s[open..].char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits at commas outside parentheses and quotes.
fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth, mut quote) = (Vec::new(), String::new(), 0i32, None::<char>);
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
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

/// A search or vector index's `OPTIONS (k = v, …)` from its DDL. Values
/// keep their GoogleSQL spelling (`'COSINE'`, `true`).
pub fn ddl_options(stmt: &str) -> BTreeMap<String, String> {
    let upper = stmt.to_ascii_uppercase();
    let Some(at) = upper.rfind("OPTIONS") else { return BTreeMap::new() };
    let Some(open) = stmt[at..].find('(').map(|o| at + o) else { return BTreeMap::new() };
    let Some(inner) = balanced(stmt, open) else { return BTreeMap::new() };
    split_top(inner)
        .into_iter()
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_lowercase(), v.trim().replace('"', "'")))
        })
        .collect()
}

/// Every CREATE statement of the database DDL for search and vector
/// indexes, by (schema-qualified) index name, with its `OPTIONS`.
pub fn index_options_from_ddl<'a>(statements: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for s in statements {
        for what in ["SEARCH INDEX", "VECTOR INDEX"] {
            if let Some(n) = created_name(s, what) {
                let o = ddl_options(s);
                if !o.is_empty() {
                    out.insert(n, o);
                }
            }
        }
    }
    out
}

/// One index's CREATE statement.
pub fn index_ddl(schema: Option<&str>, table: &str, ix: &IndexDef, if_not_exists: bool) -> String {
    let desc: Vec<String> = split_list(ix.options.get(OPT_DESC)).into_iter().map(|c| c.to_lowercase()).collect();
    let keys: Vec<String> = ix
        .columns
        .iter()
        .map(|c| if desc.contains(&c.to_lowercase()) { format!("{} DESC", bq(c)) } else { bq(c) })
        .collect();
    let mut s = format!(
        "CREATE {}{}{} {}{} ON {} ({})",
        if ix.unique && !is(&ix.kind, SEARCH) && !is(&ix.kind, VECTOR) { "UNIQUE " } else { "" },
        if is(&ix.kind, NULL_FILTERED) { "NULL_FILTERED " } else { "" },
        index_word(ix),
        if if_not_exists { "IF NOT EXISTS " } else { "" },
        bq_qualified(schema, &ix.name),
        bq_qualified(schema, table),
        keys.join(", ")
    );
    if !ix.include.is_empty() {
        s.push_str(&format!(" STORING ({})", ix.include.iter().map(|c| bq(c)).collect::<Vec<_>>().join(", ")));
    }
    if let Some(p) = ix.options.get(OPT_PARTITION_BY).filter(|p| !p.is_empty()) {
        s.push_str(&format!(" PARTITION BY {p}"));
    }
    if let Some(o) = ix.options.get(OPT_ORDER_BY).filter(|o| !o.is_empty()) {
        s.push_str(&format!(" ORDER BY {o}"));
    }
    if let Some(f) = ix.filter.as_deref().filter(|f| !f.trim().is_empty()) {
        s.push_str(&format!(" WHERE {}", f.trim()));
    }
    if let Some(p) = ix.options.get(OPT_INTERLEAVE).filter(|p| !p.is_empty()) {
        s.push_str(&format!(", INTERLEAVE IN {}", p.split('.').map(bq).collect::<Vec<_>>().join(".")));
    }
    let opts: Vec<String> = ix.options.iter().filter(|(k, _)| !STRUCTURAL.contains(&k.as_str())).map(|(k, v)| format!("{k} = {v}")).collect();
    if !opts.is_empty() {
        s.push_str(&format!(" OPTIONS ({})", opts.join(", ")));
    }
    s.push(';');
    s
}

/// One index's DROP statement.
pub fn drop_index(schema: Option<&str>, ix: &IndexDef, if_exists: bool) -> String {
    format!("DROP {} {}{};", index_word(ix), if if_exists { "IF EXISTS " } else { "" }, bq_qualified(schema, &ix.name))
}

fn split_list(v: Option<&String>) -> Vec<String> {
    v.map(|s| s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()).unwrap_or_default()
}

/// A CHECK as CREATE TABLE and ADD CONSTRAINT write it. A name Spanner
/// generated is left out: the engine generates another.
pub fn check_clause(table: &str, c: &CheckDef) -> String {
    let named = c.name.as_deref().filter(|n| !n.is_empty() && !generated_check_name(table, n));
    let expr = c.expression.trim();
    match named {
        Some(n) => format!("CONSTRAINT {} CHECK ({expr})", bq(n)),
        None => format!("CHECK ({expr})"),
    }
}

/// Before the generic planner: a CHECK whose name Spanner generated is
/// matched by condition (it takes the other side's name, so nothing is
/// remade), and a new one goes without a name.
pub fn prepare_checks(changes: &[TableChange]) -> Vec<TableChange> {
    changes
        .iter()
        .map(|ch| match ch {
            TableChange::Alter { old, new } => {
                let mut new = new.clone();
                for c in new.checks.iter_mut() {
                    if !c.name.as_deref().is_some_and(|n| generated_check_name(&new.name, n)) {
                        continue;
                    }
                    let twin = old.checks.iter().find(|o| {
                        o.name.as_deref().is_some_and(|m| generated_check_name(&old.name, m)) && check_expr(&o.expression) == check_expr(&c.expression)
                    });
                    c.name = twin.and_then(|o| o.name.clone());
                }
                TableChange::Alter { old: old.clone(), new }
            }
            other => other.clone(),
        })
        .collect()
}

/// After the generic planner: search and vector indexes drop with their
/// own keyword.
pub fn fix_script(script: &mut SyncScript, changes: &[TableChange]) {
    for ch in changes {
        let TableChange::Alter { old, .. } = ch else { continue };
        let schema = old.schema.as_deref().filter(|s| !s.is_empty());
        for ix in old.indexes.iter().filter(|i| is(&i.kind, SEARCH) || is(&i.kind, VECTOR)) {
            let plain = format!("DROP INDEX {};", bq_qualified(schema, &ix.name));
            for s in script.statements.iter_mut().filter(|s| **s == plain) {
                *s = drop_index(schema, ix, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_check_names() {
        assert!(generated_check_name("p", "CK_p_38D5BCC6B025E47F_1"));
        assert!(!generated_check_name("p", "ck_a"));
        assert!(!generated_check_name("p", "CK_q_38D5BCC6B025E47F_1"));
        assert!(!generated_check_name("p", "CK_p_38D5BCC6_1"));
        assert!(not_null_check("CK_IS_NOT_NULL_p_id"));
    }

    #[test]
    fn names_and_options_from_the_ddl() {
        assert_eq!(created_name("CREATE SEQUENCE s1 OPTIONS (\n  sequence_kind = 'bit_reversed_positive')", "SEQUENCE").as_deref(), Some("s1"));
        assert_eq!(created_name("CREATE SEQUENCE `a`.`s` OPTIONS ()", "SEQUENCE").as_deref(), Some("a.s"));
        assert_eq!(created_name("CREATE SEARCH INDEX sx ON p(toks)", "SEARCH INDEX").as_deref(), Some("sx"));
        assert_eq!(created_name("CREATE INDEX sx ON p(toks)", "SEARCH INDEX"), None);
        let ddl = ["CREATE VECTOR INDEX vx ON v(e) WHERE e IS NOT NULL OPTIONS ( distance_type = 'COSINE', tree_depth = 2 )", "CREATE SEARCH INDEX sx ON p(toks)"];
        let m = index_options_from_ddl(ddl);
        assert_eq!(m.len(), 1);
        assert_eq!(m["vx"].get("distance_type").map(String::as_str), Some("'COSINE'"));
        assert_eq!(m["vx"].get("tree_depth").map(String::as_str), Some("2"));
    }

    #[test]
    fn index_statements() {
        let mut ix = IndexDef { name: "ix".into(), columns: vec!["a".into(), "c".into()], include: vec!["b".into()], ..Default::default() };
        ix.options.insert(OPT_DESC.into(), "a".into());
        assert_eq!(index_ddl(None, "p", &ix, false), "CREATE INDEX `ix` ON `p` (`a` DESC, `c`) STORING (`b`);");
        let mut ix = IndexDef { name: "ic".into(), columns: vec!["id".into()], unique: true, kind: Some(NULL_FILTERED.into()), filter: Some("x IS NOT NULL".into()), ..Default::default() };
        ix.options.insert(OPT_INTERLEAVE.into(), "p".into());
        assert_eq!(index_ddl(Some("s"), "ch", &ix, true), "CREATE UNIQUE NULL_FILTERED INDEX IF NOT EXISTS `s`.`ic` ON `s`.`ch` (`id`) WHERE x IS NOT NULL, INTERLEAVE IN `p`;");
        let mut ix = IndexDef { name: "vx".into(), columns: vec!["e".into()], kind: Some(VECTOR.into()), filter: Some("e IS NOT NULL".into()), ..Default::default() };
        ix.options.insert("distance_type".into(), "'COSINE'".into());
        assert_eq!(index_ddl(None, "v", &ix, false), "CREATE VECTOR INDEX `vx` ON `v` (`e`) WHERE e IS NOT NULL OPTIONS (distance_type = 'COSINE');");
        assert_eq!(drop_index(None, &ix, true), "DROP VECTOR INDEX IF EXISTS `vx`;");
        let mut ix = IndexDef { name: "sx".into(), columns: vec!["toks".into()], unique: true, kind: Some(SEARCH.into()), ..Default::default() };
        ix.options.insert(OPT_PARTITION_BY.into(), "a".into());
        ix.options.insert(OPT_ORDER_BY.into(), "c DESC".into());
        assert_eq!(index_ddl(None, "p", &ix, false), "CREATE SEARCH INDEX `sx` ON `p` (`toks`) PARTITION BY a ORDER BY c DESC;");
    }

    #[test]
    fn generated_check_names_pair_by_condition() {
        let chk = |n: &str, e: &str| CheckDef { name: Some(n.into()), expression: e.into() };
        let old = TableSchema { name: "p".into(), checks: vec![chk("CK_p_38D5BCC6B025E47F_1", "c < 100")], ..Default::default() };
        let new = TableSchema {
            name: "p".into(),
            checks: vec![chk("CK_p_0000000000000000_1", "c < 100"), chk("CK_p_0000000000000000_2", "c > 1"), chk("ck_a", "a > 0")],
            ..Default::default()
        };
        let ch = prepare_checks(&[TableChange::Alter { old, new }]);
        let TableChange::Alter { new, .. } = &ch[0] else { unreachable!() };
        assert_eq!(new.checks[0].name.as_deref(), Some("CK_p_38D5BCC6B025E47F_1"));
        assert_eq!(check_clause("p", &new.checks[1]), "CHECK (c > 1)");
        assert_eq!(check_clause("p", &new.checks[2]), "CONSTRAINT `ck_a` CHECK (a > 0)");
    }
}
