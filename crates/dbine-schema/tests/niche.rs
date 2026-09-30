//! Conversions to and from the analytical, search and time-series engines
//! (StarRocks / Doris, Databend, GreptimeDB, Manticore, Phoenix / Avatica,
//! Dremio / Drill, TDengine, IoTDB, InfluxDB, ksqlDB), end to end with
//! tables spelled the way each driver reports them, and against real
//! servers (ignored tests, see `live_*`).

mod common;

use dbine_driver::{ColumnDef, DdlParts, IndexDef, KeyDef, ObjectRef, Session, TableSchema};
use dbine_schema::{convert, Conversion, IssueCode, Options, Severity};
use serde_json::{json, Value};

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
}

fn not_null(mut c: ColumnDef) -> ColumnDef {
    c.nullable = false;
    c
}

fn with_default(mut c: ColumnDef, d: &str) -> ColumnDef {
    c.default_value = Some(d.into());
    c
}

fn table(name: &str, cols: Vec<ColumnDef>, pk: &[&str]) -> TableSchema {
    TableSchema {
        kind: "table".into(),
        name: name.into(),
        columns: cols,
        primary_key: (!pk.is_empty()).then(|| KeyDef { name: None, columns: pk.iter().map(|s| s.to_string()).collect() }),
        ..Default::default()
    }
}

fn ty<'a>(t: &'a TableSchema, c: &str) -> &'a str {
    &t.columns.iter().find(|x| x.name == c).unwrap_or_else(|| panic!("no column {c} in {t:?}")).data_type
}

fn column<'a>(t: &'a TableSchema, c: &str) -> &'a ColumnDef {
    t.columns.iter().find(|x| x.name == c).unwrap_or_else(|| panic!("no column {c}"))
}

fn has(r: &Conversion, code: IssueCode, object: &str) -> bool {
    r.issues.iter().any(|i| i.code == code && i.object.as_deref() == Some(object))
}

/// A PostgreSQL table as `database_schema` reports it.
fn pg_medidas() -> TableSchema {
    let mut t = table(
        "medidas",
        vec![
            not_null(col("id", "bigint")),
            with_default(not_null(col("sensor", "character varying(40)")), "'s1'::character varying"),
            col("valor", "numeric(12,4)"),
            col("ratio", "double precision"),
            with_default(col("activo", "boolean"), "true"),
            with_default(col("creado", "timestamp with time zone"), "now()"),
            col("dia", "date"),
            col("datos", "jsonb"),
            col("etiquetas", "text[]"),
            col("notas", "text"),
            col("codigo", "uuid"),
            col("chico", "smallint"),
            col("bytes", "bytea"),
        ],
        &["id"],
    );
    t.schema = Some("public".into());
    t.indexes.push(IndexDef { name: "ix_medidas_sensor".into(), columns: vec!["sensor".into()], ..Default::default() });
    t.indexes.push(IndexDef { name: "ux_medidas_codigo".into(), columns: vec!["codigo".into()], unique: true, ..Default::default() });
    t.indexes.push(IndexDef { name: "ix_medidas_dos".into(), columns: vec!["sensor".into(), "dia".into()], ..Default::default() });
    t
}

#[test]
fn postgres_to_starrocks() {
    let r = convert(&[pg_medidas()], "postgres", "starrocks", &Options::default()).unwrap();
    let t = &r.tables[0];
    assert_eq!(ty(t, "id"), "BIGINT");
    assert_eq!(ty(t, "sensor"), "VARCHAR(160)");
    assert_eq!(column(t, "sensor").default_value.as_deref(), Some("'s1'"));
    assert_eq!(ty(t, "valor"), "DECIMAL(12, 4)");
    assert_eq!(ty(t, "activo"), "BOOLEAN");
    assert_eq!(column(t, "activo").default_value.as_deref(), Some("1"));
    assert_eq!(ty(t, "creado"), "DATETIME");
    assert_eq!(column(t, "creado").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert!(has(&r, IssueCode::TimeZoneLoss, "creado"));
    assert_eq!(ty(t, "datos"), "JSON");
    assert_eq!(ty(t, "etiquetas"), "ARRAY<VARCHAR(1048576)>");
    assert_eq!(ty(t, "codigo"), "VARCHAR(36)");
    assert_eq!(ty(t, "bytes"), "VARBINARY");
    // Key model from the primary key, in the designer's options.
    assert_eq!(t.options.get("key_model").map(String::as_str), Some("primary"));
    assert_eq!(t.options.get("key_columns").map(String::as_str), Some("id"));
    assert_eq!(t.options.get("distributed_by").map(String::as_str), Some("id"));
    assert_eq!(t.options.get("replication_num").map(String::as_str), Some("1"));
    assert!(has(&r, IssueCode::OptionAdded, "key_model"));
    // Only the one-column, non-unique index survives.
    assert_eq!(t.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["ix_medidas_sensor"]);
    assert!(has(&r, IssueCode::IndexDropped, "ux_medidas_codigo") && has(&r, IssueCode::IndexDropped, "ix_medidas_dos"));
}

#[test]
fn mysql_to_doris_and_decimal_keys() {
    let t = table(
        "ventas",
        vec![not_null(col("nro", "decimal(10,0)")), col("monto", "decimal(12,2)"), col("alta", "datetime(3)"), col("texto", "longtext")],
        &["nro"],
    );
    let r = convert(&[t.clone()], "mysql", "doris", &Options::default()).unwrap();
    let d = &r.tables[0];
    assert_eq!(d.options.get("key_model").map(String::as_str), Some("unique"));
    assert_eq!(ty(d, "alta"), "DATETIME(3)");
    assert_eq!(ty(d, "texto"), "STRING");
    // StarRocks' PRIMARY KEY doesn't take decimals: UNIQUE KEY.
    let r = convert(&[t], "mysql", "starrocks", &Options::default()).unwrap();
    assert_eq!(r.tables[0].options.get("key_model").map(String::as_str), Some("unique"));
    // A key on a float can't be kept.
    let f = table("f", vec![not_null(col("x", "double")), col("y", "int")], &["x"]);
    let r = convert(&[f], "mysql", "starrocks", &Options::default()).unwrap();
    assert_eq!(r.tables[0].options.get("key_model").map(String::as_str), Some("duplicate"));
    assert_eq!(r.tables[0].options.get("key_columns").map(String::as_str), Some("y"));
    assert!(r.issues.iter().any(|i| i.code == IssueCode::PrimaryKeyDropped && i.severity == Severity::Loss));
}

#[test]
fn starrocks_to_postgres() {
    let t = table(
        "sr",
        vec![
            not_null(col("id", "bigint(20)")),
            col("b", "tinyint(1)"),
            col("li", "bigint(20) unsigned"),
            col("s", "varchar(65533)"),
            col("v", "varchar(20)"),
            col("ts", "datetime"),
            col("a", "array<int(11)>"),
            col("m", "map<varchar(10),int(11)>"),
            col("j", "json"),
        ],
        &["id"],
    );
    let r = convert(&[t], "starrocks", "postgres", &Options::default()).unwrap();
    let p = &r.tables[0];
    assert_eq!(ty(p, "b"), "boolean");
    assert_eq!(ty(p, "li"), "numeric(39, 0)");
    assert_eq!(ty(p, "s"), "text");
    assert_eq!(ty(p, "v"), "varchar(20)");
    assert_eq!(ty(p, "ts"), "timestamp(6)");
    assert_eq!(ty(p, "a"), "integer[]");
    assert_eq!(ty(p, "m"), "jsonb");
    assert_eq!(ty(p, "j"), "jsonb");
}

#[test]
fn postgres_to_databend() {
    let r = convert(&[pg_medidas()], "postgres", "databend", &Options::default()).unwrap();
    let t = &r.tables[0];
    assert_eq!(t.name, "medidas");
    assert!(t.primary_key.is_none());
    assert!(t.indexes.is_empty());
    assert_eq!(ty(t, "sensor"), "VARCHAR");
    assert_eq!(ty(t, "creado"), "TIMESTAMP");
    assert_eq!(column(t, "creado").default_value.as_deref(), Some("now()"));
    assert_eq!(column(t, "codigo").default_value, None);
    assert_eq!(ty(t, "datos"), "VARIANT");
    assert_eq!(ty(t, "etiquetas"), "ARRAY(VARCHAR)");
    assert!(r.issues.iter().any(|i| i.code == IssueCode::PrimaryKeyDropped));
    // Back to SQL Server.
    let back = convert(&r.tables, "databend", "sqlserver", &Options::default()).unwrap();
    assert_eq!(ty(&back.tables[0], "creado"), "datetimeoffset(6)");
}

#[test]
fn sqlserver_to_greptimedb_picks_the_time_index() {
    let t = table(
        "Lecturas",
        vec![
            not_null(col("Id", "int")),
            col("Equipo", "nvarchar(50)"),
            col("Fecha", "datetime2(7)"),
            col("Valor", "decimal(18,4)"),
            col("Hora", "time(7)"),
        ],
        &["Id"],
    );
    let r = convert(&[t], "sqlserver", "greptimedb", &Options::default()).unwrap();
    let g = &r.tables[0];
    assert_eq!(g.options.get("time_index").map(String::as_str), Some("Fecha"));
    assert_eq!(ty(g, "Fecha"), "TIMESTAMP(9)");
    assert!(!column(g, "Fecha").nullable);
    assert_eq!(ty(g, "Hora"), "STRING");
    assert!(has(&r, IssueCode::TypeApproximated, "Hora"));
    // Without a timestamp: greptime_timestamp is added.
    let r = convert(&[table("t", vec![col("a", "int")], &[])], "sqlserver", "greptimedb", &Options::default()).unwrap();
    assert_eq!(r.tables[0].options.get("time_index").map(String::as_str), Some("greptime_timestamp"));
    assert_eq!(ty(&r.tables[0], "greptime_timestamp"), "TIMESTAMP(3)");
}

#[test]
fn greptimedb_to_mysql() {
    let mut t = table("cpu", vec![not_null(col("ts", "timestamp(3)")), col("host", "string"), col("v", "double"), col("n", "bigint unsigned")], &["host"]);
    t.options.insert("time_index".into(), "ts".into());
    let r = convert(&[t], "greptimedb", "mysql", &Options::default()).unwrap();
    let m = &r.tables[0];
    assert_eq!(ty(m, "ts"), "datetime(3)");
    assert!(has(&r, IssueCode::TimeZoneLoss, "ts"));
    // A key column: MySQL needs a bounded type.
    assert!(ty(m, "host").starts_with("varchar"), "{}", ty(m, "host"));
    assert_eq!(ty(m, "n"), "bigint unsigned");
    assert!(r.issues.iter().any(|i| i.code == IssueCode::OptionDropped && i.object.as_deref() == Some("time_index")));
}

#[test]
fn postgres_to_manticore() {
    let r = convert(&[pg_medidas()], "postgres", "manticore", &Options::default()).unwrap();
    let t = &r.tables[0];
    assert_eq!(ty(t, "id"), "bigint");
    assert_eq!(ty(t, "sensor"), "string");
    assert_eq!(ty(t, "notas"), "text");
    assert_eq!(ty(t, "valor"), "float");
    assert!(has(&r, IssueCode::PrecisionLoss, "valor"));
    assert_eq!(ty(t, "creado"), "timestamp");
    assert!(has(&r, IssueCode::RangeLoss, "creado"));
    assert_eq!(ty(t, "chico"), "bigint");
    assert!(t.columns.iter().all(|c| c.nullable && c.default_value.is_none()));
    assert!(t.primary_key.is_none() && t.indexes.is_empty());
    let m = table("docs", vec![col("id", "bigint"), col("title", "text"), col("tags", "mva"), col("precio", "float")], &[]);
    let back = convert(&[m], "manticore", "mysql", &Options::default()).unwrap();
    assert_eq!(ty(&back.tables[0], "tags"), "json");
    assert_eq!(ty(&back.tables[0], "precio"), "float");
}

#[test]
fn postgres_to_phoenix() {
    let mut t = pg_medidas();
    t.columns.push(not_null(col("obligatorio", "integer")));
    let r = convert(&[t, table("sin_clave", vec![col("a", "integer")], &[])], "postgres", "phoenix", &Options::default()).unwrap();
    let p = &r.tables[0];
    assert_eq!(p.name, "MEDIDAS");
    assert_eq!(ty(p, "ID"), "BIGINT");
    assert_eq!(ty(p, "SENSOR"), "VARCHAR(40)");
    assert_eq!(ty(p, "ETIQUETAS"), "VARCHAR ARRAY");
    assert_eq!(ty(p, "CREADO"), "TIMESTAMP");
    // Phoenix only takes constant defaults.
    assert_eq!(column(p, "CREADO").default_value, None);
    assert!(has(&r, IssueCode::DefaultDropped, "creado"));
    assert_eq!(ty(p, "CODIGO"), "CHAR(36)");
    // NOT NULL only on the key; unique index kept as a plain one.
    assert!(column(p, "OBLIGATORIO").nullable);
    assert!(has(&r, IssueCode::NullabilityChanged, "OBLIGATORIO"));
    assert!(p.indexes.iter().any(|i| i.name == "UX_MEDIDAS_CODIGO" && !i.unique));
    // A table without a key gets ROW_ID.
    let s = &r.tables[1];
    assert_eq!(s.primary_key.as_ref().unwrap().columns, ["ROW_ID"]);
    assert!(has(&r, IssueCode::PrimaryKeyAdded, "ROW_ID"));
}

#[test]
fn phoenix_to_postgres() {
    let t = table(
        "T",
        vec![not_null(col("ID", "BIGINT")), col("D", "DATE"), col("U", "UNSIGNED_INT"), col("TAGS", "VARCHAR ARRAY"), col("N", "DECIMAL")],
        &["ID"],
    );
    let r = convert(&[t], "phoenix", "postgres", &Options::default()).unwrap();
    let p = &r.tables[0];
    assert_eq!(p.name, "t");
    assert_eq!(ty(p, "d"), "timestamp(3)");
    assert_eq!(ty(p, "u"), "integer");
    assert_eq!(ty(p, "tags"), "text[]");
    assert_eq!(ty(p, "n"), "numeric");
}

#[test]
fn postgres_to_dremio_and_drill() {
    let r = convert(&[pg_medidas()], "postgres", "dremio", &Options::default()).unwrap();
    let t = &r.tables[0];
    assert_eq!(ty(t, "sensor"), "VARCHAR");
    assert_eq!(ty(t, "etiquetas"), "LIST<VARCHAR>");
    assert_eq!(ty(t, "creado"), "TIMESTAMP");
    assert!(t.primary_key.is_none());
    assert!(t.columns.iter().all(|c| c.default_value.is_none()));
    // Drill: source only.
    assert!(matches!(convert(&[pg_medidas()], "postgres", "drill", &Options::default()), Err(dbine_schema::Error::SourceOnly { .. })));
    let d = table("x", vec![col("a", "CHARACTER VARYING(10)"), col("b", "DECIMAL(10,2)"), col("c", "TIMESTAMP"), col("d", "MAP")], &[]);
    let back = convert(&[d], "drill", "postgres", &Options::default()).unwrap();
    assert_eq!(ty(&back.tables[0], "a"), "varchar(10)");
    assert_eq!(ty(&back.tables[0], "c"), "timestamp(3)");
    assert_eq!(ty(&back.tables[0], "d"), "json");
}

#[test]
fn postgres_to_tdengine() {
    let r = convert(&[pg_medidas()], "postgres", "tdengine", &Options::default()).unwrap();
    let t = &r.tables[0];
    // The timestamp moves first; the key and the indexes go.
    assert_eq!(t.columns[0].name, "creado");
    assert_eq!(ty(t, "creado"), "TIMESTAMP");
    assert!(t.primary_key.is_none() && t.indexes.is_empty());
    assert!(has(&r, IssueCode::PrimaryKeyAdded, "creado"));
    assert_eq!(ty(t, "sensor"), "NCHAR(40)");
    assert_eq!(ty(t, "notas"), "NCHAR(4096)");
    assert!(has(&r, IssueCode::LengthLoss, "notas"));
    assert_eq!(ty(t, "dia"), "TIMESTAMP");
    assert_eq!(ty(t, "datos"), "NCHAR(4096)");
    assert!(t.columns.iter().all(|c| c.default_value.is_none()));
    // A date (also TIMESTAMP in TDengine) isn't taken for the time axis.
    let d = table("d", vec![col("id", "integer"), col("dia", "date"), col("hora", "timestamp without time zone")], &["id"]);
    let r = convert(&[d], "postgres", "tdengine", &Options::default()).unwrap();
    assert_eq!(r.tables[0].columns[0].name, "hora");
    let td = table("d1", vec![col("ts", "TIMESTAMP"), col("v", "FLOAT"), col("u", "INT UNSIGNED"), col("loc", "NCHAR(20)")], &[]);
    let back = convert(&[td], "tdengine", "mysql", &Options::default()).unwrap();
    assert_eq!(ty(&back.tables[0], "ts"), "datetime(6)");
    assert_eq!(ty(&back.tables[0], "u"), "int unsigned");
    assert_eq!(ty(&back.tables[0], "loc"), "varchar(20)");
}

#[test]
fn postgres_to_iotdb() {
    let r = convert(&[pg_medidas()], "postgres", "iotdb", &Options::default()).unwrap();
    let t = &r.tables[0];
    assert_eq!(t.columns[0].name, "Time");
    assert!(has(&r, IssueCode::IdentifierRenamed, "creado"));
    assert_eq!(ty(t, "id"), "INT64");
    assert_eq!(ty(t, "chico"), "INT32");
    assert_eq!(ty(t, "valor"), "DOUBLE");
    assert_eq!(ty(t, "sensor"), "TEXT");
    assert_eq!(ty(t, "bytes"), "BLOB");
    assert_eq!(t.options.get("aligned").map(String::as_str), Some("true"));
    let d = table("d1", vec![not_null(col("Time", "TIMESTAMP")), col("temp", "FLOAT"), col("ok", "BOOLEAN"), col("n", "TEXT")], &["Time"]);
    let back = convert(&[d], "timechodb", "postgres", &Options::default()).unwrap();
    assert_eq!(ty(&back.tables[0], "Time"), "timestamp(3) with time zone");
    assert_eq!(ty(&back.tables[0], "temp"), "real");
}

#[test]
fn postgres_to_ksqldb() {
    let r = convert(&[pg_medidas()], "postgres", "ksqldb", &Options::default()).unwrap();
    let t = &r.tables[0];
    assert_eq!(t.name, "MEDIDAS");
    assert_eq!(t.options.get("object").map(String::as_str), Some("TABLE"));
    assert_eq!(ty(t, "ID"), "BIGINT");
    assert_eq!(ty(t, "SENSOR"), "STRING");
    assert_eq!(ty(t, "VALOR"), "DECIMAL(12, 4)");
    assert_eq!(ty(t, "CREADO"), "TIMESTAMP");
    assert_eq!(ty(t, "ETIQUETAS"), "ARRAY<STRING>");
    assert_eq!(ty(t, "BYTES"), "BYTES");
    let k = table("S", vec![col("K", "STRING"), col("V", "MAP<STRING, INTEGER>"), col("T", "TIMESTAMP")], &[]);
    let back = convert(&[k], "ksqldb", "sqlserver", &Options::default()).unwrap();
    assert_eq!(ty(&back.tables[0], "K"), "nvarchar(max)");
    assert_eq!(ty(&back.tables[0], "T"), "datetime2(3)");
}

#[test]
fn source_only_targets_refuse() {
    for to in ["influxdb1", "influxdb", "influxdb3", "drill", "avatica"] {
        let r = convert(&[pg_medidas()], "postgres", to, &Options::default());
        assert!(matches!(r, Err(dbine_schema::Error::SourceOnly { .. })), "{to}");
    }
    let m = table("cpu", vec![not_null(col("time", "time")), not_null(col("host", "tag")), col("usage", "float"), col("n", "integer"), col("u", "unsigned")], &["time", "host"]);
    let r = convert(&[m], "influxdb1", "postgres", &Options::default()).unwrap();
    let p = &r.tables[0];
    assert_eq!(ty(p, "time"), "timestamp(6) with time zone");
    assert!(has(&r, IssueCode::PrecisionLoss, "time"));
    assert_eq!(ty(p, "host"), "text");
    assert_eq!(ty(p, "u"), "numeric(39, 0)");
}

// ---------------------------------------------------------------------------
// Against real servers.
//
// docker start dbine-test-postgres dbine-test-mysql (see the drivers' tests)
// DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres
// DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011/dbine_nx
// DBINE_TEST_STARROCKS_URL=mysql://root@localhost:25030/dbine
// DBINE_TEST_GREPTIMEDB_URL=mysql://localhost:25017/public
// DBINE_TEST_MANTICORE_URL=mysql://localhost:25016
// DBINE_TEST_TDENGINE_URL=http://root:taosdata@localhost:25641/dbine
// DBINE_TEST_IOTDB_URL=http://root:root@localhost:25405/root.dbine
// DBINE_TEST_KSQLDB_URL=http://localhost:25188
// DBINE_TEST_PHOENIX_URL=http://localhost:25165
// DBINE_TEST_DREMIO_URL=http://dbine:secreto123@localhost:25947/$scratch
// cargo test -p dbine-schema --test niche -- --ignored --test-threads=1
// ---------------------------------------------------------------------------

/// One trip: tables created on the source, converted, created on the target
/// and read back.
struct Trip {
    conversion: Conversion,
    back: Vec<TableSchema>,
}

async fn open(driver: &str, env: &str) -> Option<Box<dyn Session>> {
    common::session(driver, env).await
}

async fn exec_all(s: &mut Box<dyn Session>, statements: &[&str]) {
    for st in statements {
        common::exec(s, st).await;
    }
}

/// Read `names` from `src`, convert to `dst_driver`, put them in `schema`
/// (the target's database or schema), drop and create them on `dst`, and
/// read them back.
async fn trip(
    src: &mut Box<dyn Session>,
    src_driver: &str,
    dst: &mut Box<dyn Session>,
    dst_driver: &str,
    names: &[&str],
    schema: Option<&str>,
    rename: impl Fn(&str) -> String,
) -> Trip {
    let tables = common::read(src, names).await;
    assert_eq!(tables.len(), names.len(), "{src_driver}: no se leyeron {names:?}");
    let mut conversion = convert(&tables, src_driver, dst_driver, &Options::default()).expect("convert");
    for t in &mut conversion.tables {
        t.schema = schema.map(str::to_string);
        let name = rename(&t.name);
        if name != t.name {
            // A copy next to the original: its constraint names would clash.
            if let Some(k) = &mut t.primary_key {
                k.name = None;
            }
            t.name = name;
        }
    }
    let d = dbine_drivers::find(dst_driver).unwrap();
    for t in conversion.tables.iter().rev() {
        let drop = d.table_ddl(t, DdlParts { drop: true, if_exists: true, ..Default::default() }).unwrap_or_default();
        common::exec_quiet(dst, &[drop.as_str()]).await;
    }
    for script in common::target_ddl(dst_driver, &conversion.tables) {
        for st in split(&script) {
            common::exec(dst, &st).await;
        }
    }
    let target_names: Vec<&str> = conversion.tables.iter().map(|t| t.name.as_str()).collect();
    let back = common::read(dst, &target_names).await;
    assert_eq!(back.len(), target_names.len(), "{dst_driver}: no se releyeron {target_names:?}");
    Trip { conversion, back }
}

/// Statements of a script, one per `;` at a line end (DDL has no `;` in literals here).
fn split(script: &str) -> Vec<String> {
    script.split(";\n").map(|s| s.trim().trim_end_matches(';').trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Insert one row through the driver's own INSERT script.
async fn insert(s: &mut Box<dyn Session>, driver: &str, t: &TableSchema, row: &[(&str, Value)]) {
    let d = dbine_drivers::find(driver).unwrap();
    let target = ObjectRef { kind: t.kind.clone(), schema: t.schema.clone(), name: t.name.clone() };
    let cols: Vec<String> = row.iter().map(|(c, _)| c.to_string()).collect();
    let vals: Vec<Value> = row.iter().map(|(_, v)| v.clone()).collect();
    let script = d.insert_script(&target, &cols, &[vals]).expect("insert_script");
    for st in split(&script) {
        common::exec(s, &st).await;
    }
}

/// Rows in a table, by the driver's own browse query.
async fn count(s: &mut Box<dyn Session>, t: &TableSchema) -> usize {
    let obj = ObjectRef { kind: t.kind.clone(), schema: t.schema.clone(), name: t.name.clone() };
    let sql = s.browse_query(&obj, 1000);
    let mut out = dbine_driver::QueryOutcome::default();
    s.execute(&sql, 1000, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
    out.results.last().map_or(0, |r| r.rows.len())
}

fn back_type(t: &TableSchema, c: &str) -> String {
    common::col(t, c).data_type.to_ascii_lowercase()
}

/// The PostgreSQL source every live test starts from: a rich table and a
/// row with limit values.
const PG_TABLE: &str = "CREATE TABLE nx_tipos (
    id bigint PRIMARY KEY,
    chico smallint,
    entero integer NOT NULL DEFAULT 0,
    grande bigint,
    precio numeric(12,2) DEFAULT 0,
    ratio double precision,
    activo boolean DEFAULT true,
    nombre varchar(40) DEFAULT 'sin nombre',
    notas text,
    alta date,
    creado timestamp(3) with time zone DEFAULT now(),
    datos jsonb,
    uid uuid
)";

const PG_ROW: &str = "INSERT INTO nx_tipos VALUES (9223372036854775807, 32767, -2147483648, -9223372036854775808,
    9999999999.99, 1.5e300, false, 'ñandú — 日本 😀', repeat('x', 3000), '2099-12-31',
    '2024-02-29 23:59:59.123+03', '{\"a\": [1, 2]}', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11')";

async fn pg_source() -> Option<Box<dyn Session>> {
    let mut pg = open("postgres", "DBINE_TEST_POSTGRES_URL").await?;
    common::exec_quiet(&mut pg, &["DROP TABLE IF EXISTS nx_tipos_vuelta", "DROP TABLE IF EXISTS nx_tipos"]).await;
    exec_all(&mut pg, &[PG_TABLE, PG_ROW]).await;
    Some(pg)
}

/// Back to PostgreSQL as `nx_tipos_vuelta`; returns the table read back.
async fn back_to_pg(pg: &mut Box<dyn Session>, dst: &mut Box<dyn Session>, dst_driver: &str, name: &str) -> (Conversion, TableSchema) {
    let t = trip(dst, dst_driver, pg, "postgres", &[name], None, |_| "nx_tipos_vuelta".into()).await;
    (t.conversion, t.back.into_iter().next().unwrap())
}

#[tokio::test]
#[ignore]
async fn live_postgres_starrocks_postgres() {
    let Some(mut pg) = pg_source().await else { return };
    let Some(mut sr) = open("starrocks", "DBINE_TEST_STARROCKS_URL").await else { return };
    let t = trip(&mut pg, "postgres", &mut sr, "starrocks", &["nx_tipos"], None, str::to_string).await;
    let b = &t.back[0];
    assert_eq!(b.primary_key.as_ref().unwrap().columns, ["id"]);
    for (c, want) in [
        ("id", "bigint(20)"),
        ("chico", "smallint(6)"),
        ("entero", "int(11)"),
        ("precio", "decimal(12, 2)"),
        ("ratio", "double"),
        ("activo", "tinyint(1)"),
        ("nombre", "varchar(160)"),
        ("notas", "varchar(1048576)"),
        ("alta", "date"),
        ("creado", "datetime"),
        ("datos", "json"),
        ("uid", "varchar(36)"),
    ] {
        assert_eq!(back_type(b, c), want, "{c}");
    }
    assert!(!common::col(b, "entero").nullable);
    insert(&mut sr, "starrocks", b, &[
        ("id", json!(9223372036854775807i64)),
        ("chico", json!(32767)),
        ("entero", json!(-2147483648i64)),
        ("grande", json!(-9223372036854775808i64)),
        ("precio", json!("9999999999.99")),
        ("ratio", json!(1.5e300)),
        ("activo", json!(false)),
        ("nombre", json!("ñandú — 日本 😀")),
        ("notas", json!("x".repeat(3000))),
        ("alta", json!("2099-12-31")),
        ("creado", json!("2024-02-29 23:59:59.123")),
        ("datos", json!("{\"a\": [1, 2]}")),
        ("uid", json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")),
    ])
    .await;
    // Defaults applied by the target.
    common::exec(&mut sr, "INSERT INTO nx_tipos (id) VALUES (1)").await;
    assert_eq!(count(&mut sr, b).await, 2);

    let (conv, v) = back_to_pg(&mut pg, &mut sr, "starrocks", "nx_tipos").await;
    for (c, want) in [
        ("id", "bigint"),
        ("chico", "smallint"),
        ("precio", "numeric(12,2)"),
        ("activo", "boolean"),
        ("nombre", "character varying(160)"),
        ("notas", "text"),
        ("creado", "timestamp(6) without time zone"),
        ("datos", "jsonb"),
    ] {
        assert_eq!(back_type(&v, c), want, "vuelta {c}: {conv:?}");
    }
    assert_eq!(v.primary_key.as_ref().unwrap().columns, ["id"]);
    common::exec(&mut pg, "INSERT INTO nx_tipos_vuelta SELECT id, chico, entero, grande, precio, ratio, activo, nombre, notas, alta, creado, datos FROM nx_tipos").await;
}

#[tokio::test]
#[ignore]
async fn live_mysql_starrocks() {
    let Some(mut my) = open("mysql", "DBINE_TEST_MYSQL_URL").await else { return };
    let Some(mut sr) = open("starrocks", "DBINE_TEST_STARROCKS_URL").await else { return };
    common::exec_quiet(&mut my, &["DROP TABLE IF EXISTS nx_my"]).await;
    exec_all(&mut my, &[
        "CREATE TABLE nx_my (id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY, nombre VARCHAR(20) NOT NULL, precio DECIMAL(8,3) DEFAULT 1.5,
            tipo ENUM('a','b'), alta DATETIME(3) DEFAULT CURRENT_TIMESTAMP(3), grande BIGINT UNSIGNED, flag TINYINT(1) DEFAULT 1,
            texto MEDIUMTEXT, KEY ix_nombre (nombre))",
        "INSERT INTO nx_my (nombre, precio, tipo, grande, texto) VALUES ('ñandú 日本', 99999.999, 'b', 18446744073709551615, 'z')",
    ])
    .await;
    let t = trip(&mut my, "mysql", &mut sr, "starrocks", &["nx_my"], None, str::to_string).await;
    let b = &t.back[0];
    assert_eq!(back_type(b, "id"), "bigint(20)");
    assert_eq!(back_type(b, "grande"), "bigint(20) unsigned", "LARGEINT");
    assert_eq!(back_type(b, "flag"), "tinyint(1)");
    assert_eq!(back_type(b, "nombre"), "varchar(80)");
    assert!(t.conversion.issues.iter().any(|i| i.code == IssueCode::AutoIncrementDropped));
    // Created as a bitmap index (StarRocks' catalog doesn't list them back).
    assert!(t.conversion.tables[0].indexes.iter().any(|i| i.name == "ix_nombre"));
    insert(&mut sr, "starrocks", b, &[
        ("id", json!(4294967295u64)),
        ("nombre", json!("ñandú 日本")),
        ("tipo", json!("b")),
        ("grande", json!("18446744073709551615")),
        ("texto", json!("z")),
    ])
    .await;
    assert_eq!(count(&mut sr, b).await, 1);
}

#[tokio::test]
#[ignore]
async fn live_postgres_greptimedb_postgres() {
    let Some(mut pg) = pg_source().await else { return };
    let Some(mut g) = open("greptimedb", "DBINE_TEST_GREPTIMEDB_URL").await else { return };
    let t = trip(&mut pg, "postgres", &mut g, "greptimedb", &["nx_tipos"], None, str::to_string).await;
    let b = &t.back[0];
    assert_eq!(b.options.get("time_index").map(String::as_str), Some("creado"));
    assert_eq!(b.primary_key.as_ref().unwrap().columns, ["id"]);
    for (c, want) in [
        ("id", "bigint"),
        ("chico", "smallint"),
        ("precio", "decimal(12,2)"),
        ("activo", "boolean"),
        ("nombre", "string"),
        ("creado", "timestamp(3)"),
        ("datos", "json"),
        ("uid", "string"),
    ] {
        assert_eq!(back_type(b, c), want, "{c}");
    }
    insert(&mut g, "greptimedb", b, &[
        ("id", json!(9223372036854775807i64)),
        ("chico", json!(32767)),
        ("entero", json!(-2147483648i64)),
        ("precio", json!("9999999999.99")),
        ("ratio", json!(1.5e300)),
        ("activo", json!(false)),
        ("nombre", json!("ñandú — 日本 😀")),
        ("alta", json!("2099-12-31")),
        ("creado", json!("2024-02-29 20:59:59.123")),
        ("datos", json!("{\"a\": [1, 2]}")),
    ])
    .await;
    common::exec(&mut g, "INSERT INTO nx_tipos (id, entero) VALUES (1, 0)").await;
    assert_eq!(count(&mut g, b).await, 2);

    let (_, v) = back_to_pg(&mut pg, &mut g, "greptimedb", "nx_tipos").await;
    assert_eq!(back_type(&v, "creado"), "timestamp(3) with time zone");
    assert_eq!(back_type(&v, "nombre"), "text");
    assert_eq!(back_type(&v, "precio"), "numeric(12,2)");
}

#[tokio::test]
#[ignore]
async fn live_mysql_greptimedb_without_timestamp() {
    let Some(mut my) = open("mysql", "DBINE_TEST_MYSQL_URL").await else { return };
    let Some(mut g) = open("greptimedb", "DBINE_TEST_GREPTIMEDB_URL").await else { return };
    common::exec_quiet(&mut my, &["DROP TABLE IF EXISTS nx_sin_tiempo"]).await;
    exec_all(&mut my, &["CREATE TABLE nx_sin_tiempo (id INT PRIMARY KEY, nombre VARCHAR(10), u INT UNSIGNED, t TIME(3))"]).await;
    let t = trip(&mut my, "mysql", &mut g, "greptimedb", &["nx_sin_tiempo"], None, str::to_string).await;
    let b = &t.back[0];
    assert_eq!(b.options.get("time_index").map(String::as_str), Some("greptime_timestamp"));
    assert_eq!(back_type(b, "u"), "int unsigned");
    assert_eq!(back_type(b, "t"), "string");
    // The added time index fills itself.
    common::exec(&mut g, "INSERT INTO nx_sin_tiempo (id, nombre, u, t) VALUES (1, 'a', 4294967295, '23:59:59.999')").await;
    assert_eq!(count(&mut g, b).await, 1);
}

#[tokio::test]
#[ignore]
async fn live_postgres_manticore_postgres() {
    let Some(mut pg) = pg_source().await else { return };
    let Some(mut m) = open("manticore", "DBINE_TEST_MANTICORE_URL").await else { return };
    let t = trip(&mut pg, "postgres", &mut m, "manticore", &["nx_tipos"], None, str::to_string).await;
    let b = &t.back[0];
    for (c, want) in [
        ("id", "bigint"),
        ("chico", "bigint"),
        ("precio", "float"),
        ("activo", "bool"),
        ("nombre", "string"),
        ("notas", "text"),
        ("creado", "timestamp"),
        ("datos", "json"),
    ] {
        assert_eq!(back_type(b, c), want, "{c}");
    }
    insert(&mut m, "manticore", b, &[
        ("id", json!(9223372036854775807i64)),
        ("chico", json!(32767)),
        ("entero", json!(-2147483648i64)),
        ("precio", json!(9999999999.99)),
        ("activo", json!(false)),
        ("nombre", json!("ñandú — 日本 😀")),
        ("notas", json!("x".repeat(3000))),
        ("alta", json!(4102358400u64)),
        ("creado", json!(1709240399)),
        ("datos", json!({"a": [1, 2]})),
    ])
    .await;
    assert_eq!(count(&mut m, b).await, 1);
    let (_, v) = back_to_pg(&mut pg, &mut m, "manticore", "nx_tipos").await;
    assert_eq!(back_type(&v, "precio"), "real");
    assert_eq!(back_type(&v, "creado"), "timestamp(0) with time zone");
    assert_eq!(back_type(&v, "datos"), "json");
}

#[tokio::test]
#[ignore]
async fn live_postgres_tdengine_postgres() {
    let Some(mut pg) = pg_source().await else { return };
    let Some(mut td) = open("tdengine", "DBINE_TEST_TDENGINE_URL").await else { return };
    common::exec_quiet(&mut td, &["CREATE DATABASE IF NOT EXISTS dbine"]).await;
    let t = trip(&mut pg, "postgres", &mut td, "tdengine", &["nx_tipos"], Some("dbine"), str::to_string).await;
    let b = &t.back[0];
    assert_eq!(b.columns[0].name, "creado");
    for (c, want) in [
        ("creado", "timestamp"),
        ("id", "bigint"),
        ("chico", "smallint"),
        ("precio", "decimal(12, 2)"),
        ("activo", "bool"),
        ("nombre", "nchar(40)"),
        ("notas", "nchar(4096)"),
        ("alta", "timestamp"),
        ("uid", "varchar(36)"),
    ] {
        assert_eq!(back_type(b, c), want, "{c}");
    }
    insert(&mut td, "tdengine", b, &[
        ("creado", json!("2024-02-29 20:59:59.123")),
        ("id", json!(9223372036854775807i64)),
        ("chico", json!(32767)),
        ("entero", json!(-2147483648i64)),
        ("precio", json!(9999999999.99)),
        ("ratio", json!(1.5e300)),
        ("activo", json!(false)),
        ("nombre", json!("ñandú — 日本 😀")),
        ("notas", json!("x".repeat(3000))),
        ("alta", json!("2099-12-31 00:00:00.000")),
        ("uid", json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")),
    ])
    .await;
    assert_eq!(count(&mut td, b).await, 1);
    let (_, v) = back_to_pg(&mut pg, &mut td, "tdengine", "nx_tipos").await;
    assert_eq!(back_type(&v, "creado"), "timestamp with time zone");
    assert_eq!(back_type(&v, "nombre"), "character varying(40)");
    assert_eq!(back_type(&v, "precio"), "numeric(12,2)");
}

#[tokio::test]
#[ignore]
async fn live_postgres_iotdb_postgres() {
    let Some(mut pg) = pg_source().await else { return };
    let Some(mut io) = open("iotdb", "DBINE_TEST_IOTDB_URL").await else { return };
    let db = std::env::var("DBINE_TEST_IOTDB_URL").unwrap().rsplit('/').next().unwrap().to_string();
    common::exec_quiet(&mut io, &[&format!("CREATE DATABASE {db}")]).await;
    let t = trip(&mut pg, "postgres", &mut io, "iotdb", &["nx_tipos"], Some(&db), str::to_string).await;
    let b = &t.back[0];
    assert_eq!(b.options.get("aligned").map(String::as_str), Some("true"));
    for (c, want) in [("id", "int64"), ("chico", "int32"), ("precio", "double"), ("activo", "boolean"), ("nombre", "text"), ("alta", "date"), ("datos", "text")] {
        assert_eq!(back_type(b, c), want, "{c}");
    }
    insert(&mut io, "iotdb", b, &[
        ("Time", json!("2024-02-29 20:59:59.123")),
        ("id", json!(9223372036854775807i64)),
        ("chico", json!(32767)),
        ("precio", json!(9999999999.99)),
        ("activo", json!(false)),
        ("nombre", json!("ñandú — 日本 😀")),
        ("alta", json!("2099-12-31")),
    ])
    .await;
    assert_eq!(count(&mut io, b).await, 1);
    let (_, v) = back_to_pg(&mut pg, &mut io, "iotdb", "nx_tipos").await;
    assert_eq!(back_type(&v, "time"), "timestamp(3) with time zone");
    assert_eq!(back_type(&v, "id"), "bigint");
    assert_eq!(back_type(&v, "nombre"), "text");
}

#[tokio::test]
#[ignore]
async fn live_postgres_ksqldb_postgres() {
    let Some(mut pg) = pg_source().await else { return };
    let Some(mut k) = open("ksqldb", "DBINE_TEST_KSQLDB_URL").await else { return };
    let t = trip(&mut pg, "postgres", &mut k, "ksqldb", &["nx_tipos"], None, str::to_string).await;
    let b = &t.back[0];
    assert_eq!(b.kind, "table");
    assert_eq!(b.primary_key.as_ref().unwrap().columns, ["ID"]);
    for (c, want) in [("ID", "bigint"), ("CHICO", "integer"), ("PRECIO", "decimal(12, 2)"), ("ACTIVO", "boolean"), ("NOMBRE", "string"), ("CREADO", "timestamp"), ("ALTA", "date")] {
        assert_eq!(back_type(b, c), want, "{c}");
    }
    insert(&mut k, "ksqldb", b, &[
        ("ID", json!(9223372036854775807i64)),
        ("CHICO", json!(32767)),
        ("PRECIO", json!(9999999999.99)),
        ("ACTIVO", json!(false)),
        ("NOMBRE", json!("ñandú — 日本 😀")),
    ])
    .await;
    let (_, v) = back_to_pg(&mut pg, &mut k, "ksqldb", "NX_TIPOS").await;
    assert_eq!(back_type(&v, "creado"), "timestamp(3) without time zone");
    assert_eq!(back_type(&v, "precio"), "numeric(12,2)");
    assert_eq!(v.primary_key.as_ref().unwrap().columns, ["id"]);
    common::exec_quiet(&mut k, &["DROP TABLE IF EXISTS `NX_TIPOS` DELETE TOPIC"]).await;
}

#[tokio::test]
#[ignore]
async fn live_postgres_phoenix_postgres() {
    let Some(mut pg) = pg_source().await else { return };
    let Some(mut ph) = open("phoenix", "DBINE_TEST_PHOENIX_URL").await else { return };
    let t = trip(&mut pg, "postgres", &mut ph, "phoenix", &["nx_tipos"], None, str::to_string).await;
    let b = &t.back[0];
    assert_eq!(b.name, "NX_TIPOS");
    assert_eq!(b.primary_key.as_ref().unwrap().columns, ["ID"]);
    for (c, want) in [
        ("ID", "bigint"),
        ("CHICO", "smallint"),
        ("PRECIO", "decimal(12, 2)"),
        ("ACTIVO", "boolean"),
        ("NOMBRE", "varchar(40)"),
        ("NOTAS", "varchar"),
        ("ALTA", "date"),
        ("CREADO", "timestamp"),
        ("UID", "char(36)"),
    ] {
        assert_eq!(back_type(b, c), want, "{c}");
    }
    insert(&mut ph, "phoenix", b, &[
        ("ID", json!(9223372036854775807i64)),
        ("CHICO", json!(32767)),
        ("ENTERO", json!(-2147483648i64)),
        ("PRECIO", json!(9999999999.99)),
        ("ACTIVO", json!(false)),
        ("NOMBRE", json!("ñandú — 日本 😀")),
        ("UID", json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")),
    ])
    .await;
    assert_eq!(count(&mut ph, b).await, 1);
    let (_, v) = back_to_pg(&mut pg, &mut ph, "phoenix", "NX_TIPOS").await;
    assert_eq!(back_type(&v, "alta"), "timestamp(3) without time zone");
    assert_eq!(back_type(&v, "nombre"), "character varying(40)");
    assert_eq!(v.primary_key.as_ref().unwrap().columns, ["id"]);
}

#[tokio::test]
#[ignore]
async fn live_postgres_dremio_postgres() {
    let Some(mut pg) = pg_source().await else { return };
    let Some(mut dr) = open("dremio", "DBINE_TEST_DREMIO_URL").await else { return };
    let t = trip(&mut pg, "postgres", &mut dr, "dremio", &["nx_tipos"], Some("$scratch"), str::to_string).await;
    let b = &t.back[0];
    for (c, want) in [("id", "bigint"), ("chico", "integer"), ("precio", "decimal(12,2)"), ("activo", "boolean"), ("nombre", "character varying"), ("creado", "timestamp")] {
        assert_eq!(back_type(b, c), want, "{c}");
    }
    insert(&mut dr, "dremio", b, &[
        ("id", json!(9223372036854775807i64)),
        ("chico", json!(32767)),
        ("precio", json!(9999999999.99)),
        ("activo", json!(false)),
        // Dremio fails on string literals beyond Latin-1 (Calcite: "failed to
        // preserve datatypes"), with or without CAST: a driver matter.
        ("nombre", json!("ñandú")),
        ("creado", json!("2024-02-29 20:59:59.123")),
    ])
    .await;
    assert_eq!(count(&mut dr, b).await, 1);
    let (_, v) = back_to_pg(&mut pg, &mut dr, "dremio", "nx_tipos").await;
    assert_eq!(back_type(&v, "creado"), "timestamp(3) without time zone");
    assert_eq!(back_type(&v, "nombre"), "text");
}
