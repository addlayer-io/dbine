//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//!
//! - PostgreSQL and the distributions and services that keep its
//!   `CREATE DATABASE` (TimescaleDB, EDB, Fujitsu, AlloyDB, Cloud SQL,
//!   Aurora): owner, template, encoding, locale provider (15+), LC_COLLATE /
//!   LC_CTYPE, ICU locale (15+), builtin locale (17+), tablespace,
//!   connection limit and IS_TEMPLATE (9.5+).
//! - KingbaseES and the Greenplum family (Greenplum, Cloudberry,
//!   Greengage): the same without the locale provider, which their
//!   PostgreSQL base doesn't have.
//! - YugabyteDB: no tablespace (its tablespaces place tables, not
//!   databases), plus COLOCATION.
//! - openGauss: no IS_TEMPLATE, plus DBCOMPATIBILITY (the SQL dialect).
//! - CockroachDB: owner and the multi-region clauses (PRIMARY REGION,
//!   REGIONS, SURVIVE … FAILURE). Its ENCODING and CONNECTION LIMIT only
//!   take the defaults, so they aren't offered.
//! - Redshift: owner, connection limit, COLLATE CASE_SENSITIVE /
//!   CASE_INSENSITIVE and ISOLATION LEVEL.
//! - RisingWave: owner, resource group and the per-database barrier
//!   interval and checkpoint frequency.
//! - Yellowbrick: owner, encoding (UTF8 or LATIN9), connection limit and
//!   HOT_STANDBY.
//! - Materialize (`CREATE DATABASE name` takes nothing else): none.
//!   Denodo, H2 and CrateDB don't create databases from DBine.
//!
//! Every value is checked before it reaches the SQL; names are quoted and
//! literals escaped with the crate's helpers.

use crate::catalog::{cell, lit};
use crate::session::PgSession;
use crate::{err, Variant};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use std::collections::BTreeMap;

/// The PostgreSQL-like variants whose base has `LOCALE_PROVIDER` /
/// `ICU_LOCALE` (PostgreSQL 15+, checked against the server's version).
fn has_locale_provider(v: Variant) -> bool {
    matches!(
        v,
        Variant::Postgres | Variant::Timescale | Variant::AlloyDb | Variant::CloudSql | Variant::Aurora | Variant::Edb | Variant::Fujitsu
    )
}

/// The variants with PostgreSQL's `CREATE DATABASE` (some clauses more or
/// less).
fn pg_like(v: Variant) -> bool {
    has_locale_provider(v) || v.mpp() || matches!(v, Variant::Kingbase | Variant::Yugabyte | Variant::OpenGauss)
}

fn owner() -> Field {
    Field::new("owner", "Dueño (owner)", FieldKind::Text).help("Vacío: el usuario con el que estás conectado.")
}

fn connection_limit() -> Field {
    Field::new("connection_limit", "Límite de conexiones", FieldKind::Number).help("Vacío o -1: sin límite.")
}

pub(crate) fn fields(v: Variant) -> Vec<Field> {
    if pg_like(v) {
        let template_help = if v == Variant::OpenGauss {
            "Vacía: template0. La base nueva es una copia de esta."
        } else {
            "Vacía: template1. Para otra codificación o locale que el de template1, usá template0."
        };
        let mut out = vec![
            owner(),
            Field::new("template", "Plantilla (template)", FieldKind::Text).help(template_help),
            Field::new("encoding", "Codificación (encoding)", FieldKind::Text).placeholder("UTF8"),
        ];
        if has_locale_provider(v) {
            out.push(
                Field::new(
                    "locale_provider",
                    "Proveedor de locale (locale provider)",
                    FieldKind::Select(vec![("libc", "libc"), ("icu", "ICU"), ("builtin", "builtin (PostgreSQL 17+)")]),
                )
                .help("PostgreSQL 15 o posterior. Vacío: el de la plantilla."),
            );
        }
        out.push(
            Field::new("lc_collate", "Intercalación (LC_COLLATE)", FieldKind::Text)
                .help("Vacía: la de la plantilla. Define cómo se ordenan y comparan los textos."),
        );
        out.push(Field::new("lc_ctype", "Clasificación de caracteres (LC_CTYPE)", FieldKind::Text).help("Vacía: la de la plantilla."));
        if has_locale_provider(v) {
            out.push(
                Field::new("icu_locale", "Locale ICU (ICU_LOCALE)", FieldKind::Text)
                    .placeholder("es-AR")
                    .when("locale_provider", &["icu"]),
            );
            out.push(
                Field::new("builtin_locale", "Locale builtin (BUILTIN_LOCALE)", FieldKind::Text)
                    .placeholder("C.UTF-8")
                    .when("locale_provider", &["builtin"]),
            );
        }
        if v != Variant::Yugabyte {
            out.push(Field::new("tablespace", "Tablespace", FieldKind::Text).help("Vacío: pg_default."));
        }
        out.push(connection_limit());
        if v == Variant::OpenGauss {
            out.push(
                Field::new(
                    "dbcompatibility",
                    "Compatibilidad (DBCOMPATIBILITY)",
                    FieldKind::Select(vec![("A", "A (Oracle)"), ("B", "B (MySQL)"), ("C", "C (Teradata)"), ("PG", "PG (PostgreSQL)")]),
                )
                .help("El dialecto SQL de la base. No se puede cambiar después."),
            );
        } else {
            out.push(Field::new("is_template", "Es plantilla (IS_TEMPLATE)", FieldKind::Bool).help("Cualquier usuario con CREATEDB puede copiarla."));
        }
        if v == Variant::Yugabyte {
            out.push(
                Field::new("colocation", "Colocación (COLOCATION)", FieldKind::Select(vec![("true", "Sí"), ("false", "No")]))
                    .help("Sí: las tablas chicas comparten un tablet. Vacío: lo que diga el clúster."),
            );
        }
        return out;
    }
    match v {
        Variant::Cockroach => vec![
            owner(),
            Field::new("primary_region", "Región principal (PRIMARY REGION)", FieldKind::Text)
                .help("La vuelve una base multirregión. Las regiones salen de SHOW REGIONS FROM CLUSTER."),
            Field::new("regions", "Regiones (REGIONS)", FieldKind::Text)
                .placeholder("us-east1, us-west1")
                .help("Separadas por comas. Necesitan la región principal."),
            Field::new(
                "survive",
                "Supervivencia (SURVIVE)",
                FieldKind::Select(vec![("ZONE", "Falla de una zona (ZONE)"), ("REGION", "Falla de una región (REGION)")]),
            )
            .help("REGION necesita al menos 3 regiones."),
        ],
        Variant::Redshift => vec![
            owner(),
            Field::new("connection_limit", "Límite de conexiones", FieldKind::Text).placeholder("UNLIMITED o un número"),
            Field::new(
                "collate",
                "Intercalación (COLLATE)",
                FieldKind::Select(vec![("CASE_SENSITIVE", "Distingue mayúsculas (CASE_SENSITIVE)"), ("CASE_INSENSITIVE", "No distingue mayúsculas (CASE_INSENSITIVE)")]),
            ),
            Field::new(
                "isolation",
                "Nivel de aislamiento (ISOLATION LEVEL)",
                FieldKind::Select(vec![("SERIALIZABLE", "SERIALIZABLE"), ("SNAPSHOT", "SNAPSHOT")]),
            ),
        ],
        Variant::RisingWave => vec![
            owner(),
            Field::new("resource_group", "Grupo de recursos (resource_group)", FieldKind::Text).help("Vacío: el grupo por defecto."),
            Field::new("barrier_interval_ms", "Intervalo de barreras (ms)", FieldKind::Number)
                .help("RisingWave 2.5 o posterior, con licencia. Vacío: el del sistema."),
            Field::new("checkpoint_frequency", "Frecuencia de checkpoint (barreras)", FieldKind::Number)
                .help("RisingWave 2.5 o posterior, con licencia. Vacía: la del sistema."),
        ],
        Variant::Yellowbrick => vec![
            owner(),
            Field::new("encoding", "Codificación (encoding)", FieldKind::Select(vec![("LATIN9", "LATIN9"), ("UTF8", "UTF8")]))
                .help("Vacía: LATIN9."),
            connection_limit(),
            Field::new("hot_standby", "Destino de restauración o réplica (HOT_STANDBY)", FieldKind::Bool),
        ],
        _ => Vec::new(),
    }
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn check(ok: bool, what: &str, v: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Query(format!("{what}: «{v}» no es un valor válido")))
    }
}

/// A name for an identifier: anything printable (it's quoted).
fn name_ok(v: &str) -> bool {
    v.len() <= 128 && !v.chars().any(char::is_control)
}

/// `UTF8`, `LATIN1`, `EUC_JP`, `WIN1252`.
fn encoding_ok(v: &str) -> bool {
    v.len() <= 32 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `en_US.UTF-8`, `C`, `es-AR-x-icu`, `de@collation=phonebook`, `English_United States.1252`.
fn locale_ok(v: &str) -> bool {
    v.len() <= 128 && v.chars().all(|c| c.is_ascii_alphanumeric() || " _.-@=;".contains(c))
}

fn int(v: &str, min: i64) -> Option<i64> {
    v.parse::<i64>().ok().filter(|n| *n >= min)
}

fn ident(what: &str, v: &str) -> Result<String> {
    check(name_ok(v), what, v)?;
    Ok(quote_ident(Quote::Double, v))
}

fn yes(o: &BTreeMap<String, String>, key: &str) -> bool {
    opt(o, key).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// The `WITH` clauses of the PostgreSQL-like variants.
fn pg_clauses(v: Variant, o: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(x) = opt(o, "owner") {
        out.push(format!("OWNER = {}", ident("dueño", x)?));
    }
    if let Some(x) = opt(o, "template") {
        out.push(format!("TEMPLATE = {}", ident("plantilla", x)?));
    }
    if let Some(x) = opt(o, "encoding") {
        check(encoding_ok(x), "codificación", x)?;
        out.push(format!("ENCODING = {}", lit(v, x)));
    }
    if has_locale_provider(v) {
        if let Some(x) = opt(o, "locale_provider") {
            check(matches!(x, "libc" | "icu" | "builtin"), "proveedor de locale", x)?;
            out.push(format!("LOCALE_PROVIDER = {x}"));
        }
    }
    for (key, clause, what) in [("lc_collate", "LC_COLLATE", "intercalación"), ("lc_ctype", "LC_CTYPE", "clasificación de caracteres")] {
        if let Some(x) = opt(o, key) {
            check(locale_ok(x), what, x)?;
            out.push(format!("{clause} = {}", lit(v, x)));
        }
    }
    if has_locale_provider(v) {
        // Each only with its provider (the form hides it otherwise).
        let provider = opt(o, "locale_provider");
        for (key, clause, p, what) in [("icu_locale", "ICU_LOCALE", "icu", "locale ICU"), ("builtin_locale", "BUILTIN_LOCALE", "builtin", "locale builtin")] {
            if let Some(x) = opt(o, key).filter(|_| provider == Some(p)) {
                check(locale_ok(x), what, x)?;
                out.push(format!("{clause} = {}", lit(v, x)));
            }
        }
    }
    if v != Variant::Yugabyte {
        if let Some(x) = opt(o, "tablespace") {
            out.push(format!("TABLESPACE = {}", ident("tablespace", x)?));
        }
    }
    if let Some(x) = opt(o, "connection_limit") {
        let n = int(x, -1).ok_or_else(|| Error::Query(format!("límite de conexiones: «{x}» no es un número (-1 o más)")))?;
        out.push(format!("CONNECTION LIMIT {n}"));
    }
    if v == Variant::OpenGauss {
        if let Some(x) = opt(o, "dbcompatibility") {
            check(matches!(x, "A" | "B" | "C" | "PG"), "compatibilidad", x)?;
            out.push(format!("DBCOMPATIBILITY = '{x}'"));
        }
    } else if yes(o, "is_template") {
        out.push("IS_TEMPLATE = true".into());
    }
    if v == Variant::Yugabyte {
        if let Some(x) = opt(o, "colocation") {
            check(matches!(x, "true" | "false"), "colocación", x)?;
            out.push(format!("COLOCATION = {x}"));
        }
    }
    Ok(out)
}

/// The clauses of the other variants that take some.
fn other_clauses(v: Variant, o: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    match v {
        Variant::Cockroach => {
            // CockroachDB's grammar wants them in this order: regions,
            // survival goal, owner.
            let primary = opt(o, "primary_region");
            let regions: Vec<&str> = opt(o, "regions").map(|r| r.split(',').map(str::trim).filter(|r| !r.is_empty()).collect()).unwrap_or_default();
            let survive = opt(o, "survive");
            if primary.is_none() && (!regions.is_empty() || survive.is_some()) {
                return Err(Error::Query("las regiones y la supervivencia necesitan una región principal (PRIMARY REGION)".into()));
            }
            if let Some(p) = primary {
                out.push(format!("PRIMARY REGION {}", ident("región principal", p)?));
            }
            if !regions.is_empty() {
                let list = regions.iter().map(|r| ident("región", r)).collect::<Result<Vec<_>>>()?;
                out.push(format!("REGIONS {}", list.join(", ")));
            }
            if let Some(s) = survive {
                check(matches!(s, "ZONE" | "REGION"), "supervivencia", s)?;
                out.push(format!("SURVIVE {s} FAILURE"));
            }
            if let Some(x) = opt(o, "owner") {
                out.push(format!("OWNER = {}", ident("dueño", x)?));
            }
        }
        Variant::Redshift => {
            if let Some(x) = opt(o, "owner") {
                out.push(format!("OWNER = {}", ident("dueño", x)?));
            }
            if let Some(x) = opt(o, "connection_limit") {
                let n = if x.eq_ignore_ascii_case("UNLIMITED") {
                    "UNLIMITED".to_string()
                } else {
                    int(x, 0).ok_or_else(|| Error::Query(format!("límite de conexiones: «{x}» no es un número ni UNLIMITED")))?.to_string()
                };
                out.push(format!("CONNECTION LIMIT {n}"));
            }
            if let Some(x) = opt(o, "collate") {
                check(matches!(x, "CASE_SENSITIVE" | "CASE_INSENSITIVE"), "intercalación", x)?;
                out.push(format!("COLLATE {x}"));
            }
            if let Some(x) = opt(o, "isolation") {
                check(matches!(x, "SERIALIZABLE" | "SNAPSHOT"), "nivel de aislamiento", x)?;
                out.push(format!("ISOLATION LEVEL {x}"));
            }
        }
        Variant::RisingWave => {
            if let Some(x) = opt(o, "owner") {
                out.push(format!("OWNER = {}", ident("dueño", x)?));
            }
            if let Some(x) = opt(o, "resource_group") {
                check(name_ok(x), "grupo de recursos", x)?;
                // A string, not an identifier.
                out.push(format!("resource_group = {}", lit(v, x)));
            }
            for (key, what) in [("barrier_interval_ms", "intervalo de barreras"), ("checkpoint_frequency", "frecuencia de checkpoint")] {
                if let Some(x) = opt(o, key) {
                    let n = int(x, 1).ok_or_else(|| Error::Query(format!("{what}: «{x}» no es un número positivo")))?;
                    out.push(format!("{key} = {n}"));
                }
            }
        }
        Variant::Yellowbrick => {
            if let Some(x) = opt(o, "owner") {
                out.push(format!("OWNER = {}", ident("dueño", x)?));
            }
            if let Some(x) = opt(o, "encoding") {
                check(matches!(x, "LATIN9" | "UTF8"), "codificación", x)?;
                out.push(format!("ENCODING = {x}"));
            }
            if let Some(x) = opt(o, "connection_limit") {
                let n = int(x, -1).ok_or_else(|| Error::Query(format!("límite de conexiones: «{x}» no es un número (-1 o más)")))?;
                out.push(format!("CONNECTION LIMIT {n}"));
            }
            if yes(o, "hot_standby") {
                out.push("HOT_STANDBY ON".into());
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Why a variant doesn't create databases from DBine (as
/// `Session::create_database` says).
pub(crate) fn unsupported(v: Variant) -> Option<&'static str> {
    match v {
        Variant::Denodo => Some("las bases de Denodo se crean desde Denodo"),
        Variant::CrateDb => Some("CrateDB tiene una sola base por clúster: se organizan en esquemas"),
        Variant::H2 => Some("H2 crea la base al conectarse a un nombre nuevo (con -ifNotExists)"),
        _ => None,
    }
}

/// The statements that create `name`. Today a single `CREATE DATABASE`
/// on every variant; kept a list so a follow-up step reports which one
/// failed after the create.
pub(crate) fn statements(v: Variant, name: &str, o: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if let Some(why) = unsupported(v) {
        return Err(Error::Unsupported(why.into()));
    }
    let db = quote_ident(Quote::Double, name);
    let clauses = if pg_like(v) { pg_clauses(v, o)? } else { other_clauses(v, o)? };
    if clauses.is_empty() {
        return Ok(vec![format!("CREATE DATABASE {db}")]);
    }
    Ok(vec![format!("CREATE DATABASE {db}\n    WITH {}", clauses.join("\n    "))])
}

/// What "Ver script" shows.
pub(crate) fn script(v: Variant, name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    Ok(statements(v, name, o)?.join(";\n"))
}

/// Clauses the connected server is too old for, refused before running
/// with the version they need (`version`: `server_version_num`, 0 when
/// unknown).
pub(crate) fn check_version(v: Variant, version: i32, o: &BTreeMap<String, String>) -> Result<()> {
    if !pg_like(v) || version <= 0 {
        return Ok(());
    }
    let need = |clause: &str, pg: &str| Err(Error::Query(format!("{clause} necesita PostgreSQL {pg} o posterior en el servidor")));
    if has_locale_provider(v) && version < 150000 && opt(o, "locale_provider").is_some() {
        return need("LOCALE_PROVIDER", "15");
    }
    if has_locale_provider(v) && version < 170000 && opt(o, "locale_provider") == Some("builtin") {
        return need("El proveedor builtin", "17");
    }
    if v != Variant::OpenGauss && version < 90500 && yes(o, "is_template") {
        return need("IS_TEMPLATE", "9.5");
    }
    Ok(())
}

impl PgSession {
    /// All the cells of the first column.
    async fn column(&self, sql: &str) -> Vec<String> {
        self.text(sql).await.map(|rows| rows.iter().filter_map(|r| r.get(0).map(str::to_string)).collect()).unwrap_or_default()
    }

    async fn one(&self, sql: &str) -> Option<String> {
        self.column(sql).await.into_iter().next()
    }

    /// The server's roles (default: the one connected), templates,
    /// encodings, locales from `pg_collation`, tablespaces, and the
    /// template's own settings as the defaults; the regions on
    /// CockroachDB.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let v = self.variant;
        let mut out = Vec::new();
        let fields = fields(v);
        if fields.is_empty() {
            return Ok(out);
        }
        let roles_sql = match v {
            Variant::Redshift => "SELECT usename FROM pg_user ORDER BY 1",
            Variant::RisingWave => "SELECT name FROM rw_catalog.rw_users ORDER BY 1",
            _ => "SELECT rolname FROM pg_roles WHERE rolname NOT LIKE 'pg\\_%' AND rolname NOT LIKE 'crdb\\_internal%' ORDER BY 1",
        };
        let roles = self.column(roles_sql).await;
        let me = self.one("SELECT current_user").await;
        out.push(FieldChoices { key: "owner".into(), default: me, values: roles });

        if v == Variant::Cockroach {
            let regions = self.column("SELECT region FROM [SHOW REGIONS FROM CLUSTER] ORDER BY 1").await;
            out.push(FieldChoices { key: "primary_region".into(), default: None, values: regions.clone() });
            out.push(FieldChoices { key: "regions".into(), default: None, values: regions });
            return Ok(out);
        }
        if v == Variant::Redshift {
            out.push(FieldChoices { key: "connection_limit".into(), default: Some("UNLIMITED".into()), values: vec!["UNLIMITED".into()] });
            out.push(FieldChoices { key: "collate".into(), default: Some("CASE_SENSITIVE".into()), values: Vec::new() });
            return Ok(out);
        }
        if v == Variant::RisingWave {
            let groups = self.column("SELECT name FROM rw_catalog.rw_resource_groups ORDER BY 1").await;
            out.push(FieldChoices { key: "resource_group".into(), default: None, values: groups });
            return Ok(out);
        }
        if !pg_like(v) {
            return Ok(out);
        }

        let template = if v == Variant::OpenGauss { "template0" } else { "template1" };
        let templates = self.column("SELECT datname FROM pg_database WHERE datistemplate OR datallowconn ORDER BY NOT datistemplate, datname").await;
        out.push(FieldChoices { key: "template".into(), default: Some(template.into()), values: templates });

        // The template's settings are what an empty field gets.
        let ver = self.version;
        let provider = if has_locale_provider(v) && ver >= 150000 { "d.datlocprovider::text" } else { "NULL" };
        let icu = if !has_locale_provider(v) || ver < 150000 {
            "NULL"
        } else if ver >= 170000 {
            "d.datlocale"
        } else {
            "d.daticulocale"
        };
        let compat = if v == Variant::OpenGauss { "d.datcompatibility::text" } else { "NULL" };
        let row = self
            .text(&format!(
                "SELECT pg_encoding_to_char(d.encoding) AS enc, d.datcollate::text AS coll, d.datctype::text AS ctype, \
                 {provider} AS prov, {icu} AS icu, {compat} AS compat, t.spcname AS spc, d.datconnlimit::text AS lim \
                 FROM pg_database d LEFT JOIN pg_tablespace t ON t.oid = d.dattablespace WHERE d.datname = '{template}'"
            ))
            .await
            .ok()
            .and_then(|rows| rows.into_iter().next());
        let get = |c: &str| row.as_ref().and_then(|r| cell(r, c));

        // Client-only encodings can't be a database's, except where the
        // server takes them (openGauss and KingbaseES store GBK, GB18030).
        // Unknown ids give '', which openGauss's Oracle mode reads as NULL:
        // hence the length.
        let client_only = if matches!(v, Variant::OpenGauss | Variant::Kingbase) {
            "'SJIS', 'BIG5', 'UHC', 'JOHAB', 'SHIFT_JIS_2004'"
        } else {
            "'SJIS', 'BIG5', 'GBK', 'UHC', 'GB18030', 'JOHAB', 'SHIFT_JIS_2004'"
        };
        let encodings = self
            .column(&format!(
                "SELECT e FROM (SELECT pg_encoding_to_char(i) AS e FROM generate_series(0, 63) AS i) s \
                 WHERE COALESCE(length(e), 0) > 0 AND e NOT IN ({client_only}) ORDER BY e"
            ))
            .await;
        out.push(FieldChoices { key: "encoding".into(), default: get("enc"), values: encodings });

        // libc locales; before PostgreSQL 10 every collation is one.
        let libc = if ver >= 100000 && v != Variant::OpenGauss { "WHERE collprovider = 'c'" } else { "" };
        let locales = self.column(&format!("SELECT DISTINCT collcollate::text FROM pg_collation {libc} ORDER BY 1")).await;
        let locales: Vec<String> = locales.into_iter().filter(|l| !l.is_empty()).collect();
        out.push(FieldChoices { key: "lc_collate".into(), default: get("coll"), values: locales.clone() });
        out.push(FieldChoices { key: "lc_ctype".into(), default: get("ctype"), values: locales });

        if has_locale_provider(v) && ver >= 150000 {
            let provider = get("prov").map(|p| match p.as_str() {
                "i" => "icu".to_string(),
                "b" => "builtin".to_string(),
                _ => "libc".to_string(),
            });
            out.push(FieldChoices { key: "locale_provider".into(), default: provider, values: Vec::new() });
            let col = if ver >= 170000 { "colllocale" } else { "colliculocale" };
            let icu = self.column(&format!("SELECT DISTINCT {col}::text FROM pg_collation WHERE collprovider = 'i' AND {col} IS NOT NULL ORDER BY 1")).await;
            out.push(FieldChoices { key: "icu_locale".into(), default: get("icu"), values: icu });
            if ver >= 170000 {
                let builtin = self.column("SELECT DISTINCT colllocale::text FROM pg_collation WHERE collprovider = 'b' ORDER BY 1").await;
                out.push(FieldChoices { key: "builtin_locale".into(), default: None, values: builtin });
            }
        }
        if v != Variant::Yugabyte {
            let spaces = self.column("SELECT spcname FROM pg_tablespace WHERE spcname <> 'pg_global' ORDER BY 1").await;
            out.push(FieldChoices { key: "tablespace".into(), default: get("spc"), values: spaces });
        }
        out.push(FieldChoices { key: "connection_limit".into(), default: get("lim").or(Some("-1".into())), values: Vec::new() });
        if v == Variant::OpenGauss {
            out.push(FieldChoices { key: "dbcompatibility".into(), default: get("compat"), values: Vec::new() });
        }
        Ok(out)
    }

    /// Run the statements one by one over the simple protocol: a failure
    /// after the create says the database exists but that step didn't
    /// apply.
    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        if o.values().all(|v| v.trim().is_empty()) {
            return dbine_driver::Session::create_database(self, name).await;
        }
        check_version(self.variant, self.version, o)?;
        let statements = statements(self.variant, name, o)?;
        for (i, sql) in statements.iter().enumerate() {
            if let Err(e) = self.client.batch_execute(sql).await.map_err(err) {
                return Err(if i == 0 {
                    e
                } else {
                    Error::Query(format!("la base «{name}» se creó, pero falló este paso: {sql}\n{e}"))
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_is_the_old_create() {
        for v in Variant::ALL {
            if unsupported(v).is_some() {
                assert!(script(v, "ventas", &o(&[])).is_err(), "{v:?}");
                continue;
            }
            assert_eq!(script(v, "ventas", &o(&[])).unwrap(), "CREATE DATABASE \"ventas\"", "{v:?}");
            // Blank values and an unchecked box are no option at all.
            assert_eq!(script(v, "ven\"tas", &o(&[("owner", " "), ("is_template", ""), ("hot_standby", "")])).unwrap(), "CREATE DATABASE \"ven\"\"tas\"");
        }
        // Materialize has no clauses: whatever comes is ignored.
        assert_eq!(script(Variant::Materialize, "v", &o(&[("owner", "x")])).unwrap(), "CREATE DATABASE \"v\"");
    }

    #[test]
    fn postgres_every_option() {
        let s = script(
            Variant::Postgres,
            "ventas",
            &o(&[
                ("owner", "app"),
                ("template", "template0"),
                ("encoding", "UTF8"),
                ("locale_provider", "icu"),
                ("lc_collate", "en_US.utf8"),
                ("lc_ctype", "en_US.utf8"),
                ("icu_locale", "es-AR"),
                ("builtin_locale", "C.UTF-8"),
                ("tablespace", "rápido"),
                ("connection_limit", "20"),
                ("is_template", "true"),
            ]),
        )
        .unwrap();
        assert_eq!(
            s,
            "CREATE DATABASE \"ventas\"
    WITH OWNER = \"app\"
    TEMPLATE = \"template0\"
    ENCODING = 'UTF8'
    LOCALE_PROVIDER = icu
    LC_COLLATE = 'en_US.utf8'
    LC_CTYPE = 'en_US.utf8'
    ICU_LOCALE = 'es-AR'
    TABLESPACE = \"rápido\"
    CONNECTION LIMIT 20
    IS_TEMPLATE = true"
        );
        let b = script(Variant::Postgres, "v", &o(&[("locale_provider", "builtin"), ("builtin_locale", "C.UTF-8")])).unwrap();
        assert_eq!(b, "CREATE DATABASE \"v\"\n    WITH LOCALE_PROVIDER = builtin\n    BUILTIN_LOCALE = 'C.UTF-8'");
    }

    #[test]
    fn variant_quirks() {
        let yb = script(Variant::Yugabyte, "v", &o(&[("colocation", "true"), ("tablespace", "t"), ("locale_provider", "icu")])).unwrap();
        assert_eq!(yb, "CREATE DATABASE \"v\"\n    WITH COLOCATION = true");
        let og = script(Variant::OpenGauss, "v", &o(&[("dbcompatibility", "B"), ("is_template", "true"), ("encoding", "GBK")])).unwrap();
        assert_eq!(og, "CREATE DATABASE \"v\"\n    WITH ENCODING = 'GBK'\n    DBCOMPATIBILITY = 'B'");
        // Greenplum: no locale provider.
        let gp = script(Variant::Greenplum, "v", &o(&[("locale_provider", "icu"), ("lc_collate", "C")])).unwrap();
        assert_eq!(gp, "CREATE DATABASE \"v\"\n    WITH LC_COLLATE = 'C'");
        assert!(!fields(Variant::Greenplum).iter().any(|f| f.key == "locale_provider"));
        assert!(fields(Variant::Aurora).iter().any(|f| f.key == "locale_provider"));
        assert!(fields(Variant::Materialize).is_empty() && fields(Variant::H2).is_empty());
    }

    #[test]
    fn cockroach_regions() {
        let s = script(
            Variant::Cockroach,
            "v",
            &o(&[("owner", "root"), ("primary_region", "us-east1"), ("regions", "us-east1, us-west1,,eu-west1"), ("survive", "REGION")]),
        )
        .unwrap();
        assert_eq!(
            s,
            "CREATE DATABASE \"v\"\n    WITH PRIMARY REGION \"us-east1\"\n    REGIONS \"us-east1\", \"us-west1\", \"eu-west1\"\n    SURVIVE REGION FAILURE\n    OWNER = \"root\""
        );
        assert!(script(Variant::Cockroach, "v", &o(&[("survive", "ZONE")])).is_err(), "survival needs a primary region");
    }

    #[test]
    fn redshift_risingwave_yellowbrick() {
        assert_eq!(
            script(Variant::Redshift, "v", &o(&[("owner", "u"), ("connection_limit", "unlimited"), ("collate", "CASE_INSENSITIVE"), ("isolation", "SNAPSHOT")])).unwrap(),
            "CREATE DATABASE \"v\"\n    WITH OWNER = \"u\"\n    CONNECTION LIMIT UNLIMITED\n    COLLATE CASE_INSENSITIVE\n    ISOLATION LEVEL SNAPSHOT"
        );
        assert_eq!(
            script(Variant::RisingWave, "v", &o(&[("owner", "root"), ("barrier_interval_ms", "500"), ("checkpoint_frequency", "10")])).unwrap(),
            "CREATE DATABASE \"v\"\n    WITH OWNER = \"root\"\n    barrier_interval_ms = 500\n    checkpoint_frequency = 10"
        );
        assert_eq!(
            script(Variant::Yellowbrick, "v", &o(&[("encoding", "UTF8"), ("connection_limit", "5"), ("hot_standby", "true")])).unwrap(),
            "CREATE DATABASE \"v\"\n    WITH ENCODING = UTF8\n    CONNECTION LIMIT 5\n    HOT_STANDBY ON"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [
            ("encoding", "UTF8'; DROP"),
            ("lc_collate", "en_US'--"),
            ("locale_provider", "icu;"),
            ("connection_limit", "-2"),
            ("connection_limit", "10 OR 1"),
            ("owner", "a\nb"),
        ] {
            assert!(script(Variant::Postgres, "v", &o(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script(Variant::OpenGauss, "v", &o(&[("dbcompatibility", "MYSQL")])).is_err());
        assert!(script(Variant::Yugabyte, "v", &o(&[("colocation", "yes")])).is_err());
        assert!(script(Variant::Cockroach, "v", &o(&[("primary_region", "r"), ("survive", "NODE")])).is_err());
        assert!(script(Variant::Redshift, "v", &o(&[("collate", "CI; x")])).is_err());
        assert!(script(Variant::Redshift, "v", &o(&[("connection_limit", "-1")])).is_err());
        assert!(script(Variant::RisingWave, "v", &o(&[("barrier_interval_ms", "0")])).is_err());
        assert!(script(Variant::Yellowbrick, "v", &o(&[("encoding", "LATIN1")])).is_err());
        // Quotes inside names are doubled, not rejected.
        assert!(script(Variant::Postgres, "v", &o(&[("owner", "o\"k")])).unwrap().contains("OWNER = \"o\"\"k\""));
    }

    #[test]
    fn old_servers_refuse_newer_clauses() {
        let icu = o(&[("locale_provider", "icu"), ("icu_locale", "es")]);
        assert!(check_version(Variant::Postgres, 140000, &icu).is_err());
        assert!(check_version(Variant::Postgres, 150000, &icu).is_ok());
        assert!(check_version(Variant::Postgres, 160000, &o(&[("locale_provider", "builtin")])).is_err());
        assert!(check_version(Variant::Greenplum, 90400, &o(&[("is_template", "true")])).is_err());
        assert!(check_version(Variant::Greenplum, 120000, &o(&[("is_template", "true")])).is_ok());
        assert!(check_version(Variant::Postgres, 0, &icu).is_ok(), "unknown version: let the server say");
    }
}
