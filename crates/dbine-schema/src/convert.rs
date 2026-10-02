//! The conversion pipeline: source tables in, target tables out, plus the
//! report of everything that changed. The output is a plain
//! [`TableSchema`] list in the target's native terms, ready for the target
//! driver's `table_ddl`.

use crate::default::{parse_default, DefaultValue};
use crate::dialect::{self, Caps, Dialect};
use crate::ident;
use crate::issue::{Issue, IssueCode, Report, Severity};
use crate::logical::LogicalType;
use crate::parse::{self, TypeSpec};
use dbine_driver::{ColumnDef, ForeignKeyDef, IndexDef, KeyDef, TableSchema};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct Options {
    /// Refold regular names to the target's case (see [`ident::refold`]).
    pub fold_case: bool,
    /// Target capabilities to use instead of the family's (the IDE passes
    /// them narrowed by the target driver's designer).
    pub target_caps: Option<Caps>,
    /// Where the tables go in the target (schema, keyspace, `bucket.scope`…).
    /// `None`: the target's default, except within one family, where the
    /// source schema is kept. References between converted tables follow.
    pub target_schema: Option<String>,
    /// Keep each table in its source schema (the name refolded to the
    /// target's case) instead of the target's default one; only when the
    /// target has schemas and `target_schema` is `None`.
    pub keep_schemas: bool,
    /// Source schema → target schema, applied before case folding (the
    /// IDE maps the default ones: SQL Server's `dbo` → PostgreSQL's
    /// `public`).
    pub rename_schemas: Vec<(String, String)>,
}

impl Default for Options {
    fn default() -> Self {
        Self { fold_case: true, target_caps: None, target_schema: None, keep_schemas: false, rename_schemas: Vec::new() }
    }
}

/// One column's journey, for the review table.
#[derive(Debug, Clone, Serialize)]
pub struct ColumnMapping {
    /// Table as named in the source.
    pub table: String,
    /// Source column; empty for a column the target needs and the source
    /// doesn't have (a time index, a row id): the data copy leaves it to
    /// its default.
    pub column: String,
    /// The column's name in the target: the data copy pairs columns by it
    /// (a target's `finalize` may rename or reorder them).
    pub target_column: String,
    pub source_type: String,
    pub logical: LogicalType,
    pub target_type: String,
}

/// Marks each target column with the source column it came from while the
/// target's `finalize` runs, which may add, move or rename columns.
const ORIGIN: &str = "__dbine_source_column";

#[derive(Debug, Clone, Serialize)]
pub struct Conversion {
    pub tables: Vec<TableSchema>,
    pub columns: Vec<ColumnMapping>,
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No dialect for this driver id yet.
    UnknownEngine(String),
    /// The engine only works as a source: tables can't be created on it.
    SourceOnly { engine: String, why: &'static str },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnknownEngine(id) => write!(f, "todavía no hay conversión de esquemas para el motor «{id}»"),
            Error::SourceOnly { engine, why } => write!(f, "«{engine}» sirve como origen pero no como destino: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// Convert `tables` read from a `from` engine (driver id) into `to`'s terms.
pub fn convert(tables: &[TableSchema], from: &str, to: &str, opts: &Options) -> Result<Conversion, Error> {
    let src = dialect::for_driver(from).ok_or_else(|| Error::UnknownEngine(from.into()))?;
    let dst = dialect::for_driver(to).ok_or_else(|| Error::UnknownEngine(to.into()))?;
    if let Some(why) = dst.target_refusal(to) {
        return Err(Error::SourceOnly { engine: to.into(), why });
    }
    let caps = opts.target_caps.clone().unwrap_or_else(|| dst.caps());
    // MariaDB has types (`uuid`, `inet6`), defaults (`curdate()` without
    // parentheses) and collations MySQL lacks: its native spelling only
    // carries over to MariaDB itself.
    let same_family = src.id() == dst.id() && !(from == "mariadb" && to != "mariadb");
    // Tables stay in their own schemas: names only need to be unique within
    // each one.
    let keep = opts.target_schema.is_none() && (same_family || opts.keep_schemas);
    let names = Names::build(tables, src, &caps, opts, keep);

    let mut out = Conversion { tables: Vec::new(), columns: Vec::new(), issues: Vec::new() };
    for t in tables {
        let mut report = Report::default();
        for (from, to) in names.renames_of(t) {
            report.push(Severity::Info, IssueCode::IdentifierRenamed, &t.name, Some(&from), format!("«{from}» pasa a llamarse «{to}»."));
        }
        let mut target = TableSchema {
            kind: t.kind.clone(),
            // A source schema (`public`, `dbo`) means nothing to another
            // engine, unless asked to keep them.
            schema: if same_family { t.schema.clone() } else if keep { t.schema.as_deref().map(|s| names.schema(s)) } else { None },
            name: names.table(&tk(t)),
            comment: None,
            ..Default::default()
        };
        let mut mappings = Vec::new();
        for c in &t.columns {
            let (mut col, mapping) = column(t, c, src, dst, &caps, &names, same_family, &mut report);
            col.options.insert(ORIGIN.into(), c.name.clone());
            target.columns.push(col);
            mappings.push(mapping);
        }
        target.primary_key = t.primary_key.as_ref().map(|k| KeyDef {
            name: k.name.as_ref().map(|n| names.object(&tk(t), n)),
            columns: k.columns.iter().map(|c| names.column(&tk(t), c)).collect(),
        });
        for fk in &t.foreign_keys {
            if let Some(fk) = foreign_key(t, fk, &caps, &names, same_family, keep, &mut report) {
                target.foreign_keys.push(fk);
            }
        }
        for ix in &t.indexes {
            if let Some(ix) = index(t, ix, &caps, &names, same_family, max_index_columns(dst.id()), to, dialect::include_support(to, caps.supports_include), &mut report) {
                target.indexes.push(ix);
            }
        }
        for ck in &t.checks {
            if same_family {
                target.checks.push(ck.clone());
            } else {
                let label = ck.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&ck.expression);
                report.push(
                    Severity::Warning,
                    IssueCode::CheckDropped,
                    &t.name,
                    Some(label),
                    format!(
                        "se quita la restricción CHECK «{label}» ({}): su expresión es SQL de otro motor; revisala y creala a mano si hace falta",
                        ck.expression
                    ),
                );
            }
        }
        match &t.comment {
            Some(c) if caps.comments => target.comment = Some(c.clone()),
            Some(_) => report.push(Severity::Info, IssueCode::CommentDropped, &t.name, None, "El destino no guarda comentarios de tabla."),
            None => {}
        }
        if same_family {
            target.options = t.options.clone();
        } else {
            for k in t.options.keys() {
                report.push(Severity::Info, IssueCode::OptionDropped, &t.name, Some(k), format!("La opción «{k}» es propia del motor de origen."));
            }
        }
        if let Some(ts) = &opts.target_schema {
            target.schema = Some(ts.clone());
            for fk in &mut target.foreign_keys {
                if tables.iter().any(|x| names.table(&tk(x)) == fk.ref_table) {
                    fk.ref_schema = Some(ts.clone());
                }
            }
        }
        dst.finalize(&mut target, &mut report);
        // Pair target columns with their source by the mark, not by position:
        // finalize may have added (a time index), moved or renamed columns.
        for c in &mut target.columns {
            let origin = c.options.remove(ORIGIN);
            let source = origin.as_deref().and_then(|o| mappings.iter().find(|m| m.column == o));
            out.columns.push(match source {
                Some(m) => ColumnMapping { target_column: c.name.clone(), target_type: c.data_type.clone(), ..m.clone() },
                None => ColumnMapping {
                    table: t.name.clone(),
                    column: String::new(),
                    target_column: c.name.clone(),
                    source_type: String::new(),
                    logical: LogicalType::Other { native: c.data_type.clone() },
                    target_type: c.data_type.clone(),
                },
            });
        }
        out.issues.extend(report.issues);
        out.tables.push(target);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn column(
    t: &TableSchema,
    c: &ColumnDef,
    src: &dyn Dialect,
    dst: &dyn Dialect,
    caps: &Caps,
    names: &Names,
    same_family: bool,
    report: &mut Report,
) -> (ColumnDef, ColumnMapping) {
    let spec = parse::parse(&c.data_type);
    let logical = logical_of(src, &spec);
    let col_name = c.name.as_str();

    // Type.
    let native = if same_family {
        c.data_type.clone()
    } else if let LogicalType::Other { native } = &logical {
        report.push(
            Severity::Warning,
            IssueCode::TypeUnknown,
            &t.name,
            Some(col_name),
            format!("No se reconoce el tipo «{native}»: queda igual y hay que revisarlo."),
        );
        native.clone()
    } else {
        let r = dst.render_type(&logical);
        for n in r.notes {
            report.push(n.severity, n.code, &t.name, Some(col_name), n.message);
        }
        r.native
    };

    // Computed columns keep their type, not their expression.
    if !same_family && (spec.rest.iter().any(|w| w == "generated") || spec.name == "as") {
        report.push(
            Severity::Warning,
            IssueCode::TypeChanged,
            &t.name,
            Some(col_name),
            "Columna calculada: en el destino queda como columna común, sin la expresión.",
        );
    }

    // Nullability: ClickHouse `Nullable(T)` is the column's nullability.
    let mut nullable = c.nullable || spec.wrappers.iter().any(|w| w == "nullable");
    // Nothing fills a row version the target doesn't generate.
    if !same_family && !nullable && logical == LogicalType::RowVersion && logical_of(dst, &parse::parse(&native)) != LogicalType::RowVersion {
        nullable = true;
        report.push(
            Severity::Warning,
            IssueCode::NullabilityChanged,
            &t.name,
            Some(col_name),
            "El destino no genera la versión de fila: la columna acepta nulos.",
        );
    }
    if !nullable && !caps.nullability {
        nullable = true;
        report.push(Severity::Warning, IssueCode::NullabilityChanged, &t.name, Some(col_name), "El destino no tiene NOT NULL: la columna acepta nulos.");
    }

    // Auto-increment, from the flag, the type (`serial`) or a sequence default.
    let parsed_default = c.default_value.as_deref().map(parse_default).map(|d| match (d, &logical) {
        // Engines without a boolean spell its defaults 0 / 1 (`bit DEFAULT ((1))`).
        (DefaultValue::Number(n), LogicalType::Bool) if n == "0" || n == "1" => DefaultValue::Bool(n == "1"),
        // …or as text (`'t'`, `'true'`: DuckDB, PostgreSQL catalogs, Informix).
        (DefaultValue::Text(v), LogicalType::Bool) if matches!(v.to_ascii_lowercase().as_str(), "t" | "f" | "true" | "false" | "y" | "n") => {
            DefaultValue::Bool(matches!(v.to_ascii_lowercase().as_str(), "t" | "true" | "y"))
        }
        (d, _) => d,
    });
    let mut auto = c.auto_increment || src.implies_auto_increment(&spec) || matches!(parsed_default, Some(DefaultValue::NextVal(_)));
    if auto && !caps.auto_increment {
        auto = false;
        report.push(
            Severity::Loss,
            IssueCode::AutoIncrementDropped,
            &t.name,
            Some(col_name),
            "El destino no tiene columnas autoincrementales: los valores nuevos hay que generarlos al insertar.",
        );
    }

    // MySQL's `CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`: the default carries over, the ON UPDATE doesn't.
    if !same_family && c.default_value.as_deref().is_some_and(|d| d.to_ascii_lowercase().contains(" on update ")) {
        report.push(
            Severity::Warning,
            IssueCode::DefaultRewritten,
            &t.name,
            Some(col_name),
            "ON UPDATE (actualizar al modificar la fila) no se traslada: en el destino hace falta un trigger.",
        );
    }

    // Default.
    let default_value = match (&c.default_value, parsed_default) {
        (_, Some(DefaultValue::NextVal(_))) => None,
        (Some(raw), _) if same_family => Some(raw.clone()),
        (Some(raw), Some(d)) if caps.defaults => match dst.render_default(&d, &logical) {
            Some(v) => Some(v),
            None => {
                report.push(
                    Severity::Warning,
                    IssueCode::DefaultDropped,
                    &t.name,
                    Some(col_name),
                    format!("El valor por defecto «{raw}» no tiene equivalente en el destino."),
                );
                None
            }
        },
        (Some(raw), Some(_)) => {
            report.push(Severity::Warning, IssueCode::DefaultDropped, &t.name, Some(col_name), format!("El destino no admite valores por defecto («{raw}»)."));
            None
        }
        _ => None,
    };

    let comment = match &c.comment {
        Some(x) if caps.comments => Some(x.clone()),
        Some(_) => {
            report.push(Severity::Info, IssueCode::CommentDropped, &t.name, Some(col_name), "El destino no guarda comentarios de columna.");
            None
        }
        None => None,
    };

    let options = if same_family {
        c.options.clone()
    } else {
        for k in c.options.keys() {
            report.push(Severity::Info, IssueCode::OptionDropped, &t.name, Some(col_name), format!("La opción de columna «{k}» es propia del motor de origen."));
        }
        Default::default()
    };

    let mapping = ColumnMapping {
        table: t.name.clone(),
        column: c.name.clone(),
        target_column: names.column(&tk(t), &c.name),
        source_type: c.data_type.clone(),
        logical: logical.clone(),
        target_type: native.clone(),
    };
    (
        ColumnDef {
            name: names.column(&tk(t), &c.name),
            data_type: native,
            nullable,
            default_value,
            auto_increment: auto,
            comment,
            options,
        },
        mapping,
    )
}

/// Classify a spelling, arrays included.
pub fn logical_of(d: &dyn Dialect, spec: &TypeSpec) -> LogicalType {
    let mut base = spec.clone();
    base.array_dims = 0;
    let mut t = d.parse_type(&base);
    for _ in 0..spec.array_dims {
        t = LogicalType::Array { of: Box::new(t) };
    }
    t
}

#[allow(clippy::too_many_arguments)]
fn foreign_key(
    t: &TableSchema,
    fk: &ForeignKeyDef,
    caps: &Caps,
    names: &Names,
    same_family: bool,
    keep: bool,
    report: &mut Report,
) -> Option<ForeignKeyDef> {
    // The referenced table: in the FK's schema, or the table's own.
    let ref_schema = fk.ref_schema.as_deref().or(t.schema.as_deref());
    let ref_key = key(ref_schema, &fk.ref_table);
    let label = fk.name.clone().unwrap_or_else(|| format!("→ {}", fk.ref_table));
    if !caps.foreign_keys {
        report.push(
            Severity::Dropped,
            IssueCode::ForeignKeyDropped,
            &t.name,
            Some(&label),
            format!("El destino no tiene claves foráneas: se omite la referencia a «{}».", fk.ref_table),
        );
        return None;
    }
    let mut action = |a: &Option<String>, allowed: &[&str], clause: &str| -> Option<String> {
        let a = a.as_ref()?.trim().to_ascii_uppercase();
        if allowed.contains(&a.as_str()) {
            return Some(a);
        }
        // RESTRICT and NO ACTION only differ in when the check runs.
        if a == "RESTRICT" && allowed.contains(&"NO ACTION") {
            report.push(Severity::Info, IssueCode::ForeignKeyActionChanged, &t.name, Some(&label), format!("{clause} RESTRICT pasa a NO ACTION."));
            return Some("NO ACTION".into());
        }
        if a != "NO ACTION" {
            report.push(Severity::Warning, IssueCode::ForeignKeyActionChanged, &t.name, Some(&label), format!("El destino no admite {clause} {a}: queda el comportamiento por defecto."));
        }
        None
    };
    let on_delete = action(&fk.on_delete, caps.on_delete, "ON DELETE");
    let on_update = action(&fk.on_update, caps.on_update, "ON UPDATE");
    Some(ForeignKeyDef {
        name: fk.name.as_ref().map(|n| names.object(&tk(t), n)),
        columns: fk.columns.iter().map(|c| names.column(&tk(t), c)).collect(),
        ref_schema: if same_family { fk.ref_schema.clone() } else if keep { ref_schema.map(|s| names.schema(s)) } else { None },
        ref_table: names.table(&ref_key),
        ref_columns: fk.ref_columns.iter().map(|c| names.column(&ref_key, c)).collect(),
        on_delete,
        on_update,
    })
}

/// Most columns an index can have in the target family (`None`: no limit
/// worth checking).
fn max_index_columns(target_family: &str) -> Option<usize> {
    match target_family {
        "postgres" | "oracle" | "mssql" => Some(32),
        "mysql" => Some(16),
        "db2" => Some(64),
        "sybase" => Some(31),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn index(
    t: &TableSchema,
    ix: &IndexDef,
    caps: &Caps,
    names: &Names,
    same_family: bool,
    max_columns: Option<usize>,
    engine: &str,
    include_support: dialect::IncludeSupport,
    report: &mut Report,
) -> Option<IndexDef> {
    if !caps.indexes {
        report.push(Severity::Dropped, IssueCode::IndexDropped, &t.name, Some(&ix.name), "El destino no tiene índices secundarios.");
        return None;
    }
    // A ClickHouse projection (a query), a PostgreSQL EXCLUDE constraint or
    // a Dremio reflection (RAW / AGGREGATION, a materialized copy) has no
    // counterpart in another engine.
    if let Some(kind) = ix.kind.as_deref().filter(|k| !same_family && ["projection", "exclude", "raw", "aggregation"].iter().any(|x| k.eq_ignore_ascii_case(x))) {
        report.push(
            Severity::Warning,
            IssueCode::IndexDropped,
            &t.name,
            Some(&ix.name),
            format!("se omite «{}» ({}) de la tabla «{}»: no tiene equivalente en el motor de destino", ix.name, kind.to_uppercase(), t.name),
        );
        return None;
    }
    if !same_family && ix.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("fulltext")) {
        report.push(
            Severity::Warning,
            IssueCode::IndexDropped,
            &t.name,
            Some(&ix.name),
            format!(
                "se omite el índice de texto completo «{}» de la tabla «{}» ({}): no se puede traducir a otro motor; creá el equivalente a mano",
                ix.name,
                t.name,
                ix.columns.join(", ")
            ),
        );
        return None;
    }
    if let Some(max) = max_columns.filter(|m| ix.columns.len() > *m) {
        report.push(
            Severity::Dropped,
            IssueCode::IndexDropped,
            &t.name,
            Some(&ix.name),
            format!("El índice tiene {} columnas y el destino admite hasta {max} por índice: se omite.", ix.columns.len()),
        );
        return None;
    }
    let mut filter = ix.filter.clone();
    if filter.is_some() && (!caps.partial_indexes || !same_family) {
        if ix.unique {
            // Without its filter a unique index would forbid more than the source did.
            report.push(
                Severity::Dropped,
                IssueCode::IndexDropped,
                &t.name,
                Some(&ix.name),
                format!("Índice único filtrado ({}): sin el filtro sería más estricto, así que se omite.", ix.filter.as_deref().unwrap_or("")),
            );
            return None;
        }
        // The filter is SQL of the source (quoting, functions, booleans): the
        // index is kept whole rather than failing on the target.
        let why = if caps.partial_indexes { "El filtro está escrito en el SQL del origen" } else { "El destino no tiene índices filtrados" };
        report.push(
            Severity::Warning,
            IssueCode::IndexChanged,
            &t.name,
            Some(&ix.name),
            format!("{why}: el índice queda completo (filtro: {}).", ix.filter.as_deref().unwrap_or("")),
        );
        filter = None;
    }
    let kind = if same_family {
        ix.kind.clone()
    } else {
        if let Some(k) = ix.kind.as_deref().filter(|k| !k.eq_ignore_ascii_case("btree") && !k.eq_ignore_ascii_case("nonclustered")) {
            report.push(Severity::Info, IssueCode::IndexChanged, &t.name, Some(&ix.name), format!("El tipo de índice «{k}» es propio del origen: queda el tipo por defecto."));
        }
        None
    };
    let include: Vec<String> = ix.include.iter().map(|c| names.column(&tk(t), c)).collect();
    let (include, options) = if same_family {
        (include, ix.options.clone())
    } else {
        if !ix.options.is_empty() {
            let list: Vec<String> = ix.options.iter().map(|(k, v)| format!("{k}={v}")).collect();
            report.push(
                Severity::Info,
                IssueCode::OptionDropped,
                &t.name,
                Some(&ix.name),
                format!("se quitan las opciones del índice «{}»: {}", ix.name, list.join(", ")),
            );
        }
        let unique_only = include_support == dialect::IncludeSupport::UniqueOnly && !ix.unique;
        if !include.is_empty() && (include_support == dialect::IncludeSupport::No || unique_only) {
            report.push(
                Severity::Warning,
                IssueCode::IndexChanged,
                &t.name,
                Some(&ix.name),
                format!(
                    "el índice «{}» pierde las columnas incluidas ({}): {engine} no tiene INCLUDE{}; el índice sigue, sin esas columnas de cobertura",
                    ix.name,
                    include.join(", "),
                    if unique_only { " en índices no únicos" } else { "" }
                ),
            );
            (Vec::new(), Default::default())
        } else {
            (include, Default::default())
        }
    };
    Some(IndexDef {
        name: names.object(&tk(t), &ix.name),
        columns: ix.columns.iter().map(|c| index_column(t, c, names, same_family)).collect(),
        unique: ix.unique,
        kind,
        filter,
        include,
        options,
    })
}

/// An index column in the target's names. MySQL reports prefix indexes as
/// `col(10)`: other engines index the whole column.
fn index_column(t: &TableSchema, c: &str, names: &Names, same_family: bool) -> String {
    if !same_family {
        if let Some((base, n)) = c.strip_suffix(')').and_then(|s| s.rsplit_once('(')) {
            if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && t.columns.iter().any(|x| x.name == base) {
                return names.column(&tk(t), base);
            }
        }
    }
    names.column(&tk(t), c)
}

/// A table's key in [`Names`]: schema and name (the same name may be in
/// several schemas).
fn key(schema: Option<&str>, name: &str) -> String {
    format!("{}\u{0}{name}", schema.unwrap_or(""))
}

fn tk(t: &TableSchema) -> String {
    key(t.schema.as_deref(), &t.name)
}

/// Target names of tables, columns and constraints, decided up front so
/// foreign keys point at the renamed tables and columns.
struct Names {
    /// Source schema → target schema name.
    schemas: HashMap<String, String>,
    tables: HashMap<String, String>,
    columns: HashMap<(String, String), String>,
    objects: HashMap<(String, String), String>,
}

impl Names {
    /// `per_schema`: tables keep their schemas, so a name only has to be
    /// unique within its schema (two schemas may each have a `log` table).
    fn build(tables: &[TableSchema], src: &dyn Dialect, caps: &Caps, opts: &Options, per_schema: bool) -> Self {
        let (from_case, to_case) = (src.caps().case, caps.case);
        let conv = |n: &str| if opts.fold_case { ident::refold(n, from_case, to_case) } else { n.to_string() };
        let max = caps.max_identifier;
        let mut table_taken: HashMap<String, HashSet<String>> = HashMap::new();
        let mut object_taken: HashMap<String, HashSet<String>> = HashMap::new();
        let mut names = Names { schemas: HashMap::new(), tables: HashMap::new(), columns: HashMap::new(), objects: HashMap::new() };
        for t in tables {
            if let Some(sc) = &t.schema {
                names.schemas.entry(sc.clone()).or_insert_with(|| match opts.rename_schemas.iter().find(|(from, _)| from == sc) {
                    Some((_, to)) => to.clone(),
                    None => ident::fit(&conv(sc), max, &mut HashSet::new()),
                });
            }
            let ns = if per_schema { t.schema.clone().unwrap_or_default() } else { String::new() };
            let k = tk(t);
            names.tables.insert(k.clone(), ident::fit(&conv(&t.name), max, table_taken.entry(ns.clone()).or_default()));
            let mut col_taken = HashSet::new();
            for c in &t.columns {
                names.columns.insert((k.clone(), c.name.clone()), ident::fit(&conv(&c.name), max, &mut col_taken));
            }
            // Constraint and index names share one namespace in many engines.
            let objects = t.primary_key.iter().filter_map(|k| k.name.clone())
                .chain(t.foreign_keys.iter().filter_map(|f| f.name.clone()))
                .chain(t.indexes.iter().map(|i| i.name.clone()));
            for o in objects {
                names.objects.insert((k.clone(), o.clone()), ident::fit(&conv(&o), max, object_taken.entry(ns.clone()).or_default()));
            }
        }
        names
    }

    fn schema(&self, s: &str) -> String {
        self.schemas.get(s).cloned().unwrap_or_else(|| s.to_string())
    }

    /// `k` is a table key ([`key`]); an unknown one (a table not being
    /// converted) keeps its own name.
    fn table(&self, k: &str) -> String {
        self.tables.get(k).cloned().unwrap_or_else(|| k.rsplit('\u{0}').next().unwrap_or(k).to_string())
    }

    fn column(&self, table: &str, n: &str) -> String {
        self.columns.get(&(table.to_string(), n.to_string())).cloned().unwrap_or_else(|| n.to_string())
    }

    fn object(&self, table: &str, n: &str) -> String {
        self.objects.get(&(table.to_string(), n.to_string())).cloned().unwrap_or_else(|| n.to_string())
    }

    /// Renames that aren't just case folding, for the report.
    fn renames_of(&self, t: &TableSchema) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let differs = |a: &str, b: &str| !a.eq_ignore_ascii_case(b);
        let tn = self.table(&tk(t));
        if differs(&t.name, &tn) {
            out.push((t.name.clone(), tn));
        }
        for c in &t.columns {
            let n = self.column(&tk(t), &c.name);
            if differs(&c.name, &n) {
                out.push((c.name.clone(), n));
            }
        }
        for ((table, o), n) in &self.objects {
            if *table == tk(t) && differs(o, n) {
                out.push((o.clone(), n.clone()));
            }
        }
        out
    }
}
