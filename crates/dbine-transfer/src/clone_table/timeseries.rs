//! The clone on time series and wide-column engines, where the generic
//! path (structure → `CREATE` → rows) would lose something without saying:
//!
//! - TDengine: its `SHOW CREATE TABLE` carries what the structure doesn't
//!   (a composite key, each column's encoding, compression and level): the
//!   clone is created from it, under the new name, and checked against it.
//!   Supertables and subtables are refused ([`NOT_CLONEABLE_ON`]).
//! - IoTDB: a series' alias, tags and attributes and view series aren't
//!   in the structure: refused when there are any. The new device has to be
//!   one plain node, with nothing under its path (its drop is
//!   `DELETE TIMESERIES <device>.**`).
//! - InfluxDB 1: a measurement exists while it has points: no `CREATE`,
//!   the rows make it and `DROP MEASUREMENT` undoes it; its tags and fields
//!   are compared after the copy. Points kept under a retention policy
//!   other than the default are refused, and so is a name that is both a
//!   tag and a field (no point can carry both). InfluxDB 2 and 3 can't drop a
//!   measurement from their query language: refused.
//! - Cassandra / ScyllaDB: counter tables are refused (a counter only takes
//!   increments, and a retried increment would count twice); table names
//!   are letters, digits and `_`; each row's TTL and writetime can't be
//!   copied, and the report says so.

use super::{column_differences, exec, scalar, CloneOptions, Ddl};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, ColumnInfo, DdlParts, Driver, DriverInfo, Error, Language, ObjectRef, QueryOutcome, Result, Session, StatementResult, TableSchema};

/// Kinds one engine can't clone faithfully: (driver id, kind, why). The
/// explorer hides "Clonar…" for them (`NOT_CLONEABLE_ON` in
/// `ExplorerSidebar.vue`).
pub const NOT_CLONEABLE_ON: &[(&str, &str, &str)] = &[
    (
        "tdengine",
        "supertable",
        "las filas de una supertabla viven en sus subtablas, cada una con su nombre (tbname) y sus tags: el clon necesitaría subtablas con otros nombres y sus filas no quedarían iguales",
    ),
    (
        "tdengine",
        "subtable",
        "una subtabla es parte de su supertabla: el clon sería otra subtabla de la misma supertabla y sus filas aparecerían dos veces en las consultas sobre ella",
    ),
    (
        "influxdb",
        kinds::MEASUREMENT,
        "Flux no puede borrar un measurement, así que si la copia fallara a medias DBine no podría deshacer el clon",
    ),
    (
        "influxdb3",
        kinds::MEASUREMENT,
        "el SQL de InfluxDB 3 no puede borrar una tabla, así que si la copia fallara a medias DBine no podría deshacer el clon",
    ),
];

/// Why `kind` can't be cloned on the engine `driver_id`, if it can't.
pub fn refused(driver_id: &str, kind: &str) -> Option<&'static str> {
    NOT_CLONEABLE_ON.iter().find(|(d, k, _)| *d == driver_id && *k == kind).map(|(_, _, r)| *r)
}

/// Why the engine would refuse `name` for a new table, or read it as
/// something else. In Spanish.
pub fn name_problem(info: &DriverInfo, name: &str) -> Option<String> {
    let plain = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if info.language == Language::Cql && !plain(name) {
        return Some(format!(
            "el nombre «{name}» no sirve en {}: el nombre de una tabla solo admite letras sin tildes ni eñes, números y guion bajo (_)",
            info.name
        ));
    }
    if info.dialect == "iotdb" {
        // A dot is a level of the path: `a.b` would be a device inside `a`.
        // Anything else would need backquotes the device path doesn't get.
        let reserved = matches!(name.to_ascii_lowercase().as_str(), "time" | "timestamp" | "root");
        if !plain(name) || name.chars().all(|c| c.is_ascii_digit()) || reserved {
            return Some(format!(
                "el nombre «{name}» no sirve para un dispositivo de IoTDB: solo letras sin tildes ni eñes, números y guion bajo (_), sin ser solo números ni time, timestamp o root (un punto pondría el clon dentro de otro dispositivo)"
            ));
        }
    }
    None
}

/// The last result set of a statement (at most `max_rows` rows).
async fn rows_of(s: &mut dyn Session, sql: &str, max_rows: usize) -> Result<StatementResult> {
    let mut out = QueryOutcome::default();
    s.execute(sql, max_rows, &mut out).await?;
    if let Some(e) = out.error {
        return Err(Error::Query(e));
    }
    Ok(out.results.into_iter().rev().find(|r| !r.columns.is_empty()).unwrap_or_default())
}

/// An InfluxQL identifier, double-quoted.
fn influxql_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `DROP MEASUREMENT "name"`: a measurement exists while it has points,
/// and dropping it is the only way to remove one.
pub fn influxql_drop(name: &str) -> String {
    format!("DROP MEASUREMENT {}", influxql_ident(name))
}

/// An IoTDB device's full path, as its driver writes it (from the
/// `DELETE TIMESERIES <device>.**` of its drop).
fn iotdb_device(driver: &dyn Driver, t: &TableSchema) -> Result<String> {
    let drop = driver.table_ddl(t, DdlParts { drop: true, ..Default::default() })?;
    drop.trim()
        .strip_prefix("DELETE TIMESERIES ")
        .and_then(|s| s.trim_end_matches(';').strip_suffix(".**"))
        .map(str::to_string)
        .ok_or_else(|| Error::State(format!("ruta de dispositivo inesperada: {drop}")))
}

/// TDengine's `SHOW CREATE TABLE` without its head (``CREATE TABLE `name` ``):
/// the columns, with the composite key and each column's encoding,
/// compression and level, and the table's options.
pub fn tdengine_body(definition: &str, name: &str) -> Option<String> {
    let head = format!("CREATE TABLE {} ", quote_ident(Quote::Backtick, name));
    definition.trim().strip_prefix(&head).map(|b| b.trim().trim_end_matches(';').trim_end().to_string())
}

/// Parts the structure leaves out that the clone would lose, or a table
/// the engine can't copy faithfully.
pub async fn check(driver: &dyn Driver, s: &mut dyn Session, t: &TableSchema) -> Result<()> {
    let info = driver.info();
    if info.language == Language::Cql {
        let counters: Vec<&str> = t.columns.iter().filter(|c| c.data_type.trim().eq_ignore_ascii_case("counter")).map(|c| c.name.as_str()).collect();
        if !counters.is_empty() {
            return Err(Error::Unsupported(format!(
                "no se puede clonar: la tabla tiene columnas counter ({}), y {} solo las cambia sumando (UPDATE … SET c = c + n), nunca con INSERT; un reintento de esa suma durante la copia duplicaría el valor sin avisar",
                counters.join(", "),
                info.name
            )));
        }
    }
    match info.dialect {
        "iotdb" => iotdb_check(driver, s, t).await,
        "influxql" => influxql_check(s, t).await,
        _ => Ok(()),
    }
}

/// IoTDB: what a series has besides its type, encoding and compression
/// (alias, tags, attributes), and view series. DBine's device structure
/// doesn't carry them.
async fn iotdb_check(driver: &dyn Driver, s: &mut dyn Session, t: &TableSchema) -> Result<()> {
    let device = iotdb_device(driver, t)?;
    let r = rows_of(s, &format!("SHOW TIMESERIES {device}.*"), 100_000).await?;
    let lost = iotdb_lost(&r, &device);
    if lost.is_empty() {
        return Ok(());
    }
    Err(Error::Unsupported(format!(
        "no se puede clonar: DBine todavía no copia el alias, los tags, los atributos ni las vistas de las series de IoTDB, y el clon los perdería: {}",
        lost.join("; ")
    )))
}

/// The series of a `SHOW TIMESERIES` answer with something the clone
/// would lose, as `name (alias, tags…)`.
fn iotdb_lost(r: &StatementResult, device: &str) -> Vec<String> {
    use serde_json::Value;
    let col = |n: &str| r.columns.iter().position(|c| c.name.eq_ignore_ascii_case(n));
    let (ts, view) = (col("Timeseries"), col("ViewType"));
    let parts = [("alias", col("Alias")), ("tags", col("Tags")), ("atributos", col("Attributes"))];
    let set = |row: &[Value], i: Option<usize>| {
        i.and_then(|i| row.get(i)).is_some_and(|v| match v {
            Value::Null => false,
            Value::String(s) => !matches!(s.trim(), "" | "null" | "{}"),
            Value::Object(o) => !o.is_empty(),
            _ => true,
        })
    };
    let prefix = format!("{device}.");
    let mut lost = Vec::new();
    for row in &r.rows {
        let full = ts.and_then(|i| row.get(i)).and_then(|v| v.as_str()).unwrap_or("?");
        let name = full.strip_prefix(&prefix).unwrap_or(full);
        let mut what: Vec<&str> = parts.iter().filter(|(_, i)| set(row, *i)).map(|(w, _)| *w).collect();
        if view.and_then(|i| row.get(i)).and_then(|v| v.as_str()) == Some("VIEW") {
            what.push("es una vista");
        }
        if !what.is_empty() {
            lost.push(format!("{name} ({})", what.join(", ")));
        }
    }
    lost
}

/// InfluxDB 1: names that are both a tag and a field of the measurement.
/// InfluxDB allows it (a `SELECT` answers `host` and `host_1`), but a
/// point can't be written with both, so the clone couldn't hold them.
fn influxql_tag_field_overlap(t: &TableSchema) -> Vec<&str> {
    let is_tag = |c: &dbine_driver::ColumnDef| c.data_type.eq_ignore_ascii_case("tag");
    let mut both: Vec<&str> = t
        .columns
        .iter()
        .filter(|c| is_tag(c))
        .map(|c| c.name.as_str())
        .filter(|n| t.columns.iter().any(|c| !is_tag(c) && c.name != "time" && c.name == *n))
        .collect();
    both.sort_unstable();
    both.dedup();
    both
}

/// InfluxDB 1: fields that `SHOW FIELD KEYS` lists more than once, one
/// per type (a field's type is fixed per shard, not per measurement), as
/// `«v»: float, integer`.
fn influxql_mixed_fields(r: &StatementResult) -> Vec<String> {
    let col = |n: &str| r.columns.iter().position(|c| c.name.eq_ignore_ascii_case(n));
    let (Some(key), ty) = (col("fieldKey"), col("fieldType")) else { return Vec::new() };
    let mut seen: Vec<(String, Vec<String>)> = Vec::new();
    for row in &r.rows {
        let Some(name) = row.get(key).and_then(|v| v.as_str()) else { continue };
        let t = ty.and_then(|i| row.get(i)).and_then(|v| v.as_str()).unwrap_or("?").to_string();
        match seen.iter_mut().find(|(n, _)| n == name) {
            Some((_, types)) => types.push(t),
            None => seen.push((name.to_string(), vec![t])),
        }
    }
    seen.into_iter().filter(|(_, types)| types.len() > 1).map(|(n, types)| format!("«{n}»: {}", types.join(", "))).collect()
}

/// InfluxDB 1: a tag and a field with the same name can't be written
/// together; the copy reads the default retention policy, and points kept
/// under another one would be left out.
async fn influxql_check(s: &mut dyn Session, t: &TableSchema) -> Result<()> {
    let both = influxql_tag_field_overlap(t);
    if !both.is_empty() {
        let list = both.iter().map(|n| format!("«{n}»")).collect::<Vec<_>>().join(", ");
        return Err(Error::Unsupported(format!(
            "no se puede clonar: en «{}», {list} {} a la vez tag y field; InfluxDB lo permite en un measurement que ya existe, pero no deja escribir un punto con un tag y un field del mismo nombre, así que el clon no podría tener los mismos datos",
            t.name,
            if both.len() == 1 { "es" } else { "son" }
        )));
    }
    let fields = rows_of(s, &format!("SHOW FIELD KEYS FROM {}", influxql_ident(&t.name)), 100_000).await?;
    let mixed = influxql_mixed_fields(&fields);
    if !mixed.is_empty() {
        return Err(Error::Unsupported(format!(
            "no se puede clonar: en «{}», {} tipos distintos en distintos shards ({}); InfluxDB 1 lo permite porque fija el tipo de un field por shard, pero DBine lee cada field con un solo tipo, así que el clon no tendría los mismos valores",
            t.name,
            if mixed.len() == 1 { "un field tiene" } else { "hay fields con" },
            mixed.join("; ")
        )));
    }
    let r = rows_of(s, "SHOW RETENTION POLICIES", 1_000).await?;
    let col = |n: &str| r.columns.iter().position(|c| c.name.eq_ignore_ascii_case(n));
    let (Some(name), Some(default)) = (col("name"), col("default")) else {
        return Err(Error::State("InfluxDB no informó sus políticas de retención".into()));
    };
    for row in &r.rows {
        if row.get(default).and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let Some(rp) = row.get(name).and_then(|v| v.as_str()) else { continue };
        let found = rows_of(s, &format!("SELECT * FROM {}.{} LIMIT 1", influxql_ident(rp), influxql_ident(&t.name)), 1).await?;
        if !found.rows.is_empty() {
            return Err(Error::Unsupported(format!(
                "no se puede clonar: «{}» tiene puntos en la política de retención «{rp}», que no es la predeterminada; el clon copia solo la predeterminada y esos puntos se perderían",
                t.name
            )));
        }
    }
    Ok(())
}

/// The DDL where the engine makes the table with its first row (InfluxDB
/// 1): no `CREATE` (empty), `DROP MEASUREMENT` to undo it.
pub fn implicit_ddl(driver: &dyn Driver, t: &TableSchema) -> Option<Ddl> {
    (driver.info().dialect == "influxql").then(|| Ddl { create: String::new(), indexes: None, foreign_keys: None, drop: influxql_drop(&t.name) })
}

/// What [`prepare`] decided, for the steps after the `CREATE`.
#[derive(Default)]
pub struct Native {
    /// The table comes with its first row (InfluxDB 1): its columns are
    /// compared after the copy, not after the (absent) `CREATE`.
    pub implicit: bool,
    /// TDengine: the original's definition without its head, which the
    /// clone's has to match.
    pub tdengine_body: Option<String>,
}

/// Before anything is written: the clone's `CREATE` where the engine's own
/// is the exact one (TDengine), the notes about what can't be copied
/// (Cassandra's TTL and writetime), and a new name whose path holds other
/// devices (IoTDB).
#[allow(clippy::too_many_arguments)]
pub async fn prepare(
    driver: &dyn Driver,
    src: &mut dyn Session,
    source: &ObjectRef,
    original: &TableSchema,
    clone: &TableSchema,
    options: &CloneOptions,
    ddl: &mut Ddl,
    notes: &mut Vec<String>,
) -> Result<Native> {
    let info = driver.info();
    let mut native = Native { implicit: info.dialect == "influxql", ..Default::default() };
    if native.implicit && !options.with_data {
        return Err(Error::Unsupported("no se puede clonar sin los datos: en InfluxDB un measurement existe solo mientras tiene puntos".into()));
    }
    if info.id == "tdengine" && original.kind == kinds::TABLE {
        let def = src.definition(source).await?.unwrap_or_default();
        let body = tdengine_body(&def, &original.name)
            .ok_or_else(|| Error::Unsupported("no se puede clonar: TDengine no devolvió la definición de la tabla (SHOW CREATE TABLE)".into()))?;
        let name = dbine_driver::sql::qualified_name(Quote::Backtick, clone.schema.as_deref().filter(|s| !s.is_empty()), &clone.name);
        ddl.create = format!("CREATE TABLE {name} {body};");
        native.tdengine_body = Some(body);
    }
    if info.dialect == "iotdb" {
        // Series under the new path that belong to other devices
        // (`<name>.x`): the clone would hold them, and its drop
        // (`DELETE TIMESERIES <name>.**`) would take them.
        let device = iotdb_device(driver, clone)?;
        if scalar(src, &format!("COUNT TIMESERIES {device}.**")).await?.unwrap_or(0) > 0 {
            return Err(Error::State(format!("ya hay series bajo {device} (de otro dispositivo); elegí otro nombre")));
        }
    }
    if options.with_data && info.language == Language::Cql {
        notes.push(cql_ttl_note(info, original));
    }
    Ok(native)
}

/// What a CQL clone can't keep of each row.
fn cql_ttl_note(info: &DriverInfo, t: &TableSchema) -> String {
    match t.options.get("default_time_to_live").map(|v| v.trim()).filter(|v| !v.is_empty() && *v != "0") {
        Some(s) => format!(
            "{} no deja copiar el vencimiento (TTL) ni la hora de escritura (writetime) de cada fila: en el clon todas las filas vencen a los {s} s de la copia (el TTL por defecto de la tabla), aunque en el original les quedara más o menos tiempo, y su hora de escritura es la de la copia",
            info.name
        ),
        None => format!(
            "{} no deja copiar el vencimiento (TTL) ni la hora de escritura (writetime) de cada fila: en el clon ninguna fila vence, aunque en el original tuviera TTL, y su hora de escritura es la de la copia",
            info.name
        ),
    }
}

/// Run the clone's `CREATE`, if it has one.
pub async fn create(s: &mut dyn Session, ddl: &Ddl) -> Result<()> {
    if ddl.create.trim().is_empty() {
        return Ok(());
    }
    exec(s, &ddl.create).await
}

/// After the `CREATE`: TDengine's clone has the original's definition.
pub async fn after_create(tgt: &mut dyn Session, native: &Native, clone: &ObjectRef) -> Result<()> {
    if let Some(body) = &native.tdengine_body {
        let def = tgt.definition(clone).await?.unwrap_or_default();
        if tdengine_body(&def, &clone.name).as_deref() != Some(body.as_str()) {
            return Err(Error::State(format!("el clon no quedó igual al original (TDengine lo creó así: {def}); no se clona")));
        }
    }
    Ok(())
}

/// After the rows, where they made the table (InfluxDB 1): it has points,
/// and the original's tags and fields with their types.
pub async fn after_copy(tgt: &mut dyn Session, native: &Native, source: &ObjectRef, clone: &ObjectRef, source_cols: &[ColumnInfo], rows: u64) -> Result<()> {
    if !native.implicit {
        return Ok(());
    }
    if rows == 0 {
        return Err(Error::State(format!("«{}» no tiene puntos en la política de retención predeterminada: no hay nada que clonar", source.name)));
    }
    let diffs = column_differences(source_cols, &tgt.columns(clone).await?);
    if !diffs.is_empty() {
        return Err(Error::State(format!("el clon no quedó igual al original ({}); no se clona", diffs.join("; "))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{Family, ResultColumn};
    use serde_json::json;

    fn info(id: &'static str, dialect: &'static str, language: Language) -> DriverInfo {
        DriverInfo {
            id,
            name: id,
            family: Family::TimeSeries,
            language,
            dialect,
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: false,
            object_kinds: vec![],
        }
    }

    #[test]
    fn refused_kinds() {
        assert!(refused("tdengine", "supertable").is_some());
        assert!(refused("tdengine", "subtable").is_some());
        assert!(refused("tdengine", "table").is_none());
        assert!(refused("influxdb", "measurement").is_some());
        assert!(refused("influxdb3", "measurement").is_some());
        assert!(refused("influxdb1", "measurement").is_none());
    }

    #[test]
    fn names_the_engine_would_refuse_or_misread() {
        let cass = info("cassandra", "", Language::Cql);
        assert!(name_problem(&cass, "t_20260930_070509").is_none());
        for bad in ["ñandú", "src clone", "a-b", "a.b"] {
            let e = name_problem(&cass, bad).unwrap();
            assert!(e.contains(bad) && e.contains("guion bajo"), "{e}");
        }
        let iot = info("iotdb", "iotdb", Language::Sql);
        assert!(name_problem(&iot, "dn_2026").is_none());
        for bad in ["dn.sub", "px inner", "123", "time", "Root", "ñ"] {
            assert!(name_problem(&iot, bad).is_some(), "{bad}");
        }
        assert!(name_problem(&info("postgres", "postgres", Language::Sql), "a.b ñ").is_none());
    }

    #[test]
    fn tdengine_definitions() {
        let def = "CREATE TABLE `pk2` (`ts` TIMESTAMP ENCODE 'delta-i' COMPRESS 'lz4' LEVEL 'medium', `id` INT ENCODE 'simple8b' COMPRESS 'lz4' LEVEL 'medium' COMPOSITE KEY, `v` DOUBLE ENCODE 'delta-d' COMPRESS 'zstd' LEVEL 'high') COMMENT 'c' TTL 3;";
        let body = tdengine_body(def, "pk2").unwrap();
        assert!(body.starts_with("(`ts` TIMESTAMP") && body.contains("COMPOSITE KEY") && body.ends_with("TTL 3"), "{body}");
        assert_eq!(tdengine_body(&def.replace("`pk2`", "`pk2_1`"), "pk2_1").unwrap(), body);
        assert!(tdengine_body(def, "pk").is_none());
        assert!(tdengine_body("CREATE STABLE `pk2` (`ts` TIMESTAMP) TAGS (`t` INT)", "pk2").is_none());
        assert_eq!(tdengine_body("CREATE TABLE `a``b` (x INT)", "a`b").as_deref(), Some("(x INT)"));
    }

    #[test]
    fn influxql_quoting() {
        assert_eq!(influxql_drop(r#"m "x" \ y"#), r#"DROP MEASUREMENT "m \"x\" \\ y""#);
    }

    #[test]
    fn iotdb_series_with_more_than_a_type() {
        let cols = ["Timeseries", "Alias", "Database", "DataType", "Encoding", "Compression", "Tags", "Attributes", "Deadband", "DeadbandParameters", "ViewType"];
        let r = StatementResult {
            columns: cols.iter().map(|c| ResultColumn { name: c.to_string(), type_name: String::new() }).collect(),
            rows: vec![
                vec![json!("root.db.dn.s1"), json!(null), json!("root.db"), json!("INT32"), json!("TS_2DIFF"), json!("LZ4"), json!(null), json!(null), json!(null), json!(null), json!("BASE")],
                vec![json!("root.db.dn.s2"), json!("alias2"), json!("root.db"), json!("INT32"), json!("TS_2DIFF"), json!("LZ4"), json!("{\"owner\":\"x\"}"), json!("{\"desc\":\"d\"}"), json!(null), json!(null), json!("BASE")],
                vec![json!("root.db.dn.v"), json!(null), json!("root.db"), json!("INT32"), json!(null), json!(null), json!(null), json!(null), json!(null), json!(null), json!("VIEW")],
            ],
            ..Default::default()
        };
        assert_eq!(iotdb_lost(&r, "root.db.dn"), vec!["s2 (alias, tags, atributos)".to_string(), "v (es una vista)".to_string()]);
    }

    #[test]
    fn cql_notes_say_what_the_rows_lose() {
        let cass = info("cassandra", "", Language::Cql);
        let mut t = TableSchema::default();
        assert!(cql_ttl_note(&cass, &t).contains("ninguna fila vence"));
        t.options.insert("default_time_to_live".into(), "86400".into());
        let n = cql_ttl_note(&cass, &t);
        assert!(n.contains("86400 s") && n.contains("writetime") && n.contains("aunque en el original les quedara más o menos tiempo"), "{n}");
    }

    #[test]
    fn influxql_field_with_two_types() {
        let col = |n: &str| ResultColumn { name: n.into(), type_name: String::new() };
        let r = StatementResult {
            columns: vec![col("fieldKey"), col("fieldType")],
            rows: vec![vec![json!("v"), json!("float")], vec![json!("w"), json!("string")], vec![json!("v"), json!("integer")]],
            ..Default::default()
        };
        assert_eq!(influxql_mixed_fields(&r), vec!["«v»: float, integer"]);
        let one = StatementResult { rows: vec![vec![json!("v"), json!("float")]], ..r };
        assert!(influxql_mixed_fields(&one).is_empty());
    }

    #[test]
    fn influxql_tag_and_field_with_one_name() {
        use dbine_driver::ColumnDef;
        let col = |name: &str, data_type: &str| ColumnDef { name: name.into(), data_type: data_type.into(), ..Default::default() };
        let mut t = TableSchema { name: "dup".into(), columns: vec![col("time", "time"), col("host", "tag"), col("region", "tag"), col("v", "integer")], ..Default::default() };
        assert!(influxql_tag_field_overlap(&t).is_empty());
        t.columns.push(col("host", "string"));
        t.columns.push(col("region", "float"));
        assert_eq!(influxql_tag_field_overlap(&t), vec!["host", "region"]);
    }
}
