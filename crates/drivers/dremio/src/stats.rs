//! What Dremio's catalog already keeps on the session context's views
//! ([`dbine_driver::Session::object_comments`]).
//!
//! - Rows: Dremio exposes no row statistics (neither `INFORMATION_SCHEMA`
//!   nor the catalog API carry a count), and counting would run a job that
//!   reads the source: [`dbine_driver::Session::row_estimates`] stays empty.
//! - Comments: Dremio has no `COMMENT`; a dataset's description is its wiki
//!   (`/api/v3/catalog/{id}/collaboration/wiki`). Views (virtual datasets)
//!   get theirs here, two catalog calls each (`by-path` for the id, then
//!   the wiki), up to [`MAX_VIEWS`]; no job runs.

use crate::{encode, text, DremioSession};
use dbine_driver::stats::ObjectComment;
use dbine_driver::{kinds, ObjectRef, Result, Session};
use serde_json::Value;

/// Views whose wiki is read at most.
const MAX_VIEWS: usize = 500;

/// The `by-path` path of `schema.name`: each segment escaped, `/` between.
pub(crate) fn by_path(schema: Option<&str>, name: &str) -> String {
    schema.into_iter().flat_map(|s| s.split('.')).chain([name]).map(encode).collect::<Vec<_>>().join("/")
}

/// A wiki's text, when there is one.
pub(crate) fn wiki_text(v: &Value) -> Option<String> {
    v.get("text").map(text).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

pub(crate) async fn object_comments(s: &mut DremioSession) -> Result<Vec<ObjectComment>> {
    let objs = s.list_objects().await.unwrap_or_default();
    let mut out = Vec::new();
    for o in objs.iter().filter(|o| o.kind == kinds::VIEW).take(MAX_VIEWS) {
        let path = format!("/api/v3/catalog/by-path/{}", by_path(o.schema.as_deref(), &o.name));
        let Ok(e) = s.conn.send(reqwest::Method::GET, &path, None).await else { continue };
        let Some(id) = e.get("id").map(text).filter(|i| !i.is_empty()) else { continue };
        // No wiki: 404.
        let Ok(w) = s.conn.send(reqwest::Method::GET, &format!("/api/v3/catalog/{}/collaboration/wiki", encode(&id)), None).await else {
            continue;
        };
        if let Some(comment) = wiki_text(&w) {
            out.push(ObjectComment { object: ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() }, comment });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths() {
        assert_eq!(by_path(Some("ventas.2024"), "por dia"), "ventas/2024/por%20dia");
        assert_eq!(by_path(None, "v"), "v");
    }

    #[test]
    fn wiki() {
        assert_eq!(wiki_text(&json!({"text": "Ventas por día", "version": 0})).as_deref(), Some("Ventas por día"));
        assert_eq!(wiki_text(&json!({"text": "", "version": 0})), None);
    }
}
