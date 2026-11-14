//! A model's metadata, from the richest tier the user can read (spec §4.3).
//! Owned by part C; part B reads [`ModelMeta`] (browse of hierarchies,
//! column types, keys for paging, columns of an empty JSON result).
//!
//! - Tier A ([`info`]): `INFO.VIEW.*` (Read/Build users, a live check) and,
//!   when the user may, `INFO.*` (model admin): everything. Arrow only.
//! - Tier B ([`crate::definition`]): Fabric `getDefinition` (TMSL), for
//!   users with write permission (decision D2, on by default).
//! - Tier C ([`stats`]): `COLUMNSTATISTICS()`: tables and columns only.
//!
//! Picked per model on first need; the result is cached per `(account,
//! model)` for 60 s in the driver host. `COLUMNSTATISTICS` Min/Max values
//! are used for type inference and dropped: they never reach `ModelMeta`,
//! logs or the explorer cache (S4). Partition sources with embedded data
//! (`Binary.FromText("…")`) keep a placeholder instead (S4).

pub mod info;
pub(crate) mod rows;
pub(crate) mod service;
pub mod stats;

#[cfg(test)]
pub(crate) mod fake;

use crate::catalog::Model;
use crate::config::QueryApi;
use crate::query::Target;
use crate::Ctx;
use dbine_driver::{Error, Result};
use service::Service;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where the metadata came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// `INFO.VIEW.*` / `INFO.*` over `executeDaxQueries`.
    Info,
    /// Fabric's definition API (TMSL).
    Definition,
    /// `COLUMNSTATISTICS()`: tables and columns only.
    #[default]
    Stats,
}

impl Tier {
    /// How properties and notes name it (Spanish).
    pub fn label(self) -> &'static str {
        match self {
            Tier::Info => "funciones INFO (executeDaxQueries)",
            Tier::Definition => "definición del modelo (API de Fabric)",
            Tier::Stats => "estadísticas de columnas (COLUMNSTATISTICS)",
        }
    }
}

/// A model column's data type (TOM's names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataType {
    Int64,
    /// Fixed decimal (currency, four decimals).
    Decimal,
    Double,
    String,
    DateTime,
    Boolean,
    Binary,
    Variant,
    #[default]
    Unknown,
}

impl DataType {
    /// The name `ColumnInfo::data_type` carries (and the schema dialect
    /// `powerbi` reads).
    pub fn name(self) -> &'static str {
        match self {
            DataType::Int64 => "Int64",
            DataType::Decimal => "Decimal",
            DataType::Double => "Double",
            DataType::String => "String",
            DataType::DateTime => "DateTime",
            DataType::Boolean => "Boolean",
            DataType::Binary => "Binary",
            DataType::Variant => "Variant",
            DataType::Unknown => "Unknown",
        }
    }

    /// A type as TMSL (`int64`, `dateTime`), `INFO.VIEW.COLUMNS` (`Text`,
    /// `Whole Number`, `Date`…) or the `INFO.COLUMNS` codes (`2`, `6`…)
    /// spell it.
    pub fn parse(s: &str) -> DataType {
        match s.trim().to_ascii_lowercase().as_str() {
            "string" | "text" | "2" => DataType::String,
            "int64" | "integer" | "whole number" | "wholenumber" | "6" => DataType::Int64,
            "double" | "decimal number" | "decimalnumber" | "8" => DataType::Double,
            "decimal" | "fixed decimal number" | "fixeddecimalnumber" | "currency" | "10" => {
                DataType::Decimal
            }
            "datetime" | "date" | "date/time" | "time" | "9" => DataType::DateTime,
            "boolean" | "true/false" | "11" => DataType::Boolean,
            "binary" | "17" => DataType::Binary,
            "variant" | "20" => DataType::Variant,
            _ => DataType::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TableKind {
    /// Imported, DirectQuery, Direct Lake.
    #[default]
    Data,
    /// A calculated table (DAX).
    Calculated,
    /// A calculation group's table.
    CalculationGroup,
    /// Auto date/time (`LocalDateTable_*`, `DateTableTemplate_*`).
    AutoDate,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    /// The DAX of a calculated column.
    pub expression: Option<String>,
    pub description: Option<String>,
    pub hidden: bool,
    pub key: bool,
    pub unique: bool,
    pub nullable: bool,
    /// Distinct values (`COLUMNSTATISTICS`), when known.
    pub cardinality: Option<u64>,
    pub format_string: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Partition {
    pub name: String,
    /// `m`, `calculated`, `entity`, `policyRange`…
    pub source_kind: Option<String>,
    /// M or DAX source, embedded data replaced by a placeholder.
    pub expression: Option<String>,
    /// `import`, `directQuery`, `directLake`, `dual`.
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub name: String,
    pub kind: TableKind,
    pub description: Option<String>,
    pub hidden: bool,
    /// A calculated table's DAX.
    pub expression: Option<String>,
    pub columns: Vec<Column>,
    pub partitions: Vec<Partition>,
    /// Rows (`INFO.STORAGETABLES`, or `RowNumber` cardinality).
    pub rows: Option<u64>,
    pub storage_mode: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measure {
    /// Home table.
    pub table: String,
    pub name: String,
    pub expression: Option<String>,
    pub description: Option<String>,
    /// The result's type, when the tier says it (`INFO.VIEW.MEASURES[DataType]`).
    pub data_type: Option<DataType>,
    pub format_string: Option<String>,
    pub display_folder: Option<String>,
    pub hidden: bool,
    /// `INFO.VIEW.MEASURES[State]` (`Ready`, `SemanticError`…).
    pub state: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Relationship {
    pub name: Option<String>,
    /// The many side.
    pub from_table: String,
    pub from_column: String,
    /// The one side.
    pub to_table: String,
    pub to_column: String,
    /// `one` / `many`, each end.
    pub from_cardinality: Option<String>,
    pub to_cardinality: Option<String>,
    /// `oneDirection` / `bothDirections`.
    pub cross_filtering: Option<String>,
    pub active: bool,
    pub security_filtering: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Level {
    pub name: String,
    pub column: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hierarchy {
    pub table: String,
    pub name: String,
    pub levels: Vec<Level>,
    pub description: Option<String>,
    pub hidden: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CalculationItem {
    pub name: String,
    pub expression: Option<String>,
    pub ordinal: Option<i64>,
    pub format_string_expression: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CalculationGroup {
    pub table: String,
    pub precedence: Option<i64>,
    pub items: Vec<CalculationItem>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Role {
    pub name: String,
    pub description: Option<String>,
    /// `read`, `readRefresh`, `administrator`…
    pub model_permission: Option<String>,
    /// `(table, DAX filter)`.
    pub table_filters: Vec<(String, String)>,
    /// `(table, column, permission)`: object-level security (G15).
    pub column_permissions: Vec<(String, String, String)>,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Perspective {
    pub name: String,
    pub description: Option<String>,
    pub tables: Vec<String>,
    /// `(table, column)`.
    pub columns: Vec<(String, String)>,
    /// `(table, measure)`.
    pub measures: Vec<(String, String)>,
}

/// A shared M expression or parameter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NamedExpression {
    pub name: String,
    /// `m`, `parameter`…
    pub kind: Option<String>,
    pub expression: Option<String>,
    pub description: Option<String>,
}

/// A DAX user-defined function of the model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Function {
    pub name: String,
    pub expression: Option<String>,
    pub description: Option<String>,
}

/// Everything the explorer, the definitions and the rest read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelMeta {
    pub tier: Tier,
    pub tables: Vec<Table>,
    pub measures: Vec<Measure>,
    pub relationships: Vec<Relationship>,
    pub hierarchies: Vec<Hierarchy>,
    pub calculation_groups: Vec<CalculationGroup>,
    pub roles: Vec<Role>,
    pub perspectives: Vec<Perspective>,
    pub expressions: Vec<NamedExpression>,
    pub functions: Vec<Function>,
    pub compatibility_level: Option<i64>,
    pub culture: Option<String>,
    /// What this tier couldn't read and why (Spanish): properties, the
    /// health check and engine support say it.
    pub notes: Vec<String>,
}

impl ModelMeta {
    /// A table by name (any case).
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| same(&t.name, name))
    }

    pub fn measure(&self, name: &str) -> Option<&Measure> {
        self.measures.iter().find(|m| same(&m.name, name))
    }

    /// The tables the explorer lists as tables: no calculation groups, no
    /// auto date/time.
    pub fn plain_tables(&self) -> impl Iterator<Item = &Table> {
        self.tables
            .iter()
            .filter(|t| matches!(t.kind, TableKind::Data | TableKind::Calculated))
    }

    /// The relationships the explorer, the schema and the checks show:
    /// none to or from an auto date/time table, which are hidden like the
    /// tables themselves (the `auto_date_time` health check covers them).
    pub fn model_relationships(&self) -> impl Iterator<Item = &Relationship> {
        self.relationships
            .iter()
            .filter(|r| !is_auto_date(&r.from_table) && !is_auto_date(&r.to_table))
    }

    /// A table's key: its `IsKey` columns, else the one column other tables'
    /// relationships point at (the one side), when there's exactly one.
    pub fn key_columns(&self, table: &str) -> Vec<String> {
        let Some(t) = self.table(table) else {
            return Vec::new();
        };
        let keys: Vec<String> = t
            .columns
            .iter()
            .filter(|c| c.key)
            .map(|c| c.name.clone())
            .collect();
        if !keys.is_empty() {
            return keys;
        }
        let mut ones: Vec<String> = Vec::new();
        for r in &self.relationships {
            if same(&r.to_table, table) && is_one(r.to_cardinality.as_deref(), true) {
                push_unique(&mut ones, &r.to_column);
            }
            if same(&r.from_table, table) && is_one(r.from_cardinality.as_deref(), false) {
                push_unique(&mut ones, &r.from_column);
            }
        }
        if ones.len() == 1 {
            ones
        } else {
            Vec::new()
        }
    }
}

/// `one` at that end (`default`: what an absent value means there).
pub fn is_one(cardinality: Option<&str>, default: bool) -> bool {
    match cardinality {
        Some(c) => c.eq_ignore_ascii_case("one"),
        None => default,
    }
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| same(x, s)) {
        v.push(s.to_string());
    }
}

/// Model names compare case-insensitively (as the engine does).
pub fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

/// `LocalDateTable_<guid>` / `DateTableTemplate_<guid>`: the tables Power
/// BI Desktop adds for its auto date/time.
pub fn is_auto_date(table: &str) -> bool {
    table.starts_with("LocalDateTable_") || table.starts_with("DateTableTemplate_")
}

/// `'Table'[Hierarchy]`: how the explorer names a hierarchy.
pub fn hierarchy_name(h: &Hierarchy) -> String {
    crate::dax::quote::column_ref(&h.table, &h.name)
}

/// `'Sales'[CustomerKey] → 'Customer'[CustomerKey]`: how the explorer names
/// a relationship (many side first).
pub fn relationship_name(r: &Relationship) -> String {
    format!(
        "{} → {}",
        crate::dax::quote::column_ref(&r.from_table, &r.from_column),
        crate::dax::quote::column_ref(&r.to_table, &r.to_column)
    )
}

/// `OneDirection` → `oneDirection`, `Many` → `many`: the INFO functions'
/// spelling as TMSL writes it.
pub fn camel(s: &str) -> String {
    let mut c = s.trim().chars();
    match c.next() {
        Some(f) => f.to_lowercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Puts the many side first: `from` is the many side of the model's
/// relationships, but nothing stops a reader from getting them the other
/// way round.
pub fn normalize_relationship(mut r: Relationship) -> Relationship {
    r.from_cardinality = r.from_cardinality.map(|c| camel(&c));
    r.to_cardinality = r.to_cardinality.map(|c| camel(&c));
    r.cross_filtering = r.cross_filtering.map(|c| camel(&c));
    r.security_filtering = r.security_filtering.map(|c| camel(&c));
    if is_one(r.from_cardinality.as_deref(), false) && !is_one(r.to_cardinality.as_deref(), true) {
        std::mem::swap(&mut r.from_table, &mut r.to_table);
        std::mem::swap(&mut r.from_column, &mut r.to_column);
        std::mem::swap(&mut r.from_cardinality, &mut r.to_cardinality);
    }
    r
}

/// Whether a name is a GUID (TMSL names relationships with one).
pub fn is_guid_like(s: &str) -> bool {
    crate::catalog::valid_guid(s)
}

/// What a partition or expression's M is shown as (S4): the data an "Enter
/// data" table carries (`Binary.FromText("…")`) becomes a placeholder,
/// secret-looking query-string values in URLs (`key=`, `token=`, `sig=`…)
/// become `***`, and so do the text values of record fields and `let`
/// names that look like secrets (`[Headers=[Authorization="Bearer …"]]`,
/// `Query=[api_key="…"]`, `#"Ocp-Apim-Subscription-Key"="…"`). The rest of
/// the M stays as written.
pub fn redact_m(m: &str) -> String {
    const BLOB: &str = "Binary.FromText(";
    let mut out = String::with_capacity(m.len());
    let mut rest = m;
    while let Some(i) = rest.find(BLOB) {
        let after = i + BLOB.len();
        out.push_str(&rest[..after]);
        let tail = &rest[after..];
        let ws = tail.len() - tail.trim_start().len();
        let lit = &tail[ws..];
        match m_string_end(lit) {
            Some(end) => {
                out.push_str(&tail[..ws]);
                out.push_str("\"(datos incrustados omitidos)\"");
                rest = &lit[end..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    redact_field_secrets(&redact_url_secrets(&out))
}

/// Whether an M field (or `let`) name holds a secret: compared without
/// case, `-`, `_` or spaces.
fn is_secret_field(name: &str) -> bool {
    const IN: &[&str] = &[
        "authorization",
        "apikey",
        "token",
        "secret",
        "password",
        "passwd",
        "credential",
        "signature",
        "accesskey",
        "sharedaccess",
    ];
    const EXACT: &[&str] = &["key", "sig", "pwd", "code", "auth", "sas", "bearer"];
    let n: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    IN.iter().any(|s| n.contains(s)) || EXACT.contains(&n.as_str()) || n.ends_with("key")
}

/// `name = "…"` and `#"name" = "…"` (record fields, `let` names) whose
/// name looks like a secret get `"***"` as their text. Comments and other
/// strings are copied as they are; an unterminated string or comment ends
/// the scan with nothing lost.
fn redact_field_secrets(m: &str) -> String {
    let b = m.as_bytes();
    let mut out = String::with_capacity(m.len());
    // The last name read, then whether a `=` after it made the next string
    // its value (and a secret one).
    let mut name: Option<String> = None;
    let mut value_is_secret: Option<bool> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            i = m[i..].find('\n').map_or(b.len(), |n| i + n);
            out.push_str(&m[start..i]);
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i = m[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
            out.push_str(&m[start..i]);
            continue;
        }
        if c == b'"' || (c == b'#' && b.get(i + 1) == Some(&b'"')) {
            let quote = if c == b'#' { i + 1 } else { i };
            let Some(end) = m_string_end(&m[quote..]) else {
                out.push_str(&m[start..]);
                return out;
            };
            i = quote + end;
            if c == b'#' {
                name = Some(m[quote + 1..i - 1].replace("\"\"", "\""));
                value_is_secret = None;
                out.push_str(&m[start..i]);
            } else {
                if value_is_secret == Some(true) {
                    out.push_str("\"***\"");
                } else {
                    out.push_str(&m[start..i]);
                }
                name = None;
                value_is_secret = None;
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'.') || b[i] >= 0x80)
            {
                i += 1;
            }
            name = Some(m[start..i].to_string());
            value_is_secret = None;
            out.push_str(&m[start..i]);
            continue;
        }
        i += 1;
        if c == b'=' && b.get(i) != Some(&b'>') {
            value_is_secret = name.take().map(|n| is_secret_field(&n));
        } else if !c.is_ascii_whitespace() {
            name = None;
            value_is_secret = None;
        }
        out.push_str(&m[start..i]);
    }
    out
}

/// The byte just past an M string literal that starts `text` (`"…"`, a
/// quote doubled inside); `None` when `text` doesn't start with one or it
/// never ends.
fn m_string_end(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    if b.first() != Some(&b'"') {
        return None;
    }
    let mut i = 1;
    while i < b.len() {
        if b[i] == b'"' {
            if b.get(i + 1) == Some(&b'"') {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn redact_url_secrets(m: &str) -> String {
    const SECRET: &[&str] = &[
        "key",
        "token",
        "secret",
        "password",
        "pwd",
        "sig",
        "signature",
        "code",
        "auth",
    ];
    let mut out = String::with_capacity(m.len());
    let mut rest = m;
    while let Some(q) = rest.find('"') {
        out.push_str(&rest[..q]);
        let lit = &rest[q..];
        let Some(end) = m_string_end(lit) else {
            out.push_str(lit);
            return out;
        };
        let body = &lit[1..end - 1];
        if body.contains("://") && body.contains('?') {
            let (base, query) = body.split_once('?').unwrap_or((body, ""));
            let pairs: Vec<String> = query
                .split('&')
                .map(|p| match p.split_once('=') {
                    Some((k, _)) if SECRET.iter().any(|s| k.to_ascii_lowercase().contains(s)) => {
                        format!("{k}=***")
                    }
                    _ => p.to_string(),
                })
                .collect();
            out.push('"');
            out.push_str(base);
            out.push('?');
            out.push_str(&pairs.join("&"));
            out.push('"');
        } else {
            out.push_str(&lit[..end]);
        }
        rest = &lit[end..];
    }
    out.push_str(rest);
    out
}

/// What the session caches (the host-wide cache is in this module).
#[derive(Debug, Default)]
pub struct MetaState {
    pub cached: Option<Arc<ModelMeta>>,
    /// The model `cached` is for, and when it was read.
    pub dataset: Option<String>,
    pub at: Option<Instant>,
    /// Which admin `INFO` functions work per model, when the account isn't
    /// known (else it's host-wide, [`info`]).
    pub admin_works: BTreeMap<String, Vec<&'static str>>,
}

/// How long a model's metadata is kept (spec §4.3).
pub const TTL: Duration = Duration::from_secs(60);

struct Cached {
    at: Instant,
    meta: Arc<ModelMeta>,
}

static HOST: Mutex<BTreeMap<String, Cached>> = Mutex::new(BTreeMap::new());

/// The host-wide key of a model's metadata for this connection's account;
/// `None` before the account is known (the session keeps it then). The
/// options that change the tier are part of it.
pub(crate) fn host_key<S: Service>(s: &S, model: &Model) -> Option<String> {
    let a = s.account()?;
    let st = s.settings();
    Some(format!(
        "{:?}|{}:{}|{}|{}|{:?}",
        st.cloud, a.tenant_id, a.object_id, model.id, st.definition_api, st.query_api
    ))
}

/// The session's model's metadata (cached 60 s per account and model).
pub async fn model(cx: &mut Ctx) -> Result<Arc<ModelMeta>> {
    let m = crate::catalog::model(cx)?.clone();
    model_for(cx, &m).await
}

/// The tier the session's model is read with.
pub async fn tier(cx: &mut Ctx) -> Result<Tier> {
    Ok(model(cx).await?.tier)
}

/// Forget the cached metadata (after a refresh, a rename…).
pub fn invalidate(cx: &mut Ctx) {
    forget(cx, None);
}

/// Forget a model's metadata, in the session and host-wide (`None`: the
/// session's).
pub(crate) fn forget<S: Service>(s: &mut S, model: Option<&Model>) {
    let st = s.meta_state();
    let dataset = model.map(|m| m.id.clone()).or_else(|| st.dataset.clone());
    if model.is_none() || st.dataset == dataset {
        st.cached = None;
        st.dataset = None;
        st.at = None;
    }
    if let Some(d) = dataset {
        if let Ok(mut host) = HOST.lock() {
            host.retain(|k, _| !k.contains(&format!("|{d}|")));
        }
    }
}

/// A model's metadata, from the caches or read now.
pub(crate) async fn model_for<S: Service>(s: &mut S, m: &Model) -> Result<Arc<ModelMeta>> {
    {
        let st = s.meta_state();
        if let (Some(c), Some(d), Some(at)) = (&st.cached, &st.dataset, st.at) {
            if *d == m.id && at.elapsed() < TTL {
                return Ok(c.clone());
            }
        }
    }
    let key = host_key(s, m);
    if let Some(k) = &key {
        let hit = HOST.lock().ok().and_then(|h| {
            h.get(k)
                .filter(|c| c.at.elapsed() < TTL)
                .map(|c| c.meta.clone())
        });
        if let Some(meta) = hit {
            keep(s, m, meta.clone());
            return Ok(meta);
        }
    }
    let meta = Arc::new(read(s, m).await?);
    if let Some(k) = key {
        if let Ok(mut host) = HOST.lock() {
            host.retain(|_, c| c.at.elapsed() < TTL);
            host.insert(
                k,
                Cached {
                    at: Instant::now(),
                    meta: meta.clone(),
                },
            );
        }
    }
    keep(s, m, meta.clone());
    Ok(meta)
}

fn keep<S: Service>(s: &mut S, m: &Model, meta: Arc<ModelMeta>) {
    let st = s.meta_state();
    st.cached = Some(meta);
    st.dataset = Some(m.id.clone());
    st.at = Some(Instant::now());
}

/// An error that ends the read (the sign-in, the network, a cancel), as
/// opposed to a tier the user can't read, which falls to the next one.
pub(crate) fn is_fatal(e: &Error) -> bool {
    matches!(
        e,
        Error::AuthFailed(_)
            | Error::Connect(_)
            | Error::ConnectFailed(_)
            | Error::Cancelled
            | Error::Io(_)
    )
}

/// One line of an error for a note (no bodies, at most 200 characters).
pub(crate) fn short(e: &Error) -> String {
    let s = e.to_string();
    let s = s.lines().next().unwrap_or("").trim();
    s.chars().take(200).collect()
}

/// Why the definition API isn't used for `m` (`None`: it is offered).
pub(crate) fn definition_unavailable<S: Service>(s: &S, m: &Model) -> Option<&'static str> {
    let st = s.settings();
    if !st.definition_api {
        Some(
            "La lectura de la definición completa (API de Fabric) está desactivada en la conexión.",
        )
    } else if st.cloud.fabric_api().is_none() {
        Some("La API de definición de Fabric solo está en la nube pública.")
    } else if m.workspace.id.is_none() {
        Some("La API de definición de Fabric no se usa con los modelos de «Mi área de trabajo» ni con los compartidos sin acceso a su área de trabajo.")
    } else if !m.write_permission {
        Some("La definición completa (API de Fabric) necesita permiso de escritura en el modelo.")
    } else {
        None
    }
}

/// Reads the model with the first tier that answers.
async fn read<S: Service>(s: &mut S, m: &Model) -> Result<ModelMeta> {
    let target = Target::of(m);
    let mut notes: Vec<String> = Vec::new();
    let arrow = match s.settings().query_api {
        QueryApi::Json => false,
        _ => match s.arrow_available(&target).await {
            Ok(a) => a,
            Err(e) if is_fatal(&e) => return Err(e),
            Err(_) => false,
        },
    };
    if arrow {
        match info::read_model(s, m).await {
            Ok(mut meta) => {
                notes.append(&mut meta.notes);
                meta.notes = notes;
                return Ok(meta);
            }
            Err(e) if is_fatal(&e) => return Err(e),
            Err(e) => notes.push(format!(
                "Las funciones INFO no respondieron con los permisos de esta cuenta ({}).",
                short(&e)
            )),
        }
    }
    match definition_unavailable(s, m) {
        None => match crate::definition::read_model(s, m).await {
            Ok(mut meta) => {
                notes.append(&mut meta.notes);
                meta.notes = notes;
                return Ok(meta);
            }
            Err(e) if is_fatal(&e) => return Err(e),
            Err(e) => notes.push(format!(
                "No se pudo leer la definición del modelo con la API de Fabric ({}).",
                short(&e)
            )),
        },
        Some(why) if !arrow => notes.push(why.to_string()),
        Some(_) => {}
    }
    let mut meta = stats::read_model(s, m).await?;
    notes.append(&mut meta.notes);
    meta.notes = notes;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::fake::{rowset, Fake};
    use super::*;
    use serde_json::json;

    #[test]
    fn types_read_in_every_spelling() {
        for (s, t) in [
            ("int64", DataType::Int64),
            ("Whole Number", DataType::Int64),
            ("Text", DataType::String),
            ("dateTime", DataType::DateTime),
            ("Date", DataType::DateTime),
            ("decimal", DataType::Decimal),
            ("Decimal Number", DataType::Double),
            ("double", DataType::Double),
            ("True/False", DataType::Boolean),
            ("9", DataType::DateTime),
            ("17", DataType::Binary),
            ("variant", DataType::Variant),
            ("whatever", DataType::Unknown),
        ] {
            assert_eq!(DataType::parse(s), t, "{s}");
        }
    }

    #[test]
    fn embedded_data_and_url_secrets_never_show() {
        let m = "let\n    Source = Table.FromRows(Json.Document(Binary.Decompress(Binary.FromText(\"i45WMlTSUTJUitWJVjICcozBHBMgxwTCMQVyTJViYwE=\", BinaryEncoding.Base64), Compression.Deflate)), let _t = ((type nullable text) meta [Serialized.Text = true]) in type table [Id = _t])\nin\n    Source";
        let r = redact_m(m);
        assert!(!r.contains("i45W"), "{r}");
        assert!(
            r.contains("Binary.FromText(\"(datos incrustados omitidos)\", BinaryEncoding.Base64)")
        );
        let web =
            "Web.Contents(\"https://api.example.com/v1/sales?region=west&api_key=abc123&sig=zz\")";
        let r = redact_m(web);
        assert!(!r.contains("abc123") && !r.contains("=zz"), "{r}");
        assert!(r.contains("region=west") && r.contains("api_key=***"));
        // A string with a doubled quote and no URL is left alone.
        assert_eq!(redact_m("\"a\"\"b\" & x"), "\"a\"\"b\" & x");
        // Secrets in records (headers, query) and in let names, plain or
        // #"quoted"; the URL, other fields and comments stay.
        let m = "let\n    // token = \"in a comment\"\n    apiKey = \"k-123\",\n    Source = Json.Document(Web.Contents(\"https://api.example.com/v1\", [Headers=[Authorization=\"Bearer eyJ0e\", #\"Ocp-Apim-Subscription-Key\" = \"s-456\", Accept=\"application/json\"], Query=[api_key=\"q-789\", region=\"west\"]]))\nin\n    Source";
        let r = redact_m(m);
        for secret in ["k-123", "eyJ0e", "s-456", "q-789"] {
            assert!(!r.contains(secret), "{secret}: {r}");
        }
        for kept in [
            "apiKey = \"***\"",
            "Authorization=\"***\"",
            "#\"Ocp-Apim-Subscription-Key\" = \"***\"",
            "api_key=\"***\"",
            "Accept=\"application/json\"",
            "region=\"west\"",
            "\"https://api.example.com/v1\"",
            "// token = \"in a comment\"",
        ] {
            assert!(r.contains(kept), "{kept}: {r}");
        }
        // Comparisons and functions aren't fields.
        let cmp = "each [Token] <> \"x\" and (t) => \"y\" and [Password] >= \"z\"";
        assert_eq!(redact_m(cmp), cmp);
        // Unterminated: no panic, nothing lost.
        assert_eq!(redact_m("token = \"abc"), "token = \"abc");
        assert_eq!(redact_m("x /* token = \"a\""), "x /* token = \"a\"");
        // Unterminated: no panic, nothing lost.
        assert_eq!(redact_m("Binary.FromText(\"abc"), "Binary.FromText(\"abc");
    }

    #[test]
    fn relationships_put_the_many_side_first() {
        let r = normalize_relationship(Relationship {
            from_table: "Customer".into(),
            from_column: "CustomerKey".into(),
            to_table: "Sales".into(),
            to_column: "CustomerKey".into(),
            from_cardinality: Some("One".into()),
            to_cardinality: Some("Many".into()),
            cross_filtering: Some("BothDirections".into()),
            active: true,
            ..Default::default()
        });
        assert_eq!(
            (r.from_table.as_str(), r.to_table.as_str()),
            ("Sales", "Customer")
        );
        assert_eq!(r.cross_filtering.as_deref(), Some("bothDirections"));
        assert_eq!(
            relationship_name(&r),
            "'Sales'[CustomerKey] → 'Customer'[CustomerKey]"
        );
    }

    #[tokio::test]
    async fn the_tiers_fall_through_and_say_why() {
        // No Arrow, no write permission: COLUMNSTATISTICS, with the reason.
        let mut f = Fake::new();
        f.dax_answer(
            "COLUMNSTATISTICS",
            vec![rowset(
                &[
                    "[Table Name]",
                    "[Column Name]",
                    "[Min]",
                    "[Max]",
                    "[Cardinality]",
                    "[Max Length]",
                ],
                vec![json!(["Sales", "Amount", 1, 99, 50, null])],
            )],
        );
        let m = f.model.clone().unwrap();
        let meta = model_for(&mut f, &m).await.unwrap();
        assert_eq!(meta.tier, Tier::Stats);
        assert!(meta
            .notes
            .iter()
            .any(|n| n.contains("permiso de escritura")));
        // Cached: a second read sends nothing.
        let before = f.dax_calls();
        model_for(&mut f, &m).await.unwrap();
        assert_eq!(f.dax_calls(), before);
        // Forgotten: read again.
        forget(&mut f, None);
        model_for(&mut f, &m).await.unwrap();
        assert_eq!(f.dax_calls(), before + 1);
    }

    #[tokio::test]
    async fn the_definition_api_comes_before_the_statistics_for_writers() {
        let mut f = Fake::new();
        f.model.as_mut().unwrap().write_permission = true;
        f.definition(crate::definition::tmsl::tests::SAMPLE);
        let m = f.model.clone().unwrap();
        let meta = model_for(&mut f, &m).await.unwrap();
        assert_eq!(meta.tier, Tier::Definition);
        assert!(!meta.measures.is_empty());
        assert_eq!(f.dax_calls(), 0);
    }

    #[tokio::test]
    async fn arrow_reads_the_info_functions() {
        let mut f = Fake::new();
        f.arrow = true;
        super::info::tests::viewer_answers(&mut f);
        let m = f.model.clone().unwrap();
        let meta = model_for(&mut f, &m).await.unwrap();
        assert_eq!(meta.tier, Tier::Info);
        // The sign-in failing ends the read; it doesn't fall to another tier.
        let mut f = Fake::new();
        f.arrow_error = Some(Error::AuthFailed("token vencido".into()));
        let m = f.model.clone().unwrap();
        assert!(matches!(
            model_for(&mut f, &m).await,
            Err(Error::AuthFailed(_))
        ));
    }
}
