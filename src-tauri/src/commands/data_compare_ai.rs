//! The AI on data compare (docs/data-compare.md, "With AI"): it explains
//! what the differences look like, turns a request in words into the grid's
//! filters, and suggests a key for a table without one.
//!
//! The AI never sees a row: the explanation is built from a profile computed
//! here over the whole difference (counts, and the typical gap between the
//! two sides of a column), the filters from the columns' names and types and
//! the user's words, the key from the columns. What comes back is a
//! suggestion the UI shows; nothing here reads or writes a database.

use super::ai::AiRuntime;
use super::ai_task::{self, Asker};
use super::data_compare::{
    canon, diffs, is_bool_type, number_of, sort_value, ChangedRow, Diff, RowKind, Side,
};
use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use chrono::{DateTime, NaiveDate, NaiveDateTime};
use dbine_driver::filter::{ColumnFilter, FilterOp};
use dbine_driver::SortKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use tauri::State;

// -- the profile -------------------------------------------------------------

/// The gaps between the two sides of a column (right − left).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Gaps {
    pub count: usize,
    /// The most frequent gap, and in how many rows.
    pub mode: f64,
    pub mode_rows: usize,
    pub min: f64,
    pub max: f64,
    /// Rows where the right is bigger (later).
    pub up: usize,
    /// The median of right / left − 1, where the left isn't 0 (numbers only).
    pub median_ratio: Option<f64>,
}

/// What one compared column's differences look like, over every changed row.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ColumnProfile {
    pub name: String,
    pub left_type: String,
    pub right_type: String,
    /// Changed rows where it differs…
    pub differs: usize,
    /// …and where it's the only column that does.
    pub alone: usize,
    /// NULL on one side only.
    pub null_on_one_side: usize,
    /// The same text but for case.
    pub case_only: usize,
    /// The same text but for spaces at the ends or repeated.
    pub spaces_only: usize,
    /// Both numbers.
    pub numbers: Option<Gaps>,
    /// Both dates or times, in seconds.
    pub times: Option<Gaps>,
}

/// The whole difference in numbers: no value of any row.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Profile {
    pub same: u64,
    pub changed: usize,
    pub only_left: usize,
    pub only_right: usize,
    pub partial: bool,
    pub key: Vec<String>,
    /// Only the columns that differ somewhere, the most often first.
    pub columns: Vec<ColumnProfile>,
    /// Columns that differ in the same rows, the most often first.
    pub together: Vec<(String, String, usize)>,
    /// Of the rows only on the left, the share whose key comes after the
    /// right's last key (a one-column key of numbers or dates).
    pub only_left_after_right: Option<f64>,
    pub only_right_after_left: Option<f64>,
}

/// Seconds since 1970 for a date, a date and time, or RFC 3339 text.
fn seconds(v: &Value) -> Option<f64> {
    let t = v.as_str()?.trim();
    if t.len() < 8 || !t.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        return Some(d.timestamp_millis() as f64 / 1000.0);
    }
    for f in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(d) = NaiveDateTime::parse_from_str(t, f) {
            return Some(d.and_utc().timestamp_millis() as f64 / 1000.0);
        }
    }
    NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc().timestamp() as f64)
}

fn gaps(values: &[f64], ratios: &mut [f64]) -> Option<Gaps> {
    if values.is_empty() {
        return None;
    }
    // Gaps that differ by less than a millionth count as the same one.
    let mut freq: HashMap<i64, usize> = HashMap::new();
    for v in values {
        *freq.entry((v * 1e6).round() as i64).or_default() += 1;
    }
    let (mode, mode_rows) = freq
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(k, n)| (k as f64 / 1e6, n))
        .unwrap_or_default();
    let median_ratio = (!ratios.is_empty()).then(|| {
        ratios.sort_by(f64::total_cmp);
        ratios[ratios.len() / 2]
    });
    Some(Gaps {
        count: values.len(),
        mode,
        mode_rows,
        min: values.iter().copied().fold(f64::INFINITY, f64::min),
        max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        up: values.iter().filter(|v| **v > 0.0).count(),
        median_ratio,
    })
}

fn squeezed(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Of `rows`, the share whose key (column 0) comes after `last`.
fn after(rows: &[Vec<Value>], last: &Option<Value>) -> Option<f64> {
    let last = last.as_ref()?;
    // Only keys that order by value: numbers, or dates.
    if number_of(last).is_none() && seconds(last).is_none() {
        return None;
    }
    if rows.is_empty() {
        return None;
    }
    let past = rows
        .iter()
        .filter(|r| {
            r.first().is_some_and(|k| {
                !k.is_null()
                    && sort_value(k).cmp_with(&sort_value(last), false)
                        == std::cmp::Ordering::Greater
            })
        })
        .count();
    Some(past as f64 / rows.len() as f64)
}

pub(crate) fn profile(d: &Diff) -> Profile {
    let n = d.columns.len();
    let mut cols: Vec<ColumnProfile> = d
        .columns
        .iter()
        .enumerate()
        .map(|(j, (l, _))| ColumnProfile {
            name: l.clone(),
            left_type: d.left_types.get(j).cloned().unwrap_or_default(),
            right_type: d.right_types.get(j).cloned().unwrap_or_default(),
            ..Default::default()
        })
        .collect();
    let mut nums: Vec<Vec<f64>> = vec![Vec::new(); n];
    let mut ratios: Vec<Vec<f64>> = vec![Vec::new(); n];
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); n];
    let mut together: HashMap<(usize, usize), usize> = HashMap::new();
    let boolean: Vec<bool> = (0..n)
        .map(|j| {
            d.left_types.get(j).is_some_and(|t| is_bool_type(t))
                || d.right_types.get(j).is_some_and(|t| is_bool_type(t))
        })
        .collect();
    for ChangedRow { left, right, diff } in &d.changed {
        // Pairs grow fast with the columns: past eight in a row, it's "everything".
        if diff.len() <= 8 {
            for (x, a) in diff.iter().enumerate() {
                for b in &diff[x + 1..] {
                    *together.entry((*a.min(b), *a.max(b))).or_default() += 1;
                }
            }
        }
        for &j in diff {
            let (Some(lv), Some(rv), Some(c)) = (left.get(j), right.get(j), cols.get_mut(j)) else {
                continue;
            };
            c.differs += 1;
            if diff.len() == 1 {
                c.alone += 1;
            }
            if lv.is_null() != rv.is_null() {
                c.null_on_one_side += 1;
                continue;
            }
            if !boolean[j] {
                if let (Some(a), Some(b)) = (number_of(lv), number_of(rv)) {
                    nums[j].push(b - a);
                    if a != 0.0 {
                        ratios[j].push(b / a - 1.0);
                    }
                    continue;
                }
            }
            if let (Some(a), Some(b)) = (seconds(lv), seconds(rv)) {
                times[j].push(b - a);
                continue;
            }
            let (a, b) = (canon(lv), canon(rv));
            if a.to_lowercase() == b.to_lowercase() {
                c.case_only += 1;
            } else if squeezed(&a) == squeezed(&b) {
                c.spaces_only += 1;
            }
        }
    }
    for j in 0..n {
        cols[j].numbers = gaps(&nums[j], &mut ratios[j]);
        cols[j].times = gaps(&times[j], &mut []);
    }
    cols.retain(|c| c.differs > 0);
    cols.sort_by_key(|c| std::cmp::Reverse(c.differs));
    let mut pairs: Vec<(String, String, usize)> = together
        .into_iter()
        .map(|((a, b), k)| (d.columns[a].0.clone(), d.columns[b].0.clone(), k))
        .collect();
    pairs.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    pairs.truncate(8);
    let (left_last, right_last) = d.last_keys.clone().unwrap_or((None, None));
    Profile {
        same: d.same,
        changed: d.changed.len(),
        only_left: d.only_left.len(),
        only_right: d.only_right.len(),
        partial: d.partial,
        key: d.key.clone(),
        columns: cols,
        together: pairs,
        only_left_after_right: d
            .last_keys
            .as_ref()
            .and_then(|_| after(&d.only_left, &right_last)),
        only_right_after_left: d
            .last_keys
            .as_ref()
            .and_then(|_| after(&d.only_right, &left_last)),
    }
}

fn num(f: f64) -> String {
    let s = format!("{:+.6}", f);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "+" || s == "-" || s == "-0" || s == "+0" {
        "0".into()
    } else {
        s.to_string()
    }
}

fn span(secs: f64) -> String {
    let a = secs.abs();
    let sign = if secs < 0.0 { "-" } else { "+" };
    if a >= 86_400.0 && (a % 86_400.0).abs() < 1e-6 {
        format!("{sign}{} d", a / 86_400.0)
    } else if a >= 3_600.0 && (a % 3_600.0).abs() < 1e-6 {
        format!("{sign}{} h", a / 3_600.0)
    } else {
        format!("{sign}{} s", num(a).trim_start_matches('+'))
    }
}

/// The profile as the model reads it: numbers in plain English, which every
/// model follows. The instructions around it are in the UI language.
pub(crate) fn profile_text(p: &Profile, engines: (&str, &str)) -> String {
    let mut t = String::new();
    let _ = writeln!(t, "left engine: {}; right engine: {}", engines.0, engines.1);
    let _ = writeln!(t, "key: {}", p.key.join(", "));
    let _ = writeln!(
        t,
        "rows: {} the same, {} different, {} only on the left, {} only on the right{}",
        p.same,
        p.changed,
        p.only_left,
        p.only_right,
        if p.partial {
            " (partial: a side had more rows than were read)"
        } else {
            ""
        }
    );
    for c in &p.columns {
        let _ = write!(
            t,
            "column {} ({} / {}): differs in {} rows, the only difference in {}",
            c.name, c.left_type, c.right_type, c.differs, c.alone
        );
        if c.null_on_one_side > 0 {
            let _ = write!(t, "; NULL on one side only in {}", c.null_on_one_side);
        }
        if c.case_only > 0 {
            let _ = write!(t, "; same text but for case in {}", c.case_only);
        }
        if c.spaces_only > 0 {
            let _ = write!(t, "; same text but for spaces in {}", c.spaces_only);
        }
        if let Some(g) = &c.numbers {
            let _ = write!(
                t,
                "; numbers, right minus left: most often {} ({} rows), from {} to {}, right bigger in {} of {}",
                num(g.mode),
                g.mode_rows,
                num(g.min),
                num(g.max),
                g.up,
                g.count
            );
            if let Some(r) = g.median_ratio {
                let _ = write!(t, ", median change {}%", num(r * 100.0));
            }
        }
        if let Some(g) = &c.times {
            let _ = write!(
                t,
                "; dates, right minus left: most often {} ({} rows), from {} to {}, right later in {} of {}",
                span(g.mode),
                g.mode_rows,
                span(g.min),
                span(g.max),
                g.up,
                g.count
            );
        }
        t.push('\n');
    }
    if !p.together.is_empty() {
        let list: Vec<String> = p
            .together
            .iter()
            .map(|(a, b, k)| format!("{a} + {b} in {k} rows"))
            .collect();
        let _ = writeln!(
            t,
            "columns that differ in the same rows: {}",
            list.join("; ")
        );
    }
    if let Some(s) = p.only_left_after_right {
        let _ = writeln!(
            t,
            "rows only on the left whose key comes after the right's last key: {}%",
            (s * 100.0).round()
        );
    }
    if let Some(s) = p.only_right_after_left {
        let _ = writeln!(
            t,
            "rows only on the right whose key comes after the left's last key: {}%",
            (s * 100.0).round()
        );
    }
    t
}

fn with_diff<T>(id: &str, f: impl FnOnce(&Diff) -> T) -> CommandResult<T> {
    let all = diffs().lock().unwrap();
    let d = all.get(id).ok_or_else(|| {
        CommandError::NotFound("la comparación ya no está: compará de nuevo".into())
    })?;
    Ok(f(d))
}

fn engine_name(state: &AppState, side: &Side) -> String {
    driver_of(state, &side.connection_id)
        .map(|d| d.info().name.to_string())
        .unwrap_or_default()
}

/// The company's AI policy on the compare's connections, before anything of
/// theirs goes to a model.
fn ai_allowed(state: &AppState, sides: &[&Side]) -> CommandResult<()> {
    for s in sides {
        crate::enterprise::refuse_ai_if_not_allowed(&state.store, &s.connection_id)?;
    }
    Ok(())
}

// -- explain -----------------------------------------------------------------

#[derive(Deserialize)]
pub struct ExplainArgs {
    pub id: String,
    pub asker: Asker,
}

#[derive(Serialize)]
pub struct Explanation {
    /// Markdown.
    pub text: String,
}

/// What the differences look like and their likely cause, in words, from the
/// profile of all of them.
#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare_explain(
    state: State<'_, AppState>,
    ai: State<'_, AiRuntime>,
    args: ExplainArgs,
) -> CommandResult<Explanation> {
    let (p, sides) = with_diff(&args.id, |d| {
        (profile(d), (d.left.clone(), d.right.clone()))
    })?;
    ai_allowed(&state, &[&sides.0, &sides.1])?;
    let engines = (engine_name(&state, &sides.0), engine_name(&state, &sides.1));
    let prompts = args.asker.prompts();
    let text = ai_task::fit(
        &profile_text(&p, (&engines.0, &engines.1)),
        args.asker.budget(),
        prompts,
        "task.more_lines",
    );
    let run = ai_task::Run::start(&args.asker.run_id);
    let text = ai_task::ask(
        &ai,
        &args.asker,
        &run.cancel,
        prompts.text("datacompare.explain_system").to_string(),
        text,
        |answer| {
            let a = answer.trim();
            if a.is_empty() {
                Err("the answer is empty".into())
            } else {
                Ok(a.to_string())
            }
        },
    )
    .await?;
    Ok(Explanation { text })
}

// -- filters in words ----------------------------------------------------------

#[derive(Deserialize)]
pub struct AiFilterArgs {
    pub id: String,
    pub kind: RowKind,
    /// What the user typed.
    pub request: String,
    pub asker: Asker,
}

#[derive(Serialize, Debug, Default, PartialEq)]
pub struct AiFilter {
    pub filters: Vec<ColumnFilter>,
    /// Changed rows: the columns that have to differ.
    pub changed_in: Vec<String>,
    pub sort: Vec<SortKey>,
    /// What the AI couldn't express with these filters.
    pub note: Option<String>,
}

#[derive(Deserialize)]
struct FilterAnswer {
    #[serde(default)]
    filters: Vec<FilterItem>,
    #[serde(default)]
    changed_in: Vec<String>,
    #[serde(default)]
    sort: Vec<SortItem>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
struct FilterItem {
    column: String,
    op: FilterOp,
    #[serde(default)]
    values: Vec<Value>,
}

#[derive(Deserialize)]
struct SortItem {
    column: String,
    #[serde(default)]
    descending: bool,
}

/// The operators the AI may use, with how many values each takes.
const OPS: &[(FilterOp, &str, std::ops::RangeInclusive<usize>)] = &[
    (FilterOp::Eq, "eq", 1..=1),
    (FilterOp::Ne, "ne", 1..=1),
    (FilterOp::Gt, "gt", 1..=1),
    (FilterOp::Ge, "ge", 1..=1),
    (FilterOp::Lt, "lt", 1..=1),
    (FilterOp::Le, "le", 1..=1),
    (FilterOp::Contains, "contains", 1..=1),
    (FilterOp::NotContains, "not_contains", 1..=1),
    (FilterOp::StartsWith, "starts_with", 1..=1),
    (FilterOp::EndsWith, "ends_with", 1..=1),
    (FilterOp::IsNull, "is_null", 0..=0),
    (FilterOp::NotNull, "not_null", 0..=0),
    (FilterOp::IsEmpty, "is_empty", 0..=0),
    (FilterOp::NotEmpty, "not_empty", 0..=0),
    (FilterOp::In, "in", 1..=1000),
    (FilterOp::NotIn, "not_in", 1..=1000),
    (FilterOp::IsTrue, "is_true", 0..=0),
    (FilterOp::IsFalse, "is_false", 0..=0),
];

/// The AI's filters on the comparison's own columns, one per column, with the
/// values each operator takes. What doesn't fit is dropped; an answer that
/// isn't the format is an error, so it's asked again.
pub(crate) fn read_filter(
    answer: Value,
    columns: &[String],
    changed: bool,
) -> Result<AiFilter, String> {
    let a: FilterAnswer = serde_json::from_value(answer).map_err(|e| e.to_string())?;
    let column = |name: &str| {
        columns
            .iter()
            .find(|c| c.eq_ignore_ascii_case(name.trim()))
            .cloned()
    };
    let mut out = AiFilter {
        note: a.note.filter(|n| !n.trim().is_empty()),
        ..Default::default()
    };
    for f in a.filters {
        let Some(name) = column(&f.column) else {
            continue;
        };
        let Some((_, _, takes)) = OPS.iter().find(|(op, _, _)| *op == f.op) else {
            continue;
        };
        if !takes.contains(&f.values.len()) || out.filters.iter().any(|x| x.column == name) {
            continue;
        }
        out.filters.push(ColumnFilter {
            column: name,
            op: f.op,
            values: f.values,
            sql: None,
        });
    }
    if changed {
        for c in a.changed_in {
            if let Some(name) = column(&c) {
                if !out.changed_in.contains(&name) {
                    out.changed_in.push(name);
                }
            }
        }
    }
    for s in a.sort {
        if let Some(name) = column(&s.column) {
            if !out.sort.iter().any(|k| k.column == name) {
                out.sort.push(SortKey {
                    column: name,
                    descending: s.descending,
                });
            }
        }
    }
    Ok(out)
}

/// A request in words ("the ones whose price changed, the most expensive
/// first") as the grid's filters, its "differs in" columns and its order.
/// The AI gets the columns' names and types and the words, never a row.
#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare_ai_filter(
    state: State<'_, AppState>,
    ai: State<'_, AiRuntime>,
    args: AiFilterArgs,
) -> CommandResult<AiFilter> {
    let request = args.request.trim();
    if request.is_empty() {
        return Ok(AiFilter::default());
    }
    let (columns, types, sides) = with_diff(&args.id, |d| {
        (
            d.columns.iter().map(|c| c.0.clone()).collect::<Vec<_>>(),
            d.left_types.clone(),
            (d.left.clone(), d.right.clone()),
        )
    })?;
    ai_allowed(&state, &[&sides.0, &sides.1])?;
    let changed = args.kind == RowKind::Changed;
    let mut text = String::new();
    let kind = match args.kind {
        RowKind::Changed => {
            "rows whose values differ (a filter passes if the left or the right value passes)"
        }
        RowKind::OnlyLeft => "rows only on the left",
        RowKind::OnlyRight => "rows only on the right",
    };
    let _ = writeln!(text, "list: {kind}");
    let _ = writeln!(text, "columns:");
    for (i, c) in columns.iter().enumerate() {
        let ty = types.get(i).map(String::as_str).unwrap_or("");
        let _ = writeln!(text, "  {c} {ty}");
    }
    let ops: Vec<&str> = OPS.iter().map(|o| o.1).collect();
    let _ = writeln!(text, "operators: {}", ops.join(", "));
    if !changed {
        let _ = writeln!(text, "changed_in: not available on this list");
    }
    let _ = writeln!(text, "request: {request}");
    let prompts = args.asker.prompts();
    let run = ai_task::Run::start(&args.asker.run_id);
    ai_task::ask(
        &ai,
        &args.asker,
        &run.cancel,
        prompts.text("datacompare.filter_system").to_string(),
        text,
        |answer| read_filter(ai_task::json_in(answer)?, &columns, changed),
    )
    .await
}

// -- a key for a table without one ---------------------------------------------

#[derive(Deserialize)]
pub struct KeyHintArgs {
    pub left: Side,
    /// None: only what the structure says (no AI set up).
    #[serde(default)]
    pub asker: Option<Asker>,
}

#[derive(Serialize, Debug, Default, PartialEq)]
pub struct KeyHint {
    pub key: Vec<String>,
    /// `unique`: a unique index of NOT NULL columns; `ai`; "" when none.
    pub by: String,
}

#[derive(Deserialize)]
struct KeyAnswer {
    #[serde(default)]
    key: Vec<String>,
}

/// The columns that identify each row of a table without a primary key: a
/// unique index whose columns can't be NULL, else what the AI says from the
/// columns' names and types.
#[tauri::command(rename_all = "camelCase")]
pub async fn data_compare_key_hint(
    state: State<'_, AppState>,
    ai: State<'_, AiRuntime>,
    args: KeyHintArgs,
) -> CommandResult<KeyHint> {
    let side = &args.left;
    let tables = super::ai::schema_for(&state, &ai, &side.connection_id, &side.database).await?;
    let schema = side.object.schema().unwrap_or_default();
    let Some(t) = tables.iter().find(|t| {
        t.name.eq_ignore_ascii_case(&side.object.name)
            && t.schema
                .as_deref()
                .unwrap_or_default()
                .eq_ignore_ascii_case(schema)
    }) else {
        return Ok(KeyHint::default());
    };
    let not_null = |c: &String| {
        t.columns
            .iter()
            .any(|x| x.name.eq_ignore_ascii_case(c) && !x.nullable)
    };
    if let Some(ix) = t
        .indexes
        .iter()
        .filter(|ix| ix.unique && ix.filter.is_none() && !ix.columns.is_empty())
        .filter(|ix| ix.columns.iter().all(not_null))
        .min_by_key(|ix| ix.columns.len())
    {
        return Ok(KeyHint {
            key: ix.columns.clone(),
            by: "unique".into(),
        });
    }
    let Some(asker) = args.asker else {
        return Ok(KeyHint::default());
    };
    ai_allowed(&state, &[side])?;
    let mut text = String::new();
    let _ = writeln!(text, "table {}", t.name);
    for c in &t.columns {
        let _ = writeln!(
            text,
            "  {} {}{}",
            c.name,
            c.data_type,
            if c.nullable { "" } else { " NOT NULL" }
        );
    }
    let names: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
    let prompts = asker.prompts();
    let run = ai_task::Run::start(&asker.run_id);
    let key = ai_task::ask(
        &ai,
        &asker,
        &run.cancel,
        prompts.text("datacompare.key_system").to_string(),
        text,
        |answer| {
            let a: KeyAnswer =
                serde_json::from_value(ai_task::json_in(answer)?).map_err(|e| e.to_string())?;
            let mut key = Vec::new();
            for k in a.key {
                match names.iter().find(|n| n.eq_ignore_ascii_case(k.trim())) {
                    Some(n) if !key.contains(n) => key.push(n.clone()),
                    Some(_) => {}
                    None => return Err(format!("{k} isn't a column of the table")),
                }
            }
            Ok(key)
        },
    )
    .await?;
    Ok(KeyHint {
        by: if key.is_empty() { "" } else { "ai" }.into(),
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;
    use serde_json::json;

    fn side() -> Side {
        Side {
            connection_id: "c".into(),
            database: "d".into(),
            object: ObjectRef {
                kind: "table".into(),
                schema: None,
                name: "t".into(),
            },
        }
    }

    fn diff(changed: Vec<ChangedRow>) -> Diff {
        Diff {
            left: side(),
            right: side(),
            key: vec!["id".into()],
            columns: ["id", "updated_at", "total", "name", "active"]
                .iter()
                .map(|c| (c.to_string(), c.to_string()))
                .collect(),
            left_identity: Vec::new(),
            right_identity: Vec::new(),
            only_left: vec![vec![json!(11)], vec![json!(12)], vec![json!(3)]],
            only_right: Vec::new(),
            changed,
            left_types: ["int", "timestamp", "numeric(10,2)", "text", "boolean"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            right_types: ["int", "datetime", "decimal(10,2)", "varchar(50)", "bit"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            partial: false,
            same: 100,
            last_keys: Some((Some(json!(12)), Some(json!(10)))),
        }
    }

    fn row(left: Value, right: Value, diff: Vec<usize>) -> ChangedRow {
        let cells = |v: Value| v.as_array().unwrap().clone();
        ChangedRow {
            left: cells(left),
            right: cells(right),
            diff,
        }
    }

    #[test]
    fn the_profile_counts_each_columns_differences_without_values() {
        let d = diff(vec![
            // Three hours later, alone.
            row(
                json!([1, "2024-01-01 10:00:00", "5.00", "Ana", true]),
                json!([1, "2024-01-01T13:00:00", "5.00", "Ana", true]),
                vec![1],
            ),
            row(
                json!([2, "2024-01-02 10:00:00", "5.00", "Bo", true]),
                json!([2, "2024-01-02 13:00:00", "5.50", "bo", false]),
                vec![1, 2, 3, 4],
            ),
            row(
                json!([3, "2024-01-03 10:00:00", "10", " Eva  Paz", true]),
                json!([3, "2024-01-03 13:00:00", "11", "Eva Paz", true]),
                vec![1, 2, 3],
            ),
            row(
                json!([4, null, "1", "x", true]),
                json!([4, "2024-01-01 00:00:00", "1", "x", true]),
                vec![1],
            ),
        ]);
        let p = profile(&d);
        assert_eq!((p.same, p.changed, p.only_left), (100, 4, 3));
        let col = |n: &str| p.columns.iter().find(|c| c.name == n).unwrap();
        let at = col("updated_at");
        assert_eq!((at.differs, at.alone, at.null_on_one_side), (4, 2, 1));
        let t = at.times.as_ref().unwrap();
        assert_eq!((t.mode, t.mode_rows, t.up, t.count), (10_800.0, 3, 3, 3));
        let total = col("total").numbers.as_ref().unwrap();
        assert_eq!((total.min, total.max, total.up), (0.5, 1.0, 2));
        assert!((total.median_ratio.unwrap() - 0.1).abs() < 1e-9);
        let name = col("name");
        assert_eq!((name.case_only, name.spaces_only), (1, 1));
        // A boolean isn't a number that went down by one.
        assert!(col("active").numbers.is_none());
        assert_eq!(p.columns[0].name, "updated_at");
        assert!(p
            .together
            .contains(&("updated_at".into(), "total".into(), 2)));
        // Keys 11 and 12 come after the right's last (10); 3 doesn't.
        assert_eq!(
            p.only_left_after_right.map(|s| (s * 100.0).round()),
            Some(67.0)
        );

        let text = profile_text(&p, ("PostgreSQL", "MySQL"));
        assert!(text.contains(
            "rows: 100 the same, 4 different, 3 only on the left, 0 only on the right\n"
        ));
        assert!(text.contains("most often +3 h (3 rows)"), "{text}");
        assert!(
            text.contains("rows only on the left whose key comes after the right's last key: 67%")
        );
        // No value of any row goes.
        for v in ["Ana", "Eva", "2024-01", "5.50"] {
            assert!(!text.contains(v), "{v} in {text}");
        }
    }

    #[test]
    fn filters_in_words_keep_only_what_the_comparison_has() {
        let cols: Vec<String> = ["id", "price", "name"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let answer = json!({
            "filters": [
                {"column": "PRICE", "op": "gt", "values": [100]},
                {"column": "price", "op": "lt", "values": [5]},
                {"column": "nope", "op": "eq", "values": [1]},
                {"column": "name", "op": "is_null", "values": ["x"]},
                {"column": "id", "op": "in", "values": [1, 2]}
            ],
            "changed_in": ["price", "ghost"],
            "sort": [{"column": "price", "descending": true}],
            "note": "a change in percent can't be filtered"
        });
        let f = read_filter(answer.clone(), &cols, true).unwrap();
        assert_eq!(f.filters.len(), 2);
        assert_eq!(
            (f.filters[0].column.as_str(), f.filters[0].op),
            ("price", FilterOp::Gt)
        );
        assert_eq!(f.filters[1].op, FilterOp::In);
        assert_eq!(f.changed_in, vec!["price".to_string()]);
        assert!(f.sort[0].descending);
        assert!(f.note.is_some());
        // "differs in" only means something on changed rows.
        assert!(read_filter(answer, &cols, false)
            .unwrap()
            .changed_in
            .is_empty());
        // SQL conditions aren't the AI's to write, and a wrong shape is asked again.
        let sql = json!({"filters": [{"column": "id", "op": "sql", "values": []}]});
        assert!(read_filter(sql, &cols, true).unwrap().filters.is_empty());
        assert!(read_filter(json!({"filters": "x"}), &cols, true).is_err());
    }

    #[test]
    fn dates_and_times_read_in_the_usual_shapes() {
        let s = |t: &str| seconds(&json!(t));
        assert_eq!(s("1970-01-02"), Some(86_400.0));
        assert_eq!(s("1970-01-01 01:00:00"), Some(3_600.0));
        assert_eq!(s("1970-01-01T01:00:00.500"), Some(3_600.5));
        assert_eq!(s("1970-01-01T01:00:00+01:00"), Some(0.0));
        assert_eq!(s("hello"), None);
        assert_eq!(s("12345"), None);
        assert_eq!(span(-7_200.0), "-2 h");
        assert_eq!(span(90.0), "+90 s");
        assert_eq!(num(0.0), "0");
        assert_eq!(num(1.5), "+1.5");
    }
}
