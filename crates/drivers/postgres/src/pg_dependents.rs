//! What depends on an object (`Session::dependents`), from PostgreSQL's
//! catalog: the same sources as [`crate::pg_references`] read the other way,
//! filtered by the object they point at, in two queries instead of reading
//! every definition of the database.
//!
//! - `pg_depend` rows whose referenced object is the target (for a column,
//!   the rows naming that column): the rewrite rules of views, materialized
//!   views and rules, routines (PostgreSQL 14+ records what a SQL-standard
//!   body uses; any body, its signature's types), triggers, column defaults
//!   and generated columns, checks, unique and exclusion constraints,
//!   indexes, row-level security policies. A routine that takes or returns
//!   a table's row type depends on the type, not the table: the type is
//!   followed to its table. Confirmed.
//! - Foreign keys, from `pg_constraint` (`confrelid`, `confkey`), and the
//!   tables that inherit the target (`pg_inherits`). Confirmed.
//! - The source of a routine whose body the catalog doesn't track (PL/pgSQL,
//!   a string body): one query that keeps, on the server, only the bodies
//!   whose text holds the name; the generic parser reads those (Probable or
//!   Review, with the lines). A routine the catalog already returned keeps
//!   Confirmed and gains the lines.
//!
//! Only the variants that keep PostgreSQL's catalog
//! ([`crate::references::trusts_catalog`]) read it; the rest, and the kinds
//! the catalog doesn't answer (a schema, an index, a constraint, a type…),
//! run the generic scan. So does any catalog read that fails, with a note
//! saying why.

use crate::catalog::{cell, lit};
use crate::lineage::default_text;
use crate::pg_references::{routine_target, Catalog};
use crate::references::trusts_catalog;
use crate::session::PgSession;
use dbine_driver::dependencies::{
    find_mentions, scan, Confidence, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    Relation,
};
use dbine_driver::{kinds, Error, Result};
use tokio_postgres::SimpleQueryRow;

/// Whether the catalog answers for this kind of target.
fn answers(kind: &str) -> bool {
    matches!(
        kind,
        kinds::TABLE
            | kinds::VIEW
            | kinds::MATERIALIZED_VIEW
            | kinds::SEQUENCE
            | kinds::FUNCTION
            | kinds::PROCEDURE
    )
}

/// The catalog query for what depends on `kind` `schema`.`name` (and its
/// `column`), all literals: `kind`, `sch`, `name`, `parent` (a trigger's
/// table), `rel` (`code`, `check`, `index` or `fk`) and `detail`. `None`
/// for a kind it doesn't read. `user_schema(col)` keeps the server's
/// schemas out.
pub(crate) fn dependents_sql(
    c: Catalog,
    kind: &str,
    schema: &str,
    name: &str,
    column: Option<&str>,
    user_schema: impl Fn(&str) -> String,
) -> Option<String> {
    if !answers(kind) {
        return None;
    }
    let routine = matches!(kind, kinds::FUNCTION | kinds::PROCEDURE);
    let (s, n) = (schema, name);
    let relation = |relkinds: &str| {
        format!(
            "SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relname = {n} AND c.relkind IN ({relkinds})
               AND (n.nspname = {s} OR ({s} = '' AND pg_table_is_visible(c.oid)))"
        )
    };
    let (target, refclass) = match kind {
        kinds::TABLE => (relation("'r', 'p', 'f'"), "pg_class"),
        kinds::VIEW => (relation("'v'"), "pg_class"),
        kinds::MATERIALIZED_VIEW => (relation("'m'"), "pg_class"),
        kinds::SEQUENCE => (relation("'S'"), "pg_class"),
        _ => (routine_target(c, kind, s, n), "pg_proc"),
    };
    let column = column.filter(|_| !routine);
    // A column narrows the rows to the ones that name it.
    let (col_cte, col_filter) = match column {
        Some(col) => (
            format!(
                ",
         col AS (SELECT a.attnum FROM pg_attribute a JOIN tgt ON a.attrelid = tgt.oid
                  WHERE a.attname = {col} AND NOT a.attisdropped)"
            ),
            " AND d.refobjsubid IN (SELECT attnum FROM col)",
        ),
        None => (String::new(), ""),
    };
    // A routine that takes or returns the table's row type depends on the type.
    let row_type = if refclass == "pg_class" && column.is_none() {
        "
          UNION
          SELECT d.classid, d.objid FROM pg_depend d
            JOIN pg_type ty ON d.refclassid = 'pg_type'::regclass AND d.refobjid = ty.oid
            JOIN tgt ON ty.typrelid = tgt.oid
           WHERE d.deptype IN ('n', 'a')"
    } else {
        ""
    };
    let ns = user_schema("n.nspname");
    let default = default_text(c);
    let ext = "NOT EXISTS (SELECT 1 FROM pg_depend e
                              WHERE e.classid = 'pg_proc'::regclass AND e.objid = p.oid AND e.deptype = 'e')";
    let own_proc = if routine {
        " AND p.oid NOT IN (SELECT oid FROM tgt)"
    } else {
        ""
    };
    let cols_of = |tbl: &str, arr: &str| {
        format!(
            "(SELECT string_agg(a.attname::text, ', ' ORDER BY u.ord)
                FROM unnest({arr}) WITH ORDINALITY u(attnum, ord)
                JOIN pg_attribute a ON a.attrelid = {tbl} AND a.attnum = u.attnum)"
        )
    };
    let key_cols = cols_of("k.conrelid", "k.conkey");
    let index_keys = if c.version >= 110000 {
        "indnkeyatts"
    } else {
        "indnatts"
    };
    let mut branches = vec![
        // Views, materialized views and rules.
        format!(
            "SELECT CASE v.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized_view' ELSE 'table' END AS kind,
                    n.nspname::text AS sch, v.relname::text AS name, NULL::text AS parent, 'code'::text AS rel,
                    CASE WHEN v.relkind IN ('v', 'm') THEN NULL ELSE 'RULE ' || w.rulename::text END AS detail,
                    v.oid AS hold
               FROM refs r JOIN pg_rewrite w ON r.classid = 'pg_rewrite'::regclass AND w.oid = r.objid
               JOIN pg_class v ON v.oid = w.ev_class JOIN pg_namespace n ON n.oid = v.relnamespace
              WHERE {ns}"
        ),
        // Routines.
        format!(
            "SELECT {}, n.nspname::text, p.proname::text, NULL::text, 'code', NULL::text, NULL::oid
               FROM refs r JOIN pg_proc p ON r.classid = 'pg_proc'::regclass AND p.oid = r.objid
               JOIN pg_namespace n ON n.oid = p.pronamespace
              WHERE {ns} AND {ext}{own_proc}",
            c.routine_kind()
        ),
        // Triggers (the target's own too: they are written on it).
        format!(
            "SELECT 'trigger', n.nspname::text, g.tgname::text, tc.relname::text, 'code', NULL::text, NULL::oid
               FROM refs r JOIN pg_trigger g ON r.classid = 'pg_trigger'::regclass AND g.oid = r.objid AND NOT g.tgisinternal
               JOIN pg_class tc ON tc.oid = g.tgrelid JOIN pg_namespace n ON n.oid = tc.relnamespace
              WHERE {ns}"
        ),
        // Defaults and generated columns.
        format!(
            "SELECT 'table', n.nspname::text, tc.relname::text, NULL::text, 'code', a.attname::text || {default}, tc.oid
               FROM refs r JOIN pg_attrdef d ON r.classid = 'pg_attrdef'::regclass AND d.oid = r.objid
               JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum
               JOIN pg_class tc ON tc.oid = d.adrelid JOIN pg_namespace n ON n.oid = tc.relnamespace
              WHERE {ns}"
        ),
        // Checks, primary keys, unique and exclusion constraints.
        format!(
            "SELECT 'table', n.nspname::text, tc.relname::text, NULL::text,
                    CASE k.contype WHEN 'c' THEN 'check' ELSE 'index' END,
                    CASE k.contype WHEN 'c' THEN k.conname::text || ': ' || pg_get_constraintdef(k.oid)
                         ELSE k.conname::text || ' (' || {key_cols} || ')' END, tc.oid
               FROM refs r JOIN pg_constraint k ON r.classid = 'pg_constraint'::regclass AND k.oid = r.objid
                AND k.conrelid <> 0 AND k.contype IN ('c', 'p', 'u', 'x')
               JOIN pg_class tc ON tc.oid = k.conrelid JOIN pg_namespace n ON n.oid = tc.relnamespace
              WHERE {ns}"
        ),
        // Indexes.
        format!(
            "SELECT 'table', n.nspname::text, tc.relname::text, NULL::text, 'index',
                    i.relname::text || ' (' || (SELECT string_agg(pg_get_indexdef(x.indexrelid, k, true), ', ' ORDER BY k)
                                                  FROM generate_series(1, x.{index_keys}) k) || ')', tc.oid
               FROM refs r JOIN pg_class i ON r.classid = 'pg_class'::regclass AND i.oid = r.objid AND i.relkind IN ('i', 'I')
               JOIN pg_index x ON x.indexrelid = i.oid
               JOIN pg_class tc ON tc.oid = x.indrelid JOIN pg_namespace n ON n.oid = tc.relnamespace
              WHERE {ns}"
        ),
    ];
    if c.policies() {
        branches.push(format!(
            "SELECT 'table', n.nspname::text, tc.relname::text, NULL::text, 'code', 'POLICY ' || p.polname::text, tc.oid
               FROM refs r JOIN pg_policy p ON r.classid = 'pg_policy'::regclass AND p.oid = r.objid
               JOIN pg_class tc ON tc.oid = p.polrelid JOIN pg_namespace n ON n.oid = tc.relnamespace
              WHERE {ns}"
        ));
    }
    if kind == kinds::TABLE {
        // Foreign keys: those that point at the table (or at the column) and,
        // for a column, the table's own.
        let part = if c.version >= 110000 {
            " AND k.conparentid = 0"
        } else {
            ""
        };
        let (join, hit) = match column {
            None => (
                "k.confrelid = tgt.oid",
                "k.conrelid <> tgt.oid".to_string(),
            ),
            Some(_) => (
                "k.confrelid = tgt.oid OR k.conrelid = tgt.oid",
                "((k.confrelid = tgt.oid AND EXISTS (SELECT 1 FROM col WHERE col.attnum = ANY (k.confkey)))
                   OR (k.conrelid = tgt.oid AND EXISTS (SELECT 1 FROM col WHERE col.attnum = ANY (k.conkey))))"
                    .to_string(),
            ),
        };
        let ref_cols = cols_of("k.confrelid", "k.confkey");
        branches.push(format!(
            "SELECT 'table', n.nspname::text, tc.relname::text, NULL::text, 'fk',
                    k.conname::text || ' (' || {key_cols} || ') → ' || rn.nspname::text || '.' || rc.relname::text
                      || ' (' || {ref_cols} || ')', tc.oid
               FROM pg_constraint k JOIN tgt ON {join}
               JOIN pg_class tc ON tc.oid = k.conrelid JOIN pg_namespace n ON n.oid = tc.relnamespace
               JOIN pg_class rc ON rc.oid = k.confrelid JOIN pg_namespace rn ON rn.oid = rc.relnamespace
              WHERE k.contype = 'f' AND {hit}{part} AND {ns}"
        ));
        if c.full && column.is_none() {
            let how = if c.version >= 100000 {
                "CASE WHEN tc.relispartition THEN 'PARTITION OF' ELSE 'INHERITS' END"
            } else {
                "'INHERITS'"
            };
            branches.push(format!(
                "SELECT 'table', n.nspname::text, tc.relname::text, NULL::text, 'code', {how}, tc.oid
                   FROM pg_inherits i JOIN tgt ON i.inhparent = tgt.oid
                   JOIN pg_class tc ON tc.oid = i.inhrelid JOIN pg_namespace n ON n.oid = tc.relnamespace
                  WHERE {ns}"
            ));
        }
    }
    // What belongs to the target itself (its indexes, checks, defaults…) only
    // counts for a column; the target is never its own dependent.
    let own = if routine || column.is_some() {
        "TRUE"
    } else {
        "(u.hold IS NULL OR u.hold NOT IN (SELECT oid FROM tgt))"
    };
    Some(format!(
        "WITH tgt AS ({target}){col_cte},
         refs AS (SELECT d.classid, d.objid FROM pg_depend d
                    JOIN tgt ON d.refclassid = '{refclass}'::regclass AND d.refobjid = tgt.oid
                   WHERE d.deptype IN ('n', 'a'){col_filter}{row_type})
         SELECT DISTINCT u.kind, u.sch, u.name, u.parent, u.rel, u.detail FROM (
         {}
         ) u WHERE {own}
         ORDER BY 2, 3, 1",
        branches.join("\n         UNION ALL\n         ")
    ))
}

/// The routines whose body the catalog doesn't track and whose text holds
/// `pattern` (a literal; matched without regard to case): `kind`, `sch`,
/// `name`, `body`. The server keeps only those, so the others are never
/// read. `target` (kind, schema and name as literals) is the routine being
/// asked about, left out.
pub(crate) fn bodies_sql(
    c: Catalog,
    target: Option<(&str, &str, &str)>,
    pattern: &str,
    user_schema: impl Fn(&str) -> String,
) -> String {
    let filter = if c.prokind() {
        "p.prokind IN ('f', 'p')"
    } else {
        "NOT p.proisagg AND NOT p.proiswindow"
    };
    let tracked = if c.version >= 140000 {
        " AND p.prosqlbody IS NULL"
    } else {
        ""
    };
    let (cte, own) = match target {
        Some((kind, s, n)) => (
            format!("WITH tgt AS ({}) ", routine_target(c, kind, s, n)),
            " AND p.oid NOT IN (SELECT oid FROM tgt)",
        ),
        None => (String::new(), ""),
    };
    format!(
        "{cte}SELECT {routine_kind} AS kind, n.nspname::text AS sch, p.proname::text AS name,
                pg_get_functiondef(p.oid) AS body
           FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
           JOIN pg_language l ON l.oid = p.prolang
          WHERE {filter} AND {ns} AND l.lanname NOT IN ('c', 'internal'){tracked}{own}
            AND position(lower({pattern}) in lower(p.prosrc)) > 0
            AND NOT EXISTS (SELECT 1 FROM pg_depend e
                             WHERE e.classid = 'pg_proc'::regclass AND e.objid = p.oid AND e.deptype = 'e')
          ORDER BY 2, 3, 1",
        routine_kind = c.routine_kind(),
        ns = user_schema("n.nspname"),
    )
}

/// A row of [`dependents_sql`].
fn dependent(r: &SimpleQueryRow) -> Option<Dependent> {
    Some(Dependent {
        kind: cell(r, "kind")?,
        schema: cell(r, "sch").filter(|s| !s.is_empty()),
        name: cell(r, "name")?,
        parent: cell(r, "parent").filter(|s| !s.is_empty()),
        relation: match cell(r, "rel").as_deref() {
            Some("check") => Relation::Check,
            Some("index") => Relation::Index,
            Some("fk") => Relation::ForeignKey,
            _ => Relation::Code,
        },
        confidence: Confidence::Confirmed,
        detail: cell(r, "detail").filter(|s| !s.is_empty()),
        mentions: Vec::new(),
    })
}

/// A routine's source as [`bodies_sql`] returns it: kind, schema, name, body.
pub(crate) type Body = (String, Option<String>, String, String);

/// Adds what the text of the untracked routines says: a routine the catalog
/// already found keeps Confirmed and gains the lines; any other that names
/// the target is added with the parser's confidence. Returns the bodies read.
pub(crate) fn add_bodies(
    items: &mut Vec<Dependent>,
    bodies: &[Body],
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> u32 {
    let mut read = 0;
    for (kind, schema, name, body) in bodies {
        read += 1;
        let Some((confidence, mentions)) = find_mentions(body, &scan.dialect, target) else {
            continue;
        };
        if let Some(d) = items
            .iter_mut()
            .find(|d| d.kind == *kind && d.schema == *schema && d.name == *name)
        {
            d.mentions = mentions;
            continue;
        }
        items.push(Dependent {
            kind: kind.clone(),
            schema: schema.clone(),
            name: name.clone(),
            parent: None,
            relation: Relation::Code,
            confidence,
            detail: None,
            mentions,
        });
    }
    read
}

/// The note when the generic scan answers instead of the catalog: the
/// server's first line.
fn unread_note(e: &Error) -> String {
    let (view, msg) = ("pg_depend", e.to_string());
    format!(
        "No se pudo leer {view}: {}",
        msg.lines().next().unwrap_or_default().trim()
    )
}

/// Confirmed first, then foreign keys, indexes, checks and code, each by name.
fn sort(items: &mut [Dependent]) {
    items.sort_by_key(|d| {
        (
            d.confidence,
            d.relation,
            d.schema.clone(),
            d.name.to_lowercase(),
        )
    });
}

impl PgSession {
    pub(crate) async fn dependents_impl(
        &mut self,
        target: &DependencyTarget,
        ctx: &DependencyScan,
    ) -> Result<DependencyReport> {
        let v = self.variant;
        let t = &target.object;
        if !trusts_catalog(v) || !answers(&t.kind) {
            return scan(self, target, ctx).await;
        }
        let c = self.pg_catalog();
        let (s, n) = (lit(v, t.schema().unwrap_or("")), lit(v, &t.name));
        let col = target.column.as_deref().map(|x| lit(v, x));
        let pattern = lit(v, target.column.as_deref().unwrap_or(&t.name));
        let routine = matches!(t.kind.as_str(), kinds::FUNCTION | kinds::PROCEDURE).then_some((
            t.kind.as_str(),
            s.as_str(),
            n.as_str(),
        ));
        let Some(deps) = dependents_sql(c, &t.kind, &s, &n, col.as_deref(), |x| self.filter(x))
        else {
            return scan(self, target, ctx).await;
        };
        let bodies = bodies_sql(c, routine, &pattern, |x| self.filter(x));
        let read = async {
            let items = self.text(&deps).await?;
            let bodies = self.text(&bodies).await?;
            Result::Ok((items, bodies))
        }
        .await;
        let (items, bodies) = match read {
            Ok(r) => r,
            Err(e @ (Error::Query(_) | Error::Statement(_) | Error::Unsupported(_))) => {
                tracing::debug!("{v:?}: dependents catalog unavailable: {e}");
                let mut r = scan(self, target, ctx).await?;
                r.note = Some(unread_note(&e));
                return Ok(r);
            }
            Err(e) => return Err(e),
        };
        let mut report = DependencyReport {
            items: items.iter().filter_map(dependent).collect(),
            ..Default::default()
        };
        let bodies: Vec<Body> = bodies
            .iter()
            .filter_map(|r| {
                Some((
                    cell(r, "kind")?,
                    cell(r, "sch").filter(|s| !s.is_empty()),
                    cell(r, "name")?,
                    cell(r, "body")?,
                ))
            })
            .collect();
        report.scanned = add_bodies(&mut report.items, &bodies, target, ctx);
        sort(&mut report.items);
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ObjectRef, ScriptDialect};

    fn cat(version: i32) -> Catalog {
        Catalog {
            version,
            full: true,
        }
    }

    fn keep(col: &str) -> String {
        format!("{col} NOT IN ('pg_catalog', 'information_schema')")
    }

    fn target(kind: &str, name: &str, column: Option<&str>) -> DependencyTarget {
        DependencyTarget {
            object: ObjectRef {
                kind: kind.into(),
                schema: Some("ventas".into()),
                name: name.into(),
            },
            column: column.map(Into::into),
        }
    }

    fn sql(kind: &str, column: Option<&str>) -> String {
        dependents_sql(cat(160000), kind, "'ventas'", "'clientes'", column, keep).unwrap()
    }

    #[test]
    fn only_the_kinds_the_catalog_records_are_read() {
        for k in [
            "table",
            "view",
            "materialized_view",
            "sequence",
            "function",
            "procedure",
        ] {
            assert!(
                dependents_sql(cat(160000), k, "''", "'x'", None, keep).is_some(),
                "{k}"
            );
        }
        for k in [
            "schema",
            "index",
            "constraint",
            "trigger",
            "type",
            "synonym",
        ] {
            assert!(
                dependents_sql(cat(160000), k, "''", "'x'", None, keep).is_none(),
                "{k}"
            );
        }
    }

    #[test]
    fn a_table_is_asked_for_by_what_points_at_it_not_by_scanning() {
        let q = sql("table", None);
        // Filtered by the referenced object, never by reading definitions.
        assert!(q.contains("d.refobjid = tgt.oid"));
        assert!(q.contains("k.confrelid = tgt.oid"));
        assert!(q.contains("pg_inherits"));
        assert!(!q.contains("pg_get_viewdef") && !q.contains("pg_get_functiondef"));
        // Its own indexes and defaults aren't its dependents; a column's are.
        assert!(q.contains("u.hold NOT IN (SELECT oid FROM tgt)"));
        // Row type: a routine that takes the row depends on the type.
        assert!(q.contains("pg_type"));
    }

    #[test]
    fn a_column_narrows_to_its_own_rows() {
        let q = sql("table", Some("'id'"));
        assert!(q.contains("d.refobjsubid IN (SELECT attnum FROM col)"));
        assert!(q.contains("a.attname = 'id'"));
        assert!(q.contains("ANY (k.confkey)") && q.contains("ANY (k.conkey)"));
        assert!(q.contains(") u WHERE TRUE"));
        assert!(!q.contains("pg_inherits") && !q.contains("pg_type"));
    }

    #[test]
    fn a_routine_is_read_through_pg_proc_and_leaves_itself_out() {
        let q = sql("function", None);
        assert!(q.contains("'pg_proc'::regclass AND d.refobjid = tgt.oid"));
        assert!(q.contains("p.oid NOT IN (SELECT oid FROM tgt)"));
        assert!(!q.contains("pg_inherits"));
        // A column means nothing to a routine.
        assert_eq!(q, sql("function", Some("'x'")));
    }

    #[test]
    fn older_servers_lack_what_came_later() {
        let q = dependents_sql(cat(90200), "table", "''", "'t'", None, keep).unwrap();
        assert!(!q.contains("pg_policy") && !q.contains("conparentid"));
        assert!(q.contains("indnatts") && !q.contains("indnkeyatts"));
        assert!(q.contains("'INHERITS'") && !q.contains("relispartition"));
        let q = sql("table", None);
        assert!(q.contains("pg_policy") && q.contains("conparentid = 0"));
    }

    #[test]
    fn untracked_bodies_are_filtered_by_the_server() {
        let q = bodies_sql(cat(160000), None, "'clientes'", keep);
        assert!(q.contains("position(lower('clientes') in lower(p.prosrc)) > 0"));
        assert!(q.contains("p.prosqlbody IS NULL"));
        assert!(!q.contains("WITH tgt"));
        let q = bodies_sql(cat(130000), Some(("function", "''", "'f'")), "'f'", keep);
        assert!(q.contains("WITH tgt AS") && q.contains("p.oid NOT IN (SELECT oid FROM tgt)"));
        assert!(!q.contains("prosqlbody"));
    }

    fn scan_ctx() -> DependencyScan {
        DependencyScan {
            source_kinds: vec![],
            dialect: ScriptDialect::default(),
            foreign_keys: true,
            written_kinds: vec![],
            has_schemas: true,
            databases: None,
        }
    }

    fn found(kind: &str, name: &str) -> Dependent {
        Dependent {
            kind: kind.into(),
            schema: Some("ventas".into()),
            name: name.into(),
            parent: None,
            relation: Relation::Code,
            confidence: Confidence::Confirmed,
            detail: None,
            mentions: Vec::new(),
        }
    }

    #[test]
    fn the_text_adds_routines_the_catalog_cannot_see_and_keeps_confirmed_ones() {
        let t = target("table", "clientes", None);
        let mut items = vec![found("function", "con_tipo")];
        let bodies: Vec<Body> = vec![
            (
                "function".into(),
                Some("ventas".into()),
                "con_tipo".into(),
                "BEGIN\n  SELECT 1 FROM ventas.clientes;\nEND".into(),
            ),
            (
                "procedure".into(),
                Some("ventas".into()),
                "plpgsql".into(),
                "BEGIN\n  UPDATE ventas.clientes SET x = 1;\nEND".into(),
            ),
            (
                "function".into(),
                Some("ventas".into()),
                "otra".into(),
                "BEGIN\n  EXECUTE 'select * from ventas.clientes';\nEND".into(),
            ),
            (
                "function".into(),
                Some("ventas".into()),
                "nada".into(),
                "BEGIN\n  RETURN 1;\nEND".into(),
            ),
        ];
        let read = add_bodies(&mut items, &bodies, &t, &scan_ctx());
        assert_eq!(read, 4);
        assert_eq!(items.len(), 3, "{items:?}");
        let by = |n: &str| items.iter().find(|d| d.name == n).unwrap();
        // The catalog's stays Confirmed and gains the lines.
        assert_eq!(by("con_tipo").confidence, Confidence::Confirmed);
        assert!(!by("con_tipo").mentions.is_empty());
        assert_eq!(by("plpgsql").confidence, Confidence::Probable);
        assert_eq!(by("otra").confidence, Confidence::Review);
        assert!(items.iter().all(|d| d.name != "nada"));
    }

    #[test]
    fn a_failed_read_says_what_the_server_said() {
        let e = Error::Query("ERROR: permission denied for table pg_depend\nDetalle: x".into());
        assert_eq!(
            unread_note(&e),
            "No se pudo leer pg_depend: ERROR: permission denied for table pg_depend"
        );
    }

    #[test]
    fn kinds_the_catalog_answers_run_it_only_on_the_variants_that_keep_it() {
        use crate::Variant;
        assert!(trusts_catalog(Variant::Postgres) && answers("view"));
        assert!(!trusts_catalog(Variant::Redshift));
        assert!(!trusts_catalog(Variant::Cockroach));
    }
}
