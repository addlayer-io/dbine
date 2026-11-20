//! `list_objects_page` (see `dbine_driver::objects` and docs/drivers.md,
//! "Objects by page"): a page of the cluster's indices, aliases and data
//! streams.
//!
//! A page reads only the listings of the kinds it can show, the same ones
//! `list_objects` reads: the indices folder `_cat/indices`, the aliases
//! folder `_cat/aliases`, the data streams folder `_data_stream`, "Otros"
//! none. None of them pages, so what they answer is put in the tree's
//! order and cut here.
//!
//! A search or a lookup that can find indices answers `Unsupported`, and
//! the app pages its whole list (read once and kept a while) as before:
//! the server resolves an index expression (`*text*`) through aliases and
//! data streams under its hidden-index rules, so no expression lists
//! exactly the indices of the whole listing that hold the text (a hidden
//! index reached through a visible alias, a data stream's `.ds-…` backing
//! indices, are in the listing and not in the expression's answer). One
//! that can only find aliases or data streams reads their listing.
//!
//! - **Order:** name ignoring case, then as written, then the kind: what
//!   `objects::page_of` makes of `list_objects`.
//! - **Cursor:** the page's last object (`[schema, name, kind, parent]`);
//!   the next page starts after it (keyset), so pages neither skip nor
//!   repeat objects even when indices come and go in between.

use crate::json::J;
use crate::{EsSession, KIND_ALIAS};
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};

/// The kinds `list_objects` gives, in its order.
const KINDS: &[&str] = &[kinds::INDEX, KIND_ALIAS, kinds::STREAM];

/// Whether the query is answered here (else the app pages the whole list):
/// not a search or a lookup that can find indices.
fn answered(q: &ObjectQuery, wanted: &[&str]) -> bool {
    let search = q.pattern.as_deref().is_some_and(|p| !p.is_empty()) || q.name.is_some();
    !(search && wanted.contains(&kinds::INDEX))
}

impl EsSession {
    /// A page of the indices, aliases and data streams: the same ones as
    /// `list_objects`, reading only the listings of the kinds asked for
    /// (see the module's notes).
    pub(crate) async fn objects_page(&self, q: &ObjectQuery) -> Result<ObjectPage> {
        // No object has a schema.
        if !q.no_schema && q.schema.is_some() {
            return Ok(ObjectPage::default());
        }
        let wanted = wanted(q);
        if !answered(q, &wanted) {
            return Err(Error::Unsupported(
                "este motor todavía no lista sus objetos por páginas".into(),
            ));
        }
        let obj = |kind: &str, name: &str| DbObject {
            kind: kind.into(),
            schema: None,
            name: name.into(),
            parent: None,
        };
        let mut found = Vec::new();
        if wanted.contains(&kinds::INDEX) {
            let indices = self.get_json("/_cat/indices?format=json&h=index").await?;
            for i in indices.as_arr().unwrap_or(&[]) {
                if let Some(n) = i.get("index").and_then(J::as_str) {
                    found.push(obj(kinds::INDEX, n));
                }
            }
        }
        if wanted.contains(&KIND_ALIAS) {
            // Absent or forbidden: none, as in `list_objects`.
            if let Ok(aliases) = self.get_json("/_cat/aliases?format=json&h=alias").await {
                for a in aliases.as_arr().unwrap_or(&[]) {
                    if let Some(n) = a.get("alias").and_then(J::as_str) {
                        found.push(obj(KIND_ALIAS, n));
                    }
                }
            }
        }
        if wanted.contains(&kinds::STREAM) {
            // Absent on old clusters, may be forbidden; none then.
            if let Ok(ds) = self.get_json("/_data_stream").await {
                for d in ds.get("data_streams").and_then(J::as_arr).unwrap_or(&[]) {
                    if let Some(n) = d.get("name").and_then(J::as_str) {
                        found.push(obj(kinds::STREAM, n));
                    }
                }
            }
        }
        found.retain(|o| self.visible(&o.name));
        Ok(page(q, found))
    }
}

// -- the tree's order and cursor ------------------------------------------------------

/// `objects::page_of`'s order (schema, none first, then name, both
/// ignoring case, then as written), then the kind in `list_objects`' order
/// and the parent: what `page_of`'s stable sort keeps of `list_objects`
/// among equal names.
type Key = (bool, String, String, String, String, usize, String);

fn key(o: &DbObject) -> Key {
    let s = o.schema.clone().unwrap_or_default();
    (
        o.schema.is_some(),
        s.to_lowercase(),
        o.name.to_lowercase(),
        s,
        o.name.clone(),
        KINDS
            .iter()
            .position(|k| *k == o.kind)
            .unwrap_or(KINDS.len()),
        o.parent.clone().unwrap_or_default(),
    )
}

/// The previous page's last object, from the cursor. One this driver
/// didn't write starts at the top, as `page_of` does.
fn after(q: &ObjectQuery) -> Option<DbObject> {
    let (schema, name, kind, parent) = serde_json::from_str(q.after.as_deref()?).ok()?;
    Some(DbObject {
        kind,
        schema,
        name,
        parent,
    })
}

fn cursor(o: &DbObject) -> String {
    serde_json::json!([o.schema, o.name, o.kind, o.parent]).to_string()
}

/// The kinds the query admits.
fn wanted(q: &ObjectQuery) -> Vec<&'static str> {
    KINDS
        .iter()
        .copied()
        .filter(|k| {
            q.kind.as_deref().is_none_or(|w| w == *k) && !q.not_kinds.iter().any(|n| n == k)
        })
        .collect()
}

/// The page out of the objects found: those the query admits, after the
/// cursor, in the tree's order, once each, cut at the page size.
fn page(q: &ObjectQuery, found: Vec<DbObject>) -> ObjectPage {
    let from = after(q).as_ref().map(key);
    let mut found: Vec<(Key, DbObject)> = found
        .into_iter()
        .filter(|o| q.matches(o))
        .map(|o| (key(&o), o))
        .filter(|(k, _)| from.as_ref().is_none_or(|f| k > f))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let size = q.page_size();
    let next = (found.len() > size).then(|| cursor(&found[size - 1].1));
    found.truncate(size);
    ObjectPage {
        objects: found.into_iter().map(|(_, o)| o).collect(),
        next,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::objects::page_of;

    fn with(f: impl FnOnce(&mut ObjectQuery)) -> ObjectQuery {
        let mut q = ObjectQuery::default();
        f(&mut q);
        q
    }

    #[test]
    fn searches_that_can_find_indices_are_the_apps() {
        let answered = |q: ObjectQuery| answered(&q, &wanted(&q));
        assert!(answered(ObjectQuery::default()));
        assert!(answered(with(|q| q.kind = Some(kinds::INDEX.into()))));
        assert!(answered(with(|q| q.pattern = Some(String::new()))));
        assert!(!answered(with(|q| q.pattern = Some("logs".into()))));
        assert!(!answered(with(|q| q.name = Some("logs".into()))));
        assert!(!answered(with(|q| {
            q.kind = Some(kinds::INDEX.into());
            q.name = Some("logs".into());
        })));
        assert!(answered(with(|q| {
            q.kind = Some(KIND_ALIAS.into());
            q.pattern = Some("logs".into());
        })));
        assert!(answered(with(|q| {
            q.not_kinds = vec![kinds::INDEX.into()];
            q.name = Some("logs".into());
        })));
    }

    #[test]
    fn pages_follow_page_of() {
        let obj = |kind: &str, name: String| DbObject {
            kind: kind.into(),
            schema: None,
            name,
            parent: None,
        };
        let mut all: Vec<DbObject> = (0..25)
            .map(|i| obj(kinds::INDEX, format!("logs-{i:02}")))
            .collect();
        all.extend((0..5).map(|i| obj(KIND_ALIAS, format!("Logs_{i}"))));
        all.push(obj(kinds::STREAM, "logs-ds".into()));
        for q in [
            ObjectQuery::default(),
            with(|q| q.kind = Some(kinds::INDEX.into())),
            with(|q| q.pattern = Some("LOGS_".into())),
            with(|q| q.not_kinds = vec![kinds::INDEX.into()]),
        ] {
            let (mut mine, mut theirs) = (q.clone(), q.clone());
            mine.limit = 10;
            theirs.limit = 10;
            loop {
                let got = page(&mine, all.clone());
                let want = page_of(&all, &theirs);
                assert_eq!(got.objects, want.objects, "{q:?}");
                match (got.next, want.next) {
                    (Some(a), Some(b)) => {
                        mine.after = Some(a);
                        theirs.after = Some(b);
                    }
                    (None, None) => break,
                    other => panic!("{q:?}: {other:?}"),
                }
            }
        }
    }
}
