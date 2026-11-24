//! What depends on an object (`Session::dependents`) on Firebird, from the
//! dependency catalog filtered by the target:
//!
//! - `RDB$DEPENDENCIES WHERE RDB$DEPENDED_ON_NAME = target` gives the views,
//!   procedures, functions, triggers and packages that use it (and, joined
//!   to `RDB$CHECK_CONSTRAINTS`, `RDB$INDICES` and `RDB$RELATION_FIELDS`,
//!   the table's CHECK constraints, expression indexes and computed
//!   columns that read it). Firebird records the *column* too
//!   (`RDB$FIELD_NAME`), so a column's dependents are exact: confirmed.
//! - A table's foreign keys that point at it (`RDB$REF_CONSTRAINTS`; for a
//!   column, also the ones it holds) and, for a column, the table's
//!   indexes (`RDB$INDEX_SEGMENTS`).
//! - One more query brings the source (`CONTAINING` the name, in the
//!   server) of the views, procedures, functions, triggers and package
//!   bodies: the lines for what the catalog recorded, and what the catalog
//!   can't know (`EXECUTE STATEMENT` text) for the rest, read by the code
//!   (probable, or review inside a string).
//!
//! The default's scan read every object's source one by one. A catalog the
//! login can't read gives it back, with a note.

use crate::references::{cell, number, sort, system, Rows};
use crate::{text, FirebirdSession, PACKAGE};
use dbine_driver::dependencies::{find_mentions, DependencyScan, DependencyTarget};
use dbine_driver::{kinds, Confidence, DependencyReport, Dependent, Error, Relation, Result};

/// The recorded dependents, once each: type, name, 0 table / 1 view / -1
/// (a relation), the table of a trigger, an index or a computed column,
/// the trigger's system flag, the CHECK constraint behind a system
/// trigger, and the computed column's name. `$field` narrows to a column.
macro_rules! dependents_sql {
    ($types:literal, $field:literal) => {
        concat!(
            "SELECT DISTINCT d.RDB$DEPENDENT_TYPE, TRIM(d.RDB$DEPENDENT_NAME),
       CASE WHEN r.RDB$RELATION_NAME IS NULL THEN -1 WHEN r.RDB$VIEW_BLR IS NULL THEN 0 ELSE 1 END,
       COALESCE(TRIM(tr.RDB$RELATION_NAME), TRIM(ix.RDB$RELATION_NAME), TRIM(rf.RDB$RELATION_NAME)),
       tr.RDB$SYSTEM_FLAG, TRIM(cc.RDB$CONSTRAINT_NAME), TRIM(rf.RDB$FIELD_NAME)
  FROM RDB$DEPENDENCIES d
  LEFT JOIN RDB$RELATIONS r
    ON d.RDB$DEPENDENT_TYPE IN (0, 1) AND r.RDB$RELATION_NAME = d.RDB$DEPENDENT_NAME
  LEFT JOIN RDB$TRIGGERS tr
    ON d.RDB$DEPENDENT_TYPE = 2 AND tr.RDB$TRIGGER_NAME = d.RDB$DEPENDENT_NAME
  LEFT JOIN RDB$CHECK_CONSTRAINTS cc
    ON d.RDB$DEPENDENT_TYPE = 2 AND cc.RDB$TRIGGER_NAME = d.RDB$DEPENDENT_NAME
  LEFT JOIN RDB$INDICES ix
    ON d.RDB$DEPENDENT_TYPE = 6 AND ix.RDB$INDEX_NAME = d.RDB$DEPENDENT_NAME
  LEFT JOIN RDB$RELATION_FIELDS rf
    ON d.RDB$DEPENDENT_TYPE = 3 AND rf.RDB$FIELD_SOURCE = d.RDB$DEPENDENT_NAME
 WHERE d.RDB$DEPENDED_ON_NAME = ? AND d.RDB$DEPENDED_ON_TYPE IN (",
            $types,
            ")",
            $field,
            "
 ORDER BY 2"
        )
    };
}

/// [`dependents_sql!`] for the target's kind, narrowed to a column or not
/// (that one takes the column as a second parameter).
fn dependents_sql(kind: &str, column: bool) -> Option<&'static str> {
    macro_rules! both {
        ($types:literal) => {
            if column {
                dependents_sql!($types, " AND d.RDB$FIELD_NAME = ?")
            } else {
                dependents_sql!($types, "")
            }
        };
    }
    Some(match kind {
        kinds::TABLE | kinds::VIEW => both!("0, 1"),
        kinds::PROCEDURE => both!("5"),
        kinds::FUNCTION => both!("15"),
        kinds::TRIGGER => both!("2"),
        kinds::SEQUENCE => both!("14"),
        kinds::TYPE => both!("9"),
        PACKAGE => both!("18, 19"),
        _ => return None,
    })
}

/// The foreign keys that point at the table (`?` twice for the column
/// variant's second): constraint, holder, its column, referenced table and
/// column, in order.
const FOREIGN_KEYS_TO: &str = "
SELECT TRIM(rc.RDB$CONSTRAINT_NAME), TRIM(rc.RDB$RELATION_NAME), TRIM(s.RDB$FIELD_NAME), TRIM(uq.RDB$RELATION_NAME), TRIM(us.RDB$FIELD_NAME)
  FROM RDB$RELATION_CONSTRAINTS rc
  JOIN RDB$REF_CONSTRAINTS ref ON ref.RDB$CONSTRAINT_NAME = rc.RDB$CONSTRAINT_NAME
  JOIN RDB$RELATION_CONSTRAINTS uq ON uq.RDB$CONSTRAINT_NAME = ref.RDB$CONST_NAME_UQ
  JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = rc.RDB$INDEX_NAME
  JOIN RDB$INDEX_SEGMENTS us
    ON us.RDB$INDEX_NAME = uq.RDB$INDEX_NAME AND us.RDB$FIELD_POSITION = s.RDB$FIELD_POSITION
 WHERE rc.RDB$CONSTRAINT_TYPE = 'FOREIGN KEY' AND (uq.RDB$RELATION_NAME = ?)
 ORDER BY rc.RDB$RELATION_NAME, rc.RDB$CONSTRAINT_NAME, s.RDB$FIELD_POSITION";
const FOREIGN_KEYS_TO_HELD: &str = "
SELECT TRIM(rc.RDB$CONSTRAINT_NAME), TRIM(rc.RDB$RELATION_NAME), TRIM(s.RDB$FIELD_NAME), TRIM(uq.RDB$RELATION_NAME), TRIM(us.RDB$FIELD_NAME)
  FROM RDB$RELATION_CONSTRAINTS rc
  JOIN RDB$REF_CONSTRAINTS ref ON ref.RDB$CONSTRAINT_NAME = rc.RDB$CONSTRAINT_NAME
  JOIN RDB$RELATION_CONSTRAINTS uq ON uq.RDB$CONSTRAINT_NAME = ref.RDB$CONST_NAME_UQ
  JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = rc.RDB$INDEX_NAME
  JOIN RDB$INDEX_SEGMENTS us
    ON us.RDB$INDEX_NAME = uq.RDB$INDEX_NAME AND us.RDB$FIELD_POSITION = s.RDB$FIELD_POSITION
 WHERE rc.RDB$CONSTRAINT_TYPE = 'FOREIGN KEY' AND (uq.RDB$RELATION_NAME = ? OR rc.RDB$RELATION_NAME = ?)
 ORDER BY rc.RDB$RELATION_NAME, rc.RDB$CONSTRAINT_NAME, s.RDB$FIELD_POSITION";

/// The table's index columns, in order: index, column.
const INDEXES: &str = "SELECT TRIM(i.RDB$INDEX_NAME), TRIM(s.RDB$FIELD_NAME)
  FROM RDB$INDICES i JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = i.RDB$INDEX_NAME
 WHERE i.RDB$RELATION_NAME = ?
 ORDER BY i.RDB$INDEX_NAME, s.RDB$FIELD_POSITION";

/// The source that contains the name (`?` five times): kind, name, source,
/// and a trigger's table. User objects only.
const SOURCES: &str = "SELECT 'view', TRIM(RDB$RELATION_NAME), RDB$VIEW_SOURCE, CAST(NULL AS VARCHAR(63))
  FROM RDB$RELATIONS
 WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$VIEW_BLR IS NOT NULL AND RDB$VIEW_SOURCE CONTAINING ?
UNION ALL
SELECT 'procedure', TRIM(RDB$PROCEDURE_NAME), RDB$PROCEDURE_SOURCE, NULL
  FROM RDB$PROCEDURES
 WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$PACKAGE_NAME IS NULL AND RDB$PROCEDURE_SOURCE CONTAINING ?
UNION ALL
SELECT 'function', TRIM(RDB$FUNCTION_NAME), RDB$FUNCTION_SOURCE, NULL
  FROM RDB$FUNCTIONS
 WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$PACKAGE_NAME IS NULL AND RDB$FUNCTION_SOURCE CONTAINING ?
UNION ALL
SELECT 'trigger', TRIM(RDB$TRIGGER_NAME), RDB$TRIGGER_SOURCE, TRIM(RDB$RELATION_NAME)
  FROM RDB$TRIGGERS
 WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$TRIGGER_SOURCE CONTAINING ?
UNION ALL
SELECT 'package', TRIM(RDB$PACKAGE_NAME), RDB$PACKAGE_BODY_SOURCE, NULL
  FROM RDB$PACKAGES
 WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$PACKAGE_BODY_SOURCE CONTAINING ?";

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

fn item(
    kind: &str,
    name: String,
    relation: Relation,
    confidence: Confidence,
    detail: Option<String>,
) -> Dependent {
    Dependent {
        kind: kind.into(),
        schema: None,
        name,
        parent: None,
        relation,
        confidence,
        detail,
        mentions: Vec::new(),
    }
}

/// The [`dependents_sql`] rows as dependents (confirmed): views, routines,
/// triggers and packages by name; a system trigger is its table's CHECK
/// constraint, an expression index and a computed column are their table's.
/// The target itself and system objects are left out.
pub(crate) fn parse_recorded(
    rows: &[Vec<Option<String>>],
    target: &DependencyTarget,
) -> Vec<Dependent> {
    let t = &target.object;
    let mut out: Vec<Dependent> = Vec::new();
    let mut put = |d: Dependent| {
        let same = |x: &Dependent| {
            x.kind == d.kind
                && x.relation == d.relation
                && eq(&x.name, &d.name)
                && x.detail == d.detail
        };
        if !out.iter().any(same) {
            out.push(d);
        }
    };
    for r in rows {
        let (Some(ty), Some(name)) = (number(r, 0), cell(r, 1)) else {
            continue;
        };
        if system(&name) && ty != 3 {
            continue;
        }
        let table = cell(r, 3);
        let d = match ty {
            0 | 1 => {
                let kind = match number(r, 2) {
                    Some(1) => kinds::VIEW,
                    _ if ty == 1 => kinds::VIEW,
                    _ => kinds::TABLE,
                };
                item(kind, name, Relation::Code, Confidence::Confirmed, None)
            }
            // A trigger of the system's (flag 3) is a CHECK constraint.
            2 if number(r, 4).is_some_and(|f| f != 0) => {
                let (Some(table), Some(constraint)) = (table, cell(r, 5)) else {
                    continue;
                };
                item(
                    kinds::TABLE,
                    table,
                    Relation::Check,
                    Confidence::Confirmed,
                    Some(constraint),
                )
            }
            2 => {
                let mut d = item(
                    kinds::TRIGGER,
                    name,
                    Relation::Code,
                    Confidence::Confirmed,
                    None,
                );
                d.parent = table;
                d
            }
            3 => {
                let Some(table) = table else { continue };
                item(
                    kinds::TABLE,
                    table,
                    Relation::Code,
                    Confidence::Confirmed,
                    cell(r, 6).map(|c| format!("COMPUTED BY {c}")),
                )
            }
            5 => item(
                kinds::PROCEDURE,
                name,
                Relation::Code,
                Confidence::Confirmed,
                None,
            ),
            6 => {
                let Some(table) = table else { continue };
                item(
                    kinds::TABLE,
                    table,
                    Relation::Index,
                    Confidence::Confirmed,
                    Some(name),
                )
            }
            15 => item(
                kinds::FUNCTION,
                name,
                Relation::Code,
                Confidence::Confirmed,
                None,
            ),
            18 | 19 => item(PACKAGE, name, Relation::Code, Confidence::Confirmed, None),
            _ => continue,
        };
        let is_target = d.relation == Relation::Code && d.kind == t.kind && eq(&d.name, &t.name);
        if !is_target {
            put(d);
        }
    }
    out
}

/// The [`SOURCES`] rows: the lines for what the catalog recorded, and as
/// probable (or review) what only the source names. A column needs the
/// source to name it as well as its table.
pub(crate) fn add_sources(
    items: &mut Vec<Dependent>,
    rows: &[Vec<Option<String>>],
    target: &DependencyTarget,
    scan: &DependencyScan,
    report: &mut DependencyReport,
) {
    let t = &target.object;
    for r in rows {
        let (Some(tag), Some(name)) = (cell(r, 0), cell(r, 1)) else {
            continue;
        };
        let kind = match tag.as_str() {
            "view" => kinds::VIEW,
            "procedure" => kinds::PROCEDURE,
            "function" => kinds::FUNCTION,
            "trigger" => kinds::TRIGGER,
            "package" => PACKAGE,
            _ => continue,
        };
        if kind == t.kind && eq(&name, &t.name) || system(&name) {
            continue;
        }
        let Some(body) = r.get(2).cloned().flatten().filter(|b| !b.trim().is_empty()) else {
            continue;
        };
        report.scanned += 1;
        let found = find_mentions(&body, &scan.dialect, target);
        let known = items
            .iter_mut()
            .find(|d| d.relation == Relation::Code && d.kind == kind && eq(&d.name, &name));
        match (known, found) {
            (Some(d), Some((_, m))) => d.mentions = m,
            (Some(_), None) => {}
            (None, Some((confidence, mentions))) => {
                let mut d = item(kind, name, Relation::Code, confidence, None);
                d.parent = if kind == kinds::TRIGGER {
                    cell(r, 3)
                } else {
                    None
                };
                d.mentions = mentions;
                items.push(d);
            }
            (None, None) => {}
        }
    }
}

/// [`FOREIGN_KEYS_TO`]' rows: the keys that point at the table (not its
/// own), or for a column the ones that point at it or that it holds.
pub(crate) fn parse_foreign_keys(
    rows: &[Vec<Option<String>>],
    target: &DependencyTarget,
) -> Vec<Dependent> {
    struct Fk {
        name: String,
        holder: String,
        to: String,
        columns: Vec<String>,
        ref_columns: Vec<String>,
    }
    let mut keys: Vec<Fk> = Vec::new();
    for r in rows {
        let (Some(name), Some(holder), Some(col), Some(to), Some(rc)) =
            (cell(r, 0), cell(r, 1), cell(r, 2), cell(r, 3), cell(r, 4))
        else {
            continue;
        };
        match keys.iter_mut().find(|k| k.name == name) {
            Some(k) => {
                k.columns.push(col);
                k.ref_columns.push(rc);
            }
            None => keys.push(Fk {
                name,
                holder,
                to,
                columns: vec![col],
                ref_columns: vec![rc],
            }),
        }
    }
    let t = &target.object;
    let mut out = Vec::new();
    for k in keys {
        let points_here = eq(&k.to, &t.name);
        let holds = eq(&k.holder, &t.name);
        let hit = match target.column.as_deref() {
            None => points_here && !holds,
            Some(c) => {
                (points_here && k.ref_columns.iter().any(|r| eq(r, c)))
                    || (holds && k.columns.iter().any(|r| eq(r, c)))
            }
        };
        if hit {
            // Firebird's own names (`INTEG_…`) tell nothing.
            let name = if k.name.starts_with("INTEG_") {
                String::new()
            } else {
                format!("{} ", k.name)
            };
            out.push(item(
                kinds::TABLE,
                k.holder,
                Relation::ForeignKey,
                Confidence::Confirmed,
                Some(format!(
                    "{name}({}) → {} ({})",
                    k.columns.join(", "),
                    k.to,
                    k.ref_columns.join(", ")
                )),
            ));
        }
    }
    out
}

/// [`INDEXES`]' rows: the indexes of the table that hold the column.
pub(crate) fn parse_indexes(
    rows: &[Vec<Option<String>>],
    target: &DependencyTarget,
) -> Vec<Dependent> {
    let Some(column) = target.column.as_deref() else {
        return Vec::new();
    };
    let mut ixs: Vec<(String, Vec<String>)> = Vec::new();
    for r in rows {
        let (Some(ix), Some(col)) = (cell(r, 0), cell(r, 1)) else {
            continue;
        };
        match ixs.iter_mut().find(|(n, _)| *n == ix) {
            Some((_, cols)) => cols.push(col),
            None => ixs.push((ix, vec![col])),
        }
    }
    ixs.into_iter()
        .filter(|(_, cols)| cols.iter().any(|c| eq(c, column)))
        .map(|(ix, cols)| {
            item(
                kinds::TABLE,
                target.object.name.clone(),
                Relation::Index,
                Confidence::Confirmed,
                Some(format!("{ix} ({})", cols.join(", "))),
            )
        })
        .collect()
}

/// `Rows` of a catalog query with these parameters.
async fn query(s: &FirebirdSession, sql: &'static str, params: Vec<String>) -> Result<Rows> {
    Ok(s.rows(sql, params)
        .await?
        .iter()
        .map(|r| r.iter().map(text).collect())
        .collect())
}

/// `Session::dependents` (see the module's docs).
pub(crate) async fn dependents(
    s: &mut FirebirdSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let t = &target.object;
    let column = target.column.clone();
    let Some(sql) = dependents_sql(&t.kind, column.is_some()) else {
        return dbine_driver::dependencies::scan(s, target, scan).await;
    };
    let name = t.name.clone();
    let read = async {
        let mut params = vec![name.clone()];
        params.extend(column.clone());
        let recorded = query(s, sql, params).await?;
        let keys = if t.kind == kinds::TABLE {
            if column.is_some() {
                query(s, FOREIGN_KEYS_TO_HELD, vec![name.clone(), name.clone()]).await?
            } else {
                query(s, FOREIGN_KEYS_TO, vec![name.clone()]).await?
            }
        } else {
            Rows::new()
        };
        let indexes = if column.is_some() && t.kind == kinds::TABLE {
            query(s, INDEXES, vec![name.clone()]).await?
        } else {
            Rows::new()
        };
        let sources = query(s, SOURCES, vec![name.clone(); 5]).await?;
        Ok::<_, Error>((recorded, keys, indexes, sources))
    };
    let (recorded, keys, indexes, sources) = match read.await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("firebird: the dependency catalog not read: {e}");
            let mut report = dbine_driver::dependencies::scan(s, target, scan).await?;
            report.note = Some(format!(
                "No se pudo leer el catálogo de dependencias (RDB$DEPENDENCIES): {e}"
            ));
            return Ok(report);
        }
    };
    let mut report = DependencyReport::default();
    let mut items = parse_recorded(&recorded, target);
    add_sources(&mut items, &sources, target, scan, &mut report);
    items.extend(parse_foreign_keys(&keys, target));
    items.extend(parse_indexes(&indexes, target));
    sort(&mut items);
    report.items = items;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::sql::ScriptDialect;
    use dbine_driver::ObjectRef;

    fn row(cells: &[Option<&str>]) -> Vec<Option<String>> {
        cells.iter().map(|c| c.map(str::to_string)).collect()
    }

    fn target(kind: &str, name: &str, column: Option<&str>) -> DependencyTarget {
        DependencyTarget {
            object: ObjectRef {
                kind: kind.into(),
                schema: None,
                name: name.into(),
            },
            column: column.map(Into::into),
        }
    }

    fn scan() -> DependencyScan {
        DependencyScan {
            source_kinds: Vec::new(),
            dialect: ScriptDialect::generic(),
            foreign_keys: true,
            written_kinds: Vec::new(),
            has_schemas: false,
        }
    }

    #[test]
    fn the_query_is_filtered_by_the_target_and_its_column() {
        let t = dependents_sql(kinds::TABLE, false).unwrap();
        assert!(t.contains(
            "WHERE d.RDB$DEPENDED_ON_NAME = ? AND d.RDB$DEPENDED_ON_TYPE IN (0, 1)\n ORDER BY 2"
        ));
        assert_eq!(t.matches('?').count(), 1);
        let c = dependents_sql(kinds::TABLE, true).unwrap();
        assert!(c.contains("AND d.RDB$FIELD_NAME = ?"));
        assert_eq!(c.matches('?').count(), 2);
        assert!(dependents_sql(kinds::PROCEDURE, false)
            .unwrap()
            .contains("RDB$DEPENDED_ON_TYPE IN (5)"));
        assert!(dependents_sql(kinds::FUNCTION, false)
            .unwrap()
            .contains("IN (15)"));
        assert!(dependents_sql(PACKAGE, false)
            .unwrap()
            .contains("IN (18, 19)"));
        assert!(dependents_sql(kinds::INDEX, false).is_none());
        assert_eq!(SOURCES.matches('?').count(), 5);
        assert_eq!(SOURCES.matches("CONTAINING ?").count(), 5);
        assert_eq!(FOREIGN_KEYS_TO.matches('?').count(), 1);
        assert_eq!(FOREIGN_KEYS_TO_HELD.matches('?').count(), 2);
    }

    #[test]
    fn rows_are_views_routines_triggers_and_the_tables_own_things() {
        // type, name, relation, table, system flag, constraint, column
        let rows = vec![
            row(&[Some("1"), Some("V_T"), Some("1"), None, None, None, None]),
            row(&[Some("0"), Some("T_GEN"), Some("0"), None, None, None, None]),
            row(&[
                Some("2"),
                Some("TR"),
                None,
                Some("T"),
                Some("0"),
                None,
                None,
            ]),
            row(&[
                Some("2"),
                Some("CHECK_1"),
                None,
                Some("T"),
                Some("3"),
                Some("CK_T"),
                None,
            ]),
            row(&[
                Some("2"),
                Some("CHECK_2"),
                None,
                Some("T"),
                Some("3"),
                Some("CK_T"),
                None,
            ]),
            row(&[
                Some("3"),
                Some("RDB$77"),
                None,
                Some("T"),
                None,
                None,
                Some("CALC"),
            ]),
            row(&[
                Some("6"),
                Some("IX_EXPR"),
                None,
                Some("T"),
                None,
                None,
                None,
            ]),
            row(&[Some("5"), Some("P"), None, None, None, None, None]),
            row(&[Some("15"), Some("F"), None, None, None, None, None]),
            row(&[Some("19"), Some("PK"), None, None, None, None, None]),
            // The system's, the target itself and what isn't an object.
            row(&[Some("5"), Some("RDB$SYS"), None, None, None, None, None]),
            row(&[Some("5"), Some("SELF"), None, None, None, None, None]),
            row(&[Some("7"), Some("EXC"), None, None, None, None, None]),
        ];
        let out = parse_recorded(&rows, &target(kinds::PROCEDURE, "SELF", None));
        let by: Vec<_> = out
            .iter()
            .map(|d| {
                (
                    d.kind.as_str(),
                    d.name.as_str(),
                    d.relation,
                    d.detail.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            by,
            [
                ("view", "V_T", Relation::Code, None),
                ("table", "T_GEN", Relation::Code, None),
                ("trigger", "TR", Relation::Code, None),
                ("table", "T", Relation::Check, Some("CK_T")),
                ("table", "T", Relation::Code, Some("COMPUTED BY CALC")),
                ("table", "T", Relation::Index, Some("IX_EXPR")),
                ("procedure", "P", Relation::Code, None),
                ("function", "F", Relation::Code, None),
                ("package", "PK", Relation::Code, None),
            ]
        );
        assert_eq!(out[2].parent.as_deref(), Some("T"));
        assert!(out.iter().all(|d| d.confidence == Confidence::Confirmed));
    }

    #[test]
    fn the_source_gives_lines_to_the_recorded_and_finds_the_rest() {
        let t = target(kinds::TABLE, "ORDERS", None);
        let mut items = parse_recorded(
            &[row(&[Some("5"), Some("P1"), None, None, None, None, None])],
            &t,
        );
        let src = vec![
            row(&[
                Some("procedure"),
                Some("P1"),
                Some("BEGIN\n DELETE FROM ORDERS;\nEND"),
                None,
            ]),
            // Dynamic SQL: only a string names it.
            row(&[
                Some("procedure"),
                Some("P2"),
                Some("BEGIN EXECUTE STATEMENT 'DELETE FROM ORDERS'; END"),
                None,
            ]),
            // A longer name.
            row(&[
                Some("procedure"),
                Some("P3"),
                Some("SELECT 1 FROM ORDERS_OLD"),
                None,
            ]),
            row(&[
                Some("trigger"),
                Some("TG"),
                Some("BEGIN INSERT INTO ORDERS VALUES (1); END"),
                Some("LINES"),
            ]),
            row(&[Some("view"), Some("V"), Some("SELECT * FROM ORDERS"), None]),
        ];
        let mut report = DependencyReport::default();
        add_sources(&mut items, &src, &t, &scan(), &mut report);
        sort(&mut items);
        let by: Vec<_> = items
            .iter()
            .map(|d| (d.name.as_str(), d.confidence, d.mentions.len()))
            .collect();
        assert_eq!(
            by,
            [
                ("P1", Confidence::Confirmed, 1),
                ("TG", Confidence::Probable, 1),
                ("V", Confidence::Probable, 1),
                ("P2", Confidence::Review, 1),
            ]
        );
        let tg = items.iter().find(|d| d.name == "TG").unwrap();
        assert_eq!(tg.parent.as_deref(), Some("LINES"));
        assert_eq!(report.scanned, 5);
    }

    #[test]
    fn a_column_needs_the_source_to_name_it() {
        let t = target(kinds::TABLE, "ORDERS", Some("TOTAL"));
        let mut items = Vec::new();
        let src = vec![
            row(&[
                Some("procedure"),
                Some("PA"),
                Some("SELECT TOTAL FROM ORDERS"),
                None,
            ]),
            row(&[
                Some("procedure"),
                Some("PB"),
                Some("SELECT ID FROM ORDERS"),
                None,
            ]),
        ];
        add_sources(
            &mut items,
            &src,
            &t,
            &scan(),
            &mut DependencyReport::default(),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "PA");
    }

    #[test]
    fn foreign_keys_and_indexes() {
        let rows = vec![
            row(&[
                Some("FK_L"),
                Some("LINES"),
                Some("ORDER_ID"),
                Some("ORDERS"),
                Some("ID"),
            ]),
            row(&[
                Some("INTEG_9"),
                Some("X"),
                Some("A"),
                Some("ORDERS"),
                Some("ID"),
            ]),
            row(&[
                Some("FK_SELF"),
                Some("ORDERS"),
                Some("PARENT"),
                Some("ORDERS"),
                Some("ID"),
            ]),
            row(&[
                Some("FK_UP"),
                Some("ORDERS"),
                Some("GRP"),
                Some("GROUPS"),
                Some("ID"),
            ]),
        ];
        let out = parse_foreign_keys(&rows, &target(kinds::TABLE, "ORDERS", None));
        let d: Vec<_> = out
            .iter()
            .map(|d| (d.name.as_str(), d.detail.clone().unwrap()))
            .collect();
        assert_eq!(
            d,
            [
                ("LINES", "FK_L (ORDER_ID) → ORDERS (ID)".to_string()),
                ("X", "(A) → ORDERS (ID)".to_string()),
            ]
        );
        let col = parse_foreign_keys(&rows, &target(kinds::TABLE, "ORDERS", Some("GRP")));
        assert_eq!(col.len(), 1);
        assert_eq!(col[0].name, "ORDERS");

        let ix = vec![
            row(&[Some("PK_O"), Some("ID")]),
            row(&[Some("IX_T"), Some("TOTAL")]),
            row(&[Some("IX_T"), Some("ID")]),
        ];
        let out = parse_indexes(&ix, &target(kinds::TABLE, "ORDERS", Some("TOTAL")));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].detail.as_deref(), Some("IX_T (TOTAL, ID)"));
        assert!(parse_indexes(&ix, &target(kinds::TABLE, "ORDERS", None)).is_empty());
    }
}
