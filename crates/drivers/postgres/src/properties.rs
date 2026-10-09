//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! what the catalogs report about it and what `ALTER DATABASE` changes, in
//! tabs.
//!
//! - PostgreSQL and the variants that keep its `ALTER DATABASE`
//!   (TimescaleDB, EDB, Fujitsu, AlloyDB, Cloud SQL, Aurora, KingbaseES, the
//!   Greenplum family, YugabyteDB, openGauss): owner, connection limit,
//!   ALLOW_CONNECTIONS and IS_TEMPLATE (9.5+, where the variant has them),
//!   tablespace, REFRESH COLLATION VERSION (15+), the comment, and the
//!   per-database defaults of `pg_db_role_setting` (`SET` / `RESET`). Facts:
//!   size, creation time where the server shows it, connections, encoding
//!   and locale, collation versions, XID age.
//! - CockroachDB: owner, regions (primary, added, dropped, secondary),
//!   survival goal, placement, comment and the per-database defaults.
//! - Redshift: owner, connection limit, case sensitivity (COLLATE),
//!   isolation level and comment.
//! - Yellowbrick: owner, connection limit, ALLOW_CONNECTIONS, HOT_STANDBY,
//!   READONLY and MAX_SIZE.
//! - RisingWave: owner, resource group, barrier interval and checkpoint
//!   frequency.
//! - Materialize: owner and comment.
//! - Denodo, CrateDB and H2 have no databases DBine manages: none.
//!
//! Each change is one statement; settings are checked against whitelists
//! or the crate's quoting helpers before they reach the SQL.

use crate::catalog::{cell, lit};
use crate::create_db::{check, ident, int, name_ok, pg_like};
use crate::session::PgSession;
use crate::{err, Variant};
use dbine_driver::serde_static::intern;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{DatabaseProperties, Error, Field, FieldChoices, FieldKind, PropertyInfo, Result};
use std::collections::{BTreeMap, BTreeSet};
use tokio_postgres::SimpleQueryRow;

const CONFIG: &str = "Configuración";
const LOCALE: &str = "Idioma y codificación";
const STORAGE: &str = "Almacenamiento";
const REGIONS: &str = "Regiones";

/// Why a variant has no "Propiedades" (`None`: it has them).
pub(crate) fn unsupported(v: Variant) -> Option<&'static str> {
    match v {
        Variant::Denodo => Some("las bases de Denodo se administran desde Denodo"),
        Variant::CrateDb => Some("CrateDB tiene una sola base por clúster: se organizan en esquemas"),
        Variant::H2 => Some("H2 tiene una base por archivo y no la modifica con ALTER DATABASE"),
        _ => None,
    }
}

pub(crate) fn supported(v: Variant) -> bool {
    unsupported(v).is_none()
}

/// The variants that keep per-database defaults (`ALTER DATABASE … SET`).
fn has_settings(v: Variant) -> bool {
    pg_like(v) || v == Variant::Cockroach
}

fn has_comment(v: Variant) -> bool {
    pg_like(v) || matches!(v, Variant::Cockroach | Variant::Redshift | Variant::Materialize)
}

/// How a setting's value is written.
#[derive(Clone, Copy)]
enum Kind {
    Text,
    /// A list of names (`search_path`): each item its own literal.
    List,
    OnOff,
    Pick(&'static [(&'static str, &'static str)]),
}

const ISOLATION: &[(&str, &str)] = &[
    ("read committed", "READ COMMITTED"),
    ("repeatable read", "REPEATABLE READ"),
    ("serializable", "SERIALIZABLE"),
    ("read uncommitted", "READ UNCOMMITTED"),
];

const TIME_HELP: &str = "Con unidad: 30s, 5min, 500ms. 0 la desactiva. Vacío: el valor del servidor (RESET).";
const MEM_HELP: &str = "Con unidad: 64MB, 1GB. Vacío: el valor del servidor (RESET).";
const EMPTY_HELP: &str = "Vacío: el valor del servidor (RESET).";

/// The settings offered as fields (when the server has them), with their
/// label, kind and help.
const SETTINGS: &[(&str, &str, Kind, &str)] = &[
    ("search_path", "Ruta de búsqueda de esquemas (search_path)", Kind::List, "Esquemas separados por comas, p. ej. \"$user\", public. Vacío: el valor del servidor (RESET)."),
    ("timezone", "Zona horaria (TimeZone)", Kind::Text, "p. ej. UTC o America/Argentina/Buenos_Aires. Vacío: el valor del servidor (RESET)."),
    ("datestyle", "Formato de fechas (DateStyle)", Kind::Text, "p. ej. ISO, DMY. Vacío: el valor del servidor (RESET)."),
    ("statement_timeout", "Tiempo máximo por sentencia (statement_timeout)", Kind::Text, TIME_HELP),
    ("lock_timeout", "Espera máxima por un bloqueo (lock_timeout)", Kind::Text, TIME_HELP),
    ("idle_in_transaction_session_timeout", "Inactividad máxima dentro de una transacción (idle_in_transaction_session_timeout)", Kind::Text, TIME_HELP),
    ("idle_session_timeout", "Inactividad máxima de una sesión (idle_session_timeout)", Kind::Text, TIME_HELP),
    ("default_transaction_isolation", "Aislamiento por defecto (default_transaction_isolation)", Kind::Pick(ISOLATION), EMPTY_HELP),
    ("default_transaction_read_only", "Transacciones de solo lectura por defecto (default_transaction_read_only)", Kind::OnOff, EMPTY_HELP),
    ("work_mem", "Memoria por operación (work_mem)", Kind::Text, MEM_HELP),
    ("maintenance_work_mem", "Memoria de mantenimiento (maintenance_work_mem)", Kind::Text, MEM_HELP),
    ("effective_cache_size", "Caché efectiva estimada (effective_cache_size)", Kind::Text, MEM_HELP),
    ("random_page_cost", "Costo de lectura aleatoria (random_page_cost)", Kind::Text, EMPTY_HELP),
    ("default_statistics_target", "Detalle de estadísticas (default_statistics_target)", Kind::Text, EMPTY_HELP),
    ("jit", "Compilación JIT (jit)", Kind::OnOff, EMPTY_HELP),
    ("default_tablespace", "Tablespace por defecto (default_tablespace)", Kind::Text, EMPTY_HELP),
    ("temp_tablespaces", "Tablespaces temporales (temp_tablespaces)", Kind::List, "Separados por comas. Vacío: el valor del servidor (RESET)."),
    ("row_security", "Seguridad por filas (row_security)", Kind::OnOff, EMPTY_HELP),
    ("lc_messages", "Idioma de los mensajes (lc_messages)", Kind::Text, EMPTY_HELP),
    ("lc_monetary", "Formato de moneda (lc_monetary)", Kind::Text, EMPTY_HELP),
    ("lc_numeric", "Formato de números (lc_numeric)", Kind::Text, EMPTY_HELP),
    ("lc_time", "Formato de fechas y horas (lc_time)", Kind::Text, EMPTY_HELP),
    ("log_min_duration_statement", "Registrar sentencias que tarden más de (log_min_duration_statement)", Kind::Text, "Con unidad: 500ms, 2s. -1 lo desactiva. Necesita superusuario. Vacío: RESET."),
    ("log_statement", "Registrar sentencias (log_statement)", Kind::Pick(&[("none", "Ninguna"), ("ddl", "DDL"), ("mod", "DDL y cambios de datos"), ("all", "Todas")]), "Necesita superusuario. Vacío: RESET."),
    // CockroachDB's own.
    ("default_int_size", "Tamaño de INT (default_int_size)", Kind::Pick(&[("4", "4 bytes"), ("8", "8 bytes")]), EMPTY_HELP),
    ("serial_normalization", "Cómo se generan los SERIAL (serial_normalization)", Kind::Text, "rowid, virtual_sequence, sql_sequence… Vacío: RESET."),
    ("sql_safe_updates", "Actualizaciones seguras (sql_safe_updates)", Kind::OnOff, "Rechaza UPDATE y DELETE sin WHERE. Vacío: RESET."),
    ("vectorize", "Ejecución vectorizada (vectorize)", Kind::Pick(&[("on", "on"), ("off", "off")]), EMPTY_HELP),
    ("distsql", "Ejecución distribuida (distsql)", Kind::Pick(&[("auto", "auto"), ("on", "on"), ("off", "off"), ("always", "always")]), EMPTY_HELP),
];

/// PostgreSQL settings CockroachDB lists only for compatibility (it
/// ignores them).
const PG_ONLY: &[&str] = &[
    "work_mem",
    "maintenance_work_mem",
    "effective_cache_size",
    "random_page_cost",
    "default_statistics_target",
    "jit",
    "default_tablespace",
    "temp_tablespaces",
    "row_security",
    "lc_messages",
    "lc_monetary",
    "lc_numeric",
    "lc_time",
    "log_min_duration_statement",
    "log_statement",
];

/// Settings whose value is a list of names (each item quoted on its own).
const LISTS: &[&str] = &["search_path", "temp_tablespaces", "session_preload_libraries", "local_preload_libraries"];

fn kind_of(name: &str) -> Kind {
    SETTINGS.iter().find(|s| s.0 == name).map(|s| s.2).unwrap_or(if LISTS.contains(&name) { Kind::List } else { Kind::Text })
}

/// `work_mem`, `TimeZone`, `app.tenant`: what `SET` takes unquoted.
fn param_ok(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 63
        && v.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

fn yes(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "t" | "on" | "1" | "yes")
}

fn flag(v: Option<String>) -> String {
    if v.as_deref().is_some_and(yes) {
        "true".into()
    } else {
        String::new()
    }
}

/// Splits `"$user", public, "a,b"` into its items, unquoted.
fn split_list(v: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in v.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => parts.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    parts.push(cur);
    parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| match p.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
            Some(inner) => inner.replace("\"\"", "\""),
            None => p.to_string(),
        })
        .collect()
}

/// `ALTER DATABASE … SET name = value`, or `RESET name` when empty.
fn setting(v: Variant, db: &str, name: &str, value: &str) -> Result<String> {
    check(param_ok(name), "parámetro", name)?;
    if value.is_empty() {
        return Ok(format!("ALTER DATABASE {db} RESET {name}"));
    }
    check(value.len() <= 1024 && !value.chars().any(char::is_control), name, value)?;
    let rhs = match kind_of(name) {
        Kind::List => {
            let items = split_list(value);
            check(!items.is_empty() && items.iter().all(|i| !i.chars().any(char::is_control)), name, value)?;
            items.iter().map(|i| lit(v, i)).collect::<Vec<_>>().join(", ")
        }
        Kind::OnOff if yes(value) => "on".to_string(),
        Kind::OnOff => {
            check(matches!(value.to_ascii_lowercase().as_str(), "off" | "false" | "f" | "0" | "no"), name, value)?;
            "off".to_string()
        }
        Kind::Pick(options) => {
            let picked = value.to_ascii_lowercase();
            check(options.iter().any(|o| o.0 == picked), name, value)?;
            lit(v, &picked)
        }
        Kind::Text => lit(v, value),
    };
    Ok(format!("ALTER DATABASE {db} SET {name} = {rhs}"))
}

fn comment(v: Variant, db: &str, value: &str) -> Result<String> {
    check(!value.contains('\0') && value.len() <= 8192, "comentario", value)?;
    Ok(format!("COMMENT ON DATABASE {db} IS {}", if value.is_empty() { "NULL".to_string() } else { lit(v, value) }))
}

fn owner(db: &str, value: &str) -> Result<String> {
    check(!value.is_empty(), "dueño", value)?;
    Ok(format!("ALTER DATABASE {db} OWNER TO {}", ident("dueño", value)?))
}

fn unknown(k: &str) -> Error {
    Error::Query(format!("propiedad desconocida: {k}"))
}

fn regions(v: &str) -> Result<Vec<String>> {
    v.split(',').map(str::trim).filter(|r| !r.is_empty()).map(|r| ident("región", r)).collect()
}

/// `500MB`, `2 GB`, `1TB` (Yellowbrick's MAX_SIZE), normalized.
fn max_size(v: &str) -> Option<String> {
    let s: String = v.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_uppercase();
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    let (n, unit) = s.split_at(digits);
    let n: u64 = n.parse().ok().filter(|n| *n > 0)?;
    matches!(unit, "" | "MB" | "GB" | "TB").then(|| format!("{n}{}", if unit.is_empty() { "MB" } else { unit }))
}

/// The statements for `changes`, in a safe order: owner and simple
/// options first, then the defaults and the comment, and what rewrites or
/// moves data (SET TABLESPACE, regions dropped) last.
pub(crate) fn alter(v: Variant, database: &str, changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if let Some(why) = unsupported(v) {
        return Err(Error::Unsupported(why.into()));
    }
    check(name_ok(database) && !database.is_empty(), "base", database)?;
    let db = quote_ident(Quote::Double, database);
    let mut out = Vec::new();
    let mut late = Vec::new();
    // Settings and the comment go after the rest, in key order.
    let mut settings = Vec::new();
    let mut note = None;
    for (key, value) in changes {
        let value = value.trim();
        if let Some(name) = key.strip_prefix("set:") {
            if !has_settings(v) {
                return Err(unknown(key));
            }
            settings.push(setting(v, &db, name, value)?);
            continue;
        }
        if key == "comment" {
            if !has_comment(v) {
                return Err(unknown(key));
            }
            note = Some(comment(v, &db, value)?);
            continue;
        }
        if key == "owner" {
            out.push(owner(&db, value)?);
            continue;
        }
        match v {
            Variant::Cockroach => match key.as_str() {
                // Handled together below.
                "primary_region" | "add_regions" | "drop_regions" | "secondary_region" | "survive" | "placement" => {}
                k => return Err(unknown(k)),
            },
            Variant::Redshift => match key.as_str() {
                "connection_limit" => {
                    let n = if value.is_empty() || value.eq_ignore_ascii_case("UNLIMITED") {
                        "UNLIMITED".to_string()
                    } else {
                        int(value, 0).ok_or_else(|| Error::Query(format!("límite de conexiones: «{value}» no es un número ni UNLIMITED")))?.to_string()
                    };
                    out.push(format!("ALTER DATABASE {db} CONNECTION LIMIT {n}"));
                }
                "collate" => {
                    check(matches!(value, "CASE_SENSITIVE" | "CASE_INSENSITIVE"), "intercalación", value)?;
                    out.push(format!("ALTER DATABASE {db} COLLATE {value}"));
                }
                "isolation" => {
                    check(matches!(value, "SERIALIZABLE" | "SNAPSHOT"), "nivel de aislamiento", value)?;
                    late.push(format!("ALTER DATABASE {db} ISOLATION LEVEL {value}"));
                }
                k => return Err(unknown(k)),
            },
            Variant::Yellowbrick => match key.as_str() {
                "connection_limit" => out.push(connection_limit(&db, value)?),
                "allow_connections" => out.push(format!("ALTER DATABASE {db} ALLOW_CONNECTIONS {}", yes(value))),
                "hot_standby" => out.push(format!("ALTER DATABASE {db} SET HOT_STANDBY {}", if yes(value) { "ON" } else { "OFF" })),
                "max_size" if value.is_empty() => out.push(format!("ALTER DATABASE {db} RESET MAX_SIZE")),
                "max_size" => {
                    let s = max_size(value).ok_or_else(|| Error::Query(format!("tamaño máximo: «{value}» no es un tamaño (p. ej. 500GB)")))?;
                    out.push(format!("ALTER DATABASE {db} SET MAX_SIZE = '{s}'"));
                }
                "read_only" => late.push(format!("ALTER DATABASE {db} SET READONLY {}", if yes(value) { "ON" } else { "OFF" })),
                k => return Err(unknown(k)),
            },
            Variant::RisingWave => match key.as_str() {
                "resource_group" if value.is_empty() => out.push(format!("ALTER DATABASE {db} RESET RESOURCE_GROUP DEFERRED")),
                "resource_group" => out.push(format!("ALTER DATABASE {db} SET RESOURCE_GROUP = {} DEFERRED", ident("grupo de recursos", value)?)),
                "barrier_interval_ms" | "checkpoint_frequency" => {
                    let n = if value.is_empty() {
                        "DEFAULT".to_string()
                    } else {
                        int(value, 1).ok_or_else(|| Error::Query(format!("{key}: «{value}» no es un número positivo")))?.to_string()
                    };
                    out.push(format!("ALTER DATABASE {db} SET {key} = {n}"));
                }
                k => return Err(unknown(k)),
            },
            Variant::Materialize => return Err(unknown(key)),
            _ => match key.as_str() {
                "connection_limit" => out.push(connection_limit(&db, value)?),
                "allow_connections" if v != Variant::OpenGauss => out.push(format!("ALTER DATABASE {db} ALLOW_CONNECTIONS {}", yes(value))),
                "is_template" if !matches!(v, Variant::OpenGauss | Variant::Yugabyte) => out.push(format!("ALTER DATABASE {db} IS_TEMPLATE {}", yes(value))),
                "refresh_collation_version" => {
                    if yes(value) {
                        late.push(format!("ALTER DATABASE {db} REFRESH COLLATION VERSION"));
                    }
                }
                "tablespace" if v != Variant::Yugabyte => {
                    check(!value.is_empty(), "tablespace", value)?;
                    late.push(format!("ALTER DATABASE {db} SET TABLESPACE {}", ident("tablespace", value)?));
                }
                k => return Err(unknown(k)),
            },
        }
    }
    if v == Variant::Cockroach {
        let (first, last) = cockroach_regions(&db, changes)?;
        out.splice(0..0, first);
        late.extend(last);
    }
    out.extend(settings);
    out.extend(note);
    out.extend(late);
    Ok(out)
}

fn connection_limit(db: &str, value: &str) -> Result<String> {
    let n = if value.is_empty() { -1 } else { int(value, -1).ok_or_else(|| Error::Query(format!("límite de conexiones: «{value}» no es un número (-1 o más)")))? };
    Ok(format!("ALTER DATABASE {db} CONNECTION LIMIT {n}"))
}

/// CockroachDB's region changes: (what goes first, what goes last).
/// A database becomes multi-region with SET PRIMARY REGION before any ADD
/// REGION; on one that already is, a new primary has to be added first, so
/// a primary that is also in "Agregar regiones" goes after the ADDs. Drops
/// go last (a primary can only be dropped when it's the last region).
fn cockroach_regions(db: &str, changes: &BTreeMap<String, String>) -> Result<(Vec<String>, Vec<String>)> {
    let get = |k: &str| changes.get(k).map(|v| v.trim());
    let (mut first, mut last) = (Vec::new(), Vec::new());
    let adds = get("add_regions").map(regions).transpose()?.unwrap_or_default();
    let primary = match get("primary_region") {
        Some("") => {
            return Err(Error::Query(
                "la región principal no se puede vaciar: para que la base deje de ser multirregión, quitá sus regiones (la principal, la última)".into(),
            ))
        }
        Some(p) => Some(ident("región principal", p)?),
        None => None,
    };
    let set_primary = primary.as_ref().map(|p| format!("ALTER DATABASE {db} SET PRIMARY REGION {p}"));
    let primary_added = primary.as_ref().is_some_and(|p| adds.contains(p));
    if !primary_added {
        first.extend(set_primary.clone());
    }
    for r in &adds {
        first.push(format!("ALTER DATABASE {db} ADD REGION {r}"));
    }
    if primary_added {
        first.extend(set_primary);
    }
    match get("secondary_region") {
        Some("") => first.push(format!("ALTER DATABASE {db} DROP SECONDARY REGION")),
        Some(r) => first.push(format!("ALTER DATABASE {db} SET SECONDARY REGION {}", ident("región secundaria", r)?)),
        None => {}
    }
    if let Some(s) = get("survive") {
        check(matches!(s, "ZONE" | "REGION"), "supervivencia", s)?;
        first.push(format!("ALTER DATABASE {db} SURVIVE {s} FAILURE"));
    }
    if let Some(p) = get("placement") {
        check(matches!(p, "DEFAULT" | "RESTRICTED"), "ubicación", p)?;
        first.push(format!("ALTER DATABASE {db} PLACEMENT {p}"));
    }
    for r in get("drop_regions").map(regions).transpose()?.unwrap_or_default() {
        last.push(format!("ALTER DATABASE {db} DROP REGION {r}"));
    }
    Ok((first, last))
}

/// What "Ver script" shows: exactly what `alter_database` runs.
pub(crate) fn script(v: Variant, database: &str, changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(v, database, changes)?.join(";\n"))
}

fn sel(key: &'static str, label: &'static str, options: Vec<(&'static str, &'static str)>) -> Field {
    Field::new(key, label, FieldKind::Select(options))
}

fn bool_field(key: &'static str, label: &'static str) -> Field {
    Field::new(key, label, FieldKind::Bool)
}

fn info(group: &str, label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo { group: group.into(), label: label.into(), value: value.into() }
}

fn owner_field() -> Field {
    Field::new("owner", "Dueño (owner)", FieldKind::Text)
}

fn comment_field() -> Field {
    Field::new("comment", "Comentario (COMMENT ON DATABASE)", FieldKind::Textarea).help("Vacío: sin comentario.")
}

/// Bytes as `12 MB`, `3.4 GB`.
fn pretty_bytes(b: f64) -> String {
    let units = ["bytes", "kB", "MB", "GB", "TB", "PB"];
    let mut n = b;
    let mut i = 0;
    while n >= 1024.0 && i < units.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    if i == 0 || n >= 100.0 {
        format!("{n:.0} {}", units[i])
    } else {
        format!("{n:.1} {}", units[i])
    }
}

/// Yellowbrick's `max_size_bytes` as the field shows it (`500GB`).
fn bytes_to_size(b: u64) -> String {
    const MB: u64 = 1024 * 1024;
    for (unit, n) in [("TB", MB * 1024 * 1024), ("GB", MB * 1024)] {
        if b >= n && b.is_multiple_of(n) {
            return format!("{}{unit}", b / n);
        }
    }
    format!("{}MB", b.div_ceil(MB))
}

impl PgSession {
    /// The rows of `sql`, or none when it fails (an optional fact).
    async fn maybe(&self, sql: &str) -> Vec<SimpleQueryRow> {
        self.text(sql).await.unwrap_or_default()
    }

    /// The first cell of `sql`, `None` when it fails or is null.
    async fn maybe_one(&self, sql: &str) -> Option<String> {
        self.maybe(sql).await.first().and_then(|r| r.get(0).map(str::to_string))
    }

    async fn first_column(&self, sql: &str) -> Vec<String> {
        self.maybe(sql).await.iter().filter_map(|r| r.get(0).map(str::to_string)).collect()
    }

    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        if let Some(why) = unsupported(self.variant) {
            return Err(Error::Unsupported(why.into()));
        }
        let mut p = DatabaseProperties::default();
        match self.variant {
            Variant::Cockroach => self.cockroach_properties(database, &mut p).await?,
            Variant::Redshift => self.redshift_properties(database, &mut p).await?,
            Variant::Yellowbrick => self.yellowbrick_properties(database, &mut p).await?,
            Variant::RisingWave => self.risingwave_properties(database, &mut p).await?,
            Variant::Materialize => self.materialize_properties(database, &mut p).await?,
            _ => self.pg_properties(database, &mut p).await?,
        }
        if p.fields.iter().any(|f| f.key == "owner") {
            let roles_sql = match self.variant {
                Variant::Redshift => "SELECT usename FROM pg_user ORDER BY 1",
                Variant::RisingWave => "SELECT name FROM rw_catalog.rw_users ORDER BY 1",
                Variant::Materialize => "SELECT name FROM mz_roles WHERE name NOT LIKE 'mz\\_%' ORDER BY 1",
                _ => "SELECT rolname FROM pg_roles WHERE rolname NOT LIKE 'pg\\_%' AND rolname NOT LIKE 'crdb\\_internal%' ORDER BY 1",
            };
            let roles = self.first_column(roles_sql).await;
            p.choices.push(FieldChoices { key: "owner".into(), default: None, values: roles });
        }
        if has_settings(self.variant) {
            self.settings(database, &mut p).await;
        }
        Ok(p)
    }

    /// PostgreSQL and the variants with its catalog.
    async fn pg_properties(&mut self, database: &str, p: &mut DatabaseProperties) -> Result<()> {
        let v = self.variant;
        let ver = self.version;
        let name = lit(v, database);
        let cols: BTreeSet<String> =
            self.first_column("SELECT attname::text FROM pg_attribute WHERE attrelid = 'pg_catalog.pg_database'::regclass AND attnum > 0").await.into_iter().collect();
        let col = |c: &str, expr: &str| if cols.contains(c) { expr.to_string() } else { "NULL".to_string() };
        let locale = if cols.contains("datlocale") { "d.datlocale::text" } else if cols.contains("daticulocale") { "d.daticulocale::text" } else { "NULL" };
        let sql = format!(
            "SELECT d.oid::text AS oid, pg_get_userbyid(d.datdba) AS owner, pg_encoding_to_char(d.encoding) AS enc, \
             d.datcollate::text AS coll, d.datctype::text AS ctype, d.datconnlimit::text AS lim, \
             {} AS tmpl, {} AS allow, t.spcname::text AS spc, {} AS prov, {locale} AS loc, {} AS rules, \
             {} AS collver, {} AS compat \
             FROM pg_database d LEFT JOIN pg_tablespace t ON t.oid = d.dattablespace WHERE d.datname = {name}",
            col("datistemplate", "d.datistemplate::text"),
            col("datallowconn", "d.datallowconn::text"),
            col("datlocprovider", "d.datlocprovider::text"),
            col("daticurules", "d.daticurules::text"),
            col("datcollversion", "d.datcollversion::text"),
            col("datcompatibility", "d.datcompatibility::text"),
        );
        let rows = self.text(&sql).await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let get = |c: &str| cell(r, c);
        let oid = get("oid").unwrap_or_default();
        // Only digits reach the SQL below.
        let oid_ok = !oid.is_empty() && oid.chars().all(|c| c.is_ascii_digit());

        // General: the facts.
        if oid_ok {
            if let Some(size) = self
                .maybe_one(&format!("SELECT CASE WHEN has_database_privilege({oid}::oid, 'CONNECT') THEN pg_database_size({oid}::oid)::text END"))
                .await
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|s| *s > 0.0)
            {
                p.info.push(info("", "Tamaño", pretty_bytes(size)));
            }
            // PostgreSQL keeps no creation date; PG_VERSION is written when
            // the database is created (needs pg_stat_file's privilege).
            if v != Variant::Yugabyte {
                if let Some(t) = self.maybe_one(&format!("SELECT ((pg_stat_file('base/{oid}/PG_VERSION')).modification)::text")).await {
                    p.info.push(info("", "Creada", t));
                }
            }
        }
        if let Some(n) = self.maybe_one(&format!("SELECT count(*)::text FROM pg_stat_activity WHERE datname = {name}")).await {
            p.info.push(info("", "Conexiones abiertas", n));
        }
        if let Some(c) = get("compat") {
            p.info.push(info("", "Compatibilidad (DBCOMPATIBILITY)", c));
        }
        p.info.push(info("", "OID", oid.clone()));

        // Locale: facts and the collation version action.
        p.info.push(info(LOCALE, "Codificación (encoding)", get("enc").unwrap_or_default()));
        if let Some(prov) = get("prov") {
            let label = match prov.as_str() {
                "i" => "ICU",
                "b" => "builtin",
                _ => "libc",
            };
            p.info.push(info(LOCALE, "Proveedor de locale", label));
        }
        p.info.push(info(LOCALE, "Intercalación (LC_COLLATE)", get("coll").unwrap_or_default()));
        p.info.push(info(LOCALE, "Clasificación de caracteres (LC_CTYPE)", get("ctype").unwrap_or_default()));
        if let Some(l) = get("loc").filter(|l| !l.is_empty()) {
            p.info.push(info(LOCALE, "Locale del proveedor", l));
        }
        if let Some(rules) = get("rules").filter(|l| !l.is_empty()) {
            p.info.push(info(LOCALE, "Reglas ICU", rules));
        }
        // REFRESH COLLATION VERSION (15+), when a version is recorded.
        if cols.contains("datcollversion") && ver >= 150000 && oid_ok {
            if let Some(recorded) = get("collver").filter(|c| !c.is_empty()) {
                let actual = self.maybe_one(&format!("SELECT pg_database_collation_actual_version({oid}::oid)")).await;
                p.info.push(info(LOCALE, "Versión de intercalación registrada", recorded.clone()));
                p.info.push(info(LOCALE, "Versión de intercalación del sistema", actual.clone().unwrap_or_else(|| "—".into())));
                if actual.as_ref().is_some_and(|a| *a != recorded) {
                    p.info.push(info(
                        LOCALE,
                        "Atención",
                        "La versión de intercalación cambió: reconstruí los índices sobre textos (REINDEX) y después actualizá la versión registrada.",
                    ));
                }
                p.fields.push(
                    bool_field("refresh_collation_version", "Actualizar la versión de intercalación registrada (REFRESH COLLATION VERSION)").group(LOCALE).help(
                        "Registra la versión de intercalación del sistema operativo o de ICU como la de la base. Se aplica una vez; no queda marcado.",
                    ),
                );
                p.values.insert("refresh_collation_version".into(), String::new());
            }
        }

        // Storage.
        if let Some(spc) = get("spc") {
            p.info.push(info(STORAGE, "Tablespace actual", spc.clone()));
            if v != Variant::Yugabyte {
                p.values.insert("tablespace".into(), spc);
            }
        }
        if v != Variant::Yugabyte && oid_ok {
            if let Some(age) = self.maybe_one(&format!("SELECT age(datfrozenxid)::text FROM pg_database WHERE oid = {oid}::oid")).await {
                p.info.push(info(STORAGE, "Edad del XID congelado (age(datfrozenxid))", age));
            }
        }

        // The editable fields.
        p.fields.push(owner_field());
        p.values.insert("owner".into(), get("owner").unwrap_or_default());
        p.fields.push(Field::new("connection_limit", "Límite de conexiones (CONNECTION LIMIT)", FieldKind::Number).help("-1: sin límite."));
        p.values.insert("connection_limit".into(), get("lim").unwrap_or_else(|| "-1".into()));
        let new_options = ver >= 90500 && v != Variant::OpenGauss;
        if new_options && cols.contains("datallowconn") {
            p.fields.push(bool_field("allow_connections", "Permitir conexiones (ALLOW_CONNECTIONS)").help("Desactivado, nadie puede conectarse a la base."));
            p.values.insert("allow_connections".into(), flag(get("allow")));
        }
        if new_options && v != Variant::Yugabyte && cols.contains("datistemplate") {
            p.fields.push(bool_field("is_template", "Es plantilla (IS_TEMPLATE)").help("Cualquier usuario con CREATEDB puede copiarla; no se puede borrar mientras lo sea."));
            p.values.insert("is_template".into(), flag(get("tmpl")));
        }
        p.fields.push(comment_field());
        let note = if oid_ok { self.maybe_one(&format!("SELECT shobj_description({oid}::oid, 'pg_database')")).await } else { None };
        p.values.insert("comment".into(), note.unwrap_or_default());
        if v != Variant::Yugabyte {
            p.fields.push(Field::new("tablespace", "Tablespace (SET TABLESPACE)", FieldKind::Text).group(STORAGE).help(
                "Mueve los archivos de la base al tablespace elegido. Nadie puede estar conectado a la base mientras tanto.",
            ));
            let spaces = self.first_column("SELECT spcname::text FROM pg_tablespace WHERE spcname <> 'pg_global' ORDER BY 1").await;
            p.choices.push(FieldChoices { key: "tablespace".into(), default: None, values: spaces });
        }

        p.warnings.insert(
            "tablespace".into(),
            "Cambiar el tablespace copia la base entera al nuevo: tarda según su tamaño, necesita que nadie esté conectado a ella y la bloquea mientras dura.".into(),
        );
        p.warnings.insert(
            "allow_connections".into(),
            "Si se desactiva, nadie podrá conectarse a la base (tampoco DBine para explorarla) hasta volver a activarlo.".into(),
        );
        p.warnings.insert("is_template".into(), "Una base marcada como plantilla no se puede borrar y cualquier usuario con CREATEDB puede copiarla.".into());
        p.warnings.insert(
            "refresh_collation_version".into(),
            "Solo registra la versión nueva: los índices sobre textos creados con la versión anterior pueden estar mal ordenados y conviene reconstruirlos (REINDEX) antes.".into(),
        );
        Ok(())
    }

    /// `pg_db_role_setting` (setrole 0) as the "Configuración" tab: the
    /// common settings the server has, plus whatever else is set.
    async fn settings(&mut self, database: &str, p: &mut DatabaseProperties) {
        let v = self.variant;
        let name = lit(v, database);
        let set = self
            .first_column(&format!(
                "SELECT unnest(s.setconfig) FROM pg_db_role_setting s JOIN pg_database d ON d.oid = s.setdatabase \
                 WHERE s.setrole = 0 AND d.datname = {name}"
            ))
            .await;
        let mut current: BTreeMap<String, String> = BTreeMap::new();
        for entry in set {
            if let Some((k, val)) = entry.split_once('=') {
                current.insert(k.to_ascii_lowercase(), val.to_string());
            }
        }
        // What the server knows, with its value outside this database.
        let mut server: BTreeMap<String, Option<String>> = BTreeMap::new();
        for r in self.maybe("SELECT lower(name) AS name, setting, unit FROM pg_settings").await {
            let Some(n) = cell(&r, "name") else { continue };
            let unit = cell(&r, "unit").unwrap_or_default();
            let val = cell(&r, "setting").map(|s| if unit.is_empty() || unit.starts_with(|c: char| c.is_ascii_digit()) { s } else { format!("{s}{unit}") });
            server.insert(n, val);
        }
        let mut names: Vec<String> = SETTINGS.iter().map(|s| s.0.to_string()).filter(|n| server.is_empty() || server.contains_key(n)).collect();
        // CockroachDB's own only on CockroachDB, PostgreSQL's logging not there.
        let crdb_only = ["default_int_size", "serial_normalization", "sql_safe_updates", "vectorize", "distsql"];
        names.retain(|n| if v == Variant::Cockroach { !PG_ONLY.contains(&n.as_str()) } else { !crdb_only.contains(&n.as_str()) });
        for n in current.keys() {
            if !names.contains(n) && param_ok(n) {
                names.push(n.clone());
            }
        }
        for n in names {
            let key = intern(&format!("set:{n}"));
            let (label, kind, help) = SETTINGS.iter().find(|s| s.0 == n).map(|s| (s.1, s.2, s.3)).unwrap_or((intern(&n), kind_of(&n), EMPTY_HELP));
            let field_kind = match kind {
                Kind::OnOff => FieldKind::Select(vec![("on", "Sí (on)"), ("off", "No (off)")]),
                Kind::Pick(options) => FieldKind::Select(options.to_vec()),
                Kind::Text | Kind::List => FieldKind::Text,
            };
            p.fields.push(Field::new(key, label, field_kind).group(CONFIG).help(help));
            let value = current.get(&n).cloned().map(|val| match kind {
                Kind::OnOff => (if yes(&val) { "on" } else { "off" }).to_string(),
                Kind::Pick(_) => val.to_ascii_lowercase(),
                _ => val,
            });
            p.values.insert(key.to_string(), value.unwrap_or_default());
            if let Some(Some(d)) = server.get(&n) {
                p.choices.push(FieldChoices { key: key.into(), default: Some(d.clone()), values: Vec::new() });
            }
        }
        p.warnings.insert(
            "set:default_transaction_read_only".into(),
            "Las sesiones nuevas en la base empiezan en solo lectura: no podrán escribir sin cambiarlo en la sesión.".into(),
        );
        p.info.push(info(CONFIG, "Alcance", "Valores por defecto de las sesiones que se conecten a esta base, para cualquier usuario (ALTER DATABASE … SET)."));
    }

    async fn cockroach_properties(&mut self, database: &str, p: &mut DatabaseProperties) -> Result<()> {
        let name = lit(Variant::Cockroach, database);
        let rows = self
            .text(&format!(
                "SELECT owner, primary_region, secondary_region, array_to_string(regions, ', ') AS regions, survival_goal \
                 FROM [SHOW DATABASES] WHERE database_name = {name}"
            ))
            .await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let get = |c: &str| cell(r, c).filter(|s| !s.is_empty());
        let pg = self
            .maybe(&format!("SELECT oid::text AS oid, pg_encoding_to_char(encoding) AS enc, datcollate::text AS coll FROM pg_database WHERE datname = {name}"))
            .await;
        let pg = pg.first();
        if let Some(n) = self.maybe_one(&format!("SELECT count(*)::text FROM pg_stat_activity WHERE datname = {name}")).await {
            p.info.push(info("", "Conexiones abiertas", n));
        }
        if let Some(r) = pg {
            p.info.push(info("", "Codificación (encoding)", cell(r, "enc").unwrap_or_default()));
            p.info.push(info("", "Intercalación", cell(r, "coll").unwrap_or_default()));
        }
        p.fields.push(owner_field());
        p.values.insert("owner".into(), get("owner").unwrap_or_default());
        p.fields.push(comment_field());
        let oid = pg.and_then(|r| cell(r, "oid")).filter(|o| !o.is_empty() && o.chars().all(|c| c.is_ascii_digit()));
        let note = match oid {
            Some(oid) => self.maybe_one(&format!("SELECT shobj_description({oid}::oid, 'pg_database')")).await,
            None => None,
        };
        p.values.insert("comment".into(), note.unwrap_or_default());

        // Regions, when the cluster has them.
        let cluster = self.first_column("SELECT region FROM [SHOW REGIONS FROM CLUSTER] ORDER BY 1").await;
        let primary = get("primary_region");
        p.info.push(info(REGIONS, "Regiones de la base", get("regions").unwrap_or_else(|| "Ninguna (no es multirregión)".into())));
        p.info.push(info(REGIONS, "Regiones del clúster", if cluster.is_empty() { "Ninguna: el clúster no declara localidades (--locality)".into() } else { cluster.join(", ") }));
        if !cluster.is_empty() || primary.is_some() {
            let placement = if primary.is_some() {
                let create = self.maybe_one(&format!("SELECT create_statement FROM [SHOW CREATE DATABASE {}]", quote_ident(Quote::Double, database))).await;
                Some(if create.is_some_and(|c| c.contains("PLACEMENT RESTRICTED")) { "RESTRICTED" } else { "DEFAULT" })
            } else {
                None
            };
            p.fields.extend([
                Field::new("primary_region", "Región principal (PRIMARY REGION)", FieldKind::Text)
                    .group(REGIONS)
                    .help("Ponerla vuelve multirregión a la base. Si la región nueva no es de la base todavía, ponela también en «Agregar regiones»."),
                Field::new("add_regions", "Agregar regiones (ADD REGION)", FieldKind::Text).placeholder("us-east1, us-west1").group(REGIONS).help("Separadas por comas."),
                Field::new("drop_regions", "Quitar regiones (DROP REGION)", FieldKind::Text).group(REGIONS).help(
                    "Separadas por comas. La principal solo se puede quitar cuando es la última; entonces la base deja de ser multirregión.",
                ),
                Field::new("secondary_region", "Región secundaria (SECONDARY REGION)", FieldKind::Text).group(REGIONS).help("Vacía: sin región secundaria."),
                sel("survive", "Supervivencia (SURVIVE)", vec![("ZONE", "Falla de una zona (ZONE)"), ("REGION", "Falla de una región (REGION)")])
                    .group(REGIONS)
                    .help("REGION necesita al menos 3 regiones."),
                sel("placement", "Ubicación de réplicas (PLACEMENT)", vec![("DEFAULT", "Por defecto (DEFAULT)"), ("RESTRICTED", "Restringida (RESTRICTED)")])
                    .group(REGIONS)
                    .help("RESTRICTED deja las réplicas de las tablas regionales solo en su región."),
            ]);
            p.values.insert("primary_region".into(), primary.clone().unwrap_or_default());
            p.values.insert("add_regions".into(), String::new());
            p.values.insert("drop_regions".into(), String::new());
            p.values.insert("secondary_region".into(), get("secondary_region").unwrap_or_default());
            p.values.insert("survive".into(), get("survival_goal").map(|s| s.to_ascii_uppercase()).unwrap_or_default());
            p.values.insert("placement".into(), placement.unwrap_or_default().into());
            for k in ["primary_region", "add_regions", "drop_regions", "secondary_region"] {
                p.choices.push(FieldChoices { key: k.into(), default: None, values: cluster.clone() });
            }
            p.warnings.insert("drop_regions".into(), "Quitar una región mueve sus réplicas y sus datos a las demás regiones de la base.".into());
            p.warnings.insert(
                "survive".into(),
                "Cambiar la supervivencia vuelve a ubicar las réplicas de toda la base: con REGION, las escrituras pasan a esperar entre regiones.".into(),
            );
            p.warnings.insert("primary_region".into(), "Cambiar la región principal mueve los leaseholders de las tablas globales y regionales por defecto.".into());
            p.warnings.insert("placement".into(), "RESTRICTED quita las réplicas no votantes de otras regiones: las lecturas desde ellas dejan de ser locales.".into());
        }
        Ok(())
    }

    async fn redshift_properties(&mut self, database: &str, p: &mut DatabaseProperties) -> Result<()> {
        let v = Variant::Redshift;
        let name = lit(v, database);
        let rows = self
            .text(&format!(
                "SELECT u.usename AS owner, d.datconnlimit::text AS lim FROM pg_database_info d \
                 LEFT JOIN pg_user u ON u.usesysid = d.datdba WHERE d.datname = {name}"
            ))
            .await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        let lim = cell(r, "lim").map(|l| if l == "-1" || l.is_empty() { "UNLIMITED".to_string() } else { l.to_ascii_uppercase() });
        p.fields.push(owner_field());
        p.values.insert("owner".into(), cell(r, "owner").unwrap_or_default());
        p.fields.push(Field::new("connection_limit", "Límite de conexiones (CONNECTION LIMIT)", FieldKind::Text).placeholder("UNLIMITED o un número"));
        p.values.insert("connection_limit".into(), lim.unwrap_or_else(|| "UNLIMITED".into()));
        p.choices.push(FieldChoices { key: "connection_limit".into(), default: None, values: vec!["UNLIMITED".into()] });

        let meta = self
            .maybe(&format!(
                "SELECT database_type, database_options, database_isolation_level FROM svv_redshift_databases WHERE database_name = {name}"
            ))
            .await;
        if let Some(m) = meta.first() {
            if let Some(t) = cell(m, "database_type") {
                p.info.push(info("", "Tipo", t));
            }
            let options = cell(m, "database_options").unwrap_or_default().to_ascii_lowercase();
            let collate = if options.contains("case_insensitive") || options.contains("\"ci\"") { "CASE_INSENSITIVE" } else { "CASE_SENSITIVE" };
            p.values.insert("collate".into(), collate.into());
            if let Some(iso) = cell(m, "database_isolation_level") {
                let iso = if iso.to_ascii_lowercase().contains("snapshot") { "SNAPSHOT" } else { "SERIALIZABLE" };
                p.values.insert("isolation".into(), iso.into());
            }
        }
        if let Some(n) = self.maybe_one(&format!("SELECT count(*)::text FROM stv_sessions WHERE trim(db_name) = {name}")).await {
            p.info.push(info("", "Conexiones abiertas", n));
        }
        p.fields.push(
            sel("collate", "Intercalación (COLLATE)", vec![("CASE_SENSITIVE", "Distingue mayúsculas (CASE_SENSITIVE)"), ("CASE_INSENSITIVE", "No distingue mayúsculas (CASE_INSENSITIVE)")])
                .help("Cómo se comparan y buscan los textos."),
        );
        if database != "dev" {
            p.fields.push(
                sel("isolation", "Nivel de aislamiento (ISOLATION LEVEL)", vec![("SERIALIZABLE", "SERIALIZABLE"), ("SNAPSHOT", "SNAPSHOT")])
                    .help("No puede haber otras conexiones a la base al cambiarlo. La base dev no lo admite."),
            );
        }
        p.fields.push(comment_field());
        let note = self
            .maybe_one(&format!("SELECT description FROM pg_description WHERE objoid = (SELECT oid FROM pg_database WHERE datname = {name}) LIMIT 1"))
            .await;
        p.values.insert("comment".into(), note.unwrap_or_default());
        p.warnings.insert("isolation".into(), "Falla si hay otras conexiones abiertas a la base. Rige para las sesiones nuevas.".into());
        p.warnings.insert("collate".into(), "Cambia cómo se comparan los textos en todas las consultas de la base: los resultados de filtros y uniones pueden cambiar.".into());
        Ok(())
    }

    async fn yellowbrick_properties(&mut self, database: &str, p: &mut DatabaseProperties) -> Result<()> {
        let v = Variant::Yellowbrick;
        let name = lit(v, database);
        let rows = self
            .text(&format!(
                "SELECT pg_get_userbyid(datdba) AS owner, datconnlimit::text AS lim, datallowconn::text AS allow FROM pg_database WHERE datname = {name}"
            ))
            .await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        p.fields.push(owner_field());
        p.values.insert("owner".into(), cell(r, "owner").unwrap_or_default());
        p.fields.push(Field::new("connection_limit", "Límite de conexiones (CONNECTION LIMIT)", FieldKind::Number).help("-1: sin límite."));
        p.values.insert("connection_limit".into(), cell(r, "lim").unwrap_or_else(|| "-1".into()));
        p.fields.push(bool_field("allow_connections", "Permitir conexiones (ALLOW_CONNECTIONS)"));
        p.values.insert("allow_connections".into(), flag(cell(r, "allow")));

        let sys = self
            .maybe(&format!(
                "SELECT encoding, collation, is_readonly::text AS ro, readonly_reason, is_hot_standby::text AS hs, table_count::text AS tables, \
                 compressed_bytes::text AS comp, uncompressed_bytes::text AS raw, max_size_bytes::text AS max FROM sys.database WHERE name = {name}"
            ))
            .await;
        if let Some(s) = sys.first() {
            let bytes = |c: &str| cell(s, c).and_then(|b| b.parse::<f64>().ok()).map(pretty_bytes);
            if let Some(b) = bytes("comp") {
                p.info.push(info("", "Tamaño comprimido", b));
            }
            if let Some(b) = bytes("raw") {
                p.info.push(info("", "Tamaño sin comprimir", b));
            }
            if let Some(t) = cell(s, "tables") {
                p.info.push(info("", "Tablas", t));
            }
            p.info.push(info("", "Codificación (encoding)", cell(s, "encoding").unwrap_or_default()));
            p.info.push(info("", "Intercalación", cell(s, "collation").unwrap_or_default()));
            if let Some(why) = cell(s, "readonly_reason").filter(|w| !w.is_empty()) {
                p.info.push(info("", "Motivo de solo lectura", why));
            }
            p.values.insert("read_only".into(), flag(cell(s, "ro")));
            p.values.insert("hot_standby".into(), flag(cell(s, "hs")));
            p.values.insert("max_size".into(), cell(s, "max").and_then(|m| m.parse::<u64>().ok()).map(bytes_to_size).unwrap_or_default());
        }
        p.fields.extend([
            bool_field("read_only", "Solo lectura (READONLY)").help("Espera a que terminen las transacciones que escriben."),
            bool_field("hot_standby", "Destino de restauración o réplica (HOT_STANDBY)").help("Solo sobre una base vacía."),
            Field::new("max_size", "Tamaño máximo (MAX_SIZE)", FieldKind::Text).placeholder("500GB").help("En MB, GB o TB. Vacío: sin límite."),
        ]);
        p.warnings.insert("read_only".into(), "En solo lectura nadie puede escribir en la base; las transacciones que escriben y estén en curso se esperan.".into());
        p.warnings.insert(
            "hot_standby".into(),
            "HOT_STANDBY prepara una base vacía para recibir una restauración o una réplica: mientras esté activo, no acepta escrituras comunes.".into(),
        );
        p.warnings.insert(
            "allow_connections".into(),
            "Si se desactiva, nadie podrá conectarse a la base (tampoco DBine para explorarla) hasta volver a activarlo.".into(),
        );
        Ok(())
    }

    async fn risingwave_properties(&mut self, database: &str, p: &mut DatabaseProperties) -> Result<()> {
        let v = Variant::RisingWave;
        let name = lit(v, database);
        let cols: BTreeSet<String> = self
            .first_column("SELECT column_name::text FROM information_schema.columns WHERE table_schema = 'rw_catalog' AND table_name = 'rw_databases'")
            .await
            .into_iter()
            .collect();
        let col = |c: &str| if cols.contains(c) { format!("d.{c}::text") } else { "NULL".into() };
        let rows = self
            .text(&format!(
                "SELECT d.id::text AS id, u.name AS owner, {} AS rg, {} AS barrier, {} AS checkpoint \
                 FROM rw_catalog.rw_databases d LEFT JOIN rw_catalog.rw_users u ON u.id = d.owner WHERE d.name = {name}",
                col("resource_group"),
                col("barrier_interval_ms"),
                col("checkpoint_frequency"),
            ))
            .await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        p.info.push(info("", "ID", cell(r, "id").unwrap_or_default()));
        p.fields.push(owner_field());
        p.values.insert("owner".into(), cell(r, "owner").unwrap_or_default());
        if cols.contains("resource_group") {
            p.fields.push(Field::new("resource_group", "Grupo de recursos (RESOURCE_GROUP)", FieldKind::Text).help(
                "Función de la edición con licencia. Cambia el grupo de la base sin mover los trabajos que ya corren (DEFERRED). Vacío: el grupo por defecto.",
            ));
            p.values.insert("resource_group".into(), cell(r, "rg").unwrap_or_default());
            let groups = self.first_column("SELECT name FROM rw_catalog.rw_resource_groups ORDER BY 1").await;
            p.choices.push(FieldChoices { key: "resource_group".into(), default: None, values: groups });
        }
        for (k, label, c) in [
            ("barrier_interval_ms", "Intervalo de barreras (ms)", "barrier_interval_ms"),
            ("checkpoint_frequency", "Frecuencia de checkpoint (barreras)", "checkpoint_frequency"),
        ] {
            if cols.contains(c) {
                p.fields.push(Field::new(k, label, FieldKind::Number).group(CONFIG).help("RisingWave 2.5 o posterior, con licencia. Vacío: el del sistema (DEFAULT)."));
                p.values.insert(k.into(), cell(r, if k == "barrier_interval_ms" { "barrier" } else { "checkpoint" }).unwrap_or_default());
            }
        }
        Ok(())
    }

    async fn materialize_properties(&mut self, database: &str, p: &mut DatabaseProperties) -> Result<()> {
        let name = lit(Variant::Materialize, database);
        let rows = self
            .text(&format!(
                "SELECT d.id AS id, r.name AS owner, c.comment AS note FROM mz_databases d LEFT JOIN mz_roles r ON r.id = d.owner_id \
                 LEFT JOIN mz_internal.mz_comments c ON c.id = d.id AND c.object_type = 'database' WHERE d.name = {name}"
            ))
            .await?;
        let r = rows.first().ok_or_else(|| Error::Query(format!("no existe la base «{database}»")))?;
        p.info.push(info("", "ID", cell(r, "id").unwrap_or_default()));
        if let Some(n) = self
            .maybe_one(&format!("SELECT count(*)::text FROM mz_schemas s JOIN mz_databases d ON d.id = s.database_id WHERE d.name = {name}"))
            .await
        {
            p.info.push(info("", "Esquemas", n));
        }
        p.fields.push(owner_field());
        p.values.insert("owner".into(), cell(r, "owner").unwrap_or_default());
        p.fields.push(comment_field());
        p.values.insert("comment".into(), cell(r, "note").unwrap_or_default());
        Ok(())
    }

    /// Run the statements one by one; a later failure says how many applied.
    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let statements = alter(self.variant, database, changes)?;
        for (i, sql) in statements.iter().enumerate() {
            if let Err(e) = self.client.batch_execute(sql).await.map_err(err) {
                return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn postgres_changes_in_a_safe_order() {
        let s = script(
            Variant::Postgres,
            "ventas",
            &c(&[
                ("tablespace", "rápido"),
                ("set:work_mem", "64MB"),
                ("set:statement_timeout", ""),
                ("comment", "La base de o'ventas"),
                ("refresh_collation_version", "true"),
                ("owner", "app"),
                ("connection_limit", "20"),
                ("allow_connections", ""),
                ("is_template", "true"),
            ]),
        )
        .unwrap();
        assert_eq!(
            s,
            "ALTER DATABASE \"ventas\" ALLOW_CONNECTIONS false;
ALTER DATABASE \"ventas\" CONNECTION LIMIT 20;
ALTER DATABASE \"ventas\" IS_TEMPLATE true;
ALTER DATABASE \"ventas\" OWNER TO \"app\";
ALTER DATABASE \"ventas\" RESET statement_timeout;
ALTER DATABASE \"ventas\" SET work_mem = E'64MB';
COMMENT ON DATABASE \"ventas\" IS E'La base de o''ventas';
ALTER DATABASE \"ventas\" REFRESH COLLATION VERSION;
ALTER DATABASE \"ventas\" SET TABLESPACE \"rápido\""
        );
        // An unchecked action is nothing; an empty comment drops it; an
        // empty limit is no limit.
        assert_eq!(
            script(Variant::Postgres, "v", &c(&[("refresh_collation_version", ""), ("comment", ""), ("connection_limit", "")])).unwrap(),
            "ALTER DATABASE \"v\" CONNECTION LIMIT -1;\nCOMMENT ON DATABASE \"v\" IS NULL"
        );
    }

    #[test]
    fn settings_are_quoted_by_kind() {
        let s = |k: &str, v: &str| script(Variant::Postgres, "v", &c(&[(k, v)]));
        assert_eq!(s("set:search_path", "\"$user\", public, \"a, b\"").unwrap(), "ALTER DATABASE \"v\" SET search_path = E'$user', E'public', E'a, b'");
        assert_eq!(s("set:timezone", "America/Argentina/Buenos_Aires").unwrap(), "ALTER DATABASE \"v\" SET timezone = E'America/Argentina/Buenos_Aires'");
        assert_eq!(s("set:default_transaction_read_only", "on").unwrap(), "ALTER DATABASE \"v\" SET default_transaction_read_only = on");
        assert_eq!(s("set:default_transaction_isolation", "SERIALIZABLE").unwrap(), "ALTER DATABASE \"v\" SET default_transaction_isolation = E'serializable'");
        assert_eq!(s("set:app.tenant", "it's").unwrap(), "ALTER DATABASE \"v\" SET app.tenant = E'it''s'");
        for bad in [
            ("set:work_mem; DROP", "1"),
            ("set:", "1"),
            ("set:default_transaction_isolation", "chaos"),
            ("set:jit", "maybe"),
            ("set:work_mem", "64\nMB"),
            ("set:search_path", ",,"),
        ] {
            assert!(script(Variant::Postgres, "v", &c(&[bad])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn lists_split_outside_quotes() {
        assert_eq!(split_list("\"$user\", public"), vec!["$user", "public"]);
        assert_eq!(split_list(" a ,\"b \"\"c\"\"\", , d"), vec!["a", "b \"c\"", "d"]);
        assert!(split_list(" , ").is_empty());
    }

    #[test]
    fn values_are_checked() {
        for bad in [("connection_limit", "-2"), ("connection_limit", "5 OR 1"), ("owner", ""), ("owner", "a\nb"), ("tablespace", ""), ("nope", "1"), ("comment", "a\0b")] {
            assert!(script(Variant::Postgres, "v", &c(&[bad])).is_err(), "{bad:?}");
        }
        // Names are quoted, not rejected.
        assert!(script(Variant::Postgres, "v\"x", &c(&[("owner", "o\"k")])).unwrap().contains("ALTER DATABASE \"v\"\"x\" OWNER TO \"o\"\"k\""));
        for v in [Variant::Denodo, Variant::CrateDb, Variant::H2] {
            assert!(!supported(v) && script(v, "v", &c(&[])).is_err());
        }
    }

    #[test]
    fn variant_quirks() {
        // openGauss: no IS_TEMPLATE nor ALLOW_CONNECTIONS; YugabyteDB: no
        // tablespace nor IS_TEMPLATE.
        assert!(script(Variant::OpenGauss, "v", &c(&[("is_template", "true")])).is_err());
        assert!(script(Variant::OpenGauss, "v", &c(&[("allow_connections", "true")])).is_err());
        assert!(script(Variant::Yugabyte, "v", &c(&[("tablespace", "t")])).is_err());
        assert!(script(Variant::Yugabyte, "v", &c(&[("is_template", "")])).is_err());
        assert_eq!(script(Variant::Greenplum, "v", &c(&[("set:search_path", "s")])).unwrap(), "ALTER DATABASE \"v\" SET search_path = E's'");
        // Materialize: owner and comment only.
        assert_eq!(
            script(Variant::Materialize, "v", &c(&[("owner", "r"), ("comment", "x")])).unwrap(),
            "ALTER DATABASE \"v\" OWNER TO \"r\";\nCOMMENT ON DATABASE \"v\" IS 'x'"
        );
        assert!(script(Variant::Materialize, "v", &c(&[("set:work_mem", "1MB")])).is_err());
        assert!(script(Variant::Materialize, "v", &c(&[("connection_limit", "1")])).is_err());
    }

    #[test]
    fn cockroach_regions_order() {
        // Becoming multi-region: the primary first, then the others.
        assert_eq!(
            script(Variant::Cockroach, "v", &c(&[("primary_region", "us-east1"), ("add_regions", "us-west1, eu-west1"), ("survive", "REGION")])).unwrap(),
            "ALTER DATABASE \"v\" SET PRIMARY REGION \"us-east1\";
ALTER DATABASE \"v\" ADD REGION \"us-west1\";
ALTER DATABASE \"v\" ADD REGION \"eu-west1\";
ALTER DATABASE \"v\" SURVIVE REGION FAILURE"
        );
        // A new primary that is added too: added first; drops go last.
        assert_eq!(
            script(
                Variant::Cockroach,
                "v",
                &c(&[("primary_region", "eu-west1"), ("add_regions", "eu-west1"), ("drop_regions", "us-east1"), ("owner", "root"), ("set:sql_safe_updates", "on"), ("placement", "RESTRICTED"), ("secondary_region", "")])
            )
            .unwrap(),
            "ALTER DATABASE \"v\" ADD REGION \"eu-west1\";
ALTER DATABASE \"v\" SET PRIMARY REGION \"eu-west1\";
ALTER DATABASE \"v\" DROP SECONDARY REGION;
ALTER DATABASE \"v\" PLACEMENT RESTRICTED;
ALTER DATABASE \"v\" OWNER TO \"root\";
ALTER DATABASE \"v\" SET sql_safe_updates = on;
ALTER DATABASE \"v\" DROP REGION \"us-east1\""
        );
        assert!(script(Variant::Cockroach, "v", &c(&[("primary_region", "")])).is_err());
        assert!(script(Variant::Cockroach, "v", &c(&[("survive", "NODE")])).is_err());
        assert!(script(Variant::Cockroach, "v", &c(&[("placement", "x")])).is_err());
        assert!(script(Variant::Cockroach, "v", &c(&[("tablespace", "x")])).is_err());
    }

    #[test]
    fn redshift_yellowbrick_risingwave() {
        assert_eq!(
            script(Variant::Redshift, "v", &c(&[("isolation", "SNAPSHOT"), ("collate", "CASE_INSENSITIVE"), ("connection_limit", "unlimited"), ("comment", "x\\y")])).unwrap(),
            "ALTER DATABASE \"v\" COLLATE CASE_INSENSITIVE;
ALTER DATABASE \"v\" CONNECTION LIMIT UNLIMITED;
COMMENT ON DATABASE \"v\" IS 'x\\\\y';
ALTER DATABASE \"v\" ISOLATION LEVEL SNAPSHOT"
        );
        assert!(script(Variant::Redshift, "v", &c(&[("connection_limit", "-1")])).is_err());
        assert!(script(Variant::Redshift, "v", &c(&[("set:work_mem", "1")])).is_err());
        assert_eq!(
            script(Variant::Yellowbrick, "v", &c(&[("read_only", "true"), ("max_size", "500 gb"), ("hot_standby", ""), ("allow_connections", "true")])).unwrap(),
            "ALTER DATABASE \"v\" ALLOW_CONNECTIONS true;
ALTER DATABASE \"v\" SET HOT_STANDBY OFF;
ALTER DATABASE \"v\" SET MAX_SIZE = '500GB';
ALTER DATABASE \"v\" SET READONLY ON"
        );
        assert_eq!(script(Variant::Yellowbrick, "v", &c(&[("max_size", "")])).unwrap(), "ALTER DATABASE \"v\" RESET MAX_SIZE");
        assert!(script(Variant::Yellowbrick, "v", &c(&[("max_size", "5 PB")])).is_err());
        assert_eq!(
            script(Variant::RisingWave, "v", &c(&[("barrier_interval_ms", "500"), ("checkpoint_frequency", ""), ("resource_group", "rg1")])).unwrap(),
            "ALTER DATABASE \"v\" SET barrier_interval_ms = 500;
ALTER DATABASE \"v\" SET checkpoint_frequency = DEFAULT;
ALTER DATABASE \"v\" SET RESOURCE_GROUP = \"rg1\" DEFERRED"
        );
        assert!(script(Variant::RisingWave, "v", &c(&[("barrier_interval_ms", "0")])).is_err());
        assert!(script(Variant::RisingWave, "v", &c(&[("comment", "x")])).is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(pretty_bytes(7_700_000.0), "7.3 MB");
        assert_eq!(bytes_to_size(500 * 1024 * 1024 * 1024), "500GB");
        assert_eq!(bytes_to_size(3 * 1024 * 1024 + 1), "4MB");
        assert_eq!(max_size("2tb").as_deref(), Some("2TB"));
        assert_eq!(max_size("100").as_deref(), Some("100MB"));
        assert!(max_size("0GB").is_none());
    }
}
