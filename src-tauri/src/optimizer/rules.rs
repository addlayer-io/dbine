//! Rule-based rewrites (docs/query-optimizer.md). Each rule offers a
//! candidate only when it's certainly equivalent: what it can't prove
//! (with the query's own text, or with the structure of the tables) it
//! leaves alone, or reports as a note without rewriting. The text the user
//! wrote is kept: a rewrite replaces only the tokens it changes.
//!
//! The UI words each rule (`optimizer:rules.<rule>`); `params` fill it.

use super::catalog::{type_class, Catalog, TypeClass};
use super::lex::{Flavor, Kind};
use super::parse::{Block, ColRef, FromItem, JoinKind, Sql, R};
use super::{Candidate, Note, Source};
use dbine_driver::TableSchema;
use std::collections::BTreeMap;

/// Dialects whose query language lacks the constructs the rules rewrite
/// (subqueries, UNION, joins in the general form) or reads NULLs its own way.
const NO_RULES: &[&str] = &["cosmos", "partiql", "ksql", "iotdb", "tdengine", "influxql", "orientdb", "n1ql"];

const AGGREGATES: &[&str] = &[
    "count", "count_big", "sum", "min", "max", "avg", "group_concat", "string_agg", "listagg", "array_agg", "stddev", "stdev", "variance", "var",
    "bool_and", "bool_or", "every", "json_agg", "jsonb_agg", "median", "percentile_cont", "percentile_disc", "any_value", "approx_count_distinct",
    "collect_list", "collect_set", "groupuniqarray", "uniq",
];

/// Functions that give another value on each call: splitting or moving them changes the result.
const VOLATILE: &[&str] = &[
    "rand", "random", "newid", "uuid", "gen_random_uuid", "now", "sysdate", "systimestamp", "getdate", "sysdatetime", "current_timestamp", "nextval",
    "rownum", "row_number", "dbms_random", "uuid_generate_v4", "newsequentialid",
];

pub fn rules_apply(dialect: &str) -> bool {
    !NO_RULES.contains(&dialect)
}

struct Edit {
    start: usize,
    end: usize,
    text: String,
}

fn apply(src: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut out = src.to_string();
    let mut floor = usize::MAX;
    for e in edits {
        // Overlapping edits can't both apply; the later-starting one wins.
        if e.end > floor {
            continue;
        }
        out.replace_range(e.start..e.end, &e.text);
        floor = e.start;
    }
    out
}

struct Ctx<'a> {
    sql: &'a Sql<'a>,
    cat: Option<&'a Catalog<'a>>,
    dialect: &'a str,
}

/// The FROM items of a block and their tables in the catalog.
struct Scope<'a> {
    items: Vec<FromItem>,
    tables: Vec<Option<&'a TableSchema>>,
}

impl<'a> Scope<'a> {
    fn new(cx: &Ctx<'a>, b: &Block) -> Option<Self> {
        let items = cx.sql.from_items(b)?;
        let tables = items.iter().map(|it| it.name.as_ref().and_then(|n| cx.cat.and_then(|c| c.table(n)))).collect();
        Some(Self { items, tables })
    }

    fn labels(&self) -> Vec<String> {
        self.items.iter().filter_map(|i| i.label().map(str::to_lowercase)).collect()
    }

    fn item_of(&self, qualifier: &str) -> Option<usize> {
        let hits: Vec<usize> = (0..self.items.len()).filter(|&i| self.items[i].label().is_some_and(|l| l.eq_ignore_ascii_case(qualifier))).collect();
        (hits.len() == 1).then(|| hits[0])
    }

    /// Which item a column belongs to: by its qualifier; unqualified, the
    /// only item, or the only table (all of them known) that has it.
    fn resolve(&self, c: &ColRef) -> Option<usize> {
        if let Some(q) = c.qualifier() {
            return self.item_of(q);
        }
        if self.items.len() == 1 {
            return Some(0);
        }
        if self.tables.iter().any(Option::is_none) {
            return None;
        }
        let hits: Vec<usize> = (0..self.items.len()).filter(|&i| self.tables[i].is_some_and(|t| Catalog::column(t, c.column()).is_some())).collect();
        (hits.len() == 1).then(|| hits[0])
    }

    /// The table and column definition a reference reads, when known.
    fn column(&self, c: &ColRef) -> Option<(&'a TableSchema, &'a dbine_driver::ColumnDef)> {
        let t = self.tables[self.resolve(c)?]?;
        Some((t, Catalog::column(t, c.column())?))
    }

    /// Some known table of the scope has a column of this name; `None`
    /// when a table isn't known.
    fn any_has(&self, column: &str) -> Option<bool> {
        let mut any = false;
        for t in &self.tables {
            any |= Catalog::column((*t)?, column).is_some();
        }
        Some(any)
    }
}

/// Everything the rules found in `src`: candidates and notes.
pub fn analyze(src: &str, dialect: &str, tables: Option<&[TableSchema]>) -> (Vec<Candidate>, Vec<Note>) {
    if !rules_apply(dialect) {
        return (Vec::new(), Vec::new());
    }
    let sql = Sql::parse(src, Flavor::for_dialect(dialect));
    let catalog = tables.map(|t| Catalog::new(t, dialect));
    let cx = Ctx { sql: &sql, cat: catalog.as_ref(), dialect };
    let mut out = Out::default();
    for b in &sql.blocks {
        in_to_exists(&cx, b, &mut out);
        exists_to_in(&cx, b, &mut out);
        scalar_to_join(&cx, b, &mut out);
        or_to_union(&cx, b, &mut out);
        redundant_distinct(&cx, b, &mut out);
        function_to_range(&cx, b, &mut out);
        select_star(&cx, b, &mut out);
        implicit_conversions(&cx, b, &mut out);
    }
    count_to_exists(&cx, &mut out);
    let mut seen = std::collections::HashSet::new();
    out.notes.retain(|n| seen.insert((n.rule.clone(), n.params.clone())));
    let candidates = out
        .candidates
        .into_iter()
        .enumerate()
        .map(|(i, (rule, params, edits, verify))| Candidate {
            id: format!("rule-{}-{i}", rule),
            source: Source::Rule,
            rule: Some(rule.to_string()),
            params,
            title: None,
            explanation: None,
            sql: apply(src, edits),
            verify,
        })
        .filter(|c| c.sql != src)
        .collect();
    (candidates, out.notes)
}

#[derive(Default)]
struct Out {
    candidates: Vec<(&'static str, BTreeMap<String, String>, Vec<Edit>, bool)>,
    notes: Vec<Note>,
}

impl Out {
    fn candidate(&mut self, rule: &'static str, params: &[(&str, String)], edits: Vec<Edit>) {
        self.candidates.push((rule, params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(), edits, false));
    }
    fn note(&mut self, rule: &str, params: &[(&str, String)], sql: Option<String>) {
        self.notes.push(Note { rule: rule.into(), params: params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(), sql });
    }
}

fn replace(sql: &Sql, r: &R, text: String) -> Edit {
    Edit { start: sql.start_of(r.start), end: sql.end_of(r.end - 1), text }
}

/// A select item without its alias (`expr AS a`, `expr a`).
fn strip_alias(sql: &Sql, r: R) -> R {
    let n = r.end - r.start;
    if n >= 3 && sql.kw(r.end - 2, "as") {
        return r.start..r.end - 2;
    }
    if n >= 2 && sql.name(r.end - 1).is_some() && matches!(sql.kind(r.end - 2), Some(Kind::Word | Kind::Ident | Kind::RParen | Kind::Num | Kind::Str)) {
        // `a.b` is a column, not `a` aliased `b`.
        if !sql.is(r.end - 2, Kind::Dot) {
            return r.start..r.end - 1;
        }
    }
    r
}

/// `( … )` when the expression isn't a single token or a column.
fn paren_expr(sql: &Sql, r: &R) -> String {
    if r.end - r.start == 1 || sql.col_ref(r.clone()).is_some() || sql.unwrap_parens(r).is_some() {
        sql.slice(r.clone()).to_string()
    } else {
        format!("({})", sql.slice(r.clone()))
    }
}

/// A condition as one AND operand.
fn paren_cond(sql: &Sql, r: &R) -> String {
    if sql.disjuncts(r.clone()).len() > 1 {
        format!("({})", sql.slice(r.clone()))
    } else {
        sql.slice(r.clone()).to_string()
    }
}

/// A subquery the rules can move around: one SELECT, no grouping, limits or
/// clauses they don't read.
fn plain(b: &Block) -> bool {
    !b.set_op && b.group_by.is_none() && b.having.is_none() && b.order_by.is_none() && !b.limit && !b.top && !b.exotic && b.from.is_some()
}

/// `name .` for any of `labels` in `r`: the range names those tables.
fn names_qualifier(sql: &Sql, r: R, labels: &[String]) -> bool {
    r.into_iter().any(|i| sql.is(i + 1, Kind::Dot) && sql.name(i).is_some_and(|n| labels.contains(&n.to_lowercase())))
}

/// `a = b` with both sides plain column references.
fn col_equality(sql: &Sql, r: &R) -> Option<(ColRef, ColRef)> {
    let base = sql.depth[r.start];
    let eqs: Vec<usize> = r.clone().filter(|&i| sql.depth[i] == base && sql.op(i, "=")).collect();
    if eqs.len() != 1 {
        return None;
    }
    let e = eqs[0];
    Some((sql.col_ref(r.start..e)?, sql.col_ref(e + 1..r.end)?))
}

// ---------------------------------------------------------------------------
// x IN (SELECT y …) → EXISTS (SELECT 1 … AND y = x); NOT IN → NOT EXISTS
// when neither side can be NULL.

fn in_to_exists(cx: &Ctx, b: &Block, out: &mut Out) {
    let sql = cx.sql;
    let Some(w) = b.where_.clone() else { return };
    let Some(outer) = Scope::new(cx, b) else { return };
    for c in sql.conjuncts(w) {
        let base = sql.depth[c.start];
        let Some(k) = c.clone().find(|&i| sql.depth[i] == base && sql.kw(i, "in")) else { continue };
        let not = k > c.start && sql.kw(k - 1, "not");
        let lhs_end = if not { k - 1 } else { k };
        let Some(lhs) = sql.col_ref(c.start..lhs_end) else { continue };
        let open = k + 1;
        if sql.close.get(open).copied().flatten() != Some(c.end - 1) {
            continue;
        }
        let Some(s) = sql.subquery(open) else { continue };
        if !plain(s) || sql.has_subquery(s.select + 1..s.end) {
            continue;
        }
        let items = sql.items(s.list.clone());
        if items.len() != 1 {
            continue;
        }
        let item = strip_alias(sql, items[0].clone());
        if sql.slice(item.clone()).trim() == "*" || !sql.calls(item.clone(), AGGREGATES).is_empty() || sql.mentions(item.clone(), &["over"]) {
            continue;
        }
        let Some(inner) = Scope::new(cx, s) else { continue };
        // `x` must still mean the outer column inside the subquery.
        match lhs.qualifier() {
            Some(q) => {
                if outer.item_of(q).is_none() || inner.labels().contains(&q.to_lowercase()) {
                    continue;
                }
            }
            None => {
                if outer.column(&lhs).is_none() || inner.any_has(lhs.column()) != Some(false) {
                    continue;
                }
            }
        }
        if not {
            let lhs_safe = outer.column(&lhs).is_some_and(|(t, col)| Catalog::not_null(t, &col.name));
            let rhs_safe = sql.col_ref(item.clone()).and_then(|r| inner.column(&r)).is_some_and(|(t, col)| Catalog::not_null(t, &col.name));
            if !(lhs_safe && rhs_safe) {
                // Proven nullable: worth knowing even without a rewrite.
                let nullable = outer.column(&lhs).is_some_and(|(t, col)| !Catalog::not_null(t, &col.name))
                    || sql.col_ref(item.clone()).and_then(|r| inner.column(&r)).is_some_and(|(t, col)| !Catalog::not_null(t, &col.name));
                if nullable {
                    out.note("not_in_nullable", &[("column", sql.slice(lhs.range.clone()).to_string())], None);
                }
                continue;
            }
        }
        let cond = format!("{} = {}", paren_expr(sql, &item), sql.slice(lhs.range.clone()));
        let filter = match &s.where_ {
            Some(sw) => format!(" WHERE {} AND {cond}", paren_cond(sql, sw)),
            None => format!(" WHERE {cond}"),
        };
        let from = sql.slice(s.from_kw.unwrap_or(s.select)..s.where_kw.unwrap_or(s.end));
        let text = format!("{}EXISTS (SELECT 1 {from}{filter})", if not { "NOT " } else { "" });
        let rule = if not { "not_in_to_not_exists" } else { "in_to_exists" };
        out.candidate(rule, &[("column", sql.slice(lhs.range.clone()).to_string())], vec![replace(sql, &c, text)]);
    }
}

// ---------------------------------------------------------------------------
// EXISTS (SELECT … FROM t WHERE t.y = o.x AND …) → o.x IN (SELECT t.y FROM t WHERE …)

fn exists_to_in(cx: &Ctx, b: &Block, out: &mut Out) {
    let sql = cx.sql;
    let Some(w) = b.where_.clone() else { return };
    let Some(outer) = Scope::new(cx, b) else { return };
    let outer_labels = outer.labels();
    for c in sql.conjuncts(w) {
        if !sql.kw(c.start, "exists") || sql.close.get(c.start + 1).copied().flatten() != Some(c.end - 1) {
            continue;
        }
        let Some(s) = sql.subquery(c.start + 1) else { continue };
        if !plain(s) || !sql.calls(s.list.clone(), AGGREGATES).is_empty() || sql.has_subquery(s.select + 1..s.end) {
            continue;
        }
        let Some(inner) = Scope::new(cx, s) else { continue };
        if inner.items.len() != 1 || inner.items[0].name.is_none() {
            continue;
        }
        let Some(label) = inner.items[0].label().map(str::to_lowercase) else { continue };
        if outer_labels.contains(&label) {
            continue;
        }
        let Some(sw) = s.where_.clone() else { continue };
        let conj = sql.conjuncts(sw);
        let mut corr = Vec::new();
        for (n, cj) in conj.iter().enumerate() {
            let Some((a, bb)) = col_equality(sql, cj) else { continue };
            let side = |r: &ColRef| r.qualifier().map(str::to_lowercase);
            match (side(&a), side(&bb)) {
                (Some(x), Some(y)) if x == label && outer.item_of(&y).is_some() => corr.push((n, a, bb)),
                (Some(x), Some(y)) if y == label && outer.item_of(&x).is_some() => corr.push((n, bb, a)),
                _ => {}
            }
        }
        if corr.len() != 1 {
            continue;
        }
        let (n, inner_col, outer_col) = corr.remove(0);
        let rest: Vec<&R> = conj.iter().enumerate().filter(|(i, _)| *i != n).map(|(_, r)| r).collect();
        // The IN subquery stands alone: nothing else in it may read the outer query.
        if rest.iter().any(|r| names_qualifier(sql, (*r).clone(), &outer_labels)) {
            continue;
        }
        let filter = if rest.is_empty() { String::new() } else { format!(" WHERE {}", rest.iter().map(|r| sql.slice((*r).clone())).collect::<Vec<_>>().join(" AND ")) };
        let from = sql.slice(s.from_kw.unwrap_or(s.select)..s.where_kw.unwrap_or(s.end));
        let text = format!("{} IN (SELECT {} {from}{filter})", sql.slice(outer_col.range.clone()), sql.slice(inner_col.range.clone()));
        out.candidate("exists_to_in", &[("column", sql.slice(outer_col.range.clone()).to_string())], vec![replace(sql, &c, text)]);
    }
}

// ---------------------------------------------------------------------------
// SELECT …, (SELECT COUNT(*) FROM d WHERE d.k = o.id) AS n FROM o
//   → SELECT …, COALESCE(sq1.v, 0) AS n FROM o LEFT JOIN (SELECT d.k AS k1, COUNT(*) AS v FROM d GROUP BY d.k) sq1 ON sq1.k1 = o.id

fn scalar_to_join(cx: &Ctx, b: &Block, out: &mut Out) {
    let sql = cx.sql;
    if b.group_by.is_some() || b.having.is_some() || b.exotic || b.from.is_none() {
        return;
    }
    if !sql.calls(b.list.clone(), AGGREGATES).is_empty() || sql.mentions(b.list.clone(), &["over"]) {
        return;
    }
    let Some(outer) = Scope::new(cx, b) else { return };
    if outer.items.iter().any(|i| i.join == JoinKind::Comma) {
        return;
    }
    let outer_labels = outer.labels();
    let mut edits = Vec::new();
    let mut joins = Vec::new();
    let mut names = Vec::new();
    let mut n = 0;
    for item in sql.items(b.list.clone()) {
        let open = item.start;
        let Some(close) = sql.close.get(open).copied().flatten() else { continue };
        let Some(s) = sql.subquery(open) else { continue };
        // `(…) AS name` or `(…) name`, nothing else.
        let alias_ok = (close + 3 == item.end && sql.kw(close + 1, "as") && sql.name(close + 2).is_some()) || (close + 2 == item.end && sql.name(close + 1).is_some());
        if !alias_ok || !plain(s) || s.distinct.is_some() || sql.has_subquery(s.select + 1..s.end) {
            continue;
        }
        let list = sql.items(s.list.clone());
        if list.len() != 1 {
            continue;
        }
        let agg = strip_alias(sql, list[0].clone());
        let f = agg.start;
        let is_agg = sql.kw_any(f, &["count", "sum", "min", "max", "avg"]) && sql.close.get(f + 1).copied().flatten() == Some(agg.end - 1);
        if !is_agg || sql.calls(f + 2..agg.end - 1, AGGREGATES).len() > 0 || sql.mentions(f..agg.end, &["over"]) {
            continue;
        }
        let Some(inner) = Scope::new(cx, s) else { continue };
        if inner.items.len() != 1 || inner.items[0].name.is_none() {
            continue;
        }
        let Some(label) = inner.items[0].label().map(str::to_lowercase) else { continue };
        if outer_labels.contains(&label) {
            continue;
        }
        let Some(sw) = s.where_.clone() else { continue };
        let conj = sql.conjuncts(sw);
        let mut corr = Vec::new();
        let mut rest = Vec::new();
        for cj in &conj {
            let pair = col_equality(sql, cj).and_then(|(a, bb)| {
                let side = |r: &ColRef| r.qualifier().map(str::to_lowercase);
                match (side(&a), side(&bb)) {
                    (Some(x), Some(y)) if x == label && outer.item_of(&y).is_some() => Some((a, bb)),
                    (Some(x), Some(y)) if y == label && outer.item_of(&x).is_some() => Some((bb, a)),
                    _ => None,
                }
            });
            match pair {
                Some(p) => corr.push(p),
                None => rest.push(cj.clone()),
            }
        }
        if corr.is_empty() {
            continue;
        }
        // The rest and the aggregate read only the inner table: qualified
        // with its name, or unqualified and proven to be its columns (the
        // derived table can't see the outer query).
        let inner_only = |r: R| {
            sql.col_refs(r).iter().all(|c| match c.qualifier() {
                Some(q) => q.eq_ignore_ascii_case(&label),
                None => inner.tables[0].is_some_and(|t| Catalog::column(t, c.column()).is_some()),
            })
        };
        if !rest.iter().all(|r| inner_only(r.clone())) || !inner_only(f + 2..agg.end - 1) || rest.iter().any(|r| names_qualifier(sql, r.clone(), &outer_labels)) {
            continue;
        }
        n += 1;
        let d = (1..).map(|i| format!("sq{i}")).find(|a| !names.contains(a) && !(0..sql.len()).any(|t| sql.text(t).eq_ignore_ascii_case(a))).unwrap_or_default();
        names.push(d.clone());
        let keys: Vec<String> = corr.iter().enumerate().map(|(i, (ic, _))| format!("{} AS k{}", sql.slice(ic.range.clone()), i + 1)).collect();
        let group: Vec<&str> = corr.iter().map(|(ic, _)| sql.slice(ic.range.clone())).collect();
        let on: Vec<String> = corr.iter().enumerate().map(|(i, (_, oc))| format!("{d}.k{} = {}", i + 1, sql.slice(oc.range.clone()))).collect();
        let filter = if rest.is_empty() { String::new() } else { format!(" WHERE {}", rest.iter().map(|r| sql.slice(r.clone())).collect::<Vec<_>>().join(" AND ")) };
        let from = sql.slice(s.from_kw.unwrap_or(s.select)..s.where_kw.unwrap_or(s.end));
        joins.push(format!(
            "\nLEFT JOIN (SELECT {}, {} AS v {from}{filter} GROUP BY {}) {d} ON {}",
            keys.join(", "),
            sql.slice(agg.clone()),
            group.join(", "),
            on.join(" AND ")
        ));
        let value = if sql.kw(f, "count") { format!("COALESCE({d}.v, 0)") } else { format!("{d}.v") };
        edits.push(replace(sql, &(open..close + 1), value));
    }
    if n == 0 {
        return;
    }
    let Some(from) = b.from.clone() else { return };
    let at = sql.end_of(from.end - 1);
    edits.push(Edit { start: at, end: at, text: joins.concat() });
    out.candidate("scalar_to_join", &[("count", n.to_string())], edits);
}

// ---------------------------------------------------------------------------
// WHERE a = 1 OR b = 2 → … WHERE a = 1 UNION ALL … WHERE b = 2 AND CASE WHEN a = 1 THEN 0 ELSE 1 END = 1

fn or_to_union(cx: &Ctx, b: &Block, out: &mut Out) {
    let sql = cx.sql;
    if b.depth != 0 || b.parens.is_some() || b.set_op || b.distinct.is_some() || b.top || b.limit || b.exotic {
        return;
    }
    if b.group_by.is_some() || b.having.is_some() || b.order_by.is_some() || b.from.is_none() {
        return;
    }
    if !sql.calls(b.list.clone(), AGGREGATES).is_empty() || sql.mentions(b.select..b.end, &["over"]) || sql.mentions(b.select..b.end, VOLATILE) {
        return;
    }
    // The statement is this block (after a WITH, an INSERT INTO … or a CREATE VIEW … AS).
    if b.end < sql.len() && !sql.is(b.end, Kind::Semi) {
        return;
    }
    let Some(w) = b.where_.clone() else { return };
    let Some(where_kw) = b.where_kw else { return };
    let (group, others): (Vec<R>, Vec<R>) = {
        let ds = sql.disjuncts(w.clone());
        if ds.len() > 1 {
            (ds, Vec::new())
        } else {
            let conj = sql.conjuncts(w.clone());
            let Some(pos) = conj.iter().position(|c| sql.unwrap_parens(c).is_some_and(|i| sql.disjuncts(i).len() > 1)) else { return };
            let inner = sql.unwrap_parens(&conj[pos]).unwrap_or(conj[pos].clone());
            (sql.disjuncts(inner), conj.iter().enumerate().filter(|(i, _)| *i != pos).map(|(_, r)| r.clone()).collect())
        }
    };
    if !(2..=4).contains(&group.len()) {
        return;
    }
    // Different columns on each side: on one column, IN is the rewrite.
    let sets: Vec<std::collections::BTreeSet<String>> = group.iter().map(|d| sql.col_refs(d.clone()).iter().map(|c| c.column().to_lowercase()).collect()).collect();
    if sets.iter().any(|s| s.is_empty()) || sets.iter().all(|s| s == &sets[0]) {
        return;
    }
    let head = &sql.src[sql.start_of(b.select)..sql.start_of(where_kw)];
    let wrap = |r: &R| if sql.unwrap_parens(r).is_some() { sql.slice(r.clone()).to_string() } else { format!("({})", sql.slice(r.clone())) };
    let branches: Vec<String> = (0..group.len())
        .map(|i| {
            let mut conds: Vec<String> = others.iter().map(|r| sql.slice(r.clone()).to_string()).collect();
            conds.push(wrap(&group[i]));
            // Rows an earlier branch already returned (a NULL there is "not returned").
            for d in &group[..i] {
                conds.push(format!("CASE WHEN {} THEN 0 ELSE 1 END = 1", sql.slice(d.clone())));
            }
            format!("{head}WHERE {}", conds.join("\n  AND "))
        })
        .collect();
    let columns: Vec<String> = sets.iter().map(|s| s.iter().cloned().collect::<Vec<_>>().join(", ")).collect();
    out.candidate("or_to_union", &[("columns", columns.join(" · "))], vec![replace(sql, &(b.select..b.end), branches.join("\nUNION ALL\n"))]);
}

// ---------------------------------------------------------------------------
// SELECT DISTINCT with every table's key in the list: the rows are unique already.

fn redundant_distinct(cx: &Ctx, b: &Block, out: &mut Out) {
    let sql = cx.sql;
    let Some(d) = b.distinct else { return };
    if b.top || b.group_by.is_some() || cx.cat.is_none() {
        return;
    }
    let Some(scope) = Scope::new(cx, b) else { return };
    if scope.items.iter().any(|i| i.subquery.is_some()) || scope.tables.iter().any(Option::is_none) {
        return;
    }
    let mut cols: Vec<Vec<String>> = vec![Vec::new(); scope.items.len()];
    let mut star = vec![false; scope.items.len()];
    for item in sql.items(b.list.clone()) {
        let text = sql.slice(item.clone()).trim().to_string();
        if text == "*" {
            star.iter_mut().for_each(|s| *s = true);
            continue;
        }
        // `t.*`
        if item.end - item.start == 3 && sql.is(item.start + 1, Kind::Dot) && sql.op(item.start + 2, "*") {
            if let Some(i) = sql.name(item.start).and_then(|q| scope.item_of(&q)) {
                star[i] = true;
            }
            continue;
        }
        if let Some(c) = sql.col_ref(strip_alias(sql, item.clone())) {
            if let Some(i) = scope.resolve(&c) {
                cols[i].push(c.column().to_lowercase());
            }
        }
    }
    let proven = (0..scope.items.len()).all(|i| {
        star[i] || scope.tables[i].is_some_and(|t| Catalog::keys(t).iter().any(|k| k.iter().all(|c| cols[i].contains(&c.to_lowercase()))))
    });
    if !proven {
        return;
    }
    let tables: Vec<String> = scope.items.iter().filter_map(|i| i.table().map(str::to_string)).collect();
    out.candidate(
        "redundant_distinct",
        &[("tables", tables.join(", "))],
        vec![Edit { start: sql.start_of(d), end: sql.start_of(d + 1), text: String::new() }],
    );
}

// ---------------------------------------------------------------------------
// YEAR(col) = 2024 → col >= '2024-01-01' AND col < '2025-01-01' (and the date of a datetime).

#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Year,
    Day,
}

/// `YEAR(col)`, `EXTRACT(YEAR FROM col)`, `DATE(col)`, `CAST(col AS DATE)`, `col::date`, `TRUNC(col)` filling `r`.
fn date_function(sql: &Sql, r: &R, dialect: &str) -> Option<(Unit, ColRef)> {
    let (s, e) = (r.start, r.end);
    let call = |name: &str| sql.kw(s, name) && sql.close.get(s + 1).copied().flatten() == Some(e - 1);
    let mysql = dialect == "mysql";
    let mssql = dialect == "mssql";
    let pg = dialect == "postgres";
    let ora = dialect == "oracle";
    if call("year") && (mysql || mssql) {
        return Some((Unit::Year, sql.col_ref(s + 2..e - 1)?));
    }
    if call("extract") && (pg || ora || mysql) && sql.kw(s + 2, "year") && sql.kw(s + 3, "from") {
        return Some((Unit::Year, sql.col_ref(s + 4..e - 1)?));
    }
    if call("date_part") && pg && sql.is(s + 2, Kind::Str) && sql.text(s + 2).eq_ignore_ascii_case("'year'") && sql.is(s + 3, Kind::Comma) {
        return Some((Unit::Year, sql.col_ref(s + 4..e - 1)?));
    }
    if call("date") && mysql {
        return Some((Unit::Day, sql.col_ref(s + 2..e - 1)?));
    }
    if call("trunc") && ora {
        return Some((Unit::Day, sql.col_ref(s + 2..e - 1)?));
    }
    if call("cast") && (mssql || pg || mysql) && e >= s + 6 && sql.kw(e - 2, "date") && sql.kw(e - 3, "as") {
        return Some((Unit::Day, sql.col_ref(s + 2..e - 3)?));
    }
    if pg && e >= s + 3 && sql.op(e - 2, "::") && sql.kw(e - 1, "date") {
        return Some((Unit::Day, sql.col_ref(s..e - 2)?));
    }
    None
}

/// The literal on the other side: a year, or a day ('2024-03-01', DATE '…', '…'::date).
fn date_literal(sql: &Sql, r: &R, unit: Unit) -> Option<chrono::NaiveDate> {
    match unit {
        Unit::Year => {
            if r.end - r.start != 1 || !sql.is(r.start, Kind::Num) {
                return None;
            }
            let y: i32 = sql.text(r.start).parse().ok()?;
            (1..=9998).contains(&y).then(|| chrono::NaiveDate::from_ymd_opt(y, 1, 1)).flatten()
        }
        Unit::Day => {
            let s = match r.end - r.start {
                1 => r.start,
                2 if sql.kw(r.start, "date") => r.start + 1,
                3 if sql.op(r.start + 1, "::") && sql.kw(r.start + 2, "date") => r.start,
                _ => return None,
            };
            if !sql.is(s, Kind::Str) {
                return None;
            }
            let t = sql.text(s);
            let v = t.strip_prefix('\'')?.strip_suffix('\'')?;
            chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").or_else(|_| chrono::NaiveDate::parse_from_str(v, "%Y%m%d")).ok()
        }
    }
}

fn date_sql(dialect: &str, d: chrono::NaiveDate) -> String {
    match dialect {
        "mssql" => format!("'{}'", d.format("%Y%m%d")),
        "oracle" => format!("DATE '{}'", d.format("%Y-%m-%d")),
        _ => format!("'{}'", d.format("%Y-%m-%d")),
    }
}

fn function_to_range(cx: &Ctx, b: &Block, out: &mut Out) {
    let sql = cx.sql;
    if !matches!(cx.dialect, "postgres" | "mysql" | "mssql" | "oracle") || cx.cat.is_none() {
        return;
    }
    let Some(w) = b.where_.clone() else { return };
    let Some(scope) = Scope::new(cx, b) else { return };
    for c in sql.conjuncts(w) {
        let base = sql.depth[c.start];
        let eqs: Vec<usize> = c.clone().filter(|&i| sql.depth[i] == base && sql.op(i, "=")).collect();
        if eqs.len() != 1 {
            continue;
        }
        let (l, r) = (c.start..eqs[0], eqs[0] + 1..c.end);
        let Some((unit, col, lit)) = date_function(sql, &l, cx.dialect).map(|(u, col)| (u, col, r.clone())).or_else(|| date_function(sql, &r, cx.dialect).map(|(u, col)| (u, col, l.clone()))) else {
            continue;
        };
        let Some(day) = date_literal(sql, &lit, unit) else { continue };
        let Some((table, def)) = scope.column(&col) else { continue };
        let ty = def.data_type.to_lowercase();
        let zoned = ty.contains("zone") || ty.contains("offset") || ty == "timestamptz";
        if type_class(&def.data_type) != TypeClass::DateTime || (zoned && cx.dialect != "postgres") || !Catalog::leads_an_index(table, &def.name) {
            continue;
        }
        let next = match unit {
            Unit::Year => chrono::NaiveDate::from_ymd_opt(chrono::Datelike::year(&day) + 1, 1, 1),
            Unit::Day => day.succ_opt(),
        };
        let Some(next) = next else { continue };
        let name = sql.slice(col.range.clone());
        let text = format!("{name} >= {} AND {name} < {}", date_sql(cx.dialect, day), date_sql(cx.dialect, next));
        out.candidate("function_to_range", &[("column", name.to_string()), ("function", sql.slice(if lit == r { l.clone() } else { r.clone() }).to_string())], vec![replace(sql, &c, text)]);
    }
}

// ---------------------------------------------------------------------------
// SELECT * → its columns (a note: only the user knows which ones the code needs).

fn select_star(cx: &Ctx, b: &Block, out: &mut Out) {
    let sql = cx.sql;
    if b.depth != 0 || b.list.end - b.list.start != 1 || !sql.op(b.list.start, "*") {
        return;
    }
    let Some(scope) = Scope::new(cx, b) else { return };
    if scope.items.len() != 1 {
        return;
    }
    let Some(t) = scope.tables[0] else { return };
    if t.columns.is_empty() {
        return;
    }
    let cols: Vec<String> = t.columns.iter().map(|c| ident(cx.dialect, &c.name)).collect();
    let mut edits = vec![replace(sql, &b.list, cols.join(", "))];
    let text = apply(sql.src, std::mem::take(&mut edits));
    out.note("select_star", &[("table", t.name.clone()), ("count", t.columns.len().to_string())], Some(text));
}

/// A name as the dialect writes it, quoted only when it needs it.
pub fn ident(dialect: &str, name: &str) -> String {
    let simple = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !super::parse::is_reserved(name);
    // PostgreSQL folds unquoted names to lower case, Oracle to upper case.
    let folds = match dialect {
        "postgres" => name.chars().any(|c| c.is_ascii_uppercase()),
        "oracle" => name.chars().any(|c| c.is_ascii_lowercase()),
        _ => false,
    };
    if simple && !folds {
        return name.to_string();
    }
    let q = match dialect {
        "mssql" | "sybase" | "access" => dbine_driver::sql::Quote::Bracket,
        "mysql" | "bigquery" | "hive" | "clickhouse" | "sparksql" | "databricks" => dbine_driver::sql::Quote::Backtick,
        _ => dbine_driver::sql::Quote::Double,
    };
    dbine_driver::sql::quote_ident(q, name)
}

// ---------------------------------------------------------------------------
// (SELECT COUNT(*) FROM …) > 0 → EXISTS (SELECT 1 FROM …)

fn count_to_exists(cx: &Ctx, out: &mut Out) {
    let sql = cx.sql;
    let arith = |i: usize| sql.is(i, Kind::Op) && matches!(sql.text(i), "+" | "-" | "*" | "/" | "%" | "||" | "::");
    for open in 0..sql.len() {
        let Some(s) = sql.subquery(open) else { continue };
        let Some(close) = sql.close[open] else { continue };
        if !plain(s) || s.distinct.is_some() {
            continue;
        }
        let list = strip_alias(sql, s.list.clone());
        let l = list.start;
        let counts = list.end - l == 4
            && sql.kw(l, "count")
            && sql.is(l + 1, Kind::LParen)
            && (sql.op(l + 2, "*") || (sql.is(l + 2, Kind::Num) && sql.text(l + 2) == "1"))
            && sql.is(l + 3, Kind::RParen);
        if !counts {
            continue;
        }
        let num = |i: usize, v: &str| sql.is(i, Kind::Num) && sql.text(i) == v;
        // `(…) > 0` and the other ways round.
        let after = |op: &str, v: &str| sql.op(close + 1, op) && num(close + 2, v);
        let before = |op: &str, v: &str| open >= 2 && sql.op(open - 1, op) && num(open - 2, v);
        let (exists, range) = if after(">", "0") || after(">=", "1") || after("<>", "0") || after("!=", "0") {
            (true, open..close + 3)
        } else if after("=", "0") || after("<", "1") || after("<=", "0") {
            (false, open..close + 3)
        } else if before("<", "0") || before("<=", "1") || before("<>", "0") || before("!=", "0") {
            (true, open - 2..close + 1)
        } else if before("=", "0") || before(">", "1") || before(">=", "0") {
            (false, open - 2..close + 1)
        } else {
            continue;
        };
        // Nothing binds tighter around it (`… > 0 + x`).
        if arith(range.end) || (range.start > 0 && arith(range.start - 1)) {
            continue;
        }
        let rest = sql.slice(s.from_kw.unwrap_or(s.select)..s.end);
        let text = format!("{}EXISTS (SELECT 1 {rest})", if exists { "" } else { "NOT " });
        out.candidate("count_to_exists", &[], vec![replace(sql, &range, text)]);
    }
}

// ---------------------------------------------------------------------------
// Comparisons that make the engine convert the column (notes only: the
// rewrite isn't equivalent for every value).

fn implicit_conversions(cx: &Ctx, b: &Block, out: &mut Out) {
    let sql = cx.sql;
    if cx.cat.is_none() {
        return;
    }
    let Some(scope) = Scope::new(cx, b) else { return };
    let mut conds: Vec<R> = b.where_.iter().cloned().collect();
    conds.extend(scope.items.iter().filter_map(|i| i.on.clone()));
    for cond in conds {
        for c in sql.conjuncts(cond) {
            for d in sql.disjuncts(c) {
                let base = sql.depth[d.start];
                let ops: Vec<usize> = d.clone().filter(|&i| sql.depth[i] == base && sql.is(i, Kind::Op) && matches!(sql.text(i), "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=")).collect();
                let pairs: Vec<(R, R)> = if ops.len() == 1 {
                    vec![(d.start..ops[0], ops[0] + 1..d.end), (ops[0] + 1..d.end, d.start..ops[0])]
                } else if let Some(k) = d.clone().find(|&i| sql.depth[i] == base && sql.kw(i, "in")).filter(|k| sql.close.get(k + 1).copied().flatten() == Some(d.end - 1)) {
                    // `col IN (1, 2)`: each value.
                    let lhs = d.start..if k > d.start && sql.kw(k - 1, "not") { k - 1 } else { k };
                    sql.items(k + 2..d.end - 1).into_iter().map(|v| (lhs.clone(), v)).collect()
                } else {
                    Vec::new()
                };
                for (colr, litr) in pairs {
                    let Some(col) = sql.col_ref(colr) else { continue };
                    if litr.end - litr.start != 1 {
                        continue;
                    }
                    let Some((_, def)) = scope.column(&col) else { continue };
                    let lit = litr.start;
                    let name = sql.slice(col.range.clone()).to_string();
                    if type_class(&def.data_type) == TypeClass::Text && sql.is(lit, Kind::Num) {
                        out.note("text_vs_number", &[("column", name), ("type", def.data_type.clone()), ("value", sql.text(lit).to_string())], None);
                    } else if cx.dialect == "mssql" && sql.is(lit, Kind::Str) && sql.text(lit).starts_with(['N', 'n']) {
                        let ty = def.data_type.to_lowercase();
                        if ty.starts_with("varchar") || ty.starts_with("char") || ty == "text" {
                            out.note("nvarchar_literal", &[("column", name), ("type", def.data_type.clone())], None);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizer::catalog::tests::table;

    fn rules(sql: &str, dialect: &str, tables: Option<&[TableSchema]>) -> Vec<(String, String)> {
        analyze(sql, dialect, tables).0.into_iter().map(|c| (c.rule.unwrap(), c.sql)).collect()
    }

    fn only(sql: &str, dialect: &str, tables: Option<&[TableSchema]>, rule: &str) -> Vec<String> {
        rules(sql, dialect, tables).into_iter().filter(|(r, _)| r == rule).map(|(_, s)| s).collect()
    }

    fn notes(sql: &str, dialect: &str, tables: &[TableSchema]) -> Vec<(String, BTreeMap<String, String>)> {
        analyze(sql, dialect, Some(tables)).1.into_iter().map(|n| (n.rule, n.params)).collect()
    }

    fn shop() -> Vec<TableSchema> {
        vec![
            table(None, "orders", &[("id", "int", false), ("customer_id", "int", false), ("total", "decimal(10,2)", true), ("created", "datetime", false), ("code", "varchar(20)", true), ("note", "nvarchar(100)", true)], &["id"], &[(&["created"], false), (&["code"], false)]),
            table(None, "customers", &[("id", "int", false), ("name", "varchar(50)", false), ("email", "varchar(80)", true), ("region", "int", true)], &["id"], &[(&["email"], true)]),
            table(None, "items", &[("order_id", "int", false), ("line", "int", false), ("qty", "int", true), ("sku", "varchar(10)", true)], &["order_id", "line"], &[]),
        ]
    }

    // -- IN ↔ EXISTS -----------------------------------------------------------

    #[test]
    fn in_becomes_exists() {
        let out = only("SELECT * FROM customers c WHERE c.id IN (SELECT o.customer_id FROM orders o WHERE o.total > 100)", "postgres", None, "in_to_exists");
        assert_eq!(out, vec!["SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.total > 100 AND o.customer_id = c.id)"]);
    }

    #[test]
    fn in_without_where_and_with_or_inside() {
        let out = only("SELECT * FROM customers c WHERE c.id IN (SELECT customer_id FROM orders)", "mysql", None, "in_to_exists");
        assert_eq!(out, vec!["SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders WHERE customer_id = c.id)"]);
        let out = only("SELECT * FROM customers c WHERE c.id IN (SELECT o.customer_id FROM orders o WHERE o.total > 1 OR o.code = 'x') AND c.region = 2", "mysql", None, "in_to_exists");
        assert_eq!(out, vec!["SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE (o.total > 1 OR o.code = 'x') AND o.customer_id = c.id) AND c.region = 2"]);
    }

    #[test]
    fn in_left_alone_when_unsafe() {
        // Unqualified outer column with no structure to prove where it binds.
        assert!(only("SELECT * FROM customers WHERE id IN (SELECT customer_id FROM orders)", "postgres", None, "in_to_exists").is_empty());
        // The outer alias is reused inside.
        assert!(only("SELECT * FROM customers o WHERE o.id IN (SELECT o.customer_id FROM orders o)", "postgres", None, "in_to_exists").is_empty());
        // Under OR: NULL and FALSE differ there.
        assert!(only("SELECT * FROM customers c WHERE c.id IN (SELECT customer_id FROM orders) OR c.region = 1", "postgres", None, "in_to_exists").is_empty());
        // Grouped, limited, aggregated or a UNION inside.
        assert!(only("SELECT * FROM customers c WHERE c.id IN (SELECT customer_id FROM orders GROUP BY customer_id HAVING COUNT(*) > 2)", "postgres", None, "in_to_exists").is_empty());
        assert!(only("SELECT * FROM customers c WHERE c.id IN (SELECT customer_id FROM orders LIMIT 5)", "postgres", None, "in_to_exists").is_empty());
        assert!(only("SELECT * FROM customers c WHERE c.id IN (SELECT MAX(customer_id) FROM orders)", "postgres", None, "in_to_exists").is_empty());
        assert!(only("SELECT * FROM customers c WHERE c.id IN (SELECT customer_id FROM orders UNION SELECT 1)", "postgres", None, "in_to_exists").is_empty());
        // A list of values, not a subquery.
        assert!(only("SELECT * FROM customers c WHERE c.id IN (1, 2)", "postgres", None, "in_to_exists").is_empty());
    }

    #[test]
    fn unqualified_in_with_structure_that_proves_it() {
        let t = shop();
        let out = only("SELECT name FROM customers WHERE region IN (SELECT qty FROM items)", "postgres", Some(&t), "in_to_exists");
        assert_eq!(out, vec!["SELECT name FROM customers WHERE EXISTS (SELECT 1 FROM items WHERE qty = region)"]);
        // `id` exists in orders too: inside the subquery it would mean orders.id.
        assert!(only("SELECT name FROM customers WHERE id IN (SELECT customer_id FROM orders)", "postgres", Some(&t), "in_to_exists").is_empty());
    }

    #[test]
    fn not_in_only_when_neither_side_is_nullable() {
        let t = shop();
        let out = only("SELECT * FROM customers c WHERE c.id NOT IN (SELECT o.customer_id FROM orders o)", "postgres", Some(&t), "not_in_to_not_exists");
        assert_eq!(out, vec!["SELECT * FROM customers c WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id)"]);
        assert!(only("SELECT * FROM customers c WHERE c.region NOT IN (SELECT i.qty FROM items i)", "postgres", Some(&t), "not_in_to_not_exists").is_empty());
        let n = notes("SELECT * FROM customers c WHERE c.region NOT IN (SELECT i.qty FROM items i)", "postgres", &t);
        assert!(n.iter().any(|(r, p)| r == "not_in_nullable" && p["column"] == "c.region"));
        // Without the structure, nothing at all.
        assert!(only("SELECT * FROM customers c WHERE c.id NOT IN (SELECT o.customer_id FROM orders o)", "postgres", None, "not_in_to_not_exists").is_empty());
    }

    #[test]
    fn exists_becomes_in() {
        let out = only("SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.total > 10)", "mysql", None, "exists_to_in");
        assert_eq!(out, vec!["SELECT * FROM customers c WHERE c.id IN (SELECT o.customer_id FROM orders o WHERE o.total > 10)"]);
        let out = only("SELECT * FROM customers c WHERE EXISTS (SELECT * FROM orders o WHERE c.id = o.customer_id)", "mysql", None, "exists_to_in");
        assert_eq!(out, vec!["SELECT * FROM customers c WHERE c.id IN (SELECT o.customer_id FROM orders o)"]);
    }

    #[test]
    fn exists_left_alone_when_unsafe() {
        // NOT EXISTS → NOT IN breaks with NULLs.
        assert!(only("SELECT * FROM customers c WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id)", "mysql", None, "exists_to_in").is_empty());
        // Two correlations, or another reference to the outer query.
        assert!(only("SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.code = c.name)", "mysql", None, "exists_to_in").is_empty());
        assert!(only("SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.total > c.region)", "mysql", None, "exists_to_in").is_empty());
        // An aggregate makes EXISTS always true.
        assert!(only("SELECT * FROM customers c WHERE EXISTS (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id)", "mysql", None, "exists_to_in").is_empty());
        // A join inside.
        assert!(only("SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o JOIN items i ON i.order_id = o.id WHERE o.customer_id = c.id)", "mysql", None, "exists_to_in").is_empty());
    }

    // -- scalar subquery → LEFT JOIN ---------------------------------------------

    #[test]
    fn scalar_count_becomes_left_join() {
        let out = only("SELECT c.id, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id) AS n FROM customers c", "postgres", None, "scalar_to_join");
        assert_eq!(
            out,
            vec!["SELECT c.id, COALESCE(sq1.v, 0) AS n FROM customers c\nLEFT JOIN (SELECT o.customer_id AS k1, COUNT(*) AS v FROM orders o GROUP BY o.customer_id) sq1 ON sq1.k1 = c.id"]
        );
    }

    #[test]
    fn scalar_sum_with_filter_and_two_subqueries() {
        let sql = "SELECT c.id, (SELECT SUM(o.total) FROM orders o WHERE c.id = o.customer_id AND o.total > 0) total, (SELECT MAX(o.created) FROM orders o WHERE o.customer_id = c.id) AS last FROM customers c WHERE c.region = 1";
        let out = only(sql, "mysql", None, "scalar_to_join");
        assert_eq!(
            out,
            vec!["SELECT c.id, sq1.v total, sq2.v AS last FROM customers c\nLEFT JOIN (SELECT o.customer_id AS k1, SUM(o.total) AS v FROM orders o WHERE o.total > 0 GROUP BY o.customer_id) sq1 ON sq1.k1 = c.id\nLEFT JOIN (SELECT o.customer_id AS k1, MAX(o.created) AS v FROM orders o GROUP BY o.customer_id) sq2 ON sq2.k1 = c.id WHERE c.region = 1"]
        );
    }

    #[test]
    fn scalar_left_alone_when_unsafe() {
        // No alias: the column's name would change.
        assert!(only("SELECT c.id, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id) FROM customers c", "postgres", None, "scalar_to_join").is_empty());
        // Not an aggregate: it may return several rows.
        assert!(only("SELECT c.id, (SELECT o.total FROM orders o WHERE o.customer_id = c.id) AS t FROM customers c", "postgres", None, "scalar_to_join").is_empty());
        // Comma joins: the ON couldn't see the first table (MySQL).
        assert!(only("SELECT c.id, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id) AS n FROM customers c, items i", "postgres", None, "scalar_to_join").is_empty());
        // The outer query groups.
        assert!(only("SELECT c.region, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.region) AS n FROM customers c GROUP BY c.region", "postgres", None, "scalar_to_join").is_empty());
        // Another reference to the outer query besides the correlation.
        assert!(only("SELECT c.id, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id AND o.total > c.region) AS n FROM customers c", "postgres", None, "scalar_to_join").is_empty());
        // Unqualified inner column with no structure: it might be the outer one.
        assert!(only("SELECT c.id, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id AND total > 1) AS n FROM customers c", "postgres", None, "scalar_to_join").is_empty());
        // Uncorrelated.
        assert!(only("SELECT c.id, (SELECT COUNT(*) FROM orders o) AS n FROM customers c", "postgres", None, "scalar_to_join").is_empty());
    }

    #[test]
    fn scalar_with_structure_allows_unqualified_inner_columns() {
        let t = shop();
        let out = only("SELECT c.id, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id AND total > 1) AS n FROM customers c", "postgres", Some(&t), "scalar_to_join");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("WHERE total > 1 GROUP BY o.customer_id"));
    }

    // -- OR → UNION ALL -----------------------------------------------------------

    #[test]
    fn or_on_different_columns_becomes_union_all() {
        let out = only("SELECT id, name FROM customers WHERE email = 'a@b' OR region = 3", "postgres", None, "or_to_union");
        assert_eq!(
            out,
            vec!["SELECT id, name FROM customers WHERE (email = 'a@b')\nUNION ALL\nSELECT id, name FROM customers WHERE (region = 3)\n  AND CASE WHEN email = 'a@b' THEN 0 ELSE 1 END = 1"]
        );
    }

    #[test]
    fn or_inside_an_and_keeps_the_rest() {
        let out = only("SELECT * FROM orders WHERE total > 5 AND (code = 'x' OR customer_id = 2);", "mysql", None, "or_to_union");
        assert_eq!(
            out,
            vec!["SELECT * FROM orders WHERE total > 5\n  AND (code = 'x')\nUNION ALL\nSELECT * FROM orders WHERE total > 5\n  AND (customer_id = 2)\n  AND CASE WHEN code = 'x' THEN 0 ELSE 1 END = 1;"]
        );
    }

    #[test]
    fn or_left_alone_when_unsafe() {
        // Same column: IN is the rewrite, not UNION.
        assert!(only("SELECT * FROM t WHERE a = 1 OR a = 2", "postgres", None, "or_to_union").is_empty());
        // DISTINCT, ORDER BY, LIMIT, TOP, aggregates, GROUP BY.
        assert!(only("SELECT DISTINCT a FROM t WHERE a = 1 OR b = 2", "postgres", None, "or_to_union").is_empty());
        assert!(only("SELECT a FROM t WHERE a = 1 OR b = 2 ORDER BY a", "postgres", None, "or_to_union").is_empty());
        assert!(only("SELECT a FROM t WHERE a = 1 OR b = 2 LIMIT 3", "postgres", None, "or_to_union").is_empty());
        assert!(only("SELECT TOP 3 a FROM t WHERE a = 1 OR b = 2", "mssql", None, "or_to_union").is_empty());
        assert!(only("SELECT COUNT(*) FROM t WHERE a = 1 OR b = 2", "postgres", None, "or_to_union").is_empty());
        assert!(only("SELECT a FROM t WHERE a = 1 OR b = 2 GROUP BY a", "postgres", None, "or_to_union").is_empty());
        // Inside a subquery, or part of a UNION already.
        assert!(only("SELECT * FROM (SELECT a FROM t WHERE a = 1 OR b = 2) x", "postgres", None, "or_to_union").is_empty());
        assert!(only("SELECT a FROM t WHERE a = 1 OR b = 2 UNION SELECT 1", "postgres", None, "or_to_union").is_empty());
        // Volatile functions and window functions.
        assert!(only("SELECT a, RANDOM() FROM t WHERE a = 1 OR b = 2", "postgres", None, "or_to_union").is_empty());
        assert!(only("SELECT a, ROW_NUMBER() OVER (ORDER BY a) FROM t WHERE a = 1 OR b = 2", "postgres", None, "or_to_union").is_empty());
    }

    // -- redundant DISTINCT -------------------------------------------------------

    #[test]
    fn distinct_over_a_key_is_dropped() {
        let t = shop();
        assert_eq!(only("SELECT DISTINCT id, name FROM customers", "postgres", Some(&t), "redundant_distinct"), vec!["SELECT id, name FROM customers"]);
        assert_eq!(only("SELECT DISTINCT c.email, c.name FROM customers c", "postgres", Some(&t), "redundant_distinct").len(), 0, "email is unique but nullable");
        assert_eq!(only("SELECT DISTINCT * FROM items", "postgres", Some(&t), "redundant_distinct"), vec!["SELECT * FROM items"]);
        let out = only("SELECT DISTINCT o.id, i.order_id, i.line FROM orders o JOIN items i ON i.order_id = o.id", "postgres", Some(&t), "redundant_distinct");
        assert_eq!(out, vec!["SELECT o.id, i.order_id, i.line FROM orders o JOIN items i ON i.order_id = o.id"]);
    }

    #[test]
    fn distinct_kept_without_proof() {
        let t = shop();
        assert!(only("SELECT DISTINCT name FROM customers", "postgres", Some(&t), "redundant_distinct").is_empty());
        assert!(only("SELECT DISTINCT o.id, i.order_id FROM orders o JOIN items i ON i.order_id = o.id", "postgres", Some(&t), "redundant_distinct").is_empty());
        assert!(only("SELECT DISTINCT id FROM customers", "postgres", None, "redundant_distinct").is_empty());
        assert!(only("SELECT DISTINCT ON (id) id FROM customers", "postgres", Some(&t), "redundant_distinct").is_empty());
        assert!(only("SELECT DISTINCT x.id FROM (SELECT id FROM customers) x", "postgres", Some(&t), "redundant_distinct").is_empty());
        assert!(only("SELECT DISTINCT id FROM nowhere", "postgres", Some(&t), "redundant_distinct").is_empty());
    }

    // -- functions on indexed columns → ranges --------------------------------------

    #[test]
    fn year_becomes_a_range_per_dialect() {
        let t = shop();
        assert_eq!(only("SELECT * FROM orders WHERE YEAR(created) = 2024", "mysql", Some(&t), "function_to_range"), vec!["SELECT * FROM orders WHERE created >= '2024-01-01' AND created < '2025-01-01'"]);
        assert_eq!(only("SELECT * FROM orders o WHERE YEAR(o.created) = 2024 AND o.total > 1", "mssql", Some(&t), "function_to_range"), vec!["SELECT * FROM orders o WHERE o.created >= '20240101' AND o.created < '20250101' AND o.total > 1"]);
        let mut pg = shop();
        pg[0].columns[3].data_type = "timestamp without time zone".into();
        assert_eq!(only("SELECT * FROM orders WHERE 2024 = EXTRACT(YEAR FROM created)", "postgres", Some(&pg), "function_to_range"), vec!["SELECT * FROM orders WHERE created >= '2024-01-01' AND created < '2025-01-01'"]);
        assert_eq!(only("SELECT * FROM orders WHERE date_part('year', created) = 1999", "postgres", Some(&pg), "function_to_range"), vec!["SELECT * FROM orders WHERE created >= '1999-01-01' AND created < '2000-01-01'"]);
        let mut ora = shop();
        ora[0].columns[3].data_type = "DATE".into();
        assert_eq!(only("SELECT * FROM orders WHERE TRUNC(created) = DATE '2024-02-29'", "oracle", Some(&ora), "function_to_range"), vec!["SELECT * FROM orders WHERE created >= DATE '2024-02-29' AND created < DATE '2024-03-01'"]);
    }

    #[test]
    fn date_of_a_datetime_becomes_a_range() {
        let t = shop();
        assert_eq!(only("SELECT * FROM orders WHERE DATE(created) = '2024-12-31'", "mysql", Some(&t), "function_to_range"), vec!["SELECT * FROM orders WHERE created >= '2024-12-31' AND created < '2025-01-01'"]);
        assert_eq!(only("SELECT * FROM orders WHERE CAST(created AS DATE) = '2024-03-01'", "mssql", Some(&t), "function_to_range"), vec!["SELECT * FROM orders WHERE created >= '20240301' AND created < '20240302'"]);
        let mut pg = shop();
        pg[0].columns[3].data_type = "timestamp with time zone".into();
        assert_eq!(only("SELECT * FROM orders WHERE created::date = '2024-03-01'", "postgres", Some(&pg), "function_to_range"), vec!["SELECT * FROM orders WHERE created >= '2024-03-01' AND created < '2024-03-02'"]);
    }

    #[test]
    fn functions_left_alone_when_unsafe() {
        let t = shop();
        // Not indexed, not a date, unknown table, or no structure at all.
        assert!(only("SELECT * FROM orders WHERE YEAR(total) = 2024", "mysql", Some(&t), "function_to_range").is_empty());
        let mut noidx = shop();
        noidx[0].indexes.clear();
        assert!(only("SELECT * FROM orders WHERE YEAR(created) = 2024", "mysql", Some(&noidx), "function_to_range").is_empty());
        assert!(only("SELECT * FROM orders WHERE YEAR(created) = 2024", "mysql", None, "function_to_range").is_empty());
        // Under OR, a non-literal, or an engine whose dates are text.
        assert!(only("SELECT * FROM orders WHERE YEAR(created) = 2024 OR id = 1", "mysql", Some(&t), "function_to_range").is_empty());
        assert!(only("SELECT * FROM orders WHERE YEAR(created) = YEAR(NOW())", "mysql", Some(&t), "function_to_range").is_empty());
        assert!(only("SELECT * FROM orders WHERE strftime('%Y', created) = '2024'", "sqlite", Some(&t), "function_to_range").is_empty());
        // A zoned type where the engine extracts in UTC (SQL Server's datetimeoffset).
        let mut dto = shop();
        dto[0].columns[3].data_type = "datetimeoffset".into();
        assert!(only("SELECT * FROM orders WHERE YEAR(created) = 2024", "mssql", Some(&dto), "function_to_range").is_empty());
        // MySQL's YEAR isn't SQL Server's EXTRACT, and an invalid date.
        assert!(only("SELECT * FROM orders WHERE DATE(created) = '2024-02-30'", "mysql", Some(&t), "function_to_range").is_empty());
    }

    // -- COUNT(*) > 0 → EXISTS ------------------------------------------------------

    #[test]
    fn count_greater_than_zero_becomes_exists() {
        let out = only("SELECT * FROM customers c WHERE (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id) > 0", "postgres", None, "count_to_exists");
        assert_eq!(out, vec!["SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id)"]);
        let out = only("IF (SELECT COUNT(1) FROM orders WHERE total > 9) = 0 PRINT 'none'", "mssql", None, "count_to_exists");
        assert_eq!(out, vec!["IF NOT EXISTS (SELECT 1 FROM orders WHERE total > 9) PRINT 'none'"]);
        let out = only("SELECT CASE WHEN 0 < (SELECT COUNT(*) FROM orders) THEN 1 END", "postgres", None, "count_to_exists");
        assert_eq!(out, vec!["SELECT CASE WHEN EXISTS (SELECT 1 FROM orders) THEN 1 END"]);
    }

    #[test]
    fn count_left_alone_when_unsafe() {
        assert!(only("SELECT * FROM c WHERE (SELECT COUNT(*) FROM o) > 1", "postgres", None, "count_to_exists").is_empty());
        assert!(only("SELECT * FROM c WHERE (SELECT COUNT(x) FROM o) > 0", "postgres", None, "count_to_exists").is_empty());
        assert!(only("SELECT * FROM c WHERE (SELECT COUNT(*) FROM o GROUP BY k) > 0", "postgres", None, "count_to_exists").is_empty());
        assert!(only("SELECT * FROM c WHERE (SELECT COUNT(*) FROM o) > 0 + c.x", "postgres", None, "count_to_exists").is_empty());
        assert!(only("SELECT (SELECT COUNT(*) FROM o) AS n FROM c", "postgres", None, "count_to_exists").is_empty());
    }

    // -- notes --------------------------------------------------------------------------

    #[test]
    fn select_star_lists_the_columns() {
        let t = shop();
        let r = analyze("SELECT * FROM customers WHERE id = 1", "postgres", Some(&t));
        let n = r.1.iter().find(|n| n.rule == "select_star").unwrap();
        assert_eq!(n.sql.as_deref(), Some("SELECT id, name, email, region FROM customers WHERE id = 1"));
        let mut t2 = shop();
        t2[1].columns[1].name = "Name".into();
        let r = analyze("SELECT * FROM customers", "postgres", Some(&t2));
        assert_eq!(r.1[0].sql.as_deref(), Some("SELECT id, \"Name\", email, region FROM customers"));
        assert!(analyze("SELECT * FROM customers c JOIN orders o ON o.customer_id = c.id", "postgres", Some(&t)).1.iter().all(|n| n.rule != "select_star"));
    }

    #[test]
    fn implicit_conversions_are_noted() {
        let t = shop();
        let n = notes("SELECT * FROM orders WHERE code = 123", "mysql", &t);
        assert!(n.iter().any(|(r, p)| r == "text_vs_number" && p["column"] == "code" && p["value"] == "123"));
        let n = notes("SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.name = N'Ana' OR o.code IN (1, 2)", "mssql", &t);
        assert!(n.iter().any(|(r, p)| r == "nvarchar_literal" && p["column"] == "c.name"));
        assert!(n.iter().any(|(r, p)| r == "text_vs_number" && p["column"] == "o.code"));
        // Text against text, numbers against numbers: nothing.
        assert!(notes("SELECT * FROM orders WHERE code = '123' AND total = 5 AND note = N'x'", "mssql", &t).iter().all(|(r, _)| r != "text_vs_number" && r != "nvarchar_literal"));
    }

    #[test]
    fn dialects_without_rules_and_untouched_text() {
        assert!(analyze("SELECT * FROM c WHERE (SELECT COUNT(*) FROM o) > 0", "cosmos", None).0.is_empty());
        // The rest of the text stays as written (comments, spacing).
        let out = only("-- top\nSELECT *\n  FROM customers c /* x */\n WHERE (SELECT COUNT(*) FROM orders) > 0;\nSELECT 2;", "postgres", None, "count_to_exists");
        assert_eq!(out, vec!["-- top\nSELECT *\n  FROM customers c /* x */\n WHERE EXISTS (SELECT 1 FROM orders);\nSELECT 2;"]);
    }
}
