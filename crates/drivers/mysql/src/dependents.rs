//! What depends on an object (`Session::dependents`) on MySQL and MariaDB:
//! a few catalog queries filtered by the target, not the contract's scan
//! (the whole database's schema, then one `SHOW CREATE` per code object).
//!
//! - Foreign keys: `KEY_COLUMN_USAGE` rows that point at the target table
//!   (for a column, also the keys its table holds on it): confirmed.
//! - Views: MySQL 8.0.13+ records what a view reads in `VIEW_TABLE_USAGE`
//!   and `VIEW_ROUTINE_USAGE` (confirmed; the same query brings the view's
//!   definition for the mentions, and a column is probable when the view's
//!   text names it). MariaDB and older MySQL record nothing: the
//!   `VIEWS` definitions that contain the name (`LIKE`, server side) are
//!   read and confirmed in code ([`find_mentions`]: probable).
//! - Routines and triggers: the engines don't track what they use, so
//!   `ROUTINES.ROUTINE_DEFINITION` and `TRIGGERS.ACTION_STATEMENT` that
//!   contain the name come back, and the code confirms them. Routines whose
//!   body the login can't see (NULL) come back too, as unreadable.
//! - A column's indexes (`STATISTICS`) and checks (`CHECK_CONSTRAINTS`).
//!
//! Every query is filtered by the target, so what is read grows with what
//! depends on it, not with the database. A schema, index or constraint
//! target (what a rename looks for as a qualifier), another engine of this
//! family, and a catalog query the server refuses all use the contract's
//! scan; in the last case the note says which view couldn't be read.

use crate::references::{catalog_rows, note_catalog_denied, Cells, NOTE_CODE_HIDDEN};
use crate::session::{lit, MySqlSession};
use crate::Variant;
use dbine_driver::dependencies::{
    find_mentions, Confidence, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    Relation, CONSTRAINT, SCHEMA,
};
use dbine_driver::{kinds, ObjectRef, Result};

pub(crate) async fn dependents(
    s: &mut MySqlSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let t = &target.object;
    let native = matches!(s.variant, Variant::MySql | Variant::MariaDb)
        && !matches!(t.kind.as_str(), SCHEMA | kinds::INDEX | CONSTRAINT);
    let db = match t.schema() {
        Some(d) if native => Some(d.to_string()),
        None if native => s.current_database().await,
        _ => None,
    };
    let Some(db) = db else {
        return dbine_driver::dependencies::scan(s, target, scan).await;
    };
    let mut report = DependencyReport::default();
    let mut items = Vec::new();
    macro_rules! rows {
        ($sql:expr, $view:expr) => {
            match catalog_rows(s, &$sql).await {
                Ok(rows) => rows,
                Err(denied) => return fallback(s, target, scan, $view, denied).await,
            }
        };
    }

    if t.kind == kinds::TABLE {
        items.extend(foreign_keys(
            &rows!(foreign_keys_sql(&db, target), "KEY_COLUMN_USAGE"),
            &db,
            target,
        ));
    }
    if let (Some(c), kinds::TABLE) = (target.column.as_deref(), t.kind.as_str()) {
        // Indexes and checks are the column's own extras: a server without
        // the view (an older one) just lacks them.
        if let Ok(rows) = catalog_rows(s, &indexes_sql(&db, &t.name)).await {
            items.extend(index_holders(&rows, &t.name, c));
        }
        if let Ok(rows) = catalog_rows(s, &checks_sql(s.variant, &db, &t.name)).await {
            items.extend(check_holders(&rows, &t.name, c));
        }
    }

    // Views: the catalog where the server has it, else the definitions
    // that contain the name.
    let by_catalog = s.variant == Variant::MySql
        && matches!(
            t.kind.as_str(),
            kinds::TABLE | kinds::VIEW | kinds::FUNCTION
        );
    let mut view_rows = None;
    if by_catalog {
        match catalog_rows(s, &view_usage_sql(&db, target)).await {
            Ok(rows) => view_rows = Some(rows),
            Err(true) => report.note = Some(note_catalog_denied("VIEW_TABLE_USAGE")),
            Err(false) => {}
        }
    }
    let recorded = view_rows.is_some();
    let view_rows = match view_rows {
        Some(r) => r,
        None => rows!(views_like_sql(&db, &t.name), "VIEWS"),
    };
    items.extend(views(&view_rows, &db, target, scan, recorded, &mut report));

    let routine_rows = rows!(routines_sql(&db, &t.name), "ROUTINES");
    items.extend(routines(&routine_rows, target, scan, &mut report));
    if s.variant.has_triggers() {
        let trigger_rows = rows!(triggers_sql(&db, &t.name), "TRIGGERS");
        items.extend(triggers(&trigger_rows, target, scan, &mut report));
    }
    sort(&mut items);
    report.items = items;
    Ok(report)
}

/// The contract's scan, with the note on what couldn't be read.
async fn fallback(
    s: &mut MySqlSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
    view: &str,
    denied: bool,
) -> Result<DependencyReport> {
    tracing::debug!(
        "{:?}: dependents: {view} unreadable (denied: {denied})",
        s.variant
    );
    let mut report = dbine_driver::dependencies::scan(s, target, scan).await?;
    report.note = Some(note_catalog_denied(view));
    Ok(report)
}

// -- the queries --------------------------------------------------------------------------

/// The `LIKE` condition for a body that contains `name` (`!` escapes, so
/// the server's backslash mode doesn't matter).
fn like(column: &str, name: &str) -> String {
    let mut p = String::from("%");
    for c in name.chars() {
        if matches!(c, '%' | '_' | '!') {
            p.push('!');
        }
        p.push(c);
    }
    p.push('%');
    format!("{column} LIKE {} ESCAPE '!'", lit(&p))
}

/// Every key row that points at the table (and, for a column, the keys the
/// table holds), one per column: `(constraint, holder's database, holder,
/// column, referenced database, referenced table, referenced column)`.
fn foreign_keys_sql(db: &str, target: &DependencyTarget) -> String {
    let (d, t) = (lit(db), lit(&target.object.name));
    let held = if target.column.is_some() {
        format!(" OR (TABLE_SCHEMA = {d} AND TABLE_NAME = {t})")
    } else {
        String::new()
    };
    format!(
        "SELECT CONSTRAINT_NAME, TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME,
       REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME
  FROM information_schema.KEY_COLUMN_USAGE
 WHERE REFERENCED_TABLE_NAME IS NOT NULL
   AND ((REFERENCED_TABLE_SCHEMA = {d} AND REFERENCED_TABLE_NAME = {t}){held})
 ORDER BY TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION"
    )
}

/// The views that read the target (MySQL 8.0.13+), with their definition:
/// `(view's database, view, definition)`.
fn view_usage_sql(db: &str, target: &DependencyTarget) -> String {
    let (d, n) = (lit(db), lit(&target.object.name));
    if target.object.kind == kinds::FUNCTION {
        format!(
            "SELECT DISTINCT r.TABLE_SCHEMA, r.TABLE_NAME, v.VIEW_DEFINITION
  FROM information_schema.VIEW_ROUTINE_USAGE r
  LEFT JOIN information_schema.VIEWS v
    ON v.TABLE_SCHEMA = r.TABLE_SCHEMA AND v.TABLE_NAME = r.TABLE_NAME
 WHERE r.SPECIFIC_SCHEMA = {d} AND r.SPECIFIC_NAME = {n}"
        )
    } else {
        format!(
            "SELECT DISTINCT u.VIEW_SCHEMA, u.VIEW_NAME, v.VIEW_DEFINITION
  FROM information_schema.VIEW_TABLE_USAGE u
  LEFT JOIN information_schema.VIEWS v
    ON v.TABLE_SCHEMA = u.VIEW_SCHEMA AND v.TABLE_NAME = u.VIEW_NAME
 WHERE u.TABLE_SCHEMA = {d} AND u.TABLE_NAME = {n}"
        )
    }
}

/// The views whose definition contains the name, in this database or (when
/// they also name this database) another one: `(database, view, definition)`.
fn views_like_sql(db: &str, name: &str) -> String {
    format!(
        "SELECT TABLE_SCHEMA, TABLE_NAME, VIEW_DEFINITION
  FROM information_schema.VIEWS
 WHERE {} AND (TABLE_SCHEMA = {d} OR {})",
        like("VIEW_DEFINITION", name),
        like("VIEW_DEFINITION", db),
        d = lit(db)
    )
}

/// The routines whose body contains the name, or can't be read (NULL):
/// `(type, name, body)`.
fn routines_sql(db: &str, name: &str) -> String {
    format!(
        "SELECT ROUTINE_TYPE, ROUTINE_NAME, ROUTINE_DEFINITION
  FROM information_schema.ROUTINES
 WHERE ROUTINE_SCHEMA = {} AND (ROUTINE_DEFINITION IS NULL OR {})",
        lit(db),
        like("ROUTINE_DEFINITION", name)
    )
}

/// The triggers whose body contains the name: `(trigger, table, body)`.
fn triggers_sql(db: &str, name: &str) -> String {
    format!(
        "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE, ACTION_STATEMENT
  FROM information_schema.TRIGGERS
 WHERE TRIGGER_SCHEMA = {} AND {}",
        lit(db),
        like("ACTION_STATEMENT", name)
    )
}

/// The table's index columns, in order: `(index, column)`.
fn indexes_sql(db: &str, table: &str) -> String {
    format!(
        "SELECT INDEX_NAME, COLUMN_NAME
  FROM information_schema.STATISTICS
 WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}
 ORDER BY INDEX_NAME, SEQ_IN_INDEX",
        lit(db),
        lit(table)
    )
}

/// The table's checks: `(constraint, expression)`. MySQL's
/// `CHECK_CONSTRAINTS` has no table: `TABLE_CONSTRAINTS` gives it.
fn checks_sql(v: Variant, db: &str, table: &str) -> String {
    let (d, t) = (lit(db), lit(table));
    if v == Variant::MariaDb {
        format!(
            "SELECT CONSTRAINT_NAME, CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS
 WHERE CONSTRAINT_SCHEMA = {d} AND TABLE_NAME = {t}"
        )
    } else {
        format!(
            "SELECT c.CONSTRAINT_NAME, c.CHECK_CLAUSE
  FROM information_schema.CHECK_CONSTRAINTS c
  JOIN information_schema.TABLE_CONSTRAINTS k
    ON k.CONSTRAINT_SCHEMA = c.CONSTRAINT_SCHEMA AND k.CONSTRAINT_NAME = c.CONSTRAINT_NAME
 WHERE k.TABLE_SCHEMA = {d} AND k.TABLE_NAME = {t} AND k.CONSTRAINT_TYPE = 'CHECK'"
        )
    }
}

// -- the rows -----------------------------------------------------------------------------

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

/// `None` for the session's database, else the other one's name.
fn schema_of(db: &str, of: String) -> Option<String> {
    (!same(&of, db)).then_some(of)
}

fn is_target(target: &DependencyTarget, kind: &str, name: &str) -> bool {
    target.object.kind == kind && same(&target.object.name, name)
}

fn item(
    kind: &str,
    schema: Option<String>,
    name: String,
    relation: Relation,
    confidence: Confidence,
) -> Dependent {
    Dependent {
        kind: kind.into(),
        schema,
        name,
        parent: None,
        relation,
        confidence,
        detail: None,
        mentions: Vec::new(),
    }
}

/// `foreign_keys_sql`'s rows: the keys that point at the target table (not
/// its own, self-references aside), or for a column the keys that point at
/// it or that it holds.
fn foreign_keys<R: Cells>(rows: &[R], db: &str, target: &DependencyTarget) -> Vec<Dependent> {
    struct Fk {
        name: String,
        holder: (String, String),
        to: (String, String),
        columns: Vec<String>,
        ref_columns: Vec<String>,
    }
    let mut keys: Vec<Fk> = Vec::new();
    for r in rows {
        let (Some(name), Some(hs), Some(h), Some(col), Some(rs), Some(rt)) = (
            r.cell(0),
            r.cell(1),
            r.cell(2),
            r.cell(3),
            r.cell(4),
            r.cell(5),
        ) else {
            continue;
        };
        let ref_col = r.cell(6).unwrap_or_default();
        match keys
            .iter_mut()
            .find(|k| k.name == name && k.holder == (hs.clone(), h.clone()))
        {
            Some(k) => {
                k.columns.push(col);
                k.ref_columns.push(ref_col);
            }
            None => keys.push(Fk {
                name,
                holder: (hs, h),
                to: (rs, rt),
                columns: vec![col],
                ref_columns: vec![ref_col],
            }),
        }
    }
    let t = &target.object;
    let mut out = Vec::new();
    for k in keys {
        let points_here = same(&k.to.0, db) && same(&k.to.1, &t.name);
        let holds = same(&k.holder.0, db) && same(&k.holder.1, &t.name);
        let hit = match target.column.as_deref() {
            None => points_here && !holds,
            Some(c) => {
                (points_here && k.ref_columns.iter().any(|r| same(r, c)))
                    || (holds && k.columns.iter().any(|r| same(r, c)))
            }
        };
        if !hit {
            continue;
        }
        let to = if same(&k.to.0, &k.holder.0) {
            k.to.1.clone()
        } else {
            format!("{}.{}", k.to.0, k.to.1)
        };
        let mut d = item(
            kinds::TABLE,
            schema_of(db, k.holder.0),
            k.holder.1,
            Relation::ForeignKey,
            Confidence::Confirmed,
        );
        d.detail = Some(format!(
            "{} ({}) → {to} ({})",
            k.name,
            k.columns.join(", "),
            k.ref_columns.join(", ")
        ));
        out.push(d);
    }
    out
}

/// `indexes_sql`'s rows: the indexes (and the primary key) with the column.
fn index_holders<R: Cells>(rows: &[R], table: &str, column: &str) -> Vec<Dependent> {
    let mut indexes: Vec<(String, Vec<String>)> = Vec::new();
    for r in rows {
        let (Some(ix), Some(col)) = (r.cell(0), r.cell(1)) else {
            continue;
        };
        match indexes.iter_mut().find(|(n, _)| *n == ix) {
            Some((_, cols)) => cols.push(col),
            None => indexes.push((ix, vec![col])),
        }
    }
    indexes
        .into_iter()
        .filter(|(_, cols)| cols.iter().any(|c| same(c, column)))
        .map(|(ix, cols)| {
            let mut d = item(
                kinds::TABLE,
                None,
                table.to_string(),
                Relation::Index,
                Confidence::Confirmed,
            );
            d.detail = Some(format!("{ix} ({})", cols.join(", ")));
            d
        })
        .collect()
}

/// `checks_sql`'s rows whose expression names the column.
fn check_holders<R: Cells>(rows: &[R], table: &str, column: &str) -> Vec<Dependent> {
    rows.iter()
        .filter_map(|r| {
            let (name, clause) = (r.cell(0)?, r.cell(1)?);
            names_word(&clause, column).then(|| {
                let mut d = item(
                    kinds::TABLE,
                    None,
                    table.to_string(),
                    Relation::Check,
                    Confidence::Confirmed,
                );
                d.detail = Some(format!("{name}: {clause}"));
                d
            })
        })
        .collect()
}

/// `word` appears in `text` as a whole identifier (any case).
fn names_word(text: &str, word: &str) -> bool {
    let (text, word) = (text.to_lowercase(), word.to_lowercase());
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    text.match_indices(&word).any(|(i, m)| {
        !text[..i].chars().next_back().is_some_and(ident)
            && !text[i + m.len()..].chars().next().is_some_and(ident)
    })
}

/// A view's row: `(database, view, definition)`. `recorded` is whether the
/// catalog says the view reads the target (so the text only adds the
/// lines, and for a column decides if it's named).
fn views<R: Cells>(
    rows: &[R],
    db: &str,
    target: &DependencyTarget,
    scan: &DependencyScan,
    recorded: bool,
    report: &mut DependencyReport,
) -> Vec<Dependent> {
    let mut out = Vec::new();
    for r in rows {
        let (Some(vs), Some(name)) = (r.cell(0), r.cell(1)) else {
            continue;
        };
        if same(&vs, db) && is_target(target, kinds::VIEW, &name) {
            continue;
        }
        let body = r.cell(2).filter(|b| !b.trim().is_empty());
        if body.is_some() {
            report.scanned += 1;
        }
        // Another database's view reads the target only by a qualified name.
        let elsewhere = !same(&vs, db);
        let qualified = DependencyTarget {
            object: ObjectRef {
                schema: Some(db.to_string()),
                ..target.object.clone()
            },
            column: target.column.clone(),
        };
        let found = body
            .as_deref()
            .filter(|b| recorded || !elsewhere || names_word(b, db))
            .and_then(|b| {
                find_mentions(
                    b,
                    &scan.dialect,
                    if elsewhere { &qualified } else { target },
                )
            });
        let (confidence, mentions) = match (recorded, target.column.is_some(), found) {
            (_, true, Some((c, m))) => (c, m),
            (_, true, None) => {
                if body.is_none() && recorded {
                    report.unreadable.push(name);
                }
                continue;
            }
            (true, false, found) => (
                Confidence::Confirmed,
                found.map(|(_, m)| m).unwrap_or_default(),
            ),
            (false, false, Some((c, m))) => (c, m),
            (false, false, None) => continue,
        };
        let mut d = item(
            kinds::VIEW,
            schema_of(db, vs),
            name,
            Relation::Code,
            confidence,
        );
        d.mentions = mentions;
        out.push(d);
    }
    out
}

/// `routines_sql`'s rows: `(type, name, body)`.
fn routines<R: Cells>(
    rows: &[R],
    target: &DependencyTarget,
    scan: &DependencyScan,
    report: &mut DependencyReport,
) -> Vec<Dependent> {
    let mut out = Vec::new();
    for r in rows {
        let (Some(ty), Some(name)) = (r.cell(0), r.cell(1)) else {
            continue;
        };
        let kind = if ty.eq_ignore_ascii_case("PROCEDURE") {
            kinds::PROCEDURE
        } else {
            kinds::FUNCTION
        };
        if is_target(target, kind, &name) {
            continue;
        }
        let Some(body) = r.cell(2) else {
            report.unreadable.push(name);
            report.note.get_or_insert_with(|| NOTE_CODE_HIDDEN.into());
            continue;
        };
        report.scanned += 1;
        if let Some((confidence, mentions)) = find_mentions(&body, &scan.dialect, target) {
            let mut d = item(kind, None, name, Relation::Code, confidence);
            d.mentions = mentions;
            out.push(d);
        }
    }
    out
}

/// `triggers_sql`'s rows: `(trigger, table, body)`.
fn triggers<R: Cells>(
    rows: &[R],
    target: &DependencyTarget,
    scan: &DependencyScan,
    report: &mut DependencyReport,
) -> Vec<Dependent> {
    let mut out = Vec::new();
    for r in rows {
        let (Some(name), Some(body)) = (r.cell(0), r.cell(2)) else {
            continue;
        };
        if is_target(target, kinds::TRIGGER, &name) {
            continue;
        }
        report.scanned += 1;
        if let Some((confidence, mentions)) = find_mentions(&body, &scan.dialect, target) {
            let mut d = item(kinds::TRIGGER, None, name, Relation::Code, confidence);
            d.parent = r.cell(1);
            d.mentions = mentions;
            out.push(d);
        }
    }
    out
}

/// Confirmed first, then by relation and name (as the contract sorts).
fn sort(items: &mut [Dependent]) {
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
    use dbine_driver::sql::ScriptDialect;
    use dbine_driver::ObjectRef;

    struct Row(Vec<Option<&'static str>>);
    impl Cells for Row {
        fn cell(&self, i: usize) -> Option<String> {
            self.0.get(i).copied().flatten().map(str::to_string)
        }
    }
    fn row(cells: &[&'static str]) -> Row {
        Row(cells.iter().map(|c| Some(*c)).collect())
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
            dialect: ScriptDialect {
                backtick_idents: true,
                ..Default::default()
            },
            foreign_keys: true,
            written_kinds: Vec::new(),
            has_schemas: false,
            databases: None,
        }
    }

    #[test]
    fn every_query_is_filtered_by_the_target() {
        let t = target(kinds::TABLE, "clientes", None);
        for sql in [
            foreign_keys_sql("ventas", &t),
            view_usage_sql("ventas", &t),
            views_like_sql("ventas", "clientes"),
            routines_sql("ventas", "clientes"),
            triggers_sql("ventas", "clientes"),
        ] {
            assert!(
                sql.contains("'clientes'") || sql.contains("%clientes%"),
                "{sql}"
            );
            assert!(sql.contains("'ventas'"), "{sql}");
        }
        // A function is read from the routine usage; a table from the table's.
        assert!(
            view_usage_sql("d", &target(kinds::FUNCTION, "f", None)).contains("VIEW_ROUTINE_USAGE")
        );
        assert!(view_usage_sql("d", &t).contains("VIEW_TABLE_USAGE"));
        // The keys a table holds are read only for a column.
        assert!(!foreign_keys_sql("d", &t).contains("OR (TABLE_SCHEMA"));
        assert!(foreign_keys_sql("d", &target(kinds::TABLE, "t", Some("c")))
            .contains("OR (TABLE_SCHEMA"));
    }

    #[test]
    fn like_takes_wildcards_and_quotes_literally() {
        assert_eq!(like("c", "a_b%c'd"), "c LIKE '%a!_b!%c''d%' ESCAPE '!'");
        assert!(like("c", "x!y").contains("x!!y"));
    }

    #[test]
    fn a_table_is_pointed_at_by_other_tables_keys() {
        let rows = [
            row(&[
                "fk_pc",
                "ventas",
                "pedidos",
                "cliente_id",
                "ventas",
                "clientes",
                "id",
            ]),
            row(&["fk_lc", "otra", "lineas", "a", "ventas", "clientes", "id"]),
            row(&["fk_lc", "otra", "lineas", "b", "ventas", "clientes", "id2"]),
            // The table's own self-reference isn't a dependent.
            row(&[
                "fk_self", "ventas", "clientes", "padre", "ventas", "clientes", "id",
            ]),
        ];
        let out = foreign_keys(&rows, "ventas", &target(kinds::TABLE, "clientes", None));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "pedidos");
        assert_eq!(out[0].schema, None);
        assert_eq!(out[0].relation, Relation::ForeignKey);
        assert_eq!(out[0].confidence, Confidence::Confirmed);
        assert_eq!(
            out[0].detail.as_deref(),
            Some("fk_pc (cliente_id) → clientes (id)")
        );
        assert_eq!(out[1].schema.as_deref(), Some("otra"));
        assert_eq!(
            out[1].detail.as_deref(),
            Some("fk_lc (a, b) → ventas.clientes (id, id2)")
        );
    }

    #[test]
    fn a_column_is_in_the_keys_that_point_at_it_and_the_ones_it_holds() {
        let rows = [
            row(&[
                "fk_pc",
                "ventas",
                "pedidos",
                "cliente_id",
                "ventas",
                "clientes",
                "id",
            ]),
            row(&[
                "fk_x", "ventas", "pedidos", "otro", "ventas", "clientes", "codigo",
            ]),
            row(&[
                "fk_up", "ventas", "clientes", "padre", "ventas", "grupos", "id",
            ]),
        ];
        let by = |c: &str| {
            foreign_keys(&rows, "ventas", &target(kinds::TABLE, "clientes", Some(c)))
                .into_iter()
                .map(|d| d.detail.unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(by("id"), ["fk_pc (cliente_id) → clientes (id)"]);
        assert_eq!(by("padre"), ["fk_up (padre) → grupos (id)"]);
        assert!(by("nombre").is_empty());
    }

    #[test]
    fn indexes_and_checks_of_a_column() {
        let ix = [
            row(&["PRIMARY", "id"]),
            row(&["ix_nom", "nombre"]),
            row(&["ix_nom", "id"]),
            row(&["ix_otro", "otro"]),
        ];
        let out = index_holders(&ix, "clientes", "ID");
        let detail: Vec<_> = out.iter().map(|d| d.detail.clone().unwrap()).collect();
        assert_eq!(detail, ["PRIMARY (id)", "ix_nom (nombre, id)"]);
        assert!(out
            .iter()
            .all(|d| d.relation == Relation::Index && d.name == "clientes"));

        let ck = [
            row(&["ck_id", "(`id` > 0)"]),
            row(&["ck_otro", "(`identidad` > 0)"]),
        ];
        let out = check_holders(&ck, "clientes", "id");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].relation, Relation::Check);
        assert_eq!(out[0].detail.as_deref(), Some("ck_id: (`id` > 0)"));
    }

    #[test]
    fn a_recorded_view_is_confirmed_with_its_lines() {
        let rows = [
            row(&[
                "ventas",
                "v_pedidos",
                "select `p`.`id` AS `id` from (`ventas`.`pedidos` `p` join `ventas`.`clientes` `c`)",
            ]),
            // The login can't see the definition: still recorded.
            Row(vec![Some("ventas"), Some("v_ocultas"), Some("")]),
            Row(vec![Some("otra"), Some("v_lejos"), None]),
        ];
        let mut report = DependencyReport::default();
        let t = target(kinds::TABLE, "clientes", None);
        let out = views(&rows, "ventas", &t, &scan(), true, &mut report);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|d| d.confidence == Confidence::Confirmed));
        assert!(!out[0].mentions.is_empty());
        assert!(out[1].mentions.is_empty());
        assert_eq!(out[2].schema.as_deref(), Some("otra"));
        assert_eq!(report.scanned, 1);
    }

    #[test]
    fn a_column_needs_the_view_text_to_name_it() {
        let rows = [
            row(&["ventas", "v_uno", "select `c`.`nombre` from `clientes` `c`"]),
            row(&["ventas", "v_dos", "select `c`.`id` from `clientes` `c`"]),
            Row(vec![Some("ventas"), Some("v_ciega"), None]),
        ];
        let mut report = DependencyReport::default();
        let t = target(kinds::TABLE, "clientes", Some("nombre"));
        let out = views(&rows, "ventas", &t, &scan(), true, &mut report);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "v_uno");
        assert_eq!(out[0].confidence, Confidence::Probable);
        assert_eq!(report.unreadable, ["v_ciega"]);
    }

    #[test]
    fn without_a_catalog_the_text_decides() {
        let rows = [
            row(&["ventas", "v_si", "select * from `clientes`"]),
            // `LIKE` also matches a longer name.
            row(&["ventas", "v_no", "select * from `clientes_viejos`"]),
            // Another database's: only by a qualified name.
            row(&["otra", "v_lejos", "select * from `ventas`.`clientes`"]),
            row(&["otra", "v_propia", "select * from `clientes`"]),
        ];
        let mut report = DependencyReport::default();
        let t = target(kinds::TABLE, "clientes", None);
        let out = views(&rows, "ventas", &t, &scan(), false, &mut report);
        let by: Vec<_> = out
            .iter()
            .map(|d| (d.schema.as_deref(), d.name.as_str()))
            .collect();
        assert_eq!(by, [(None, "v_si"), (Some("otra"), "v_lejos")]);
        assert_eq!(out[0].confidence, Confidence::Probable);
        assert_eq!(report.scanned, 4);
    }

    #[test]
    fn routines_and_triggers_are_confirmed_in_code() {
        let t = target(kinds::TABLE, "clientes", None);
        let mut report = DependencyReport::default();
        let rs = [
            row(&[
                "PROCEDURE",
                "p_alta",
                "BEGIN INSERT INTO clientes (id) VALUES (1); END",
            ]),
            row(&["FUNCTION", "f_str", "RETURN 'clientes'"]),
            row(&[
                "FUNCTION",
                "f_viejo",
                "RETURN (SELECT 1 FROM clientes_viejos)",
            ]),
            Row(vec![Some("PROCEDURE"), Some("p_oculto"), None]),
        ];
        let out = routines(&rs, &t, &scan(), &mut report);
        let by: Vec<_> = out
            .iter()
            .map(|d| (d.name.as_str(), d.kind.as_str(), d.confidence))
            .collect();
        assert_eq!(
            by,
            [
                ("p_alta", kinds::PROCEDURE, Confidence::Probable),
                ("f_str", kinds::FUNCTION, Confidence::Review),
            ]
        );
        assert_eq!(report.unreadable, ["p_oculto"]);
        assert_eq!(report.note.as_deref(), Some(NOTE_CODE_HIDDEN));

        let ts = [row(&[
            "tg_c",
            "pedidos",
            "INSERT INTO bitacora SELECT id FROM clientes",
        ])];
        let out = triggers(&ts, &t, &scan(), &mut report);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, kinds::TRIGGER);
        assert_eq!(out[0].parent.as_deref(), Some("pedidos"));
    }

    #[test]
    fn the_target_is_not_its_own_dependent() {
        let mut report = DependencyReport::default();
        let t = target(kinds::PROCEDURE, "p_rec", None);
        let rs = [row(&["PROCEDURE", "p_rec", "CALL p_rec()"])];
        assert!(routines(&rs, &t, &scan(), &mut report).is_empty());
        // A function of the same name is another object.
        let rs = [row(&["FUNCTION", "p_rec", "RETURN p_rec(1)"])];
        assert_eq!(routines(&rs, &t, &scan(), &mut report).len(), 1);
    }

    #[test]
    fn a_word_is_a_whole_identifier() {
        assert!(names_word("(`id` > 0)", "ID"));
        assert!(!names_word("(`identidad` > 0)", "id"));
        assert!(names_word("a.id=1", "id"));
    }
}
