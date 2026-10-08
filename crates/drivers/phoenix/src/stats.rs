//! What Phoenix's statistics already know about the tables
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: `SYSTEM.STATS`, the guide posts that `UPDATE STATISTICS` (and
//!   major compactions) collect per region and column family: the sum of
//!   `GUIDE_POSTS_ROW_COUNT` per physical table, taking the largest column
//!   family. It runs short by the rows after each region's last guide post
//!   (less than one guide post width, 300 MB by default). A table smaller
//!   than that keeps only an empty guide post: a count of 0 that says
//!   nothing, so 0 is left out, as is a table without statistics. No table
//!   is scanned. Views share their table's storage and get none.
//! - Comments: Phoenix keeps none (no `COMMENT`):
//!   [`dbine_driver::Session::object_comments`] stays empty.
//!
//! A generic Avatica server has no `SYSTEM.STATS`: empty.

use crate::{text, PhoenixSession};
use dbine_driver::stats::RowEstimate;
use dbine_driver::{kinds, ObjectRef, Result, Session};
use serde_json::Value;
use std::collections::HashMap;

fn count(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f.round() as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `[PHYSICAL_NAME, COLUMN_FAMILY, SUM(GUIDE_POSTS_ROW_COUNT)]` rows: per
/// physical table, its largest column family's count.
pub(crate) fn per_table(rows: &[Vec<Value>]) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for r in rows.iter().filter(|r| r.len() == 3) {
        let Some(n) = count(&r[2]).filter(|n| *n > 0) else { continue };
        let e = out.entry(text(&r[0])).or_default();
        *e = (*e).max(n);
    }
    out
}

/// A table's physical name: `S.T`, or `S:T` with namespace mapping.
pub(crate) fn physical_names(schema: Option<&str>, name: &str) -> [String; 2] {
    match schema {
        Some(s) => [format!("{s}.{name}"), format!("{s}:{name}")],
        None => [name.to_string(), name.to_string()],
    }
}

pub(crate) async fn row_estimates(s: &mut PhoenixSession) -> Result<Vec<RowEstimate>> {
    if s.generic {
        return Ok(Vec::new());
    }
    let Ok(rows) = s
        .rows(
            "SELECT PHYSICAL_NAME, COLUMN_FAMILY, SUM(GUIDE_POSTS_ROW_COUNT) FROM SYSTEM.STATS
             GROUP BY PHYSICAL_NAME, COLUMN_FAMILY",
        )
        .await
    else {
        return Ok(Vec::new());
    };
    let counts = per_table(&rows);
    if counts.is_empty() {
        return Ok(Vec::new());
    }
    let objs = s.list_objects().await.unwrap_or_default();
    Ok(objs
        .iter()
        .filter(|o| o.kind == kinds::TABLE)
        .filter_map(|o| {
            let rows = physical_names(o.schema.as_deref(), &o.name).iter().find_map(|p| counts.get(p).copied())?;
            Some(RowEstimate { object: ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() }, rows })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn largest_family_per_table() {
        let rows = vec![
            vec![json!("DBINE.VENTAS"), json!("0"), json!(1200)],
            vec![json!("DBINE.VENTAS"), json!("B"), json!(300)],
            vec![json!("T"), json!("0"), Value::Null],
            vec![json!("CHICA"), json!("0"), json!(0)],
        ];
        let m = per_table(&rows);
        assert_eq!(m.get("DBINE.VENTAS"), Some(&1200));
        assert_eq!(m.get("T"), None);
        assert_eq!(m.get("CHICA"), None);
    }

    #[test]
    fn physical() {
        assert_eq!(physical_names(Some("S"), "T"), ["S.T".to_string(), "S:T".to_string()]);
        assert_eq!(physical_names(None, "T")[0], "T");
    }
}
