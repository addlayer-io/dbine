//! Table designer, DDL, INSERT scripts and create templates per variant.
//! The MySQL servers (and TiDB, OceanBase, SingleStore, Databend) go
//! through the shared builder; the engines with their own CREATE TABLE
//! (StarRocks / Doris key models, GreptimeDB's time index, Manticore's
//! bare attribute list) are written here.

use crate::session::lit;
use crate::Variant;
use dbine_driver::ddl::{self, AutoIncrement, SqlFlavor};
use dbine_driver::filter::{insert_where, sql_condition, ColumnFilter, FilterOp, SqlFilterStyle};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{
    kinds, Capabilities, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, ObjectRef, Result,
    RowChange, TableSchema,
};
use serde_json::Value;

fn q(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

fn opt<'a>(t: &'a TableSchema, key: &str) -> Option<&'a str> {
    t.options.get(key).map(|s| s.trim()).filter(|s| !s.is_empty())
}

pub(crate) fn capabilities(v: Variant) -> Capabilities {
    let db = v != Variant::Manticore;
    Capabilities {
        create_database: db,
        drop_database: db,
        foreign_keys: v.has_foreign_keys(),
        monitor: true,
        blocking: v.has_lock_waits(),
        kill_session: crate::processes::can_kill(v),
        processes: true,
        cancel_query: true,
        database_properties: crate::properties::supported(v),
    }
}

fn data_types(v: Variant) -> Vec<&'static str> {
    match v {
        Variant::StarRocks | Variant::Doris => vec![
            "BOOLEAN", "TINYINT", "SMALLINT", "INT", "BIGINT", "LARGEINT", "DECIMAL(10,2)", "FLOAT", "DOUBLE",
            "CHAR(10)", "VARCHAR(255)", "STRING", "DATE", "DATETIME", "JSON", "ARRAY<INT>",
        ],
        Variant::Databend => vec![
            "BOOLEAN", "TINYINT", "SMALLINT", "INT", "BIGINT", "DECIMAL(10,2)", "FLOAT", "DOUBLE", "VARCHAR",
            "DATE", "TIMESTAMP", "VARIANT", "ARRAY(INT)", "MAP(STRING, STRING)", "BINARY", "BITMAP",
        ],
        Variant::Manticore => {
            vec!["text", "string", "integer", "bigint", "float", "bool", "timestamp", "json", "multi", "multi64", "float_vector"]
        }
        Variant::GreptimeDb => vec![
            "TIMESTAMP(3)", "TIMESTAMP(9)", "STRING", "BOOLEAN", "INT", "BIGINT", "INT UNSIGNED", "BIGINT UNSIGNED",
            "FLOAT", "DOUBLE", "DECIMAL(10,2)", "DATE", "JSON", "BINARY",
        ],
        _ => {
            let mut t = vec![
                "int", "bigint", "smallint", "tinyint", "tinyint(1)", "decimal(10,2)", "float", "double",
                "varchar(255)", "char(10)", "text", "mediumtext", "longtext", "date", "datetime", "timestamp",
                "time", "year", "json", "blob", "longblob", "binary(16)", "varbinary(255)", "enum('a','b')",
            ];
            if v == Variant::MariaDb {
                t.extend(["uuid", "inet6"]);
            }
            t
        }
    }
}

fn charset_fields() -> [Field; 2] {
    [
        Field::new("charset", "Juego de caracteres", FieldKind::Text).placeholder("utf8mb4"),
        Field::new("collation", "Intercalación", FieldKind::Text).placeholder("utf8mb4_general_ci"),
    ]
}

pub(crate) fn designer(v: Variant) -> DesignerSpec {
    let mut d = DesignerSpec::sql_table(data_types(v));
    d.comments = true;
    d.foreign_keys = v.has_foreign_keys();
    match v {
        Variant::MySql | Variant::MariaDb => {
            d.table_options = vec![Field::new("engine", "Motor de almacenamiento", FieldKind::Text).placeholder("InnoDB")];
            d.table_options.extend(charset_fields());
        }
        Variant::TiDb => {
            d.table_options = charset_fields().into();
            d.table_options.push(
                Field::new(
                    "clustered_index",
                    "Clave primaria agrupada",
                    FieldKind::Select(vec![("", "Según el servidor"), ("CLUSTERED", "CLUSTERED"), ("NONCLUSTERED", "NONCLUSTERED")]),
                )
                .help("CLUSTERED guarda las filas ordenadas por la clave primaria. No se cambia con ALTER: hay que recrear la tabla."),
            );
        }
        Variant::OceanBase => d.table_options = charset_fields().into(),
        // Managed products (Aurora, Cloud SQL, VeloDB) arrive as their base engine.
        Variant::SingleStore => {
            d.table_options = vec![
                Field::new(
                    "table_type",
                    "Tipo de tabla",
                    FieldKind::Select(vec![("", "Columnar (por defecto)"), ("ROWSTORE", "ROWSTORE"), ("REFERENCE", "REFERENCE"), ("ROWSTORE REFERENCE", "ROWSTORE REFERENCE")]),
                ),
                Field::new("shard_key", "Clave de distribución (SHARD KEY)", FieldKind::Text)
                    .placeholder("`id`")
                    .help("Columnas que reparten las filas entre particiones. No se cambia con ALTER."),
                Field::new("sort_key", "Clave de orden (SORT KEY)", FieldKind::Text)
                    .placeholder("`fecha` DESC")
                    .help("Orden de las tablas columnares. No se cambia con ALTER."),
            ];
        }
        Variant::AuroraMySql | Variant::CloudSqlMySql | Variant::VeloDb => {}
        Variant::StarRocks | Variant::Doris => {
            d.auto_increment = false;
            let mut models = vec![
                ("auto", "Automático (según la clave primaria)"),
                ("duplicate", "DUPLICATE KEY"),
                ("unique", "UNIQUE KEY"),
            ];
            if v == Variant::StarRocks {
                models.push(("primary", "PRIMARY KEY"));
            }
            d.table_options = vec![
                Field::new("key_model", "Modelo de clave", FieldKind::Select(models)).default_value("auto").help(
                    "Automático: con clave primaria, PRIMARY KEY (StarRocks) o UNIQUE KEY (Doris); sin ella, DUPLICATE KEY.",
                ),
                Field::new("key_columns", "Columnas de la clave", FieldKind::Text)
                    .placeholder("id, fecha")
                    .help("Por defecto la clave primaria o la primera columna. Van primero en la tabla."),
                Field::new("distributed_by", "Distribuir por (HASH)", FieldKind::Text)
                    .help("Por defecto las columnas de la clave."),
                Field::new("buckets", "Buckets", FieldKind::Number).placeholder("automático"),
                Field::new("replication_num", "Réplicas", FieldKind::Number)
                    .default_value("1")
                    .help("1 para un servidor de un solo nodo; vacío usa el valor del clúster."),
            ];
        }
        Variant::Databend => {
            d.primary_key = false;
            d.auto_increment = false;
            d.indexes = false;
        }
        Variant::Manticore => {
            d.primary_key = false;
            d.auto_increment = false;
            d.defaults = false;
            d.nullability = false;
            d.comments = false;
            d.indexes = false;
        }
        Variant::GreptimeDb => {
            d.auto_increment = false;
            d.indexes = false;
            d.table_options = vec![Field::new("time_index", "Índice de tiempo (columna)", FieldKind::Text)
                .help("Columna TIMESTAMP obligatoria; por defecto la primera de tipo TIMESTAMP. La clave primaria son las etiquetas.")];
        }
    }
    d
}

pub(crate) fn templates(v: Variant, object_kinds: &[&'static str]) -> Vec<CreateTemplate> {
    let singlestore = v == Variant::SingleStore;
    object_kinds
        .iter()
        .filter_map(|&k| {
            let (label, template) = match k {
                kinds::VIEW => ("Nueva vista", "CREATE VIEW `{name}` AS\nSELECT id, nombre\nFROM tabla\nWHERE activo = 1;"),
                kinds::MATERIALIZED_VIEW => (
                    "Nueva vista materializada",
                    "CREATE MATERIALIZED VIEW `{name}`\nDISTRIBUTED BY HASH(`id`)\nREFRESH ASYNC EVERY (INTERVAL 1 HOUR)\nAS\nSELECT id, count(*) AS total\nFROM tabla\nGROUP BY id;",
                ),
                kinds::PROCEDURE if singlestore => (
                    "Nuevo procedimiento",
                    "CREATE OR REPLACE PROCEDURE `{name}`(p_id INT) AS\nBEGIN\n    ECHO SELECT id, nombre FROM tabla WHERE id = p_id;\nEND",
                ),
                kinds::PROCEDURE => (
                    "Nuevo procedimiento",
                    "CREATE PROCEDURE `{name}`(IN p_id INT)\nBEGIN\n    SELECT id, nombre FROM tabla WHERE id = p_id;\nEND",
                ),
                kinds::FUNCTION if singlestore => (
                    "Nueva función",
                    "CREATE OR REPLACE FUNCTION `{name}`(p_x INT) RETURNS INT AS\nBEGIN\n    RETURN p_x + 1;\nEND",
                ),
                kinds::FUNCTION => (
                    "Nueva función",
                    "CREATE FUNCTION `{name}`(p_x INT) RETURNS INT\nDETERMINISTIC\nBEGIN\n    RETURN p_x + 1;\nEND",
                ),
                kinds::TRIGGER => (
                    "Nuevo trigger",
                    "CREATE TRIGGER `{name}` BEFORE INSERT ON `tabla`\nFOR EACH ROW\nSET NEW.actualizado = CURRENT_TIMESTAMP;",
                ),
                _ => return None,
            };
            Some(CreateTemplate { kind: k, label, template: template.into() })
        })
        .collect()
}

/// The table with comments escaped for engines that read backslash
/// escapes in string literals (the shared builder only doubles quotes).
fn escaped_comments(t: &TableSchema) -> TableSchema {
    let esc = |c: &Option<String>| c.as_ref().map(|s| s.replace('\\', "\\\\"));
    let mut t = t.clone();
    t.comment = esc(&t.comment);
    for c in &mut t.columns {
        c.comment = esc(&c.comment);
    }
    t
}

fn flavor(v: Variant) -> SqlFlavor {
    let bools = matches!(v, Variant::Databend | Variant::GreptimeDb);
    SqlFlavor {
        quote: Quote::Backtick,
        auto_increment: if v == Variant::Databend { AutoIncrement::None } else { AutoIncrement::AutoIncrementKeyword },
        comment_on: false,
        inline_comments: true,
        if_exists: true,
        fk_inline: false,
        multi_row_insert: true,
        true_literal: if bools { "TRUE" } else { "1" },
        false_literal: if bools { "FALSE" } else { "0" },
    }
}

/// The script that applies schema changes ("Comparar esquemas"): `MODIFY
/// COLUMN` with the whole column, indexes and keys dropped the MySQL way.
pub(crate) fn sync_script(v: Variant, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
    use dbine_driver::alter::{AlterStyle, ColumnAlter, DropIndex};
    let f = flavor(v);
    let column = match v {
        // Columns are only added and dropped.
        Variant::Manticore => ColumnAlter::None,
        _ => ColumnAlter::Modify { keyword: "MODIFY COLUMN" },
    };
    let cd = |t: &TableSchema, c: &dbine_driver::ColumnDef| column_definition(v, t, c);
    let dd = |t: &TableSchema, p: DdlParts| table_ddl(v, t, p);
    let mut st = AlterStyle::from_flavor(&f, column, &cd, &dd);
    st.drop_index = if matches!(v, Variant::StarRocks | Variant::Doris | Variant::VeloDb) { DropIndex::OnTable } else { DropIndex::AlterTable };
    st.drop_fk = "DROP FOREIGN KEY";
    st.drop_pk_keyword = true;
    let comments = |t: &TableSchema, c: Option<&dbine_driver::ColumnDef>, text: Option<&str>| comment_change(v, t, c, text);
    let mut script = dbine_driver::alter::sync_script_with_comments(&st, Some(&comments), changes)?;
    if matches!(v, Variant::StarRocks | Variant::Doris | Variant::VeloDb) && changes.iter().any(|c| matches!(c, dbine_driver::TableChange::Alter { .. })) {
        script.warnings.push("Los cambios de columnas e índices son trabajos asincrónicos del servidor: cada uno tiene que terminar antes del siguiente en la misma tabla (SHOW ALTER TABLE COLUMN). Si una sentencia falla porque la tabla está ocupada, volvé a ejecutarla cuando termine.".into());
    }
    engine_sync(v, changes, &mut script)?;
    Ok(script)
}

/// The whole column as `MODIFY` / `CHANGE COLUMN` take it (name included):
/// type, generated expression, default, nullability, `AUTO_INCREMENT` and
/// comment, from `t`'s own copy of the column when it has one.
pub(crate) fn column_definition(v: Variant, t: &TableSchema, c: &dbine_driver::ColumnDef) -> String {
    let f = flavor(v);
    let t = escaped_comments(t);
    let mut c = t.columns.iter().find(|x| x.name == c.name).cloned().unwrap_or_else(|| c.clone());
    // A MariaDB column CHECK goes after the rest of the column.
    let (ty, check) = column_check(&c.data_type);
    c.data_type = ty;
    let def = ddl::column_def(&f, &t, &c);
    match check {
        Some(ch) => format!("{def} {ch}"),
        None => def,
    }
}

/// A table comment change (the schema sync): `ALTER TABLE … COMMENT =`
/// (Doris: `MODIFY COMMENT`; GreptimeDB: `COMMENT ON`). A column's goes
/// with `MODIFY COLUMN` and its `COMMENT`, except in GreptimeDB, whose
/// `MODIFY COLUMN` only changes the type.
fn comment_change(v: Variant, t: &TableSchema, c: Option<&dbine_driver::ColumnDef>, text: Option<&str>) -> Option<String> {
    let name = dbine_driver::sql::qualified_name(Quote::Backtick, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name);
    let value = |null: &str| text.map(lit).unwrap_or_else(|| null.to_string());
    match (v, c) {
        (Variant::Manticore, _) => None,
        (Variant::GreptimeDb, Some(c)) => Some(format!("COMMENT ON COLUMN {name}.{} IS {};", quote_ident(Quote::Backtick, &c.name), value("NULL"))),
        (Variant::GreptimeDb, None) => Some(format!("COMMENT ON TABLE {name} IS {};", value("NULL"))),
        (_, Some(_)) => None,
        (Variant::Doris | Variant::VeloDb, None) => Some(format!("ALTER TABLE {name} MODIFY COMMENT {};", value("''"))),
        (_, None) => Some(format!("ALTER TABLE {name} COMMENT = {};", value("''"))),
    }
}

/// What the generic plan can't write for these engines: CHECKs that aren't
/// enforced, TiDB's primary key kind, GreptimeDB's column indexes,
/// StarRocks / Doris bloom filter columns, Manticore's table settings.
fn engine_sync(v: Variant, changes: &[dbine_driver::TableChange], script: &mut dbine_driver::SyncScript) -> Result<()> {
    use dbine_driver::TableChange;
    let checks: Vec<dbine_driver::CheckDef> = changes
        .iter()
        .flat_map(|c| match c {
            TableChange::Create { table } | TableChange::Alter { new: table, .. } => table.checks.clone(),
            TableChange::Drop { .. } => Vec::new(),
        })
        .collect();
    for s in &mut script.statements {
        *s = crate::structure::place_not_enforced(s, &checks);
        // ALTER only adds NONCLUSTERED primary keys to TiDB tables.
        if v == Variant::TiDb && s.contains(" ADD PRIMARY KEY (") && s.ends_with(");") {
            s.pop();
            s.push_str(" NONCLUSTERED;");
        }
    }
    if matches!(v, Variant::StarRocks | Variant::Doris | Variant::Databend) {
        // Rollups (StarRocks, Doris) and Databend's indexes aren't dropped with DROP INDEX.
        for c in changes {
            let TableChange::Alter { old, new } = c else { continue };
            let name = dbine_driver::sql::qualified_name(Quote::Backtick, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name);
            for o in &old.indexes {
                let (generic, own) = if v == Variant::Databend {
                    (format!("ALTER TABLE {name} DROP INDEX {};", q(&o.name)), databend_index(old, o, false)?)
                } else if o.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("ROLLUP")) {
                    (format!("DROP INDEX {} ON {name};", q(&o.name)), format!("ALTER TABLE {name} DROP ROLLUP {};", q(&o.name)))
                } else {
                    continue;
                };
                if let Some(s) = script.statements.iter_mut().find(|s| **s == generic) {
                    *s = own;
                }
            }
        }
    }
    if matches!(v, Variant::StarRocks | Variant::Doris) {
        merge_olap_drops(&mut script.statements);
        // One schema change per statement, so each can wait for the one before.
        let one_line = |l: &str| ["ALTER TABLE ", "CREATE INDEX ", "DROP INDEX "].iter().any(|k| l.starts_with(k)) && l.ends_with(';');
        script.statements = std::mem::take(&mut script.statements)
            .into_iter()
            .flat_map(|s| if s.lines().all(one_line) { s.lines().map(str::to_string).collect() } else { vec![s] })
            .collect();
    }
    for c in changes {
        let TableChange::Alter { old, new } = c else { continue };
        let name = dbine_driver::sql::qualified_name(Quote::Backtick, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name);
        let changed = |k: &str| opt(old, k) != opt(new, k);
        match v {
            Variant::TiDb if changed("clustered_index") && old.primary_key.is_some() && new.primary_key.is_some() => {
                script.warnings.push(format!(
                    "{}: TiDB no cambia una clave primaria entre CLUSTERED y NONCLUSTERED con ALTER; hay que recrear la tabla.",
                    new.name
                ));
            }
            Variant::GreptimeDb => {
                // The generic plan drops indexes by name; GreptimeDB unsets them on the column.
                for o in &old.indexes {
                    let drop = format!("ALTER TABLE {name} DROP INDEX {};", q(&o.name));
                    let Some(at) = script.statements.iter().position(|s| *s == drop) else { continue };
                    // A column keeps its full-text analyzer and case_sensitive
                    // even after UNSET: those only change with a new column.
                    let locked = |n: &dbine_driver::IndexDef| {
                        ["analyzer", "case_sensitive"].iter().any(|k| o.options.get(*k) != n.options.get(*k))
                    };
                    let fulltext = o.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("FULLTEXT"));
                    if let Some(n) = new.indexes.iter().find(|n| n.name == o.name && n.columns == o.columns).filter(|n| fulltext && locked(n)) {
                        let set = greptime_index(new, n, true)?;
                        script.statements.remove(at);
                        for s in &mut script.statements {
                            *s = s.lines().filter(|l| *l != set).collect::<Vec<_>>().join("\n");
                        }
                        script.statements.retain(|s| !s.trim().is_empty());
                        script.warnings.push(format!(
                            "{}.{}: GreptimeDB no cambia el analizador ni case_sensitive de un índice FULLTEXT ya creado; hay que recrear la columna o la tabla.",
                            new.name, o.columns.join(", ")
                        ));
                        continue;
                    }
                    script.statements[at] = greptime_index(old, o, false)?;
                }
                // Table options: SET the changed ones, UNSET the ones that go.
                let keys: std::collections::BTreeSet<&String> = greptime_options(old).chain(greptime_options(new)).map(|(k, _)| k).collect();
                // compaction.type isn't set on its own: setting a
                // compaction.twcs.* option sets it (TWCS is the only one).
                for k in keys.into_iter().filter(|k| changed(k) && k.as_str() != "compaction.type") {
                    let key = format!("'{}'", k.replace('\'', "''"));
                    script.statements.push(match opt(new, k) {
                        Some(val) => format!("ALTER TABLE {name} SET {key}='{}';", val.replace('\'', "''")),
                        None => format!("ALTER TABLE {name} UNSET {key};"),
                    });
                }
            }
            Variant::SingleStore => {
                for (k, what) in [("shard_key", "SHARD KEY"), ("sort_key", "SORT KEY"), ("table_type", "el tipo de tabla")] {
                    if changed(k) {
                        script.warnings.push(format!("{}: SingleStore no cambia {what} con ALTER; hay que recrear la tabla.", new.name));
                    }
                }
            }
            Variant::StarRocks | Variant::Doris if changed("bloom_filter_columns") => {
                let cols = opt(new, "bloom_filter_columns").unwrap_or("");
                script.statements.push(format!("ALTER TABLE {name} SET (\"bloom_filter_columns\" = \"{}\");", cols.replace('"', "")));
            }
            Variant::Manticore => {
                let keys: std::collections::BTreeSet<&String> = old.options.keys().chain(new.options.keys()).collect();
                let set: Vec<String> = keys
                    .into_iter()
                    .filter(|k| changed(k))
                    .map(|k| crate::structure::manticore_setting(k, opt(new, k).unwrap_or("")))
                    .collect();
                if !set.is_empty() {
                    script.statements.push(format!("ALTER TABLE {name} {};", set.join(" ")));
                    script.warnings.push(format!("{}: cambiar los ajustes de indexación no reindexa los documentos que ya están.", new.name));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn table_ddl(v: Variant, t: &TableSchema, parts: DdlParts) -> Result<String> {
    match v {
        Variant::StarRocks | Variant::Doris => olap_ddl(v, t, parts),
        Variant::GreptimeDb => greptime_ddl(t, parts),
        Variant::Manticore => Ok(manticore_ddl(t, parts)),
        _ => Ok(mysql_ddl(v, t, parts)),
    }
}

/// `PRIMARY KEY (…)` followed by TiDB's CLUSTERED / NONCLUSTERED.
fn tidb_clustered(create: &str, how: &str) -> String {
    create
        .lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("PRIMARY KEY (") || (t.starts_with("CONSTRAINT ") && t.contains(" PRIMARY KEY (")) {
                match l.strip_suffix(',') {
                    Some(body) => format!("{body} {how},"),
                    None => format!("{l} {how}"),
                }
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `CREATE … INDEX` with the key parts (prefixes, expressions, DESC) and
/// the index options MySQL, MariaDB and TiDB take.
fn mysql_index(v: Variant, t: &TableSchema, ix: &dbine_driver::IndexDef, if_exists: bool) -> String {
    let kind = ix.kind.as_deref().unwrap_or("").to_ascii_uppercase();
    if v == Variant::SingleStore && kind == "FULLTEXT" {
        // SingleStore adds full-text indexes with ALTER TABLE only.
        let version = ix.options.get("VERSION").map(|n| format!("USING VERSION {n} ")).unwrap_or_default();
        let cols: Vec<String> = ix.columns.iter().map(|c| q(c)).collect();
        return format!("ALTER TABLE {} ADD FULLTEXT {version}{} ({});", q(&t.name), q(&ix.name), cols.join(", "));
    }
    let prefix = match kind.as_str() {
        "FULLTEXT" | "SPATIAL" => format!("{kind} "),
        _ if ix.unique => "UNIQUE ".into(),
        _ => String::new(),
    };
    let guard = if if_exists && v == Variant::MariaDb { "IF NOT EXISTS " } else { "" };
    let cols: Vec<String> = ix.columns.iter().map(|c| crate::structure::key_part(ix, c)).collect();
    let mut s = format!("CREATE {prefix}INDEX {guard}{} ON {} ({})", q(&ix.name), q(&t.name), cols.join(", "));
    let memory = opt(t, "engine").is_some_and(|e| e.eq_ignore_ascii_case("MEMORY") || e.eq_ignore_ascii_case("HEAP"));
    match kind.as_str() {
        "HASH" => s.push_str(" USING HASH"),
        // MEMORY tables default to HASH.
        "BTREE" if memory => s.push_str(" USING BTREE"),
        _ => {}
    }
    let o = |k: &str| ix.options.get(k).map(|s| s.as_str());
    if let Some(p) = o("WITH PARSER") {
        s.push_str(&format!(" WITH PARSER {}", q(p)));
    }
    if let Some(c) = o("COMMENT") {
        s.push_str(&format!(" COMMENT {}", lit(c)));
    }
    if o("INVISIBLE") == Some("YES") {
        s.push_str(" INVISIBLE");
    }
    if o("IGNORED") == Some("YES") {
        s.push_str(" IGNORED");
    }
    s.push(';');
    s
}

/// A column type without its MariaDB column CHECK, and that CHECK.
fn column_check(ty: &str) -> (String, Option<String>) {
    let (ty, check) = crate::structure::split_column_check(ty);
    (ty.to_string(), check.map(str::to_string))
}

/// SingleStore: the table type (`CREATE ROWSTORE TABLE`) and the shard and
/// sort keys inside the column list.
fn singlestore_create(create: &str, t: &TableSchema) -> String {
    let mut s = create.to_string();
    let keys: Vec<String> = [("shard_key", "SHARD KEY"), ("sort_key", "SORT KEY")]
        .iter()
        .filter_map(|(k, kw)| opt(t, k).map(|cols| format!("    {kw} ({cols})")))
        .collect();
    if let Some(at) = s.find("CREATE TABLE ") {
        if !keys.is_empty() {
            if let Some(close) = s[at..].find("\n)") {
                s.insert_str(at + close, &format!(",\n{}", keys.join(",\n")));
            }
        }
        if let Some(ty) = opt(t, "table_type") {
            s.replace_range(at..at + "CREATE TABLE ".len(), &format!("CREATE {ty} TABLE "));
        }
    }
    s
}

/// Databend's indexes: inverted and ngram ones on columns, aggregating ones
/// on a query.
fn databend_index(t: &TableSchema, ix: &dbine_driver::IndexDef, create: bool) -> Result<String> {
    let kind = ix.kind.as_deref().unwrap_or("").trim().to_ascii_uppercase();
    let name = q(&ix.name);
    if kind == "AGGREGATING" {
        let query = ix.options.get("query").ok_or_else(|| Error::Query(format!("el índice agregado «{}» no tiene consulta", ix.name)))?;
        return Ok(if create { format!("CREATE AGGREGATING INDEX {name} AS {query};") } else { format!("DROP AGGREGATING INDEX {name};") });
    }
    if !["INVERTED", "NGRAM"].contains(&kind.as_str()) {
        return Err(Error::Unsupported(format!("Databend solo tiene índices INVERTED, NGRAM y AGGREGATING (índice «{}»)", ix.name)));
    }
    let table = q(&t.name);
    if !create {
        return Ok(format!("DROP {kind} INDEX {name} ON {table};"));
    }
    let cols: Vec<String> = ix.columns.iter().map(|c| q(c)).collect();
    let opts: String = ix.options.iter().map(|(k, v)| format!(" {k}='{}'", v.replace('\'', "''"))).collect();
    Ok(format!("CREATE {kind} INDEX {name} ON {table} ({}){opts};", cols.join(", ")))
}

fn mysql_ddl(v: Variant, t: &TableSchema, parts: DdlParts) -> String {
    let f = flavor(v);
    let mut t = escaped_comments(t);
    if v == Variant::Databend {
        t.primary_key = None;
    }
    // MariaDB column CHECKs: written last in their column's line.
    let mut column_checks: Vec<(String, String)> = Vec::new();
    for c in &mut t.columns {
        if let (ty, Some(ch)) = column_check(&c.data_type) {
            c.data_type = ty;
            column_checks.push((c.name.clone(), ch));
        }
    }
    let mut out = Vec::new();
    if parts.drop || parts.create {
        let mut s = ddl::table_ddl(&f, &t, DdlParts { indexes: false, foreign_keys: false, ..parts });
        if parts.create && !column_checks.is_empty() {
            s = s
                .lines()
                .map(|l| match column_checks.iter().find(|(n, _)| l.starts_with(&format!("    {} ", q(n)))) {
                    Some((_, ch)) => match l.strip_suffix(',') {
                        Some(body) => format!("{body} {ch},"),
                        None => format!("{l} {ch}"),
                    },
                    None => l.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        if parts.create {
            // Generated columns take no NULL (MariaDB refuses it).
            for c in t.columns.iter().filter(|c| c.data_type.contains(" GENERATED ALWAYS AS ")) {
                let line = format!("{} {}", q(&c.name), c.data_type);
                s = s.replace(&format!("{line} NULL"), &line);
            }
            let mut opts = Vec::new();
            if let Some(e) = opt(&t, "engine") {
                opts.push(format!("ENGINE={e}"));
            }
            let collation = opt(&t, "collation");
            if let Some(c) = opt(&t, "charset").or_else(|| collation.and_then(|c| c.split('_').next())) {
                opts.push(format!("DEFAULT CHARSET={c}"));
            }
            if let Some(c) = collation {
                opts.push(format!("COLLATE={c}"));
            }
            if !opts.is_empty() {
                s.pop();
                s.push_str(&format!(" {};", opts.join(" ")));
            }
            if v == Variant::TiDb && t.primary_key.is_some() {
                if let Some(how) = opt(&t, "clustered_index") {
                    s = tidb_clustered(&s, how);
                }
            }
            s = crate::structure::place_not_enforced(&s, &t.checks);
            if v == Variant::SingleStore {
                s = singlestore_create(&s, &t);
            }
        }
        out.push(s);
    }
    if parts.indexes && v != Variant::Databend {
        for ix in &t.indexes {
            out.push(mysql_index(v, &t, ix, parts.if_exists));
        }
    }
    if parts.indexes && v == Variant::Databend {
        for ix in &t.indexes {
            match databend_index(&t, ix, true) {
                Ok(s) => out.push(s),
                Err(e) => tracing::debug!("databend: {e}"),
            }
        }
    }
    if parts.foreign_keys && v.has_foreign_keys() {
        let fks = ddl::table_ddl(&f, &t, DdlParts { foreign_keys: true, ..Default::default() });
        if !fks.is_empty() {
            out.push(fks);
        }
    }
    out.retain(|s| !s.is_empty());
    out.join("\n")
}

fn is_call(d: &str) -> bool {
    let d = d.trim();
    d.ends_with(')') && d.split('(').next().is_some_and(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// StarRocks and Doris: a key model, key columns first, hash distribution.
fn olap_ddl(v: Variant, t: &TableSchema, parts: DdlParts) -> Result<String> {
    let name = q(&t.name);
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let pk: Vec<String> = t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default();
        let model = match (opt(t, "key_model").unwrap_or("auto"), v) {
            ("duplicate", _) => "DUPLICATE",
            ("primary", Variant::StarRocks) => "PRIMARY",
            ("unique" | "primary", _) => "UNIQUE",
            _ if pk.is_empty() => "DUPLICATE",
            (_, Variant::StarRocks) => "PRIMARY",
            _ => "UNIQUE",
        };
        let keys: Vec<String> = match opt(t, "key_columns") {
            Some(k) => k.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect(),
            None if !pk.is_empty() => pk,
            None => t.columns.first().map(|c| vec![c.name.clone()]).unwrap_or_default(),
        };
        if keys.is_empty() {
            return Err(Error::Query("la tabla necesita al menos una columna".into()));
        }
        // Key columns go first, in key order.
        let mut cols = Vec::new();
        for k in &keys {
            let c = t.columns.iter().find(|c| &c.name == k);
            cols.push(c.ok_or_else(|| Error::Query(format!("la columna de clave «{k}» no está en la tabla")))?);
        }
        cols.extend(t.columns.iter().filter(|c| !keys.contains(&c.name)));
        let lines: Vec<String> = cols
            .iter()
            .map(|c| {
                let key = keys.contains(&c.name) && model != "DUPLICATE";
                let mut l = format!("    {} {}", q(&c.name), c.data_type);
                l.push_str(if !c.nullable || key { " NOT NULL" } else { " NULL" });
                if let Some(d) = c.default_value.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                    // Defaults are string literals, CURRENT_TIMESTAMP aside.
                    let quoted = d.starts_with('\'') || d.starts_with('"') || is_call(d) || d.eq_ignore_ascii_case("CURRENT_TIMESTAMP");
                    l.push_str(&format!(" DEFAULT {}", if quoted { d.to_string() } else { lit(d) }));
                }
                if let Some(cm) = c.comment.as_deref().filter(|s| !s.is_empty()) {
                    l.push_str(&format!(" COMMENT {}", lit(cm)));
                }
                l
            })
            .collect();
        let key_list = keys.iter().map(|k| q(k)).collect::<Vec<_>>().join(", ");
        let mut s = format!(
            "CREATE TABLE {}{name} (\n{}\n)\n{model} KEY({key_list})",
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            lines.join(",\n")
        );
        if let Some(cm) = t.comment.as_deref().filter(|s| !s.is_empty()) {
            s.push_str(&format!("\nCOMMENT {}", lit(cm)));
        }
        let dist = match opt(t, "distributed_by") {
            Some(d) => d.split(',').map(|c| q(c.trim())).collect::<Vec<_>>().join(", "),
            None => key_list,
        };
        s.push_str(&format!("\nDISTRIBUTED BY HASH({dist})"));
        if let Some(b) = opt(t, "buckets") {
            s.push_str(&format!(" BUCKETS {b}"));
        }
        let mut props = Vec::new();
        if let Some(r) = opt(t, "replication_num") {
            props.push(format!("\"replication_num\" = \"{}\"", r.replace('"', "")));
        }
        if let Some(b) = opt(t, "bloom_filter_columns") {
            props.push(format!("\"bloom_filter_columns\" = \"{}\"", b.replace('"', "")));
        }
        if !props.is_empty() {
            s.push_str(&format!("\nPROPERTIES ({})", props.join(", ")));
        }
        s.push(';');
        out.push(s);
    }
    if parts.indexes {
        let is_rollup = |ix: &&dbine_driver::IndexDef| ix.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("ROLLUP"));
        let rollups: Vec<String> =
            t.indexes.iter().filter(is_rollup).map(|r| format!("{} ({})", q(&r.name), r.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "))).collect();
        let mut adds = Vec::new();
        for ix in t.indexes.iter().filter(|i| !is_rollup(i)) {
            if ix.unique {
                return Err(Error::Unsupported(format!(
                    "{} no tiene índices únicos (índice «{}»): use el modelo UNIQUE KEY / PRIMARY KEY",
                    if v == Variant::Doris { "Doris" } else { "StarRocks" },
                    ix.name
                )));
            }
            let kind = ix.kind.as_deref().filter(|k| !k.is_empty() && !k.eq_ignore_ascii_case("btree"));
            let kind = kind.unwrap_or(if v == Variant::Doris { "INVERTED" } else { "BITMAP" }).to_ascii_uppercase();
            let guard = if parts.if_exists && v == Variant::Doris { "IF NOT EXISTS " } else { "" };
            let cols: Vec<String> = ix.columns.iter().map(|c| q(c)).collect();
            // Index properties (NGRAMBF's gram_num, GIN's parser…): StarRocks
            // takes them after the type, Doris in PROPERTIES.
            let props: Vec<String> = ix
                .options
                .iter()
                .filter(|(k, _)| k.as_str() != "COMMENT")
                .map(|(k, val)| format!("\"{}\" = \"{}\"", k.replace('"', ""), val.replace('"', "")))
                .collect();
            let props = match (props.is_empty(), v) {
                (true, _) => String::new(),
                (false, Variant::Doris) => format!(" PROPERTIES({})", props.join(", ")),
                (false, _) => format!(" ({})", props.join(", ")),
            };
            let comment = ix.options.get("COMMENT").map(|c| format!(" COMMENT {}", lit(c))).unwrap_or_default();
            adds.push((format!("{guard}{}", q(&ix.name)), format!("({}) USING {kind}{props}{comment}", cols.join(", "))));
        }
        // Each index change is a schema change job, and a table takes one
        // at a time: several indexes go in one ALTER TABLE.
        match adds.as_slice() {
            [] => {}
            [(n, rest)] => out.push(format!("CREATE INDEX {n} ON {name} {rest};")),
            _ => {
                let all: Vec<String> = adds.iter().map(|(n, rest)| format!("ADD INDEX {n} {rest}")).collect();
                out.push(format!("ALTER TABLE {name} {};", all.join(", ")));
            }
        }
        // Rollups: other sort orders of the table's data, one job for all.
        if !rollups.is_empty() {
            out.push(format!("ALTER TABLE {name} ADD ROLLUP {};", rollups.join(", ")));
        }
    }
    Ok(out.join("\n"))
}

/// GreptimeDB: a TIME INDEX column, tags as the primary key, the table
/// comment in `WITH`.
fn greptime_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    let name = q(&t.name);
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let ts = match opt(t, "time_index") {
            Some(c) => c.to_string(),
            None => t
                .columns
                .iter()
                .find(|c| c.data_type.to_ascii_lowercase().starts_with("timestamp"))
                .map(|c| c.name.clone())
                .ok_or_else(|| Error::Query("GreptimeDB necesita una columna TIMESTAMP como índice de tiempo".into()))?,
        };
        let t = escaped_comments(t);
        let mut lines: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                let mut l = format!("    {} {}", q(&c.name), c.data_type);
                l.push_str(if !c.nullable || c.name == ts { " NOT NULL" } else { " NULL" });
                if let Some(d) = c.default_value.as_deref().filter(|d| !d.is_empty()) {
                    l.push_str(&format!(" DEFAULT {d}"));
                }
                if let Some(cm) = c.comment.as_deref().filter(|s| !s.is_empty()) {
                    l.push_str(&format!(" COMMENT '{}'", cm.replace('\'', "''")));
                }
                l
            })
            .collect();
        lines.push(format!("    TIME INDEX ({})", q(&ts)));
        let tags: Vec<String> =
            t.primary_key.iter().flat_map(|k| &k.columns).filter(|c| **c != ts).map(|c| q(c)).collect();
        if !tags.is_empty() {
            lines.push(format!("    PRIMARY KEY ({})", tags.join(", ")));
        }
        let mut s = format!(
            "CREATE TABLE {}{name} (\n{}\n) ENGINE=mito",
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            lines.join(",\n")
        );
        let mut with: Vec<String> = t.comment.as_deref().filter(|s| !s.is_empty()).map(|cm| format!("comment = '{}'", cm.replace('\'', "''"))).into_iter().collect();
        // Table options (ttl, append_mode, compaction…).
        with.extend(greptime_options(&t).map(|(k, v)| format!("{} = '{}'", crate::structure::greptime_key(k), v.replace('\'', "''"))));
        if !with.is_empty() {
            s.push_str(&format!("\nWITH ({})", with.join(", ")));
        }
        s.push(';');
        out.push(s);
    }
    if parts.indexes {
        for ix in &t.indexes {
            out.push(greptime_index(t, ix, true)?);
        }
    }
    Ok(out.join("\n"))
}

/// GreptimeDB's table options (all but the designer's time index).
fn greptime_options(t: &TableSchema) -> impl Iterator<Item = (&String, &String)> {
    t.options.iter().filter(|(k, _)| k.as_str() != "time_index")
}

/// GreptimeDB's indexes are column settings: `SET` (with their `WITH`
/// options) or `UNSET` them.
fn greptime_index(t: &TableSchema, ix: &dbine_driver::IndexDef, set: bool) -> Result<String> {
    let kind = ix.kind.as_deref().unwrap_or("").to_ascii_uppercase();
    let [col] = ix.columns.as_slice() else {
        return Err(Error::Unsupported(format!("GreptimeDB indexa columnas de a una (índice «{}»)", ix.name)));
    };
    if !["INVERTED", "FULLTEXT", "SKIPPING"].contains(&kind.as_str()) {
        return Err(Error::Unsupported(format!(
            "GreptimeDB solo tiene índices INVERTED, FULLTEXT y SKIPPING (índice «{}»)",
            ix.name
        )));
    }
    let table = q(&t.name);
    if !set {
        return Ok(format!("ALTER TABLE {table} MODIFY COLUMN {} UNSET {kind} INDEX;", q(col)));
    }
    let with: Vec<String> = ix.options.iter().map(|(k, v)| format!("{k} = '{}'", v.replace('\'', "''"))).collect();
    let with = if with.is_empty() { String::new() } else { format!(" WITH({})", with.join(", ")) };
    Ok(format!("ALTER TABLE {table} MODIFY COLUMN {} SET {kind} INDEX{with};", q(col)))
}

/// `DROP INDEX a ON t; DROP INDEX b ON t` as one `ALTER TABLE t DROP
/// INDEX a, DROP INDEX b` (one schema change job instead of two).
fn merge_olap_drops(statements: &mut Vec<String>) {
    let mut drops: Vec<(String, Vec<String>)> = Vec::new();
    let mut first: Vec<usize> = Vec::new();
    let mut keep = Vec::new();
    for (i, s) in statements.iter().enumerate() {
        let parsed = s.strip_prefix("DROP INDEX ").and_then(|r| r.strip_suffix(';')).and_then(|r| r.split_once(" ON "));
        match parsed {
            Some((ix, table)) if !ix.contains('\n') => {
                match drops.iter_mut().find(|(t, _)| t == table) {
                    Some((_, list)) => list.push(ix.to_string()),
                    None => {
                        drops.push((table.to_string(), vec![ix.to_string()]));
                        first.push(i);
                    }
                }
                keep.push(first.contains(&i));
            }
            _ => keep.push(true),
        }
    }
    let mut out = Vec::new();
    for (i, s) in statements.drain(..).enumerate() {
        if !keep[i] {
            continue;
        }
        match first.iter().position(|&f| f == i) {
            Some(k) if drops[k].1.len() > 1 => {
                let (table, list) = &drops[k];
                out.push(format!("ALTER TABLE {table} {};", list.iter().map(|x| format!("DROP INDEX {x}")).collect::<Vec<_>>().join(", ")));
            }
            _ => out.push(s),
        }
    }
    *statements = out;
}

/// Manticore: attributes and full-text fields, nothing else (`id` is
/// implicit; DESCRIBE's `mva` types are spelled `multi` in CREATE).
fn manticore_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let name = q(&t.name);
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let cols: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                let ty = match c.data_type.to_ascii_lowercase().as_str() {
                    "mva" => "multi".to_string(),
                    "mva64" => "multi64".to_string(),
                    _ => c.data_type.clone(),
                };
                format!("    {} {ty}", q(&c.name))
            })
            .collect();
        let settings: String = t.options.iter().map(|(k, v)| format!(" {}", crate::structure::manticore_setting(k, v))).collect();
        out.push(format!(
            "CREATE TABLE {}{name} (\n{}\n){settings};",
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            cols.join(",\n")
        ));
    }
    out.join("\n")
}

pub(crate) fn insert_script(v: Variant, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> String {
    if v == Variant::Manticore {
        return manticore_inserts(&target.name, columns, rows);
    }
    // Backslashes are escapes in the default sql_mode (and in the
    // emulations); the builder doubles the quotes.
    let rows: Vec<Vec<Value>> = rows.iter().map(|r| r.iter().map(escape_backslashes).collect()).collect();
    ddl::insert_script(&flavor(v), target.schema(), &target.name, columns, &rows, 100)
}

/// `UPDATE … WHERE <key>` per changed row, with the same literals as
/// [`insert_script`]. GreptimeDB has no UPDATE (a row is replaced by
/// inserting it again, whole) and Manticore has no NULL.
pub(crate) fn update_script(v: Variant, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
    match v {
        Variant::GreptimeDb => Err(Error::Unsupported(
            "GreptimeDB no tiene UPDATE: una fila se reemplaza insertándola de nuevo completa".into(),
        )),
        Variant::Manticore => {
            if changes.iter().any(|c| c.set.iter().any(|(_, v)| v.is_null())) {
                return Err(Error::Unsupported("Manticore no admite NULL: no se puede asignar un valor nulo".into()));
            }
            Ok(ddl::update_script_with(Quote::Backtick, None, &target.name, changes, &manticore_value))
        }
        _ => {
            let f = flavor(v);
            Ok(ddl::update_script_with(Quote::Backtick, target.schema(), &target.name, changes, &|c| {
                ddl::sql_literal(&f, &escape_backslashes(c))
            }))
        }
    }
}

/// `DELETE … WHERE <key>` per row key, with the same literals as
/// [`update_script`]. Manticore has no NULL, so a null key part can't
/// match any document.
pub(crate) fn delete_script(v: Variant, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
    match v {
        Variant::Manticore => {
            if keys.iter().any(|k| k.iter().any(|(_, v)| v.is_null())) {
                return Err(Error::Unsupported("Manticore no admite NULL: no se puede borrar por una clave con valor nulo".into()));
            }
            Ok(ddl::delete_script_with(Quote::Backtick, None, &target.name, keys, &manticore_value))
        }
        _ => {
            let f = flavor(v);
            Ok(ddl::delete_script_with(Quote::Backtick, target.schema(), &target.name, keys, &|c| {
                ddl::sql_literal(&f, &escape_backslashes(c))
            }))
        }
    }
}

/// Manticore takes no NULL and no doubled quotes: one INSERT per row
/// with its non-null values, `\'` escapes, `(1,2)` for multi-values.
fn manticore_inserts(table: &str, columns: &[String], rows: &[Vec<Value>]) -> String {
    let value = manticore_value;
    rows.iter()
        .filter_map(|r| {
            let (cols, vals): (Vec<String>, Vec<String>) =
                columns.iter().zip(r).filter(|(_, v)| !v.is_null()).map(|(c, v)| (q(c), value(v))).unzip();
            (!cols.is_empty()).then(|| format!("INSERT INTO {} ({}) VALUES ({});", q(table), cols.join(", "), vals.join(", ")))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Backslashes are escapes in MySQL string literals: doubled.
fn escape_backslashes(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace('\\', "\\\\")),
        Value::Array(_) | Value::Object(_) => Value::String(v.to_string().replace('\\', "\\\\")),
        other => other.clone(),
    }
}

/// The browse query restricted by the grid's column filters. Literals as
/// in [`update_script`]; LIKE patterns rely on the default `\` escape (an
/// `ESCAPE '\'` clause isn't even a valid literal with backslash escapes).
/// Manticore has no LIKE on attributes: text matches go through REGEX().
pub(crate) fn filtered_browse(v: Variant, browse: &str, filters: &[ColumnFilter]) -> Result<String> {
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let f = flavor(v);
    let lit = |c: &Value| if v == Variant::Manticore { manticore_value(c) } else { ddl::sql_literal(&f, &escape_backslashes(c)) };
    let style = SqlFilterStyle { quote: Quote::Backtick, literal: &lit, like: "LIKE", true_literal: f.true_literal, false_literal: f.false_literal };
    let mut parts = Vec::new();
    for c in filters {
        let one = std::slice::from_ref(c);
        let text = |c: &ColumnFilter| c.values.first().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()));
        let like = matches!(c.op, FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith);
        if v == Variant::Manticore && like {
            let t = text(c).ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", c.column)))?;
            let re = regex_escape(&t);
            let re = match c.op {
                FilterOp::StartsWith => format!("(?i)^{re}"),
                FilterOp::EndsWith => format!("(?i){re}$"),
                FilterOp::Contains => format!("(?i){re}"),
                _ => return Err(Error::Unsupported("Manticore no filtra por «no contiene» en el servidor".into())),
            };
            parts.push(format!("REGEX({}, {})", q(&c.column), manticore_value(&Value::String(re))));
            continue;
        }
        let cond = sql_condition(one, &style)?;
        parts.push(match cond.strip_suffix(" ESCAPE '\\'") {
            Some(s) if like => s.to_string(),
            _ => cond,
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

/// Characters with a meaning in RE2, escaped.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if "\\.+*?()|[]{}^$".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// A Manticore literal: `\'` escapes, `1`/`0` booleans, `(1,2)` multi-values.
fn manticore_value(v: &Value) -> String {
    let text = |s: &str| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"));
    match v {
        Value::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => text(s),
        Value::Array(a) if a.iter().all(Value::is_number) => {
            format!("({})", a.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(","))
        }
        other => text(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, ForeignKeyDef, IndexDef, KeyDef};
    use serde_json::json;

    fn cf(column: &str, op: FilterOp, values: Vec<Value>) -> ColumnFilter {
        ColumnFilter { column: column.into(), op, values, sql: None }
    }

    #[test]
    fn filtered_browse_escapes_backslashes_and_likes() {
        let browse = "SELECT *\nFROM `ventas`.`clientes`\nLIMIT 200";
        let filters = [
            cf("nombre", FilterOp::Eq, vec![json!("O'Brien\\x")]),
            cf("nota", FilterOp::Contains, vec![json!("50%")]),
            cf("id", FilterOp::In, vec![json!(1), json!(2)]),
            cf("baja", FilterOp::IsNull, vec![]),
            cf("activo", FilterOp::IsTrue, vec![]),
        ];
        assert_eq!(
            filtered_browse(Variant::MySql, browse, &filters).unwrap(),
            "SELECT *\nFROM `ventas`.`clientes`\nWHERE `nombre` = 'O''Brien\\\\x'\n  AND `nota` LIKE '%50\\\\%%'\n  AND `id` IN (1, 2)\n  AND `baja` IS NULL\n  AND `activo` = 1\nLIMIT 200"
        );
        assert_eq!(filtered_browse(Variant::MySql, browse, &[]).unwrap(), browse);
        let m = filtered_browse(Variant::Manticore, "SELECT *\nFROM `idx`\nLIMIT 20", &[cf("title", FilterOp::StartsWith, vec![json!("a.b's")])]).unwrap();
        assert_eq!(m, "SELECT *\nFROM `idx`\nWHERE REGEX(`title`, '(?i)^a\\\\.b\\'s')\nLIMIT 20");
        assert!(filtered_browse(Variant::Manticore, browse, &[cf("t", FilterOp::NotContains, vec![json!("x")])]).is_err());
    }

    #[test]
    fn update_script_escapes_per_variant() {
        let c = RowChange {
            key: vec![("id".into(), json!(7)), ("region".into(), Value::Null)],
            set: vec![("nombre".into(), json!("O'Brien\\x")), ("baja".into(), Value::Null)], ..Default::default()
        };
        let t = ObjectRef { kind: "table".into(), schema: Some("ventas".into()), name: "clientes".into() };
        assert_eq!(
            update_script(Variant::MySql, &t, std::slice::from_ref(&c)).unwrap(),
            "UPDATE `ventas`.`clientes` SET `nombre` = 'O''Brien\\\\x', `baja` = NULL WHERE `id` = 7 AND `region` IS NULL;"
        );
        assert!(update_script(Variant::GreptimeDb, &t, std::slice::from_ref(&c)).is_err());
        assert!(update_script(Variant::Manticore, &t, std::slice::from_ref(&c)).is_err());
        let m = RowChange { key: vec![("id".into(), json!(7))], set: vec![("nombre".into(), json!("O'Brien"))], ..Default::default() };
        assert_eq!(
            update_script(Variant::Manticore, &t, &[m]).unwrap(),
            "UPDATE `clientes` SET `nombre` = 'O\\'Brien' WHERE `id` = 7;"
        );
    }

    #[test]
    fn delete_script_by_composite_key() {
        let keys = vec![vec![("nombre".into(), json!("O'Brien\\x")), ("region".into(), Value::Null)], vec![]];
        let t = ObjectRef { kind: "table".into(), schema: Some("ventas".into()), name: "clientes".into() };
        assert_eq!(
            delete_script(Variant::MySql, &t, &keys).unwrap(),
            "DELETE FROM `ventas`.`clientes` WHERE `nombre` = 'O''Brien\\\\x' AND `region` IS NULL;"
        );
        assert!(delete_script(Variant::Manticore, &t, &keys).is_err());
        let m = vec![vec![("id".into(), json!(7)), ("nombre".into(), json!("O'Brien"))]];
        assert_eq!(delete_script(Variant::Manticore, &t, &m).unwrap(), "DELETE FROM `clientes` WHERE `id` = 7 AND `nombre` = 'O\\'Brien';");
    }

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn pedidos() -> TableSchema {
        TableSchema {
            kind: "table".into(),
            name: "pedidos".into(),
            columns: vec![
                ColumnDef { nullable: false, auto_increment: true, comment: Some("clave \\ única".into()), ..col("id", "int") },
                ColumnDef { nullable: false, ..col("cliente_id", "int") },
                ColumnDef { default_value: Some("'nuevo'".into()), ..col("estado", "varchar(20)") },
                col("nota", "text"),
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            foreign_keys: vec![ForeignKeyDef {
                name: Some("fk_cli".into()),
                columns: vec!["cliente_id".into()],
                ref_table: "clientes".into(),
                ref_columns: vec!["id".into()],
                on_delete: Some("CASCADE".into()),
                ..Default::default()
            }],
            indexes: vec![
                IndexDef { name: "ix_estado".into(), columns: vec!["estado".into()], kind: Some("BTREE".into()), ..Default::default() },
                IndexDef { name: "ux_nota".into(), columns: vec!["nota(10)".into()], unique: true, ..Default::default() },
                IndexDef { name: "ft_nota".into(), columns: vec!["nota".into()], kind: Some("FULLTEXT".into()), ..Default::default() },
            ],
            comment: Some("Pedidos".into()),
            options: [("engine".to_string(), "InnoDB".to_string()), ("collation".into(), "utf8mb4_bin".into())].into(),
            ..Default::default()
        }
    }

    const ALL: DdlParts = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };

    #[test]
    fn mysql_table() {
        let s = table_ddl(Variant::MySql, &pedidos(), ALL).unwrap();
        assert!(s.starts_with("DROP TABLE IF EXISTS `pedidos`;\nCREATE TABLE `pedidos` (\n"), "{s}");
        assert!(s.contains("`id` int AUTO_INCREMENT NOT NULL COMMENT 'clave \\\\ única',"), "{s}");
        assert!(s.contains("`estado` varchar(20) DEFAULT 'nuevo' NULL,"));
        assert!(s.contains(") COMMENT='Pedidos' ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;"), "{s}");
        assert!(s.contains("CREATE INDEX `ix_estado` ON `pedidos` (`estado`);"));
        assert!(s.contains("CREATE UNIQUE INDEX `ux_nota` ON `pedidos` (`nota`(10));"));
        assert!(s.contains("CREATE FULLTEXT INDEX `ft_nota` ON `pedidos` (`nota`);"));
        assert!(s.ends_with(
            "ALTER TABLE `pedidos` ADD CONSTRAINT `fk_cli` FOREIGN KEY (`cliente_id`) REFERENCES `clientes` (`id`) ON DELETE CASCADE;"
        ));
        let only_fk = table_ddl(Variant::MySql, &pedidos(), DdlParts { foreign_keys: true, ..Default::default() }).unwrap();
        assert!(only_fk.starts_with("ALTER TABLE"), "{only_fk}");
        let m = table_ddl(Variant::MariaDb, &pedidos(), DdlParts { indexes: true, if_exists: true, ..Default::default() }).unwrap();
        assert!(m.contains("CREATE INDEX IF NOT EXISTS `ix_estado`"), "{m}");
    }

    #[test]
    fn singlestore_and_databend_skip_what_they_lack() {
        let s = table_ddl(Variant::SingleStore, &pedidos(), ALL).unwrap();
        assert!(!s.contains("FOREIGN KEY") && s.contains("PRIMARY KEY (`id`)"), "{s}");
        let d = table_ddl(Variant::Databend, &pedidos(), ALL).unwrap();
        assert!(!d.contains("PRIMARY KEY") && !d.contains("INDEX") && !d.contains("AUTO_INCREMENT"), "{d}");
        assert!(d.contains("`id` int NOT NULL COMMENT"), "{d}");
    }

    #[test]
    fn starrocks_and_doris_key_models() {
        let mut t = pedidos();
        t.indexes.clear();
        t.options.clear();
        let s = table_ddl(Variant::StarRocks, &t, DdlParts { create: true, ..Default::default() }).unwrap();
        assert!(s.contains("`estado` varchar(20) NULL DEFAULT 'nuevo',"), "{s}");
        assert!(s.contains("\n)\nPRIMARY KEY(`id`)\nCOMMENT 'Pedidos'\nDISTRIBUTED BY HASH(`id`);"), "{s}");
        let d = table_ddl(Variant::Doris, &t, DdlParts { create: true, ..Default::default() }).unwrap();
        assert!(d.contains("UNIQUE KEY(`id`)"), "{d}");

        t.primary_key = None;
        t.options = [
            ("key_columns".to_string(), "estado".to_string()),
            ("buckets".into(), "4".into()),
            ("replication_num".into(), "1".into()),
        ]
        .into();
        t.columns[3].default_value = Some("0".into());
        t.indexes = vec![IndexDef { name: "ix_cli".into(), columns: vec!["cliente_id".into()], ..Default::default() }];
        let s = table_ddl(Variant::StarRocks, &t, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
        assert!(s.contains("(\n    `estado` varchar(20) NULL DEFAULT 'nuevo',\n    `id` int NOT NULL"), "{s}");
        assert!(s.contains("`nota` text NULL DEFAULT '0'"), "{s}");
        assert!(s.contains("DUPLICATE KEY(`estado`)"));
        assert!(s.contains("DISTRIBUTED BY HASH(`estado`) BUCKETS 4\nPROPERTIES (\"replication_num\" = \"1\");"), "{s}");
        assert!(s.ends_with("CREATE INDEX `ix_cli` ON `pedidos` (`cliente_id`) USING BITMAP;"));
        let d = table_ddl(Variant::Doris, &t, DdlParts { indexes: true, ..Default::default() }).unwrap();
        assert_eq!(d, "CREATE INDEX `ix_cli` ON `pedidos` (`cliente_id`) USING INVERTED;");
        t.indexes[0].unique = true;
        assert!(table_ddl(Variant::Doris, &t, DdlParts { indexes: true, ..Default::default() }).is_err());
    }

    #[test]
    fn greptime_time_index() {
        let t = TableSchema {
            name: "cpu".into(),
            columns: vec![
                ColumnDef { nullable: false, default_value: Some("current_timestamp()".into()), ..col("ts", "timestamp(3)") },
                ColumnDef { comment: Some("máquina".into()), ..col("host", "string") },
                col("v", "double"),
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["host".into()] }),
            comment: Some("uso de 'cpu'".into()),
            ..Default::default()
        };
        let s = table_ddl(Variant::GreptimeDb, &t, ALL).unwrap();
        assert_eq!(
            s,
            "DROP TABLE IF EXISTS `cpu`;\nCREATE TABLE `cpu` (\n    `ts` timestamp(3) NOT NULL DEFAULT current_timestamp(),\n    `host` string NULL COMMENT 'máquina',\n    `v` double NULL,\n    TIME INDEX (`ts`),\n    PRIMARY KEY (`host`)\n) ENGINE=mito\nWITH (comment = 'uso de ''cpu''');"
        );
        let mut bad = t.clone();
        bad.columns.remove(0);
        assert!(table_ddl(Variant::GreptimeDb, &bad, ALL).is_err());
    }

    #[test]
    fn manticore_table_and_inserts() {
        let t = TableSchema {
            name: "docs".into(),
            columns: vec![col("id", "bigint"), col("title", "text"), col("tags", "mva")],
            ..Default::default()
        };
        let s = table_ddl(Variant::Manticore, &t, DdlParts { create: true, if_exists: true, ..Default::default() }).unwrap();
        assert_eq!(s, "CREATE TABLE IF NOT EXISTS `docs` (\n    `id` bigint,\n    `title` text,\n    `tags` multi\n);");
        let target = ObjectRef { kind: "table".into(), schema: None, name: "docs".into() };
        let rows = vec![vec![json!(1), json!("O'B \\ x"), json!([1, 2])], vec![json!(2), Value::Null, Value::Null]];
        let s = insert_script(Variant::Manticore, &target, &["id".into(), "title".into(), "tags".into()], &rows);
        assert_eq!(
            s,
            "INSERT INTO `docs` (`id`, `title`, `tags`) VALUES (1, 'O\\'B \\\\ x', (1,2));\nINSERT INTO `docs` (`id`) VALUES (2);"
        );
    }

    #[test]
    fn mysql_inserts_escape_backslashes() {
        let target = ObjectRef { kind: "table".into(), schema: None, name: "t".into() };
        let rows = vec![vec![json!("a\\b'c"), json!(true), Value::Null, json!({"k": "v"})]];
        let cols = ["a".into(), "b".into(), "c".into(), "d".into()];
        let s = insert_script(Variant::MySql, &target, &cols, &rows);
        assert_eq!(s, "INSERT INTO `t` (`a`, `b`, `c`, `d`) VALUES\n  ('a\\\\b''c', 1, NULL, '{\"k\":\"v\"}');");
        assert!(insert_script(Variant::Databend, &target, &cols, &rows).contains(", TRUE, "));
    }

    #[test]
    fn designer_and_templates_follow_the_engine() {
        assert!(designer(Variant::MySql).foreign_keys && designer(Variant::MySql).comments);
        assert!(!designer(Variant::SingleStore).foreign_keys);
        let m = designer(Variant::Manticore);
        assert!(!m.primary_key && !m.indexes && !m.defaults && !m.nullability);
        assert!(designer(Variant::Doris).table_options.iter().any(|f| f.key == "replication_num"));
        assert!(designer(Variant::GreptimeDb).table_options.iter().any(|f| f.key == "time_index"));
        assert!(!capabilities(Variant::Manticore).create_database && capabilities(Variant::Doris).drop_database);
        let t = templates(Variant::SingleStore, &[kinds::TABLE, kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION]);
        assert_eq!(t.len(), 3);
        assert!(t[1].template.starts_with("CREATE OR REPLACE PROCEDURE `{name}`(p_id INT) AS"));
    }

    fn ix(name: &str, cols: &[&str], kind: Option<&str>, options: &[(&str, &str)]) -> IndexDef {
        IndexDef {
            name: name.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            kind: kind.map(Into::into),
            options: options.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn mysql_index_options_and_checks() {
        let mut t = pedidos();
        t.indexes = vec![
            ix("ix_fn", &["(lower(`estado`))", "nota(10)"], Some("BTREE"), &[("desc", "nota(10)"), ("INVISIBLE", "YES"), ("COMMENT", "it's")]),
            ix("ft", &["nota"], Some("FULLTEXT"), &[("WITH PARSER", "ngram")]),
            ix("ig", &["estado"], None, &[("IGNORED", "YES")]),
        ];
        t.checks = vec![
            dbine_driver::CheckDef { name: Some("ck_a".into()), expression: "(`id` > 0)".into() },
            dbine_driver::CheckDef { name: Some("ck_b".into()), expression: "(`estado` <> 'x') NOT ENFORCED".into() },
        ];
        let s = table_ddl(Variant::MySql, &t, ALL).unwrap();
        assert!(s.contains("CONSTRAINT `ck_a` CHECK (`id` > 0),\n    CONSTRAINT `ck_b` CHECK (`estado` <> 'x') NOT ENFORCED\n)"), "{s}");
        assert!(s.contains("CREATE INDEX `ix_fn` ON `pedidos` ((lower(`estado`)), `nota`(10) DESC) COMMENT 'it''s' INVISIBLE;"), "{s}");
        assert!(s.contains("CREATE FULLTEXT INDEX `ft` ON `pedidos` (`nota`) WITH PARSER `ngram`;"), "{s}");
        assert!(s.contains("CREATE INDEX `ig` ON `pedidos` (`estado`) IGNORED;"), "{s}");
        t.options.insert("engine".into(), "MEMORY".into());
        t.indexes = vec![ix("b", &["id"], Some("BTREE"), &[]), ix("h", &["id"], Some("HASH"), &[])];
        let s = table_ddl(Variant::MySql, &t, DdlParts { indexes: true, ..Default::default() }).unwrap();
        assert_eq!(s, "CREATE INDEX `b` ON `pedidos` (`id`) USING BTREE;\nCREATE INDEX `h` ON `pedidos` (`id`) USING HASH;");

        // A CHECK that turns unenforced is dropped and added back, after its condition.
        let mut new = pedidos();
        new.checks = vec![t.checks[1].clone()];
        let mut old = new.clone();
        old.checks[0].expression = "(`estado` <> 'x')".into();
        let s = sync_script(Variant::MySql, &[dbine_driver::TableChange::Alter { old, new }]).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `pedidos` DROP CONSTRAINT `ck_b`;", "ALTER TABLE `pedidos` ADD CONSTRAINT `ck_b` CHECK (`estado` <> 'x') NOT ENFORCED;"]);
    }

    #[test]
    fn mariadb_column_checks_go_last() {
        let mut t = pedidos();
        t.columns[2].data_type = "varchar(20) CHECK (`estado` <> '')".into();
        let s = table_ddl(Variant::MariaDb, &t, DdlParts { create: true, ..Default::default() }).unwrap();
        assert!(s.contains("    `estado` varchar(20) DEFAULT 'nuevo' NULL CHECK (`estado` <> ''),\n"), "{s}");
        let mut old = t.clone();
        old.columns[2].data_type = "varchar(20)".into();
        let s = sync_script(Variant::MariaDb, &[dbine_driver::TableChange::Alter { old, new: t }]).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `pedidos` MODIFY COLUMN `estado` varchar(20) DEFAULT 'nuevo' NULL CHECK (`estado` <> '');"]);
    }

    #[test]
    fn table_comments_sync_with_alter_table() {
        let old = TableSchema { comment: None, ..pedidos() };
        let mut new = old.clone();
        new.comment = Some("it's \\ new".into());
        let sync = |v, old: &TableSchema, new: &TableSchema| sync_script(v, &[dbine_driver::TableChange::Alter { old: old.clone(), new: new.clone() }]).unwrap().statements;
        assert_eq!(sync(Variant::MySql, &old, &new), ["ALTER TABLE `pedidos` COMMENT = 'it''s \\\\ new';"]);
        assert_eq!(sync(Variant::MySql, &new, &old), ["ALTER TABLE `pedidos` COMMENT = '';"]);
        assert_eq!(sync(Variant::StarRocks, &old, &new), ["ALTER TABLE `pedidos` COMMENT = 'it''s \\\\ new';"]);
        assert_eq!(sync(Variant::Doris, &new, &old), ["ALTER TABLE `pedidos` MODIFY COMMENT '';"]);
        assert_eq!(sync(Variant::GreptimeDb, &new, &old), ["COMMENT ON TABLE `pedidos` IS NULL;"]);
        assert!(sync(Variant::Manticore, &old, &new).is_empty());
        // A column's comment goes with MODIFY COLUMN (GreptimeDB: COMMENT ON).
        let mut new = old.clone();
        new.columns[2].comment = Some("estado".into());
        let s = sync(Variant::MySql, &old, &new);
        assert!(s.len() == 1 && s[0].starts_with("ALTER TABLE `pedidos` MODIFY COLUMN `estado`") && s[0].ends_with(" COMMENT 'estado';"), "{s:?}");
        assert_eq!(sync(Variant::GreptimeDb, &old, &new), ["COMMENT ON COLUMN `pedidos`.`estado` IS 'estado';"]);
    }

    #[test]
    fn tidb_clustered_primary_key() {
        let mut t = pedidos();
        t.options = [("clustered_index".to_string(), "NONCLUSTERED".to_string())].into();
        t.checks = vec![dbine_driver::CheckDef { name: Some("ck".into()), expression: "(`id` > 0)".into() }];
        let s = table_ddl(Variant::TiDb, &t, DdlParts { create: true, ..Default::default() }).unwrap();
        assert!(s.contains("    PRIMARY KEY (`id`) NONCLUSTERED,\n    CONSTRAINT `ck` CHECK (`id` > 0)\n)"), "{s}");
        let mut old = t.clone();
        old.primary_key = None;
        let s = sync_script(Variant::TiDb, &[dbine_driver::TableChange::Alter { old, new: t.clone() }]).unwrap();
        assert!(s.statements.iter().any(|s| s == "ALTER TABLE `pedidos` ADD PRIMARY KEY (`id`) NONCLUSTERED;"), "{:?}", s.statements);
        let mut old = t.clone();
        old.options.insert("clustered_index".into(), "CLUSTERED".into());
        let s = sync_script(Variant::TiDb, &[dbine_driver::TableChange::Alter { old, new: t }]).unwrap();
        assert!(s.warnings.iter().any(|w| w.contains("CLUSTERED")), "{:?}", s.warnings);
    }

    #[test]
    fn olap_index_properties_and_bloom_filters() {
        let mut t = pedidos();
        t.options = [("bloom_filter_columns".to_string(), "cliente_id, nota".to_string()), ("replication_num".into(), "1".into())].into();
        t.indexes = vec![
            ix("bm", &["estado"], Some("BITMAP"), &[("COMMENT", "estado")]),
            ix("ng", &["nota"], Some("NGRAMBF"), &[("gram_num", "4"), ("bloom_filter_fpp", "0.05")]),
        ];
        let s = table_ddl(Variant::StarRocks, &t, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
        assert!(s.contains("PROPERTIES (\"replication_num\" = \"1\", \"bloom_filter_columns\" = \"cliente_id, nota\");"), "{s}");
        assert!(s.ends_with("ALTER TABLE `pedidos` ADD INDEX `bm` (`estado`) USING BITMAP COMMENT 'estado', ADD INDEX `ng` (`nota`) USING NGRAMBF (\"bloom_filter_fpp\" = \"0.05\", \"gram_num\" = \"4\");"), "{s}");
        let d = table_ddl(Variant::Doris, &t, DdlParts { indexes: true, ..Default::default() }).unwrap();
        assert!(d.contains("USING NGRAMBF PROPERTIES(\"bloom_filter_fpp\" = \"0.05\", \"gram_num\" = \"4\");"), "{d}");

        let mut two = t.clone();
        two.indexes[0].options.clear();
        let old = TableSchema { indexes: vec![], ..two.clone() };
        let s = sync_script(Variant::StarRocks, &[dbine_driver::TableChange::Alter { old: two.clone(), new: old }]).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `pedidos` DROP INDEX `bm`, DROP INDEX `ng`;"]);

        let mut old = t.clone();
        old.options.remove("bloom_filter_columns");
        old.indexes[1].options.insert("gram_num".into(), "3".into());
        let s = sync_script(Variant::StarRocks, &[dbine_driver::TableChange::Alter { old, new: t }]).unwrap();
        assert_eq!(
            s.statements,
            [
                "DROP INDEX `ng` ON `pedidos`;",
                "CREATE INDEX `ng` ON `pedidos` (`nota`) USING NGRAMBF (\"bloom_filter_fpp\" = \"0.05\", \"gram_num\" = \"4\");",
                "ALTER TABLE `pedidos` SET (\"bloom_filter_columns\" = \"cliente_id, nota\");"
            ]
        );
    }

    #[test]
    fn greptime_column_indexes() {
        let mut t = TableSchema {
            name: "cpu".into(),
            columns: vec![ColumnDef { nullable: false, ..col("ts", "timestamp(3)") }, col("host", "string"), col("msg", "string")],
            indexes: vec![
                ix("INVERTED_INDEX_host", &["host"], Some("INVERTED"), &[]),
                ix("FULLTEXT_INDEX_msg", &["msg"], Some("FULLTEXT"), &[("analyzer", "English")]),
            ],
            ..Default::default()
        };
        let s = table_ddl(Variant::GreptimeDb, &t, DdlParts { indexes: true, ..Default::default() }).unwrap();
        assert_eq!(
            s,
            "ALTER TABLE `cpu` MODIFY COLUMN `host` SET INVERTED INDEX;\nALTER TABLE `cpu` MODIFY COLUMN `msg` SET FULLTEXT INDEX WITH(analyzer = 'English');"
        );
        let old = t.clone();
        t.indexes.remove(0);
        t.indexes[0].options.insert("granularity".into(), "1024".into());
        let s = sync_script(Variant::GreptimeDb, &[dbine_driver::TableChange::Alter { old: old.clone(), new: t.clone() }]).unwrap();
        assert_eq!(
            s.statements,
            [
                "ALTER TABLE `cpu` MODIFY COLUMN `host` UNSET INVERTED INDEX;",
                "ALTER TABLE `cpu` MODIFY COLUMN `msg` UNSET FULLTEXT INDEX;",
                "ALTER TABLE `cpu` MODIFY COLUMN `msg` SET FULLTEXT INDEX WITH(analyzer = 'English', granularity = '1024');"
            ]
        );
        // The analyzer stays with the column: left as it is, with a warning.
        let mut other = old.clone();
        other.indexes[1].options.insert("analyzer".into(), "Chinese".into());
        let s = sync_script(Variant::GreptimeDb, &[dbine_driver::TableChange::Alter { old, new: other }]).unwrap();
        assert!(s.statements.is_empty(), "{:?}", s.statements);
        assert!(s.warnings.iter().any(|w| w.contains("analizador")));
        t.indexes[0].kind = Some("BTREE".into());
        assert!(table_ddl(Variant::GreptimeDb, &t, DdlParts { indexes: true, ..Default::default() }).is_err());
    }

    #[test]
    fn singlestore_keys_and_fulltext() {
        let mut t = pedidos();
        t.foreign_keys.clear();
        t.options = [("shard_key".to_string(), "`id`".to_string()), ("sort_key".into(), "`estado` DESC".into()), ("table_type".into(), "ROWSTORE".into())].into();
        t.indexes = vec![ix("ft", &["nota"], Some("FULLTEXT"), &[("VERSION", "2")]), ix("h", &["estado"], Some("HASH"), &[])];
        let s = table_ddl(Variant::SingleStore, &t, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
        assert!(s.starts_with("CREATE ROWSTORE TABLE `pedidos` ("), "{s}");
        assert!(s.contains("    PRIMARY KEY (`id`),\n    SHARD KEY (`id`),\n    SORT KEY (`estado` DESC)\n)"), "{s}");
        assert!(s.contains("ALTER TABLE `pedidos` ADD FULLTEXT USING VERSION 2 `ft` (`nota`);"), "{s}");
        assert!(s.contains("CREATE INDEX `h` ON `pedidos` (`estado`) USING HASH;"), "{s}");
        let mut old = t.clone();
        old.options.insert("shard_key".into(), "`cliente_id`".into());
        let s = sync_script(Variant::SingleStore, &[dbine_driver::TableChange::Alter { old, new: t }]).unwrap();
        assert!(s.warnings.iter().any(|w| w.contains("SHARD KEY")), "{:?}", s.warnings);
    }

    #[test]
    fn databend_index_ddl() {
        let mut t = pedidos();
        t.indexes = vec![
            ix("inv", &["nota"], Some("INVERTED"), &[("tokenizer", "english")]),
            ix("agg", &[], Some("AGGREGATING"), &[("query", "SELECT MAX(id) FROM pedidos")]),
        ];
        let s = table_ddl(Variant::Databend, &t, DdlParts { indexes: true, ..Default::default() }).unwrap();
        assert_eq!(s, "CREATE INVERTED INDEX `inv` ON `pedidos` (`nota`) tokenizer='english';\nCREATE AGGREGATING INDEX `agg` AS SELECT MAX(id) FROM pedidos;");
        let mut new = t.clone();
        new.indexes[0].options.insert("tokenizer".into(), "chinese".into());
        new.indexes.remove(1);
        let s = sync_script(Variant::Databend, &[dbine_driver::TableChange::Alter { old: t, new }]).unwrap();
        assert_eq!(
            s.statements,
            [
                "DROP INVERTED INDEX `inv` ON `pedidos`;",
                "DROP AGGREGATING INDEX `agg`;",
                "CREATE INVERTED INDEX `inv` ON `pedidos` (`nota`) tokenizer='chinese';"
            ]
        );
    }

    #[test]
    fn olap_rollups() {
        let mut t = pedidos();
        t.indexes = vec![ix("r1", &["estado", "id"], Some("ROLLUP"), &[]), ix("r2", &["nota"], Some("ROLLUP"), &[])];
        let s = table_ddl(Variant::StarRocks, &t, DdlParts { indexes: true, ..Default::default() }).unwrap();
        assert_eq!(s, "ALTER TABLE `pedidos` ADD ROLLUP `r1` (`estado`, `id`), `r2` (`nota`);");
        let mut new = t.clone();
        new.indexes.remove(0);
        let s = sync_script(Variant::StarRocks, &[dbine_driver::TableChange::Alter { old: t, new }]).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `pedidos` DROP ROLLUP `r1`;"]);
    }

    #[test]
    fn greptime_table_options() {
        let mut t = TableSchema { name: "w".into(), columns: vec![ColumnDef { nullable: false, ..col("ts", "timestamp(3)") }], ..Default::default() };
        t.options = [("ttl".to_string(), "7days".to_string()), ("compaction.type".into(), "twcs".into()), ("time_index".into(), "ts".into())].into();
        t.comment = Some("c".into());
        let s = table_ddl(Variant::GreptimeDb, &t, DdlParts { create: true, ..Default::default() }).unwrap();
        assert!(s.ends_with(") ENGINE=mito\nWITH (comment = 'c', 'compaction.type' = 'twcs', ttl = '7days');"), "{s}");
        let mut old = t.clone();
        old.options.insert("ttl".into(), "1day".into());
        old.options.insert("append_mode".into(), "true".into());
        let s = sync_script(Variant::GreptimeDb, &[dbine_driver::TableChange::Alter { old, new: t }]).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `w` UNSET 'append_mode';", "ALTER TABLE `w` SET 'ttl'='7days';"]);
    }

    #[test]
    fn manticore_settings() {
        let mut t = TableSchema { name: "docs".into(), columns: vec![col("title", "text")], ..Default::default() };
        t.options = [("morphology".to_string(), "stem_en".to_string()), ("min_infix_len".into(), "3".into())].into();
        let s = table_ddl(Variant::Manticore, &t, DdlParts { create: true, ..Default::default() }).unwrap();
        assert_eq!(s, "CREATE TABLE `docs` (\n    `title` text\n) min_infix_len='3' morphology='stem_en';");
        let mut old = t.clone();
        old.options.remove("morphology");
        old.options.insert("min_infix_len".into(), "2".into());
        let s = sync_script(Variant::Manticore, &[dbine_driver::TableChange::Alter { old, new: t }]).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `docs` min_infix_len='3' morphology='stem_en';"]);
    }
}
