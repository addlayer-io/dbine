//! What an object uses (`Session::references`), from Firebird's dependency
//! catalog:
//!
//! - a view, procedure, function, trigger or package: its rows in
//!   `RDB$DEPENDENCIES` (`RDB$DEPENDENT_NAME`), each depended-on object
//!   once (the per-column rows collapse into their table or view);
//! - a table: the tables its foreign keys point at
//!   (`RDB$RELATION_CONSTRAINTS` + `RDB$REF_CONSTRAINTS`, the columns from
//!   the indexes' segments).
//!
//! Those rows are confirmed. `RDB$DEPENDED_ON_TYPE`: 0 a table or view
//! (told apart by `RDB$RELATIONS.RDB$VIEW_BLR`), 1 a view, 2 a trigger, 5
//! a procedure, 9 a domain, 14 a generator, 15 a function, 18 and 19 a
//! package; a routine with `RDB$PACKAGE_NAME` is its package's. The rest
//! (exceptions, collations, character sets, indexes, the columns' own
//! domains `RDB$…`) and the system objects (`RDB$`, `MON$`, `SEC$`) are
//! left out.
//!
//! The target's own source is read too ([`with_source`]): the lines that
//! name each object, and what it names that the catalog didn't record,
//! resolved as the default does (probable). Generators and domains, which
//! don't use anything the catalog records, get the default.

use crate::{text, FirebirdSession, PACKAGE};
use dbine_driver::dependencies::{
    find_mentions, foreign_key_parents, references_scan, used_names, used_objects, ExternalRef,
    UsedName, LOOKUP_CAP,
};
use dbine_driver::{
    kinds, Confidence, DbObject, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    Error, ForeignKeyDef, ObjectQuery, ObjectRef, Relation, Result, Session,
};

type Rows = Vec<Vec<Option<String>>>;

/// The depended-on objects of the target, once each: type, name, package
/// (of a packaged routine) and, for a relation, 0 table / 1 view (-1: not
/// one). `$types` are the target's `RDB$DEPENDENT_TYPE`s.
macro_rules! uses_sql {
    ($types:literal) => {
        concat!(
            "SELECT DISTINCT d.RDB$DEPENDED_ON_TYPE, TRIM(d.RDB$DEPENDED_ON_NAME), TRIM(d.RDB$PACKAGE_NAME),
       CASE WHEN r.RDB$RELATION_NAME IS NULL THEN -1 WHEN r.RDB$VIEW_BLR IS NULL THEN 0 ELSE 1 END
  FROM RDB$DEPENDENCIES d
  LEFT JOIN RDB$RELATIONS r
    ON d.RDB$DEPENDED_ON_TYPE IN (0, 1) AND r.RDB$RELATION_NAME = d.RDB$DEPENDED_ON_NAME
 WHERE d.RDB$DEPENDENT_NAME = ? AND d.RDB$DEPENDENT_TYPE IN (",
            $types,
            ")
 ORDER BY 2"
        )
    };
}

/// The [`uses_sql!`] query for a target of `kind`; `None` for kinds whose
/// uses the catalog doesn't record (tables go through their foreign keys).
pub(crate) fn uses_sql(kind: &str) -> Option<&'static str> {
    Some(match kind {
        kinds::VIEW => uses_sql!("0, 1"),
        kinds::PROCEDURE => uses_sql!("5"),
        kinds::FUNCTION => uses_sql!("15"),
        kinds::TRIGGER => uses_sql!("2"),
        PACKAGE => uses_sql!("18, 19"),
        _ => return None,
    })
}

/// A table's foreign keys: name, column, referenced table and column, in
/// order.
pub(crate) const FOREIGN_KEYS: &str = "
SELECT TRIM(rc.RDB$CONSTRAINT_NAME), TRIM(s.RDB$FIELD_NAME), TRIM(uq.RDB$RELATION_NAME), TRIM(us.RDB$FIELD_NAME)
  FROM RDB$RELATION_CONSTRAINTS rc
  JOIN RDB$REF_CONSTRAINTS ref ON ref.RDB$CONSTRAINT_NAME = rc.RDB$CONSTRAINT_NAME
  JOIN RDB$RELATION_CONSTRAINTS uq ON uq.RDB$CONSTRAINT_NAME = ref.RDB$CONST_NAME_UQ
  JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = rc.RDB$INDEX_NAME
  JOIN RDB$INDEX_SEGMENTS us
    ON us.RDB$INDEX_NAME = uq.RDB$INDEX_NAME AND us.RDB$FIELD_POSITION = s.RDB$FIELD_POSITION
 WHERE rc.RDB$CONSTRAINT_TYPE = 'FOREIGN KEY' AND rc.RDB$RELATION_NAME = ?
 ORDER BY rc.RDB$CONSTRAINT_NAME, s.RDB$FIELD_POSITION";

/// The kind of an `RDB$DEPENDED_ON_TYPE`: `relation` tells a table (0)
/// from a view (1) for type 0; a packaged routine (`packaged`) is its
/// package. `None` for what the explorer doesn't show.
pub(crate) fn kind_of(dep_type: i64, packaged: bool, relation: i64) -> Option<&'static str> {
    if packaged && matches!(dep_type, 5 | 15 | 18 | 19) {
        return Some(PACKAGE);
    }
    Some(match (dep_type, relation) {
        (0 | 1, 1) | (1, _) => kinds::VIEW,
        (0, _) => kinds::TABLE,
        (2, _) => kinds::TRIGGER,
        (5, _) => kinds::PROCEDURE,
        (9, _) => kinds::TYPE,
        (14, _) => kinds::SEQUENCE,
        (15, _) => kinds::FUNCTION,
        (18 | 19, _) => PACKAGE,
        _ => return None,
    })
}

/// Firebird's own objects, and the domains it makes up for columns.
fn system(name: &str) -> bool {
    ["RDB$", "MON$", "SEC$"].iter().any(|p| name.starts_with(p))
}

fn cell(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i)
        .cloned()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn number(r: &[Option<String>], i: usize) -> Option<i64> {
    cell(r, i)?.parse().ok()
}

/// The [`uses_sql`] rows as the objects the target uses (confirmed). A
/// package shows the routines of it used. The target itself and system
/// objects are left out.
pub(crate) fn parse_uses(rows: &[Vec<Option<String>>], target: &ObjectRef) -> Vec<Dependent> {
    let mut items: Vec<Dependent> = Vec::new();
    for r in rows {
        let (Some(dep_type), Some(name)) = (number(r, 0), cell(r, 1)) else {
            continue;
        };
        let package = cell(r, 2);
        let Some(kind) = kind_of(dep_type, package.is_some(), number(r, 3).unwrap_or(-1)) else {
            continue;
        };
        let (name, routine) = match package {
            Some(p) if kind == PACKAGE && p != name => (p, Some(name)),
            _ => (name, None),
        };
        if system(&name) {
            continue;
        }
        let d = confirmed(kind, None, name);
        if is_target(&d, target) {
            continue;
        }
        match items.iter_mut().find(|x| same(x, &d)) {
            Some(x) => {
                if let Some(r) = routine {
                    let detail = x.detail.get_or_insert_with(String::new);
                    if !detail.split(", ").any(|n| n == r) {
                        if !detail.is_empty() {
                            detail.push_str(", ");
                        }
                        detail.push_str(&r);
                    }
                }
            }
            None => items.push(Dependent {
                detail: routine,
                ..d
            }),
        }
    }
    items
}

/// [`FOREIGN_KEYS`]' rows as the table's foreign keys; names Firebird
/// made up (`INTEG_…`) are left out.
pub(crate) fn parse_foreign_keys(rows: &[Vec<Option<String>>]) -> Vec<ForeignKeyDef> {
    let mut out: Vec<(String, ForeignKeyDef)> = Vec::new();
    for r in rows {
        let (Some(name), Some(col), Some(table), Some(ref_col)) =
            (cell(r, 0), cell(r, 1), cell(r, 2), cell(r, 3))
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
                    name: (!name.starts_with("INTEG_")).then_some(name),
                    columns: vec![col],
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
async fn text_rows(s: &FirebirdSession, sql: &'static str, name: &str) -> Result<Rows> {
    Ok(s.rows(sql, vec![name.to_string()])
        .await?
        .iter()
        .map(|r| r.iter().map(text).collect())
        .collect())
}

/// `Session::references` (see the module's docs).
pub(crate) async fn references(
    s: &mut FirebirdSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let t = &target.object;
    if t.kind == kinds::TABLE {
        return match text_rows(s, FOREIGN_KEYS, &t.name).await {
            Ok(rows) => {
                let mut report = DependencyReport {
                    items: foreign_key_parents(&parse_foreign_keys(&rows), t),
                    ..Default::default()
                };
                sort(&mut report.items);
                Ok(report)
            }
            Err(e) => fallback(s, target, scan, "RDB$REF_CONSTRAINTS", e).await,
        };
    }
    let Some(sql) = uses_sql(&t.kind) else {
        return references_scan(s, target, scan).await;
    };
    match text_rows(s, sql, &t.name).await {
        Ok(rows) => {
            let items = parse_uses(&rows, t);
            with_source(s, target, scan, items, Vec::new()).await
        }
        Err(e) => fallback(s, target, scan, "RDB$DEPENDENCIES", e).await,
    }
}

/// The default, saying which catalog table couldn't be read.
async fn fallback<S: Session + ?Sized>(
    s: &mut S,
    target: &DependencyTarget,
    scan: &DependencyScan,
    table: &str,
    e: Error,
) -> Result<DependencyReport> {
    tracing::debug!("firebird: {table} not read: {e}");
    let mut report = references_scan(s, target, scan).await?;
    report.note = Some(format!("No se pudo leer {table}: {e}"));
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

fn is_target(d: &Dependent, t: &ObjectRef) -> bool {
    d.kind == t.kind && eq(&d.name, &t.name) && same_schema(d.schema.as_deref(), t.schema())
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
    fn each_kind_reads_its_dependent_types() {
        let view = uses_sql(kinds::VIEW).unwrap();
        assert!(view
            .starts_with("SELECT DISTINCT d.RDB$DEPENDED_ON_TYPE, TRIM(d.RDB$DEPENDED_ON_NAME)"));
        assert!(view.contains("FROM RDB$DEPENDENCIES d"));
        assert!(view.contains("WHERE d.RDB$DEPENDENT_NAME = ? AND d.RDB$DEPENDENT_TYPE IN (0, 1)"));
        assert!(uses_sql(kinds::PROCEDURE)
            .unwrap()
            .contains("RDB$DEPENDENT_TYPE IN (5)"));
        assert!(uses_sql(kinds::FUNCTION)
            .unwrap()
            .contains("RDB$DEPENDENT_TYPE IN (15)"));
        assert!(uses_sql(kinds::TRIGGER)
            .unwrap()
            .contains("RDB$DEPENDENT_TYPE IN (2)"));
        assert!(uses_sql(PACKAGE)
            .unwrap()
            .contains("RDB$DEPENDENT_TYPE IN (18, 19)"));
        for k in [kinds::TABLE, kinds::SEQUENCE, kinds::TYPE] {
            assert!(uses_sql(k).is_none(), "{k}");
        }
        for sql in [view, FOREIGN_KEYS] {
            assert_eq!(sql.matches('?').count(), 1);
        }
        assert!(FOREIGN_KEYS.contains("JOIN RDB$REF_CONSTRAINTS ref"));
    }

    #[test]
    fn dependency_types_map_to_kinds() {
        assert_eq!(kind_of(0, false, 0), Some(kinds::TABLE));
        assert_eq!(kind_of(0, false, 1), Some(kinds::VIEW));
        assert_eq!(kind_of(1, false, -1), Some(kinds::VIEW));
        assert_eq!(kind_of(2, false, -1), Some(kinds::TRIGGER));
        assert_eq!(kind_of(5, false, -1), Some(kinds::PROCEDURE));
        assert_eq!(kind_of(9, false, -1), Some(kinds::TYPE));
        assert_eq!(kind_of(14, false, -1), Some(kinds::SEQUENCE));
        assert_eq!(kind_of(15, false, -1), Some(kinds::FUNCTION));
        assert_eq!(kind_of(18, false, -1), Some(PACKAGE));
        assert_eq!(kind_of(15, true, -1), Some(PACKAGE));
        assert_eq!(kind_of(5, true, -1), Some(PACKAGE));
        // Exceptions, indexes, collations, character sets, computed fields.
        for t in [3, 6, 7, 10, 11, 17] {
            assert_eq!(kind_of(t, false, -1), None, "{t}");
        }
    }

    #[test]
    fn rows_collapse_into_confirmed_objects() {
        let target = obj(kinds::PROCEDURE, "P_LOAD");
        let rows = vec![
            row(&[Some("0"), Some("ORDERS"), None, Some("0")]),
            row(&[Some("0"), Some("V_TOTALS"), None, Some("1")]),
            row(&[Some("1"), Some("V_LINES"), None, Some("1")]),
            row(&[Some("14"), Some("SEQ_ORDERS"), None, Some("-1")]),
            row(&[Some("15"), Some("F_TAX"), None, Some("-1")]),
            row(&[Some("5"), Some("P_LOG"), None, Some("-1")]),
            row(&[Some("9"), Some("D_AMOUNT"), None, Some("-1")]),
            // Two routines of one package: the package once.
            row(&[Some("15"), Some("G"), Some("PK_UTIL"), Some("-1")]),
            row(&[Some("5"), Some("H"), Some("PK_UTIL"), Some("-1")]),
            // Left out: the target, its columns' domains, exceptions, system tables.
            row(&[Some("5"), Some("P_LOAD"), None, Some("-1")]),
            row(&[Some("9"), Some("RDB$12"), None, Some("-1")]),
            row(&[Some("7"), Some("E_BOOM"), None, Some("-1")]),
            row(&[Some("0"), Some("RDB$DATABASE"), None, Some("0")]),
            row(&[
                Some("15"),
                Some("DATABASE_VERSION"),
                Some("RDB$PROFILER"),
                Some("-1"),
            ]),
        ];
        let items = parse_uses(&rows, &target);
        let got: Vec<(&str, &str, Option<&str>)> = items
            .iter()
            .map(|d| (d.kind.as_str(), d.name.as_str(), d.detail.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                (kinds::TABLE, "ORDERS", None),
                (kinds::VIEW, "V_TOTALS", None),
                (kinds::VIEW, "V_LINES", None),
                (kinds::SEQUENCE, "SEQ_ORDERS", None),
                (kinds::FUNCTION, "F_TAX", None),
                (kinds::PROCEDURE, "P_LOG", None),
                (kinds::TYPE, "D_AMOUNT", None),
                (PACKAGE, "PK_UTIL", Some("G, H")),
            ]
        );
        assert!(items.iter().all(|d| d.confidence == Confidence::Confirmed
            && d.relation == Relation::Code
            && d.schema.is_none()));

        // A package body's rows on its own header are the package itself.
        let pkg = obj(PACKAGE, "PK_UTIL");
        let own = [row(&[Some("18"), Some("PK_UTIL"), None, Some("-1")])];
        assert!(parse_uses(&own, &pkg).is_empty());
    }

    #[test]
    fn foreign_key_rows_group_by_constraint() {
        let rows = vec![
            row(&[
                Some("FK_LINES_ORDER"),
                Some("ORDER_ID"),
                Some("ORDERS"),
                Some("ID"),
            ]),
            row(&[Some("INTEG_31"), Some("SKU"), Some("PRODUCTS"), Some("SKU")]),
            row(&[
                Some("INTEG_31"),
                Some("VARIANT"),
                Some("PRODUCTS"),
                Some("VARIANT"),
            ]),
        ];
        let fks = parse_foreign_keys(&rows);
        assert_eq!(fks.len(), 2);
        assert_eq!(fks[1].name, None);
        assert_eq!(fks[1].columns, ["SKU", "VARIANT"]);
        let parents = foreign_key_parents(&fks, &obj(kinds::TABLE, "LINES"));
        assert_eq!(
            parents[0].detail.as_deref(),
            Some("FK_LINES_ORDER (ORDER_ID) → ORDERS (ID)")
        );
        assert_eq!(
            parents[1].detail.as_deref(),
            Some("(SKU, VARIANT) → PRODUCTS (SKU, VARIANT)")
        );
        assert!(parents.iter().all(|d| d.relation == Relation::ForeignKey
            && d.confidence == Confidence::Confirmed
            && d.schema.is_none()));
    }
}
