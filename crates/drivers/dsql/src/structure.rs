//! What "Comparar esquemas" needs beyond columns and keys: CHECK
//! constraints, indexes with expressions, `INCLUDE`, descending keys, NULLS
//! order and `NULLS NOT DISTINCT`, and domains (DSQL's only user-defined
//! type: it has no CREATE TYPE), read from pg_catalog and written back as
//! the SQL DSQL takes.
//!
//! Index settings go into [`IndexDef::options`]: `desc`, `nulls_first` and
//! `nulls_last` (key columns, where not the default) and
//! `nulls_not_distinct`. An expression key goes into `columns` inside
//! parentheses, as `CREATE INDEX` writes it.

use crate::{err, SYSTEM_SCHEMAS};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{kinds, CheckDef, DbObject, IndexDef, KeyDef, Result, SyncScript, TableSchema};
use tokio_postgres::Client;

pub const OPT_DESC: &str = "desc";
pub const OPT_NULLS_FIRST: &str = "nulls_first";
pub const OPT_NULLS_LAST: &str = "nulls_last";
pub const OPT_NULLS_NOT_DISTINCT: &str = "nulls_not_distinct";

fn qi(s: &str) -> String {
    quote_ident(Quote::Double, s)
}

fn list(v: Option<&String>) -> Vec<String> {
    v.map(|s| s.split(',').map(|c| c.trim().to_lowercase()).filter(|c| !c.is_empty()).collect()).unwrap_or_default()
}

/// `a` → `"a"`; an expression (`(lower(a))`) stays as written.
fn key(c: &str) -> String {
    if c.starts_with('(') {
        c.to_string()
    } else {
        qi(c)
    }
}

/// One index's `CREATE INDEX ASYNC` (a plain CREATE INDEX only works on
/// empty tables).
pub fn index_ddl(t: &TableSchema, ix: &IndexDef, if_not_exists: bool) -> String {
    let table = qualified_name(Quote::Double, t.schema.as_deref(), &t.name);
    let (desc, first, last) = (list(ix.options.get(OPT_DESC)), list(ix.options.get(OPT_NULLS_FIRST)), list(ix.options.get(OPT_NULLS_LAST)));
    let keys: Vec<String> = ix
        .columns
        .iter()
        .map(|c| {
            let low = c.to_lowercase();
            let mut k = key(c);
            if desc.contains(&low) {
                k.push_str(" DESC");
            }
            if first.contains(&low) {
                k.push_str(" NULLS FIRST");
            } else if last.contains(&low) {
                k.push_str(" NULLS LAST");
            }
            k
        })
        .collect();
    let mut s = format!(
        "CREATE {}INDEX ASYNC {}{} ON {table} ({})",
        if ix.unique { "UNIQUE " } else { "" },
        if if_not_exists { "IF NOT EXISTS " } else { "" },
        qi(&ix.name),
        keys.join(", ")
    );
    if !ix.include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", ix.include.iter().map(|c| qi(c)).collect::<Vec<_>>().join(", ")));
    }
    if ix.options.get(OPT_NULLS_NOT_DISTINCT).is_some_and(|v| v == "true") {
        s.push_str(" NULLS NOT DISTINCT");
    }
    if let Some(w) = ix.filter.as_deref().filter(|w| !w.is_empty()) {
        s.push_str(&format!(" WHERE {w}"));
    }
    s.push(';');
    s
}

/// DSQL adds a CHECK to an existing table only `NOT VALID`; the rows that
/// are there are checked after, with an asynchronous `VALIDATE CONSTRAINT`.
pub fn fix_script(script: &mut SyncScript) {
    let mut out = Vec::with_capacity(script.statements.len());
    for s in script.statements.drain(..) {
        let add = s.strip_prefix("ALTER TABLE ").and_then(|r| {
            let (table, rest) = r.split_once(" ADD CONSTRAINT ")?;
            let name = rest.split_once(" CHECK ")?.0;
            Some((table.to_string(), name.to_string()))
        });
        match add {
            Some((table, name)) => {
                out.push(format!("{} NOT VALID;", s.trim_end_matches(';')));
                out.push(format!("ALTER TABLE ASYNC {table} VALIDATE CONSTRAINT {name};"));
            }
            None => out.push(s),
        }
    }
    script.statements = out;
}

/// `CHECK ((a > 0)) NOT VALID` → `(a > 0)`.
pub fn check_expression(def: &str) -> String {
    let d = def.trim();
    let d = d.strip_prefix("CHECK").unwrap_or(d).trim();
    let d = d.strip_suffix("NOT VALID").unwrap_or(d).trim();
    d.strip_suffix("NO INHERIT").unwrap_or(d).trim().to_string()
}

/// Every index (the primary key among them) and CHECK of the tables.
pub async fn complete(client: &Client, out: &mut [TableSchema]) -> Result<()> {
    let idx = format!(
        "SELECT n.nspname::text, t.relname::text, i.relname::text, x.indisprimary, x.indisunique,
                pg_catalog.pg_get_expr(x.indpred, x.indrelid),
                ARRAY(SELECT CASE WHEN k.attnum = 0 THEN '(' || pg_catalog.pg_get_indexdef(x.indexrelid, k.ord::int, true) || ')'
                                  ELSE a.attname::text END
                      FROM unnest(x.indkey::int2[]) WITH ORDINALITY k(attnum, ord)
                      LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = k.attnum
                      WHERE k.ord <= x.indnkeyatts ORDER BY k.ord),
                ARRAY(SELECT a.attname::text FROM unnest(x.indkey::int2[]) WITH ORDINALITY k(attnum, ord)
                      JOIN pg_catalog.pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = k.attnum
                      WHERE k.ord > x.indnkeyatts ORDER BY k.ord),
                ARRAY(SELECT o.opt::int FROM unnest(x.indoption::int2[]) WITH ORDINALITY o(opt, ord) ORDER BY o.ord),
                COALESCE(to_jsonb(x) ->> 'indnullsnotdistinct', 'false')
         FROM pg_catalog.pg_index x
         JOIN pg_catalog.pg_class i ON i.oid = x.indexrelid
         JOIN pg_catalog.pg_class t ON t.oid = x.indrelid
         JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
         WHERE t.relkind IN ('r', 'p') AND n.nspname NOT IN {SYSTEM_SCHEMAS} AND n.nspname NOT LIKE 'pg\\_%'
         ORDER BY 1, 2, 3"
    );
    for r in client.query(idx.as_str(), &[]).await.map_err(err)? {
        let (schema, table): (String, String) = (r.get(0), r.get(1));
        let Some(t) = out.iter_mut().find(|t| t.schema.as_deref() == Some(schema.as_str()) && t.name == table) else {
            continue;
        };
        let (name, columns): (String, Vec<String>) = (r.get(2), r.get(6));
        if r.get(3) {
            t.primary_key = Some(KeyDef { name: Some(name), columns });
            continue;
        }
        let opts: Vec<i32> = r.get(8);
        let mut ix = IndexDef { name, unique: r.get(4), filter: r.get(5), include: r.get(7), ..Default::default() };
        index_options(&mut ix, &columns, &opts, r.get::<_, String>(9) == "true");
        ix.columns = columns;
        t.indexes.push(ix);
    }

    let checks = format!(
        "SELECT n.nspname::text, c.relname::text, k.conname::text, pg_catalog.pg_get_constraintdef(k.oid, true)
         FROM pg_catalog.pg_constraint k
         JOIN pg_catalog.pg_class c ON c.oid = k.conrelid
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE k.contype = 'c' AND n.nspname NOT IN {SYSTEM_SCHEMAS} AND n.nspname NOT LIKE 'pg\\_%'
         ORDER BY 1, 2, 3"
    );
    for r in client.query(checks.as_str(), &[]).await.map_err(err)? {
        let (schema, table): (String, String) = (r.get(0), r.get(1));
        if let Some(t) = out.iter_mut().find(|t| t.schema.as_deref() == Some(schema.as_str()) && t.name == table) {
            t.checks.push(CheckDef { name: Some(r.get(2)), expression: check_expression(&r.get::<_, String>(3)) });
        }
    }
    Ok(())
}

/// `indoption` per key column: bit 1 DESC, bit 2 NULLS FIRST. The default
/// is NULLS LAST for ASC and NULLS FIRST for DESC.
pub fn index_options(ix: &mut IndexDef, columns: &[String], opts: &[i32], nulls_not_distinct: bool) {
    let (mut desc, mut first, mut last) = (Vec::new(), Vec::new(), Vec::new());
    for (c, o) in columns.iter().zip(opts) {
        let (d, nf) = (o & 1 != 0, o & 2 != 0);
        if d {
            desc.push(c.clone());
        }
        match (d, nf) {
            (false, true) => first.push(c.clone()),
            (true, false) => last.push(c.clone()),
            _ => {}
        }
    }
    for (k, v) in [(OPT_DESC, desc), (OPT_NULLS_FIRST, first), (OPT_NULLS_LAST, last)] {
        if !v.is_empty() {
            ix.options.insert(k.into(), v.join(", "));
        }
    }
    if nulls_not_distinct {
        ix.options.insert(OPT_NULLS_NOT_DISTINCT.into(), "true".into());
    }
}

/// The domains, as [`kinds::TYPE`].
pub async fn list_domains(client: &Client) -> Vec<DbObject> {
    let q = format!(
        "SELECT n.nspname::text, t.typname::text FROM pg_catalog.pg_type t
         JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
         WHERE t.typtype = 'd' AND n.nspname NOT IN {SYSTEM_SCHEMAS} AND n.nspname NOT LIKE 'pg\\_%'
         ORDER BY 1, 2"
    );
    client
        .query(q.as_str(), &[])
        .await
        .map(|rows| rows.iter().map(|r| DbObject { kind: kinds::TYPE.into(), schema: Some(r.get(0)), name: r.get(1), parent: None }).collect())
        .unwrap_or_default()
}

/// A domain's `CREATE DOMAIN`.
pub async fn domain_definition(client: &Client, schema: &str, name: &str) -> Result<Option<String>> {
    let rows = client
        .query(
            "SELECT pg_catalog.format_type(t.typbasetype, t.typtypmod), t.typnotnull, t.typdefault,
                    CASE WHEN t.typcollation <> 0 AND t.typcollation <> b.typcollation
                         THEN (SELECT quote_ident(nc.nspname) || '.' || quote_ident(c.collname) FROM pg_catalog.pg_collation c
                               JOIN pg_catalog.pg_namespace nc ON nc.oid = c.collnamespace WHERE c.oid = t.typcollation) END,
                    ARRAY(SELECT k.conname::text || E'\\t' || pg_catalog.pg_get_constraintdef(k.oid, true)
                          FROM pg_catalog.pg_constraint k WHERE k.contypid = t.oid AND k.contype = 'c' ORDER BY k.conname)
             FROM pg_catalog.pg_type t
             JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
             JOIN pg_catalog.pg_type b ON b.oid = t.typbasetype
             WHERE t.typtype = 'd' AND n.nspname = $1 AND t.typname = $2",
            &[&schema, &name],
        )
        .await
        .map_err(err)?;
    Ok(rows.first().map(|r| {
        let checks: Vec<String> = r.get(4);
        domain_ddl(schema, name, &r.get::<_, String>(0), r.get(1), r.get(2), r.get(3), &checks)
    }))
}

pub fn domain_ddl(schema: &str, name: &str, base: &str, not_null: bool, default: Option<String>, collation: Option<String>, checks: &[String]) -> String {
    let mut s = format!("CREATE DOMAIN {} AS {base}", qualified_name(Quote::Double, Some(schema), name));
    if let Some(c) = collation {
        s.push_str(&format!("\n    COLLATE {c}"));
    }
    if let Some(d) = default {
        s.push_str(&format!("\n    DEFAULT {d}"));
    }
    if not_null {
        s.push_str("\n    NOT NULL");
    }
    for c in checks {
        match c.split_once('\t') {
            Some((n, def)) => s.push_str(&format!("\n    CONSTRAINT {} {def}", qi(n))),
            None => s.push_str(&format!("\n    {c}")),
        }
    }
    s.push(';');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_with_everything() {
        let t = TableSchema { schema: Some("s".into()), name: "t".into(), ..Default::default() };
        let mut ix = IndexDef { name: "ix".into(), unique: true, filter: Some("(b > 0)".into()), include: vec!["c".into()], ..Default::default() };
        let cols = vec!["a".to_string(), "(lower(b))".to_string(), "d".to_string()];
        index_options(&mut ix, &cols, &[3, 0, 1], true);
        ix.columns = cols;
        assert_eq!(ix.options.get(OPT_DESC).map(String::as_str), Some("a, d"));
        assert_eq!(ix.options.get(OPT_NULLS_LAST).map(String::as_str), Some("d"));
        assert!(!ix.options.contains_key(OPT_NULLS_FIRST));
        assert_eq!(
            index_ddl(&t, &ix, false),
            "CREATE UNIQUE INDEX ASYNC \"ix\" ON \"s\".\"t\" (\"a\" DESC, (lower(b)), \"d\" DESC NULLS LAST) INCLUDE (\"c\") NULLS NOT DISTINCT WHERE (b > 0);"
        );
    }

    #[test]
    fn checks_are_added_not_valid_then_validated() {
        assert_eq!(check_expression("CHECK ((a > 0)) NOT VALID"), "((a > 0))");
        assert_eq!(check_expression("CHECK (a > 0)"), "(a > 0)");
        let mut s = SyncScript {
            statements: vec!["ALTER TABLE \"s\".\"t\" ADD CONSTRAINT \"ck\" CHECK (a > 0);".into(), "DROP INDEX \"x\";".into()],
            warnings: vec![],
        };
        fix_script(&mut s);
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"s\".\"t\" ADD CONSTRAINT \"ck\" CHECK (a > 0) NOT VALID;",
                "ALTER TABLE ASYNC \"s\".\"t\" VALIDATE CONSTRAINT \"ck\";",
                "DROP INDEX \"x\";",
            ]
        );
    }

    #[test]
    fn domain_statement() {
        assert_eq!(
            domain_ddl("public", "precio", "numeric(10,2)", true, Some("0".into()), None, &["precio_pos\tCHECK (VALUE >= 0::numeric)".into()]),
            "CREATE DOMAIN \"public\".\"precio\" AS numeric(10,2)\n    DEFAULT 0\n    NOT NULL\n    CONSTRAINT \"precio_pos\" CHECK (VALUE >= 0::numeric);"
        );
    }
}
