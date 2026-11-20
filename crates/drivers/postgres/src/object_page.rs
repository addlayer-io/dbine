//! The explorer's pages of objects ([`dbine_driver::Session::list_objects_page`])
//! as one catalog query. The branches are the queries `list_objects` reads
//! (each a SELECT with `kind`, `sch`, `name` and maybe `parent` columns);
//! each one is narrowed by the query's filters and the cursor, they are
//! joined with `UNION ALL`, and the result is one row per kind, schema and
//! name (a trigger keeps its first table, as `list_objects` does), in the
//! tree's order, at most a page and one more (to know whether more remain).
//!
//! The cursor is the last object's schema, name and kind (keyset paging):
//! the next page starts right after it, in the same order, whatever was
//! created or dropped in between.
//!
//! Only builds SQL and reads rows. Shared with the DSQL driver (`#[path]`).

use dbine_driver::{DbObject, Error, ObjectPage, ObjectQuery, Result};

/// The kinds a branch returns.
pub(crate) enum Kinds {
    /// Every row is this kind; the SQL has no `kind` column.
    One(&'static str),
    /// The SQL's `kind` column, one of these.
    Column(&'static [&'static str]),
}

/// One of the catalog queries `list_objects` reads.
pub(crate) struct Branch {
    pub kinds: Kinds,
    /// A SELECT with `sch` (unless the engine is schemaless), `name`, the
    /// `kind` column of [`Kinds::Column`] and, when `parent`, `parent`.
    pub sql: String,
    pub parent: bool,
}

impl Branch {
    /// A branch with a `kind` column.
    pub fn column(kinds: &'static [&'static str], sql: String) -> Self {
        Self {
            kinds: Kinds::Column(kinds),
            sql,
            parent: false,
        }
    }

    /// A branch of a single kind.
    pub fn one(kind: &'static str, sql: String) -> Self {
        Self {
            kinds: Kinds::One(kind),
            sql,
            parent: false,
        }
    }

    /// Its rows have a `parent` column (a trigger's table).
    pub fn with_parent(mut self) -> Self {
        self.parent = true;
        self
    }
}

/// How the engine compares and names things.
#[derive(Clone, Copy)]
pub(crate) struct Dialect {
    /// Appended to every sort key: ` COLLATE "C"` where the engine has it,
    /// so the order is by bytes, as the tree sorts. Engines without
    /// collations already compare text that way.
    pub collate: &'static str,
    /// The engine has no schemas: the branches have no `sch` column and
    /// the objects none.
    pub schemaless: bool,
}

/// The query for a page, with its parameters (`$1`, `$2`…, all text).
#[derive(Debug, PartialEq)]
pub(crate) struct PageSql {
    pub sql: String,
    pub params: Vec<String>,
}

/// A row of [`PageSql`]: kind, schema, name and parent.
pub(crate) type PageRow = [Option<String>; 4];

/// The query for the page `q` asks for, or `None` when nothing can match
/// (a kind the engine doesn't list, objects without a schema on an engine
/// with schemas). A cursor that isn't one of ours (the app's own position
/// in a whole list) answers `Unsupported`, so the app keeps paging the list.
pub(crate) fn plan(branches: &[Branch], q: &ObjectQuery, d: Dialect) -> Result<Option<PageSql>> {
    let after = match q.after.as_deref() {
        None => None,
        Some(a) => Some(parse_cursor(a)?),
    };
    // Objects without a schema on an engine with them, or a schema's on one
    // without them: none.
    let none = if d.schemaless {
        !q.no_schema && q.schema.is_some()
    } else {
        q.no_schema
    };
    if none {
        return Ok(None);
    }
    let mut params = Params::default();
    let mut conds = Vec::new();
    if !d.schemaless {
        if let Some(s) = q.schema.as_deref().filter(|_| !q.no_schema) {
            conds.push(format!("c.sch = {}", params.bind(s)));
        }
    }
    if let Some(n) = &q.name {
        conds.push(format!("lower(c.name) = lower({})", params.bind(n)));
    }
    if let Some(p) = &q.pattern {
        conds.push(format!(
            "lower(c.name) LIKE lower({}) ESCAPE '!'",
            params.bind(&like_contains(p))
        ));
    }
    if let Some((s, n, k)) = &after {
        let (s, n, k) = (params.bind(s), params.bind(n), params.bind(k));
        let values = [
            format!("lower({s})"),
            format!("lower({n})"),
            s.clone(),
            n.clone(),
            k,
        ];
        conds.push(greater(&keys("c", d.collate), &values));
    }
    let wanted =
        |k: &str| q.kind.as_deref().is_none_or(|w| w == k) && !q.not_kinds.iter().any(|n| n == k);
    let mut selects = Vec::new();
    for b in branches {
        let mut own = Vec::new();
        let kind = match b.kinds {
            Kinds::One(k) if wanted(k) => format!("CAST({} AS VARCHAR)", literal(k)),
            Kinds::One(_) => continue,
            Kinds::Column(all) => {
                let some: Vec<&str> = all.iter().copied().filter(|k| wanted(k)).collect();
                if some.is_empty() {
                    continue;
                }
                if some.len() < all.len() {
                    let list: Vec<String> = some.iter().map(|k| literal(k)).collect();
                    own.push(format!("c.kind IN ({})", list.join(", ")));
                }
                "CAST(b.kind AS VARCHAR)".to_string()
            }
        };
        let sch = if d.schemaless {
            "CAST('' AS VARCHAR)"
        } else {
            "CAST(b.sch AS VARCHAR)"
        };
        let parent = if b.parent {
            "CAST(b.parent AS VARCHAR)"
        } else {
            "CAST(NULL AS VARCHAR)"
        };
        let filter: Vec<&str> = own.iter().chain(&conds).map(String::as_str).collect();
        let filter = if filter.is_empty() {
            "1 = 1".to_string()
        } else {
            filter.join(" AND ")
        };
        selects.push(format!(
            "SELECT c.kind, c.sch, c.name, c.parent FROM (SELECT {kind} AS kind, {sch} AS sch, \
             CAST(b.name AS VARCHAR) AS name, {parent} AS parent FROM ({sql}) b) c WHERE {filter}",
            sql = b.sql
        ));
    }
    if selects.is_empty() {
        return Ok(None);
    }
    let c = d.collate;
    Ok(Some(PageSql {
        sql: format!(
            "SELECT u.kind, u.sch, u.name, MIN(u.parent{c}) AS parent FROM ({}) u \
             GROUP BY u.kind, u.sch, u.name ORDER BY {} LIMIT {}",
            selects.join(" UNION ALL "),
            keys("u", c).join(", "),
            q.page_size() as u64 + 1
        ),
        params: params.0,
    }))
}

/// The page out of the rows of [`plan`]'s query.
pub(crate) fn page(rows: Vec<PageRow>, q: &ObjectQuery) -> ObjectPage {
    let size = q.page_size();
    let more = rows.len() > size;
    let objects: Vec<DbObject> = rows
        .into_iter()
        .take(size)
        .filter_map(|[kind, sch, name, parent]| {
            Some(DbObject {
                kind: kind?,
                schema: sch.filter(|s| !s.is_empty()),
                name: name?,
                parent,
            })
        })
        .collect();
    let next = more
        .then(|| objects.last())
        .flatten()
        .map(|o| cursor(o.schema.as_deref().unwrap_or(""), &o.name, &o.kind));
    ObjectPage { objects, next }
}

/// The page after the object with this schema, name and kind.
fn cursor(schema: &str, name: &str, kind: &str) -> String {
    serde_json::json!([schema, name, kind]).to_string()
}

fn parse_cursor(after: &str) -> Result<(String, String, String)> {
    serde_json::from_str(after).map_err(|_| unsupported())
}

/// The contract's own answer: the app then pages `list_objects` itself.
pub(crate) fn unsupported() -> Error {
    Error::Unsupported("este motor todavía no lista sus objetos por páginas".into())
}

/// The sort keys, in the tree's order: schema, then name, ignoring case,
/// then as written, then the kind (a table and a function with the same
/// name).
fn keys(t: &str, collate: &str) -> [String; 5] {
    [
        format!("lower({t}.sch){collate}"),
        format!("lower({t}.name){collate}"),
        format!("{t}.sch{collate}"),
        format!("{t}.name{collate}"),
        format!("{t}.kind{collate}"),
    ]
}

/// `keys` > `values`, compared in order (what a row comparison does, written
/// out for the engines without one).
fn greater(keys: &[String], values: &[String]) -> String {
    let last = keys.len() - 1;
    let mut s = format!("{} > {}", keys[last], values[last]);
    for i in (0..last).rev() {
        s = format!(
            "{k} > {v} OR ({k} = {v} AND ({s}))",
            k = keys[i],
            v = values[i]
        );
    }
    format!("({s})")
}

/// `LIKE` operand for names containing `p`: its `%`, `_` and the escape
/// character itself are escaped with `!` (a backslash means different
/// things with `standard_conforming_strings` on and off).
fn like_contains(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 2);
    out.push('%');
    for ch in p.chars() {
        if matches!(ch, '%' | '_' | '!') {
            out.push('!');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

/// A kind id as a string literal (the driver's own constants).
fn literal(k: &str) -> String {
    format!("'{}'", k.replace('\'', "''"))
}

/// The query's parameters: each value once, as `CAST($n AS VARCHAR)`.
#[derive(Default)]
struct Params(Vec<String>);

impl Params {
    fn bind(&mut self, v: &str) -> String {
        self.0.push(v.to_string());
        format!("CAST(${} AS VARCHAR)", self.0.len())
    }
}

/// Runs the page query over the extended protocol, its values bound.
pub(crate) async fn query(
    client: &tokio_postgres::Client,
    p: &PageSql,
) -> std::result::Result<Vec<PageRow>, tokio_postgres::Error> {
    let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = p
        .params
        .iter()
        .map(|s| s as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();
    client
        .query(p.sql.as_str(), &params)
        .await?
        .iter()
        .map(|r| Ok([r.try_get(0)?, r.try_get(1)?, r.try_get(2)?, r.try_get(3)?]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: Dialect = Dialect {
        collate: " COLLATE \"C\"",
        schemaless: false,
    };

    fn branches() -> Vec<Branch> {
        vec![
            Branch::column(
                &["table", "view"],
                "SELECT 'table' AS kind, s AS sch, n AS name FROM rel".into(),
            ),
            Branch::one("sequence", "SELECT s AS sch, n AS name FROM seq".into()),
            Branch::column(
                &["trigger"],
                "SELECT 'trigger' AS kind, s AS sch, n AS name, t AS parent FROM trg".into(),
            )
            .with_parent(),
        ]
    }

    fn row(kind: &str, sch: &str, name: &str) -> PageRow {
        [Some(kind.into()), Some(sch.into()), Some(name.into()), None]
    }

    #[test]
    fn a_folder_reads_only_its_branch_and_kind() {
        let q = ObjectQuery {
            schema: Some("public".into()),
            kind: Some("view".into()),
            limit: 7,
            ..Default::default()
        };
        let p = plan(&branches(), &q, PG).unwrap().unwrap();
        assert_eq!(p.params, ["public"]);
        assert!(p.sql.contains("FROM rel"), "{}", p.sql);
        assert!(!p.sql.contains("FROM seq") && !p.sql.contains("FROM trg"));
        assert!(p.sql.contains("c.kind IN ('view')"), "{}", p.sql);
        assert!(p.sql.contains("c.sch = CAST($1 AS VARCHAR)"), "{}", p.sql);
        assert!(p.sql.ends_with("LIMIT 8"), "{}", p.sql);
        assert!(p.sql.contains(
            "ORDER BY lower(u.sch) COLLATE \"C\", lower(u.name) COLLATE \"C\", u.sch COLLATE \"C\", \
             u.name COLLATE \"C\", u.kind COLLATE \"C\""
        ));
        // A single-kind branch of that kind: its kind as a constant.
        let q = ObjectQuery {
            kind: Some("sequence".into()),
            ..Default::default()
        };
        let p = plan(&branches(), &q, PG).unwrap().unwrap();
        assert!(
            p.sql.contains("CAST('sequence' AS VARCHAR) AS kind"),
            "{}",
            p.sql
        );
        assert!(p.sql.contains("WHERE 1 = 1"), "{}", p.sql);
        assert!(p.sql.ends_with("LIMIT 1001"), "{}", p.sql);
    }

    #[test]
    fn other_kinds_leave_out_the_declared_ones() {
        let q = ObjectQuery {
            not_kinds: vec!["table".into(), "sequence".into(), "trigger".into()],
            ..Default::default()
        };
        let p = plan(&branches(), &q, PG).unwrap().unwrap();
        assert!(p.sql.contains("c.kind IN ('view')"), "{}", p.sql);
        assert!(!p.sql.contains("FROM seq") && !p.sql.contains("FROM trg"));
        // Nothing left to read: no query at all.
        let q = ObjectQuery {
            kind: Some("synonym".into()),
            ..Default::default()
        };
        assert_eq!(plan(&branches(), &q, PG).unwrap(), None);
    }

    #[test]
    fn names_are_bound_and_wildcards_escaped() {
        let q = ObjectQuery {
            pattern: Some("50%_off!'".into()),
            name: Some("O'Brien".into()),
            ..Default::default()
        };
        let p = plan(&branches(), &q, PG).unwrap().unwrap();
        assert_eq!(p.params, ["O'Brien", "%50!%!_off!!'%"]);
        assert!(!p.sql.contains("Brien") && !p.sql.contains("off"));
        assert!(p.sql.contains("lower(c.name) = lower(CAST($1 AS VARCHAR))"));
        assert!(p
            .sql
            .contains("lower(c.name) LIKE lower(CAST($2 AS VARCHAR)) ESCAPE '!'"));
        assert_eq!(like_contains(""), "%%");
    }

    #[test]
    fn schemas_and_schemaless_engines() {
        // No schema asked on an engine with schemas: nothing.
        let q = ObjectQuery {
            no_schema: true,
            schema: Some("public".into()),
            ..Default::default()
        };
        assert_eq!(plan(&branches(), &q, PG).unwrap(), None);
        let flat = Dialect {
            collate: "",
            schemaless: true,
        };
        // On one without schemas, every object; a schema, none.
        let p = plan(&branches(), &q, flat).unwrap().unwrap();
        assert!(p.params.is_empty());
        assert!(p.sql.contains("CAST('' AS VARCHAR) AS sch"), "{}", p.sql);
        assert!(!p.sql.contains("COLLATE"), "{}", p.sql);
        let q = ObjectQuery {
            schema: Some("public".into()),
            ..Default::default()
        };
        assert_eq!(plan(&branches(), &q, flat).unwrap(), None);
    }

    #[test]
    fn the_cursor_is_the_last_object_and_starts_the_next_page() {
        let q = ObjectQuery {
            limit: 2,
            ..Default::default()
        };
        let p = page(
            vec![
                row("table", "a", "x"),
                row("view", "a", "y"),
                row("table", "b", "z"),
            ],
            &q,
        );
        assert_eq!(p.objects.len(), 2);
        let next = p.next.unwrap();
        assert_eq!(next, r#"["a","y","view"]"#);
        // The last page has no cursor.
        let p = page(vec![row("table", "a", "x")], &q);
        assert_eq!(p.next, None);
        // Objects without a schema come back without one.
        let p = page(vec![row("table", "", "x")], &q);
        assert_eq!(p.objects[0].schema, None);

        let q = ObjectQuery {
            after: Some(next),
            limit: 2,
            ..Default::default()
        };
        let p = plan(&branches(), &q, PG).unwrap().unwrap();
        assert_eq!(p.params, ["a", "y", "view"]);
        let c = " COLLATE \"C\"";
        let (k1, k2, k3, k4, k5) = (
            format!("lower(c.sch){c}"),
            format!("lower(c.name){c}"),
            format!("c.sch{c}"),
            format!("c.name{c}"),
            format!("c.kind{c}"),
        );
        let (s, n, k) = (
            "CAST($1 AS VARCHAR)",
            "CAST($2 AS VARCHAR)",
            "CAST($3 AS VARCHAR)",
        );
        let expected = format!(
            "({k1} > lower({s}) OR ({k1} = lower({s}) AND ({k2} > lower({n}) OR ({k2} = lower({n}) AND \
             ({k3} > {s} OR ({k3} = {s} AND ({k4} > {n} OR ({k4} = {n} AND ({k5} > {k})))))))))"
        );
        assert!(p.sql.contains(&expected), "{}", p.sql);
    }

    #[test]
    fn a_cursor_from_the_whole_list_is_not_ours() {
        let q = ObjectQuery {
            after: Some("1000".into()),
            ..Default::default()
        };
        assert!(matches!(
            plan(&branches(), &q, PG),
            Err(Error::Unsupported(_))
        ));
    }
}
