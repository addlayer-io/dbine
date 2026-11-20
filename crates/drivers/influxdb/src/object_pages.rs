//! The explorer's pages (`Session::list_objects_page`, see
//! `dbine_driver::objects` and docs/drivers.md, "Objects by page") put
//! together from several sources, each read in the tree's order from its
//! own offset: a catalog query per kind paged on the server
//! (`LIMIT size + 1 OFFSET offset`), or the objects a server can't order as
//! the tree does, read whole and sorted here ([`slice`]). Shared with the
//! other drivers that build pages so (`#[path]`): Neo4j, OrientDB, TDengine
//! and IoTDB.
//!
//! - **Order:** `objects::page_of`'s (schema, none first, then name, both
//!   ignoring case, then as written). Between two sources with the same
//!   key the earlier one goes first: sources go in `list_objects`' order of
//!   kinds, which is what `page_of`'s stable sort keeps.
//! - **Cursor:** each source's offset, `o1,o2,…`, trailing zeros left out.
//!   While only the first source has given objects it is `page_of`'s own
//!   offset; a cursor that isn't one of these starts from the top, as
//!   `page_of` does.

// Not every driver uses every function.
#![allow(dead_code)]

use dbine_driver::{DbObject, Error, ObjectPage, ObjectQuery};

/// The most objects a source read whole may hold: with more, the pages of
/// that query are left to the app ([`unsupported`]), which reads
/// `list_objects` once and keeps it. Every page of the query, not only the
/// first: the app's next pages come with `page_of`'s cursors, which mean
/// positions in the whole sorted list, not each source's offsets.
pub const WHOLE_MAX: usize = 10_000;

/// The answer that leaves a page to the app (it pages `list_objects`).
pub fn unsupported() -> Error {
    Error::Unsupported("este motor todavía no lista sus objetos por páginas".into())
}

/// The tree's sort key (`objects::page_of`'s).
pub type Key = (bool, String, String, String, String);

pub fn key(o: &DbObject) -> Key {
    let s = o.schema.clone().unwrap_or_default();
    (
        o.schema.is_some(),
        s.to_lowercase(),
        o.name.to_lowercase(),
        s,
        o.name.clone(),
    )
}

/// Whether `q` keeps objects of `kind`.
pub fn wants(q: &ObjectQuery, kind: &str) -> bool {
    q.kind.as_deref().is_none_or(|k| k == kind) && !q.not_kinds.iter().any(|k| k == kind)
}

/// Whether `q` keeps objects in `schema` (`None`: without one).
pub fn schema_ok(q: &ObjectQuery, schema: Option<&str>) -> bool {
    if q.no_schema {
        schema.is_none()
    } else {
        q.schema.as_deref().is_none_or(|s| Some(s) == schema)
    }
}

/// Each of `n` sources' offset in `q.after`.
pub fn offsets(q: &ObjectQuery, n: usize) -> Vec<usize> {
    let mut v = vec![0; n];
    let Some(after) = q.after.as_deref() else {
        return v;
    };
    let parts: Vec<&str> = after.split(',').collect();
    if parts.len() > n {
        return v;
    }
    for (slot, p) in v.iter_mut().zip(parts) {
        match p.parse() {
            Ok(o) => *slot = o,
            Err(_) => return vec![0; n],
        }
    }
    v
}

/// The cursor of `offsets`.
fn cursor(offsets: &[usize]) -> String {
    let last = offsets.iter().rposition(|o| *o != 0).unwrap_or(0);
    offsets[..=last]
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// A source read whole: what `q` keeps of `all`, in the tree's order (ties
/// as `all` has them), from `offset`, at most `size + 1` of them, as a
/// server query with `LIMIT size + 1 OFFSET offset` gives.
pub fn slice(all: Vec<DbObject>, q: &ObjectQuery, offset: usize, size: usize) -> Vec<DbObject> {
    let mut hits: Vec<(Key, DbObject)> = all
        .into_iter()
        .filter(|o| q.matches(o))
        .map(|o| (key(&o), o))
        .collect();
    hits.sort_by(|a, b| a.0.cmp(&b.0));
    hits.into_iter()
        .skip(offset)
        .take(size.saturating_add(1))
        .map(|(_, o)| o)
        .collect()
}

/// The page out of each source's rows: from the source's offset, in the
/// tree's order, at most `size + 1` (one more than the page tells that
/// more remain).
pub fn merge(size: usize, offsets: &[usize], sources: Vec<Vec<DbObject>>) -> ObjectPage {
    let keys: Vec<Vec<Key>> = sources
        .iter()
        .map(|s| s.iter().map(key).collect())
        .collect();
    let mut taken = vec![0; sources.len()];
    let mut picks: Vec<(usize, usize)> = Vec::new();
    while picks.len() < size {
        let mut best: Option<usize> = None;
        for (i, ks) in keys.iter().enumerate() {
            if let Some(k) = ks.get(taken[i]) {
                if best.is_none_or(|b| *k < keys[b][taken[b]]) {
                    best = Some(i);
                }
            }
        }
        let Some(b) = best else { break };
        picks.push((b, taken[b]));
        taken[b] += 1;
    }
    let more = sources.iter().zip(&taken).any(|(s, t)| s.len() > *t);
    let mut sources: Vec<Vec<Option<DbObject>>> = sources
        .into_iter()
        .map(|s| s.into_iter().map(Some).collect())
        .collect();
    let objects = picks
        .into_iter()
        .filter_map(|(i, j)| sources[i][j].take())
        .collect();
    let next: Vec<usize> = offsets
        .iter()
        .chain(std::iter::repeat(&0))
        .zip(&taken)
        .map(|(o, t)| o + t)
        .collect();
    ObjectPage {
        objects,
        next: more.then(|| cursor(&next)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::objects::page_of;

    fn obj(kind: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: None,
            name: name.into(),
            parent: None,
        }
    }

    /// Two kinds, a name in both, capitals and accents.
    fn sample() -> Vec<DbObject> {
        let mut v: Vec<DbObject> = (0..23).map(|i| obj("a", &format!("n{i:02}"))).collect();
        v.extend(["Zeta", "same", "Ñandú", "b_1", "B2"].map(|n| obj("a", n)));
        v.extend(["same", "m", "N05", "ñu"].map(|n| obj("b", n)));
        v
    }

    /// Pages `q` from the two kinds as two sources read whole, as a driver
    /// would from its catalog queries.
    fn paged(all: &[DbObject], q: &ObjectQuery) -> Vec<ObjectPage> {
        let mut q = q.clone();
        let mut pages = Vec::new();
        loop {
            let off = offsets(&q, 2);
            let size = q.page_size();
            let of = |k: &str, o: usize| {
                let v: Vec<DbObject> = all.iter().filter(|x| x.kind == k).cloned().collect();
                slice(v, &q, o, size)
            };
            let p = merge(size, &off, vec![of("a", off[0]), of("b", off[1])]);
            let next = p.next.clone();
            pages.push(p);
            match next {
                Some(n) => q.after = Some(n),
                None => return pages,
            }
        }
    }

    #[test]
    fn merged_sources_page_as_page_of() {
        let all = sample();
        for limit in [1, 3, 7, 0] {
            for q in [
                ObjectQuery::default(),
                ObjectQuery {
                    pattern: Some("N".into()),
                    ..Default::default()
                },
                ObjectQuery {
                    name: Some("SAME".into()),
                    ..Default::default()
                },
                ObjectQuery {
                    kind: Some("b".into()),
                    ..Default::default()
                },
            ] {
                let q = ObjectQuery { limit, ..q };
                let got: Vec<DbObject> = paged(&all, &q)
                    .into_iter()
                    .flat_map(|p| p.objects)
                    .collect();
                let want = page_of(
                    &all,
                    &ObjectQuery {
                        limit: u32::MAX,
                        ..q.clone()
                    },
                )
                .objects;
                assert_eq!(got, want, "{q:?}");
            }
        }
        // The kind of the first source first when names tie.
        let p = paged(
            &all,
            &ObjectQuery {
                name: Some("same".into()),
                ..Default::default()
            },
        );
        let kinds: Vec<&str> = p[0].objects.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(kinds, ["a", "b"]);
    }

    #[test]
    fn cursors_are_offsets() {
        let all: Vec<DbObject> = (0..10).map(|i| obj("a", &format!("x{i}"))).collect();
        let q = ObjectQuery {
            limit: 4,
            ..Default::default()
        };
        let pages = paged(&all, &q);
        let nexts: Vec<Option<String>> = pages.iter().map(|p| p.next.clone()).collect();
        // Only the first source gave objects: page_of's own cursor.
        assert_eq!(nexts, [Some("4".into()), Some("8".into()), None]);
        let q = ObjectQuery {
            limit: 4,
            after: Some("4".into()),
            ..Default::default()
        };
        assert_eq!(offsets(&q, 3), [4, 0, 0]);
        for bad in ["x", "1,2,3,4", "1,,2", "-1"] {
            let q = ObjectQuery {
                after: Some(bad.into()),
                ..Default::default()
            };
            assert_eq!(offsets(&q, 3), [0, 0, 0], "{bad}");
        }
        assert_eq!(cursor(&[3, 0, 2, 0]), "3,0,2");
        assert_eq!(cursor(&[0, 0]), "0");
    }

    #[test]
    fn filters_by_kind_and_schema() {
        let q = ObjectQuery {
            not_kinds: vec!["a".into()],
            ..Default::default()
        };
        assert!(!wants(&q, "a") && wants(&q, "b"));
        let q = ObjectQuery {
            kind: Some("a".into()),
            ..Default::default()
        };
        assert!(wants(&q, "a") && !wants(&q, "b"));
        let q = ObjectQuery {
            schema: Some("s".into()),
            ..Default::default()
        };
        assert!(schema_ok(&q, Some("s")) && !schema_ok(&q, None) && !schema_ok(&q, Some("t")));
        let q = ObjectQuery {
            schema: Some("s".into()),
            no_schema: true,
            ..Default::default()
        };
        assert!(schema_ok(&q, None) && !schema_ok(&q, Some("s")));
        assert!(schema_ok(&ObjectQuery::default(), None));
    }
}
