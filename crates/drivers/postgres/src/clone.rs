//! Same-engine clone (see `dbine_driver::transfer::CloneScript`): what makes
//! the target identical to the source for a set of tables, read from the
//! source's catalog and written as idempotent DDL.
//!
//! - **before:** schemas, the extensions the tables, their types, defaults,
//!   indexes and code use (only those the target offers,
//!   `pg_available_extensions`), user collations, enum / composite / domain
//!   / range types (in dependency order), sequences behind defaults, and
//!   the functions and procedures (`check_function_bodies` off, so their
//!   bodies may name what comes later).
//! - **each table:** columns (identity with its sequence options,
//!   generated columns, collations, defaults, `NOT NULL`), the primary key
//!   with its index options, declarative partitioning (the partitions that
//!   weren't listed are created before the table's data), `INHERITS`,
//!   `UNLOGGED`, access method and storage parameters; before its data,
//!   column storage, compression and statistics; after it, every other
//!   index (methods, options, partial, `INCLUDE`), unique constraints and
//!   the clustered index.
//! - **after:** foreign keys, checks and exclusion constraints, sequences
//!   owned by their columns, SQL-body functions and those that take a
//!   table's row type, views and materialized views (with their indexes) in
//!   dependency order (`pg_depend`), triggers with their enabled state, row
//!   level security and its policies, TOAST parameters, replica identity,
//!   comments, and last every sequence's current value (`setval`).
//!
//! Nothing names an owner, a tablespace or a grant: the target's objects
//! belong to whoever runs the script. Every statement can run again: `IF
//! NOT EXISTS`, `CREATE OR REPLACE`, or a `DO` block that ignores "already
//! exists". The target is only asked about its version, extensions, roles,
//! collations and access methods.
//!
//! Variants: PostgreSQL and the distributions that keep its catalog and
//! DDL (TimescaleDB, KingbaseES, AlloyDB, Cloud SQL, Aurora, EDB, Fujitsu).
//! Not: YugabyteDB (tablets and hash sharding aren't in PostgreSQL's
//! catalog), openGauss (a 9.2 catalog with its own storage options),
//! Greenplum and its forks (distribution policies), CockroachDB, Redshift,
//! Denodo, H2, CrateDB, Yellowbrick and the streaming engines (their
//! catalog or DDL isn't PostgreSQL's).

use crate::session::PgSession;
use crate::{err, Variant};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::transfer::{CloneScript, CloneTable};
use dbine_driver::{Error, ObjectRef, QueryOutcome, Result, Session};
use std::collections::{BTreeSet, HashMap, HashSet};

pub(crate) fn capable(v: Variant) -> bool {
    matches!(
        v,
        Variant::Postgres
            | Variant::Timescale
            | Variant::Kingbase
            | Variant::AlloyDb
            | Variant::CloudSql
            | Variant::Aurora
            | Variant::Edb
            | Variant::Fujitsu
    )
}

// ---------------------------------------------------------------- helpers

fn qi(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn qn(schema: &str, name: &str) -> String {
    format!("{}.{}", qi(schema), qi(name))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `stmt` in a `DO` block that ignores "already exists" (constraints,
/// triggers, policies, types), so it can run again.
pub(crate) fn guarded(stmt: &str) -> String {
    let mut tag = "$dbine$".to_string();
    let mut i = 0;
    while stmt.contains(&tag) {
        i += 1;
        tag = format!("$dbine{i}$");
    }
    format!("DO {tag} BEGIN {stmt}; EXCEPTION WHEN duplicate_object OR duplicate_table THEN NULL; END {tag}")
}

/// `CREATE [UNIQUE] INDEX` made idempotent, on the whole partitioned table.
pub(crate) fn index_if_not_exists(def: &str) -> String {
    let def = def.replacen(" ON ONLY ", " ON ", 1);
    if let Some(rest) = def.strip_prefix("CREATE UNIQUE INDEX ") {
        format!("CREATE UNIQUE INDEX IF NOT EXISTS {rest}")
    } else if let Some(rest) = def.strip_prefix("CREATE INDEX ") {
        format!("CREATE INDEX IF NOT EXISTS {rest}")
    } else {
        def
    }
}

/// A constraint definition with its index's storage parameters (which
/// `pg_get_constraintdef` leaves out) and without a tablespace.
pub(crate) fn constraint_def(def: &str, kind: &str, index_options: Option<&str>) -> String {
    let mut def = match def.find(" USING INDEX TABLESPACE ") {
        Some(i) => {
            // Anything after the tablespace name (DEFERRABLE…) stays.
            let rest = &def[i + " USING INDEX TABLESPACE ".len()..];
            let tail = rest.find(' ').map_or("", |j| &rest[j..]);
            format!("{}{tail}", &def[..i])
        }
        None => def.to_string(),
    };
    if let Some(opts) = index_options.filter(|o| !o.is_empty()) {
        let with = format!(" WITH ({opts})");
        let at = [" WHERE (", " DEFERRABLE", " NOT DEFERRABLE", " INITIALLY "]
            .iter()
            .filter(|m| kind == "x" || !m.starts_with(" WHERE"))
            .filter_map(|m| def.find(m))
            .min()
            .unwrap_or(def.len());
        def.insert_str(at, &with);
    }
    def
}

/// Dependency order (`deps` point at other nodes by index); a cycle is
/// broken where it's found.
pub(crate) fn topo(deps: &[Vec<usize>]) -> Vec<usize> {
    fn visit(i: usize, deps: &[Vec<usize>], state: &mut [u8], out: &mut Vec<usize>) {
        if state[i] != 0 {
            return;
        }
        state[i] = 1;
        for &d in &deps[i] {
            if d != i && d < deps.len() {
                visit(d, deps, state, out);
            }
        }
        state[i] = 2;
        out.push(i);
    }
    let mut state = vec![0u8; deps.len()];
    let mut out = Vec::with_capacity(deps.len());
    for i in 0..deps.len() {
        visit(i, deps, &mut state, &mut out);
    }
    out
}

fn storage_word(c: &str) -> Option<&'static str> {
    Some(match c {
        "p" => "PLAIN",
        "e" => "EXTERNAL",
        "m" => "MAIN",
        "x" => "EXTENDED",
        _ => return None,
    })
}

fn policy_cmd(c: &str) -> &'static str {
    match c {
        "r" => "SELECT",
        "a" => "INSERT",
        "w" => "UPDATE",
        "d" => "DELETE",
        _ => "ALL",
    }
}

// ---------------------------------------------------------------- the target

/// What the target offers (`None`: it couldn't be asked; assume it does).
#[derive(Default)]
struct Target {
    version: i32,
    extensions: Option<HashSet<String>>,
    roles: Option<HashSet<String>>,
    collations: Option<HashSet<String>>,
    access_methods: Option<HashSet<String>>,
}

fn has(set: &Option<HashSet<String>>, name: &str) -> bool {
    set.as_ref().is_none_or(|s| s.contains(name))
}

/// The first column of a query on the target (read only).
async fn target_column(t: &mut dyn Session, sql: &str) -> Option<Vec<String>> {
    let mut out = QueryOutcome::default();
    if let Err(e) = t.execute(sql, usize::MAX, &mut out).await {
        tracing::debug!("clone: target query failed: {e}");
        return None;
    }
    if out.error.is_some() {
        return None;
    }
    let r = out.results.into_iter().next()?;
    Some(r.rows.into_iter().filter_map(|row| row.into_iter().next()?.as_str().map(str::to_string)).collect())
}

async fn target_info(t: &mut dyn Session) -> Target {
    let set = |v: Option<Vec<String>>| v.map(|v| v.into_iter().collect::<HashSet<_>>());
    Target {
        version: target_column(t, "SELECT current_setting('server_version_num')")
            .await
            .and_then(|v| v.first()?.parse().ok())
            .unwrap_or(0),
        extensions: set(target_column(t, "SELECT name::text FROM pg_available_extensions").await),
        roles: set(target_column(t, "SELECT rolname::text FROM pg_roles").await),
        collations: set(
            target_column(t, "SELECT n.nspname || '.' || c.collname FROM pg_collation c JOIN pg_namespace n ON n.oid = c.collnamespace")
                .await,
        ),
        access_methods: set(target_column(t, "SELECT amname::text FROM pg_am").await),
    }
}

// ---------------------------------------------------------------- the source

struct Table {
    oid: u32,
    schema: String,
    name: String,
    unlogged: bool,
    partition_key: Option<String>,
    bound: Option<String>,
    parent: Option<u32>,
    options: Option<String>,
    toast_options: Option<String>,
    access_method: Option<String>,
    rls: bool,
    force_rls: bool,
    comment: Option<String>,
    inherits: Vec<u32>,
    typed: bool,
    tablespace: bool,
    replica_identity: String,
}

impl Table {
    fn qname(&self) -> String {
        qn(&self.schema, &self.name)
    }
}

struct Column {
    table: u32,
    name: String,
    type_name: String,
    not_null: bool,
    default: Option<String>,
    identity: String,
    generated: String,
    local: bool,
    collation: Option<(String, String)>,
    storage: Option<String>,
    compression: Option<String>,
    statistics: i32,
    comment: Option<String>,
}

struct Sequence {
    table: u32,
    column: Option<String>,
    /// `i` identity, `a` owned by a column, `n` only used by a default.
    dep: String,
    schema: String,
    name: String,
    type_name: String,
    start: i64,
    increment: i64,
    max: i64,
    min: i64,
    cache: i64,
    cycle: bool,
    last: Option<i64>,
    comment: Option<String>,
}

impl Sequence {
    fn options(&self) -> String {
        format!(
            "INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {} CACHE {} {}",
            self.increment,
            self.min,
            self.max,
            self.start,
            self.cache,
            if self.cycle { "CYCLE" } else { "NO CYCLE" }
        )
    }
}

struct Constraint {
    table: u32,
    name: String,
    kind: String,
    def: String,
    references: u32,
    inherited: bool,
    comment: Option<String>,
}

struct Index {
    table: u32,
    name: String,
    schema: String,
    def: String,
    clustered: bool,
    attached: bool,
    backs_constraint: bool,
    comment: Option<String>,
}

struct Function {
    oid: u32,
    kind: String,
    signature: String,
    def: Option<String>,
    /// Parsed at creation (SQL-standard body) or takes a table's row type:
    /// after the tables.
    late: bool,
    comment: Option<String>,
    types: Vec<u32>,
}

struct View {
    oid: u32,
    schema: String,
    name: String,
    kind: String,
    def: String,
    options: Option<String>,
    populated: bool,
    access_method: Option<String>,
    comment: Option<String>,
    rel_refs: Vec<u32>,
    func_refs: Vec<u32>,
}

async fn query(s: &PgSession, sql: &str, params: &[&(dyn tokio_postgres::types::ToSql + Sync)]) -> Result<Vec<tokio_postgres::Row>> {
    s.client.query(sql, params).await.map_err(err)
}

async fn tables(s: &PgSession, oids: &[u32]) -> Result<Vec<Table>> {
    let am = if s.version >= 120000 { "(SELECT a.amname::text FROM pg_am a WHERE a.oid = c.relam)" } else { "NULL::text" };
    let sql = format!(
        "SELECT c.oid, n.nspname::text, c.relname::text, c.relkind::text, c.relpersistence = 'u',
                CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END,
                CASE WHEN c.relispartition THEN pg_get_expr(c.relpartbound, c.oid) END,
                (SELECT i.inhparent FROM pg_inherits i WHERE i.inhrelid = c.oid AND c.relispartition LIMIT 1),
                array_to_string(c.reloptions, ', '), array_to_string(tc.reloptions, ', '), {am},
                c.relrowsecurity, c.relforcerowsecurity, obj_description(c.oid, 'pg_class'),
                ARRAY(SELECT i.inhparent FROM pg_inherits i WHERE i.inhrelid = c.oid AND NOT c.relispartition ORDER BY i.inhseqno),
                c.reloftype <> 0, c.reltablespace <> 0, c.relreplident::text
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace LEFT JOIN pg_class tc ON tc.oid = c.reltoastrelid
         WHERE c.oid = ANY($1::oid[])"
    );
    Ok(query(s, &sql, &[&oids])
        .await?
        .iter()
        .map(|r| Table {
            oid: r.get(0),
            schema: r.get(1),
            name: r.get(2),
            unlogged: r.get(4),
            partition_key: r.get(5),
            bound: r.get(6),
            parent: r.get(7),
            options: r.get::<_, Option<String>>(8).filter(|s| !s.is_empty()),
            toast_options: r.get::<_, Option<String>>(9).filter(|s| !s.is_empty()),
            access_method: r.get(10),
            rls: r.get(11),
            force_rls: r.get(12),
            comment: r.get(13),
            inherits: r.get(14),
            typed: r.get(15),
            tablespace: r.get(16),
            replica_identity: r.get(17),
        })
        .collect())
}

async fn columns(s: &PgSession, oids: &[u32]) -> Result<Vec<Column>> {
    let generated = if s.version >= 120000 { "a.attgenerated::text" } else { "''::text" };
    let compression = if s.version >= 140000 { "NULLIF(a.attcompression::text, '')" } else { "NULL::text" };
    let sql = format!(
        "SELECT a.attrelid, a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull,
                pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, {generated}, a.attislocal,
                CASE WHEN a.attcollation <> t.typcollation THEN cn.nspname::text END,
                CASE WHEN a.attcollation <> t.typcollation THEN co.collname::text END,
                CASE WHEN a.attstorage <> t.typstorage THEN a.attstorage::text END, {compression},
                COALESCE(a.attstattarget::int, -1), col_description(a.attrelid, a.attnum)
         FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
         LEFT JOIN pg_collation co ON co.oid = a.attcollation LEFT JOIN pg_namespace cn ON cn.oid = co.collnamespace
         WHERE a.attrelid = ANY($1::oid[]) AND a.attnum > 0 AND NOT a.attisdropped
         ORDER BY a.attrelid, a.attnum"
    );
    Ok(query(s, &sql, &[&oids])
        .await?
        .iter()
        .map(|r| Column {
            table: r.get(0),
            name: r.get(1),
            type_name: r.get(2),
            not_null: r.get(3),
            default: r.get(4),
            identity: r.get(5),
            generated: r.get(6),
            local: r.get(7),
            collation: r.get::<_, Option<String>>(8).zip(r.get::<_, Option<String>>(9)),
            storage: r.get(10),
            compression: r.get(11),
            statistics: r.get(12),
            comment: r.get(13),
        })
        .collect())
}

/// The tables' identity and serial sequences, those their defaults use, and
/// the free-standing ones in their schemas (code may use them by name).
async fn sequences(s: &PgSession, oids: &[u32], schemas: &[String]) -> Result<Vec<Sequence>> {
    let cols = "n.nspname::text, c.relname::text, format_type(q.seqtypid, NULL), q.seqstart, q.seqincrement, q.seqmax, q.seqmin,
                q.seqcache, q.seqcycle, CASE WHEN has_sequence_privilege(c.oid, 'SELECT') THEN pg_sequence_last_value(c.oid) END,
                obj_description(c.oid, 'pg_class')";
    let sql = format!(
        "SELECT d.refobjid, a.attname::text, d.deptype::text, {cols}
         FROM pg_depend d JOIN pg_sequence q ON q.seqrelid = d.objid JOIN pg_class c ON c.oid = q.seqrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         LEFT JOIN pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid
         WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass
           AND d.refobjid = ANY($1::oid[]) AND d.deptype IN ('a', 'i')
         UNION ALL
         SELECT ad.adrelid, NULL::text, 'n', {cols}
         FROM pg_attrdef ad JOIN pg_depend d ON d.classid = 'pg_attrdef'::regclass AND d.objid = ad.oid
              AND d.refclassid = 'pg_class'::regclass
         JOIN pg_sequence q ON q.seqrelid = d.refobjid JOIN pg_class c ON c.oid = q.seqrelid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE ad.adrelid = ANY($1::oid[])
         UNION ALL
         SELECT 0::oid, NULL::text, 'n', {cols}
         FROM pg_sequence q JOIN pg_class c ON c.oid = q.seqrelid JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = ANY($2::text[])
           AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid
                           AND d.deptype IN ('a', 'i', 'e'))"
    );
    let mut out: Vec<Sequence> = Vec::new();
    for r in query(s, &sql, &[&oids, &schemas]).await? {
        let seq = Sequence {
            table: r.get(0),
            column: r.get(1),
            dep: r.get(2),
            schema: r.get(3),
            name: r.get(4),
            type_name: r.get(5),
            start: r.get(6),
            increment: r.get(7),
            max: r.get(8),
            min: r.get(9),
            cache: r.get(10),
            cycle: r.get(11),
            last: r.get(12),
            comment: r.get(13),
        };
        // Owned (or identity) wins over "used by a default".
        match out.iter_mut().find(|x| x.schema == seq.schema && x.name == seq.name) {
            Some(x) if x.dep == "n" && seq.dep != "n" => *x = seq,
            Some(_) => {}
            None => out.push(seq),
        }
    }
    Ok(out)
}

async fn constraints(s: &PgSession, oids: &[u32]) -> Result<Vec<Constraint>> {
    let parent = if s.version >= 110000 { "con.conparentid <> 0" } else { "false" };
    let sql = format!(
        "SELECT con.conrelid, con.conname::text, con.contype::text, pg_get_constraintdef(con.oid), con.confrelid,
                NOT con.conislocal OR {parent}, obj_description(con.oid, 'pg_constraint'),
                (SELECT array_to_string(ic.reloptions, ', ') FROM pg_class ic WHERE ic.oid = con.conindid AND con.contype IN ('p', 'u', 'x'))
         FROM pg_constraint con
         WHERE con.conrelid = ANY($1::oid[]) AND con.contype IN ('p', 'u', 'f', 'c', 'x')
         ORDER BY con.conrelid, con.contype, con.conname"
    );
    Ok(query(s, &sql, &[&oids])
        .await?
        .iter()
        .map(|r| {
            let kind: String = r.get(2);
            let def: String = r.get(3);
            Constraint {
                table: r.get(0),
                name: r.get(1),
                def: constraint_def(&def, &kind, r.get::<_, Option<String>>(7).as_deref()),
                kind,
                references: r.get(4),
                inherited: r.get(5),
                comment: r.get(6),
            }
        })
        .collect())
}

async fn indexes(s: &PgSession, oids: &[u32]) -> Result<Vec<Index>> {
    let sql = "SELECT i.indrelid, ic.relname::text, n.nspname::text, pg_get_indexdef(i.indexrelid), i.indisclustered,
                      EXISTS (SELECT 1 FROM pg_inherits h WHERE h.inhrelid = i.indexrelid),
                      EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = i.indexrelid AND c.conrelid = i.indrelid
                              AND c.contype IN ('p', 'u', 'x')),
                      obj_description(i.indexrelid, 'pg_class')
               FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid JOIN pg_namespace n ON n.oid = ic.relnamespace
               WHERE i.indrelid = ANY($1::oid[])
               ORDER BY i.indrelid, ic.relname";
    Ok(query(s, sql, &[&oids])
        .await?
        .iter()
        .map(|r| Index {
            table: r.get(0),
            name: r.get(1),
            schema: r.get(2),
            def: r.get(3),
            clustered: r.get(4),
            attached: r.get(5),
            backs_constraint: r.get(6),
            comment: r.get(7),
        })
        .collect())
}

struct Trigger {
    table: u32,
    name: String,
    def: String,
    enabled: String,
    comment: Option<String>,
}

async fn triggers(s: &PgSession, oids: &[u32]) -> Result<(Vec<Trigger>, Vec<u32>)> {
    let cloned = if s.version >= 130000 { "AND t.tgparentid = 0" } else { "" };
    let sql = format!(
        "SELECT t.tgrelid, t.tgname::text, pg_get_triggerdef(t.oid), t.tgenabled::text, obj_description(t.oid, 'pg_trigger'), t.tgfoid
         FROM pg_trigger t WHERE t.tgrelid = ANY($1::oid[]) AND NOT t.tgisinternal {cloned}
         ORDER BY t.tgrelid, t.tgname"
    );
    let rows = query(s, &sql, &[&oids]).await?;
    let funcs = rows.iter().map(|r| r.get::<_, u32>(5)).collect();
    Ok((
        rows.iter()
            .map(|r| Trigger { table: r.get(0), name: r.get(1), def: r.get(2), enabled: r.get(3), comment: r.get(4) })
            .collect(),
        funcs,
    ))
}

struct Policy {
    table: u32,
    name: String,
    permissive: bool,
    cmd: String,
    roles: Vec<String>,
    public: bool,
    using: Option<String>,
    check: Option<String>,
    comment: Option<String>,
}

async fn policies(s: &PgSession, oids: &[u32]) -> Result<Vec<Policy>> {
    let sql = "SELECT p.polrelid, p.polname::text, p.polpermissive, p.polcmd::text,
                      ARRAY(SELECT r.rolname::text FROM pg_roles r WHERE r.oid = ANY(p.polroles) ORDER BY 1),
                      0::oid = ANY(p.polroles),
                      pg_get_expr(p.polqual, p.polrelid), pg_get_expr(p.polwithcheck, p.polrelid),
                      obj_description(p.oid, 'pg_policy')
               FROM pg_policy p WHERE p.polrelid = ANY($1::oid[]) ORDER BY p.polrelid, p.polname";
    Ok(query(s, sql, &[&oids])
        .await?
        .iter()
        .map(|r| Policy {
            table: r.get(0),
            name: r.get(1),
            permissive: r.get(2),
            cmd: r.get(3),
            roles: r.get(4),
            public: r.get(5),
            using: r.get(6),
            check: r.get(7),
            comment: r.get(8),
        })
        .collect())
}

/// Functions the tables use (defaults, checks, indexes, triggers,
/// policies).
async fn used_functions(s: &PgSession, oids: &[u32]) -> Result<Vec<u32>> {
    let sql = "SELECT DISTINCT d.refobjid FROM pg_depend d
               WHERE d.refclassid = 'pg_proc'::regclass AND (
                    (d.classid = 'pg_attrdef'::regclass AND d.objid IN (SELECT oid FROM pg_attrdef WHERE adrelid = ANY($1::oid[])))
                 OR (d.classid = 'pg_constraint'::regclass AND d.objid IN (SELECT oid FROM pg_constraint WHERE conrelid = ANY($1::oid[])))
                 OR (d.classid = 'pg_class'::regclass AND d.objid IN (SELECT indexrelid FROM pg_index WHERE indrelid = ANY($1::oid[])))
                 OR (d.classid = 'pg_trigger'::regclass AND d.objid IN (SELECT oid FROM pg_trigger WHERE tgrelid = ANY($1::oid[])))
                 OR (d.classid = 'pg_policy'::regclass AND d.objid IN (SELECT oid FROM pg_policy WHERE polrelid = ANY($1::oid[]))))";
    Ok(query(s, sql, &[&oids]).await?.iter().map(|r| r.get(0)).collect())
}

async fn functions(s: &PgSession, schemas: &[String], extra: &[u32]) -> Result<Vec<Function>> {
    let kind = if s.version >= 110000 {
        "p.prokind::text"
    } else {
        "CASE WHEN p.proisagg THEN 'a' WHEN p.proiswindow THEN 'w' ELSE 'f' END"
    };
    let body = if s.version >= 140000 { "p.prosqlbody IS NOT NULL" } else { "false" };
    let sql = format!(
        "SELECT p.oid, {kind}, quote_ident(n.nspname) || '.' || quote_ident(p.proname) || '(' || pg_get_function_identity_arguments(p.oid) || ')',
                CASE WHEN {kind} <> 'a' THEN pg_get_functiondef(p.oid) END,
                {body} OR EXISTS (
                    SELECT 1 FROM unnest(ARRAY[p.prorettype] || p.proargtypes::oid[] || COALESCE(p.proallargtypes, '{{}}'::oid[])) u(t)
                    JOIN pg_type ty ON ty.oid = u.t OR ty.typarray = u.t
                    JOIN pg_class rc ON rc.oid = ty.typrelid WHERE rc.relkind IN ('r', 'p', 'v', 'm', 'f')),
                obj_description(p.oid, 'pg_proc'),
                ARRAY[p.prorettype] || p.proargtypes::oid[] || COALESCE(p.proallargtypes, '{{}}'::oid[])
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
         WHERE (n.nspname = ANY($1::text[]) OR p.oid = ANY($2::oid[]))
           AND n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND NOT EXISTS (SELECT 1 FROM pg_depend e WHERE e.classid = 'pg_proc'::regclass AND e.objid = p.oid AND e.deptype = 'e')
         ORDER BY n.nspname, p.proname, p.oid"
    );
    Ok(query(s, &sql, &[&schemas, &extra])
        .await?
        .iter()
        .map(|r| Function {
            oid: r.get(0),
            kind: r.get(1),
            signature: r.get(2),
            def: r.get(3),
            late: r.get(4),
            comment: r.get(5),
            types: r.get(6),
        })
        .collect())
}

/// Relations and functions each function's body depends on (SQL-standard
/// bodies record them).
async fn function_deps(s: &PgSession, oids: &[u32]) -> Result<Vec<(u32, bool, u32)>> {
    let sql = "SELECT d.objid, d.refclassid = 'pg_proc'::regclass, d.refobjid FROM pg_depend d
               WHERE d.classid = 'pg_proc'::regclass AND d.objid = ANY($1::oid[])
                 AND d.refclassid IN ('pg_proc'::regclass, 'pg_class'::regclass)";
    Ok(query(s, sql, &[&oids]).await?.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect())
}

async fn views(s: &PgSession, schemas: &[String]) -> Result<Vec<View>> {
    let am = if s.version >= 120000 { "(SELECT a.amname::text FROM pg_am a WHERE a.oid = c.relam)" } else { "NULL::text" };
    let refs = |class: &str| {
        format!(
            "ARRAY(SELECT DISTINCT d.refobjid FROM pg_rewrite r JOIN pg_depend d ON d.classid = 'pg_rewrite'::regclass
                   AND d.objid = r.oid AND d.refclassid = '{class}'::regclass WHERE r.ev_class = c.oid AND d.refobjid <> c.oid)"
        )
    };
    let sql = format!(
        "SELECT c.oid, n.nspname::text, c.relname::text, c.relkind::text, pg_get_viewdef(c.oid), array_to_string(c.reloptions, ', '),
                c.relispopulated, {am}, obj_description(c.oid, 'pg_class'), {}, {}
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relkind IN ('v', 'm') AND n.nspname = ANY($1::text[])
           AND NOT EXISTS (SELECT 1 FROM pg_depend e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid AND e.deptype = 'e')
         ORDER BY n.nspname, c.relname",
        refs("pg_class"),
        refs("pg_proc")
    );
    Ok(query(s, &sql, &[&schemas])
        .await?
        .iter()
        .map(|r| View {
            oid: r.get(0),
            schema: r.get(1),
            name: r.get(2),
            kind: r.get(3),
            def: r.get::<_, String>(4).trim().trim_end_matches(';').to_string(),
            options: r.get::<_, Option<String>>(5).filter(|s| !s.is_empty()),
            populated: r.get(6),
            access_method: r.get(7),
            comment: r.get(8),
            rel_refs: r.get(9),
            func_refs: r.get(10),
        })
        .collect())
}

async fn column_comments(s: &PgSession, oids: &[u32]) -> Result<Vec<(u32, String, String)>> {
    let sql = "SELECT a.attrelid, a.attname::text, col_description(a.attrelid, a.attnum) FROM pg_attribute a
               WHERE a.attrelid = ANY($1::oid[]) AND a.attnum > 0 AND NOT a.attisdropped
                 AND col_description(a.attrelid, a.attnum) IS NOT NULL ORDER BY a.attrelid, a.attnum";
    Ok(query(s, sql, &[&oids]).await?.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect())
}

struct UserType {
    oid: u32,
    schema: String,
    name: String,
    kind: String,
    labels: Option<Vec<String>>,
    attributes: Option<String>,
    base: Option<String>,
    not_null: bool,
    default: Option<String>,
    collation: Option<String>,
    checks: Option<String>,
    range: Option<String>,
    comment: Option<String>,
    deps: Vec<u32>,
}

impl UserType {
    fn create(&self) -> Option<String> {
        let name = qn(&self.schema, &self.name);
        let stmt = match self.kind.as_str() {
            "e" => {
                let labels = self.labels.as_deref().unwrap_or_default().iter().map(|l| lit(l)).collect::<Vec<_>>().join(", ");
                format!("CREATE TYPE {name} AS ENUM ({labels})")
            }
            "c" => format!("CREATE TYPE {name} AS ({})", self.attributes.as_deref().unwrap_or_default()),
            "d" => {
                let mut s = format!("CREATE DOMAIN {name} AS {}", self.base.as_deref()?);
                if let Some(c) = &self.collation {
                    s.push_str(&format!(" COLLATE {c}"));
                }
                if let Some(d) = &self.default {
                    s.push_str(&format!(" DEFAULT {d}"));
                }
                if self.not_null {
                    s.push_str(" NOT NULL");
                }
                if let Some(c) = self.checks.as_deref().filter(|c| !c.is_empty()) {
                    s.push_str(&format!(" {c}"));
                }
                s
            }
            "r" => format!("CREATE TYPE {name} AS RANGE (SUBTYPE = {})", self.range.as_deref()?),
            _ => return None,
        };
        Some(guarded(&stmt))
    }
}

async fn user_types(s: &PgSession, seed: &[u32]) -> Result<Vec<UserType>> {
    let sql = "WITH RECURSIVE t(oid) AS (
                   SELECT unnest($1::oid[])
                   UNION
                   SELECT x.oid FROM t JOIN pg_type ty ON ty.oid = t.oid
                   CROSS JOIN LATERAL (
                       SELECT ty.typelem WHERE ty.typelem <> 0
                       UNION ALL SELECT ty.typbasetype WHERE ty.typbasetype <> 0
                       UNION ALL SELECT a.atttypid FROM pg_attribute a JOIN pg_class rc ON rc.oid = a.attrelid AND rc.relkind = 'c'
                                 WHERE a.attrelid = ty.typrelid AND a.attnum > 0 AND NOT a.attisdropped
                       UNION ALL SELECT r.rngsubtype FROM pg_range r WHERE r.rngtypid = ty.oid
                   ) x(oid)
               )
               SELECT ty.oid, n.nspname::text, ty.typname::text, ty.typtype::text,
                      CASE WHEN ty.typtype = 'e' THEN ARRAY(SELECT e.enumlabel::text FROM pg_enum e WHERE e.enumtypid = ty.oid ORDER BY e.enumsortorder) END,
                      CASE WHEN ty.typtype = 'c' THEN (
                          SELECT string_agg(quote_ident(a.attname) || ' ' || format_type(a.atttypid, a.atttypmod)
                                 || CASE WHEN a.attcollation <> at.typcollation THEN ' COLLATE ' || quote_ident(cn.nspname) || '.' || quote_ident(co.collname) ELSE '' END,
                                 ', ' ORDER BY a.attnum)
                          FROM pg_attribute a JOIN pg_type at ON at.oid = a.atttypid
                          LEFT JOIN pg_collation co ON co.oid = a.attcollation LEFT JOIN pg_namespace cn ON cn.oid = co.collnamespace
                          WHERE a.attrelid = ty.typrelid AND a.attnum > 0 AND NOT a.attisdropped) END,
                      CASE WHEN ty.typtype = 'd' THEN format_type(ty.typbasetype, ty.typtypmod) END,
                      ty.typnotnull, ty.typdefault,
                      CASE WHEN ty.typtype = 'd' AND ty.typcollation <> bt.typcollation THEN quote_ident(dcn.nspname) || '.' || quote_ident(dco.collname) END,
                      CASE WHEN ty.typtype = 'd' THEN (SELECT string_agg('CONSTRAINT ' || quote_ident(c.conname) || ' ' || pg_get_constraintdef(c.oid), ' ' ORDER BY c.conname)
                                                       FROM pg_constraint c WHERE c.contypid = ty.oid AND c.contype = 'c') END,
                      CASE WHEN ty.typtype = 'r' THEN (SELECT format_type(r.rngsubtype, NULL)
                               || CASE WHEN r.rngsubdiff <> 0 THEN ', SUBTYPE_DIFF = ' || r.rngsubdiff::regproc::text ELSE '' END
                               FROM pg_range r WHERE r.rngtypid = ty.oid) END,
                      obj_description(ty.oid, 'pg_type'),
                      ARRAY(SELECT CASE WHEN dt.typcategory = 'A' THEN dt.typelem ELSE dt.oid END FROM pg_type dt
                            WHERE dt.oid = ty.typbasetype
                               OR dt.oid IN (SELECT a.atttypid FROM pg_attribute a WHERE ty.typrelid <> 0 AND a.attrelid = ty.typrelid AND a.attnum > 0)
                               OR dt.oid IN (SELECT r.rngsubtype FROM pg_range r WHERE r.rngtypid = ty.oid))
               FROM t JOIN pg_type ty ON ty.oid = t.oid JOIN pg_namespace n ON n.oid = ty.typnamespace
               LEFT JOIN pg_type bt ON bt.oid = ty.typbasetype
               LEFT JOIN pg_collation dco ON dco.oid = ty.typcollation LEFT JOIN pg_namespace dcn ON dcn.oid = dco.collnamespace
               WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND ty.typtype IN ('e', 'c', 'd', 'r')
                 AND (ty.typtype <> 'c' OR EXISTS (SELECT 1 FROM pg_class rc WHERE rc.oid = ty.typrelid AND rc.relkind = 'c'))
                 AND NOT EXISTS (SELECT 1 FROM pg_depend e WHERE e.classid = 'pg_type'::regclass AND e.objid = ty.oid AND e.deptype = 'e')";
    Ok(query(s, sql, &[&seed])
        .await?
        .iter()
        .map(|r| UserType {
            oid: r.get(0),
            schema: r.get(1),
            name: r.get(2),
            kind: r.get(3),
            labels: r.get(4),
            attributes: r.get(5),
            base: r.get(6),
            not_null: r.get(7),
            default: r.get(8),
            collation: r.get(9),
            checks: r.get(10),
            range: r.get(11),
            comment: r.get(12),
            deps: r.get(13),
        })
        .collect())
}

struct Collation {
    schema: String,
    name: String,
    provider: String,
    collate: Option<String>,
    ctype: Option<String>,
    locale: Option<String>,
    deterministic: bool,
}

async fn collations(s: &PgSession, tables: &[u32], types: &[u32]) -> Result<Vec<Collation>> {
    let locale = if s.version >= 170000 {
        "co.colllocale"
    } else if s.version >= 150000 {
        "co.colliculocale"
    } else {
        "NULL::text"
    };
    let det = if s.version >= 120000 { "co.collisdeterministic" } else { "true" };
    let sql = format!(
        "SELECT DISTINCT n.nspname::text, co.collname::text, co.collprovider::text, co.collcollate::text, co.collctype::text, {locale}::text, {det}
         FROM pg_collation co JOIN pg_namespace n ON n.oid = co.collnamespace
         WHERE co.oid <> 100 AND co.oid IN (
             SELECT a.attcollation FROM pg_attribute a WHERE a.attrelid = ANY($1::oid[])
             UNION SELECT ty.typcollation FROM pg_type ty WHERE ty.oid = ANY($2::oid[])
             UNION SELECT a.attcollation FROM pg_attribute a JOIN pg_type ty ON ty.typrelid = a.attrelid WHERE ty.oid = ANY($2::oid[]))"
    );
    Ok(query(s, &sql, &[&tables, &types])
        .await?
        .iter()
        .map(|r| Collation {
            schema: r.get(0),
            name: r.get(1),
            provider: r.get(2),
            collate: r.get(3),
            ctype: r.get(4),
            locale: r.get(5),
            deterministic: r.get(6),
        })
        .collect())
}

impl Collation {
    fn create(&self) -> Option<String> {
        let name = qn(&self.schema, &self.name);
        let opts = match self.provider.as_str() {
            "i" => {
                let locale = self.locale.clone().or_else(|| self.collate.clone())?;
                format!("provider = icu, locale = {}, deterministic = {}", lit(&locale), self.deterministic)
            }
            "c" => format!(
                "provider = libc, lc_collate = {}, lc_ctype = {}",
                lit(self.collate.as_deref()?),
                lit(self.ctype.as_deref().or(self.collate.as_deref())?)
            ),
            "b" => format!("provider = builtin, locale = {}", lit(self.locale.as_deref()?)),
            _ => return None,
        };
        Some(format!("CREATE COLLATION IF NOT EXISTS {name} ({opts})"))
    }
}

/// The extensions (name, schema) the objects depend on.
async fn extensions(s: &PgSession, rels: &[u32], funcs: &[u32], types: &[u32]) -> Result<Vec<(String, String)>> {
    let sql = "WITH objs(classid, objid) AS (
                   SELECT 'pg_class'::regclass::oid, x FROM unnest($1::oid[]) x
                   UNION ALL SELECT 'pg_class'::regclass::oid, i.indexrelid FROM pg_index i WHERE i.indrelid = ANY($1::oid[])
                   UNION ALL SELECT 'pg_attrdef'::regclass::oid, d.oid FROM pg_attrdef d WHERE d.adrelid = ANY($1::oid[])
                   UNION ALL SELECT 'pg_constraint'::regclass::oid, c.oid FROM pg_constraint c WHERE c.conrelid = ANY($1::oid[])
                   UNION ALL SELECT 'pg_trigger'::regclass::oid, t.oid FROM pg_trigger t WHERE t.tgrelid = ANY($1::oid[])
                   UNION ALL SELECT 'pg_rewrite'::regclass::oid, r.oid FROM pg_rewrite r WHERE r.ev_class = ANY($1::oid[])
                   UNION ALL SELECT 'pg_proc'::regclass::oid, x FROM unnest($2::oid[]) x
                   UNION ALL SELECT 'pg_type'::regclass::oid, x FROM unnest($3::oid[]) x
               ), refs AS (
                   SELECT d.refclassid, d.refobjid FROM pg_depend d JOIN objs o ON o.classid = d.classid AND o.objid = d.objid
               ), refs2 AS (
                   SELECT refclassid, refobjid FROM refs
                   UNION SELECT 'pg_type'::regclass::oid, ty.typelem FROM refs
                         JOIN pg_type ty ON refs.refclassid = 'pg_type'::regclass AND ty.oid = refs.refobjid AND ty.typelem <> 0
               )
               SELECT DISTINCT e.extname::text, n.nspname::text FROM refs2 r
               JOIN pg_depend x ON x.classid = r.refclassid AND x.objid = r.refobjid AND x.deptype = 'e'
               JOIN pg_extension e ON e.oid = x.refobjid JOIN pg_namespace n ON n.oid = e.extnamespace
               WHERE e.extname <> 'plpgsql' ORDER BY 1";
    Ok(query(s, sql, &[&rels, &funcs, &types]).await?.iter().map(|r| (r.get(0), r.get(1))).collect())
}

async fn schema_comments(s: &PgSession, schemas: &[String]) -> Result<Vec<(String, String)>> {
    let sql = "SELECT nspname::text, obj_description(oid, 'pg_namespace') FROM pg_namespace
               WHERE nspname = ANY($1::text[]) AND nspname <> 'public' AND obj_description(oid, 'pg_namespace') IS NOT NULL";
    Ok(query(s, sql, &[&schemas]).await?.iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// The listed tables' oids, and the partitions below / parents above them.
async fn resolve(s: &PgSession, tables: &[ObjectRef]) -> Result<Vec<u32>> {
    let mut oids = Vec::new();
    for t in tables {
        let name = match t.schema() {
            Some(sc) => qn(sc, &t.name),
            None => qi(&t.name),
        };
        let row = s
            .client
            .query_one("SELECT c.oid, c.relkind::text FROM pg_class c WHERE c.oid = to_regclass($1::text)", &[&name])
            .await
            .ok()
            .ok_or_else(|| Error::Query(format!("no existe la tabla {name}")))?;
        let kind: String = row.get(1);
        if kind != "r" && kind != "p" {
            return Err(Error::Query(format!("{name} no es una tabla")));
        }
        let oid: u32 = row.get(0);
        if !oids.contains(&oid) {
            oids.push(oid);
        }
    }
    Ok(oids)
}

async fn partition_family(s: &PgSession, oids: &[u32]) -> Result<(Vec<u32>, Vec<u32>)> {
    let below = "WITH RECURSIVE p(oid) AS (
                     SELECT i.inhrelid FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid AND c.relispartition
                     WHERE i.inhparent = ANY($1::oid[])
                     UNION SELECT i.inhrelid FROM pg_inherits i JOIN p ON i.inhparent = p.oid
                     JOIN pg_class c ON c.oid = i.inhrelid AND c.relispartition)
                 SELECT oid FROM p";
    let above = "WITH RECURSIVE a(oid) AS (
                     SELECT i.inhparent FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid AND c.relispartition
                     WHERE i.inhrelid = ANY($1::oid[])
                     UNION SELECT i.inhparent FROM pg_inherits i JOIN a ON i.inhrelid = a.oid
                     JOIN pg_class c ON c.oid = i.inhrelid AND c.relispartition)
                 SELECT oid FROM a";
    let b = query(s, below, &[&oids]).await?.iter().map(|r| r.get(0)).collect();
    let a = query(s, above, &[&oids]).await?.iter().map(|r| r.get(0)).collect();
    Ok((b, a))
}

// ---------------------------------------------------------------- the script

struct Ctx<'a> {
    tables: &'a HashMap<u32, Table>,
    columns: &'a [Column],
    sequences: &'a [Sequence],
    constraints: &'a [Constraint],
    target: &'a Target,
    notes: &'a mut BTreeSet<String>,
}

impl Ctx<'_> {
    fn collation_ok(&mut self, schema: &str, name: &str, created: &HashSet<String>) -> bool {
        let full = format!("{schema}.{name}");
        if created.contains(&full) || has(&self.target.collations, &full) {
            return true;
        }
        self.notes.insert(format!("El destino no tiene la collation «{full}»: esas columnas usan la de la base."));
        false
    }

    fn column_def(&mut self, c: &Column, created: &HashSet<String>) -> String {
        let mut d = format!("{} {}", qi(&c.name), c.type_name);
        if let Some((sc, n)) = &c.collation {
            if self.collation_ok(sc, n, created) {
                d.push_str(&format!(" COLLATE {}", qn(sc, n)));
            }
        }
        match (c.generated.as_str(), c.identity.as_str()) {
            ("s", _) => d.push_str(&format!(" GENERATED ALWAYS AS ({}) STORED", c.default.as_deref().unwrap_or("NULL"))),
            ("v", _) => d.push_str(&format!(" GENERATED ALWAYS AS ({}) VIRTUAL", c.default.as_deref().unwrap_or("NULL"))),
            (_, "a" | "d") => {
                let how = if c.identity == "a" { "ALWAYS" } else { "BY DEFAULT" };
                d.push_str(&format!(" GENERATED {how} AS IDENTITY"));
                if let Some(q) =
                    self.sequences.iter().find(|q| q.dep == "i" && q.table == c.table && q.column.as_deref() == Some(c.name.as_str()))
                {
                    d.push_str(&format!(" (SEQUENCE NAME {} {})", qn(&q.schema, &q.name), q.options()));
                }
            }
            _ => {
                if let Some(def) = &c.default {
                    d.push_str(&format!(" DEFAULT {def}"));
                }
            }
        }
        if c.not_null {
            d.push_str(" NOT NULL");
        }
        d
    }

    /// The table's `CREATE` (columns, key, partitioning, storage).
    fn create(&mut self, t: &Table, created: &HashSet<String>) -> String {
        let name = t.qname();
        let unlogged = if t.unlogged { "UNLOGGED " } else { "" };
        let pk = self.constraints.iter().find(|c| c.table == t.oid && c.kind == "p" && !c.inherited);
        let pk = pk.map(|c| format!("CONSTRAINT {} {}", qi(&c.name), c.def));
        let mut tail = String::new();
        if let Some(k) = &t.partition_key {
            tail.push_str(&format!(" PARTITION BY {k}"));
        }
        if let Some(am) = t.access_method.as_deref().filter(|a| *a != "heap") {
            if has(&self.target.access_methods, am) {
                tail.push_str(&format!(" USING {}", qi(am)));
            } else {
                self.notes.insert(format!("El destino no tiene el método de acceso «{am}»: {name} queda como heap."));
            }
        }
        if let Some(o) = &t.options {
            tail.push_str(&format!(" WITH ({o})"));
        }
        if let (Some(parent), Some(bound)) = (t.parent.and_then(|p| self.tables.get(&p)), &t.bound) {
            let elems = pk.map(|p| format!(" ({p})")).unwrap_or_default();
            return format!("CREATE {unlogged}TABLE IF NOT EXISTS {name} PARTITION OF {}{elems} {bound}{tail}", parent.qname());
        }
        let parents: Vec<&Table> = t.inherits.iter().filter_map(|p| self.tables.get(p)).collect();
        let inherits = !t.inherits.is_empty() && parents.len() == t.inherits.len();
        if !t.inherits.is_empty() && !inherits {
            self.notes.insert(format!("{name} hereda de tablas que no se clonan: se crea con todas sus columnas y sin INHERITS."));
        }
        if t.typed {
            self.notes.insert(format!("{name} es una tabla con tipo (OF): se crea como tabla común con las mismas columnas."));
        }
        let cols: Vec<&Column> = self.columns.iter().filter(|c| c.table == t.oid && (c.local || !inherits)).collect();
        let mut elems: Vec<String> = cols.iter().map(|c| self.column_def(c, created)).collect();
        elems.extend(pk);
        let mut s = format!("CREATE {unlogged}TABLE IF NOT EXISTS {name} (\n    {}\n)", elems.join(",\n    "));
        if inherits {
            s.push_str(&format!(" INHERITS ({})", parents.iter().map(|p| p.qname()).collect::<Vec<_>>().join(", ")));
        }
        s.push_str(&tail);
        s
    }

    /// Column storage, compression and statistics (before the data).
    fn column_settings(&self, t: &Table) -> Vec<String> {
        let name = t.qname();
        let mut out = Vec::new();
        for c in self.columns.iter().filter(|c| c.table == t.oid) {
            if let Some(w) = c.storage.as_deref().and_then(storage_word) {
                out.push(format!("ALTER TABLE {name} ALTER COLUMN {} SET STORAGE {w}", qi(&c.name)));
            }
            if let Some(m) = &c.compression {
                let m = if m == "l" { "lz4" } else { "pglz" };
                if self.target.version == 0 || self.target.version >= 140000 {
                    out.push(format!("ALTER TABLE {name} ALTER COLUMN {} SET COMPRESSION {m}", qi(&c.name)));
                }
            }
            if c.statistics >= 0 {
                out.push(format!("ALTER TABLE {name} ALTER COLUMN {} SET STATISTICS {}", qi(&c.name), c.statistics));
            }
        }
        out
    }
}

/// How far below a root (parent tables first).
fn depth(tables: &HashMap<u32, Table>, oid: u32) -> usize {
    let mut d = 0;
    let mut cur = oid;
    let mut seen = HashSet::new();
    while let Some(t) = tables.get(&cur) {
        if !seen.insert(cur) {
            break;
        }
        match t.parent.or_else(|| t.inherits.iter().copied().find(|p| tables.contains_key(p))) {
            Some(p) => {
                d += 1;
                cur = p;
            }
            None => break,
        }
    }
    d
}

pub(crate) async fn script(v: Variant, source: &mut dyn Session, target: &mut dyn Session, tables: &[ObjectRef]) -> Result<CloneScript> {
    if !capable(v) {
        return Err(Error::Unsupported(format!("{} no clona bases", v.info().name)));
    }
    let tgt = target_info(target).await;
    let s = source
        .as_any()
        .and_then(|a| a.downcast_mut::<PgSession>())
        .ok_or_else(|| Error::Unsupported("el origen no es una sesión de PostgreSQL".into()))?;
    s.require_extended()?;
    if s.version > 0 && s.version < 100000 {
        return Err(Error::Unsupported("clonar necesita PostgreSQL 10 o posterior en el origen".into()));
    }
    let mut notes = BTreeSet::new();
    notes.insert("No se clonan dueños, permisos ni tablespaces: los objetos quedan del usuario que ejecuta el script.".to_string());
    if tgt.version > 0 && s.version > 0 && tgt.version / 10000 < s.version / 10000 {
        notes.insert(format!(
            "El destino es PostgreSQL {} y el origen {}: lo que el destino no tenga va a fallar y queda en el registro.",
            tgt.version / 10000,
            s.version / 10000
        ));
    }
    match v {
        Variant::Timescale => {
            notes.insert("Las hipertablas de TimescaleDB se clonan como tablas comunes (sus fragmentos no).".into());
        }
        Variant::Edb => {
            notes.insert("Los paquetes y sinónimos de EDB no se clonan.".into());
        }
        _ => {}
    }

    // The tables: the listed ones, their partitions and their parents.
    let listed = resolve(s, tables).await?;
    let (below, above) = partition_family(s, &listed).await?;
    let mut all: Vec<u32> = listed.clone();
    for o in below.iter().chain(&above) {
        if !all.contains(o) {
            all.push(*o);
        }
    }
    let table_map: HashMap<u32, Table> = self::tables(s, &all).await?.into_iter().map(|t| (t.oid, t)).collect();
    let cols = columns(s, &all).await?;
    let table_schemas: Vec<String> =
        table_map.values().map(|t| t.schema.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let seqs = sequences(s, &all, &table_schemas).await?;
    let cons = constraints(s, &all).await?;
    let idx = indexes(s, &all).await?;
    let (trgs, trigger_funcs) = triggers(s, &all).await?;
    let pols = policies(s, &all).await?;
    if table_map.values().any(|t| t.tablespace) {
        notes.insert("Hay tablas en tablespaces propios: en el destino quedan en el tablespace por defecto.".into());
    }

    // Schemas of the tables: their views and functions come along.
    let mut schemas: BTreeSet<String> = table_schemas.iter().cloned().collect();

    // Views and materialized views whose relations are all cloned.
    let candidates = views(s, &table_schemas).await?;
    let mut ok: HashSet<u32> = candidates.iter().map(|v| v.oid).collect();
    loop {
        let bad: Vec<u32> = candidates
            .iter()
            .filter(|v| ok.contains(&v.oid))
            .filter(|v| v.rel_refs.iter().any(|r| !table_map.contains_key(r) && !ok.contains(r)))
            .map(|v| v.oid)
            .collect();
        if bad.is_empty() {
            break;
        }
        for b in bad {
            ok.remove(&b);
        }
    }
    let skipped: Vec<String> =
        candidates.iter().filter(|v| !ok.contains(&v.oid)).map(|v| format!("{}.{}", v.schema, v.name)).collect();
    if !skipped.is_empty() {
        notes.insert(format!("No se clonan estas vistas porque usan tablas que no se clonan: {}.", skipped.join(", ")));
    }
    let views: Vec<View> = candidates.into_iter().filter(|v| ok.contains(&v.oid)).collect();
    let view_oids: Vec<u32> = views.iter().map(|v| v.oid).collect();

    // Functions: those in the schemas, and any the tables or views use.
    let mut extra = used_functions(s, &all).await?;
    extra.extend(trigger_funcs);
    extra.extend(views.iter().flat_map(|v| v.func_refs.iter().copied()));
    let funcs_all = functions(s, &table_schemas, &extra).await?;
    let aggregates: Vec<&Function> = funcs_all.iter().filter(|f| f.kind == "a").collect();
    if !aggregates.is_empty() {
        notes.insert(format!(
            "No se clonan las funciones de agregado: {}.",
            aggregates.iter().map(|f| f.signature.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    let funcs: Vec<&Function> = funcs_all.iter().filter(|f| f.kind != "a" && f.def.is_some()).collect();
    let func_oids: Vec<u32> = funcs.iter().map(|f| f.oid).collect();
    let fdeps = function_deps(s, &func_oids).await?;

    // Types the columns and functions use (and theirs).
    let seed: Vec<u32> = {
        let q = "SELECT DISTINCT a.atttypid FROM pg_attribute a WHERE a.attrelid = ANY($1::oid[]) AND a.attnum > 0 AND NOT a.attisdropped";
        let mut v: Vec<u32> = query(s, q, &[&all]).await?.iter().map(|r| r.get(0)).collect();
        v.extend(funcs.iter().flat_map(|f| f.types.iter().copied()));
        v
    };
    let types = user_types(s, &seed).await?;
    let type_oids: Vec<u32> = types.iter().map(|t| t.oid).collect();
    let colls = collations(s, &all, &type_oids).await?;
    let mut rels: Vec<u32> = all.clone();
    rels.extend(&view_oids);
    let exts = extensions(s, &rels, &func_oids, &type_oids).await?;

    for t in &types {
        schemas.insert(t.schema.clone());
    }
    for q in &seqs {
        schemas.insert(q.schema.clone());
    }
    for c in colls.iter().filter(|c| c.schema != "pg_catalog") {
        schemas.insert(c.schema.clone());
    }
    for (_, sc) in &exts {
        schemas.insert(sc.clone());
    }
    let fschemas = "SELECT DISTINCT n.nspname::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE p.oid = ANY($1::oid[])";
    for r in query(s, fschemas, &[&func_oids]).await? {
        schemas.insert(r.get(0));
    }
    schemas.remove("pg_catalog");
    let schema_list: Vec<String> = schemas.iter().cloned().collect();
    let sch_comments = schema_comments(s, &schema_list).await?;
    let view_col_comments = column_comments(s, &view_oids).await?;
    let view_idx = indexes(s, &view_oids).await?;

    let mut out = CloneScript::default();

    // ---- before
    for sc in &schemas {
        out.before.push(format!("CREATE SCHEMA IF NOT EXISTS {}", qi(sc)));
    }
    for (name, sc) in &exts {
        if has(&tgt.extensions, name) {
            out.before.push(format!("CREATE EXTENSION IF NOT EXISTS {} WITH SCHEMA {} CASCADE", qi(name), qi(sc)));
        } else {
            notes.insert(format!("El destino no ofrece la extensión «{name}»: lo que la usa va a fallar."));
        }
    }
    let mut created_colls = HashSet::new();
    for c in colls.iter().filter(|c| c.schema != "pg_catalog") {
        if let Some(sql) = c.create() {
            created_colls.insert(format!("{}.{}", c.schema, c.name));
            out.before.push(sql);
        }
    }
    let type_index: HashMap<u32, usize> = types.iter().enumerate().map(|(i, t)| (t.oid, i)).collect();
    let type_deps: Vec<Vec<usize>> =
        types.iter().map(|t| t.deps.iter().filter_map(|d| type_index.get(d).copied()).collect()).collect();
    for i in topo(&type_deps) {
        if let Some(sql) = types[i].create() {
            out.before.push(sql);
        }
    }
    for q in seqs.iter().filter(|q| q.dep != "i") {
        out.before.push(format!("CREATE SEQUENCE IF NOT EXISTS {} AS {} {}", qn(&q.schema, &q.name), q.type_name, q.options()));
    }
    let (late, early): (Vec<&&Function>, Vec<&&Function>) = funcs.iter().partition(|f| f.late);
    if !early.is_empty() {
        out.before.push("SET check_function_bodies = false".into());
        for f in &early {
            out.before.push(f.def.clone().unwrap_or_default());
        }
    }

    let mut ctx = Ctx { tables: &table_map, columns: &cols, sequences: &seqs, constraints: &cons, target: &tgt, notes: &mut notes };

    // Parents of listed partitions that weren't listed.
    let mut parents: Vec<&Table> = above.iter().filter(|o| !listed.contains(o)).filter_map(|o| table_map.get(o)).collect();
    parents.sort_by_key(|t| depth(&table_map, t.oid));
    for p in parents {
        ctx.notes.insert(format!("Se crea {} (sin sus otras particiones) porque se clona una de sus particiones.", p.qname()));
        let sql = ctx.create(p, &created_colls);
        out.before.push(sql);
    }

    // ---- tables
    // Which listed table each unlisted partition hangs from.
    let owner = |oid: u32| -> Option<u32> {
        let mut cur = table_map.get(&oid)?.parent?;
        loop {
            if listed.contains(&cur) {
                return Some(cur);
            }
            cur = table_map.get(&cur)?.parent?;
        }
    };
    let mut order = listed.clone();
    order.sort_by_key(|o| depth(&table_map, *o));
    let mut partitions: Vec<u32> = below.iter().copied().filter(|o| !listed.contains(o)).collect();
    partitions.sort_by_key(|o| depth(&table_map, *o));
    for &oid in &order {
        let Some(t) = table_map.get(&oid) else { continue };
        let mut ct = CloneTable {
            table: ObjectRef { kind: "table".into(), schema: Some(t.schema.clone()), name: t.name.clone() },
            create: ctx.create(t, &created_colls),
            before_data: ctx.column_settings(t),
            after_data: Vec::new(),
        };
        let family: Vec<u32> = std::iter::once(oid).chain(partitions.iter().copied().filter(|p| owner(*p) == Some(oid))).collect();
        for &p in &family[1..] {
            if let Some(pt) = table_map.get(&p) {
                let sql = ctx.create(pt, &created_colls);
                ct.before_data.push(sql);
                ct.before_data.extend(ctx.column_settings(pt));
            }
        }
        for &member in &family {
            let Some(mt) = table_map.get(&member) else { continue };
            for i in idx.iter().filter(|i| i.table == member && !i.backs_constraint) {
                // A partition's index attached to its parent's comes from it.
                let parent_cloned = mt.parent.is_some_and(|p| table_map.contains_key(&p));
                if i.attached && parent_cloned {
                    continue;
                }
                ct.after_data.push(index_if_not_exists(&i.def));
            }
            for c in cons.iter().filter(|c| c.table == member && c.kind == "u" && !c.inherited) {
                ct.after_data.push(guarded(&format!("ALTER TABLE {} ADD CONSTRAINT {} {}", mt.qname(), qi(&c.name), c.def)));
            }
            if let Some(i) = idx.iter().find(|i| i.table == member && i.clustered) {
                ct.after_data.push(format!("ALTER TABLE {} CLUSTER ON {}", mt.qname(), qi(&i.name)));
            }
        }
        out.tables.push(ct);
    }

    // ---- after
    let mut after = Vec::new();
    for kind in ["f", "c", "x"] {
        for c in cons.iter().filter(|c| c.kind == kind && !c.inherited) {
            let Some(t) = table_map.get(&c.table) else { continue };
            if kind == "f" && !table_map.contains_key(&c.references) {
                ctx.notes.insert(format!("No se crea la clave foránea {} de {}: apunta a una tabla que no se clona.", c.name, t.qname()));
                continue;
            }
            after.push(guarded(&format!("ALTER TABLE {} ADD CONSTRAINT {} {}", t.qname(), qi(&c.name), c.def)));
        }
    }
    for q in seqs.iter().filter(|q| q.dep == "a") {
        if let (Some(t), Some(c)) = (table_map.get(&q.table), &q.column) {
            after.push(format!("ALTER SEQUENCE {} OWNED BY {}.{}", qn(&q.schema, &q.name), t.qname(), qi(c)));
        }
    }

    // Code after the tables, in dependency order.
    enum Code<'a> {
        Func(&'a Function),
        View(&'a View),
    }
    let mut code: Vec<Code> = late.iter().map(|f| Code::Func(f)).collect();
    code.extend(views.iter().map(Code::View));
    let pos: HashMap<(bool, u32), usize> = code
        .iter()
        .enumerate()
        .map(|(i, c)| match c {
            Code::Func(f) => ((true, f.oid), i),
            Code::View(v) => ((false, v.oid), i),
        })
        .collect();
    let code_deps: Vec<Vec<usize>> = code
        .iter()
        .map(|c| match c {
            Code::Func(f) => fdeps.iter().filter(|d| d.0 == f.oid).filter_map(|d| pos.get(&(d.1, d.2)).copied()).collect(),
            Code::View(v) => v
                .rel_refs
                .iter()
                .filter_map(|r| pos.get(&(false, *r)))
                .chain(v.func_refs.iter().filter_map(|r| pos.get(&(true, *r))))
                .copied()
                .collect(),
        })
        .collect();
    if !code.is_empty() {
        after.push("SET check_function_bodies = false".into());
    }
    for i in topo(&code_deps) {
        match &code[i] {
            Code::Func(f) => after.push(f.def.clone().unwrap_or_default()),
            Code::View(v) => {
                let name = qn(&v.schema, &v.name);
                let with = v.options.as_ref().map(|o| format!(" WITH ({o})")).unwrap_or_default();
                if v.kind == "m" {
                    let using = v
                        .access_method
                        .as_deref()
                        .filter(|a| *a != "heap" && has(&tgt.access_methods, a))
                        .map(|a| format!(" USING {}", qi(a)))
                        .unwrap_or_default();
                    let data = if v.populated { "WITH DATA" } else { "WITH NO DATA" };
                    after.push(format!("CREATE MATERIALIZED VIEW IF NOT EXISTS {name}{using}{with} AS\n{}\n{data}", v.def));
                    for ix in view_idx.iter().filter(|x| x.table == v.oid) {
                        after.push(index_if_not_exists(&ix.def));
                        if ix.clustered {
                            after.push(format!("ALTER MATERIALIZED VIEW {name} CLUSTER ON {}", qi(&ix.name)));
                        }
                    }
                } else {
                    after.push(format!("CREATE OR REPLACE VIEW {name}{with} AS\n{}", v.def));
                }
            }
        }
    }

    for t in &trgs {
        let Some(tb) = table_map.get(&t.table) else { continue };
        after.push(guarded(&t.def));
        let state = match t.enabled.as_str() {
            "D" => Some("DISABLE TRIGGER"),
            "R" => Some("ENABLE REPLICA TRIGGER"),
            "A" => Some("ENABLE ALWAYS TRIGGER"),
            _ => None,
        };
        if let Some(st) = state {
            after.push(format!("ALTER TABLE {} {st} {}", tb.qname(), qi(&t.name)));
        }
    }

    for &oid in &all {
        let Some(t) = table_map.get(&oid) else { continue };
        if t.rls {
            after.push(format!("ALTER TABLE {} ENABLE ROW LEVEL SECURITY", t.qname()));
        }
        if t.force_rls {
            after.push(format!("ALTER TABLE {} FORCE ROW LEVEL SECURITY", t.qname()));
        }
    }
    for p in &pols {
        let Some(t) = table_map.get(&p.table) else { continue };
        let missing: Vec<&String> = p.roles.iter().filter(|r| !has(&tgt.roles, r)).collect();
        if !missing.is_empty() {
            ctx.notes.insert(format!(
                "No se crea la política {} de {}: el destino no tiene {}.",
                p.name,
                t.qname(),
                missing.iter().map(|r| format!("el rol «{r}»")).collect::<Vec<_>>().join(", ")
            ));
            continue;
        }
        let mut roles: Vec<String> = p.roles.iter().map(|r| qi(r)).collect();
        if p.public || roles.is_empty() {
            roles.insert(0, "PUBLIC".into());
        }
        let mut sql = format!(
            "CREATE POLICY {} ON {} AS {} FOR {} TO {}",
            qi(&p.name),
            t.qname(),
            if p.permissive { "PERMISSIVE" } else { "RESTRICTIVE" },
            policy_cmd(&p.cmd),
            roles.join(", ")
        );
        if let Some(u) = &p.using {
            sql.push_str(&format!(" USING ({u})"));
        }
        if let Some(c) = &p.check {
            sql.push_str(&format!(" WITH CHECK ({c})"));
        }
        after.push(guarded(&sql));
    }

    for &oid in &all {
        let Some(t) = table_map.get(&oid) else { continue };
        if let Some(o) = &t.toast_options {
            let opts = o.split(", ").map(|x| format!("toast.{x}")).collect::<Vec<_>>().join(", ");
            after.push(format!("ALTER TABLE {} SET ({opts})", t.qname()));
        }
        match t.replica_identity.as_str() {
            "f" => after.push(format!("ALTER TABLE {} REPLICA IDENTITY FULL", t.qname())),
            "n" => after.push(format!("ALTER TABLE {} REPLICA IDENTITY NOTHING", t.qname())),
            "i" => {
                // The index behind it (a unique constraint's or its own).
                let q = "SELECT ic.relname::text FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid
                         WHERE i.indrelid = $1 AND i.indisreplident";
                if let Ok(Some(r)) = s.client.query_opt(q, &[&oid]).await {
                    after.push(format!("ALTER TABLE {} REPLICA IDENTITY USING INDEX {}", t.qname(), qi(&r.get::<_, String>(0))));
                }
            }
            _ => {}
        }
    }

    // Comments.
    let comment = |what: String, text: &str| format!("COMMENT ON {what} IS {}", lit(text));
    for (sc, c) in &sch_comments {
        after.push(comment(format!("SCHEMA {}", qi(sc)), c));
    }
    for t in &types {
        if let Some(c) = &t.comment {
            let what = if t.kind == "d" { "DOMAIN" } else { "TYPE" };
            after.push(comment(format!("{what} {}", qn(&t.schema, &t.name)), c));
        }
    }
    for q in &seqs {
        if let Some(c) = &q.comment {
            after.push(comment(format!("SEQUENCE {}", qn(&q.schema, &q.name)), c));
        }
    }
    for &oid in &all {
        let Some(t) = table_map.get(&oid) else { continue };
        if let Some(c) = &t.comment {
            after.push(comment(format!("TABLE {}", t.qname()), c));
        }
        for col in cols.iter().filter(|c| c.table == oid) {
            if let Some(c) = &col.comment {
                after.push(comment(format!("COLUMN {}.{}", t.qname(), qi(&col.name)), c));
            }
        }
        for con in cons.iter().filter(|c| c.table == oid && !c.inherited) {
            if let Some(c) = &con.comment {
                after.push(comment(format!("CONSTRAINT {} ON {}", qi(&con.name), t.qname()), c));
            }
        }
        for i in idx.iter().filter(|i| i.table == oid && !i.backs_constraint) {
            if let Some(c) = &i.comment {
                after.push(comment(format!("INDEX {}", qn(&i.schema, &i.name)), c));
            }
        }
        for tr in trgs.iter().filter(|x| x.table == oid) {
            if let Some(c) = &tr.comment {
                after.push(comment(format!("TRIGGER {} ON {}", qi(&tr.name), t.qname()), c));
            }
        }
        for p in pols.iter().filter(|p| p.table == oid) {
            if let Some(c) = &p.comment {
                after.push(comment(format!("POLICY {} ON {}", qi(&p.name), t.qname()), c));
            }
        }
    }
    for f in &funcs {
        if let Some(c) = &f.comment {
            let what = if f.kind == "p" { "PROCEDURE" } else { "FUNCTION" };
            after.push(comment(format!("{what} {}", f.signature), c));
        }
    }
    for v in &views {
        let name = qn(&v.schema, &v.name);
        let what = if v.kind == "m" { "MATERIALIZED VIEW" } else { "VIEW" };
        if let Some(c) = &v.comment {
            after.push(comment(format!("{what} {name}"), c));
        }
        for (_, col, c) in view_col_comments.iter().filter(|x| x.0 == v.oid) {
            after.push(comment(format!("COLUMN {name}.{}", qi(col)), c));
        }
        for ix in view_idx.iter().filter(|x| x.table == v.oid) {
            if let Some(c) = &ix.comment {
                after.push(comment(format!("INDEX {}", qn(&ix.schema, &ix.name)), c));
            }
        }
    }

    // Last, after the data: each sequence where the source's is.
    for q in &seqs {
        let name = lit(&qn(&q.schema, &q.name));
        after.push(match q.last {
            Some(v) => format!("SELECT setval({name}, {v}, true)"),
            None => format!("SELECT setval({name}, {}, false)", q.start),
        });
    }

    out.after = after;
    out.notes = notes.into_iter().collect();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_are_idempotent() {
        assert_eq!(
            index_if_not_exists("CREATE INDEX t_a ON ONLY public.t USING btree (a) INCLUDE (b) WHERE (a > 0)"),
            "CREATE INDEX IF NOT EXISTS t_a ON public.t USING btree (a) INCLUDE (b) WHERE (a > 0)"
        );
        assert_eq!(
            index_if_not_exists("CREATE UNIQUE INDEX u ON public.t USING btree (lower(a))"),
            "CREATE UNIQUE INDEX IF NOT EXISTS u ON public.t USING btree (lower(a))"
        );
        let g = guarded("ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0)");
        assert_eq!(
            g,
            "DO $dbine$ BEGIN ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0); \
             EXCEPTION WHEN duplicate_object OR duplicate_table THEN NULL; END $dbine$"
        );
        // A body that already uses the tag gets another one.
        assert!(guarded("SELECT '$dbine$'").starts_with("DO $dbine1$ "));
    }

    #[test]
    fn constraint_definitions_keep_index_options() {
        assert_eq!(constraint_def("PRIMARY KEY (a)", "p", Some("fillfactor='80'")), "PRIMARY KEY (a) WITH (fillfactor='80')");
        assert_eq!(
            constraint_def("UNIQUE (a) INCLUDE (b) DEFERRABLE INITIALLY DEFERRED", "u", Some("fillfactor='70'")),
            "UNIQUE (a) INCLUDE (b) WITH (fillfactor='70') DEFERRABLE INITIALLY DEFERRED"
        );
        assert_eq!(
            constraint_def("EXCLUDE USING gist (r WITH &&) WHERE ((a > 0))", "x", Some("fillfactor='50'")),
            "EXCLUDE USING gist (r WITH &&) WITH (fillfactor='50') WHERE ((a > 0))"
        );
        assert_eq!(constraint_def("PRIMARY KEY (a) USING INDEX TABLESPACE fast", "p", None), "PRIMARY KEY (a)");
        assert_eq!(constraint_def("CHECK ((a > 0)) NOT VALID", "c", None), "CHECK ((a > 0)) NOT VALID");
    }

    #[test]
    fn dependency_order() {
        // 0 needs 2, 2 needs 1; 3 and 4 need each other (cycle broken).
        let order = topo(&[vec![2], vec![], vec![1], vec![4], vec![3]]);
        let at = |i| order.iter().position(|x| *x == i).unwrap();
        assert!(at(1) < at(2) && at(2) < at(0));
        assert_eq!(order.len(), 5);
    }

    #[test]
    fn user_types() {
        let base = UserType {
            oid: 1,
            schema: "app".into(),
            name: "mood".into(),
            kind: "e".into(),
            labels: Some(vec!["sad".into(), "it's ok".into()]),
            attributes: None,
            base: None,
            not_null: false,
            default: None,
            collation: None,
            checks: None,
            range: None,
            comment: None,
            deps: vec![],
        };
        assert!(base.create().unwrap().contains("CREATE TYPE \"app\".\"mood\" AS ENUM ('sad', 'it''s ok')"));
        let d = UserType {
            kind: "d".into(),
            base: Some("integer".into()),
            not_null: true,
            default: Some("1".into()),
            checks: Some("CONSTRAINT pos CHECK ((VALUE > 0))".into()),
            ..base
        };
        assert!(d
            .create()
            .unwrap()
            .contains("CREATE DOMAIN \"app\".\"mood\" AS integer DEFAULT 1 NOT NULL CONSTRAINT pos CHECK ((VALUE > 0))"));
    }

    #[test]
    fn collations() {
        let c = Collation {
            schema: "app".into(),
            name: "ci".into(),
            provider: "i".into(),
            collate: None,
            ctype: None,
            locale: Some("und-u-ks-level2".into()),
            deterministic: false,
        };
        assert_eq!(
            c.create().unwrap(),
            "CREATE COLLATION IF NOT EXISTS \"app\".\"ci\" (provider = icu, locale = 'und-u-ks-level2', deterministic = false)"
        );
    }
}
