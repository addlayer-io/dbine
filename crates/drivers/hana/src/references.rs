//! What an object uses (`Session::references`), from HANA's dependency
//! catalog:
//!
//! - a view, procedure, function, trigger, synonym or sequence: its direct
//!   dependencies in `SYS.OBJECT_DEPENDENCIES` (`DEPENDENCY_TYPE` 1), with
//!   the base object's type as the kind (a table that is a table type is a
//!   type);
//! - a table: the tables its foreign keys point at
//!   (`SYS.REFERENTIAL_CONSTRAINTS`).
//!
//! Those rows are confirmed. A virtual table (`SYS.VIRTUAL_TABLES`) is a
//! remote source's object: as the target, its remote source, database,
//! owner and object go in `external`; as something the target uses, it's
//! listed and its remote object goes in `external` too, naming it. The
//! remote source's adapter (`SYS.REMOTE_SOURCES`) is the system, when the
//! login can read it. `SYS` and `PUBLIC` objects (monitoring views, the
//! public synonyms) are left out.
//!
//! The target's own source is read too ([`with_source`]): the lines that
//! name each object, and what it names that the catalog didn't record,
//! resolved as the default does (probable). Table types, which the catalog
//! doesn't track, and a catalog view the login can't read get the default,
//! the latter with a note.

use crate::{text, HanaSession};
use dbine_driver::dependencies::{
    find_mentions, foreign_key_parents, references_scan, used_names, used_objects, ExternalRef,
    UsedName, LOOKUP_CAP,
};
use dbine_driver::{
    kinds, Confidence, DbObject, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    Error, ForeignKeyDef, ObjectQuery, ObjectRef, Relation, Result, Session,
};
use std::collections::HashMap;

type Rows = Vec<Vec<Option<String>>>;

/// The target's direct dependencies (`DEPENDENT_*` = the target), with
/// whether a base table is a table type and, for a virtual table, its
/// remote side.
pub(crate) const USES: &str = "SELECT d.BASE_SCHEMA_NAME, d.BASE_OBJECT_NAME, d.BASE_OBJECT_TYPE,
       t.IS_USER_DEFINED_TYPE,
       v.REMOTE_SOURCE_NAME, v.REMOTE_DATABASE_NAME, v.REMOTE_OWNER_NAME, v.REMOTE_OBJECT_NAME
  FROM SYS.OBJECT_DEPENDENCIES d
  LEFT JOIN SYS.TABLES t
    ON d.BASE_OBJECT_TYPE = 'TABLE' AND t.SCHEMA_NAME = d.BASE_SCHEMA_NAME AND t.TABLE_NAME = d.BASE_OBJECT_NAME
  LEFT JOIN SYS.VIRTUAL_TABLES v
    ON d.BASE_OBJECT_TYPE = 'TABLE' AND v.SCHEMA_NAME = d.BASE_SCHEMA_NAME AND v.TABLE_NAME = d.BASE_OBJECT_NAME
 WHERE d.DEPENDENT_SCHEMA_NAME = ? AND d.DEPENDENT_OBJECT_NAME = ? AND d.DEPENDENT_OBJECT_TYPE = ?
   AND d.DEPENDENCY_TYPE = 1
 ORDER BY d.BASE_SCHEMA_NAME, d.BASE_OBJECT_NAME";

/// A table's foreign keys, a row per column, in order.
pub(crate) const FOREIGN_KEYS: &str = "SELECT CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_SCHEMA_NAME,
       REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME
  FROM SYS.REFERENTIAL_CONSTRAINTS WHERE SCHEMA_NAME = ? AND TABLE_NAME = ?
 ORDER BY CONSTRAINT_NAME, POSITION";

/// The remote side of a virtual table.
pub(crate) const VIRTUAL_TABLE: &str =
    "SELECT REMOTE_SOURCE_NAME, REMOTE_DATABASE_NAME, REMOTE_OWNER_NAME, REMOTE_OBJECT_NAME
  FROM SYS.VIRTUAL_TABLES WHERE SCHEMA_NAME = ? AND TABLE_NAME = ?";

const ADAPTERS: &str = "SELECT REMOTE_SOURCE_NAME, ADAPTER_NAME FROM SYS.REMOTE_SOURCES";

/// `DEPENDENT_OBJECT_TYPE` for a target of `kind`; `None` for kinds the
/// catalog doesn't track (tables go through their foreign keys).
pub(crate) fn dependent_type(kind: &str) -> Option<&'static str> {
    Some(match kind {
        kinds::VIEW => "VIEW",
        kinds::PROCEDURE => "PROCEDURE",
        kinds::FUNCTION => "FUNCTION",
        kinds::TRIGGER => "TRIGGER",
        kinds::SYNONYM => "SYNONYM",
        kinds::SEQUENCE => "SEQUENCE",
        _ => return None,
    })
}

/// The kind of a `BASE_OBJECT_TYPE`; `None` for what the explorer doesn't
/// show (indexes, schemas, remote sources, libraries…).
pub(crate) fn kind_of(base_type: &str, table_type: bool) -> Option<&'static str> {
    Some(match base_type {
        "TABLE" if table_type => kinds::TYPE,
        "TABLE" => kinds::TABLE,
        "VIEW" => kinds::VIEW,
        "PROCEDURE" => kinds::PROCEDURE,
        "FUNCTION" => kinds::FUNCTION,
        "SEQUENCE" => kinds::SEQUENCE,
        "SYNONYM" => kinds::SYNONYM,
        "TRIGGER" => kinds::TRIGGER,
        _ => return None,
    })
}

/// Monitoring views and the public synonyms of every system view.
fn system(schema: &str) -> bool {
    matches!(schema, "SYS" | "PUBLIC")
}

/// A schema as the explorer writes it: none for the session's.
fn local_schema(schema: String, session: &str) -> Option<String> {
    (schema != session).then_some(schema)
}

fn cell(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i)
        .cloned()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A virtual table's remote object; `None` when the row has none. A part
/// the remote source doesn't have is `<NULL>` in the catalog.
fn remote(r: &[Option<String>], from: usize, detail: Option<String>) -> Option<ExternalRef> {
    let part = |i: usize| cell(r, from + i).filter(|v| v != "<NULL>");
    Some(ExternalRef {
        system: None,
        server: Some(part(0)?),
        database: part(1),
        schema: part(2),
        name: part(3),
        detail,
        mentions: Vec::new(),
    })
}

/// The [`USES`] rows: the objects (confirmed) and the remote objects of the
/// virtual tables among them, each with the virtual table that names it.
/// The target itself and `SYS` / `PUBLIC` objects are left out.
pub(crate) fn parse_uses(
    rows: &[Vec<Option<String>>],
    target: &ObjectRef,
    session: &str,
) -> (Vec<Dependent>, Vec<(ExternalRef, Option<ObjectRef>)>) {
    let mut items: Vec<Dependent> = Vec::new();
    let mut external: Vec<(ExternalRef, Option<ObjectRef>)> = Vec::new();
    for r in rows {
        let (Some(schema), Some(name), Some(base)) = (cell(r, 0), cell(r, 1), cell(r, 2)) else {
            continue;
        };
        if system(&schema) {
            continue;
        }
        let table_type = cell(r, 3).as_deref() == Some("TRUE");
        let Some(kind) = kind_of(&base, table_type) else {
            continue;
        };
        let display = format!("{schema}.{name}");
        let schema = local_schema(schema, session);
        if let Some(x) = remote(r, 4, Some(display)) {
            if !external.iter().any(|(e, _)| *e == x) {
                let local = ObjectRef {
                    kind: kind.into(),
                    schema: schema.clone(),
                    name: name.clone(),
                };
                external.push((x, Some(local)));
            }
        }
        let d = confirmed(kind, schema, name);
        if !is_target(&d, target, session) && !items.iter().any(|x| same(x, &d)) {
            items.push(d);
        }
    }
    (items, external)
}

/// [`FOREIGN_KEYS`]' rows as the table's foreign keys. A referenced schema
/// is named only when it isn't the table's (`owner`); names HANA made up
/// (`_SYS_…`) are left out.
pub(crate) fn parse_foreign_keys(rows: &[Vec<Option<String>>], owner: &str) -> Vec<ForeignKeyDef> {
    let mut out: Vec<(String, ForeignKeyDef)> = Vec::new();
    for r in rows {
        let (Some(name), Some(col), Some(table), Some(ref_col)) =
            (cell(r, 0), cell(r, 1), cell(r, 3), cell(r, 4))
        else {
            continue;
        };
        match out.last_mut().filter(|(n, _)| *n == name) {
            Some((_, fk)) => {
                fk.columns.push(col);
                fk.ref_columns.push(ref_col);
            }
            None => out.push((
                name.clone(),
                ForeignKeyDef {
                    name: (!name.starts_with("_SYS_")).then_some(name),
                    columns: vec![col],
                    ref_schema: cell(r, 2).filter(|s| s != owner),
                    ref_table: table,
                    ref_columns: vec![ref_col],
                    ..Default::default()
                },
            )),
        }
    }
    out.into_iter().map(|(_, fk)| fk).collect()
}

/// Rows of a catalog query, as text.
async fn text_rows(s: &HanaSession, sql: &str, params: &[&str]) -> Result<Rows> {
    Ok(s.rows(sql, params)
        .await?
        .iter()
        .map(|r| r.iter().map(text).collect())
        .collect())
}

/// `Session::references` (see the module's docs).
pub(crate) async fn references(
    s: &mut HanaSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let t = &target.object;
    let owner = s.owner(t);
    let session = s.schema.clone();
    if t.kind == kinds::TABLE {
        let fks = match text_rows(s, FOREIGN_KEYS, &[&owner, &t.name]).await {
            Ok(rows) => parse_foreign_keys(&rows, &owner),
            Err(e) => return fallback(s, target, scan, "SYS.REFERENTIAL_CONSTRAINTS", e).await,
        };
        let mut report = DependencyReport {
            items: foreign_key_parents(&fks, t),
            ..Default::default()
        };
        match text_rows(s, VIRTUAL_TABLE, &[&owner, &t.name]).await {
            Ok(rows) => report
                .external
                .extend(rows.iter().filter_map(|r| remote(r, 0, None))),
            Err(e) => tracing::debug!("hana: SYS.VIRTUAL_TABLES not read: {e}"),
        }
        name_systems(s, &mut report.external).await;
        sort(&mut report.items);
        return Ok(report);
    }
    let Some(dep_type) = dependent_type(&t.kind) else {
        return references_scan(s, target, scan).await;
    };
    let rows = match text_rows(s, USES, &[&owner, &t.name, dep_type]).await {
        Ok(rows) => rows,
        Err(e) => return fallback(s, target, scan, "SYS.OBJECT_DEPENDENCIES", e).await,
    };
    let (items, mut external) = parse_uses(&rows, t, &session);
    let mut sources: Vec<ExternalRef> = external.iter().map(|(x, _)| x.clone()).collect();
    name_systems(s, &mut sources).await;
    for ((x, _), named) in external.iter_mut().zip(sources) {
        *x = named;
    }
    with_source(s, target, scan, items, external).await
}

/// The remote sources' adapters as their system, when the login can read
/// `SYS.REMOTE_SOURCES`.
async fn name_systems(s: &HanaSession, external: &mut [ExternalRef]) {
    if external.is_empty() {
        return;
    }
    match text_rows(s, ADAPTERS, &[]).await {
        Ok(rows) => {
            let adapters: HashMap<String, String> = rows
                .iter()
                .filter_map(|r| Some((cell(r, 0)?, cell(r, 1)?)))
                .collect();
            set_systems(external, &adapters);
        }
        Err(e) => tracing::debug!("hana: SYS.REMOTE_SOURCES not read: {e}"),
    }
}

fn set_systems(external: &mut [ExternalRef], adapters: &HashMap<String, String>) {
    for x in external {
        if let Some(a) = x.server.as_ref().and_then(|s| adapters.get(s)) {
            x.system = Some(a.clone());
        }
    }
}

/// The default, saying which catalog view couldn't be read.
async fn fallback<S: Session + ?Sized>(
    s: &mut S,
    target: &DependencyTarget,
    scan: &DependencyScan,
    view: &str,
    e: Error,
) -> Result<DependencyReport> {
    tracing::debug!("hana: {view} not read: {e}");
    let mut report = references_scan(s, target, scan).await?;
    report.note = Some(format!("No se pudo leer {view}: {e}"));
    Ok(report)
}

// -- what every catalog answer shares --------------------------------------------------------

/// A catalog row: confirmed, used as code.
pub(crate) fn confirmed(kind: &str, schema: Option<String>, name: String) -> Dependent {
    Dependent {
        kind: kind.into(),
        schema,
        name,
        parent: None,
        relation: Relation::Code,
        confidence: Confidence::Confirmed,
        detail: None,
        mentions: Vec::new(),
    }
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

fn same_schema(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => eq(a, b),
        (a, b) => a.is_none() && b.is_none(),
    }
}

fn same(a: &Dependent, b: &Dependent) -> bool {
    a.kind == b.kind
        && eq(&a.name, &b.name)
        && same_schema(a.schema.as_deref(), b.schema.as_deref())
}

/// `d` is the target (whose schema may be written out or the session's).
fn is_target(d: &Dependent, t: &ObjectRef, session: &str) -> bool {
    let ts = t.schema().filter(|s| *s != session);
    d.kind == t.kind && eq(&d.name, &t.name) && same_schema(d.schema.as_deref(), ts)
}

/// Where `body` names `object`.
fn mentions_of(body: &str, scan: &DependencyScan, object: ObjectRef) -> Vec<dbine_driver::Mention> {
    let target = DependencyTarget {
        object,
        column: None,
    };
    find_mentions(body, &scan.dialect, &target)
        .map(|(_, m)| m)
        .unwrap_or_default()
}

/// What the catalog recorded (`items`, `external` with the local object
/// that stands for each), completed with the target's own source when its
/// kind has written source: the lines that name each, and what it names
/// that the catalog didn't record, resolved against the database's objects
/// as the default does (probable). So the answer never has less than the
/// default's.
pub(crate) async fn with_source<S: Session + ?Sized>(
    s: &mut S,
    target: &DependencyTarget,
    scan: &DependencyScan,
    mut items: Vec<Dependent>,
    external: Vec<(ExternalRef, Option<ObjectRef>)>,
) -> Result<DependencyReport> {
    let t = &target.object;
    let mut report = DependencyReport::default();
    let body = if scan.written_kinds.contains(&t.kind) {
        match s.definition(t).await {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!("references: definition of {} not read: {e}", t.name);
                report.unreadable.push(match t.schema() {
                    Some(sc) if !sc.is_empty() => format!("{sc}.{}", t.name),
                    _ => t.name.clone(),
                });
                None
            }
        }
    } else {
        None
    };
    let Some(body) = body else {
        report.items = items;
        report.external = external.into_iter().map(|(x, _)| x).collect();
        sort(&mut report.items);
        return Ok(report);
    };
    report.scanned = 1;
    for d in &mut items {
        let object = ObjectRef {
            kind: d.kind.clone(),
            schema: d.schema.clone(),
            name: d.name.clone(),
        };
        d.mentions = mentions_of(&body, scan, object);
    }
    let mut locals: Vec<String> = Vec::new();
    for (mut x, local) in external {
        if let Some(local) = local {
            locals.push(local.name.clone());
            x.mentions = mentions_of(&body, scan, local);
        }
        report.external.push(x);
    }
    // What the source names that the catalog didn't record.
    let rest: Vec<UsedName> = used_names(&body, &scan.dialect)
        .into_iter()
        .filter(|u| !items.iter().any(|d| eq(&d.name, &u.name)))
        .filter(|u| !locals.iter().any(|n| eq(n, &u.name)))
        .collect();
    match resolve(s, &rest).await {
        Ok(objects) => {
            for d in used_objects(&body, &rest, &objects, t) {
                if !items.iter().any(|x| same(x, &d)) {
                    items.push(d);
                }
            }
        }
        Err(e) => tracing::debug!("references: names not resolved: {e}"),
    }
    report.items = items;
    sort(&mut report.items);
    Ok(report)
}

/// The database's objects named like `used`, as the default resolves them:
/// one lookup per name, or the whole list once when there are many names
/// or the driver can't look up by name.
async fn resolve<S: Session + ?Sized>(s: &mut S, used: &[UsedName]) -> Result<Vec<DbObject>> {
    let mut names: Vec<&str> = Vec::new();
    for u in used {
        if !names.iter().any(|n| eq(n, &u.name)) {
            names.push(&u.name);
        }
    }
    if names.is_empty() {
        return Ok(Vec::new());
    }
    if names.len() > LOOKUP_CAP {
        return s.list_objects().await;
    }
    let mut found = Vec::new();
    for name in names {
        let q = ObjectQuery {
            name: Some(name.to_string()),
            limit: 50,
            ..Default::default()
        };
        match s.list_objects_page(&q).await {
            Ok(page) => found.extend(page.objects),
            Err(Error::Unsupported(_)) => return s.list_objects().await,
            Err(e) => return Err(e),
        }
    }
    Ok(found)
}

/// Confirmed first, then by relation, schema and name.
pub(crate) fn sort(items: &mut [Dependent]) {
    items.sort_by_key(|d| {
        (
            d.confidence,
            d.relation,
            d.schema.clone().unwrap_or_default(),
            d.name.to_lowercase(),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: &[Option<&str>]) -> Vec<Option<String>> {
        cells.iter().map(|c| c.map(str::to_string)).collect()
    }

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        }
    }

    #[test]
    fn the_queries_take_the_target() {
        assert!(USES.contains("FROM SYS.OBJECT_DEPENDENCIES d"));
        assert!(USES.contains("d.DEPENDENCY_TYPE = 1"));
        assert!(USES.contains(
            "WHERE d.DEPENDENT_SCHEMA_NAME = ? AND d.DEPENDENT_OBJECT_NAME = ? AND d.DEPENDENT_OBJECT_TYPE = ?"
        ));
        assert!(USES.contains("LEFT JOIN SYS.VIRTUAL_TABLES v"));
        assert_eq!(USES.matches('?').count(), 3);
        assert!(FOREIGN_KEYS
            .contains("FROM SYS.REFERENTIAL_CONSTRAINTS WHERE SCHEMA_NAME = ? AND TABLE_NAME = ?"));
        assert_eq!(VIRTUAL_TABLE.matches('?').count(), 2);
        for (kind, t) in [
            (kinds::VIEW, Some("VIEW")),
            (kinds::PROCEDURE, Some("PROCEDURE")),
            (kinds::FUNCTION, Some("FUNCTION")),
            (kinds::TRIGGER, Some("TRIGGER")),
            (kinds::SYNONYM, Some("SYNONYM")),
            (kinds::SEQUENCE, Some("SEQUENCE")),
            (kinds::TABLE, None),
            (kinds::TYPE, None),
        ] {
            assert_eq!(dependent_type(kind), t, "{kind}");
        }
    }

    #[test]
    fn base_types_map_to_kinds() {
        assert_eq!(kind_of("TABLE", false), Some(kinds::TABLE));
        assert_eq!(kind_of("TABLE", true), Some(kinds::TYPE));
        assert_eq!(kind_of("VIEW", false), Some(kinds::VIEW));
        assert_eq!(kind_of("PROCEDURE", false), Some(kinds::PROCEDURE));
        assert_eq!(kind_of("FUNCTION", false), Some(kinds::FUNCTION));
        assert_eq!(kind_of("SEQUENCE", false), Some(kinds::SEQUENCE));
        assert_eq!(kind_of("SYNONYM", false), Some(kinds::SYNONYM));
        assert_eq!(kind_of("TRIGGER", false), Some(kinds::TRIGGER));
        assert_eq!(kind_of("INDEX", false), None);
        assert_eq!(kind_of("REMOTE SOURCE", false), None);
    }

    #[test]
    fn rows_become_confirmed_objects_and_virtual_tables_external() {
        let target = obj(kinds::PROCEDURE, "P_LOAD");
        let none = None;
        let rows = vec![
            row(&[
                Some("APP"),
                Some("ORDERS"),
                Some("TABLE"),
                Some("FALSE"),
                none,
                none,
                none,
                none,
            ]),
            row(&[
                Some("APP"),
                Some("V_TOTALS"),
                Some("VIEW"),
                none,
                none,
                none,
                none,
                none,
            ]),
            row(&[
                Some("APP"),
                Some("TT_LINES"),
                Some("TABLE"),
                Some("TRUE"),
                none,
                none,
                none,
                none,
            ]),
            row(&[
                Some("APP"),
                Some("SEQ_ORDERS"),
                Some("SEQUENCE"),
                none,
                none,
                none,
                none,
                none,
            ]),
            row(&[
                Some("SALES"),
                Some("F_TAX"),
                Some("FUNCTION"),
                none,
                none,
                none,
                none,
                none,
            ]),
            row(&[
                Some("APP"),
                Some("P_LOG"),
                Some("PROCEDURE"),
                none,
                none,
                none,
                none,
                none,
            ]),
            // A virtual table: listed, and its remote object external.
            row(&[
                Some("APP"),
                Some("VT_CLIENTS"),
                Some("TABLE"),
                Some("FALSE"),
                Some("CRM_SRC"),
                Some("CRMDB"),
                Some("dbo"),
                Some("Clients"),
            ]),
            // Left out: the target itself, indexes, SYS and PUBLIC.
            row(&[
                Some("APP"),
                Some("P_LOAD"),
                Some("PROCEDURE"),
                none,
                none,
                none,
                none,
                none,
            ]),
            row(&[
                Some("APP"),
                Some("IX_ORDERS"),
                Some("INDEX"),
                none,
                none,
                none,
                none,
                none,
            ]),
            row(&[
                Some("PUBLIC"),
                Some("DUMMY"),
                Some("SYNONYM"),
                none,
                none,
                none,
                none,
                none,
            ]),
            row(&[
                Some("SYS"),
                Some("M_TABLES"),
                Some("MONITORVIEW"),
                none,
                none,
                none,
                none,
                none,
            ]),
        ];
        let (items, external) = parse_uses(&rows, &target, "APP");
        let got: Vec<(&str, Option<&str>, &str)> = items
            .iter()
            .map(|d| (d.kind.as_str(), d.schema.as_deref(), d.name.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (kinds::TABLE, None, "ORDERS"),
                (kinds::VIEW, None, "V_TOTALS"),
                (kinds::TYPE, None, "TT_LINES"),
                (kinds::SEQUENCE, None, "SEQ_ORDERS"),
                (kinds::FUNCTION, Some("SALES"), "F_TAX"),
                (kinds::PROCEDURE, None, "P_LOG"),
                (kinds::TABLE, None, "VT_CLIENTS"),
            ]
        );
        assert!(items
            .iter()
            .all(|d| d.confidence == Confidence::Confirmed && d.relation == Relation::Code));
        assert_eq!(external.len(), 1);
        let (x, local) = &external[0];
        assert_eq!(
            *x,
            ExternalRef {
                system: None,
                server: Some("CRM_SRC".into()),
                database: Some("CRMDB".into()),
                schema: Some("dbo".into()),
                name: Some("Clients".into()),
                detail: Some("APP.VT_CLIENTS".into()),
                mentions: Vec::new(),
            }
        );
        assert_eq!(local.as_ref().unwrap().name, "VT_CLIENTS");

        // The target in another schema: its own row is still left out.
        let other = ObjectRef {
            schema: Some("SALES".into()),
            ..obj(kinds::FUNCTION, "F_TAX")
        };
        let (items, _) = parse_uses(&rows[4..5], &other, "APP");
        assert!(items.is_empty());
    }

    #[test]
    fn foreign_key_rows_group_by_constraint() {
        let rows = vec![
            row(&[
                Some("FK_LINES_ORDER"),
                Some("ORDER_ID"),
                Some("APP"),
                Some("ORDERS"),
                Some("ID"),
            ]),
            row(&[
                Some("_SYS_CONSTRAINT_1"),
                Some("SKU"),
                Some("CAT"),
                Some("PRODUCTS"),
                Some("SKU"),
            ]),
            row(&[
                Some("_SYS_CONSTRAINT_1"),
                Some("VARIANT"),
                Some("CAT"),
                Some("PRODUCTS"),
                Some("VARIANT"),
            ]),
        ];
        let fks = parse_foreign_keys(&rows, "APP");
        assert_eq!(fks.len(), 2);
        assert_eq!(fks[0].ref_schema, None);
        assert_eq!(fks[1].name, None);
        assert_eq!(fks[1].ref_schema.as_deref(), Some("CAT"));
        assert_eq!(fks[1].columns, ["SKU", "VARIANT"]);
        let parents = foreign_key_parents(&fks, &obj(kinds::TABLE, "LINES"));
        assert_eq!(
            parents[0].detail.as_deref(),
            Some("FK_LINES_ORDER (ORDER_ID) → ORDERS (ID)")
        );
        assert_eq!(parents[0].schema, None);
        assert_eq!(
            parents[1].detail.as_deref(),
            Some("(SKU, VARIANT) → CAT.PRODUCTS (SKU, VARIANT)")
        );
        assert!(parents
            .iter()
            .all(|d| d.relation == Relation::ForeignKey && d.confidence == Confidence::Confirmed));
    }

    #[test]
    fn a_virtual_tables_remote_side_and_its_adapter() {
        let rows = [row(&[
            Some("CRM_SRC"),
            Some("<NULL>"),
            Some("dbo"),
            Some("Clients"),
        ])];
        let mut external: Vec<ExternalRef> =
            rows.iter().filter_map(|r| remote(r, 0, None)).collect();
        // No remote source: not a virtual table.
        assert!(remote(&row(&[None, None, None, None]), 0, None).is_none());
        let adapters = HashMap::from([("CRM_SRC".to_string(), "MSSQL".to_string())]);
        set_systems(&mut external, &adapters);
        assert_eq!(external[0].system.as_deref(), Some("MSSQL"));
        assert_eq!(external[0].server.as_deref(), Some("CRM_SRC"));
        assert_eq!(external[0].database, None);
        assert_eq!(external[0].schema.as_deref(), Some("dbo"));
        assert_eq!(external[0].name.as_deref(), Some("Clients"));
        assert_eq!(external[0].detail, None);
    }
}
