//! The lineage of the whole database (`Session::lineage_graph`), per
//! variant, from one catalog query for the dependencies instead of one per
//! object:
//!
//! - PostgreSQL and the variants that keep its catalog: `pg_depend` from
//!   the rewrite rule of each view and materialized view, from each routine
//!   (PostgreSQL 14+ records what a SQL-standard body uses, any body its
//!   signature's types), trigger, default, check, index and policy of each
//!   table, plus `pg_inherits` and `pg_constraint` (foreign keys). All of
//!   it is Confirmed. The source of the routines whose body the catalog
//!   doesn't track (PL/pgSQL, a string body) is read in one query and
//!   parsed (Probable, with the lines). Foreign tables go to `external`
//!   with their wrapper and server. See [`crate::pg_references`] for what
//!   each row means.
//! - Redshift: the same query (its `pg_depend` has the views); every
//!   source is parsed, as late-binding views record nothing.
//! - CockroachDB: `crdb_internal.backward_dependencies` and the foreign
//!   keys; functions aren't in it, so views and routines are parsed too.
//!   Another database's objects go to `external`.
//! - Materialize: `mz_internal.mz_object_dependencies`; connections and
//!   another database's objects go to `external`.
//! - RisingWave: `rw_catalog.rw_depend` and the connector of each source.
//! - Denodo, CrateDB, H2: the generic scan.
//!
//! The server's jobs (pg_cron, pgAgent…) come from `Session::jobs` and
//! `Session::job_references` when the scope asks for them. If the catalog
//! can't be read the answer is the generic scan's, with a note saying why.

use crate::catalog::cell;
use crate::pg_references::{self as pr, Catalog};
use crate::references::{row5, rw_prop, trusts_catalog};
use crate::session::{join_definitions, PgSession};
use crate::Variant;
use dbine_driver::dependencies::{
    find_mentions, graph_scan, used_names, used_objects, Confidence, DependencyScan,
    DependencyTarget, Dependent, ExternalRef, LineageEdge, LineageExternal, LineageGraph,
    LineageJob, LineageScope, Relation, GRAPH_DEFINITIONS, GRAPH_OBJECTS, JOBS_READ,
};
use dbine_driver::{kinds, DbObject, Error, ObjectRef, Result, Session};
use std::collections::HashSet;
use tokio_postgres::SimpleQueryRow;

/// An object by what names it: kind, schema and name, lower case.
type Key = (String, String, String);

fn key(kind: &str, schema: Option<&str>, name: &str) -> Key {
    (
        kind.to_string(),
        schema.unwrap_or_default().to_lowercase(),
        name.to_lowercase(),
    )
}

fn key_of(o: &ObjectRef) -> Key {
    key(&o.kind, o.schema(), &o.name)
}

fn dep_ref(d: &Dependent) -> ObjectRef {
    ObjectRef {
        kind: d.kind.clone(),
        schema: d.schema.clone(),
        name: d.name.clone(),
    }
}

/// What a catalog read found: who uses what, and what comes from outside.
#[derive(Default)]
pub(crate) struct Found {
    pub(crate) edges: Vec<(ObjectRef, Dependent)>,
    pub(crate) external: Vec<LineageExternal>,
    /// The remote source of each foreign table (the explorer doesn't list
    /// them, so they are not nodes: whoever reads one gets it as `external`).
    pub(crate) foreign: Vec<(Key, ExternalRef)>,
}

/// A row of the dependency queries: `ukind`, `usch`, `uname` (who uses it)
/// and the five columns of [`row5`].
fn user_and_used(r: &SimpleQueryRow) -> Option<(ObjectRef, Dependent)> {
    let user = ObjectRef {
        kind: cell(r, "ukind")?,
        schema: cell(r, "usch").filter(|s| !s.is_empty()),
        name: cell(r, "uname")?,
    };
    Some((user, pr::used(row5(r))?))
}

/// What a table's `pg_attrdef` row says, as the object lineage reads it.
pub(crate) fn default_text(c: Catalog) -> String {
    let expr = "pg_get_expr(d.adbin, d.adrelid)";
    if c.generated() {
        format!(
            "CASE WHEN a.attgenerated <> '' THEN ' GENERATED ALWAYS AS (' || {expr} || ')' \
             ELSE ' DEFAULT ' || {expr} END"
        )
    } else {
        format!("' DEFAULT ' || {expr}")
    }
}

/// PostgreSQL's catalog for the whole database: one row per (who, what it
/// uses): `ukind`, `usch`, `uname`, `kind`, `sch`, `name`, `rel`, `detail`.
/// `user_schema(col)` keeps the server's schemas out.
pub(crate) fn uses_sql(c: Catalog, tables: bool, user_schema: impl Fn(&str) -> String) -> String {
    let default = default_text(c);
    let policies = if c.policies() {
        "
          UNION ALL
          SELECT 'pg_policy'::regclass, p.oid, 'table', t.sch, t.name, 'code', 'POLICY ' || p.polname::text
            FROM pg_policy p JOIN tbl t ON t.oid = p.polrelid"
    } else {
        ""
    };
    let inherits = if c.full {
        let (join, how) = if c.version >= 100000 {
            (
                " JOIN pg_class tc ON tc.oid = i.inhrelid",
                "CASE WHEN tc.relispartition THEN 'PARTITION OF' ELSE 'INHERITS' END",
            )
        } else {
            ("", "'INHERITS'")
        };
        format!(
            "
               UNION ALL
               SELECT 'table', t.sch, t.name, i.inhparent, 'code', {how}
                 FROM pg_inherits i JOIN tbl t ON t.oid = i.inhrelid{join}"
        )
    } else {
        String::new()
    };
    let prokind = if c.prokind() {
        "p.prokind IN ('f', 'p')"
    } else {
        "NOT p.proisagg AND NOT p.proiswindow"
    };
    let table_src = if tables {
        format!(
            "
          UNION ALL
          SELECT 'pg_attrdef'::regclass, d.oid, 'table', t.sch, t.name, 'code', a.attname::text || {default}
            FROM pg_attrdef d JOIN tbl t ON t.oid = d.adrelid
            JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum
          UNION ALL
          SELECT 'pg_constraint'::regclass, k.oid, 'table', t.sch, t.name, 'check',
                 k.conname::text || ': ' || pg_get_constraintdef(k.oid)
            FROM pg_constraint k JOIN tbl t ON t.oid = k.conrelid WHERE k.contype = 'c'
          UNION ALL
          SELECT 'pg_class'::regclass, x.indexrelid, 'table', t.sch, t.name, 'index', i.relname::text
            FROM pg_index x JOIN tbl t ON t.oid = x.indrelid JOIN pg_class i ON i.oid = x.indexrelid
          UNION ALL
          SELECT 'pg_trigger'::regclass, g.oid, 'trigger', t.sch, g.tgname::text, 'code', NULL
            FROM pg_trigger g JOIN tbl t ON t.oid = g.tgrelid WHERE NOT g.tgisinternal{policies}"
        )
    } else {
        String::new()
    };
    let rel_detail = if c.foreign_tables() {
        "COALESCE(r.via, (SELECT w.fdwname::text || ' · ' || fs.srvname::text
                            FROM pg_foreign_table ft JOIN pg_foreign_server fs ON fs.oid = ft.ftserver
                            JOIN pg_foreign_data_wrapper w ON w.oid = fs.srvfdw
                           WHERE ft.ftrelid = c.oid))"
    } else {
        "r.via"
    };
    format!(
        "WITH tbl AS (SELECT c.oid, n.nspname::text AS sch, c.relname::text AS name
                        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                       WHERE c.relkind IN ('r', 'p', 'f') AND {table_schema}),
         src AS (
          SELECT 'pg_rewrite'::regclass AS cls, r.oid AS id,
                 CASE v.relkind WHEN 'm' THEN 'materialized_view' ELSE 'view' END AS ukind,
                 n.nspname::text AS usch, v.relname::text AS uname, 'code' AS rel, NULL::text AS via
            FROM pg_rewrite r JOIN pg_class v ON v.oid = r.ev_class JOIN pg_namespace n ON n.oid = v.relnamespace
           WHERE v.relkind IN ('v', 'm') AND {view_schema}
          UNION ALL
          SELECT 'pg_proc'::regclass, p.oid, {routine_kind}, n.nspname::text, p.proname::text, 'code', NULL
            FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
           WHERE {prokind} AND {proc_schema}
             AND NOT EXISTS (SELECT 1 FROM pg_depend e
                              WHERE e.classid = 'pg_proc'::regclass AND e.objid = p.oid AND e.deptype = 'e')
{table_src}),
         dep AS (SELECT DISTINCT s.ukind, s.usch, s.uname, s.rel, s.via, d.refclassid, d.refobjid
                   FROM src s JOIN pg_depend d ON d.classid = s.cls AND d.objid = s.id
                  WHERE d.deptype IN ('n', 'a')),
         rels AS (SELECT dep.ukind, dep.usch, dep.uname, dep.refobjid AS id, dep.rel, dep.via
                    FROM dep WHERE dep.refclassid = 'pg_class'::regclass
               UNION ALL
               SELECT dep.ukind, dep.usch, dep.uname, ty.typrelid, dep.rel, dep.via
                 FROM dep JOIN pg_type ty ON ty.oid = dep.refobjid
                WHERE dep.refclassid = 'pg_type'::regclass AND ty.typrelid <> 0{inherits}),
         procs AS (SELECT dep.ukind, dep.usch, dep.uname, dep.refobjid AS id, dep.rel, dep.via
                     FROM dep WHERE dep.refclassid = 'pg_proc'::regclass)
         SELECT r.ukind, r.usch, r.uname,
                CASE WHEN c.relkind IN ('r', 'p') THEN 'table' WHEN c.relkind = 'f' THEN 'foreign_table'
                     WHEN c.relkind = 'v' THEN 'view'
                     WHEN c.relkind = 'm' THEN 'materialized_view' ELSE 'sequence' END AS kind,
                n.nspname::text AS sch, c.relname::text AS name, r.rel, {rel_detail} AS detail
           FROM rels r JOIN pg_class c ON c.oid = r.id JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE c.relkind IN ('r', 'p', 'f', 'v', 'm', 'S') AND {rel_schema}
         UNION
         SELECT r.ukind, r.usch, r.uname, {routine_kind}, n.nspname::text, p.proname::text, r.rel, r.via
           FROM procs r JOIN pg_proc p ON p.oid = r.id JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE {proc_schema}
            AND NOT EXISTS (SELECT 1 FROM pg_depend e
                             WHERE e.classid = 'pg_proc'::regclass AND e.objid = p.oid AND e.deptype = 'e')",
        table_schema = user_schema("n.nspname"),
        view_schema = user_schema("n.nspname"),
        proc_schema = user_schema("n.nspname"),
        rel_schema = user_schema("n.nspname"),
        routine_kind = c.routine_kind(),
    )
}

/// The foreign keys of every table: the table that holds it uses the one it
/// points at. Same columns as [`uses_sql`].
pub(crate) fn foreign_keys_sql(user_schema: impl Fn(&str) -> String) -> String {
    format!(
        "SELECT 'table' AS ukind, n.nspname::text AS usch, c.relname::text AS uname,
                'table' AS kind, pn.nspname::text AS sch, pc.relname::text AS name,
                'fk' AS rel, k.conname::text AS detail
           FROM pg_constraint k
           JOIN pg_class c ON c.oid = k.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace
           JOIN pg_class pc ON pc.oid = k.confrelid JOIN pg_namespace pn ON pn.oid = pc.relnamespace
          WHERE k.contype = 'f' AND {}",
        user_schema("n.nspname")
    )
}

/// The source of every view, materialized view and routine that has
/// source: `kind`, `sch`, `name`, `tracked` (`true` when the catalog already
/// lists what it uses: a view, a routine with a SQL-standard body) and
/// `body` (the text the object page shows).
pub(crate) fn bodies_sql(c: Catalog, user_schema: impl Fn(&str) -> String, limit: usize) -> String {
    let routine_filter = if c.prokind() {
        "p.prokind IN ('f', 'p')"
    } else {
        "NOT p.proisagg AND NOT p.proiswindow"
    };
    let tracked = if c.version >= 140000 {
        "(p.prosqlbody IS NOT NULL)::text"
    } else {
        "'false'"
    };
    format!(
        "SELECT kind, sch, name, tracked, body FROM (
           SELECT CASE v.relkind WHEN 'm' THEN 'materialized_view' ELSE 'view' END AS kind,
                  n.nspname::text AS sch, v.relname::text AS name, 'true' AS tracked,
                  CASE v.relkind WHEN 'm' THEN 'CREATE MATERIALIZED VIEW ' ELSE 'CREATE OR REPLACE VIEW ' END
                    || quote_ident(n.nspname) || '.' || quote_ident(v.relname) || ' AS' || chr(10)
                    || pg_get_viewdef(v.oid, true) AS body, v.oid::bigint AS ord
             FROM pg_class v JOIN pg_namespace n ON n.oid = v.relnamespace
            WHERE v.relkind IN ('v', 'm') AND {views}
           UNION ALL
           SELECT {routine_kind}, n.nspname::text, p.proname::text, {tracked},
                  pg_get_functiondef(p.oid), p.oid::bigint
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
             JOIN pg_language l ON l.oid = p.prolang
            WHERE {routine_filter} AND {routines} AND l.lanname NOT IN ('c', 'internal')
              AND NOT EXISTS (SELECT 1 FROM pg_depend e
                               WHERE e.classid = 'pg_proc'::regclass AND e.objid = p.oid AND e.deptype = 'e')
         ) b ORDER BY ord LIMIT {limit}",
        views = user_schema("n.nspname"),
        routines = user_schema("n.nspname"),
        routine_kind = c.routine_kind(),
    )
}

/// Every foreign table and its remote source, the columns
/// [`pr::foreign_source`] reads.
pub(crate) fn foreign_tables_sql(user_schema: impl Fn(&str) -> String) -> String {
    format!(
        "SELECT w.fdwname::text AS fdw, fs.srvname::text AS srv,
                array_to_string(fs.srvoptions, chr(10)) AS srvopts,
                array_to_string(ft.ftoptions, chr(10)) AS ftopts,
                n.nspname::text AS sch, c.relname::text AS name
           FROM pg_foreign_table ft JOIN pg_class c ON c.oid = ft.ftrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
           JOIN pg_foreign_server fs ON fs.oid = ft.ftserver
           JOIN pg_foreign_data_wrapper w ON w.oid = fs.srvfdw
          WHERE {}",
        user_schema("n.nspname")
    )
}

/// CockroachDB: who uses what, from `crdb_internal.backward_dependencies`
/// (the foreign keys are [`foreign_keys_sql`]'s). `db` is set for another
/// database's object.
pub(crate) fn cockroach_sql(user_schema: impl Fn(&str) -> String) -> String {
    format!(
        "SELECT DISTINCT
                CASE uc.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized_view' ELSE 'table' END AS ukind,
                un.nspname AS usch, uc.relname AS uname,
                CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized_view'
                               WHEN 'S' THEN 'sequence' ELSE 'table' END AS kind,
                t.schema_name AS sch, t.name AS name,
                CASE WHEN t.database_name <> current_database() THEN t.database_name END AS db,
                'code' AS rel, NULL::STRING AS detail
         FROM crdb_internal.backward_dependencies b
         JOIN crdb_internal.tables t ON t.table_id = b.dependson_id
         LEFT JOIN pg_class c ON c.oid::INT8 = b.dependson_id
         JOIN pg_class uc ON uc.oid::INT8 = b.descriptor_id
         JOIN pg_namespace un ON un.oid = uc.relnamespace
         WHERE b.dependson_type <> 'fk' AND uc.relkind IN ('r', 'v', 'm') AND {}",
        user_schema("un.nspname")
    )
}

/// Materialize: who uses what, from `mz_internal.mz_object_dependencies`
/// (user objects of the current database). `db` is set for another
/// database's object, a `connection` has its type in `detail`.
pub(crate) fn materialize_sql() -> &'static str {
    "SELECT DISTINCT CASE o.type WHEN 'materialized-view' THEN 'materialized_view' ELSE o.type END AS ukind,
            os.name AS usch, o.name AS uname,
            CASE r.type WHEN 'materialized-view' THEN 'materialized_view' ELSE r.type END AS kind,
            rs.name AS sch, r.name AS name,
            CASE WHEN rd.name IS DISTINCT FROM current_database() THEN rd.name END AS db,
            'code' AS rel, cn.type AS detail
     FROM mz_internal.mz_object_dependencies d
     JOIN mz_catalog.mz_objects o ON o.id = d.object_id
     JOIN mz_catalog.mz_schemas os ON os.id = o.schema_id
     JOIN mz_catalog.mz_databases od ON od.id = os.database_id
     JOIN mz_catalog.mz_objects r ON r.id = d.referenced_object_id
     JOIN mz_catalog.mz_schemas rs ON rs.id = r.schema_id
     LEFT JOIN mz_catalog.mz_databases rd ON rd.id = rs.database_id
     LEFT JOIN mz_catalog.mz_connections cn ON cn.id = r.id
     WHERE od.name = current_database() AND o.id LIKE 'u%' AND r.id LIKE 'u%'
       AND o.type IN ('table', 'view', 'materialized-view', 'source', 'sink')
       AND r.type IN ('table', 'view', 'materialized-view', 'source', 'sink', 'connection')"
}

/// RisingWave: who uses what, from `rw_catalog.rw_depend`.
pub(crate) fn risingwave_sql() -> &'static str {
    "SELECT DISTINCT CASE o.relation_type WHEN 'materialized view' THEN 'materialized_view' ELSE o.relation_type END AS ukind,
            os.name AS usch, o.name AS uname,
            CASE r.relation_type WHEN 'materialized view' THEN 'materialized_view' ELSE r.relation_type END AS kind,
            rs.name AS sch, r.name AS name, 'code' AS rel, NULL AS detail
     FROM rw_catalog.rw_depend d
     JOIN rw_catalog.rw_relations o ON o.id = d.objid
     JOIN rw_catalog.rw_schemas os ON os.id = o.schema_id
     JOIN rw_catalog.rw_relations r ON r.id = d.refobjid
     JOIN rw_catalog.rw_schemas rs ON rs.id = r.schema_id
     WHERE o.relation_type IN ('table', 'view', 'materialized view', 'source', 'sink')
       AND r.relation_type IN ('table', 'view', 'materialized view', 'source', 'sink')
       AND os.name NOT IN ('rw_catalog', 'information_schema', 'pg_catalog')"
}

/// RisingWave: the connector of every source and connector table, with
/// the plaintext properties only (see [`rw_prop`]).
pub(crate) fn risingwave_connectors_sql() -> String {
    format!(
        "SELECT CASE o.relation_type WHEN 'materialized view' THEN 'materialized_view' ELSE o.relation_type END AS ukind,
                sc.name AS usch, s.name AS uname, lower(s.connector) AS sys,
                {host} AS host, {port} AS port, {db} AS db, {sch} AS sch, {obj} AS obj
         FROM (SELECT x.id, x.name, x.schema_id, x.connector, x.connector_props AS p FROM rw_catalog.rw_sources x) s
         JOIN rw_catalog.rw_schemas sc ON sc.id = s.schema_id
         JOIN rw_catalog.rw_relations o ON o.id = s.id
         WHERE s.connector IS NOT NULL",
        host = rw_prop(&[
            "properties.bootstrap.server",
            "hostname",
            "host",
            "endpoint"
        ]),
        port = rw_prop(&["port"]),
        db = rw_prop(&["database.name", "database"]),
        sch = rw_prop(&["schema.name"]),
        obj = rw_prop(&[
            "topic",
            "table.name",
            "stream",
            "s3.bucket_name",
            "collection.name"
        ]),
    )
}

/// Rows of the variants' queries with a `db` column: the current
/// database's are edges, another's are `external` (of `system`); a
/// `connection` is `external` too, of its type.
fn split_rows(rows: &[SimpleQueryRow], system: &str) -> Found {
    let mut f = Found::default();
    for r in rows {
        let Some((user, item)) = user_and_used(r) else {
            continue;
        };
        let source = if item.kind == "connection" {
            Some(ExternalRef {
                system: item.detail.clone(),
                detail: Some(match &item.schema {
                    Some(s) => format!("{s}.{}", item.name),
                    None => item.name.clone(),
                }),
                ..Default::default()
            })
        } else {
            cell(r, "db").map(|db| ExternalRef {
                system: Some(system.into()),
                database: Some(db),
                schema: item.schema.clone(),
                name: Some(item.name.clone()),
                ..Default::default()
            })
        };
        match source {
            Some(source) => f.external.push(LineageExternal { source, to: user }),
            None => f.edges.push((user, item)),
        }
    }
    f
}

/// The nodes: what the generic scan would list, in the scope.
fn pick_nodes(
    all: &[DbObject],
    scope: &LineageScope,
    scan: &DependencyScan,
) -> (Vec<ObjectRef>, bool) {
    let limit = if scope.limit == 0 {
        GRAPH_OBJECTS
    } else {
        scope.limit
    } as usize;
    let mut nodes = Vec::new();
    let mut truncated = false;
    for o in all {
        let kind_ok = o.kind == kinds::TABLE
            || o.kind == kinds::COLLECTION
            || scan.source_kinds.contains(&o.kind)
            || scan.written_kinds.contains(&o.kind);
        let scope_ok = (scope.kinds.is_empty() || scope.kinds.contains(&o.kind))
            && (scope.schemas.is_empty()
                || o.schema
                    .as_deref()
                    .is_some_and(|s| scope.schemas.iter().any(|x| x.eq_ignore_ascii_case(s))));
        if !kind_ok || !scope_ok {
            continue;
        }
        if nodes.len() >= limit {
            truncated = true;
            break;
        }
        nodes.push(ObjectRef {
            kind: o.kind.clone(),
            schema: o.schema.clone(),
            name: o.name.clone(),
        });
    }
    (nodes, truncated)
}

/// The source of an object, and whether the catalog already lists what it
/// uses.
struct Body {
    object: ObjectRef,
    text: String,
    tracked: bool,
}

type Seen = HashSet<(Key, Key, String)>;

impl PgSession {
    pub(crate) async fn lineage_impl(
        &mut self,
        database: &str,
        scope: &LineageScope,
        scan: &DependencyScan,
    ) -> Result<LineageGraph> {
        let v = self.variant;
        if matches!(v, Variant::Denodo | Variant::CrateDb | Variant::H2) {
            return graph_scan(self, database, scope, scan).await;
        }
        let all = Session::list_objects(self).await?;
        let (nodes, truncated) = pick_nodes(&all, scope, scan);
        let keys: HashSet<Key> = nodes.iter().map(key_of).collect();
        let found = match self.lineage_catalog().await {
            Ok(f) => f,
            Err(e @ (Error::Query(_) | Error::Statement(_) | Error::Unsupported(_))) => {
                tracing::debug!("{v:?}: lineage catalog unavailable: {e}");
                let mut g = graph_scan(self, database, scope, scan).await?;
                g.notes.insert(0, pr::unread_note(&e));
                return Ok(g);
            }
            Err(e) => return Err(e),
        };
        let mut g = LineageGraph {
            truncated,
            ..Default::default()
        };
        let mut seen = Seen::new();
        let mut external = found.external;
        for (user, d) in found.edges {
            if d.kind == "foreign_table" {
                let tk = key_of(&user);
                let fk = key(kinds::TABLE, d.schema.as_deref(), &d.name);
                if let Some((_, source)) = found.foreign.iter().find(|(k, _)| *k == fk) {
                    if keys.contains(&tk) {
                        external.push(LineageExternal {
                            source: source.clone(),
                            to: user,
                        });
                    }
                }
                continue;
            }
            let from = dep_ref(&d);
            let (fk, tk) = (key_of(&from), key_of(&user));
            if fk == tk || !keys.contains(&fk) || !keys.contains(&tk) {
                continue;
            }
            if seen.insert((fk, tk, format!("{:?}", d.relation))) {
                g.edges.push(LineageEdge {
                    from,
                    to: user,
                    relation: d.relation,
                    confidence: Confidence::Confirmed,
                    detail: d.detail,
                    mentions: Vec::new(),
                });
            }
        }
        g.external = external
            .into_iter()
            .filter(|x| keys.contains(&key_of(&x.to)))
            .collect();
        self.lineage_code(&nodes, &all, &keys, scan, &mut g, &mut seen)
            .await;
        if scope.jobs {
            self.lineage_jobs(database, scan, &keys, &mut g).await;
        }
        g.objects = nodes;
        Ok(g)
    }

    /// What the catalog of this variant says, whole database.
    async fn lineage_catalog(&self) -> Result<Found> {
        let v = self.variant;
        let c = self.pg_catalog();
        match v {
            Variant::Cockroach => {
                // Since v25 crdb_internal is closed unless the session opts in.
                let _ = self
                    .client
                    .batch_execute("SET allow_unsafe_internals = true")
                    .await;
                let rows = self.text(&cockroach_sql(|col| self.filter(col))).await;
                let _ = self
                    .client
                    .batch_execute("RESET allow_unsafe_internals")
                    .await;
                let mut f = split_rows(&rows?, "CockroachDB");
                self.add_foreign_keys(&mut f).await?;
                Ok(f)
            }
            Variant::Materialize => Ok(split_rows(
                &self.text(materialize_sql()).await?,
                "Materialize",
            )),
            Variant::RisingWave => {
                let mut f = split_rows(&self.text(risingwave_sql()).await?, "RisingWave");
                match self.text(&risingwave_connectors_sql()).await {
                    Ok(rows) => {
                        for r in &rows {
                            let (Some(kind), Some(name), Some(source)) =
                                (cell(r, "ukind"), cell(r, "uname"), rw_connector(r))
                            else {
                                continue;
                            };
                            f.external.push(LineageExternal {
                                source,
                                to: ObjectRef {
                                    kind,
                                    schema: cell(r, "usch"),
                                    name,
                                },
                            });
                        }
                    }
                    Err(e) => tracing::debug!("risingwave: connectors unavailable: {e}"),
                }
                Ok(f)
            }
            _ => {
                let rows = self
                    .text(&uses_sql(c, v != Variant::Redshift, |col| self.filter(col)))
                    .await?;
                let mut f = Found {
                    edges: rows.iter().filter_map(user_and_used).collect(),
                    ..Default::default()
                };
                self.add_foreign_keys(&mut f).await?;
                if c.foreign_tables() {
                    match self.text(&foreign_tables_sql(|col| self.filter(col))).await {
                        Ok(rows) => {
                            for r in &rows {
                                let source = pr::foreign_source(
                                    ["fdw", "srv", "srvopts", "ftopts", "sch", "name"]
                                        .map(|k| cell(r, k)),
                                );
                                if let (Some(source), Some(name)) = (source, cell(r, "name")) {
                                    f.foreign.push((
                                        key(kinds::TABLE, cell(r, "sch").as_deref(), &name),
                                        source,
                                    ));
                                }
                            }
                        }
                        Err(e) => tracing::debug!("{v:?}: foreign tables unavailable: {e}"),
                    }
                }
                Ok(f)
            }
        }
    }

    async fn add_foreign_keys(&self, f: &mut Found) -> Result<()> {
        let rows = self.text(&foreign_keys_sql(|col| self.filter(col))).await?;
        f.edges.extend(rows.iter().filter_map(user_and_used));
        Ok(())
    }

    /// The source of views and routines: lines for the catalog's edges and,
    /// for what the catalog doesn't track, the names it uses (Probable).
    async fn lineage_code(
        &mut self,
        nodes: &[ObjectRef],
        all: &[DbObject],
        keys: &HashSet<Key>,
        scan: &DependencyScan,
        g: &mut LineageGraph,
        seen: &mut Seen,
    ) {
        let v = self.variant;
        let batch = !matches!(
            v,
            Variant::Cockroach | Variant::Redshift | Variant::Materialize | Variant::RisingWave
        );
        let mut bodies: Vec<Body> = Vec::new();
        let mut one_by_one = !batch;
        if batch {
            let sql = bodies_sql(self.pg_catalog(), |col| self.filter(col), GRAPH_DEFINITIONS);
            match self.text(&sql).await {
                Ok(rows) => {
                    let mut grouped: Vec<(ObjectRef, bool, Vec<String>)> = Vec::new();
                    for r in &rows {
                        let (Some(kind), Some(name), Some(body)) =
                            (cell(r, "kind"), cell(r, "name"), cell(r, "body"))
                        else {
                            continue;
                        };
                        let object = ObjectRef {
                            kind,
                            schema: cell(r, "sch"),
                            name,
                        };
                        let tracked =
                            trusts_catalog(v) && cell(r, "tracked").as_deref() == Some("true");
                        match grouped
                            .iter_mut()
                            .find(|(o, _, _)| key_of(o) == key_of(&object))
                        {
                            Some((_, t, parts)) => {
                                *t &= tracked;
                                parts.push(body);
                            }
                            None => grouped.push((object, tracked, vec![body])),
                        }
                    }
                    for (object, tracked, parts) in grouped {
                        if let Some(text) = join_definitions(&parts) {
                            bodies.push(Body {
                                object,
                                text,
                                tracked,
                            });
                        }
                    }
                    if rows.len() >= GRAPH_DEFINITIONS {
                        g.notes.push(format!(
                            "se leyeron las primeras {GRAPH_DEFINITIONS} definiciones; el resto no aporta líneas al diagrama"
                        ));
                    }
                }
                Err(e) => {
                    tracing::debug!("{v:?}: sources unavailable: {e}");
                    one_by_one = true;
                }
            }
        }
        if one_by_one {
            self.bodies_one_by_one(nodes, scan, g, &mut bodies).await;
        }
        g.scanned += bodies.len() as u32;

        for b in &bodies {
            let bk = key_of(&b.object);
            if !keys.contains(&bk) {
                continue;
            }
            // The lines of the edges the catalog gave.
            for e in g.edges.iter_mut().filter(|e| key_of(&e.to) == bk) {
                if e.mentions.is_empty() {
                    let target = DependencyTarget {
                        object: e.from.clone(),
                        column: None,
                    };
                    if let Some((_, m)) = find_mentions(&b.text, &scan.dialect, &target) {
                        e.mentions = m;
                    }
                }
            }
            if b.tracked {
                continue;
            }
            let used = used_names(&b.text, &scan.dialect);
            for d in used_objects(&b.text, &used, all, &b.object) {
                let from = dep_ref(&d);
                let fk = key_of(&from);
                if fk == bk || !keys.contains(&fk) {
                    continue;
                }
                if seen.insert((fk, bk.clone(), format!("{:?}", Relation::Code))) {
                    g.edges.push(LineageEdge {
                        from,
                        to: b.object.clone(),
                        relation: Relation::Code,
                        confidence: d.confidence,
                        detail: None,
                        mentions: d.mentions,
                    });
                }
            }
        }
    }

    /// The same sources through `Session::definition`, for the variants
    /// whose catalog has no `pg_get_functiondef`.
    async fn bodies_one_by_one(
        &mut self,
        nodes: &[ObjectRef],
        scan: &DependencyScan,
        g: &mut LineageGraph,
        bodies: &mut Vec<Body>,
    ) {
        let mut read = 0usize;
        for n in nodes {
            if !scan.written_kinds.contains(&n.kind)
                || !matches!(
                    n.kind.as_str(),
                    kinds::VIEW | kinds::MATERIALIZED_VIEW | kinds::FUNCTION | kinds::PROCEDURE
                )
            {
                continue;
            }
            if read >= GRAPH_DEFINITIONS {
                g.notes.push(format!(
                    "se leyeron las primeras {GRAPH_DEFINITIONS} definiciones; el resto no aporta líneas al diagrama"
                ));
                break;
            }
            read += 1;
            match Session::definition(self, n).await {
                Ok(Some(text)) => bodies.push(Body {
                    object: n.clone(),
                    text,
                    tracked: false,
                }),
                Ok(None) => {}
                Err(_) => g.unreadable.push(match n.schema() {
                    Some(s) => format!("{s}.{}", n.name),
                    None => n.name.clone(),
                }),
            }
        }
    }

    /// The server's jobs, each with the objects its steps name.
    async fn lineage_jobs(
        &mut self,
        database: &str,
        scan: &DependencyScan,
        keys: &HashSet<Key>,
        g: &mut LineageGraph,
    ) {
        let list = match Session::jobs(self).await {
            Ok(l) => l,
            Err(Error::Unsupported(_)) => return,
            Err(e) => {
                g.job_notes
                    .push(format!("no se pudieron leer los jobs del servidor: {e}"));
                return;
            }
        };
        g.job_notes.extend(list.notes);
        let total = list.jobs.len();
        for job in list.jobs.into_iter().take(JOBS_READ) {
            let r = match Session::job_references(self, database, &job.id, scan).await {
                Ok(r) => r,
                Err(_) => {
                    g.unreadable.push(job.name);
                    continue;
                }
            };
            g.scanned += 1;
            let uses: Vec<Dependent> = r
                .items
                .into_iter()
                .filter(|d| keys.contains(&key(&d.kind, d.schema.as_deref(), &d.name)))
                .collect();
            if uses.is_empty() && r.external.is_empty() {
                continue;
            }
            g.jobs.push(LineageJob {
                id: job.id,
                name: job.name,
                kind: job.kind,
                group: job.group,
                uses,
                external: r.external,
            });
        }
        if total > JOBS_READ {
            g.job_notes.push(format!(
                "se leyeron los primeros {JOBS_READ} jobs del servidor, de {total}"
            ));
        }
    }
}

/// A row of [`risingwave_connectors_sql`] as the source it reads.
fn rw_connector(r: &SimpleQueryRow) -> Option<ExternalRef> {
    let (host, port) = (cell(r, "host"), cell(r, "port"));
    Some(ExternalRef {
        system: Some(cell(r, "sys")?),
        server: match (host, port) {
            (Some(h), Some(p)) if !h.contains(':') && !h.contains(',') => Some(format!("{h}:{p}")),
            (h, _) => h,
        },
        database: cell(r, "db"),
        schema: cell(r, "sch"),
        name: cell(r, "obj"),
        detail: None,
        mentions: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(v: i32) -> String {
        uses_sql(
            Catalog {
                version: v,
                full: true,
            },
            true,
            |c| format!("{c} NOT IN ('pg_catalog')"),
        )
    }

    #[test]
    fn one_query_reads_every_source_of_dependencies() {
        let q = all(160000);
        for part in [
            "FROM pg_rewrite r JOIN pg_class v ON v.oid = r.ev_class",
            "'pg_proc'::regclass, p.oid",
            "'pg_attrdef'::regclass",
            "'pg_trigger'::regclass, g.oid, 'trigger'",
            "'pg_policy'::regclass",
            "FROM pg_inherits i JOIN tbl t ON t.oid = i.inhrelid",
            "CASE WHEN tc.relispartition THEN 'PARTITION OF'",
        ] {
            assert!(q.contains(part), "{part}");
        }
        // A server of the old catalog has no prokind, policies or generated columns.
        let old = all(90400);
        assert!(!old.contains("pg_policy") && !old.contains("attgenerated"));
        assert!(old.contains("NOT p.proisagg"));
    }

    #[test]
    fn a_server_without_tables_in_its_catalog_reads_views_and_routines_only() {
        let q = uses_sql(
            Catalog {
                version: 80000,
                full: false,
            },
            false,
            |c| format!("{c} <> 'x'"),
        );
        assert!(q.contains("pg_rewrite") && q.contains("pg_proc"));
        assert!(!q.contains("pg_trigger") && !q.contains("pg_attrdef") && !q.contains("pg_index"));
    }

    #[test]
    fn the_sources_say_which_the_catalog_already_tracks() {
        let q = bodies_sql(
            Catalog {
                version: 160000,
                full: true,
            },
            |c| format!("{c} <> 'x'"),
            600,
        );
        assert!(q.contains("(p.prosqlbody IS NOT NULL)::text"));
        assert!(q.contains("pg_get_viewdef(v.oid, true)") && q.contains("LIMIT 600"));
        assert!(q.contains("l.lanname NOT IN ('c', 'internal')"));
        let old = bodies_sql(
            Catalog {
                version: 130000,
                full: true,
            },
            |c| c.to_string(),
            5,
        );
        assert!(!old.contains("prosqlbody"));
    }

    fn object(kind: &str, schema: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
            parent: None,
        }
    }

    #[test]
    fn nodes_follow_the_scope_and_the_limit() {
        let scan = DependencyScan {
            source_kinds: vec!["view".into(), "function".into()],
            dialect: dbine_driver::sql::ScriptDialect::postgres(),
            foreign_keys: true,
            written_kinds: vec!["view".into(), "function".into()],
            has_schemas: true,
        };
        let objects = [
            object("table", "ventas", "pedidos"),
            object("view", "ventas", "v"),
            object("sequence", "ventas", "s"),
            object("table", "otro", "x"),
        ];
        let (n, t) = pick_nodes(&objects, &LineageScope::default(), &scan);
        assert_eq!((n.len(), t), (3, false));
        let only = LineageScope {
            schemas: vec!["VENTAS".into()],
            kinds: vec!["table".into()],
            ..Default::default()
        };
        let (n, _) = pick_nodes(&objects, &only, &scan);
        assert_eq!(n.len(), 1);
        let few = LineageScope {
            limit: 2,
            ..Default::default()
        };
        let (n, t) = pick_nodes(&objects, &few, &scan);
        assert_eq!((n.len(), t), (2, true));
    }
}
