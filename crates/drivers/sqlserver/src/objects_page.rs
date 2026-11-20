//! The explorer's pages of objects ([`dbine_driver::Session::list_objects_page`]):
//! one catalog query per page, narrowed by schema, kind and name, in the
//! tree's order, starting after the previous page's last object (keyset).
//!
//! It reads what `list_objects` reads: `sys.objects` (tables, views,
//! procedures, functions and triggers, plus sequences and synonyms on SQL
//! Server and Azure SQL), the user types (not on Fabric) and, on SQL Server
//! and Azure SQL, the full-text catalogs and stoplists, which have no
//! schema. Each source joins the query only if the filters can match it.
//!
//! The tree's order ([`dbine_driver::objects::page_of`]) compares lowercased
//! names by code point, so the query lowercases and sorts under a binary
//! collation, whatever the database's. Two objects can still tie on schema
//! and name (a type and a table, Babelfish's overloaded procedures): the
//! source and the catalog id break the tie, and the cursor carries all of
//! them, so pages neither skip nor repeat.

use crate::structure::{FULLTEXT_CATALOG, FULLTEXT_STOPLIST};
use crate::variant::Variant;
use crate::{text, SqlServerSession};
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};

/// The `sys.objects` types `list_objects` lists, with their kind.
const OBJECT_TYPES: &[(&str, &str)] = &[
    ("U", kinds::TABLE),
    ("V", kinds::VIEW),
    ("P", kinds::PROCEDURE),
    ("PC", kinds::PROCEDURE),
    ("FN", kinds::FUNCTION),
    ("IF", kinds::FUNCTION),
    ("TF", kinds::FUNCTION),
    ("FS", kinds::FUNCTION),
    ("FT", kinds::FUNCTION),
    ("TR", kinds::TRIGGER),
];

/// The ones `structure::list_objects` adds on SQL Server and Azure SQL.
const MORE_OBJECT_TYPES: &[(&str, &str)] = &[("SO", kinds::SEQUENCE), ("SN", kinds::SYNONYM)];

/// Where the previous page ended: its last object's schema, name, source
/// and catalog id (the whole sort key). Written as a JSON array.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor(Option<String>, String, i32, i64);

impl Cursor {
    fn parse(s: &str) -> Option<Cursor> {
        let (schema, name, source, id) = serde_json::from_str(s).ok()?;
        Some(Cursor(schema, name, source, id))
    }

    fn write(&self) -> String {
        serde_json::to_string(&(&self.0, &self.1, self.2, self.3)).unwrap_or_default()
    }
}

/// The page `q` asks for.
pub(crate) async fn page(s: &mut SqlServerSession, q: &ObjectQuery) -> Result<ObjectPage> {
    let after = match q.after.as_deref().map(Cursor::parse) {
        None => None,
        Some(Some(c)) => Some(c),
        // Not ours: the app's own position in the whole list, after an
        // earlier page fell back to it. It goes on paging that list.
        Some(None) => return Err(unsupported()),
    };
    let sources = sources(s.variant, q);
    if sources.is_empty() {
        return Ok(ObjectPage::default());
    }
    let (sql, params) = page_sql(s.variant, q, after.as_ref(), &sources);
    let params: Vec<&str> = params.iter().map(String::as_str).collect();
    let rows = match s.catalog_rows(&sql, &params).await {
        Ok(rows) => rows,
        // A first page the server refuses (a catalog or collation this
        // engine lacks): the app pages the whole list, as before.
        Err(e) if e.is_query() && after.is_none() => {
            tracing::warn!("sqlserver: object page refused, the app reads the whole list: {e}");
            return Err(unsupported());
        }
        Err(e) => return Err(e),
    };
    let mut got: Vec<(DbObject, Cursor)> = rows
        .iter()
        .filter_map(|r| {
            let kind = text(r, 0)?;
            let schema = text(r, 1);
            let name = text(r, 2)?;
            let cursor = Cursor(schema.clone(), name.clone(), r.get(4)?, r.get(5)?);
            let parent = if kind == kinds::TRIGGER {
                text(r, 3)
            } else {
                None
            };
            Some((
                DbObject {
                    kind,
                    schema,
                    name,
                    parent,
                },
                cursor,
            ))
        })
        .collect();
    let size = q.page_size();
    let more = got.len() > size;
    got.truncate(size);
    Ok(ObjectPage {
        next: if more {
            got.last().map(|(_, c)| c.write())
        } else {
            None
        },
        objects: got.into_iter().map(|(o, _)| o).collect(),
    })
}

/// What the app takes as "page the whole list yourself" (the contract's
/// default).
fn unsupported() -> Error {
    Error::Unsupported("este motor todavía no lista sus objetos por páginas".into())
}

/// The `SELECT`s whose objects `q` can match, each with the columns `r`
/// (the source, for ties), `id`, `k` (the kind), `sch`, `nm` and `par` (a
/// trigger's table). Fixed text: the kinds and type codes are this
/// module's own.
fn sources(v: Variant, q: &ObjectQuery) -> Vec<String> {
    let wanted = |kind: &str| {
        q.kind.as_deref().is_none_or(|k| k == kind) && !q.not_kinds.iter().any(|k| k == kind)
    };
    // As `structure::list_objects`: Babelfish adds only its types, Fabric
    // nothing.
    let rich = matches!(v, Variant::SqlServer | Variant::AzureSql);
    let null = "CAST(NULL AS nvarchar(128))";
    let mut parts = Vec::new();
    if !q.no_schema {
        let more = if rich { MORE_OBJECT_TYPES } else { &[] };
        let types: Vec<&(&str, &str)> = OBJECT_TYPES
            .iter()
            .chain(more)
            .filter(|(_, k)| wanted(k))
            .collect();
        if !types.is_empty() {
            let case: String = types
                .iter()
                .map(|(t, k)| format!(" WHEN '{t}' THEN N'{k}'"))
                .collect();
            let codes: Vec<String> = types.iter().map(|(t, _)| format!("'{t}'")).collect();
            parts.push(format!(
                "SELECT 0 AS r, CAST(o.object_id AS bigint) AS id, CASE RTRIM(o.type){case} END AS k,
                        s.name AS sch, o.name AS nm,
                        CASE WHEN o.type = 'TR' THEN OBJECT_NAME(o.parent_object_id) END AS par
                   FROM sys.objects o
                   JOIN sys.schemas s ON s.schema_id = o.schema_id
                  WHERE o.is_ms_shipped = 0 AND o.type IN ({})",
                codes.join(", ")
            ));
        }
        if v != Variant::Fabric && wanted(kinds::TYPE) {
            parts.push(format!(
                "SELECT 1 AS r, CAST(user_type_id AS bigint) AS id, N'{}' AS k,
                        SCHEMA_NAME(schema_id) AS sch, name AS nm, {null} AS par
                   FROM sys.types
                  WHERE is_user_defined = 1{}",
                kinds::TYPE,
                if rich {
                    " AND is_assembly_type = 0"
                } else {
                    ""
                }
            ));
        }
    }
    // Without a schema: never in one schema's folder.
    if rich && (q.no_schema || q.schema.is_none()) {
        if wanted(FULLTEXT_CATALOG) {
            parts.push(format!(
                "SELECT 2 AS r, CAST(fulltext_catalog_id AS bigint) AS id, N'{FULLTEXT_CATALOG}' AS k,
                        {null} AS sch, name AS nm, {null} AS par
                   FROM sys.fulltext_catalogs"
            ));
        }
        if wanted(FULLTEXT_STOPLIST) {
            parts.push(format!(
                "SELECT 3 AS r, CAST(stoplist_id AS bigint) AS id, N'{FULLTEXT_STOPLIST}' AS k,
                        {null} AS sch, name AS nm, {null} AS par
                   FROM sys.fulltext_stoplists"
            ));
        }
    }
    parts
}

/// The collation names are sorted and compared under: binary, so by code
/// point as [`dbine_driver::objects::page_of`] does. A Fabric warehouse
/// takes only its own two collations.
fn binary(v: Variant) -> &'static str {
    match v {
        Variant::Fabric => "Latin1_General_100_BIN2_UTF8",
        _ => "Latin1_General_100_BIN2",
    }
}

/// `e` lowercased, under the binary collation. SQL Server lowercases with
/// its input's collation: the binary one, so the casing doesn't depend on
/// the database's collation. Babelfish takes a binary collation as C, whose
/// LOWER only changes ASCII: it lowercases first, with the database's.
fn lower(v: Variant, e: &str) -> String {
    match v {
        Variant::Babelfish => format!("LOWER({e}) COLLATE {}", binary(v)),
        _ => format!("LOWER({e} COLLATE {})", binary(v)),
    }
}

/// `keys` after `values`, in that order: `(a > x OR (a = x AND …))`.
fn after_sql(keys: &[(&str, String)]) -> String {
    match keys {
        [] => "1 = 1".into(),
        [(k, v)] => format!("{k} > {v}"),
        [(k, v), rest @ ..] => format!("({k} > {v} OR ({k} = {v} AND {}))", after_sql(rest)),
    }
}

/// The page's query and its parameters (`@P1`…): every text from the query
/// or the cursor is bound, never written into the SQL. One row more than
/// the page tells whether another page follows.
fn page_sql(
    v: Variant,
    q: &ObjectQuery,
    after: Option<&Cursor>,
    sources: &[String],
) -> (String, Vec<String>) {
    let bin = binary(v);
    let mut params: Vec<String> = Vec::new();
    let mut bind = |s: &str| {
        params.push(s.to_string());
        format!("@P{}", params.len())
    };
    let mut filters = Vec::new();
    if q.no_schema {
        filters.push("x.h = 0".to_string());
    } else if let Some(s) = &q.schema {
        let p = bind(s);
        // DATALENGTH: `=` ignores trailing spaces, the tree doesn't.
        filters.push(format!(
            "x.h = 1 AND x.s = {p} AND DATALENGTH(x.s) = DATALENGTH({p})"
        ));
    }
    if let Some(n) = &q.name {
        let l = lower(v, &bind(n));
        filters.push(format!("x.ln = {l} AND DATALENGTH(x.ln) = DATALENGTH({l})"));
    }
    // Every name contains "" (and CHARINDEX finds it nowhere).
    if let Some(p) = q.pattern.as_deref().filter(|p| !p.is_empty()) {
        filters.push(format!("CHARINDEX({}, x.ln) > 0", lower(v, &bind(p))));
    }
    if let Some(Cursor(schema, name, source, id)) = after {
        let s = bind(schema.as_deref().unwrap_or(""));
        let n = bind(name);
        filters.push(after_sql(&[
            ("x.h", u8::from(schema.is_some()).to_string()),
            ("x.ls", lower(v, &s)),
            ("x.ln", lower(v, &n)),
            ("x.s", s),
            ("x.n", n),
            ("x.r", source.to_string()),
            ("x.id", id.to_string()),
        ]));
    }
    let filters = if filters.is_empty() {
        String::new()
    } else {
        format!("\n WHERE {}", filters.join("\n   AND "))
    };
    let sql = format!(
        "SELECT TOP ({top}) x.k, x.sch, x.nm, x.par, x.r, x.id
  FROM (SELECT u.r, u.id, u.k, u.sch, u.nm, u.par,
               CASE WHEN u.sch IS NULL THEN 0 ELSE 1 END AS h,
               {ls} AS ls, {ln} AS ln,
               ISNULL(u.sch, N'') COLLATE {bin} AS s, u.nm COLLATE {bin} AS n
          FROM ({sources}) u) x{filters}
 ORDER BY x.h, x.ls, x.ln, x.s, x.n, x.r, x.id",
        top = q.page_size() as u64 + 1,
        ls = lower(v, "ISNULL(u.sch, N'')"),
        ln = lower(v, "u.nm"),
        sources = sources.join("\n UNION ALL\n"),
    );
    (sql, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(f: impl FnOnce(&mut ObjectQuery)) -> ObjectQuery {
        let mut q = ObjectQuery::default();
        f(&mut q);
        q
    }

    /// The kinds a query reaches, read back from its sources' text.
    fn kinds_of(v: Variant, q: &ObjectQuery) -> Vec<String> {
        sources(v, q)
            .iter()
            .flat_map(|s| {
                s.split(" N'")
                    .skip(1)
                    .filter_map(|p| p.split_once('\''))
                    .map(|(k, _)| k.to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn a_cursor_reads_back_whatever_the_names_hold() {
        for c in [
            Cursor(Some("dbo".into()), "Orders".into(), 0, 1_234_567),
            Cursor(None, "cat \"a\"], 'b'".into(), 3, -5),
            Cursor(Some("Ünïcode \u{2028}".into()), String::new(), 1, i64::MAX),
        ] {
            assert_eq!(Cursor::parse(&c.write()), Some(c));
        }
        // The app's own cursor (a position in the whole list) isn't ours.
        assert_eq!(Cursor::parse("1000"), None);
        assert_eq!(Cursor::parse("garbage"), None);
    }

    #[test]
    fn a_folder_reads_only_its_kind() {
        let q = query(|q| {
            q.schema = Some("dbo".into());
            q.kind = Some(kinds::TABLE.into());
        });
        let s = sources(Variant::SqlServer, &q);
        assert_eq!(s.len(), 1);
        assert!(s[0].contains("o.type IN ('U')"), "{}", s[0]);
        assert!(!s[0].contains("sys.types"));
        let q = query(|q| q.kind = Some(kinds::FUNCTION.into()));
        let s = sources(Variant::SqlServer, &q);
        assert_eq!(s.len(), 1);
        assert!(
            s[0].contains("o.type IN ('FN', 'IF', 'TF', 'FS', 'FT')"),
            "{}",
            s[0]
        );
        assert_eq!(
            kinds_of(
                Variant::SqlServer,
                &query(|q| q.kind = Some(kinds::TYPE.into()))
            ),
            [kinds::TYPE]
        );
        // A kind the engine doesn't have: no query at all.
        for (v, kind) in [
            (Variant::SqlServer, "collection"),
            (Variant::Fabric, kinds::TYPE),
            (Variant::Fabric, kinds::SEQUENCE),
            (Variant::Babelfish, kinds::SYNONYM),
            (Variant::Babelfish, FULLTEXT_CATALOG),
        ] {
            let q = query(|q| q.kind = Some(kind.into()));
            assert!(sources(v, &q).is_empty(), "{v:?} {kind}");
        }
    }

    #[test]
    fn each_engine_reads_what_list_objects_reads() {
        let all = ObjectQuery::default();
        let mut sql_server = vec![
            kinds::TABLE,
            kinds::VIEW,
            kinds::PROCEDURE,
            kinds::PROCEDURE,
            kinds::FUNCTION,
            kinds::FUNCTION,
            kinds::FUNCTION,
            kinds::FUNCTION,
            kinds::FUNCTION,
            kinds::TRIGGER,
        ];
        let fabric = sql_server.clone();
        let mut babelfish = sql_server.clone();
        babelfish.push(kinds::TYPE);
        sql_server.extend([
            kinds::SEQUENCE,
            kinds::SYNONYM,
            kinds::TYPE,
            FULLTEXT_CATALOG,
            FULLTEXT_STOPLIST,
        ]);
        assert_eq!(kinds_of(Variant::SqlServer, &all), sql_server);
        assert_eq!(kinds_of(Variant::AzureSql, &all), sql_server);
        assert_eq!(kinds_of(Variant::Babelfish, &all), babelfish);
        assert_eq!(kinds_of(Variant::Fabric, &all), fabric);
        // Assembly types are left out where list_objects leaves them out.
        let types = query(|q| q.kind = Some(kinds::TYPE.into()));
        assert!(sources(Variant::SqlServer, &types)[0].contains("is_assembly_type = 0"));
        assert!(!sources(Variant::Babelfish, &types)[0].contains("is_assembly_type"));
    }

    #[test]
    fn schema_less_objects_and_other_kinds() {
        // Only the full-text catalogs and stoplists have no schema.
        let q = query(|q| {
            q.no_schema = true;
            q.schema = Some("dbo".into());
        });
        assert_eq!(
            kinds_of(Variant::SqlServer, &q),
            [FULLTEXT_CATALOG, FULLTEXT_STOPLIST]
        );
        assert!(sources(Variant::Babelfish, &q).is_empty());
        assert!(sources(Variant::Fabric, &q).is_empty());
        // ...so a schema's folder never reads them.
        let q = query(|q| q.schema = Some("dbo".into()));
        assert!(!kinds_of(Variant::SqlServer, &q).contains(&FULLTEXT_CATALOG.to_string()));
        // "Otros" leaves out the declared kinds.
        let q = query(|q| {
            q.not_kinds = vec![
                kinds::TABLE.into(),
                kinds::VIEW.into(),
                kinds::PROCEDURE.into(),
                kinds::FUNCTION.into(),
            ]
        });
        assert_eq!(
            kinds_of(Variant::Fabric, &q),
            [kinds::TRIGGER],
            "Fabric doesn't declare triggers"
        );
        let q = query(|q| q.not_kinds = vec![kinds::TABLE.into(), FULLTEXT_STOPLIST.into()]);
        let k = kinds_of(Variant::SqlServer, &q);
        assert!(!k.contains(&kinds::TABLE.to_string()), "{k:?}");
        assert!(!k.contains(&FULLTEXT_STOPLIST.to_string()), "{k:?}");
        assert!(k.contains(&FULLTEXT_CATALOG.to_string()), "{k:?}");
    }

    #[test]
    fn user_text_is_bound_never_written() {
        let evil = "x']; DROP TABLE t; --";
        let after = Cursor(Some(evil.into()), evil.into(), 0, 42);
        let q = query(|q| {
            q.schema = Some(evil.into());
            q.name = Some(evil.into());
            q.pattern = Some(evil.into());
            q.limit = 7;
        });
        let (sql, params) = page_sql(
            Variant::SqlServer,
            &q,
            Some(&after),
            &sources(Variant::SqlServer, &q),
        );
        assert!(!sql.contains("DROP"), "{sql}");
        assert_eq!(params, [evil; 5]);
        for p in ["@P1", "@P2", "@P3", "@P4", "@P5"] {
            assert!(sql.contains(p), "{p} in {sql}");
        }
        assert!(sql.contains("SELECT TOP (8) "), "one more than the page");
        assert!(sql.contains("CHARINDEX(LOWER(@P3 COLLATE Latin1_General_100_BIN2), x.ln) > 0"));
        assert!(sql.contains("x.h = 1 AND x.s = @P1 AND DATALENGTH(x.s) = DATALENGTH(@P1)"));
    }

    #[test]
    fn the_cursor_resumes_after_the_whole_sort_key() {
        let after = Cursor(None, "cat".into(), 2, 9);
        let q = ObjectQuery::default();
        let (sql, params) = page_sql(Variant::SqlServer, &q, Some(&after), &[]);
        assert_eq!(params, ["", "cat"]);
        let l = |p: &str| format!("LOWER({p} COLLATE Latin1_General_100_BIN2)");
        let want = format!(
            "(x.h > 0 OR (x.h = 0 AND (x.ls > {s} OR (x.ls = {s} AND (x.ln > {n} OR (x.ln = {n} AND \
             (x.s > @P1 OR (x.s = @P1 AND (x.n > @P2 OR (x.n = @P2 AND (x.r > 2 OR (x.r = 2 AND \
             x.id > 9))))))))))))",
            s = l("@P1"),
            n = l("@P2"),
        );
        assert!(sql.contains(&want), "{sql}\n---\n{want}");
        assert!(sql.ends_with("ORDER BY x.h, x.ls, x.ln, x.s, x.n, x.r, x.id"));
        // A page with no filters and no cursor has no WHERE.
        let (sql, params) = page_sql(Variant::SqlServer, &q, None, &[]);
        assert!(!sql.contains("WHERE"), "{sql}");
        assert!(params.is_empty());
        assert!(sql.contains("SELECT TOP (1001) "), "the default page");
    }

    #[test]
    fn each_engine_lowercases_and_sorts_by_code_point() {
        let q = query(|q| q.pattern = Some(String::new()));
        let (sql, params) = page_sql(Variant::Babelfish, &q, None, &[]);
        assert!(params.is_empty(), "an empty search is no filter");
        assert!(
            sql.contains("LOWER(u.nm) COLLATE Latin1_General_100_BIN2 AS ln"),
            "{sql}"
        );
        let (sql, _) = page_sql(Variant::AzureSql, &q, None, &[]);
        assert!(
            sql.contains("LOWER(u.nm COLLATE Latin1_General_100_BIN2) AS ln"),
            "{sql}"
        );
        let (sql, _) = page_sql(Variant::Fabric, &q, None, &[]);
        assert!(
            sql.contains("u.nm COLLATE Latin1_General_100_BIN2_UTF8 AS n"),
            "{sql}"
        );
        let q = query(|q| q.no_schema = true);
        let (sql, _) = page_sql(Variant::SqlServer, &q, None, &[]);
        assert!(sql.contains("WHERE x.h = 0\n"), "{sql}");
    }

    #[test]
    fn after_sql_compares_keys_in_order() {
        assert_eq!(after_sql(&[("a", "1".into())]), "a > 1");
        assert_eq!(
            after_sql(&[("a", "1".into()), ("b", "@P1".into())]),
            "(a > 1 OR (a = 1 AND b > @P1))"
        );
    }
}
