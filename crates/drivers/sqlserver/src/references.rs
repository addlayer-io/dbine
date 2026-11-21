//! What an object uses (`Session::references`), from the catalog.
//!
//! - Views, procedures, functions and triggers: what
//!   `sys.sql_expression_dependencies` records for them (`referencing_id`
//!   the target), which the server keeps from the body it compiled. A name
//!   it resolves to an object of this database is confirmed; one with
//!   another database or a linked server goes to `external`. A trigger
//!   also uses its table (`sys.triggers.parent_id`).
//! - Tables: the tables their foreign keys point at, and the functions and
//!   types their computed columns, CHECK and DEFAULT constraints use (the
//!   same view, with the column or the constraint as the referencing
//!   entity).
//! - Synonyms: `sys.synonyms.base_object_name`, in this database or
//!   outside it.
//!
//! The body is still read once: each object gets the lines that name it,
//! and the names the catalog doesn't resolve (an object that doesn't exist
//! yet, a name resolved with each caller's default schema) are looked up by
//! name like the generic scan does, as probable, so nothing the generic
//! version finds is lost. A name written with another database is the
//! catalog's (it records every one). Where the view can't be read (no
//! permission, an engine without it), the generic version runs and the
//! note says why.

use crate::objects_page::{MORE_OBJECT_TYPES, OBJECT_TYPES};
use crate::variant::{self, Variant};
use crate::{index_usage, text, SqlServerSession};
use dbine_driver::dependencies::{
    find_mentions, foreign_key_parents, references_scan, used_names, used_objects, Confidence,
    DependencyReport, DependencyScan, DependencyTarget, Dependent, ExternalRef, Mention, Relation,
    UsedName, LINE_CHARS,
};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{kinds, DbObject, ObjectRef, Result, Session};
use tiberius::Row;

/// `@P1` the target (`[schema].[name]`). One row per object it uses: how
/// (`via`: '' its body, `column`, `check`, `default`, `parent`, `synonym`),
/// the column or constraint that uses it, the name as written (server,
/// database, schema, entity), what it resolves to here (`sys.objects.type`
/// or `TYPE`, schema, name), `DB_NAME()` and a linked server's product.
const USES_SQL: &str = "WITH target AS (SELECT OBJECT_ID(@P1) AS id),
src AS (
  SELECT t.id AS object_id, CAST(N'' AS nvarchar(8)) AS via, CAST(NULL AS sysname) AS sub
    FROM target t
  UNION ALL
  SELECT o.object_id, CASE o.type WHEN 'C' THEN N'check' ELSE N'default' END, o.name
    FROM sys.objects o
    JOIN target t ON o.parent_object_id = t.id
   WHERE o.type IN ('C', 'D'))
SELECT DISTINCT CASE WHEN d.referencing_minor_id > 0 THEN N'column' ELSE src.via END,
       CASE WHEN d.referencing_minor_id > 0 THEN COL_NAME(d.referencing_id, d.referencing_minor_id) ELSE src.sub END,
       d.referenced_server_name, d.referenced_database_name, d.referenced_schema_name, d.referenced_entity_name,
       CASE WHEN d.referenced_class = 6 THEN 'TYPE' ELSE RTRIM(o.type) END,
       COALESCE(SCHEMA_NAME(o.schema_id), SCHEMA_NAME(ty.schema_id)), COALESCE(o.name, ty.name),
       DB_NAME(), COALESCE(NULLIF(sv.product, N''), sv.provider)
  FROM src
  JOIN sys.sql_expression_dependencies d ON d.referencing_id = src.object_id
  LEFT JOIN sys.objects o ON d.referenced_class = 1 AND o.object_id = d.referenced_id
  LEFT JOIN sys.types ty ON d.referenced_class = 6 AND ty.user_type_id = d.referenced_id
  LEFT JOIN sys.servers sv ON sv.name COLLATE DATABASE_DEFAULT = d.referenced_server_name
 WHERE d.referenced_class IN (1, 6)
   AND NOT (d.referenced_class = 1 AND ISNULL(d.referenced_id, 0) = (SELECT id FROM target))
UNION ALL
SELECT N'parent', NULL, NULL, NULL, SCHEMA_NAME(p.schema_id), p.name,
       RTRIM(p.type), SCHEMA_NAME(p.schema_id), p.name, DB_NAME(), NULL
  FROM sys.triggers tr
  JOIN sys.objects p ON p.object_id = tr.parent_id
 WHERE tr.object_id = OBJECT_ID(@P1) AND tr.parent_class = 1
UNION ALL
SELECT N'synonym', NULL, PARSENAME(sn.base_object_name, 4), PARSENAME(sn.base_object_name, 3),
       PARSENAME(sn.base_object_name, 2), PARSENAME(sn.base_object_name, 1),
       RTRIM(o.type), SCHEMA_NAME(o.schema_id), o.name, DB_NAME(), COALESCE(NULLIF(sv.product, N''), sv.provider)
  FROM sys.synonyms sn
  LEFT JOIN sys.objects o ON PARSENAME(sn.base_object_name, 4) IS NULL
        AND ISNULL(PARSENAME(sn.base_object_name, 3), DB_NAME()) = DB_NAME()
        AND o.object_id = OBJECT_ID(sn.base_object_name)
  LEFT JOIN sys.servers sv ON sv.name COLLATE DATABASE_DEFAULT = PARSENAME(sn.base_object_name, 4)
 WHERE sn.object_id = OBJECT_ID(@P1)";

/// `USES_SQL` for `v`. A Fabric warehouse has no linked servers, triggers
/// or synonyms: only the dependency view is read there.
fn uses_sql(v: Variant) -> std::borrow::Cow<'static, str> {
    if v != Variant::Fabric {
        return USES_SQL.into();
    }
    USES_SQL
        .split("\nUNION ALL\nSELECT N'parent'")
        .next()
        .unwrap_or_default()
        .replace("COALESCE(NULLIF(sv.product, N''), sv.provider)", "NULL")
        .replace(
            "\n  LEFT JOIN sys.servers sv ON sv.name COLLATE DATABASE_DEFAULT = d.referenced_server_name",
            "",
        )
        .into()
}

/// The `sys.objects` types a body can use, as `list_objects` names them.
const USABLE_TYPES: &str = "'U','V','P','PC','FN','IF','TF','FS','FT','SO','SN'";

/// Names in one lookup (SQL Server takes up to 2,100 parameters).
const NAMES_PER_LOOKUP: usize = 1000;

/// The objects named `@P1`…`@Pn` (`USABLE_TYPES`): type, schema, name.
fn named_sql(n: usize) -> String {
    let params: Vec<String> = (1..=n).map(|i| format!("@P{i}")).collect();
    format!(
        "SELECT RTRIM(o.type), s.name, o.name
  FROM sys.objects o
  JOIN sys.schemas s ON s.schema_id = o.schema_id
 WHERE o.is_ms_shipped = 0
   AND o.type IN ({USABLE_TYPES})
   AND o.name IN ({})",
        params.join(", ")
    )
}

/// The kinds answered here; the rest take the generic version.
const KINDS: &[&str] = &[
    kinds::TABLE,
    kinds::VIEW,
    kinds::PROCEDURE,
    kinds::FUNCTION,
    kinds::TRIGGER,
    kinds::SYNONYM,
];
/// The kinds with a body in `sys.sql_modules`.
const MODULE_KINDS: &[&str] = &[
    kinds::VIEW,
    kinds::PROCEDURE,
    kinds::FUNCTION,
    kinds::TRIGGER,
];

pub(crate) async fn references(
    s: &mut SqlServerSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let t = &target.object;
    if !KINDS.contains(&t.kind.as_str()) {
        return references_scan(s, target, scan).await;
    }
    let mut report = match catalog(s, t).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("sqlserver: dependency catalog unreadable: {e}");
            let mut r = references_scan(s, target, scan).await?;
            r.note = Some(note(s.variant).into());
            return Ok(r);
        }
    };
    if MODULE_KINDS.contains(&t.kind.as_str()) {
        match s.definition(t).await {
            Ok(Some(body)) => {
                report.scanned = 1;
                let used = used_names(&body, &scan.dialect);
                add_mentions(&body, &used, scan, t, &mut report);
                let rest = unresolved(used, &report);
                let objects = s.objects_named(&rest).await?;
                report.items.extend(used_objects(&body, &rest, &objects, t));
            }
            // Encrypted, or no VIEW DEFINITION: the catalog's rows stand.
            Ok(None) | Err(_) => report.unreadable.push(
                t.schema()
                    .map_or(t.name.clone(), |sc| format!("{sc}.{}", t.name)),
            ),
        }
    }
    report.items.sort_by_key(|d| {
        (
            d.confidence,
            d.relation,
            d.schema.clone(),
            d.name.to_lowercase(),
        )
    });
    Ok(report)
}

/// The note when `sys.sql_expression_dependencies` can't be read.
fn note(v: Variant) -> &'static str {
    match v {
        Variant::SqlServer | Variant::AzureSql => {
            "No se pudo leer sys.sql_expression_dependencies (hace falta VIEW DEFINITION en la base y SELECT sobre esa vista, que por omisión solo tiene db_owner): lo que usa su código se buscó en su definición."
        }
        Variant::Fabric | Variant::Babelfish => {
            "No se pudo leer sys.sql_expression_dependencies: lo que usa su código se buscó en su definición."
        }
    }
}

/// What the catalog records `t` uses: foreign keys (a table), then
/// `USES_SQL`.
async fn catalog(s: &mut SqlServerSession, t: &ObjectRef) -> Result<DependencyReport> {
    let object = qualified_name(Quote::Bracket, t.schema(), &t.name);
    let mut report = DependencyReport::default();
    if t.kind == kinds::TABLE {
        let rows: Vec<_> = s
            .catalog_rows(index_usage::FOREIGN_KEYS_SQL, &[&object])
            .await?
            .iter()
            .map(|r| {
                (
                    text(r, 0).unwrap_or_default(),
                    text(r, 1).unwrap_or_default(),
                    text(r, 2),
                    text(r, 3).unwrap_or_default(),
                    text(r, 4).unwrap_or_default(),
                )
            })
            .collect();
        report
            .items
            .extend(foreign_key_parents(&index_usage::foreign_keys(&rows), t));
    }
    let engine = variant::info(s.variant).name;
    for r in s.catalog_rows(&uses_sql(s.variant), &[&object]).await? {
        match classify(&use_row(&r), engine) {
            Used::Item(d) => add_item(&mut report.items, d),
            Used::External(x) => {
                if !report.external.contains(&x) {
                    report.external.push(x);
                }
            }
            Used::Unresolved => {}
        }
    }
    Ok(report)
}

impl SqlServerSession {
    /// The objects a body may use that are named like one of `used`.
    async fn objects_named(&mut self, used: &[UsedName]) -> Result<Vec<DbObject>> {
        let mut names: Vec<&str> = Vec::new();
        for u in used {
            if !names.iter().any(|n| same(n, &u.name)) {
                names.push(&u.name);
            }
        }
        let mut out = Vec::new();
        for chunk in names.chunks(NAMES_PER_LOOKUP) {
            for r in self.catalog_rows(&named_sql(chunk.len()), chunk).await? {
                if let (Some(kind), Some(name)) =
                    (text(&r, 0).as_deref().and_then(kind_of), text(&r, 2))
                {
                    out.push(DbObject {
                        kind: kind.into(),
                        schema: text(&r, 1),
                        name,
                        parent: None,
                    });
                }
            }
        }
        Ok(out)
    }
}

/// One `USES_SQL` row.
#[derive(Debug, Clone, Default, PartialEq)]
struct UseRow {
    via: String,
    /// The computed column or the constraint that uses it.
    sub: Option<String>,
    server: Option<String>,
    database: Option<String>,
    schema: Option<String>,
    entity: Option<String>,
    /// `sys.objects.type` (or `TYPE`) of what it resolves to here.
    kind: Option<String>,
    resolved_schema: Option<String>,
    resolved_name: Option<String>,
    /// `DB_NAME()`.
    current: Option<String>,
    /// A linked server's product (or provider).
    product: Option<String>,
}

fn use_row(r: &Row) -> UseRow {
    UseRow {
        via: text(r, 0).unwrap_or_default(),
        sub: text(r, 1),
        server: text(r, 2),
        database: text(r, 3),
        schema: text(r, 4),
        entity: text(r, 5),
        kind: text(r, 6),
        resolved_schema: text(r, 7),
        resolved_name: text(r, 8),
        current: text(r, 9),
        product: text(r, 10),
    }
}

#[derive(Debug, PartialEq)]
enum Used {
    Item(Dependent),
    External(ExternalRef),
    /// A name in this database that resolves to nothing the explorer lists:
    /// left to the body's lookup.
    Unresolved,
}

/// A row as an object of this database (confirmed) or an outside source.
/// `engine` names the system of another database on the same server.
fn classify(u: &UseRow, engine: &str) -> Used {
    let other_db = u
        .database
        .as_deref()
        .is_some_and(|d| u.current.as_deref().is_none_or(|c| !same(d, c)));
    if u.server.is_some() || other_db {
        return Used::External(ExternalRef {
            system: if u.server.is_some() {
                u.product.clone()
            } else {
                Some(engine.into())
            },
            server: u.server.clone(),
            database: u.database.clone(),
            schema: u.schema.clone(),
            name: u.entity.clone(),
            detail: u.sub.clone(),
            mentions: Vec::new(),
        });
    }
    let (Some(kind), Some(name)) = (u.kind.as_deref().and_then(kind_of), u.resolved_name.clone())
    else {
        return Used::Unresolved;
    };
    let (relation, detail) = match u.via.as_str() {
        "check" => (Relation::Check, u.sub.clone()),
        "column" | "default" => (Relation::Code, u.sub.clone()),
        _ => (Relation::Code, None),
    };
    Used::Item(Dependent {
        kind: kind.into(),
        schema: u.resolved_schema.clone(),
        name,
        parent: None,
        relation,
        confidence: Confidence::Confirmed,
        detail,
        mentions: Vec::new(),
    })
}

/// The explorer's kind for a `sys.objects.type` (`TYPE`: a user type).
fn kind_of(code: &str) -> Option<&'static str> {
    if code == "TYPE" {
        return Some(kinds::TYPE);
    }
    OBJECT_TYPES
        .iter()
        .chain(MORE_OBJECT_TYPES)
        .find(|(c, _)| *c == code)
        .map(|&(_, k)| k)
}

/// `d` once per object and relation; the columns and constraints that use
/// it are joined in its detail.
fn add_item(items: &mut Vec<Dependent>, d: Dependent) {
    let Some(x) = items.iter_mut().find(|x| {
        x.kind == d.kind && x.schema == d.schema && x.name == d.name && x.relation == d.relation
    }) else {
        items.push(d);
        return;
    };
    if let Some(sub) = d.detail {
        x.detail = Some(match x.detail.take() {
            Some(prev) if prev.split(", ").any(|p| p == sub) => prev,
            Some(prev) => format!("{prev}, {sub}"),
            None => sub,
        });
    }
}

/// The lines of `body` that name each object and outside source.
fn add_mentions(
    body: &str,
    used: &[UsedName],
    scan: &DependencyScan,
    target: &ObjectRef,
    report: &mut DependencyReport,
) {
    let code = without_variables(body);
    for d in &mut report.items {
        let named = DependencyTarget {
            object: ObjectRef {
                kind: d.kind.clone(),
                schema: d.schema.clone(),
                name: d.name.clone(),
            },
            column: None,
        };
        if let Some((_, mut m)) = find_mentions(&code, &scan.dialect, &named) {
            for m in &mut m {
                m.text = line(body, m.line);
            }
            d.mentions = m;
        }
    }
    for x in &mut report.external {
        x.mentions = external_mentions(body, used, x, target);
    }
}

/// `body` with the names of its variables (`@ids`, `@@ROWCOUNT`) blanked,
/// lines and offsets kept: the tokenizer reads `@ids` as `@` and `ids`, and
/// a table variable named like a type or a table isn't that object.
fn without_variables(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut var = false;
    for c in body.chars() {
        if var && (c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$')) {
            out.extend(std::iter::repeat_n(' ', c.len_utf8()));
            continue;
        }
        var = c == '@';
        out.push(c);
    }
    out
}

/// Line `n` (1-based) of `body` as a mention shows it.
fn line(body: &str, n: u32) -> String {
    body.split('\n')
        .nth(n.saturating_sub(1) as usize)
        .unwrap_or_default()
        .trim()
        .chars()
        .take(LINE_CHARS)
        .collect()
}

/// Where `body` writes `x` with its database (and server): a local object
/// of the same name, or the same one through another server, isn't it.
fn external_mentions(
    body: &str,
    used: &[UsedName],
    x: &ExternalRef,
    target: &ObjectRef,
) -> Vec<Mention> {
    let Some(name) = x.name.as_deref() else {
        return Vec::new();
    };
    // Nearest first, as `UsedName::quals`.
    let mut want = vec![
        x.schema.as_deref(),
        x.database.as_deref(),
        x.server.as_deref(),
    ];
    while want.last() == Some(&None) {
        want.pop();
    }
    let hits: Vec<UsedName> = used
        .iter()
        .filter(|u| {
            same(&u.name, name)
                && u.quals.len() == want.len()
                && u.quals
                    .iter()
                    .zip(&want)
                    .all(|(q, w)| w.is_some_and(|w| same(q, w)))
        })
        .cloned()
        .collect();
    let Some(first) = hits.first() else {
        return Vec::new();
    };
    let source = DbObject {
        kind: "external".into(),
        schema: None,
        name: first.name.clone(),
        parent: None,
    };
    used_objects(body, &hits, &[source], target)
        .pop()
        .map(|d| d.mentions)
        .unwrap_or_default()
}

/// The names the body uses that the catalog didn't resolve: not named like
/// an object it records, not a temporary table or a variable, and not
/// written with a database (the catalog records those).
fn unresolved(used: Vec<UsedName>, report: &DependencyReport) -> Vec<UsedName> {
    used.into_iter()
        .filter(|u| {
            !u.name.starts_with(['#', '@'])
                && u.quals.len() < 2
                && !report.items.iter().any(|d| same(&d.name, &u.name))
        })
        .collect()
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ScriptDialect;

    fn row(via: &str, kind: Option<&str>, schema: &str, name: &str) -> UseRow {
        UseRow {
            via: via.into(),
            schema: Some(schema.into()),
            entity: Some(name.into()),
            kind: kind.map(Into::into),
            resolved_schema: kind.map(|_| schema.into()),
            resolved_name: kind.map(|_| name.into()),
            current: Some("ventas_db".into()),
            ..Default::default()
        }
    }

    fn item(u: &UseRow) -> Dependent {
        match classify(u, "SQL Server") {
            Used::Item(d) => d,
            other => panic!("{other:?}"),
        }
    }

    fn scan() -> DependencyScan {
        DependencyScan {
            source_kinds: Vec::new(),
            dialect: ScriptDialect::tsql(),
            foreign_keys: true,
            written_kinds: Vec::new(),
            has_schemas: true,
        }
    }

    fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    #[test]
    fn the_query_reads_the_dependency_view_triggers_and_synonyms() {
        for part in [
            "JOIN sys.sql_expression_dependencies d ON d.referencing_id = src.object_id",
            "WHERE o.type IN ('C', 'D')",
            "COL_NAME(d.referencing_id, d.referencing_minor_id)",
            "d.referenced_class IN (1, 6)",
            // Another database's rows have no id: kept (`NULL = id` would drop them).
            "AND NOT (d.referenced_class = 1 AND ISNULL(d.referenced_id, 0) = (SELECT id FROM target))",
            "WHERE tr.object_id = OBJECT_ID(@P1) AND tr.parent_class = 1",
            "PARSENAME(sn.base_object_name, 4)",
            "WHERE sn.object_id = OBJECT_ID(@P1)",
            "sv.name COLLATE DATABASE_DEFAULT = d.referenced_server_name",
        ] {
            assert!(USES_SQL.contains(part), "{part}");
        }
        // Another database's id never resolves against this one's objects.
        assert!(USES_SQL.contains(
            "ISNULL(PARSENAME(sn.base_object_name, 3), DB_NAME()) = DB_NAME()\n        AND o.object_id = OBJECT_ID(sn.base_object_name)"
        ));
    }

    #[test]
    fn fabric_reads_only_the_dependency_view() {
        assert_eq!(uses_sql(Variant::SqlServer), USES_SQL);
        assert_eq!(uses_sql(Variant::Babelfish), USES_SQL);
        let fabric = uses_sql(Variant::Fabric);
        assert!(fabric.contains("JOIN sys.sql_expression_dependencies d"));
        for gone in [
            "sys.servers",
            "sys.triggers",
            "sys.synonyms",
            "sv.",
            "UNION ALL\nSELECT N'",
        ] {
            assert!(!fabric.contains(gone), "{gone}: {fabric}");
        }
        assert!(fabric.contains("DB_NAME(), NULL\n  FROM src"), "{fabric}");
    }

    #[test]
    fn the_lookup_binds_every_name() {
        let sql = named_sql(3);
        assert!(sql.ends_with("AND o.name IN (@P1, @P2, @P3)"), "{sql}");
        assert!(sql.contains("o.is_ms_shipped = 0"));
        for code in USABLE_TYPES.split(',') {
            assert!(kind_of(code.trim_matches('\'')).is_some(), "{code}");
        }
        assert!(!USABLE_TYPES.contains("'TR'"));
    }

    #[test]
    fn rows_of_this_database_are_confirmed_objects() {
        let view = item(&row("", Some("V"), "dbo", "vClientes"));
        assert_eq!(view.kind, kinds::VIEW);
        assert_eq!(view.schema.as_deref(), Some("dbo"));
        assert_eq!(view.relation, Relation::Code);
        assert_eq!(view.confidence, Confidence::Confirmed);
        assert_eq!(view.detail, None);
        for (code, kind) in [
            ("U", kinds::TABLE),
            ("P", kinds::PROCEDURE),
            ("IF", kinds::FUNCTION),
            ("SO", kinds::SEQUENCE),
            ("SN", kinds::SYNONYM),
            ("TYPE", kinds::TYPE),
        ] {
            assert_eq!(item(&row("", Some(code), "dbo", "x")).kind, kind, "{code}");
        }
        // Written with this database's own name: still here.
        let mut own = row("", Some("U"), "dbo", "Pedidos");
        own.database = Some("VENTAS_DB".into());
        assert_eq!(item(&own).kind, kinds::TABLE);
        // A name that resolves to nothing, or to a kind the explorer doesn't list.
        assert_eq!(
            classify(&row("", None, "dbo", "NoExiste"), "SQL Server"),
            Used::Unresolved
        );
        assert_eq!(
            classify(&row("", Some("ET"), "dbo", "x"), "SQL Server"),
            Used::Unresolved
        );
    }

    #[test]
    fn a_table_says_which_column_or_constraint_uses_it() {
        let mut check = row("check", Some("FN"), "dbo", "f_ok");
        check.sub = Some("CK_q".into());
        let check = item(&check);
        assert_eq!(check.relation, Relation::Check);
        assert_eq!(check.detail.as_deref(), Some("CK_q"));
        let mut column = row("column", Some("FN"), "dbo", "f_ok");
        column.sub = Some("total".into());
        let mut default = row("default", Some("FN"), "dbo", "f_ok");
        default.sub = Some("DF_d".into());
        let mut items = Vec::new();
        for d in [item(&column), item(&default), item(&column), check] {
            add_item(&mut items, d);
        }
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].relation, Relation::Code);
        assert_eq!(items[0].detail.as_deref(), Some("total, DF_d"));
        assert_eq!(items[1].relation, Relation::Check);
    }

    #[test]
    fn a_trigger_uses_its_table() {
        let parent = item(&row("parent", Some("U"), "dbo", "Pedidos"));
        assert_eq!(parent.kind, kinds::TABLE);
        assert_eq!(parent.name, "Pedidos");
        assert_eq!(parent.confidence, Confidence::Confirmed);
    }

    #[test]
    fn other_databases_and_linked_servers_are_external() {
        let mut other = row("", None, "dbo", "Remota");
        other.database = Some("otra".into());
        let Used::External(x) = classify(&other, "SQL Server") else {
            panic!()
        };
        assert_eq!(
            x,
            ExternalRef {
                system: Some("SQL Server".into()),
                server: None,
                database: Some("otra".into()),
                schema: Some("dbo".into()),
                name: Some("Remota".into()),
                detail: None,
                mentions: Vec::new(),
            }
        );
        let mut linked = other.clone();
        linked.server = Some("LNK".into());
        linked.product = Some("Oracle".into());
        let Used::External(x) = classify(&linked, "SQL Server") else {
            panic!()
        };
        assert_eq!(x.system.as_deref(), Some("Oracle"));
        assert_eq!(x.server.as_deref(), Some("LNK"));
        // A synonym to another database, even when an object here has that id.
        let mut synonym = row("synonym", Some("U"), "dbo", "Remota");
        synonym.database = Some("otra".into());
        assert!(matches!(
            classify(&synonym, "SQL Server"),
            Used::External(_)
        ));
    }

    #[test]
    fn each_external_gets_the_lines_that_write_it() {
        let body = "CREATE PROCEDURE dbo.p AS\n  SELECT * FROM otra.dbo.Remota;\n  SELECT * FROM LNK.otra.dbo.Remota;\n  SELECT * FROM dbo.Remota;";
        let t = obj(kinds::PROCEDURE, "dbo", "p");
        let used = used_names(body, &ScriptDialect::tsql());
        let mut report = DependencyReport::default();
        for (server, product) in [(None, None), (Some("LNK"), Some("SQL Server"))] {
            let mut u = row("", None, "dbo", "Remota");
            u.database = Some("otra".into());
            u.server = server.map(Into::into);
            u.product = product.map(Into::into);
            let Used::External(x) = classify(&u, "SQL Server") else {
                panic!()
            };
            report.external.push(x);
        }
        add_mentions(body, &used, &scan(), &t, &mut report);
        let lines =
            |i: usize| -> Vec<u32> { report.external[i].mentions.iter().map(|m| m.line).collect() };
        assert_eq!(lines(0), [2]);
        assert_eq!(lines(1), [3]);
    }

    #[test]
    fn catalog_objects_get_their_lines() {
        let body = "CREATE PROCEDURE dbo.p @ids dbo.Ids READONLY AS\nSELECT NEXT VALUE FOR dbo.seq;\nSELECT * FROM Clientes c JOIN ventas.Clientes v ON v.id = c.id;\nSELECT id FROM @ids WHERE @@ROWCOUNT > 0;";
        let t = obj(kinds::PROCEDURE, "dbo", "p");
        let used = used_names(body, &ScriptDialect::tsql());
        let mut report = DependencyReport::default();
        for (code, schema, name) in [
            ("TYPE", "dbo", "Ids"),
            ("SO", "dbo", "seq"),
            ("U", "ventas", "Clientes"),
        ] {
            add_item(&mut report.items, item(&row("", Some(code), schema, name)));
        }
        add_mentions(body, &used, &scan(), &t, &mut report);
        let lines: Vec<Vec<u32>> = report
            .items
            .iter()
            .map(|d| d.mentions.iter().map(|m| m.line).collect())
            .collect();
        // `@ids` is a variable, not the type.
        assert_eq!(lines, [vec![1], vec![2], vec![3]]);
        assert_eq!(
            report.items[0].mentions[0].text,
            "CREATE PROCEDURE dbo.p @ids dbo.Ids READONLY AS"
        );
    }

    #[test]
    fn variables_are_blanked_keeping_lines_and_offsets() {
        let body = "SELECT @ids, @@ROWCOUNT, 'a@b'\nFROM @Año";
        let code = without_variables(body);
        assert_eq!(code.len(), body.len());
        assert_eq!(code, "SELECT @   , @         , 'a@ '\nFROM @    ");
        assert_eq!(line(body, 2), "FROM @Año");
        assert_eq!(line(body, 9), "");
    }

    #[test]
    fn what_the_catalog_left_is_looked_up_by_name() {
        let body = "SELECT * FROM Clientes;\nSELECT * FROM #tmp;\nSELECT * FROM otra.dbo.Remota;\nSELECT * FROM dbo.Pedidos;\nSELECT * FROM NoExiste;";
        let used = used_names(body, &ScriptDialect::tsql());
        let mut report = DependencyReport::default();
        report
            .items
            .push(item(&row("", Some("U"), "dbo", "pedidos")));
        let rest: Vec<String> = unresolved(used, &report)
            .into_iter()
            .map(|u| u.name)
            .collect();
        assert_eq!(rest, ["Clientes", "NoExiste"]);
    }

    #[test]
    fn the_note_says_what_permission_is_missing() {
        assert!(note(Variant::SqlServer).contains("VIEW DEFINITION"));
        assert!(note(Variant::AzureSql).contains("db_owner"));
        assert!(!note(Variant::Babelfish).contains("VIEW DEFINITION"));
        assert!(!note(Variant::Fabric).contains("VIEW DEFINITION"));
    }
}
