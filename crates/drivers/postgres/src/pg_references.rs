//! What an object uses, from PostgreSQL's catalog (`Session::references`).
//! Shared with the Aurora DSQL driver, which keeps the same catalog.
//!
//! - Views and materialized views: `pg_depend` from the view's rewrite rule
//!   (`pg_rewrite.ev_class`): the tables, views, sequences and functions its
//!   query reads.
//! - Functions and procedures: `pg_depend` from the routine. PostgreSQL 14+
//!   records what a SQL-standard body (`BEGIN ATOMIC`, `RETURN …`) uses;
//!   other bodies (PL/pgSQL, a string) only record their signature's types,
//!   so a table whose row type the routine takes or returns counts, and the
//!   driver adds what the text of the body names.
//! - Triggers: their table (`tgrelid`), their function (`tgfoid`) and what
//!   their `WHEN` calls.
//! - Tables: the sequences and functions their defaults and generated
//!   columns use (`pg_attrdef`), the functions of their checks and index
//!   expressions, what their row-level security policies read, the parent
//!   of a partition or an inherited table (`pg_inherits`), the sequences
//!   they own (`OWNED BY`) and, from another query, their foreign keys'
//!   tables. A foreign table's remote source, from `pg_foreign_table` and
//!   its server, is [`foreign_source`].
//!
//! Every row is the catalog's: confirmed. Objects of the server, of
//! extensions (their functions) and the target itself are left out.

use dbine_driver::dependencies::{
    find_mentions, Confidence, DependencyReport, DependencyTarget, Dependent, ExternalRef, Relation,
};
use dbine_driver::sql::ScriptDialect;
use dbine_driver::{kinds, Error, ObjectRef};

/// What the server's catalog has.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Catalog {
    /// `server_version_num`.
    pub version: i32,
    /// `pg_policy`, `pg_inherits`, `pg_foreign_table` and generated columns
    /// can be asked about (not on DSQL or Redshift).
    pub full: bool,
}

impl Catalog {
    fn policies(self) -> bool {
        self.full && self.version >= 90500
    }

    fn foreign_tables(self) -> bool {
        self.full && self.version >= 90100
    }

    fn generated(self) -> bool {
        self.full && self.version >= 120000
    }

    fn prokind(self) -> bool {
        self.version >= 110000
    }

    /// `p.prokind` as the explorer's kind.
    fn routine_kind(self) -> &'static str {
        if self.prokind() {
            "CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END"
        } else {
            "'function'"
        }
    }
}

/// The target's routines (every overload of the name), as `tgt`. `schema`
/// and `name` are literals; an empty schema is whatever the search path
/// finds.
fn routine_target(c: Catalog, kind: &str, schema: &str, name: &str) -> String {
    let prokind = match (c.prokind(), kind) {
        (true, kinds::PROCEDURE) => " AND p.prokind = 'p'",
        (true, _) => " AND p.prokind <> 'p'",
        _ => "",
    };
    format!(
        "SELECT p.oid FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
         WHERE p.proname = {name}{prokind}
           AND (n.nspname = {schema} OR ({schema} = '' AND pg_function_is_visible(p.oid)))"
    )
}

/// The catalog query for what `kind` `schema`.`name` uses (both literals):
/// `kind`, `sch`, `name`, `rel` (`code`, `check` or `index`) and `detail`
/// (the default, check or index that uses it, `PARTITION OF`…). `None` for
/// a kind it doesn't read. `user_schema(col)` is the filter that keeps the
/// server's schemas out.
pub(crate) fn uses_sql(
    c: Catalog,
    kind: &str,
    schema: &str,
    name: &str,
    user_schema: impl Fn(&str) -> String,
) -> Option<String> {
    let (s, n) = (schema, name);
    let relation = |relkinds: &str| {
        format!(
            "SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relname = {n} AND c.relkind IN ({relkinds})
               AND (n.nspname = {s} OR ({s} = '' AND pg_table_is_visible(c.oid)))"
        )
    };
    const RULE: &str =
        "SELECT 'pg_rewrite'::regclass AS cls, r.oid AS id, 'code' AS rel, NULL::text AS via
          FROM pg_rewrite r JOIN tgt ON r.ev_class = tgt.oid";
    let mut more_rels = String::new();
    let mut more_procs = String::new();
    let (target, sources) = match kind {
        kinds::TABLE => {
            let expr = "pg_get_expr(d.adbin, d.adrelid)";
            let default = if c.generated() {
                format!(
                    "CASE WHEN a.attgenerated <> '' THEN ' GENERATED ALWAYS AS (' || {expr} || ')' \
                     ELSE ' DEFAULT ' || {expr} END"
                )
            } else {
                format!("' DEFAULT ' || {expr}")
            };
            let mut src = format!(
                "SELECT 'pg_attrdef'::regclass AS cls, d.oid AS id, 'code' AS rel, a.attname::text || {default} AS via
                   FROM pg_attrdef d JOIN tgt ON d.adrelid = tgt.oid
                   JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum
                 UNION ALL
                 SELECT 'pg_constraint'::regclass, k.oid, 'check', k.conname::text || ': ' || pg_get_constraintdef(k.oid)
                   FROM pg_constraint k JOIN tgt ON k.conrelid = tgt.oid WHERE k.contype = 'c'
                 UNION ALL
                 SELECT 'pg_class'::regclass, x.indexrelid, 'index', i.relname::text
                   FROM pg_index x JOIN tgt ON x.indrelid = tgt.oid JOIN pg_class i ON i.oid = x.indexrelid"
            );
            if c.policies() {
                src.push_str(
                    "
                 UNION ALL
                 SELECT 'pg_policy'::regclass, p.oid, 'code', 'POLICY ' || p.polname::text
                   FROM pg_policy p JOIN tgt ON p.polrelid = tgt.oid",
                );
            }
            if c.full {
                let (join, how) = if c.version >= 100000 {
                    (
                        " JOIN pg_class tc ON tc.oid = i.inhrelid",
                        "CASE WHEN tc.relispartition THEN 'PARTITION OF' ELSE 'INHERITS' END",
                    )
                } else {
                    ("", "'INHERITS'")
                };
                more_rels.push_str(&format!(
                    "
               UNION ALL
               SELECT i.inhparent, 'code', {how} FROM pg_inherits i JOIN tgt ON i.inhrelid = tgt.oid{join}"
                ));
            }
            // A sequence the table owns (OWNED BY) that no default uses.
            more_rels.push_str(
                "
               UNION ALL
               SELECT d.objid, 'code', 'OWNED BY ' || a.attname::text
                 FROM pg_depend d JOIN tgt ON d.refobjid = tgt.oid
                 JOIN pg_class sq ON sq.oid = d.objid AND sq.relkind = 'S'
                 JOIN pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid
                WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass AND d.deptype = 'a'
                  AND NOT EXISTS (SELECT 1 FROM dep WHERE dep.refclassid = 'pg_class'::regclass AND dep.refobjid = d.objid)",
            );
            (relation("'r', 'p', 'f'"), src)
        }
        kinds::VIEW => (relation("'v'"), RULE.to_string()),
        kinds::MATERIALIZED_VIEW => (relation("'m'"), RULE.to_string()),
        kinds::FUNCTION | kinds::PROCEDURE => (
            routine_target(c, kind, s, n),
            "SELECT 'pg_proc'::regclass AS cls, tgt.oid AS id, 'code' AS rel, NULL::text AS via FROM tgt"
                .to_string(),
        ),
        kinds::TRIGGER => {
            more_rels.push_str(
                "
               UNION ALL
               SELECT t.tgrelid, 'code', NULL FROM pg_trigger t JOIN tgt ON t.oid = tgt.oid",
            );
            more_procs.push_str(
                "
               UNION ALL
               SELECT t.tgfoid, 'code', NULL FROM pg_trigger t JOIN tgt ON t.oid = tgt.oid",
            );
            (
                format!(
                    "SELECT t.oid FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
                       JOIN pg_namespace n ON n.oid = c.relnamespace
                     WHERE t.tgname = {n} AND NOT t.tgisinternal
                       AND (n.nspname = {s} OR ({s} = '' AND pg_table_is_visible(c.oid)))"
                ),
                "SELECT 'pg_trigger'::regclass AS cls, tgt.oid AS id, 'code' AS rel, NULL::text AS via FROM tgt"
                    .to_string(),
            )
        }
        _ => return None,
    };
    // A foreign table says through which wrapper and server.
    let rel_detail = if c.foreign_tables() {
        "COALESCE(r.via, (SELECT w.fdwname::text || ' · ' || fs.srvname::text
                            FROM pg_foreign_table ft JOIN pg_foreign_server fs ON fs.oid = ft.ftserver
                            JOIN pg_foreign_data_wrapper w ON w.oid = fs.srvfdw
                           WHERE ft.ftrelid = c.oid))"
    } else {
        "r.via"
    };
    Some(format!(
        "WITH tgt AS ({target}),
         src AS ({sources}),
         dep AS (SELECT DISTINCT d.refclassid, d.refobjid, s.rel, s.via
                   FROM src s JOIN pg_depend d ON d.classid = s.cls AND d.objid = s.id
                  WHERE d.deptype IN ('n', 'a')),
         rels AS (SELECT dep.refobjid AS id, dep.rel, dep.via FROM dep WHERE dep.refclassid = 'pg_class'::regclass
               UNION ALL
               SELECT ty.typrelid, dep.rel, dep.via FROM dep JOIN pg_type ty ON ty.oid = dep.refobjid
                WHERE dep.refclassid = 'pg_type'::regclass AND ty.typrelid <> 0{more_rels}),
         procs AS (SELECT dep.refobjid AS id, dep.rel, dep.via FROM dep WHERE dep.refclassid = 'pg_proc'::regclass{more_procs})
         SELECT CASE WHEN c.relkind IN ('r', 'p', 'f') THEN 'table' WHEN c.relkind = 'v' THEN 'view'
                     WHEN c.relkind = 'm' THEN 'materialized_view' ELSE 'sequence' END AS kind,
                n.nspname::text AS sch, c.relname::text AS name, r.rel, {rel_detail} AS detail
           FROM rels r JOIN pg_class c ON c.oid = r.id JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE c.relkind IN ('r', 'p', 'f', 'v', 'm', 'S') AND c.oid NOT IN (SELECT oid FROM tgt)
            AND {rel_schema}
         UNION
         SELECT {routine_kind}, n.nspname::text, p.proname::text, r.rel, r.via
           FROM procs r JOIN pg_proc p ON p.oid = r.id JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE p.oid NOT IN (SELECT oid FROM tgt) AND {proc_schema}
            AND NOT EXISTS (SELECT 1 FROM pg_depend e
                             WHERE e.classid = 'pg_proc'::regclass AND e.objid = p.oid AND e.deptype = 'e')
         ORDER BY 2, 3, 1",
        rel_schema = user_schema("n.nspname"),
        proc_schema = user_schema("n.nspname"),
        routine_kind = c.routine_kind(),
    ))
}

/// Whether every overload of the routine has a SQL-standard body
/// (PostgreSQL 14+), whose uses the catalog records: `true`, `false` or
/// NULL (none found). `None` before 14, where none has.
pub(crate) fn sql_bodies_sql(c: Catalog, kind: &str, schema: &str, name: &str) -> Option<String> {
    (c.version >= 140000 && matches!(kind, kinds::FUNCTION | kinds::PROCEDURE)).then(|| {
        format!(
            "SELECT bool_and(p.prosqlbody IS NOT NULL)::text FROM pg_proc p
             WHERE p.oid IN ({})",
            routine_target(c, kind, schema, name)
        )
    })
}

/// The foreign table `schema`.`name` (literals) and its remote source:
/// `fdw`, `srv`, `srvopts`, `ftopts` (the options, one `key=value` a line),
/// `sch`, `name`. `None` where the server has no foreign tables.
pub(crate) fn foreign_table_sql(c: Catalog, schema: &str, name: &str) -> Option<String> {
    c.foreign_tables().then(|| {
        format!(
            "SELECT w.fdwname::text AS fdw, fs.srvname::text AS srv,
                    array_to_string(fs.srvoptions, chr(10)) AS srvopts,
                    array_to_string(ft.ftoptions, chr(10)) AS ftopts,
                    n.nspname::text AS sch, c.relname::text AS name
             FROM pg_foreign_table ft JOIN pg_class c ON c.oid = ft.ftrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_foreign_server fs ON fs.oid = ft.ftserver
             JOIN pg_foreign_data_wrapper w ON w.oid = fs.srvfdw
             WHERE c.relname = {name} AND (n.nspname = {schema} OR ({schema} = '' AND pg_table_is_visible(c.oid)))"
        )
    })
}

/// The report's note when the catalog couldn't be read: the server's
/// first line.
pub(crate) fn unread_note(e: &Error) -> String {
    let msg = e.to_string();
    format!(
        "No se pudo leer el catálogo de dependencias ({}): se muestran solo sus claves foráneas y los nombres que usa su definición.",
        msg.lines().next().unwrap_or_default().trim()
    )
}

/// A row of [`uses_sql`] (and of the variants' queries, which answer the
/// same columns): `kind`, `sch`, `name`, `rel`, `detail`.
pub(crate) fn used(row: [Option<String>; 5]) -> Option<Dependent> {
    let [kind, schema, name, rel, detail] = row;
    Some(Dependent {
        kind: kind?,
        schema: schema.filter(|s| !s.is_empty()),
        name: name?,
        parent: None,
        relation: match rel.as_deref() {
            Some("check") => Relation::Check,
            Some("index") => Relation::Index,
            Some("fk") => Relation::ForeignKey,
            _ => Relation::Code,
        },
        confidence: Confidence::Confirmed,
        detail: detail.filter(|d| !d.is_empty()),
        mentions: Vec::new(),
    })
}

/// A `key=value` option of a list (one a line), the first of `keys` there.
fn option(opts: &str, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        opts.lines().find_map(|l| {
            l.split_once('=')
                .filter(|(key, v)| key.trim() == *k && !v.is_empty())
                .map(|(_, v)| v.to_string())
        })
    })
}

/// A foreign table's remote source from a row of [`foreign_table_sql`]:
/// the wrapper is the system, the server's `host`/`port` (or what the
/// wrapper calls them) the server, the table's `schema_name`/`table_name`
/// (or `dbname`, `collection`, `filename`…) what it reads. postgres_fdw
/// reads the table's own schema and name when the options don't say.
/// Only those options are read: never a user mapping or a connection
/// string, which may carry a password.
pub(crate) fn foreign_source(row: [Option<String>; 6]) -> Option<ExternalRef> {
    let [fdw, srv, srv_opts, ft_opts, schema, name] = row;
    let fdw = fdw?;
    let (so, fo) = (srv_opts.unwrap_or_default(), ft_opts.unwrap_or_default());
    let host = option(
        &so,
        &[
            "host",
            "hostname",
            "address",
            "servername",
            "server",
            "dbserver",
        ],
    );
    let server = match (host, option(&so, &["port"])) {
        (Some(h), Some(p)) if !h.contains(':') && !h.contains('/') => Some(format!("{h}:{p}")),
        (h, _) => h,
    };
    let database = option(&fo, &["dbname", "database", "db"])
        .or_else(|| option(&so, &["dbname", "database", "db"]));
    let pg = fdw == "postgres_fdw";
    Some(ExternalRef {
        system: Some(fdw),
        server,
        database,
        schema: option(&fo, &["schema_name", "schema"]).or_else(|| schema.filter(|_| pg)),
        name: option(
            &fo,
            &["table_name", "table", "collection", "filename", "file"],
        )
        .or_else(|| name.filter(|_| pg)),
        detail: srv,
        mentions: Vec::new(),
    })
}

/// The lines of `body` (the target's source) that name each item that has
/// none yet.
pub(crate) fn add_mentions(items: &mut [Dependent], body: &str, dialect: &ScriptDialect) {
    for d in items.iter_mut().filter(|d| d.mentions.is_empty()) {
        let used = DependencyTarget {
            object: ObjectRef {
                kind: d.kind.clone(),
                schema: d.schema.clone(),
                name: d.name.clone(),
            },
            column: None,
        };
        if let Some((_, m)) = find_mentions(body, dialect, &used) {
            d.mentions = m;
        }
    }
}

fn same(a: &Dependent, b: &Dependent) -> bool {
    let eq = |x: &str, y: &str| x.eq_ignore_ascii_case(y);
    a.relation == b.relation
        && a.kind == b.kind
        && eq(&a.name, &b.name)
        && eq(
            a.schema.as_deref().unwrap_or(""),
            b.schema.as_deref().unwrap_or(""),
        )
        // One item per foreign key, check or index; one per object for code.
        && (a.relation == Relation::Code || a.detail == b.detail)
}

/// The catalog's items plus what only `text` (the generic scan) found;
/// an object both have keeps the catalog's confidence and the text's lines
/// when it has none.
pub(crate) fn merge(mut catalog: DependencyReport, text: DependencyReport) -> DependencyReport {
    for d in text.items {
        match catalog.items.iter_mut().find(|c| same(c, &d)) {
            Some(c) => {
                if c.mentions.is_empty() {
                    c.mentions = d.mentions;
                }
            }
            None => catalog.items.push(d),
        }
    }
    catalog.scanned = catalog.scanned.max(text.scanned);
    for u in text.unreadable {
        if !catalog.unreadable.contains(&u) {
            catalog.unreadable.push(u);
        }
    }
    for x in text.external {
        if !catalog.external.contains(&x) {
            catalog.external.push(x);
        }
    }
    catalog.note = catalog.note.or(text.note);
    catalog
}

/// Confirmed first, then by relation, schema and name (as the generic scan).
pub(crate) fn sort(items: &mut [Dependent]) {
    items.sort_by(|a, b| {
        (
            a.confidence,
            a.relation,
            a.schema.as_deref().unwrap_or(""),
            a.name.to_lowercase(),
        )
            .cmp(&(
                b.confidence,
                b.relation,
                b.schema.as_deref().unwrap_or(""),
                b.name.to_lowercase(),
            ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::dependencies::Mention;

    const PG16: Catalog = Catalog {
        version: 160000,
        full: true,
    };

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    fn schemas(col: &str) -> String {
        format!("{col} <> 'pg_catalog'")
    }

    #[test]
    fn a_view_reads_its_rule_and_a_table_its_defaults_checks_indexes_and_parents() {
        let v = uses_sql(PG16, "view", "'ventas'", "'v'", schemas).unwrap();
        assert!(v.contains("c.relname = 'v' AND c.relkind IN ('v')"));
        assert!(
            v.contains("n.nspname = 'ventas' OR ('ventas' = '' AND pg_table_is_visible(c.oid))")
        );
        assert!(v.contains("FROM pg_rewrite r JOIN tgt ON r.ev_class = tgt.oid"));
        assert!(!v.contains("pg_attrdef") && !v.contains("pg_inherits"));
        assert!(v.contains("d.deptype IN ('n', 'a')"));
        assert!(v.contains("CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END"));
        assert!(v.contains("e.deptype = 'e'") && v.contains("n.nspname <> 'pg_catalog'"));
        assert!(v.contains("ty.typrelid <> 0"));
        assert!(v.contains("w.fdwname::text || ' · ' || fs.srvname::text"));

        let m = uses_sql(PG16, "materialized_view", "''", "'m'", schemas).unwrap();
        assert!(m.contains("c.relkind IN ('m')") && m.contains("pg_rewrite"));

        let t = uses_sql(PG16, "table", "'ventas'", "'pedidos'", schemas).unwrap();
        assert!(t.contains("c.relkind IN ('r', 'p', 'f')"));
        for part in [
            "FROM pg_attrdef d JOIN tgt",
            "' GENERATED ALWAYS AS ('",
            "k.contype = 'c'",
            "FROM pg_index x JOIN tgt",
            "FROM pg_policy p JOIN tgt",
            "FROM pg_inherits i JOIN tgt",
            "CASE WHEN tc.relispartition THEN 'PARTITION OF' ELSE 'INHERITS' END",
            "'OWNED BY ' || a.attname::text",
            "NOT EXISTS (SELECT 1 FROM dep WHERE",
        ] {
            assert!(t.contains(part), "{part}");
        }
        assert!(!t.contains("pg_rewrite"));
    }

    #[test]
    fn routines_and_triggers_read_their_own_dependencies() {
        let f = uses_sql(PG16, "function", "'ventas'", "'total'", schemas).unwrap();
        assert!(f.contains("p.proname = 'total' AND p.prokind <> 'p'"));
        assert!(f.contains("pg_function_is_visible(p.oid)"));
        assert!(f.contains("SELECT 'pg_proc'::regclass AS cls, tgt.oid AS id"));
        let p = uses_sql(PG16, "procedure", "'ventas'", "'cargar'", schemas).unwrap();
        assert!(p.contains("p.prokind = 'p'"));

        let tr = uses_sql(PG16, "trigger", "'ventas'", "'trg'", schemas).unwrap();
        assert!(tr.contains("t.tgname = 'trg' AND NOT t.tgisinternal"));
        assert!(tr.contains("SELECT 'pg_trigger'::regclass AS cls"));
        assert!(tr.contains("SELECT t.tgrelid, 'code', NULL FROM pg_trigger t JOIN tgt"));
        assert!(tr.contains("SELECT t.tgfoid, 'code', NULL FROM pg_trigger t JOIN tgt"));

        assert!(uses_sql(PG16, "sequence", "''", "'s'", schemas).is_none());
        assert!(uses_sql(PG16, "source", "''", "'s'", schemas).is_none());

        let b = sql_bodies_sql(PG16, "function", "'ventas'", "'total'").unwrap();
        assert!(
            b.contains("bool_and(p.prosqlbody IS NOT NULL)") && b.contains("p.proname = 'total'")
        );
        let pg13 = Catalog {
            version: 130000,
            full: true,
        };
        assert!(sql_bodies_sql(pg13, "function", "''", "'f'").is_none());
        assert!(sql_bodies_sql(PG16, "view", "''", "'v'").is_none());
    }

    #[test]
    fn older_servers_and_dsql_skip_what_their_catalog_lacks() {
        // openGauss / Greenplum 6: no prokind, policies, generated columns or relispartition.
        let old = Catalog {
            version: 90204,
            full: true,
        };
        let t = uses_sql(old, "table", "''", "'t'", schemas).unwrap();
        assert!(!t.contains("prokind") && !t.contains("pg_policy") && !t.contains("attgenerated"));
        assert!(t.contains("SELECT i.inhparent, 'code', 'INHERITS' FROM pg_inherits i JOIN tgt ON i.inhrelid = tgt.oid\n"));
        assert!(t.contains("'function'") && t.contains("pg_foreign_table"));
        assert!(!uses_sql(old, "function", "''", "'f'", schemas)
            .unwrap()
            .contains("prokind"));

        let dsql = Catalog {
            version: 160000,
            full: false,
        };
        let t = uses_sql(dsql, "table", "'public'", "'t'", schemas).unwrap();
        for absent in ["pg_policy", "pg_inherits", "pg_foreign", "attgenerated"] {
            assert!(!t.contains(absent), "{absent}");
        }
        assert!(t.contains("' DEFAULT ' || pg_get_expr(d.adbin, d.adrelid)"));
        assert!(foreign_table_sql(dsql, "''", "'t'").is_none());
        let ft = foreign_table_sql(PG16, "'ventas'", "'remota'").unwrap();
        assert!(ft.contains("FROM pg_foreign_table ft") && ft.contains("c.relname = 'remota'"));
        assert!(ft.contains("array_to_string(fs.srvoptions, chr(10)) AS srvopts"));
        assert!(!ft.contains("pg_user_mapping"));
    }

    #[test]
    fn the_note_keeps_the_servers_first_line() {
        let e = Error::Query("ERROR: permission denied for table pg_depend\nDetalle: x".into());
        assert_eq!(
            unread_note(&e),
            "No se pudo leer el catálogo de dependencias (ERROR: permission denied for table pg_depend): \
             se muestran solo sus claves foráneas y los nombres que usa su definición."
        );
    }

    #[test]
    fn rows_become_confirmed_items() {
        let d = used([s("table"), s("ventas"), s("clientes"), s("code"), None]).unwrap();
        assert_eq!(
            (d.kind.as_str(), d.schema.as_deref(), d.name.as_str()),
            ("table", Some("ventas"), "clientes")
        );
        assert_eq!(
            (d.relation, d.confidence, d.detail),
            (Relation::Code, Confidence::Confirmed, None)
        );
        let c = used([
            s("function"),
            s("ventas"),
            s("positivo"),
            s("check"),
            s("pedidos_total_check: CHECK (ventas.positivo(total))"),
        ])
        .unwrap();
        assert_eq!(c.relation, Relation::Check);
        assert_eq!(
            c.detail.as_deref(),
            Some("pedidos_total_check: CHECK (ventas.positivo(total))")
        );
        let i = used([s("function"), s("x"), s("f"), s("index"), s("ix_f")]).unwrap();
        assert_eq!(i.relation, Relation::Index);
        assert!(used([None, s("x"), s("f"), s("code"), None]).is_none());
        assert!(used([s("table"), s("x"), None, s("code"), None]).is_none());
    }

    #[test]
    fn a_foreign_table_names_its_remote_source() {
        let pg = foreign_source([
            s("postgres_fdw"),
            s("srv_ventas"),
            s("host=10.0.0.5\nport=5433\ndbname=ventas"),
            None,
            s("local"),
            s("pedidos_remotos"),
        ])
        .unwrap();
        assert_eq!(
            pg,
            ExternalRef {
                system: s("postgres_fdw"),
                server: s("10.0.0.5:5433"),
                database: s("ventas"),
                schema: s("local"),
                name: s("pedidos_remotos"),
                detail: s("srv_ventas"),
                mentions: Vec::new(),
            }
        );
        let named = foreign_source([
            s("postgres_fdw"),
            s("srv"),
            s("host=db.example.com"),
            s("schema_name=public\ntable_name=orders"),
            s("local"),
            s("ft"),
        ])
        .unwrap();
        assert_eq!(
            (
                named.server.as_deref(),
                named.schema.as_deref(),
                named.name.as_deref()
            ),
            (Some("db.example.com"), Some("public"), Some("orders"))
        );
        let my = foreign_source([
            s("mysql_fdw"),
            s("srv_my"),
            s("host=mysql\nport=3306"),
            s("dbname=shop\ntable_name=items"),
            s("local"),
            s("items"),
        ])
        .unwrap();
        assert_eq!(
            (
                my.server.as_deref(),
                my.database.as_deref(),
                my.schema,
                my.name.as_deref()
            ),
            (Some("mysql:3306"), Some("shop"), None, Some("items"))
        );
        let file = foreign_source([
            s("file_fdw"),
            s("files"),
            None,
            s("filename=/data/a=b.csv\nformat=csv"),
            s("local"),
            s("csv"),
        ])
        .unwrap();
        assert_eq!(
            (file.server, file.name.as_deref()),
            (None, Some("/data/a=b.csv"))
        );
        let ora = foreign_source([
            s("oracle_fdw"),
            s("ora"),
            s("dbserver=//ora:1521/XE"),
            s("schema=HR\ntable=EMP"),
            s("local"),
            s("emp"),
        ])
        .unwrap();
        assert_eq!(
            (
                ora.server.as_deref(),
                ora.schema.as_deref(),
                ora.name.as_deref()
            ),
            (Some("//ora:1521/XE"), Some("HR"), Some("EMP"))
        );
        assert!(foreign_source([None, None, None, None, None, None]).is_none());
    }

    fn item(kind: &str, name: &str, confidence: Confidence) -> Dependent {
        Dependent {
            kind: kind.into(),
            schema: Some("ventas".into()),
            name: name.into(),
            parent: None,
            relation: Relation::Code,
            confidence,
            detail: None,
            mentions: Vec::new(),
        }
    }

    #[test]
    fn mentions_come_from_the_targets_source() {
        let body = "CREATE OR REPLACE VIEW ventas.v AS\n SELECT p.id\n   FROM ventas.pedidos p\n   JOIN otra.pedidos q ON q.id = p.id\n  WHERE ventas.activo(p.id);";
        let mut items = vec![
            item("table", "pedidos", Confidence::Confirmed),
            item("function", "activo", Confidence::Confirmed),
        ];
        add_mentions(&mut items, body, &ScriptDialect::postgres());
        assert_eq!(
            items[0].mentions,
            [Mention {
                line: 3,
                text: "FROM ventas.pedidos p".into(),
                dynamic: false
            }]
        );
        assert_eq!(items[1].mentions[0].line, 5);
    }

    #[test]
    fn the_catalog_wins_and_text_adds_what_it_alone_found() {
        let mut cat = DependencyReport::default();
        cat.items
            .push(item("table", "pedidos", Confidence::Confirmed));
        let mut text = DependencyReport {
            scanned: 1,
            unreadable: vec!["ventas.x".into()],
            ..Default::default()
        };
        let mut found = item("table", "Pedidos", Confidence::Probable);
        found.mentions.push(Mention {
            line: 2,
            text: "FROM pedidos".into(),
            dynamic: false,
        });
        text.items.push(found);
        text.items
            .push(item("function", "auditar", Confidence::Probable));
        let mut r = merge(cat, text);
        sort(&mut r.items);
        assert_eq!(r.items.len(), 2);
        assert_eq!(
            (r.items[0].name.as_str(), r.items[0].confidence),
            ("pedidos", Confidence::Confirmed)
        );
        assert_eq!(r.items[0].mentions[0].line, 2);
        assert_eq!(
            (r.items[1].name.as_str(), r.items[1].confidence),
            ("auditar", Confidence::Probable)
        );
        assert_eq!((r.scanned, r.unreadable.len()), (1, 1));

        // Two foreign keys to one table stay two.
        let fk = |detail: &str| Dependent {
            relation: Relation::ForeignKey,
            detail: Some(detail.into()),
            ..item("table", "clientes", Confidence::Confirmed)
        };
        let cat = DependencyReport {
            items: vec![fk("a"), fk("b")],
            ..Default::default()
        };
        let text = DependencyReport {
            items: vec![fk("a"), fk("b")],
            ..Default::default()
        };
        assert_eq!(merge(cat, text).items.len(), 2);
    }
}
