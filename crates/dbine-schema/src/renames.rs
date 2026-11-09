//! Probable renames in a comparison: a column (or a table) on one side only
//! whose twin is on the other side only. Carried as drop plus add, a rename
//! loses the data; done as a rename, it keeps it. The UI offers each one, and
//! the rename itself is the engine's (`Driver::rename_script`).
//!
//! The pairs found here come from the structure alone. The ones whose names
//! don't look alike (`nombre` and `name`) are a question for the AI:
//! [`question`] says what to ask and [`read`] keeps only valid answers.

use crate::compare::{same_type, CompareResult, DbModel, Status, TableDiff};
use dbine_driver::{ColumnDef, TableSchema};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A pair that's probably one thing under two names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameHint {
    /// `column` or `table`.
    pub kind: String,
    /// The table's key in the comparison (`TableDiff::key`). For a table, the
    /// left one's.
    pub table: String,
    /// The name on the left: a column, or a table's key.
    pub left: String,
    /// The name on the right.
    pub right: String,
    /// Why: `shape` (the only pair left of that type), `name` (alike names,
    /// same type), `columns` (tables with the same columns) or `ai`.
    pub by: String,
}

/// How alike two names are, from 0 to 1: case, `_`, `-` and spaces aside.
fn likeness(a: &str, b: &str) -> f64 {
    let squash = |s: &str| {
        s.to_lowercase()
            .chars()
            .filter(|c| !matches!(c, '_' | '-' | ' '))
            .collect::<Vec<char>>()
    };
    let (a, b) = (squash(a), squash(b));
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 0.0;
    }
    // Levenshtein over two rows.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            row[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1);
        }
        prev = row;
    }
    1.0 - prev[b.len()] as f64 / longest as f64
}

/// Names alike enough to say so without asking.
const ALIKE: f64 = 0.6;

/// Pairs each left item with at most one right item and back: the best
/// score for both. `score` is None where they can't pair.
fn mutual_best(
    lefts: usize,
    rights: usize,
    score: impl Fn(usize, usize) -> Option<f64>,
) -> Vec<(usize, usize, f64)> {
    let best_for_left = |i: usize| {
        (0..rights)
            .filter_map(|j| score(i, j).map(|s| (j, s)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
    };
    let best_for_right = |j: usize| {
        (0..lefts)
            .filter_map(|i| score(i, j).map(|s| (i, s)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
    };
    (0..lefts)
        .filter_map(|i| {
            let (j, s) = best_for_left(i)?;
            (best_for_right(j)?.0 == i).then_some((i, j, s))
        })
        .collect()
}

fn unmatched<'a>(td: &TableDiff, t: &'a TableSchema, status: Status) -> Vec<&'a ColumnDef> {
    td.columns
        .iter()
        .filter(|d| d.status == status)
        .filter_map(|d| match status {
            Status::OnlyLeft => d.left,
            _ => d.right,
        })
        .filter_map(|i| t.columns.get(i))
        .collect()
}

fn column_pairs(left: &DbModel, right: &DbModel, td: &TableDiff) -> Vec<RenameHint> {
    let (Some(i), Some(j)) = (td.left, td.right) else {
        return Vec::new();
    };
    let (lt, rt) = (&left.tables[i], &right.tables[j]);
    let (ls, rs) = (
        unmatched(td, lt, Status::OnlyLeft),
        unmatched(td, rt, Status::OnlyRight),
    );
    if ls.is_empty() || rs.is_empty() {
        return Vec::new();
    }
    let same = |a: &ColumnDef, b: &ColumnDef| {
        same_type(&a.data_type, &left.driver, &b.data_type, &right.driver)
    };
    // The only pair left of its type is a rename whatever the names say.
    let alone = |a: &ColumnDef, b: &ColumnDef| {
        ls.iter().filter(|x| same(x, b)).count() == 1
            && rs.iter().filter(|y| same(a, y)).count() == 1
    };
    mutual_best(ls.len(), rs.len(), |x, y| {
        let (a, b) = (ls[x], rs[y]);
        if !same(a, b) {
            return None;
        }
        let like = likeness(&a.name, &b.name);
        (like >= ALIKE || alone(a, b)).then_some(like)
    })
    .into_iter()
    .map(|(x, y, like)| RenameHint {
        kind: "column".into(),
        table: td.key.clone(),
        left: ls[x].name.clone(),
        right: rs[y].name.clone(),
        by: if like >= ALIKE { "name" } else { "shape" }.into(),
    })
    .collect()
}

fn column_names(t: &TableSchema) -> BTreeSet<String> {
    t.columns.iter().map(|c| c.name.to_lowercase()).collect()
}

/// Tables on one side only with nearly the same columns.
fn table_pairs(left: &DbModel, right: &DbModel, result: &CompareResult) -> Vec<RenameHint> {
    let ls: Vec<&TableDiff> = result
        .tables
        .iter()
        .filter(|t| t.status == Status::OnlyLeft)
        .collect();
    let rs: Vec<&TableDiff> = result
        .tables
        .iter()
        .filter(|t| t.status == Status::OnlyRight)
        .collect();
    let cols = |m: &DbModel, i: Option<usize>| {
        i.and_then(|i| m.tables.get(i))
            .map(column_names)
            .unwrap_or_default()
    };
    let lc: Vec<_> = ls.iter().map(|t| cols(left, t.left)).collect();
    let rc: Vec<_> = rs.iter().map(|t| cols(right, t.right)).collect();
    mutual_best(ls.len(), rs.len(), |x, y| {
        let (a, b) = (&lc[x], &rc[y]);
        let union = a.union(b).count();
        if union < 2 {
            return None;
        }
        let jaccard = a.intersection(b).count() as f64 / union as f64;
        (jaccard >= 0.7).then_some(jaccard)
    })
    .into_iter()
    .map(|(x, y, _)| RenameHint {
        kind: "table".into(),
        table: ls[x].key.clone(),
        left: ls[x].key.clone(),
        right: rs[y].key.clone(),
        by: "columns".into(),
    })
    .collect()
}

/// The renames the structure alone points at.
pub fn probable(left: &DbModel, right: &DbModel, result: &CompareResult) -> Vec<RenameHint> {
    let mut out: Vec<RenameHint> = result
        .tables
        .iter()
        .filter(|t| t.status == Status::Changed)
        .flat_map(|t| column_pairs(left, right, t))
        .collect();
    out.extend(table_pairs(left, right, result));
    out
}

/// What's left to ask the AI: the items still unpaired, by table.
#[derive(Debug, Default)]
pub struct Question {
    /// The prompt's data: one block per table, `<` left only, `>` right only.
    pub text: String,
    /// Per table key: the unpaired columns on each side.
    columns: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)>,
    tables: (BTreeSet<String>, BTreeSet<String>),
}

fn column_line(mark: char, c: &ColumnDef) -> String {
    format!("  {mark} {} {}", c.name, c.data_type)
}

/// None when nothing is left that could pair. Only names and types: never data.
pub fn question(
    left: &DbModel,
    right: &DbModel,
    result: &CompareResult,
    known: &[RenameHint],
) -> Option<Question> {
    let taken = |kind: &str, table: &str, side: &str, name: &str| {
        known.iter().any(|h| {
            h.kind == kind
                && (kind == "table" || h.table == table)
                && (if side == "left" { &h.left } else { &h.right }) == name
        })
    };
    let mut q = Question::default();
    let mut text = String::new();
    for td in result.tables.iter().filter(|t| t.status == Status::Changed) {
        let (Some(i), Some(j)) = (td.left, td.right) else {
            continue;
        };
        let ls: Vec<_> = unmatched(td, &left.tables[i], Status::OnlyLeft)
            .into_iter()
            .filter(|c| !taken("column", &td.key, "left", &c.name))
            .collect();
        let rs: Vec<_> = unmatched(td, &right.tables[j], Status::OnlyRight)
            .into_iter()
            .filter(|c| !taken("column", &td.key, "right", &c.name))
            .collect();
        if ls.is_empty() || rs.is_empty() {
            continue;
        }
        text.push_str(&format!("table {}\n", td.key));
        for c in &ls {
            text.push_str(&column_line('<', c));
            text.push('\n');
        }
        for c in &rs {
            text.push_str(&column_line('>', c));
            text.push('\n');
        }
        q.columns.insert(
            td.key.clone(),
            (
                ls.iter().map(|c| c.name.clone()).collect(),
                rs.iter().map(|c| c.name.clone()).collect(),
            ),
        );
    }
    let alone = |status: Status, side: &str| -> Vec<(String, usize)> {
        result
            .tables
            .iter()
            .filter(|t| t.status == status && !taken("table", "", side, &t.key))
            .filter_map(|t| {
                let i = if side == "left" { t.left } else { t.right };
                Some((t.key.clone(), i?))
            })
            .collect()
    };
    let (lt, rt) = (
        alone(Status::OnlyLeft, "left"),
        alone(Status::OnlyRight, "right"),
    );
    if !lt.is_empty() && !rt.is_empty() {
        for (mark, list, m) in [('<', &lt, left), ('>', &rt, right)] {
            for (key, i) in list.iter() {
                let Some(t) = m.tables.get(*i) else {
                    continue;
                };
                let cols = t
                    .columns
                    .iter()
                    .map(|c| format!("{} {}", c.name, c.data_type))
                    .collect::<Vec<_>>()
                    .join(", ");
                text.push_str(&format!("{mark} table {key} ({cols})\n"));
            }
        }
        q.tables = (
            lt.iter().map(|(k, _)| k.clone()).collect(),
            rt.iter().map(|(k, _)| k.clone()).collect(),
        );
    }
    if text.is_empty() {
        return None;
    }
    q.text = text;
    Some(q)
}

#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    columns: Vec<ColumnAnswer>,
    #[serde(default)]
    tables: Vec<TableAnswer>,
}

#[derive(Deserialize)]
struct ColumnAnswer {
    table: String,
    left: String,
    right: String,
}

#[derive(Deserialize)]
struct TableAnswer {
    left: String,
    right: String,
}

/// The AI's pairs that are in the question, each name used once. An answer
/// that isn't the asked format is an error, so it can be asked again.
pub fn read(answer: serde_json::Value, q: &Question) -> Result<Vec<RenameHint>, String> {
    let a: Answer = serde_json::from_value(answer).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut used = BTreeSet::new();
    for c in a.columns {
        let Some((ls, rs)) = q.columns.get(&c.table) else {
            continue;
        };
        if !ls.contains(&c.left) || !rs.contains(&c.right) {
            continue;
        }
        if !used.insert((c.table.clone(), "l", c.left.clone()))
            || !used.insert((c.table.clone(), "r", c.right.clone()))
        {
            continue;
        }
        out.push(RenameHint {
            kind: "column".into(),
            table: c.table,
            left: c.left,
            right: c.right,
            by: "ai".into(),
        });
    }
    for t in a.tables {
        if !q.tables.0.contains(&t.left) || !q.tables.1.contains(&t.right) {
            continue;
        }
        if !used.insert((String::new(), "l", t.left.clone()))
            || !used.insert((String::new(), "r", t.right.clone()))
        {
            continue;
        }
        out.push(RenameHint {
            kind: "table".into(),
            table: t.left.clone(),
            left: t.left,
            right: t.right,
            by: "ai".into(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::{compare, CompareOptions};

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            nullable: true,
            ..Default::default()
        }
    }

    fn table(name: &str, cols: Vec<ColumnDef>) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("dbo".into()),
            name: name.into(),
            columns: cols,
            ..Default::default()
        }
    }

    fn model(tables: Vec<TableSchema>) -> DbModel {
        DbModel {
            driver: "sqlserver".into(),
            tables,
            objects: Vec::new(),
        }
    }

    fn hints(left: &DbModel, right: &DbModel) -> Vec<RenameHint> {
        let r = compare(left, right, &CompareOptions::default());
        probable(left, right, &r)
    }

    #[test]
    fn alike_names_of_the_same_type_are_a_rename() {
        let l = model(vec![table(
            "clientes",
            vec![
                col("id", "int"),
                col("cliente_id", "int"),
                col("tel", "varchar(20)"),
            ],
        )]);
        let r = model(vec![table(
            "clientes",
            vec![
                col("id", "int"),
                col("client_id", "int"),
                col("phone", "varchar(20)"),
            ],
        )]);
        let h = hints(&l, &r);
        assert!(
            h.contains(&RenameHint {
                kind: "column".into(),
                table: "dbo.clientes".into(),
                left: "cliente_id".into(),
                right: "client_id".into(),
                by: "name".into(),
            }),
            "{h:?}"
        );
        // The only varchar pair left: a rename by shape, though the names differ.
        assert!(
            h.iter()
                .any(|x| x.left == "tel" && x.right == "phone" && x.by == "shape"),
            "{h:?}"
        );
    }

    #[test]
    fn different_names_with_a_type_in_common_are_left_to_ask() {
        let l = model(vec![table(
            "t",
            vec![
                col("id", "int"),
                col("nombre", "varchar(50)"),
                col("apellido", "varchar(50)"),
            ],
        )]);
        let r = model(vec![table(
            "t",
            vec![
                col("id", "int"),
                col("name", "varchar(50)"),
                col("surname", "varchar(50)"),
            ],
        )]);
        let h = hints(&l, &r);
        assert!(h.is_empty(), "{h:?}");
        let result = compare(&l, &r, &CompareOptions::default());
        let q = question(&l, &r, &result, &h).unwrap();
        assert!(q.text.contains("table dbo.t\n"));
        assert!(q.text.contains("  < nombre varchar(50)\n"));
        assert!(q.text.contains("  > surname varchar(50)\n"));

        let answer = serde_json::json!({
            "columns": [
                {"table": "dbo.t", "left": "nombre", "right": "name"},
                {"table": "dbo.t", "left": "apellido", "right": "surname"},
                // Not in the question, or a name used twice: dropped.
                {"table": "dbo.t", "left": "id", "right": "name"},
                {"table": "dbo.otra", "left": "x", "right": "y"},
                {"table": "dbo.t", "left": "nombre", "right": "surname"}
            ]
        });
        let got = read(answer, &q).unwrap();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got.iter().all(|x| x.by == "ai"));
        assert!(read(serde_json::json!({"columns": "no"}), &q).is_err());
    }

    #[test]
    fn a_column_pairs_with_one_other_at_most() {
        // Two left ids, one right: only the most alike pairs.
        let l = model(vec![table(
            "t",
            vec![
                col("k", "int"),
                col("user_id", "int"),
                col("users_id", "int"),
            ],
        )]);
        let r = model(vec![table(
            "t",
            vec![col("k", "int"), col("userid", "int")],
        )]);
        let h = hints(&l, &r);
        assert_eq!(h.len(), 1, "{h:?}");
        assert_eq!(h[0].left, "user_id");
    }

    #[test]
    fn tables_with_the_same_columns_are_a_rename() {
        let cols = || {
            vec![
                col("id", "int"),
                col("total", "decimal(10,2)"),
                col("fecha", "date"),
            ]
        };
        let l = model(vec![
            table("ventas", cols()),
            table("otra", vec![col("a", "int"), col("b", "int")]),
        ]);
        let r = model(vec![
            table("sales", cols()),
            table("nada", vec![col("x", "int"), col("y", "int")]),
        ]);
        let h = hints(&l, &r);
        assert_eq!(
            h,
            vec![RenameHint {
                kind: "table".into(),
                table: "dbo.ventas".into(),
                left: "dbo.ventas".into(),
                right: "dbo.sales".into(),
                by: "columns".into(),
            }]
        );
        // The rest goes to the AI, columns and all.
        let result = compare(&l, &r, &CompareOptions::default());
        let q = question(&l, &r, &result, &h).unwrap();
        assert!(
            q.text.contains("< table dbo.otra (a int, b int)\n"),
            "{}",
            q.text
        );
        assert!(!q.text.contains("ventas"));
        let got = read(
            serde_json::json!({"tables": [{"left": "dbo.otra", "right": "dbo.nada"}]}),
            &q,
        )
        .unwrap();
        assert_eq!(got[0].kind, "table");
    }

    #[test]
    fn nothing_to_ask_when_every_column_pairs() {
        let l = model(vec![table("t", vec![col("id", "int")])]);
        let r = model(vec![table("t", vec![col("id", "bigint")])]);
        let result = compare(&l, &r, &CompareOptions::default());
        assert!(question(&l, &r, &result, &[]).is_none());
    }
}
