//! What an object uses (`Session::references`) on Db2 LUW, from its
//! dependency catalog. The other presets keep the contract's default.
//!
//! - A view: `SYSCAT.VIEWDEP`.
//! - A procedure or function: `SYSCAT.ROUTINEDEP` (every overload of the
//!   name) and, for SQL routines, what their package uses
//!   (`SYSCAT.PACKAGEDEP`, through the routine's `K` row).
//! - A trigger: `SYSCAT.TRIGDEP` (and its package's), plus its table
//!   (`SYSCAT.TRIGGERS`).
//! - An alias: its base table, view or alias (`SYSCAT.TABLES`).
//! - A table: the tables its foreign keys point at (`SYSCAT.REFERENCES`,
//!   the columns from `SYSCAT.KEYCOLUSE`).
//!
//! Those rows are confirmed. A routine's `BNAME` is its specific name: its
//! name and kind come from `SYSCAT.ROUTINES`. A nickname (`N`, federation)
//! goes in `external` with its server, remote schema and table
//! (`SYSCAT.NICKNAMES`, `SYSCAT.SERVERS` and the server's `DBNAME` option).
//! Indexes, columns, packages, privileges and system schemas' objects are
//! left out.
//!
//! The target's own source is read too ([`with_source`]): the lines that
//! name each object, and what it names that the catalog didn't record,
//! resolved as the default does (probable). Sequences and types, which the
//! catalog doesn't track, and a catalog the login can't read (a database
//! created `RESTRICTIVE`) get the default, the latter with a note.

use crate::{OdbcSession, Rows};
use dbine_driver::dependencies::{
    find_mentions, foreign_key_parents, references_scan, used_names, used_objects, ExternalRef,
    UsedName, LOOKUP_CAP,
};
use dbine_driver::{
    kinds, Confidence, DbObject, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    Error, ForeignKeyDef, ObjectQuery, ObjectRef, Relation, Result, Session,
};

/// The objects the target's catalog rows name (`BTYPE`, `BSCHEMA`,
/// `BNAME`), with the routine behind a specific name and the remote side
/// of a nickname. `{deps}` is one of [`deps_sql`]'s.
const USES: &str = "WITH dep (BTYPE, BSCHEMA, BNAME) AS ({deps})
SELECT DISTINCT dep.BTYPE, TRIM(dep.BSCHEMA), dep.BNAME, r.ROUTINETYPE, r.ROUTINENAME,
       n.SERVERNAME, TRIM(n.REMOTE_SCHEMA), n.REMOTE_TABLE, s.SERVERTYPE, o.SETTING
  FROM dep
  LEFT JOIN SYSCAT.ROUTINES r
    ON dep.BTYPE = 'F' AND r.ROUTINESCHEMA = dep.BSCHEMA AND r.SPECIFICNAME = dep.BNAME
  LEFT JOIN SYSCAT.NICKNAMES n
    ON dep.BTYPE = 'N' AND n.TABSCHEMA = dep.BSCHEMA AND n.TABNAME = dep.BNAME
  LEFT JOIN SYSCAT.SERVERS s ON s.SERVERNAME = n.SERVERNAME
  LEFT JOIN SYSCAT.SERVEROPTIONS o ON o.SERVERNAME = n.SERVERNAME AND o.OPTION = 'DBNAME'
 ORDER BY 2, 3";

const VIEW_DEPS: &str =
    "SELECT BTYPE, BSCHEMA, BNAME FROM SYSCAT.VIEWDEP WHERE VIEWSCHEMA = ? AND VIEWNAME = ?";

/// `{type}` is `P` or `F`.
const ROUTINE_DEPS: &str = "SELECT d.BTYPE, d.BSCHEMA, d.BNAME
  FROM SYSCAT.ROUTINEDEP d
  JOIN SYSCAT.ROUTINES x ON x.ROUTINESCHEMA = d.ROUTINESCHEMA AND x.SPECIFICNAME = d.SPECIFICNAME
 WHERE x.ROUTINESCHEMA = ? AND x.ROUTINENAME = ? AND x.ROUTINETYPE = '{type}'
UNION ALL
SELECT p.BTYPE, p.BSCHEMA, p.BNAME
  FROM SYSCAT.ROUTINEDEP d
  JOIN SYSCAT.ROUTINES x ON x.ROUTINESCHEMA = d.ROUTINESCHEMA AND x.SPECIFICNAME = d.SPECIFICNAME
  JOIN SYSCAT.PACKAGEDEP p ON p.PKGSCHEMA = d.BSCHEMA AND p.PKGNAME = d.BNAME
 WHERE d.BTYPE = 'K' AND x.ROUTINESCHEMA = ? AND x.ROUTINENAME = ? AND x.ROUTINETYPE = '{type}'";

const TRIGGER_DEPS: &str = "SELECT BTYPE, BSCHEMA, BNAME
  FROM SYSCAT.TRIGDEP WHERE TRIGSCHEMA = ? AND TRIGNAME = ?
UNION ALL
SELECT p.BTYPE, p.BSCHEMA, p.BNAME
  FROM SYSCAT.TRIGDEP d
  JOIN SYSCAT.PACKAGEDEP p ON p.PKGSCHEMA = d.BSCHEMA AND p.PKGNAME = d.BNAME
 WHERE d.BTYPE = 'K' AND d.TRIGSCHEMA = ? AND d.TRIGNAME = ?
UNION ALL
SELECT t.TYPE, t.TABSCHEMA, t.TABNAME
  FROM SYSCAT.TRIGGERS g
  JOIN SYSCAT.TABLES t ON t.TABSCHEMA = g.TABSCHEMA AND t.TABNAME = g.TABNAME
 WHERE g.TRIGSCHEMA = ? AND g.TRIGNAME = ?";

const ALIAS_DEPS: &str = "SELECT b.TYPE, a.BASE_TABSCHEMA, a.BASE_TABNAME
  FROM SYSCAT.TABLES a
  JOIN SYSCAT.TABLES b ON b.TABSCHEMA = a.BASE_TABSCHEMA AND b.TABNAME = a.BASE_TABNAME
 WHERE a.TABSCHEMA = ? AND a.TABNAME = ? AND a.TYPE = 'A'";

/// A table's foreign keys: name, referenced schema and table, then each
/// column with the one it references, in order.
pub(crate) const FOREIGN_KEYS: &str =
    "SELECT TRIM(r.CONSTNAME), TRIM(r.REFTABSCHEMA), r.REFTABNAME, k.COLNAME, p.COLNAME
  FROM SYSCAT.REFERENCES r
  JOIN SYSCAT.KEYCOLUSE k
    ON k.CONSTNAME = r.CONSTNAME AND k.TABSCHEMA = r.TABSCHEMA AND k.TABNAME = r.TABNAME
  JOIN SYSCAT.KEYCOLUSE p
    ON p.CONSTNAME = r.REFKEYNAME AND p.TABSCHEMA = r.REFTABSCHEMA AND p.TABNAME = r.REFTABNAME
   AND p.COLSEQ = k.COLSEQ
 WHERE r.TABSCHEMA = ? AND r.TABNAME = ?
 ORDER BY 1, k.COLSEQ";

/// The catalog query for a target of `kind`, the catalog view it reads
/// (for the note when it can't), and how many times it takes the target's
/// schema and name, in that order.
pub(crate) fn uses_sql(kind: &str) -> Option<(String, &'static str, usize)> {
    let (deps, view, pairs) = match kind {
        kinds::VIEW => (VIEW_DEPS.to_string(), "SYSCAT.VIEWDEP", 1),
        kinds::PROCEDURE => (ROUTINE_DEPS.replace("{type}", "P"), "SYSCAT.ROUTINEDEP", 2),
        kinds::FUNCTION => (ROUTINE_DEPS.replace("{type}", "F"), "SYSCAT.ROUTINEDEP", 2),
        kinds::TRIGGER => (TRIGGER_DEPS.to_string(), "SYSCAT.TRIGDEP", 3),
        kinds::SYNONYM => (ALIAS_DEPS.to_string(), "SYSCAT.TABLES", 1),
        _ => return None,
    };
    Some((USES.replace("{deps}", &deps), view, pairs))
}

/// The kind of a catalog `BTYPE` (`TYPE` for `SYSCAT.TABLES`); a routine's
/// (`F`) by its `ROUTINETYPE`. `None` for what isn't an object the
/// explorer shows (indexes, columns, packages, privileges, modules,
/// variables…) and for nicknames, which are external.
pub(crate) fn kind_of(btype: &str, routine: Option<&str>) -> Option<&'static str> {
    Some(match btype {
        // Table, typed table, created temporary, hierarchy, detached, MQT.
        "T" | "U" | "G" | "H" | "L" | "S" => kinds::TABLE,
        "V" | "W" => kinds::VIEW,
        "F" => match routine {
            Some("P") => kinds::PROCEDURE,
            _ => kinds::FUNCTION,
        },
        "Q" => kinds::SEQUENCE,
        "A" => kinds::SYNONYM,
        "R" => kinds::TYPE,
        "B" => kinds::TRIGGER,
        _ => return None,
    })
}

fn cell(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i)
        .cloned()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The [`uses_sql`] rows: the objects (confirmed) and the nicknames'
/// remote sources, each with the nickname that stands for it in the
/// source. The target itself and `system` schemas' objects are left out.
pub(crate) fn parse_uses(
    rows: &Rows,
    target: &ObjectRef,
    system: impl Fn(&str) -> bool,
) -> (Vec<Dependent>, Vec<(ExternalRef, Option<ObjectRef>)>) {
    let mut items: Vec<Dependent> = Vec::new();
    let mut external: Vec<(ExternalRef, Option<ObjectRef>)> = Vec::new();
    for r in rows {
        let (Some(btype), Some(schema), Some(bname)) = (cell(r, 0), cell(r, 1), cell(r, 2)) else {
            continue;
        };
        if btype == "N" {
            let source = ExternalRef {
                system: cell(r, 8),
                server: cell(r, 5),
                database: cell(r, 9),
                schema: cell(r, 6),
                name: cell(r, 7),
                detail: Some(format!("{schema}.{bname}")),
                mentions: Vec::new(),
            };
            if !external.iter().any(|(x, _)| *x == source) {
                let local = ObjectRef {
                    kind: kinds::TABLE.into(),
                    schema: Some(schema),
                    name: bname,
                };
                external.push((source, Some(local)));
            }
            continue;
        }
        let Some(kind) = kind_of(&btype, cell(r, 3).as_deref()) else {
            continue;
        };
        if system(&schema) {
            continue;
        }
        let name = match kind {
            kinds::PROCEDURE | kinds::FUNCTION => cell(r, 4).unwrap_or(bname),
            _ => bname,
        };
        let d = confirmed(kind, Some(schema), name);
        if !is_target(&d, target) && !items.iter().any(|x| same(x, &d)) {
            items.push(d);
        }
    }
    (items, external)
}

/// [`FOREIGN_KEYS`]' rows as the table's foreign keys.
pub(crate) fn parse_foreign_keys(rows: &Rows) -> Vec<ForeignKeyDef> {
    let mut out: Vec<ForeignKeyDef> = Vec::new();
    for r in rows {
        let (Some(name), Some(table), Some(col), Some(ref_col)) =
            (cell(r, 0), cell(r, 2), cell(r, 3), cell(r, 4))
        else {
            continue;
        };
        match out.last_mut().filter(|f| f.name.as_deref() == Some(&name)) {
            Some(fk) => {
                fk.columns.push(col);
                fk.ref_columns.push(ref_col);
            }
            None => out.push(ForeignKeyDef {
                name: Some(name),
                columns: vec![col],
                ref_schema: cell(r, 1),
                ref_table: table,
                ref_columns: vec![ref_col],
                ..Default::default()
            }),
        }
    }
    out
}

/// `Session::references` on Db2 LUW (see the module's docs).
pub(crate) async fn db2(
    s: &mut OdbcSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let t = &target.object;
    let params = |pairs: usize| -> Vec<String> {
        let schema = t.schema().unwrap_or_default().to_string();
        (0..pairs)
            .flat_map(|_| [schema.clone(), t.name.clone()])
            .collect()
    };
    if t.kind == kinds::TABLE {
        return match s.query(FOREIGN_KEYS.to_string(), params(1)).await {
            Ok(rows) => {
                let mut report = DependencyReport {
                    items: foreign_key_parents(&parse_foreign_keys(&rows), t),
                    ..Default::default()
                };
                sort(&mut report.items);
                Ok(report)
            }
            Err(e) => fallback(s, target, scan, "SYSCAT.REFERENCES", e).await,
        };
    }
    let Some((sql, view, pairs)) = uses_sql(&t.kind) else {
        return references_scan(s, target, scan).await;
    };
    match s.query(sql, params(pairs)).await {
        Ok(rows) => {
            let preset = s.preset;
            let (items, external) = parse_uses(&rows, t, |sc| preset.is_system_schema(sc));
            with_source(s, target, scan, items, external).await
        }
        Err(e) => fallback(s, target, scan, view, e).await,
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
    tracing::debug!("odbc: {view} not read: {e}");
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

    fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    #[test]
    fn each_kind_reads_its_catalog_view() {
        let (sql, view, pairs) = uses_sql(kinds::VIEW).unwrap();
        assert_eq!((view, pairs), ("SYSCAT.VIEWDEP", 1));
        assert!(sql.starts_with("WITH dep (BTYPE, BSCHEMA, BNAME) AS (SELECT BTYPE, BSCHEMA, BNAME FROM SYSCAT.VIEWDEP WHERE VIEWSCHEMA = ? AND VIEWNAME = ?)"));
        assert!(sql.contains("r.SPECIFICNAME = dep.BNAME"));
        assert!(sql.contains("o.OPTION = 'DBNAME'"));
        assert_eq!(sql.matches('?').count(), 2);

        let (sql, view, pairs) = uses_sql(kinds::PROCEDURE).unwrap();
        assert_eq!((view, pairs), ("SYSCAT.ROUTINEDEP", 2));
        assert_eq!(sql.matches("x.ROUTINETYPE = 'P'").count(), 2);
        assert!(sql.contains(
            "JOIN SYSCAT.PACKAGEDEP p ON p.PKGSCHEMA = d.BSCHEMA AND p.PKGNAME = d.BNAME"
        ));
        assert_eq!(sql.matches('?').count(), 4);
        let (sql, _, _) = uses_sql(kinds::FUNCTION).unwrap();
        assert_eq!(sql.matches("x.ROUTINETYPE = 'F'").count(), 2);

        let (sql, view, pairs) = uses_sql(kinds::TRIGGER).unwrap();
        assert_eq!((view, pairs), ("SYSCAT.TRIGDEP", 3));
        assert!(sql.contains("FROM SYSCAT.TRIGGERS g"));
        assert_eq!(sql.matches('?').count(), 6);

        let (sql, view, pairs) = uses_sql(kinds::SYNONYM).unwrap();
        assert_eq!((view, pairs), ("SYSCAT.TABLES", 1));
        assert!(sql.contains("a.TYPE = 'A'"));

        // Tables go through their foreign keys; the rest, the default.
        for k in [kinds::TABLE, kinds::SEQUENCE, kinds::TYPE] {
            assert!(uses_sql(k).is_none(), "{k}");
        }
        assert_eq!(FOREIGN_KEYS.matches('?').count(), 2);
    }

    #[test]
    fn btype_codes_map_to_kinds() {
        for (b, kind) in [
            ("T", Some(kinds::TABLE)),
            ("S", Some(kinds::TABLE)),
            ("U", Some(kinds::TABLE)),
            ("V", Some(kinds::VIEW)),
            ("W", Some(kinds::VIEW)),
            ("Q", Some(kinds::SEQUENCE)),
            ("A", Some(kinds::SYNONYM)),
            ("R", Some(kinds::TYPE)),
            ("B", Some(kinds::TRIGGER)),
            ("I", None),
            ("K", None),
            ("C", None),
            ("O", None),
            ("N", None),
            ("v", None),
            ("m", None),
        ] {
            assert_eq!(kind_of(b, None), kind, "{b}");
        }
        assert_eq!(kind_of("F", Some("P")), Some(kinds::PROCEDURE));
        assert_eq!(kind_of("F", Some("F")), Some(kinds::FUNCTION));
        assert_eq!(kind_of("F", None), Some(kinds::FUNCTION));
    }

    #[test]
    fn rows_become_confirmed_objects_and_nicknames_external() {
        let target = obj(kinds::PROCEDURE, "APP", "P1");
        let rows = vec![
            row(&[
                Some("T"),
                Some("APP"),
                Some("ORDERS"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ]),
            // The same table through the routine's package.
            row(&[
                Some("T"),
                Some("APP"),
                Some("ORDERS"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ]),
            row(&[
                Some("V"),
                Some("APP"),
                Some("V_TOTALS"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ]),
            // A routine's BNAME is its specific name.
            row(&[
                Some("F"),
                Some("APP"),
                Some("SQL261120203627648"),
                Some("F"),
                Some("F_TAX"),
                None,
                None,
                None,
                None,
                None,
            ]),
            row(&[
                Some("F"),
                Some("APP"),
                Some("SQL261120203627750"),
                Some("P"),
                Some("P_LOG"),
                None,
                None,
                None,
                None,
                None,
            ]),
            row(&[
                Some("Q"),
                Some("APP"),
                Some("SEQ_ORDERS"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ]),
            // Left out: the target itself, indexes, packages, system schemas.
            row(&[
                Some("F"),
                Some("APP"),
                Some("SQL1"),
                Some("P"),
                Some("P1"),
                None,
                None,
                None,
                None,
                None,
            ]),
            row(&[
                Some("I"),
                Some("SYSIBM"),
                Some("SQL261120203626780"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ]),
            row(&[
                Some("K"),
                Some("APP"),
                Some("P1059022783"),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ]),
            row(&[
                Some("F"),
                Some("SYSPROC"),
                Some("ADMIN_CMD"),
                Some("P"),
                Some("ADMIN_CMD"),
                None,
                None,
                None,
                None,
                None,
            ]),
            // A nickname: its server's remote table.
            row(&[
                Some("N"),
                Some("APP"),
                Some("NK_CLIENTS"),
                None,
                None,
                Some("CRM_SRV"),
                Some("CRM"),
                Some("CLIENTS"),
                Some("DB2/UDB"),
                Some("CRMDB"),
            ]),
        ];
        let (items, external) = parse_uses(&rows, &target, |s| s.starts_with("SYS"));
        let names: Vec<(&str, &str)> = items
            .iter()
            .map(|d| (d.kind.as_str(), d.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                (kinds::TABLE, "ORDERS"),
                (kinds::VIEW, "V_TOTALS"),
                (kinds::FUNCTION, "F_TAX"),
                (kinds::PROCEDURE, "P_LOG"),
                (kinds::SEQUENCE, "SEQ_ORDERS"),
            ]
        );
        assert!(items.iter().all(|d| d.confidence == Confidence::Confirmed
            && d.relation == Relation::Code
            && d.schema.as_deref() == Some("APP")));
        assert_eq!(external.len(), 1);
        let (x, local) = &external[0];
        assert_eq!(
            *x,
            ExternalRef {
                system: Some("DB2/UDB".into()),
                server: Some("CRM_SRV".into()),
                database: Some("CRMDB".into()),
                schema: Some("CRM".into()),
                name: Some("CLIENTS".into()),
                detail: Some("APP.NK_CLIENTS".into()),
                mentions: Vec::new(),
            }
        );
        assert_eq!(local.as_ref().unwrap().name, "NK_CLIENTS");
    }

    #[test]
    fn foreign_key_rows_group_by_constraint() {
        let rows = vec![
            row(&[
                Some("FK_LINES_ORDER"),
                Some("APP"),
                Some("ORDERS"),
                Some("ORDER_ID"),
                Some("ID"),
            ]),
            row(&[
                Some("FK_LINES_PROD"),
                Some("CAT"),
                Some("PRODUCTS"),
                Some("SKU"),
                Some("SKU"),
            ]),
            row(&[
                Some("FK_LINES_PROD"),
                Some("CAT"),
                Some("PRODUCTS"),
                Some("VARIANT"),
                Some("VARIANT"),
            ]),
        ];
        let fks = parse_foreign_keys(&rows);
        assert_eq!(fks.len(), 2);
        assert_eq!(fks[1].columns, ["SKU", "VARIANT"]);
        assert_eq!(fks[1].ref_columns, ["SKU", "VARIANT"]);
        let parents = foreign_key_parents(&fks, &obj(kinds::TABLE, "APP", "LINES"));
        assert_eq!(parents[0].relation, Relation::ForeignKey);
        assert_eq!(parents[0].confidence, Confidence::Confirmed);
        assert_eq!(
            parents[1].detail.as_deref(),
            Some("FK_LINES_PROD (SKU, VARIANT) → CAT.PRODUCTS (SKU, VARIANT)")
        );
        assert_eq!(parents[1].schema.as_deref(), Some("CAT"));
    }

    #[test]
    fn sort_puts_confirmed_first() {
        let mut items = vec![
            Dependent {
                confidence: Confidence::Probable,
                ..confirmed(kinds::TABLE, Some("APP".into()), "A".into())
            },
            confirmed(kinds::VIEW, Some("APP".into()), "Z".into()),
        ];
        sort(&mut items);
        assert_eq!(items[0].name, "Z");
    }
}
