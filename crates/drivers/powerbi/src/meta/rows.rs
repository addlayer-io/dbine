//! Reading the result sets of the driver's own DAX reads by column name.
//!
//! The query APIs name an `INFO` function's columns `[Name]` (Arrow) or
//! `[Name]` / `Table[Name]` (JSON); `COLUMNSTATISTICS()` says `[Table Name]`.
//! [`Rows`] finds a column by its bare name, case-insensitively, whatever
//! the decoration, and reads cells leniently: a boolean may come as `true`,
//! `"True"` or `1`, a number as a number or its text.

use crate::query::Rowset;
use serde_json::Value;

/// The bare name of a result column: `INFO.VIEW.TABLES[Name]`, `[Name]`,
/// `'T'[Name]` → `Name`.
pub fn bare(column: &str) -> &str {
    let c = column.trim();
    match (c.rfind('['), c.ends_with(']')) {
        (Some(i), true) => &c[i + 1..c.len() - 1],
        _ => c,
    }
}

/// One result set, read by column name.
pub struct Rows<'a> {
    set: &'a Rowset,
    names: Vec<String>,
}

impl<'a> Rows<'a> {
    pub fn new(set: &'a Rowset) -> Self {
        Rows {
            set,
            names: set
                .columns
                .iter()
                .map(|c| bare(c).to_ascii_lowercase())
                .collect(),
        }
    }

    /// The index of `name` (bare, any case).
    pub fn col(&self, name: &str) -> Option<usize> {
        let name = name.to_ascii_lowercase();
        self.names.iter().position(|n| *n == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> + '_ {
        self.set
            .rows
            .iter()
            .map(move |r| Row { rows: self, row: r })
    }
}

/// One row of [`Rows`].
#[derive(Clone, Copy)]
pub struct Row<'a> {
    rows: &'a Rows<'a>,
    row: &'a [Value],
}

impl<'a> Row<'a> {
    pub fn value(&self, name: &str) -> Option<&'a Value> {
        self.rows
            .col(name)
            .and_then(|i| self.row.get(i))
            .filter(|v| !v.is_null())
    }

    /// Text (numbers and booleans as their text); `None` when blank.
    pub fn str(&self, name: &str) -> Option<String> {
        match self.value(name)? {
            Value::String(s) if s.is_empty() => None,
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            other => Some(other.to_string()),
        }
    }

    pub fn i64(&self, name: &str) -> Option<i64> {
        match self.value(name)? {
            Value::Number(n) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
            Value::String(s) => s.trim().parse().ok(),
            Value::Bool(b) => Some(i64::from(*b)),
            _ => None,
        }
    }

    pub fn u64(&self, name: &str) -> Option<u64> {
        self.i64(name).and_then(|n| u64::try_from(n).ok())
    }

    /// `true`, `"True"`, `"TRUE"`, `1`; `false` when blank.
    pub fn bool(&self, name: &str) -> bool {
        match self.value(name) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => {
                let s = s.trim();
                s.eq_ignore_ascii_case("true") || s == "1"
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn columns_are_found_by_their_bare_name() {
        assert_eq!(bare("INFO.VIEW.TABLES[Name]"), "Name");
        assert_eq!(bare("[Table Name]"), "Table Name");
        assert_eq!(bare("'T'[x]"), "x");
        assert_eq!(bare("Plain"), "Plain");
        let set = Rowset {
            columns: vec!["[ID]".into(), "[IsHidden]".into(), "T[Rows]".into()],
            types: vec![],
            rows: vec![
                vec![json!(1), json!("True"), json!("12")],
                vec![json!(2), json!(false), json!(null)],
            ],
            truncated: false,
        };
        let rows = Rows::new(&set);
        let all: Vec<_> = rows.iter().collect();
        assert_eq!(all[0].i64("id"), Some(1));
        assert!(all[0].bool("ISHIDDEN") && !all[1].bool("IsHidden"));
        assert_eq!(all[0].u64("Rows"), Some(12));
        assert_eq!(all[1].u64("Rows"), None);
        assert_eq!(all[0].str("Missing"), None);
    }
}
