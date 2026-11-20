//! The explorer's pages of objects ([`dbine_driver::Session::list_objects_page`],
//! docs/drivers.md, "Objects by page"): what `list_objects` reads, narrowed
//! to the page on the server, in one query.
//!
//! - **Source:** `INFORMATION_SCHEMA."TABLES"` of the session's space or
//!   source and its folders (`TABLE_SCHEMA` is the container or starts with
//!   it and a dot), as `list_objects`.
//! - **Order:** schema, then name, both ignoring case, then as written:
//!   `ORDER BY LOWER(s), LOWER(n), s, n` (a schema and name are unique).
//! - **Cursor:** the page's last object, `[schema, name]` (keyset): the next
//!   page asks for what sorts after it, so pages neither skip nor repeat
//!   objects even when the catalog changes in between.
//! - **Page:** the query asks for one object more than the page, which
//!   tells whether more remain.
//!
//! Values from the query or the cursor go in as string literals (quotes
//! doubled: Dremio's strings have no other escape).

use crate::ddl::lit;
use crate::DremioSession;
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};

/// The largest page asked of the server: a bigger `limit` gets pages of
/// this size (a page holds at most the limit).
const MAX_PAGE: usize = 10_000;

/// The kinds `list_objects` gives.
const KINDS: &[&str] = &[kinds::TABLE, kinds::VIEW];

/// The objects a page holds.
fn size(q: &ObjectQuery) -> usize {
    q.page_size().min(MAX_PAGE)
}

/// The previous page's last object: its schema and name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    schema: String,
    name: String,
}

impl Cursor {
    /// `None` for a cursor this driver didn't write.
    fn parse(s: &str) -> Option<Cursor> {
        let (schema, name) = serde_json::from_str(s).ok()?;
        Some(Cursor { schema, name })
    }

    fn write(&self) -> String {
        serde_json::json!([self.schema, self.name]).to_string()
    }
}

/// The tree's order ([`dbine_driver::objects::page_of`]'s).
type Key = (String, String, String, String);

fn key(schema: &str, name: &str) -> Key {
    (
        schema.to_lowercase(),
        name.to_lowercase(),
        schema.to_string(),
        name.to_string(),
    )
}

/// What the app takes as "page the whole list yourself" (the contract's
/// default).
fn unsupported() -> Error {
    Error::Unsupported("este motor todavía no lista sus objetos por páginas".into())
}

/// The kinds among [`KINDS`] the query admits.
fn wanted(q: &ObjectQuery) -> Vec<&'static str> {
    KINDS
        .iter()
        .copied()
        .filter(|k| {
            q.kind.as_deref().is_none_or(|w| w == *k) && !q.not_kinds.iter().any(|n| n == k)
        })
        .collect()
}

/// `(c1 > v1 OR (c1 = v1 AND (c2 > v2 OR (… AND cN > vN))))`.
fn after_sql(keys: &[(String, String)]) -> String {
    match keys {
        [] => "TRUE".into(),
        [(c, v)] => format!("{c} > {v}"),
        [(c, v), rest @ ..] => format!("({c} > {v} OR ({c} = {v} AND {}))", after_sql(rest)),
    }
}

/// The page's query in the container `context`, `None` when nothing can
/// match it. Rows `TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE`.
fn page_sql(context: &str, q: &ObjectQuery, after: Option<&Cursor>) -> Option<String> {
    // Every object has a schema.
    if q.no_schema {
        return None;
    }
    let kinds = wanted(q);
    if kinds.is_empty() {
        return None;
    }
    let folders = format!(
        "{}.%",
        context
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let mut filters = vec![format!(
        "(TABLE_SCHEMA = {} OR TABLE_SCHEMA LIKE {} ESCAPE '\\')",
        lit(context),
        lit(&folders)
    )];
    if let Some(s) = &q.schema {
        filters.push(format!("TABLE_SCHEMA = {}", lit(s)));
    }
    if kinds.len() < KINDS.len() {
        filters.push(if kinds == [kinds::VIEW] {
            "TABLE_TYPE = 'VIEW'".into()
        } else {
            "TABLE_TYPE <> 'VIEW'".into()
        });
    }
    if let Some(p) = q.pattern.as_deref().filter(|p| !p.is_empty()) {
        filters.push(format!("STRPOS(LOWER(TABLE_NAME), LOWER({})) > 0", lit(p)));
    }
    if let Some(n) = &q.name {
        filters.push(format!("LOWER(TABLE_NAME) = LOWER({})", lit(n)));
    }
    if let Some(a) = after {
        let (s, n) = (lit(&a.schema), lit(&a.name));
        filters.push(after_sql(&[
            ("LOWER(TABLE_SCHEMA)".into(), format!("LOWER({s})")),
            ("LOWER(TABLE_NAME)".into(), format!("LOWER({n})")),
            ("TABLE_SCHEMA".into(), s),
            ("TABLE_NAME".into(), n),
        ]));
    }
    Some(format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.\"TABLES\" WHERE {} \
         ORDER BY LOWER(TABLE_SCHEMA), LOWER(TABLE_NAME), TABLE_SCHEMA, TABLE_NAME LIMIT {}",
        filters.join(" AND "),
        size(q) + 1
    ))
}

/// The page out of the rows (at most one more than the page): sorted, past
/// the cursor, cut at the page; the cursor is the last object kept. What's
/// shown is checked against the query once more (the server's lowercase
/// may differ from Rust's in rare letters).
fn merge(q: &ObjectQuery, after: Option<&Cursor>, found: Vec<DbObject>) -> ObjectPage {
    let from = after.map(|a| key(&a.schema, &a.name));
    let mut found: Vec<(Key, DbObject)> = found
        .into_iter()
        .map(|o| (key(o.schema.as_deref().unwrap_or(""), &o.name), o))
        .filter(|(k, _)| from.as_ref().is_none_or(|f| k > f))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let size = size(q);
    let next = (found.len() > size).then(|| {
        let (_, _, schema, name) = found[size - 1].0.clone();
        Cursor { schema, name }.write()
    });
    found.truncate(size);
    ObjectPage {
        objects: found
            .into_iter()
            .map(|(_, o)| o)
            .filter(|o| q.matches(o))
            .collect(),
        next,
    }
}

impl DremioSession {
    /// A page of the space's or source's objects (see the module's doc).
    pub(crate) async fn objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
        let after = match q.after.as_deref().map(Cursor::parse) {
            None => None,
            Some(Some(c)) => Some(c),
            // Not ours: the app's own position in the whole list, after an
            // earlier page fell back to it. It goes on paging that list.
            Some(None) => return Err(unsupported()),
        };
        let c = self.context()?;
        let Some(sql) = page_sql(&c, q, after.as_ref()) else {
            return Ok(ObjectPage::default());
        };
        let found = self
            .strings(&sql)
            .await?
            .into_iter()
            .filter(|r| r.len() == 3)
            .map(|r| DbObject {
                kind: if r[2] == "VIEW" {
                    kinds::VIEW
                } else {
                    kinds::TABLE
                }
                .into(),
                schema: Some(r[0].clone()),
                name: r[1].clone(),
                parent: None,
            })
            .collect();
        Ok(merge(q, after.as_ref(), found))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
            parent: None,
        }
    }

    /// A folder of one kind: the container and its folders, the schema, the
    /// kind, the search and the cursor, quotes doubled and LIKE's wildcards
    /// escaped; one more than the page.
    #[test]
    fn a_folder_query() {
        let q = ObjectQuery {
            schema: Some("my_sp.ca'rp".into()),
            kind: Some(kinds::VIEW.into()),
            pattern: Some("o'r".into()),
            limit: 7,
            ..Default::default()
        };
        let after = Cursor {
            schema: "my_sp.ca'rp".into(),
            name: "b".into(),
        };
        let sql = page_sql("my_sp", &q, Some(&after)).unwrap();
        assert!(sql.contains("WHERE (TABLE_SCHEMA = 'my_sp' OR TABLE_SCHEMA LIKE 'my\\_sp.%' ESCAPE '\\') AND TABLE_SCHEMA = 'my_sp.ca''rp' AND TABLE_TYPE = 'VIEW' AND STRPOS(LOWER(TABLE_NAME), LOWER('o''r')) > 0 AND (LOWER(TABLE_SCHEMA) > LOWER('my_sp.ca''rp') OR"), "{sql}");
        assert!(
            sql.contains("(TABLE_SCHEMA > 'my_sp.ca''rp' OR (TABLE_SCHEMA = 'my_sp.ca''rp' AND TABLE_NAME > 'b'))"),
            "{sql}"
        );
        assert!(
            sql.ends_with(
                "ORDER BY LOWER(TABLE_SCHEMA), LOWER(TABLE_NAME), TABLE_SCHEMA, TABLE_NAME LIMIT 8"
            ),
            "{sql}"
        );
        let q = ObjectQuery {
            kind: Some(kinds::TABLE.into()),
            name: Some("X".into()),
            ..Default::default()
        };
        let sql = page_sql("s", &q, None).unwrap();
        assert!(
            sql.contains("AND TABLE_TYPE <> 'VIEW' AND LOWER(TABLE_NAME) = LOWER('X') ORDER BY"),
            "{sql}"
        );
    }

    #[test]
    fn nothing_without_a_schema_or_a_kind() {
        let q = ObjectQuery {
            no_schema: true,
            ..Default::default()
        };
        assert!(page_sql("c", &q, None).is_none());
        let q = ObjectQuery {
            not_kinds: KINDS.iter().map(|k| k.to_string()).collect(),
            ..Default::default()
        };
        assert!(page_sql("c", &q, None).is_none());
        let sql = page_sql("c", &ObjectQuery::default(), None).unwrap();
        assert!(!sql.contains("TABLE_TYPE ="), "{sql}");
        assert!(!sql.contains("TABLE_TYPE <>"), "{sql}");
    }

    #[test]
    fn pages_merge_and_follow_the_cursor() {
        let q = ObjectQuery {
            limit: 2,
            ..Default::default()
        };
        let found = vec![
            obj(kinds::TABLE, "sp.b", "x"),
            obj(kinds::VIEW, "SP", "s"),
            obj(kinds::TABLE, "sp", "S"),
        ];
        let p = merge(&q, None, found.clone());
        let got: Vec<_> = p.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(got, ["s", "S"]);
        let after = Cursor::parse(p.next.as_deref().unwrap()).unwrap();
        let p2 = merge(&q, Some(&after), found);
        assert_eq!(p2.objects.len(), 1);
        assert_eq!(p2.next, None);
        assert_eq!(Cursor::parse("1000"), None);
    }
}
