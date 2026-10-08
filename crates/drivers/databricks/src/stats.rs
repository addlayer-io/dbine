//! What Unity Catalog already knows about the session catalog's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Only REST calls to the control plane (`/api/2.1/unity-catalog/tables`,
//! `/functions`): nothing runs on the SQL warehouse, so none wakes up or
//! bills DBUs. `information_schema` and `DESCRIBE DETAIL` would need a
//! running warehouse.
//!
//! - Rows: the `spark.sql.statistics.numRows` table property, which
//!   `ANALYZE TABLE … COMPUTE STATISTICS` (or predictive optimization)
//!   leaves on the table. A table never analyzed has none and is left out.
//! - Comments: the `comment` of views, materialized views and functions.
//!
//! The legacy `hive_metastore` catalog isn't in Unity Catalog's API: empty.

use crate::health::escape;
use crate::DatabricksSession;
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};
use serde_json::Value as Json;

/// Objects listed at most, across the catalog's schemas.
const MAX_OBJECTS: usize = 10_000;
const NUM_ROWS: &str = "spark.sql.statistics.numRows";

/// A Unity Catalog `table_type` as the kind `list_objects` gives it.
pub(crate) fn kind_of(table_type: &str) -> &'static str {
    match table_type {
        "VIEW" => kinds::VIEW,
        "MATERIALIZED_VIEW" => kinds::MATERIALIZED_VIEW,
        _ => kinds::TABLE,
    }
}

fn object(kind: &str, v: &Json) -> Option<ObjectRef> {
    Some(ObjectRef {
        kind: kind.into(),
        schema: v.get("schema_name").and_then(Json::as_str).map(str::to_string),
        name: v.get("name")?.as_str()?.to_string(),
    })
}

fn comment(v: &Json) -> Option<String> {
    v.get("comment").and_then(Json::as_str).map(str::trim).filter(|c| !c.is_empty()).map(str::to_string)
}

pub(crate) fn estimate(t: &Json) -> Option<RowEstimate> {
    let kind = kind_of(t.get("table_type").and_then(Json::as_str).unwrap_or_default());
    if kind == kinds::VIEW {
        return None;
    }
    let rows = t.get("properties")?.get(NUM_ROWS)?.as_str()?.trim().parse().ok()?;
    Some(RowEstimate { object: object(kind, t)?, rows })
}

pub(crate) fn table_comment(t: &Json) -> Option<ObjectComment> {
    let kind = kind_of(t.get("table_type").and_then(Json::as_str).unwrap_or_default());
    if kind == kinds::TABLE {
        return None;
    }
    Some(ObjectComment { object: object(kind, t)?, comment: comment(t)? })
}

pub(crate) fn function_comment(f: &Json) -> Option<ObjectComment> {
    Some(ObjectComment { object: object(kinds::FUNCTION, f)?, comment: comment(f)? })
}

impl DatabricksSession {
    /// Every item of `what` (`tables`, `functions`) in each schema of the
    /// session catalog, but information_schema. A schema that can't be
    /// listed is skipped.
    async fn uc_objects(&self, what: &str) -> Vec<Json> {
        let Some(cat) = self.catalog.clone() else { return Vec::new() };
        let Ok(schemas) = self.uc_list(&format!("/api/2.1/unity-catalog/schemas?catalog_name={}", escape(&cat)), "schemas", 10_000).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for s in schemas.iter().filter_map(|s| s.get("name").and_then(Json::as_str)).filter(|s| *s != "information_schema") {
            let extra = if what == "tables" { "&omit_columns=true" } else { "" };
            let path = format!(
                "/api/2.1/unity-catalog/{what}?catalog_name={}&schema_name={}&max_results=1000{extra}",
                escape(&cat),
                escape(s)
            );
            if let Ok(items) = self.uc_list(&path, what, MAX_OBJECTS).await {
                out.extend(items);
            }
            if out.len() >= MAX_OBJECTS {
                break;
            }
        }
        out
    }
}

pub(crate) async fn row_estimates(s: &DatabricksSession) -> Result<Vec<RowEstimate>> {
    Ok(s.uc_objects("tables").await.iter().filter_map(estimate).collect())
}

pub(crate) async fn object_comments(s: &DatabricksSession) -> Result<Vec<ObjectComment>> {
    let mut out: Vec<ObjectComment> = s.uc_objects("tables").await.iter().filter_map(table_comment).collect();
    out.extend(s.uc_objects("functions").await.iter().filter_map(function_comment));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_from_statistics() {
        let t = json!({"name": "ventas", "schema_name": "default", "table_type": "MANAGED",
            "properties": {"spark.sql.statistics.numRows": "1200", "delta.minReaderVersion": "1"}});
        let e = estimate(&t).unwrap();
        assert_eq!((e.object.kind.as_str(), e.object.schema.as_deref(), e.object.name.as_str(), e.rows), ("table", Some("default"), "ventas", 1200));
        assert!(estimate(&json!({"name": "nunca", "schema_name": "default", "table_type": "MANAGED", "properties": {}})).is_none());
        let mv = json!({"name": "mv", "schema_name": "default", "table_type": "MATERIALIZED_VIEW", "properties": {"spark.sql.statistics.numRows": "3"}});
        assert_eq!(estimate(&mv).unwrap().object.kind, kinds::MATERIALIZED_VIEW);
    }

    #[test]
    fn comments_of_views_and_functions() {
        let v = json!({"name": "v", "schema_name": "default", "table_type": "VIEW", "comment": "ventas por día"});
        assert_eq!(table_comment(&v).unwrap().object.kind, kinds::VIEW);
        assert!(table_comment(&json!({"name": "t", "schema_name": "default", "table_type": "MANAGED", "comment": "x"})).is_none());
        assert!(table_comment(&json!({"name": "v", "schema_name": "default", "table_type": "VIEW"})).is_none());
        let f = function_comment(&json!({"name": "f", "schema_name": "default", "comment": "suma"})).unwrap();
        assert_eq!((f.object.kind.as_str(), f.comment.as_str()), ("function", "suma"));
    }
}
