//! What depends on an object (`Session::dependents`) on Db2 LUW, from its
//! dependency catalog filtered by the target: `SYSCAT.VIEWDEP`,
//! `ROUTINEDEP` (and, for SQL routines, the package's `PACKAGEDEP`),
//! `TRIGDEP`, the triggers on a table (`SYSCAT.TRIGGERS`), an alias's base
//! (`SYSCAT.TABLES`) and, for a table, the foreign keys that point at it
//! (`SYSCAT.REFERENCES`). The other presets keep the contract's scan.
//!
//! - One query returns the views, routines, triggers and aliases the
//!   catalog records as using the target, each with its source (`TEXT`),
//!   so the lines that name it come with the row: confirmed. Nothing is
//!   read object by object.
//! - Dynamic SQL isn't recorded: a second query brings the routines and
//!   triggers whose `TEXT` contains the name, and the ones the catalog
//!   didn't list are read by the code (probable, or review when the name
//!   only shows in a string).
//! - A column: the objects the catalog records for its table, kept when
//!   their source names the column (probable); the table's indexes
//!   (`SYSCAT.INDEXCOLUSE`) and checks (`SYSCAT.COLCHECKS`) on it, and the
//!   keys that point at it or that it holds.
//!
//! A catalog the login can't read (a database created `RESTRICTIVE`) gives
//! the contract's scan, with a note. Triggers, indexes, schemas and
//! constraints as targets use the scan as well: the catalog records nothing
//! that depends on them.

use crate::references::{cell, kind_of, sort};
use crate::{OdbcSession, Rows};
use dbine_driver::dependencies::{find_mentions, DependencyScan, DependencyTarget};
use dbine_driver::{kinds, Confidence, DependencyReport, Dependent, Error, Relation, Result};

/// What the catalog records as using the target. `{b}` is the match on the
/// target in a dependency table (`{p}.BSCHEMA = ? AND …`), which takes
/// `{n}` parameters; `{on_table}` is the triggers on the table (a table or
/// view target only).
///
/// `(K, schema, name, type, TEXT)`: `K` is `V` (a view or MQT), `R` (a
/// routine), `B` (a trigger) or `A` (an alias).
fn deps_sql(b: impl Fn(&str) -> String, on_table: bool) -> String {
    let mut sql = format!(
        "SELECT 'V', TRIM(v.VIEWSCHEMA), v.VIEWNAME, t.TYPE, v.TEXT
  FROM SYSCAT.VIEWS v
  JOIN SYSCAT.TABLES t ON t.TABSCHEMA = v.VIEWSCHEMA AND t.TABNAME = v.VIEWNAME
 WHERE EXISTS (SELECT 1 FROM SYSCAT.VIEWDEP d WHERE d.VIEWSCHEMA = v.VIEWSCHEMA AND d.VIEWNAME = v.VIEWNAME AND {})
UNION ALL
SELECT 'R', TRIM(x.ROUTINESCHEMA), x.ROUTINENAME, x.ROUTINETYPE, x.TEXT
  FROM SYSCAT.ROUTINES x
 WHERE x.ROUTINETYPE IN ('P', 'F')
   AND (EXISTS (SELECT 1 FROM SYSCAT.ROUTINEDEP d WHERE d.ROUTINESCHEMA = x.ROUTINESCHEMA AND d.SPECIFICNAME = x.SPECIFICNAME AND {})
     OR EXISTS (SELECT 1 FROM SYSCAT.ROUTINEDEP d
                  JOIN SYSCAT.PACKAGEDEP p ON p.PKGSCHEMA = d.BSCHEMA AND p.PKGNAME = d.BNAME
                 WHERE d.BTYPE = 'K' AND d.ROUTINESCHEMA = x.ROUTINESCHEMA AND d.SPECIFICNAME = x.SPECIFICNAME AND {}))
UNION ALL
SELECT 'B', TRIM(g.TRIGSCHEMA), g.TRIGNAME, 'B', g.TEXT
  FROM SYSCAT.TRIGGERS g
 WHERE (EXISTS (SELECT 1 FROM SYSCAT.TRIGDEP d WHERE d.TRIGSCHEMA = g.TRIGSCHEMA AND d.TRIGNAME = g.TRIGNAME AND {})
     OR EXISTS (SELECT 1 FROM SYSCAT.TRIGDEP d
                  JOIN SYSCAT.PACKAGEDEP p ON p.PKGSCHEMA = d.BSCHEMA AND p.PKGNAME = d.BNAME
                 WHERE d.BTYPE = 'K' AND d.TRIGSCHEMA = g.TRIGSCHEMA AND d.TRIGNAME = g.TRIGNAME AND {}))",
        b("d"),
        b("d"),
        b("p"),
        b("d"),
        b("p"),
    );
    if on_table {
        sql.push_str(
            "
UNION ALL
SELECT 'B', TRIM(g.TRIGSCHEMA), g.TRIGNAME, 'B', g.TEXT
  FROM SYSCAT.TRIGGERS g WHERE g.TABSCHEMA = ? AND g.TABNAME = ?",
        );
    }
    sql.push_str(
        "
UNION ALL
SELECT 'A', TRIM(a.TABSCHEMA), a.TABNAME, 'A', CAST(NULL AS CLOB(1M))
  FROM SYSCAT.TABLES a WHERE a.TYPE = 'A' AND a.BASE_TABSCHEMA = ? AND a.BASE_TABNAME = ?",
    );
    sql
}

/// Routines and triggers whose source contains the name (`?` is the
/// pattern): what the catalog doesn't record (dynamic SQL).
const TEXT_SQL: &str = "SELECT 'R', TRIM(x.ROUTINESCHEMA), x.ROUTINENAME, x.ROUTINETYPE, x.TEXT
  FROM SYSCAT.ROUTINES x
 WHERE x.ROUTINETYPE IN ('P', 'F') AND x.ROUTINESCHEMA NOT LIKE 'SYS%'
   AND x.ROUTINESCHEMA NOT IN ('NULLID', 'SQLJ') AND UCASE(x.TEXT) LIKE ? ESCAPE '\\'
UNION ALL
SELECT 'B', TRIM(g.TRIGSCHEMA), g.TRIGNAME, 'B', g.TEXT
  FROM SYSCAT.TRIGGERS g
 WHERE g.TRIGSCHEMA NOT LIKE 'SYS%' AND g.TRIGSCHEMA NOT IN ('NULLID', 'SQLJ')
   AND UCASE(g.TEXT) LIKE ? ESCAPE '\\'";

/// The foreign keys that point at the table (and, for a column, those it
/// holds): constraint, holder's schema and table, its column, referenced
/// schema and table, referenced column, in order.
const FOREIGN_KEYS_TO: &str =
    "SELECT TRIM(r.CONSTNAME), TRIM(r.TABSCHEMA), r.TABNAME, k.COLNAME, TRIM(r.REFTABSCHEMA), r.REFTABNAME, p.COLNAME
  FROM SYSCAT.REFERENCES r
  JOIN SYSCAT.KEYCOLUSE k
    ON k.CONSTNAME = r.CONSTNAME AND k.TABSCHEMA = r.TABSCHEMA AND k.TABNAME = r.TABNAME
  JOIN SYSCAT.KEYCOLUSE p
    ON p.CONSTNAME = r.REFKEYNAME AND p.TABSCHEMA = r.REFTABSCHEMA AND p.TABNAME = r.REFTABNAME
   AND p.COLSEQ = k.COLSEQ
 WHERE (r.REFTABSCHEMA = ? AND r.REFTABNAME = ?){held}
 ORDER BY 2, 3, 1, k.COLSEQ";
const HELD: &str = " OR (r.TABSCHEMA = ? AND r.TABNAME = ?)";

/// The table's index columns: index, column.
const INDEXES: &str = "SELECT TRIM(i.INDNAME), c.COLNAME
  FROM SYSCAT.INDEXES i
  JOIN SYSCAT.INDEXCOLUSE c ON c.INDSCHEMA = i.INDSCHEMA AND c.INDNAME = i.INDNAME
 WHERE i.TABSCHEMA = ? AND i.TABNAME = ?
 ORDER BY i.INDNAME, c.COLSEQ";
/// The table's checks that name the column: constraint, expression.
const CHECKS: &str = "SELECT TRIM(c.CONSTNAME), c.TEXT
  FROM SYSCAT.CHECKS c
  JOIN SYSCAT.COLCHECKS cc
    ON cc.CONSTNAME = c.CONSTNAME AND cc.TABSCHEMA = c.TABSCHEMA AND cc.TABNAME = c.TABNAME
 WHERE c.TABSCHEMA = ? AND c.TABNAME = ? AND cc.COLNAME = ?";

/// The `BTYPE`s a kind has in the dependency tables, and whether its
/// `BNAME` is a routine's specific name.
fn btypes(kind: &str) -> Option<(&'static str, bool)> {
    Some(match kind {
        kinds::TABLE => ("'T', 'U', 'G', 'H', 'L', 'S'", false),
        kinds::VIEW => ("'V', 'W'", false),
        kinds::SYNONYM => ("'A'", false),
        kinds::SEQUENCE => ("'Q'", false),
        kinds::TYPE => ("'R'", false),
        kinds::PROCEDURE | kinds::FUNCTION => ("'F'", true),
        _ => return None,
    })
}

/// The match on the target in a dependency table with alias `p`, and how
/// many parameters it takes (the target's schema and name, in that order,
/// once or twice).
fn matching(kind: &str, p: &str) -> Option<(String, usize)> {
    let (types, routine) = btypes(kind)?;
    Some(if routine {
        let t = if kind == kinds::PROCEDURE { 'P' } else { 'F' };
        (
            format!(
                "{p}.BSCHEMA = ? AND {p}.BTYPE = 'F' AND {p}.BNAME IN (SELECT SPECIFICNAME FROM SYSCAT.ROUTINES WHERE ROUTINESCHEMA = ? AND ROUTINENAME = ? AND ROUTINETYPE = '{t}')"
            ),
            3,
        )
    } else {
        (
            format!("{p}.BSCHEMA = ? AND {p}.BNAME = ? AND {p}.BTYPE IN ({types})"),
            2,
        )
    })
}

/// The query and its parameters for a target of `kind` (`schema`, `name`).
fn deps_query(kind: &str, schema: &str, name: &str) -> Option<(String, Vec<String>)> {
    let (_, routine) = btypes(kind)?;
    let on_table = matches!(kind, kinds::TABLE | kinds::VIEW);
    // Each `{}` of `deps_sql`: five matches, then the triggers on a table
    // and the alias's base.
    let sql = deps_sql(
        |p| matching(kind, p).map(|m| m.0).unwrap_or_default(),
        on_table,
    );
    let one: Vec<String> = if routine {
        vec![schema.into(), schema.into(), name.into()]
    } else {
        vec![schema.into(), name.into()]
    };
    let mut params: Vec<String> = Vec::new();
    for _ in 0..5 {
        params.extend(one.clone());
    }
    if on_table {
        params.extend([schema.to_string(), name.to_string()]);
    }
    params.extend([schema.to_string(), name.to_string()]);
    Some((sql, params))
}

/// `%NAME%`, the name's `LIKE` metacharacters taken literally.
fn like_pattern(name: &str) -> String {
    let mut p = String::from("%");
    for c in name.to_uppercase().chars() {
        if matches!(c, '%' | '_' | '\\') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

fn same(a: &Dependent, kind: &str, schema: &str, name: &str) -> bool {
    a.kind == kind && eq(&a.name, name) && a.schema.as_deref().is_some_and(|s| eq(s, schema))
}

/// The rows of [`deps_query`] (confirmed) and of [`TEXT_SQL`] (by the
/// code), as dependents of `target`. Objects of system schemas and the
/// target itself are left out.
pub(crate) fn parse_objects(
    recorded: &Rows,
    by_text: &Rows,
    target: &DependencyTarget,
    scan: &DependencyScan,
    system: impl Fn(&str) -> bool,
    report: &mut DependencyReport,
) -> Vec<Dependent> {
    let t = &target.object;
    let mut out: Vec<Dependent> = Vec::new();
    for (is_recorded, rows) in [(true, recorded), (false, by_text)] {
        for r in rows {
            let (Some(k), Some(schema), Some(name)) = (cell(r, 0), cell(r, 1), cell(r, 2)) else {
                continue;
            };
            let sub = cell(r, 3);
            let kind = match k.as_str() {
                "V" => kind_of(sub.as_deref().unwrap_or("V"), None),
                "R" => kind_of("F", sub.as_deref()),
                "B" => Some(kinds::TRIGGER),
                "A" => Some(kinds::SYNONYM),
                _ => None,
            };
            let Some(kind) = kind else { continue };
            if system(&schema) {
                continue;
            }
            let is_target =
                kind == t.kind && eq(&name, &t.name) && t.schema().is_some_and(|s| eq(s, &schema));
            if is_target || out.iter().any(|d| same(d, kind, &schema, &name)) {
                continue;
            }
            // The source: absent for an alias and an external routine.
            let body = r.get(4).cloned().flatten().filter(|b| !b.trim().is_empty());
            if body.is_some() {
                report.scanned += 1;
            }
            let found = body
                .as_deref()
                .and_then(|b| find_mentions(b, &scan.dialect, target));
            let (confidence, mentions) = match (is_recorded, target.column.is_some(), found) {
                (true, false, found) => (
                    Confidence::Confirmed,
                    found.map(|(_, m)| m).unwrap_or_default(),
                ),
                (_, true, Some((c, m))) => (c, m),
                (true, true, None) => {
                    // The catalog says it uses the table; without its source
                    // there's no telling whether it names the column.
                    if body.is_none() && k != "A" {
                        report.unreadable.push(format!("{schema}.{name}"));
                    }
                    continue;
                }
                (false, _, Some((c, m))) => (c, m),
                (false, _, None) => continue,
            };
            out.push(Dependent {
                kind: kind.into(),
                schema: Some(schema),
                name,
                parent: None,
                relation: Relation::Code,
                confidence,
                detail: None,
                mentions,
            });
        }
    }
    out
}

/// [`FOREIGN_KEYS_TO`]'s rows: the keys that point at the table (not its
/// own), or for a column the ones that point at it or that it holds.
pub(crate) fn parse_foreign_keys(rows: &Rows, target: &DependencyTarget) -> Vec<Dependent> {
    struct Fk {
        name: String,
        holder: (String, String),
        to: (String, String),
        columns: Vec<String>,
        ref_columns: Vec<String>,
    }
    let mut keys: Vec<Fk> = Vec::new();
    for r in rows {
        let (Some(name), Some(hs), Some(h), Some(col), Some(rs), Some(rt), Some(rc)) = (
            cell(r, 0),
            cell(r, 1),
            cell(r, 2),
            cell(r, 3),
            cell(r, 4),
            cell(r, 5),
            cell(r, 6),
        ) else {
            continue;
        };
        match keys
            .iter_mut()
            .find(|k| k.name == name && k.holder == (hs.clone(), h.clone()))
        {
            Some(k) => {
                k.columns.push(col);
                k.ref_columns.push(rc);
            }
            None => keys.push(Fk {
                name,
                holder: (hs, h),
                to: (rs, rt),
                columns: vec![col],
                ref_columns: vec![rc],
            }),
        }
    }
    let t = &target.object;
    let ts = t.schema().unwrap_or_default();
    let mut out = Vec::new();
    for k in keys {
        let points_here = eq(&k.to.0, ts) && eq(&k.to.1, &t.name);
        let holds = eq(&k.holder.0, ts) && eq(&k.holder.1, &t.name);
        let hit = match target.column.as_deref() {
            None => points_here && !holds,
            Some(c) => {
                (points_here && k.ref_columns.iter().any(|r| eq(r, c)))
                    || (holds && k.columns.iter().any(|r| eq(r, c)))
            }
        };
        if !hit {
            continue;
        }
        out.push(Dependent {
            kind: kinds::TABLE.into(),
            schema: Some(k.holder.0),
            name: k.holder.1,
            parent: None,
            relation: Relation::ForeignKey,
            confidence: Confidence::Confirmed,
            detail: Some(format!(
                "{} ({}) → {}.{} ({})",
                k.name,
                k.columns.join(", "),
                k.to.0,
                k.to.1,
                k.ref_columns.join(", ")
            )),
            mentions: Vec::new(),
        });
    }
    out
}

/// [`INDEXES`]' and [`CHECKS`]' rows that hold the column.
pub(crate) fn parse_column_holders(
    indexes: &Rows,
    checks: &Rows,
    target: &DependencyTarget,
) -> Vec<Dependent> {
    let (t, Some(column)) = (&target.object, target.column.as_deref()) else {
        return Vec::new();
    };
    let holder = |relation, detail: String| Dependent {
        kind: t.kind.clone(),
        schema: t.schema().map(str::to_string),
        name: t.name.clone(),
        parent: None,
        relation,
        confidence: Confidence::Confirmed,
        detail: Some(detail),
        mentions: Vec::new(),
    };
    let mut ixs: Vec<(String, Vec<String>)> = Vec::new();
    for r in indexes {
        let (Some(ix), Some(col)) = (cell(r, 0), cell(r, 1)) else {
            continue;
        };
        match ixs.iter_mut().find(|(n, _)| *n == ix) {
            Some((_, cols)) => cols.push(col),
            None => ixs.push((ix, vec![col])),
        }
    }
    let mut out: Vec<Dependent> = ixs
        .into_iter()
        .filter(|(_, cols)| cols.iter().any(|c| eq(c, column)))
        .map(|(ix, cols)| holder(Relation::Index, format!("{ix} ({})", cols.join(", "))))
        .collect();
    for r in checks {
        if let (Some(name), Some(text)) = (cell(r, 0), cell(r, 1)) {
            out.push(holder(Relation::Check, format!("{name}: {text}")));
        }
    }
    out
}

/// `Session::dependents` on Db2 LUW (see the module's docs).
pub(crate) async fn db2(
    s: &mut OdbcSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let t = &target.object;
    let schema = t.schema().unwrap_or_default().to_string();
    let query = (!schema.is_empty())
        .then(|| deps_query(&t.kind, &schema, &t.name))
        .flatten();
    let Some((sql, params)) = query else {
        return dbine_driver::dependencies::scan(s, target, scan).await;
    };
    let mut report = DependencyReport::default();
    let read = async {
        let recorded = s.query(sql, params).await?;
        let by_text = s
            .query(
                TEXT_SQL.to_string(),
                vec![like_pattern(&t.name), like_pattern(&t.name)],
            )
            .await?;
        let mut keys = Rows::new();
        if t.kind == kinds::TABLE {
            let mut sql = FOREIGN_KEYS_TO.replace("{held}", "");
            let mut params = vec![schema.clone(), t.name.clone()];
            if target.column.is_some() {
                sql = FOREIGN_KEYS_TO.replace("{held}", HELD);
                params.extend([schema.clone(), t.name.clone()]);
            }
            keys = s.query(sql, params).await?;
        }
        let (mut indexes, mut checks) = (Rows::new(), Rows::new());
        if let (Some(c), kinds::TABLE) = (target.column.as_deref(), t.kind.as_str()) {
            let table = vec![schema.clone(), t.name.clone()];
            indexes = s.query(INDEXES.to_string(), table.clone()).await?;
            checks = s
                .query(
                    CHECKS.to_string(),
                    vec![schema.clone(), t.name.clone(), c.to_string()],
                )
                .await?;
        }
        Ok::<_, Error>((recorded, by_text, keys, indexes, checks))
    };
    let (recorded, by_text, keys, indexes, checks) = match read.await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("odbc: the Db2 dependency catalog not read: {e}");
            let mut report = dbine_driver::dependencies::scan(s, target, scan).await?;
            report.note = Some(format!(
                "No se pudo leer el catálogo de dependencias (SYSCAT): {e}"
            ));
            return Ok(report);
        }
    };
    let preset = s.preset;
    let mut items = parse_objects(
        &recorded,
        &by_text,
        target,
        scan,
        |sc| preset.is_system_schema(sc),
        &mut report,
    );
    items.extend(parse_foreign_keys(&keys, target));
    items.extend(parse_column_holders(&indexes, &checks, target));
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
    fn r5(k: &str, s: &str, n: &str, ty: &str, text: Option<&str>) -> Vec<Option<String>> {
        row(&[Some(k), Some(s), Some(n), Some(ty), text])
    }

    fn target(kind: &str, name: &str, column: Option<&str>) -> DependencyTarget {
        DependencyTarget {
            object: ObjectRef {
                kind: kind.into(),
                schema: Some("APP".into()),
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
            has_schemas: true,
        }
    }

    fn parse(
        recorded: &Rows,
        by_text: &Rows,
        t: &DependencyTarget,
    ) -> (Vec<Dependent>, DependencyReport) {
        let mut report = DependencyReport::default();
        let sys = |s: &str| s.starts_with("SYS");
        let items = parse_objects(recorded, by_text, t, &scan(), sys, &mut report);
        (items, report)
    }

    #[test]
    fn every_kind_matches_the_target_in_each_catalog_table() {
        let (sql, params) = deps_query(kinds::TABLE, "APP", "ORDERS").unwrap();
        // Five matches (views, routines, their packages, triggers, theirs),
        // the triggers on the table and the alias's base.
        assert_eq!(sql.matches('?').count(), params.len());
        assert_eq!(params.len(), 5 * 2 + 2 + 2);
        assert!(sql.contains("d.BTYPE IN ('T', 'U', 'G', 'H', 'L', 'S')"));
        assert!(sql.contains("p.BSCHEMA = ? AND p.BNAME = ?"));
        assert!(sql.contains("FROM SYSCAT.TRIGGERS g WHERE g.TABSCHEMA = ?"));

        let (sql, params) = deps_query(kinds::PROCEDURE, "APP", "P_LOAD").unwrap();
        assert_eq!(sql.matches('?').count(), params.len());
        assert_eq!(params.len(), 5 * 3 + 2);
        assert!(sql.contains("d.BTYPE = 'F' AND d.BNAME IN (SELECT SPECIFICNAME FROM SYSCAT.ROUTINES WHERE ROUTINESCHEMA = ? AND ROUTINENAME = ? AND ROUTINETYPE = 'P')"));
        assert_eq!(&params[..3], ["APP", "APP", "P_LOAD"]);
        // Only a table or view has triggers on it.
        assert!(!sql.contains("g.TABSCHEMA"));

        assert!(deps_query(kinds::TRIGGER, "APP", "T").is_none());
        assert!(deps_query(kinds::INDEX, "APP", "I").is_none());
    }

    #[test]
    fn the_text_search_takes_wildcards_literally() {
        assert_eq!(like_pattern("a_b%c"), "%A\\_B\\%C%");
        assert_eq!(TEXT_SQL.matches('?').count(), 2);
    }

    #[test]
    fn what_the_catalog_records_is_confirmed_with_its_lines() {
        let t = target(kinds::TABLE, "ORDERS", None);
        let recorded = vec![
            r5(
                "V",
                "APP",
                "V_TOTALS",
                "V",
                Some("CREATE VIEW APP.V_TOTALS AS SELECT * FROM APP.ORDERS"),
            ),
            r5(
                "R",
                "APP",
                "P_LOAD",
                "P",
                Some("BEGIN\n INSERT INTO APP.ORDERS VALUES (1);\nEND"),
            ),
            r5(
                "B",
                "APP",
                "TRG",
                "B",
                Some("BEGIN ATOMIC INSERT INTO APP.LINES VALUES (1); END"),
            ),
            r5("A", "APP", "A_ORDERS", "A", None),
            r5("V", "SYSIBM", "SYSV", "V", None),
            // An MQT is a table in the explorer.
            r5(
                "V",
                "APP",
                "MQT_ORDERS",
                "S",
                Some("SELECT 1 FROM APP.ORDERS"),
            ),
        ];
        let (items, report) = parse(&recorded, &vec![], &t);
        let by: Vec<_> = items
            .iter()
            .map(|d| {
                (
                    d.kind.as_str(),
                    d.name.as_str(),
                    d.confidence,
                    d.mentions.len(),
                )
            })
            .collect();
        assert_eq!(
            by,
            [
                ("view", "V_TOTALS", Confidence::Confirmed, 1),
                ("procedure", "P_LOAD", Confidence::Confirmed, 1),
                ("trigger", "TRG", Confidence::Confirmed, 0),
                ("synonym", "A_ORDERS", Confidence::Confirmed, 0),
                ("table", "MQT_ORDERS", Confidence::Confirmed, 1),
            ]
        );
        assert_eq!(report.scanned, 4);
        assert!(report.unreadable.is_empty());
    }

    #[test]
    fn the_text_adds_only_what_the_catalog_did_not_list() {
        let t = target(kinds::TABLE, "ORDERS", None);
        let recorded = vec![r5(
            "R",
            "APP",
            "P1",
            "P",
            Some("INSERT INTO APP.ORDERS VALUES (1)"),
        )];
        let by_text = vec![
            // Listed already: kept once, confirmed.
            r5(
                "R",
                "APP",
                "P1",
                "P",
                Some("INSERT INTO APP.ORDERS VALUES (1)"),
            ),
            // Dynamic SQL: only a string names it.
            r5(
                "R",
                "APP",
                "P2",
                "P",
                Some("EXECUTE IMMEDIATE 'DELETE FROM APP.ORDERS'"),
            ),
            // A longer name isn't it.
            r5("R", "APP", "P3", "P", Some("SELECT 1 FROM APP.ORDERS_OLD")),
            // Another schema's ORDERS.
            r5("R", "OTHER", "P4", "P", Some("SELECT 1 FROM OTHER.ORDERS")),
            r5(
                "B",
                "APP",
                "TG",
                "B",
                Some("BEGIN ATOMIC SET N = (SELECT COUNT(*) FROM APP.ORDERS); END"),
            ),
        ];
        let (items, _) = parse(&recorded, &by_text, &t);
        let by: Vec<_> = items
            .iter()
            .map(|d| {
                (
                    d.name.as_str(),
                    d.confidence,
                    d.mentions.iter().any(|m| m.dynamic),
                )
            })
            .collect();
        assert_eq!(
            by,
            [
                ("P1", Confidence::Confirmed, false),
                ("P2", Confidence::Review, true),
                ("TG", Confidence::Probable, false),
            ]
        );
    }

    #[test]
    fn a_column_needs_the_source_to_name_it() {
        let t = target(kinds::TABLE, "ORDERS", Some("TOTAL"));
        let recorded = vec![
            r5(
                "V",
                "APP",
                "V_A",
                "V",
                Some("SELECT o.TOTAL FROM APP.ORDERS o"),
            ),
            r5(
                "V",
                "APP",
                "V_B",
                "V",
                Some("SELECT o.ID FROM APP.ORDERS o"),
            ),
            r5("R", "APP", "P_X", "P", None),
        ];
        let (items, report) = parse(&recorded, &vec![], &t);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "V_A");
        assert_eq!(items[0].confidence, Confidence::Probable);
        assert_eq!(report.unreadable, ["APP.P_X"]);
    }

    #[test]
    fn the_target_is_not_its_own_dependent() {
        let t = target(kinds::PROCEDURE, "P_REC", None);
        let recorded = vec![
            r5("R", "APP", "P_REC", "P", Some("CALL APP.P_REC()")),
            r5("R", "APP", "P_REC", "F", Some("RETURN APP.P_REC()")),
        ];
        let (items, _) = parse(&recorded, &vec![], &t);
        // A function of the same name is another object.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "function");
    }

    fn fk(
        c: &str,
        hs: &str,
        h: &str,
        col: &str,
        rs: &str,
        rt: &str,
        rc: &str,
    ) -> Vec<Option<String>> {
        row(&[
            Some(c),
            Some(hs),
            Some(h),
            Some(col),
            Some(rs),
            Some(rt),
            Some(rc),
        ])
    }

    #[test]
    fn foreign_keys_point_at_the_table_and_the_column() {
        let rows = vec![
            fk(
                "FK_LINES", "APP", "LINES", "ORDER_ID", "APP", "ORDERS", "ID",
            ),
            fk("FK_X", "OTHER", "X", "A", "APP", "ORDERS", "ID"),
            fk("FK_X", "OTHER", "X", "B", "APP", "ORDERS", "ID2"),
            fk("FK_SELF", "APP", "ORDERS", "PARENT", "APP", "ORDERS", "ID"),
            fk("FK_UP", "APP", "ORDERS", "GRP", "APP", "GROUPS", "ID"),
        ];
        let out = parse_foreign_keys(&rows, &target(kinds::TABLE, "ORDERS", None));
        let names: Vec<_> = out
            .iter()
            .map(|d| (d.schema.as_deref().unwrap(), d.name.as_str()))
            .collect();
        assert_eq!(names, [("APP", "LINES"), ("OTHER", "X")]);
        assert_eq!(
            out[1].detail.as_deref(),
            Some("FK_X (A, B) → APP.ORDERS (ID, ID2)")
        );
        let col = |c: &str| {
            parse_foreign_keys(&rows, &target(kinds::TABLE, "ORDERS", Some(c)))
                .into_iter()
                .map(|d| d.detail.unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(col("GRP"), ["FK_UP (GRP) → APP.GROUPS (ID)"]);
        assert_eq!(col("ID").len(), 3);
        assert!(col("TOTAL").is_empty());
    }

    #[test]
    fn a_columns_indexes_and_checks() {
        let ix = vec![
            row(&[Some("SQL1"), Some("ID")]),
            row(&[Some("IX_T"), Some("TOTAL")]),
            row(&[Some("IX_T"), Some("ID")]),
        ];
        let ck = vec![row(&[Some("CK_T"), Some("TOTAL > 0")])];
        let out = parse_column_holders(&ix, &ck, &target(kinds::TABLE, "ORDERS", Some("TOTAL")));
        let d: Vec<_> = out
            .iter()
            .map(|d| (d.relation, d.detail.clone().unwrap()))
            .collect();
        assert_eq!(
            d,
            [
                (Relation::Index, "IX_T (TOTAL, ID)".to_string()),
                (Relation::Check, "CK_T: TOTAL > 0".to_string()),
            ]
        );
        assert!(parse_column_holders(&ix, &ck, &target(kinds::TABLE, "ORDERS", None)).is_empty());
    }
}
