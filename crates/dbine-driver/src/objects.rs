//! One page of a database's objects, as the explorer asks for them
//! ([`crate::Session::list_objects_page`]): a schema's objects of one kind
//! for a folder of the tree, a name search for completion or the filter, a
//! lookup by name. The tree never needs the whole database: a database with
//! a million views opens its one table at once.

use crate::DbObject;
use serde::{Deserialize, Serialize};

/// A page's size when the query doesn't say ([`ObjectQuery::limit`] 0).
pub const DEFAULT_PAGE: u32 = 1000;

/// Which objects, and which page of them. Every filter is optional; they
/// combine (all must hold).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectQuery {
    /// Only this schema's objects (`None`: any schema).
    #[serde(default)]
    pub schema: Option<String>,
    /// Only objects without a schema (those at the database level of an
    /// engine with schemas); `schema` is then ignored.
    #[serde(default)]
    pub no_schema: bool,
    /// Only this kind (one of the driver's `ObjectKindInfo::id`s).
    #[serde(default)]
    pub kind: Option<String>,
    /// None of these kinds: with the declared ones, "Otros" (kinds the
    /// driver returns but doesn't declare).
    #[serde(default)]
    pub not_kinds: Vec<String>,
    /// Names containing this, ignoring case (a search).
    #[serde(default)]
    pub pattern: Option<String>,
    /// This name exactly, ignoring case (a lookup).
    #[serde(default)]
    pub name: Option<String>,
    /// The page after this one: the previous page's [`ObjectPage::next`].
    /// Opaque: each driver writes its own.
    #[serde(default)]
    pub after: Option<String>,
    /// At most this many objects (0: [`DEFAULT_PAGE`]).
    #[serde(default)]
    pub limit: u32,
}

impl ObjectQuery {
    /// The page size asked for.
    pub fn page_size(&self) -> usize {
        if self.limit == 0 {
            DEFAULT_PAGE as usize
        } else {
            self.limit as usize
        }
    }

    /// Whether `o` passes every filter.
    pub fn matches(&self, o: &DbObject) -> bool {
        if self.no_schema {
            if o.schema.is_some() {
                return false;
            }
        } else if let Some(s) = &self.schema {
            if o.schema.as_deref() != Some(s.as_str()) {
                return false;
            }
        }
        if self.kind.as_deref().is_some_and(|k| k != o.kind) {
            return false;
        }
        if self.not_kinds.contains(&o.kind) {
            return false;
        }
        if let Some(n) = &self.name {
            if o.name.to_lowercase() != n.to_lowercase() {
                return false;
            }
        }
        if let Some(p) = &self.pattern {
            if !o.name.to_lowercase().contains(&p.to_lowercase()) {
                return false;
            }
        }
        true
    }
}

/// A page of objects, in the order the tree shows them (by schema, then by
/// name, ignoring case).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectPage {
    pub objects: Vec<DbObject>,
    /// More remain: pass it as [`ObjectQuery::after`] for the next page.
    /// `None`: this was the last one.
    #[serde(default)]
    pub next: Option<String>,
}

/// The page `q` asks for out of a whole list (what the app does with
/// [`crate::Session::list_objects`] for a driver without its own
/// `list_objects_page`). The cursor is the position in the filtered,
/// sorted list.
pub fn page_of(objects: &[DbObject], q: &ObjectQuery) -> ObjectPage {
    let mut hits: Vec<&DbObject> = objects.iter().filter(|o| q.matches(o)).collect();
    hits.sort_by(|a, b| order(a).cmp(&order(b)));
    let from = q
        .after
        .as_deref()
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(0)
        .min(hits.len());
    let to = (from + q.page_size()).min(hits.len());
    ObjectPage {
        objects: hits[from..to].iter().map(|o| (*o).clone()).collect(),
        next: (to < hits.len()).then(|| to.to_string()),
    }
}

/// The sort key of the tree's order: schema (none first), then name, both
/// ignoring case, then as written (so the order is total).
fn order(o: &DbObject) -> (bool, String, String, &str, &str) {
    (
        o.schema.is_some(),
        o.schema.as_deref().unwrap_or("").to_lowercase(),
        o.name.to_lowercase(),
        o.schema.as_deref().unwrap_or(""),
        o.name.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: schema.map(Into::into),
            name: name.into(),
            parent: None,
        }
    }

    fn names(p: &ObjectPage) -> Vec<String> {
        p.objects
            .iter()
            .map(|o| format!("{}.{}", o.schema.as_deref().unwrap_or("-"), o.name))
            .collect()
    }

    fn sample() -> Vec<DbObject> {
        let mut v: Vec<DbObject> = (0..2500)
            .map(|i| obj("view", Some("dbo"), &format!("v{i:04}")))
            .collect();
        v.push(obj("table", Some("dbo"), "Orders"));
        v.push(obj("table", Some("sales"), "orders"));
        v.push(obj("function", Some("dbo"), "f_total"));
        v.push(obj("synonym", Some("dbo"), "s_orders"));
        v.push(obj("table", None, "loose"));
        v
    }

    #[test]
    fn one_kind_of_one_schema_skips_the_rest() {
        let all = sample();
        let q = ObjectQuery {
            schema: Some("dbo".into()),
            kind: Some("table".into()),
            ..Default::default()
        };
        let p = page_of(&all, &q);
        assert_eq!(names(&p), ["dbo.Orders"]);
        assert_eq!(p.next, None);
    }

    #[test]
    fn pages_follow_each_other_to_the_last() {
        let all = sample();
        let mut q = ObjectQuery {
            kind: Some("view".into()),
            ..Default::default()
        };
        let mut seen = Vec::new();
        let mut pages = 0;
        loop {
            let p = page_of(&all, &q);
            pages += 1;
            seen.extend(names(&p));
            match p.next {
                Some(n) => q.after = Some(n),
                None => break,
            }
        }
        assert_eq!(pages, 3, "2,500 views in pages of 1,000");
        assert_eq!(seen.len(), 2500);
        assert_eq!(seen.first().unwrap(), "dbo.v0000");
        assert_eq!(seen.last().unwrap(), "dbo.v2499");
        q.limit = 10;
        q.after = None;
        let p = page_of(&all, &q);
        assert_eq!(p.objects.len(), 10);
        assert_eq!(p.next.as_deref(), Some("10"));
    }

    #[test]
    fn searches_lookups_other_kinds_and_loose_objects() {
        let all = sample();
        // A search ignores case and matches inside the name.
        let p = page_of(
            &all,
            &ObjectQuery {
                pattern: Some("ORDER".into()),
                ..Default::default()
            },
        );
        assert_eq!(names(&p), ["dbo.Orders", "dbo.s_orders", "sales.orders"]);
        // A lookup is the whole name, ignoring case, in any schema.
        let p = page_of(
            &all,
            &ObjectQuery {
                name: Some("orders".into()),
                ..Default::default()
            },
        );
        assert_eq!(names(&p), ["dbo.Orders", "sales.orders"]);
        // "Otros": what isn't of the declared kinds.
        let p = page_of(
            &all,
            &ObjectQuery {
                not_kinds: vec!["table".into(), "view".into(), "function".into()],
                ..Default::default()
            },
        );
        assert_eq!(names(&p), ["dbo.s_orders"]);
        // Objects without a schema, at the database level.
        let p = page_of(
            &all,
            &ObjectQuery {
                no_schema: true,
                schema: Some("dbo".into()),
                ..Default::default()
            },
        );
        assert_eq!(names(&p), ["-.loose"]);
    }

    #[test]
    fn a_cursor_past_the_end_is_an_empty_last_page() {
        let all = sample();
        let p = page_of(
            &all,
            &ObjectQuery {
                after: Some("99999".into()),
                ..Default::default()
            },
        );
        assert!(p.objects.is_empty());
        assert_eq!(p.next, None);
    }
}
