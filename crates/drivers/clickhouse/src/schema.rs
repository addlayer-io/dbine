//! Table structure for ClickHouse and Timeplus: the catalog read, the table
//! designer and DDL.
//!
//! How a [`TableSchema`] maps to ClickHouse:
//! - `nullable` wraps the type in `Nullable(…)` (read back unwrapped);
//! - `primary_key` is the sorting key when no `order_by` option is given,
//!   else `PRIMARY KEY (…)` next to `ORDER BY`;
//! - options `engine` (+ `engine_args`), `order_by`, `partition_by`,
//!   `sample_by`, `ttl`, `settings` (Timeplus: `mode`, `settings`, `ttl`);
//! - column options `default_kind` (DEFAULT / MATERIALIZED / ALIAS /
//!   EPHEMERAL) and `codec`;
//! - indexes are data-skipping indexes: `kind` is the index type
//!   (`minmax`, `bloom_filter(0.01)`, `set(100)`…), optionally followed by
//!   `GRANULARITY n`.

use crate::Flavor;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{
    kinds, CheckDef, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Field, FieldKind, IndexDef, KeyDef, RowChange,
    TableSchema,
};
use dbine_driver::{Error, Result};
use serde_json::Value;
use std::collections::BTreeMap;

const ENGINES: &[(&str, &str)] = &[
    ("MergeTree", "MergeTree"),
    ("ReplacingMergeTree", "ReplacingMergeTree"),
    ("SummingMergeTree", "SummingMergeTree"),
    ("AggregatingMergeTree", "AggregatingMergeTree"),
    ("CollapsingMergeTree", "CollapsingMergeTree (requiere columna sign)"),
    ("VersionedCollapsingMergeTree", "VersionedCollapsingMergeTree (sign, versión)"),
    ("ReplicatedMergeTree", "ReplicatedMergeTree"),
    ("Log", "Log"),
    ("TinyLog", "TinyLog"),
    ("StripeLog", "StripeLog"),
    ("Memory", "Memory"),
];

fn text_field(key: &'static str, label: &'static str, placeholder: &'static str, help: &'static str) -> Field {
    Field::new(key, label, FieldKind::Text).placeholder(placeholder).help(help)
}

fn default_kind() -> Field {
    Field::new(
        "default_kind",
        "Tipo de valor por defecto",
        FieldKind::Select(vec![
            ("DEFAULT", "DEFAULT"),
            ("MATERIALIZED", "MATERIALIZED"),
            ("ALIAS", "ALIAS"),
            ("EPHEMERAL", "EPHEMERAL"),
        ]),
    )
    .default_value("DEFAULT")
    .help("Cómo se usa la expresión del valor por defecto.")
}

fn codec() -> Field {
    text_field("codec", "Códec", "ZSTD(1)", "Compresión de la columna: CODEC(…).")
}

pub fn designer(flavor: Flavor) -> DesignerSpec {
    match flavor {
        Flavor::ClickHouse => DesignerSpec {
            auto_increment: false,
            comments: true,
            foreign_keys: false,
            column_options: vec![default_kind(), codec()],
            table_options: vec![
                Field::new("engine", "Motor", FieldKind::Select(ENGINES.to_vec())).default_value("MergeTree").required(),
                text_field("engine_args", "Argumentos del motor", "ver", "Por ejemplo la columna de versión de ReplacingMergeTree o sign de CollapsingMergeTree."),
                text_field("order_by", "ORDER BY", "(id, fecha)", "Clave de ordenamiento. Vacío: las columnas de la clave primaria."),
                text_field("partition_by", "PARTITION BY", "toYYYYMM(fecha)", ""),
                text_field("sample_by", "SAMPLE BY", "intHash32(id)", ""),
                text_field("ttl", "TTL", "fecha + INTERVAL 1 MONTH", ""),
                text_field("settings", "SETTINGS", "index_granularity = 8192", ""),
            ],
            ..DesignerSpec::sql_table(vec![
                "UInt8", "UInt16", "UInt32", "UInt64", "Int8", "Int16", "Int32", "Int64", "Int128", "Float32", "Float64",
                "Decimal(18, 2)", "Bool", "String", "FixedString(16)", "LowCardinality(String)", "UUID", "Date",
                "Date32", "DateTime", "DateTime64(3)", "Enum8('a' = 1, 'b' = 2)", "IPv4", "IPv6", "JSON",
                "Array(String)", "Map(String, String)", "Tuple(String, UInt32)",
            ])
        },
        Flavor::Timeplus => DesignerSpec {
            kind: kinds::STREAM,
            label: "Nuevo stream",
            auto_increment: false,
            comments: true,
            foreign_keys: false,
            column_options: vec![default_kind(), codec()],
            table_options: vec![
                Field::new(
                    "mode",
                    "Modo",
                    FieldKind::Select(vec![
                        ("append", "append"),
                        ("versioned_kv", "versioned_kv (requiere clave primaria)"),
                        ("changelog_kv", "changelog_kv (requiere clave primaria)"),
                        ("mutable", "mutable (requiere clave primaria)"),
                    ]),
                )
                .default_value("append"),
                text_field("ttl", "TTL", "to_datetime(_tp_time) + INTERVAL 1 DAY", ""),
                text_field("settings", "SETTINGS", "event_time_column = 'ts'", "Otros ajustes del stream, separados por comas."),
            ],
            ..DesignerSpec::sql_table(vec![
                "uint8", "uint16", "uint32", "uint64", "int8", "int16", "int32", "int64", "float32", "float64",
                "decimal(18, 2)", "bool", "string", "low_cardinality(string)", "uuid", "date", "datetime",
                "datetime64(3)", "ipv4", "json", "array(string)", "map(string, string)", "tuple(string, uint32)",
            ])
        },
    }
}

pub fn templates(flavor: Flavor) -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate { kind, label, template: template.to_string() };
    match flavor {
        Flavor::ClickHouse => vec![
            t(kinds::VIEW, "Nueva vista", "CREATE VIEW {name} AS\nSELECT *\nFROM tabla;\n"),
            t(
                kinds::MATERIALIZED_VIEW,
                "Nueva vista materializada",
                "CREATE MATERIALIZED VIEW {name}\nENGINE = SummingMergeTree\nORDER BY clave\nAS SELECT clave, count() AS total\nFROM tabla\nGROUP BY clave;\n",
            ),
            t(
                kinds::MATERIALIZED_VIEW,
                "Nueva vista materializada refrescable",
                "CREATE MATERIALIZED VIEW {name}\nREFRESH EVERY 1 HOUR\nENGINE = MergeTree ORDER BY tuple()\nAS SELECT *\nFROM tabla;\n",
            ),
            t(
                crate::DICTIONARY,
                "Nuevo diccionario",
                "CREATE DICTIONARY {name}\n(\n    id UInt64,\n    nombre String\n)\nPRIMARY KEY id\nSOURCE(CLICKHOUSE(TABLE 'tabla'))\nLAYOUT(HASHED())\nLIFETIME(MIN 300 MAX 600);\n",
            ),
            t(kinds::FUNCTION, "Nueva función", "CREATE FUNCTION {name} AS (a, b) -> a + b;\n"),
        ],
        Flavor::Timeplus => vec![
            t(kinds::VIEW, "Nueva vista", "CREATE VIEW {name} AS\nSELECT *\nFROM stream_origen;\n"),
            t(
                kinds::MATERIALIZED_VIEW,
                "Nueva vista materializada",
                "CREATE MATERIALIZED VIEW {name} AS\nSELECT window_start, count() AS total\nFROM tumble(stream_origen, 1m)\nGROUP BY window_start;\n",
            ),
            t(
                kinds::STREAM,
                "Nuevo stream externo (Kafka)",
                "CREATE EXTERNAL STREAM {name} (raw string)\nSETTINGS type = 'kafka', brokers = 'localhost:9092', topic = 'topico';\n",
            ),
            t(
                crate::DICTIONARY,
                "Nuevo diccionario",
                "CREATE DICTIONARY {name}\n(\n    id uint64,\n    nombre string\n)\nPRIMARY KEY id\nSOURCE(PROTON(TABLE 'stream_origen'))\nLAYOUT(HASHED())\nLIFETIME(MIN 300 MAX 600);\n",
            ),
            t(
                kinds::FUNCTION,
                "Nueva función JavaScript",
                "CREATE FUNCTION {name}(a float64)\nRETURNS float64\nLANGUAGE JAVASCRIPT AS $$\n  function {name}(a) { return a.map(x => x * 2); }\n$$;\n",
            ),
        ],
    }
}

pub(crate) fn q(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

/// A string literal: ClickHouse strings take backslash escapes.
pub fn string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub fn literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => string_literal(s),
        other => string_literal(&other.to_string()),
    }
}

/// The browse query restricted by the grid's column filters (also the
/// `table(stream)` one of Timeplus). Literals take backslash escapes, so
/// LIKE patterns rely on the default `\` escape: no ESCAPE clause.
pub fn filtered_browse(browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
    use dbine_driver::filter::{insert_where, sql_condition, FilterOp, SqlFilterStyle};
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let style = SqlFilterStyle { quote: Quote::Backtick, literal: &literal, like: "LIKE", true_literal: "true", false_literal: "false" };
    let mut parts = Vec::new();
    for f in filters {
        let c = sql_condition(std::slice::from_ref(f), &style)?;
        let like = matches!(f.op, FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith);
        parts.push(match c.strip_suffix(" ESCAPE '\\'") {
            Some(s) if like => s.to_string(),
            _ => c,
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

/// `INSERT INTO t (…) VALUES (…), (…)`, 1000 rows per statement.
pub fn insert_script(schema: Option<&str>, table: &str, columns: &[String], rows: &[Vec<Value>]) -> String {
    let name = qualified_name(Quote::Backtick, schema.filter(|s| !s.is_empty()), table);
    let cols: Vec<String> = columns.iter().map(|c| q(c)).collect();
    rows.chunks(1000)
        .map(|chunk| {
            let tuples: Vec<String> = chunk
                .iter()
                .map(|r| format!("({})", r.iter().map(literal).collect::<Vec<_>>().join(", ")))
                .collect();
            format!("INSERT INTO {name} ({}) VALUES\n  {};", cols.join(", "), tuples.join(",\n  "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Edited rows as mutations, one per row: `ALTER TABLE t UPDATE a = …
/// WHERE …;` (`ALTER STREAM` in Timeplus). A mutation needs a WHERE, so a
/// row without key columns gets `WHERE 1`.
pub fn update_script(flavor: Flavor, schema: Option<&str>, table: &str, changes: &[RowChange]) -> String {
    let name = qualified_name(Quote::Backtick, schema.filter(|s| !s.is_empty()), table);
    let what = match flavor {
        Flavor::ClickHouse => "TABLE",
        Flavor::Timeplus => "STREAM",
    };
    changes
        .iter()
        .filter(|c| !c.set.is_empty())
        .map(|c| {
            let set: Vec<String> = c.set.iter().map(|(k, v)| format!("{} = {}", q(k), literal(v))).collect();
            let wh: Vec<String> =
                c.key.iter().map(|(k, v)| if v.is_null() { format!("{} IS NULL", q(k)) } else { format!("{} = {}", q(k), literal(v)) }).collect();
            let wh = if wh.is_empty() { "1".to_string() } else { wh.join(" AND ") };
            format!("ALTER {what} {name} UPDATE {} WHERE {wh};", set.join(", "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rows to delete as mutations, one per key: `ALTER TABLE t DELETE WHERE
/// …;` (`ALTER STREAM` in Timeplus), as in [`update_script`]. A key without
/// columns is skipped: it would delete the whole table.
pub fn delete_script(flavor: Flavor, schema: Option<&str>, table: &str, keys: &[Vec<(String, Value)>]) -> String {
    let name = qualified_name(Quote::Backtick, schema.filter(|s| !s.is_empty()), table);
    let what = match flavor {
        Flavor::ClickHouse => "TABLE",
        Flavor::Timeplus => "STREAM",
    };
    keys.iter()
        .filter(|k| !k.is_empty())
        .map(|k| {
            let wh: Vec<String> =
                k.iter().map(|(c, v)| if v.is_null() { format!("{} IS NULL", q(c)) } else { format!("{} = {}", q(c), literal(v)) }).collect();
            format!("ALTER {what} {name} DELETE WHERE {};", wh.join(" AND "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wrap a type in `Nullable(…)` where ClickHouse allows it.
fn nullable_type(t: &str, flavor: Flavor) -> String {
    let lower = t.trim().to_ascii_lowercase();
    let (nullable, lc) = match flavor {
        Flavor::ClickHouse => ("Nullable", "lowcardinality("),
        Flavor::Timeplus => ("nullable", "low_cardinality("),
    };
    if lower.starts_with("nullable(") || lower.contains("(nullable(") {
        return t.to_string();
    }
    if lower.starts_with(lc) && lower.ends_with(')') {
        let open = t.find('(').unwrap_or(0);
        return format!("{}({nullable}({}))", &t[..open], &t[open + 1..t.len() - 1]);
    }
    let never = ["array(", "map(", "tuple(", "nested(", "json", "object(", "variant(", "dynamic", "aggregatefunction("];
    if never.iter().any(|p| lower.starts_with(p)) {
        return t.to_string();
    }
    format!("{nullable}({t})")
}

/// `Nullable(T)` → `(T, true)`; other types as they are.
fn unwrap_nullable(t: &str) -> (String, bool) {
    let trimmed = t.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("nullable(") && trimmed.ends_with(')') {
        return (trimmed[9..trimmed.len() - 1].to_string(), true);
    }
    (trimmed.to_string(), lower.contains("nullable("))
}

fn opt<'a>(t: &'a TableSchema, key: &str) -> Option<&'a str> {
    t.options.get(key).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn copt<'a>(c: &'a ColumnDef, key: &str) -> Option<&'a str> {
    c.options.get(key).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn is_merge_tree(engine: &str) -> bool {
    engine.contains("MergeTree")
}

/// `x` or `(a, b)` from a key list.
fn key_expr(cols: &[String]) -> String {
    match cols {
        [one] => one.clone(),
        many => format!("({})", many.join(", ")),
    }
}

/// Index kind of a projection: its only column is the query, in
/// parentheses (`(SELECT a, count() GROUP BY a)`).
pub const PROJECTION: &str = "PROJECTION";
/// Table option prefix: `assume:<name>` holds a `CONSTRAINT name ASSUME
/// <expr>` (a hint for the optimizer that nothing checks).
pub const ASSUME: &str = "assume:";

pub(crate) fn is_projection(ix: &IndexDef) -> bool {
    ix.kind.as_deref().is_some_and(|k| k.trim().eq_ignore_ascii_case(PROJECTION))
}

/// A CHECK's name: ClickHouse needs one.
pub(crate) fn check_name(t: &TableSchema, i: usize, c: &CheckDef) -> String {
    c.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("{}_check_{}", t.name, i + 1))
}

/// `CONSTRAINT n CHECK …` for the table's CHECKs, then its ASSUMEs.
pub(crate) fn constraint_clauses(t: &TableSchema) -> Vec<String> {
    let mut out: Vec<String> = t.checks.iter().enumerate().map(|(i, c)| format!("CONSTRAINT {} CHECK {}", q(&check_name(t, i, c)), c.expression.trim())).collect();
    for (k, v) in &t.options {
        if let Some(n) = k.strip_prefix(ASSUME) {
            out.push(format!("CONSTRAINT {} ASSUME {}", q(n), v.trim()));
        }
    }
    out
}

pub(crate) fn index_clause(ix: &IndexDef) -> String {
    if is_projection(ix) {
        return format!("PROJECTION {} {}", q(&ix.name), ix.columns.join(", "));
    }
    let kind = ix.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()).unwrap_or("minmax");
    let kind = if kind.to_ascii_uppercase().contains("GRANULARITY") { kind.to_string() } else { format!("{kind} GRANULARITY 1") };
    format!("INDEX {} {} TYPE {kind}", q(&ix.name), key_expr(&ix.columns))
}

/// Column names are quoted in key expressions only when they are plain
/// column names; anything else is taken as an expression.
fn key_part(t: &TableSchema, part: &str) -> String {
    if t.columns.iter().any(|c| c.name == part) { q(part) } else { part.to_string() }
}

/// The type as the server stores it: `Nullable(…)` when the column takes NULL.
pub(crate) fn full_type(flavor: Flavor, c: &ColumnDef) -> String {
    if c.nullable { nullable_type(&c.data_type, flavor) } else { c.data_type.clone() }
}

/// A column as CREATE TABLE and ALTER … ADD / MODIFY COLUMN write it.
pub(crate) fn column_sql(flavor: Flavor, c: &ColumnDef) -> String {
    let mut l = format!("{} {}", q(&c.name), full_type(flavor, c));
    if let Some(d) = c.default_value.as_deref().filter(|d| !d.trim().is_empty()) {
        l.push_str(&format!(" {} {d}", copt(c, "default_kind").unwrap_or("DEFAULT")));
    }
    if let Some(codec) = copt(c, "codec") {
        if codec.to_ascii_uppercase().starts_with("CODEC(") {
            l.push_str(&format!(" {codec}"));
        } else {
            l.push_str(&format!(" CODEC({codec})"));
        }
    }
    if let Some(cm) = c.comment.as_deref().filter(|s| !s.is_empty()) {
        l.push_str(&format!(" COMMENT {}", string_literal(cm)));
    }
    l
}

pub fn table_ddl(flavor: Flavor, t: &TableSchema, parts: DdlParts) -> String {
    let name = qualified_name(Quote::Backtick, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name);
    let what = match flavor {
        Flavor::ClickHouse => "TABLE",
        Flavor::Timeplus => "STREAM",
    };
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP {what} {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    let inline_indexes = parts.create && parts.indexes;
    if parts.create {
        let mut lines: Vec<String> = t.columns.iter().map(|c| format!("    {}", column_sql(flavor, c))).collect();
        if inline_indexes {
            lines.extend(t.indexes.iter().filter(|ix| !is_projection(ix)).map(|ix| format!("    {}", index_clause(ix))));
        }
        lines.extend(constraint_clauses(t).into_iter().map(|c| format!("    {c}")));
        if inline_indexes {
            lines.extend(t.indexes.iter().filter(|ix| is_projection(ix)).map(|ix| format!("    {}", index_clause(ix))));
        }
        let pk: Vec<String> =
            t.primary_key.as_ref().map(|k| k.columns.iter().map(|c| key_part(t, c)).collect()).unwrap_or_default();
        let mut s = format!(
            "CREATE {what} {}{name}\n(\n{}\n)",
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            lines.join(",\n")
        );
        match flavor {
            Flavor::ClickHouse => {
                let engine = opt(t, "engine").unwrap_or("MergeTree");
                let args = opt(t, "engine_args").unwrap_or("");
                s.push_str(&format!("\nENGINE = {engine}"));
                if !args.is_empty() || engine.starts_with("Replicated") {
                    s.push_str(&format!("({args})"));
                }
                if is_merge_tree(engine) {
                    let order = opt(t, "order_by").map(str::to_string);
                    if let Some(p) = opt(t, "partition_by") {
                        s.push_str(&format!("\nPARTITION BY {p}"));
                    }
                    if order.is_some() && !pk.is_empty() && order.as_deref() != Some(key_expr(&pk).as_str()) {
                        s.push_str(&format!("\nPRIMARY KEY {}", key_expr(&pk)));
                    }
                    let order = order.unwrap_or_else(|| if pk.is_empty() { "tuple()".into() } else { key_expr(&pk) });
                    s.push_str(&format!("\nORDER BY {order}"));
                    if let Some(x) = opt(t, "sample_by") {
                        s.push_str(&format!("\nSAMPLE BY {x}"));
                    }
                    if let Some(x) = opt(t, "ttl") {
                        s.push_str(&format!("\nTTL {x}"));
                    }
                    if let Some(x) = opt(t, "settings") {
                        s.push_str(&format!("\nSETTINGS {x}"));
                    }
                }
            }
            Flavor::Timeplus => {
                if !pk.is_empty() {
                    s.push_str(&format!("\nPRIMARY KEY {}", key_expr(&pk)));
                }
                if let Some(x) = opt(t, "ttl") {
                    s.push_str(&format!("\nTTL {x}"));
                }
                let mut settings: Vec<String> = Vec::new();
                if let Some(m) = opt(t, "mode").filter(|m| *m != "append") {
                    settings.push(format!("mode = {}", string_literal(m)));
                }
                if let Some(x) = opt(t, "settings") {
                    settings.push(x.to_string());
                }
                if !settings.is_empty() {
                    s.push_str(&format!("\nSETTINGS {}", settings.join(", ")));
                }
            }
        }
        if let Some(cm) = t.comment.as_deref().filter(|s| !s.is_empty()) {
            s.push_str(&format!("\nCOMMENT {}", string_literal(cm)));
        }
        s.push(';');
        out.push(s);
    }
    if parts.indexes && !inline_indexes {
        for ix in &t.indexes {
            let (word, clause) = if is_projection(ix) { ("PROJECTION", index_clause(ix)) } else { ("INDEX", index_clause(ix)) };
            out.push(format!(
                "ALTER {what} {name} ADD {word} {}{};",
                if parts.if_exists { "IF NOT EXISTS " } else { "" },
                clause.trim_start_matches(word).trim_start()
            ));
        }
    }
    out.join("\n")
}

/// Split on commas outside parentheses and quotes.
pub fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth, mut quote) = (Vec::new(), String::new(), 0i32, None::<char>);
    let mut prev = ' ';
    for c in s.chars() {
        match quote {
            Some(qc) => {
                if c == qc && prev != '\\' {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '`' | '"' => quote = Some(c),
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(std::mem::take(&mut cur).trim().to_string());
                    prev = c;
                    continue;
                }
                _ => {}
            },
        }
        cur.push(c);
        prev = c;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// `(a, b)` / `a, b` / `tuple()` → key parts, unquoted when they are
/// backticked names.
pub(crate) fn key_parts(expr: &str) -> Vec<String> {
    let e = expr.trim();
    if e.is_empty() || e == "tuple()" {
        return Vec::new();
    }
    let inner = if e.starts_with('(') && e.ends_with(')') && split_top(e).len() == 1 { &e[1..e.len() - 1] } else { e };
    split_top(inner)
        .into_iter()
        .map(|p| match p.strip_prefix('`').and_then(|x| x.strip_suffix('`')) {
            Some(n) => n.replace("``", "`"),
            None => p,
        })
        .collect()
}

/// Clause positions in `engine_full`, which reads
/// `Engine(args) PARTITION BY … PRIMARY KEY … ORDER BY … SAMPLE BY … TTL … SETTINGS …`.
fn engine_clauses(full: &str) -> BTreeMap<&'static str, String> {
    const KEYS: [(&str, &str); 6] = [
        (" PARTITION BY ", "partition_by"),
        (" PRIMARY KEY ", "primary_key"),
        (" ORDER BY ", "order_by"),
        (" SAMPLE BY ", "sample_by"),
        (" TTL ", "ttl"),
        (" SETTINGS ", "settings"),
    ];
    let mut found: Vec<(usize, &str, usize)> =
        KEYS.iter().filter_map(|(kw, k)| full.find(kw).map(|p| (p, *k, kw.len()))).collect();
    found.sort();
    let mut out = BTreeMap::new();
    let head_end = found.first().map_or(full.len(), |f| f.0);
    out.insert("engine", full[..head_end].trim().to_string());
    for (i, (p, k, len)) in found.iter().enumerate() {
        let end = found.get(i + 1).map_or(full.len(), |f| f.0);
        out.insert(*k, full[p + len..end].trim().to_string());
    }
    out
}

/// `a, b` (as system.tables spells multi-part keys) → `(a, b)`.
fn tuple(expr: &str) -> String {
    if split_top(expr).len() > 1 { format!("({expr})") } else { expr.to_string() }
}

/// A name at the start of `s` (backticked or bare) and what follows it.
fn leading_name(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix('`') {
        let mut name = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    if let Some((_, n)) = chars.next() {
                        name.push(n);
                    }
                }
                '`' if chars.peek().is_some_and(|(_, n)| *n == '`') => {
                    name.push('`');
                    chars.next();
                }
                '`' => return Some((name, &rest[i + 1..])),
                c => name.push(c),
            }
        }
        return None;
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    (end > 0).then(|| (s[..end].to_string(), &s[end..]))
}

/// `KEYWORD rest` → `rest` (case-insensitive, whole word).
fn after_word<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = s.trim_start();
    let head = s.get(..kw.len())?;
    (head.eq_ignore_ascii_case(kw) && s[kw.len()..].starts_with(char::is_whitespace)).then(|| s[kw.len()..].trim_start())
}

/// What `create_table_query` has besides columns and data-skipping
/// indexes (system.data_skipping_indices has those): CHECK constraints,
/// ASSUME constraints (name, expression) and projections (as indexes).
pub fn table_elements(create: &str) -> (Vec<CheckDef>, Vec<(String, String)>, Vec<IndexDef>) {
    let (mut checks, mut assumes, mut projections) = (Vec::new(), Vec::new(), Vec::new());
    // The element list: the first parenthesis outside quotes, to its match.
    let (mut quote, mut depth, mut prev) = (None::<char>, 0, ' ');
    let (mut open, mut close) = (None, None);
    for (i, c) in create.char_indices() {
        match quote {
            Some(q) => {
                if c == q && prev != '\\' {
                    quote = None;
                }
            }
            None => match c {
                '`' | '\'' | '"' => quote = Some(c),
                '(' => {
                    depth += 1;
                    open = open.or(Some(i));
                }
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            },
        }
        prev = if prev == '\\' { ' ' } else { c };
    }
    let (Some(open), Some(close)) = (open, close) else { return (checks, assumes, projections) };
    for item in split_top(&create[open + 1..close]) {
        if let Some(rest) = after_word(&item, "CONSTRAINT") {
            let Some((name, rest)) = leading_name(rest) else { continue };
            if let Some(e) = after_word(rest, "CHECK") {
                checks.push(CheckDef { name: Some(name), expression: e.trim().to_string() });
            } else if let Some(e) = after_word(rest, "ASSUME") {
                assumes.push((name, e.trim().to_string()));
            }
        } else if let Some(rest) = after_word(&item, "PROJECTION") {
            let Some((name, body)) = leading_name(rest) else { continue };
            projections.push(IndexDef { name, columns: vec![body.trim().to_string()], kind: Some(PROJECTION.into()), ..Default::default() });
        }
    }
    (checks, assumes, projections)
}

/// Raw catalog rows.
pub struct Catalog {
    /// name, engine, engine_full, comment, sorting_key, primary_key,
    /// partition_key, sampling_key, create_table_query
    pub tables: Vec<Vec<Value>>,
    /// table, name, type, default_kind, default_expression, comment, compression_codec
    pub columns: Vec<Vec<Value>>,
    /// table, name, type_full, expr, granularity
    pub indexes: Vec<Vec<Value>>,
}

fn s(r: &[Value], i: usize) -> String {
    r.get(i).map(crate::text).unwrap_or_default()
}

pub fn assemble(flavor: Flavor, database: &str, cat: Catalog) -> Vec<TableSchema> {
    let mut tables: BTreeMap<String, TableSchema> = BTreeMap::new();
    for r in &cat.tables {
        let name = s(r, 0);
        let engine = s(r, 1);
        let mut options = BTreeMap::new();
        let mut primary_key = None;
        match flavor {
            Flavor::ClickHouse => {
                let clauses = engine_clauses(&s(r, 2));
                let head = clauses.get("engine").cloned().unwrap_or_default();
                match head.find('(') {
                    Some(p) if head.ends_with(')') => {
                        options.insert("engine".to_string(), head[..p].to_string());
                        let args = head[p + 1..head.len() - 1].trim().to_string();
                        if !args.is_empty() {
                            options.insert("engine_args".to_string(), args);
                        }
                    }
                    _ => {
                        options.insert("engine".to_string(), if head.is_empty() { engine.clone() } else { head });
                    }
                }
                let sorting = s(r, 4);
                let pk = key_parts(&s(r, 5));
                if !sorting.is_empty() && key_parts(&sorting) != pk {
                    options.insert("order_by".to_string(), tuple(&sorting));
                }
                for (k, i) in [("partition_by", 6), ("sample_by", 7)] {
                    let v = s(r, i);
                    if !v.is_empty() {
                        options.insert(k.to_string(), tuple(&v));
                    }
                }
                for k in ["ttl", "settings"] {
                    if let Some(v) = clauses.get(k).filter(|v| !v.is_empty()) {
                        options.insert(k.to_string(), v.clone());
                    }
                }
                if !pk.is_empty() {
                    primary_key = Some(KeyDef { name: None, columns: pk });
                }
            }
            Flavor::Timeplus => {
                let full = s(r, 2);
                if let Some(p) = full.find("mode = '") {
                    let m: String = full[p + 8..].chars().take_while(|c| *c != '\'').collect();
                    if m != "append" {
                        options.insert("mode".to_string(), m);
                    }
                }
                let pk = key_parts(&s(r, 5));
                // Append streams report the internal _tp_time sort key.
                if options.contains_key("mode") && !pk.is_empty() {
                    primary_key = Some(KeyDef { name: None, columns: pk });
                }
            }
        }
        let (checks, assumes, projections) = match flavor {
            Flavor::ClickHouse => table_elements(&s(r, 8)),
            Flavor::Timeplus => Default::default(),
        };
        for (n, e) in assumes {
            options.insert(format!("{ASSUME}{n}"), e);
        }
        tables.insert(
            name.clone(),
            TableSchema {
                kind: crate::kind_of(&engine, flavor).to_string(),
                schema: Some(database.to_string()),
                name,
                comment: Some(s(r, 3)).filter(|c| !c.is_empty()),
                primary_key,
                // Projections after the data-skipping indexes.
                indexes: projections,
                checks,
                options,
                ..Default::default()
            },
        );
    }
    for r in &cat.columns {
        let Some(t) = tables.get_mut(&s(r, 0)) else { continue };
        let name = s(r, 1);
        if flavor == Flavor::Timeplus && name.starts_with("_tp_") {
            continue;
        }
        let (data_type, nullable) = unwrap_nullable(&s(r, 2));
        let (kind, expr) = (s(r, 3), s(r, 4));
        let mut options = BTreeMap::new();
        if !kind.is_empty() && kind != "DEFAULT" {
            options.insert("default_kind".to_string(), kind);
        }
        let codec = s(r, 6);
        if !codec.is_empty() {
            options.insert("codec".to_string(), codec);
        }
        t.columns.push(ColumnDef {
            name,
            data_type,
            nullable,
            default_value: Some(expr).filter(|e| !e.is_empty()),
            comment: Some(s(r, 5)).filter(|c| !c.is_empty()),
            options,
            ..Default::default()
        });
    }
    for r in &cat.indexes {
        let Some(t) = tables.get_mut(&s(r, 0)) else { continue };
        // Timeplus's own indexes on _tp_time / _tp_sn.
        if flavor == Flavor::Timeplus && s(r, 1).starts_with("_tp_") {
            continue;
        }
        let gran = s(r, 4);
        let mut kind = s(r, 2);
        if !gran.is_empty() && gran != "1" {
            kind.push_str(&format!(" GRANULARITY {gran}"));
        }
        let at = t.indexes.iter().take_while(|i| !is_projection(i)).count();
        t.indexes.insert(at, IndexDef { name: s(r, 1), columns: key_parts(&s(r, 3)), kind: Some(kind), ..Default::default() });
    }
    tables.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filtered_browse_with_backslash_literals() {
        use dbine_driver::{ColumnFilter, FilterOp};
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        let filters = [
            f("name", FilterOp::Eq, vec![json!("O'Brien")]),
            f("note", FilterOp::Contains, vec![json!("50%")]),
            f("n", FilterOp::Gt, vec![json!(7)]),
            f("gone", FilterOp::IsNull, vec![]),
            f("id", FilterOp::In, vec![json!(1), json!(2)]),
            f("ok", FilterOp::IsTrue, vec![]),
        ];
        assert_eq!(
            filtered_browse("SELECT *\nFROM `db`.`t`\nLIMIT 200", &filters).unwrap(),
            "SELECT *\nFROM `db`.`t`\nWHERE `name` = 'O\\'Brien'\n  AND `note` LIKE '%50\\\\%%'\n  AND `n` > 7\n  AND `gone` IS NULL\n  AND `id` IN (1, 2)\n  AND `ok` = true\nLIMIT 200"
        );
        // Timeplus streams read through table().
        assert_eq!(
            filtered_browse("SELECT *\nFROM table(`s`)\nLIMIT 50", &filters[2..3]).unwrap(),
            "SELECT *\nFROM table(`s`)\nWHERE `n` > 7\nLIMIT 50"
        );
    }

    fn t() -> TableSchema {
        TableSchema {
            schema: Some("db".into()),
            name: "eventos".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "UInt64".into(), nullable: false, ..Default::default() },
                ColumnDef { name: "fecha".into(), data_type: "DateTime".into(), nullable: false, default_value: Some("now()".into()), ..Default::default() },
                ColumnDef {
                    name: "texto".into(),
                    data_type: "LowCardinality(String)".into(),
                    nullable: true,
                    comment: Some("it's".into()),
                    options: [("codec".to_string(), "ZSTD(1)".to_string())].into(),
                    ..Default::default()
                },
                ColumnDef {
                    name: "dia".into(),
                    data_type: "Date".into(),
                    nullable: false,
                    default_value: Some("toDate(fecha)".into()),
                    options: [("default_kind".to_string(), "MATERIALIZED".to_string())].into(),
                    ..Default::default()
                },
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            indexes: vec![IndexDef { name: "ix_texto".into(), columns: vec!["texto".into()], kind: Some("bloom_filter(0.01)".into()), ..Default::default() }],
            comment: Some("Eventos".into()),
            options: [
                ("engine".to_string(), "ReplacingMergeTree".to_string()),
                ("partition_by".to_string(), "toYYYYMM(fecha)".to_string()),
                ("order_by".to_string(), "(id, fecha)".to_string()),
            ]
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn update_script_as_mutations() {
        let c = RowChange {
            key: vec![("id".into(), json!(7)), ("region".into(), Value::Null)],
            set: vec![("nombre".into(), json!("O'Brien")), ("baja".into(), Value::Null)], ..Default::default()
        };
        assert_eq!(
            update_script(Flavor::ClickHouse, Some("db"), "clientes", &[c.clone(), RowChange::default()]),
            "ALTER TABLE `db`.`clientes` UPDATE `nombre` = 'O\\'Brien', `baja` = NULL WHERE `id` = 7 AND `region` IS NULL;"
        );
        let all = RowChange { key: vec![], set: vec![("activo".into(), json!(false))], ..Default::default() };
        assert_eq!(
            update_script(Flavor::Timeplus, None, "eventos", &[all]),
            "ALTER STREAM `eventos` UPDATE `activo` = false WHERE 1;"
        );
    }

    #[test]
    fn delete_script_as_mutations() {
        let keys = vec![vec![("nombre".into(), json!("O'Brien")), ("region".into(), Value::Null)], vec![]];
        assert_eq!(
            delete_script(Flavor::ClickHouse, Some("db"), "clientes", &keys),
            "ALTER TABLE `db`.`clientes` DELETE WHERE `nombre` = 'O\\'Brien' AND `region` IS NULL;"
        );
        assert_eq!(
            delete_script(Flavor::Timeplus, None, "eventos", &keys),
            "ALTER STREAM `eventos` DELETE WHERE `nombre` = 'O\\'Brien' AND `region` IS NULL;"
        );
    }

    #[test]
    fn merge_tree_table() {
        let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
        let s = table_ddl(Flavor::ClickHouse, &t(), all);
        assert!(s.starts_with("DROP TABLE IF EXISTS `db`.`eventos`;\nCREATE TABLE `db`.`eventos`\n("), "{s}");
        assert!(s.contains("`texto` LowCardinality(Nullable(String)) CODEC(ZSTD(1)) COMMENT 'it\\'s',"), "{s}");
        assert!(s.contains("`dia` Date MATERIALIZED toDate(fecha)"));
        assert!(s.contains("`fecha` DateTime DEFAULT now()"));
        assert!(s.contains("    INDEX `ix_texto` texto TYPE bloom_filter(0.01) GRANULARITY 1\n)"), "{s}");
        assert!(s.contains("ENGINE = ReplacingMergeTree\nPARTITION BY toYYYYMM(fecha)\nPRIMARY KEY `id`\nORDER BY (id, fecha)\nCOMMENT 'Eventos';"), "{s}");
    }

    #[test]
    fn defaults_and_separate_indexes() {
        let mut tt = t();
        tt.options.clear();
        tt.columns[0].nullable = true;
        let s = table_ddl(Flavor::ClickHouse, &tt, DdlParts { create: true, ..Default::default() });
        assert!(s.contains("ENGINE = MergeTree\nORDER BY `id`"), "{s}");
        assert!(s.contains("`id` Nullable(UInt64)"));
        let ix = table_ddl(Flavor::ClickHouse, &tt, DdlParts { indexes: true, if_exists: true, ..Default::default() });
        assert_eq!(ix, "ALTER TABLE `db`.`eventos` ADD INDEX IF NOT EXISTS `ix_texto` texto TYPE bloom_filter(0.01) GRANULARITY 1;");
        tt.options.insert("engine".into(), "Memory".into());
        let s = table_ddl(Flavor::ClickHouse, &tt, DdlParts { create: true, ..Default::default() });
        assert!(s.contains("ENGINE = Memory\nCOMMENT"), "{s}");
    }

    #[test]
    fn timeplus_stream() {
        let mut tt = t();
        tt.options = [("mode".to_string(), "versioned_kv".to_string())].into();
        tt.columns[2].data_type = "string".into();
        let s = table_ddl(Flavor::Timeplus, &tt, DdlParts { create: true, if_exists: true, ..Default::default() });
        assert!(s.starts_with("CREATE STREAM IF NOT EXISTS `db`.`eventos`"), "{s}");
        assert!(s.contains("`texto` nullable(string)"));
        assert!(s.contains("PRIMARY KEY `id`\nSETTINGS mode = 'versioned_kv'"), "{s}");
    }

    #[test]
    fn constraints_and_projections_from_the_create_statement() {
        let (checks, assumes, projections) = table_elements(
            "CREATE TABLE `db`.`t (x)` (`id` UInt64, `x` String DEFAULT 'a)(', INDEX ix x TYPE bloom_filter(0.01) GRANULARITY 3, \
             CONSTRAINT c1 CHECK (id > 0) AND (x != 'a,b'), CONSTRAINT `c 2` ASSUME d > '2000-01-01', \
             PROJECTION p (SELECT x, count() GROUP BY x), PROJECTION `p2` (SELECT * ORDER BY d)) ENGINE = MergeTree ORDER BY id",
        );
        assert_eq!(checks, [CheckDef { name: Some("c1".into()), expression: "(id > 0) AND (x != 'a,b')".into() }]);
        assert_eq!(assumes, [("c 2".to_string(), "d > '2000-01-01'".to_string())]);
        let p: Vec<(&str, &str)> = projections.iter().map(|i| (i.name.as_str(), i.columns[0].as_str())).collect();
        assert_eq!(p, [("p", "(SELECT x, count() GROUP BY x)"), ("p2", "(SELECT * ORDER BY d)")]);
        assert!(projections.iter().all(is_projection));

        let mut tt = t();
        tt.checks = checks;
        tt.options.insert(format!("{ASSUME}c 2"), "d > '2000-01-01'".into());
        tt.indexes.extend(projections);
        let s = table_ddl(Flavor::ClickHouse, &tt, DdlParts { create: true, indexes: true, ..Default::default() });
        assert!(
            s.contains("GRANULARITY 1,\n    CONSTRAINT `c1` CHECK (id > 0) AND (x != 'a,b'),\n    CONSTRAINT `c 2` ASSUME d > '2000-01-01',\n    PROJECTION `p` (SELECT x, count() GROUP BY x),\n    PROJECTION `p2` (SELECT * ORDER BY d)\n)"),
            "{s}"
        );
        let apart = table_ddl(Flavor::ClickHouse, &tt, DdlParts { indexes: true, if_exists: true, ..Default::default() });
        assert!(apart.ends_with("ALTER TABLE `db`.`eventos` ADD PROJECTION IF NOT EXISTS `p2` (SELECT * ORDER BY d);"), "{apart}");
    }

    #[test]
    fn engine_full_parses() {
        let c = engine_clauses("ReplacingMergeTree(ver) PARTITION BY toYYYYMM(d) PRIMARY KEY id ORDER BY (id, d) TTL d + toIntervalDay(1) SETTINGS index_granularity = 8192");
        assert_eq!(c["engine"], "ReplacingMergeTree(ver)");
        assert_eq!(c["order_by"], "(id, d)");
        assert_eq!(c["ttl"], "d + toIntervalDay(1)");
        assert_eq!(c["settings"], "index_granularity = 8192");
        assert_eq!(key_parts("(id, cityHash64(a, b))"), vec!["id", "cityHash64(a, b)"]);
        assert_eq!(key_parts("`a b`"), vec!["a b"]);
    }

    #[test]
    fn inserts_escape_backslashes() {
        let s = insert_script(None, "t", &["a".into(), "b".into()], &[vec![json!("C:\\x'y"), json!(true)], vec![Value::Null, json!(2)]]);
        assert_eq!(s, "INSERT INTO `t` (`a`, `b`) VALUES\n  ('C:\\\\x\\'y', true),\n  (NULL, 2);");
    }
}
