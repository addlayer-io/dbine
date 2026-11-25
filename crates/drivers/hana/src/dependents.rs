//! What depends on an object (`Session::dependents`), from HANA's
//! dependency catalog: the views [`crate::references`] reads, read the other
//! way, filtered by the object they point at instead of reading every
//! object's source.
//!
//! - `SYS.OBJECT_DEPENDENCIES WHERE BASE_SCHEMA_NAME / BASE_OBJECT_NAME /
//!   BASE_OBJECT_TYPE = target` (`DEPENDENCY_TYPE` 1): the views,
//!   procedures, functions, triggers, synonyms (public ones too) and
//!   sequences HANA recorded as using it. Confirmed. It records no columns,
//!   so for a column the objects of its table are kept when their source
//!   names the column (Probable), and only those are read.
//! - Foreign keys: `SYS.REFERENTIAL_CONSTRAINTS` rows that point at the
//!   table (or that it holds). For a column, also its own indexes and
//!   primary key (`SYS.INDEXES`, `SYS.INDEX_COLUMNS`, `SYS.CONSTRAINTS`).
//! - What the catalog can't resolve, SQLScript that names the target inside
//!   a string (`EXEC`): one query over the procedures', functions' and
//!   triggers' definitions of the target's schema and the session's keeps,
//!   on the server, those whose text holds the name; only those the
//!   dependency catalog didn't return are read and parsed (Probable or
//!   Review, with the lines).
//!
//! `SYS` and `_SYS_` objects are left out. At most [`READ_CAP`] definitions
//! are read; the rest are listed as unreadable. If the dependency catalog
//! can't be read, the generic scan answers, with a note.

use crate::references::{cell, eq, kind_of, local_schema, sort, text_rows, Rows};
use crate::HanaSession;
use dbine_driver::dependencies::{
    find_mentions, scan, Confidence, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    Relation,
};
use dbine_driver::{kinds, ObjectRef, Result, Session};

/// Definitions read for a column or from the source search; the rest go to
/// `unreadable`.
pub(crate) const READ_CAP: usize = 300;

/// `BASE_OBJECT_TYPE` of a target of `kind`; `None`: the generic scan
/// answers. A table type is a table in the catalog.
fn base_type(kind: &str) -> Option<&'static str> {
    Some(match kind {
        kinds::TABLE | kinds::TYPE => "TABLE",
        kinds::VIEW => "VIEW",
        kinds::PROCEDURE => "PROCEDURE",
        kinds::FUNCTION => "FUNCTION",
        kinds::SYNONYM => "SYNONYM",
        kinds::SEQUENCE => "SEQUENCE",
        _ => return None,
    })
}

/// What is recorded as using `?`.`?` (of type `?`): the dependent, its type
/// and a trigger's table.
const DEPENDENTS: &str = "SELECT d.DEPENDENT_SCHEMA_NAME, d.DEPENDENT_OBJECT_NAME, d.DEPENDENT_OBJECT_TYPE,
       t.SUBJECT_TABLE_NAME
  FROM SYS.OBJECT_DEPENDENCIES d
  LEFT JOIN SYS.TRIGGERS t
    ON d.DEPENDENT_OBJECT_TYPE = 'TRIGGER' AND t.SCHEMA_NAME = d.DEPENDENT_SCHEMA_NAME
   AND t.TRIGGER_NAME = d.DEPENDENT_OBJECT_NAME
 WHERE d.BASE_SCHEMA_NAME = ? AND d.BASE_OBJECT_NAME = ? AND d.BASE_OBJECT_TYPE = ?
   AND d.DEPENDENCY_TYPE = 1
   AND d.DEPENDENT_OBJECT_TYPE IN ('VIEW', 'PROCEDURE', 'FUNCTION', 'TRIGGER', 'SYNONYM', 'SEQUENCE')
 ORDER BY d.DEPENDENT_SCHEMA_NAME, d.DEPENDENT_OBJECT_NAME";

/// The foreign keys that point at the table `?`.`?` or that it holds (`?`.`?`
/// again), a row per column pair: holder schema, table, constraint, column,
/// referenced schema, table and column.
const FOREIGN_KEYS: &str = "SELECT SCHEMA_NAME, TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME,
       REFERENCED_SCHEMA_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME
  FROM SYS.REFERENTIAL_CONSTRAINTS
 WHERE (REFERENCED_SCHEMA_NAME = ? AND REFERENCED_TABLE_NAME = ?) OR (SCHEMA_NAME = ? AND TABLE_NAME = ?)
 ORDER BY SCHEMA_NAME, TABLE_NAME, CONSTRAINT_NAME, POSITION";

/// The indexes (and unique constraints, which HANA backs with one) of the
/// table `?`.`?` on the column `?`, a row per key column.
const INDEXES: &str = "SELECT i.INDEX_NAME, k.COLUMN_NAME
  FROM SYS.INDEXES i
  JOIN SYS.INDEX_COLUMNS c
    ON c.SCHEMA_NAME = i.SCHEMA_NAME AND c.TABLE_NAME = i.TABLE_NAME AND c.INDEX_NAME = i.INDEX_NAME
  JOIN SYS.INDEX_COLUMNS k
    ON k.SCHEMA_NAME = i.SCHEMA_NAME AND k.TABLE_NAME = i.TABLE_NAME AND k.INDEX_NAME = i.INDEX_NAME
 WHERE i.SCHEMA_NAME = ? AND i.TABLE_NAME = ? AND c.COLUMN_NAME = ?
   AND (i.CONSTRAINT IS NULL OR i.CONSTRAINT <> 'PRIMARY KEY')
 ORDER BY i.INDEX_NAME, k.POSITION";

/// The primary key of the table `?`.`?` when it holds the column `?`.
const PRIMARY_KEY: &str = "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME FROM SYS.CONSTRAINTS k
 WHERE k.SCHEMA_NAME = ? AND k.TABLE_NAME = ? AND k.IS_PRIMARY_KEY = 'TRUE'
   AND k.CONSTRAINT_NAME IN (SELECT CONSTRAINT_NAME FROM SYS.CONSTRAINTS
                              WHERE SCHEMA_NAME = ? AND TABLE_NAME = ? AND IS_PRIMARY_KEY = 'TRUE' AND COLUMN_NAME = ?)
 ORDER BY k.POSITION";

/// The code objects of the schemas `?` and `?` whose text holds the pattern
/// `?` (a `LIKE` pattern, upper case): schema, name, type.
const SOURCE: &str = "SELECT SCHEMA_NAME, PROCEDURE_NAME, 'PROCEDURE' FROM SYS.PROCEDURES
 WHERE SCHEMA_NAME IN (?, ?) AND UPPER(DEFINITION) LIKE ? ESCAPE '\\'
UNION ALL
SELECT SCHEMA_NAME, FUNCTION_NAME, 'FUNCTION' FROM SYS.FUNCTIONS
 WHERE SCHEMA_NAME IN (?, ?) AND UPPER(DEFINITION) LIKE ? ESCAPE '\\'
UNION ALL
SELECT SCHEMA_NAME, TRIGGER_NAME, 'TRIGGER' FROM SYS.TRIGGERS
 WHERE SCHEMA_NAME IN (?, ?) AND UPPER(DEFINITION) LIKE ? ESCAPE '\\'";

/// `%`, `_` and the escape taken literally, upper case.
fn like_pattern(name: &str) -> String {
    let mut out = String::from("%");
    for c in name.to_uppercase().chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// What HANA's own schemas hold (monitoring views, generated objects).
fn maintained(schema: &str) -> bool {
    schema == "SYS" || schema.starts_with("_SYS")
}

/// What the catalog says about the target.
#[derive(Debug, Default)]
pub(crate) struct Read {
    pub deps: Rows,
    pub fks: Rows,
    pub indexes: Rows,
    pub primary_key: Rows,
    pub hits: Rows,
    /// Why the source search failed (the catalog's answer stands).
    pub source_error: Option<String>,
}

/// The objects of `read` as the explorer lists them, once each: `recorded`
/// is whether the dependency catalog says the target is used there.
pub(crate) fn candidates(read: &Read, session: &str, target: &ObjectRef) -> Vec<(Dependent, bool)> {
    let mut out: Vec<(Dependent, bool)> = Vec::new();
    let mut add = |schema: String, name: String, ty: &str, table: Option<String>, recorded| {
        if maintained(&schema) {
            return;
        }
        let Some(kind) = kind_of(ty, false) else {
            return;
        };
        let schema = local_schema(schema, session);
        let own = target.kind == kind
            && eq(&target.name, &name)
            && target.schema().filter(|s| *s != session) == schema.as_deref();
        if own {
            return;
        }
        match out.iter_mut().find(|(d, _)| {
            d.kind == kind && eq(&d.name, &name) && d.schema.as_deref() == schema.as_deref()
        }) {
            Some((_, was)) => *was |= recorded,
            None => out.push((
                Dependent {
                    kind: kind.into(),
                    schema,
                    name,
                    parent: table.filter(|_| kind == kinds::TRIGGER),
                    relation: Relation::Code,
                    confidence: if recorded {
                        Confidence::Confirmed
                    } else {
                        Confidence::Probable
                    },
                    detail: None,
                    mentions: Vec::new(),
                },
                recorded,
            )),
        }
    };
    for r in &read.deps {
        if let (Some(schema), Some(name), Some(ty)) = (cell(r, 0), cell(r, 1), cell(r, 2)) {
            add(schema, name, &ty, cell(r, 3), true);
        }
    }
    for r in &read.hits {
        if let (Some(schema), Some(name), Some(ty)) = (cell(r, 0), cell(r, 1), cell(r, 2)) {
            add(schema, name, &ty, None, false);
        }
    }
    out
}

/// The foreign keys that hold or point at the target (a column: the ones
/// that name it), as [`dbine_driver::dependencies::schema_dependents`] shows
/// them.
pub(crate) fn foreign_keys(
    rows: &Rows,
    session: &str,
    owner: &str,
    target: &ObjectRef,
    column: Option<&str>,
) -> Vec<Dependent> {
    let mut groups: Vec<Vec<[String; 7]>> = Vec::new();
    for r in rows {
        let v: Vec<String> = (0..7).map(|i| cell(r, i).unwrap_or_default()).collect();
        let row: [String; 7] = match v.try_into() {
            Ok(row) => row,
            Err(_) => continue,
        };
        match groups.last_mut() {
            Some(g) if g[0][..3] == row[..3] => g.push(row),
            _ => groups.push(vec![row]),
        }
    }
    let mut out = Vec::new();
    for g in groups {
        let f = &g[0];
        // _SYS_ constraints are HANA's own.
        if f[2].starts_with("_SYS_") {
            continue;
        }
        let points_here = eq(&f[4], owner) && eq(&f[5], &target.name);
        let held_here = eq(&f[0], owner) && eq(&f[1], &target.name);
        let hit = match column {
            None => points_here && !held_here,
            Some(c) => {
                (points_here && g.iter().any(|p| eq(&p[6], c)))
                    || (held_here && g.iter().any(|p| eq(&p[3], c)))
            }
        };
        if !hit {
            continue;
        }
        let cols: Vec<&str> = g.iter().map(|p| p[3].as_str()).collect();
        let refs: Vec<&str> = g.iter().map(|p| p[6].as_str()).collect();
        let to = if eq(&f[4], &f[0]) {
            f[5].clone()
        } else {
            format!("{}.{}", f[4], f[5])
        };
        out.push(Dependent {
            kind: kinds::TABLE.into(),
            schema: local_schema(f[0].clone(), session),
            name: f[1].clone(),
            parent: None,
            relation: Relation::ForeignKey,
            confidence: Confidence::Confirmed,
            detail: Some(format!(
                "{} ({}) → {to} ({})",
                f[2],
                cols.join(", "),
                refs.join(", ")
            )),
            mentions: Vec::new(),
        });
    }
    out
}

/// A name and its key columns from rows of (name, column) in key order.
fn keys(rows: &Rows) -> Vec<(String, String)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for r in rows {
        let (Some(name), Some(col)) = (cell(r, 0), cell(r, 1)) else {
            continue;
        };
        match out.last_mut() {
            Some((n, cols)) if *n == name => cols.push(col),
            _ => out.push((name, vec![col])),
        }
    }
    out.into_iter()
        .filter(|(n, _)| !n.starts_with("_SYS_"))
        .map(|(n, cols)| (n, cols.join(", ")))
        .collect()
}

/// The target table's own indexes and primary key on the column.
pub(crate) fn own_items(
    read: &Read,
    session: &str,
    owner: &str,
    target: &ObjectRef,
) -> Vec<Dependent> {
    let holder = |(name, cols): (String, String)| Dependent {
        kind: kinds::TABLE.into(),
        schema: local_schema(owner.to_string(), session),
        name: target.name.clone(),
        parent: None,
        relation: Relation::Index,
        confidence: Confidence::Confirmed,
        detail: Some(format!("{name} ({cols})")),
        mentions: Vec::new(),
    };
    keys(&read.indexes)
        .into_iter()
        .chain(keys(&read.primary_key))
        .map(holder)
        .collect()
}

async fn read_catalog(
    s: &HanaSession,
    base: &str,
    owner: &str,
    target: &ObjectRef,
    column: Option<&str>,
    session: &str,
) -> Result<Read> {
    let mut read = Read {
        deps: text_rows(s, DEPENDENTS, &[owner, &target.name, base]).await?,
        ..Default::default()
    };
    if target.kind == kinds::TABLE {
        read.fks = text_rows(s, FOREIGN_KEYS, &[owner, &target.name, owner, &target.name]).await?;
        if let Some(c) = column {
            read.indexes = text_rows(s, INDEXES, &[owner, &target.name, c]).await?;
            read.primary_key = text_rows(
                s,
                PRIMARY_KEY,
                &[owner, &target.name, owner, &target.name, c],
            )
            .await?;
        }
    }
    // What the catalog can't see: a string with the name. A failure is told,
    // not hidden, and the catalog's answer stands.
    let pattern = like_pattern(&target.name);
    let p = pattern.as_str();
    match text_rows(
        s,
        SOURCE,
        &[owner, session, p, owner, session, p, owner, session, p],
    )
    .await
    {
        Ok(rows) => read.hits = rows,
        Err(e) => {
            tracing::debug!("hana: source search not run: {e}");
            read.source_error = Some(e.to_string());
        }
    }
    Ok(read)
}

impl HanaSession {
    pub(crate) async fn dependents_impl(
        &mut self,
        target: &DependencyTarget,
        ctx: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = &target.object;
        let Some(base) = base_type(&t.kind) else {
            return scan(self, target, ctx).await;
        };
        let owner = self.owner(t);
        let session = self.schema.clone();
        let column = target.column.clone();
        let read = match read_catalog(self, base, &owner, t, column.as_deref(), &session).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("hana: dependents not read from the catalog: {e}");
                let mut report = scan(self, target, ctx).await?;
                let view = "SYS.OBJECT_DEPENDENCIES";
                report.note = Some(format!("No se pudo leer {view}: {e}"));
                return Ok(report);
            }
        };
        let mut report = DependencyReport::default();
        report.items.extend(foreign_keys(
            &read.fks,
            &session,
            &owner,
            t,
            column.as_deref(),
        ));
        report.items.extend(own_items(&read, &session, &owner, t));
        let mut reads = 0;
        for (mut d, recorded) in candidates(&read, &session, t) {
            if recorded && column.is_none() {
                report.items.push(d);
                continue;
            }
            let name = display(&d);
            if reads >= READ_CAP {
                report.unreadable.push(name);
                continue;
            }
            reads += 1;
            let o = ObjectRef {
                kind: d.kind.clone(),
                schema: d.schema.clone(),
                name: d.name.clone(),
            };
            let body = match self.definition(&o).await {
                Ok(Some(b)) => b,
                Ok(None) => continue,
                Err(_) => {
                    report.unreadable.push(name);
                    continue;
                }
            };
            report.scanned += 1;
            let Some((confidence, mentions)) = find_mentions(&body, &ctx.dialect, target) else {
                continue;
            };
            d.confidence = confidence;
            d.mentions = mentions;
            report.items.push(d);
        }
        sort(&mut report.items);
        Ok(report)
    }
}

fn display(d: &Dependent) -> String {
    match &d.schema {
        Some(s) => format!("{s}.{}", d.name),
        None => d.name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: &[&str]) -> Vec<Option<String>> {
        cells.iter().map(|c| Some(c.to_string())).collect()
    }

    fn target(kind: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        }
    }

    #[test]
    fn the_queries_take_the_target_and_nothing_else() {
        assert_eq!(DEPENDENTS.matches('?').count(), 3);
        assert!(DEPENDENTS.contains(
            "d.BASE_SCHEMA_NAME = ? AND d.BASE_OBJECT_NAME = ? AND d.BASE_OBJECT_TYPE = ?"
        ));
        assert!(DEPENDENTS.contains("DEPENDENCY_TYPE = 1"));
        assert_eq!(FOREIGN_KEYS.matches('?').count(), 4);
        assert_eq!(INDEXES.matches('?').count(), 3);
        assert_eq!(PRIMARY_KEY.matches('?').count(), 5);
        assert_eq!(SOURCE.matches('?').count(), 9);
        assert!(SOURCE.contains("UPPER(DEFINITION) LIKE ?"));
        assert_eq!(like_pattern("a_b"), "%A\\_B%");
    }

    #[test]
    fn only_the_kinds_the_catalog_records_are_read() {
        for k in [
            "table",
            "type",
            "view",
            "procedure",
            "function",
            "synonym",
            "sequence",
        ] {
            assert!(base_type(k).is_some(), "{k}");
        }
        for k in ["trigger", "schema", "index", "constraint"] {
            assert!(base_type(k).is_none(), "{k}");
        }
        assert_eq!(base_type("type"), Some("TABLE"));
    }

    #[test]
    fn rows_become_the_explorers_objects_once_each() {
        let read = Read {
            deps: vec![
                row(&["APP", "V_EMP", "VIEW"]),
                {
                    let mut r = row(&["APP", "TRG", "TRIGGER"]);
                    r.push(Some("EMP".into()));
                    r
                },
                row(&["OTRO", "S_EMP", "SYNONYM"]),
                row(&["PUBLIC", "EMP", "SYNONYM"]),
                row(&["SYS", "M_EMP", "VIEW"]),
                row(&["_SYS_BI", "X", "VIEW"]),
                row(&["APP", "IDX", "INDEX"]),
            ],
            hits: vec![
                row(&["APP", "V_EMP", "PROCEDURE"]),
                row(&["APP", "P_DIN", "PROCEDURE"]),
                row(&["APP", "EMP", "FUNCTION"]),
            ],
            ..Default::default()
        };
        let c = candidates(&read, "APP", &target("function", "EMP"));
        let by = |n: &str| c.iter().filter(|(d, _)| d.name == n).collect::<Vec<_>>();
        assert_eq!(by("V_EMP").len(), 2, "a view and a procedure of that name");
        let v = &by("V_EMP")[0];
        assert!(v.1 && v.0.kind == "view" && v.0.schema.is_none());
        assert_eq!(by("TRG")[0].0.parent.as_deref(), Some("EMP"));
        assert_eq!(by("S_EMP")[0].0.schema.as_deref(), Some("OTRO"));
        assert_eq!(by("EMP")[0].0.schema.as_deref(), Some("PUBLIC"));
        assert_eq!(by("EMP").len(), 1, "the target function itself is out");
        let p = &by("P_DIN")[0];
        assert!(!p.1);
        assert_eq!(p.0.confidence, Confidence::Probable);
        for n in ["M_EMP", "X", "IDX"] {
            assert!(by(n).is_empty(), "{n}");
        }
    }

    #[test]
    fn foreign_keys_point_at_the_table_or_name_the_column() {
        let rows = vec![
            row(&["APP", "EMP", "FK_DEPT", "DEPT_ID", "APP", "DEPT", "ID"]),
            row(&["APP", "HIST", "FK_X", "A", "OTRO", "DEPT", "ID"]),
            row(&["APP", "HIST", "FK_X", "B", "OTRO", "DEPT", "CODE"]),
            row(&["APP", "DEPT", "FK_SELF", "PARENT", "APP", "DEPT", "ID"]),
            row(&["APP", "EMP", "_SYS_FK", "DEPT_ID", "APP", "DEPT", "ID"]),
        ];
        let t = target("table", "DEPT");
        let all = foreign_keys(&rows, "APP", "APP", &t, None);
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].name, "EMP");
        assert_eq!(
            all[0].detail.as_deref(),
            Some("FK_DEPT (DEPT_ID) → DEPT (ID)")
        );
        assert_eq!(foreign_keys(&rows, "APP", "APP", &t, Some("ID")).len(), 2);
        let own = foreign_keys(&rows, "APP", "APP", &t, Some("PARENT"));
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].name, "DEPT");
        let other = foreign_keys(&rows, "APP", "OTRO", &t, None);
        assert_eq!(other.len(), 1);
        assert_eq!(
            other[0].detail.as_deref(),
            Some("FK_X (A, B) → OTRO.DEPT (ID, CODE)")
        );
    }

    #[test]
    fn the_columns_own_indexes_and_primary_key() {
        let read = Read {
            indexes: vec![
                row(&["IX_AB", "A"]),
                row(&["IX_AB", "B"]),
                row(&["_SYS_TREE_CS_1", "A"]),
            ],
            primary_key: vec![row(&["PK_T", "A"])],
            ..Default::default()
        };
        let items = own_items(&read, "APP", "APP", &target("table", "T"));
        let shown: Vec<_> = items.iter().map(|d| d.detail.clone().unwrap()).collect();
        assert_eq!(shown, ["IX_AB (A, B)", "PK_T (A)"]);
        assert!(items
            .iter()
            .all(|d| d.relation == Relation::Index && d.name == "T"));
    }
}
