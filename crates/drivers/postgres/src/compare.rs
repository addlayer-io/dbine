//! What "Comparar esquemas" reads beyond tables: sequences, user-defined
//! types (enums, domains, composites, ranges) and synonyms as runnable
//! CREATE statements, plus the pieces of an index that `pg_get_indexdef`
//! knows and the per-column catalog doesn't (sort order, operator classes,
//! collations), and the sync fixes for indexes that back a constraint.

use crate::catalog::{cell, lit};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{kinds, CheckDef, DbObject, IndexDef, ObjectKindInfo, ObjectRef, Result, TableChange};

/// The object kinds read here.
pub(crate) const KINDS: &[&str] = &[kinds::SEQUENCE, kinds::TYPE, kinds::SYNONYM, DOMAIN];

/// H2's domains: its only user types, dropped with `DROP DOMAIN`.
pub(crate) const DOMAIN: &str = "domain";

/// Index options that aren't storage parameters (`WITH (…)`):
/// - `keys`: the key list as the server writes it, when a column has a
///   sort order, operator class or collation of its own;
/// - `constraint`: the index backs a UNIQUE or EXCLUDE constraint, whose
///   definition this is (made with `ALTER TABLE … ADD CONSTRAINT`);
/// - `nulls_not_distinct`: `NULLS NOT DISTINCT` (PostgreSQL 15+).
pub(crate) const KEYS: &str = "keys";
pub(crate) const CONSTRAINT: &str = "constraint";
pub(crate) const NULLS_NOT_DISTINCT: &str = "nulls_not_distinct";
/// RisingWave: `DISTRIBUTED BY (…)` of an index.
pub(crate) const DISTRIBUTED_BY: &str = "distributed_by";
const STRUCTURAL: &[&str] = &[KEYS, CONSTRAINT, NULLS_NOT_DISTINCT, DISTRIBUTED_BY, COLUMN_INDEX, "analyzer"];

/// An exclusion constraint's index kind.
pub(crate) const EXCLUDE: &str = "EXCLUDE";

pub(crate) fn has_sequences(v: Variant) -> bool {
    !matches!(v, Variant::Redshift | Variant::Denodo | Variant::CrateDb | Variant::RisingWave | Variant::Materialize)
}

pub(crate) fn has_types(v: Variant) -> bool {
    (has_sequences(v) && !matches!(v, Variant::Yellowbrick | Variant::H2)) || v == Variant::Materialize
}

/// Oracle-compatible synonyms (`pg_synonym`).
pub(crate) fn has_synonyms(v: Variant) -> bool {
    matches!(v, Variant::OpenGauss | Variant::Edb | Variant::Kingbase)
}

/// The compare-only kinds a variant lists.
pub(crate) fn object_kinds(v: Variant) -> Vec<ObjectKindInfo> {
    let mut out = Vec::new();
    if has_sequences(v) {
        out.push(ObjectKindInfo::sequences());
    }
    if has_synonyms(v) {
        out.push(ObjectKindInfo::synonyms());
    }
    if has_types(v) {
        out.push(ObjectKindInfo::types());
    }
    if v == Variant::H2 {
        out.push(ObjectKindInfo::new(DOMAIN, "Dominios", false, false, true));
    }
    out
}

// ------------------------------------------------------------ index keys

/// The first parenthesized list after ` USING ` in a `pg_get_indexdef`
/// (the key columns, without INCLUDE).
pub(crate) fn key_list(def: &str) -> Option<String> {
    let at = def.find(" USING ")?;
    let open = at + def[at..].find('(')?;
    let mut depth = 0;
    let mut quote: Option<char> = None;
    for (i, ch) in def[open..].char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(def[open + 1..open + i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// `list` split at its top-level commas.
pub(crate) fn split_top(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0, None::<char>, 0);
    for (i, ch) in list.char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, ',') if depth == 0 => {
                out.push(list[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    if !list[start..].trim().is_empty() {
        out.push(list[start..].trim().to_string());
    }
    out
}

/// The key list, when some key carries more than its column or expression
/// (`DESC`, `NULLS FIRST`, an operator class, `COLLATE`). `exprs` are the
/// keys alone, as `pg_get_indexdef(oid, n, true)` gives them. CockroachDB
/// writes `ASC` on every key, which is the default.
pub(crate) fn decorated_keys(def: &str, exprs: &[String]) -> Option<String> {
    let list = key_list(def)?;
    let parts = split_top(&list);
    if parts.len() != exprs.len() {
        return None;
    }
    // The whole definition adds parentheses the key alone doesn't have.
    let bare = |s: &str| s.chars().filter(|c| !matches!(c, '(' | ')' | ' ')).collect::<String>();
    let plain = |p: &str, e: &str| bare(p.strip_suffix(" ASC").unwrap_or(p)) == bare(e);
    parts.iter().zip(exprs).any(|(p, e)| !plain(p, e)).then_some(list)
}

/// `name=value` storage parameters from `pg_class.reloptions` (one per line).
pub(crate) fn reloptions(lines: &str) -> Vec<(String, String)> {
    lines.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}

/// A CockroachDB index access method by PostgreSQL's name: 26.x reports
/// its ordinary ordered index as `prefix` (a B-tree; older versions say
/// `btree`) and `inverted` is its GIN. Anything else stays as it is.
pub(crate) fn crdb_index_method(am: &str) -> &str {
    if am.eq_ignore_ascii_case("prefix") {
        "btree"
    } else if am.eq_ignore_ascii_case("inverted") {
        "gin"
    } else {
        am
    }
}

/// `CREATE INDEX` / `ALTER TABLE … ADD CONSTRAINT` for one index of `table`
/// (already qualified and quoted). `using` is the access method to name.
pub(crate) fn index_ddl(v: Variant, table: &str, ix: &IndexDef, using: Option<&str>, if_exists: bool) -> String {
    let q = |s: &str| quote_ident(Quote::Double, s);
    if let Some(c) = ix.options.get(CONSTRAINT).filter(|_| v != Variant::Cockroach) {
        let s = format!("ALTER TABLE {table} ADD CONSTRAINT {} {c};", q(&ix.name));
        return if if_exists && v.plpgsql() {
            format!("DO $$ BEGIN\n    {s}\nEXCEPTION WHEN duplicate_object OR duplicate_table THEN NULL;\nEND $$;")
        } else {
            s
        };
    }
    // H2 names its index kinds before INDEX.
    let h2_kind = match ix.kind.as_deref() {
        Some("SPATIAL") if v == Variant::H2 => "SPATIAL ",
        Some("HASH") if v == Variant::H2 => "HASH ",
        _ => "",
    };
    let mut s = format!(
        "CREATE {}{h2_kind}INDEX {}{} ON {table}",
        if ix.unique { "UNIQUE " } else { "" },
        if if_exists { "IF NOT EXISTS " } else { "" },
        q(&ix.name),
    );
    if let Some(u) = using {
        s.push_str(&format!(" USING {u}"));
    }
    let keys = match ix.options.get(KEYS) {
        Some(k) => k.clone(),
        None => ix.columns.iter().map(|c| index_column(c)).collect::<Vec<_>>().join(", "),
    };
    s.push_str(&format!(" ({keys})"));
    if !ix.include.is_empty() {
        let inc = ix.include.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
        s.push_str(&format!(" {} ({inc})", if v == Variant::Cockroach { "STORING" } else { "INCLUDE" }));
    }
    if ix.options.contains_key(NULLS_NOT_DISTINCT) {
        s.push_str(" NULLS NOT DISTINCT");
    }
    if let Some(d) = ix.options.get(DISTRIBUTED_BY) {
        s.push_str(&format!(" DISTRIBUTED BY ({d})"));
    }
    let with: Vec<String> = ix
        .options
        .iter()
        .filter(|(k, _)| !STRUCTURAL.contains(&k.as_str()))
        .map(|(k, val)| format!("{k}={}", dbine_driver::ddl::sql_literal(&dbine_driver::ddl::SqlFlavor::ansi(), &serde_json::Value::String(val.clone()))))
        .collect();
    if !with.is_empty() {
        s.push_str(&format!(" WITH ({})", with.join(", ")));
    }
    // H2 has no partial indexes.
    if let Some(w) = ix.filter.as_deref().filter(|w| !w.is_empty() && v != Variant::H2) {
        s.push_str(&format!(" WHERE {w}"));
    }
    s.push(';');
    s
}

/// An index column: a name, or an expression kept in parentheses.
pub(crate) fn index_column(c: &str) -> String {
    if c.starts_with('(') && c.ends_with(')') {
        c.to_string()
    } else {
        quote_ident(Quote::Double, c)
    }
}

/// The generic sync drops indexes with `DROP INDEX`: one that backs a
/// constraint is dropped with the constraint instead (CockroachDB: with
/// `CASCADE`, as its unique indexes are constraints too).
pub(crate) fn fix_drops(v: Variant, statements: &mut [String], changes: &[TableChange]) {
    for ch in changes {
        let TableChange::Alter { old, new } = ch else { continue };
        let schema = new.schema.as_deref().filter(|s| !s.is_empty());
        let table = qualified_name(Quote::Double, schema, &new.name);
        for ix in &old.indexes {
            let plain = format!("DROP INDEX {};", qualified_name(Quote::Double, schema, &ix.name));
            let with = if v == Variant::Cockroach {
                if !ix.unique {
                    continue;
                }
                format!("DROP INDEX {} CASCADE;", qualified_name(Quote::Double, schema, &ix.name))
            } else if ix.options.contains_key(CONSTRAINT) {
                format!("ALTER TABLE {table} DROP CONSTRAINT {};", quote_ident(Quote::Double, &ix.name))
            } else {
                continue;
            };
            for s in statements.iter_mut().filter(|s| **s == plain) {
                *s = with.clone();
            }
        }
    }
}

/// CockroachDB leaves no table without a primary key (it drops one only
/// with a new one added in the same transaction): a key that is just
/// dropped stays, and the script says so; a key that changes is dropped
/// and added in one statement.
pub(crate) fn keep_cockroach_keys(script: &mut dbine_driver::SyncScript, changes: &[TableChange]) {
    for ch in changes {
        let TableChange::Alter { old, new } = ch else { continue };
        let Some(key) = old.primary_key.as_ref().filter(|k| !k.columns.is_empty()).and_then(|k| k.name.as_deref()).filter(|n| !n.is_empty()) else {
            continue;
        };
        let schema = new.schema.as_deref().filter(|s| !s.is_empty());
        let table = qualified_name(Quote::Double, schema, &new.name);
        let drop_clause = format!("DROP CONSTRAINT {}", quote_ident(Quote::Double, key));
        let drop = format!("ALTER TABLE {table} {drop_clause};");
        if new.primary_key.as_ref().is_some_and(|k| !k.columns.is_empty()) {
            let add = format!("ALTER TABLE {table} ADD ");
            let Some(at) = script.statements.iter().position(|s| s.starts_with(&add) && s.contains(" PRIMARY KEY (")) else { continue };
            if !script.statements.contains(&drop) {
                continue;
            }
            script.statements[at] = format!("ALTER TABLE {table} {drop_clause}, {}", &script.statements[at]["ALTER TABLE ".len() + table.len() + 1..]);
            script.statements.retain(|s| *s != drop);
            continue;
        }
        let before = script.statements.len();
        script.statements.retain(|s| *s != drop);
        if script.statements.len() < before {
            let shown = schema.map_or_else(|| new.name.clone(), |s| format!("{s}.{}", new.name));
            script.warnings.push(format!("CockroachDB no deja una tabla sin clave primaria: {shown} conserva la suya."));
        }
    }
}

/// A CHECK's condition from `pg_get_constraintdef` (`CHECK ((a > 0)) NOT
/// VALID` → `(a > 0)`): validation is not part of the structure, and `NO
/// INHERIT` has nowhere to go.
pub(crate) fn check_expression(def: &str) -> String {
    let mut e = def.trim().strip_prefix("CHECK ").unwrap_or(def.trim()).trim();
    while let Some(rest) = [" NOT VALID", " NO INHERIT"].iter().find_map(|s| e.strip_suffix(s)) {
        e = rest.trim_end();
    }
    e.to_string()
}

// ------------------------------------------------------------- sequences

/// A sequence as the catalog describes it.
#[derive(Debug, Default)]
pub(crate) struct Sequence {
    pub schema: String,
    pub name: String,
    /// `AS` type (PostgreSQL 10+).
    pub type_name: Option<String>,
    pub start: String,
    pub increment: String,
    pub min: String,
    pub max: String,
    pub cache: Option<String>,
    pub cycle: bool,
    /// `schema.table.column` it's `OWNED BY` (not as a serial).
    pub owned_by: Option<(String, String, String)>,
}

impl Sequence {
    pub(crate) fn create(&self, v: Variant) -> String {
        let q = |s: &str| quote_ident(Quote::Double, s);
        let mut s = format!("CREATE SEQUENCE {}.{}", q(&self.schema), q(&self.name));
        if let Some(t) = &self.type_name {
            s.push_str(&format!(" AS {t}"));
        }
        s.push_str(&format!(
            " INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {}",
            self.increment, self.min, self.max, self.start
        ));
        if let Some(c) = &self.cache {
            s.push_str(&format!(" CACHE {c}"));
        }
        s.push_str(if self.cycle { " CYCLE;" } else { " NO CYCLE;" });
        if let Some((ts, tt, tc)) = &self.owned_by {
            let alter = format!("ALTER SEQUENCE {}.{} OWNED BY {}.{}.{}", q(&self.schema), q(&self.name), q(ts), q(tt), q(tc));
            // Sequences are made before the tables: the table may come later.
            if v.plpgsql() {
                s.push_str(&format!(
                    "\nDO $$ BEGIN\n    {alter};\nEXCEPTION WHEN undefined_table OR undefined_column THEN NULL;\nEND $$;"
                ));
            } else {
                s.push_str(&format!("\n{alter};"));
            }
        }
        s
    }
}

// ------------------------------------------------------------------ types

#[derive(Debug, Default)]
pub(crate) struct UserType {
    pub schema: String,
    pub name: String,
    /// `pg_type.typtype`: e, d, c, r.
    pub kind: String,
    pub labels: Vec<String>,
    /// Composite: `name type[ COLLATE c]` each.
    pub attributes: Vec<String>,
    /// Domain.
    pub base: Option<String>,
    pub collation: Option<String>,
    pub default: Option<String>,
    pub not_null: bool,
    /// Domain: `CONSTRAINT n CHECK (…)` each.
    pub checks: Vec<String>,
    /// Range: `SUBTYPE = …, …`.
    pub range: Option<String>,
}

impl UserType {
    pub(crate) fn create(&self, v: Variant) -> Option<String> {
        let name = format!("{}.{}", quote_ident(Quote::Double, &self.schema), quote_ident(Quote::Double, &self.name));
        let lit = |s: &str| crate::catalog::lit(v, s);
        Some(match self.kind.as_str() {
            "e" => format!("CREATE TYPE {name} AS ENUM ({});", self.labels.iter().map(|l| lit(l)).collect::<Vec<_>>().join(", ")),
            "c" => format!("CREATE TYPE {name} AS (\n    {}\n);", self.attributes.join(",\n    ")),
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
                for c in &self.checks {
                    s.push_str(&format!("\n    {c}"));
                }
                s.push(';');
                s
            }
            "r" => format!("CREATE TYPE {name} AS RANGE ({});", self.range.as_deref()?),
            _ => return None,
        })
    }
}

// --------------------------------------------------------------- session

fn obj(kind: &str, schema: Option<String>, name: Option<String>) -> Option<DbObject> {
    Some(DbObject { kind: kind.into(), schema, name: name?, parent: None })
}

impl PgSession {
    /// Sequences, types and synonyms of the user schemas.
    pub(crate) async fn compare_objects(&self) -> Vec<DbObject> {
        let v = self.variant;
        let mut out = Vec::new();
        if has_sequences(v) {
            match self.sequence_names().await {
                Ok(o) => out.extend(o),
                Err(e) => tracing::debug!("{v:?}: sequences unavailable: {e}"),
            }
        }
        if v == Variant::Materialize {
            // List, map and record types; user objects have `u` ids.
            let sql = "SELECT s.name AS sch, t.name AS name FROM mz_catalog.mz_types t
                       JOIN mz_catalog.mz_schemas s ON s.id = t.schema_id
                       JOIN mz_catalog.mz_databases d ON d.id = s.database_id
                       WHERE t.id LIKE 'u%' AND d.name = current_database()";
            match self.text(sql).await {
                Ok(rows) => out.extend(rows.iter().filter_map(|r| obj(kinds::TYPE, cell(r, "sch"), cell(r, "name")))),
                Err(e) => tracing::debug!("{v:?}: types unavailable: {e}"),
            }
        } else if has_types(v) {
            let sql = format!(
                "SELECT n.nspname AS sch, t.typname AS name FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
                 WHERE t.typtype IN ('e', 'd', 'c', 'r') AND {}
                   AND (t.typtype <> 'c' OR NOT EXISTS (SELECT 1 FROM pg_class rc WHERE rc.oid = t.typrelid AND rc.relkind <> 'c'))
                   AND NOT EXISTS (SELECT 1 FROM pg_depend e WHERE e.classid = 'pg_type'::regclass AND e.objid = t.oid AND e.deptype = 'e')",
                self.filter("n.nspname")
            );
            match self.text(&sql).await {
                Ok(rows) => out.extend(rows.iter().filter_map(|r| obj(kinds::TYPE, cell(r, "sch"), cell(r, "name")))),
                Err(e) => tracing::debug!("{v:?}: types unavailable: {e}"),
            }
        }
        if v == Variant::H2 {
            let sql = format!(
                "SELECT domain_schema AS sch, domain_name AS name FROM information_schema.domains WHERE {}",
                self.filter("domain_schema")
            );
            match self.text(&sql).await {
                Ok(rows) => out.extend(rows.iter().filter_map(|r| obj(DOMAIN, cell(r, "sch"), cell(r, "name")))),
                Err(e) => tracing::debug!("{v:?}: domains unavailable: {e}"),
            }
        }
        if has_synonyms(v) {
            match self.synonym_rows(None).await {
                Ok(rows) => out.extend(rows.into_iter().filter_map(|(s, n, _)| obj(kinds::SYNONYM, Some(s), Some(n)))),
                Err(e) => tracing::debug!("{v:?}: synonyms unavailable: {e}"),
            }
        }
        out
    }

    /// Free-standing sequences: not an identity's, not a serial's (the
    /// table writes those), not an extension's.
    async fn sequence_names(&self) -> Result<Vec<DbObject>> {
        let filter = self.filter("n.nspname");
        let pg = format!(
            "SELECT n.nspname AS sch, c.relname AS name FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind = 'S' AND {filter}
               AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid
                               AND d.deptype IN ('i', 'e'))
               AND NOT EXISTS (SELECT 1 FROM pg_depend d JOIN pg_attrdef ad ON ad.adrelid = d.refobjid AND ad.adnum = d.refobjsubid
                               WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'a'
                                 AND pg_get_expr(ad.adbin, ad.adrelid) LIKE 'nextval(%')"
        );
        // H2's pg_class is a thin emulation: its own catalog instead.
        let pg = if self.variant == Variant::H2 { Err(dbine_driver::Error::Unsupported("H2".into())) } else { self.text(&pg).await };
        let rows = match pg {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("{:?}: pg_class sequences unavailable, using information_schema: {e}", self.variant);
                self.text(&format!(
                    "SELECT sequence_schema AS sch, sequence_name AS name FROM information_schema.sequences WHERE {}",
                    self.filter("sequence_schema")
                ))
                .await?
            }
        };
        Ok(rows.iter().filter_map(|r| obj(kinds::SEQUENCE, cell(r, "sch"), cell(r, "name"))).collect())
    }

    /// `CREATE SEQUENCE` / `CREATE TYPE` / `CREATE DOMAIN` / `CREATE SYNONYM`.
    pub(crate) async fn compare_definition(&self, o: &ObjectRef) -> Result<Option<String>> {
        let schema = o.schema().unwrap_or("public");
        match o.kind.as_str() {
            kinds::SEQUENCE => Ok(self.sequence(schema, &o.name).await?.map(|s| s.create(self.variant))),
            kinds::TYPE if self.variant == Variant::Materialize => {
                let sql = format!("SHOW CREATE TYPE {}", qualified_name(Quote::Double, Some(schema), &o.name));
                // Names come qualified with this database: without it the
                // statement runs on the other side too.
                let db = &self.database;
                Ok(self.text(&sql).await?.first().and_then(|r| cell(r, "create_sql")).map(|d| {
                    let d = d.replace(&format!("{}.", quote_ident(Quote::Double, db)), "").replace(&format!("{db}."), "");
                    format!("{};", d.trim_end_matches(';'))
                }))
            }
            kinds::TYPE => Ok(self.user_type(schema, &o.name).await?.and_then(|t| t.create(self.variant))),
            DOMAIN => self.h2_domain(schema, &o.name).await,
            kinds::SYNONYM => Ok(self.synonym_rows(Some((schema, &o.name))).await?.into_iter().next().map(|(s, n, target)| {
                format!("CREATE SYNONYM {}.{} FOR {target};", quote_ident(Quote::Double, &s), quote_ident(Quote::Double, &n))
            })),
            _ => Ok(None),
        }
    }

    async fn sequence(&self, schema: &str, name: &str) -> Result<Option<Sequence>> {
        let v = self.variant;
        let (s, n) = (lit(v, schema), lit(v, name));
        let modern = self.version >= 100000 || v == Variant::Cockroach;
        let mut queries = Vec::new();
        if v == Variant::H2 {
            queries.push(format!(
                "SELECT data_type AS typ, start_value::varchar AS start, increment::varchar AS inc, minimum_value::varchar AS min,
                        maximum_value::varchar AS max, cache::varchar AS cache, (cycle_option = 'YES') AS cyc
                 FROM information_schema.sequences WHERE sequence_schema = {s} AND sequence_name = {n}"
            ));
        }
        if modern {
            queries.push(format!(
                "SELECT format_type(q.seqtypid, NULL) AS typ, q.seqstart::text AS start, q.seqincrement::text AS inc,
                        q.seqmin::text AS min, q.seqmax::text AS max, q.seqcache::text AS cache, q.seqcycle AS cyc
                 FROM pg_sequence q JOIN pg_class c ON c.oid = q.seqrelid JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = {s} AND c.relname = {n}"
            ));
        } else {
            // Before PostgreSQL 10 (Greenplum 6, openGauss) the sequence is
            // a one-row relation with its settings.
            queries.push(format!(
                "SELECT NULL AS typ, start_value::text AS start, increment_by::text AS inc, min_value::text AS min,
                        max_value::text AS max, cache_value::text AS cache, is_cycled AS cyc FROM {}",
                qualified_name(Quote::Double, Some(schema), name)
            ));
        }
        queries.push(format!(
            "SELECT NULL AS typ, start_value::text AS start, increment::text AS inc, minimum_value::text AS min,
                    maximum_value::text AS max, NULL AS cache, (cycle_option = 'YES') AS cyc
             FROM information_schema.sequences WHERE sequence_schema = {s} AND sequence_name = {n}"
        ));
        let mut rows = None;
        for q in &queries {
            match self.text(q).await {
                Ok(r) if !r.is_empty() => {
                    rows = Some(r);
                    break;
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("{v:?}: sequence settings unavailable: {e}"),
            }
        }
        let Some(rows) = rows else { return Ok(None) };
        let r = &rows[0];
        let flag = |c: &str| cell(r, c).is_some_and(|x| x == "t" || x == "true");
        let owner = format!(
            "SELECT tn.nspname AS osch, tc.relname AS otbl, ta.attname AS ocol FROM pg_depend d
               JOIN pg_class c ON c.oid = d.objid JOIN pg_namespace n ON n.oid = c.relnamespace
               JOIN pg_class tc ON tc.oid = d.refobjid JOIN pg_namespace tn ON tn.oid = tc.relnamespace
               JOIN pg_attribute ta ON ta.attrelid = d.refobjid AND ta.attnum = d.refobjsubid
              WHERE d.classid = 'pg_class'::regclass AND d.deptype = 'a' AND n.nspname = {s} AND c.relname = {n}"
        );
        let owned_by = match self.text(&owner).await {
            Ok(o) => o.first().and_then(|o| Some((cell(o, "osch")?, cell(o, "otbl")?, cell(o, "ocol")?))),
            Err(e) => {
                tracing::debug!("{v:?}: sequence owner unavailable: {e}");
                None
            }
        };
        Ok(Some(Sequence {
            schema: schema.to_string(),
            name: name.to_string(),
            type_name: if modern || v == Variant::H2 { cell(r, "typ") } else { None },
            start: cell(r, "start").unwrap_or_default(),
            increment: cell(r, "inc").unwrap_or_default(),
            min: cell(r, "min").unwrap_or_default(),
            max: cell(r, "max").unwrap_or_default(),
            cache: cell(r, "cache").filter(|c| !c.is_empty()),
            cycle: flag("cyc"),
            owned_by,
        }))
    }

    async fn user_type(&self, schema: &str, name: &str) -> Result<Option<UserType>> {
        let v = self.variant;
        let collation = |col: &str, base: &str| {
            format!(
                "CASE WHEN {col} <> 0 AND {col} <> {base} THEN
                   (SELECT quote_ident(cn.nspname) || '.' || quote_ident(co.collname) FROM pg_collation co
                    JOIN pg_namespace cn ON cn.oid = co.collnamespace WHERE co.oid = {col}) END"
            )
        };
        // Pieces are put together here: openGauss's `||` skips NULLs.
        let sql = format!(
            "SELECT t.typtype::text AS kind, format_type(t.typbasetype, t.typtypmod) AS base, t.typnotnull AS notnull,
                    t.typdefault AS def, {dcoll} AS coll, t.oid::text AS oid, t.typrelid::text AS rel
             FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
             LEFT JOIN pg_type bt ON bt.oid = t.typbasetype
             WHERE n.nspname = {s} AND t.typname = {n} AND t.typtype IN ('e', 'd', 'c', 'r')",
            dcoll = collation("t.typcollation", "COALESCE(bt.typcollation, 0)"),
            s = lit(v, schema),
            n = lit(v, name),
        );
        let rows = self.text(&sql).await?;
        let Some(r) = rows.first() else { return Ok(None) };
        let kind = cell(r, "kind").unwrap_or_default();
        let oid = cell(r, "oid").unwrap_or_default();
        let mut t = UserType {
            schema: schema.to_string(),
            name: name.to_string(),
            not_null: cell(r, "notnull").is_some_and(|x| x == "t" || x == "true"),
            ..Default::default()
        };
        match kind.as_str() {
            "e" => {
                let sql = format!("SELECT enumlabel AS l FROM pg_enum WHERE enumtypid = {oid} ORDER BY enumsortorder");
                t.labels = self.text(&sql).await?.iter().filter_map(|r| cell(r, "l")).collect();
            }
            "c" => {
                let rel = cell(r, "rel").unwrap_or_default();
                let sql = format!(
                    "SELECT quote_ident(a.attname) || ' ' || format_type(a.atttypid, a.atttypmod) AS a, {} AS coll
                     FROM pg_attribute a JOIN pg_type at ON at.oid = a.atttypid
                     WHERE a.attrelid = {rel} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
                    collation("a.attcollation", "at.typcollation")
                );
                t.attributes = self
                    .text(&sql)
                    .await?
                    .iter()
                    .filter_map(|r| {
                        let a = cell(r, "a")?;
                        Some(match cell(r, "coll").filter(|c| !c.is_empty()) {
                            Some(c) => format!("{a} COLLATE {c}"),
                            None => a,
                        })
                    })
                    .collect();
                if t.attributes.is_empty() {
                    // CockroachDB keeps a composite's fields out of pg_attribute.
                    let sql = format!(
                        "SELECT quote_ident(attribute_name) || ' ' || data_type AS a FROM information_schema.attributes
                         WHERE udt_schema = {} AND udt_name = {} ORDER BY ordinal_position",
                        lit(v, schema),
                        lit(v, name)
                    );
                    if let Ok(rows) = self.text(&sql).await {
                        t.attributes = rows.iter().filter_map(|r| cell(r, "a")).collect();
                    }
                }
            }
            "d" => {
                t.base = cell(r, "base");
                t.collation = cell(r, "coll");
                t.default = cell(r, "def");
                let sql = format!(
                    "SELECT 'CONSTRAINT ' || quote_ident(conname) || ' ' || pg_get_constraintdef(oid) AS c
                     FROM pg_constraint WHERE contypid = {oid} AND contype = 'c' ORDER BY conname"
                );
                if let Ok(rows) = self.text(&sql).await {
                    t.checks = rows.iter().filter_map(|r| cell(r, "c")).collect();
                }
            }
            "r" => {
                let sql = format!(
                    "SELECT format_type(r.rngsubtype, NULL) AS sub,
                            CASE WHEN NOT oc.opcdefault THEN quote_ident(ocn.nspname) || '.' || quote_ident(oc.opcname) END AS opc,
                            {} AS coll,
                            CASE WHEN r.rngcanonical <> 0 THEN r.rngcanonical::regproc::text END AS canon,
                            CASE WHEN r.rngsubdiff <> 0 THEN r.rngsubdiff::regproc::text END AS diff
                     FROM pg_range r JOIN pg_opclass oc ON oc.oid = r.rngsubopc JOIN pg_namespace ocn ON ocn.oid = oc.opcnamespace
                     WHERE r.rngtypid = {oid}",
                    collation("r.rngcollation", "(SELECT st.typcollation FROM pg_type st WHERE st.oid = r.rngsubtype)")
                );
                if let Some(r) = self.text(&sql).await?.first() {
                    let mut parts = vec![format!("SUBTYPE = {}", cell(r, "sub").unwrap_or_default())];
                    for (col, word) in [("opc", "SUBTYPE_OPCLASS"), ("coll", "COLLATION"), ("canon", "CANONICAL"), ("diff", "SUBTYPE_DIFF")] {
                        if let Some(x) = cell(r, col).filter(|x| !x.is_empty()) {
                            parts.push(format!("{word} = {x}"));
                        }
                    }
                    t.range = Some(parts.join(", "));
                }
            }
            _ => {}
        }
        t.kind = kind;
        Ok(Some(t))
    }

    /// (schema, name, target) of the synonyms, or of one.
    async fn synonym_rows(&self, one: Option<(&str, &str)>) -> Result<Vec<(String, String, String)>> {
        let v = self.variant;
        let only = match one {
            Some((s, n)) => format!(" AND n.nspname = {} AND y.synname = {}", lit(v, s), lit(v, n)),
            None => String::new(),
        };
        let link = if v == Variant::Edb { "COALESCE('@' || y.synlink, '')" } else { "''" };
        let query = |catalog: &str| {
            format!(
                "SELECT n.nspname AS sch, y.synname AS name,
                        CASE WHEN y.synobjschema IS NULL OR y.synobjschema = '' THEN quote_ident(y.synobjname)
                             ELSE quote_ident(y.synobjschema) || '.' || quote_ident(y.synobjname) END || {link} AS target
                 FROM {catalog} y JOIN pg_namespace n ON n.oid = y.synnamespace
                 WHERE {}{only} ORDER BY 1, 2",
                self.filter("n.nspname")
            )
        };
        let mut queries = vec![query("pg_synonym")];
        if v == Variant::Kingbase {
            queries.push(query("sys_synonym"));
        }
        let mut last = None;
        for q in queries {
            match self.text(&q).await {
                Ok(rows) => {
                    return Ok(rows
                        .iter()
                        .filter_map(|r| Some((cell(r, "sch")?, cell(r, "name")?, cell(r, "target")?)))
                        .collect())
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.expect("one query"))
    }
}

/// A name the engine made up for an unnamed constraint (H2's
/// `CONSTRAINT_1A`): it differs between databases, so it isn't compared.
pub(crate) fn generated_name(n: &str) -> bool {
    n.strip_prefix("CONSTRAINT_").is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
}

type TableKey = (String, String);

impl PgSession {
    /// H2's CHECK constraints from `information_schema` (NOT NULL checks
    /// left out).
    pub(crate) async fn info_schema_checks(&self) -> Vec<(TableKey, CheckDef)> {
        let sql = format!(
            "SELECT tc.table_schema AS sch, tc.table_name AS tbl, tc.constraint_name AS con, cc.check_clause AS expr
             FROM information_schema.table_constraints tc
             JOIN information_schema.check_constraints cc
               ON cc.constraint_schema = tc.constraint_schema AND cc.constraint_name = tc.constraint_name
             WHERE tc.constraint_type = 'CHECK' AND {}
             ORDER BY 1, 2, 3",
            self.filter("tc.table_schema")
        );
        match self.text(&sql).await {
            Ok(rows) => rows
                .iter()
                .filter_map(|r| {
                    let expr = cell(r, "expr")?;
                    if expr.to_ascii_uppercase().ends_with(" IS NOT NULL") {
                        return None;
                    }
                    let name = cell(r, "con").filter(|n| !generated_name(n));
                    Some(((cell(r, "sch")?, cell(r, "tbl")?), CheckDef { name, expression: expr }))
                })
                .collect(),
            Err(e) => {
                tracing::debug!("{:?}: checks unavailable: {e}", self.variant);
                Vec::new()
            }
        }
    }

    /// H2's indexes (plain, unique, hash, spatial; sort order) and UNIQUE
    /// constraints.
    pub(crate) async fn h2_indexes(&self) -> Vec<(TableKey, IndexDef)> {
        let v = self.variant;
        let mut out: Vec<(TableKey, IndexDef)> = Vec::new();
        let sql = format!(
            "SELECT i.table_schema AS sch, i.table_name AS tbl, i.index_name AS idx, i.index_type_name AS typ,
                    c.column_name AS col, c.ordering_specification AS ord
             FROM information_schema.indexes i
             JOIN information_schema.index_columns c
               ON c.index_schema = i.index_schema AND c.index_name = i.index_name AND c.table_name = i.table_name
             WHERE i.is_generated = FALSE AND {}
             ORDER BY 1, 2, 3, c.ordinal_position",
            self.filter("i.table_schema")
        );
        match self.text(&sql).await {
            Ok(rows) => {
                let mut desc: Vec<bool> = Vec::new();
                for r in rows {
                    let (Some(sch), Some(tbl), Some(name), Some(col)) = (cell(&r, "sch"), cell(&r, "tbl"), cell(&r, "idx"), cell(&r, "col")) else {
                        continue;
                    };
                    if out.last().is_none_or(|(_, ix)| ix.name != name) {
                        close_h2(out.last_mut().map(|(_, ix)| ix), &std::mem::take(&mut desc));
                        let typ = cell(&r, "typ").unwrap_or_default().to_ascii_uppercase();
                        let kind = if typ.contains("SPATIAL") {
                            Some("SPATIAL".to_string())
                        } else if typ.contains("HASH") {
                            Some("HASH".to_string())
                        } else {
                            None
                        };
                        out.push(((sch, tbl), IndexDef { name, unique: typ.contains("UNIQUE"), kind, ..Default::default() }));
                    }
                    desc.push(cell(&r, "ord").is_some_and(|o| o.eq_ignore_ascii_case("DESC")));
                    out.last_mut().expect("pushed").1.columns.push(col);
                }
                close_h2(out.last_mut().map(|(_, ix)| ix), &desc);
            }
            Err(e) => tracing::debug!("{v:?}: indexes unavailable: {e}"),
        }
        let sql = format!(
            "SELECT tc.table_schema AS sch, tc.table_name AS tbl, tc.constraint_name AS con, k.column_name AS col
             FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage k
               ON k.constraint_schema = tc.constraint_schema AND k.constraint_name = tc.constraint_name
             WHERE tc.constraint_type = 'UNIQUE' AND {}
             ORDER BY 1, 2, 3, k.ordinal_position",
            self.filter("tc.table_schema")
        );
        match self.text(&sql).await {
            Ok(rows) => {
                let mut uniques: Vec<(TableKey, IndexDef)> = Vec::new();
                for r in rows {
                    let (Some(sch), Some(tbl), Some(name), Some(col)) = (cell(&r, "sch"), cell(&r, "tbl"), cell(&r, "con"), cell(&r, "col")) else {
                        continue;
                    };
                    if uniques.last().is_none_or(|(_, ix)| ix.name != name) {
                        uniques.push(((sch, tbl), IndexDef { name, unique: true, ..Default::default() }));
                    }
                    uniques.last_mut().expect("pushed").1.columns.push(col);
                }
                for (_, ix) in &mut uniques {
                    let cols = ix.columns.iter().map(|c| quote_ident(Quote::Double, c)).collect::<Vec<_>>().join(", ");
                    ix.options.insert(CONSTRAINT.into(), format!("UNIQUE ({cols})"));
                }
                out.extend(uniques);
            }
            Err(e) => tracing::debug!("{v:?}: unique constraints unavailable: {e}"),
        }
        out
    }

    /// `CREATE DOMAIN` from H2's `information_schema`.
    async fn h2_domain(&self, schema: &str, name: &str) -> Result<Option<String>> {
        let v = self.variant;
        let (s, n) = (lit(v, schema), lit(v, name));
        let sql = format!(
            "SELECT data_type AS typ, character_maximum_length AS len, numeric_precision AS prec, numeric_scale AS scale,
                    domain_default AS def
             FROM information_schema.domains WHERE domain_schema = {s} AND domain_name = {n}"
        );
        let rows = self.text(&sql).await?;
        let Some(r) = rows.first() else { return Ok(None) };
        let typ = cell(r, "typ").unwrap_or_default().to_ascii_lowercase();
        // Only sized types keep their size: an INTEGER's precision is implied.
        let (prec, scale) = if matches!(typ.as_str(), "numeric" | "decimal") { (cell(r, "prec"), cell(r, "scale")) } else { (None, None) };
        let mut out = format!(
            "CREATE DOMAIN {} AS {}",
            qualified_name(Quote::Double, Some(schema), name),
            crate::catalog::info_type(&typ, cell(r, "len").as_deref(), prec.as_deref(), scale.as_deref())
        );
        if let Some(d) = cell(r, "def").filter(|d| !d.is_empty()) {
            out.push_str(&format!(" DEFAULT {d}"));
        }
        let checks = format!(
            "SELECT dc.constraint_name AS con, cc.check_clause AS expr
             FROM information_schema.domain_constraints dc
             JOIN information_schema.check_constraints cc
               ON cc.constraint_schema = dc.constraint_schema AND cc.constraint_name = dc.constraint_name
             WHERE dc.domain_schema = {s} AND dc.domain_name = {n}
             ORDER BY 1"
        );
        for r in self.text(&checks).await.unwrap_or_default() {
            let Some(expr) = cell(&r, "expr") else { continue };
            match cell(&r, "con").filter(|c| !generated_name(c)) {
                Some(c) => out.push_str(&format!("\n    CONSTRAINT {} CHECK ({expr})", quote_ident(Quote::Double, &c))),
                None => out.push_str(&format!("\n    CHECK ({expr})")),
            }
        }
        out.push(';');
        Ok(Some(out))
    }
}

/// An H2 index's sort order: the key list with `DESC` where it applies.
fn close_h2(ix: Option<&mut IndexDef>, desc: &[bool]) {
    let Some(ix) = ix else { return };
    if desc.iter().any(|d| *d) {
        let keys = ix
            .columns
            .iter()
            .zip(desc)
            .map(|(c, d)| format!("{}{}", quote_ident(Quote::Double, c), if *d { " DESC" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ");
        ix.options.insert(KEYS.into(), keys);
    }
}

// ---------------------------------------------------------------- CrateDB

/// A CrateDB full-text index declared on its column (`"b" TEXT INDEX
/// USING FULLTEXT …`) or a column with `INDEX OFF`, as opposed to a named
/// `INDEX n USING FULLTEXT (…)` of the table.
pub(crate) const COLUMN_INDEX: &str = "column";
/// CrateDB's index kinds.
pub(crate) const FULLTEXT: &str = "FULLTEXT";
pub(crate) const INDEX_OFF: &str = "OFF";

/// The text inside the parentheses that open at or after `from`.
fn paren_body(s: &str, from: usize) -> Option<&str> {
    let open = from + s[from..].find('(')?;
    let (mut depth, mut quote) = (0, None::<char>);
    for (i, ch) in s[open..].char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    match s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        Some(x) => x.replace("\"\"", "\""),
        None => s.to_string(),
    }
}

/// `analyzer = 'english'` from a `WITH (…)`; the default (`standard`) is left out.
fn crate_analyzer(element: &str) -> Option<String> {
    let at = element.find(" WITH ")?;
    let body = paren_body(element, at)?;
    let (_, v) = body.split_once('=')?;
    let v = v.trim().trim_matches('\'').to_string();
    (v != "standard").then_some(v)
}

/// A generated CHECK name (`doc_t_a_check_176a0ea1f859`): it carries a hash
/// that differs between tables, so it isn't compared.
fn crate_generated_check(n: &str) -> bool {
    n.rsplit_once("_check_").is_some_and(|(_, h)| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Full-text indexes, `INDEX OFF` columns and CHECKs from CrateDB's `SHOW
/// CREATE TABLE`, the one place that has all of them.
pub(crate) fn parse_crate_table(ddl: &str) -> (Vec<IndexDef>, Vec<CheckDef>) {
    let (mut indexes, mut checks) = (Vec::new(), Vec::new());
    let Some(body) = ddl.find("CREATE TABLE").and_then(|at| paren_body(ddl, at)) else { return (indexes, checks) };
    for el in split_top(body) {
        let el = el.trim();
        if let Some(rest) = el.strip_prefix("INDEX ") {
            // INDEX "name" USING FULLTEXT ("a", "b") [WITH (analyzer = '…')]
            let Some((name, rest)) = rest.split_once(" USING ") else { continue };
            let Some(using_at) = el.find(" USING ") else { continue };
            let cols = paren_body(el, using_at).map(split_top).unwrap_or_default();
            let mut ix = IndexDef {
                name: unquote(name),
                columns: cols.iter().map(|c| unquote(c)).collect(),
                kind: Some(rest.split_whitespace().next().unwrap_or(FULLTEXT).to_ascii_uppercase()),
                ..Default::default()
            };
            if let Some(a) = crate_analyzer(el) {
                ix.options.insert("analyzer".into(), a);
            }
            indexes.push(ix);
        } else if let Some(rest) = el.strip_prefix("CONSTRAINT ") {
            let Some(at) = rest.find(" CHECK") else { continue };
            let name = unquote(&rest[..at]);
            let expr = rest[at + " CHECK".len()..].trim().to_string();
            checks.push(CheckDef { name: (!crate_generated_check(&name)).then_some(name), expression: expr });
        } else if let Some(quoted) = el.strip_prefix('"') {
            // A column: `"b" TEXT INDEX USING FULLTEXT WITH (…)` or `"c" TEXT INDEX OFF`.
            let Some(end) = quoted.find('"').map(|i| i + 1) else { continue };
            let col = unquote(&el[..=end]);
            let rest = &el[end + 1..];
            let kind = if rest.contains(" INDEX USING FULLTEXT") {
                FULLTEXT
            } else if rest.contains(" INDEX OFF") {
                INDEX_OFF
            } else {
                continue;
            };
            let mut ix = IndexDef { name: col.clone(), columns: vec![col], kind: Some(kind.into()), ..Default::default() };
            ix.options.insert(COLUMN_INDEX.into(), "on".into());
            if kind == FULLTEXT {
                if let Some(a) = crate_analyzer(rest) {
                    ix.options.insert("analyzer".into(), a);
                }
            }
            indexes.push(ix);
        }
    }
    (indexes, checks)
}

/// A CrateDB `CREATE TABLE` with its indexes: the table's `INDEX … USING
/// FULLTEXT` lines, and `INDEX USING FULLTEXT` / `INDEX OFF` on columns.
pub(crate) fn crate_create_indexes(create: &str, indexes: &[IndexDef]) -> String {
    let q = |s: &str| quote_ident(Quote::Double, s);
    let with = |ix: &IndexDef| ix.options.get("analyzer").map(|a| format!(" WITH (analyzer = {})", crate::catalog::lit(Variant::CrateDb, a))).unwrap_or_default();
    let mut out = create.to_string();
    let mut lines = Vec::new();
    for ix in indexes {
        let kind = ix.kind.as_deref().unwrap_or(FULLTEXT);
        if ix.options.contains_key(COLUMN_INDEX) {
            let Some(col) = ix.columns.first() else { continue };
            let clause = if kind == INDEX_OFF { " INDEX OFF".to_string() } else { format!(" INDEX USING FULLTEXT{}", with(ix)) };
            // The column line starts `    "col" <type>`: the clause goes after the type.
            let head = format!("\n    {} ", q(col));
            if let Some(at) = out.find(&head) {
                let line_end = out[at + 1..].find('\n').map_or(out.len(), |i| at + 1 + i);
                let line = &out[at + head.len()..line_end];
                let ty_end = [" NOT NULL", " NULL", " DEFAULT ", " GENERATED "].iter().filter_map(|m| line.find(m)).min().unwrap_or(line.trim_end_matches(',').len());
                out.insert_str(at + head.len() + ty_end, &clause);
            }
        } else {
            let cols = ix.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
            lines.push(format!("    INDEX {} USING {kind} ({cols}){}", q(&ix.name), with(ix)));
        }
    }
    if !lines.is_empty() {
        if let Some(at) = out.find("\n)") {
            out.insert_str(at, &format!(",\n{}", lines.join(",\n")));
        }
    }
    out
}

/// CrateDB can't add a CHECK or an index to a table that exists: those
/// statements come out, and the user is told the table must be made again.
pub(crate) fn crate_sync(script: &mut dbine_driver::SyncScript, changes: &[TableChange]) {
    let mut added_checks: Vec<String> = Vec::new();
    for ch in changes {
        let TableChange::Alter { old, new } = ch else { continue };
        let name = match new.schema.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => format!("{s}.{}", new.name),
            None => new.name.clone(),
        };
        let mut what = Vec::new();
        let (mut oi, mut ni) = (old.indexes.clone(), new.indexes.clone());
        oi.sort_by(|a, b| a.name.cmp(&b.name));
        ni.sort_by(|a, b| a.name.cmp(&b.name));
        if oi != ni {
            what.push("índices de texto completo");
        }
        let expr = |c: &CheckDef| dbine_driver::alter::check_expr(&c.expression);
        let missing: Vec<&CheckDef> = new.checks.iter().filter(|n| !old.checks.iter().any(|o| o.name == n.name && expr(o) == expr(n))).collect();
        if !missing.is_empty() {
            // The generic warning about the new CHECK failing no longer applies.
            let generic = format!("La restricción CHECK nueva de {name} ");
            script.warnings.retain(|w| !w.starts_with(&generic));
            what.push("restricciones CHECK");
            added_checks.extend(missing.iter().filter_map(|c| c.name.clone()));
        }
        if !what.is_empty() {
            script.warnings.push(format!(
                "{name}: CrateDB no agrega {} a una tabla que ya existe. Para aplicarlos hay que recrear la tabla (crearla de nuevo, copiar los datos y reemplazar la vieja); la sincronización no lo hace.",
                what.join(" ni ")
            ));
        }
    }
    let q = |s: &str| quote_ident(Quote::Double, s);
    script.statements.retain(|s| {
        let s = s.trim();
        !(s.starts_with("DROP INDEX ")
            || (s.starts_with("ALTER TABLE ") && s.contains(" ADD CONSTRAINT ") && s.contains(" CHECK "))
            // A changed CHECK stays as it was rather than go away.
            || added_checks.iter().any(|n| s.starts_with("ALTER TABLE ") && s.ends_with(&format!(" DROP CONSTRAINT {};", q(n)))))
    });
}

/// A RisingWave index from its `CREATE INDEX` (`pg_indexes.indexdef`):
/// keys (with DESC), INCLUDE and DISTRIBUTED BY. Without INCLUDE the index
/// carries every column, so the absence is kept as is.
pub(crate) fn parse_rw_index(name: &str, def: &str) -> Option<IndexDef> {
    let on = def.find(" ON ")?;
    let keys = paren_body(def, on)?;
    let after = on + def[on..].find(keys)? + keys.len() + 1;
    let tail = &def[after..];
    let mut ix = IndexDef { name: name.to_string(), unique: def.starts_with("CREATE UNIQUE"), ..Default::default() };
    let mut decorated = false;
    for k in split_top(keys) {
        let mut bare = k.as_str();
        for suffix in [" NULLS FIRST", " NULLS LAST", " DESC", " ASC"] {
            if let Some(b) = bare.strip_suffix(suffix) {
                bare = b;
                decorated |= suffix != " ASC";
            }
        }
        let ident = bare.starts_with('"') || bare.chars().all(|c| c.is_alphanumeric() || c == '_');
        ix.columns.push(if ident { unquote(bare) } else if bare.starts_with('(') && bare.ends_with(')') { bare.to_string() } else { format!("({bare})") });
    }
    if decorated {
        ix.options.insert(KEYS.into(), keys.to_string());
    }
    if let Some(at) = tail.find("INCLUDE") {
        ix.include = paren_body(tail, at).map(split_top).unwrap_or_default().iter().map(|c| unquote(c)).collect();
    }
    if let Some(at) = tail.find("DISTRIBUTED BY") {
        if let Some(d) = paren_body(tail, at) {
            ix.options.insert(DISTRIBUTED_BY.into(), d.to_string());
        }
    }
    Some(ix)
}

impl PgSession {
    /// RisingWave's indexes, from the statements that made them.
    pub(crate) async fn rw_indexes(&self) -> Vec<(TableKey, IndexDef)> {
        let sql = format!(
            "SELECT schemaname AS sch, tablename AS tbl, indexname AS idx, indexdef AS def FROM pg_indexes
             WHERE indexdef <> '' AND {} ORDER BY 1, 2, 3",
            self.filter("schemaname")
        );
        match self.text(&sql).await {
            Ok(rows) => rows
                .iter()
                .filter_map(|r| Some(((cell(r, "sch")?, cell(r, "tbl")?), parse_rw_index(&cell(r, "idx")?, &cell(r, "def")?)?)))
                .collect(),
            Err(e) => {
                tracing::debug!("risingwave: indexes unavailable: {e}");
                Vec::new()
            }
        }
    }
}

impl PgSession {
    /// CrateDB's full-text indexes, `INDEX OFF` columns and CHECKs, table by table.
    pub(crate) async fn crate_extras(&self, tables: &[TableKey]) -> Vec<(TableKey, Vec<IndexDef>, Vec<CheckDef>)> {
        let mut out = Vec::new();
        for (s, t) in tables {
            let sql = format!("SHOW CREATE TABLE {}", qualified_name(Quote::Double, Some(s), t));
            match self.text(&sql).await {
                Ok(rows) => {
                    if let Some(ddl) = rows.first().and_then(|r| r.get(0)) {
                        let (ix, ck) = parse_crate_table(ddl);
                        out.push(((s.clone(), t.clone()), ix, ck));
                    }
                }
                Err(e) => tracing::debug!("cratedb: SHOW CREATE TABLE {s}.{t} failed: {e}"),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::TableSchema;
    use std::collections::BTreeMap;

    #[test]
    fn key_lists_and_decorations() {
        let def = r#"CREATE INDEX ix1 ON public.t USING btree (a DESC NULLS LAST, lower(b) text_pattern_ops, ((a + id)), c COLLATE "C") INCLUDE (b) WITH (fillfactor='80')"#;
        let list = key_list(def).unwrap();
        assert_eq!(list, r#"a DESC NULLS LAST, lower(b) text_pattern_ops, ((a + id)), c COLLATE "C""#);
        assert_eq!(split_top(&list).len(), 4);
        let exprs = ["a", "lower(b)", "(a + id)", "c"].map(String::from);
        assert_eq!(decorated_keys(def, &exprs).as_deref(), Some(list.as_str()));
        let plain = "CREATE INDEX ix ON public.t USING btree (a, ((a + id)), lower(b))";
        assert_eq!(decorated_keys(plain, &["a", "(a + id)", "lower(b)"].map(String::from)), None);
        // CockroachDB's explicit ASC is the default.
        let crdb = "CREATE INDEX ix ON public.t USING btree (a ASC, b ASC) INCLUDE (c)";
        assert_eq!(decorated_keys(crdb, &["a", "b"].map(String::from)), None);
        assert!(decorated_keys("CREATE INDEX ix ON public.t USING btree (a DESC, b ASC)", &["a", "b"].map(String::from)).is_some());
        // A quoted comma or parenthesis doesn't split.
        assert_eq!(split_top(r#"f(a, ','), "x,)y""#), vec![r#"f(a, ',')"#.to_string(), r#""x,)y""#.to_string()]);
        assert_eq!(reloptions("fillfactor=70\nfastupdate=off"), vec![("fillfactor".into(), "70".into()), ("fastupdate".into(), "off".into())]);
    }

    fn ix(name: &str) -> IndexDef {
        IndexDef { name: name.into(), columns: vec!["a".into(), "(lower(b))".into()], kind: Some("btree".into()), ..Default::default() }
    }

    #[test]
    fn index_statements() {
        let t = "\"s\".\"t\"";
        let mut i = ix("ix");
        i.include = vec!["c".into()];
        i.unique = true;
        i.filter = Some("(a > 0)".into());
        i.options = BTreeMap::from([("fillfactor".into(), "80".into()), (NULLS_NOT_DISTINCT.into(), "on".into())]);
        assert_eq!(
            index_ddl(Variant::Postgres, t, &i, None, false),
            "CREATE UNIQUE INDEX \"ix\" ON \"s\".\"t\" (\"a\", (lower(b))) INCLUDE (\"c\") NULLS NOT DISTINCT WITH (fillfactor='80') WHERE (a > 0);"
        );
        assert!(index_ddl(Variant::Cockroach, t, &i, None, false).contains(" STORING (\"c\")"));
        let mut k = ix("ix2");
        k.options.insert(KEYS.into(), "a DESC NULLS LAST, lower(b) text_pattern_ops".into());
        assert_eq!(
            index_ddl(Variant::Postgres, t, &k, Some("hash"), true),
            "CREATE INDEX IF NOT EXISTS \"ix2\" ON \"s\".\"t\" USING hash (a DESC NULLS LAST, lower(b) text_pattern_ops);"
        );
        let mut u = ix("uq");
        u.options.insert(CONSTRAINT.into(), "UNIQUE NULLS NOT DISTINCT (a, b) INCLUDE (c) WITH (fillfactor=70)".into());
        assert_eq!(
            index_ddl(Variant::Postgres, t, &u, None, false),
            "ALTER TABLE \"s\".\"t\" ADD CONSTRAINT \"uq\" UNIQUE NULLS NOT DISTINCT (a, b) INCLUDE (c) WITH (fillfactor=70);"
        );
        assert!(index_ddl(Variant::Postgres, t, &u, None, true).starts_with("DO $$ BEGIN"));
    }

    #[test]
    fn constraint_indexes_drop_with_their_constraint() {
        let mut old = TableSchema { schema: Some("s".into()), name: "t".into(), ..Default::default() };
        let mut u = ix("uq");
        u.unique = true;
        u.options.insert(CONSTRAINT.into(), "UNIQUE (a)".into());
        old.indexes = vec![u, ix("ix")];
        let changes = [TableChange::Alter { old: old.clone(), new: TableSchema { indexes: vec![], ..old.clone() } }];
        let mut st = vec!["DROP INDEX \"s\".\"uq\";".to_string(), "DROP INDEX \"s\".\"ix\";".to_string()];
        fix_drops(Variant::Postgres, &mut st, &changes);
        assert_eq!(st, ["ALTER TABLE \"s\".\"t\" DROP CONSTRAINT \"uq\";", "DROP INDEX \"s\".\"ix\";"]);
        let mut st = vec!["DROP INDEX \"s\".\"uq\";".to_string(), "DROP INDEX \"s\".\"ix\";".to_string()];
        fix_drops(Variant::Cockroach, &mut st, &changes);
        assert_eq!(st, ["DROP INDEX \"s\".\"uq\" CASCADE;", "DROP INDEX \"s\".\"ix\";"]);
    }

    #[test]
    fn cockroach_keeps_a_dropped_primary_key() {
        let key = dbine_driver::KeyDef { name: Some("t_pk".into()), columns: vec!["id".into()] };
        let old = TableSchema { schema: Some("s".into()), name: "t".into(), primary_key: Some(key), ..Default::default() };
        let changes = [TableChange::Alter { old: old.clone(), new: TableSchema { primary_key: None, ..old.clone() } }];
        let mut script = dbine_driver::SyncScript {
            statements: vec!["ALTER TABLE \"s\".\"t\" DROP CONSTRAINT \"t_pk\";".into(), "DROP INDEX \"s\".\"ix\";".into()],
            ..Default::default()
        };
        keep_cockroach_keys(&mut script, &changes);
        assert_eq!(script.statements, ["DROP INDEX \"s\".\"ix\";"]);
        assert_eq!(script.warnings, ["CockroachDB no deja una tabla sin clave primaria: s.t conserva la suya."]);
        // A key that changes is dropped and added in one statement.
        let other = dbine_driver::KeyDef { name: Some("t_pk2".into()), columns: vec!["a".into()] };
        let changes = [TableChange::Alter { old: old.clone(), new: TableSchema { primary_key: Some(other), ..old }}];
        let mut script = dbine_driver::SyncScript {
            statements: vec![
                "ALTER TABLE \"s\".\"t\" DROP CONSTRAINT \"t_pk\";".into(),
                "ALTER TABLE \"s\".\"t\" ADD COLUMN \"a\" int8 NOT NULL;".into(),
                "ALTER TABLE \"s\".\"t\" ADD CONSTRAINT \"t_pk2\" PRIMARY KEY (\"a\");".into(),
            ],
            ..Default::default()
        };
        keep_cockroach_keys(&mut script, &changes);
        assert_eq!(
            script.statements,
            ["ALTER TABLE \"s\".\"t\" ADD COLUMN \"a\" int8 NOT NULL;", "ALTER TABLE \"s\".\"t\" DROP CONSTRAINT \"t_pk\", ADD CONSTRAINT \"t_pk2\" PRIMARY KEY (\"a\");"]
        );
        assert!(script.warnings.is_empty());
    }

    #[test]
    fn checks_lose_validation_and_no_inherit() {
        assert_eq!(check_expression("CHECK ((a > 0))"), "((a > 0))");
        assert_eq!(check_expression("CHECK ((b <> ''::text)) NO INHERIT"), "((b <> ''::text))");
        assert_eq!(check_expression("CHECK ((a > 0)) NOT VALID"), "((a > 0))");
    }

    #[test]
    fn sequences_and_types() {
        let s = Sequence {
            schema: "app".into(),
            name: "folio".into(),
            type_name: Some("integer".into()),
            start: "100".into(),
            increment: "5".into(),
            min: "1".into(),
            max: "2147483647".into(),
            cache: Some("10".into()),
            cycle: true,
            owned_by: Some(("app".into(), "t".into(), "c".into())),
        };
        let d = s.create(Variant::Postgres);
        assert!(d.starts_with("CREATE SEQUENCE \"app\".\"folio\" AS integer INCREMENT BY 5 MINVALUE 1 MAXVALUE 2147483647 START WITH 100 CACHE 10 CYCLE;"), "{d}");
        assert!(d.contains("ALTER SEQUENCE \"app\".\"folio\" OWNED BY \"app\".\"t\".\"c\";"));
        let e = UserType { schema: "app".into(), name: "mood".into(), kind: "e".into(), labels: vec!["a".into(), "it's".into()], ..Default::default() };
        assert_eq!(e.create(Variant::Postgres).unwrap(), "CREATE TYPE \"app\".\"mood\" AS ENUM (E'a', E'it''s');");
        // A backslash before the quote can't end the label early.
        let x = UserType { schema: "app".into(), name: "x".into(), kind: "e".into(), labels: vec!["x\\'; drop table t; --".into()], ..Default::default() };
        assert_eq!(x.create(Variant::Postgres).unwrap(), "CREATE TYPE \"app\".\"x\" AS ENUM (E'x\\\\''; drop table t; --');");
        let d = UserType {
            schema: "app".into(),
            name: "pos".into(),
            kind: "d".into(),
            base: Some("integer".into()),
            default: Some("1".into()),
            not_null: true,
            checks: vec!["CONSTRAINT p CHECK ((VALUE > 0))".into()],
            ..Default::default()
        };
        assert_eq!(d.create(Variant::Postgres).unwrap(), "CREATE DOMAIN \"app\".\"pos\" AS integer DEFAULT 1 NOT NULL\n    CONSTRAINT p CHECK ((VALUE > 0));");
        let c = UserType { schema: "app".into(), name: "pt".into(), kind: "c".into(), attributes: vec!["x integer".into(), "y text".into()], ..Default::default() };
        assert_eq!(c.create(Variant::Postgres).unwrap(), "CREATE TYPE \"app\".\"pt\" AS (\n    x integer,\n    y text\n);");
        let r = UserType { schema: "app".into(), name: "fr".into(), kind: "r".into(), range: Some("SUBTYPE = double precision".into()), ..Default::default() };
        assert_eq!(r.create(Variant::Postgres).unwrap(), "CREATE TYPE \"app\".\"fr\" AS RANGE (SUBTYPE = double precision);");
    }

    const CRATE: &str = r#"CREATE TABLE IF NOT EXISTS "doc"."zz_t" (
   "id" INTEGER NOT NULL,
   "a" INTEGER,
   "b" TEXT INDEX USING FULLTEXT WITH (
      analyzer = 'english'
   ),
   "c" TEXT INDEX OFF,
   "d" TEXT,
   PRIMARY KEY ("id"),
   INDEX "ft2" USING FULLTEXT ("d") WITH (
      analyzer = 'standard'
   ),
   INDEX "ft" USING FULLTEXT ("b", "d") WITH (
      analyzer = 'english'
   ),
   CONSTRAINT doc_zz_t_a_check_176a0ea1f859 CHECK("a" > 0),
   CONSTRAINT zz_ck CHECK("id" < 1000)
)
CLUSTERED BY ("id") INTO 4 SHARDS
WITH (
   column_policy = 'strict',
   number_of_replicas = '0-1'
)"#;

    #[test]
    fn cratedb_indexes_and_checks() {
        let (ix, ck) = parse_crate_table(CRATE);
        let names: Vec<&str> = ix.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["b", "c", "ft2", "ft"]);
        assert_eq!((ix[0].kind.as_deref(), ix[0].options.get("analyzer").map(String::as_str)), (Some("FULLTEXT"), Some("english")));
        assert!(ix[0].options.contains_key(COLUMN_INDEX));
        assert_eq!(ix[1].kind.as_deref(), Some("OFF"));
        assert!(ix[2].options.is_empty());
        assert_eq!(ix[3].columns, ["b", "d"]);
        assert_eq!(ck, vec![CheckDef { name: None, expression: "(\"a\" > 0)".into() }, CheckDef { name: Some("zz_ck".into()), expression: "(\"id\" < 1000)".into() }]);

        let create = "CREATE TABLE \"doc\".\"t\" (\n    \"id\" integer NOT NULL,\n    \"b\" text NULL,\n    \"c\" text NULL,\n    \"d\" text NULL,\n    PRIMARY KEY (\"id\")\n) CLUSTERED INTO 4 SHARDS;";
        let s = crate_create_indexes(create, &ix);
        assert!(s.contains("\"b\" text INDEX USING FULLTEXT WITH (analyzer = 'english') NULL,"), "{s}");
        assert!(s.contains("\"c\" text INDEX OFF NULL,"), "{s}");
        assert!(s.contains("PRIMARY KEY (\"id\"),\n    INDEX \"ft2\" USING FULLTEXT (\"d\"),\n    INDEX \"ft\" USING FULLTEXT (\"b\", \"d\") WITH (analyzer = 'english')\n) CLUSTERED"), "{s}");
    }

    #[test]
    fn cratedb_sync_warns_instead_of_adding() {
        let (ix, ck) = parse_crate_table(CRATE);
        let new = TableSchema { schema: Some("doc".into()), name: "t".into(), indexes: ix, checks: ck, ..Default::default() };
        let old = TableSchema { indexes: vec![], checks: vec![CheckDef { name: Some("zz_ck".into()), expression: "\"id\" < 5".into() }], ..new.clone() };
        let changes = [TableChange::Alter { old, new }];
        let mut script = dbine_driver::SyncScript {
            statements: vec![
                "ALTER TABLE \"doc\".\"t\" DROP CONSTRAINT \"zz_ck\";".into(),
                "ALTER TABLE \"doc\".\"t\" ADD CONSTRAINT \"zz_ck\" CHECK (\"id\" < 1000);".into(),
                "DROP INDEX \"doc\".\"ft\";".into(),
            ],
            warnings: vec![],
        };
        crate_sync(&mut script, &changes);
        assert!(script.statements.is_empty(), "{:?}", script.statements);
        assert_eq!(script.warnings.len(), 1);
        assert!(script.warnings[0].contains("recrear la tabla") && script.warnings[0].contains("texto completo"));
    }

    #[test]
    fn risingwave_indexes() {
        let ix = parse_rw_index("zz_ix", "CREATE INDEX zz_ix ON zz_t(a DESC, b) INCLUDE(c)").unwrap();
        assert_eq!((ix.columns.as_slice(), ix.include.as_slice()), (&["a".to_string(), "b".to_string()][..], &["c".to_string()][..]));
        assert_eq!(ix.options.get(KEYS).map(String::as_str), Some("a DESC, b"));
        let d = parse_rw_index("zz_ix2", "CREATE INDEX zz_ix2 ON zz_t(lower(b)) DISTRIBUTED BY(lower(b))").unwrap();
        assert_eq!(d.columns, ["(lower(b))"]);
        assert_eq!(d.options.get(DISTRIBUTED_BY).map(String::as_str), Some("lower(b)"));
        assert!(d.include.is_empty());
        assert_eq!(
            index_ddl(Variant::RisingWave, "\"public\".\"zz_t\"", &ix, None, false),
            "CREATE INDEX \"zz_ix\" ON \"public\".\"zz_t\" (a DESC, b) INCLUDE (\"c\");"
        );
        assert_eq!(index_ddl(Variant::RisingWave, "t", &d, None, false), "CREATE INDEX \"zz_ix2\" ON t ((lower(b))) DISTRIBUTED BY (lower(b));");
    }

    #[test]
    fn cockroach_index_methods_by_postgres_name() {
        assert_eq!(crdb_index_method("prefix"), "btree");
        assert_eq!(crdb_index_method("PREFIX"), "btree");
        assert_eq!(crdb_index_method("inverted"), "gin");
        assert_eq!(crdb_index_method("btree"), "btree");
        assert_eq!(crdb_index_method("gin"), "gin");
        assert_eq!(crdb_index_method("hash"), "hash");
    }
}
