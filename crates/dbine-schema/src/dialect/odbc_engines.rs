//! The ODBC presets that are engines of their own: Ingres, Actian Zen,
//! Altibase, CUBRID, Dameng, HeavyDB, InterSystems IRIS and Caché, Machbase,
//! Access, dBase, Mimer SQL, MonetDB, NuoDB, Ocient, Virtuoso, OpenEdge,
//! MaxDB, SQream, Apache Ignite 2 and 3, NetSuite SuiteAnalytics Connect and
//! the generic `odbc` preset.
//!
//! Types come the way the ODBC driver's `database_schema` reports them: the
//! SQLColumns `TYPE_NAME`, with `(size)` added to character and binary types
//! and `(precision,scale)` to exact numerics (`format_type` in
//! `crates/drivers/odbc/src/lib.rs`); time types carry no precision. Native
//! names on the way out follow each preset's `designer()` types and what its
//! `table_ddl` writes (`crates/drivers/odbc/src/design.rs`).
//!
//! The generic preset doesn't know the engine behind the DSN: it reads the
//! union of the common spellings (standard SQL, then PostgreSQL, MySQL,
//! Oracle and SQL Server names) and writes conservative standard SQL,
//! reporting that the types need a look against the real engine.

use super::postgres::{longest, precision_loss};
use super::{mssql, mysql, oracle, postgres};
use super::{standard_default, Caps, Dialect, IdentCase, Rendered, ALL_ACTIONS};
use crate::default::{quote, DefaultValue};
use crate::issue::IssueCode::*;
use crate::issue::Report;
use crate::issue::Severity::*;
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::{IndexDef, KeyDef, TableSchema};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Generic,
    NetSuite,
    Ingres,
    Zen,
    Altibase,
    Cubrid,
    Dameng,
    HeavyDb,
    /// InterSystems IRIS and Caché.
    Iris,
    Machbase,
    Access,
    DBase,
    Mimer,
    MonetDb,
    NuoDb,
    Ocient,
    Virtuoso,
    OpenEdge,
    MaxDb,
    Sqream,
    Ignite,
    Ignite3,
}

impl Kind {
    fn of(driver_id: &str) -> Option<Kind> {
        Some(match driver_id {
            "odbc" => Kind::Generic,
            "netsuite" => Kind::NetSuite,
            "ingres" => Kind::Ingres,
            "zen" => Kind::Zen,
            "altibase" => Kind::Altibase,
            "cubrid" => Kind::Cubrid,
            "dameng" => Kind::Dameng,
            "heavydb" => Kind::HeavyDb,
            "iris" | "cache" => Kind::Iris,
            "machbase" => Kind::Machbase,
            "access" => Kind::Access,
            "dbase" => Kind::DBase,
            "mimer" => Kind::Mimer,
            "monetdb" => Kind::MonetDb,
            "nuodb" => Kind::NuoDb,
            "ocient" => Kind::Ocient,
            "virtuoso" => Kind::Virtuoso,
            "openedge" => Kind::OpenEdge,
            "maxdb" => Kind::MaxDb,
            "sqream" => Kind::Sqream,
            "ignite" => Kind::Ignite,
            "ignite3" => Kind::Ignite3,
            _ => return None,
        })
    }

    fn id(self) -> &'static str {
        match self {
            Kind::Generic => "odbc",
            Kind::NetSuite => "netsuite",
            Kind::Ingres => "ingres",
            Kind::Zen => "zen",
            Kind::Altibase => "altibase",
            Kind::Cubrid => "cubrid",
            Kind::Dameng => "dameng",
            Kind::HeavyDb => "heavydb",
            Kind::Iris => "iris",
            Kind::Machbase => "machbase",
            Kind::Access => "access",
            Kind::DBase => "dbase",
            Kind::Mimer => "mimer",
            Kind::MonetDb => "monetdb",
            Kind::NuoDb => "nuodb",
            Kind::Ocient => "ocient",
            Kind::Virtuoso => "virtuoso",
            Kind::OpenEdge => "openedge",
            Kind::MaxDb => "maxdb",
            Kind::Sqream => "sqream",
            Kind::Ignite => "ignite",
            Kind::Ignite3 => "ignite3",
        }
    }
}

/// One engine behind the ODBC driver.
pub struct OdbcEngine(Kind);

static ENGINES: [OdbcEngine; 22] = [
    OdbcEngine(Kind::Generic),
    OdbcEngine(Kind::NetSuite),
    OdbcEngine(Kind::Ingres),
    OdbcEngine(Kind::Zen),
    OdbcEngine(Kind::Altibase),
    OdbcEngine(Kind::Cubrid),
    OdbcEngine(Kind::Dameng),
    OdbcEngine(Kind::HeavyDb),
    OdbcEngine(Kind::Iris),
    OdbcEngine(Kind::Machbase),
    OdbcEngine(Kind::Access),
    OdbcEngine(Kind::DBase),
    OdbcEngine(Kind::Mimer),
    OdbcEngine(Kind::MonetDb),
    OdbcEngine(Kind::NuoDb),
    OdbcEngine(Kind::Ocient),
    OdbcEngine(Kind::Virtuoso),
    OdbcEngine(Kind::OpenEdge),
    OdbcEngine(Kind::MaxDb),
    OdbcEngine(Kind::Sqream),
    OdbcEngine(Kind::Ignite),
    OdbcEngine(Kind::Ignite3),
];

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    let k = Kind::of(driver_id)?;
    ENGINES.iter().find(|e| e.0 == k).map(|e| e as &dyn Dialect)
}

macro_rules! dispatch {
    ($kind:expr, $f:ident ( $($a:expr),* )) => {
        match $kind {
            Kind::Generic => generic::$f($($a),*),
            Kind::NetSuite => netsuite::$f($($a),*),
            Kind::Ingres => ingres::$f($($a),*),
            Kind::Zen => zen::$f($($a),*),
            Kind::Altibase => altibase::$f($($a),*),
            Kind::Cubrid => cubrid::$f($($a),*),
            Kind::Dameng => dameng::$f($($a),*),
            Kind::HeavyDb => heavydb::$f($($a),*),
            Kind::Iris => iris::$f($($a),*),
            Kind::Machbase => machbase::$f($($a),*),
            Kind::Access => access::$f($($a),*),
            Kind::DBase => dbase::$f($($a),*),
            Kind::Mimer => mimer::$f($($a),*),
            Kind::MonetDb => monetdb::$f($($a),*),
            Kind::NuoDb => nuodb::$f($($a),*),
            Kind::Ocient => ocient::$f($($a),*),
            Kind::Virtuoso => virtuoso::$f($($a),*),
            Kind::OpenEdge => openedge::$f($($a),*),
            Kind::MaxDb => maxdb::$f($($a),*),
            Kind::Sqream => sqream::$f($($a),*),
            Kind::Ignite => ignite::$f($($a),*),
            Kind::Ignite3 => ignite3::$f($($a),*),
        }
    };
}

impl Dialect for OdbcEngine {
    fn id(&self) -> &'static str {
        self.0.id()
    }

    fn target_refusal(&self, _: &str) -> Option<&'static str> {
        matches!(self.0, Kind::NetSuite)
            .then_some("SuiteAnalytics Connect de NetSuite es de solo lectura.")
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        dispatch!(self.0, parse(t))
    }

    fn render_type(&self, t: &L) -> Rendered {
        dispatch!(self.0, render(t))
    }

    fn render_default(&self, d: &DefaultValue, ty: &L) -> Option<String> {
        dispatch!(self.0, default(d, ty))
    }

    fn caps(&self) -> Caps {
        dispatch!(self.0, caps())
    }

    /// The ODBC driver flags auto-increment from `identity`, `serial` and
    /// `auto_increment` in the type name; these engines have more spellings
    /// (Access `COUNTER`, Zen `AUTOINC`).
    fn implies_auto_increment(&self, t: &TypeSpec) -> bool {
        t.has("identity")
            || t.has("auto_increment")
            || t.has("autoincrement")
            || matches!(
                t.name.as_str(),
                "identity" | "smallidentity" | "bigidentity" | "counter" | "autoincrement" | "autoinc" | "serial"
                    | "bigserial" | "smallserial" | "serial4" | "serial8"
            )
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        dispatch!(self.0, finalize(t, report))
    }
}

// ---------------------------------------------------------------------------
// Shared pieces.

fn ex(s: impl Into<String>) -> Rendered {
    Rendered::exact(s)
}

/// "El destino" at the start of a sentence, engine names as they are.
fn subject(engine: &str) -> String {
    match engine.strip_prefix("el ") {
        Some(rest) => format!("El {rest}"),
        None => engine.to_string(),
    }
}

/// The length in `VARCHAR2(20 CHAR)` / `VARCHAR(20)`.
fn len0(t: &TypeSpec) -> Option<u32> {
    t.args.first().and_then(|a| a.split_whitespace().next()).and_then(|n| n.parse().ok())
}

/// The smallest of `sizes` (bytes, name) that holds every value of the
/// integer; `wide` when none does.
fn integer(bytes: u8, unsigned: bool, sizes: &[(u8, &str)], wide: impl FnOnce() -> Rendered, engine: &str) -> Rendered {
    let need = L::signed_bytes_for(bytes, unsigned);
    match sizes.iter().find(|(b, _)| *b >= need) {
        Some((_, name)) if unsigned => {
            ex(*name).with(Info, TypeChanged, format!("{} no tiene enteros sin signo: se usa un tipo más grande con signo.", subject(engine)))
        }
        Some((_, name)) => ex(*name),
        None => wide(),
    }
}

/// Whole numbers wider than the engine's integers, as an exact decimal.
fn wide_int(bytes: u8, unsigned: bool, dec: &str, max_p: u32, engine: &str) -> Rendered {
    let digits: u32 = if bytes > 8 { 39 } else if unsigned { 20 } else { 19 };
    let what = L::Int { bytes, unsigned }.describe();
    if digits <= max_p {
        ex(format!("{dec}({digits}, 0)")).with(Info, TypeChanged, format!("El {what} queda como {dec}({digits}, 0)."))
    } else {
        ex(format!("{dec}({max_p}, 0)"))
            .with(Loss, RangeLoss, format!("{} admite hasta {max_p} dígitos: no entran los valores de {digits}.", subject(engine)))
    }
}

/// `NAME(p, s)` within the engine's limits.
fn decimal(name: &str, p: u32, s: Option<u32>, max_p: u32, max_s: u32, engine: &str) -> Rendered {
    let p = p.max(1);
    let s = s.unwrap_or(0).min(p);
    if p <= max_p && s <= max_s {
        ex(format!("{name}({p}, {s})"))
    } else if p <= max_p {
        ex(format!("{name}({p}, {max_s})"))
            .with(Loss, PrecisionLoss, format!("{} admite hasta {max_s} decimales; el origen tiene {s}.", subject(engine)))
    } else {
        ex(format!("{name}({max_p}, {})", s.min(max_s)))
            .with(Loss, PrecisionLoss, format!("{} admite hasta {max_p} dígitos; el origen tiene {p}.", subject(engine)))
    }
}

/// A numeric without precision in the source, where the target has none
/// unbounded.
fn no_precision(native: &str) -> Rendered {
    ex(native).with(Loss, PrecisionLoss, format!("El origen no fija la precisión: se usa {native}."))
}

fn money(native: &str) -> Rendered {
    ex(native).with(Info, TypeChanged, format!("Moneda como {native}."))
}

/// `NAME(n)` up to `max`, else `long`.
fn sized(name: &str, n: u32, max: u32, long: &str, engine: &str) -> Rendered {
    if n <= max {
        ex(format!("{name}({n})"))
    } else {
        ex(long).with(Info, TypeChanged, format!("{name}({n}) supera el máximo de {engine} ({max}): se usa {long}."))
    }
}

/// `NAME(p)`: the source's fractional digits (6 when it didn't say),
/// capped at `max`.
fn with_prec(name: &str, p: Option<u8>, max: u8, suffix: &str) -> Rendered {
    let digits = p.unwrap_or(6).min(max);
    ex(format!("{name}({digits}){suffix}")).with_loss(precision_loss(p, max))
}

/// A type with a fixed number of fractional digits; an unknown source
/// precision counts as microseconds.
fn fixed_frac(name: &str, p: Option<u8>, frac: u8) -> Rendered {
    ex(name).with_loss(precision_loss(Some(p.unwrap_or(6)), frac))
}

fn tz_loss(r: Rendered, tz: bool, engine: &str) -> Rendered {
    if tz {
        r.with(Loss, TimeZoneLoss, format!("{} no guarda la zona horaria: conviene guardar en UTC.", subject(engine)))
    } else {
        r
    }
}

fn with_tz(ty: &L) -> bool {
    matches!(ty, L::Timestamp { tz: true, .. } | L::Time { tz: true, .. })
}

/// Spellings the shared fallbacks use.
struct Names {
    engine: &'static str,
    /// Bounded text: `{varchar}(n)`.
    varchar: &'static str,
    /// Fixed text: `{fixed}(n)`.
    fixed: &'static str,
    /// Unbounded text.
    text: &'static str,
    smallint: &'static str,
    /// Stand-in for a row version (8 bytes).
    rowversion: &'static str,
}

/// The kinds most of these engines lack, as text or the closest type.
fn shared(t: &L, n: &Names) -> Rendered {
    let e = subject(n.engine);
    match t {
        L::Bit { len } => {
            let r = match len {
                Some(l) => ex(format!("{}({l})", n.varchar)),
                None => ex(n.text),
            };
            r.with(Warning, TypeApproximated, format!("{e} no tiene cadenas de bits: se guardan como texto de ceros y unos."))
        }
        L::Interval => ex(format!("{}(64)", n.varchar)).with(Loss, TypeApproximated, format!("{e} no tiene intervalos: queda como texto.")),
        L::Year => ex(n.smallint).with(Info, TypeChanged, format!("Año como {}.", n.smallint)),
        L::Uuid => ex(format!("{}(36)", n.fixed)).with(Info, TypeChanged, format!("UUID como {}(36).", n.fixed)),
        L::Json { .. } => ex(n.text).with(Info, TypeChanged, "JSON como texto."),
        L::Xml => ex(n.text).with(Info, TypeApproximated, "XML como texto."),
        L::Enum { values } => ex(format!("{}({})", n.varchar, longest(values)))
            .with(Warning, TypeApproximated, format!("{e} no tiene enumerados: queda como texto. Valores: {}.", values.join(", "))),
        L::Set { values } => {
            let width = values.iter().map(|v| v.chars().count() + 1).sum::<usize>().max(1);
            ex(format!("{}({width})", n.varchar))
                .with(Warning, TypeApproximated, format!("{e} no tiene conjuntos: queda como texto separado por comas. Valores: {}.", values.join(", ")))
        }
        L::Array { .. } | L::Map { .. } => ex(n.text).with(Warning, TypeApproximated, format!("{e} no tiene arreglos ni mapas: se guarda como JSON en texto.")),
        L::Geometry { .. } => ex(n.text).with(Warning, TypeApproximated, format!("{e} no tiene tipos espaciales: se guarda como texto (WKT).")),
        L::Inet => ex(format!("{}(45)", n.varchar)).with(Info, TypeApproximated, "Dirección IP como texto."),
        L::MacAddr => ex(format!("{}(17)", n.varchar)).with(Info, TypeApproximated, "Dirección MAC como texto."),
        L::RowVersion => ex(n.rowversion).with(Warning, TypeApproximated, format!("{e} no tiene versión de fila automática: no se actualiza sola.")),
        L::Other { native } => ex(native.clone()),
        other => ex(n.text).with(Warning, TypeApproximated, format!("{} como texto.", other.describe())),
    }
}

/// Engines without primary keys. With `as_index` the key stays as a unique
/// index named within `max_name` bytes, so uniqueness survives.
fn drop_primary_key(t: &mut TableSchema, report: &mut Report, engine: &str, as_index: Option<usize>) {
    let Some(k) = t.primary_key.take() else { return };
    match as_index {
        Some(max) => {
            let mut taken: HashSet<String> = t.indexes.iter().map(|i| i.name.to_ascii_lowercase()).collect();
            let wanted = k.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("pk_{}", t.name));
            let name = crate::ident::fit(&wanted, max, &mut taken);
            report.push(
                Warning,
                PrimaryKeyDropped,
                &t.name,
                None,
                format!("{engine} no tiene claves primarias: la clave ({}) queda como índice único «{name}».", k.columns.join(", ")),
            );
            t.indexes.insert(0, IndexDef { name, columns: k.columns, unique: true, kind: None, filter: None, ..Default::default() });
        }
        None => report.push(
            Dropped,
            PrimaryKeyDropped,
            &t.name,
            None,
            format!("{engine} no tiene claves primarias: se omite la clave ({}).", k.columns.join(", ")),
        ),
    }
}

/// Engines whose indexes can't be unique.
fn no_unique_indexes(t: &mut TableSchema, report: &mut Report, engine: &str) {
    for ix in t.indexes.iter_mut().filter(|i| i.unique) {
        ix.unique = false;
        report.push(Warning, IndexChanged, &t.name, Some(&ix.name), format!("{engine} no tiene índices únicos: el índice queda sin unicidad."));
    }
}

/// Engines that can't create a table without a primary key (Ignite): a
/// unique index on NOT NULL columns becomes the key; otherwise it's reported.
fn require_primary_key(t: &mut TableSchema, report: &mut Report, engine: &str) {
    if t.primary_key.is_some() {
        return;
    }
    let not_null = |c: &String| t.columns.iter().any(|x| &x.name == c && !x.nullable);
    if let Some(pos) = t.indexes.iter().position(|i| i.unique && i.filter.is_none() && !i.columns.is_empty() && i.columns.iter().all(not_null)) {
        let ix = t.indexes.remove(pos);
        report.push(
            Info,
            PrimaryKeyAdded,
            &t.name,
            Some(&ix.name),
            format!("{engine} exige clave primaria: se usa el índice único «{}» ({}).", ix.name, ix.columns.join(", ")),
        );
        t.primary_key = Some(KeyDef { name: None, columns: ix.columns });
    } else {
        report.push(
            Warning,
            PrimaryKeyAdded,
            &t.name,
            None,
            format!("{engine} exige clave primaria y la tabla no tiene ni un índice único sin nulos: hay que elegir una antes de crearla."),
        );
    }
}

fn no_finalize(_: &mut TableSchema, _: &mut Report) {}

fn caps(foreign_keys: bool, on_delete: &'static [&'static str], on_update: &'static [&'static str]) -> Caps {
    Caps {
        foreign_keys,
        on_delete,
        on_update,
        indexes: true,
        partial_indexes: false,
        supports_include: false,
        auto_increment: true,
        defaults: true,
        nullability: true,
        comments: false,
        max_identifier: 128,
        case: IdentCase::Upper,
    }
}

// ---------------------------------------------------------------------------
// Generic ODBC: the engine is unknown.

mod generic {
    use super::*;

    pub const N: Names = Names { engine: "el destino", varchar: "VARCHAR", fixed: "CHAR", text: "CLOB", smallint: "SMALLINT", rowversion: "BINARY(8)" };

    pub fn parse(t: &TypeSpec) -> L {
        let len = len0(t);
        let open = t.is_max() || len.is_none();
        match t.name.as_str() {
            // ODBC SQL_BIT is a boolean.
            "bit" if len.unwrap_or(1) == 1 => L::Bool,
            // Signed in some engines, 0–255 in others: two bytes hold both.
            "tinyint" | "byte" => L::int(2),
            "char" | "character" | "wchar" | "nchar" | "national char" | "national character" | "graphic" => {
                L::Char { len: len.or(Some(1)), unicode: true }
            }
            "varchar" | "character varying" | "char varying" | "wvarchar" | "nvarchar" | "nvarchar2" | "national character varying"
            | "national char varying" | "varchar2" | "vargraphic" | "string" => {
                if open {
                    L::Text { unicode: true }
                } else {
                    L::Varchar { len, unicode: true }
                }
            }
            "longvarchar" | "wlongvarchar" | "long varchar" | "long nvarchar" | "clob" | "nclob" | "dbclob" | "ntext" | "text"
            | "memo" | "longtext" | "mediumtext" | "longchar" | "long vargraphic" | "lvarchar" => L::Text { unicode: true },
            "binary" if !open => L::Binary { len },
            "varbinary" | "binary varying" | "varbyte" | "raw" if !open => L::Varbinary { len },
            "binary" | "varbinary" | "binary varying" | "varbyte" | "raw" | "longvarbinary" | "long varbinary" | "blob" | "image"
            | "bytea" | "long raw" | "long byte" | "longblob" | "longbinary" => L::Blob,
            "guid" | "uniqueidentifier" | "uuid" => L::Uuid,
            "" if t.has("identity") => L::int(4),
            // SQL Server / Sybase money (4 decimals), Access currency.
            "money" | "smallmoney" | "currency" => L::Money,
            _ => fallback(t),
        }
    }

    /// PostgreSQL, MySQL, Oracle and SQL Server names, in that order.
    fn fallback(t: &TypeSpec) -> L {
        let families: [&dyn Dialect; 4] = [&postgres::Postgres, &mysql::MySql, &oracle::Oracle, &mssql::MsSql];
        families
            .iter()
            .map(|d| d.parse_type(t))
            .find(|l| !matches!(l, L::Other { .. }))
            .unwrap_or_else(|| L::Other { native: t.raw.clone() })
    }

    pub fn render(t: &L) -> Rendered {
        let e = N.engine;
        match t {
            L::Bool => ex("SMALLINT").with(Info, TypeChanged, "Booleano como SMALLINT (0 o 1): no todos los motores tienen BOOLEAN."),
            L::Int { bytes, unsigned } => {
                integer(*bytes, *unsigned, &[(2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")], || wide_int(*bytes, *unsigned, "DECIMAL", 38, e), e)
            }
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 38, 38, e),
            L::Decimal { precision: None, .. } => no_precision("DECIMAL(38, 10)"),
            L::Float { bytes: 4 } => ex("REAL"),
            L::Float { .. } => ex("DOUBLE PRECISION"),
            L::Money => money("DECIMAL(19, 4)"),
            L::Char { len, .. } if len.unwrap_or(1) <= 254 => ex(format!("CHAR({})", len.unwrap_or(1))),
            L::Char { len, unicode } => render(&L::Varchar { len: *len, unicode: *unicode }),
            L::Varchar { len: Some(n), .. } => sized("VARCHAR", *n, 8000, "CLOB", e),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("CLOB"),
            L::Binary { len } => sized("BINARY", len.unwrap_or(1), 8000, "BLOB", e),
            L::Varbinary { len: Some(n) } => sized("VARBINARY", *n, 8000, "BLOB", e),
            L::Varbinary { len: None } | L::Blob => ex("BLOB"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => {
                let r = match precision {
                    Some(p) if *p > 0 => ex(format!("TIME({})", (*p).min(9))),
                    _ => ex("TIME"),
                };
                tz_loss(r.with_loss(precision_loss(*precision, 9)), *tz, e)
            }
            L::Timestamp { precision, tz } => {
                let r = match precision {
                    Some(p) => ex(format!("TIMESTAMP({})", (*p).min(9))),
                    None => ex("TIMESTAMP"),
                };
                tz_loss(r.with_loss(precision_loss(*precision, 9)), *tz, e)
            }
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        standard_default(d, ty, "CURRENT_TIMESTAMP", None, true)
    }

    pub fn caps() -> Caps {
        Caps {
            // ON DELETE CASCADE / SET NULL is what Oracle, SQL Server, Db2,
            // MySQL and PostgreSQL all take; ON UPDATE isn't in Oracle.
            on_delete: &["CASCADE", "SET NULL"],
            on_update: &[],
            // The generic flavor writes no identity and no COMMENT ON.
            auto_increment: false,
            max_identifier: 63,
            case: IdentCase::Preserve,
            ..super::caps(true, &[], &[])
        }
    }

    pub fn finalize(t: &mut TableSchema, report: &mut Report) {
        report.push(
            Warning,
            TypeApproximated,
            &t.name,
            None,
            "El destino es ODBC genérico: los tipos son SQL estándar (SMALLINT para booleanos, CLOB y BLOB para textos y binarios largos, \
             TIMESTAMP para fecha y hora) y hay que revisarlos contra el motor real. En SQL Server y Sybase TIMESTAMP es una versión de fila \
             y CLOB no existe.",
        );
    }
}

// ---------------------------------------------------------------------------
// NetSuite SuiteAnalytics Connect: read-only, Oracle-backed.

mod netsuite {
    use super::*;

    pub fn parse(t: &TypeSpec) -> L {
        generic::parse(t)
    }

    pub fn render(t: &L) -> Rendered {
        generic::render(t)
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        generic::default(d, ty)
    }

    pub fn caps() -> Caps {
        Caps { foreign_keys: false, indexes: false, ..generic::caps() }
    }

    pub fn finalize(t: &mut TableSchema, report: &mut Report) {
        report.push(
            Dropped,
            OptionDropped,
            &t.name,
            None,
            "SuiteAnalytics Connect de NetSuite es de solo lectura: la tabla no se puede crear ahí. Sirve como origen, no como destino.",
        );
    }
}

// ---------------------------------------------------------------------------
// Ingres / Actian X / Vector.

mod ingres {
    use super::*;

    const E: &str = "Ingres";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "LONG NVARCHAR", smallint: "SMALLINT", rowversion: "BYTE(8)" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let n = t.name.as_str();
        match n {
            "boolean" => L::Bool,
            "integer1" | "tinyint" | "i1" => L::int(1),
            "integer2" | "smallint" | "i2" => L::int(2),
            "integer" | "integer4" | "int" | "int4" | "i4" => L::int(4),
            "bigint" | "integer8" | "int8" | "i8" => L::int(8),
            // Ingres DECIMAL defaults to (5, 0).
            "decimal" | "numeric" | "dec" => L::Decimal { precision: p(0).or(Some(5)), scale: p(1).or(Some(0)) },
            "float4" | "real" | "f4" => L::Float { bytes: 4 },
            // FLOAT(n): n significant bits, up to 23 is a float4.
            "float" => L::Float { bytes: if p(0).is_some_and(|b| b <= 23) { 4 } else { 8 } },
            "float8" | "double precision" | "double" | "f8" => L::Float { bytes: 8 },
            "money" => L::Money,
            "char" | "character" | "c" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "nchar" | "national character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" | "char varying" | "text" | "vchar" => match p(0) {
                Some(len) => L::Varchar { len: Some(len), unicode: false },
                None => L::Text { unicode: false },
            },
            "nvarchar" | "national character varying" | "nchar varying" => match p(0) {
                Some(len) => L::Varchar { len: Some(len), unicode: true },
                None => L::Text { unicode: true },
            },
            "long varchar" | "clob" | "char large object" | "character large object" => L::Text { unicode: false },
            "long nvarchar" | "nclob" | "nchar large object" | "national character large object" => L::Text { unicode: true },
            "byte" | "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbyte" | "byte varying" | "varbinary" | "binary varying" => match p(0) {
                Some(len) => L::Varbinary { len: Some(len) },
                None => L::Blob,
            },
            "long byte" | "long varbyte" | "blob" | "binary large object" => L::Blob,
            "ansidate" | "date" => L::Date,
            // The old INGRESDATE holds a date and a time to the second.
            "ingresdate" => L::Timestamp { precision: Some(0), tz: false },
            // TIME defaults to 0 fractional digits, TIMESTAMP to 6.
            "time" => L::Time { precision: Some(p(0).unwrap_or(0) as u8), tz: t.with_tz },
            "timestamp" => L::Timestamp { precision: Some(p(0).unwrap_or(6) as u8), tz: t.with_tz },
            _ if n.starts_with("interval") => L::Interval,
            "uuid" => L::Uuid,
            "ipv4" | "ipv6" => L::Inet,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BOOLEAN"),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(1, "TINYINT"), (2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")],
                || wide_int(*bytes, *unsigned, "DECIMAL", 39, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 39, 39, E),
            L::Decimal { precision: None, .. } => no_precision("DECIMAL(39, 15)"),
            L::Float { bytes: 4 } => ex("FLOAT4"),
            L::Float { .. } => ex("FLOAT8"),
            // Ingres MONEY keeps only 2 decimals.
            L::Money => money("DECIMAL(19, 4)"),
            L::Char { len, unicode: true } => sized("NCHAR", len.unwrap_or(1), 16_000, "LONG NVARCHAR", E),
            L::Char { len, unicode: false } => sized("CHAR", len.unwrap_or(1), 32_000, "LONG VARCHAR", E),
            L::Varchar { len: Some(n), unicode: true } => sized("NVARCHAR", *n, 16_000, "LONG NVARCHAR", E),
            L::Varchar { len: Some(n), unicode: false } => sized("VARCHAR", *n, 32_000, "LONG VARCHAR", E),
            L::Varchar { len: None, unicode } | L::Text { unicode } => ex(if *unicode { "LONG NVARCHAR" } else { "LONG VARCHAR" }),
            L::Binary { len } => sized("BYTE", len.unwrap_or(1), 32_000, "LONG BYTE", E),
            L::Varbinary { len: Some(n) } => sized("VARBYTE", *n, 32_000, "LONG BYTE", E),
            L::Varbinary { len: None } | L::Blob => ex("LONG BYTE"),
            // DATE may be an alias of INGRESDATE (date_alias): ANSIDATE is unambiguous.
            L::Date => ex("ANSIDATE"),
            L::Time { precision, tz } => with_prec("TIME", *precision, 9, if *tz { " WITH TIME ZONE" } else { "" }),
            L::Timestamp { precision, tz } => with_prec("TIMESTAMP", *precision, 9, if *tz { " WITH TIME ZONE" } else { "" }),
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::CurrentTimestamp => if with_tz(ty) { "CURRENT_TIMESTAMP" } else { "LOCAL_TIMESTAMP" }.into(),
            DefaultValue::CurrentTime => if with_tz(ty) { "CURRENT_TIME" } else { "LOCAL_TIME" }.into(),
            DefaultValue::CurrentDate => "CURRENT_DATE".into(),
            other => standard_default(other, ty, "CURRENT_TIMESTAMP", None, false)?,
        })
    }

    pub fn caps() -> Caps {
        const ACTIONS: &[&str] = &["CASCADE", "SET NULL", "RESTRICT", "NO ACTION"];
        Caps { comments: true, max_identifier: 256, case: IdentCase::Lower, ..super::caps(true, ACTIONS, ACTIONS) }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// Actian Zen (Pervasive PSQL).

mod zen {
    use super::*;

    const E: &str = "Actian Zen";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "NLONGVARCHAR", smallint: "SMALLINT", rowversion: "BINARY(8)" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let int = |bytes, unsigned| L::Int { bytes, unsigned };
        match t.name.as_str() {
            "bit" | "boolean" | "logical" => L::Bool,
            "tinyint" => int(1, false),
            "utinyint" => int(1, true),
            "smallint" | "smallidentity" => int(2, false),
            "usmallint" => int(2, true),
            // A bare `IDENTITY` is a modifier to the parser: the name comes out empty.
            "integer" | "int" | "identity" => int(4, false),
            "" if t.has("identity") => int(4, false),
            "uinteger" | "uint" => int(4, true),
            "bigint" | "bigidentity" => int(8, false),
            "ubigint" => int(8, true),
            "autoinc" => int(if p(0) == Some(2) { 2 } else { 4 }, false),
            "decimal" | "dec" | "numeric" | "numericsa" | "numericsts" => L::Decimal { precision: p(0), scale: p(1).or(Some(0)) },
            // MONEY is DECIMAL(19, 2); CURRENCY an 8-byte integer of 1/10 000.
            "money" => L::Decimal { precision: Some(19), scale: Some(2) },
            "currency" => L::Money,
            "real" | "bfloat4" => L::Float { bytes: 4 },
            "double" | "double precision" | "bfloat8" => L::Float { bytes: 8 },
            "float" => L::Float { bytes: if p(0).is_some_and(|b| b <= 24) { 4 } else { 8 } },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "nchar" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" => match p(0) {
                Some(n) => L::Varchar { len: Some(n), unicode: false },
                None => L::Text { unicode: false },
            },
            "nvarchar" => match p(0) {
                Some(n) => L::Varchar { len: Some(n), unicode: true },
                None => L::Text { unicode: true },
            },
            "longvarchar" | "long varchar" | "clob" | "text" => L::Text { unicode: false },
            "nlongvarchar" | "nclob" => L::Text { unicode: true },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" => L::Varbinary { len: p(0) },
            "longvarbinary" | "long varbinary" | "blob" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: Some(0), tz: false },
            // TIMESTAMP keeps 7 fractional digits (septaseconds); DATETIME 3.
            "timestamp" | "timestamp2" | "autotimestamp" => L::Timestamp { precision: Some(7), tz: false },
            "datetime" => L::Timestamp { precision: Some(3), tz: false },
            "uniqueidentifier" => L::Uuid,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BIT"),
            // Zen has unsigned integers of every size.
            L::Int { bytes, unsigned: true } => match bytes {
                1 => ex("UTINYINT"),
                2 => ex("USMALLINT"),
                3 | 4 => ex("UINTEGER"),
                8 => ex("UBIGINT"),
                _ => wide_int(*bytes, true, "DECIMAL", 64, E),
            },
            L::Int { bytes, .. } => integer(
                *bytes,
                false,
                &[(1, "TINYINT"), (2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")],
                || wide_int(*bytes, false, "DECIMAL", 64, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 64, 64, E),
            L::Decimal { precision: None, .. } => no_precision("DECIMAL(64, 20)"),
            L::Float { bytes: 4 } => ex("REAL"),
            L::Float { .. } => ex("DOUBLE"),
            // CURRENCY: 4 decimals, the range of SQL Server's money.
            L::Money => ex("CURRENCY"),
            L::Char { len, unicode: true } => sized("NCHAR", len.unwrap_or(1), 4000, "NLONGVARCHAR", E),
            L::Char { len, unicode: false } => sized("CHAR", len.unwrap_or(1), 8000, "LONGVARCHAR", E),
            L::Varchar { len: Some(n), unicode: true } => sized("NVARCHAR", *n, 4000, "NLONGVARCHAR", E),
            L::Varchar { len: Some(n), unicode: false } => sized("VARCHAR", *n, 8000, "LONGVARCHAR", E),
            L::Varchar { len: None, unicode } | L::Text { unicode } => ex(if *unicode { "NLONGVARCHAR" } else { "LONGVARCHAR" }),
            L::Binary { len } => sized("BINARY", len.unwrap_or(1), 8000, "LONGVARBINARY", E),
            L::Varbinary { len: Some(_) } => {
                ex("LONGVARBINARY").with(Info, TypeChanged, "Zen no tiene binarios de largo variable con límite: se usa LONGVARBINARY.")
            }
            L::Varbinary { len: None } | L::Blob => ex("LONGVARBINARY"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => tz_loss(fixed_frac("TIME", *precision, 0), *tz, E),
            // DATETIME keeps milliseconds, TIMESTAMP septaseconds.
            L::Timestamp { precision: Some(p), tz } if *p <= 3 => tz_loss(ex("DATETIME"), *tz, E),
            L::Timestamp { precision, tz } => tz_loss(fixed_frac("TIMESTAMP", *precision, 7), *tz, E),
            L::Uuid => ex("UNIQUEIDENTIFIER"),
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::CurrentTimestamp => "NOW()".into(),
            DefaultValue::CurrentDate => "CURDATE()".into(),
            DefaultValue::CurrentTime => "CURTIME()".into(),
            DefaultValue::NewUuid => "NEWID()".into(),
            other => standard_default(other, ty, "NOW()", None, true)?,
        })
    }

    pub fn caps() -> Caps {
        Caps { case: IdentCase::Preserve, ..super::caps(true, &["CASCADE", "RESTRICT"], &["RESTRICT"]) }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// Altibase: Oracle-like, DATE with microseconds, no BOOLEAN or TIME.

mod altibase {
    use super::*;

    const E: &str = "Altibase";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "CLOB", smallint: "SMALLINT", rowversion: "BYTE(8)" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "smallint" => L::int(2),
            "integer" | "int" => L::int(4),
            "bigint" => L::int(8),
            "numeric" | "decimal" | "number" | "dec" => match p(0) {
                Some(pr) => L::Decimal { precision: Some(pr), scale: p(1).or(Some(0)) },
                // NUMBER without precision is a 38-digit decimal float.
                None => L::Decimal { precision: None, scale: None },
            },
            // FLOAT(p) is a decimal float too.
            "float" => L::Decimal { precision: None, scale: None },
            "real" => L::Float { bytes: 4 },
            "double" | "double precision" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: len0(t).or(Some(1)), unicode: false },
            "varchar" | "varchar2" | "character varying" => match len0(t) {
                Some(n) => L::Varchar { len: Some(n), unicode: false },
                None => L::Text { unicode: false },
            },
            "nchar" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "nvarchar" | "nvarchar2" => match p(0) {
                Some(n) => L::Varchar { len: Some(n), unicode: true },
                None => L::Text { unicode: true },
            },
            "clob" => L::Text { unicode: false },
            "blob" => L::Blob,
            "byte" => L::Binary { len: p(0).or(Some(1)) },
            "varbyte" => L::Varbinary { len: p(0) },
            "bit" => L::Bit { len: p(0).or(Some(1)) },
            "varbit" => L::Bit { len: p(0) },
            // DATE holds a date and a time to the microsecond.
            "date" | "timestamp" => L::Timestamp { precision: Some(6), tz: false },
            "geometry" => L::Geometry { kind: None, srid: None, geography: false },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("SMALLINT").with(Info, TypeChanged, "Altibase no tiene booleanos: SMALLINT con 0 o 1."),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")],
                || wide_int(*bytes, *unsigned, "NUMERIC", 38, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("NUMERIC", *p, *scale, 38, 38, E),
            L::Decimal { precision: None, .. } => ex("NUMBER"),
            L::Float { bytes: 4 } => ex("REAL"),
            L::Float { .. } => ex("DOUBLE"),
            L::Money => money("NUMERIC(19, 4)"),
            L::Char { len, unicode: true } => sized("NCHAR", len.unwrap_or(1), 16_000, "CLOB", E),
            L::Char { len, unicode: false } => sized("CHAR", len.unwrap_or(1), 32_000, "CLOB", E),
            L::Varchar { len: Some(n), unicode: true } => sized("NVARCHAR", *n, 16_000, "CLOB", E),
            L::Varchar { len: Some(n), unicode: false } => sized("VARCHAR", *n, 32_000, "CLOB", E),
            L::Varchar { len: None, unicode } | L::Text { unicode } => {
                let r = ex("CLOB");
                if *unicode {
                    r.with(Info, TypeChanged, "Altibase no tiene NCLOB: el CLOB usa el juego de caracteres de la base.")
                } else {
                    r
                }
            }
            L::Binary { len } => sized("BYTE", len.unwrap_or(1), 32_000, "BLOB", E),
            L::Varbinary { len: Some(n) } => sized("VARBYTE", *n, 32_000, "BLOB", E),
            L::Varbinary { len: None } | L::Blob => ex("BLOB"),
            L::Bit { len: Some(n) } if *n <= 64_000 => ex(format!("BIT({n})")),
            L::Date => ex("DATE").with(Info, TypeChanged, "El DATE de Altibase también guarda la hora (queda en 00:00:00)."),
            L::Time { tz, .. } => tz_loss(
                ex("DATE").with(Warning, TypeApproximated, "Altibase no tiene tipo hora: se guarda como DATE, con una fecha."),
                *tz,
                E,
            ),
            L::Timestamp { precision, tz } => tz_loss(ex("DATE").with_loss(precision_loss(*precision, 6)), *tz, E),
            L::Geometry { .. } => ex("GEOMETRY"),
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp | DefaultValue::CurrentTime => Some("SYSDATE".into()),
            DefaultValue::CurrentDate => Some("TRUNC(SYSDATE)".into()),
            // As in Oracle, '' is NULL.
            DefaultValue::Text(s) if s.is_empty() => Some("NULL".into()),
            other => standard_default(other, ty, "SYSDATE", None, true),
        }
    }

    pub fn caps() -> Caps {
        Caps { auto_increment: false, comments: true, ..super::caps(true, &["CASCADE", "SET NULL", "NO ACTION"], &[]) }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// CUBRID.

mod cubrid {
    use super::*;

    const E: &str = "CUBRID";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "STRING", smallint: "SMALLINT", rowversion: "BIT(64)" };
    const MAX_VARCHAR: u32 = 1_073_741_823;
    const MAX_BITS: u32 = 1_073_741_823;

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "short" | "smallint" => L::int(2),
            "int" | "integer" => L::int(4),
            "bigint" => L::int(8),
            "numeric" | "decimal" | "dec" => L::Decimal { precision: p(0).or(Some(15)), scale: p(1).or(Some(0)) },
            "float" | "real" => L::Float { bytes: if p(0).is_some_and(|b| b > 7) { 8 } else { 4 } },
            "double" | "double precision" => L::Float { bytes: 8 },
            // MONETARY is a double with a currency symbol.
            "monetary" => L::Money,
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "nchar" | "national character" | "national char" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "char varying" | "character varying" | "nchar varying" | "national character varying" => match p(0) {
                Some(n) if n < MAX_VARCHAR => L::Varchar { len: Some(n), unicode: true },
                _ => L::Text { unicode: true },
            },
            "string" | "clob" => L::Text { unicode: true },
            // BIT and BIT VARYING hold binary data, sized in bits.
            "bit" => match p(0).unwrap_or(1) {
                n if n > 1 && n % 8 == 0 => L::Binary { len: Some(n / 8) },
                n => L::Bit { len: Some(n) },
            },
            "bit varying" => match p(0) {
                Some(n) if n % 8 == 0 && n < MAX_BITS => L::Varbinary { len: Some(n / 8) },
                Some(n) if n < MAX_BITS => L::Bit { len: Some(n) },
                _ => L::Bit { len: None },
            },
            "blob" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: Some(0), tz: false },
            // TIMESTAMP is a UNIX time shown in the session's zone (1970–2038).
            "timestamp" | "timestampltz" | "timestamptz" => L::Timestamp { precision: Some(0), tz: true },
            "datetime" => L::Timestamp { precision: Some(3), tz: false },
            "datetimeltz" | "datetimetz" => L::Timestamp { precision: Some(3), tz: true },
            "enum" => L::Enum { values: t.args.clone() },
            "json" => L::Json { binary: true },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("SMALLINT").with(Info, TypeChanged, "CUBRID no tiene booleanos: SMALLINT con 0 o 1."),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")],
                || wide_int(*bytes, *unsigned, "NUMERIC", 38, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("NUMERIC", *p, *scale, 38, 38, E),
            L::Decimal { precision: None, .. } => no_precision("NUMERIC(38, 10)"),
            L::Float { bytes: 4 } => ex("FLOAT"),
            L::Float { .. } => ex("DOUBLE"),
            // MONETARY is a double: an exact numeric keeps the cents.
            L::Money => money("NUMERIC(19, 4)"),
            L::Char { len, .. } => sized("CHAR", len.unwrap_or(1), 268_435_455, "STRING", E),
            L::Varchar { len: Some(n), .. } if *n < MAX_VARCHAR => ex(format!("VARCHAR({n})")),
            L::Varchar { .. } | L::Text { .. } => ex("STRING"),
            L::Binary { len } => bits("BIT", len.unwrap_or(1)),
            L::Varbinary { len: Some(n) } => bits("BIT VARYING", *n),
            L::Varbinary { len: None } | L::Blob => ex("BLOB"),
            L::Bit { len: Some(n) } if *n < MAX_BITS => ex(format!("BIT({n})")),
            L::Bit { .. } => ex("BIT VARYING"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => tz_loss(fixed_frac("TIME", *precision, 0), *tz, E),
            L::Timestamp { precision, tz: false } => fixed_frac("DATETIME", *precision, 3),
            L::Timestamp { precision, tz: true } => fixed_frac("DATETIMETZ", *precision, 3),
            L::Json { .. } => ex("JSON"),
            L::Enum { values } => ex(format!("ENUM({})", values.iter().map(|v| quote(v)).collect::<Vec<_>>().join(", "))),
            other => shared(other, &N),
        }
    }

    /// Bytes as a bit string (CUBRID's binary type).
    fn bits(name: &str, bytes: u32) -> Rendered {
        match bytes.checked_mul(8).filter(|b| *b < MAX_BITS) {
            Some(b) => ex(format!("{name}({b})")).with(Info, TypeChanged, format!("Binario como {name}({b}): CUBRID mide los binarios en bits.")),
            None => ex("BLOB"),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::CurrentTimestamp => "CURRENT_DATETIME".into(),
            DefaultValue::CurrentDate => "CURRENT_DATE".into(),
            DefaultValue::CurrentTime => "CURRENT_TIME".into(),
            other => standard_default(other, ty, "CURRENT_DATETIME", None, true)?,
        })
    }

    pub fn caps() -> Caps {
        Caps {
            comments: true,
            max_identifier: 222,
            case: IdentCase::Lower,
            ..super::caps(true, &["CASCADE", "RESTRICT", "NO ACTION", "SET NULL"], &["RESTRICT", "NO ACTION", "SET NULL"])
        }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// Dameng (DM8): Oracle-compatible, with its own integer, bit and time types.

mod dameng {
    use super::*;

    const E: &str = "Dameng";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "TEXT", smallint: "SMALLINT", rowversion: "BINARY(8)" };
    /// Longest in-row CHAR / VARCHAR / BINARY on the default 8 KB page, in bytes.
    const MAX_INROW: u32 = 8188;

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let n = t.name.as_str();
        match n {
            "bit" | "boolean" | "bool" => L::Bool,
            "tinyint" | "byte" => L::int(1),
            "smallint" => L::int(2),
            "int" | "integer" | "pls_integer" => L::int(4),
            "bigint" => L::int(8),
            "number" | "numeric" | "decimal" | "dec" => match p(0) {
                Some(pr) => L::Decimal { precision: Some(pr), scale: p(1).or(Some(0)) },
                None => L::Decimal { precision: None, scale: None },
            },
            "real" => L::Float { bytes: 4 },
            "float" | "double" | "double precision" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: len0(t).or(Some(1)), unicode: false },
            "varchar" | "varchar2" | "character varying" => match len0(t) {
                Some(l) => L::Varchar { len: Some(l), unicode: false },
                None => L::Text { unicode: false },
            },
            "text" | "longvarchar" | "clob" | "long" => L::Text { unicode: false },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" | "raw" => L::Varbinary { len: p(0) },
            "blob" | "image" | "longvarbinary" | "long raw" => L::Blob,
            // Unlike Oracle, DATE is only a date.
            "date" => L::Date,
            "time" => L::Time { precision: Some(p(0).unwrap_or(0) as u8), tz: t.with_tz },
            "timestamp" | "datetime" => L::Timestamp { precision: Some(p(0).unwrap_or(6) as u8), tz: t.with_tz },
            _ if n.starts_with("interval") => L::Interval,
            "json" => L::Json { binary: false },
            _ => oracle::Oracle.parse_type(t),
        }
    }

    pub fn render(t: &L) -> Rendered {
        let tzs = |tz: bool| if tz { " WITH TIME ZONE" } else { "" };
        match t {
            L::Bool => ex("BIT"),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(1, "TINYINT"), (2, "SMALLINT"), (4, "INT"), (8, "BIGINT")],
                || wide_int(*bytes, *unsigned, "DECIMAL", 38, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 38, 38, E),
            L::Decimal { precision: None, .. } => ex("NUMBER"),
            L::Float { bytes: 4 } => ex("REAL"),
            L::Float { .. } => ex("DOUBLE"),
            L::Money => money("DECIMAL(19, 4)"),
            L::Char { len, .. } => sized("CHAR", len.unwrap_or(1), MAX_INROW, "TEXT", E),
            L::Varchar { len: Some(n), .. } => sized("VARCHAR", *n, MAX_INROW, "TEXT", E),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("TEXT"),
            L::Binary { len } => sized("BINARY", len.unwrap_or(1), MAX_INROW, "BLOB", E),
            L::Varbinary { len: Some(n) } => sized("VARBINARY", *n, MAX_INROW, "BLOB", E),
            L::Varbinary { len: None } | L::Blob => ex("BLOB"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => with_prec("TIME", *precision, 6, tzs(*tz)),
            L::Timestamp { precision, tz } => with_prec("TIMESTAMP", *precision, 6, tzs(*tz)),
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::CurrentTimestamp => if with_tz(ty) { "CURRENT_TIMESTAMP" } else { "SYSDATE" }.into(),
            DefaultValue::CurrentDate => "CURDATE()".into(),
            DefaultValue::CurrentTime => "CURTIME()".into(),
            other => standard_default(other, ty, "SYSDATE", None, true)?,
        })
    }

    pub fn caps() -> Caps {
        const ACTIONS: &[&str] = &["CASCADE", "SET NULL", "SET DEFAULT", "NO ACTION"];
        Caps { comments: true, ..super::caps(true, ACTIONS, ACTIONS) }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// HeavyDB (OmniSci / MapD): GPU analytics, no keys or indexes.

mod heavydb {
    use super::*;

    const E: &str = "HeavyDB";
    const N: Names = Names { engine: E, varchar: "TEXT", fixed: "TEXT", text: "TEXT", smallint: "SMALLINT", rowversion: "BIGINT" };
    /// Longest TEXT value, in bytes.
    const MAX_TEXT: u32 = 32_767;

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let n = t.name.as_str();
        match n {
            "boolean" | "bool" => L::Bool,
            "tinyint" => L::int(1),
            "smallint" => L::int(2),
            "integer" | "int" => L::int(4),
            "bigint" => L::int(8),
            "decimal" | "numeric" => L::Decimal { precision: p(0), scale: p(1).or(Some(0)) },
            "float" | "real" => L::Float { bytes: 4 },
            "double" | "double precision" => L::Float { bytes: 8 },
            // `TEXT ENCODING DICT(32)`: the argument is the dictionary width.
            _ if n == "text" || n.starts_with("text encoding") => L::Text { unicode: true },
            "varchar" | "char" | "string" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            _ if n == "date" || n.starts_with("date encoding") => L::Date,
            _ if n == "time" || n.starts_with("time encoding") => L::Time { precision: Some(0), tz: false },
            "timestamp" => L::Timestamp { precision: Some(p(0).unwrap_or(0) as u8), tz: false },
            _ if n.starts_with("timestamp encoding") => L::Timestamp { precision: Some(0), tz: false },
            "point" | "linestring" | "polygon" | "multipolygon" | "multilinestring" | "multipoint" => {
                L::Geometry { kind: Some(n.to_string()), srid: None, geography: false }
            }
            "geometry" | "geography" => L::Geometry {
                kind: t.args.first().map(|k| k.to_ascii_lowercase()),
                srid: p(1),
                geography: n == "geography",
            },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BOOLEAN"),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(1, "TINYINT"), (2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")],
                || wide_int(*bytes, *unsigned, "DECIMAL", 18, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 18, 18, E),
            L::Decimal { precision: None, .. } => ex("DOUBLE").with(Loss, PrecisionLoss, "El origen no fija la precisión y HeavyDB admite hasta 18 dígitos: se usa DOUBLE, que puede redondear."),
            L::Float { bytes: 4 } => ex("FLOAT"),
            L::Float { .. } => ex("DOUBLE"),
            L::Money => money("DECIMAL(18, 4)"),
            L::Char { len, .. } | L::Varchar { len, .. } => match len {
                Some(n) if *n > MAX_TEXT => ex("TEXT").with(Loss, LengthLoss, format!("HeavyDB guarda textos de hasta {MAX_TEXT} bytes; el origen admite {n}.")),
                _ => ex("TEXT"),
            },
            L::Text { .. } => ex("TEXT").with(Loss, LengthLoss, format!("HeavyDB guarda textos de hasta {MAX_TEXT} bytes.")),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => ex("TEXT ENCODING NONE")
                .with(Warning, TypeApproximated, "HeavyDB no tiene binarios: hay que guardarlos como texto (hexadecimal o base64)."),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => tz_loss(fixed_frac("TIME", *precision, 0), *tz, E),
            // TIMESTAMP(0 | 3 | 6 | 9).
            L::Timestamp { precision, tz } => {
                let want = precision.unwrap_or(6);
                let digits = [0u8, 3, 6, 9].into_iter().find(|d| *d >= want).unwrap_or(9);
                tz_loss(ex(format!("TIMESTAMP({digits})")).with_loss(precision_loss(*precision, 9)), *tz, E)
            }
            L::Uuid => ex("TEXT").with(Info, TypeChanged, "UUID como texto."),
            L::Enum { values } => ex("TEXT").with(Info, TypeApproximated, format!("Enumerado como TEXT (codificado con diccionario). Valores: {}.", values.join(", "))),
            L::Array { of } if !matches!(**of, L::Array { .. } | L::Map { .. } | L::Geometry { .. } | L::Binary { .. } | L::Varbinary { .. } | L::Blob) => {
                let inner = render(of);
                Rendered { native: format!("{}[]", inner.native), notes: inner.notes }
            }
            L::Geometry { kind: Some(k), srid, .. } if matches!(k.as_str(), "point" | "linestring" | "polygon" | "multipolygon" | "multilinestring" | "multipoint") => {
                match srid {
                    Some(s) => ex(format!("GEOMETRY({}, {s})", k.to_ascii_uppercase())),
                    None => ex(k.to_ascii_uppercase()),
                }
            }
            other => shared(other, &N),
        }
    }

    /// Only literal defaults.
    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp | DefaultValue::CurrentDate | DefaultValue::CurrentTime | DefaultValue::NewUuid => None,
            other => standard_default(other, ty, "", None, false),
        }
    }

    pub fn caps() -> Caps {
        Caps {
            indexes: false,
            auto_increment: false,
            case: IdentCase::Preserve,
            ..super::caps(false, &[], &[])
        }
    }

    pub fn finalize(t: &mut TableSchema, report: &mut Report) {
        drop_primary_key(t, report, E, None);
    }
}

// ---------------------------------------------------------------------------
// InterSystems IRIS and Caché.

mod iris {
    use super::*;

    const E: &str = "InterSystems IRIS";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "LONGVARCHAR", smallint: "SMALLINT", rowversion: "ROWVERSION" };
    /// %String MAXLEN with long strings on.
    const MAX_STRING: u32 = 3_641_144;
    /// %Numeric keeps 18 significant digits safely (19 in part of the range).
    const MAX_NUMERIC: u32 = 18;

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "bit" | "boolean" => L::Bool,
            "tinyint" => L::int(1),
            "smallint" => L::int(2),
            "integer" | "int" => L::int(4),
            "bigint" | "serial" | "identity" => L::int(8),
            // IRIS's DDL writes identity columns as a bare `IDENTITY` (a BIGINT).
            "" if t.has("identity") => L::int(8),
            "numeric" | "decimal" | "number" | "dec" => L::Decimal { precision: p(0), scale: p(1).or(Some(0)) },
            "money" | "smallmoney" => L::Money,
            // REAL and FLOAT are %Double.
            "double" | "double precision" | "float" | "real" => L::Float { bytes: 8 },
            "char" | "character" | "nchar" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" | "nvarchar" | "national varchar" | "varchar2" | "nvarchar2" | "sysname" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            "longvarchar" | "long varchar" | "text" | "ntext" | "clob" | "long" => L::Text { unicode: true },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" | "binary varying" | "raw" => match p(0) {
                Some(l) => L::Varbinary { len: Some(l) },
                None => L::Blob,
            },
            "longvarbinary" | "long varbinary" | "blob" | "image" | "long binary" | "long raw" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: Some(p(0).unwrap_or(0) as u8), tz: false },
            "timestamp" | "datetime" | "datetime2" | "smalldatetime" => L::Timestamp { precision: Some(p(0).unwrap_or(9) as u8), tz: false },
            "posixtime" => L::Timestamp { precision: Some(6), tz: false },
            "uniqueidentifier" | "guid" => L::Uuid,
            "rowversion" => L::RowVersion,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BIT"),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(1, "TINYINT"), (2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")],
                || wide_int(*bytes, *unsigned, "NUMERIC", MAX_NUMERIC, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("NUMERIC", *p, *scale, MAX_NUMERIC, MAX_NUMERIC, E),
            L::Decimal { precision: None, .. } => no_precision("NUMERIC(18, 6)"),
            L::Float { .. } => ex("DOUBLE"),
            // %Currency: 4 decimals, SQL Server money's range.
            L::Money => ex("MONEY"),
            L::Char { len, .. } => sized("CHAR", len.unwrap_or(1), MAX_STRING, "LONGVARCHAR", E),
            L::Varchar { len: Some(n), .. } => sized("VARCHAR", *n, MAX_STRING, "LONGVARCHAR", E),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("LONGVARCHAR"),
            L::Binary { len } => sized("BINARY", len.unwrap_or(1), MAX_STRING, "LONGVARBINARY", E),
            L::Varbinary { len: Some(n) } => sized("VARBINARY", *n, MAX_STRING, "LONGVARBINARY", E),
            L::Varbinary { len: None } | L::Blob => ex("LONGVARBINARY"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => {
                let r = match precision {
                    Some(p) if *p > 0 => ex(format!("TIME({})", (*p).min(9))).with_loss(precision_loss(*precision, 9)),
                    _ => ex("TIME"),
                };
                tz_loss(r, *tz, E)
            }
            // %TimeStamp keeps up to 9 fractional digits.
            L::Timestamp { precision, tz } => tz_loss(ex("TIMESTAMP").with_loss(precision_loss(*precision, 9)), *tz, E),
            L::Uuid => ex("UNIQUEIDENTIFIER"),
            L::RowVersion => ex("ROWVERSION"),
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        standard_default(d, ty, "CURRENT_TIMESTAMP", None, true)
    }

    pub fn caps() -> Caps {
        const ACTIONS: &[&str] = &["CASCADE", "SET NULL", "SET DEFAULT", "NO ACTION"];
        Caps { case: IdentCase::Preserve, ..super::caps(true, ACTIONS, ACTIONS) }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// Machbase: time-series, unsigned integers, no decimals, no keys.

mod machbase {
    use super::*;

    const E: &str = "Machbase";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "VARCHAR", text: "TEXT", smallint: "SHORT", rowversion: "LONG" };
    const MAX_VARCHAR: u32 = 32_767;

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let int = |bytes, unsigned| L::Int { bytes, unsigned };
        match t.name.as_str() {
            "short" | "smallint" => int(2, false),
            "ushort" => int(2, true),
            "integer" | "int" => int(4, false),
            "uinteger" => int(4, true),
            "long" | "bigint" => int(8, false),
            "ulong" => int(8, true),
            "float" => L::Float { bytes: 4 },
            "double" => L::Float { bytes: 8 },
            "varchar" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            "text" | "clob" => L::Text { unicode: true },
            "binary" | "blob" => L::Blob,
            // Nanoseconds.
            "datetime" => L::Timestamp { precision: Some(9), tz: false },
            "ipv4" | "ipv6" => L::Inet,
            "json" => L::Json { binary: false },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("SHORT").with(Info, TypeChanged, "Machbase no tiene booleanos: SHORT con 0 o 1."),
            L::Int { bytes, unsigned } => {
                let (s, u) = match bytes {
                    1 | 2 => ("SHORT", "USHORT"),
                    3 | 4 => ("INTEGER", "UINTEGER"),
                    8 => ("LONG", "ULONG"),
                    _ => {
                        return ex("DOUBLE").with(Loss, PrecisionLoss, "Machbase no tiene enteros de 16 bytes ni decimales: se usa DOUBLE, que redondea los valores grandes.")
                    }
                };
                ex(if *unsigned { u } else { s })
            }
            // A whole number of up to 18 digits fits a LONG.
            L::Decimal { precision: Some(p), scale: Some(0) | None } if *p <= 18 => ex("LONG").with(Info, TypeChanged, format!("Decimal sin decimales ({p} dígitos) como LONG.")),
            L::Decimal { .. } | L::Money => ex("DOUBLE").with(Loss, PrecisionLoss, "Machbase no tiene decimales exactos: se usa DOUBLE, que puede redondear."),
            L::Float { bytes: 4 } => ex("FLOAT"),
            L::Float { .. } => ex("DOUBLE"),
            L::Char { len, .. } => sized("VARCHAR", len.unwrap_or(1), MAX_VARCHAR, "TEXT", E),
            L::Varchar { len: Some(n), .. } => sized("VARCHAR", *n, MAX_VARCHAR, "TEXT", E),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("TEXT"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => ex("BINARY"),
            L::Date => ex("DATETIME").with(Info, TypeChanged, "Machbase no tiene tipo fecha: DATETIME a las 00:00."),
            L::Time { tz, .. } => tz_loss(
                ex("DATETIME").with(Warning, TypeApproximated, "Machbase no tiene tipo hora: se guarda como DATETIME, con una fecha."),
                *tz,
                E,
            ),
            L::Timestamp { precision, tz } => tz_loss(fixed_frac("DATETIME", *precision, 9), *tz, E),
            L::Json { .. } => ex("JSON"),
            L::Array { .. } | L::Map { .. } => ex("JSON").with(Warning, TypeApproximated, "Machbase no tiene arreglos ni mapas: se guarda como JSON."),
            other => shared(other, &N),
        }
    }

    pub fn default(_: &DefaultValue, _: &L) -> Option<String> {
        None
    }

    pub fn caps() -> Caps {
        Caps { auto_increment: false, defaults: false, max_identifier: 40, ..super::caps(false, &[], &[]) }
    }

    pub fn finalize(t: &mut TableSchema, report: &mut Report) {
        drop_primary_key(t, report, E, None);
        no_unique_indexes(t, report, E);
    }
}

// ---------------------------------------------------------------------------
// Microsoft Access (ACE / Jet).

mod access {
    use super::*;

    const E: &str = "Access";
    const N: Names = Names { engine: E, varchar: "TEXT", fixed: "TEXT", text: "LONGTEXT", smallint: "SHORT", rowversion: "BINARY(8)" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "counter" | "autoincrement" | "integer" | "long" | "int" | "int4" | "integer4" => L::int(4),
            "smallint" | "short" | "integer2" => L::int(2),
            // BYTE is 0–255.
            "byte" | "tinyint" | "integer1" => L::Int { bytes: 1, unsigned: true },
            "bigint" => L::int(8),
            "real" | "single" | "float4" | "ieeesingle" => L::Float { bytes: 4 },
            "double" | "float" | "float8" | "ieeedouble" | "number" => L::Float { bytes: 8 },
            "currency" | "money" => L::Money,
            "decimal" | "numeric" => L::Decimal { precision: p(0).or(Some(18)), scale: p(1).or(Some(0)) },
            "bit" | "yesno" | "logical" | "logical1" | "boolean" => L::Bool,
            // DATETIME holds both; a date-only or time-only value is a DATETIME too.
            "datetime" | "date" | "time" | "timestamp" => L::Timestamp { precision: Some(0), tz: false },
            "char" | "character" => L::Char { len: p(0).or(Some(255)), unicode: true },
            "varchar" | "text" | "string" | "alphanumeric" | "nvarchar" | "nchar" | "character varying" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            "longchar" | "longtext" | "memo" | "note" | "ntext" | "hyperlink" => L::Text { unicode: true },
            "guid" | "uniqueidentifier" => L::Uuid,
            "longbinary" | "oleobject" | "general" | "image" => L::Blob,
            "binary" | "varbinary" => match p(0) {
                Some(l) => L::Varbinary { len: Some(l) },
                None => L::Blob,
            },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("YESNO"),
            L::Int { bytes: 1, unsigned: true } => ex("BYTE"),
            L::Int { bytes, unsigned } => integer(*bytes, *unsigned, &[(2, "SHORT"), (4, "LONG")], || wide_int(*bytes, *unsigned, "DECIMAL", 28, E), E),
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 28, 28, E),
            L::Decimal { precision: None, .. } => no_precision("DECIMAL(28, 10)"),
            L::Float { bytes: 4 } => ex("SINGLE"),
            L::Float { .. } => ex("DOUBLE"),
            // CURRENCY: 4 decimals, 15 integer digits.
            L::Money => ex("CURRENCY"),
            L::Char { len, .. } => sized("TEXT", len.unwrap_or(1), 255, "LONGTEXT", E),
            L::Varchar { len: Some(n), .. } => sized("TEXT", *n, 255, "LONGTEXT", E),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("LONGTEXT"),
            L::Binary { len } | L::Varbinary { len: len @ Some(_) } => sized("BINARY", len.unwrap_or(1), 255, "LONGBINARY", E),
            L::Varbinary { len: None } | L::Blob => ex("LONGBINARY"),
            L::Date => ex("DATETIME").with(Info, TypeChanged, "Access no tiene tipo fecha: DATETIME a las 00:00."),
            L::Time { tz, .. } => tz_loss(ex("DATETIME").with(Info, TypeChanged, "Access guarda la hora como DATETIME sin fecha."), *tz, E),
            L::Timestamp { precision, tz } => tz_loss(fixed_frac("DATETIME", *precision, 0), *tz, E),
            L::Uuid => ex("GUID"),
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp => Some("Now()".into()),
            DefaultValue::CurrentDate => Some("Date()".into()),
            DefaultValue::CurrentTime => Some("Time()".into()),
            other => standard_default(other, ty, "Now()", None, false),
        }
    }

    pub fn caps() -> Caps {
        const ACTIONS: &[&str] = &["CASCADE", "SET NULL"];
        Caps { max_identifier: 64, case: IdentCase::Preserve, ..super::caps(true, ACTIONS, ACTIONS) }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// dBase files (Microsoft dBase driver).

mod dbase {
    use super::*;

    const E: &str = "dBase";
    const N: Names = Names { engine: E, varchar: "CHAR", fixed: "CHAR", text: "MEMO", smallint: "NUMERIC(6, 0)", rowversion: "CHAR(16)" };
    /// Field names: 10 characters.
    const MAX_NAME: usize = 10;

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "char" | "character" | "c" | "varchar" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "numeric" | "number" | "n" | "decimal" => L::Decimal { precision: p(0), scale: p(1).or(Some(0)) },
            "float" | "f" | "double" => L::Float { bytes: 8 },
            "integer" | "long" | "i" => L::int(4),
            "logical" | "bit" | "l" | "boolean" => L::Bool,
            "date" | "d" => L::Date,
            "timestamp" | "datetime" | "t" => L::Timestamp { precision: Some(0), tz: false },
            "memo" | "m" | "longvarchar" => L::Text { unicode: false },
            "general" | "binary" | "ole" | "longvarbinary" | "g" => L::Blob,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        let unicode = |r: Rendered, u: bool| {
            if u {
                r.with(Loss, UnicodeLoss, "dBase guarda el texto en la página de códigos del archivo: los caracteres fuera de ella se pierden.")
            } else {
                r
            }
        };
        match t {
            L::Bool => ex("BIT"),
            // NUMERIC width counts the sign.
            L::Int { bytes, unsigned } => {
                let digits = match L::signed_bytes_for(*bytes, *unsigned) {
                    1 => 4,
                    2 => 6,
                    3 => 8,
                    4 => 11,
                    8 => 20,
                    _ => return ex("FLOAT").with(Loss, PrecisionLoss, "dBase no tiene enteros de 16 bytes: FLOAT redondea los valores grandes."),
                };
                ex(format!("NUMERIC({digits}, 0)"))
            }
            L::Decimal { precision: Some(p), scale } => decimal("NUMERIC", *p, *scale, 20, 18, E),
            L::Decimal { precision: None, .. } => ex("FLOAT").with(Loss, PrecisionLoss, "El origen no fija la precisión: FLOAT puede redondear."),
            L::Float { .. } => ex("FLOAT"),
            L::Money => money("NUMERIC(20, 4)"),
            L::Char { len, unicode: u } | L::Varchar { len: len @ Some(_), unicode: u } => unicode(sized("CHAR", len.unwrap_or(1), 254, "MEMO", E), *u),
            L::Varchar { len: None, unicode: u } | L::Text { unicode: u } => unicode(ex("MEMO"), *u),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => ex("MEMO").with(Loss, TypeApproximated, "dBase no tiene binarios: hay que guardarlos como texto (hexadecimal o base64)."),
            L::Date => ex("DATE"),
            L::Time { .. } => ex("CHAR(18)").with(Warning, TypeApproximated, "dBase no tiene tipo hora: queda como texto."),
            L::Timestamp { .. } => ex("CHAR(35)").with(Warning, TypeApproximated, "dBase no tiene fecha y hora: queda como texto ISO 8601."),
            L::Year => ex("NUMERIC(4, 0)"),
            other => shared(other, &N),
        }
    }

    pub fn default(_: &DefaultValue, _: &L) -> Option<String> {
        None
    }

    pub fn caps() -> Caps {
        Caps {
            auto_increment: false,
            defaults: false,
            nullability: false,
            max_identifier: MAX_NAME,
            ..super::caps(false, &[], &[])
        }
    }

    /// No primary keys: a unique index keeps the key's uniqueness.
    pub fn finalize(t: &mut TableSchema, report: &mut Report) {
        drop_primary_key(t, report, E, Some(MAX_NAME));
    }
}

// ---------------------------------------------------------------------------
// Mimer SQL.

mod mimer {
    use super::*;

    const E: &str = "Mimer SQL";
    const N: Names = Names { engine: E, varchar: "NVARCHAR", fixed: "CHAR", text: "NCLOB", smallint: "SMALLINT", rowversion: "BINARY(8)" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let n = t.name.as_str();
        match n {
            "boolean" => L::Bool,
            "smallint" => L::int(2),
            // INTEGER(p) is a whole number of p decimal digits.
            "integer" | "int" if p(0).is_some() => L::Decimal { precision: p(0), scale: Some(0) },
            "integer" | "int" => L::int(4),
            "bigint" => L::int(8),
            "decimal" | "numeric" | "dec" => L::Decimal { precision: p(0).or(Some(15)), scale: p(1).or(Some(0)) },
            "real" => L::Float { bytes: 4 },
            "double precision" | "double" | "float" => L::Float { bytes: 8 },
            // CHARACTER is ISO 8859-1; NCHAR is Unicode.
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "varchar" | "character varying" | "char varying" => L::Varchar { len: p(0), unicode: false },
            "nchar" | "national character" | "national char" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "nvarchar" | "national character varying" | "national char varying" | "nchar varying" => L::Varchar { len: p(0), unicode: true },
            "clob" | "character large object" | "char large object" => L::Text { unicode: false },
            "nclob" | "national character large object" | "nchar large object" => L::Text { unicode: true },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" | "binary varying" => L::Varbinary { len: p(0) },
            "blob" | "binary large object" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: Some(p(0).unwrap_or(0) as u8), tz: false },
            "timestamp" => L::Timestamp { precision: Some(p(0).unwrap_or(6) as u8), tz: false },
            _ if n.starts_with("interval") => L::Interval,
            "builtin.uuid" | "uuid" => L::Uuid,
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BOOLEAN"),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")],
                || wide_int(*bytes, *unsigned, "DECIMAL", 45, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 45, 45, E),
            L::Decimal { precision: None, .. } => no_precision("DECIMAL(45, 15)"),
            L::Float { bytes: 4 } => ex("REAL"),
            L::Float { .. } => ex("DOUBLE PRECISION"),
            L::Money => money("DECIMAL(19, 4)"),
            L::Char { len, unicode: true } => sized("NCHAR", len.unwrap_or(1), 5000, "NCLOB", E),
            L::Char { len, unicode: false } => sized("CHAR", len.unwrap_or(1), 15_000, "CLOB", E),
            L::Varchar { len: Some(n), unicode: true } => sized("NVARCHAR", *n, 5000, "NCLOB", E),
            L::Varchar { len: Some(n), unicode: false } => sized("VARCHAR", *n, 15_000, "CLOB", E),
            L::Varchar { len: None, unicode } | L::Text { unicode } => ex(if *unicode { "NCLOB" } else { "CLOB" }),
            L::Binary { len } => sized("BINARY", len.unwrap_or(1), 15_000, "BLOB", E),
            L::Varbinary { len: Some(n) } => sized("VARBINARY", *n, 15_000, "BLOB", E),
            L::Varbinary { len: None } | L::Blob => ex("BLOB"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => tz_loss(with_prec("TIME", *precision, 9, ""), *tz, E),
            L::Timestamp { precision, tz } => tz_loss(with_prec("TIMESTAMP", *precision, 9, ""), *tz, E),
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::CurrentTimestamp => "LOCALTIMESTAMP".into(),
            DefaultValue::CurrentTime => "LOCALTIME".into(),
            other => standard_default(other, ty, "LOCALTIMESTAMP", None, false)?,
        })
    }

    pub fn caps() -> Caps {
        Caps {
            auto_increment: false,
            comments: true,
            ..super::caps(true, &["CASCADE", "SET NULL", "SET DEFAULT", "NO ACTION"], &["NO ACTION"])
        }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// MonetDB.

mod monetdb {
    use super::*;

    const E: &str = "MonetDB";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "CLOB", smallint: "SMALLINT", rowversion: "BLOB" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let n = t.name.as_str();
        match n {
            "boolean" | "bool" => L::Bool,
            "tinyint" => L::int(1),
            "smallint" => L::int(2),
            "int" | "integer" | "mediumint" | "serial" => L::int(4),
            "bigint" | "bigserial" => L::int(8),
            "hugeint" => L::int(16),
            "decimal" | "numeric" | "dec" => L::Decimal { precision: p(0).or(Some(18)), scale: p(1).or(Some(3).filter(|_| p(0).is_none())).or(Some(0)) },
            "real" | "float4" => L::Float { bytes: 4 },
            "float" => L::Float { bytes: if p(0).is_some_and(|b| b <= 24) { 4 } else { 8 } },
            "double" | "double precision" | "float8" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            "clob" | "text" | "string" | "character large object" | "tinytext" | "mediumtext" | "longtext" | "url" => L::Text { unicode: true },
            "blob" | "binary large object" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: Some(p(0).unwrap_or(0) as u8), tz: t.with_tz },
            "timetz" => L::Time { precision: Some(p(0).unwrap_or(0) as u8), tz: true },
            "timestamp" => L::Timestamp { precision: Some(p(0).unwrap_or(6) as u8), tz: t.with_tz },
            "timestamptz" => L::Timestamp { precision: Some(p(0).unwrap_or(6) as u8), tz: true },
            "sec_interval" | "month_interval" | "day_interval" => L::Interval,
            _ if n.starts_with("interval") => L::Interval,
            "uuid" => L::Uuid,
            "json" => L::Json { binary: false },
            "inet" => L::Inet,
            "xml" => L::Xml,
            "geometry" | "geometrya" => L::Geometry { kind: t.args.first().map(|k| k.to_ascii_lowercase()), srid: p(1), geography: false },
            "point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon" | "geometrycollection" => {
                L::Geometry { kind: Some(n.to_string()), srid: None, geography: false }
            }
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        let tzs = |tz: bool| if tz { " WITH TIME ZONE" } else { "" };
        match t {
            L::Bool => ex("BOOLEAN"),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(1, "TINYINT"), (2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT"), (16, "HUGEINT")],
                || wide_int(*bytes, *unsigned, "DECIMAL", 38, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 38, 38, E),
            L::Decimal { precision: None, .. } => no_precision("DECIMAL(38, 10)"),
            L::Float { bytes: 4 } => ex("REAL"),
            L::Float { .. } => ex("DOUBLE"),
            L::Money => money("DECIMAL(19, 4)"),
            L::Char { len, .. } => ex(format!("CHAR({})", len.unwrap_or(1))),
            L::Varchar { len: Some(n), .. } => ex(format!("VARCHAR({n})")),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("CLOB"),
            L::Binary { len } | L::Varbinary { len } => {
                let r = ex("BLOB");
                if len.is_some() {
                    r.with(Info, TypeChanged, "BLOB no limita el largo.")
                } else {
                    r
                }
            }
            L::Blob => ex("BLOB"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => with_prec("TIME", *precision, 6, tzs(*tz)),
            L::Timestamp { precision, tz } => with_prec("TIMESTAMP", *precision, 6, tzs(*tz)),
            L::Uuid => ex("UUID"),
            L::Json { .. } => ex("JSON"),
            L::Array { .. } | L::Map { .. } => ex("JSON").with(Warning, TypeApproximated, "MonetDB no tiene arreglos ni mapas: se guarda como JSON."),
            L::Geometry { kind, srid, .. } => {
                let native = match (kind, srid) {
                    (Some(k), Some(s)) => format!("GEOMETRY({}, {s})", k.to_ascii_uppercase()),
                    (Some(k), None) => format!("GEOMETRY({})", k.to_ascii_uppercase()),
                    _ => "GEOMETRY".into(),
                };
                ex(native).with(Info, TypeChanged, "Requiere el módulo geom de MonetDB.")
            }
            L::Inet => ex("INET"),
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::CurrentTimestamp => if with_tz(ty) { "CURRENT_TIMESTAMP" } else { "LOCALTIMESTAMP" }.into(),
            DefaultValue::CurrentTime => if with_tz(ty) { "CURRENT_TIME" } else { "LOCALTIME" }.into(),
            other => standard_default(other, ty, "CURRENT_TIMESTAMP", None, false)?,
        })
    }

    pub fn caps() -> Caps {
        Caps { comments: true, max_identifier: 1024, case: IdentCase::Lower, ..super::caps(true, ALL_ACTIONS, ALL_ACTIONS) }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// NuoDB.

mod nuodb {
    use super::*;

    const E: &str = "NuoDB";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "STRING", smallint: "SMALLINT", rowversion: "BINARY(8)" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "boolean" => L::Bool,
            "smallint" => L::int(2),
            "integer" | "int" => L::int(4),
            "bigint" => L::int(8),
            "numeric" | "decimal" | "dec" | "number" => L::Decimal { precision: p(0), scale: p(0).map(|_| p(1).unwrap_or(0)) },
            "real" | "smallfloat" => L::Float { bytes: 4 },
            "double" | "double precision" | "float" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" | "char varying" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            "string" | "clob" | "character large object" | "text" => L::Text { unicode: true },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" | "binary varying" | "binarystring" => match p(0) {
                Some(l) => L::Varbinary { len: Some(l) },
                None => L::Blob,
            },
            "blob" | "binary large object" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: Some(p(0).unwrap_or(0) as u8), tz: false },
            "timestamp" => L::Timestamp { precision: Some(p(0).unwrap_or(6) as u8), tz: false },
            "enum" => L::Enum { values: t.args.clone() },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BOOLEAN"),
            // NUMBER is unbounded: wider integers fit exactly.
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")],
                || ex("NUMBER").with(Info, TypeChanged, format!("El {} queda como NUMBER.", L::Int { bytes: *bytes, unsigned: *unsigned }.describe())),
                E,
            ),
            L::Decimal { precision: Some(p), scale } if *p <= 38 => ex(format!("DECIMAL({p}, {})", scale.unwrap_or(0).min(*p))),
            L::Decimal { precision: Some(p), .. } => ex("NUMBER").with(Info, TypeChanged, format!("Decimal de {p} dígitos como NUMBER (sin límite de precisión).")),
            L::Decimal { precision: None, .. } => ex("NUMBER"),
            L::Float { .. } => ex("DOUBLE"),
            L::Money => money("DECIMAL(19, 4)"),
            L::Char { len, .. } => ex(format!("CHAR({})", len.unwrap_or(1))),
            L::Varchar { len: Some(n), .. } => ex(format!("VARCHAR({n})")),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("STRING"),
            L::Binary { len } => ex(format!("BINARY({})", len.unwrap_or(1))),
            L::Varbinary { len: Some(n) } => ex(format!("VARBINARY({n})")),
            L::Varbinary { len: None } | L::Blob => ex("BLOB"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => tz_loss(with_prec("TIME", *precision, 9, ""), *tz, E),
            L::Timestamp { precision, tz } => tz_loss(with_prec("TIMESTAMP", *precision, 9, ""), *tz, E),
            L::Enum { values } => ex(format!("ENUM({})", values.iter().map(|v| quote(v)).collect::<Vec<_>>().join(", "))),
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        standard_default(d, ty, "CURRENT_TIMESTAMP", None, false)
    }

    /// NuoDB accepts foreign keys but doesn't enforce them nor their actions.
    pub fn caps() -> Caps {
        super::caps(true, &[], &[])
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// Ocient.

mod ocient {
    use super::*;

    const E: &str = "Ocient";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "VARCHAR", smallint: "SMALLINT", rowversion: "BINARY(8)" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        let n = t.name.as_str();
        match n {
            "boolean" | "bool" => L::Bool,
            "tinyint" | "byte" => L::int(1),
            "smallint" | "short" => L::int(2),
            "int" | "integer" => L::int(4),
            "bigint" | "long" => L::int(8),
            "decimal" | "numeric" => L::Decimal { precision: p(0), scale: p(1).or(Some(0)) },
            "float" | "real" | "single precision" => L::Float { bytes: 4 },
            "double" | "double precision" => L::Float { bytes: 8 },
            "char" | "character" if p(0).is_some() => L::Char { len: p(0), unicode: true },
            "char" | "character" | "varchar" | "character varying" | "text" | "string" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            "binary" | "hash" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" | "bytea" => match p(0) {
                Some(l) => L::Varbinary { len: Some(l) },
                None => L::Blob,
            },
            "date" => L::Date,
            // Nanoseconds.
            "time" => L::Time { precision: Some(9), tz: false },
            "timestamp" => L::Timestamp { precision: Some(9), tz: false },
            "uuid" => L::Uuid,
            "ip" | "ipv4" => L::Inet,
            "point" | "st_point" => L::Geometry { kind: Some("point".into()), srid: None, geography: false },
            "linestring" | "st_linestring" => L::Geometry { kind: Some("linestring".into()), srid: None, geography: false },
            "polygon" | "st_polygon" => L::Geometry { kind: Some("polygon".into()), srid: None, geography: false },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BOOLEAN"),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(1, "TINYINT"), (2, "SMALLINT"), (4, "INT"), (8, "BIGINT")],
                || wide_int(*bytes, *unsigned, "DECIMAL", 31, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 31, 31, E),
            L::Decimal { precision: None, .. } => no_precision("DECIMAL(31, 10)"),
            L::Float { bytes: 4 } => ex("FLOAT"),
            L::Float { .. } => ex("DOUBLE"),
            L::Money => money("DECIMAL(19, 4)"),
            L::Char { len, .. } => ex(format!("CHAR({})", len.unwrap_or(1))),
            L::Varchar { len: Some(n), .. } => ex(format!("VARCHAR({n})")),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("VARCHAR"),
            L::Binary { len } => ex(format!("BINARY({})", len.unwrap_or(1))),
            L::Varbinary { len: Some(n) } => ex(format!("VARBINARY({n})")),
            L::Varbinary { len: None } | L::Blob => ex("VARBINARY"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => tz_loss(fixed_frac("TIME", *precision, 9), *tz, E),
            L::Timestamp { precision, tz } => tz_loss(fixed_frac("TIMESTAMP", *precision, 9), *tz, E),
            L::Uuid => ex("UUID"),
            L::Inet => ex("IP"),
            L::Array { of } if !matches!(**of, L::Array { .. } | L::Map { .. }) => {
                let inner = render(of);
                Rendered { native: format!("{}[]", inner.native), notes: inner.notes }
            }
            L::Geometry { kind: Some(k), .. } if matches!(k.as_str(), "point" | "linestring" | "polygon") => ex(k.to_ascii_uppercase()),
            other => shared(other, &N),
        }
    }

    /// Only literal defaults.
    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp | DefaultValue::CurrentDate | DefaultValue::CurrentTime | DefaultValue::NewUuid => None,
            other => standard_default(other, ty, "", None, false),
        }
    }

    pub fn caps() -> Caps {
        Caps { auto_increment: false, case: IdentCase::Lower, ..super::caps(false, &[], &[]) }
    }

    /// No primary keys nor unique indexes.
    pub fn finalize(t: &mut TableSchema, report: &mut Report) {
        drop_primary_key(t, report, E, None);
        no_unique_indexes(t, report, E);
    }
}

// ---------------------------------------------------------------------------
// OpenLink Virtuoso.

mod virtuoso {
    use super::*;

    const E: &str = "Virtuoso";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "LONG NVARCHAR", smallint: "SMALLINT", rowversion: "VARBINARY(8)" };
    /// Longest in-row VARCHAR (rows are limited to about 4 KB).
    const MAX_INROW: u32 = 4000;

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "integer" | "int" => L::int(4),
            "smallint" => L::int(2),
            "bigint" => L::int(8),
            "decimal" | "numeric" => L::Decimal { precision: p(0), scale: p(0).map(|_| p(1).unwrap_or(0)) },
            "real" => L::Float { bytes: 4 },
            "double precision" | "double" | "float" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: false },
            "varchar" | "character varying" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: false },
                None => L::Text { unicode: false },
            },
            "nchar" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "nvarchar" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            "long varchar" => L::Text { unicode: false },
            "long nvarchar" => L::Text { unicode: true },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" => match p(0) {
                Some(l) => L::Varbinary { len: Some(l) },
                None => L::Blob,
            },
            "long varbinary" => L::Blob,
            "long xml" | "xml" => L::Xml,
            "date" => L::Date,
            "time" => L::Time { precision: None, tz: false },
            // TIMESTAMP is filled in on every insert and update: the data is a DATETIME.
            "datetime" | "timestamp" => L::Timestamp { precision: Some(6), tz: false },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("SMALLINT").with(Info, TypeChanged, "Virtuoso no tiene booleanos: SMALLINT con 0 o 1."),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")],
                || wide_int(*bytes, *unsigned, "DECIMAL", 40, E),
                E,
            ),
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 40, 40, E),
            L::Decimal { precision: None, .. } => no_precision("DECIMAL(40, 15)"),
            L::Float { bytes: 4 } => ex("REAL"),
            L::Float { .. } => ex("DOUBLE PRECISION"),
            L::Money => money("DECIMAL(19, 4)"),
            L::Char { len, unicode: true } => sized("NCHAR", len.unwrap_or(1), MAX_INROW / 4, "LONG NVARCHAR", E),
            L::Char { len, unicode: false } => sized("CHAR", len.unwrap_or(1), MAX_INROW, "LONG VARCHAR", E),
            L::Varchar { len: Some(n), unicode: true } => sized("NVARCHAR", *n, MAX_INROW / 4, "LONG NVARCHAR", E),
            L::Varchar { len: Some(n), unicode: false } => sized("VARCHAR", *n, MAX_INROW, "LONG VARCHAR", E),
            L::Varchar { len: None, unicode } | L::Text { unicode } => ex(if *unicode { "LONG NVARCHAR" } else { "LONG VARCHAR" }),
            L::Binary { len } => sized("BINARY", len.unwrap_or(1), MAX_INROW, "LONG VARBINARY", E),
            L::Varbinary { len: Some(n) } => sized("VARBINARY", *n, MAX_INROW, "LONG VARBINARY", E),
            L::Varbinary { len: None } | L::Blob => ex("LONG VARBINARY"),
            L::Date => ex("DATE"),
            L::Time { tz, .. } => tz_loss(ex("TIME"), *tz, E),
            // Not TIMESTAMP: Virtuoso fills that one in by itself.
            L::Timestamp { precision, tz } => tz_loss(fixed_frac("DATETIME", *precision, 6), *tz, E),
            L::Xml => ex("LONG XML"),
            other => shared(other, &N),
        }
    }

    /// Only literal defaults.
    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp | DefaultValue::CurrentDate | DefaultValue::CurrentTime | DefaultValue::NewUuid => None,
            other => standard_default(other, ty, "", None, true),
        }
    }

    pub fn caps() -> Caps {
        const ACTIONS: &[&str] = &["CASCADE", "SET NULL", "SET DEFAULT", "NO ACTION"];
        Caps { max_identifier: 100, case: IdentCase::Preserve, ..super::caps(true, ACTIONS, ACTIONS) }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// Progress OpenEdge (SQL engine).

mod openedge {
    use super::*;

    const E: &str = "OpenEdge";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHARACTER", text: "CLOB", smallint: "SMALLINT", rowversion: "BINARY(8)" };
    const MAX_VARCHAR: u32 = 31_995;

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "bigint" => L::int(8),
            "integer" | "int" => L::int(4),
            "smallint" => L::int(2),
            "tinyint" => L::int(1),
            // NUMERIC defaults to (32, 0).
            "numeric" | "decimal" | "number" => L::Decimal { precision: p(0).or(Some(32)), scale: p(1).or(Some(0)) },
            "real" => L::Float { bytes: 4 },
            "float" | "double precision" | "double" => L::Float { bytes: 8 },
            "bit" | "logical" => L::Bool,
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "character varying" | "nvarchar" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            "lvarchar" | "long varchar" | "clob" => L::Text { unicode: true },
            "binary" => L::Binary { len: p(0).or(Some(1)) },
            "varbinary" => match p(0) {
                Some(l) => L::Varbinary { len: Some(l) },
                None => L::Blob,
            },
            "lvarbinary" | "long varbinary" | "blob" => L::Blob,
            "date" => L::Date,
            // Milliseconds.
            "time" => L::Time { precision: Some(3), tz: false },
            "timestamp" => L::Timestamp { precision: Some(3), tz: t.with_tz },
            "timestamp_timezone" => L::Timestamp { precision: Some(3), tz: true },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BIT"),
            L::Int { bytes, unsigned } => integer(
                *bytes,
                *unsigned,
                &[(1, "TINYINT"), (2, "SMALLINT"), (4, "INTEGER"), (8, "BIGINT")],
                || wide_int(*bytes, *unsigned, "NUMERIC", 50, E),
                E,
            ),
            // Up to 50 digits, 10 of them decimals.
            L::Decimal { precision: Some(p), scale } => decimal("NUMERIC", *p, *scale, 50, 10, E),
            L::Decimal { precision: None, .. } => no_precision("NUMERIC(50, 10)"),
            L::Float { bytes: 4 } => ex("REAL"),
            L::Float { .. } => ex("DOUBLE PRECISION"),
            L::Money => money("NUMERIC(19, 4)"),
            L::Char { len, .. } => sized("CHARACTER", len.unwrap_or(1), 2000, "CLOB", E),
            L::Varchar { len: Some(n), .. } => sized("VARCHAR", *n, MAX_VARCHAR, "CLOB", E),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("CLOB"),
            L::Binary { len } => sized("BINARY", len.unwrap_or(1), 2000, "BLOB", E),
            L::Varbinary { len: Some(n) } => sized("VARBINARY", *n, MAX_VARCHAR, "BLOB", E),
            L::Varbinary { len: None } | L::Blob => ex("BLOB"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => tz_loss(fixed_frac("TIME", *precision, 3), *tz, E),
            L::Timestamp { precision, tz: false } => fixed_frac("TIMESTAMP", *precision, 3),
            L::Timestamp { precision, tz: true } => fixed_frac("TIMESTAMP WITH TIME ZONE", *precision, 3),
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::CurrentTimestamp => "SYSTIMESTAMP".into(),
            DefaultValue::CurrentDate => "SYSDATE".into(),
            DefaultValue::CurrentTime => "SYSTIME".into(),
            other => standard_default(other, ty, "SYSTIMESTAMP", None, true)?,
        })
    }

    /// Foreign keys only restrict: no ON DELETE / ON UPDATE actions.
    pub fn caps() -> Caps {
        Caps { auto_increment: false, max_identifier: 32, case: IdentCase::Preserve, ..super::caps(true, &[], &[]) }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// SAP MaxDB (SAP DB).

mod maxdb {
    use super::*;

    const E: &str = "MaxDB";
    const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "LONG UNICODE", smallint: "SMALLINT", rowversion: "CHAR(8) BYTE" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        // `CHAR(10) BYTE` or, without the size, `CHAR BYTE`.
        let (base, code) = match t.name.split_once(' ') {
            Some((b, c)) if matches!(c, "byte" | "unicode" | "ascii") => (b, c),
            _ => (
                t.name.as_str(),
                ["byte", "unicode", "ascii"].into_iter().find(|c| t.has(c)).unwrap_or(""),
            ),
        };
        let unicode = code == "unicode";
        match (base, code) {
            ("char" | "character", "byte") => L::Binary { len: p(0).or(Some(1)) },
            ("varchar", "byte") => L::Varbinary { len: p(0) },
            ("long", "byte") => L::Blob,
            ("char" | "character", _) => L::Char { len: p(0).or(Some(1)), unicode },
            ("varchar", _) => L::Varchar { len: p(0), unicode },
            ("long", _) => L::Text { unicode },
            ("smallint", _) => L::int(2),
            ("integer" | "int", _) => L::int(4),
            ("fixed" | "decimal" | "numeric", _) => L::Decimal { precision: p(0).or(Some(5)), scale: p(1).or(Some(0)) },
            // FLOAT(p) is a decimal float of p digits.
            ("float" | "real" | "double precision", _) => L::Decimal { precision: None, scale: None },
            ("boolean", _) => L::Bool,
            ("date", _) => L::Date,
            ("time", _) => L::Time { precision: Some(0), tz: false },
            ("timestamp", _) => L::Timestamp { precision: Some(6), tz: false },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BOOLEAN"),
            L::Int { bytes, unsigned } => integer(*bytes, *unsigned, &[(2, "SMALLINT"), (4, "INTEGER")], || {
                let digits = match (bytes, unsigned) {
                    (8, false) => 19,
                    (8, true) => 20,
                    _ => 39,
                };
                if digits <= 38 {
                    ex(format!("FIXED({digits}, 0)")).with(Info, TypeChanged, format!("MaxDB no tiene BIGINT: FIXED({digits}, 0)."))
                } else {
                    ex("FIXED(38, 0)").with(Loss, RangeLoss, "MaxDB admite hasta 38 dígitos: no entran los valores de 39.")
                }
            }, E),
            L::Decimal { precision: Some(p), scale } => decimal("FIXED", *p, *scale, 38, 38, E),
            L::Decimal { precision: None, .. } => ex("FLOAT(38)").with(Info, TypeChanged, "Decimal sin precisión fija como FLOAT(38), coma flotante decimal de 38 dígitos."),
            // FLOAT(p) is a decimal float: 38 digits round-trip a float4.
            L::Float { bytes: 4 } => ex("FLOAT(38)"),
            L::Float { .. } => ex("FLOAT(38)").with(Loss, RangeLoss, "El FLOAT de MaxDB llega hasta 10^62: los valores de coma flotante mayores no entran."),
            L::Money => money("FIXED(19, 4)"),
            L::Char { len, unicode: true } => sized_code("CHAR", len.unwrap_or(1), 4000, "UNICODE", "LONG UNICODE"),
            L::Char { len, unicode: false } => sized_code("CHAR", len.unwrap_or(1), 8000, "ASCII", "LONG ASCII"),
            L::Varchar { len: Some(n), unicode: true } => sized_code("VARCHAR", *n, 4000, "UNICODE", "LONG UNICODE"),
            L::Varchar { len: Some(n), unicode: false } => sized_code("VARCHAR", *n, 8000, "ASCII", "LONG ASCII"),
            L::Varchar { len: None, unicode } | L::Text { unicode } => ex(if *unicode { "LONG UNICODE" } else { "LONG ASCII" }),
            L::Binary { len } => sized_code("CHAR", len.unwrap_or(1), 8000, "BYTE", "LONG BYTE"),
            L::Varbinary { len: Some(n) } => sized_code("VARCHAR", *n, 8000, "BYTE", "LONG BYTE"),
            L::Varbinary { len: None } | L::Blob => ex("LONG BYTE"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => tz_loss(fixed_frac("TIME", *precision, 0), *tz, E),
            L::Timestamp { precision, tz } => tz_loss(fixed_frac("TIMESTAMP", *precision, 6), *tz, E),
            L::Uuid => ex("CHAR(36) ASCII").with(Info, TypeChanged, "UUID como CHAR(36) ASCII."),
            other => shared(other, &N),
        }
    }

    /// `CHAR(n) UNICODE` up to `max`, else `long`.
    fn sized_code(name: &str, n: u32, max: u32, code: &str, long: &str) -> Rendered {
        if n <= max {
            ex(format!("{name}({n}) {code}"))
        } else {
            ex(long).with(Info, TypeChanged, format!("{name}({n}) supera el máximo de MaxDB ({max}): se usa {long}."))
        }
    }

    /// MaxDB spells "now" as the bare type names.
    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        Some(match d {
            DefaultValue::CurrentTimestamp => "TIMESTAMP".into(),
            DefaultValue::CurrentDate => "DATE".into(),
            DefaultValue::CurrentTime => "TIME".into(),
            other => standard_default(other, ty, "TIMESTAMP", None, false)?,
        })
    }

    pub fn caps() -> Caps {
        Caps { comments: true, max_identifier: 32, ..super::caps(true, &["CASCADE", "SET NULL", "SET DEFAULT", "RESTRICT"], &[]) }
    }

    pub(super) use super::no_finalize as finalize;
}

// ---------------------------------------------------------------------------
// SQream DB: GPU warehouse, no keys or indexes.

mod sqream {
    use super::*;

    const E: &str = "SQream";
    const N: Names = Names { engine: E, varchar: "TEXT", fixed: "TEXT", text: "TEXT", smallint: "SMALLINT", rowversion: "BIGINT" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "bool" | "boolean" => L::Bool,
            // TINYINT is 0–255.
            "tinyint" => L::Int { bytes: 1, unsigned: true },
            "smallint" => L::int(2),
            "int" | "integer" => L::int(4),
            "bigint" => L::int(8),
            "numeric" | "decimal" => L::Decimal { precision: p(0).or(Some(38)), scale: p(1).or(Some(0)) },
            "real" => L::Float { bytes: 4 },
            "double" | "float" => L::Float { bytes: 8 },
            "text" | "nvarchar" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            // The old VARCHAR is ASCII only.
            "varchar" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: false },
                None => L::Text { unicode: false },
            },
            "date" => L::Date,
            "datetime" | "timestamp" => L::Timestamp { precision: Some(3), tz: false },
            "datetime2" => L::Timestamp { precision: Some(9), tz: false },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BOOL"),
            L::Int { bytes: 1, unsigned: true } => ex("TINYINT"),
            L::Int { bytes, unsigned } => integer(*bytes, *unsigned, &[(2, "SMALLINT"), (4, "INT"), (8, "BIGINT")], || wide_int(*bytes, *unsigned, "NUMERIC", 38, E), E),
            L::Decimal { precision: Some(p), scale } => decimal("NUMERIC", *p, *scale, 38, 38, E),
            L::Decimal { precision: None, .. } => no_precision("NUMERIC(38, 10)"),
            L::Float { bytes: 4 } => ex("REAL"),
            L::Float { .. } => ex("DOUBLE"),
            L::Money => money("NUMERIC(19, 4)"),
            L::Char { len, .. } | L::Varchar { len: len @ Some(_), .. } => ex(format!("TEXT({})", len.unwrap_or(1))),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("TEXT"),
            L::Binary { .. } | L::Varbinary { .. } | L::Blob => {
                ex("TEXT").with(Warning, TypeApproximated, "SQream no tiene binarios: hay que guardarlos como texto (hexadecimal o base64).")
            }
            L::Date => ex("DATE"),
            L::Time { .. } => ex("TEXT(18)").with(Warning, TypeApproximated, "SQream no tiene tipo hora: queda como texto."),
            // DATETIME keeps milliseconds, DATETIME2 nanoseconds.
            L::Timestamp { precision: Some(p), tz } if *p <= 3 => tz_loss(ex("DATETIME"), *tz, E),
            L::Timestamp { precision, tz } => tz_loss(ex("DATETIME2").with_loss(precision_loss(*precision, 9)), *tz, E),
            other => shared(other, &N),
        }
    }

    /// Only literal defaults.
    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp | DefaultValue::CurrentDate | DefaultValue::CurrentTime | DefaultValue::NewUuid => None,
            other => standard_default(other, ty, "", None, false),
        }
    }

    pub fn caps() -> Caps {
        Caps { indexes: false, auto_increment: false, case: IdentCase::Lower, ..super::caps(false, &[], &[]) }
    }

    pub fn finalize(t: &mut TableSchema, report: &mut Report) {
        drop_primary_key(t, report, E, None);
    }
}

// ---------------------------------------------------------------------------
// Apache Ignite 2 (H2-based SQL).

mod ignite {
    use super::*;

    pub const E: &str = "Ignite";
    pub const N: Names = Names { engine: E, varchar: "VARCHAR", fixed: "CHAR", text: "VARCHAR", smallint: "SMALLINT", rowversion: "BINARY(8)" };

    pub fn parse(t: &TypeSpec) -> L {
        let p = |i| t.arg_u32(i);
        match t.name.as_str() {
            "boolean" | "bool" | "bit" => L::Bool,
            "tinyint" => L::int(1),
            "smallint" => L::int(2),
            "int" | "integer" => L::int(4),
            "bigint" => L::int(8),
            // Java BigDecimal: unbounded unless declared.
            "decimal" | "numeric" => L::Decimal { precision: p(0), scale: p(0).map(|_| p(1).unwrap_or(0)) },
            "real" | "float4" => L::Float { bytes: 4 },
            "double" | "float" | "double precision" | "float8" => L::Float { bytes: 8 },
            "char" | "character" => L::Char { len: p(0).or(Some(1)), unicode: true },
            "varchar" | "varchar_ignorecase" | "character varying" | "nvarchar" | "longvarchar" | "string" => match p(0) {
                Some(l) => L::Varchar { len: Some(l), unicode: true },
                None => L::Text { unicode: true },
            },
            "binary" if p(0).is_some() => L::Binary { len: p(0) },
            "varbinary" | "binary varying" if p(0).is_some() => L::Varbinary { len: p(0) },
            "binary" | "varbinary" | "binary varying" | "longvarbinary" | "blob" => L::Blob,
            "date" => L::Date,
            "time" => L::Time { precision: Some(p(0).unwrap_or(0) as u8), tz: false },
            "timestamp" => L::Timestamp { precision: Some(p(0).unwrap_or(9) as u8), tz: t.with_tz },
            "uuid" => L::Uuid,
            "geometry" => L::Geometry { kind: None, srid: None, geography: false },
            _ => L::Other { native: t.raw.clone() },
        }
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Bool => ex("BOOLEAN"),
            // DECIMAL is unbounded: wider integers fit exactly.
            L::Int { bytes, unsigned } => integer(*bytes, *unsigned, &[(1, "TINYINT"), (2, "SMALLINT"), (4, "INT"), (8, "BIGINT")], || wide_int(*bytes, *unsigned, "DECIMAL", u32::MAX, E), E),
            L::Decimal { precision: Some(p), scale } => ex(format!("DECIMAL({p}, {})", scale.unwrap_or(0).min(*p))),
            L::Decimal { precision: None, .. } => ex("DECIMAL"),
            L::Float { bytes: 4 } => ex("REAL"),
            L::Float { .. } => ex("DOUBLE"),
            L::Money => money("DECIMAL(19, 4)"),
            L::Char { len, .. } => ex(format!("CHAR({})", len.unwrap_or(1))),
            L::Varchar { len: Some(n), .. } => ex(format!("VARCHAR({n})")),
            L::Varchar { len: None, .. } | L::Text { .. } => ex("VARCHAR"),
            L::Binary { len } => ex(format!("BINARY({})", len.unwrap_or(1))),
            L::Varbinary { len: Some(n) } => ex(format!("VARBINARY({n})")),
            L::Varbinary { len: None } | L::Blob => ex("VARBINARY"),
            L::Date => ex("DATE"),
            L::Time { precision, tz } => tz_loss(fixed_frac("TIME", *precision, 0), *tz, E),
            // java.sql.Timestamp: nanoseconds.
            L::Timestamp { precision, tz } => tz_loss(fixed_frac("TIMESTAMP", *precision, 9), *tz, E),
            L::Uuid => ex("UUID"),
            L::Geometry { .. } => ex("GEOMETRY").with(Info, TypeChanged, "Requiere el módulo ignite-geospatial."),
            other => shared(other, &N),
        }
    }

    /// Only literal defaults.
    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        match d {
            DefaultValue::CurrentTimestamp | DefaultValue::CurrentDate | DefaultValue::CurrentTime | DefaultValue::NewUuid => None,
            other => standard_default(other, ty, "", None, false),
        }
    }

    pub fn caps() -> Caps {
        Caps { auto_increment: false, ..super::caps(false, &[], &[]) }
    }

    pub fn finalize(t: &mut TableSchema, report: &mut Report) {
        require_primary_key(t, report, E);
    }
}

// ---------------------------------------------------------------------------
// Apache Ignite 3 (Calcite-based SQL).

mod ignite3 {
    use super::*;

    const E: &str = "Ignite 3";
    const N: Names = Names { engine: E, ..ignite::N };

    pub fn parse(t: &TypeSpec) -> L {
        ignite::parse(t)
    }

    pub fn render(t: &L) -> Rendered {
        match t {
            L::Int { bytes, unsigned } => integer(*bytes, *unsigned, &[(1, "TINYINT"), (2, "SMALLINT"), (4, "INT"), (8, "BIGINT")], || wide_int(*bytes, *unsigned, "DECIMAL", 32_767, E), E),
            // DECIMAL without precision would be (32767, 0) and drop the decimals.
            L::Decimal { precision: None, .. } => no_precision("DECIMAL(38, 16)"),
            L::Decimal { precision: Some(p), scale } => decimal("DECIMAL", *p, *scale, 32_767, 32_767, E),
            L::Time { precision, tz } => tz_loss(with_prec("TIME", *precision, 9, ""), *tz, E),
            // WITH LOCAL TIME ZONE keeps the instant, like timestamptz.
            L::Timestamp { precision, tz } => with_prec("TIMESTAMP", *precision, 9, if *tz { " WITH LOCAL TIME ZONE" } else { "" }),
            L::Geometry { .. } => shared(t, &N),
            L::Bool | L::Float { .. } | L::Money | L::Char { .. } | L::Varchar { .. } | L::Text { .. } | L::Binary { .. } | L::Varbinary { .. } | L::Blob | L::Date | L::Uuid => {
                ignite::render(t)
            }
            other => shared(other, &N),
        }
    }

    pub fn default(d: &DefaultValue, ty: &L) -> Option<String> {
        ignite::default(d, ty)
    }

    pub fn caps() -> Caps {
        ignite::caps()
    }

    pub fn finalize(t: &mut TableSchema, report: &mut Report) {
        require_primary_key(t, report, E);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::logical_of;
    use crate::issue::Severity;
    use crate::parse::parse;

    const IDS: &[&str] = &[
        "odbc", "netsuite", "ingres", "zen", "altibase", "cubrid", "dameng", "heavydb", "iris", "cache", "machbase", "access", "dbase",
        "mimer", "monetdb", "nuodb", "ocient", "virtuoso", "openedge", "maxdb", "sqream", "ignite", "ignite3",
    ];

    fn d(id: &str) -> &'static dyn Dialect {
        lookup(id).unwrap_or_else(|| panic!("no dialect for {id}"))
    }

    fn lt(id: &str, native: &str) -> L {
        logical_of(d(id), &parse(native))
    }

    fn native(id: &str, t: &L) -> String {
        d(id).render_type(t).native
    }

    /// One of every logical kind, with the edge sizes.
    fn samples() -> Vec<L> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        vec![
            L::Bool,
            L::Int { bytes: 1, unsigned: false },
            L::Int { bytes: 1, unsigned: true },
            L::Int { bytes: 2, unsigned: false },
            L::Int { bytes: 2, unsigned: true },
            L::Int { bytes: 3, unsigned: false },
            L::Int { bytes: 4, unsigned: false },
            L::Int { bytes: 4, unsigned: true },
            L::Int { bytes: 8, unsigned: false },
            L::Int { bytes: 8, unsigned: true },
            L::Int { bytes: 16, unsigned: false },
            L::Decimal { precision: Some(12), scale: Some(2) },
            L::Decimal { precision: Some(18), scale: Some(0) },
            L::Decimal { precision: Some(60), scale: Some(20) },
            L::Decimal { precision: None, scale: None },
            L::Float { bytes: 4 },
            L::Float { bytes: 8 },
            L::Money,
            L::Char { len: Some(10), unicode: false },
            L::Char { len: Some(10), unicode: true },
            L::Char { len: Some(100_000), unicode: true },
            L::Varchar { len: Some(100), unicode: true },
            L::Varchar { len: Some(100), unicode: false },
            L::Varchar { len: Some(50_000), unicode: true },
            L::Varchar { len: None, unicode: true },
            L::Text { unicode: true },
            L::Text { unicode: false },
            L::Binary { len: Some(16) },
            L::Varbinary { len: Some(200) },
            L::Varbinary { len: None },
            L::Blob,
            L::Bit { len: Some(1) },
            L::Bit { len: Some(12) },
            L::Bit { len: None },
            L::Date,
            L::Time { precision: None, tz: false },
            L::Time { precision: Some(3), tz: true },
            L::Timestamp { precision: None, tz: false },
            L::Timestamp { precision: Some(0), tz: false },
            L::Timestamp { precision: Some(3), tz: false },
            L::Timestamp { precision: Some(6), tz: true },
            L::Timestamp { precision: Some(9), tz: false },
            L::Interval,
            L::Year,
            L::Uuid,
            L::Json { binary: true },
            L::Xml,
            L::Enum { values: s(&["a", "bb", "it's"]) },
            L::Set { values: s(&["x", "y"]) },
            L::Array { of: Box::new(L::int(4)) },
            L::Array { of: Box::new(L::Text { unicode: true }) },
            L::Map { key: Box::new(L::Text { unicode: true }), value: Box::new(L::int(4)) },
            L::Geometry { kind: Some("point".into()), srid: Some(4326), geography: false },
            L::Geometry { kind: None, srid: None, geography: true },
            L::Inet,
            L::MacAddr,
            L::RowVersion,
        ]
    }

    #[test]
    fn lookup_covers_every_preset() {
        for id in IDS {
            assert!(lookup(id).is_some(), "{id}");
        }
        assert!(lookup("oracle").is_none());
        assert!(lookup("db2").is_none());
        assert_eq!(d("cache").id(), d("iris").id());
        assert_ne!(d("ignite").id(), d("ignite3").id());
    }

    /// Every rendered type is non-empty and parses back in the same dialect
    /// to something known; unless it was an approximation (binary kept as
    /// text), a second pass renders the same spelling.
    #[test]
    fn renders_every_kind_and_reads_it_back() {
        let mut bad = Vec::new();
        for id in IDS {
            for t in samples() {
                let r = d(id).render_type(&t);
                assert!(!r.native.trim().is_empty(), "{id}: {t:?}");
                let back = lt(id, &r.native);
                if matches!(back, L::Other { .. }) {
                    bad.push(format!("{id}: {t:?} → {} isn't read back", r.native));
                    continue;
                }
                let again = native(id, &back);
                if again != r.native && r.notes.iter().all(|n| n.code != TypeApproximated) {
                    bad.push(format!("{id}: {t:?} → {} → {back:?} → {again}", r.native));
                }
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// Anything that changes or loses on the way says so.
    #[test]
    fn narrowing_is_reported() {
        let loses = |id: &str, t: L| {
            let r = d(id).render_type(&t);
            assert!(r.notes.iter().any(|n| n.severity >= Severity::Loss), "{id}: {t:?} → {} without a loss note", r.native);
        };
        loses("heavydb", L::Decimal { precision: Some(30), scale: Some(2) });
        loses("machbase", L::Decimal { precision: Some(12), scale: Some(2) });
        loses("access", L::Decimal { precision: Some(38), scale: Some(0) });
        loses("dbase", L::Text { unicode: true });
        loses("openedge", L::Decimal { precision: Some(20), scale: Some(12) });
        loses("mimer", L::Timestamp { precision: Some(6), tz: true });
        loses("access", L::Timestamp { precision: None, tz: false });
        loses("cubrid", L::Timestamp { precision: Some(6), tz: false });
        loses("ocient", L::Int { bytes: 16, unsigned: false });
        loses("maxdb", L::Float { bytes: 8 });
        for id in IDS {
            loses(id, L::Interval);
        }
    }

    #[test]
    fn ingres() {
        assert_eq!(lt("ingres", "INTEGER1"), L::int(1));
        assert_eq!(lt("ingres", "integer8"), L::int(8));
        assert_eq!(lt("ingres", "DECIMAL(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(lt("ingres", "float"), L::Float { bytes: 8 });
        assert_eq!(lt("ingres", "NVARCHAR(40)"), L::Varchar { len: Some(40), unicode: true });
        assert_eq!(lt("ingres", "LONG VARCHAR"), L::Text { unicode: false });
        assert_eq!(lt("ingres", "ANSIDATE"), L::Date);
        assert_eq!(lt("ingres", "INGRESDATE"), L::Timestamp { precision: Some(0), tz: false });
        assert_eq!(lt("ingres", "TIMESTAMP WITH LOCAL TIME ZONE"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(lt("ingres", "TIME WITHOUT TIME ZONE"), L::Time { precision: Some(0), tz: false });
        assert_eq!(lt("ingres", "INTERVAL DAY TO SECOND"), L::Interval);
        assert_eq!(lt("ingres", "VARBYTE(10)"), L::Varbinary { len: Some(10) });
        assert_eq!(native("ingres", &L::Timestamp { precision: None, tz: true }), "TIMESTAMP(6) WITH TIME ZONE");
        assert_eq!(native("ingres", &L::Varchar { len: Some(40_000), unicode: false }), "LONG VARCHAR");
        assert_eq!(native("ingres", &L::Int { bytes: 8, unsigned: true }), "DECIMAL(20, 0)");
        assert_eq!(native("ingres", &L::Date), "ANSIDATE");
        let ts = L::Timestamp { precision: Some(6), tz: false };
        assert_eq!(d("ingres").render_default(&DefaultValue::CurrentTimestamp, &ts).as_deref(), Some("LOCAL_TIMESTAMP"));
        assert_eq!(d("ingres").render_default(&DefaultValue::Bool(true), &L::Bool).as_deref(), Some("TRUE"));
        assert_eq!(d("ingres").caps().case, IdentCase::Lower);
    }

    #[test]
    fn zen() {
        assert_eq!(lt("zen", "UBIGINT"), L::Int { bytes: 8, unsigned: true });
        assert_eq!(lt("zen", "BIGIDENTITY"), L::int(8));
        assert!(d("zen").implies_auto_increment(&parse("AUTOINC(4)")));
        assert!(d("zen").implies_auto_increment(&parse("IDENTITY")));
        assert_eq!(lt("zen", "IDENTITY"), L::int(4));
        assert_eq!(lt("iris", "IDENTITY"), L::int(8));
        assert_eq!(lt("zen", "CURRENCY"), L::Money);
        assert_eq!(lt("zen", "MONEY"), L::Decimal { precision: Some(19), scale: Some(2) });
        assert_eq!(lt("zen", "NLONGVARCHAR"), L::Text { unicode: true });
        assert_eq!(lt("zen", "UNIQUEIDENTIFIER"), L::Uuid);
        assert_eq!(native("zen", &L::Int { bytes: 4, unsigned: true }), "UINTEGER");
        assert_eq!(native("zen", &L::Varchar { len: Some(100), unicode: true }), "NVARCHAR(100)");
        assert_eq!(native("zen", &L::Money), "CURRENCY");
        assert_eq!(d("zen").render_default(&DefaultValue::NewUuid, &L::Uuid).as_deref(), Some("NEWID()"));
    }

    #[test]
    fn altibase_is_oracle_like() {
        assert_eq!(lt("altibase", "DATE"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(lt("altibase", "NUMBER"), L::Decimal { precision: None, scale: None });
        assert_eq!(lt("altibase", "NUMERIC(10,0)"), L::Decimal { precision: Some(10), scale: Some(0) });
        assert_eq!(lt("altibase", "VARBYTE(8)"), L::Varbinary { len: Some(8) });
        assert_eq!(native("altibase", &L::Bool), "SMALLINT");
        assert_eq!(native("altibase", &L::Timestamp { precision: Some(3), tz: false }), "DATE");
        assert_eq!(native("altibase", &L::Varchar { len: Some(50), unicode: true }), "NVARCHAR(50)");
        assert_eq!(d("altibase").render_default(&DefaultValue::CurrentDate, &L::Date).as_deref(), Some("TRUNC(SYSDATE)"));
        assert!(d("altibase").caps().on_update.is_empty());
        assert!(!d("altibase").caps().auto_increment);
    }

    #[test]
    fn cubrid() {
        assert_eq!(lt("cubrid", "STRING"), L::Text { unicode: true });
        assert_eq!(lt("cubrid", "VARCHAR(1073741823)"), L::Text { unicode: true });
        assert_eq!(lt("cubrid", "BIT(128)"), L::Binary { len: Some(16) });
        assert_eq!(lt("cubrid", "BIT VARYING(80)"), L::Varbinary { len: Some(10) });
        assert_eq!(lt("cubrid", "TIMESTAMP"), L::Timestamp { precision: Some(0), tz: true });
        assert_eq!(lt("cubrid", "DATETIME"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(lt("cubrid", "MONETARY"), L::Money);
        assert_eq!(lt("cubrid", "ENUM('x','y')"), L::Enum { values: vec!["x".into(), "y".into()] });
        assert_eq!(native("cubrid", &L::Binary { len: Some(16) }), "BIT(128)");
        assert_eq!(native("cubrid", &L::Timestamp { precision: Some(6), tz: true }), "DATETIMETZ");
        assert_eq!(native("cubrid", &L::Json { binary: true }), "JSON");
        assert_eq!(d("cubrid").caps().case, IdentCase::Lower);
        assert!(!d("cubrid").caps().on_update.contains(&"CASCADE"));
    }

    #[test]
    fn dameng() {
        assert_eq!(lt("dameng", "INT"), L::int(4));
        assert_eq!(lt("dameng", "TINYINT"), L::int(1));
        assert_eq!(lt("dameng", "BIT"), L::Bool);
        assert_eq!(lt("dameng", "DATE"), L::Date);
        assert_eq!(lt("dameng", "DATETIME(6)"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(lt("dameng", "TIMESTAMP(6) WITH TIME ZONE"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(lt("dameng", "VARCHAR2(20 CHAR)"), L::Varchar { len: Some(20), unicode: false });
        assert_eq!(lt("dameng", "NUMBER(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        // Oracle names it doesn't list itself.
        assert_eq!(lt("dameng", "NVARCHAR2(10)"), L::Varchar { len: Some(10), unicode: true });
        assert_eq!(lt("dameng", "BINARY_DOUBLE"), L::Float { bytes: 8 });
        assert_eq!(native("dameng", &L::Timestamp { precision: Some(9), tz: true }), "TIMESTAMP(6) WITH TIME ZONE");
        assert_eq!(native("dameng", &L::Varchar { len: Some(9000), unicode: true }), "TEXT");
        let ts = L::Timestamp { precision: Some(6), tz: false };
        assert_eq!(d("dameng").render_default(&DefaultValue::CurrentTimestamp, &ts).as_deref(), Some("SYSDATE"));
    }

    #[test]
    fn heavydb() {
        assert_eq!(lt("heavydb", "TEXT ENCODING DICT(32)"), L::Text { unicode: true });
        assert_eq!(lt("heavydb", "TEXT ENCODING NONE"), L::Text { unicode: true });
        assert_eq!(lt("heavydb", "TIMESTAMP(3)"), L::Timestamp { precision: Some(3), tz: false });
        assert_eq!(lt("heavydb", "DATE ENCODING DAYS(32)"), L::Date);
        assert_eq!(lt("heavydb", "INTEGER[]"), L::Array { of: Box::new(L::int(4)) });
        assert_eq!(lt("heavydb", "POINT"), L::Geometry { kind: Some("point".into()), srid: None, geography: false });
        assert_eq!(native("heavydb", &L::Timestamp { precision: Some(4), tz: false }), "TIMESTAMP(6)");
        assert_eq!(native("heavydb", &L::Array { of: Box::new(L::Text { unicode: true }) }), "TEXT[]");
        assert_eq!(native("heavydb", &L::Geometry { kind: Some("point".into()), srid: Some(4326), geography: false }), "GEOMETRY(POINT, 4326)");
        assert_eq!(d("heavydb").render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: false }), None);
        assert_eq!(d("heavydb").render_default(&DefaultValue::Number("5".into()), &L::int(4)).as_deref(), Some("5"));
        assert!(!d("heavydb").caps().indexes);
    }

    #[test]
    fn iris() {
        assert_eq!(lt("iris", "BIT"), L::Bool);
        assert_eq!(lt("iris", "LONGVARCHAR"), L::Text { unicode: true });
        assert_eq!(lt("iris", "POSIXTIME"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(lt("cache", "MONEY"), L::Money);
        assert_eq!(lt("iris", "REAL"), L::Float { bytes: 8 });
        assert_eq!(native("iris", &L::Decimal { precision: Some(30), scale: Some(2) }), "NUMERIC(18, 2)");
        assert_eq!(native("iris", &L::RowVersion), "ROWVERSION");
        assert_eq!(native("iris", &L::Uuid), "UNIQUEIDENTIFIER");
        assert!(d("iris").implies_auto_increment(&parse("SERIAL")));
    }

    #[test]
    fn machbase() {
        assert_eq!(lt("machbase", "ULONG"), L::Int { bytes: 8, unsigned: true });
        assert_eq!(lt("machbase", "DATETIME"), L::Timestamp { precision: Some(9), tz: false });
        assert_eq!(lt("machbase", "IPV6"), L::Inet);
        assert_eq!(native("machbase", &L::Int { bytes: 2, unsigned: true }), "USHORT");
        assert_eq!(native("machbase", &L::Int { bytes: 1, unsigned: false }), "SHORT");
        assert_eq!(native("machbase", &L::Decimal { precision: Some(10), scale: Some(0) }), "LONG");
        assert!(!d("machbase").caps().defaults);
    }

    #[test]
    fn access() {
        assert_eq!(lt("access", "COUNTER"), L::int(4));
        assert!(d("access").implies_auto_increment(&parse("COUNTER")));
        assert_eq!(lt("access", "BYTE"), L::Int { bytes: 1, unsigned: true });
        assert_eq!(lt("access", "LONGCHAR"), L::Text { unicode: true });
        assert_eq!(lt("access", "VARCHAR(50)"), L::Varchar { len: Some(50), unicode: true });
        assert_eq!(lt("access", "CURRENCY"), L::Money);
        assert_eq!(lt("access", "GUID"), L::Uuid);
        assert_eq!(lt("access", "DATETIME"), L::Timestamp { precision: Some(0), tz: false });
        assert_eq!(native("access", &L::Varchar { len: Some(300), unicode: true }), "LONGTEXT");
        assert_eq!(native("access", &L::Int { bytes: 4, unsigned: false }), "LONG");
        assert_eq!(native("access", &L::Int { bytes: 8, unsigned: false }), "DECIMAL(19, 0)");
        assert_eq!(d("access").render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: false }).as_deref(), Some("Now()"));
        assert_eq!(d("access").caps().max_identifier, 64);
    }

    #[test]
    fn dbase() {
        assert_eq!(lt("dbase", "NUMERIC(10,2)"), L::Decimal { precision: Some(10), scale: Some(2) });
        assert_eq!(lt("dbase", "LOGICAL"), L::Bool);
        assert_eq!(lt("dbase", "MEMO"), L::Text { unicode: false });
        assert_eq!(native("dbase", &L::int(4)), "NUMERIC(11, 0)");
        assert_eq!(native("dbase", &L::Varchar { len: Some(300), unicode: false }), "MEMO");
        let c = d("dbase").caps();
        assert!(!c.defaults && !c.nullability && !c.foreign_keys);
        assert_eq!(c.max_identifier, 10);
        // The key stays as a unique index with a short name.
        let mut t = TableSchema {
            name: "clientes".into(),
            primary_key: Some(KeyDef { name: None, columns: vec!["ID".into()] }),
            ..Default::default()
        };
        let mut r = Report::default();
        d("dbase").finalize(&mut t, &mut r);
        assert!(t.primary_key.is_none());
        assert!(t.indexes[0].unique && t.indexes[0].name.len() <= 10, "{:?}", t.indexes);
        assert!(r.issues.iter().any(|i| i.code == PrimaryKeyDropped));
    }

    #[test]
    fn mimer() {
        assert_eq!(lt("mimer", "INTEGER(5)"), L::Decimal { precision: Some(5), scale: Some(0) });
        assert_eq!(lt("mimer", "NATIONAL CHARACTER VARYING(20)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(lt("mimer", "CHARACTER VARYING(20)"), L::Varchar { len: Some(20), unicode: false });
        assert_eq!(lt("mimer", "NCLOB"), L::Text { unicode: true });
        assert_eq!(lt("mimer", "INTERVAL DAY TO SECOND"), L::Interval);
        assert_eq!(native("mimer", &L::Varchar { len: Some(20), unicode: true }), "NVARCHAR(20)");
        assert_eq!(native("mimer", &L::Varchar { len: Some(6000), unicode: true }), "NCLOB");
        assert_eq!(d("mimer").render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: false }).as_deref(), Some("LOCALTIMESTAMP"));
    }

    #[test]
    fn monetdb() {
        assert_eq!(lt("monetdb", "hugeint"), L::int(16));
        assert_eq!(lt("monetdb", "decimal(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(lt("monetdb", "decimal"), L::Decimal { precision: Some(18), scale: Some(3) });
        assert_eq!(lt("monetdb", "timestamptz"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(lt("monetdb", "timestamp with time zone"), L::Timestamp { precision: Some(6), tz: true });
        assert_eq!(lt("monetdb", "sec_interval"), L::Interval);
        assert_eq!(lt("monetdb", "clob"), L::Text { unicode: true });
        assert_eq!(lt("monetdb", "url"), L::Text { unicode: true });
        assert_eq!(native("monetdb", &L::Int { bytes: 8, unsigned: true }), "HUGEINT");
        assert_eq!(native("monetdb", &L::Inet), "INET");
        assert_eq!(native("monetdb", &L::Uuid), "UUID");
        assert!(d("monetdb").implies_auto_increment(&parse("serial")));
    }

    #[test]
    fn nuodb() {
        assert_eq!(lt("nuodb", "STRING"), L::Text { unicode: true });
        assert_eq!(lt("nuodb", "NUMBER"), L::Decimal { precision: None, scale: None });
        assert_eq!(lt("nuodb", "ENUM('a','b')"), L::Enum { values: vec!["a".into(), "b".into()] });
        assert_eq!(native("nuodb", &L::Int { bytes: 16, unsigned: false }), "NUMBER");
        assert_eq!(native("nuodb", &L::Decimal { precision: Some(60), scale: Some(2) }), "NUMBER");
        assert_eq!(native("nuodb", &L::Enum { values: vec!["a".into(), "b".into()] }), "ENUM('a', 'b')");
    }

    #[test]
    fn ocient() {
        assert_eq!(lt("ocient", "IP"), L::Inet);
        assert_eq!(lt("ocient", "VARCHAR(20)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(lt("ocient", "TIMESTAMP"), L::Timestamp { precision: Some(9), tz: false });
        assert_eq!(native("ocient", &L::Array { of: Box::new(L::int(4)) }), "INT[]");
        assert_eq!(native("ocient", &L::Decimal { precision: Some(38), scale: Some(2) }), "DECIMAL(31, 2)");
        let mut t = TableSchema {
            name: "t".into(),
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            indexes: vec![IndexDef { name: "ux".into(), columns: vec!["a".into()], unique: true, kind: None, filter: None, ..Default::default() }],
            ..Default::default()
        };
        let mut r = Report::default();
        d("ocient").finalize(&mut t, &mut r);
        assert!(t.primary_key.is_none() && !t.indexes[0].unique);
        assert_eq!(r.issues.len(), 2);
    }

    #[test]
    fn virtuoso() {
        assert_eq!(lt("virtuoso", "LONG NVARCHAR"), L::Text { unicode: true });
        assert_eq!(lt("virtuoso", "DATETIME"), L::Timestamp { precision: Some(6), tz: false });
        assert_eq!(lt("virtuoso", "LONG XML"), L::Xml);
        // TIMESTAMP fills itself in: DATETIME holds the data.
        assert_eq!(native("virtuoso", &L::Timestamp { precision: None, tz: false }), "DATETIME");
        assert_eq!(native("virtuoso", &L::Varchar { len: Some(8000), unicode: false }), "LONG VARCHAR");
        assert_eq!(d("virtuoso").render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: false }), None);
    }

    #[test]
    fn openedge() {
        assert_eq!(lt("openedge", "LVARCHAR"), L::Text { unicode: true });
        assert_eq!(lt("openedge", "NUMERIC"), L::Decimal { precision: Some(32), scale: Some(0) });
        assert_eq!(lt("openedge", "TIMESTAMP WITH TIME ZONE"), L::Timestamp { precision: Some(3), tz: true });
        assert_eq!(native("openedge", &L::Decimal { precision: Some(20), scale: Some(12) }), "NUMERIC(20, 10)");
        assert_eq!(native("openedge", &L::Timestamp { precision: Some(3), tz: true }), "TIMESTAMP WITH TIME ZONE");
        assert_eq!(d("openedge").render_default(&DefaultValue::CurrentDate, &L::Date).as_deref(), Some("SYSDATE"));
        assert!(d("openedge").caps().on_delete.is_empty());
    }

    #[test]
    fn maxdb() {
        assert_eq!(lt("maxdb", "CHAR(10) BYTE"), L::Binary { len: Some(10) });
        assert_eq!(lt("maxdb", "VARCHAR(255) UNICODE"), L::Varchar { len: Some(255), unicode: true });
        assert_eq!(lt("maxdb", "VARCHAR(255) ASCII"), L::Varchar { len: Some(255), unicode: false });
        assert_eq!(lt("maxdb", "LONG BYTE"), L::Blob);
        assert_eq!(lt("maxdb", "LONG UNICODE"), L::Text { unicode: true });
        assert_eq!(lt("maxdb", "LONG"), L::Text { unicode: false });
        assert_eq!(lt("maxdb", "FIXED(10,2)"), L::Decimal { precision: Some(10), scale: Some(2) });
        assert_eq!(lt("maxdb", "FLOAT(38)"), L::Decimal { precision: None, scale: None });
        assert_eq!(native("maxdb", &L::int(8)), "FIXED(19, 0)");
        assert_eq!(native("maxdb", &L::Varchar { len: Some(10), unicode: true }), "VARCHAR(10) UNICODE");
        assert_eq!(d("maxdb").render_default(&DefaultValue::CurrentTimestamp, &L::Timestamp { precision: None, tz: false }).as_deref(), Some("TIMESTAMP"));
    }

    #[test]
    fn sqream() {
        assert_eq!(lt("sqream", "TINYINT"), L::Int { bytes: 1, unsigned: true });
        assert_eq!(lt("sqream", "TEXT(20)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(lt("sqream", "DATETIME2"), L::Timestamp { precision: Some(9), tz: false });
        assert_eq!(native("sqream", &L::int(1)), "SMALLINT");
        assert_eq!(native("sqream", &L::Timestamp { precision: Some(3), tz: false }), "DATETIME");
        assert_eq!(native("sqream", &L::Timestamp { precision: None, tz: false }), "DATETIME2");
    }

    #[test]
    fn ignite_needs_a_primary_key() {
        let col = |n: &str, nullable| dbine_driver::ColumnDef { name: n.into(), data_type: "INT".into(), nullable, ..Default::default() };
        let mut t = TableSchema {
            name: "t".into(),
            columns: vec![col("a", false), col("b", true)],
            indexes: vec![IndexDef { name: "ux_a".into(), columns: vec!["a".into()], unique: true, kind: None, filter: None, ..Default::default() }],
            ..Default::default()
        };
        let mut r = Report::default();
        d("ignite").finalize(&mut t, &mut r);
        assert_eq!(t.primary_key.as_ref().unwrap().columns, vec!["a".to_string()]);
        assert!(t.indexes.is_empty());
        // Without a candidate it's reported.
        let mut t = TableSchema { name: "t".into(), columns: vec![col("b", true)], ..Default::default() };
        let mut r = Report::default();
        d("ignite3").finalize(&mut t, &mut r);
        assert!(t.primary_key.is_none());
        assert!(r.issues.iter().any(|i| i.severity == Severity::Warning));
    }

    #[test]
    fn ignite3() {
        assert_eq!(native("ignite3", &L::Timestamp { precision: None, tz: true }), "TIMESTAMP(6) WITH LOCAL TIME ZONE");
        assert_eq!(native("ignite", &L::Timestamp { precision: None, tz: false }), "TIMESTAMP");
        assert_eq!(native("ignite3", &L::Decimal { precision: None, scale: None }), "DECIMAL(38, 16)");
        assert_eq!(native("ignite", &L::Decimal { precision: None, scale: None }), "DECIMAL");
        assert_eq!(lt("ignite3", "TIMESTAMP(6) WITH LOCAL TIME ZONE"), L::Timestamp { precision: Some(6), tz: true });
    }

    #[test]
    fn generic_reads_common_spellings() {
        // As SQL Server's ODBC driver reports them.
        assert_eq!(lt("odbc", "int"), L::int(4));
        assert_eq!(lt("odbc", "int identity"), L::int(4));
        assert!(d("odbc").implies_auto_increment(&parse("int identity")));
        assert_eq!(lt("odbc", "bit"), L::Bool);
        assert_eq!(lt("odbc", "tinyint"), L::int(2));
        assert_eq!(lt("odbc", "nvarchar(50)"), L::Varchar { len: Some(50), unicode: true });
        assert_eq!(lt("odbc", "nvarchar"), L::Text { unicode: true });
        assert_eq!(lt("odbc", "varbinary"), L::Blob);
        assert_eq!(lt("odbc", "datetime2"), L::Timestamp { precision: Some(7), tz: false });
        assert_eq!(lt("odbc", "datetimeoffset"), L::Timestamp { precision: Some(7), tz: true });
        assert_eq!(lt("odbc", "uniqueidentifier"), L::Uuid);
        assert_eq!(lt("odbc", "decimal(12,2)"), L::Decimal { precision: Some(12), scale: Some(2) });
        assert_eq!(lt("odbc", "money"), L::Money);
        // Other engines' names.
        assert_eq!(lt("odbc", "VARCHAR2(20 CHAR)"), L::Varchar { len: Some(20), unicode: true });
        assert_eq!(lt("odbc", "NUMBER(10)"), L::int(8));
        assert_eq!(lt("odbc", "int8"), L::int(8));
        assert_eq!(lt("odbc", "timestamp with time zone"), L::Timestamp { precision: None, tz: true });
        assert_eq!(lt("odbc", "LONGVARCHAR"), L::Text { unicode: true });
        assert_eq!(lt("odbc", "mediumint"), L::int(3));
        assert!(matches!(lt("odbc", "sql_variant"), L::Other { .. }));
    }

    #[test]
    fn generic_writes_standard_sql() {
        assert_eq!(native("odbc", &L::Bool), "SMALLINT");
        assert_eq!(native("odbc", &L::Float { bytes: 8 }), "DOUBLE PRECISION");
        assert_eq!(native("odbc", &L::Text { unicode: true }), "CLOB");
        assert_eq!(native("odbc", &L::Char { len: Some(300), unicode: true }), "VARCHAR(300)");
        assert_eq!(native("odbc", &L::Timestamp { precision: Some(6), tz: false }), "TIMESTAMP(6)");
        assert_eq!(d("odbc").render_default(&DefaultValue::Bool(true), &L::Bool).as_deref(), Some("1"));
        let c = d("odbc").caps();
        assert!(!c.auto_increment && !c.comments);
        assert_eq!(c.case, IdentCase::Preserve);
        let mut t = TableSchema { name: "t".into(), ..Default::default() };
        let mut r = Report::default();
        d("odbc").finalize(&mut t, &mut r);
        assert_eq!(r.issues.len(), 1);
        let mut r = Report::default();
        d("netsuite").finalize(&mut t, &mut r);
        assert_eq!(r.issues[0].severity, Severity::Dropped);
    }

    #[test]
    fn defaults_follow_the_engine() {
        let ts = L::Timestamp { precision: None, tz: false };
        let now = |id: &str| d(id).render_default(&DefaultValue::CurrentTimestamp, &ts);
        assert_eq!(now("cubrid").as_deref(), Some("CURRENT_DATETIME"));
        assert_eq!(now("zen").as_deref(), Some("NOW()"));
        assert_eq!(now("altibase").as_deref(), Some("SYSDATE"));
        assert_eq!(now("openedge").as_deref(), Some("SYSTIMESTAMP"));
        assert_eq!(now("monetdb").as_deref(), Some("LOCALTIMESTAMP"));
        assert_eq!(now("iris").as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(now("sqream"), None);
        assert_eq!(now("machbase"), None);
        // Engines without BOOLEAN spell true as 1.
        for id in ["cubrid", "altibase", "virtuoso", "odbc", "iris", "zen", "dameng", "openedge"] {
            assert_eq!(d(id).render_default(&DefaultValue::Bool(true), &L::Bool).as_deref(), Some("1"), "{id}");
        }
        for id in IDS {
            assert_eq!(d(id).render_default(&DefaultValue::Text("it's".into()), &L::Text { unicode: true }).as_deref().map(|s| s.contains("'it''s'")), if d(id).caps().defaults { Some(true) } else { None }, "{id}");
        }
    }

    #[test]
    fn caps_follow_the_odbc_designer() {
        // Mirrors crates/drivers/odbc/src/design.rs: reports_foreign_keys, has_indexes, auto_increment.
        let no_fk = ["heavydb", "machbase", "ignite", "ignite3", "netsuite", "dbase", "ocient", "sqream"];
        let no_ix = ["heavydb", "sqream", "netsuite"];
        let no_auto = ["odbc", "netsuite", "altibase", "openedge", "mimer", "sqream", "heavydb", "machbase", "dbase", "ignite", "ignite3", "ocient"];
        for id in IDS {
            let c = d(id).caps();
            assert_eq!(c.foreign_keys, !no_fk.contains(id), "{id} foreign keys");
            assert_eq!(c.indexes, !no_ix.contains(id), "{id} indexes");
            assert_eq!(c.auto_increment, !no_auto.contains(id), "{id} auto-increment");
            assert!(!c.partial_indexes, "{id}");
        }
    }
}
