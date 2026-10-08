//! Index suggestions from the execution plan: the engine's own missing-index
//! hints (SQL Server), and full scans of a table the query filters or joins
//! by columns no index starts with (every engine whose plan names its
//! scans). The CREATE INDEX script is in the driver's language and is only
//! opened in a query for the user; nothing here runs it.

use super::catalog::Catalog;
use super::lex::{unquote, Flavor, Kind};
use super::parse::{ColRef, Sql, R};
use dbine_driver::{IndexDef, Plan, PlanNode, TableSchema};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IndexHint {
    /// `missing_index`: the engine's own suggestion; `full_scan`: a scan the
    /// filters could avoid with this index; `scan`: a full scan whose
    /// filter columns aren't known.
    pub reason: String,
    pub table: String,
    pub columns: Vec<String>,
    pub est_rows: Option<f64>,
    /// The engine's estimated improvement, in %.
    pub impact: Option<f64>,
    /// The script, to open in a query.
    pub script: Option<String>,
}

/// A warning the plan carries (spills, conversions, scans…), for the "Plan" notes.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlanWarning {
    pub op: String,
    pub object: Option<String>,
    pub text: String,
}

const MISSING: &str = "Índice faltante";

fn walk<'a>(n: &'a PlanNode, out: &mut Vec<&'a PlanNode>) {
    out.push(n);
    for c in &n.children {
        walk(c, out);
    }
}

pub fn nodes(plans: &[Plan]) -> Vec<&PlanNode> {
    let mut out = Vec::new();
    for p in plans {
        walk(&p.root, &mut out);
    }
    out
}

/// The operator reads a whole table / collection.
pub fn full_scan(n: &PlanNode) -> bool {
    let op = n.op.to_lowercase().replace('_', " ");
    let detail = n.detail.to_lowercase();
    op.contains("seq scan")
        || op == "table scan"
        || op.contains("full table scan")
        || op.contains("full scan")
        || op == "collscan"
        || op.contains("clustered index scan")
        || (op == "table access" && detail.starts_with("full"))
        // SQLite: `SCAN t` (with an index it says `USING … INDEX`).
        || (op == "scan" && !detail.contains("index") && n.object.is_some())
        || op.contains("primaryscan")
        || op.contains("allnodesscan")
        || op.contains("nodebylabelscan")
}

/// The scanned object's own name (no schema, database or quotes).
fn object_name(object: &str, collection: bool) -> String {
    let o = object.trim();
    // MongoDB: `db.collection` (the collection may have dots).
    if collection {
        return o.split_once('.').map_or(o, |(_, c)| c).to_string();
    }
    let last = o.rsplit('.').next().unwrap_or(o);
    unquote(last.trim())
}

/// Everything the plans say about indexes.
pub struct Found {
    pub hints: Vec<IndexHint>,
    pub warnings: Vec<PlanWarning>,
}

/// Builds the CREATE INDEX script of a table in the driver's language.
pub type Ddl<'a> = &'a dyn Fn(&TableSchema) -> Option<String>;

/// `non_sql`: the query isn't SQL (MongoDB, Cypher…): only MongoDB's
/// COLLSCAN filters give columns.
pub fn from_plans(plans: &[Plan], src: &str, dialect: &str, non_sql: bool, tables: Option<&[TableSchema]>, ddl: Ddl) -> Found {
    let all = nodes(plans);
    let mut hints: Vec<IndexHint> = Vec::new();
    let mut warnings: Vec<PlanWarning> = Vec::new();
    for n in &all {
        for w in &n.warnings {
            if let Some(h) = missing_index(w) {
                hints.push(h);
            } else {
                let pw = PlanWarning { op: n.op.clone(), object: n.object.clone(), text: w.clone() };
                if !warnings.contains(&pw) {
                    warnings.push(pw);
                }
            }
        }
    }
    let engine_said: Vec<String> = hints.iter().map(|h| h.table.to_lowercase()).collect();
    let catalog = tables.map(|t| Catalog::new(t, dialect));
    let sql = (!non_sql).then(|| Sql::parse(src, Flavor::for_dialect(dialect)));
    for n in all.iter().filter(|n| full_scan(n)) {
        let Some(object) = n.object.as_deref() else { continue };
        let mut name = object_name(object, n.op == "COLLSCAN");
        // SQL Server names the index too (`dbo.t.PK_t`): the table is the part before it.
        if n.op.to_lowercase().contains("index") {
            let parts: Vec<&str> = object.split('.').collect();
            if parts.len() >= 2 {
                name = unquote(parts[parts.len() - 2].trim());
            }
        }
        if name.is_empty() || engine_said.contains(&name.to_lowercase()) {
            continue;
        }
        let (table, columns) = if n.op == "COLLSCAN" {
            (Some(TableSchema { kind: dbine_driver::kinds::COLLECTION.into(), name: name.clone(), ..Default::default() }), mongo_fields(n, &all))
        } else {
            match sql.as_ref().and_then(|s| sql_columns(s, &name, catalog.as_ref())) {
                Some((t, cols)) => (Some(t), cols),
                None => (None, Vec::new()),
            }
        };
        let hint = if columns.is_empty() {
            // A SQL query that doesn't filter this table reads it whole anyway.
            if sql.is_some() && !unknown_language(dialect) {
                continue;
            }
            IndexHint { reason: "scan".into(), table: name.clone(), columns, est_rows: n.est_rows, impact: None, script: None }
        } else {
            let mut t = table.unwrap_or_default();
            // An index already starts with the first column: the engine chose the scan.
            if catalog.as_ref().and_then(|c| c.table(&[t.schema.clone().unwrap_or_default(), t.name.clone()].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>())).is_some_and(|ct| Catalog::leads_an_index(ct, &columns[0])) {
                continue;
            }
            t.indexes = vec![IndexDef { name: index_name(&t.name, &columns), columns: columns.clone(), ..Default::default() }];
            IndexHint { reason: "full_scan".into(), table: t.name.clone(), columns, est_rows: n.est_rows, impact: None, script: ddl(&t) }
        };
        if !hints.iter().any(|h| h.table.eq_ignore_ascii_case(&hint.table) && h.columns == hint.columns) {
            hints.push(hint);
        }
    }
    Found { hints, warnings }
}

/// Engines whose plan names scans but whose query isn't SQL the parser reads.
fn unknown_language(dialect: &str) -> bool {
    matches!(dialect, "n1ql" | "cosmos" | "partiql" | "")
}

/// SQL Server's "Índice faltante (impacto 87.5%): CREATE NONCLUSTERED INDEX … ON [dbo].[t] ([a], [b]) INCLUDE ([c])".
fn missing_index(w: &str) -> Option<IndexHint> {
    let rest = w.strip_prefix(MISSING)?;
    let impact = rest.split_once("impacto ").and_then(|(_, r)| r.split('%').next()).and_then(|v| v.trim().parse::<f64>().ok());
    let script = rest.split_once(": ").map(|(_, s)| s.trim().to_string())?;
    let (_, on) = script.split_once(" ON ")?;
    let (table, cols) = on.split_once(" (")?;
    let cols = cols.split(')').next().unwrap_or("");
    let table = object_name(table, false);
    let columns: Vec<String> = cols.split(',').map(|c| unquote(c.trim())).filter(|c| !c.is_empty()).collect();
    let script = script.replace("[IX_sugerido]", &format!("[{}]", index_name(&table, &columns)));
    Some(IndexHint { reason: "missing_index".into(), table, columns, est_rows: None, impact, script: Some(script) })
}

pub fn index_name(table: &str, columns: &[String]) -> String {
    let raw = format!("ix_{}_{}", table, columns.join("_"));
    let mut s: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    s.truncate(30);
    s.trim_end_matches('_').to_string()
}

/// The columns a query filters or joins `table` by: equalities first, then
/// one range. And the table as the catalog knows it (or as the query names it).
fn sql_columns(sql: &Sql, table: &str, catalog: Option<&Catalog>) -> Option<(TableSchema, Vec<String>)> {
    for b in &sql.blocks {
        let Some(items) = sql.from_items(b) else { continue };
        let Some(idx) = items.iter().position(|i| i.label().is_some_and(|l| l.eq_ignore_ascii_case(table)) || i.table().is_some_and(|t| t.eq_ignore_ascii_case(table))) else {
            continue;
        };
        let item = &items[idx];
        let Some(parts) = item.name.clone() else { continue };
        let known = catalog.and_then(|c| c.table(&parts));
        let tables: Vec<Option<&TableSchema>> = items.iter().map(|i| i.name.as_ref().and_then(|n| catalog.and_then(|c| c.table(n)))).collect();
        // Which item a column belongs to (as the rules resolve it).
        let mine = |c: &ColRef| -> bool {
            match c.qualifier() {
                Some(q) => item.label().is_some_and(|l| l.eq_ignore_ascii_case(q)),
                None if items.len() == 1 => true,
                None => {
                    let hits: Vec<usize> = (0..items.len()).filter(|&i| tables[i].is_some_and(|t| Catalog::column(t, c.column()).is_some())).collect();
                    hits == vec![idx] && tables.iter().all(Option::is_some)
                }
            }
        };
        let mut eq: Vec<String> = Vec::new();
        let mut range: Option<String> = None;
        let mut conds: Vec<R> = b.where_.iter().cloned().collect();
        conds.extend(items.iter().filter_map(|i| i.on.clone()));
        for cond in conds {
            for c in sql.conjuncts(cond) {
                if let Some((col, is_eq)) = predicate(sql, &c, &mine) {
                    let name = known.and_then(|t| Catalog::column(t, col.column())).map_or(col.column().to_string(), |d| d.name.clone());
                    if is_eq {
                        if !eq.iter().any(|e| e.eq_ignore_ascii_case(&name)) {
                            eq.push(name);
                        }
                    } else if range.is_none() {
                        range = Some(name);
                    }
                }
            }
        }
        if let Some(r) = range.filter(|r| !eq.iter().any(|e| e.eq_ignore_ascii_case(r))) {
            eq.push(r);
        }
        eq.truncate(4);
        let t = known.cloned().map(|mut t| {
            t.indexes.clear();
            t
        });
        let t = t.unwrap_or_else(|| TableSchema {
            schema: (parts.len() >= 2).then(|| parts[parts.len() - 2].clone()),
            name: parts.last().cloned().unwrap_or_default(),
            ..Default::default()
        });
        return Some((TableSchema { kind: dbine_driver::kinds::TABLE.into(), ..t }, eq));
    }
    None
}

/// `col = x`, `col IN (…)` (equality) or `col < x`, `col BETWEEN…`,
/// `col LIKE 'abc%'` (range), where `col` is one of the table's and the
/// other side isn't.
fn predicate(sql: &Sql, c: &R, mine: &dyn Fn(&ColRef) -> bool) -> Option<(ColRef, bool)> {
    let base = sql.depth[c.start];
    let at = |pred: &dyn Fn(usize) -> bool| c.clone().find(|&i| sql.depth[i] == base && pred(i));
    let other_side_free = |r: R| !sql.col_refs(r).iter().any(|x| mine(x));
    if let Some(op) = at(&|i| sql.is(i, Kind::Op) && matches!(sql.text(i), "=" | "<" | ">" | "<=" | ">=")) {
        let is_eq = sql.text(op) == "=";
        if let Some(col) = sql.col_ref(c.start..op).filter(|x| mine(x)) {
            return other_side_free(op + 1..c.end).then_some((col, is_eq));
        }
        if let Some(col) = sql.col_ref(op + 1..c.end).filter(|x| mine(x)) {
            return other_side_free(c.start..op).then_some((col, is_eq));
        }
        return None;
    }
    if let Some(k) = at(&|i| sql.kw(i, "in") || sql.kw(i, "between") || sql.kw(i, "like")) {
        if k > c.start && sql.kw(k - 1, "not") {
            return None;
        }
        let col = sql.col_ref(c.start..k).filter(|x| mine(x))?;
        if sql.kw(k, "like") {
            // Only a fixed prefix narrows the range.
            let lit = sql.text(k + 1);
            let prefix = sql.is(k + 1, Kind::Str) && lit.len() > 2 && !lit[1..].starts_with(['%', '_']);
            return prefix.then_some((col, false));
        }
        return other_side_free(k + 1..c.end).then_some((col, sql.kw(k, "in")));
    }
    None
}

/// MongoDB: the COLLSCAN's filter fields, then the SORT's, then one range (ESR).
fn mongo_fields(scan: &PlanNode, all: &[&PlanNode]) -> Vec<String> {
    let prop = |n: &PlanNode, k: &str| n.props.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone());
    let mut eq = Vec::new();
    let mut range = Vec::new();
    if let Some(filter) = prop(scan, "filter").and_then(|f| serde_json::from_str::<serde_json::Value>(&f).ok()) {
        mongo_filter(&filter, &mut eq, &mut range);
    }
    let mut out = eq;
    for n in all.iter().filter(|n| n.op == "SORT") {
        if let Some(serde_json::Value::Object(m)) = prop(n, "sortPattern").and_then(|f| serde_json::from_str(&f).ok()) {
            for k in m.keys() {
                if !out.contains(k) {
                    out.push(k.clone());
                }
            }
        }
    }
    if let Some(r) = range.into_iter().find(|r| !out.contains(r)) {
        out.push(r);
    }
    out.truncate(4);
    out
}

fn mongo_filter(v: &serde_json::Value, eq: &mut Vec<String>, range: &mut Vec<String>) {
    let Some(m) = v.as_object() else { return };
    for (k, val) in m {
        if k == "$and" {
            for part in val.as_array().into_iter().flatten() {
                mongo_filter(part, eq, range);
            }
            continue;
        }
        if k.starts_with('$') {
            continue;
        }
        let ops: Vec<&str> = val.as_object().map(|o| o.keys().map(String::as_str).filter(|k| k.starts_with('$')).collect()).unwrap_or_default();
        if ops.is_empty() || ops.iter().any(|o| matches!(*o, "$eq" | "$in")) {
            if !eq.contains(k) {
                eq.push(k.clone());
            }
        } else if ops.iter().any(|o| matches!(*o, "$gt" | "$gte" | "$lt" | "$lte" | "$regex")) && !range.contains(k) {
            range.push(k.clone());
        }
    }
}

/// A short text of the plan for the AI: one line per operator, indented.
pub fn summary(plans: &[Plan], max_lines: usize) -> String {
    fn line(n: &PlanNode, depth: usize, out: &mut Vec<String>) {
        let mut s = format!("{}{}", "  ".repeat(depth), n.op);
        if !n.detail.is_empty() {
            s.push_str(&format!(" [{}]", n.detail.chars().take(80).collect::<String>()));
        }
        if let Some(o) = &n.object {
            s.push_str(&format!(" on {o}"));
        }
        if let Some(r) = n.est_rows {
            s.push_str(&format!(" rows≈{r:.0}"));
        }
        if let Some(c) = n.total_cost {
            s.push_str(&format!(" cost={c:.2}"));
        }
        for w in &n.warnings {
            s.push_str(&format!(" ⚠ {w}"));
        }
        out.push(s);
        for c in &n.children {
            line(c, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for p in plans {
        line(&p.root, 0, &mut out);
    }
    if out.len() > max_lines {
        let more = out.len() - max_lines;
        out.truncate(max_lines);
        out.push(format!("… ({more} operadores más)"));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizer::catalog::tests::table;

    fn node(op: &str, detail: &str, object: Option<&str>, children: Vec<PlanNode>) -> PlanNode {
        PlanNode { op: op.into(), detail: detail.into(), object: object.map(str::to_string), children, est_rows: Some(5000.0), ..Default::default() }
    }

    fn plan(root: PlanNode) -> Vec<Plan> {
        vec![Plan { root, ..Default::default() }]
    }

    fn generic(t: &TableSchema) -> Option<String> {
        let ix = &t.indexes[0];
        Some(format!("CREATE INDEX {} ON {} ({});", ix.name, t.name, ix.columns.join(", ")))
    }

    #[test]
    fn full_scans_per_engine() {
        assert!(full_scan(&node("Seq Scan", "", Some("orders"), vec![])));
        assert!(full_scan(&node("Parallel Seq Scan", "", Some("orders"), vec![])));
        assert!(full_scan(&node("Table scan", "", Some("o"), vec![])));
        assert!(full_scan(&node("TABLE ACCESS", "FULL", Some("DBINE.A"), vec![])));
        assert!(!full_scan(&node("TABLE ACCESS", "BY INDEX ROWID", Some("DBINE.A"), vec![])));
        assert!(full_scan(&node("SCAN", "", Some("t"), vec![])));
        assert!(!full_scan(&node("SCAN", "USING COVERING INDEX ix", Some("t"), vec![])));
        assert!(full_scan(&node("COLLSCAN", "forward", Some("db.c"), vec![])));
        assert!(full_scan(&node("Clustered Index Scan", "", Some("[dbo].[t]"), vec![])));
        assert!(full_scan(&node("SEQ_SCAN ", "", Some("t"), vec![])));
        assert!(!full_scan(&node("Index Seek", "", Some("[dbo].[t]"), vec![])));
        assert!(!full_scan(&node("TableScan", "", Some("t"), vec![])), "columnar engines without indexes");
    }

    #[test]
    fn scan_with_filters_suggests_an_index() {
        let p = plan(node("Hash Join", "", None, vec![node("Seq Scan", "", Some("orders"), vec![]), node("Index Scan", "", Some("customers"), vec![])]));
        let f = from_plans(&p, "SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.status = 'A' AND o.total > 10", "postgres", false, None, &generic);
        assert_eq!(f.hints.len(), 1);
        let h = &f.hints[0];
        assert_eq!((h.reason.as_str(), h.table.as_str()), ("full_scan", "orders"));
        assert_eq!(h.columns, vec!["status", "customer_id", "total"]);
        assert_eq!(h.script.as_deref(), Some("CREATE INDEX ix_orders_status_customer_id_t ON orders (status, customer_id, total);"));
    }

    #[test]
    fn mysql_scans_name_the_alias_and_like_prefixes_count() {
        let p = plan(node("Table scan", "", Some("o"), vec![]));
        let f = from_plans(&p, "SELECT * FROM orders o WHERE o.code LIKE 'AB%' AND o.kind IN (1, 2)", "mysql", false, None, &generic);
        assert_eq!(f.hints[0].columns, vec!["kind", "code"]);
        let f = from_plans(&p, "SELECT * FROM orders o WHERE o.code LIKE '%AB'", "mysql", false, None, &generic);
        assert!(f.hints.is_empty(), "a leading wildcard can't use an index");
    }

    #[test]
    fn no_hint_without_filters_or_when_an_index_exists() {
        let p = plan(node("Seq Scan", "", Some("orders"), vec![]));
        assert!(from_plans(&p, "SELECT * FROM orders", "postgres", false, None, &generic).hints.is_empty());
        let t = vec![table(None, "orders", &[("id", "int", false), ("status", "text", true)], &["id"], &[(&["status"], false)])];
        assert!(from_plans(&p, "SELECT * FROM orders WHERE status = 'A'", "postgres", false, Some(&t), &generic).hints.is_empty());
        // Comparing two of its own columns doesn't narrow anything.
        assert!(from_plans(&p, "SELECT * FROM orders WHERE a = b", "postgres", false, None, &generic).hints.is_empty());
    }

    #[test]
    fn sql_server_missing_index_hints() {
        let mut root = node("Clustered Index Scan", "", Some("[dbo].[t].[PK_t]"), vec![]);
        root.warnings.push("Índice faltante (impacto 87.5%): CREATE NONCLUSTERED INDEX [IX_sugerido] ON [dbo].[t] ([a], [b]) INCLUDE ([c])".into());
        root.warnings.push("Conversión implícita: CONVERT_IMPLICIT(int, x)".into());
        let f = from_plans(&plan(root), "SELECT c FROM t WHERE a = 1 AND b > 2", "mssql", false, None, &generic);
        assert_eq!(f.hints.len(), 1, "the engine's own hint, not a duplicate");
        let h = &f.hints[0];
        assert_eq!((h.reason.as_str(), h.table.as_str(), h.impact), ("missing_index", "t", Some(87.5)));
        assert_eq!(h.columns, vec!["a", "b"]);
        assert_eq!(h.script.as_deref(), Some("CREATE NONCLUSTERED INDEX [ix_t_a_b] ON [dbo].[t] ([a], [b]) INCLUDE ([c])"));
        assert_eq!(f.warnings.len(), 1);
        assert!(f.warnings[0].text.starts_with("Conversión implícita"));
    }

    #[test]
    fn sql_server_scans_name_the_index_after_the_table() {
        let p = plan(node("Clustered Index Scan", "", Some("dbo.orders.PK__orders__3213"), vec![]));
        let f = from_plans(&p, "SELECT id FROM dbo.orders WHERE customer_id = 5", "mssql", false, None, &generic);
        assert_eq!((f.hints[0].table.as_str(), f.hints[0].columns.clone()), ("orders", vec!["customer_id".to_string()]));
    }

    #[test]
    fn mongo_collscan_follows_equality_sort_range() {
        let mut scan = node("COLLSCAN", "forward", Some("shop.people"), vec![]);
        scan.props.push(("filter".into(), r#"{"$and":[{"city":{"$eq":"X"}},{"age":{"$gt":30}}]}"#.into()));
        let mut sort = node("SORT", "", None, vec![scan]);
        sort.props.push(("sortPattern".into(), r#"{"name":1}"#.into()));
        let f = from_plans(&plan(sort), "db.people.find({city: 'X', age: {$gt: 30}}).sort({name: 1})", "", true, None, &generic);
        assert_eq!(f.hints.len(), 1);
        assert_eq!(f.hints[0].table, "people");
        assert_eq!(f.hints[0].columns, vec!["city", "name", "age"]);
        assert_eq!(f.warnings.len(), 0);
    }

    #[test]
    fn other_engines_get_the_scan_without_columns() {
        let p = plan(node("AllNodesScan", "", Some("n"), vec![]));
        let f = from_plans(&p, "MATCH (n) WHERE n.name = 'x' RETURN n", "", true, None, &generic);
        assert_eq!(f.hints[0].reason, "scan");
        assert!(f.hints[0].script.is_none());
    }

    #[test]
    fn summary_is_short() {
        let p = plan(node("Hash Join", "", None, vec![node("Seq Scan", "", Some("orders"), vec![])]));
        assert_eq!(summary(&p, 10), "Hash Join rows≈5000\n  Seq Scan on orders rows≈5000");
        assert!(summary(&p, 1).ends_with("(1 operadores más)"));
    }
}
