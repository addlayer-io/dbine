//! Reading the Scheduler's dictionary views by column name, whatever
//! columns the version has: each view is described first and read with
//! its timestamps as text (with their time zone in `<COLUMN>_TZR`), its
//! intervals as seconds and its object columns (ANYDATA) only as whether
//! they're set. The client never sees a time zone region (it panics on
//! some).

use crate::cell;
use crate::monitor::txt;
use oracledb::{Connection, ToDbValue};
use std::collections::HashMap;

/// A row by upper-case column name; null columns are missing.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Rec(pub(crate) HashMap<String, String>);

impl Rec {
    /// The value as stored.
    pub(crate) fn raw(&self, k: &str) -> Option<String> {
        self.0.get(k).cloned()
    }

    /// The value trimmed, `None` when blank.
    pub(crate) fn t(&self, k: &str) -> Option<String> {
        self.0
            .get(k)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// `TRUE`, `YES` or `Y`.
    pub(crate) fn yes(&self, k: &str) -> bool {
        matches!(self.t(k).as_deref(), Some("TRUE" | "YES" | "Y"))
    }

    pub(crate) fn int(&self, k: &str) -> Option<i64> {
        let t = self.t(k)?;
        t.parse::<i64>()
            .ok()
            .or_else(|| t.parse::<f64>().ok().map(|f| f.round() as i64))
    }

    #[cfg(test)]
    pub(crate) fn of(pairs: &[(&str, &str)]) -> Rec {
        Rec(pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect())
    }
}

/// A column of a view, quoted.
fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The select list for a view's columns (name, client type).
pub(crate) fn select_list(cols: &[(String, String)]) -> String {
    let mut out = Vec::new();
    for (name, ty) in cols {
        let c = q(name);
        match ty.as_str() {
            "DB_TYPE_TIMESTAMP_TZ" | "DB_TYPE_TIMESTAMP_LTZ" => {
                out.push(format!("TO_CHAR({c}, 'YYYY-MM-DD\"T\"HH24:MI:SS') AS {c}"));
                out.push(format!("TO_CHAR({c}, 'TZR') AS {}", q(&format!("{name}_TZR"))));
            }
            "DB_TYPE_TIMESTAMP" | "DB_TYPE_DATE" => {
                out.push(format!("TO_CHAR({c}, 'YYYY-MM-DD\"T\"HH24:MI:SS') AS {c}"));
            }
            "DB_TYPE_INTERVAL_DS" => out.push(format!(
                "ROUND(EXTRACT(DAY FROM {c}) * 86400 + EXTRACT(HOUR FROM {c}) * 3600 + EXTRACT(MINUTE FROM {c}) * 60 + EXTRACT(SECOND FROM {c})) AS {c}"
            )),
            "DB_TYPE_OBJECT" | "DB_TYPE_BLOB" | "DB_TYPE_BFILE" | "DB_TYPE_RAW"
            | "DB_TYPE_LONG_RAW" | "DB_TYPE_CURSOR" | "DB_TYPE_INTERVAL_YM" => {
                out.push(format!("CASE WHEN {c} IS NULL THEN NULL ELSE 'Y' END AS {c}"))
            }
            _ => out.push(c),
        }
    }
    out.join(", ")
}

/// The columns of `view` (a name this module writes, never the app's).
fn describe(c: &Connection, view: &str) -> Result<Vec<(String, String)>, oracledb::Error> {
    let cursor = c
        .statement(&format!("SELECT * FROM {view} WHERE 1 = 0"))?
        .exclude_from_cache()
        .build()?
        .query(&[])?;
    Ok(cursor
        .columns()
        .iter()
        .map(|m| (m.name().to_string(), m.db_type().name().to_string()))
        .collect())
}

/// Rows of `view` followed by `tail` (`WHERE …`, binds `:1`… in `params`),
/// at most `max`.
pub(crate) fn rows(
    c: &Connection,
    view: &str,
    tail: &str,
    params: &[&dyn ToDbValue],
    max: usize,
) -> Result<Vec<Rec>, oracledb::Error> {
    let cols = describe(c, view)?;
    let sql = format!("SELECT {} FROM {view} {tail}", select_list(&cols));
    let cursor = c
        .statement(&sql)?
        .exclude_from_cache()
        .build()?
        .query(params)?;
    let meta: Vec<(String, &'static oracledb::DbType)> = cursor
        .columns()
        .iter()
        .map(|m| (m.name().to_string(), m.db_type()))
        .collect();
    let mut out = Vec::new();
    for row in cursor {
        let row = row?;
        let mut rec = HashMap::new();
        for (i, (name, ty)) in meta.iter().enumerate() {
            if let Some(v) = txt(&cell(&row, i, ty)) {
                rec.insert(name.to_ascii_uppercase(), v);
            }
        }
        out.push(Rec(rec));
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

/// One row, `None` when there's none.
pub(crate) fn row(
    c: &Connection,
    view: &str,
    tail: &str,
    params: &[&dyn ToDbValue],
) -> Result<Option<Rec>, oracledb::Error> {
    Ok(rows(c, view, tail, params, 1)?.into_iter().next())
}

/// The column names `view` has here.
pub(crate) fn columns(
    c: &Connection,
    view: &str,
) -> Result<std::collections::HashSet<String>, oracledb::Error> {
    Ok(describe(c, view)?.into_iter().map(|(n, _)| n).collect())
}
