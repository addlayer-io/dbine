//! The clone on analytical engines (StarRocks, Doris, ClickHouse,
//! GreptimeDB) and on Trino, where the generic path (structure → `CREATE`
//! → rows through the client) would change the table without saying so.
//! `database_schema` doesn't read everything these tables are:
//! StarRocks and Doris have their key model and aggregations, partitions,
//! distribution and buckets, sort key, AUTO_INCREMENT and generated
//! columns. ClickHouse has MATERIALIZED / ALIAS / EPHEMERAL columns,
//! codecs, TTL, projections and settings. GreptimeDB has `PARTITION ON`.
//! The client also reshapes some values on the way: Trino's
//! `TIMESTAMP WITH TIME ZONE` comes back with its offset, not its zone name.
//!
//! - The structure is the engine's own copy of the definition. StarRocks,
//!   Doris and GreptimeDB use `CREATE TABLE new LIKE old`, and ClickHouse
//!   uses `CREATE TABLE new AS old`. StarRocks and Doris rollups are not
//!   copied by LIKE, so they are added after the rows, and the clone waits
//!   for their jobs to finish. StarRocks' synchronous materialized views
//!   aren't copied either: they are created again on the clone under a
//!   new name (theirs is unique in the database); Doris' are refused. The
//!   clone's `DESC … ALL` must then show every rollup and view the
//!   original has, field for field. Trino keeps the driver's `CREATE`.
//! - Tables whose data lives elsewhere or is shared are refused: ClickHouse
//!   Distributed, Buffer, Merge, Kafka, URL… and replicated tables;
//!   StarRocks / Doris external tables; GreptimeDB tables that aren't
//!   `mito`. For these, a clone would be a second door to the same data,
//!   and copying the rows would write into the original's.
//! - After the `CREATE`, the clone's definition must be the original's,
//!   name aside. Anything lost drops the clone.
//! - The rows are copied with one `INSERT … SELECT` inside the server, so
//!   values never pass through a client. Columns the engine computes
//!   (generated, ALIAS, EPHEMERAL) are left out; ClickHouse's MATERIALIZED
//!   columns are stored and may not compute the same again (`now()`,
//!   `rand()`), so they are copied as they are
//!   (`insert_allow_materialized_columns`).
//! - StarRocks' AUTO_INCREMENT is moved past the largest copied value (the
//!   engine doesn't move it on its own, and its next id would replace a
//!   copied row in a PRIMARY KEY table).

use super::{exec, quote_of, scalar, strings, CloneControl, CloneEvent, CloneOptions, ClonePlan, Ddl};
use dbine_driver::sql::{qualified_name, quote_ident};
use super::Rename;
use dbine_driver::{kinds, ColumnDef, Driver, DriverInfo, Error, ObjectRef, QueryOutcome, Result, Session, TableSchema};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// [`dbine_driver::ColumnDef::options`] key set on the columns the engine
/// computes when `database_schema` doesn't say so (StarRocks' generated
/// columns): never loaded.
pub(super) const COMPUTED: &str = "computed_by_engine";

fn olap(id: &str) -> bool {
    matches!(id, "starrocks" | "doris" | "velodb")
}

/// A column the engine computes but stores, whose value may not come out
/// the same if computed again (ClickHouse's MATERIALIZED: `now()`,
/// `rand()`…): copied as it is.
pub(super) fn stored_computed(info: &DriverInfo, c: &ColumnDef) -> bool {
    info.id == "clickhouse" && c.options.get("default_kind").is_some_and(|k| k.eq_ignore_ascii_case("MATERIALIZED"))
}

/// Engines whose clone is the engine's own copy of the definition
/// ([`native_create`]): a structure read from the columns alone loses
/// nothing (see [`prepare`]).
pub(super) fn copies_definition(info: &DriverInfo) -> bool {
    olap(info.id) || matches!(info.id, "greptimedb" | "clickhouse")
}

/// The catalog query that says whether `schema.name` exists, on engines
/// whose listing and `database_schema` cover only the session's database
/// (none selected: nothing). `None` elsewhere.
pub(super) fn exists_sql(info: &DriverInfo, schema: &str, name: &str) -> Option<String> {
    if info.id == "clickhouse" {
        Some(format!("SELECT count() FROM system.tables WHERE database = {} AND name = {}", lit(schema), lit(name)))
    } else if info.dialect == "mysql" {
        Some(format!("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}", lit(schema), lit(name)))
    } else {
        None
    }
}

/// Engines whose clone goes through this module.
pub(super) fn applies(driver: &dyn Driver) -> bool {
    let i = driver.info();
    olap(i.id) || matches!(i.id, "greptimedb" | "clickhouse") || i.dialect == "trino"
}

/// Why the engine would refuse `name` for a new table. In Spanish.
pub fn name_problem(info: &DriverInfo, name: &str) -> Option<String> {
    if info.id != "greptimedb" {
        return None;
    }
    // GreptimeDB: `[a-zA-Z_:-][a-zA-Z0-9_:\-.@#]*`, quoted or not.
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '_' | ':' | '-'));
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-' | '.' | '@' | '#'));
    (!(first_ok && rest_ok)).then(|| {
        format!(
            "GreptimeDB no admite el nombre «{name}»: solo letras sin tilde, números y _ : - . @ #, sin espacios, y no puede empezar con un número"
        )
    })
}

/// The engine's own copy of a table's definition, when it has one that
/// takes everything.
fn native_create(info: &DriverInfo, source: &str, clone: &str) -> Option<String> {
    if olap(info.id) || info.id == "greptimedb" {
        Some(format!("CREATE TABLE {clone} LIKE {source}"))
    } else if info.id == "clickhouse" {
        Some(format!("CREATE TABLE {clone} AS {source}"))
    } else {
        None
    }
}

fn is_rollup(ix: &dbine_driver::IndexDef) -> bool {
    ix.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("ROLLUP"))
}

/// Why the original can't be cloned on its engine, from its definition.
fn refusal(id: &str, body: &str) -> Option<String> {
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    if olap(id) {
        if !compact.contains("engine=olap") {
            return Some(
                "es una tabla externa: sus filas viven en otro sistema, y el clon apuntaría a las mismas (copiar las filas las escribiría ahí)".into(),
            );
        }
        return None;
    }
    match id {
        "greptimedb" => {
            // Its own line: `ENGINE=mito`.
            let engine = body.lines().find_map(|l| {
                let l = l.to_ascii_lowercase();
                let r = l.strip_prefix("engine")?.trim_start().strip_prefix('=')?.trim_start().to_string();
                Some(r.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
            });
            match engine.as_deref() {
                Some("mito") | None => None,
                Some(e) => Some(format!(
                    "la tabla usa el motor «{e}» de GreptimeDB (sus filas viven en una tabla física compartida o en archivos externos); solo se clonan tablas del motor mito"
                )),
            }
        }
        "clickhouse" => {
            // After the columns (a column's text could say ENGINE too).
            let at = [") ENGINE = ", "\nENGINE = ", "ENGINE = "].iter().find_map(|k| body.find(k).map(|i| i + k.len() - "ENGINE = ".len()))?;
            let rest = body[at..].trim_start_matches("ENGINE").trim_start().trim_start_matches('=').trim_start();
            let engine: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            if engine.starts_with("Replicated") || engine.starts_with("Shared") {
                return Some(format!(
                    "es una tabla replicada ({engine}): comparte su ruta en ClickHouse Keeper, así que el clon sería otra réplica de la misma tabla y lo que se escribiera en él aparecería en el original"
                ));
            }
            if engine == "Set" {
                return Some(
                    "el motor «Set» de ClickHouse guarda sus filas pero no deja leerlas (solo sirve del lado derecho de un IN), así que no se pueden copiar al clon"
                        .into(),
                );
            }
            let local = engine.ends_with("MergeTree") || matches!(engine.as_str(), "Memory" | "Log" | "TinyLog" | "StripeLog" | "EmbeddedRocksDB" | "Null" | "Join");
            (!local).then(|| {
                format!(
                    "el motor «{engine}» de ClickHouse no guarda las filas en la tabla (lee o escribe otras tablas, otro servidor o archivos): el clon apuntaría a los mismos datos y copiar las filas las escribiría ahí"
                )
            })
        }
        _ => None,
    }
}

/// The definition after the table's name (`CREATE TABLE db.name (…` →
/// `(…`), each line trimmed, blank lines left out: what the original and
/// its clone must share. `None` when the name isn't in the header.
pub(super) fn body_of(definition: &str, name: &str) -> Option<String> {
    let from = definition.to_ascii_uppercase().find("TABLE")?;
    let d = &definition[from..];
    let quoted = [format!("`{}`", name.replace('`', "``")), format!("\"{}\"", name.replace('"', "\"\""))];
    // (start, end) of each way the name may be printed; the first one wins
    // (a column may be called like the table).
    let mut found: Vec<(usize, usize)> = quoted.iter().filter_map(|q| d.find(q.as_str()).map(|i| (i, i + q.len()))).collect();
    let mut at = 0;
    while let Some(i) = d[at..].find(name) {
        let i = at + i;
        let before = d[..i].chars().next_back();
        let after = d[i + name.len()..].chars().next();
        if before.is_some_and(|c| c == '.' || c.is_whitespace()) && after.is_none_or(|c| c == '(' || c.is_whitespace()) {
            found.push((i, i + name.len()));
            break;
        }
        at = i + name.len();
    }
    let end = found.into_iter().min().map(|(_, e)| e);
    let rest = &d[end?..];
    Some(rest.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n"))
}

/// `location = '…'` / `external_location = '…'` (Trino's table
/// properties): the key and its value.
fn location(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once('=')?;
    let k = k.trim();
    matches!(k, "location" | "external_location").then(|| (k, v.trim().trim_end_matches(',').trim()))
}

/// Where the clone's definition differs from the original's, in Spanish.
/// Storage locations must differ (the same one would share the files) and
/// are otherwise left out.
pub(super) fn differences(original: &str, clone: &str) -> Option<String> {
    let lines = |s: &str| -> Vec<String> { s.lines().filter(|l| location(l).is_none()).map(|l| l.trim_end_matches(',').to_string()).collect() };
    for a in original.lines().filter_map(location) {
        if clone.lines().filter_map(location).any(|b| b == a) {
            return Some(format!(
                "el clon usaría los mismos archivos que el original ({} = {}): lo que se escribiera en uno aparecería en el otro",
                a.0, a.1
            ));
        }
    }
    let (a, b) = (lines(original), lines(clone));
    if a == b {
        return None;
    }
    let i = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    let show = |v: &[String]| v.get(i).map(|l| format!("«{l}»")).unwrap_or_else(|| "(nada)".into());
    Some(format!("el motor lo describe distinto: el original dice {} y el clon {}", show(&a), show(&b)))
}

/// Columns the engine fills or numbers by itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineColumns {
    /// Computed (generated, MATERIALIZED, ALIAS, EPHEMERAL): can't be
    /// written like the others.
    pub generated: Vec<String>,
    /// The `generated` ones stored with values that may not come out the
    /// same if computed again (ClickHouse's MATERIALIZED): the clone keeps
    /// the original's values.
    pub stored: Vec<String>,
    /// Numbered by the engine (AUTO_INCREMENT, identity…).
    pub auto_increment: Vec<String>,
}

/// StarRocks / Doris: the AUTO_INCREMENT columns in a `SHOW CREATE TABLE`
/// (the catalog's EXTRA doesn't say).
pub(super) fn auto_increment_columns(definition: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in definition.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix('`') else { continue };
        // The name, `` being an escaped backtick.
        let mut name = String::new();
        let mut chars = rest.char_indices().peekable();
        let mut after = None;
        while let Some((i, c)) = chars.next() {
            if c == '`' {
                if chars.peek().is_some_and(|(_, n)| *n == '`') {
                    chars.next();
                    name.push('`');
                    continue;
                }
                after = Some(&rest[i + 1..]);
                break;
            }
            name.push(c);
        }
        let Some(after) = after else { continue };
        let upper = after.to_ascii_uppercase();
        let head = upper.split(" COMMENT ").next().unwrap_or_default();
        if head.split_whitespace().any(|w| w == "AUTO_INCREMENT") {
            out.push(name);
        }
    }
    out
}

/// The columns of `t` its engine computes or numbers, including what
/// `database_schema` / `columns` leave out (ClickHouse's default kinds,
/// StarRocks and Doris generated and AUTO_INCREMENT columns).
pub async fn engine_columns(driver: &dyn Driver, s: &mut dyn Session, t: &TableSchema) -> Result<EngineColumns> {
    let info = driver.info();
    let mut out = EngineColumns {
        generated: t.columns.iter().filter(|c| super::generated(c, info.dialect)).map(|c| c.name.clone()).collect(),
        stored: t.columns.iter().filter(|c| stored_computed(info, c)).map(|c| c.name.clone()).collect(),
        auto_increment: t.columns.iter().filter(|c| c.auto_increment).map(|c| c.name.clone()).collect(),
    };
    if olap(info.id) {
        // The table's own database: the session's may be another one, or
        // none (`DATABASE()` is then NULL and nothing would be found).
        let db = t.schema.as_deref().filter(|d| !d.is_empty()).map(lit).unwrap_or_else(|| "DATABASE()".into());
        let sql = format!(
            "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {db} AND TABLE_NAME = {} \
             AND GENERATION_EXPRESSION IS NOT NULL AND GENERATION_EXPRESSION <> ''",
            lit(&t.name)
        );
        let rows = strings(s, &sql).await.map_err(|e| Error::Query(format!("columnas generadas: {e}")))?;
        for n in rows.into_iter().filter_map(|r| r.into_iter().next().flatten()) {
            if !out.generated.contains(&n) {
                out.generated.push(n);
            }
        }
        let obj = ObjectRef { kind: kinds::TABLE.into(), schema: t.schema.clone(), name: t.name.clone() };
        let def = s.definition(&obj).await?.unwrap_or_default();
        for n in auto_increment_columns(&def) {
            if !out.auto_increment.contains(&n) {
                out.auto_increment.push(n);
            }
        }
    }
    Ok(out)
}

/// Where `clone`'s definition differs from `original`'s, on the engines
/// whose clone is checked that way (`None` elsewhere, or when they match).
pub async fn definition_differences(driver: &dyn Driver, s: &mut dyn Session, original: &ObjectRef, clone: &ObjectRef) -> Result<Option<String>> {
    if !applies(driver) {
        return Ok(None);
    }
    let body = |def: Option<String>, name: &str| def.and_then(|d| body_of(&d, name)).unwrap_or_default();
    let a = s.definition(&ObjectRef { kind: kinds::TABLE.into(), ..original.clone() }).await?;
    let a = body(a, &original.name);
    let b = s.definition(&ObjectRef { kind: kinds::TABLE.into(), ..clone.clone() }).await?;
    let b = body(b, &clone.name);
    Ok(differences(&a, &b))
}

/// What [`prepare`] found, for the steps after the `CREATE`.
pub(super) struct Analytic {
    /// The original's definition, name aside.
    body: String,
    /// StarRocks / Doris: the rollups and synchronous materialized views.
    olap: Option<OlapIndexes>,
}

/// StarRocks / Doris: what the clone must have besides its base index.
struct OlapIndexes {
    /// The original: read again at the end.
    source: ObjectRef,
    /// The original's `DESC … ALL`: each index and its fields.
    shapes: BTreeMap<String, Vec<String>>,
    /// Rollups (created by `Ddl::indexes`): the same name in the clone.
    rollups: Vec<String>,
    /// Synchronous materialized views: (original's name, clone's name,
    /// the `CREATE` on the clone).
    views: Vec<(String, String, String)>,
}

/// One statement's rows with its column names, cells as text.
async fn named_rows(s: &mut dyn Session, sql: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100_000, &mut out).await?;
    if let Some(e) = out.error {
        return Err(Error::Query(e));
    }
    let Some(r) = out.results.iter().find(|r| !r.columns.is_empty()) else { return Ok(Default::default()) };
    let text = |v: &serde_json::Value| match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Ok((r.columns.iter().map(|c| c.name.clone()).collect(), r.rows.iter().map(|row| row.iter().map(text).collect()).collect()))
}

/// `DESC t ALL` rows as (index name, the rest of the row) into each index
/// and its fields (the index name comes only on its first field; a blank
/// row separates indexes).
fn index_shapes(rows: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut last: Option<String> = None;
    for (index, fields) in rows {
        if !index.is_empty() {
            last = Some(index.clone());
            out.entry(index.clone()).or_default();
        }
        if fields.trim().is_empty() {
            continue;
        }
        if let Some(l) = &last {
            out.entry(l.clone()).or_default().push(fields.clone());
        }
    }
    out
}

/// StarRocks / Doris rollups from `DESC t ALL` (its `cols` and `rows`):
/// (rollup, its columns). The base index (named like the table) and
/// synchronous materialized views (fields that aren't the table's
/// `columns`, like `mv_sum_b`) are left out, as the driver does.
fn desc_rollups(table: &str, columns: &[String], cols: &[String], rows: &[Vec<String>]) -> Vec<(String, Vec<String>)> {
    let col = |n: &str| cols.iter().position(|c| c.eq_ignore_ascii_case(n));
    let (Some(ix), Some(f)) = (col("IndexName"), col("Field")) else { return Vec::new() };
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for r in rows {
        let index = r.get(ix).map(String::as_str).unwrap_or_default();
        if !index.is_empty() {
            out.push((index.to_string(), Vec::new()));
        }
        if let (Some(last), Some(field)) = (out.last_mut(), r.get(f).filter(|v| !v.is_empty())) {
            last.1.push(field.clone());
        }
    }
    out.retain(|(n, c)| n != table && !c.is_empty() && c.iter().all(|x| columns.contains(x)));
    out
}

/// StarRocks / Doris: every index of `table` (quoted) and its fields.
async fn olap_shapes(s: &mut dyn Session, table: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let (cols, rows) = named_rows(s, &format!("DESC {table} ALL")).await.map_err(|e| Error::Query(format!("índices de {table}: {e}")))?;
    let at = cols
        .iter()
        .position(|c| c.eq_ignore_ascii_case("IndexName"))
        .ok_or_else(|| Error::State(format!("no se entiende lo que muestra DESC {table} ALL; no se clona")))?;
    let rows: Vec<(String, String)> = rows
        .into_iter()
        .map(|r| {
            let rest: Vec<&str> = r.iter().enumerate().filter(|(i, _)| *i != at).map(|(_, v)| v.as_str()).collect();
            (r.get(at).cloned().unwrap_or_default(), rest.join("\t"))
        })
        .collect();
    Ok(index_shapes(&rows))
}

/// A word, a quoted identifier or a qualified name (`db`.`t`) in a
/// statement, outside strings and comments.
#[derive(Debug)]
struct Tok {
    start: usize,
    end: usize,
    depth: i32,
    parts: Vec<String>,
    plain: bool,
}

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// The words and names of `sql` (MySQL quoting); `None` when a string,
/// comment or quoted name doesn't end.
fn tokens(sql: &str) -> Option<Vec<Tok>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut depth) = (0usize, 0i32);
    while i < b.len() {
        let c = b[i];
        if c == b'\'' || c == b'"' {
            i += 1;
            while i < b.len() && b[i] != c {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            if i >= b.len() {
                return None;
            }
            i += 1;
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') || c == b'#' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2 + sql[i + 2..].find("*/")? + 2;
        } else if c == b'(' {
            depth += 1;
            i += 1;
        } else if c == b')' {
            depth -= 1;
            i += 1;
        } else if c == b'`' || ident_byte(c) {
            let (start, mut parts, mut plain) = (i, Vec::new(), true);
            loop {
                if b.get(i) == Some(&b'`') {
                    plain = false;
                    let mut name = String::new();
                    i += 1;
                    loop {
                        let rel = sql[i..].find('`')?;
                        name.push_str(&sql[i..i + rel]);
                        i += rel + 1;
                        if b.get(i) == Some(&b'`') {
                            name.push('`');
                            i += 1;
                        } else {
                            break;
                        }
                    }
                    parts.push(name);
                } else {
                    let s = i;
                    while i < b.len() && ident_byte(b[i]) {
                        i += 1;
                    }
                    parts.push(sql[s..i].to_string());
                }
                if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(|n| *n == b'`' || ident_byte(*n)) {
                    i += 1;
                    continue;
                }
                break;
            }
            out.push(Tok { start, end: i, depth, parts, plain });
        } else {
            i += 1;
        }
    }
    Some(out)
}

/// A synchronous materialized view's `CREATE` (as the engine keeps it)
/// named `view` and built on `clone` instead of `table` (both already
/// quoted). `None` when the statement isn't understood: its one
/// top-level `FROM` must name `table` (in `schema`, when qualified).
fn retarget_view(def: &str, table: &str, schema: Option<&str>, view: &str, clone: &str) -> Option<String> {
    let t = tokens(def)?;
    let kw = |k: &Tok, w: &str| k.plain && k.parts.len() == 1 && k.parts[0].eq_ignore_ascii_case(w);
    let mut n = 0;
    for w in ["CREATE", "MATERIALIZED", "VIEW"] {
        if !kw(t.get(n)?, w) {
            return None;
        }
        n += 1;
    }
    if kw(t.get(n)?, "IF") {
        for w in ["IF", "NOT", "EXISTS"] {
            if !kw(t.get(n)?, w) {
                return None;
            }
            n += 1;
        }
    }
    let name = t.get(n)?;
    let froms: Vec<usize> = t.iter().enumerate().filter(|(_, k)| k.depth == 0 && kw(k, "FROM")).map(|(i, _)| i).collect();
    let [f] = froms[..] else { return None };
    let base = t.get(f + 1).filter(|b| b.depth == 0 && f > n)?;
    let ok = match base.parts.as_slice() {
        [n] => n == table,
        [s, n] => n == table && schema.is_none_or(|x| x == s),
        _ => false,
    };
    ok.then(|| format!("{}{view}{}{clone}{}", &def[..name.start], &def[name.end..base.start], &def[base.end..]))
}

/// The clone's name for the original's view `view`: the clone's suffix
/// after the view's name (`mv_t` of `t` cloned as `t_20260930_010203` →
/// `mv_t_20260930_010203`), or the clone's name after it.
fn view_name(view: &str, table: &str, clone: &str) -> String {
    match clone.strip_prefix(table).filter(|s| !s.is_empty()) {
        Some(suffix) => format!("{view}{suffix}"),
        None => format!("{view}_{clone}"),
    }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// StarRocks / Doris: the rollups and synchronous materialized views the
/// clone must get, with the views' `CREATE` for it.
#[allow(clippy::too_many_arguments)]
async fn olap_plan(
    info: &DriverInfo,
    src: &mut dyn Session,
    source: &ObjectRef,
    source_t: &TableSchema,
    clone: &TableSchema,
    options: &CloneOptions,
    renames: &mut Vec<Rename>,
    notes: &mut Vec<String>,
) -> Result<OlapIndexes> {
    let from = qname(info, source.schema(), &source.name);
    let to = qname(info, clone.schema.as_deref(), &clone.name);
    // A rollup, view or column change still being built isn't in
    // `DESC … ALL` (nor in the schema read) yet: the clone would miss it.
    let busy = busy_jobs(info, src, source.schema(), &source.name).await?;
    if !busy.is_empty() {
        return Err(Error::Unsupported(format!(
            "no se puede clonar: en la tabla «{}» se está construyendo {}; esperá a que termine y volvé a clonar",
            source.name,
            busy.join(", ")
        )));
    }
    let shapes = olap_shapes(src, &from).await?;
    let rollups: Vec<String> = source_t.indexes.iter().filter(|i| is_rollup(i)).map(|i| i.name.clone()).collect();
    let mvs: Vec<String> = shapes.keys().filter(|k| **k != source.name && !rollups.contains(k)).cloned().collect();
    let mut views = Vec::new();
    if !mvs.is_empty() && !options.with_indexes {
        notes.push(format!("sin índices: no se crean las vistas materializadas sincrónicas ({})", mvs.join(", ")));
    } else if !mvs.is_empty() && info.id != "starrocks" {
        return Err(Error::Unsupported(format!(
            "no se puede clonar: la tabla tiene vistas materializadas sincrónicas ({}) y en {} no se pueden recrear en el clon de forma comprobada; cloná sin índices para crear el clon sin ellas",
            mvs.join(", "),
            info.name
        )));
    } else {
        let db = source.schema().map(lit).unwrap_or_else(|| "DATABASE()".into());
        // Every view of the database (rollups too), names filtered here:
        // StarRocks 4 returns nothing when TABLE_NAME is in the WHERE.
        let all: Vec<(String, String)> =
            strings(src, &format!("SELECT TABLE_NAME, MATERIALIZED_VIEW_DEFINITION FROM information_schema.materialized_views WHERE TABLE_SCHEMA = {db}"))
                .await
                .map_err(|e| Error::Query(format!("vistas materializadas: {e}")))?
                .into_iter()
                .map(|r| {
                    let mut r = r.into_iter();
                    (r.next().flatten().unwrap_or_default(), r.next().flatten().unwrap_or_default())
                })
                .collect();
        for mv in &mvs {
            let def = all.iter().find(|(n, _)| n == mv).map(|(_, d)| d.clone()).unwrap_or_default();
            let new = view_name(mv, &source.name, &clone.name);
            if all.iter().any(|(n, _)| *n == new) {
                return Err(Error::State(format!(
                    "la vista materializada «{mv}» se recrearía en el clon como «{new}» y ese nombre ya existe en la base; elegí otro nombre"
                )));
            }
            let view_q = qname(info, clone.schema.as_deref(), &new);
            let create = retarget_view(&def, &source.name, source.schema(), &view_q, &to).ok_or_else(|| {
                Error::Unsupported(format!(
                    "no se puede clonar: no se entiende la definición de la vista materializada sincrónica «{mv}» para recrearla en el clon; cloná sin índices para crear el clon sin ella"
                ))
            })?;
            renames.push(Rename { from: mv.clone(), to: new.clone(), shortened: false });
            views.push((mv.clone(), new, create));
        }
    }
    let rollups = if options.with_indexes { rollups } else { Vec::new() };
    Ok(OlapIndexes { source: source.clone(), shapes, rollups, views })
}

/// `SHOW ALTER TABLE ROLLUP` (rollups and synchronous materialized views),
/// `SHOW ALTER TABLE COLUMN` or `SHOW ALTER TABLE OPTIMIZE` (StarRocks:
/// distribution, buckets, partitions) rows, by `kind`: what is still being
/// built on `table` (a job neither FINISHED nor CANCELLED), as it's told
/// to the user. `None` when the columns aren't the expected ones.
fn busy_in(cols: &[String], rows: &[Vec<String>], table: &str, kind: &str) -> Option<Vec<String>> {
    let col = |n: &str| cols.iter().position(|c| c.eq_ignore_ascii_case(n));
    let (t, st) = (col("TableName")?, col("State")?);
    let rollup = kind == "ROLLUP";
    let ix = if rollup { Some(col("RollupIndexName")?) } else { col("IndexName") };
    let mut out = Vec::new();
    for r in rows.iter().filter(|r| r.get(t).is_some_and(|n| n == table)) {
        let state = r.get(st).map(|s| s.to_ascii_uppercase()).unwrap_or_default();
        if state == "FINISHED" || state == "CANCELLED" {
            continue;
        }
        let what = match (rollup, ix.and_then(|i| r.get(i)).filter(|n| !n.is_empty())) {
            (true, Some(n)) => format!("el rollup o vista materializada «{n}»"),
            (true, None) => "un rollup o vista materializada".to_string(),
            (false, _) if kind == "OPTIMIZE" => "un cambio de distribución, buckets o particiones (OPTIMIZE)".to_string(),
            (false, _) => "un cambio de columnas".to_string(),
        };
        if !out.contains(&what) {
            out.push(what);
        }
    }
    Some(out)
}

/// StarRocks / Doris: the rollups, synchronous materialized views,
/// column changes and (StarRocks) OPTIMIZE jobs still being built on
/// `table` (see [`busy_in`]). A pending OPTIMIZE isn't in the definition
/// yet: `LIKE` would copy the old distribution.
async fn busy_jobs(info: &DriverInfo, s: &mut dyn Session, schema: Option<&str>, table: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let kinds: &[&str] = if info.id == "starrocks" { &["ROLLUP", "COLUMN", "OPTIMIZE"] } else { &["ROLLUP", "COLUMN"] };
    for &what in kinds {
        let sql = match schema.filter(|d| !d.is_empty()) {
            Some(db) => format!("SHOW ALTER TABLE {what} FROM {}", quote_ident(quote_of(info.dialect), db)),
            None => format!("SHOW ALTER TABLE {what}"),
        };
        let (cols, rows) = named_rows(s, &sql).await.map_err(|e| Error::Query(format!("trabajos en curso en «{table}»: {e}")))?;
        let busy = busy_in(&cols, &rows, table, what)
            .ok_or_else(|| Error::State(format!("no se entiende lo que muestra SHOW ALTER TABLE {what}; no se clona")))?;
        out.extend(busy);
    }
    Ok(out)
}

fn qname(info: &DriverInfo, schema: Option<&str>, name: &str) -> String {
    qualified_name(quote_of(info.dialect), schema.filter(|s| !s.is_empty()), name)
}

/// Before anything is written: the original's definition (refused when the
/// engine can't show it or the table's data lives elsewhere), the columns
/// the engine computes or numbers marked in both schemas, and, where the
/// engine copies its own definition, the `CREATE` replaced by it (indexes
/// left to StarRocks / Doris rollups, which it doesn't copy).
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare(
    driver: &dyn Driver,
    src: &mut dyn Session,
    source: &ObjectRef,
    source_t: &mut TableSchema,
    plan: &mut ClonePlan,
    options: &CloneOptions,
    ddl: &mut Ddl,
    notes: &mut Vec<String>,
) -> Result<Option<Analytic>> {
    if !applies(driver) {
        return Ok(None);
    }
    let info = driver.info();
    let obj = ObjectRef { kind: kinds::TABLE.into(), ..source.clone() };
    let definition = src
        .definition(&obj)
        .await
        .map_err(|e| Error::State(format!("no se pudo leer la definición de «{}» para comprobar el clon: {e}", source.name)))?
        .filter(|d| !d.trim().is_empty())
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "no se puede clonar: {} no muestra la definición de «{}», y sin ella no se puede comprobar que el clon quede igual",
                info.name, source.name
            ))
        })?;
    let body = body_of(&definition, &source.name)
        .ok_or_else(|| Error::State(format!("no se entiende la definición de «{}» que muestra {}; no se clona", source.name, info.name)))?;
    if let Some(r) = refusal(info.id, &body) {
        return Err(Error::Unsupported(format!("no se puede clonar: {r}")));
    }

    // ClickHouse: each column's kind (MATERIALIZED, ALIAS, EPHEMERAL),
    // from the table's own database: a structure read from the columns
    // alone (a table in another database than the session's) lacks it.
    if info.id == "clickhouse" {
        let db = source.schema().map(lit).unwrap_or_else(|| "currentDatabase()".into());
        let sql = format!("SELECT name, default_kind FROM system.columns WHERE database = {db} AND table = {}", lit(&source.name));
        let rows = strings(src, &sql).await.map_err(|e| Error::Query(format!("columnas de «{}»: {e}", source.name)))?;
        for r in rows {
            let mut r = r.into_iter();
            let (Some(name), Some(kind)) = (r.next().flatten(), r.next().flatten()) else { continue };
            if kind.is_empty() {
                continue;
            }
            for t in [&mut *source_t, &mut plan.table] {
                if let Some(c) = t.columns.iter_mut().find(|c| c.name == name) {
                    c.options.entry("default_kind".into()).or_insert_with(|| kind.clone());
                }
            }
        }
    }
    let cols = engine_columns(driver, src, source_t).await?;
    for t in [&mut *source_t, &mut plan.table] {
        for c in &mut t.columns {
            if cols.auto_increment.contains(&c.name) {
                c.auto_increment = true;
            }
            if cols.generated.contains(&c.name) && !super::generated(c, info.dialect) {
                c.options.insert(COMPUTED.into(), "true".into());
            }
        }
    }

    let from = qname(info, source.schema(), &source.name);
    let to = qname(info, plan.table.schema.as_deref(), &plan.table.name);
    if olap(info.id) {
        // The rollups, from `DESC … ALL` of the table itself: a structure
        // read from its columns alone (a table in another database than
        // the connection's) doesn't have them, and they'd be taken for
        // synchronous materialized views.
        let cols: Vec<String> = source_t.columns.iter().map(|c| c.name.clone()).collect();
        let (dcols, drows) = named_rows(src, &format!("DESC {from} ALL")).await.map_err(|e| Error::Query(format!("índices de «{}»: {e}", source.name)))?;
        for (name, columns) in desc_rollups(&source.name, &cols, &dcols, &drows) {
            for t in [&mut *source_t, &mut plan.table] {
                if !t.indexes.iter().any(|i| i.name == name) {
                    t.indexes.push(dbine_driver::IndexDef { name: name.clone(), columns: columns.clone(), kind: Some("ROLLUP".into()), ..Default::default() });
                }
            }
        }
    }
    if let Some(create) = native_create(info, &from, &to) {
        ddl.create = create;
        // Qualified, as the CREATE: the driver's DROP names the table
        // alone, and a clone in another database than the connection's
        // would be looked for (or another table dropped) in the session's.
        ddl.drop = format!("DROP TABLE IF EXISTS {to}");
        ddl.foreign_keys = None;
        // Every name belongs to the table (indexes, rollups, constraints):
        // the clone keeps them. Only rollups are left to create.
        let rollups: Vec<_> = if olap(info.id) { source_t.indexes.iter().filter(|i| is_rollup(i)).cloned().collect() } else { Vec::new() };
        // The engine's copy takes everything: not only the columns.
        notes.retain(|n| !plan.notes.contains(n) && !n.starts_with("el motor solo informa las columnas"));
        plan.renames.clear();
        plan.notes.clear();
        // From the definition too: a structure read from the columns alone
        // has no indexes.
        let others = source_t.indexes.iter().filter(|i| !is_rollup(i)).count() + column_list(&body).iter().filter(|l| l.starts_with("INDEX ")).count();
        if !options.with_indexes && others > 0 {
            notes.push(format!("en {} los índices son parte de la definición de la tabla: el clon los lleva igual", info.name));
        }
        if !options.with_indexes && !rollups.is_empty() {
            let names: Vec<&str> = rollups.iter().map(|r| r.name.as_str()).collect();
            notes.push(format!("sin índices: no se crean los rollups ({})", names.join(", ")));
        }
        // One job for all, on the clone's qualified name (the driver's
        // `table_ddl` leaves it unqualified: a table in another database
        // than the connection's wouldn't be found).
        ddl.indexes = (options.with_indexes && !rollups.is_empty()).then(|| {
            let q = |n: &str| quote_ident(quote_of(info.dialect), n);
            let each: Vec<String> =
                rollups.iter().map(|r| format!("{} ({})", q(&r.name), r.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "))).collect();
            format!("ALTER TABLE {to} ADD ROLLUP {};", each.join(", "))
        });
    }
    let olap_ix = if olap(info.id) {
        let clone_t = plan.table.clone();
        Some(olap_plan(info, src, source, source_t, &clone_t, options, &mut plan.renames, notes).await?)
    } else {
        None
    };
    Ok(Some(Analytic { body, olap: olap_ix }))
}

impl Analytic {
    /// After the `CREATE`: the clone's definition must be the original's.
    pub(super) async fn check_clone(&self, tgt: &mut dyn Session, clone: &ObjectRef) -> Result<()> {
        let obj = ObjectRef { kind: kinds::TABLE.into(), ..clone.clone() };
        let def = tgt.definition(&obj).await?.unwrap_or_default();
        let body = body_of(&def, &clone.name).ok_or_else(|| Error::State(format!("no se pudo leer la definición del clon «{}»; no se clona", clone.name)))?;
        match differences(&self.body, &body) {
            Some(d) => Err(Error::State(format!("el clon no quedó igual al original ({d}); no se clona"))),
            None => Ok(()),
        }
    }

    /// Whether [`Analytic::finish`] creates materialized views.
    pub(super) fn has_views(&self) -> bool {
        self.olap.as_ref().is_some_and(|o| !o.views.is_empty())
    }

    /// StarRocks / Doris, after `Ddl::indexes`: waits for the rollups'
    /// jobs, creates the synchronous materialized views (one job at a
    /// time), and checks the clone's `DESC … ALL` shows each of them as
    /// the original does. Anything missing or different fails the clone.
    pub(super) async fn finish(&self, driver: &dyn Driver, tgt: &mut dyn Session, clone: &ObjectRef, control: &CloneControl) -> Result<()> {
        let Some(o) = &self.olap else { return Ok(()) };
        let info = driver.info();
        let table = qname(info, clone.schema(), &clone.name);
        wait_jobs(info, tgt, clone, &o.rollups, control).await?;
        for (_, new, create) in &o.views {
            exec(tgt, create).await.map_err(|e| Error::Query(format!("vista materializada «{new}»: {e}")))?;
            wait_jobs(info, tgt, clone, std::slice::from_ref(new), control).await?;
        }
        let got = olap_shapes(tgt, &table).await?;
        let expected: Vec<(&str, &str, &str)> =
            o.rollups.iter().map(|r| (r.as_str(), r.as_str(), "el rollup")).chain(o.views.iter().map(|(a, b, _)| (a.as_str(), b.as_str(), "la vista materializada sincrónica"))).collect();
        for (orig, new, what) in &expected {
            match got.get(*new) {
                None => return Err(Error::State(format!("el clon no quedó igual al original (le falta {what} «{new}»); no se clona"))),
                Some(fields) if Some(fields) != o.shapes.get(*orig) => {
                    return Err(Error::State(format!("el clon no quedó igual al original ({what} «{new}» no tiene los mismos campos que «{orig}»); no se clona")))
                }
                Some(_) => {}
            }
        }
        if let Some(extra) = got.keys().find(|k| **k != clone.name && !expected.iter().any(|(_, n, _)| n == k)) {
            return Err(Error::State(format!("el clon no quedó igual al original (tiene un índice «{extra}» que el original no tiene); no se clona")));
        }
        // The original, again: a rollup or view added to it meanwhile (or
        // still being built) would be missing from the clone.
        let src = &o.source;
        let busy = busy_jobs(info, tgt, src.schema(), &src.name).await?;
        let now = olap_shapes(tgt, &qname(info, src.schema(), &src.name)).await?;
        if !busy.is_empty() || now != o.shapes {
            let what = if busy.is_empty() { "le cambiaron los rollups o vistas materializadas".to_string() } else { format!("se empezó a construir {}", busy.join(", ")) };
            return Err(Error::State(format!(
                "el original «{}» cambió mientras se clonaba ({what}); no se clona: esperá a que termine y volvé a clonar",
                src.name
            )));
        }
        Ok(())
    }
}

/// StarRocks / Doris: until the latest rollup / materialized view job of
/// each of `names` on `clone` ends. A cancelled one fails the clone; one
/// the engine no longer lists is left to the check after.
async fn wait_jobs(info: &DriverInfo, s: &mut dyn Session, clone: &ObjectRef, names: &[String], control: &CloneControl) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let sql = match clone.schema() {
        Some(db) => format!("SHOW ALTER TABLE ROLLUP FROM {}", quote_ident(quote_of(info.dialect), db)),
        None => "SHOW ALTER TABLE ROLLUP".to_string(),
    };
    loop {
        let (cols, rows) = named_rows(s, &sql).await.map_err(|e| Error::Query(format!("trabajos de rollup: {e}")))?;
        let col = |n: &str| cols.iter().position(|c| c.eq_ignore_ascii_case(n));
        let (Some(t), Some(ix), Some(id), Some(st)) = (col("TableName"), col("RollupIndexName"), col("JobId"), col("State")) else {
            return Err(Error::State("no se entiende lo que muestra SHOW ALTER TABLE ROLLUP; no se clona".into()));
        };
        let msg = col("Msg");
        let mut pending = false;
        for name in names {
            let latest = rows
                .iter()
                .filter(|r| r.get(t) == Some(&clone.name) && r.get(ix) == Some(name))
                .max_by_key(|r| r.get(id).and_then(|v| v.parse::<i64>().ok()).unwrap_or(-1));
            match latest.and_then(|r| r.get(st)).map(|s| s.to_ascii_uppercase()) {
                Some(s) if s == "FINISHED" => {}
                Some(s) if s == "CANCELLED" => {
                    let why = latest.and_then(|r| msg.and_then(|m| r.get(m))).cloned().unwrap_or_default();
                    return Err(Error::Query(format!("{} canceló la creación de «{name}» en el clon: {why}", info.name)));
                }
                Some(_) => pending = true,
                None => {}
            }
        }
        if !pending {
            return Ok(());
        }
        control.check()?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// ClickHouse: the constraints in a table's definition (see [`body_of`]),
/// in order: (name as printed, `CHECK` / `ASSUME`, the line without its
/// comma, for `ALTER TABLE … ADD`).
fn clickhouse_constraints(body: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for line in column_list(body) {
        let line = line.as_str();
        let Some(rest) = line.strip_prefix("CONSTRAINT ") else { continue };
        let end = if rest.starts_with('`') {
            let mut i = 1;
            let b = rest.as_bytes();
            while i < b.len() && b[i] != b'`' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            (i + 1).min(rest.len())
        } else {
            rest.find(' ').unwrap_or(rest.len())
        };
        let name = &rest[..end];
        let kind: String = rest[end..].trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        if !name.is_empty() && matches!(kind.as_str(), "CHECK" | "ASSUME") {
            out.push((name.to_string(), kind, line.to_string()));
        }
    }
    out
}

/// The items of the first parenthesized list in `body` (a table's
/// columns, indexes, projections and constraints), split on its
/// top-level commas outside strings and quoted names, each one trimmed.
fn column_list(body: &str) -> Vec<String> {
    let b = body.as_bytes();
    let Some(open) = body.find('(') else { return Vec::new() };
    let (mut out, mut depth, mut start, mut i) = (Vec::new(), 0i32, open + 1, open);
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'`' | b'"') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    out.push(body[start..i].trim().to_string());
                    break;
                }
            }
            b',' if depth == 1 => {
                out.push(body[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// `INSERT INTO to (cols) SELECT cols FROM from`. ClickHouse takes
/// MATERIALIZED columns only with `insert_allow_materialized_columns`.
fn insert_select(info: &DriverInfo, from: &str, to: &str, cols: &[String]) -> String {
    let settings = if info.id == "clickhouse" { " SETTINGS insert_allow_materialized_columns = 1" } else { "" };
    format!("INSERT INTO {to} ({0}){settings} SELECT {0} FROM {from}", cols.join(", "))
}

/// The rows, with one `INSERT … SELECT` inside the server: values never
/// pass through a client (Trino's zone names, HLL / BITMAP states…).
#[allow(clippy::too_many_arguments)]
pub(super) async fn copy_in_server(
    driver: &dyn Driver,
    tgt: &mut dyn Session,
    source: &ObjectRef,
    clone: &ObjectRef,
    loaded: &[String],
    rows_total: Option<u64>,
    control: &CloneControl,
    events: &(dyn Fn(CloneEvent) + Send + Sync),
) -> Result<u64> {
    let info = driver.info();
    let cols: Vec<String> = loaded.iter().map(|c| quote_ident(quote_of(info.dialect), c)).collect();
    let (from, to) = (qname(info, source.schema(), &source.name), qname(info, clone.schema(), &clone.name));
    let sql = insert_select(info, &from, &to, &cols);
    // ClickHouse checks CHECK constraints on every INSERT, but not on the
    // rows already there when one is added: the original may keep rows
    // that break it. The clone's constraints go away for the copy and come
    // back afterwards, in the same order, as the original has them.
    let (constraints, before) = if info.id == "clickhouse" {
        let obj = ObjectRef { kind: kinds::TABLE.into(), ..clone.clone() };
        let body = tgt.definition(&obj).await?.as_deref().and_then(|d| body_of(d, &clone.name)).unwrap_or_default();
        let c = clickhouse_constraints(&body);
        if c.iter().any(|(_, kind, _)| kind == "CHECK") { (c, body) } else { (Vec::new(), body) }
    } else {
        (Vec::new(), String::new())
    };
    for (name, _, _) in &constraints {
        exec(tgt, &format!("ALTER TABLE {to} DROP CONSTRAINT {name}"))
            .await
            .map_err(|e| Error::Query(format!("no se pudo quitar la restricción {name} del clon para copiar las filas: {e}")))?;
    }
    let start = Instant::now();
    {
        let run = exec(tgt, &sql);
        tokio::pin!(run);
        loop {
            tokio::select! {
                r = &mut run => {
                    r.map_err(|e| Error::Query(format!("copia de las filas: {e}")))?;
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    // Dropping the statement stops it; the caller drops the clone.
                    control.check()?;
                }
            }
        }
    }
    for (name, _, line) in &constraints {
        exec(tgt, &format!("ALTER TABLE {to} ADD {line}"))
            .await
            .map_err(|e| Error::Query(format!("no se pudo volver a poner la restricción {name} en el clon: {e}")))?;
    }
    if !constraints.is_empty() {
        let obj = ObjectRef { kind: kinds::TABLE.into(), ..clone.clone() };
        let after = tgt.definition(&obj).await?.as_deref().and_then(|d| body_of(d, &clone.name)).unwrap_or_default();
        if after != before {
            let d = differences(&before, &after).unwrap_or_else(|| "sus restricciones".into());
            return Err(Error::State(format!("el clon no quedó igual al original después de volver a poner sus restricciones ({d}); no se clona")));
        }
    }
    let rows = scalar(tgt, &format!("SELECT COUNT(*) FROM {to}")).await?.unwrap_or(0).max(0) as u64;
    let secs = start.elapsed().as_secs_f64();
    events(CloneEvent::Progress { rows_done: rows, rows_total, rows_per_s: if secs > 0.0 { rows as f64 / secs } else { 0.0 } });
    Ok(rows)
}

/// StarRocks / Doris: the clone's AUTO_INCREMENT past the largest copied
/// value (explicit values don't move it). `Some(note)` when the engine
/// can't be told.
pub(super) async fn resync(driver: &dyn Driver, tgt: &mut dyn Session, table: &str, column: &str, column_q: &str) -> Result<Option<String>> {
    let max = scalar(tgt, &format!("SELECT MAX({column_q}) FROM {table}")).await?.unwrap_or(0);
    match exec(tgt, &format!("ALTER TABLE {table} AUTO_INCREMENT = {}", max.max(0) + 1)).await {
        Ok(()) => Ok(None),
        // Already past it (it only moves forward).
        Err(e) if e.to_string().contains("greater than current value") => Ok(None),
        Err(e) if driver.info().id == "starrocks" => Err(Error::Query(format!("contador AUTO_INCREMENT de «{column}»: {e}"))),
        Err(_) => Ok(Some(format!(
            "{} no deja mover el contador AUTO_INCREMENT de «{column}»: las filas nuevas que no traigan «{column}» pueden repetir valores copiados (y en una tabla UNIQUE KEY reemplazar filas); cargalas con «{column}» explícito",
            driver.info().name
        ))),
    }
}

/// ClickHouse's "The max length of table name for database X is N,
/// current length is M" in Spanish; any other error as it came.
pub(super) fn in_spanish(e: Error) -> Error {
    const EN: &str = "The max length of table name for database ";
    let m = e.to_string();
    let Some(i) = m.find(EN) else { return e };
    let rest = &m[i + EN.len()..];
    let db = rest.split(" is ").next().unwrap_or_default();
    let num = |key: &str| rest.split(key).nth(1).map(|r| r.chars().take_while(char::is_ascii_digit).collect::<String>()).filter(|n| !n.is_empty());
    match (num(" is "), num("current length is ")) {
        (Some(max), Some(len)) => Error::State(format!(
            "el nombre es demasiado largo para ClickHouse: en la base «{db}» admite hasta {max} y mide {len} (las letras con tilde, la ñ y los símbolos cuentan como varios); elegí uno más corto"
        )),
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;

    #[test]
    fn clickhouse_default_kinds_are_computed() {
        let c = |kind: &str| {
            let mut c = ColumnDef { name: "x".into(), data_type: "Decimal(12, 2)".into(), ..Default::default() };
            c.options.insert("default_kind".into(), kind.into());
            c
        };
        for k in ["MATERIALIZED", "ALIAS", "EPHEMERAL"] {
            assert!(super::super::generated(&c(k), "clickhouse"), "{k}");
        }
        assert!(!super::super::generated(&c("DEFAULT"), "clickhouse"));
        let mut g = ColumnDef { name: "doble".into(), data_type: "decimal(21,2)".into(), ..Default::default() };
        assert!(!super::super::generated(&g, "mysql"));
        g.options.insert(COMPUTED.into(), "true".into());
        assert!(super::super::generated(&g, "mysql"));
    }

    const STARROCKS: &str = "CREATE TABLE `ventas` (\n  `id` bigint(20) NOT NULL AUTO_INCREMENT COMMENT \"\",\n  `dt` date NOT NULL COMMENT \"\",\n  `nota` varchar(65533) NULL COMMENT \"AUTO_INCREMENT\",\n  `do``ble` decimal(21, 2) NULL AS monto * 2 COMMENT \"\"\n) ENGINE=OLAP \nPRIMARY KEY(`id`, `dt`)\nPARTITION BY RANGE(`dt`)\n(PARTITION p1 VALUES [(\"0000-01-01\"), (\"2026-01-01\")))\nDISTRIBUTED BY HASH(`id`) BUCKETS 3 \nPROPERTIES (\n\"replication_num\" = \"1\"\n);";

    #[test]
    fn starrocks_auto_increment_from_its_definition() {
        assert_eq!(auto_increment_columns(STARROCKS), vec!["id".to_string()]);
    }

    #[test]
    fn bodies_leave_the_name_out() {
        let clone = STARROCKS.replacen("`ventas`", "`ventas_20260930_070509`", 1);
        let a = body_of(STARROCKS, "ventas").unwrap();
        assert!(a.starts_with("(\n`id` bigint(20)"), "{a}");
        assert_eq!(Some(a.clone()), body_of(&clone, "ventas_20260930_070509"));
        // Lost partitioning / distribution is told.
        let lost = clone.replace("PARTITION BY RANGE(`dt`)\n", "").replace("BUCKETS 3", "BUCKETS 10");
        let d = differences(&a, &body_of(&lost, "ventas_20260930_070509").unwrap()).unwrap();
        assert!(d.contains("PARTITION BY RANGE"), "{d}");
        // ClickHouse: one line, qualified, no quotes; a name inside another word is skipped.
        let ch = "CREATE TABLE clonef.e (`id` UInt64, `e` String) ENGINE = MergeTree ORDER BY id";
        assert_eq!(body_of(ch, "e").unwrap(), "(`id` UInt64, `e` String) ENGINE = MergeTree ORDER BY id");
        // GreptimeDB and Trino.
        assert_eq!(body_of("CREATE TABLE IF NOT EXISTS `m` (\n  `ts` TIMESTAMP(3) NOT NULL\n)", "m").unwrap(), "(\n`ts` TIMESTAMP(3) NOT NULL\n)");
        assert_eq!(body_of("CREATE TABLE memory.clonef.\"z z\" (\n   id bigint\n)", "z z").unwrap(), "(\nid bigint\n)");
        assert!(body_of("CREATE TABLE x (a int)", "otra").is_none());
    }

    #[test]
    fn storage_locations_must_differ() {
        let a = "(\nid bigint\n)\nWITH (\nformat = 'ORC',\nlocation = 's3://b/t-1'\n)";
        let b = "(\nid bigint\n)\nWITH (\nformat = 'ORC',\nlocation = 's3://b/t-2'\n)";
        assert_eq!(differences(a, b), None);
        assert!(differences(a, a).unwrap().contains("mismos archivos"));
        let c = "(\nid bigint\n)\nWITH (\nformat = 'PARQUET',\nlocation = 's3://b/t-2'\n)";
        assert!(differences(a, c).unwrap().contains("format = 'ORC'"));
    }

    #[test]
    fn engines_whose_data_lives_elsewhere_are_refused() {
        let ch = |engine: &str| refusal("clickhouse", &format!("(`id` UInt64) ENGINE = {engine} ORDER BY id"));
        assert!(ch("MergeTree").is_none());
        assert!(ch("ReplacingMergeTree(ts)").is_none());
        assert!(ch("Memory").is_none());
        for e in ["Distributed(c, db, t, rand())", "Buffer(db, t, 16, 10, 100, 10000, 1000000, 10000000, 100000000)", "Merge(db, '^t')", "Kafka", "URL('http://x', CSV)"] {
            assert!(ch(e).is_some_and(|r| r.contains("no guarda las filas")), "{e}");
        }
        assert!(ch("ReplicatedMergeTree('/clickhouse/tables/{shard}/db/t', '{replica}')").is_some_and(|r| r.contains("réplica")));
        // Set keeps its rows but can't be read: refused for that reason.
        let set = ch("Set").unwrap();
        assert!(set.contains("no deja leerlas") && !set.contains("no guarda las filas"), "{set}");
        assert!(ch("SetOther").is_some_and(|r| r.contains("no guarda las filas")));
        assert!(refusal("starrocks", "(\n`k` int\n) ENGINE=OLAP\nDUPLICATE KEY(`k`)").is_none());
        assert!(refusal("doris", "(\n`k` int\n) ENGINE=MYSQL\nPROPERTIES (...)").is_some());
        assert!(refusal("greptimedb", "(\n`ts` TIMESTAMP\n)\nENGINE=mito\nWITH(\nttl = '7days'\n)").is_none());
        assert!(refusal("greptimedb", "(\n`ts` TIMESTAMP\n)\nENGINE=metric\nWITH(\non_physical_table = 'p'\n)").is_some());
    }

    #[test]
    fn engine_copies_its_own_definition() {
        let info = |id: &'static str, dialect: &'static str| DriverInfo {
            id,
            name: id,
            family: dbine_driver::Family::Analytical,
            language: dbine_driver::Language::Sql,
            dialect,
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: false,
            object_kinds: vec![],
        };
        assert_eq!(native_create(&info("starrocks", "mysql"), "`a`", "`b`").as_deref(), Some("CREATE TABLE `b` LIKE `a`"));
        assert_eq!(native_create(&info("greptimedb", "mysql"), "`a`", "`b`").as_deref(), Some("CREATE TABLE `b` LIKE `a`"));
        assert_eq!(native_create(&info("clickhouse", "clickhouse"), "`d`.`a`", "`d`.`b`").as_deref(), Some("CREATE TABLE `d`.`b` AS `d`.`a`"));
        assert_eq!(native_create(&info("trino", "trino"), "a", "b"), None);
        assert!(name_problem(&info("greptimedb", "mysql"), "cpu_20260930_070509").is_none());
        assert!(name_problem(&info("greptimedb", "mysql"), "a.b-c@d#e:f").is_none());
        for bad in ["ñandú", "a b", "1abc"] {
            assert!(name_problem(&info("greptimedb", "mysql"), bad).is_some(), "{bad}");
        }
        assert!(name_problem(&info("starrocks", "mysql"), "ñandú").is_none());
    }

    fn test_info(id: &'static str, dialect: &'static str) -> DriverInfo {
        DriverInfo {
            id,
            name: id,
            family: dbine_driver::Family::Analytical,
            language: dbine_driver::Language::Sql,
            dialect,
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: false,
            object_kinds: vec![],
        }
    }

    #[test]
    fn clickhouse_materialized_columns_keep_their_values() {
        let ch = test_info("clickhouse", "clickhouse");
        let c = |kind: &str| {
            let mut c = ColumnDef { name: "creado".into(), data_type: "DateTime".into(), ..Default::default() };
            c.options.insert("default_kind".into(), kind.into());
            c
        };
        // Stored (now(), rand64() would change): copied as they are.
        assert!(stored_computed(&ch, &c("MATERIALIZED")));
        for k in ["ALIAS", "EPHEMERAL", "DEFAULT"] {
            assert!(!stored_computed(&ch, &c(k)), "{k}");
        }
        assert!(!stored_computed(&test_info("starrocks", "mysql"), &c("MATERIALIZED")));
        let cols = ["`id`".to_string(), "`creado`".into()];
        assert_eq!(
            insert_select(&ch, "`d`.`a`", "`d`.`b`", &cols),
            "INSERT INTO `d`.`b` (`id`, `creado`) SETTINGS insert_allow_materialized_columns = 1 SELECT `id`, `creado` FROM `d`.`a`"
        );
        assert_eq!(insert_select(&test_info("starrocks", "mysql"), "`a`", "`b`", &cols), "INSERT INTO `b` (`id`, `creado`) SELECT `id`, `creado` FROM `a`");
    }

    #[test]
    fn olap_indexes_from_desc_all() {
        let row = |i: &str, f: &str| (i.to_string(), f.to_string());
        let rows = [
            row("dup", "DUP_KEYS\tusuario\tint"),
            row("", "\tts\tdate"),
            row("", "\t\t"),
            row("mv_dup", "AGG_KEYS\tusuario\tint"),
            row("", "\tmv_largo\tbigint\tSUM"),
        ];
        let s = index_shapes(&rows);
        assert_eq!(s.keys().collect::<Vec<_>>(), ["dup", "mv_dup"]);
        assert_eq!(s["dup"].len(), 2);
        assert_eq!(s["mv_dup"], ["AGG_KEYS\tusuario\tint", "\tmv_largo\tbigint\tSUM"]);
    }

    #[test]
    fn jobs_being_built_on_the_original() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let cols = v(&["JobId", "TableName", "CreateTime", "FinishedTime", "BaseIndexName", "RollupIndexName", "RollupId", "TransactionId", "State", "Msg"]);
        let rows = vec![
            v(&["1", "rr", "", "", "rr", "r_old", "", "", "FINISHED", ""]),
            v(&["2", "rr", "", "", "rr", "r_v", "", "", "PENDING", ""]),
            v(&["3", "rr", "", "", "rr", "mv_x", "", "", "WAITING_TXN", ""]),
            v(&["4", "rr", "", "", "rr", "r_bad", "", "", "CANCELLED", "x"]),
            v(&["5", "otra", "", "", "otra", "r_z", "", "", "RUNNING", ""]),
        ];
        assert_eq!(
            busy_in(&cols, &rows, "rr", "ROLLUP").unwrap(),
            ["el rollup o vista materializada «r_v»", "el rollup o vista materializada «mv_x»"]
        );
        assert!(busy_in(&cols, &rows, "otra2", "ROLLUP").unwrap().is_empty());
        let cols = v(&["JobId", "TableName", "CreateTime", "FinishTime", "IndexName", "IndexId", "State"]);
        let rows = vec![v(&["1", "rr", "", "", "rr", "", "RUNNING"]), v(&["2", "rr", "", "", "r_v", "", "RUNNING"])];
        assert_eq!(busy_in(&cols, &rows, "rr", "COLUMN").unwrap(), ["un cambio de columnas"]);
        assert!(busy_in(&v(&["JobId", "State"]), &[], "rr", "COLUMN").is_none());
        // StarRocks 4: `ALTER TABLE od DISTRIBUTED BY HASH(j) BUCKETS 3`.
        let cols = v(&["JobId", "TableName", "CreateTime", "FinishTime", "Operation", "TransactionId", "State", "Msg", "Progress", "Timeout"]);
        let rows = vec![v(&["1", "od", "", "NULL", "", "-1", "PENDING", "", "0", "86400"]), v(&["2", "rr", "", "", "", "", "FINISHED", "", "100", ""])];
        assert_eq!(busy_in(&cols, &rows, "od", "OPTIMIZE").unwrap(), ["un cambio de distribución, buckets o particiones (OPTIMIZE)"]);
        assert!(busy_in(&cols, &rows, "rr", "OPTIMIZE").unwrap().is_empty());
    }

    #[test]
    fn sync_views_move_to_the_clone() {
        let def = "CREATE MATERIALIZED VIEW mv_dup AS SELECT usuario, ts, sum(length(evento)) AS largo FROM dup GROUP BY usuario, ts";
        assert_eq!(
            retarget_view(def, "dup", Some("db"), "`db`.`mv_dup_1`", "`db`.`dup_1`").as_deref(),
            Some("CREATE MATERIALIZED VIEW `db`.`mv_dup_1` AS SELECT usuario, ts, sum(length(evento)) AS largo FROM `db`.`dup_1` GROUP BY usuario, ts")
        );
        // Qualified and quoted, with properties and a FROM inside a function.
        let def = "create materialized view if not exists `db`.`m v` PROPERTIES (\"x\" = \"FROM dup\") as select k, max(extract(year from ts)) from `db`.`dup` where s <> 'from x' group by k";
        let got = retarget_view(def, "dup", Some("db"), "`m2`", "`dup2`").unwrap();
        assert_eq!(got, "create materialized view if not exists `m2` PROPERTIES (\"x\" = \"FROM dup\") as select k, max(extract(year from ts)) from `dup2` where s <> 'from x' group by k");
        // Another table, another database, two FROMs, not a view: not understood.
        assert!(retarget_view("CREATE MATERIALIZED VIEW m AS SELECT a FROM otra", "dup", None, "m2", "c").is_none());
        assert!(retarget_view("CREATE MATERIALIZED VIEW m AS SELECT a FROM x.dup", "dup", Some("db"), "m2", "c").is_none());
        assert!(retarget_view("CREATE MATERIALIZED VIEW m AS SELECT a FROM dup UNION SELECT a FROM dup", "dup", None, "m2", "c").is_none());
        assert!(retarget_view("CREATE VIEW m AS SELECT a FROM dup", "dup", None, "m2", "c").is_none());
        assert!(retarget_view("CREATE MATERIALIZED VIEW m AS SELECT 'a FROM dup", "dup", None, "m2", "c").is_none());
        assert_eq!(view_name("mv_dup", "dup", "dup_20260930_004221"), "mv_dup_20260930_004221");
        assert_eq!(view_name("mv_dup", "dup", "copia"), "mv_dup_copia");
    }

    #[test]
    fn rollups_from_desc_all() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let cols = v(&["IndexName", "IndexKeysType", "Field", "Type", "Null", "Key", "Default", "Extra"]);
        let rows = vec![
            v(&["rr x", "DUP_KEYS", "k", "int", "Yes", "true", "NULL", ""]),
            v(&["", "", "v", "bigint", "Yes", "false", "NULL", ""]),
            v(&["", "", "", "", "", "", "", ""]),
            v(&["r v", "DUP_KEYS", "v", "bigint", "Yes", "true", "NULL", ""]),
            v(&["", "", "k", "int", "Yes", "true", "NULL", ""]),
            v(&["mv", "AGG_KEYS", "k", "int", "Yes", "true", "NULL", ""]),
            v(&["", "", "mv_sum_v", "bigint", "Yes", "false", "NULL", "SUM"]),
        ];
        assert_eq!(desc_rollups("rr x", &v(&["k", "v"]), &cols, &rows), [("r v".to_string(), v(&["v", "k"]))]);
        assert!(desc_rollups("rr x", &v(&["k", "v"]), &v(&["Field"]), &rows).is_empty());
    }

    #[test]
    fn clickhouse_constraints_in_order() {
        let def = "CREATE TABLE clonev.ckv\n(\n    `id` UInt32,\n    `CONSTRAINT x` Int32,\n    CONSTRAINT `a b` ASSUME id > 0,\n    CONSTRAINT ck_pos CHECK v > 0,\n    CONSTRAINT `c\\`k` CHECK id < 100\n)\nENGINE = MergeTree\nORDER BY id";
        let c = clickhouse_constraints(&body_of(def, "ckv").unwrap());
        // `system.tables.create_table_query`: one line.
        let one = "CREATE TABLE clonev.ckv (`id` UInt32, `CONSTRAINT x` Int32 DEFAULT ',', CONSTRAINT `a b` ASSUME id > 0, CONSTRAINT ck_pos CHECK v > 0, CONSTRAINT `c\\`k` CHECK id < 100) ENGINE = MergeTree ORDER BY id";
        assert_eq!(clickhouse_constraints(&body_of(one, "ckv").unwrap()), c);
        let got: Vec<(&str, &str, &str)> = c.iter().map(|(a, b, l)| (a.as_str(), b.as_str(), l.as_str())).collect();
        assert_eq!(
            got,
            [
                ("`a b`", "ASSUME", "CONSTRAINT `a b` ASSUME id > 0"),
                ("ck_pos", "CHECK", "CONSTRAINT ck_pos CHECK v > 0"),
                ("`c\\`k`", "CHECK", "CONSTRAINT `c\\`k` CHECK id < 100")
            ]
        );
    }

    #[test]
    fn clickhouse_long_names_in_spanish() {
        let e = in_spanish(Error::Query("Code: 69. DB::Exception: The max length of table name for database clonef is 207, current length is 300. (ARGUMENT_OUT_OF_BOUND)".into()));
        let s = e.to_string();
        assert!(s.contains("207") && s.contains("300") && s.contains("«clonef»"), "{s}");
        assert_eq!(in_spanish(Error::Query("otro".into())).to_string(), Error::Query("otro".into()).to_string());
    }

    #[test]
    fn a_table_in_another_database_is_looked_up_in_its_own() {
        // The session's database (or none) says nothing about otra_db.gc:
        // its own catalog does, names quoted as literals.
        let sr = test_info("starrocks", "mysql");
        assert_eq!(
            exists_sql(&sr, "otra_db", "o'k").as_deref(),
            Some("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'otra_db' AND TABLE_NAME = 'o''k'")
        );
        let ch = test_info("clickhouse", "clickhouse");
        assert_eq!(
            exists_sql(&ch, "otra_ch", "plain").as_deref(),
            Some("SELECT count() FROM system.tables WHERE database = 'otra_ch' AND name = 'plain'")
        );
        assert!(exists_sql(&test_info("trino", "trino"), "c", "t").is_none());
        // Engines whose clone is their own copy of the definition can
        // clone such a table from its columns; Trino (the driver's CREATE)
        // can't.
        assert!(copies_definition(&sr) && copies_definition(&ch) && copies_definition(&test_info("greptimedb", "postgres")));
        assert!(!copies_definition(&test_info("trino", "trino")));
    }
}
