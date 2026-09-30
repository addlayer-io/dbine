//! Engines with PostgreSQL's catalog (PostgreSQL, CockroachDB, TimescaleDB,
//! YugabyteDB…): what `database_schema` flattens and the clone must keep,
//! read straight from `pg_catalog`.
//!
//! - refused: partitioned tables, hypertables, tables in an inheritance
//!   tree (the clone would be a plain table);
//! - kept: identity `ALWAYS` / `BY DEFAULT`, generated columns (`STORED`,
//!   `VIRTUAL`, CockroachDB's computed ones), column collations, `serial`
//!   columns (their own sequence), a default taken from a shared sequence
//!   (the same sequence), and the sequence's options and current value;
//! - kept too: `UNLOGGED`, and CHECKs / foreign keys `NOT VALID` or `NO
//!   INHERIT` (added after the rows, as the original has them: rows that
//!   break a `NOT VALID` one are the original's own);
//! - kept too: storage options (`WITH (fillfactor=…)`, TOAST's), column
//!   `STORAGE` / `STATISTICS` / `COMPRESSION`, row-level security (`ENABLE`
//!   / `FORCE` and the policies, a policy that reads the table itself
//!   reading the clone; refused with rows when it hides rows from the
//!   user); `REPLICA IDENTITY`, `CLUSTER ON` and extended statistics
//!   (renamed); CockroachDB's column families, zone configurations,
//!   hash-sharded indexes and primary key, `NOT VISIBLE` columns, `ON
//!   UPDATE` and storage parameters (row-level TTL) in the `CREATE`;
//! - comments on indexes and constraints (`database_schema` only has the
//!   table's and the columns'), on the clone's renamed ones;
//! - the names already used in the schema (tables, indexes, sequences,
//!   types), so the clone's never collide (CockroachDB: index names are
//!   per table, so not those);
//! - after the clone is done, its columns, checks, foreign keys and
//!   indexes are read back and compared.

use super::exec;
use dbine_driver::{DdlParts, Driver, Error, ForeignKeyDef, QueryOutcome, Result, Session, TableSchema};

/// Drivers whose catalog is PostgreSQL's (checked live: PostgreSQL,
/// CockroachDB, TimescaleDB; the others keep the same catalog tables).
const IDS: &[&str] = &[
    "postgres",
    "cockroachdb",
    "timescaledb",
    "yugabytedb",
    "kingbase",
    "alloydb",
    "cloudsql_postgres",
    "aurora_postgres",
    "edb",
    "fujitsu",
    "opengauss",
    "greenplum",
    "cloudberry",
    "greengage",
];

pub(super) fn applies(driver: &dyn Driver) -> bool {
    IDS.contains(&driver.info().id)
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn qi(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `"schema"."name"`, as `regclass` and `pg_get_serial_sequence` take it.
pub(super) fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", qi(s), qi(name)),
        None => qi(name),
    }
}

/// Every row of a query, cells as text (`None`: NULL).
async fn rows(s: &mut dyn Session, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1_000_000, &mut out).await?;
    if let Some(e) = out.error {
        return Err(Error::Query(e));
    }
    Ok(out
        .results
        .into_iter()
        .rev()
        .find(|r| !r.columns.is_empty())
        .map(|r| {
            r.rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|v| match v {
                            serde_json::Value::Null => None,
                            serde_json::Value::String(s) => Some(s),
                            other => Some(other.to_string()),
                        })
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn one(s: &mut dyn Session, sql: &str) -> Result<Option<String>> {
    Ok(rows(s, sql).await?.into_iter().next().and_then(|r| r.into_iter().next().flatten()))
}

/// A column as the catalog has it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PgColumn {
    pub name: String,
    /// `format_type`, without collation.
    pub type_name: String,
    /// `a` (ALWAYS), `d` (BY DEFAULT) or empty.
    pub identity: String,
    /// `s` (STORED), `v` (VIRTUAL) or empty.
    pub generated: String,
    /// The default, or the generation expression.
    pub default: Option<String>,
    /// As `COLLATE` takes it, when it isn't the type's own.
    pub collation: Option<String>,
    /// The sequence the column owns (identity or `serial`). Owned only:
    /// CockroachDB's `pg_get_serial_sequence` also names a shared one.
    pub sequence: Option<String>,
    /// CockroachDB: `NOT VISIBLE` (`database_schema` leaves these out).
    pub hidden: bool,
    /// `NOT NULL` (for the hidden columns the plan doesn't have).
    pub not_null: bool,
    /// CockroachDB's `ON UPDATE` expression.
    pub on_update: Option<String>,
    /// The column's comment (for the hidden columns the plan doesn't have).
    pub comment: Option<String>,
}

/// What the catalog says about the original.
#[derive(Debug, Clone, Default)]
pub(super) struct PgTable {
    pub version: i64,
    pub cockroach: bool,
    pub columns: Vec<PgColumn>,
    /// Names already used in the schema (relations and types).
    pub taken: Vec<String>,
    pub parts: Parts,
    /// CHECKs `NOT VALID` or `NO INHERIT`: (name, `pg_get_constraintdef`).
    /// `CREATE TABLE` would make them plain ones (and a `NOT VALID` one
    /// would refuse the original's own rows): added after the rows.
    pub deferred_checks: Vec<(String, String)>,
    /// Foreign keys `NOT VALID`, by name.
    pub not_valid_fks: Vec<String>,
    /// Foreign keys as `pg_get_constraintdef` writes them (`MATCH FULL`,
    /// `DEFERRABLE`, `INITIALLY DEFERRED`, `NOT VALID`… which
    /// `ForeignKeyDef` doesn't carry).
    pub foreign_keys: Vec<PgForeignKey>,
    /// Comments on the table's indexes and constraints (`database_schema`
    /// only has the table's and the columns'). `None`: unreadable.
    pub comments: Option<Vec<PgComment>>,
    /// Storage options, column settings, row-level security.
    pub settings: Settings,
    /// CockroachDB: column families, zone configurations, hash-sharded
    /// indexes.
    pub crdb: Option<Crdb>,
    /// Row-level security hides rows from this user: a read of the
    /// original doesn't see them all.
    pub rls_hides_rows: bool,
    /// The original's schema and name (policies that read the table itself
    /// are pointed at the clone).
    pub schema: Option<String>,
    pub name: String,
    /// What `database_schema` doesn't carry and planner or replication
    /// depend on: `REPLICA IDENTITY`, `CLUSTER ON`, extended statistics.
    pub extras: Extras,
}

/// An index of the original, by name; `primary`: the primary key's (the
/// clone's has the name the server gives it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PgIndexRef {
    pub name: String,
    pub primary: bool,
}

/// `CREATE STATISTICS` on the table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PgStatistics {
    /// Its own schema (not always the table's).
    pub schema: String,
    pub name: String,
    /// What follows the name, up to ` FROM` (`(ndistinct) ON a, b`).
    pub body: String,
    /// `ALTER STATISTICS … SET STATISTICS n`.
    pub target: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Extras {
    /// `relreplident`: `d` (default), `f` (FULL), `n` (NOTHING), `i`
    /// (USING INDEX); empty where the engine has none (CockroachDB).
    pub replica: String,
    pub replica_index: Option<PgIndexRef>,
    /// The index of `CLUSTER ON`.
    pub clustered: Option<PgIndexRef>,
    /// Sorted by name.
    pub statistics: Vec<PgStatistics>,
    /// Every extended statistics' `(schema, name)` in the database (the
    /// clone's must not collide).
    pub statistics_taken: Vec<(String, String)>,
}

/// The original's (or the clone's) extras.
async fn extras(s: &mut dyn Session, version: i64, cockroach: bool, schema: Option<&str>, name: &str, taken: bool) -> Result<Extras> {
    let mut out = Extras::default();
    if cockroach {
        return Ok(out);
    }
    let rel = lit(&qualified(schema, name));
    let index = |r: Vec<Vec<Option<String>>>| {
        r.into_iter().next().and_then(|r| {
            let name = r.first().cloned().flatten()?;
            Some(PgIndexRef { name, primary: truth(r.get(1).cloned().flatten()) })
        })
    };
    if version >= 90400 {
        out.replica = one(s, &format!("SELECT relreplident::text FROM pg_class WHERE oid = {rel}::regclass")).await?.unwrap_or_default().trim().to_string();
        let sql = format!(
            "SELECT ic.relname::text, x.indisprimary::text FROM pg_index x JOIN pg_class ic ON ic.oid = x.indexrelid
             WHERE x.indrelid = {rel}::regclass AND x.indisreplident"
        );
        out.replica_index = index(rows(s, &sql).await?);
    }
    let sql = format!(
        "SELECT ic.relname::text, x.indisprimary::text FROM pg_index x JOIN pg_class ic ON ic.oid = x.indexrelid
         WHERE x.indrelid = {rel}::regclass AND x.indisclustered"
    );
    out.clustered = index(rows(s, &sql).await?);
    if version >= 100000 {
        let target = if version >= 130000 { "CASE WHEN st.stxstattarget >= 0 THEN st.stxstattarget::text END" } else { "NULL::text" };
        let sql = format!(
            "SELECT n.nspname::text, st.stxname::text,
                    substr(pg_get_statisticsobjdef(st.oid), length('CREATE STATISTICS ' || quote_ident(n.nspname) || '.' || quote_ident(st.stxname)) + 1),
                    {target}, obj_description(st.oid, 'pg_statistic_ext')
             FROM pg_statistic_ext st JOIN pg_namespace n ON n.oid = st.stxnamespace
             WHERE st.stxrelid = {rel}::regclass ORDER BY 2"
        );
        for r in rows(s, &sql).await? {
            let at = |i: usize| r.get(i).cloned().flatten();
            let def = at(2).unwrap_or_default();
            // ` (ndistinct) ON a, b FROM t`: the table is the clone's.
            let body = def.rfind(" FROM ").map(|i| def[..i].trim().to_string()).ok_or_else(|| {
                refuse(format!("no se pudo leer la definición de las estadísticas extendidas «{}» ({def})", at(1).unwrap_or_default()))
            })?;
            out.statistics.push(PgStatistics { schema: at(0).unwrap_or_default(), name: at(1).unwrap_or_default(), body, target: at(3), comment: at(4) });
        }
        if taken {
            out.statistics_taken = rows(s, "SELECT n.nspname::text, st.stxname::text FROM pg_statistic_ext st JOIN pg_namespace n ON n.oid = st.stxnamespace")
                .await?
                .into_iter()
                .filter_map(|r| Some((r.first().cloned().flatten()?, r.get(1).cloned().flatten()?)))
                .collect();
        }
    }
    Ok(out)
}

/// The clone's extended statistics' names: the original's renamed like its
/// constraints, and not one another object already has.
pub(super) fn statistics_names(pg: &PgTable, new_name: &str) -> Vec<(String, String)> {
    let mut used: Vec<(String, String)> = pg.extras.statistics_taken.clone();
    let mut out = Vec::new();
    for st in &pg.extras.statistics {
        let (base, _) = super::rename_constraint(&st.name, &pg.name, new_name, 63);
        let mut n = base.clone();
        let mut k = 2;
        while used.iter().any(|(s, x)| *s == st.schema && *x == n) {
            let suffix = format!("_{k}");
            n = format!("{}{suffix}", super::fit(&base, 63 - suffix.len(), false, 0));
            k += 1;
        }
        used.push((st.schema.clone(), n.clone()));
        out.push((st.name.clone(), n));
    }
    out
}

/// The clone's index for one of the original's: the primary key's as the
/// server named it, the others renamed. `None`: the clone doesn't have it
/// (cloned without indexes).
async fn clone_index(tgt: &mut dyn Session, clone_rel: &str, ix: &PgIndexRef, renames: &[super::Rename]) -> Result<Option<String>> {
    let names = rows(
        tgt,
        &format!(
            "SELECT ic.relname::text, x.indisprimary::text FROM pg_index x JOIN pg_class ic ON ic.oid = x.indexrelid WHERE x.indrelid = {}::regclass",
            lit(clone_rel)
        ),
    )
    .await?;
    let want = renames.iter().find(|r| r.from == ix.name).map(|r| r.to.clone()).unwrap_or_else(|| ix.name.clone());
    Ok(names.into_iter().find_map(|r| {
        let n = r.first().cloned().flatten()?;
        let primary = truth(r.get(1).cloned().flatten());
        ((ix.primary && primary) || (!ix.primary && n == want)).then_some(n)
    }))
}

/// What the table has that `database_schema` doesn't carry: storage
/// options (`WITH (fillfactor=…)`), column `STORAGE` / `STATISTICS` /
/// `COMPRESSION`, and row-level security with its policies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Settings {
    /// `reloptions` (`fillfactor=60`), the TOAST table's as `toast.x=y`;
    /// sorted.
    pub options: Vec<String>,
    /// Columns with settings of their own (not CockroachDB's).
    pub columns: Vec<ColumnSettings>,
    /// `ENABLE ROW LEVEL SECURITY`.
    pub rls: bool,
    /// `FORCE ROW LEVEL SECURITY`.
    pub force_rls: bool,
    /// Sorted by name.
    pub policies: Vec<Policy>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ColumnSettings {
    pub name: String,
    /// `attstorage` when it isn't the type's.
    pub storage: Option<String>,
    /// `SET STATISTICS n`.
    pub statistics: Option<String>,
    /// `attcompression` (`p`, `l`) when set.
    pub compression: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Policy {
    pub name: String,
    /// `PERMISSIVE` / `RESTRICTIVE`.
    pub permissive: String,
    /// `ALL`, `SELECT`…
    pub cmd: String,
    /// `PUBLIC, alice`.
    pub roles: String,
    pub using: Option<String>,
    pub check: Option<String>,
}

impl Policy {
    fn sql(&self, clone_q: &str) -> String {
        let mut s = format!("CREATE POLICY {} ON {clone_q} AS {} FOR {} TO {}", qi(&self.name), self.permissive, self.cmd, self.roles);
        if let Some(u) = &self.using {
            s.push_str(&format!(" USING ({u})"));
        }
        if let Some(c) = &self.check {
            s.push_str(&format!(" WITH CHECK ({c})"));
        }
        s
    }
}

impl ColumnSettings {
    fn sql(&self, clone_q: &str) -> Result<Vec<String>> {
        let col = format!("ALTER TABLE {clone_q} ALTER COLUMN {}", qi(&self.name));
        let mut out = Vec::new();
        if let Some(s) = &self.storage {
            let how = match s.as_str() {
                "p" => "PLAIN",
                "e" => "EXTERNAL",
                "m" => "MAIN",
                "x" => "EXTENDED",
                other => return Err(refuse(format!("la columna «{}» tiene un almacenamiento desconocido ({other})", self.name))),
            };
            out.push(format!("{col} SET STORAGE {how}"));
        }
        if let Some(n) = &self.statistics {
            out.push(format!("{col} SET STATISTICS {}", number(Some(n.clone()), "estadísticas")?));
        }
        if let Some(c) = &self.compression {
            let how = match c.as_str() {
                "p" => "pglz",
                "l" => "lz4",
                other => return Err(refuse(format!("la columna «{}» tiene una compresión desconocida ({other})", self.name))),
            };
            out.push(format!("{col} SET COMPRESSION {how}"));
        }
        Ok(out)
    }
}

async fn settings(s: &mut dyn Session, version: i64, cockroach: bool, schema: Option<&str>, name: &str) -> Result<Settings> {
    let rel = lit(&qualified(schema, name));
    let mut options: Vec<String> = rows(
        s,
        &format!(
            "SELECT unnest(reloptions)::text FROM pg_class WHERE oid = {rel}::regclass
             UNION ALL
             SELECT 'toast.' || unnest(t.reloptions)::text FROM pg_class c JOIN pg_class t ON t.oid = c.reltoastrelid WHERE c.oid = {rel}::regclass"
        ),
    )
    .await?
    .into_iter()
    .filter_map(|r| r.into_iter().next().flatten())
    .collect();
    options.sort();
    let mut columns = Vec::new();
    if !cockroach {
        let compression = if version >= 140000 { "NULLIF(a.attcompression::text, '')" } else { "NULL::text" };
        let sql = format!(
            "SELECT a.attname::text,
                    CASE WHEN a.attstorage <> t.typstorage THEN a.attstorage::text END,
                    CASE WHEN a.attstattarget >= 0 THEN a.attstattarget::text END,
                    {compression}
             FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
             WHERE a.attrelid = {rel}::regclass AND a.attnum > 0 AND NOT a.attisdropped
             ORDER BY a.attnum"
        );
        for r in rows(s, &sql).await? {
            let at = |i: usize| r.get(i).cloned().flatten().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let c = ColumnSettings { name: at(0).unwrap_or_default(), storage: at(1), statistics: at(2), compression: at(3) };
            if c.storage.is_some() || c.statistics.is_some() || c.compression.is_some() {
                columns.push(c);
            }
        }
    }
    let flags = rows(s, &format!("SELECT relrowsecurity::text, relforcerowsecurity::text FROM pg_class WHERE oid = {rel}::regclass"))
        .await?
        .into_iter()
        .next()
        .unwrap_or_default();
    let (rls, force_rls) = (truth(flags.first().cloned().flatten()), truth(flags.get(1).cloned().flatten()));
    let sch = match schema.filter(|s| !s.is_empty()) {
        Some(s) => lit(s),
        None => "current_schema()".into(),
    };
    let policies = rows(
        s,
        &format!(
            "SELECT p.policyname::text, upper(p.permissive::text), upper(p.cmd::text),
                    (SELECT string_agg(CASE WHEN r::text = 'public' THEN 'PUBLIC' ELSE quote_ident(r::text) END, ', ' ORDER BY r::text)
                     FROM unnest(p.roles) AS r),
                    p.qual::text, p.with_check::text
             FROM pg_policies p WHERE p.schemaname = {sch} AND p.tablename = {}
             ORDER BY 1",
            lit(name)
        ),
    )
    .await;
    let policies = match policies {
        Ok(v) => v
            .into_iter()
            .map(|r| {
                let at = |i: usize| r.get(i).cloned().flatten().filter(|v| !v.is_empty());
                Policy {
                    name: at(0).unwrap_or_default(),
                    permissive: at(1).unwrap_or_else(|| "PERMISSIVE".into()),
                    cmd: at(2).unwrap_or_else(|| "ALL".into()),
                    roles: at(3).unwrap_or_else(|| "PUBLIC".into()),
                    using: at(4),
                    check: at(5),
                }
            })
            .collect(),
        // Row-level security on, and its policies unreadable: the clone
        // would show (or hide) other rows than the original.
        Err(e) if rls => {
            return Err(refuse(format!("la tabla tiene seguridad por filas (ROW LEVEL SECURITY) y no se pudieron leer sus políticas ({e})")))
        }
        Err(_) => Vec::new(),
    };
    Ok(Settings { options, columns, rls, force_rls, policies })
}

/// An identifier as the server writes one (quoted only when it must be).
fn ident_text(s: &str) -> String {
    let plain = s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$');
    if plain { s.to_string() } else { qi(s) }
}

/// A policy's expression (as `pg_policies` writes it) with its references
/// to the table itself (`FROM s.t x`, `FROM t`, `t.col`) pointed at the
/// clone. `None`: it names the table in a way that can't be told apart (a
/// bare `t` that isn't a column, outside `FROM` / `JOIN`).
fn retarget(expr: &str, schema: Option<&str>, table: &str, clone: &str, columns: &[PgColumn]) -> Option<String> {
    // Identifiers (with their byte span and whether they were quoted),
    // skipping string literals.
    struct Id {
        start: usize,
        end: usize,
        text: String,
        quoted: bool,
    }
    let b = expr.as_bytes();
    let mut ids: Vec<Id> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
        } else if c == b'"' {
            let start = i;
            let mut text = String::new();
            i += 1;
            let mut seg = i;
            loop {
                if i >= b.len() {
                    return None;
                }
                if b[i] == b'"' {
                    text.push_str(&expr[seg..i]);
                    if b.get(i + 1) == Some(&b'"') {
                        text.push('"');
                        i += 2;
                        seg = i;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            ids.push(Id { start, end: i, text, quoted: true });
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            ids.push(Id { start, end: i, text: expr[start..i].to_string(), quoted: false });
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    // Chains `a.b.c` (dots right between them).
    let dotted = |x: &Id, y: &Id| expr[x.end..y.start].trim() == "." && !expr[x.end..y.start].contains(char::is_whitespace);
    let mut replace: Vec<(usize, usize)> = Vec::new();
    let mut k = 0;
    while k < ids.len() {
        let mut chain = vec![k];
        while chain.len() < 4 && *chain.last().unwrap() + 1 < ids.len() && dotted(&ids[*chain.last().unwrap()], &ids[chain.last().unwrap() + 1]) {
            chain.push(chain.last().unwrap() + 1);
        }
        let next = chain.last().unwrap() + 1;
        let is = |j: usize| ids[chain[j]].text == table && (ids[chain[j]].quoted || !table.chars().any(|c| c.is_uppercase()));
        if chain.len() == 1 {
            if is(0) {
                let prev = k.checked_sub(1).map(|p| &ids[p]).filter(|p| expr[p.end..ids[k].start].trim().is_empty());
                let relation = prev.is_some_and(|p| !p.quoted && ["from", "join", "only"].contains(&p.text.to_ascii_lowercase().as_str()));
                if relation {
                    replace.push((ids[k].start, ids[k].end));
                } else if !columns.iter().any(|c| c.name == table) {
                    return None;
                }
            }
        } else {
            for j in 0..chain.len() {
                if !is(j) {
                    continue;
                }
                // `t.col`: the table as a qualifier; `s.t`, `s.t.col`,
                // `db.s.t`: in its schema. Another schema's `t`, or a
                // column `t` of another table (`x.t`): not this table.
                let own = j == 0 || schema.is_some_and(|sc| ids[chain[j - 1]].text == sc);
                if own {
                    replace.push((ids[chain[j]].start, ids[chain[j]].end));
                }
                break;
            }
        }
        k = next;
    }
    let mut out = String::new();
    let mut last = 0;
    for (a, z) in replace {
        out.push_str(&expr[last..a]);
        out.push_str(&ident_text(clone));
        last = z;
    }
    out.push_str(&expr[last..]);
    Some(out)
}

/// CockroachDB's table parts `database_schema` doesn't carry, from its own
/// `SHOW CREATE TABLE`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Crdb {
    /// `FAMILY f1 (id, t)`, as written.
    pub families: Vec<String>,
    /// Zone configurations: the index (`None`: the table) and what follows
    /// `CONFIGURE ZONE ` (`USING gc.ttlseconds = 600`).
    pub zones: Vec<Zone>,
    /// A hash-sharded primary key: its name and `PRIMARY KEY (id ASC)
    /// USING HASH WITH (bucket_count=16)`.
    pub hash_primary: Option<(String, String)>,
    /// Hash-sharded indexes: name, unique, and their definition after the
    /// table (`USING btree (n ASC) USING HASH WITH (bucket_count=16)`).
    pub hash_indexes: Vec<(String, bool, String)>,
}

/// A zone configuration: the index (`None`: the table) and its body.
type Zone = (Option<String>, String);

/// Splits `SHOW CREATE TABLE`'s text into the parts [`Crdb`] keeps.
/// `Err`: what the clone couldn't have (partitions).
/// Also the hash-sharded primary key's clause (its name comes from the
/// catalog).
fn parse_crdb(create: &str) -> std::result::Result<(Crdb, Option<String>), String> {
    let mut families = Vec::new();
    let mut primary = None;
    let mut zones = Vec::new();
    let (table, rest) = match create.find(";\n") {
        Some(i) => (&create[..i], &create[i + 2..]),
        None => (create, ""),
    };
    for line in table.lines() {
        let l = line.trim().trim_end_matches(',');
        if l.starts_with("FAMILY ") {
            families.push(l.to_string());
        } else if l.starts_with("CONSTRAINT ") && l.contains(" USING HASH") {
            if let Some(i) = l.find("PRIMARY KEY ") {
                primary = Some(l[i..].to_string());
            }
        }
        if l.starts_with("PARTITION BY ") || l.contains(" PARTITION BY ") {
            return Err("la tabla está particionada (PARTITION BY): el clon sería una tabla común, sin sus particiones".into());
        }
    }
    for st in rest.split(";\n").map(|s| s.trim().trim_end_matches(';')).filter(|s| !s.is_empty()) {
        let Some(i) = st.find(" CONFIGURE ZONE ") else { continue };
        let body = st[i + " CONFIGURE ZONE ".len()..].trim().to_string();
        let head = &st[..i];
        if head.starts_with("ALTER TABLE ") {
            zones.push((None, body));
        } else if head.starts_with("ALTER INDEX ") {
            let ix = head.rsplit_once('@').map(|(_, n)| n.trim().trim_matches('"').replace("\"\"", "\"")).unwrap_or_default();
            zones.push((Some(ix), body));
        } else {
            return Err(format!("tiene una configuración de zona que el clon no puede tener ({})", head.trim()));
        }
    }
    Ok((Crdb { families, zones, ..Default::default() }, primary))
}

async fn crdb(s: &mut dyn Session, schema: Option<&str>, name: &str) -> Result<Crdb> {
    let rel = qualified(schema, name);
    let create = rows(s, &format!("SHOW CREATE TABLE {rel}"))
        .await?
        .into_iter()
        .next()
        .and_then(|r| r.get(1).cloned().flatten())
        .ok_or_else(|| Error::State(format!("no se pudo leer la definición de «{name}» (SHOW CREATE TABLE)")))?;
    let (mut out, primary) = parse_crdb(&create).map_err(refuse)?;
    let sql = format!(
        "SELECT ic.relname::text, pg_get_indexdef(x.indexrelid), x.indisprimary::text
         FROM pg_index x JOIN pg_class ic ON ic.oid = x.indexrelid WHERE x.indrelid = {}::regclass",
        lit(&rel)
    );
    for r in rows(s, &sql).await? {
        let at = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let (ix, def) = (at(0), at(1));
        if !def.contains(" USING HASH") {
            continue;
        }
        if truth(Some(at(2))) {
            let clause = primary.clone().ok_or_else(|| refuse(format!("no se pudo leer la clave primaria con hash de «{name}»")))?;
            out.hash_primary = Some((ix, clause));
            continue;
        }
        // `CREATE [UNIQUE] INDEX x ON t USING btree (n ASC) USING HASH …`:
        // what follows the table.
        let on = def.find(" ON ").ok_or_else(|| refuse(format!("no se pudo leer el índice con hash «{ix}»")))?;
        let using = def[on..].find(" USING ").map(|i| on + i + 1).ok_or_else(|| refuse(format!("no se pudo leer el índice con hash «{ix}»")))?;
        out.hash_indexes.push((ix, def.starts_with("CREATE UNIQUE "), def[using..].trim().to_string()));
    }
    out.hash_indexes.sort();
    out.zones.sort();
    Ok(out)
}

/// The position of the `)` closing the first `(` of `sql` (quotes skipped).
fn closing_paren(sql: &str) -> Option<usize> {
    let (mut depth, mut quote) = (0usize, None::<char>);
    for (i, ch) in sql.char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `items` added to the `CREATE TABLE`'s list (after its columns and
/// constraints).
fn into_create(create: &str, items: &[String]) -> Option<String> {
    let start = create.find("CREATE ")?;
    let close = start + closing_paren(&create[start..])?;
    let body_end = create[..close].trim_end().len();
    let added: String = items.iter().map(|i| format!(",\n    {i}")).collect();
    Some(format!("{}{added}\n{}", &create[..body_end], &create[close..]))
}

/// `WITH (options)` after the `CREATE TABLE`'s list.
fn with_create(create: &str, options: &[String]) -> Option<String> {
    let start = create.find("CREATE ")?;
    let close = start + closing_paren(&create[start..])?;
    if create[close + 1..].trim_start().starts_with("WITH") {
        return None;
    }
    let list: Vec<String> = options.iter().map(|o| crdb_option(o)).collect();
    Some(format!("{} WITH ({}){}", &create[..=close], list.join(", "), &create[close + 1..]))
}

/// CockroachDB's storage parameters that go in the `CREATE` (all but
/// `schema_locked`, the server's default, which later `ALTER`s would
/// trip over).
fn crdb_create_option(o: &str) -> bool {
    o.split_once('=').map(|(k, _)| k.trim()).unwrap_or(o) != "schema_locked"
}

/// `key = value` from CockroachDB's `reloptions` (its values are already
/// literals: `ttl_expire_after='30 days':::INTERVAL`).
fn crdb_option(o: &str) -> String {
    match o.split_once('=') {
        Some((k, v)) => format!("{} = {}", k.trim(), v.trim()),
        None => o.to_string(),
    }
}

/// A comment on one of the table's indexes or constraints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PgComment {
    /// `true`: an index; `false`: a constraint.
    pub index: bool,
    pub name: String,
    pub text: String,
}

/// Comments on the indexes and constraints of a table, sorted.
async fn object_comments(s: &mut dyn Session, schema: Option<&str>, name: &str) -> Result<Vec<PgComment>> {
    let rel = lit(&qualified(schema, name));
    let sql = format!(
        "SELECT 'i', ic.relname::text, d.description
         FROM pg_index x JOIN pg_class ic ON ic.oid = x.indexrelid
         JOIN pg_description d ON d.objoid = ic.oid AND d.classoid = 'pg_class'::regclass AND d.objsubid = 0
         WHERE x.indrelid = {rel}::regclass
         UNION ALL
         SELECT 'c', c.conname::text, d.description
         FROM pg_constraint c
         JOIN pg_description d ON d.objoid = c.oid AND d.classoid = 'pg_constraint'::regclass
         WHERE c.conrelid = {rel}::regclass"
    );
    let mut out: Vec<PgComment> = rows(s, &sql)
        .await?
        .into_iter()
        .filter_map(|r| {
            let at = |i: usize| r.get(i).cloned().flatten();
            Some(PgComment { index: at(0)?.trim() == "i", name: at(1)?, text: at(2)? })
        })
        .collect();
    out.sort_by(|a, b| (a.index, &a.name).cmp(&(b.index, &b.name)));
    Ok(out)
}

/// `COMMENT ON` for one of the clone's indexes or constraints.
fn comment_sql(cockroach: bool, schema: Option<&str>, table: &str, c: &PgComment) -> String {
    let clone_q = qualified(schema, table);
    let text = lit(&c.text);
    if !c.index {
        return format!("COMMENT ON CONSTRAINT {} ON {clone_q} IS {text}", qi(&c.name));
    }
    if cockroach {
        // CockroachDB's index names are per table: `table@index`.
        format!("COMMENT ON INDEX {clone_q}@{} IS {text}", qi(&c.name))
    } else {
        format!("COMMENT ON INDEX {} IS {text}", qualified(schema, &c.name))
    }
}

/// The original's comments on indexes and constraints, on the clone's
/// renamed ones, read back and compared. Returns the notes for those the
/// clone can't have (its index isn't created without indexes;
/// CockroachDB: one on a `NOT VALID` constraint, which it stores broken).
pub(super) async fn copy_comments(
    tgt: &mut dyn Session,
    pg: &PgTable,
    clone: &TableSchema,
    renames: &[super::Rename],
) -> Result<Vec<String>> {
    let Some(comments) = pg.comments.as_ref().filter(|c| !c.is_empty()) else {
        return Ok(Vec::new());
    };
    let to = |from: &str| renames.iter().find(|r| r.from == from).map(|r| r.to.clone()).unwrap_or_else(|| from.to_string());
    let (schema, table) = (clone.schema.as_deref(), clone.name.as_str());
    let rel = lit(&qualified(schema, table));
    let first = |v: Vec<Vec<Option<String>>>, not_valid: bool| -> Vec<String> {
        v.into_iter().filter(|r| !not_valid || !truth(r.get(1).cloned().flatten())).filter_map(|r| r.into_iter().next().flatten()).collect()
    };
    let indexes = format!("SELECT ic.relname::text FROM pg_index x JOIN pg_class ic ON ic.oid = x.indexrelid WHERE x.indrelid = {rel}::regclass");
    let indexes = first(rows(tgt, &indexes).await?, false);
    let constraints = rows(tgt, &format!("SELECT conname::text, convalidated::text FROM pg_constraint WHERE conrelid = {rel}::regclass")).await?;
    let not_valid = if pg.cockroach { first(constraints.clone(), true) } else { Vec::new() };
    let constraints = first(constraints, false);
    let (mut expected, mut no_index, mut broken) = (Vec::new(), Vec::new(), Vec::new());
    for c in comments {
        let renamed = PgComment { index: c.index, name: to(&c.name), text: c.text.clone() };
        let have = if c.index { &indexes } else { &constraints };
        if !have.contains(&renamed.name) {
            no_index.push(c.name.clone());
            continue;
        }
        if !c.index && not_valid.contains(&renamed.name) {
            broken.push(c.name.clone());
            continue;
        }
        exec(tgt, &comment_sql(pg.cockroach, schema, table, &renamed)).await.map_err(|e| {
            let what = if c.index { "del índice" } else { "de la restricción" };
            Error::Query(format!("no se pudo copiar el comentario {what} «{}» al clon: {e}", renamed.name))
        })?;
        expected.push(renamed);
    }
    expected.sort_by(|a, b| (a.index, &a.name).cmp(&(b.index, &b.name)));
    let got = object_comments(tgt, schema, table)
        .await
        .map_err(|e| Error::Query(format!("no se pudieron leer los comentarios de los índices y las restricciones del clon: {e}")))?;
    if got != expected {
        let w = |v: &[PgComment]| v.iter().map(|c| format!("{}: {}", c.name, c.text)).collect::<Vec<_>>().join(", ");
        return Err(Error::State(format!(
            "el clon no quedó igual al original (comentarios de índices y restricciones: {} / {}); no se clona",
            w(&expected),
            w(&got)
        )));
    }
    let mut notes = Vec::new();
    if !no_index.is_empty() {
        notes.push(format!("sin índices: tampoco se copian sus comentarios ({})", no_index.join(", ")));
    }
    if !broken.is_empty() {
        notes.push(format!(
            "no se copian los comentarios de las restricciones NOT VALID ({}): CockroachDB los guarda mal y dejan de poder leerse los comentarios de toda la base",
            broken.join(", ")
        ));
    }
    Ok(notes)
}

/// A foreign key, split around the table it references.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PgForeignKey {
    pub name: String,
    /// `FOREIGN KEY (a, b) REFERENCES `.
    pub head: String,
    /// The referenced table, qualified; `None`: the table itself.
    pub target: Option<String>,
    /// `(x, y)` and what follows (`MATCH FULL ON DELETE … DEFERRABLE…`).
    pub rest: String,
}

impl PgForeignKey {
    /// `def` split at the referenced column list (`refcols` as
    /// `pg_get_constraintdef` writes it). `None` when it isn't there.
    fn split(name: String, def: &str, refcols: &str, target: Option<String>) -> Option<Self> {
        const R: &str = " REFERENCES ";
        let i = def.find(R)? + R.len();
        let cols = format!("({refcols})");
        let j = i + def[i..].find(&cols)?;
        Some(PgForeignKey { name, head: def[..i].to_string(), target, rest: def[j..].trim_end().to_string() })
    }

    /// Without the table's name: the original's and the clone's compare
    /// equal (a reference to the table itself is one to the clone).
    fn shape(&self) -> String {
        format!("{}{}{}", self.head, self.target.as_deref().unwrap_or("(la misma tabla)"), self.rest)
    }
}

async fn foreign_keys(s: &mut dyn Session, schema: Option<&str>, name: &str) -> Result<(Vec<PgForeignKey>, Vec<String>)> {
    let rel = lit(&qualified(schema, name));
    let mut fks = Vec::new();
    let mut unread = Vec::new();
    let sql = format!(
        "SELECT c.conname::text, pg_get_constraintdef(c.oid),
                (SELECT string_agg(quote_ident(a.attname), ', ' ORDER BY k.i)
                 FROM unnest(c.confkey) WITH ORDINALITY AS k(n, i)
                 JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.n),
                CASE WHEN c.confrelid <> c.conrelid THEN quote_ident(n.nspname) || '.' || quote_ident(r.relname) END
         FROM pg_constraint c
         JOIN pg_class r ON r.oid = c.confrelid
         JOIN pg_namespace n ON n.oid = r.relnamespace
         WHERE c.conrelid = {rel}::regclass AND c.contype = 'f'"
    );
    for r in rows(s, &sql).await? {
        let at = |i: usize| r.get(i).cloned().flatten();
        let (n, def) = (at(0).unwrap_or_default(), at(1).unwrap_or_default());
        match PgForeignKey::split(n.clone(), &def, &at(2).unwrap_or_default(), at(3)) {
            Some(f) => fks.push(f),
            None => unread.push(if def.is_empty() { n } else { def }),
        }
    }
    Ok((fks, unread))
}

/// Constraints and indexes, to compare with the clone's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Parts {
    /// Check constraints' definitions, sorted.
    pub checks: Vec<String>,
    pub foreign_keys: usize,
    /// Of those, `NOT VALID`.
    pub not_valid_fks: usize,
    /// Their definitions ([`PgForeignKey::shape`]), sorted.
    pub fk_defs: Vec<String>,
    pub indexes: usize,
    /// The primary key's index (CockroachDB: also the hidden `rowid`'s).
    pub primary: usize,
    /// `relpersistence`: `p` (logged), `u` (UNLOGGED).
    pub persistence: String,
}

async fn parts(s: &mut dyn Session, schema: Option<&str>, name: &str) -> Result<Parts> {
    let rel = lit(&qualified(schema, name));
    let mut checks: Vec<String> = rows(s, &format!("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = {rel}::regclass AND contype = 'c' AND conname::text NOT LIKE 'check\\_crdb\\_internal%'"))
        .await?
        .into_iter()
        .filter_map(|r| r.into_iter().next().flatten())
        .collect();
    checks.sort();
    let count = |v: Option<String>| v.and_then(|n| n.trim().parse::<usize>().ok()).unwrap_or(0);
    let foreign_keys = count(one(s, &format!("SELECT count(*) FROM pg_constraint WHERE conrelid = {rel}::regclass AND contype = 'f'")).await?);
    let not_valid_fks =
        count(one(s, &format!("SELECT count(*) FROM pg_constraint WHERE conrelid = {rel}::regclass AND contype = 'f' AND NOT convalidated")).await?);
    let indexes = count(one(s, &format!("SELECT count(*) FROM pg_index WHERE indrelid = {rel}::regclass")).await?);
    let primary = count(one(s, &format!("SELECT count(*) FROM pg_index WHERE indrelid = {rel}::regclass AND indisprimary")).await?);
    let persistence = one(s, &format!("SELECT relpersistence::text FROM pg_class WHERE oid = {rel}::regclass")).await?.unwrap_or_default().trim().to_string();
    let (fks, unread) = self::foreign_keys(s, schema, name).await?;
    let mut fk_defs: Vec<String> = fks.iter().map(PgForeignKey::shape).chain(unread).collect();
    fk_defs.sort();
    Ok(Parts { checks, foreign_keys, not_valid_fks, fk_defs, indexes, primary, persistence })
}

async fn columns(s: &mut dyn Session, version: i64, cockroach: bool, schema: Option<&str>, name: &str) -> Result<Vec<PgColumn>> {
    let rel = lit(&qualified(schema, name));
    let identity = if version >= 100000 { "a.attidentity::text" } else { "''" };
    let generated = if version >= 120000 { "a.attgenerated::text" } else { "''" };
    let collation = if version >= 90100 {
        "CASE WHEN a.attcollation <> 0 AND a.attcollation <> t.typcollation THEN
            CASE WHEN cn.nspname = 'pg_catalog' THEN quote_ident(co.collname)
                 ELSE quote_ident(cn.nspname) || '.' || quote_ident(co.collname) END END"
    } else {
        "NULL::text"
    };
    let joins = if version >= 90100 {
        "LEFT JOIN pg_collation co ON co.oid = a.attcollation LEFT JOIN pg_namespace cn ON cn.oid = co.collnamespace"
    } else {
        ""
    };
    // CockroachDB: hidden (`NOT VISIBLE`) columns and `ON UPDATE`, which
    // only its `information_schema` says.
    let (hidden, on_update) = if cockroach {
        let ic = "FROM information_schema.columns ic JOIN pg_class ct ON ct.oid = a.attrelid JOIN pg_namespace nt ON nt.oid = ct.relnamespace
                  WHERE ic.table_schema = nt.nspname AND ic.table_name = ct.relname AND ic.column_name = a.attname";
        (format!("COALESCE((SELECT ic.is_hidden = 'YES' {ic} LIMIT 1), false)::text"), format!("(SELECT ic.column_on_update {ic} LIMIT 1)"))
    } else {
        ("'false'".to_string(), "NULL::text".to_string())
    };
    let sql = format!(
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod), {identity}, {generated},
                pg_get_expr(d.adbin, d.adrelid), {collation},
                (SELECT pg_get_serial_sequence({rel}, a.attname)
                 WHERE EXISTS (SELECT 1 FROM pg_depend dp JOIN pg_class sc ON sc.oid = dp.objid
                               WHERE sc.relkind = 'S' AND dp.refobjid = a.attrelid AND dp.refobjsubid = a.attnum
                                 AND dp.deptype IN ('a', 'i'))),
                {hidden}, a.attnotnull::text, {on_update}, col_description(a.attrelid, a.attnum)
         FROM pg_attribute a
         JOIN pg_type t ON t.oid = a.atttypid
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
         {joins}
         WHERE a.attrelid = {rel}::regclass AND a.attnum > 0 AND NOT a.attisdropped
         ORDER BY a.attnum"
    );
    Ok(rows(s, &sql)
        .await?
        .into_iter()
        .map(|r| {
            let at = |i: usize| r.get(i).cloned().flatten();
            PgColumn {
                name: at(0).unwrap_or_default(),
                type_name: at(1).unwrap_or_default(),
                identity: at(2).unwrap_or_default().trim().to_string(),
                generated: at(3).unwrap_or_default().trim().to_string(),
                default: at(4).filter(|d| !d.is_empty()),
                collation: at(5).filter(|c| !c.is_empty()),
                sequence: at(6).filter(|c| !c.is_empty()),
                hidden: truth(at(7)),
                not_null: truth(at(8)),
                on_update: at(9).filter(|c| !c.is_empty()),
                comment: at(10).filter(|c| !c.is_empty()),
            }
        })
        .collect())
}

fn refuse(why: String) -> Error {
    Error::Unsupported(format!("no se puede clonar: {why}"))
}

/// Read the original (and refuse what a plain table can't be).
pub(super) async fn inspect(s: &mut dyn Session, driver: &dyn Driver, schema: Option<&str>, name: &str) -> Result<PgTable> {
    let version = one(s, "SELECT current_setting('server_version_num')").await?.and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(0);
    let rel = lit(&qualified(schema, name));
    let sch = match schema.filter(|s| !s.is_empty()) {
        Some(s) => lit(s),
        None => "current_schema()".into(),
    };
    let found = one(
        s,
        &format!(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {sch} AND c.relname = {}",
            lit(name)
        ),
    )
    .await?;
    if found.as_deref().unwrap_or("0") == "0" {
        return Err(Error::State(format!("no se encontró la tabla «{name}»")));
    }
    let kind = one(s, &format!("SELECT relkind::text FROM pg_class WHERE oid = {rel}::regclass")).await?.unwrap_or_default();
    if kind == "p" {
        return Err(refuse(
            "la tabla está particionada (PARTITION BY): el clon sería una tabla común, sin su clave de partición ni sus particiones".into(),
        ));
    }
    if one(s, "SELECT count(*) FROM pg_extension WHERE extname = 'timescaledb'").await?.as_deref().unwrap_or("0") != "0" {
        let n = one(
            s,
            &format!("SELECT count(*) FROM _timescaledb_catalog.hypertable WHERE schema_name = {sch} AND table_name = {}", lit(name)),
        )
        .await?;
        if n.as_deref().unwrap_or("0") != "0" {
            return Err(refuse(
                "es una hypertable de TimescaleDB: el clon sería una tabla común, sin su partición por tiempo ni sus chunks".into(),
            ));
        }
    }
    let parents = one(s, &format!("SELECT string_agg(inhparent::regclass::text, ', ') FROM pg_inherits WHERE inhrelid = {rel}::regclass")).await?;
    if let Some(p) = parents.filter(|p| !p.is_empty()) {
        return Err(refuse(format!(
            "es una partición o hereda de {p} (INHERITS): el clon sería una tabla suelta, fuera de esa jerarquía"
        )));
    }
    let children = one(s, &format!("SELECT count(*) FROM pg_inherits WHERE inhparent = {rel}::regclass")).await?;
    if let Some(n) = children.filter(|n| n != "0") {
        return Err(refuse(format!(
            "{n} tabla(s) heredan de ella (INHERITS): sus filas se leen junto con las de la tabla y el clon las mezclaría sin la jerarquía"
        )));
    }
    let cockroach = driver.info().id == "cockroachdb";
    let columns = columns(s, version, cockroach, schema, name).await?;
    // CockroachDB's index names only have to be unique within their table.
    let not_indexes = if cockroach { " AND c.relkind NOT IN ('i', 'I')" } else { "" };
    let taken = rows(
        s,
        &format!(
            "SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {sch}{not_indexes}
             UNION SELECT t.typname FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE n.nspname = {sch}"
        ),
    )
    .await?
    .into_iter()
    .filter_map(|r| r.into_iter().next().flatten())
    .collect();
    let deferred_checks = rows(s, &format!("SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = {rel}::regclass AND contype = 'c'"))
        .await?
        .into_iter()
        .filter_map(|r| {
            let mut r = r.into_iter();
            let (n, d) = (r.next().flatten()?, r.next().flatten()?);
            deferred_check(&d).then_some((n, d))
        })
        .collect();
    let not_valid_fks = rows(s, &format!("SELECT conname::text FROM pg_constraint WHERE conrelid = {rel}::regclass AND contype = 'f' AND NOT convalidated"))
        .await?
        .into_iter()
        .filter_map(|r| r.into_iter().next().flatten())
        .collect();
    let parts = parts(s, schema, name).await?;
    let (foreign_keys, _) = self::foreign_keys(s, schema, name).await?;
    let comments = object_comments(s, schema, name).await.ok();
    let settings = settings(s, version, cockroach, schema, name).await?;
    let crdb = if cockroach { Some(crdb(s, schema, name).await?) } else { None };
    let rls_hides_rows = settings.rls && rls_active(s, &rel).await;
    let extras = extras(s, version, cockroach, schema, name, true).await?;
    let pg = PgTable {
        version,
        cockroach,
        columns,
        taken,
        parts,
        deferred_checks,
        not_valid_fks,
        foreign_keys,
        comments,
        settings,
        crdb,
        rls_hides_rows,
        schema: schema.map(str::to_string),
        name: name.to_string(),
        extras,
    };
    // A policy that reads the table itself must read the clone: refused
    // now when that reference can't be told apart.
    for p in &pg.settings.policies {
        for e in [&p.using, &p.check].into_iter().flatten() {
            if retarget(e, schema, name, "x", &pg.columns).is_none() {
                return Err(refuse(format!(
                    "la política de seguridad por filas «{}» nombra a la propia tabla «{name}» de una forma que no se puede pasar al clon ({e}); en el clon seguiría leyendo la original",
                    p.name
                )));
            }
        }
    }
    Ok(pg)
}

/// Whether row-level security filters this user's reads of `rel` (a
/// literal). CockroachDB has no `row_security_active`: the owner and those
/// with `BYPASSRLS` (or admins) see every row unless it's `FORCE`d.
async fn rls_active(s: &mut dyn Session, rel: &str) -> bool {
    if let Ok(v) = one(s, &format!("SELECT row_security_active({rel}::regclass)::text")).await {
        return truth(v);
    }
    let sql = format!(
        "SELECT (SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = current_user)::text,
                (c.relowner = (SELECT oid FROM pg_roles WHERE rolname = current_user))::text,
                c.relforcerowsecurity::text
         FROM pg_class c WHERE c.oid = {rel}::regclass"
    );
    match rows(s, &sql).await.ok().and_then(|r| r.into_iter().next()) {
        Some(r) => {
            let at = |i: usize| truth(r.get(i).cloned().flatten());
            !at(0) && (at(2) || !at(1))
        }
        // Unknown: as if it did (refused with rows, not copied short).
        None => true,
    }
}

/// A CHECK that `CREATE TABLE` can't write as the original has it.
fn deferred_check(def: &str) -> bool {
    let d = def.trim_end();
    d.ends_with(" NOT VALID") || d.contains(" NO INHERIT")
}

/// Takes out of the plan what must be added after the rows as the original
/// has it (see [`PgTable::deferred_checks`]); returns those statements,
/// and marks `UNLOGGED` in the `CREATE`.
pub(super) fn defer(driver: &dyn Driver, plan: &mut super::ClonePlan, pg: &PgTable) -> Result<Deferred> {
    let to = |from: &str| plan.renames.iter().find(|r| r.from == from).map(|r| r.to.clone()).unwrap_or_else(|| from.to_string());
    let clone_q = qualified(plan.table.schema.as_deref(), &plan.table.name);
    let mut out = Deferred::default();
    for (name, def) in &pg.deferred_checks {
        let n = to(name);
        plan.table.checks.retain(|c| c.name.as_deref() != Some(n.as_str()));
        out.checks.push(format!("ALTER TABLE {clone_q} ADD CONSTRAINT {} {def};", qi(&n)));
    }
    // Foreign keys as the catalog writes them (MATCH FULL, DEFERRABLE,
    // INITIALLY DEFERRED, NOT VALID), the name and a reference to the
    // table itself changed to the clone's.
    let mut kept = Vec::new();
    for fk in std::mem::take(&mut plan.table.foreign_keys) {
        match pg.foreign_keys.iter().find(|p| fk.name.as_deref() == Some(to(&p.name).as_str())) {
            Some(p) => out.foreign_keys.push(format!(
                "ALTER TABLE {clone_q} ADD CONSTRAINT {} {}{}{};",
                qi(&to(&p.name)),
                p.head,
                p.target.as_deref().unwrap_or(&clone_q),
                p.rest
            )),
            None => kept.push(fk),
        }
    }
    let not_valid: Vec<String> = pg.not_valid_fks.iter().map(|n| to(n)).collect();
    let (later, now): (Vec<ForeignKeyDef>, Vec<ForeignKeyDef>) =
        kept.into_iter().partition(|f| f.name.as_ref().is_some_and(|n| not_valid.contains(n)));
    plan.table.foreign_keys = now;
    for fk in later {
        let one = TableSchema { schema: plan.table.schema.clone(), name: plan.table.name.clone(), foreign_keys: vec![fk], ..Default::default() };
        let s = driver.table_ddl(&one, DdlParts { foreign_keys: true, ..Default::default() })?;
        let s = s.trim_end();
        out.foreign_keys.push(format!("{} NOT VALID;", s.strip_suffix(';').unwrap_or(s)));
    }
    out.unlogged = pg.parts.persistence == "u";
    // CockroachDB: column families and a hash-sharded primary key go in
    // the CREATE; hash-sharded indexes as the original's (`database_schema`
    // gives them on their hidden shard column, which the clone doesn't have
    // until the index makes it).
    if let Some(c) = &pg.crdb {
        if let Some((name, clause)) = &c.hash_primary {
            let n = plan.renames.iter().find(|r| &r.from == name).map(|r| r.to.clone()).unwrap_or_else(|| {
                if name.ends_with("_pkey") { format!("{}_pkey", plan.table.name) } else { name.clone() }
            });
            out.create_items.push(format!("CONSTRAINT {} {clause}", qi(&n)));
        }
        out.create_items.extend(c.families.iter().cloned());
        // Its storage parameters in the CREATE, as `SHOW CREATE TABLE`
        // writes them: the row-level TTL (`ttl_expire_after`…) can only be
        // set there on a table that already has its
        // `crdb_internal_expiration` column (added as the original's).
        out.create_with = pg.settings.options.iter().filter(|o| crdb_create_option(o)).cloned().collect();
        for (name, unique, def) in &c.hash_indexes {
            let n = to(name);
            plan.table.indexes.retain(|i| i.name != n);
            let u = if *unique { "UNIQUE " } else { "" };
            out.indexes.push(format!("CREATE {u}INDEX {} ON {clone_q} {def};", qi(&n)));
        }
    }
    // Extended statistics: renamed like the constraints (reported with them).
    for (from, to) in statistics_names(pg, &plan.table.name) {
        if from != to && !plan.renames.iter().any(|r| r.from == from) {
            plan.renames.push(super::Rename { from, to, shortened: false });
        }
    }
    Ok(out)
}

/// What [`defer`] took out of the plan.
#[derive(Debug, Default)]
pub(super) struct Deferred {
    /// `ALTER TABLE … ADD CONSTRAINT … CHECK …`, after the rows.
    pub checks: Vec<String>,
    /// Foreign keys `NOT VALID`, with the others.
    pub foreign_keys: Vec<String>,
    pub unlogged: bool,
    /// Added to the `CREATE TABLE`'s list (CockroachDB: families, a
    /// hash-sharded primary key).
    pub create_items: Vec<String>,
    /// `CREATE INDEX`, with the others (CockroachDB: hash-sharded ones).
    pub indexes: Vec<String>,
    /// CockroachDB: the `CREATE TABLE`'s `WITH (…)` (`key=value`, the
    /// value as the catalog writes it: a literal).
    pub create_with: Vec<String>,
}

impl Deferred {
    /// The `CREATE` as `UNLOGGED` when the original is, and the foreign
    /// keys `NOT VALID` with the others.
    pub(super) fn apply(&self, create: &mut String, foreign_keys: &mut Option<String>, indexes: &mut Option<String>, with_indexes: bool) -> Result<()> {
        if !self.create_items.is_empty() {
            *create = into_create(create, &self.create_items)
                .ok_or_else(|| refuse("no se pudieron agregar al CREATE las familias de columnas o la clave primaria con hash".into()))?;
        }
        if !self.create_with.is_empty() {
            *create = with_create(create, &self.create_with)
                .ok_or_else(|| refuse("no se pudieron agregar al CREATE las opciones de la tabla (WITH …)".into()))?;
        }
        if with_indexes && !self.indexes.is_empty() {
            let mut all: Vec<String> = indexes.take().into_iter().collect();
            all.extend(self.indexes.iter().cloned());
            *indexes = Some(all.join("\n"));
        }
        if self.unlogged {
            if !create.contains("CREATE TABLE ") {
                return Err(refuse("la tabla es UNLOGGED y no se pudo crear el clon igual".into()));
            }
            *create = create.replacen("CREATE TABLE ", "CREATE UNLOGGED TABLE ", 1);
        }
        if !self.foreign_keys.is_empty() {
            let mut all: Vec<String> = foreign_keys.take().into_iter().collect();
            all.extend(self.foreign_keys.iter().cloned());
            *foreign_keys = Some(all.join("\n"));
        }
        Ok(())
    }
}

/// The `serial` type behind an integer column that owns its sequence.
fn serial_type(type_name: &str) -> Option<&'static str> {
    match type_name.trim().to_ascii_lowercase().as_str() {
        "smallint" | "int2" => Some("smallserial"),
        "integer" | "int" | "int4" => Some("serial"),
        "bigint" | "int8" => Some("bigserial"),
        _ => None,
    }
}

/// The clone's columns as the catalog says (`database_schema` flattens
/// identity kinds, collations, serials and CockroachDB's computed
/// columns). Returns what must run before the `CREATE`.
pub(super) fn adjust(clone: &mut TableSchema, pg: &PgTable, notes: &mut Vec<String>) -> Result<String> {
    let mut prelude = String::new();
    // CockroachDB's hidden (`NOT VISIBLE`) columns, which `database_schema`
    // leaves out, in their place (the row-level TTL's
    // `crdb_internal_expiration` among them); not the implicit `rowid`
    // key nor a hash-sharded index's shard column (the clone makes its own).
    let mut at = 0;
    for p in &pg.columns {
        if let Some(i) = clone.columns.iter().position(|c| c.name == p.name) {
            at = i + 1;
            continue;
        }
        if !p.hidden || shard_column(p) || implicit_rowid(p) {
            continue;
        }
        let c = dbine_driver::ColumnDef {
            name: p.name.clone(),
            data_type: p.type_name.clone(),
            nullable: !p.not_null,
            default_value: p.default.clone(),
            auto_increment: !p.identity.is_empty() || p.sequence.is_some(),
            comment: p.comment.clone(),
            ..Default::default()
        };
        clone.columns.insert(at, c);
        at += 1;
    }
    for c in &mut clone.columns {
        let Some(p) = pg.columns.iter().find(|p| p.name == c.name) else { continue };
        let collate = p.collation.as_deref().map(|x| format!(" COLLATE {x}")).unwrap_or_default();
        if !p.generated.is_empty() {
            let how = match p.generated.as_str() {
                "s" => "STORED",
                "v" => "VIRTUAL",
                other => return Err(refuse(format!("la columna «{}» es calculada de un tipo desconocido ({other})", c.name))),
            };
            let expr = p.default.clone().ok_or_else(|| refuse(format!("no se pudo leer la expresión de la columna calculada «{}»", c.name)))?;
            c.data_type = format!("{}{collate} GENERATED ALWAYS AS ({expr}) {how}{}", p.type_name, if p.hidden { " NOT VISIBLE" } else { "" });
            c.default_value = None;
            c.auto_increment = false;
            continue;
        }
        c.data_type = format!("{}{collate}", p.type_name);
        if !p.identity.is_empty() {
            // Written BY DEFAULT (the rows keep their values); ALWAYS is
            // set back after the load (`after_load`).
            c.auto_increment = true;
            c.default_value = None;
        } else if p.sequence.is_some() {
            // serial: a sequence of its own, like the original's.
            let Some(t) = serial_type(&p.type_name) else {
                return Err(refuse(format!(
                    "la columna «{}» ({}) toma valores de una secuencia propia y ese tipo no tiene serial equivalente",
                    c.name, p.type_name
                )));
            };
            c.data_type = t.to_string();
            c.default_value = None;
            c.auto_increment = false;
            if pg.cockroach && prelude.is_empty() {
                prelude.push_str("SET serial_normalization = 'sql_sequence';\n");
            }
        } else {
            c.default_value = p.default.clone();
            if c.auto_increment {
                c.auto_increment = false;
                if let Some(d) = p.default.as_deref() {
                    notes.push(format!("la columna «{}» sigue tomando valores de la misma secuencia que el original ({d})", c.name));
                }
            }
        }
        // CockroachDB: `NOT VISIBLE`, `ON UPDATE` (after the type: the
        // clone's DDL writes the default and the nullability after it).
        if p.hidden {
            c.data_type.push_str(" NOT VISIBLE");
        }
        if let Some(u) = &p.on_update {
            c.data_type.push_str(&format!(" ON UPDATE {u}"));
        }
    }
    Ok(prelude)
}

/// CockroachDB's implicit `rowid` key (a table without a primary key):
/// the clone makes its own.
fn implicit_rowid(c: &PgColumn) -> bool {
    c.hidden && c.name == "rowid" && c.default.as_deref().is_some_and(|d| d.trim() == "unique_rowid()")
}

/// Names the clone's own sequences would get and CockroachDB doesn't
/// choose around (PostgreSQL does).
pub(super) fn sequence_collisions(pg: &PgTable, new_name: &str) -> Vec<String> {
    if !pg.cockroach {
        return Vec::new();
    }
    pg.columns
        .iter()
        .filter(|c| c.generated.is_empty() && c.sequence.is_some())
        .map(|c| format!("{new_name}_{}_seq", c.name))
        .filter(|n| pg.taken.iter().any(|t| t == n))
        .collect()
}

fn number(v: Option<String>, what: &str) -> Result<String> {
    let v = v.unwrap_or_default();
    v.trim().parse::<i128>().map(|n| n.to_string()).map_err(|_| Error::State(format!("secuencia: {what} ilegible ({v})")))
}

fn truth(v: Option<String>) -> bool {
    matches!(v.as_deref().map(str::trim), Some("t" | "true" | "TRUE" | "1"))
}

/// After the rows: every owned sequence with the original's options and
/// current value (or restarted, without rows), and identity `ALWAYS` back.
pub(super) async fn after_load(
    src: &mut dyn Session,
    tgt: &mut dyn Session,
    pg: &PgTable,
    clone: &TableSchema,
    with_data: bool,
) -> Result<()> {
    let clone_q = qualified(clone.schema.as_deref(), &clone.name);
    for c in &pg.columns {
        if !c.generated.is_empty() {
            continue;
        }
        let Some(seq) = c.sequence.as_deref() else { continue };
        let opts = if pg.version >= 100000 {
            format!("SELECT seqstart, seqincrement, seqmin, seqmax, seqcache, seqcycle FROM pg_sequence WHERE seqrelid = {}::regclass", lit(seq))
        } else {
            format!("SELECT start_value, increment_by, min_value, max_value, cache_value, is_cycled FROM {seq}")
        };
        let o = rows(src, &opts).await?.into_iter().next().ok_or_else(|| Error::State(format!("no se pudo leer la secuencia {seq}")))?;
        let at = |i: usize| o.get(i).cloned().flatten();
        let Some(cseq) = one(tgt, &format!("SELECT pg_get_serial_sequence({}, {})", lit(&clone_q), lit(&c.name))).await? else {
            return Err(Error::State(format!("la columna «{}» del clon quedó sin su secuencia", c.name)));
        };
        let cycle = if truth(at(5)) { "CYCLE" } else { "NO CYCLE" };
        // RESTART WITH in the same statement: the clone's sequence stands
        // at 1 (default options), outside a range like the original's
        // (descending, negative) and the new bounds are checked against it.
        let start = number(at(0), "inicio")?;
        exec(
            tgt,
            &format!(
                "ALTER SEQUENCE {cseq} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {start} RESTART WITH {start} CACHE {} {cycle}",
                number(at(1), "incremento")?,
                number(at(2), "mínimo")?,
                number(at(3), "máximo")?,
                number(at(4), "caché")?,
            ),
        )
        .await?;
        if with_data {
            let st = rows(src, &format!("SELECT last_value, is_called FROM {seq}")).await?.into_iter().next().unwrap_or_default();
            let last = number(st.first().cloned().flatten(), "valor actual")?;
            let called = truth(st.get(1).cloned().flatten());
            exec(tgt, &format!("SELECT setval({}, {last}, {called})", lit(&cseq))).await?;
        } else {
            exec(tgt, &format!("ALTER SEQUENCE {cseq} RESTART")).await?;
        }
    }
    for c in pg.columns.iter().filter(|c| c.identity == "a") {
        exec(tgt, &format!("ALTER TABLE {clone_q} ALTER COLUMN {} SET GENERATED ALWAYS", qi(&c.name))).await?;
    }
    Ok(())
}

/// CockroachDB's hidden column of a hash-sharded index
/// (`crdb_internal_n_shard_16`).
fn shard_column(c: &PgColumn) -> bool {
    c.generated == "v" && c.name.starts_with("crdb_internal_") && c.name.contains("_shard_")
}

/// Right after the `CREATE`, before the rows: the original's storage
/// options (the clone's own defaults reset) and column settings.
pub(super) async fn after_create(tgt: &mut dyn Session, pg: &PgTable, clone: &TableSchema, with_data: bool) -> Result<()> {
    if with_data && pg.rls_hides_rows {
        return Err(refuse(
            "la seguridad por filas (ROW LEVEL SECURITY) de la tabla le oculta filas a este usuario, así que el clon no tendría todas; clonala sin datos o con un usuario que no esté sujeto a sus políticas (dueño sin FORCE, BYPASSRLS o superusuario)".into(),
        ));
    }
    let (schema, table) = (clone.schema.as_deref(), clone.name.as_str());
    let clone_q = qualified(schema, table);
    let fail = |e: Error| Error::Query(format!("opciones de almacenamiento de la tabla y sus columnas: {e}"));
    let now = settings(tgt, pg.version, pg.cockroach, schema, table).await.map_err(fail)?;
    let key = |o: &str| o.split_once('=').map(|(k, _)| k.to_string()).unwrap_or_else(|| o.to_string());
    let reset: Vec<String> = now.options.iter().map(|o| key(o)).filter(|k| !pg.settings.options.iter().any(|o| key(o) == *k)).collect();
    if !reset.is_empty() {
        exec(tgt, &format!("ALTER TABLE {clone_q} RESET ({})", reset.join(", "))).await.map_err(fail)?;
    }
    let set: Vec<String> = pg
        .settings
        .options
        .iter()
        .filter(|o| !now.options.contains(o))
        .map(|o| match o.split_once('=') {
            _ if pg.cockroach => crdb_option(o),
            Some((k, v)) => format!("{k} = {}", lit(v)),
            None => o.clone(),
        })
        .collect();
    if !set.is_empty() {
        exec(tgt, &format!("ALTER TABLE {clone_q} SET ({})", set.join(", "))).await.map_err(fail)?;
    }
    for c in &pg.settings.columns {
        for sql in c.sql(&clone_q)? {
            exec(tgt, &sql).await.map_err(fail)?;
        }
    }
    Ok(())
}

/// After the rows, indexes and foreign keys: row-level security (policies,
/// then `ENABLE` / `FORCE`) and CockroachDB's zone configurations. Returns
/// the notes for what the clone can't have without indexes.
pub(super) async fn finish(tgt: &mut dyn Session, pg: &PgTable, clone: &TableSchema, renames: &[super::Rename], with_indexes: bool) -> Result<Vec<String>> {
    let clone_q = qualified(clone.schema.as_deref(), &clone.name);
    let mut notes = Vec::new();
    let rls = |e: Error| Error::Query(format!("seguridad por filas (ROW LEVEL SECURITY): {e}"));
    for p in clone_policies(pg, &clone.name) {
        exec(tgt, &p.sql(&clone_q)).await.map_err(rls)?;
    }
    notes.extend(finish_extras(tgt, pg, clone, renames).await?);
    if pg.settings.rls {
        exec(tgt, &format!("ALTER TABLE {clone_q} ENABLE ROW LEVEL SECURITY")).await.map_err(rls)?;
    }
    if pg.settings.force_rls {
        exec(tgt, &format!("ALTER TABLE {clone_q} FORCE ROW LEVEL SECURITY")).await.map_err(rls)?;
    }
    if let Some(c) = &pg.crdb {
        let (zones, skipped) = clone_zones(c, clone, renames, with_indexes);
        for (ix, body) in &zones {
            let target = match ix {
                None => format!("TABLE {clone_q}"),
                Some(i) => format!("INDEX {clone_q}@{}", qi(i)),
            };
            exec(tgt, &format!("ALTER {target} CONFIGURE ZONE {body}"))
                .await
                .map_err(|e| Error::Query(format!("configuración de zona: {e}")))?;
        }
        if !skipped.is_empty() {
            notes.push(format!("sin índices: tampoco se copian sus configuraciones de zona ({})", skipped.join(", ")));
        }
    }
    Ok(notes)
}

/// The original's policies, their references to the table itself pointed
/// at the clone (checked in [`inspect`]).
fn clone_policies(pg: &PgTable, clone: &str) -> Vec<Policy> {
    let re = |e: &Option<String>| e.as_ref().map(|e| retarget(e, pg.schema.as_deref(), &pg.name, clone, &pg.columns).unwrap_or_else(|| e.clone()));
    pg.settings.policies.iter().map(|p| Policy { using: re(&p.using), check: re(&p.check), ..p.clone() }).collect()
}

/// What the clone's extras must be: the original's, on its renamed
/// indexes (or without them when it doesn't have them) and statistics.
async fn expected_extras(tgt: &mut dyn Session, pg: &PgTable, clone: &TableSchema, renames: &[super::Rename]) -> Result<Extras> {
    let clone_rel = qualified(clone.schema.as_deref(), &clone.name);
    let e = &pg.extras;
    let mut out = Extras { replica: e.replica.clone(), ..Default::default() };
    if let Some(ix) = &e.replica_index {
        match clone_index(tgt, &clone_rel, ix, renames).await? {
            Some(n) => out.replica_index = Some(PgIndexRef { name: n, primary: ix.primary }),
            None if e.replica == "i" => out.replica = "d".into(),
            None => {}
        }
    }
    if let Some(ix) = &e.clustered {
        out.clustered = clone_index(tgt, &clone_rel, ix, renames).await?.map(|n| PgIndexRef { name: n, primary: ix.primary });
    }
    let names = statistics_names(pg, &clone.name);
    for (st, (_, n)) in e.statistics.iter().zip(names) {
        out.statistics.push(PgStatistics { name: n, ..st.clone() });
    }
    out.statistics.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// `REPLICA IDENTITY`, `CLUSTER ON` and extended statistics on the clone;
/// notes for what it can't have (its index wasn't created).
async fn finish_extras(tgt: &mut dyn Session, pg: &PgTable, clone: &TableSchema, renames: &[super::Rename]) -> Result<Vec<String>> {
    let clone_q = qualified(clone.schema.as_deref(), &clone.name);
    let want = expected_extras(tgt, pg, clone, renames).await?;
    let e = &pg.extras;
    let mut notes = Vec::new();
    let fail = |what: &'static str| move |err: Error| Error::Query(format!("{what}: {}", super::in_spanish(err)));
    let identity = match (want.replica.as_str(), &want.replica_index) {
        ("f", _) => Some("FULL".to_string()),
        ("n", _) => Some("NOTHING".to_string()),
        ("i", Some(ix)) => Some(format!("USING INDEX {}", qi(&ix.name))),
        _ => None,
    };
    if let Some(i) = identity {
        exec(tgt, &format!("ALTER TABLE {clone_q} REPLICA IDENTITY {i}")).await.map_err(fail("identidad de réplica (REPLICA IDENTITY)"))?;
    }
    if e.replica == "i" && want.replica != "i" {
        let ix = e.replica_index.as_ref().map(|i| i.name.clone()).unwrap_or_default();
        notes.push(format!(
            "sin índices: el clon queda con la identidad de réplica por omisión (la del original es REPLICA IDENTITY USING INDEX {ix})"
        ));
    }
    if let Some(ix) = &want.clustered {
        exec(tgt, &format!("ALTER TABLE {clone_q} CLUSTER ON {}", qi(&ix.name))).await.map_err(fail("CLUSTER ON"))?;
    } else if let Some(ix) = &e.clustered {
        notes.push(format!("sin índices: el clon no queda marcado para CLUSTER (el original lo está sobre {})", ix.name));
    }
    for st in &want.statistics {
        let q = qualified(Some(&st.schema), &st.name);
        exec(tgt, &format!("CREATE STATISTICS {q} {} FROM {clone_q}", st.body)).await.map_err(fail("estadísticas extendidas (CREATE STATISTICS)"))?;
        if let Some(t) = &st.target {
            exec(tgt, &format!("ALTER STATISTICS {q} SET STATISTICS {}", number(Some(t.clone()), "objetivo de estadísticas")?))
                .await
                .map_err(fail("estadísticas extendidas (CREATE STATISTICS)"))?;
        }
        if let Some(c) = &st.comment {
            exec(tgt, &format!("COMMENT ON STATISTICS {q} IS {}", lit(c))).await.map_err(fail("estadísticas extendidas (CREATE STATISTICS)"))?;
        }
    }
    Ok(notes)
}

/// The original's zone configurations on the clone's (renamed) indexes;
/// and those left out (indexes not created).
fn clone_zones(c: &Crdb, clone: &TableSchema, renames: &[super::Rename], with_indexes: bool) -> (Vec<(Option<String>, String)>, Vec<String>) {
    let to = |from: &str| renames.iter().find(|r| r.from == from).map(|r| r.to.clone()).unwrap_or_else(|| from.to_string());
    let primary = c.hash_primary.as_ref().map(|(n, _)| n.clone());
    let (mut out, mut skipped) = (Vec::new(), Vec::new());
    for (ix, body) in &c.zones {
        match ix {
            None => out.push((None, body.clone())),
            Some(i) => {
                let pk = primary.as_deref() == Some(i.as_str()) || i.ends_with("_pkey");
                if !with_indexes && !pk {
                    skipped.push(i.clone());
                    continue;
                }
                let n = if pk && i.ends_with("_pkey") && !renames.iter().any(|r| &r.from == i) { format!("{}_pkey", clone.name) } else { to(i) };
                out.push((Some(n), body.clone()));
            }
        }
    }
    out.sort();
    (out, skipped)
}

/// Differences between the original's columns and the clone's, as the
/// catalog has them (a serial's default names its own sequence: compared
/// by having one).
pub(super) fn differences(original: &[PgColumn], clone: &[PgColumn]) -> Vec<String> {
    let mut out = Vec::new();
    for a in original {
        let Some(b) = clone.iter().find(|b| b.name == a.name) else {
            // CockroachDB: a hash-sharded index's hidden column, which the
            // clone has only with its index (compared with the indexes).
            if shard_column(a) {
                continue;
            }
            out.push(format!("falta la columna «{}»", a.name));
            continue;
        };
        let n = &a.name;
        if a.type_name != b.type_name {
            out.push(format!("{n}: tipo {} / {}", a.type_name, b.type_name));
        }
        if a.identity != b.identity {
            let w = |i: &str| match i {
                "a" => "GENERATED ALWAYS AS IDENTITY",
                "d" => "GENERATED BY DEFAULT AS IDENTITY",
                _ => "sin identidad",
            };
            out.push(format!("{n}: {} / {}", w(&a.identity), w(&b.identity)));
        }
        if a.generated != b.generated {
            out.push(format!("{n}: columna calculada «{}» / «{}»", a.generated, b.generated));
        }
        if a.hidden != b.hidden {
            let w = |h: bool| if h { "oculta (NOT VISIBLE)" } else { "visible" };
            out.push(format!("{n}: {} / {}", w(a.hidden), w(b.hidden)));
        }
        if a.on_update != b.on_update {
            let w = |u: &Option<String>| u.clone().unwrap_or_else(|| "sin ON UPDATE".into());
            out.push(format!("{n}: ON UPDATE {} / {}", w(&a.on_update), w(&b.on_update)));
        }
        if a.collation != b.collation {
            let w = |c: &Option<String>| c.clone().unwrap_or_else(|| "la del tipo".into());
            out.push(format!("{n}: collation {} / {}", w(&a.collation), w(&b.collation)));
        }
        if a.sequence.is_some() != b.sequence.is_some() {
            out.push(format!("{n}: {} secuencia propia", if a.sequence.is_some() { "el clon no tiene" } else { "el clon tiene" }));
        } else if a.sequence.is_none() && a.default != b.default {
            let w = |d: &Option<String>| d.clone().unwrap_or_else(|| "sin valor por defecto".into());
            out.push(format!("{n}: {} / {}", w(&a.default), w(&b.default)));
        }
    }
    out
}

/// Storage options, column settings and row-level security that differ.
fn settings_differences(a: &Settings, b: &Settings) -> Vec<String> {
    let mut out = Vec::new();
    if a.options != b.options {
        let w = |v: &[String]| if v.is_empty() { "ninguna".to_string() } else { v.join(", ") };
        out.push(format!("opciones de la tabla: {} / {}", w(&a.options), w(&b.options)));
    }
    if a.columns != b.columns {
        let w = |v: &[ColumnSettings]| {
            let v: Vec<String> = v
                .iter()
                .map(|c| {
                    format!(
                        "{} (STORAGE {}, STATISTICS {}, COMPRESSION {})",
                        c.name,
                        c.storage.as_deref().unwrap_or("-"),
                        c.statistics.as_deref().unwrap_or("-"),
                        c.compression.as_deref().unwrap_or("-")
                    )
                })
                .collect();
            if v.is_empty() { "ninguno".to_string() } else { v.join(", ") }
        };
        out.push(format!("ajustes de columnas: {} / {}", w(&a.columns), w(&b.columns)));
    }
    if (a.rls, a.force_rls) != (b.rls, b.force_rls) {
        let w = |s: &Settings| match (s.rls, s.force_rls) {
            (true, true) => "activada y forzada",
            (true, false) => "activada",
            (false, true) => "desactivada (FORCE)",
            (false, false) => "desactivada",
        };
        out.push(format!("seguridad por filas {} / {}", w(a), w(b)));
    }
    if a.policies != b.policies {
        let w = |v: &[Policy]| v.iter().map(|p| p.sql("…")).collect::<Vec<_>>().join("; ");
        out.push(format!("políticas: {} / {}", w(&a.policies), w(&b.policies)));
    }
    out
}

/// `REPLICA IDENTITY`, `CLUSTER ON` and extended statistics that differ.
fn extras_differences(a: &Extras, b: &Extras) -> Vec<String> {
    let mut out = Vec::new();
    let ix = |i: &Option<PgIndexRef>| i.as_ref().map(|i| i.name.clone());
    let replica = |e: &Extras| match e.replica.as_str() {
        "f" => "FULL".to_string(),
        "n" => "NOTHING".to_string(),
        "i" => format!("USING INDEX {}", ix(&e.replica_index).unwrap_or_default()),
        _ => "DEFAULT".to_string(),
    };
    if replica(a) != replica(b) {
        out.push(format!("identidad de réplica {} / {}", replica(a), replica(b)));
    }
    if ix(&a.clustered) != ix(&b.clustered) {
        let w = |i: Option<String>| i.unwrap_or_else(|| "sin CLUSTER ON".into());
        out.push(format!("CLUSTER ON {} / {}", w(ix(&a.clustered)), w(ix(&b.clustered))));
    }
    if a.statistics != b.statistics {
        let w = |v: &[PgStatistics]| {
            if v.is_empty() {
                return "ninguna".to_string();
            }
            v.iter()
                .map(|s| format!("{} {}{}", s.name, s.body, s.target.as_deref().map(|t| format!(" (STATISTICS {t})")).unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push(format!("estadísticas extendidas: {} / {}", w(&a.statistics), w(&b.statistics)));
    }
    out
}

/// The clone, read back and compared with the original.
pub(super) async fn verify(tgt: &mut dyn Session, pg: &PgTable, clone: &TableSchema, renames: &[super::Rename], with_indexes: bool) -> Result<()> {
    let got = columns(tgt, pg.version, pg.cockroach, clone.schema.as_deref(), &clone.name).await?;
    let mut diffs = differences(&pg.columns, &got);
    let p = parts(tgt, clone.schema.as_deref(), &clone.name).await?;
    if p.checks != pg.parts.checks {
        diffs.push(format!("restricciones CHECK: {} / {}", pg.parts.checks.join(", "), p.checks.join(", ")));
    }
    if p.foreign_keys != pg.parts.foreign_keys {
        diffs.push(format!("{} claves foráneas en el original, {} en el clon", pg.parts.foreign_keys, p.foreign_keys));
    }
    if p.not_valid_fks != pg.parts.not_valid_fks {
        diffs.push(format!("{} claves foráneas NOT VALID en el original, {} en el clon", pg.parts.not_valid_fks, p.not_valid_fks));
    }
    if p.foreign_keys == pg.parts.foreign_keys && p.fk_defs != pg.parts.fk_defs {
        diffs.push(format!("claves foráneas: {} / {}", pg.parts.fk_defs.join(", "), p.fk_defs.join(", ")));
    }
    if with_indexes && p.indexes != pg.parts.indexes {
        diffs.push(format!("{} índices en el original, {} en el clon", pg.parts.indexes, p.indexes));
    }
    // Without indexes: the primary key's, and nothing else.
    if !with_indexes && p.indexes != pg.parts.primary {
        diffs.push(format!("sin índices el clon debía tener {} (el de la clave primaria) y tiene {}", pg.parts.primary, p.indexes));
    }
    if p.persistence != pg.parts.persistence {
        let w = |x: &str| if x == "u" { "UNLOGGED" } else { "con registro (logged)" };
        diffs.push(format!("tabla {} / {}", w(&pg.parts.persistence), w(&p.persistence)));
    }
    let (schema, table) = (clone.schema.as_deref(), clone.name.as_str());
    let st = settings(tgt, pg.version, pg.cockroach, schema, table).await?;
    let original = Settings { policies: clone_policies(pg, table), ..pg.settings.clone() };
    diffs.extend(settings_differences(&original, &st));
    let want = expected_extras(tgt, pg, clone, renames).await?;
    let got = extras(tgt, pg.version, pg.cockroach, schema, table, false).await?;
    diffs.extend(extras_differences(&want, &got));
    if let Some(c) = &pg.crdb {
        let got = crdb(tgt, schema, table).await?;
        if got.families != c.families {
            diffs.push(format!("familias de columnas: {} / {}", c.families.join(", "), got.families.join(", ")));
        }
        let (zones, _) = clone_zones(c, clone, renames, with_indexes);
        if got.zones != zones {
            let w = |v: &[(Option<String>, String)]| v.iter().map(|(i, b)| format!("{}: {b}", i.as_deref().unwrap_or("tabla"))).collect::<Vec<_>>().join("; ");
            diffs.push(format!("configuración de zona: {} / {}", w(&c.zones), w(&got.zones)));
        }
        let pk = |x: &Option<(String, String)>| x.as_ref().map(|(_, c)| c.clone()).unwrap_or_else(|| "sin hash".into());
        if pk(&got.hash_primary) != pk(&c.hash_primary) {
            diffs.push(format!("clave primaria {} / {}", pk(&c.hash_primary), pk(&got.hash_primary)));
        }
        let to = |from: &str| renames.iter().find(|r| r.from == from).map(|r| r.to.clone()).unwrap_or_else(|| from.to_string());
        let mut want: Vec<(String, bool, String)> =
            if with_indexes { c.hash_indexes.iter().map(|(n, u, d)| (to(n), *u, d.clone())).collect() } else { Vec::new() };
        want.sort();
        if got.hash_indexes != want {
            let w = |v: &[(String, bool, String)]| v.iter().map(|(n, _, d)| format!("{n} {d}")).collect::<Vec<_>>().join(", ");
            diffs.push(format!("índices con hash: {} / {}", w(&want), w(&got.hash_indexes)));
        }
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(Error::State(format!("el clon no quedó igual al original ({}); no se clona", diffs.join("; "))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;

    fn col(name: &str, t: &str) -> PgColumn {
        PgColumn { name: name.into(), type_name: t.into(), ..Default::default() }
    }

    fn table(cols: &[&str]) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("public".into()),
            name: "t2".into(),
            // As `database_schema` reports them.
            columns: cols.iter().map(|c| ColumnDef { name: (*c).into(), data_type: "integer".into(), ..Default::default() }).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn keeps_what_database_schema_flattens() {
        let mut t = table(&["id", "ser", "shared", "s", "g", "v", "k"]);
        t.columns[0].auto_increment = true;
        t.columns[1].auto_increment = true;
        t.columns[2].auto_increment = true;
        // CockroachDB: a computed column comes as a plain one with a default.
        t.columns[6].default_value = Some("1".into());
        let pg = PgTable {
            version: 160000,
            cockroach: false,
            columns: vec![
                PgColumn { identity: "a".into(), sequence: Some("public.t_id_seq".into()), ..col("id", "integer") },
                PgColumn { sequence: Some("public.t_ser_seq".into()), default: Some("nextval('t_ser_seq'::regclass)".into()), ..col("ser", "bigint") },
                PgColumn { default: Some("nextval('shared'::regclass)".into()), ..col("shared", "integer") },
                PgColumn { collation: Some("\"C\"".into()), ..col("s", "text") },
                PgColumn { generated: "s".into(), default: Some("id * 2".into()), ..col("g", "integer") },
                PgColumn { generated: "v".into(), default: Some("id + 1".into()), ..col("v", "integer") },
                PgColumn { generated: "s".into(), default: Some("1".into()), ..col("k", "bigint") },
            ],
            ..Default::default()
        };
        let mut notes = Vec::new();
        let prelude = adjust(&mut t, &pg, &mut notes).unwrap();
        assert!(prelude.is_empty());
        let c = &t.columns;
        assert!(c[0].auto_increment && c[0].default_value.is_none());
        assert_eq!(c[1].data_type, "bigserial");
        assert!(!c[1].auto_increment && c[1].default_value.is_none());
        assert_eq!(c[2].default_value.as_deref(), Some("nextval('shared'::regclass)"));
        assert!(!c[2].auto_increment);
        assert!(notes.iter().any(|n| n.contains("shared")), "{notes:?}");
        assert_eq!(c[3].data_type, "text COLLATE \"C\"");
        assert_eq!(c[4].data_type, "integer GENERATED ALWAYS AS (id * 2) STORED");
        assert_eq!(c[5].data_type, "integer GENERATED ALWAYS AS (id + 1) VIRTUAL");
        assert_eq!(c[6].data_type, "bigint GENERATED ALWAYS AS (1) STORED");
        assert!(c[6].default_value.is_none());
        assert!(super::super::generated(&c[6], "postgres"));
        // CockroachDB needs its serials as real sequences.
        let mut t = table(&["ser"]);
        let crdb = PgTable { cockroach: true, ..pg.clone() };
        assert!(adjust(&mut t, &crdb, &mut Vec::new()).unwrap().contains("sql_sequence"));
        // A sequence of its own on a type serial can't write: refused.
        let mut t = table(&["n"]);
        let odd = PgTable { columns: vec![PgColumn { sequence: Some("s".into()), ..col("n", "numeric(10,0)") }], ..Default::default() };
        assert!(adjust(&mut t, &odd, &mut Vec::new()).is_err());
    }

    #[test]
    fn compares_what_the_clone_must_keep() {
        let a = vec![
            PgColumn { identity: "a".into(), sequence: Some("public.t_id_seq".into()), ..col("id", "integer") },
            PgColumn { collation: Some("\"C\"".into()), ..col("s", "text") },
            PgColumn { sequence: Some("public.t_n_seq".into()), default: Some("nextval('t_n_seq'::regclass)".into()), ..col("n", "integer") },
        ];
        let mut b = a.clone();
        b[2].sequence = Some("public.t2_n_seq".into());
        b[2].default = Some("nextval('t2_n_seq'::regclass)".into());
        assert!(differences(&a, &b).is_empty(), "{:?}", differences(&a, &b));
        b[0].identity = "d".into();
        b[1].collation = None;
        let d = differences(&a, &b);
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(d[0].contains("ALWAYS") && d[1].contains("collation"), "{d:?}");
    }

    #[test]
    fn not_valid_and_unlogged_are_kept() {
        use dbine_driver::{CheckDef, ConnectionConfig, DriverInfo, Family, Language};
        struct D(DriverInfo);
        #[dbine_driver::async_trait]
        impl Driver for D {
            fn info(&self) -> &DriverInfo {
                &self.0
            }
            async fn connect(&self, _: &ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
                Err(Error::Unsupported("test".into()))
            }
            fn table_ddl(&self, t: &TableSchema, _: DdlParts) -> Result<String> {
                let fk = &t.foreign_keys[0];
                Ok(format!("ALTER TABLE \"{}\" ADD CONSTRAINT \"{}\" FOREIGN KEY (p) REFERENCES \"{}\" (x);", t.name, fk.name.as_deref().unwrap_or(""), fk.ref_table))
            }
        }
        let d = D(DriverInfo {
            id: "postgres",
            name: "PostgreSQL",
            family: Family::Relational,
            language: Language::Sql,
            dialect: "postgres",
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: true,
            object_kinds: vec![],
        });
        assert!(deferred_check("CHECK ((x > 0)) NOT VALID"));
        assert!(deferred_check("CHECK ((x > 0)) NO INHERIT"));
        assert!(!deferred_check("CHECK ((x > 0))"));
        let mut t = table(&["x", "p"]);
        t.name = "r2".into();
        t.checks = vec![
            CheckDef { name: Some("r2_ck_x".into()), expression: "(x > 0)".into() },
            CheckDef { name: Some("r2_ck_ok".into()), expression: "(x < 9)".into() },
        ];
        t.foreign_keys = vec![
            ForeignKeyDef { name: Some("r2_fk_p".into()), columns: vec!["p".into()], ref_table: "r2".into(), ref_columns: vec!["x".into()], ..Default::default() },
            ForeignKeyDef { name: Some("r2_fk_ok".into()), columns: vec!["p".into()], ref_table: "o".into(), ref_columns: vec!["x".into()], ..Default::default() },
        ];
        let rn = |a: &str, b: &str| super::super::Rename { from: a.into(), to: b.into(), shortened: false };
        let mut plan = super::super::ClonePlan { table: t, renames: vec![rn("ck_x", "r2_ck_x"), rn("fk_p", "r2_fk_p")], notes: Vec::new() };
        let pg = PgTable {
            deferred_checks: vec![("ck_x".into(), "CHECK ((x > 0)) NOT VALID".into())],
            not_valid_fks: vec!["fk_p".into()],
            parts: Parts { persistence: "u".into(), ..Default::default() },
            ..Default::default()
        };
        let d = defer(&d, &mut plan, &pg).unwrap();
        assert_eq!(plan.table.checks.len(), 1);
        assert_eq!(d.checks, vec!["ALTER TABLE \"public\".\"r2\" ADD CONSTRAINT \"r2_ck_x\" CHECK ((x > 0)) NOT VALID;".to_string()]);
        assert_eq!(plan.table.foreign_keys.len(), 1);
        assert_eq!(d.foreign_keys.len(), 1);
        assert!(d.foreign_keys[0].contains("\"r2_fk_p\"") && d.foreign_keys[0].ends_with(") NOT VALID;"), "{:?}", d.foreign_keys);
        let mut create = "CREATE TABLE \"public\".\"r2\" (\n    x integer\n);".to_string();
        let mut fks = Some("ALTER TABLE x ADD CONSTRAINT \"r2_fk_ok\" FOREIGN KEY (p) REFERENCES o (x);".to_string());
        d.apply(&mut create, &mut fks, &mut None, true).unwrap();
        assert!(create.starts_with("CREATE UNLOGGED TABLE "), "{create}");
        assert_eq!(fks.unwrap().lines().count(), 2);
    }

    #[test]
    fn comments_on_renamed_indexes_and_constraints() {
        let ix = PgComment { index: true, name: "adv3_k_ix_inc".into(), text: "it's".into() };
        let ck = PgComment { index: false, name: "adv3_k_ck_multi".into(), text: "ck com".into() };
        assert_eq!(comment_sql(false, Some("public"), "adv3_k", &ix), "COMMENT ON INDEX \"public\".\"adv3_k_ix_inc\" IS 'it''s'");
        assert_eq!(comment_sql(true, Some("public"), "adv3_k", &ix), "COMMENT ON INDEX \"public\".\"adv3_k\"@\"adv3_k_ix_inc\" IS 'it''s'");
        assert_eq!(comment_sql(false, None, "adv3_k", &ck), "COMMENT ON CONSTRAINT \"adv3_k_ck_multi\" ON \"adv3_k\" IS 'ck com'");
    }

    #[test]
    fn cockroach_families_zones_and_hash_sharding() {
        let create = "CREATE TABLE public.cadv (\n\tid INT8 NOT NULL,\n\tn INT8 NULL,\n\tcrdb_internal_id_shard_4 INT8 NOT VISIBLE NOT NULL AS (mod(fnv32(md5(crdb_internal.datums_to_bytes(id))), 4:::INT8)) VIRTUAL,\n\tCONSTRAINT cadv_pkey PRIMARY KEY (id ASC) USING HASH WITH (bucket_count=4),\n\tINDEX cadv_hs (n ASC) USING HASH WITH (bucket_count=16),\n\tFAMILY f1 (id),\n\tFAMILY \"f 2\" (n)\n) WITH (schema_locked = true);\nALTER TABLE clonev.public.cadv CONFIGURE ZONE USING\n\tgc.ttlseconds = 600;\nALTER INDEX clonev.public.cadv@\"ix x\" CONFIGURE ZONE USING\n\tgc.ttlseconds = 700;\nALTER TABLE public.cadv ENABLE ROW LEVEL SECURITY;";
        let (Crdb { families, zones, .. }, primary) = parse_crdb(create).unwrap();
        assert_eq!(families, vec!["FAMILY f1 (id)".to_string(), "FAMILY \"f 2\" (n)".to_string()]);
        assert_eq!(primary.as_deref(), Some("PRIMARY KEY (id ASC) USING HASH WITH (bucket_count=4)"));
        assert_eq!(
            zones,
            vec![(None, "USING\n\tgc.ttlseconds = 600".to_string()), (Some("ix x".to_string()), "USING\n\tgc.ttlseconds = 700".to_string())]
        );
        assert!(parse_crdb("CREATE TABLE t (\n\tid INT8\n) PARTITION BY LIST (id) (\n\tPARTITION a VALUES IN (1)\n);").is_err());

        // Families and the hash-sharded key go in the CREATE; hash-sharded
        // indexes as the original's, and not the plan's (on the shard column).
        let mut t = table(&["id", "n"]);
        t.name = "cadv_c".into();
        t.indexes = vec![dbine_driver::IndexDef { name: "cadv_c_hs".into(), columns: vec!["crdb_internal_n_shard_16".into(), "n".into()], ..Default::default() }];
        let rn = |a: &str, b: &str| super::super::Rename { from: a.into(), to: b.into(), shortened: false };
        let mut plan = super::super::ClonePlan { table: t, renames: vec![rn("cadv_hs", "cadv_c_hs")], notes: Vec::new() };
        let pg = PgTable {
            cockroach: true,
            crdb: Some(Crdb {
                families: families.clone(),
                zones: zones.clone(),
                hash_primary: Some(("cadv_pkey".into(), primary.unwrap())),
                hash_indexes: vec![("cadv_hs".into(), false, "USING btree (n ASC) USING HASH WITH (bucket_count=16)".into())],
            }),
            ..Default::default()
        };
        struct D;
        #[dbine_driver::async_trait]
        impl Driver for D {
            fn info(&self) -> &dbine_driver::DriverInfo {
                unreachable!()
            }
            async fn connect(&self, _: &dbine_driver::ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
                unreachable!()
            }
        }
        let d = defer(&D, &mut plan, &pg).unwrap();
        assert!(plan.table.indexes.is_empty());
        let mut create = "CREATE TABLE \"public\".\"cadv_c\" (\n    \"id\" bigint NOT NULL,\n    \"n\" bigint NULL\n);\nCOMMENT ON TABLE x IS 'a (b';".to_string();
        let mut ix = Some("CREATE INDEX \"cadv_c_plain\" ON \"public\".\"cadv_c\" (\"n\");".to_string());
        d.apply(&mut create, &mut None, &mut ix, true).unwrap();
        assert_eq!(
            create,
            "CREATE TABLE \"public\".\"cadv_c\" (\n    \"id\" bigint NOT NULL,\n    \"n\" bigint NULL,\n    CONSTRAINT \"cadv_c_pkey\" PRIMARY KEY (id ASC) USING HASH WITH (bucket_count=4),\n    FAMILY f1 (id),\n    FAMILY \"f 2\" (n)\n);\nCOMMENT ON TABLE x IS 'a (b';"
        );
        assert!(ix.as_deref().unwrap().ends_with("\nCREATE INDEX \"cadv_c_hs\" ON \"public\".\"cadv_c\" USING btree (n ASC) USING HASH WITH (bucket_count=16);"), "{ix:?}");
        // Without indexes: none of them, nor their zone configurations.
        let mut ix = None;
        d.apply(&mut "CREATE TABLE t (a int)".to_string(), &mut None, &mut ix, false).unwrap();
        assert!(ix.is_none());
        let c = pg.crdb.as_ref().unwrap();
        let (z, skipped) = clone_zones(c, &plan.table, &[rn("ix x", "cadv_c_ix x")], false);
        assert_eq!(z, vec![(None, "USING\n\tgc.ttlseconds = 600".to_string())]);
        assert_eq!(skipped, vec!["ix x".to_string()]);
        let (z, _) = clone_zones(c, &plan.table, &[rn("ix x", "cadv_c_ix x")], true);
        assert_eq!(z[1].0.as_deref(), Some("cadv_c_ix x"));
        // The hidden shard column the clone has only with its index.
        let shard = PgColumn { generated: "v".into(), ..col("crdb_internal_n_shard_16", "bigint") };
        assert!(differences(&[col("id", "bigint"), shard], &[col("id", "bigint")]).is_empty());
        assert!(!differences(&[col("id", "bigint"), col("n", "bigint")], &[col("id", "bigint")]).is_empty());
    }

    #[test]
    fn storage_settings_and_row_level_security() {
        let c = ColumnSettings { name: "b x".into(), storage: Some("e".into()), statistics: Some("500".into()), compression: Some("p".into()) };
        assert_eq!(
            c.sql("\"s\".\"t\"").unwrap(),
            vec![
                "ALTER TABLE \"s\".\"t\" ALTER COLUMN \"b x\" SET STORAGE EXTERNAL".to_string(),
                "ALTER TABLE \"s\".\"t\" ALTER COLUMN \"b x\" SET STATISTICS 500".to_string(),
                "ALTER TABLE \"s\".\"t\" ALTER COLUMN \"b x\" SET COMPRESSION pglz".to_string(),
            ]
        );
        assert!(ColumnSettings { storage: Some("?".into()), ..c.clone() }.sql("t").is_err());
        let p = Policy {
            name: "p 2".into(),
            permissive: "RESTRICTIVE".into(),
            cmd: "UPDATE".into(),
            roles: "PUBLIC, \"Bob\"".into(),
            using: Some("(owner = CURRENT_USER)".into()),
            check: Some("(id < 100)".into()),
        };
        assert_eq!(
            p.sql("\"s\".\"t_c\""),
            "CREATE POLICY \"p 2\" ON \"s\".\"t_c\" AS RESTRICTIVE FOR UPDATE TO PUBLIC, \"Bob\" USING ((owner = CURRENT_USER)) WITH CHECK ((id < 100))"
        );
        let a = Settings {
            options: vec!["autovacuum_enabled=false".into(), "fillfactor=60".into(), "toast.autovacuum_enabled=false".into()],
            columns: vec![c],
            rls: true,
            force_rls: true,
            policies: vec![p],
        };
        assert!(settings_differences(&a, &a.clone()).is_empty());
        // What the clone lost before: all of it, silently.
        let d = settings_differences(&a, &Settings::default());
        assert_eq!(d.len(), 4, "{d:?}");
        assert!(d[0].contains("fillfactor=60") && d[1].contains("STORAGE e") && d[2].contains("seguridad por filas") && d[3].contains("políticas"), "{d:?}");
    }

    #[test]
    fn cockroach_sequence_names() {
        let pg = PgTable {
            cockroach: true,
            columns: vec![PgColumn { sequence: Some("x".into()), ..col("id", "bigint") }],
            taken: vec!["t2_id_seq".into()],
            ..Default::default()
        };
        assert_eq!(sequence_collisions(&pg, "t2"), vec!["t2_id_seq".to_string()]);
        assert!(sequence_collisions(&PgTable { cockroach: false, ..pg }, "t2").is_empty());
    }

    #[test]
    fn foreign_keys_keep_match_and_deferrable() {
        let def = "FOREIGN KEY (a, b) REFERENCES padre2(a, \"B x\") MATCH FULL ON UPDATE CASCADE ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED";
        let f = PgForeignKey::split("fk_h".into(), def, "a, \"B x\"", Some("\"public\".\"padre2\"".into())).unwrap();
        assert_eq!(f.head, "FOREIGN KEY (a, b) REFERENCES ");
        assert_eq!(f.rest, "(a, \"B x\") MATCH FULL ON UPDATE CASCADE ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED");
        assert!(PgForeignKey::split("x".into(), def, "zz", None).is_none());
        // A reference to the table itself: the clone's, and the same shape.
        let own = PgForeignKey::split("fk_p".into(), "FOREIGN KEY (parent_code) REFERENCES cat(code) NOT VALID", "code", None).unwrap();
        let clone_own = PgForeignKey::split("cat_c_fk_p".into(), "FOREIGN KEY (parent_code) REFERENCES cat_c(code) NOT VALID", "code", None).unwrap();
        assert_eq!(own.shape(), clone_own.shape());
        let lost = PgForeignKey::split("x".into(), "FOREIGN KEY (a, b) REFERENCES padre2(a, \"B x\") ON UPDATE CASCADE ON DELETE SET NULL", "a, \"B x\"", f.target.clone()).unwrap();
        assert_ne!(f.shape(), lost.shape());

        let mut t = table(&["a", "b", "parent_code"]);
        t.name = "hijo2_c".into();
        let fk = |n: &str, to: &str| ForeignKeyDef { name: Some(n.into()), columns: vec!["a".into()], ref_table: to.into(), ref_columns: vec!["a".into()], ..Default::default() };
        t.foreign_keys = vec![fk("hijo2_c_fk_h", "padre2"), fk("hijo2_c_fk_p", "hijo2_c")];
        let rn = |a: &str, b: &str| super::super::Rename { from: a.into(), to: b.into(), shortened: false };
        let mut plan = super::super::ClonePlan { table: t, renames: vec![rn("fk_h", "hijo2_c_fk_h"), rn("fk_p", "hijo2_c_fk_p")], notes: Vec::new() };
        let pg = PgTable { foreign_keys: vec![PgForeignKey { name: "fk_h".into(), ..f }, PgForeignKey { name: "fk_p".into(), ..own }], ..Default::default() };
        struct D;
        #[dbine_driver::async_trait]
        impl Driver for D {
            fn info(&self) -> &dbine_driver::DriverInfo {
                unreachable!()
            }
            async fn connect(&self, _: &dbine_driver::ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
                unreachable!()
            }
        }
        let d = defer(&D, &mut plan, &pg).unwrap();
        assert!(plan.table.foreign_keys.is_empty());
        assert_eq!(
            d.foreign_keys,
            vec![
                "ALTER TABLE \"public\".\"hijo2_c\" ADD CONSTRAINT \"hijo2_c_fk_h\" FOREIGN KEY (a, b) REFERENCES \"public\".\"padre2\"(a, \"B x\") MATCH FULL ON UPDATE CASCADE ON DELETE SET NULL DEFERRABLE INITIALLY DEFERRED;".to_string(),
                "ALTER TABLE \"public\".\"hijo2_c\" ADD CONSTRAINT \"hijo2_c_fk_p\" FOREIGN KEY (parent_code) REFERENCES \"public\".\"hijo2_c\"(code) NOT VALID;".to_string(),
            ]
        );
    }

    #[test]
    fn policies_that_read_the_table_itself_read_the_clone() {
        let cols = vec![col("id", "integer"), col("p", "integer")];
        // As pg_policies writes them (PostgreSQL 16), in and out of the
        // search_path.
        let q = "(id IN ( SELECT b1_1.id\n   FROM cv3.b1 b1_1\n  WHERE (b1_1.p > 0)))";
        assert_eq!(retarget(q, Some("cv3"), "b1", "b1_c", &cols).unwrap(), "(id IN ( SELECT b1_1.id\n   FROM cv3.b1_c b1_1\n  WHERE (b1_1.p > 0)))");
        let q = "(EXISTS ( SELECT 1\n   FROM b1 x\n  WHERE (x.id = b1.a)))";
        assert_eq!(retarget(q, Some("cv3"), "b1", "b1_c", &cols).unwrap(), "(EXISTS ( SELECT 1\n   FROM b1_c x\n  WHERE (x.id = b1_c.a)))");
        // Another schema's b1, a column b1 of another table, a string: kept.
        let q = "(EXISTS ( SELECT 1 FROM otro.b1 x WHERE ((x.b1 = 'b1') AND (x.id = cv3.b1.id))))";
        assert_eq!(retarget(q, Some("cv3"), "b1", "B c", &cols).unwrap(), "(EXISTS ( SELECT 1 FROM otro.b1 x WHERE ((x.b1 = 'b1') AND (x.id = cv3.\"B c\".id))))");
        // Quoted names.
        let q = "(id IN ( SELECT \"Mi T_1\".id FROM \"Mi T\" \"Mi T_1\"))";
        assert_eq!(retarget(q, None, "Mi T", "Mi T_c", &cols).unwrap(), "(id IN ( SELECT \"Mi T_1\".id FROM \"Mi T_c\" \"Mi T_1\"))");
        // No reference: as it is.
        assert_eq!(retarget("(p > 0)", Some("cv3"), "b1", "b1_c", &cols).unwrap(), "(p > 0)");
        // A bare b1 that isn't a column nor a relation: can't be told apart.
        assert!(retarget("(b1(id) > 0)", Some("cv3"), "b1", "b1_c", &cols).is_none());
        // ... unless the table has a column b1.
        let with_b1 = vec![col("b1", "integer")];
        assert_eq!(retarget("(b1 > 0)", Some("cv3"), "b1", "b1_c", &with_b1).unwrap(), "(b1 > 0)");
        // Verified against the clone's policies: the original's, retargeted.
        let pg = PgTable {
            schema: Some("cv3".into()),
            name: "b1".into(),
            columns: cols,
            settings: Settings {
                policies: vec![Policy { name: "p_self".into(), using: Some("(id IN ( SELECT b1_1.id FROM b1 b1_1))".into()), ..Default::default() }],
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(clone_policies(&pg, "b1_c")[0].using.as_deref(), Some("(id IN ( SELECT b1_1.id FROM b1_c b1_1))"));
    }

    #[test]
    fn replica_identity_cluster_and_statistics() {
        let st = |n: &str| PgStatistics { schema: "cv3".into(), name: n.into(), body: "(ndistinct, dependencies) ON a, bb".into(), ..Default::default() };
        let pg = PgTable {
            name: "b1".into(),
            extras: Extras {
                statistics: vec![st("b1_st"), st("otra")],
                statistics_taken: vec![("cv3".into(), "b1_st".into()), ("cv3".into(), "otra".into()), ("cv3".into(), "b1_c_otra".into())],
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            statistics_names(&pg, "b1_c"),
            vec![("b1_st".to_string(), "b1_c_st".to_string()), ("otra".to_string(), "b1_c_otra_2".to_string())]
        );
        let ix = |n: &str| Some(PgIndexRef { name: n.into(), primary: false });
        let a = Extras { replica: "f".into(), clustered: ix("b1_c_ix"), statistics: vec![st("b1_c_st")], ..Default::default() };
        assert!(extras_differences(&a, &a.clone()).is_empty());
        // What the clone lost before, silently.
        let d = extras_differences(&a, &Extras { replica: "d".into(), ..Default::default() });
        assert_eq!(d.len(), 3, "{d:?}");
        assert!(d[0].contains("FULL") && d[1].contains("CLUSTER ON b1_c_ix") && d[2].contains("b1_c_st"), "{d:?}");
        let u = Extras { replica: "i".into(), replica_index: ix("b1_c_uq"), ..Default::default() };
        assert!(extras_differences(&u, &Extras { replica: "i".into(), replica_index: ix("otro"), ..Default::default() })[0].contains("USING INDEX b1_c_uq"));
    }

    #[test]
    fn cockroach_hidden_columns_on_update_and_ttl() {
        let mut t = table(&["id", "v"]);
        let pg = PgTable {
            cockroach: true,
            columns: vec![
                col("id", "bigint"),
                PgColumn { hidden: true, comment: Some("oculta".into()), ..col("secret", "text") },
                PgColumn { on_update: Some("now()".into()), default: Some("now()".into()), ..col("v", "timestamp with time zone") },
                PgColumn { hidden: true, default: Some("unique_rowid()".into()), not_null: true, ..col("rowid", "bigint") },
                PgColumn { hidden: true, generated: "v".into(), ..col("crdb_internal_n_shard_16", "bigint") },
                PgColumn {
                    hidden: true,
                    not_null: true,
                    default: Some("current_timestamp():::TIMESTAMPTZ + '30 days':::INTERVAL".into()),
                    on_update: Some("current_timestamp():::TIMESTAMPTZ + '30 days':::INTERVAL".into()),
                    ..col("crdb_internal_expiration", "timestamp with time zone")
                },
            ],
            settings: Settings {
                options: vec!["schema_locked=true".into(), "ttl='on'".into(), "ttl_expire_after='30 days':::INTERVAL".into(), "ttl_job_cron='@daily'".into()],
                ..Default::default()
            },
            crdb: Some(Crdb::default()),
            ..Default::default()
        };
        adjust(&mut t, &pg, &mut Vec::new()).unwrap();
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "secret", "v", "crdb_internal_expiration"]);
        assert_eq!(t.columns[1].data_type, "text NOT VISIBLE");
        assert!(t.columns[1].nullable && t.columns[1].comment.as_deref() == Some("oculta"));
        assert_eq!(t.columns[2].data_type, "timestamp with time zone ON UPDATE now()");
        let e = &t.columns[3];
        assert_eq!(e.data_type, "timestamp with time zone NOT VISIBLE ON UPDATE current_timestamp():::TIMESTAMPTZ + '30 days':::INTERVAL");
        assert!(!e.nullable && e.default_value.is_some());
        // The TTL in the CREATE (it can't be set later on a table that
        // already has its expiration column); schema_locked left out.
        struct D;
        #[dbine_driver::async_trait]
        impl Driver for D {
            fn info(&self) -> &dbine_driver::DriverInfo {
                unreachable!()
            }
            async fn connect(&self, _: &dbine_driver::ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
                unreachable!()
            }
        }
        let mut plan = super::super::ClonePlan { table: t, renames: Vec::new(), notes: Vec::new() };
        let d = defer(&D, &mut plan, &pg).unwrap();
        let mut create = "CREATE TABLE \"public\".\"tt_c\" (\n    \"id\" bigint NOT NULL\n);\nCOMMENT ON COLUMN x IS 'a)';".to_string();
        d.apply(&mut create, &mut None, &mut None, true).unwrap();
        assert_eq!(
            create,
            "CREATE TABLE \"public\".\"tt_c\" (\n    \"id\" bigint NOT NULL\n) WITH (ttl = 'on', ttl_expire_after = '30 days':::INTERVAL, ttl_job_cron = '@daily');\nCOMMENT ON COLUMN x IS 'a)';"
        );
        // Compared: hidden and ON UPDATE.
        let a = vec![PgColumn { hidden: true, on_update: Some("now()".into()), ..col("s", "text") }];
        let d = differences(&a, &[col("s", "text")]);
        assert_eq!(d.len(), 2, "{d:?}");
    }
}
