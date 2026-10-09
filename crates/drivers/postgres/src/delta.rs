//! Sync by rows (see `dbine_driver::transfer::DeltaSpec`): bucket
//! summaries on each side, the source filter of the changed buckets, and
//! the apply on the target.
//!
//! - **Buckets.** `Range`: `CASE WHEN k < lo THEN -1 WHEN k > hi THEN n
//!   ELSE (k - lo) / width END` on the first key column. `Hash`:
//!   `((hashtextextended(key_text, 0) % n) + n) % n` (never `abs`, which
//!   overflows on the smallest bigint), where `key_text` is the key
//!   columns' text, each one length-prefixed. A `timestamptz` key is
//!   taken in UTC so the bucket doesn't depend on the session's time zone
//!   (source and target servers may have different ones).
//! - **Row hash.** The first 8 bytes of the `md5` of the compared columns'
//!   text, each as `<chars>:<text>` or `N` for NULL, so NULL and empty and
//!   `('ab','c')` and `('a','bc')` never collide. `Sizes` takes large
//!   columns (the toastable ones) by their length only; for `text`,
//!   `varchar` and `bytea` the length comes from the TOAST header, so no
//!   off-row page is read. `Keys` hashes the key columns only.
//! - **Summary.** `bucket, count(*), sum(hash)` (a `numeric` sum, which
//!   never overflows), in a read-only transaction with the output settings
//!   that change a value's text (time zone, float digits, bytea format)
//!   pinned, so both servers spell the same value the same way.
//! - **Filter.** Adjacent `Range` buckets merge into one `k >= a AND k < b`
//!   range, so the key's index is used; `Hash` buckets are an `IN` list of
//!   the bucket expression.
//! - **Apply.** The source's rows of the changed buckets go into a
//!   temporary table with binary `COPY` (`transfer::bulk_load`); then, in one
//!   transaction: the target's rows of those buckets that aren't staged are
//!   deleted, the staged rows that differ are updated (compared as the row
//!   hash sees them, so a bucket that differs always converges) and the
//!   missing ones are inserted. PostgreSQL 17 and later update and insert in
//!   one `MERGE … RETURNING merge_action()`; earlier servers run `UPDATE`
//!   and `INSERT` (15 and 16 have `MERGE`, but without `RETURNING` it can't
//!   tell updates from inserts). User triggers and foreign key checks are
//!   off during the apply (`session_replication_role = replica`) when the
//!   role may set it; otherwise they fire, and the log says so.
//!
//! Variants: PostgreSQL and the distributions that keep its engine
//! (TimescaleDB, YugabyteDB, KingbaseES, AlloyDB, Cloud SQL, Aurora, EDB,
//! Fujitsu), from PostgreSQL 11 (`hashtextextended`). Not the rest:
//! openGauss (a 9.2 catalog without `hashtextextended`), Greenplum and its
//! forks (no temporary-table `COPY` binary and no `hashtextextended` in
//! Greenplum 6), CockroachDB (no `COPY FROM` over the extended protocol, no
//! `hashtextextended`), Redshift, Denodo, H2, CrateDB, Yellowbrick and the
//! streaming engines (not PostgreSQL's engine).

use crate::session::PgSession;
use crate::{err, Variant};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::transfer::{BatchSource, BucketSum, Buckets, DeltaDepth, DeltaResult, DeltaSpec, LoadSpec, Progress, TransferColumn};
use dbine_driver::{Error, ObjectRef, Result};
use tokio_postgres::SimpleQueryMessage;

/// The staging table (one per session, in its temporary schema).
const STAGE: &str = "dbine_delta_stage";

pub(crate) fn capable(v: Variant) -> bool {
    matches!(
        v,
        Variant::Postgres
            | Variant::Timescale
            | Variant::Yugabyte
            | Variant::Kingbase
            | Variant::AlloyDb
            | Variant::CloudSql
            | Variant::Aurora
            | Variant::Edb
            | Variant::Fujitsu
    )
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn table_name(t: &ObjectRef) -> String {
    qualified_name(Quote::Double, t.schema(), &t.name)
}

fn check(s: &PgSession) -> Result<()> {
    if !capable(s.variant) {
        return Err(Error::Unsupported(format!("{} no sincroniza por filas", s.variant.info().name)));
    }
    if s.version > 0 && s.version < 110000 {
        return Err(Error::Unsupported("sincronizar por filas necesita PostgreSQL 11 o posterior".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------- SQL

/// A key column's text for the hash: independent of the session's time
/// zone for `timestamptz`.
fn key_text(col: &str) -> String {
    let c = q(col);
    format!(
        "CASE WHEN pg_typeof({c}) = 'timestamp with time zone'::regtype \
         THEN (({c})::text::timestamptz AT TIME ZONE 'UTC')::text ELSE ({c})::text END"
    )
}

/// The bucket of a row, as a `bigint`.
pub(crate) fn bucket_expr(spec: &DeltaSpec) -> Result<String> {
    match &spec.buckets {
        Buckets::Range { column, lo, hi, width, n } => {
            if *width <= 0 || *n == 0 || hi < lo {
                return Err(Error::Query(format!("rangos inválidos: {lo}..{hi}, ancho {width}, {n} grupos")));
            }
            let c = q(column);
            // `k - lo` overflows a bigint when the range is wider than it.
            let offset = if hi.checked_sub(*lo).is_some() {
                format!("({c}::int8 - ({lo})) / {width}")
            } else {
                format!("div({c}::numeric - ({lo}), {width})::int8")
            };
            Ok(format!("(CASE WHEN {c} < {lo} THEN -1 WHEN {c} > {hi} THEN {n} ELSE {offset} END)"))
        }
        Buckets::Hash { n } => {
            if *n == 0 || spec.key.is_empty() {
                return Err(Error::Query("grupos por hash sin clave o sin cantidad".into()));
            }
            let text = if spec.key.len() == 1 {
                key_text(&spec.key[0])
            } else {
                spec.key
                    .iter()
                    .map(|k| {
                        let t = key_text(k);
                        format!("length({t})::text || ':' || {t}")
                    })
                    .collect::<Vec<_>>()
                    .join(" || ")
            };
            Ok(format!("(((hashtextextended({text}, 0) % {n}) + {n}) % {n})"))
        }
    }
}

/// The condition that selects the rows of `buckets`.
pub(crate) fn filter(spec: &DeltaSpec, buckets: &[i64]) -> Result<String> {
    if buckets.is_empty() {
        return Ok("FALSE".into());
    }
    let mut b = buckets.to_vec();
    b.sort_unstable();
    b.dedup();
    match &spec.buckets {
        Buckets::Hash { .. } => {
            let list = b.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
            Ok(format!("{} IN ({list})", bucket_expr(spec)?))
        }
        Buckets::Range { column, lo, hi, width, n } => {
            bucket_expr(spec)?; // validates
            let c = q(column);
            let (lo, hi, w, n) = (*lo as i128, *hi as i128, *width as i128, *n as i128);
            // A bucket's first value and the first one past it (`None`:
            // unbounded). Buckets `-1` and `n` hold the rows outside lo..=hi.
            let start = |k: i128| -> Option<i128> {
                match k {
                    k if k < 0 => None,
                    k if k >= n => Some((lo + n * w).min(hi + 1)),
                    k => Some(lo + k * w),
                }
            };
            let end = |k: i128| -> Option<i128> {
                match k {
                    k if k < 0 => Some(lo),
                    k if k >= n => None,
                    k => Some((lo + (k + 1) * w).min(hi + 1)),
                }
            };
            let mut runs: Vec<(i128, i128)> = Vec::new();
            for k in b.iter().map(|&k| k as i128) {
                match runs.last_mut() {
                    Some(r) if r.1 + 1 == k => r.1 = k,
                    _ => runs.push((k, k)),
                }
            }
            let (min, max) = (i64::MIN as i128, i64::MAX as i128);
            let parts: Vec<String> = runs
                .iter()
                .map(|&(a, z)| {
                    let mut conds: Vec<String> = Vec::new();
                    if let Some(s) = start(a).filter(|s| *s > min) {
                        conds.push(format!("{c} >= {s}"));
                    }
                    if let Some(e) = end(z).filter(|e| *e <= max) {
                        conds.push(format!("{c} < {e}"));
                    }
                    match conds.len() {
                        0 => "TRUE".to_string(),
                        1 => conds.remove(0),
                        _ if runs.len() > 1 => format!("({})", conds.join(" AND ")),
                        _ => conds.join(" AND "),
                    }
                })
                .collect();
            Ok(if parts.len() == 1 { parts.into_iter().next().unwrap_or_default() } else { format!("({})", parts.join(" OR ")) })
        }
    }
}

/// What the catalog says about a column.
#[derive(Debug, Clone)]
pub(crate) struct ColMeta {
    pub name: String,
    /// The type, or a domain's base type (`text`, `int4`…).
    pub base: String,
    /// Toastable (can live off-row).
    pub large: bool,
    /// `GENERATED ALWAYS AS (…) STORED`.
    pub generated: bool,
    /// `GENERATED ALWAYS AS IDENTITY`.
    pub identity_always: bool,
    /// Its identity or serial sequence.
    pub sequence: Option<String>,
}

async fn columns(s: &PgSession, table: &str) -> Result<Vec<ColMeta>> {
    let generated = if s.version >= 120000 { "a.attgenerated::text = 's'" } else { "false" };
    let sql = format!(
        "SELECT a.attname::text, bt.typname::text, t.typstorage::text IN ('x', 'e'), {generated}, a.attidentity::text = 'a',
                pg_get_serial_sequence($1::text, a.attname::text)
         FROM pg_attribute a
         JOIN pg_type t ON t.oid = a.atttypid
         JOIN pg_type bt ON bt.oid = CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END
         WHERE a.attrelid = $1::text::regclass AND a.attnum > 0 AND NOT a.attisdropped
         ORDER BY a.attnum"
    );
    let rows = s.client.query(&sql, &[&table]).await.map_err(err)?;
    Ok(rows
        .iter()
        .map(|r| ColMeta {
            name: r.get(0),
            base: r.get(1),
            large: r.get(2),
            generated: r.get(3),
            identity_always: r.get(4),
            sequence: r.get(5),
        })
        .collect())
}

fn find<'a>(cols: &'a [ColMeta], name: &str) -> Result<&'a ColMeta> {
    cols.iter().find(|c| c.name == name).ok_or_else(|| Error::Query(format!("la tabla no tiene la columna «{name}»")))
}

/// Types whose length comes from the TOAST header (`octet_length` without
/// reading the value).
fn cheap_length(base: &str) -> bool {
    matches!(base, "text" | "varchar" | "bpchar" | "bytea")
}

/// The row hash, a `bigint` from the first 8 bytes of an `md5`.
pub(crate) fn row_hash(spec: &DeltaSpec, cols: &[ColMeta]) -> Result<String> {
    let names: &[String] = if spec.depth == DeltaDepth::Keys { &spec.key } else { &spec.columns };
    let mut parts = Vec::with_capacity(names.len());
    for name in names {
        let col = find(cols, name)?;
        let c = q(name);
        let text = if spec.depth == DeltaDepth::Sizes && col.large && !spec.key.contains(name) {
            if cheap_length(&col.base) {
                format!("octet_length({c})::text")
            } else {
                format!("octet_length({c}::text)::text")
            }
        } else {
            format!("({c})::text")
        };
        parts.push(format!("COALESCE(length({text})::text || ':' || {text}, 'N')"));
    }
    if parts.is_empty() {
        return Err(Error::Query("no hay columnas para comparar".into()));
    }
    Ok(format!("('x' || substr(md5({}), 1, 16))::bit(64)::int8", parts.join(" || ")))
}

pub(crate) fn summary_sql(table: &str, bucket: &str, hash: &str) -> String {
    format!(
        "SELECT b::text, count(*)::text, COALESCE(sum(h), 0)::text \
         FROM (SELECT {bucket} AS b, {hash} AS h FROM {table}) x GROUP BY b"
    )
}

/// A column as compared by the apply: typed when equality means the same
/// text (what the row hash sees), otherwise by its text under `"C"`, so a
/// bucket that differs always converges (`1.0` vs `1.00`, `-0` vs `0`, a
/// case-insensitive collation, `json` without `=`…).
fn compared(alias: &str, col: &ColMeta) -> String {
    let c = format!("{alias}.{}", q(&col.name));
    match col.base.as_str() {
        "int2" | "int4" | "int8" | "bool" | "uuid" | "bytea" | "date" | "timestamp" | "timestamptz" | "time" | "oid" => c,
        _ => format!("({c})::text COLLATE \"C\""),
    }
}

/// The statements of an apply.
pub(crate) struct ApplySql {
    pub delete: String,
    /// `UPDATE` (`None`: no column to update).
    pub update: Option<String>,
    pub insert: String,
    /// PostgreSQL 17+: `UPDATE` and `INSERT` in one `MERGE`, returning
    /// (updated, inserted).
    pub merge: String,
}

pub(crate) fn apply_sql(table: &str, stage: &str, spec: &DeltaSpec, cols: &[ColMeta], scope: &str) -> Result<ApplySql> {
    let key: Vec<&ColMeta> = spec.key.iter().map(|k| find(cols, k)).collect::<Result<_>>()?;
    let written: Vec<&ColMeta> =
        spec.columns.iter().map(|c| find(cols, c)).collect::<Result<Vec<_>>>()?.into_iter().filter(|c| !c.generated).collect();
    let updatable: Vec<&ColMeta> =
        written.iter().copied().filter(|c| !c.identity_always && !spec.key.contains(&c.name)).collect();
    let on = key.iter().map(|k| format!("s.{0} = d.{0}", q(&k.name))).collect::<Vec<_>>().join(" AND ");
    let list = written.iter().map(|c| q(&c.name)).collect::<Vec<_>>().join(", ");
    let s_list = written.iter().map(|c| format!("s.{}", q(&c.name))).collect::<Vec<_>>().join(", ");
    let overriding = if written.iter().any(|c| c.identity_always) { " OVERRIDING SYSTEM VALUE" } else { "" };
    let row = |alias: &str| updatable.iter().map(|c| compared(alias, c)).collect::<Vec<_>>().join(", ");
    // A one-column row still needs ROW() to be a row.
    let distinct = format!("ROW({}) IS DISTINCT FROM ROW({})", row("d"), row("s"));
    let set = updatable.iter().map(|c| format!("{0} = s.{0}", q(&c.name))).collect::<Vec<_>>().join(", ");

    let delete = format!("DELETE FROM {table} AS d WHERE ({scope}) AND NOT EXISTS (SELECT 1 FROM {stage} s WHERE {on})");
    let update = (!updatable.is_empty()).then(|| format!("UPDATE {table} AS d SET {set} FROM {stage} s WHERE {on} AND {distinct}"));
    let insert = format!(
        "INSERT INTO {table} ({list}){overriding} SELECT {s_list} FROM {stage} s \
         WHERE NOT EXISTS (SELECT 1 FROM {table} d WHERE {on})"
    );
    let matched = if updatable.is_empty() { String::new() } else { format!(" WHEN MATCHED AND {distinct} THEN UPDATE SET {set}") };
    let merge = format!(
        "WITH m AS (MERGE INTO {table} AS d USING {stage} AS s ON {on}{matched} \
         WHEN NOT MATCHED THEN INSERT ({list}){overriding} VALUES ({s_list}) RETURNING merge_action() AS a) \
         SELECT count(*) FILTER (WHERE a = 'UPDATE'), count(*) FILTER (WHERE a = 'INSERT') FROM m"
    );
    Ok(ApplySql { delete, update, insert, merge })
}

// ---------------------------------------------------------------- session

pub(crate) async fn key_range(s: &PgSession, table: &ObjectRef, column: &str) -> Result<Option<(i64, i64, u64)>> {
    let name = table_name(table);
    // Only an integer key makes ranges (a numeric one would be rounded).
    let cols = columns(s, &name).await?;
    let col = find(&cols, column)?;
    if !matches!(col.base.as_str(), "int2" | "int4" | "int8") {
        return Err(Error::Unsupported(format!("la columna «{column}» no es entera ({})", col.base)));
    }
    let c = q(column);
    let sql = format!("SELECT min({c})::int8, max({c})::int8, count(*) FROM {name}");
    let row = s.client.query_one(&sql, &[]).await.map_err(err)?;
    let (lo, hi, n): (Option<i64>, Option<i64>, i64) = (row.get(0), row.get(1), row.get(2));
    Ok(match (lo, hi) {
        (Some(lo), Some(hi)) if n > 0 => Some((lo, hi, n as u64)),
        _ => None,
    })
}

pub(crate) async fn summary(s: &PgSession, spec: &DeltaSpec) -> Result<Vec<BucketSum>> {
    check(s)?;
    let table = table_name(&spec.table);
    let cols = columns(s, &table).await?;
    let sql = summary_sql(&table, &bucket_expr(spec)?, &row_hash(spec, &cols)?);
    let mut setup = String::from(
        "BEGIN READ ONLY; SET LOCAL TimeZone = 'UTC'; SET LOCAL extra_float_digits = 3; SET LOCAL bytea_output = 'hex';",
    );
    if spec.max_cores > 0 {
        setup.push_str(&format!(" SET LOCAL max_parallel_workers_per_gather = {};", spec.max_cores - 1));
    }
    s.client.batch_execute(&setup).await.map_err(err)?;
    let result = s.client.simple_query(&sql).await;
    let _ = s.client.batch_execute(if result.is_ok() { "COMMIT" } else { "ROLLBACK" }).await;
    let mut out = Vec::new();
    for m in result.map_err(err)? {
        if let SimpleQueryMessage::Row(r) = m {
            let bucket = r.get(0).and_then(|v| v.parse().ok()).ok_or_else(|| Error::Query("grupo inválido".into()))?;
            let rows = r.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            out.push(BucketSum { bucket, rows, sum: r.get(2).unwrap_or("0").to_string() });
        }
    }
    out.sort_by_key(|b| b.bucket);
    Ok(out)
}

pub(crate) async fn apply(
    s: &mut PgSession,
    spec: &DeltaSpec,
    buckets: &[i64],
    columns: &[TransferColumn],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<DeltaResult> {
    check(s)?;
    let table = table_name(&spec.table);
    let cols = self::columns(s, &table).await?;
    // The source's cells come in the read's order.
    let names: Vec<String> = if columns.is_empty() { spec.columns.clone() } else { columns.iter().map(|c| c.name.clone()).collect() };
    for n in names.iter().chain(&spec.key) {
        find(&cols, n)?;
    }
    for k in &spec.key {
        if !names.contains(k) {
            return Err(Error::Query(format!("la lectura no trae la columna de clave «{k}»")));
        }
    }
    let stage = format!("pg_temp.{}", q(STAGE));
    let scope = if buckets.is_empty() { "TRUE".to_string() } else { filter(spec, buckets)? };
    let mut spec_w = spec.clone();
    spec_w.columns = names.clone();
    let sql = apply_sql(&table, &stage, &spec_w, &cols, &scope)?;

    // Stage the source rows (temporary table: never logged, dropped with
    // the session at the latest).
    s.client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {stage}; CREATE TEMP TABLE {} AS SELECT {} FROM {table} WITH NO DATA",
            q(STAGE),
            names.iter().map(|n| q(n)).collect::<Vec<_>>().join(", ")
        ))
        .await
        .map_err(err)?;
    let load = LoadSpec {
        table: ObjectRef { kind: "table".into(), schema: Some("pg_temp".into()), name: STAGE.into() },
        columns: names.clone(),
        table_lock: false,
        keep_identity: true,
        commit_rows: 0,
        commit_bytes: 0,
    };
    let result = async {
        crate::transfer::bulk_load(s, &load, source, progress).await?;
        s.client.batch_execute(&format!("ANALYZE {stage}")).await.map_err(err)?;
        run_apply(s, &sql, &cols, &table).await
    }
    .await;
    if let Err(e) = s.client.batch_execute(&format!("DROP TABLE IF EXISTS {stage}")).await {
        tracing::debug!("delta staging not dropped: {e}");
    }
    result
}

async fn run_apply(s: &PgSession, sql: &ApplySql, cols: &[ColMeta], table: &str) -> Result<DeltaResult> {
    s.client.batch_execute("BEGIN").await.map_err(err)?;
    let r = async {
        // Triggers (and foreign key checks, which are triggers) off, when
        // the role may; a refusal only rolls back to the savepoint.
        s.client.batch_execute("SAVEPOINT dbine_role").await.map_err(err)?;
        match s.client.batch_execute("SET LOCAL session_replication_role = replica").await {
            Ok(()) => s.client.batch_execute("RELEASE SAVEPOINT dbine_role").await.map_err(err)?,
            Err(e) => {
                tracing::info!("sync by rows: triggers stay on (session_replication_role refused: {e})");
                s.client.batch_execute("ROLLBACK TO SAVEPOINT dbine_role").await.map_err(err)?;
            }
        }
        let mut r = DeltaResult { deleted: s.client.execute(&sql.delete, &[]).await.map_err(err)?, ..Default::default() };
        if s.version >= 170000 {
            let row = s.client.query_one(&sql.merge, &[]).await.map_err(err)?;
            r.updated = row.get::<_, i64>(0) as u64;
            r.inserted = row.get::<_, i64>(1) as u64;
        } else {
            if let Some(u) = &sql.update {
                r.updated = s.client.execute(u, &[]).await.map_err(err)?;
            }
            r.inserted = s.client.execute(&sql.insert, &[]).await.map_err(err)?;
        }
        if r.inserted > 0 {
            for seq_sql in resync(table, cols) {
                s.client.batch_execute(&seq_sql).await.map_err(err)?;
            }
        }
        Ok::<_, Error>(r)
    }
    .await;
    match r {
        Ok(r) => {
            s.client.batch_execute("COMMIT").await.map_err(err)?;
            Ok(r)
        }
        Err(e) => {
            let _ = s.client.batch_execute("ROLLBACK").await;
            Err(e)
        }
    }
}

/// Identity and serial sequences moved past the inserted values (only
/// forward).
fn resync(table: &str, cols: &[ColMeta]) -> Vec<String> {
    cols.iter()
        .filter_map(|c| {
            let seq = c.sequence.as_deref()?;
            // Every capable variant reads `E'…'`.
            let lit = crate::catalog::lit(Variant::Postgres, seq);
            Some(format!(
                "SELECT setval({lit}, m) FROM (SELECT max({}) AS m FROM {table}) x, {seq} q \
                 WHERE x.m IS NOT NULL AND (x.m > q.last_value OR NOT q.is_called)",
                q(&c.name)
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(buckets: Buckets, key: &[&str]) -> DeltaSpec {
        DeltaSpec {
            table: ObjectRef { kind: "table".into(), schema: Some("public".into()), name: "t".into() },
            key: key.iter().map(|s| s.to_string()).collect(),
            columns: vec!["id".into(), "name".into(), "body".into()],
            buckets,
            depth: DeltaDepth::Full,
            max_cores: 0,
        }
    }

    fn col(name: &str, base: &str, large: bool) -> ColMeta {
        ColMeta { name: name.into(), base: base.into(), large, generated: false, identity_always: false, sequence: None }
    }

    #[test]
    fn range_buckets_and_merged_filters() {
        let s = spec(Buckets::Range { column: "id".into(), lo: 1, hi: 1000, width: 100, n: 10 }, &["id"]);
        assert_eq!(
            bucket_expr(&s).unwrap(),
            "(CASE WHEN \"id\" < 1 THEN -1 WHEN \"id\" > 1000 THEN 10 ELSE (\"id\"::int8 - (1)) / 100 END)"
        );
        // 2,3,4 merge; -1 is unbounded below; 9 stops at hi; 10 is unbounded above.
        assert_eq!(filter(&s, &[3, 2, 4]).unwrap(), "\"id\" >= 201 AND \"id\" < 501");
        assert_eq!(filter(&s, &[-1, 0]).unwrap(), "\"id\" < 101");
        assert_eq!(filter(&s, &[9, 10]).unwrap(), "\"id\" >= 901");
        assert_eq!(filter(&s, &[9]).unwrap(), "\"id\" >= 901 AND \"id\" < 1001");
        assert_eq!(filter(&s, &[-1, 0, 5, 10]).unwrap(), "(\"id\" < 101 OR (\"id\" >= 501 AND \"id\" < 601) OR \"id\" >= 1001)");
        assert_eq!(filter(&s, &[-1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).unwrap(), "TRUE");
        assert_eq!(filter(&s, &[]).unwrap(), "FALSE");
        // Wider than a bigint: numeric arithmetic.
        let wide = spec(Buckets::Range { column: "id".into(), lo: i64::MIN, hi: i64::MAX, width: i64::MAX, n: 3 }, &["id"]);
        assert!(bucket_expr(&wide).unwrap().contains("div(\"id\"::numeric"));
        assert!(bucket_expr(&spec(Buckets::Range { column: "id".into(), lo: 5, hi: 1, width: 1, n: 1 }, &["id"])).is_err());
    }

    #[test]
    fn hash_buckets_never_use_abs() {
        let s = spec(Buckets::Hash { n: 17 }, &["a", "b"]);
        let e = bucket_expr(&s).unwrap();
        assert!(e.starts_with("(((hashtextextended(length(CASE WHEN pg_typeof(\"a\")"));
        assert!(e.ends_with(", 0) % 17) + 17) % 17)"));
        assert!(!e.contains("abs("));
        assert!(filter(&s, &[5, 1, 5]).unwrap().ends_with(" IN (1, 5)"));
        let one = bucket_expr(&spec(Buckets::Hash { n: 7 }, &["a"])).unwrap();
        assert!(!one.contains("length("), "{one}");
    }

    #[test]
    fn row_hash_depths() {
        let cols = vec![col("id", "int4", false), col("name", "varchar", true), col("body", "jsonb", true)];
        let mut s = spec(Buckets::Hash { n: 7 }, &["id"]);
        let full = row_hash(&s, &cols).unwrap();
        assert!(full.starts_with("('x' || substr(md5(COALESCE(length((\"id\")::text)::text || ':' || (\"id\")::text, 'N')"));
        assert!(full.ends_with(", 1, 16))::bit(64)::int8"));
        s.depth = DeltaDepth::Sizes;
        let sizes = row_hash(&s, &cols).unwrap();
        assert!(sizes.contains("octet_length(\"name\")::text") && sizes.contains("octet_length(\"body\"::text)::text"));
        assert!(sizes.contains("(\"id\")::text"));
        s.depth = DeltaDepth::Keys;
        let keys = row_hash(&s, &cols).unwrap();
        assert!(!keys.contains("name") && keys.contains("\"id\""));
        s.depth = DeltaDepth::Full;
        s.columns.push("missing".into());
        assert!(row_hash(&s, &cols).is_err());
    }

    #[test]
    fn apply_statements() {
        let mut cols = vec![col("id", "int4", false), col("name", "varchar", true), col("body", "jsonb", true), col("g", "int4", false)];
        cols[0].identity_always = true;
        cols[3].generated = true;
        let mut s = spec(Buckets::Hash { n: 7 }, &["id"]);
        s.columns.push("g".into());
        let a = apply_sql("\"public\".\"t\"", "pg_temp.\"st\"", &s, &cols, "x IN (1)").unwrap();
        assert_eq!(
            a.delete,
            "DELETE FROM \"public\".\"t\" AS d WHERE (x IN (1)) AND NOT EXISTS (SELECT 1 FROM pg_temp.\"st\" s WHERE s.\"id\" = d.\"id\")"
        );
        let u = a.update.unwrap();
        assert!(u.contains("SET \"name\" = s.\"name\", \"body\" = s.\"body\""), "{u}");
        assert!(u.contains("ROW((d.\"name\")::text COLLATE \"C\", (d.\"body\")::text COLLATE \"C\") IS DISTINCT FROM"), "{u}");
        assert!(!u.contains("\"g\""));
        assert!(a.insert.starts_with("INSERT INTO \"public\".\"t\" (\"id\", \"name\", \"body\") OVERRIDING SYSTEM VALUE SELECT s.\"id\""));
        assert!(a.merge.contains("WHEN NOT MATCHED THEN INSERT") && a.merge.contains("RETURNING merge_action()"));
        // Only the key: nothing to update.
        s.columns = vec!["id".into()];
        let a = apply_sql("t", "st", &s, &cols, "TRUE").unwrap();
        assert!(a.update.is_none() && !a.merge.contains("WHEN MATCHED"));
    }

    #[test]
    fn resync_sequence_name_stays_inside_its_literal() {
        let mut cols = vec![col("id", "int8", false)];
        cols[0].sequence = Some("x\\'; drop table t; --".into());
        let r = resync("\"public\".\"t\"", &cols);
        assert!(r[0].starts_with("SELECT setval(E'x\\\\''; drop table t; --', m)"), "{}", r[0]);
    }

    #[test]
    fn resync_moves_sequences_forward_only() {
        let mut cols = vec![col("id", "int8", false)];
        cols[0].sequence = Some("public.t_id_seq".into());
        let r = resync("\"public\".\"t\"", &cols);
        assert_eq!(r.len(), 1);
        assert!(r[0].starts_with("SELECT setval(E'public.t_id_seq', m) FROM (SELECT max(\"id\") AS m FROM \"public\".\"t\") x"), "{}", r[0]);
    }
}
