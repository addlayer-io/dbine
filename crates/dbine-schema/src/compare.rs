//! Schema comparison: two databases' tables (and views, procedures…) paired
//! by name, with what differs in each one down to the column property.
//!
//! Pure and cheap: the UI calls it again after every change it carries from
//! one side to the other, over its in-memory copies of both schemas.

use crate::default::parse_default;
use crate::dialect::for_driver;
use crate::parse::parse;
use dbine_driver::{CheckDef, ColumnDef, ForeignKeyDef, IndexDef, TableSchema};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A view, procedure, function, trigger… with its source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeObject {
    pub kind: String,
    #[serde(default)]
    pub schema: Option<String>,
    pub name: String,
    pub definition: String,
}

/// One side of a comparison.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DbModel {
    /// The driver id (type comparison follows its dialect).
    pub driver: String,
    #[serde(default)]
    pub tables: Vec<TableSchema>,
    #[serde(default)]
    pub objects: Vec<CodeObject>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct CompareOptions {
    /// Names match regardless of case.
    #[serde(default)]
    pub ignore_case: bool,
    /// Pair by name only (comparing one schema against another).
    #[serde(default)]
    pub ignore_schema: bool,
    /// Comments don't count as differences.
    #[serde(default)]
    pub ignore_comments: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Equal,
    Changed,
    OnlyLeft,
    OnlyRight,
}

/// A column, index or foreign key pair.
#[derive(Debug, Clone, Serialize)]
pub struct ItemDiff {
    pub name: String,
    /// Positions in the side's list (`columns`, `indexes`, `foreign_keys`,
    /// `checks`).
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub status: Status,
    /// Properties that differ: `type`, `nullable`, `default`,
    /// `auto_increment`, `comment`, `columns`, `unique`, `filter`,
    /// `kind`, `include`, `options`, `expression`, `on_delete`, `on_update`.
    pub fields: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TableDiff {
    /// `schema.name` (as the left side writes it, else the right).
    pub key: String,
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub status: Status,
    pub columns: Vec<ItemDiff>,
    pub indexes: Vec<ItemDiff>,
    pub foreign_keys: Vec<ItemDiff>,
    pub checks: Vec<ItemDiff>,
    /// The primary key (columns, in order).
    pub primary_key: Status,
    /// Table properties that differ (`comment`, `options`).
    pub fields: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObjectDiff {
    pub kind: String,
    pub key: String,
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub status: Status,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompareResult {
    pub tables: Vec<TableDiff>,
    pub objects: Vec<ObjectDiff>,
}

fn norm(s: &str, o: &CompareOptions) -> String {
    if o.ignore_case { s.to_lowercase() } else { s.to_string() }
}

fn key_of(schema: Option<&str>, name: &str, o: &CompareOptions) -> String {
    match schema.filter(|s| !s.is_empty() && !o.ignore_schema) {
        Some(s) => format!("{}.{}", norm(s, o), norm(name, o)),
        None => norm(name, o),
    }
}

fn display(schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// Pair two lists by key: `(left index, right index)` in left order, then
/// the right-only ones.
fn pair<T>(left: &[T], right: &[T], key: impl Fn(&T) -> String) -> Vec<(Option<usize>, Option<usize>)> {
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for (i, r) in right.iter().enumerate() {
        by_key.entry(key(r)).or_insert(i);
    }
    let mut used = vec![false; right.len()];
    let mut out = Vec::new();
    for (i, l) in left.iter().enumerate() {
        match by_key.get(&key(l)).copied().filter(|j| !used[*j]) {
            Some(j) => {
                used[j] = true;
                out.push((Some(i), Some(j)));
            }
            None => out.push((Some(i), None)),
        }
    }
    out.extend(used.iter().enumerate().filter(|(_, u)| !**u).map(|(j, _)| (None, Some(j))));
    out
}

/// Types compared the way the engines see them: the same text (case and
/// spacing aside), or the same logical type (`int` / `integer` / `int4`,
/// and across engines `nvarchar(50)` / `varchar(50)`).
pub fn same_type(a: &str, da: &str, b: &str, db: &str) -> bool {
    let squash = |s: &str| s.to_lowercase().chars().filter(|c| !c.is_whitespace() && *c != '"' && *c != '`' && *c != '[' && *c != ']').collect::<String>();
    if squash(a) == squash(b) {
        return true;
    }
    match (for_driver(da), for_driver(db)) {
        (Some(x), Some(y)) => {
            let (la, lb) = (crate::convert::logical_of(x, &parse(a)), crate::convert::logical_of(y, &parse(b)));
            // Unknown types only match by text.
            la == lb && !matches!(la, crate::LogicalType::Other { .. })
        }
        _ => false,
    }
}

fn same_default(a: &Option<String>, b: &Option<String>) -> bool {
    let v = |d: &Option<String>| d.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(parse_default);
    v(a) == v(b)
}

fn column_fields(l: &ColumnDef, dl: &str, r: &ColumnDef, dr: &str, o: &CompareOptions) -> Vec<&'static str> {
    let mut f = Vec::new();
    if !same_type(&l.data_type, dl, &r.data_type, dr) {
        f.push("type");
    }
    if l.nullable != r.nullable {
        f.push("nullable");
    }
    if !same_default(&l.default_value, &r.default_value) {
        f.push("default");
    }
    if l.auto_increment != r.auto_increment {
        f.push("auto_increment");
    }
    if !o.ignore_comments && l.comment.as_deref().unwrap_or("") != r.comment.as_deref().unwrap_or("") {
        f.push("comment");
    }
    // Engine-specific settings (analyzer, encoding, a Cassandra key role…):
    // only meaningful between two databases of the same engine.
    if dl == dr && l.options != r.options {
        f.push("options");
    }
    f
}

fn cols_key(cols: &[String], o: &CompareOptions) -> String {
    cols.iter().map(|c| norm(c, o)).collect::<Vec<_>>().join(",")
}

fn index_fields(l: &IndexDef, r: &IndexDef, o: &CompareOptions) -> Vec<&'static str> {
    let mut f = Vec::new();
    if cols_key(&l.columns, o) != cols_key(&r.columns, o) {
        f.push("columns");
    }
    if l.unique != r.unique {
        f.push("unique");
    }
    let w = |x: &Option<String>| x.as_deref().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    if w(&l.filter) != w(&r.filter) {
        f.push("filter");
    }
    let kind = |x: &Option<String>| x.as_deref().unwrap_or("").trim().to_lowercase();
    if kind(&l.kind) != kind(&r.kind) {
        f.push("kind");
    }
    // Included columns: their order doesn't matter.
    let inc = |x: &[String]| {
        let mut v: Vec<String> = x.iter().map(|c| norm(c, o)).collect();
        v.sort();
        v
    };
    if inc(&l.include) != inc(&r.include) {
        f.push("include");
    }
    if l.options != r.options {
        f.push("options");
    }
    f
}

use dbine_driver::alter::check_expr;

/// CHECKs pair by name; the unnamed ones (or with generated names on one
/// side), by condition.
fn check_key(c: &CheckDef, o: &CompareOptions) -> String {
    match c.name.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => norm(n, o),
        None => format!("({})", check_expr(&c.expression)),
    }
}

/// Foreign keys pair by what they link (their names are usually generated).
fn fk_key(fk: &ForeignKeyDef, o: &CompareOptions) -> String {
    let rs = if o.ignore_schema { None } else { fk.ref_schema.as_deref() };
    format!("{}>{}({})", cols_key(&fk.columns, o), key_of(rs, &fk.ref_table, o), cols_key(&fk.ref_columns, o))
}

fn fk_fields(l: &ForeignKeyDef, r: &ForeignKeyDef) -> Vec<&'static str> {
    let action = |a: &Option<String>| a.as_deref().map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty() && s != "NO ACTION" && s != "RESTRICT");
    let mut f = Vec::new();
    if action(&l.on_delete) != action(&r.on_delete) {
        f.push("on_delete");
    }
    if action(&l.on_update) != action(&r.on_update) {
        f.push("on_update");
    }
    f
}

fn status_of(l: Option<usize>, r: Option<usize>, changed: bool) -> Status {
    match (l, r) {
        (Some(_), None) => Status::OnlyLeft,
        (None, Some(_)) => Status::OnlyRight,
        _ if changed => Status::Changed,
        _ => Status::Equal,
    }
}

type TableItems = (Vec<ItemDiff>, Vec<ItemDiff>, Vec<ItemDiff>, Vec<ItemDiff>, Status, Vec<&'static str>);

fn compare_table(l: &TableSchema, dl: &str, r: &TableSchema, dr: &str, o: &CompareOptions) -> TableItems {
    let columns = pair(&l.columns, &r.columns, |c| norm(&c.name, o))
        .into_iter()
        .map(|(a, b)| {
            let fields = match (a, b) {
                (Some(i), Some(j)) => column_fields(&l.columns[i], dl, &r.columns[j], dr, o),
                _ => Vec::new(),
            };
            let name = a.map(|i| &l.columns[i]).or(b.map(|j| &r.columns[j])).map(|c| c.name.clone()).unwrap_or_default();
            ItemDiff { name, left: a, right: b, status: status_of(a, b, !fields.is_empty()), fields }
        })
        .collect();

    // Indexes by name; unmatched ones then by what they index.
    let mut ix_pairs = pair(&l.indexes, &r.indexes, |i| norm(&i.name, o));
    let lonely_l: Vec<usize> = ix_pairs.iter().filter_map(|p| match p { (Some(i), None) => Some(*i), _ => None }).collect();
    let lonely_r: Vec<usize> = ix_pairs.iter().filter_map(|p| match p { (None, Some(j)) => Some(*j), _ => None }).collect();
    for i in lonely_l {
        let k = (cols_key(&l.indexes[i].columns, o), l.indexes[i].unique);
        if let Some(&j) = lonely_r.iter().find(|&&j| (cols_key(&r.indexes[j].columns, o), r.indexes[j].unique) == k && ix_pairs.contains(&(None, Some(j)))) {
            ix_pairs.retain(|p| *p != (Some(i), None) && *p != (None, Some(j)));
            ix_pairs.push((Some(i), Some(j)));
        }
    }
    let indexes = ix_pairs
        .into_iter()
        .map(|(a, b)| {
            let fields = match (a, b) {
                (Some(i), Some(j)) => index_fields(&l.indexes[i], &r.indexes[j], o),
                _ => Vec::new(),
            };
            let name = a.map(|i| l.indexes[i].name.clone()).or(b.map(|j| r.indexes[j].name.clone())).unwrap_or_default();
            ItemDiff { name, left: a, right: b, status: status_of(a, b, !fields.is_empty()), fields }
        })
        .collect();

    let foreign_keys = pair(&l.foreign_keys, &r.foreign_keys, |f| fk_key(f, o))
        .into_iter()
        .map(|(a, b)| {
            let fields = match (a, b) {
                (Some(i), Some(j)) => fk_fields(&l.foreign_keys[i], &r.foreign_keys[j]),
                _ => Vec::new(),
            };
            let fk = a.map(|i| &l.foreign_keys[i]).or(b.map(|j| &r.foreign_keys[j]));
            let name = fk.map(|f| f.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("({}) → {}", f.columns.join(", "), f.ref_table))).unwrap_or_default();
            ItemDiff { name, left: a, right: b, status: status_of(a, b, !fields.is_empty()), fields }
        })
        .collect();

    // CHECKs by name; unmatched ones then by condition (names are often generated).
    let mut ck_pairs = pair(&l.checks, &r.checks, |c| check_key(c, o));
    let lonely_l: Vec<usize> = ck_pairs.iter().filter_map(|p| match p { (Some(i), None) => Some(*i), _ => None }).collect();
    let lonely_r: Vec<usize> = ck_pairs.iter().filter_map(|p| match p { (None, Some(j)) => Some(*j), _ => None }).collect();
    for i in lonely_l {
        let e = check_expr(&l.checks[i].expression);
        if let Some(&j) = lonely_r.iter().find(|&&j| check_expr(&r.checks[j].expression) == e && ck_pairs.contains(&(None, Some(j)))) {
            ck_pairs.retain(|p| *p != (Some(i), None) && *p != (None, Some(j)));
            ck_pairs.push((Some(i), Some(j)));
        }
    }
    let checks = ck_pairs
        .into_iter()
        .map(|(a, b)| {
            let fields = match (a, b) {
                (Some(i), Some(j)) if check_expr(&l.checks[i].expression) != check_expr(&r.checks[j].expression) => vec!["expression"],
                _ => Vec::new(),
            };
            let c = a.map(|i| &l.checks[i]).or(b.map(|j| &r.checks[j]));
            let name = c.map(|c| c.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("CHECK ({})", c.expression.trim()))).unwrap_or_default();
            ItemDiff { name, left: a, right: b, status: status_of(a, b, !fields.is_empty()), fields }
        })
        .collect();

    let pk = |t: &TableSchema| t.primary_key.as_ref().map(|k| cols_key(&k.columns, o)).filter(|k| !k.is_empty());
    let primary_key = match (pk(l), pk(r)) {
        (None, None) => Status::Equal,
        (Some(_), None) => Status::OnlyLeft,
        (None, Some(_)) => Status::OnlyRight,
        (Some(a), Some(b)) => if a == b { Status::Equal } else { Status::Changed },
    };
    let mut fields = Vec::new();
    if !o.ignore_comments && l.comment.as_deref().unwrap_or("") != r.comment.as_deref().unwrap_or("") {
        fields.push("comment");
    }
    // Table options (partitioning, clustering, TTL, shards, a collection's
    // collation…): same engine only, like column options.
    if dl == dr && l.options != r.options {
        fields.push("options");
    }
    (columns, indexes, foreign_keys, checks, primary_key, fields)
}

/// Code as compared: whitespace collapsed, a trailing `;` dropped.
fn code_norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches([';', '/', ' ']).to_string()
}

pub fn compare(left: &DbModel, right: &DbModel, o: &CompareOptions) -> CompareResult {
    let (dl, dr) = (left.driver.as_str(), right.driver.as_str());
    let tables = pair(&left.tables, &right.tables, |t| key_of(t.schema.as_deref(), &t.name, o))
        .into_iter()
        .map(|(a, b)| {
            let t = a.map(|i| &left.tables[i]).or(b.map(|j| &right.tables[j])).expect("one side");
            let key = display(t.schema.as_deref(), &t.name);
            match (a, b) {
                (Some(i), Some(j)) => {
                    let (columns, indexes, foreign_keys, checks, primary_key, fields) = compare_table(&left.tables[i], dl, &right.tables[j], dr, o);
                    let changed = !fields.is_empty()
                        || primary_key != Status::Equal
                        || [&columns, &indexes, &foreign_keys, &checks].iter().any(|v| v.iter().any(|d: &ItemDiff| d.status != Status::Equal));
                    TableDiff { key, left: a, right: b, status: status_of(a, b, changed), columns, indexes, foreign_keys, checks, primary_key, fields }
                }
                _ => TableDiff {
                    key,
                    left: a,
                    right: b,
                    status: status_of(a, b, false),
                    columns: Vec::new(),
                    indexes: Vec::new(),
                    foreign_keys: Vec::new(),
                    checks: Vec::new(),
                    primary_key: Status::Equal,
                    fields: Vec::new(),
                },
            }
        })
        .collect();
    let objects = pair(&left.objects, &right.objects, |c| format!("{}:{}", c.kind, key_of(c.schema.as_deref(), &c.name, o)))
        .into_iter()
        .map(|(a, b)| {
            let c = a.map(|i| &left.objects[i]).or(b.map(|j| &right.objects[j])).expect("one side");
            let changed = matches!((a, b), (Some(i), Some(j)) if code_norm(&left.objects[i].definition) != code_norm(&right.objects[j].definition));
            ObjectDiff { kind: c.kind.clone(), key: display(c.schema.as_deref(), &c.name), left: a, right: b, status: status_of(a, b, changed) }
        })
        .collect();
    CompareResult { tables, objects }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::KeyDef;

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable, ..Default::default() }
    }

    fn table(name: &str, cols: Vec<ColumnDef>) -> TableSchema {
        TableSchema { kind: "table".into(), schema: Some("dbo".into()), name: name.into(), columns: cols, primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }), ..Default::default() }
    }

    #[test]
    fn finds_what_differs() {
        let mut l = table("clientes", vec![col("id", "int", false), col("nombre", "varchar(10)", true), col("alta", "datetime", true)]);
        l.indexes.push(IndexDef { name: "IX_1".into(), columns: vec!["nombre".into()], unique: false, kind: None, filter: None, ..Default::default() });
        let mut r = table("Clientes", vec![col("id", "INT", false), col("nombre", "varchar(5)", false), col("email", "varchar(50)", true)]);
        r.indexes.push(IndexDef { name: "ix_auto_123".into(), columns: vec!["nombre".into()], unique: false, kind: None, filter: None, ..Default::default() });
        let left = DbModel { driver: "sqlserver".into(), tables: vec![l, table("solo_izq", vec![col("id", "int", false)])], objects: vec![] };
        let right = DbModel {
            driver: "sqlserver".into(),
            tables: vec![r],
            objects: vec![CodeObject { kind: "view".into(), schema: Some("dbo".into()), name: "v".into(), definition: "select 1".into() }],
        };
        let res = compare(&left, &right, &CompareOptions { ignore_case: true, ..Default::default() });
        assert_eq!(res.tables.len(), 2);
        let t = &res.tables[0];
        assert_eq!(t.status, Status::Changed);
        let by = |n: &str| t.columns.iter().find(|c| c.name == n).unwrap();
        assert_eq!(by("id").status, Status::Equal);
        assert_eq!((by("nombre").status, by("nombre").fields.clone()), (Status::Changed, vec!["type", "nullable"]));
        assert_eq!(by("alta").status, Status::OnlyLeft);
        assert_eq!(by("email").status, Status::OnlyRight);
        assert_eq!(t.indexes.len(), 1, "paired by columns");
        assert_eq!(t.indexes[0].status, Status::Equal);
        assert_eq!(res.tables[1].status, Status::OnlyLeft);
        assert_eq!(res.objects[0].status, Status::OnlyRight);

        // Case matters unless told otherwise.
        let res = compare(&left, &right, &CompareOptions::default());
        assert!(res.tables.iter().any(|t| t.key == "dbo.Clientes" && t.status == Status::OnlyRight));
    }

    #[test]
    fn checks_included_columns_and_index_options() {
        let chk = |n: Option<&str>, e: &str| CheckDef { name: n.map(Into::into), expression: e.into() };
        let mut l = table("t", vec![col("id", "int", false), col("precio", "int", true)]);
        let mut r = l.clone();
        l.checks = vec![chk(Some("CK_p"), "([precio]>(0))"), chk(Some("CK__t__id__1A2B"), "([id]>(0))"), chk(Some("CK_x"), "id < 9")];
        r.checks = vec![chk(Some("CK_p"), "precio > 0"), chk(Some("CK__t__id__9Z8Y"), "[id] > 0"), chk(Some("CK_x"), "id < 10")];
        l.indexes.push(IndexDef { name: "IX".into(), columns: vec!["precio".into()], include: vec!["id".into()], ..Default::default() });
        r.indexes.push(IndexDef { name: "IX".into(), columns: vec!["precio".into()], ..Default::default() });
        r.indexes[0].options.insert("fillfactor".into(), "80".into());
        let o = CompareOptions::default();
        let res = compare(&DbModel { driver: "sqlserver".into(), tables: vec![l], objects: vec![] }, &DbModel { driver: "sqlserver".into(), tables: vec![r], objects: vec![] }, &o);
        let t = &res.tables[0];
        assert_eq!(t.status, Status::Changed);
        assert_eq!(t.checks.len(), 3, "generated names pair by condition: {:?}", t.checks);
        let by = |n: &str| t.checks.iter().find(|c| c.name.starts_with(n)).unwrap();
        assert_eq!(by("CK_p").status, Status::Equal);
        assert_eq!(by("CK_x").fields, vec!["expression"]);
        assert!(t.checks.iter().any(|c| c.name.starts_with("CK__t__id") && c.status == Status::Equal));
        assert_eq!(t.indexes[0].fields, vec!["include", "options"]);
    }

    #[test]
    fn engine_options_count_between_the_same_engine() {
        let mut l = table("t", vec![col("id", "int", false)]);
        let mut r = l.clone();
        l.options.insert("ttl".into(), "7d".into());
        r.options.insert("ttl".into(), "30d".into());
        l.columns[0].options.insert("encoding".into(), "zstd".into());
        let m = |d: &str, t: &TableSchema| DbModel { driver: d.into(), tables: vec![t.clone()], objects: vec![] };
        let o = CompareOptions::default();
        let res = compare(&m("greptimedb", &l), &m("greptimedb", &r), &o);
        assert_eq!(res.tables[0].fields, vec!["options"]);
        assert_eq!(res.tables[0].columns[0].fields, vec!["options"]);
        // Another engine's options aren't comparable.
        let res = compare(&m("greptimedb", &l), &m("mysql", &r), &o);
        assert!(res.tables[0].fields.is_empty() && res.tables[0].columns[0].fields.is_empty());
    }

    #[test]
    fn types_by_meaning() {
        assert!(same_type("integer", "postgres", "int4", "postgres"));
        assert!(same_type("VARCHAR ( 10 )", "postgres", "varchar(10)", "postgres"));
        assert!(!same_type("varchar(10)", "postgres", "varchar(5)", "postgres"));
        assert!(same_type("int", "sqlserver", "integer", "postgres"));
        assert!(!same_type("int", "sqlserver", "bigint", "postgres"));
    }
}
