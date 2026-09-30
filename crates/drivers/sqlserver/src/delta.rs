//! Sync by rows (see `dbine_driver::transfer::DeltaSpec`): SQL Server and
//! Azure SQL.
//!
//! - **Buckets.** Range buckets on an integer first key column
//!   (`CASE WHEN c<lo THEN -1 WHEN c>hi THEN n ELSE (c-lo)/width END`: integer
//!   equality is exact, and rows only on the target fall in the edges), or
//!   `((CHECKSUM(key) % n) + n) % n` with a prime `n`. Never `ABS`: it
//!   overflows on `INT_MIN`. CHECKSUM follows the column's collation, so keys
//!   the collation calls equal share a bucket.
//! - **Row hash.** Per column `NULL → 0x00`, otherwise `0x01 + DATALENGTH (4
//!   bytes) + bytes`, so NULL vs empty and `'ab','c'` vs `'a','bc'` differ.
//!   `HASHBYTES('MD5')`'s first 8 bytes, summed as `decimal(38,0)` (a SUM:
//!   CHECKSUM_AGG's XOR cancels pairs). Computed and `rowversion` columns
//!   are left out; `text`/`ntext`/`xml` are cast first.
//! - **Depth** only decides which buckets look changed; the merge always
//!   compares every byte.
//! - **Apply.** Staging table cloned from the destination (its collations,
//!   no identity), bulk loaded with the source rows, then one transaction on
//!   a dedicated connection: triggers off, FKs on the table and pointing at
//!   it NOCHECK, `IDENTITY_INSERT`, a `MERGE` whose target is a CTE over only
//!   the changed buckets' rows, everything back on before the commit. The
//!   staging table is dropped on every path.
//!
//! Key collations can only be compared across both sides through the
//! summaries, and only hash buckets depend on them (CHECKSUM follows the
//! collation; range buckets are integers). With hash buckets each side adds
//! a fingerprint of its key columns' collations to every bucket's sum: equal
//! collations cancel out; different ones make every non-empty bucket of
//! either side a changed one, so the apply covers every row on both sides
//! (a key may land in different buckets on each side), filtered or not. No
//! sentinel bucket: the engine's filtered / whole-table threshold counts
//! only real buckets. Any apply refuses source keys the target's collation
//! calls equal.
//!
//! Key columns must be NOT NULL on both sides (refused with the reason): a
//! NULL key falls in no bucket and never matches in the MERGE; a NULL bucket
//! is an error, never folded into another one.
//!
//! Foreign keys. Sync by rows doesn't mirror the source's trust (that's
//! schema: clone and compare handle it). It guarantees that a foreign key the
//! target trusted before the apply is trusted after it, or says why not:
//! - Incoming ones with an `ON DELETE` action on exactly the key columns stay
//!   enabled during the merge, so a delete cascades on the target as on the
//!   source (the merge never changes a key value, so they can't fail).
//! - The rest (the table's own, other incoming ones) are NOCHECK during the
//!   merge, so tables sync in any order; those trusted before are checked
//!   again after the commit (`WITH CHECK CHECK CONSTRAINT`, by name).
//! - One that fails (typically: its other table hasn't synced yet) stays
//!   untrusted, gets the extended property [`UNTRUSTED_MARK`] (valued with
//!   the schema-qualified table whose sync left it) and a
//!   [`DeltaResult::notes`] entry naming it, both tables and what to do.
//! - Every apply of either table checks the marked ones again and clears the
//!   mark on success, with a note. After its merge, not before it: the rows
//!   a child-first sync waits for are only there once the parent's merge
//!   committed, and one validation scan per key is enough.
//! - Parent and child syncing at once (the engine runs tables in parallel):
//!   a key is marked *before* its check, not after it fails, and each apply
//!   reads the marks again after its own checks and checks the ones set
//!   since its snapshot. A child whose check misses the parent's rows has
//!   marked the key before that check began; the parent's merge then
//!   committed after it, so the parent's second read sees the mark. Else the
//!   parent read the marks before the child set it, so the parent committed
//!   before the child's check began and the check sees its rows. A check that
//!   passes clears the mark; one that fails and finds the key trusted anyway
//!   (the other sync checked it meanwhile) says nothing. When both pass, only
//!   the one that actually removed the mark says so. Adding or removing the
//!   mark races the other sync harmlessly ("already there" / "not there" are
//!   what was wanted). A check that loses a deadlock is run again, and one
//!   that keeps losing says so, not that the data fails the key.
//! - The staging table's `SELECT … INTO` can fail with error 539 while the
//!   other table's merge switches a foreign key on this one, leaving the
//!   staging table behind: dropped and run again, and dropped on every
//!   failure.
//! - Untrusted before and not marked: left as it is. Keys only the target
//!   has follow the same rules. A mark doesn't force an apply (no sentinel):
//!   while neither table changes, a sync has nothing that could make it valid.
//!
//! Identity: only the range column carries the source's last identity
//! value (the source's `key_range` reads its `last_value` and stretches `hi`
//! to it), so the target is reseeded past every value the source used, not
//! just to its own largest key. A target behind it adds a fingerprint to its
//! highest bucket's sum, so it's applied (and reseeded) even when its rows
//! are equal, without adding a bucket. See [`identity_behind_hi`] for why a
//! side whose largest key is `hi` never counts. Any other synced identity
//! column (hash buckets, not the first key column, negative increment) can't
//! follow the source: refused.

use crate::{connect_once, connect_error, err, format_type, text, SqlServerSession};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::transfer::{BatchSource, BucketSum, Buckets, DeltaDepth, DeltaResult, DeltaSpec, LoadSpec, Progress, TransferColumn};
use dbine_driver::{Error, ObjectRef, Result};

/// Forces hash buckets (`key_range` declines).
pub const HASH_ENV: &str = "DBINE_SQLSERVER_HASH_BUCKETS";
/// Staging load commit window.
const STAGE_BATCH_ROWS: u64 = 100_000;
/// Extended property on a foreign key a sync left untrusted; its value is
/// the `[schema].[table]` whose sync left it.
pub const UNTRUSTED_MARK: &str = "dbine_delta_untrusted";
/// Added to the highest bucket's sum of a target whose identity is behind
/// the source's last value (`hi` of the range buckets), so it's applied even
/// with equal rows ([`force_apply`]).
const IDENTITY_BEHIND: u64 = 0x9e37_79b9_7f4a_7c15;

// Server error numbers.
/// `SELECT … INTO`: the source table's schema changed after the target
/// table was created (another sync's merge switching a foreign key on it);
/// the target table is left behind.
const SCHEMA_CHANGED: u32 = 539;
const DEADLOCK: u32 = 1205;
/// `sp_dropextendedproperty`: the property doesn't exist.
const PROPERTY_MISSING: u32 = 15217;
/// `sp_addextendedproperty`: the property already exists.
const PROPERTY_EXISTS: u32 = 15233;
/// Tries of the staging table's `SELECT … INTO` against [`SCHEMA_CHANGED`].
const STAGING_TRIES: u64 = 6;
/// Tries of a foreign key's check after the merge against [`DEADLOCK`].
const CHECK_TRIES: u64 = 4;

/// A column as the delta needs it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Col {
    pub name: String,
    /// System type, lowercase (`nvarchar`, `int`…).
    pub ty: String,
    pub max_len: i32,
    pub precision: i32,
    pub scale: i32,
    pub computed: bool,
    pub identity: bool,
    pub collation: Option<String>,
    pub nullable: bool,
    /// An identity column with a negative increment.
    pub ident_down: bool,
}

impl Col {
    fn rowversion(&self) -> bool {
        self.ty == "timestamp"
    }

    /// Stored off-row when large: `(max)` types, `text`, `ntext`, `image`, `xml`.
    fn large(&self) -> bool {
        self.max_len == -1 || matches!(self.ty.as_str(), "text" | "ntext" | "image" | "xml")
    }

    fn integer(&self) -> bool {
        matches!(self.ty.as_str(), "tinyint" | "smallint" | "int" | "bigint")
    }
}

const COLUMNS_SQL: &str = "SELECT c.name,
        CASE WHEN t.is_user_defined = 1 AND t.is_assembly_type = 0 THEN TYPE_NAME(c.system_type_id) ELSE t.name END,
        CAST(c.max_length AS int), CAST(c.precision AS int), CAST(c.scale AS int),
        c.is_computed, c.is_identity, c.collation_name, c.is_nullable,
        CAST(CASE WHEN c.is_identity = 1 AND IDENT_INCR(@P1) < 0 THEN 1 ELSE 0 END AS bit)
   FROM sys.columns c
   JOIN sys.types t ON t.user_type_id = c.user_type_id
  WHERE c.object_id = OBJECT_ID(@P1)
  ORDER BY c.column_id";

fn table_name(t: &ObjectRef) -> String {
    qualified_name(Quote::Bracket, t.schema(), &t.name)
}

fn q(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn aliased(alias: Option<&str>, name: &str) -> String {
    match alias {
        Some(a) => format!("{a}.{}", q(name)),
        None => q(name),
    }
}

async fn catalog(s: &mut SqlServerSession, table: &ObjectRef) -> Result<Vec<Col>> {
    let rows = s.rows(COLUMNS_SQL, &[&table_name(table)]).await?;
    if rows.is_empty() {
        return Err(Error::Query(format!("No existe la tabla {}", table_name(table))));
    }
    Ok(rows
        .iter()
        .map(|r| {
            let ty = text(r, 1).unwrap_or_default().to_ascii_lowercase();
            Col {
                name: text(r, 0).unwrap_or_default(),
                ty: if ty == "sysname" { "nvarchar".into() } else { ty },
                max_len: r.get(2).unwrap_or(0),
                precision: r.get(3).unwrap_or(0),
                scale: r.get(4).unwrap_or(0),
                computed: r.get(5).unwrap_or(false),
                identity: r.get(6).unwrap_or(false),
                collation: text(r, 7),
                nullable: r.get(8).unwrap_or(true),
                ident_down: r.get(9).unwrap_or(false),
            }
        })
        .collect())
}

fn find<'a>(cat: &'a [Col], name: &str) -> Result<&'a Col> {
    cat.iter()
        .find(|c| c.name == name)
        .or_else(|| cat.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
        .ok_or_else(|| Error::Query(format!("La tabla no tiene la columna «{name}»")))
}

/// Refuse what the delta can't handle: temporal and memory-optimized
/// tables, `sql_variant` / `float` / `real` keys.
async fn check_table(s: &mut SqlServerSession, spec: &DeltaSpec, cat: &[Col]) -> Result<()> {
    let name = table_name(&spec.table);
    let rows = s
        .rows(
            "SELECT CAST(t.temporal_type AS int), t.is_memory_optimized FROM sys.tables t WHERE t.object_id = OBJECT_ID(@P1)",
            &[&name],
        )
        .await?;
    if let Some(r) = rows.first() {
        if r.get::<i32, _>(0).unwrap_or(0) != 0 {
            return Err(Error::Unsupported(format!("{name} es una tabla temporal (con versiones del sistema): no se sincroniza por filas")));
        }
        if r.get::<bool, _>(1).unwrap_or(false) {
            return Err(Error::Unsupported(format!("{name} está optimizada para memoria: no se sincroniza por filas")));
        }
    }
    check_key(spec, cat)?;
    check_identity(spec, cat)
}

/// A synced identity column must be the range column: only `hi` carries the
/// source's last identity value to the target. Any other one would leave
/// the target free to hand out values the source already used.
pub(crate) fn check_identity(spec: &DeltaSpec, cat: &[Col]) -> Result<()> {
    let synced = |c: &Col| spec.columns.iter().any(|n| n.eq_ignore_ascii_case(&c.name));
    let Some(c) = cat.iter().find(|c| c.identity && synced(c)) else { return Ok(()) };
    let why = if c.ident_down {
        "tiene incremento negativo".to_string()
    } else {
        match &spec.buckets {
            Buckets::Range { column, .. } if column.eq_ignore_ascii_case(&c.name) => return Ok(()),
            _ if spec.key.first().is_some_and(|k| k.eq_ignore_ascii_case(&c.name)) => "se está agrupando por hash".to_string(),
            _ => "no es la primera columna de la clave".to_string(),
        }
    };
    Err(Error::Unsupported(format!(
        "la columna identidad «{}» {why}: sincronizando por filas no se puede llevar al destino el último valor de \
         identidad que entregó el origen, y el destino podría volver a entregar valores que el origen ya usó \
         (hay que vaciar y copiar la tabla)",
        c.name
    )))
}

fn check_key(spec: &DeltaSpec, cat: &[Col]) -> Result<()> {
    if spec.key.is_empty() {
        return Err(Error::State("para sincronizar por filas hace falta una clave".into()));
    }
    for k in &spec.key {
        let c = find(cat, k)?;
        if matches!(c.ty.as_str(), "sql_variant" | "float" | "real") {
            return Err(Error::Unsupported(format!(
                "la columna «{}» de la clave es {}: no sirve para comparar filas entre bases",
                c.name, c.ty
            )));
        }
        if c.nullable {
            return Err(Error::Unsupported(format!(
                "la columna «{}» de la clave admite NULL: una fila con la clave en NULL no cae en ningún grupo ni se \
                 empareja con la del otro lado, así que no se puede sincronizar por filas (la clave tiene que ser NOT NULL \
                 en origen y destino)",
                c.name
            )));
        }
    }
    Ok(())
}

// ------------------------------------------------------------ SQL pieces

/// Integer literal, `i64::MIN` included (its digits alone overflow).
fn lit(v: i128) -> String {
    if v == i64::MIN as i128 {
        "(-9223372036854775807-1)".into()
    } else {
        v.to_string()
    }
}

/// The bucket of a row, as a `bigint` expression (`alias` qualifies the columns).
pub(crate) fn bucket_expr(spec: &DeltaSpec, alias: Option<&str>) -> String {
    match &spec.buckets {
        Buckets::Range { column, lo, hi, width, n } => {
            let c = aliased(alias, column);
            // (c - lo) fits a bigint unless the span itself doesn't.
            let inner = if (*hi as i128 - *lo as i128) <= i64::MAX as i128 {
                format!("(CAST({c} AS bigint) - CAST({} AS bigint)) / CAST({width} AS bigint)", lit(*lo as i128))
            } else {
                format!("CAST(FLOOR((CAST({c} AS decimal(20,0)) - {}) / {width}) AS bigint)", lit(*lo as i128))
            };
            format!(
                "CASE WHEN {c} < {} THEN CAST(-1 AS bigint) WHEN {c} > {} THEN CAST({n} AS bigint) ELSE {inner} END",
                lit(*lo as i128),
                lit(*hi as i128)
            )
        }
        Buckets::Hash { n } => hash_bucket(&spec.key, alias, *n),
    }
}

/// `((CHECKSUM(key) % n) + n) % n`: never ABS (INT_MIN overflows it).
fn hash_bucket(key: &[String], alias: Option<&str>, n: u64) -> String {
    let cols: Vec<String> = key.iter().map(|k| aliased(alias, k)).collect();
    format!("((CHECKSUM({}) % {n}) + {n}) % {n}", cols.join(", "))
}

/// One column's contribution to the row hash.
fn col_hash(c: &Col, alias: Option<&str>, sizes_only: bool) -> String {
    let n = aliased(alias, &c.name);
    if sizes_only && c.large() {
        return format!("CASE WHEN {n} IS NULL THEN 0x00 ELSE 0x01 + CAST(CAST(DATALENGTH({n}) AS bigint) AS binary(8)) END");
    }
    let v = match c.ty.as_str() {
        "text" => format!("CAST({n} AS varchar(max))"),
        "ntext" | "xml" => format!("CAST({n} AS nvarchar(max))"),
        _ => n.clone(),
    };
    format!("CASE WHEN {n} IS NULL THEN 0x00 ELSE 0x01 + CAST(CAST(DATALENGTH({v}) AS int) AS binary(4)) + CAST({v} AS varbinary(max)) END")
}

/// The bytes a row hashes (computed and rowversion columns left out).
pub(crate) fn row_bytes(cols: &[&Col], alias: Option<&str>, depth: DeltaDepth) -> String {
    let parts: Vec<String> =
        cols.iter().filter(|c| !c.computed && !c.rowversion()).map(|c| col_hash(c, alias, depth == DeltaDepth::Sizes)).collect();
    if parts.is_empty() {
        "0x".into()
    } else {
        parts.join(" + ")
    }
}

/// The columns a summary hashes at `depth`.
fn hashed<'a>(spec: &DeltaSpec, cat: &'a [Col]) -> Result<Vec<&'a Col>> {
    let names = if spec.depth == DeltaDepth::Keys { &spec.key } else { &spec.columns };
    names.iter().map(|n| find(cat, n)).collect()
}

pub(crate) fn summary_sql(spec: &DeltaSpec, cols: &[&Col]) -> String {
    let h = format!(
        "CAST(CAST(SUBSTRING(HASHBYTES('MD5', {}), 1, 8) AS bigint) AS decimal(38,0))",
        row_bytes(cols, Some("t"), spec.depth)
    );
    let mut sql = format!(
        "SELECT x.b, COUNT_BIG(*), CAST(SUM(x.h) AS varchar(40)) FROM {} AS t CROSS APPLY (SELECT CAST({} AS bigint) AS b, {h} AS h) AS x GROUP BY x.b",
        table_name(&spec.table),
        bucket_expr(spec, Some("t"))
    );
    if spec.max_cores > 0 {
        sql.push_str(&format!(" OPTION (MAXDOP {})", spec.max_cores));
    }
    sql
}

/// Merge sorted buckets into runs of adjacent ones.
fn runs(buckets: &[i64]) -> Vec<(i64, i64)> {
    let mut v: Vec<i64> = buckets.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for b in v {
        match out.last_mut() {
            Some((_, z)) if *z + 1 == b => *z = b,
            _ => out.push((b, b)),
        }
    }
    out
}

/// The condition that selects the rows of `buckets` (out-of-range ones
/// ignored).
/// Range buckets become key ranges, adjacent ones merged, so the server
/// seeks the key index; hash buckets an `IN` list.
pub(crate) fn filter(spec: &DeltaSpec, buckets: &[i64]) -> Result<String> {
    match &spec.buckets {
        Buckets::Range { column, lo, hi, width, n } => {
            let (lo, hi, width, n) = (*lo as i128, *hi as i128, *width as i128, *n as i64);
            if width <= 0 {
                return Err(Error::Query("ancho de grupo inválido".into()));
            }
            let c = q(column);
            let valid: Vec<i64> = buckets.iter().copied().filter(|b| (-1..=n).contains(b)).collect();
            let mut parts = Vec::new();
            for (a, z) in runs(&valid) {
                let lower = match a {
                    -1 => None,
                    a if a == n => Some(format!("{c} > {}", lit(hi))),
                    a => Some(format!("{c} >= {}", lit(lo + a as i128 * width))),
                };
                let upper = match z {
                    -1 => Some(format!("{c} < {}", lit(lo))),
                    z if z == n => None,
                    z if z == n - 1 => Some(format!("{c} <= {}", lit(hi))),
                    z => Some(format!("{c} < {}", lit(lo + (z as i128 + 1) * width))),
                };
                parts.push(match (lower, upper) {
                    (None, None) => return Ok("1 = 1".into()),
                    (Some(l), None) => l,
                    (None, Some(u)) => u,
                    (Some(l), Some(u)) => format!("({l} AND {u})"),
                });
            }
            Ok(if parts.is_empty() { "1 = 0".into() } else { parts.join(" OR ") })
        }
        Buckets::Hash { n } => {
            let mut v: Vec<i64> = buckets.iter().copied().filter(|b| *b >= 0 && (*b as u64) < *n).collect();
            v.sort_unstable();
            v.dedup();
            if v.is_empty() {
                return Ok("1 = 0".into());
            }
            let list: Vec<String> = v.iter().map(i64::to_string).collect();
            Ok(format!("{} IN ({})", hash_bucket(&spec.key, None, *n), list.join(", ")))
        }
    }
}

/// FNV-1a of the key columns' collations; `None` without collated ones.
pub(crate) fn collation_fingerprint(spec: &DeltaSpec, cat: &[Col]) -> Result<Option<u64>> {
    let mut any = false;
    let mut text = String::new();
    for k in &spec.key {
        let c = find(cat, k)?;
        any |= c.collation.is_some();
        text.push_str(c.collation.as_deref().unwrap_or("-"));
        text.push(';');
    }
    if !any {
        return Ok(None);
    }
    Ok(Some(fnv(&text.to_ascii_lowercase())))
}

/// FNV-1a.
fn fnv(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------- session side

pub(crate) async fn key_range(s: &mut SqlServerSession, table: &ObjectRef, column: &str) -> Result<Option<(i64, i64, u64)>> {
    if std::env::var(HASH_ENV).is_ok_and(|v| !v.is_empty() && v != "0") {
        return Err(Error::Unsupported("se pidió agrupar por hash".into()));
    }
    let cat = catalog(s, table).await?;
    let c = find(&cat, column)?;
    if !c.integer() {
        return Err(Error::Unsupported(format!("la columna «{}» es {}, no un entero", c.name, c.ty)));
    }
    let name = table_name(table);
    // The last identity value (ascending identities only): values the
    // source used and then deleted must not be reused on the target.
    let sql = format!(
        "SELECT CAST(MIN({c}) AS bigint), CAST(MAX({c}) AS bigint),
                (SELECT CAST(SUM(p.rows) AS bigint) FROM sys.partitions p WHERE p.object_id = OBJECT_ID(@P1) AND p.index_id IN (0, 1)),
                (SELECT CAST(ic.last_value AS bigint) FROM sys.identity_columns ic
                  WHERE ic.object_id = OBJECT_ID(@P1) AND ic.name = @P2 AND CAST(ic.increment_value AS bigint) > 0)
           FROM {name}",
        c = q(&c.name)
    );
    let rows = s.rows(&sql, &[&name, &c.name]).await?;
    let Some(r) = rows.first() else { return Ok(None) };
    let last = r.get::<i64, _>(3);
    match (r.get::<i64, _>(0), r.get::<i64, _>(1)) {
        (Some(lo), Some(hi)) => Ok(Some((lo, stretch_hi(hi, last),r.get::<i64, _>(2).unwrap_or(0).max(1) as u64))),
        // Empty, but the identity already handed out values.
        _ => Ok(last.map(|v| (v, v, 0))),
    }
}

/// `hi`, stretched to the last identity value when it's past `hi` (however
/// far: the buckets only get wider). Carried in [`Buckets::Range`], it
/// tells `apply` where to reseed.
pub(crate) fn stretch_hi(hi: i64, last: Option<i64>) -> i64 {
    last.map_or(hi, |v| v.max(hi))
}

pub(crate) async fn summary(s: &mut SqlServerSession, spec: &DeltaSpec) -> Result<Vec<BucketSum>> {
    let cat = catalog(s, &spec.table).await?;
    check_table(s, spec, &cat).await?;
    let cols = hashed(spec, &cat)?;
    let rows = s.rows(&summary_sql(spec, &cols), &[]).await?;
    let out: Vec<BucketSum> = rows
        .iter()
        .map(|r| {
            // Only a NULL key has no bucket, and check_key refuses those
            // columns; never fold it into another bucket.
            let bucket = r.get::<i64, _>(0).ok_or_else(|| {
                Error::Unsupported("hay filas con la clave en NULL: no se puede sincronizar por filas".into())
            })?;
            Ok(BucketSum { bucket, rows: r.get::<i64, _>(1).unwrap_or(0) as u64, sum: text(r, 2).unwrap_or_else(|| "0".into()) })
        })
        .collect::<Result<_>>()?;
    let behind = identity_behind(s, spec, &cat).await?;
    seal(spec, &cat, out, behind)
}

/// What a side's summary adds to its bucket sums, never a bucket of its own:
/// with hash buckets, the key collations' fingerprint on every bucket; a
/// target whose identity is behind, [`force_apply`].
pub(crate) fn seal(spec: &DeltaSpec, cat: &[Col], mut out: Vec<BucketSum>, identity_behind: bool) -> Result<Vec<BucketSum>> {
    if matches!(spec.buckets, Buckets::Hash { .. }) {
        if let Some(fp) = collation_fingerprint(spec, cat)? {
            for b in &mut out {
                b.sum = add_to_sum(&b.sum, fp)?;
            }
        }
    }
    if identity_behind {
        force_apply(&mut out, IDENTITY_BEHIND)?;
    }
    out.sort_unstable_by_key(|b| b.bucket);
    Ok(out)
}

/// `v` added to a bucket's decimal sum (a `decimal(38,0)`: fits an i128).
pub(crate) fn add_to_sum(sum: &str, v: u64) -> Result<String> {
    let n: i128 = sum.trim().parse().map_err(|_| Error::Query(format!("suma de grupo inválida: {sum}")))?;
    Ok(n.wrapping_add(v as i128).to_string())
}

/// The highest bucket's sum changed by `fp` (bucket 0, empty, when there's
/// none): the table is applied even with equal rows, and the count of
/// changed buckets the engine weighs grows by one at most (by none when
/// that bucket changed anyway, as the top one usually does with new rows).
pub(crate) fn force_apply(out: &mut Vec<BucketSum>, fp: u64) -> Result<()> {
    match out.iter_mut().max_by_key(|b| b.bucket) {
        Some(b) => b.sum = add_to_sum(&b.sum, fp)?,
        None => out.push(BucketSum { bucket: 0, rows: 0, sum: fp.to_string() }),
    }
    Ok(())
}

/// Whether a side's range-column identity is behind `hi` in a way only a
/// target can be: its next value (`next`) is not past `hi` and its own
/// largest key (`max`, `None`: empty) is below `hi`.
///
/// The fingerprint only cancels when both sides agree, and the source must
/// never report: `hi` is the larger of its largest key and its last value,
/// so its next value is only `<= hi` when someone reseeded it below its own
/// rows, and then `hi` is its largest key. A target whose rows equal the
/// source's and whose last value is past `hi` doesn't report either. What
/// looks the same from both sides is a target reseeded below its own rows
/// while the source's last value is its largest key: not forced, reseeded on
/// the table's next real change.
pub(crate) fn identity_behind_hi(next: i128, hi: i64, max: Option<i64>) -> bool {
    next <= hi as i128 && max.is_none_or(|m| m < hi)
}

/// [`identity_behind_hi`] for this side's table (range buckets on an
/// identity column only).
async fn identity_behind(s: &mut SqlServerSession, spec: &DeltaSpec, cat: &[Col]) -> Result<bool> {
    let Buckets::Range { column, hi, .. } = &spec.buckets else { return Ok(false) };
    let Some(id) = cat.iter().find(|c| c.identity && c.name.eq_ignore_ascii_case(column)) else { return Ok(false) };
    let rows = s
        .rows(
            "SELECT CAST(IDENT_CURRENT(@P1) AS bigint), CAST(IDENT_INCR(@P1) AS bigint),
                    CAST(CASE WHEN ic.last_value IS NULL THEN 1 ELSE 0 END AS bit)
               FROM sys.identity_columns ic WHERE ic.object_id = OBJECT_ID(@P1)",
            &[&table_name(&spec.table)],
        )
        .await?;
    let Some(r) = rows.first() else { return Ok(false) };
    let (Some(current), Some(incr)) = (r.get::<i64, _>(0), r.get::<i64, _>(1)) else { return Ok(false) };
    // Never used (or truncated): the next row takes the current value itself.
    let next = if r.get::<bool, _>(2).unwrap_or(false) { current as i128 } else { current as i128 + incr as i128 };
    if next > *hi as i128 {
        return Ok(false);
    }
    let max = s.rows(&format!("SELECT CAST(MAX({}) AS bigint) FROM {}", q(&id.name), table_name(&spec.table)), &[]).await?;
    Ok(identity_behind_hi(next, *hi, max.first().and_then(|r| r.get::<i64, _>(0))))
}

/// `[schema].[__dbine_delta_<name≤80>_<hash of the full name>]`: tables
/// whose names share the first 80 characters get different staging tables.
fn staging_ref(table: &ObjectRef) -> ObjectRef {
    let name: String = table.name.chars().take(80).collect();
    let h = fnv(&table.name);
    ObjectRef { kind: "table".into(), schema: table.schema.clone(), name: format!("__dbine_delta_{name}_{h:016x}") }
}

fn drop_sql(t: &ObjectRef) -> String {
    let name = table_name(t);
    format!("IF OBJECT_ID(N'{}', N'U') IS NOT NULL DROP TABLE {name}", name.replace('\'', "''"))
}

/// A trigger or foreign key switched off during the merge.
#[derive(Debug, Clone)]
pub(crate) struct Switch {
    /// Schema and table it belongs to, unquoted.
    schema: String,
    owner: String,
    /// `[schema].[table]` it belongs to.
    table: String,
    name: String,
    /// A foreign key the server trusted before.
    trusted: bool,
    /// A foreign key an earlier sync left untrusted ([`UNTRUSTED_MARK`]).
    marked: bool,
    /// An incoming foreign key with a delete action on exactly the key
    /// columns: left enabled during the merge (it cascades as on the source).
    keep: bool,
    /// `[schema].[table]` of the foreign key's other table (the table being
    /// synced for a self-reference).
    other: String,
    /// `[schema].[table]` being synced, as the server names it.
    this: String,
}

impl Switch {
    /// Checked again after the merge: a kept key stays as trusted as it was.
    fn retrust(&self) -> bool {
        self.marked || (self.trusted && !self.keep)
    }

    /// Set NOCHECK during the merge.
    fn switched(&self) -> bool {
        !self.keep
    }

    /// `[schema].[table]` the foreign key points at.
    fn referenced(&self) -> &str {
        if self.table == self.this {
            &self.other
        } else {
            &self.this
        }
    }
}

/// The note for a foreign key checked again after the merge that the
/// target's data doesn't satisfy; `mark_failed`: why it couldn't be marked.
pub(crate) fn untrusted_note(fk: &Switch, why: &str, mark_failed: Option<&str>) -> String {
    let what = format!(
        "la clave foránea {} de {} a {} quedó sin verificar: los datos del destino todavía no la cumplen ({why}).",
        fk_name(fk),
        fk.table,
        fk.referenced()
    );
    let todo = if fk.other == fk.this {
        format!(
            " Revisá los datos de {}: la próxima sincronización de la tabla la vuelve a validar (si el origen tampoco la \
             cumple, hay que corregirlos ahí; comparando solo claves, las filas modificadas no se copian).",
            fk.this
        )
    } else {
        format!(
            " Sincronizá {o} y la próxima sincronización la vuelve a validar (si {o} ya está al día, el origen tampoco \
             la cumple, o se comparó solo por claves y las filas modificadas no se copiaron).",
            o = fk.other
        )
    };
    format!("{what}{todo}{}", manual_note(fk, mark_failed))
}

/// The note for a foreign key whose check after the merge kept losing a
/// deadlock against another operation on the same tables: nothing is known
/// about its data.
pub(crate) fn blocked_note(fk: &Switch, mark_failed: Option<&str>) -> String {
    format!(
        "la clave foránea {} de {} a {} quedó sin verificar: al validarla chocó una y otra vez con otra operación que \
         usaba las mismas tablas (bloqueo mutuo), así que no se sabe si los datos la cumplen. La próxima sincronización \
         la vuelve a validar.{}",
        fk_name(fk),
        fk.table,
        fk.referenced(),
        manual_note(fk, mark_failed)
    )
}

/// What to do by hand when the key couldn't be marked (`mark_failed`: why).
fn manual_note(fk: &Switch, mark_failed: Option<&str>) -> String {
    mark_failed.map_or(String::new(), |e| {
        format!(
            " No se le pudo poner la marca {UNTRUSTED_MARK} ({e}), así que no se reintenta sola: validala con \
             ALTER TABLE {} WITH CHECK CHECK CONSTRAINT {}.",
            fk.table,
            q(&fk.name)
        )
    })
}

fn nstr(v: &str) -> String {
    format!("N'{}'", v.replace('\'', "''"))
}

/// `[schema].[name]` of the foreign key itself, for `OBJECT_ID`.
fn fk_name(fk: &Switch) -> String {
    format!("{}.{}", q(&fk.schema), q(&fk.name))
}

/// Whether the foreign key has the `UNTRUSTED_MARK`, as a condition.
pub(crate) fn mark_exists(fk: &Switch) -> String {
    format!(
        "EXISTS (SELECT 1 FROM sys.extended_properties WHERE class = 1 AND major_id = OBJECT_ID({}) AND minor_id = 0 AND name = {})",
        nstr(&fk_name(fk)),
        nstr(UNTRUSTED_MARK)
    )
}

/// The foreign key's `UNTRUSTED_MARK` added (`on`, valued with the table
/// being synced) or removed, each only if needed. The check and the change
/// aren't atomic: the other table's sync may add or remove the mark in
/// between, so "already there" on add (15233) and "not there" on remove
/// (15217) are what was wanted and not errors. Removing answers one row:
/// 1 when this batch removed the mark, 0 when there was none.
pub(crate) fn mark_sql(fk: &Switch, on: bool) -> String {
    let level = format!(
        "@level0type = N'SCHEMA', @level0name = {}, @level1type = N'TABLE', @level1name = {}, @level2type = N'CONSTRAINT', @level2name = {}",
        nstr(&fk.schema),
        nstr(&fk.owner),
        nstr(&fk.name)
    );
    let exists = mark_exists(fk);
    if on {
        format!(
            "BEGIN TRY
    IF NOT {exists} EXEC sys.sp_addextendedproperty @name = {}, @value = {}, {level};
END TRY
BEGIN CATCH
    IF ERROR_NUMBER() <> {PROPERTY_EXISTS} THROW;
END CATCH;",
            nstr(UNTRUSTED_MARK),
            nstr(&fk.this)
        )
    } else {
        format!(
            "SET NOCOUNT ON;
DECLARE @dropped int = 0;
BEGIN TRY
    IF {exists}
    BEGIN
        EXEC sys.sp_dropextendedproperty @name = {}, {level};
        SET @dropped = 1;
    END
END TRY
BEGIN CATCH
    IF ERROR_NUMBER() <> {PROPERTY_MISSING} THROW;
END CATCH;
SELECT @dropped;",
            nstr(UNTRUSTED_MARK)
        )
    }
}

/// `SELECT TOP 0 … INTO` from the destination: its types and collations,
/// identity stripped by `CONVERT`.
pub(crate) fn staging_sql(table: &ObjectRef, staging: &ObjectRef, cols: &[&Col]) -> String {
    let list: Vec<String> = cols
        .iter()
        .map(|c| {
            if c.identity {
                format!("CONVERT({}, {n}) AS {n}", format_type(&c.ty, c.max_len, c.precision, c.scale), n = q(&c.name))
            } else {
                q(&c.name)
            }
        })
        .collect();
    format!("SELECT TOP 0 {} INTO {} FROM {}", list.join(", "), table_name(staging), table_name(table))
}

pub(crate) struct MergePlan<'a> {
    pub table: &'a ObjectRef,
    pub staging: &'a ObjectRef,
    pub key: &'a [String],
    pub cols: &'a [&'a Col],
    /// Rows of the changed buckets; `None`: the whole table.
    pub filter: Option<String>,
    pub triggers: &'a [Switch],
    pub fks: &'a [Switch],
}

/// The whole merge, one transaction, counts as the last result.
pub(crate) fn merge_sql(p: &MergePlan) -> String {
    let t = table_name(p.table);
    let identity = p.cols.iter().any(|c| c.identity);
    let mut sql = String::from("SET XACT_ABORT ON;\nSET NOCOUNT ON;\nDECLARE @a TABLE (a nvarchar(10) NOT NULL);\nBEGIN TRANSACTION;\n");
    for tr in p.triggers {
        sql.push_str(&format!("DISABLE TRIGGER {} ON {};\n", q(&tr.name), tr.table));
    }
    for fk in p.fks.iter().filter(|f| f.switched()) {
        sql.push_str(&format!("ALTER TABLE {} NOCHECK CONSTRAINT {};\n", fk.table, q(&fk.name)));
    }
    if identity {
        sql.push_str(&format!("SET IDENTITY_INSERT {t} ON;\n"));
    }
    let on: Vec<String> = p.key.iter().map(|k| format!("d.{k} = s.{k}", k = q(k))).collect();
    let names: Vec<String> = p.cols.iter().map(|c| q(&c.name)).collect();
    let values: Vec<String> = names.iter().map(|n| format!("s.{n}")).collect();
    let set: Vec<String> = p.cols.iter().filter(|c| !c.identity).map(|c| format!("d.{n} = s.{n}", n = q(&c.name))).collect();
    sql.push_str(&format!("WITH d AS (SELECT * FROM {t}"));
    if let Some(f) = &p.filter {
        sql.push_str(&format!(" WHERE {f}"));
    }
    sql.push_str(&format!(")\nMERGE d USING {} AS s ON {}\n", table_name(p.staging), on.join(" AND ")));
    if !set.is_empty() {
        sql.push_str(&format!(
            "WHEN MATCHED AND HASHBYTES('MD5', {}) <> HASHBYTES('MD5', {}) THEN UPDATE SET {}\n",
            row_bytes(p.cols, Some("d"), DeltaDepth::Full),
            row_bytes(p.cols, Some("s"), DeltaDepth::Full),
            set.join(", ")
        ));
    }
    sql.push_str(&format!(
        "WHEN NOT MATCHED BY TARGET THEN INSERT ({}) VALUES ({})\nWHEN NOT MATCHED BY SOURCE THEN DELETE\nOUTPUT $action INTO @a;\n",
        names.join(", "),
        values.join(", ")
    ));
    if identity {
        sql.push_str(&format!("SET IDENTITY_INSERT {t} OFF;\n"));
    }
    for fk in p.fks.iter().filter(|f| f.switched()) {
        sql.push_str(&format!("ALTER TABLE {} CHECK CONSTRAINT {};\n", fk.table, q(&fk.name)));
    }
    for tr in p.triggers {
        sql.push_str(&format!("ENABLE TRIGGER {} ON {};\n", q(&tr.name), tr.table));
    }
    sql.push_str(
        "COMMIT TRANSACTION;\n\
         SELECT COUNT_BIG(CASE WHEN a = N'INSERT' THEN 1 END), COUNT_BIG(CASE WHEN a = N'UPDATE' THEN 1 END), \
         COUNT_BIG(CASE WHEN a = N'DELETE' THEN 1 END) FROM @a;",
    );
    sql
}

/// Reseed the identity after the merge. When it's the range column, `hi`
/// is the source's last identity value (see [`stretch_hi`]): the target
/// goes past it, never back. Otherwise the server's own `RESEED` (up to the
/// largest value in the table).
pub(crate) fn reseed_sql(table: &ObjectRef, col: &Col, hi: Option<i64>) -> String {
    let t = nstr(&table_name(table));
    let plain = format!("DBCC CHECKIDENT ({t}, RESEED) WITH NO_INFOMSGS;");
    let Some(hi) = hi else { return plain };
    format!(
        "DECLARE @v bigint = {hi}, @m bigint, @c bigint, @i bigint;
SELECT @m = CAST(MAX({c}) AS bigint) FROM {tn};
SELECT @c = CAST(last_value AS bigint), @i = CAST(increment_value AS bigint)
  FROM sys.identity_columns WHERE object_id = OBJECT_ID({t}) AND name = {cn};
IF @i > 0
BEGIN
    IF @m > @v SET @v = @m;
    IF @c > @v SET @v = @c;
    -- No row ever inserted: the next one takes the reseed value itself.
    IF @c IS NULL SET @v = @v + @i;
    DBCC CHECKIDENT ({t}, RESEED, @v) WITH NO_INFOMSGS;
END
ELSE {plain}",
        hi = lit(hi as i128),
        c = q(&col.name),
        tn = table_name(table),
        cn = nstr(&col.name),
    )
}

type Conn = tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>;

async fn on_conn(conn: &mut Conn, sql: String) -> tiberius::Result<()> {
    conn.simple_query(sql).await?.into_results().await.map(|_| ())
}

pub(crate) fn check_sql(fk: &Switch) -> String {
    format!("ALTER TABLE {} WITH CHECK CHECK CONSTRAINT {}", fk.table, q(&fk.name))
}

/// The foreign key is untrusted now (when it can't be read: assumed so, and
/// reported).
async fn still_untrusted(conn: &mut Conn, fk: &Switch) -> bool {
    let sql = format!("SELECT CAST(is_not_trusted AS int) FROM sys.foreign_keys WHERE object_id = OBJECT_ID({})", nstr(&fk_name(fk)));
    let rows = match conn.simple_query(sql).await {
        Ok(st) => st.into_first_result().await.ok(),
        Err(_) => None,
    };
    rows.and_then(|r| r.first().and_then(|r| r.get::<i32, _>(0))).is_none_or(|v| v != 0)
}

/// The server's error number, for a server error.
fn server_code(e: &tiberius::error::Error) -> Option<u32> {
    match e {
        tiberius::error::Error::Server(t) => Some(t.code()),
        _ => None,
    }
}

/// `WITH CHECK CHECK CONSTRAINT`, run again when chosen as a deadlock
/// victim (another table's sync at once): the scan decides nothing then.
async fn validate(conn: &mut Conn, fk: &Switch) -> tiberius::Result<()> {
    let mut attempt = 1;
    loop {
        match on_conn(conn, check_sql(fk)).await {
            Err(e) if server_code(&e) == Some(DEADLOCK) && attempt < CHECK_TRIES => {
                tokio::time::sleep(std::time::Duration::from_millis(150 * attempt)).await;
                attempt += 1;
            }
            r => return r,
        }
    }
}

/// Removes the mark: `true` when this call removed it, `false` when it
/// wasn't there (never set, or the other table's sync removed it first).
async fn unmark(conn: &mut Conn, fk: &Switch) -> tiberius::Result<bool> {
    let results = conn.simple_query(mark_sql(fk, false)).await?.into_results().await?;
    Ok(results.iter().flatten().last().and_then(|r| r.get::<i32, _>(0)).is_some_and(|v| v != 0))
}

/// Checks a foreign key again: marked first (unless it already was), the
/// mark cleared when it passes. `Ok(true)`: passed, it had a mark from an
/// earlier sync and this call removed it (worth a note; when the other
/// table's sync removed it first, that one says it); `Ok(false)`: passed;
/// `Err(note)`: still untrusted, marked, with the note. A failure that finds
/// the key trusted (another sync checked it meanwhile) counts as passed.
async fn recheck(conn: &mut Conn, fk: &Switch) -> std::result::Result<bool, String> {
    let pre = if fk.marked { Ok(()) } else { on_conn(conn, mark_sql(fk, true)).await };
    match validate(conn, fk).await {
        Ok(()) => match unmark(conn, fk).await {
            Ok(removed) => Ok(fk.marked && removed),
            Err(e) => Err(format!(
                "la clave foránea {} de {} quedó verificada, pero no se le pudo quitar la marca {UNTRUSTED_MARK} ({e})",
                fk_name(fk),
                fk.table
            )),
        },
        Err(e) => {
            if !still_untrusted(conn, fk).await {
                return Ok(false);
            }
            tracing::warn!("sqlserver delta: foreign key {} on {} left untrusted: {e}", fk.name, fk.table);
            // Again, in case another sync's pass cleared it meanwhile.
            let marked = match pre {
                Ok(()) => on_conn(conn, mark_sql(fk, true)).await,
                Err(e) => Err(e),
            };
            let mark_err = marked.err().map(|e| e.to_string());
            if server_code(&e) == Some(DEADLOCK) {
                Err(blocked_note(fk, mark_err.as_deref()))
            } else {
                Err(untrusted_note(fk, &e.to_string(), mark_err.as_deref()))
            }
        }
    }
}

async fn exec(s: &mut SqlServerSession, sql: &str) -> Result<()> {
    s.client.simple_query(sql).await.map_err(err)?.into_results().await.map_err(err)?;
    Ok(())
}

/// Drop the staging table, retried on a new connection each time: a broken
/// connection, or a deadlock victim among the catalog locks of other tables
/// syncing at once. What's still left is dropped by the table's next apply.
async fn drop_staging(s: &mut SqlServerSession, staging: &ObjectRef) {
    let sql = drop_sql(staging);
    let mut last = None;
    for attempt in 0..4u64 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(200 * attempt)).await;
            if let Err(e) = s.reconnect().await {
                last = Some(e);
                continue;
            }
        }
        match exec(s, &sql).await {
            Ok(()) => return,
            Err(e) => last = Some(e),
        }
    }
    if let Some(e) = last {
        tracing::warn!("sqlserver delta: staging table {} left behind: {e}", table_name(staging));
    }
}

/// Creates the staging table. `SELECT … INTO` fails with [`SCHEMA_CHANGED`]
/// when another table's sync switches a foreign key on this table (its
/// merge's `NOCHECK` / `CHECK`) between creating the staging table and
/// reading the table's schema; the staging table is left behind, so it's
/// dropped and created again. The caller drops it on every other failure.
async fn create_staging(s: &mut SqlServerSession, table: &ObjectRef, staging: &ObjectRef, cols: &[&Col]) -> Result<()> {
    let sql = staging_sql(table, staging, cols);
    for attempt in 1..=STAGING_TRIES {
        let e = match s.client.simple_query(sql.as_str()).await {
            Ok(st) => match st.into_results().await {
                Ok(_) => return Ok(()),
                Err(e) => e,
            },
            Err(e) => e,
        };
        if server_code(&e) != Some(SCHEMA_CHANGED) {
            return Err(err(e));
        }
        tracing::debug!("sqlserver delta: staging table for {} hit a schema change, try {attempt}", table_name(table));
        exec(s, &drop_sql(staging)).await?;
        tokio::time::sleep(std::time::Duration::from_millis(100 * attempt)).await;
    }
    Err(Error::Query(format!(
        "no se pudo preparar la sincronización de {}: otra operación cambió la tabla (por ejemplo, la sincronización \
         de una tabla relacionada activando o desactivando sus claves foráneas) mientras se creaba la tabla de paso, \
         {STAGING_TRIES} veces seguidas. No se cambió nada en el destino; volvé a sincronizar la tabla.",
        table_name(table)
    )))
}

/// The destination has a unique, unfiltered index (or PK) on exactly `key`.
async fn has_unique_key(s: &mut SqlServerSession, table: &ObjectRef, key: &[String]) -> Result<bool> {
    let rows = s
        .rows(
            "SELECT i.index_id, c.name
               FROM sys.indexes i
               JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0
               JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
              WHERE i.object_id = OBJECT_ID(@P1) AND i.is_unique = 1 AND i.has_filter = 0 AND i.is_disabled = 0",
            &[&table_name(table)],
        )
        .await?;
    let mut by_index: std::collections::BTreeMap<i32, Vec<String>> = Default::default();
    for r in &rows {
        by_index.entry(r.get::<i32, _>(0).unwrap_or(0)).or_default().push(text(r, 1).unwrap_or_default().to_lowercase());
    }
    let mut want: Vec<String> = key.iter().map(|k| k.to_lowercase()).collect();
    want.sort();
    Ok(by_index.into_values().any(|mut v| {
        v.sort();
        v == want
    }))
}

/// The foreign keys on or into `table` (`@P1`, the mark's name `@P2`), as
/// `parent schema, parent, name, is_not_trusted, marked, keep, other
/// schema, other, this`.
pub(crate) fn fk_sql(key: &[String]) -> String {
    let keys: Vec<String> = key.iter().map(|k| nstr(k)).collect();
    format!(
        "SELECT OBJECT_SCHEMA_NAME(fk.parent_object_id), OBJECT_NAME(fk.parent_object_id), fk.name, fk.is_not_trusted,
                CAST(CASE WHEN EXISTS (SELECT 1 FROM sys.extended_properties ep
                                        WHERE ep.class = 1 AND ep.major_id = fk.object_id AND ep.minor_id = 0 AND ep.name = @P2)
                     THEN 1 ELSE 0 END AS bit),
                -- Incoming, with a delete action, on exactly the key: the merge
                -- never changes a key value, so it stays on and cascades.
                CAST(CASE WHEN fk.parent_object_id <> OBJECT_ID(@P1) AND fk.delete_referential_action <> 0
                           AND (SELECT COUNT(*) FROM sys.foreign_key_columns fc WHERE fc.constraint_object_id = fk.object_id) = {n}
                           AND NOT EXISTS (SELECT 1 FROM sys.foreign_key_columns fc
                                            WHERE fc.constraint_object_id = fk.object_id
                                              AND COL_NAME(fc.referenced_object_id, fc.referenced_column_id) NOT IN ({keys}))
                     THEN 1 ELSE 0 END AS bit),
                OBJECT_SCHEMA_NAME(CASE WHEN fk.parent_object_id = OBJECT_ID(@P1) THEN fk.referenced_object_id ELSE fk.parent_object_id END),
                OBJECT_NAME(CASE WHEN fk.parent_object_id = OBJECT_ID(@P1) THEN fk.referenced_object_id ELSE fk.parent_object_id END),
                QUOTENAME(OBJECT_SCHEMA_NAME(OBJECT_ID(@P1))) + N'.' + QUOTENAME(OBJECT_NAME(OBJECT_ID(@P1)))
           FROM sys.foreign_keys fk
          WHERE (fk.parent_object_id = OBJECT_ID(@P1) OR fk.referenced_object_id = OBJECT_ID(@P1)) AND fk.is_disabled = 0",
        n = key.len(),
        keys = keys.join(", ")
    )
}

async fn switches(s: &mut SqlServerSession, table: &ObjectRef, key: &[String]) -> Result<(Vec<Switch>, Vec<Switch>)> {
    let name = table_name(table);
    let tr = s
        .rows(
            "SELECT OBJECT_SCHEMA_NAME(parent_id), OBJECT_NAME(parent_id), name
               FROM sys.triggers WHERE parent_id = OBJECT_ID(@P1) AND is_disabled = 0",
            &[&name],
        )
        .await?;
    let fk = s.rows(&fk_sql(key), &[&name, UNTRUSTED_MARK]).await?;
    let switch = |r: &tiberius::Row| {
        let (schema, owner) = (text(r, 0).unwrap_or_default(), text(r, 1).unwrap_or_default());
        Switch {
            table: format!("{}.{}", q(&schema), q(&owner)),
            schema,
            owner,
            name: text(r, 2).unwrap_or_default(),
            trusted: false,
            marked: false,
            keep: false,
            other: String::new(),
            this: String::new(),
        }
    };
    Ok((
        tr.iter().map(switch).collect(),
        fk.iter()
            .map(|r| Switch {
                trusted: !r.get::<bool, _>(3).unwrap_or(true),
                marked: r.get::<bool, _>(4).unwrap_or(false),
                keep: r.get::<bool, _>(5).unwrap_or(false),
                other: format!("{}.{}", q(&text(r, 6).unwrap_or_default()), q(&text(r, 7).unwrap_or_default())),
                this: text(r, 8).unwrap_or_default(),
                ..switch(r)
            })
            .collect(),
    ))
}

/// Checks on the loaded staging rows before the merge, as one row of
/// flags: source keys the target's collation calls equal, and non-key
/// identity columns whose value differs on a matched row (SQL Server can't
/// update an identity column, so the row would never converge). `None`:
/// nothing to check (keys without collations are unique as loaded).
pub(crate) fn precheck_sql(p: &MergePlan) -> Option<String> {
    let collated = p.cols.iter().any(|c| c.collation.is_some() && p.key.iter().any(|k| k.eq_ignore_ascii_case(&c.name)));
    let identities: Vec<&&Col> =
        p.cols.iter().filter(|c| c.identity && !p.key.iter().any(|k| k.eq_ignore_ascii_case(&c.name))).collect();
    if !collated && identities.is_empty() {
        return None;
    }
    let mut sql = if collated {
        let key: Vec<String> = p.key.iter().map(|k| q(k)).collect();
        format!(
            "SELECT CAST(CASE WHEN EXISTS (SELECT 1 FROM {} GROUP BY {} HAVING COUNT_BIG(*) > 1) THEN 1 ELSE 0 END AS int)",
            table_name(p.staging),
            key.join(", ")
        )
    } else {
        "SELECT 0".to_string()
    };
    let on: Vec<String> = p.key.iter().map(|k| format!("d.{k} = s.{k}", k = q(k))).collect();
    // A key matches rows of the same bucket on both sides, so the join
    // needs no bucket filter.
    for c in identities {
        sql.push_str(&format!(
            ", CAST(CASE WHEN EXISTS (SELECT 1 FROM {} AS d JOIN {} AS s ON {} WHERE d.{n} <> s.{n}) THEN 1 ELSE 0 END AS int)",
            table_name(p.table),
            table_name(p.staging),
            on.join(" AND "),
            n = q(&c.name)
        ));
    }
    Some(sql)
}

pub(crate) async fn apply(
    s: &mut SqlServerSession,
    spec: &DeltaSpec,
    buckets: &[i64],
    columns: &[TransferColumn],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<DeltaResult> {
    let cat = catalog(s, &spec.table).await?;
    check_table(s, spec, &cat).await?;
    let cols: Vec<&Col> = spec.columns.iter().map(|n| find(&cat, n)).collect::<Result<_>>()?;
    for c in &cols {
        if c.computed || c.rowversion() {
            return Err(Error::Query(format!("La columna «{}» del destino es calculada o rowversion: no se puede cargar", c.name)));
        }
    }
    for k in &spec.key {
        if !cols.iter().any(|c| c.name.eq_ignore_ascii_case(k)) {
            return Err(Error::Query(format!("La columna «{k}» de la clave no está entre las que se copian")));
        }
    }
    if !has_unique_key(s, &spec.table, &spec.key).await? {
        return Err(Error::Unsupported(format!(
            "{} no tiene en el destino una clave única sobre ({}): no se puede sincronizar por filas",
            table_name(&spec.table),
            spec.key.join(", ")
        )));
    }
    let filter = if buckets.is_empty() { None } else { Some(filter(spec, buckets)?) };
    let (triggers, fks) = switches(s, &spec.table, &spec.key).await?;

    let staging = staging_ref(&spec.table);
    exec(s, &drop_sql(&staging)).await?;
    let res = async {
        // Inside: any failure from here on drops the staging table.
        create_staging(s, &spec.table, &staging, &cols).await?;
        let load = LoadSpec {
            table: staging.clone(),
            columns: spec.columns.clone(),
            table_lock: true,
            keep_identity: false,
            commit_rows: STAGE_BATCH_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        };
        crate::transfer::bulk_load(s, &load, columns, source, progress).await?;
        let plan = MergePlan { table: &spec.table, staging: &staging, key: &spec.key, cols: &cols, filter, triggers: &triggers, fks: &fks };
        if let Some(sql) = precheck_sql(&plan) {
            let rows = s.rows(&sql, &[]).await?;
            let flag = |i: usize| rows.first().and_then(|r| r.get::<i32, _>(i)).unwrap_or(0) != 0;
            if flag(0) {
                return Err(Error::Unsupported(format!(
                    "hay filas del origen cuya clave ({}) es la misma según la intercalación (collation) del destino: \
                     no entran en {} sin perder filas",
                    spec.key.join(", "),
                    table_name(&spec.table)
                )));
            }
            let identities = cols.iter().filter(|c| c.identity && !spec.key.iter().any(|k| k.eq_ignore_ascii_case(&c.name)));
            if let Some((c, _)) = identities.enumerate().map(|(i, c)| (c, flag(i + 1))).find(|(_, differs)| *differs) {
                return Err(Error::Unsupported(format!(
                    "la columna identidad «{}» no es parte de la clave y tiene otros valores en el destino: SQL Server no \
                     deja actualizar una columna identidad, así que esas filas no se pueden igualar sincronizando por filas \
                     (hay que vaciar y copiar la tabla)",
                    c.name
                )));
            }
        }
        // IDENTITY_INSERT is per session: a connection of its own, closed after.
        let mut conn = connect_once(s.config.clone()).await.map_err(connect_error)?;
        let results = conn.simple_query(merge_sql(&plan)).await.map_err(err)?.into_results().await.map_err(err)?;
        let row = results.last().and_then(|r| r.first());
        let get = |i: usize| row.and_then(|r| r.get::<i64, _>(i)).unwrap_or(0) as u64;
        let mut result = DeltaResult { inserted: get(0), updated: get(1), deleted: get(2), ..Default::default() };
        // After the commit: what can't be finished is reported as an error
        // naming the applied counts, never only logged.
        let mut problems = Vec::new();
        if let Some(id) = cols.iter().find(|c| c.identity) {
            let hi = match &spec.buckets {
                Buckets::Range { column, hi, .. } if column.eq_ignore_ascii_case(&id.name) => Some(*hi),
                _ => None,
            };
            if let Err(e) = on_conn(&mut conn, reseed_sql(&spec.table, id, hi)).await {
                problems.push(format!("no se pudo ajustar la identidad «{}» ({e})", id.name));
            }
        }
        let verified = |fk: &Switch| format!("la clave foránea {} de {} a {} volvió a quedar verificada", fk_name(fk), fk.table, fk.referenced());
        // Trusted before (the merge left the switched ones untrusted) or
        // marked by an earlier sync: checked again, by name.
        for fk in fks.iter().filter(|f| f.retrust()) {
            match recheck(&mut conn, fk).await {
                Ok(true) => result.notes.push(verified(fk)),
                Ok(false) => {}
                Err(note) => result.notes.push(note),
            }
        }
        // Marks set since the snapshot, by the other table's sync running at
        // once: its merge committed before ours and its check may have missed
        // our rows (see the module docs). Checked here too; a failure is that
        // sync's note, not ours.
        match switches(s, &spec.table, &spec.key).await {
            Ok((_, now)) => {
                let handled = |f: &Switch| fks.iter().any(|o| o.retrust() && o.table == f.table && o.name == f.name);
                for fk in now.iter().filter(|f| f.marked && !handled(f)) {
                    if !still_untrusted(&mut conn, fk).await {
                        continue;
                    }
                    match recheck(&mut conn, fk).await {
                        Ok(true) => result.notes.push(verified(fk)),
                        // The other sync removed the mark first: its note.
                        Ok(false) => {}
                        Err(note) => tracing::warn!("sqlserver delta: {note}"),
                    }
                }
            }
            Err(e) => tracing::warn!("sqlserver delta: foreign keys not read again after the merge: {e}"),
        }
        if !problems.is_empty() {
            problems.extend(result.notes.iter().cloned());
            return Err(Error::State(format!(
                "{}: se aplicaron los cambios ({} insertadas, {} actualizadas, {} borradas), pero {}",
                table_name(&spec.table),
                result.inserted,
                result.updated,
                result.deleted,
                problems.join("; ")
            )));
        }
        Ok::<_, Error>(result)
    }
    .await;
    drop_staging(s, &staging).await;
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> Col {
        Col {
            name: name.into(),
            ty: ty.into(),
            max_len: if ty.ends_with("(max)") { -1 } else { 4 },
            precision: 10,
            scale: 0,
            computed: false,
            identity: false,
            collation: None,
            nullable: false,
            ident_down: false,
        }
    }

    fn switch(owner: &str, name: &str, trusted: bool, marked: bool) -> Switch {
        Switch {
            schema: "dbo".into(),
            owner: owner.into(),
            table: format!("[dbo].[{owner}]"),
            name: name.into(),
            trusted,
            marked,
            keep: false,
            other: "[dbo].[p]".into(),
            this: format!("[dbo].[{owner}]"),
        }
    }

    fn spec(buckets: Buckets) -> DeltaSpec {
        DeltaSpec {
            table: ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "t".into() },
            key: vec!["id".into()],
            columns: vec!["id".into(), "v".into()],
            buckets,
            depth: DeltaDepth::Full,
            max_cores: 0,
        }
    }

    fn range() -> Buckets {
        Buckets::Range { column: "id".into(), lo: 1, hi: 10_000, width: 1_000, n: 10 }
    }

    #[test]
    fn range_bucket_expression() {
        let e = bucket_expr(&spec(range()), Some("t"));
        assert_eq!(
            e,
            "CASE WHEN t.[id] < 1 THEN CAST(-1 AS bigint) WHEN t.[id] > 10000 THEN CAST(10 AS bigint) \
             ELSE (CAST(t.[id] AS bigint) - CAST(1 AS bigint)) / CAST(1000 AS bigint) END"
        );
        // A span wider than a bigint goes through decimal.
        let wide = Buckets::Range { column: "id".into(), lo: i64::MIN, hi: i64::MAX, width: i64::MAX, n: 3 };
        let e = bucket_expr(&spec(wide), None);
        assert!(e.contains("decimal(20,0)") && e.contains("(-9223372036854775807-1)"), "{e}");
    }

    #[test]
    fn hash_bucket_never_abs() {
        let mut s = spec(Buckets::Hash { n: 101 });
        s.key = vec!["a".into(), "b".into()];
        let e = bucket_expr(&s, Some("t"));
        assert_eq!(e, "((CHECKSUM(t.[a], t.[b]) % 101) + 101) % 101");
        assert!(!e.to_uppercase().contains("ABS"));
        let f = filter(&s, &[7, 3, 3, 200]).unwrap();
        assert_eq!(f, "((CHECKSUM([a], [b]) % 101) + 101) % 101 IN (3, 7)");
        assert!(!summary_sql(&s, &[&col("a", "int")]).to_uppercase().contains("ABS("));
    }

    #[test]
    fn hash_buckets_from_the_orchestrator_are_prime() {
        // The n this module is handed comes from dbine-transfer's primes; the
        // expression uses it as is, so a prime n stays prime.
        let s = spec(Buckets::Hash { n: 65_537 });
        assert!(bucket_expr(&s, None).contains("% 65537) + 65537) % 65537"));
    }

    #[test]
    fn range_filters_merge_adjacent_buckets() {
        let s = spec(range());
        assert_eq!(filter(&s, &[2, 3, 4]).unwrap(), "([id] >= 2001 AND [id] < 5001)");
        assert_eq!(filter(&s, &[0, 5, 6, 9]).unwrap(), "([id] >= 1 AND [id] < 1001) OR ([id] >= 5001 AND [id] < 7001) OR ([id] >= 9001 AND [id] <= 10000)");
        // Edges: below lo and above hi, open-ended.
        assert_eq!(filter(&s, &[-1]).unwrap(), "[id] < 1");
        assert_eq!(filter(&s, &[10]).unwrap(), "[id] > 10000");
        assert_eq!(filter(&s, &[-1, 0]).unwrap(), "[id] < 1001");
        assert_eq!(filter(&s, &[9, 10]).unwrap(), "[id] >= 9001");
        assert_eq!(filter(&s, &(-1..=10).collect::<Vec<_>>()).unwrap(), "1 = 1");
        // Strays ignored.
        assert_eq!(filter(&s, &[i64::MIN, i64::MIN + 1, 11]).unwrap(), "1 = 0");
        assert_eq!(filter(&s, &[i64::MIN + 1, 2]).unwrap(), "([id] >= 2001 AND [id] < 3001)");
        let h = spec(Buckets::Hash { n: 17 });
        assert_eq!(filter(&h, &[i64::MIN, -1, 17]).unwrap(), "1 = 0");
    }

    #[test]
    fn row_hash_shape() {
        let cols = [col("a", "int"), col("t", "text"), col("x", "xml"), col("m", "nvarchar(max)")];
        let refs: Vec<&Col> = cols.iter().collect();
        let full = row_bytes(&refs, None, DeltaDepth::Full);
        assert!(full.starts_with("CASE WHEN [a] IS NULL THEN 0x00 ELSE 0x01 + CAST(CAST(DATALENGTH([a]) AS int) AS binary(4)) + CAST([a] AS varbinary(max)) END"));
        assert!(full.contains("CAST(CAST([t] AS varchar(max)) AS varbinary(max))"));
        assert!(full.contains("CAST(CAST([x] AS nvarchar(max)) AS varbinary(max))"));
        let sizes = row_bytes(&refs, None, DeltaDepth::Sizes);
        assert!(sizes.contains("CAST(CAST(DATALENGTH([m]) AS bigint) AS binary(8))"));
        assert!(!sizes.contains("CAST([m] AS varbinary(max))"));
        assert!(sizes.contains("CAST([a] AS varbinary(max))"));
        let mut calc = col("c", "int");
        calc.computed = true;
        let rv = col("rv", "timestamp");
        assert_eq!(row_bytes(&[&calc, &rv], None, DeltaDepth::Full), "0x");
    }

    #[test]
    fn summary_groups_without_order_and_honours_maxdop() {
        let mut s = spec(range());
        let (a, b) = (col("id", "int"), col("v", "int"));
        let sql = summary_sql(&s, &[&a, &b]);
        assert!(sql.contains("CROSS APPLY") && sql.contains("GROUP BY x.b") && !sql.contains("ORDER BY") && !sql.contains("MAXDOP"));
        assert!(sql.contains("SUBSTRING(HASHBYTES('MD5', "));
        s.max_cores = 2;
        assert!(summary_sql(&s, &[&a, &b]).ends_with(" OPTION (MAXDOP 2)"));
    }

    #[test]
    fn keys_depth_hashes_the_key_only() {
        let mut s = spec(range());
        s.depth = DeltaDepth::Keys;
        let cat = [col("id", "int"), col("v", "int")];
        let h = hashed(&s, &cat).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].name, "id");
    }

    #[test]
    fn refused_keys() {
        let s = spec(range());
        for ty in ["float", "real", "sql_variant"] {
            assert!(check_key(&s, &[col("id", ty), col("v", "int")]).is_err(), "{ty}");
        }
        assert!(check_key(&s, &[col("id", "bigint")]).is_ok());
    }

    #[test]
    fn staging_and_merge() {
        let t = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "x".repeat(120) };
        let st = staging_ref(&t);
        assert_eq!(st.name.len(), "__dbine_delta_".len() + 80 + 17);
        assert!(st.name.len() <= 128);
        let mut id = col("id", "int");
        id.identity = true;
        let v = col("v", "nvarchar(max)");
        let cols = [&id, &v];
        assert!(staging_sql(&t, &st, &cols).starts_with("SELECT TOP 0 CONVERT(int, [id]) AS [id], [v] INTO [dbo].[__dbine_delta_"));
        let tr = [switch("t", "trg", false, false)];
        let fk = [switch("child", "fk_c", true, false)];
        let key = ["id".to_string()];
        let sql = merge_sql(&MergePlan {
            table: &t,
            staging: &st,
            key: &key,
            cols: &cols,
            filter: Some("[id] < 5".into()),
            triggers: &tr,
            fks: &fk,
        });
        let pos = |p: &str| sql.find(p).unwrap_or_else(|| panic!("{p} missing:\n{sql}"));
        assert!(pos("SET XACT_ABORT ON") < pos("BEGIN TRANSACTION"));
        assert!(pos("DISABLE TRIGGER [trg] ON [dbo].[t]") < pos("MERGE d"));
        assert!(pos("NOCHECK CONSTRAINT [fk_c]") < pos("SET IDENTITY_INSERT"));
        assert!(pos(" WHERE [id] < 5)") < pos("MERGE d"));
        assert!(sql.contains("UPDATE SET d.[v] = s.[v]\n") && !sql.contains("d.[id] = s.[id],"));
        assert!(pos("OUTPUT $action INTO @a") < pos(" OFF;\n"));
        assert!(pos("ENABLE TRIGGER") < pos("COMMIT TRANSACTION"));
        assert!(pos("ALTER TABLE [dbo].[child] CHECK CONSTRAINT [fk_c]") < pos("COMMIT TRANSACTION"));
    }

    #[test]
    fn nullable_key_columns_are_refused() {
        let mut s = spec(range());
        let mut id = col("id", "int");
        id.nullable = true;
        let e = check_key(&s, &[id.clone(), col("v", "int")]).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("admite NULL")), "{e}");
        // A composite key with one nullable column too.
        s.key = vec!["a".into(), "b".into()];
        let mut b = col("b", "varchar");
        b.nullable = true;
        assert!(matches!(check_key(&s, &[col("a", "int"), b]), Err(Error::Unsupported(_))));
        // Nullable non-key columns are fine.
        let mut v = col("v", "int");
        v.nullable = true;
        assert!(check_key(&spec(range()), &[col("id", "int"), v]).is_ok());
    }

    #[test]
    fn staging_names_differ_past_the_prefix() {
        let t = |n: String| ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: n };
        let a = staging_ref(&t(format!("{}_a", "x".repeat(100))));
        let b = staging_ref(&t(format!("{}_b", "x".repeat(100))));
        assert_ne!(a.name, b.name);
        assert_eq!(staging_ref(&t("t".into())).name, staging_ref(&t("t".into())).name, "stable per table");
    }

    #[test]
    fn identity_stretches_hi_however_far() {
        // Burned and deleted 102 past a max of 101.
        assert_eq!(stretch_hi(101, Some(102)), 102);
        assert_eq!(stretch_hi(101, Some(101)), 101);
        assert_eq!(stretch_hi(101, None), 101);
        // Far past the largest key: still followed (the buckets get wider).
        assert_eq!(stretch_hi(101, Some(1_000_000)), 1_000_000);
        assert_eq!(stretch_hi(i64::MAX - 1, Some(i64::MAX)), i64::MAX);
        // Reseeded below the rows: hi stays the largest key.
        assert_eq!(stretch_hi(101, Some(50)), 101);
    }

    #[test]
    fn only_the_range_column_may_be_a_synced_identity() {
        let mut id = col("id", "int");
        id.identity = true;
        let cat = [id.clone(), col("v", "int")];
        // Range buckets on the identity: carried.
        assert!(check_identity(&spec(range()), &cat).is_ok());
        // Hash buckets: refused, with the reason.
        let e = check_identity(&spec(Buckets::Hash { n: 17 }), &cat).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("columna identidad «id»") && m.contains("hash")), "{e}");
        // Not the first key column (range on another one).
        let mut s = spec(Buckets::Range { column: "k".into(), lo: 1, hi: 10, width: 1, n: 10 });
        s.key = vec!["k".into(), "id".into()];
        s.columns = vec!["k".into(), "id".into(), "v".into()];
        let e = check_identity(&s, &[col("k", "int"), id.clone(), col("v", "int")]).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("no es la primera columna")), "{e}");
        // Not synced: nothing to follow.
        s.columns = vec!["k".into(), "v".into()];
        assert!(check_identity(&s, &[col("k", "int"), id.clone(), col("v", "int")]).is_ok());
        // Negative increment, even on the range column.
        let mut down = id;
        down.ident_down = true;
        let e = check_identity(&spec(range()), &[down, col("v", "int")]).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("incremento negativo")), "{e}");
    }

    #[test]
    fn delete_action_keys_on_the_key_stay_on_the_rest_are_switched() {
        let sql = fk_sql(&["id".to_string(), "o'k".to_string()]);
        assert!(sql.contains("fk.delete_referential_action <> 0"), "{sql}");
        assert!(sql.contains("WHERE fc.constraint_object_id = fk.object_id) = 2"), "{sql}");
        assert!(sql.contains("NOT IN (N'id', N'o''k')"), "{sql}");
        assert!(!sql.contains("AND NOT (fk.parent_object_id"), "no key is left out of the list: {sql}");
        // A kept key: never NOCHECK, checked again only when marked.
        let mut kept = switch("ch", "fk_ch_p", true, false);
        kept.keep = true;
        assert!(!kept.switched() && !kept.retrust(), "trusted and kept: nothing to do");
        kept.marked = true;
        assert!(kept.retrust(), "N1: a marked kept key is checked again");
        let t = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "p".into() };
        let st = staging_ref(&t);
        let (id, v) = (col("id", "int"), col("v", "int"));
        let cols = [&id, &v];
        let key = ["id".to_string()];
        let fks = [kept, switch("ch", "fk_ch_code", true, false)];
        let sql = merge_sql(&MergePlan { table: &t, staging: &st, key: &key, cols: &cols, filter: None, triggers: &[], fks: &fks });
        assert!(!sql.contains("[fk_ch_p]"), "kept on during the merge:\n{sql}");
        assert!(sql.contains("NOCHECK CONSTRAINT [fk_ch_code]") && sql.contains(" CHECK CONSTRAINT [fk_ch_code]"), "{sql}");
    }

    #[test]
    fn reseed_follows_the_source_never_back() {
        let t = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "idg".into() };
        let mut id = col("id", "int");
        id.identity = true;
        let sql = reseed_sql(&t, &id, Some(102));
        assert!(sql.starts_with("DECLARE @v bigint = 102,"), "{sql}");
        assert!(sql.contains("IF @m > @v SET @v = @m;") && sql.contains("IF @c > @v SET @v = @c;"), "{sql}");
        assert!(sql.contains("IF @c IS NULL SET @v = @v + @i;"), "{sql}");
        assert!(sql.contains("DBCC CHECKIDENT (N'[dbo].[idg]', RESEED, @v)"), "{sql}");
        assert_eq!(reseed_sql(&t, &id, None), "DBCC CHECKIDENT (N'[dbo].[idg]', RESEED) WITH NO_INFOMSGS;");
    }

    #[test]
    fn prechecks_duplicates_and_identity() {
        let t = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "t".into() };
        let st = staging_ref(&t);
        let key = ["k".to_string()];
        let check = |cols: &[&Col]| {
            precheck_sql(&MergePlan { table: &t, staging: &st, key: &key, cols, filter: None, triggers: &[], fks: &[] })
        };
        let (k, v) = (col("k", "int"), col("v", "int"));
        // Integer key, no identity: nothing to check.
        assert_eq!(check(&[&k, &v]), None);
        // Collated key: duplicates under the target's collation.
        let mut sk = col("k", "varchar");
        sk.collation = Some("Latin1_General_CI_AS".into());
        let sql = check(&[&sk, &v]).unwrap();
        assert!(sql.contains("GROUP BY [k] HAVING COUNT_BIG(*) > 1"), "{sql}");
        // Non-key identity: differing values on matched rows.
        let mut n = col("n", "int");
        n.identity = true;
        let sql = check(&[&k, &n]).unwrap();
        assert!(sql.starts_with("SELECT 0, "), "{sql}");
        assert!(sql.contains("JOIN [dbo].[__dbine_delta_t_") && sql.contains("ON d.[k] = s.[k] WHERE d.[n] <> s.[n]"), "{sql}");
        // An identity key is matched, not compared.
        let mut ik = col("k", "int");
        ik.identity = true;
        assert_eq!(check(&[&ik, &v]), None);
    }

    #[test]
    fn untrusted_mark_is_added_and_removed_by_name() {
        let fk = switch("ch", "fk_ch_p", false, true);
        assert!(fk.retrust(), "a marked key is checked again");
        assert!(switch("ch", "x", true, false).retrust());
        assert!(!switch("ch", "x", false, false).retrust(), "untrusted before, untouched");
        let add = mark_sql(&fk, true);
        assert!(add.contains("IF NOT EXISTS (SELECT 1 FROM sys.extended_properties WHERE class = 1 AND major_id = OBJECT_ID(N'[dbo].[fk_ch_p]')"), "{add}");
        assert!(add.contains("EXEC sys.sp_addextendedproperty @name = N'dbine_delta_untrusted', @value = N'[dbo].[ch]'"), "{add}");
        assert!(add.contains("@level1name = N'ch'") && add.contains("@level2type = N'CONSTRAINT', @level2name = N'fk_ch_p'"), "{add}");
        let drop = mark_sql(&fk, false);
        assert!(drop.contains("IF EXISTS (") && drop.contains("EXEC sys.sp_dropextendedproperty @name = N'dbine_delta_untrusted'"), "{drop}");
        assert!(mark_sql(&switch("o'k", "f", false, false), true).contains("N'o''k'"));
    }

    /// The other table's sync may add or remove the mark between the check
    /// and the change: "already there" on add and "not there" on remove are
    /// swallowed, anything else is thrown again; the remove says whether it
    /// removed the mark.
    #[test]
    fn mark_changes_tolerate_the_other_sync() {
        let fk = switch("ch", "fk_ch_p", false, true);
        let add = mark_sql(&fk, true);
        let pos = |sql: &str, p: &str| sql.find(p).unwrap_or_else(|| panic!("{p} missing:\n{sql}"));
        assert!(pos(&add, "BEGIN TRY") < pos(&add, "sp_addextendedproperty"), "{add}");
        assert!(add.contains("IF ERROR_NUMBER() <> 15233 THROW;"), "{add}");
        let drop = mark_sql(&fk, false);
        assert!(pos(&drop, "BEGIN TRY") < pos(&drop, "sp_dropextendedproperty"), "{drop}");
        assert!(pos(&drop, "sp_dropextendedproperty") < pos(&drop, "SET @dropped = 1;"), "set only when removed: {drop}");
        assert!(drop.contains("IF ERROR_NUMBER() <> 15217 THROW;") && drop.trim_end().ends_with("SELECT @dropped;"), "{drop}");
    }

    /// A check that kept losing deadlocks says so, never that the data
    /// doesn't satisfy the key.
    #[test]
    fn a_deadlocked_check_is_not_blamed_on_the_data() {
        let fk = switch("ch", "fk_ch_p", true, false);
        let n = blocked_note(&fk, None);
        assert!(n.contains("clave foránea [dbo].[fk_ch_p] de [dbo].[ch] a [dbo].[p] quedó sin verificar"), "{n}");
        assert!(n.contains("bloqueo mutuo") && n.contains("no se sabe si los datos la cumplen"), "{n}");
        assert!(!n.contains("todavía no la cumplen") && !n.contains("WITH CHECK"), "{n}");
        let n = blocked_note(&fk, Some("sin permiso"));
        assert!(n.contains("ALTER TABLE [dbo].[ch] WITH CHECK CHECK CONSTRAINT [fk_ch_p]"), "{n}");
    }

    /// [`mark_sql`] against a real server, with the `EXISTS` forced true as
    /// when the other sync changes the mark right after it: never an error.
    /// Uses `DBINE_TEST_SQLSERVER_URL` like tests/delta.rs (by default the
    /// `dbine-test-sqlserver` container).
    #[test]
    #[ignore]
    fn delta_mark_races_live() {
        let url = std::env::var("DBINE_TEST_SQLSERVER_URL").unwrap_or_else(|_| "mssql://sa:Pw_12345!@localhost:25013".into());
        let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
        let (auth, hostport) = rest.rsplit_once('@').unwrap();
        let (user, pass) = auth.split_once(':').unwrap();
        let (host, port) = hostport.rsplit_once(':').unwrap();
        let mut cfg = tiberius::Config::new();
        cfg.host(host);
        cfg.port(port.trim_end_matches('/').parse().unwrap());
        cfg.authentication(tiberius::AuthMethod::sql_server(user, pass));
        cfg.trust_cert();
        let db = "dbine_delta_mark_race";
        let reset = format!("IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut admin = connect_once(cfg.clone()).await.unwrap();
            on_conn(&mut admin, format!("{reset}; CREATE DATABASE [{db}]")).await.unwrap();
            let mut in_db = cfg.clone();
            in_db.database(db);
            let mut c = connect_once(in_db).await.unwrap();
            // As on the merge's connection.
            on_conn(&mut c, "SET XACT_ABORT ON; CREATE TABLE dbo.p (id int PRIMARY KEY); \
                             CREATE TABLE dbo.ch (id int PRIMARY KEY, p int NOT NULL CONSTRAINT fk_ch_p REFERENCES dbo.p(id))".into())
                .await
                .unwrap();
            let fk = switch("ch", "fk_ch_p", true, false);
            async fn count(c: &mut Conn) -> i32 {
                let marks = "SELECT COUNT(*) FROM sys.extended_properties WHERE name = N'dbine_delta_untrusted'";
                c.simple_query(marks).await.unwrap().into_row().await.unwrap().unwrap().get::<i32, _>(0).unwrap()
            }
            on_conn(&mut c, mark_sql(&fk, true)).await.unwrap();
            assert_eq!(count(&mut c).await, 1);
            // Add raced by the other sync's add: already there, fine.
            let raced_add = mark_sql(&fk, true).replace(&format!("NOT {}", mark_exists(&fk)), "1 = 1");
            assert!(raced_add.contains("IF 1 = 1 EXEC"), "{raced_add}");
            on_conn(&mut c, raced_add).await.expect("15233 swallowed");
            assert_eq!(count(&mut c).await, 1);
            assert!(unmark(&mut c, &fk).await.unwrap(), "removed by this call");
            assert_eq!(count(&mut c).await, 0);
            assert!(!unmark(&mut c, &fk).await.unwrap(), "nothing to remove");
            // Remove raced by the other sync's remove: not there, fine, and
            // not reported as removed.
            let raced_drop = mark_sql(&fk, false).replace(&mark_exists(&fk), "1 = 1");
            let rows = c.simple_query(raced_drop).await.expect("15217 swallowed").into_results().await.expect("15217 swallowed");
            assert_eq!(rows.iter().flatten().last().and_then(|r| r.get::<i32, _>(0)), Some(0));
            // Any other error still is one.
            let missing = switch("ch", "no_such_fk", true, false);
            assert!(on_conn(&mut c, mark_sql(&missing, true).replace(&format!("NOT {}", mark_exists(&missing)), "1 = 1")).await.is_err());
            assert!(unmark(&mut c, &missing).await.is_ok_and(|r| !r));
            drop(c);
            on_conn(&mut admin, reset).await.unwrap();
        });
    }

    fn sums(v: &[(i64, u64, &str)]) -> Vec<BucketSum> {
        v.iter().map(|(b, r, s)| BucketSum { bucket: *b, rows: *r, sum: s.to_string() }).collect()
    }

    /// Hash buckets: different key collations make every non-empty bucket of
    /// either side differ, so the apply covers every row; equal ones cancel
    /// out. Never a bucket of its own.
    #[test]
    fn collations_change_sums_not_bucket_counts() {
        let h = spec(Buckets::Hash { n: 17 });
        let mut s = h.clone();
        s.key = vec!["k".into()];
        let key = |coll: &str| {
            let mut c = col("k", "varchar");
            c.collation = Some(coll.into());
            vec![c, col("v", "int")]
        };
        let (cs, ci) = (key("Latin1_General_CS_AS"), key("Latin1_General_CI_AS"));
        let src = sums(&[(3, 2, "10"), (9, 1, "-7")]);
        let dst = sums(&[(3, 2, "10"), (12, 1, "5")]);
        let a = seal(&s, &cs, src.clone(), false).unwrap();
        let b = seal(&s, &ci, dst.clone(), false).unwrap();
        assert_eq!((a.len(), b.len()), (2, 2), "no sentinel");
        assert_eq!(dbine_driver::transfer::changed_buckets(&a, &b), vec![3, 9, 12], "every bucket either side has");
        let b = seal(&s, &cs, dst.clone(), false).unwrap();
        assert_eq!(dbine_driver::transfer::changed_buckets(&a, &b), vec![9, 12], "same collation: only real changes");
        // Range buckets don't depend on collations: sums untouched.
        let mut r = s.clone();
        r.buckets = range();
        assert_eq!(seal(&r, &ci, src.clone(), false).unwrap(), src);
        // An integer key has nothing to fingerprint.
        assert_eq!(seal(&h, &[col("id", "int"), col("v", "int")], src.clone(), false).unwrap(), src);
    }

    #[test]
    fn identity_behind_forces_one_bucket_without_adding_one() {
        let s = spec(range());
        let cat = [col("id", "int"), col("v", "int")];
        let src = sums(&[(0, 1000, "99"), (4, 3, "12")]);
        let dst = seal(&s, &cat, src.clone(), true).unwrap();
        assert_eq!(dst.len(), 2);
        assert_eq!(dbine_driver::transfer::changed_buckets(&src, &dst), vec![4], "the highest bucket");
        // The top bucket changed anyway (new rows): still one changed bucket.
        let mut moved = src.clone();
        moved[1].rows = 4;
        assert_eq!(dbine_driver::transfer::changed_buckets(&moved, &dst), vec![4]);
        // An empty target: bucket 0, no rows.
        let empty = seal(&s, &cat, vec![], true).unwrap();
        assert_eq!((empty.len(), empty[0].bucket, empty[0].rows), (1, 0, 0));
        // Sums past 64 bits and negative ones stay decimal text.
        assert_eq!(add_to_sum("-5", 7).unwrap(), "2");
        assert_eq!(add_to_sum("99999999999999999999999999999", 1).unwrap(), "100000000000000000000000000000");
        assert!(add_to_sum("x", 1).is_err());
    }

    /// V1: a source reseeded below its own rows (rows 1..5, reseeded to 2:
    /// next 3, `hi` 5) never reports, so equal tables converge; a target
    /// behind a burned source value still does.
    #[test]
    fn only_a_target_is_behind_the_source() {
        // The source: its largest key is hi whenever its next value is <= hi.
        assert!(!identity_behind_hi(3, 5, Some(5)), "V1: source reseeded below its rows");
        assert!(!identity_behind_hi(1, 5, Some(5)), "source truncated/reseeded to the seed");
        assert!(!identity_behind_hi(6, 5, Some(5)), "source as usual");
        // Its target, reseeded past hi by the previous sync.
        assert!(!identity_behind_hi(6, 5, Some(5)));
        // A target behind values the source burned (hi 104, rows to 101).
        assert!(identity_behind_hi(104, 104, Some(101)));
        assert!(identity_behind_hi(3, 104, Some(101)), "also below its own rows");
        // An empty target that never had rows.
        assert!(identity_behind_hi(1, 2, None));
        // Rows past hi: the edge bucket differs anyway.
        assert!(!identity_behind_hi(3, 5, Some(9)));
    }

    /// Trusted before or marked: checked again. A kept (cascading) key that
    /// was trusted stays untouched. Untrusted before and unmarked: left.
    #[test]
    fn which_keys_are_checked_again() {
        let mut kept = switch("ch", "f", true, false);
        kept.keep = true;
        assert!(!kept.retrust());
        kept.marked = true;
        kept.trusted = false;
        assert!(kept.retrust(), "a marked kept key");
        assert!(switch("ch", "f", true, false).retrust());
        assert!(switch("ch", "f", false, true).retrust());
        assert!(!switch("ch", "f", false, false).retrust(), "untrusted before: the source's trust isn't mirrored");
    }

    /// Schema-qualified everywhere: dbo.ch and x.ch with a key of the same
    /// name never mix.
    #[test]
    fn notes_and_marks_are_schema_qualified() {
        let mut x = switch("ch", "fk_ch_p", true, false);
        x.schema = "x".into();
        x.table = "[x].[ch]".into();
        x.this = "[x].[ch]".into();
        x.other = "[dbo].[p]".into();
        let n = untrusted_note(&x, "conflicto", None);
        assert!(n.contains("clave foránea [x].[fk_ch_p] de [x].[ch] a [dbo].[p] quedó sin verificar"), "{n}");
        assert!(n.contains("Sincronizá [dbo].[p] y la próxima sincronización la vuelve a validar"), "{n}");
        assert!(!n.contains("WITH CHECK"), "{n}");
        let add = mark_sql(&x, true);
        assert!(add.contains("OBJECT_ID(N'[x].[fk_ch_p]')") && add.contains("@level0name = N'x'") && add.contains("@value = N'[x].[ch]'"), "{add}");
        // Syncing the parent: the child's key into it names the child.
        let mut into = x.clone();
        into.this = "[dbo].[p]".into();
        into.other = "[x].[ch]".into();
        let n = untrusted_note(&into, "conflicto", Some("sin permiso"));
        assert!(n.contains("de [x].[ch] a [dbo].[p]") && n.contains("Sincronizá [x].[ch]"), "{n}");
        assert!(n.contains("ALTER TABLE [x].[ch] WITH CHECK CHECK CONSTRAINT [fk_ch_p]"), "{n}");
        // A self-reference.
        let mut me = switch("t", "fk_t_t", true, false);
        me.other = me.this.clone();
        let n = untrusted_note(&me, "conflicto", None);
        assert!(n.contains("de [dbo].[t] a [dbo].[t]") && n.contains("Revisá los datos de [dbo].[t]"), "{n}");
    }
}
