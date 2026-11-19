//! What "Comparar esquemas" needs beyond columns and keys: CHECK
//! constraints, every index kind (secondary with `STORING`, descending keys,
//! `WHERE … IS NOT NULL` and `INTERLEAVE IN`; search and vector indexes with
//! their `OPTIONS`) and sequences, read from INFORMATION_SCHEMA plus the
//! database DDL (the index `OPTIONS` are only there) and written back as
//! GoogleSQL.
//!
//! Index settings go into [`IndexDef::options`]: `desc` (the descending key
//! columns), `interleave_in`, `partition_by` / `order_by` (search indexes)
//! and each `OPTIONS (…)` entry under its own name. A vector index also
//! gets its [`IndexDef::vector`]: the distance (`cosine`, `l2`, `ip`) and
//! the tree's sizes (`num_leaves` as the standard `lists`), which win over
//! the native options when the index is written (the designer edits them
//! there).

use crate::{bq, bq_qualified};
use dbine_driver::alter::check_expr;
use dbine_driver::vector::{metric, param};
use dbine_driver::{CheckDef, IndexDef, SyncScript, TableChange, TableSchema, VectorIndex};
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

/// A vector index, by its kind or by its vector settings (the designer
/// may set only those).
pub fn is_vector(ix: &IndexDef) -> bool {
    ix.vector.is_some() || is(&ix.kind, VECTOR)
}

/// `CREATE [SEARCH |VECTOR ]INDEX` / `DROP … INDEX`: the keyword for the kind.
fn index_word(ix: &IndexDef) -> &'static str {
    if is(&ix.kind, SEARCH) {
        "SEARCH INDEX"
    } else if is_vector(ix) {
        "VECTOR INDEX"
    } else {
        "INDEX"
    }
}

/// A vector index's `OPTIONS` entries (the distance and the tree's sizes).
const VECTOR_OPTIONS: &[&str] = &[
    "distance_type",
    "tree_depth",
    "num_leaves",
    "num_branches",
    "leaf_scatter_factor",
    "min_leaf_splits",
    "min_branch_splits",
];

/// What a vector parameter is called in `OPTIONS` (`lists` is `num_leaves`).
fn spanner_param(k: &str) -> &str {
    if k == param::LISTS {
        "num_leaves"
    } else {
        k
    }
}

fn unquoted(v: &str) -> String {
    v.trim().trim_matches(|c| c == '\'' || c == '"').to_string()
}

/// The vector settings of a vector index's `OPTIONS`.
pub fn vector_of(options: &BTreeMap<String, String>) -> VectorIndex {
    let metric =
        options
            .get("distance_type")
            .map(|d| match unquoted(d).to_ascii_uppercase().as_str() {
                "COSINE" => metric::COSINE.to_string(),
                "EUCLIDEAN" => metric::L2.to_string(),
                "DOT_PRODUCT" => metric::IP.to_string(),
                other => other.to_ascii_lowercase(),
            });
    let params = VECTOR_OPTIONS[1..]
        .iter()
        .filter_map(|k| {
            let v = options.get(*k)?;
            let k = if *k == "num_leaves" { param::LISTS } else { k };
            Some((k.to_string(), unquoted(v)))
        })
        .collect();
    VectorIndex {
        algorithm: None,
        metric,
        params,
    }
}

/// The index's options, with a vector index's settings from its
/// [`IndexDef::vector`] in Spanner's words. A vector index needs a
/// distance: COSINE when none was chosen.
pub fn effective_options(ix: &IndexDef) -> BTreeMap<String, String> {
    let Some(v) = &ix.vector else {
        return ix.options.clone();
    };
    let mut o: BTreeMap<String, String> = ix
        .options
        .iter()
        .filter(|(k, _)| !VECTOR_OPTIONS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let distance = match v.metric.as_deref() {
        None | Some(metric::COSINE) => "COSINE".to_string(),
        Some(metric::L2) => "EUCLIDEAN".into(),
        Some(metric::IP) => "DOT_PRODUCT".into(),
        Some(other) => other.to_ascii_uppercase(),
    };
    o.insert("distance_type".into(), format!("'{distance}'"));
    for (k, val) in &v.params {
        let k = spanner_param(k);
        if VECTOR_OPTIONS[1..].contains(&k) && !val.trim().is_empty() {
            o.insert(k.to_string(), val.trim().to_string());
        }
    }
    o
}

/// A vector index's settings, checked before anything runs (Spanner says
/// the same, but only after the script started): `tree_depth` 2 or 3,
/// whole numbers above zero for the tree's sizes, and a
/// `leaf_scatter_factor` from 0 to 32.
pub fn check_vectors(t: &TableSchema) -> dbine_driver::Result<()> {
    for ix in t.indexes.iter().filter(|ix| is_vector(ix)) {
        for (k, v) in effective_options(ix) {
            if k == "distance_type" || !VECTOR_OPTIONS.contains(&k.as_str()) {
                continue;
            }
            let n = unquoted(&v).parse::<i64>().ok();
            let ok = match k.as_str() {
                "tree_depth" => matches!(n, Some(2 | 3)),
                "leaf_scatter_factor" => n.is_some_and(|n| (0..=32).contains(&n)),
                _ => n.is_some_and(|n| n > 0),
            };
            if !ok {
                let why = match k.as_str() {
                    "tree_depth" => "la profundidad del árbol es 2 o 3",
                    "leaf_scatter_factor" => "va de 0 a 32",
                    _ => "es un número entero mayor que cero",
                };
                return Err(dbine_driver::Error::Unsupported(format!(
                    "Índice vectorial {}: {} = {} no sirve ({why}).",
                    ix.name,
                    spanner_label(&k),
                    v.trim()
                )));
            }
        }
    }
    Ok(())
}

/// A tree setting as the designer shows it (`num_leaves` is its lists).
fn spanner_label(k: &str) -> &str {
    if k == "num_leaves" {
        "num_leaves (listas)"
    } else {
        k
    }
}

/// A vector index as Spanner writes it: one key column (the vector; the
/// designer's filter columns are `STORING`), a vector kind, its options from
/// its settings, and, on a nullable column, the `IS NOT NULL` filter
/// Spanner requires. Any other index is left as it is.
pub fn prepared(t: &TableSchema, ix: &IndexDef) -> IndexDef {
    if !is_vector(ix) {
        return ix.clone();
    }
    let mut out = ix.clone();
    out.kind = Some(VECTOR.into());
    out.unique = false;
    out.options = effective_options(ix);
    if out.columns.len() > 1 {
        let rest = out.columns.split_off(1);
        for c in rest {
            if !out.include.iter().any(|i| i.eq_ignore_ascii_case(&c)) {
                out.include.push(c);
            }
        }
    }
    if out.filter.as_deref().is_none_or(|f| f.trim().is_empty()) {
        if let Some(c) = out.columns.first() {
            let nullable = t
                .columns
                .iter()
                .find(|x| x.name.eq_ignore_ascii_case(c))
                .is_none_or(|x| x.nullable);
            if nullable {
                // As Spanner reports it back (INFORMATION_SCHEMA.INDEXES.FILTER).
                let col = if c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
                    c.clone()
                } else {
                    bq(c)
                };
                out.filter = Some(format!("{col} IS NOT NULL"));
            }
        }
    }
    out
}

/// Spanner names an unnamed CHECK `CK_<table>_<16 hex>_<n>`; the name
/// doesn't carry over to another database.
pub fn generated_check_name(table: &str, name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("CK_")
        .and_then(|r| r.strip_prefix(table))
        .and_then(|r| r.strip_prefix('_'))
    else {
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
    let name: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '(')
        .collect();
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
    let Some(at) = upper.rfind("OPTIONS") else {
        return BTreeMap::new();
    };
    let Some(open) = stmt[at..].find('(').map(|o| at + o) else {
        return BTreeMap::new();
    };
    let Some(inner) = balanced(stmt, open) else {
        return BTreeMap::new();
    };
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
pub fn index_options_from_ddl<'a>(
    statements: impl IntoIterator<Item = &'a str>,
) -> BTreeMap<String, BTreeMap<String, String>> {
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
    let desc: Vec<String> = split_list(ix.options.get(OPT_DESC))
        .into_iter()
        .map(|c| c.to_lowercase())
        .collect();
    let keys: Vec<String> = ix
        .columns
        .iter()
        .map(|c| {
            if desc.contains(&c.to_lowercase()) {
                format!("{} DESC", bq(c))
            } else {
                bq(c)
            }
        })
        .collect();
    let mut s = format!(
        "CREATE {}{}{} {}{} ON {} ({})",
        if ix.unique && !is(&ix.kind, SEARCH) && !is_vector(ix) {
            "UNIQUE "
        } else {
            ""
        },
        if is(&ix.kind, NULL_FILTERED) {
            "NULL_FILTERED "
        } else {
            ""
        },
        index_word(ix),
        if if_not_exists { "IF NOT EXISTS " } else { "" },
        bq_qualified(schema, &ix.name),
        bq_qualified(schema, table),
        keys.join(", ")
    );
    if !ix.include.is_empty() {
        s.push_str(&format!(
            " STORING ({})",
            ix.include
                .iter()
                .map(|c| bq(c))
                .collect::<Vec<_>>()
                .join(", ")
        ));
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
        s.push_str(&format!(
            ", INTERLEAVE IN {}",
            p.split('.').map(bq).collect::<Vec<_>>().join(".")
        ));
    }
    let opts: Vec<String> = ix
        .options
        .iter()
        .filter(|(k, _)| !STRUCTURAL.contains(&k.as_str()))
        .map(|(k, v)| format!("{k} = {v}"))
        .collect();
    if !opts.is_empty() {
        s.push_str(&format!(" OPTIONS ({})", opts.join(", ")));
    }
    s.push(';');
    s
}

/// One index's DROP statement.
pub fn drop_index(schema: Option<&str>, ix: &IndexDef, if_exists: bool) -> String {
    format!(
        "DROP {} {}{};",
        index_word(ix),
        if if_exists { "IF EXISTS " } else { "" },
        bq_qualified(schema, &ix.name)
    )
}

fn split_list(v: Option<&String>) -> Vec<String> {
    v.map(|s| {
        s.split(',')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// A CHECK as CREATE TABLE and ADD CONSTRAINT write it. A name Spanner
/// generated is left out: the engine generates another.
pub fn check_clause(table: &str, c: &CheckDef) -> String {
    let named = c
        .name
        .as_deref()
        .filter(|n| !n.is_empty() && !generated_check_name(table, n));
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
                    if !c
                        .name
                        .as_deref()
                        .is_some_and(|n| generated_check_name(&new.name, n))
                    {
                        continue;
                    }
                    let twin = old.checks.iter().find(|o| {
                        o.name
                            .as_deref()
                            .is_some_and(|m| generated_check_name(&old.name, m))
                            && check_expr(&o.expression) == check_expr(&c.expression)
                    });
                    c.name = twin.and_then(|o| o.name.clone());
                }
                TableChange::Alter {
                    old: old.clone(),
                    new,
                }
            }
            other => other.clone(),
        })
        .collect()
}

/// Before the generic planner: vector indexes in the form Spanner writes
/// and reports them ([`prepared`]), so a designer's change to their
/// settings is a change and an untouched one isn't.
pub fn prepare_vectors(changes: &[TableChange]) -> Vec<TableChange> {
    let fix = |t: &TableSchema| {
        let mut t = t.clone();
        t.indexes = t.indexes.iter().map(|ix| prepared(&t, ix)).collect();
        t
    };
    changes
        .iter()
        .map(|ch| match ch {
            TableChange::Alter { old, new } => TableChange::Alter {
                old: fix(old),
                new: fix(new),
            },
            TableChange::Create { table } => TableChange::Create { table: fix(table) },
            other => other.clone(),
        })
        .collect()
}

/// After the generic planner: search and vector indexes drop with their
/// own keyword.
pub fn fix_script(script: &mut SyncScript, changes: &[TableChange]) {
    for ch in changes {
        let TableChange::Alter { old, .. } = ch else {
            continue;
        };
        let schema = old.schema.as_deref().filter(|s| !s.is_empty());
        for ix in old
            .indexes
            .iter()
            .filter(|i| is(&i.kind, SEARCH) || is_vector(i))
        {
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
        assert_eq!(
            created_name(
                "CREATE SEQUENCE s1 OPTIONS (\n  sequence_kind = 'bit_reversed_positive')",
                "SEQUENCE"
            )
            .as_deref(),
            Some("s1")
        );
        assert_eq!(
            created_name("CREATE SEQUENCE `a`.`s` OPTIONS ()", "SEQUENCE").as_deref(),
            Some("a.s")
        );
        assert_eq!(
            created_name("CREATE SEARCH INDEX sx ON p(toks)", "SEARCH INDEX").as_deref(),
            Some("sx")
        );
        assert_eq!(
            created_name("CREATE INDEX sx ON p(toks)", "SEARCH INDEX"),
            None
        );
        let ddl = ["CREATE VECTOR INDEX vx ON v(e) WHERE e IS NOT NULL OPTIONS ( distance_type = 'COSINE', tree_depth = 2 )", "CREATE SEARCH INDEX sx ON p(toks)"];
        let m = index_options_from_ddl(ddl);
        assert_eq!(m.len(), 1);
        assert_eq!(
            m["vx"].get("distance_type").map(String::as_str),
            Some("'COSINE'")
        );
        assert_eq!(m["vx"].get("tree_depth").map(String::as_str), Some("2"));
    }

    #[test]
    fn index_statements() {
        let mut ix = IndexDef {
            name: "ix".into(),
            columns: vec!["a".into(), "c".into()],
            include: vec!["b".into()],
            ..Default::default()
        };
        ix.options.insert(OPT_DESC.into(), "a".into());
        assert_eq!(
            index_ddl(None, "p", &ix, false),
            "CREATE INDEX `ix` ON `p` (`a` DESC, `c`) STORING (`b`);"
        );
        let mut ix = IndexDef {
            name: "ic".into(),
            columns: vec!["id".into()],
            unique: true,
            kind: Some(NULL_FILTERED.into()),
            filter: Some("x IS NOT NULL".into()),
            ..Default::default()
        };
        ix.options.insert(OPT_INTERLEAVE.into(), "p".into());
        assert_eq!(index_ddl(Some("s"), "ch", &ix, true), "CREATE UNIQUE NULL_FILTERED INDEX IF NOT EXISTS `s`.`ic` ON `s`.`ch` (`id`) WHERE x IS NOT NULL, INTERLEAVE IN `p`;");
        let mut ix = IndexDef {
            name: "vx".into(),
            columns: vec!["e".into()],
            kind: Some(VECTOR.into()),
            filter: Some("e IS NOT NULL".into()),
            ..Default::default()
        };
        ix.options.insert("distance_type".into(), "'COSINE'".into());
        assert_eq!(index_ddl(None, "v", &ix, false), "CREATE VECTOR INDEX `vx` ON `v` (`e`) WHERE e IS NOT NULL OPTIONS (distance_type = 'COSINE');");
        assert_eq!(
            drop_index(None, &ix, true),
            "DROP VECTOR INDEX IF EXISTS `vx`;"
        );
        let mut ix = IndexDef {
            name: "sx".into(),
            columns: vec!["toks".into()],
            unique: true,
            kind: Some(SEARCH.into()),
            ..Default::default()
        };
        ix.options.insert(OPT_PARTITION_BY.into(), "a".into());
        ix.options.insert(OPT_ORDER_BY.into(), "c DESC".into());
        assert_eq!(
            index_ddl(None, "p", &ix, false),
            "CREATE SEARCH INDEX `sx` ON `p` (`toks`) PARTITION BY a ORDER BY c DESC;"
        );
    }

    #[test]
    fn vector_indexes_carry_their_settings() {
        let ddl = "CREATE VECTOR INDEX vx ON v(f) STORING (e) WHERE f IS NOT NULL OPTIONS ( tree_depth = 3, num_leaves = 100, num_branches = 10, distance_type = 'DOT_PRODUCT' )";
        let opts = ddl_options(ddl);
        let v = vector_of(&opts);
        assert_eq!(v.metric.as_deref(), Some("ip"));
        assert_eq!(v.algorithm, None);
        assert_eq!(v.params.get("lists").map(String::as_str), Some("100"));
        assert_eq!(v.params.get("tree_depth").map(String::as_str), Some("3"));
        assert_eq!(v.params.get("num_branches").map(String::as_str), Some("10"));
        // Read back as it was written: nothing to sync.
        let ix = IndexDef {
            name: "vx".into(),
            columns: vec!["f".into()],
            include: vec!["e".into()],
            kind: Some(VECTOR.into()),
            filter: Some("f IS NOT NULL".into()),
            options: opts.clone(),
            vector: Some(v),
            ..Default::default()
        };
        assert_eq!(effective_options(&ix), opts);
        let t = TableSchema {
            name: "v".into(),
            columns: vec![
                dbine_driver::ColumnDef {
                    name: "id".into(),
                    data_type: "INT64".into(),
                    nullable: false,
                    ..Default::default()
                },
                dbine_driver::ColumnDef {
                    name: "f".into(),
                    data_type: "ARRAY<FLOAT64>(vector_length=>2)".into(),
                    nullable: true,
                    ..Default::default()
                },
                dbine_driver::ColumnDef {
                    name: "e".into(),
                    data_type: "ARRAY<FLOAT32>(vector_length=>3)".into(),
                    nullable: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(prepared(&t, &ix), ix);
        assert_eq!(index_ddl(None, "v", &prepared(&t, &ix), false), "CREATE VECTOR INDEX `vx` ON `v` (`f`) STORING (`e`) WHERE f IS NOT NULL OPTIONS (distance_type = 'DOT_PRODUCT', num_branches = 10, num_leaves = 100, tree_depth = 3);");

        // From the designer: only its vector settings, a filter column, a
        // nullable vector column (Spanner wants it null-filtered).
        let mut d = dbine_driver::VectorIndex {
            metric: Some("l2".into()),
            ..Default::default()
        };
        d.params.insert("lists".into(), "50".into());
        d.params.insert("m".into(), "16".into());
        let designed = IndexDef {
            name: "vy".into(),
            columns: vec!["f".into(), "e".into()],
            vector: Some(d),
            ..Default::default()
        };
        let p = prepared(&t, &designed);
        assert_eq!(index_ddl(Some("s"), "v", &p, true), "CREATE VECTOR INDEX IF NOT EXISTS `s`.`vy` ON `s`.`v` (`f`) STORING (`e`) WHERE f IS NOT NULL OPTIONS (distance_type = 'EUCLIDEAN', num_leaves = 50);");
        assert_eq!(
            drop_index(None, &designed, false),
            "DROP VECTOR INDEX `vy`;"
        );
        // A NOT NULL column takes no filter; no distance chosen is COSINE.
        let p = prepared(
            &t,
            &IndexDef {
                name: "vz".into(),
                columns: vec!["e".into()],
                vector: Some(Default::default()),
                ..Default::default()
            },
        );
        assert_eq!(
            index_ddl(None, "v", &p, false),
            "CREATE VECTOR INDEX `vz` ON `v` (`e`) OPTIONS (distance_type = 'COSINE');"
        );
        // The designer changing the distance of an index read from the
        // database: its options say so, so the sync sees the change.
        let mut edited = ix.clone();
        edited.vector.as_mut().unwrap().metric = Some("cosine".into());
        let ch = prepare_vectors(&[TableChange::Alter {
            old: TableSchema {
                indexes: vec![ix.clone()],
                ..t.clone()
            },
            new: TableSchema {
                indexes: vec![edited],
                ..t.clone()
            },
        }]);
        let TableChange::Alter { old, new } = &ch[0] else {
            unreachable!()
        };
        assert_ne!(old.indexes[0].options, new.indexes[0].options);
        assert_eq!(
            new.indexes[0]
                .options
                .get("distance_type")
                .map(String::as_str),
            Some("'COSINE'")
        );
    }

    #[test]
    fn generated_check_names_pair_by_condition() {
        let chk = |n: &str, e: &str| CheckDef {
            name: Some(n.into()),
            expression: e.into(),
        };
        let old = TableSchema {
            name: "p".into(),
            checks: vec![chk("CK_p_38D5BCC6B025E47F_1", "c < 100")],
            ..Default::default()
        };
        let new = TableSchema {
            name: "p".into(),
            checks: vec![
                chk("CK_p_0000000000000000_1", "c < 100"),
                chk("CK_p_0000000000000000_2", "c > 1"),
                chk("ck_a", "a > 0"),
            ],
            ..Default::default()
        };
        let ch = prepare_checks(&[TableChange::Alter { old, new }]);
        let TableChange::Alter { new, .. } = &ch[0] else {
            unreachable!()
        };
        assert_eq!(
            new.checks[0].name.as_deref(),
            Some("CK_p_38D5BCC6B025E47F_1")
        );
        assert_eq!(check_clause("p", &new.checks[1]), "CHECK (c > 1)");
        assert_eq!(
            check_clause("p", &new.checks[2]),
            "CONSTRAINT `ck_a` CHECK (a > 0)"
        );
    }

    /// Settings Spanner refuses are refused before the script runs, with
    /// the same limits the emulator applies.
    #[test]
    fn vector_settings_are_checked() {
        let mk = |params: &[(&str, &str)]| {
            let mut v = VectorIndex {
                metric: Some(metric::COSINE.into()),
                ..Default::default()
            };
            for (k, val) in params {
                v.params.insert(k.to_string(), val.to_string());
            }
            TableSchema {
                name: "v".into(),
                indexes: vec![IndexDef {
                    name: "vx".into(),
                    columns: vec!["e".into()],
                    vector: Some(v),
                    ..Default::default()
                }],
                ..Default::default()
            }
        };
        assert!(check_vectors(&mk(&[("tree_depth", "2"), ("lists", "1000")])).is_ok());
        assert!(check_vectors(&mk(&[
            ("tree_depth", "3"),
            ("num_branches", "10"),
            ("leaf_scatter_factor", "0"),
            ("min_leaf_splits", "")
        ]))
        .is_ok());
        for bad in [
            ("tree_depth", "4"),
            ("lists", "0"),
            ("lists", "mil"),
            ("num_branches", "-1"),
            ("leaf_scatter_factor", "40"),
        ] {
            let e = check_vectors(&mk(&[bad])).unwrap_err().to_string();
            assert!(e.contains("vx") && e.contains("no sirve"), "{bad:?}: {e}");
        }
        // Through the driver's own entry points.
        let t = mk(&[("tree_depth", "5")]);
        assert!(crate::sync::sync_script(&[TableChange::Create { table: t.clone() }]).is_err());
        assert!(crate::sync::sync_script(&[TableChange::Alter {
            old: TableSchema {
                indexes: vec![],
                ..t.clone()
            },
            new: t
        }])
        .is_err());
    }
}
