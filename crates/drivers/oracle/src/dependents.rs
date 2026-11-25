//! What depends on an object (`Session::dependents`), from Oracle's
//! dictionary: the same views as [`crate::references`] read the other way,
//! filtered by the object they point at, instead of reading every object's
//! source.
//!
//! - `ALL_DEPENDENCIES WHERE referenced_owner/name/type = target`: the
//!   views, materialized views, procedures, functions, packages, triggers,
//!   types, synonyms (public ones too) and tables Oracle recorded as using
//!   it when it compiled them. Confirmed. It records no columns, so for a
//!   column the objects of its table are kept when their source names the
//!   column (Probable), and only those are read.
//! - Foreign keys: `ALL_CONSTRAINTS` rows whose `r_constraint_name` is a
//!   constraint of the target. For a column, also its own indexes
//!   (`ALL_IND_COLUMNS`) and checks (`ALL_CONSTRAINTS`).
//! - What the dictionary can't resolve, the PL/SQL that names the target in
//!   a string (`EXECUTE IMMEDIATE`) or whose compilation failed: one
//!   `ALL_SOURCE` query keeps, on the server, the objects whose text holds
//!   the name, and only those the dictionary didn't return are read and
//!   parsed (Probable or Review, with the lines). Views have no source in
//!   `ALL_SOURCE`: an invalid view is not searched.
//!
//! What Oracle maintains (SYS's…) is left out. At most [`READ_CAP`]
//! definitions are read; the rest are listed as unreadable. If the
//! dictionary can't be read, the generic scan answers, with a note.

use crate::references::{maintained, ref_kind, schema_of, sort};
use crate::{db_code, err, OracleSession};
use dbine_driver::dependencies::{
    find_mentions, scan, Confidence, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    Relation,
};
use dbine_driver::{kinds, ObjectRef, Result, Session};
use oracledb::Connection;

/// Definitions read for a column or from the source search; the rest go to
/// `unreadable`.
pub(crate) const READ_CAP: usize = 300;

/// The note when the generic scan answers instead of the dictionary: the
/// server's first line.
fn unread_note(e: &dbine_driver::Error) -> String {
    let (view, msg) = ("ALL_DEPENDENCIES", e.to_string());
    format!(
        "No se pudo leer {view}: {}",
        msg.lines().next().unwrap_or_default().trim()
    )
}

/// The `ALL_DEPENDENCIES.REFERENCED_TYPE`s of an explorer kind; `None`: the
/// generic scan answers. A materialized view is recorded as itself or as
/// its container table, depending on the version.
fn referenced_types(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        kinds::TABLE => &["TABLE"],
        kinds::VIEW => &["VIEW"],
        kinds::MATERIALIZED_VIEW => &["MATERIALIZED VIEW", "TABLE"],
        kinds::PROCEDURE => &["PROCEDURE"],
        kinds::FUNCTION => &["FUNCTION"],
        crate::PACKAGE => &["PACKAGE"],
        kinds::TYPE => &["TYPE"],
        kinds::SYNONYM => &["SYNONYM"],
        kinds::SEQUENCE => &["SEQUENCE"],
        _ => return None,
    })
}

fn type_list(types: &[&str]) -> String {
    types
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `:1` the owner, `:2` the name. A row on the object itself (a package
/// body on its spec) is left out; a trigger may share its table's name. A
/// materialized view's container table is the materialized view.
pub(crate) fn dependents_sql(types: &[&str], legacy: bool) -> String {
    format!(
        "SELECT d.owner, d.name,
       CASE WHEN d.type = 'TABLE' AND EXISTS (
         SELECT 1 FROM all_mviews m WHERE m.owner = d.owner AND m.mview_name = d.name)
       THEN 'MATERIALIZED VIEW' ELSE d.type END, t.table_name
  FROM all_dependencies d
  LEFT JOIN all_triggers t ON d.type = 'TRIGGER' AND t.owner = d.owner AND t.trigger_name = d.name
 WHERE d.referenced_owner = :1 AND d.referenced_name = :2 AND d.referenced_type IN ({})
   AND d.referenced_link_name IS NULL
   AND NOT (d.owner = d.referenced_owner AND d.name = d.referenced_name AND d.type <> 'TRIGGER')
   AND NOT ({})
 ORDER BY d.owner, d.name, d.type",
        type_list(types),
        maintained("d.owner", legacy)
    )
}

/// The foreign keys that point at the table `:1`.`:2` (or that it holds:
/// `:3` and `:4` repeat the pair), one row per column pair: holder owner,
/// table, constraint, column, referenced owner, table and column.
pub(crate) const FOREIGN_KEYS_SQL: &str = "SELECT c.owner, c.table_name, c.constraint_name, cc.column_name, r.owner, r.table_name, rc.column_name
  FROM all_constraints c
  JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name
  JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name
  JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name AND rc.position = cc.position
 WHERE c.constraint_type = 'R' AND c.table_name NOT LIKE 'BIN$%'
   AND ((r.owner = :1 AND r.table_name = :2) OR (c.owner = :3 AND c.table_name = :4))
 ORDER BY c.owner, c.table_name, c.constraint_name, cc.position";

/// The indexes of the table `:1`.`:2` on the column `:3`, one row per key
/// column.
const INDEXES_SQL: &str = "SELECT i.index_name, k.column_name
  FROM all_indexes i
  JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name
  JOIN all_ind_columns k ON k.index_owner = i.owner AND k.index_name = i.index_name
 WHERE i.table_owner = :1 AND i.table_name = :2 AND ic.column_name = :3
 ORDER BY i.index_name, k.column_position";

/// The checks of the table `:1`.`:2` on the column `:3` (SEARCH_CONDITION is
/// a LONG: last).
const CHECKS_SQL: &str = "SELECT c.constraint_name, c.search_condition
  FROM all_constraints c
  JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name
 WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = 'C' AND cc.column_name = :3
 ORDER BY c.constraint_name";

/// The PL/SQL objects whose text holds `:1` (a `LIKE` pattern, upper case).
fn source_sql(legacy: bool) -> String {
    format!(
        "SELECT DISTINCT s.owner, s.name, s.type FROM all_source s
 WHERE s.type IN ('PROCEDURE', 'FUNCTION', 'PACKAGE', 'PACKAGE BODY', 'TRIGGER', 'TYPE', 'TYPE BODY')
   AND NOT ({}) AND UPPER(s.text) LIKE :1 ESCAPE '\\'",
        maintained("s.owner", legacy)
    )
}

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

/// An `ALL_DEPENDENCIES` row: owner, name, type, a trigger's table.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DepRow {
    pub owner: String,
    pub name: String,
    pub ty: String,
    pub table: Option<String>,
}

/// A foreign key's column pair.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FkPair {
    pub owner: String,
    pub table: String,
    pub name: String,
    pub column: String,
    pub ref_owner: String,
    pub ref_table: String,
    pub ref_column: String,
}

/// What the dictionary says about the target.
#[derive(Debug, Default)]
pub(crate) struct Read {
    pub deps: Vec<DepRow>,
    pub fks: Vec<FkPair>,
    /// Index name and key column, in key order.
    pub indexes: Vec<(String, String)>,
    /// Check name and condition.
    pub checks: Vec<(String, String)>,
    /// Objects whose text holds the name: owner, name, type.
    pub hits: Vec<(String, String, String)>,
}

fn text(r: &oracledb::Row, i: usize) -> Result<Option<String>> {
    r.get::<Option<String>>(i).map_err(err)
}

fn string(r: &oracledb::Row, i: usize) -> Result<String> {
    Ok(text(r, i)?.unwrap_or_default())
}

/// Runs `sql` with the modern `ORACLE_MAINTAINED` filter and, before 12c
/// (ORA-00904), with the list.
fn query(
    c: &Connection,
    sql: impl Fn(bool) -> String,
    params: &[&dyn oracledb::ToDbValue],
) -> Result<Vec<oracledb::Row>> {
    let run = |legacy| -> std::result::Result<Vec<oracledb::Row>, oracledb::Error> {
        c.query(&sql(legacy), params)?.collect()
    };
    match run(false) {
        Err(e) if db_code(&e) == Some(904) => run(true),
        r => r,
    }
    .map_err(err)
}

pub(crate) fn read_catalog(
    c: &Connection,
    types: &[&str],
    owner: &str,
    name: &str,
    column: Option<&str>,
    kind: &str,
) -> Result<Read> {
    let mut read = Read::default();
    for r in query(c, |l| dependents_sql(types, l), &[&owner, &name])? {
        read.deps.push(DepRow {
            owner: string(&r, 0)?,
            name: string(&r, 1)?,
            ty: string(&r, 2)?,
            table: text(&r, 3)?,
        });
    }
    if kind == kinds::TABLE {
        for r in c
            .query(FOREIGN_KEYS_SQL, &[&owner, &name, &owner, &name])
            .and_then(|rows| rows.collect::<std::result::Result<Vec<_>, _>>())
            .map_err(err)?
        {
            read.fks.push(FkPair {
                owner: string(&r, 0)?,
                table: string(&r, 1)?,
                name: string(&r, 2)?,
                column: string(&r, 3)?,
                ref_owner: string(&r, 4)?,
                ref_table: string(&r, 5)?,
                ref_column: string(&r, 6)?,
            });
        }
        if let Some(col) = column {
            for r in c
                .query(INDEXES_SQL, &[&owner, &name, &col])
                .and_then(|rows| rows.collect::<std::result::Result<Vec<_>, _>>())
                .map_err(err)?
            {
                read.indexes.push((string(&r, 0)?, string(&r, 1)?));
            }
            for r in c
                .query(CHECKS_SQL, &[&owner, &name, &col])
                .and_then(|rows| rows.collect::<std::result::Result<Vec<_>, _>>())
                .map_err(err)?
            {
                read.checks.push((string(&r, 0)?, string(&r, 1)?));
            }
        }
    }
    let pattern = like_pattern(name);
    for r in query(c, source_sql, &[&pattern.as_str()])? {
        read.hits
            .push((string(&r, 0)?, string(&r, 1)?, string(&r, 2)?));
    }
    Ok(read)
}

/// A dependent as the explorer names it: `owner` is the schema unless it is
/// the session's.
fn object(kind: &'static str, owner: &str, session: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind: kind.into(),
        schema: schema_of(owner, session),
        name: name.into(),
    }
}

fn dependent(o: ObjectRef, parent: Option<String>, confidence: Confidence) -> Dependent {
    Dependent {
        kind: o.kind,
        schema: o.schema,
        name: o.name,
        parent,
        relation: Relation::Code,
        confidence,
        detail: None,
        mentions: Vec::new(),
    }
}

/// The objects of `rows` and `hits`, as the explorer lists them: a package
/// body is its package, a type body its type, once each. `recorded`: the
/// dictionary says the target is used there.
pub(crate) fn candidates(read: &Read, session: &str, target: &ObjectRef) -> Vec<(Dependent, bool)> {
    let mut out: Vec<(Dependent, bool)> = Vec::new();
    // The target itself (its own source holds its name) is out. Compared
    // by owner: the target may name the session's schema or leave it out.
    let target_owner = target.schema().unwrap_or(session);
    let mut add = |ty: &str, owner: &str, name: &str, table: Option<&String>, recorded: bool| {
        let Some(kind) = ref_kind(ty, false) else {
            return;
        };
        if kind == target.kind
            && owner.eq_ignore_ascii_case(target_owner)
            && name.eq_ignore_ascii_case(&target.name)
        {
            return;
        }
        let o = object(kind, owner, session, name);
        match out.iter_mut().find(|(d, _)| {
            d.kind == o.kind
                && d.schema
                    .as_deref()
                    .unwrap_or("")
                    .eq_ignore_ascii_case(o.schema().unwrap_or(""))
                && d.name.eq_ignore_ascii_case(&o.name)
        }) {
            Some((_, was)) => *was |= recorded,
            None => out.push((
                dependent(
                    o,
                    table.filter(|_| kind == kinds::TRIGGER).cloned(),
                    if recorded {
                        Confidence::Confirmed
                    } else {
                        Confidence::Probable
                    },
                ),
                recorded,
            )),
        }
    };
    for d in &read.deps {
        add(&d.ty, &d.owner, &d.name, d.table.as_ref(), true);
    }
    for (owner, name, ty) in &read.hits {
        add(ty, owner, name, None, false);
    }
    out
}

/// The foreign keys that hold or point at the target (a column: the ones
/// that name it), as [`dbine_driver::dependencies::schema_dependents`] shows
/// them.
pub(crate) fn foreign_keys(
    pairs: &[FkPair],
    session: &str,
    owner: &str,
    target: &ObjectRef,
    column: Option<&str>,
) -> Vec<Dependent> {
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let mut groups: Vec<Vec<&FkPair>> = Vec::new();
    for p in pairs {
        match groups.last_mut() {
            Some(g) if g[0].owner == p.owner && g[0].table == p.table && g[0].name == p.name => {
                g.push(p)
            }
            _ => groups.push(vec![p]),
        }
    }
    let mut out = Vec::new();
    for g in groups {
        let f = g[0];
        let points_here = eq(&f.ref_owner, owner) && eq(&f.ref_table, &target.name);
        let held_here = eq(&f.owner, owner) && eq(&f.table, &target.name);
        let hit = match column {
            None => points_here && !held_here,
            Some(c) => {
                (points_here && g.iter().any(|p| eq(&p.ref_column, c)))
                    || (held_here && g.iter().any(|p| eq(&p.column, c)))
            }
        };
        if !hit {
            continue;
        }
        let cols: Vec<&str> = g.iter().map(|p| p.column.as_str()).collect();
        let refs: Vec<&str> = g.iter().map(|p| p.ref_column.as_str()).collect();
        let to = if eq(&f.ref_owner, &f.owner) {
            f.ref_table.clone()
        } else {
            format!("{}.{}", f.ref_owner, f.ref_table)
        };
        out.push(Dependent {
            kind: kinds::TABLE.into(),
            schema: schema_of(&f.owner, session),
            name: f.table.clone(),
            parent: None,
            relation: Relation::ForeignKey,
            confidence: Confidence::Confirmed,
            detail: Some(format!(
                "{} ({}) → {to} ({})",
                f.name,
                cols.join(", "),
                refs.join(", ")
            )),
            mentions: Vec::new(),
        });
    }
    out
}

/// A NOT NULL constraint (`"COL" IS NOT NULL`) is the column's nullability,
/// not a check.
fn is_not_null(condition: &str) -> bool {
    let c = condition.trim().to_uppercase();
    c.ends_with("IS NOT NULL") && !c.contains(" AND ") && !c.contains(" OR ")
}

/// The target table's own indexes and checks on the column.
pub(crate) fn own_items(
    read: &Read,
    session: &str,
    owner: &str,
    target: &ObjectRef,
) -> Vec<Dependent> {
    let holder = |relation, detail: String| Dependent {
        kind: kinds::TABLE.into(),
        schema: schema_of(owner, session),
        name: target.name.clone(),
        parent: None,
        relation,
        confidence: Confidence::Confirmed,
        detail: Some(detail),
        mentions: Vec::new(),
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < read.indexes.len() {
        let name = &read.indexes[i].0;
        let cols: Vec<&str> = read.indexes[i..]
            .iter()
            .take_while(|(n, _)| n == name)
            .map(|(_, c)| c.as_str())
            .collect();
        i += cols.len();
        out.push(holder(
            Relation::Index,
            format!("{name} ({})", cols.join(", ")),
        ));
    }
    for (name, condition) in &read.checks {
        if !is_not_null(condition) {
            out.push(holder(
                Relation::Check,
                format!("{name}: {}", condition.trim()),
            ));
        }
    }
    out
}

impl OracleSession {
    pub(crate) async fn dependents_impl(
        &mut self,
        target: &DependencyTarget,
        ctx: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = &target.object;
        let Some(types) = referenced_types(&t.kind) else {
            return scan(self, target, ctx).await;
        };
        let owner = self.owner(t);
        let column = target.column.clone().filter(|_| t.kind != kinds::SEQUENCE);
        let read = {
            let (kind, owner, name, column) = (
                t.kind.clone(),
                owner.clone(),
                t.name.clone(),
                column.clone(),
            );
            self.run(move |c| read_catalog(c, types, &owner, &name, column.as_deref(), &kind))
                .await
        };
        let read = match read {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("oracle: dependents not read from the dictionary: {e}");
                let mut report = scan(self, target, ctx).await?;
                report.note = Some(unread_note(&e));
                return Ok(report);
            }
        };
        let session = self.schema.clone();
        let mut report = DependencyReport::default();
        report.items.extend(foreign_keys(
            &read.fks,
            &session,
            &owner,
            t,
            column.as_deref(),
        ));
        report.items.extend(own_items(&read, &session, &owner, t));
        // The dictionary names tables, not columns; and the source search
        // finds objects the dictionary didn't return.
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

    fn target(kind: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        }
    }

    fn dep(owner: &str, name: &str, ty: &str, table: Option<&str>) -> DepRow {
        DepRow {
            owner: owner.into(),
            name: name.into(),
            ty: ty.into(),
            table: table.map(Into::into),
        }
    }

    fn pair(holder: (&str, &str, &str, &str), to: (&str, &str, &str)) -> FkPair {
        FkPair {
            owner: holder.0.into(),
            table: holder.1.into(),
            name: holder.2.into(),
            column: holder.3.into(),
            ref_owner: to.0.into(),
            ref_table: to.1.into(),
            ref_column: to.2.into(),
        }
    }

    #[test]
    fn the_dictionary_is_asked_for_what_points_at_the_target() {
        let q = dependents_sql(&["TABLE"], false);
        assert!(q.contains("d.referenced_owner = :1 AND d.referenced_name = :2"));
        assert!(q.contains("d.referenced_type IN ('TABLE')"));
        assert!(q.contains("oracle_maintained = 'Y'"));
        assert!(!q.contains("d.owner = :1"));
        let q = dependents_sql(&["MATERIALIZED VIEW", "TABLE"], true);
        assert!(q.contains("IN ('MATERIALIZED VIEW', 'TABLE')"));
        assert!(q.contains("'SYS'") && !q.contains("oracle_maintained"));
    }

    #[test]
    fn the_source_search_is_filtered_by_the_server() {
        let q = source_sql(false);
        assert!(q.contains("UPPER(s.text) LIKE :1 ESCAPE"));
        assert!(!q.contains("VIEW"));
        assert_eq!(like_pattern("emp_x"), "%EMP\\_X%");
    }

    #[test]
    fn only_the_kinds_the_dictionary_records_are_read() {
        for k in [
            "table",
            "view",
            "materialized_view",
            "procedure",
            "function",
            "package",
            "type",
            "synonym",
            "sequence",
        ] {
            assert!(referenced_types(k).is_some(), "{k}");
        }
        for k in ["trigger", "schema", "index", "constraint"] {
            assert!(referenced_types(k).is_none(), "{k}");
        }
    }

    #[test]
    fn rows_become_the_explorers_objects_once_each() {
        let read = Read {
            deps: vec![
                dep("APP", "V_EMP", "VIEW", None),
                dep("APP", "PKG", "PACKAGE", None),
                dep("APP", "PKG", "PACKAGE BODY", None),
                dep("APP", "TRG", "TRIGGER", Some("EMP")),
                dep("OTRO", "S_EMP", "SYNONYM", None),
                dep("PUBLIC", "EMP", "SYNONYM", None),
                dep("APP", "EMP", "TABLE", None),
                dep("APP", "J", "JAVA CLASS", None),
            ],
            hits: vec![
                ("APP".into(), "PKG".into(), "PACKAGE BODY".into()),
                ("APP".into(), "P_DIN".into(), "PROCEDURE".into()),
            ],
            ..Default::default()
        };
        let c = candidates(&read, "APP", &target("table", "EMP"));
        let by = |n: &str| c.iter().find(|(d, _)| d.name == n).unwrap();
        assert_eq!(by("V_EMP").0.schema, None);
        assert!(by("V_EMP").1);
        assert_eq!(by("PKG").0.kind, "package");
        assert_eq!(c.iter().filter(|(d, _)| d.name == "PKG").count(), 1);
        assert_eq!(by("TRG").0.parent.as_deref(), Some("EMP"));
        assert_eq!(by("S_EMP").0.schema.as_deref(), Some("OTRO"));
        // A public synonym is PUBLIC's.
        let public = c
            .iter()
            .find(|(d, _)| d.schema.as_deref() == Some("PUBLIC"))
            .unwrap();
        assert_eq!(public.0.kind, "synonym");
        // What only the text found is not recorded; the target itself and
        // what the explorer doesn't list are out.
        assert!(!by("P_DIN").1);
        assert_eq!(by("P_DIN").0.confidence, Confidence::Probable);
        assert!(c.iter().all(|(d, _)| d.kind != "table"), "{c:?}");
        assert!(c.iter().all(|(d, _)| d.name != "J"));
    }

    /// Its own source names it: the target is left out whether it comes
    /// with the session's schema or without one (the app sends both).
    #[test]
    fn the_target_is_never_its_own_dependent() {
        let read = Read {
            hits: vec![
                ("APP".into(), "P_USA".into(), "PROCEDURE".into()),
                ("APP".into(), "P_LLAMA".into(), "PROCEDURE".into()),
                ("OTRO".into(), "P_USA".into(), "PROCEDURE".into()),
            ],
            ..Default::default()
        };
        let names = |t: &ObjectRef| -> Vec<(Option<String>, String)> {
            candidates(&read, "APP", t)
                .into_iter()
                .map(|(d, _)| (d.schema, d.name))
                .collect()
        };
        for schema in [None, Some("APP"), Some("app")] {
            let t = ObjectRef {
                kind: kinds::PROCEDURE.into(),
                schema: schema.map(Into::into),
                name: "P_USA".into(),
            };
            assert_eq!(
                names(&t),
                [
                    (None, "P_LLAMA".to_string()),
                    (Some("OTRO".to_string()), "P_USA".to_string())
                ],
                "{schema:?}"
            );
        }
        // Another schema's P_USA: the session's one of that name is a dependent.
        let other = ObjectRef {
            kind: kinds::PROCEDURE.into(),
            schema: Some("OTRO".into()),
            name: "P_USA".into(),
        };
        assert_eq!(
            names(&other),
            [(None, "P_USA".to_string()), (None, "P_LLAMA".to_string())]
        );
    }

    #[test]
    fn foreign_keys_point_at_the_table_or_name_the_column() {
        let pairs = vec![
            pair(("APP", "EMP", "FK_DEPT", "DEPT_ID"), ("APP", "DEPT", "ID")),
            pair(("APP", "HIST", "FK_X", "A"), ("OTRO", "DEPT", "ID")),
            pair(("APP", "HIST", "FK_X", "B"), ("OTRO", "DEPT", "CODE")),
            pair(("APP", "DEPT", "FK_SELF", "PARENT"), ("APP", "DEPT", "ID")),
        ];
        let t = target("table", "DEPT");
        let all = foreign_keys(&pairs, "APP", "APP", &t, None);
        // Only the one in this schema points at APP.DEPT, and not the table's own.
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].name, "EMP");
        assert_eq!(all[0].relation, Relation::ForeignKey);
        assert_eq!(
            all[0].detail.as_deref(),
            Some("FK_DEPT (DEPT_ID) → DEPT (ID)")
        );
        // A column: the keys that name it, the table's own included.
        let by_id = foreign_keys(&pairs, "APP", "APP", &t, Some("ID"));
        assert_eq!(by_id.len(), 2, "{by_id:?}");
        let own = foreign_keys(&pairs, "APP", "APP", &t, Some("PARENT"));
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].name, "DEPT");
        // Another owner's table, shown with its schema.
        let other = foreign_keys(&pairs, "APP", "OTRO", &t, None);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].name, "HIST");
        assert_eq!(
            other[0].detail.as_deref(),
            Some("FK_X (A, B) → OTRO.DEPT (ID, CODE)")
        );
    }

    #[test]
    fn the_columns_own_indexes_and_checks_leave_out_not_null() {
        let read = Read {
            indexes: vec![
                ("IX_AB".into(), "A".into()),
                ("IX_AB".into(), "B".into()),
                ("PK_T".into(), "A".into()),
            ],
            checks: vec![
                ("SYS_C001".into(), "\"A\" IS NOT NULL".into()),
                ("CK_A".into(), "a > 0 ".into()),
            ],
            ..Default::default()
        };
        let items = own_items(&read, "APP", "APP", &target("table", "T"));
        let shown: Vec<_> = items
            .iter()
            .map(|d| (d.relation, d.detail.clone().unwrap()))
            .collect();
        assert_eq!(
            shown,
            vec![
                (Relation::Index, "IX_AB (A, B)".to_string()),
                (Relation::Index, "PK_T (A)".to_string()),
                (Relation::Check, "CK_A: a > 0".to_string()),
            ]
        );
        assert!(items
            .iter()
            .all(|d| d.name == "T" && d.confidence == Confidence::Confirmed));
        assert!(!is_not_null("a IS NOT NULL OR b IS NOT NULL"));
    }

    #[test]
    fn a_failed_read_says_what_the_server_said() {
        let e = dbine_driver::Error::Query("ORA-00942: table or view does not exist\nmore".into());
        assert_eq!(
            unread_note(&e),
            "No se pudo leer ALL_DEPENDENCIES: ORA-00942: table or view does not exist"
        );
    }
}
