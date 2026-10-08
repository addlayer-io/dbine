//! What BigQuery's metadata already knows about the session dataset's
//! objects ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Only the REST API's `tables.get` and `routines.get`: free metadata
//! calls, no query job (`INFORMATION_SCHEMA` and `__TABLES__` are queries,
//! billed like any other). `tables.list`/`routines.list` don't carry row
//! counts or descriptions, so each object is read on its own, a few at a
//! time, up to [`MAX_OBJECTS`].
//!
//! - Rows: `numRows` of tables (snapshots and clones too) and materialized
//!   views, from the storage metadata. Views and external tables have none.
//! - Comments: the `description` of views, materialized views, functions
//!   and procedures.

use crate::{Api, BigQuerySession};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};
use serde_json::Value as Json;

/// Objects read one by one at most.
const MAX_OBJECTS: usize = 2000;
/// Concurrent `*.get` calls.
const PARALLEL: usize = 8;

/// A table resource's kind, as `list_objects` gives it.
pub(crate) fn table_kind(t: &Json) -> &'static str {
    match t.get("type").and_then(Json::as_str) {
        Some("VIEW") => kinds::VIEW,
        Some("MATERIALIZED_VIEW") => kinds::MATERIALIZED_VIEW,
        _ => kinds::TABLE,
    }
}

fn routine_kind(r: &Json) -> &'static str {
    if r.get("routineType").and_then(Json::as_str) == Some("PROCEDURE") {
        kinds::PROCEDURE
    } else {
        kinds::FUNCTION
    }
}

/// `numRows` (an int64 as a string) of a table resource.
pub(crate) fn num_rows(t: &Json) -> Option<u64> {
    match t.get("numRows")? {
        Json::String(s) => s.parse().ok(),
        v => v.as_u64(),
    }
}

fn description(v: &Json) -> Option<String> {
    v.get("description").and_then(Json::as_str).map(str::trim).filter(|d| !d.is_empty()).map(str::to_string)
}

/// Each `(collection, id)` read with `<collection>.get`, [`PARALLEL`] at a
/// time; one that fails is skipped.
async fn get_each(api: &Api, ds: &str, collection: &'static str, ids: Vec<String>) -> Vec<Json> {
    let mut out = Vec::new();
    for chunk in ids.chunks(PARALLEL) {
        let mut set = tokio::task::JoinSet::new();
        for id in chunk {
            let (api, ds, id) = (api.clone(), ds.to_string(), id.clone());
            set.spawn(async move { api.get(&["datasets", &ds, collection, &id], &[]).await });
        }
        while let Some(r) = set.join_next().await {
            if let Ok(Ok(v)) = r {
                out.push(v);
            }
        }
    }
    out
}

/// The ids of the session dataset's tables of `kinds`.
async fn tables_of(api: &Api, ds: &str, want: &[&str]) -> Vec<String> {
    let list = api.list_all(&["datasets", ds, "tables"], "tables").await.unwrap_or_default();
    list.iter()
        .filter(|t| want.contains(&table_kind(t)))
        .filter_map(|t| t.pointer("/tableReference/tableId").and_then(Json::as_str).map(str::to_string))
        .take(MAX_OBJECTS)
        .collect()
}

fn table_ref(kind: &str, t: &Json) -> Option<ObjectRef> {
    Some(ObjectRef { kind: kind.into(), schema: None, name: t.pointer("/tableReference/tableId")?.as_str()?.to_string() })
}

pub(crate) fn estimate(t: &Json) -> Option<RowEstimate> {
    let kind = table_kind(t);
    if kind == kinds::VIEW {
        return None;
    }
    Some(RowEstimate { object: table_ref(kind, t)?, rows: num_rows(t)? })
}

pub(crate) fn table_comment(t: &Json) -> Option<ObjectComment> {
    let kind = table_kind(t);
    if kind == kinds::TABLE {
        return None;
    }
    Some(ObjectComment { object: table_ref(kind, t)?, comment: description(t)? })
}

pub(crate) fn routine_comment(r: &Json) -> Option<ObjectComment> {
    let name = r.pointer("/routineReference/routineId")?.as_str()?.to_string();
    Some(ObjectComment { object: ObjectRef { kind: routine_kind(r).into(), schema: None, name }, comment: description(r)? })
}

fn sorted<T>(mut v: Vec<T>, key: impl Fn(&T) -> &ObjectRef) -> Vec<T> {
    v.sort_by(|a, b| (&key(a).kind, &key(a).name).cmp(&(&key(b).kind, &key(b).name)));
    v
}

pub(crate) async fn row_estimates(s: &BigQuerySession) -> Result<Vec<RowEstimate>> {
    let Some(ds) = s.dataset.clone() else { return Ok(Vec::new()) };
    let ids = tables_of(&s.api, &ds, &[kinds::TABLE, kinds::MATERIALIZED_VIEW]).await;
    let tables = get_each(&s.api, &ds, "tables", ids).await;
    Ok(sorted(tables.iter().filter_map(estimate).collect(), |e| &e.object))
}

pub(crate) async fn object_comments(s: &BigQuerySession) -> Result<Vec<ObjectComment>> {
    let Some(ds) = s.dataset.clone() else { return Ok(Vec::new()) };
    let ids = tables_of(&s.api, &ds, &[kinds::VIEW, kinds::MATERIALIZED_VIEW]).await;
    let mut out: Vec<ObjectComment> = get_each(&s.api, &ds, "tables", ids).await.iter().filter_map(table_comment).collect();
    // Emulators may not implement routines.
    if let Ok(list) = s.api.list_all(&["datasets", &ds, "routines"], "routines").await {
        let ids: Vec<String> = list
            .iter()
            .filter_map(|r| r.pointer("/routineReference/routineId").and_then(Json::as_str).map(str::to_string))
            .take(MAX_OBJECTS)
            .collect();
        out.extend(get_each(&s.api, &ds, "routines", ids).await.iter().filter_map(routine_comment));
    }
    Ok(sorted(out, |c| &c.object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_of_tables_and_materialized_views() {
        let t = json!({"type": "TABLE", "tableReference": {"tableId": "ventas"}, "numRows": "1200"});
        let e = estimate(&t).unwrap();
        assert_eq!((e.object.kind.as_str(), e.object.name.as_str(), e.rows), ("table", "ventas", 1200));
        let mv = json!({"type": "MATERIALIZED_VIEW", "tableReference": {"tableId": "mv"}, "numRows": "3"});
        assert_eq!(estimate(&mv).unwrap().object.kind, kinds::MATERIALIZED_VIEW);
        assert!(estimate(&json!({"type": "VIEW", "tableReference": {"tableId": "v"}, "numRows": "0"})).is_none());
        assert!(estimate(&json!({"type": "EXTERNAL", "tableReference": {"tableId": "x"}})).is_none());
    }

    #[test]
    fn descriptions() {
        let v = json!({"type": "VIEW", "tableReference": {"tableId": "v"}, "description": "ventas por día"});
        assert_eq!(table_comment(&v).unwrap().comment, "ventas por día");
        assert!(table_comment(&json!({"type": "TABLE", "tableReference": {"tableId": "t"}, "description": "x"})).is_none());
        assert!(table_comment(&json!({"type": "VIEW", "tableReference": {"tableId": "v"}, "description": " "})).is_none());
        let r = json!({"routineReference": {"routineId": "p"}, "routineType": "PROCEDURE", "description": "carga"});
        let c = routine_comment(&r).unwrap();
        assert_eq!((c.object.kind.as_str(), c.object.name.as_str()), ("procedure", "p"));
    }
}
