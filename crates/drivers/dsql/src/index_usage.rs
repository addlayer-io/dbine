//! A table's indexes (`Session::index_usage`). Aurora DSQL keeps no usage
//! statistics (no `pg_stat_*` counters for its distributed storage) and has
//! no foreign keys: the report lists the indexes, the primary key among
//! them, with `stats_available: false` and a note. They come from
//! `pg_index` (keys in key order, DESC from `indoption`, then INCLUDE).

use crate::err;
use dbine_driver::{IndexUsage, IndexUsageReport, ObjectRef, Result};
use tokio_postgres::Client;

/// One row per index of `$1.$2` (empty schema: the search path's).
pub const INDEXES_SQL: &str = "SELECT i.relname::text, COALESCE(am.amname::text, ''), x.indisprimary, x.indisunique,
        pg_catalog.pg_get_expr(x.indpred, x.indrelid),
        ARRAY(SELECT CASE WHEN k.attnum = 0 THEN '(' || pg_catalog.pg_get_indexdef(x.indexrelid, k.ord::int, true) || ')'
                          ELSE a.attname::text END
              FROM unnest(x.indkey::int2[]) WITH ORDINALITY k(attnum, ord)
              LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = k.attnum
              WHERE k.ord <= x.indnkeyatts ORDER BY k.ord),
        ARRAY(SELECT a.attname::text FROM unnest(x.indkey::int2[]) WITH ORDINALITY k(attnum, ord)
              JOIN pg_catalog.pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = k.attnum
              WHERE k.ord > x.indnkeyatts ORDER BY k.ord),
        ARRAY(SELECT o.opt::int FROM unnest(x.indoption::int2[]) WITH ORDINALITY o(opt, ord) ORDER BY o.ord)
 FROM pg_catalog.pg_index x
 JOIN pg_catalog.pg_class i ON i.oid = x.indexrelid
 LEFT JOIN pg_catalog.pg_am am ON am.oid = i.relam
 JOIN pg_catalog.pg_class t ON t.oid = x.indrelid
 JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
 WHERE t.relname = $2 AND (n.nspname = $1 OR ($1 = '' AND pg_catalog.pg_table_is_visible(t.oid)))
 ORDER BY x.indisprimary DESC, i.relname";

pub const NOTE: &str = "Aurora DSQL no informa cuánto se usa cada índice. Se listan los índices sin estadísticas de uso.";

/// Key columns with ` DESC` where `indoption` (bit 1) says so.
pub fn keys(keys: Vec<String>, opts: &[i32]) -> Vec<String> {
    keys.into_iter().enumerate().map(|(i, k)| if opts.get(i).is_some_and(|o| o & 1 != 0) { format!("{k} DESC") } else { k }).collect()
}

/// The access method as the index's kind.
pub fn kind(am: &str) -> String {
    if am.is_empty() { "INDEX".into() } else { am.to_ascii_uppercase() }
}

pub async fn report(client: &Client, table: &ObjectRef) -> Result<IndexUsageReport> {
    let schema = table.schema().unwrap_or("");
    let rows = client.query(INDEXES_SQL, &[&schema, &table.name]).await.map_err(err)?;
    let indexes = rows
        .iter()
        .map(|r| {
            let (primary_key, unique): (bool, bool) = (r.get(2), r.get(3));
            IndexUsage {
                name: r.get(0),
                kind: kind(&r.get::<_, String>(1)),
                unique: unique || primary_key,
                primary_key,
                filter: r.get(4),
                key_columns: keys(r.get(5), &r.get::<_, Vec<i32>>(7)),
                included_columns: r.get(6),
                ..Default::default()
            }
        })
        .collect();
    Ok(IndexUsageReport { indexes, note: Some(NOTE.into()), seek_scan_split: false, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_carry_desc_and_kinds_their_method() {
        assert_eq!(keys(vec!["a".into(), "(lower(b))".into()], &[1, 0]), ["a DESC", "(lower(b))"]);
        assert_eq!(keys(vec!["a".into()], &[]), ["a"]);
        assert_eq!((kind("btree"), kind("")), ("BTREE".to_string(), "INDEX".to_string()));
    }

    #[test]
    fn one_table_by_schema_or_search_path() {
        assert!(INDEXES_SQL.contains("t.relname = $2 AND (n.nspname = $1 OR ($1 = '' AND pg_catalog.pg_table_is_visible(t.oid)))"));
        assert!(INDEXES_SQL.contains("ORDER BY x.indisprimary DESC"), "primary key first");
    }
}
