//! Analytical engines: ClickHouse / Timeplus, DuckDB, Snowflake, BigQuery,
//! Spanner, Trino / Presto / Athena and the Hive family (Hive, Impala,
//! Spark, Databricks). End-to-end conversions with tables spelled the way
//! each driver reports them, and round trips against real servers.
//!
//! Live tests (`cargo test -p dbine-schema --test analytics -- --ignored`)
//! read the `DBINE_TEST_<ENGINE>_URL` variables and skip without them:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011/dbine_an      # the database must exist
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123
//! DBINE_TEST_TIMEPLUS_URL=http://localhost:25119
//! DBINE_TEST_TRINO_URL=http://dbine@localhost:25180/memory
//! DBINE_TEST_BIGQUERY_URL=http://localhost:25302                      # bigquery-emulator, project test, dataset ds1
//! DBINE_TEST_SPANNER_URL=http://localhost:25303                       # spanner emulator REST gateway
//! ```
//! DuckDB runs embedded on a temporary file.

mod common;

use common::{col, exec, exec_quiet, read, target_ddl};
use dbine_driver::{ColumnDef, ConnectionConfig, DdlParts, ForeignKeyDef, IndexDef, KeyDef, Session, TableSchema};
use dbine_schema::{convert, Conversion, IssueCode, Options, Severity};

// ---------------------------------------------------------------------------
// Helpers

fn c(name: &str, ty: &str) -> ColumnDef {
    ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
}

fn nn(mut c: ColumnDef) -> ColumnDef {
    c.nullable = false;
    c
}

fn def(mut c: ColumnDef, d: &str) -> ColumnDef {
    c.default_value = Some(d.into());
    c
}

fn auto(mut c: ColumnDef) -> ColumnDef {
    c.auto_increment = true;
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

fn conv(tables: &[TableSchema], from: &str, to: &str) -> Conversion {
    convert(tables, from, to, &Options::default()).unwrap()
}

/// The PostgreSQL tables of the end-to-end tests, as `database_schema` reports them.
fn pg_tables() -> Vec<TableSchema> {
    let clientes = table("clientes", vec![nn(c("id", "integer")), c("nombre", "character varying(120)")], &["id"]);
    let mut pedidos = table(
        "pedidos",
        vec![
            nn(def(c("id", "bigint"), "nextval('pedidos_id_seq'::regclass)")),
            nn(c("cliente_id", "integer")),
            def(c("total", "numeric(12,2)"), "0"),
            c("grande", "numeric"),
            def(c("creado", "timestamp with time zone"), "now()"),
            c("local", "timestamp(3) without time zone"),
            c("dia", "date"),
            c("hora", "time without time zone"),
            def(c("activo", "boolean"), "true"),
            def(c("codigo", "uuid"), "gen_random_uuid()"),
            c("datos", "jsonb"),
            c("etiquetas", "text[]"),
            def(c("estado", "character varying(20)"), "'nuevo'::character varying"),
            c("bytes", "bytea"),
            c("ip", "inet"),
        ],
        &["id"],
    );
    pedidos.foreign_keys.push(ForeignKeyDef {
        name: Some("fk_pedidos_clientes".into()),
        columns: vec!["cliente_id".into()],
        ref_table: "clientes".into(),
        ref_columns: vec!["id".into()],
        on_delete: Some("CASCADE".into()),
        ..Default::default()
    });
    pedidos.indexes.push(IndexDef { name: "ix_pedidos_creado".into(), columns: vec!["creado".into()], kind: Some("btree".into()), ..Default::default() });
    pedidos.indexes.push(IndexDef { name: "ux_pedidos_codigo".into(), columns: vec!["codigo".into()], unique: true, ..Default::default() });
    vec![clientes, pedidos]
}

// ---------------------------------------------------------------------------
// End to end, no servers

#[test]
fn postgres_to_clickhouse() {
    let r = conv(&pg_tables(), "postgres", "clickhouse");
    let p = &r.tables[1];
    assert_eq!(ty(p, "id"), "Int64");
    assert!(!column(p, "id").auto_increment);
    assert!(has(&r, IssueCode::AutoIncrementDropped, "id"));
    assert_eq!(ty(p, "total"), "Decimal(12, 2)");
    assert_eq!(ty(p, "grande"), "Decimal(76, 20)");
    assert_eq!(ty(p, "creado"), "DateTime64(6)");
    assert_eq!(column(p, "creado").default_value.as_deref(), Some("now64(6)"));
    assert_eq!(ty(p, "local"), "DateTime64(3)");
    assert_eq!(ty(p, "dia"), "Date32");
    assert_eq!(ty(p, "hora"), "String");
    assert_eq!(ty(p, "activo"), "Bool");
    assert_eq!(column(p, "activo").default_value.as_deref(), Some("true"));
    assert_eq!(ty(p, "codigo"), "UUID");
    assert_eq!(column(p, "codigo").default_value.as_deref(), Some("generateUUIDv4()"));
    assert_eq!(ty(p, "datos"), "JSON");
    assert_eq!(ty(p, "etiquetas"), "Array(String)");
    assert_eq!(ty(p, "estado"), "String");
    assert_eq!(column(p, "estado").default_value.as_deref(), Some("'nuevo'"));
    assert_eq!(ty(p, "bytes"), "String");
    // No foreign keys; ENGINE and ORDER BY from the primary key.
    assert!(p.foreign_keys.is_empty());
    assert!(r.issues.iter().any(|i| i.code == IssueCode::ForeignKeyDropped && i.severity == Severity::Dropped));
    assert_eq!(p.options.get("engine").map(String::as_str), Some("MergeTree"));
    assert_eq!(p.options.get("order_by").map(String::as_str), Some("`id`"));
    // The unique index can't be unique.
    assert!(p.indexes.iter().all(|i| !i.unique));
    assert!(has(&r, IssueCode::IndexChanged, "ux_pedidos_codigo"));
    // The DDL the driver writes from it.
    let ddl = dbine_drivers::find("clickhouse").unwrap().table_ddl(p, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
    assert!(ddl.contains("ENGINE = MergeTree\nORDER BY `id`"), "{ddl}");
    assert!(ddl.contains("`total` Nullable(Decimal(12, 2)) DEFAULT 0"), "{ddl}");
}

#[test]
fn mysql_to_clickhouse_keeps_unsigned() {
    let t = table(
        "m",
        vec![
            nn(auto(c("id", "int unsigned"))),
            c("u8", "tinyint unsigned"),
            c("big", "bigint unsigned"),
            c("tipo", "enum('a','b')"),
            c("ts", "timestamp"),
            c("fijo", "char(2)"),
        ],
        &[],
    );
    let r = conv(&[t], "mysql", "clickhouse");
    let m = &r.tables[0];
    assert_eq!(ty(m, "id"), "UInt32");
    assert_eq!(ty(m, "u8"), "UInt8");
    assert_eq!(ty(m, "big"), "UInt64");
    assert_eq!(ty(m, "tipo"), "Enum8('a' = 1, 'b' = 2)");
    assert_eq!(ty(m, "ts"), "DateTime64(6)");
    assert_eq!(ty(m, "fijo"), "String");
    assert_eq!(m.options.get("order_by").map(String::as_str), Some("tuple()"));
    assert!(r.issues.iter().any(|i| i.code == IssueCode::OptionAdded && i.severity == Severity::Warning));
}

#[test]
fn clickhouse_to_postgres_and_sqlserver() {
    let mut t = table(
        "eventos",
        vec![
            c("id", "UInt64"),
            c("ts", "DateTime64(3, 'UTC')"),
            c("nombre", "LowCardinality(String)"),
            c("monto", "Decimal(18, 4)"),
            c("tags", "Array(String)"),
            c("attrs", "Map(String, UInt32)"),
            c("pais", "FixedString(2)"),
            c("tipo", "Enum8('alta' = 1, 'baja' = 2)"),
            def(c("alta", "DateTime"), "now()"),
            c("ip", "IPv4"),
            c("flag", "Bool"),
        ],
        &["id"],
    );
    t.options.insert("engine".into(), "MergeTree".into());
    let r = conv(std::slice::from_ref(&t), "clickhouse", "postgres");
    let e = &r.tables[0];
    assert_eq!(ty(e, "id"), "numeric(39, 0)");
    assert_eq!(ty(e, "ts"), "timestamp(3) with time zone");
    assert_eq!(ty(e, "nombre"), "text");
    assert_eq!(ty(e, "monto"), "numeric(18, 4)");
    assert_eq!(ty(e, "tags"), "text[]");
    assert_eq!(ty(e, "attrs"), "jsonb");
    assert_eq!(ty(e, "pais"), "char(2)");
    assert_eq!(ty(e, "tipo"), "varchar(4)");
    assert_eq!(ty(e, "alta"), "timestamp(0) with time zone");
    assert_eq!(column(e, "alta").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(ty(e, "ip"), "inet");
    assert_eq!(ty(e, "flag"), "boolean");
    assert!(has(&r, IssueCode::OptionDropped, "engine"));

    let r = conv(&[t], "clickhouse", "sqlserver");
    let e = &r.tables[0];
    assert_eq!(ty(e, "id"), "decimal(20, 0)");
    assert_eq!(ty(e, "ts"), "datetimeoffset(3)");
    assert_eq!(ty(e, "pais"), "char(2)");
}

#[test]
fn postgres_to_timeplus() {
    let r = conv(&pg_tables(), "postgres", "timeplus");
    let p = &r.tables[1];
    assert_eq!(ty(p, "id"), "int64");
    assert_eq!(ty(p, "creado"), "datetime64(6)");
    assert_eq!(ty(p, "etiquetas"), "array(string)");
    assert_eq!(p.options.get("mode").map(String::as_str), Some("versioned_kv"));
}

#[test]
fn postgres_to_duckdb_and_back() {
    let r = conv(&pg_tables(), "postgres", "duckdb");
    let p = &r.tables[1];
    assert_eq!(ty(p, "id"), "BIGINT");
    assert!(column(p, "id").auto_increment);
    assert_eq!(ty(p, "total"), "DECIMAL(12, 2)");
    assert_eq!(ty(p, "grande"), "DECIMAL(38, 10)");
    assert!(has(&r, IssueCode::PrecisionLoss, "grande"));
    assert_eq!(ty(p, "creado"), "TIMESTAMPTZ");
    assert_eq!(ty(p, "local"), "TIMESTAMP");
    assert_eq!(ty(p, "hora"), "TIME");
    assert_eq!(ty(p, "codigo"), "UUID");
    assert_eq!(column(p, "codigo").default_value.as_deref(), Some("gen_random_uuid()"));
    assert_eq!(ty(p, "datos"), "JSON");
    assert_eq!(ty(p, "etiquetas"), "VARCHAR[]");
    assert_eq!(ty(p, "bytes"), "BLOB");
    // DuckDB has no ON DELETE CASCADE.
    assert_eq!(p.foreign_keys[0].on_delete, None);
    assert!(r.issues.iter().any(|i| i.code == IssueCode::ForeignKeyActionChanged && i.severity == Severity::Warning));

    let d = table(
        "d",
        vec![
            c("h", "HUGEINT"),
            c("u", "UBIGINT"),
            c("l", "INTEGER[]"),
            c("f", "DOUBLE[3]"),
            c("m", "MAP(VARCHAR, INTEGER)"),
            c("s", "STRUCT(a INTEGER, b VARCHAR)"),
            c("e", "ENUM('x', 'y')"),
            c("tz", "TIMESTAMP WITH TIME ZONE"),
            c("ns", "TIMESTAMP_NS"),
            c("iv", "INTERVAL"),
        ],
        &[],
    );
    let r = conv(std::slice::from_ref(&d), "duckdb", "postgres");
    let t = &r.tables[0];
    assert_eq!(ty(t, "h"), "numeric(39, 0)");
    assert_eq!(ty(t, "u"), "numeric(39, 0)");
    assert_eq!(ty(t, "l"), "integer[]");
    assert_eq!(ty(t, "f"), "double precision[]");
    assert_eq!(ty(t, "m"), "jsonb");
    assert_eq!(ty(t, "s"), "jsonb");
    assert_eq!(ty(t, "e"), "varchar(1)");
    assert_eq!(ty(t, "tz"), "timestamp(6) with time zone");
    assert_eq!(ty(t, "ns"), "timestamp(6)");
    assert!(has(&r, IssueCode::PrecisionLoss, "ns"));
    assert_eq!(ty(t, "iv"), "interval");

    let r = conv(&[d], "duckdb", "sqlserver");
    assert_eq!(ty(&r.tables[0], "h"), "decimal(38, 0)");
    assert_eq!(ty(&r.tables[0], "u"), "decimal(20, 0)");
}

#[test]
fn postgres_to_snowflake_and_back() {
    let r = conv(&pg_tables(), "postgres", "snowflake");
    let p = &r.tables[1];
    assert_eq!(p.name, "PEDIDOS");
    assert_eq!(ty(p, "ID"), "NUMBER(19, 0)");
    assert!(column(p, "ID").auto_increment);
    assert_eq!(ty(p, "TOTAL"), "NUMBER(12, 2)");
    assert_eq!(ty(p, "CREADO"), "TIMESTAMP_TZ");
    assert_eq!(column(p, "CREADO").default_value.as_deref(), Some("CURRENT_TIMESTAMP()"));
    assert_eq!(ty(p, "LOCAL"), "TIMESTAMP_NTZ(3)");
    assert_eq!(ty(p, "ACTIVO"), "BOOLEAN");
    assert_eq!(ty(p, "CODIGO"), "VARCHAR(36)");
    assert_eq!(column(p, "CODIGO").default_value.as_deref(), Some("UUID_STRING()"));
    assert_eq!(ty(p, "DATOS"), "VARIANT");
    assert_eq!(ty(p, "ETIQUETAS"), "ARRAY");
}

#[test]
fn snowflake_to_postgres() {
    let t = table(
        "VENTAS",
        vec![
            nn(auto(c("ID", "NUMBER(38,0)"))),
            c("CANT", "NUMBER(9,0)"),
            c("MONTO", "NUMBER(12,2)"),
            c("NOMBRE", "TEXT(100)"),
            c("NOTAS", "TEXT(16777216)"),
            c("NTZ", "TIMESTAMP_NTZ(9)"),
            c("LTZ", "TIMESTAMP_LTZ(9)"),
            c("DOC", "VARIANT"),
            c("RAW", "BINARY(8388608)"),
            def(c("ALTA", "TIMESTAMP_NTZ(9)"), "CURRENT_TIMESTAMP()"),
        ],
        &["ID"],
    );
    let r = conv(&[t], "snowflake", "postgres");
    let v = &r.tables[0];
    assert_eq!(v.name, "ventas");
    // NUMBER(38, 0) is a decimal, but PostgreSQL identity columns are integers.
    assert_eq!(ty(v, "id"), "bigint");
    assert!(column(v, "id").auto_increment);
    assert!(has(&r, IssueCode::RangeLoss, "ID") || has(&r, IssueCode::RangeLoss, "id"));
    assert_eq!(ty(v, "cant"), "integer");
    assert_eq!(ty(v, "monto"), "numeric(12, 2)");
    assert_eq!(ty(v, "nombre"), "varchar(100)");
    assert_eq!(ty(v, "notas"), "text");
    assert_eq!(ty(v, "ntz"), "timestamp(6)");
    assert!(has(&r, IssueCode::PrecisionLoss, "NTZ"));
    assert_eq!(ty(v, "ltz"), "timestamp(6) with time zone");
    assert_eq!(ty(v, "doc"), "jsonb");
    assert_eq!(ty(v, "raw"), "bytea");
    assert_eq!(column(v, "alta").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));
}

#[test]
fn postgres_and_sqlserver_to_bigquery() {
    let r = conv(&pg_tables(), "postgres", "bigquery");
    let p = &r.tables[1];
    assert_eq!(ty(p, "id"), "INT64");
    assert!(!column(p, "id").auto_increment);
    assert!(has(&r, IssueCode::AutoIncrementDropped, "id"));
    assert_eq!(ty(p, "total"), "NUMERIC(12, 2)");
    assert_eq!(ty(p, "grande"), "BIGNUMERIC");
    assert_eq!(ty(p, "creado"), "TIMESTAMP");
    assert_eq!(column(p, "creado").default_value.as_deref(), Some("CURRENT_TIMESTAMP()"));
    assert_eq!(ty(p, "local"), "DATETIME");
    assert_eq!(ty(p, "hora"), "TIME");
    assert_eq!(ty(p, "codigo"), "STRING");
    assert_eq!(column(p, "codigo").default_value.as_deref(), Some("GENERATE_UUID()"));
    assert_eq!(ty(p, "datos"), "JSON");
    assert_eq!(ty(p, "etiquetas"), "ARRAY<STRING>");
    assert_eq!(ty(p, "estado"), "STRING(20)");
    assert_eq!(ty(p, "bytes"), "BYTES");
    // Keys stay informational, without actions; no indexes.
    assert_eq!(p.foreign_keys.len(), 1);
    assert_eq!(p.foreign_keys[0].on_delete, None);
    assert!(p.indexes.is_empty());
    assert!(r.issues.iter().any(|i| i.code == IssueCode::IndexDropped));
    assert!(has(&r, IssueCode::OptionAdded, "PRIMARY KEY"));

    let t = table("t", vec![nn(auto(c("Id", "int"))), c("Monto", "money"), c("Zona", "datetimeoffset(7)"), c("N", "nvarchar(max)"), c("B", "bit")], &["Id"]);
    let r = conv(&[t], "sqlserver", "bigquery");
    let t = &r.tables[0];
    assert_eq!(ty(t, "Monto"), "NUMERIC(19, 4)");
    assert_eq!(ty(t, "Zona"), "TIMESTAMP");
    assert!(has(&r, IssueCode::PrecisionLoss, "Zona"));
    assert_eq!(ty(t, "N"), "STRING");
    assert_eq!(ty(t, "B"), "BOOL");
}

#[test]
fn bigquery_to_postgres_and_mysql() {
    let t = table(
        "people",
        vec![
            nn(c("id", "INT64")),
            c("name", "STRING"),
            c("score", "FLOAT64"),
            c("tags", "ARRAY<STRING>"),
            c("addr", "STRUCT<city STRING, zip INT64>"),
            c("ts", "TIMESTAMP"),
            c("dt", "DATETIME"),
            c("amount", "NUMERIC"),
            c("big", "BIGNUMERIC(50, 10)"),
            def(c("alta", "TIMESTAMP"), "CURRENT_TIMESTAMP()"),
        ],
        &["id"],
    );
    let r = conv(std::slice::from_ref(&t), "bigquery", "postgres");
    let p = &r.tables[0];
    assert_eq!(ty(p, "id"), "bigint");
    assert_eq!(ty(p, "name"), "text");
    assert_eq!(ty(p, "tags"), "text[]");
    assert_eq!(ty(p, "addr"), "jsonb");
    assert_eq!(ty(p, "ts"), "timestamp(6) with time zone");
    assert_eq!(ty(p, "dt"), "timestamp(6)");
    assert_eq!(ty(p, "amount"), "numeric(38, 9)");
    assert_eq!(ty(p, "big"), "numeric(50, 10)");
    assert_eq!(column(p, "alta").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));

    let r = conv(&[t], "bigquery", "mysql");
    let m = &r.tables[0];
    assert_eq!(ty(m, "amount"), "decimal(38, 9)");
    assert_eq!(ty(m, "tags"), "json");
}

#[test]
fn postgres_to_spanner_and_back() {
    let mut tables = pg_tables();
    tables.push(table("sin_clave", vec![c("x", "integer")], &[]));
    let r = conv(&tables, "postgres", "spanner");
    let p = &r.tables[1];
    assert_eq!(ty(p, "id"), "INT64");
    assert!(column(p, "id").auto_increment);
    assert!(has(&r, IssueCode::AutoIncrementChanged, "id"));
    assert_eq!(ty(p, "total"), "NUMERIC");
    assert!(has(&r, IssueCode::PrecisionLoss, "grande"));
    assert_eq!(ty(p, "creado"), "TIMESTAMP");
    assert_eq!(column(p, "creado").default_value.as_deref(), Some("CURRENT_TIMESTAMP()"));
    assert_eq!(ty(p, "hora"), "STRING(32)");
    assert_eq!(ty(p, "codigo"), "STRING(36)");
    assert_eq!(ty(p, "etiquetas"), "ARRAY<STRING(MAX)>");
    assert_eq!(ty(p, "estado"), "STRING(20)");
    assert_eq!(column(p, "estado").default_value.as_deref(), Some("'nuevo'"));
    assert_eq!(ty(p, "bytes"), "BYTES(MAX)");
    assert_eq!(p.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    assert_eq!(p.indexes.len(), 2);
    assert!(r.issues.iter().any(|i| i.code == IssueCode::CommentDropped) || tables[0].comment.is_none());
    // A table without a key gets one.
    let s = &r.tables[2];
    assert_eq!(s.primary_key.as_ref().unwrap().columns, vec!["row_id".to_string()]);
    assert!(has(&r, IssueCode::PrimaryKeyAdded, "row_id"));

    let t = table(
        "people",
        vec![nn(auto(c("id", "INT64"))), c("name", "STRING(20)"), c("raw", "BYTES(10)"), c("tags", "ARRAY<STRING(5)>"), c("f", "FLOAT32"), c("ts", "TIMESTAMP")],
        &["id"],
    );
    let r = conv(&[t], "spanner", "mysql");
    let m = &r.tables[0];
    assert_eq!(ty(m, "id"), "bigint");
    assert!(column(m, "id").auto_increment);
    assert_eq!(ty(m, "name"), "varchar(20)");
    assert_eq!(ty(m, "raw"), "varbinary(10)");
    assert_eq!(ty(m, "tags"), "json");
    assert_eq!(ty(m, "f"), "float");
    assert_eq!(ty(m, "ts"), "datetime(6)");
    assert!(has(&r, IssueCode::TimeZoneLoss, "ts"));
}

#[test]
fn postgres_to_trino_and_back() {
    let r = conv(&pg_tables(), "postgres", "trino");
    let p = &r.tables[1];
    assert_eq!(ty(p, "id"), "bigint");
    assert!(p.primary_key.is_none());
    assert!(r.issues.iter().any(|i| i.code == IssueCode::PrimaryKeyDropped));
    assert!(p.foreign_keys.is_empty() && p.indexes.is_empty());
    assert_eq!(ty(p, "total"), "decimal(12, 2)");
    assert_eq!(ty(p, "creado"), "timestamp(6) with time zone");
    // Trino takes only literals as defaults.
    assert_eq!(column(p, "creado").default_value, None);
    assert!(has(&r, IssueCode::DefaultDropped, "creado"));
    assert_eq!(column(p, "estado").default_value.as_deref(), Some("'nuevo'"));
    assert_eq!(ty(p, "local"), "timestamp(3)");
    assert_eq!(ty(p, "hora"), "time(6)");
    assert_eq!(ty(p, "codigo"), "uuid");
    assert_eq!(ty(p, "etiquetas"), "array(varchar)");
    assert_eq!(ty(p, "estado"), "varchar(20)");
    assert_eq!(ty(p, "bytes"), "varbinary");
    assert_eq!(ty(p, "ip"), "ipaddress");
    // Presto has no column defaults.
    let r = conv(&pg_tables(), "postgres", "presto");
    assert_eq!(column(&r.tables[1], "estado").default_value, None);
    assert!(has(&r, IssueCode::DefaultDropped, "estado"));

    let t = table(
        "t",
        vec![c("a", "array(varchar)"), c("m", "map(varchar, integer)"), c("ts", "timestamp(3) with time zone"), c("r", "real"), c("v", "varchar")],
        &[],
    );
    let r = conv(&[t], "trino", "sqlserver");
    let m = &r.tables[0];
    assert_eq!(ty(m, "a"), "nvarchar(max)");
    assert_eq!(ty(m, "ts"), "datetimeoffset(3)");
    assert_eq!(ty(m, "r"), "real");
    assert_eq!(ty(m, "v"), "nvarchar(max)");
}

#[test]
fn hive_family() {
    let r = conv(&pg_tables(), "postgres", "databricks");
    let p = &r.tables[1];
    assert_eq!(ty(p, "id"), "BIGINT");
    assert!(column(p, "id").auto_increment);
    assert_eq!(ty(p, "creado"), "TIMESTAMP");
    assert_eq!(column(p, "creado").default_value.as_deref(), Some("current_timestamp()"));
    assert_eq!(ty(p, "local"), "TIMESTAMP_NTZ");
    assert_eq!(ty(p, "etiquetas"), "ARRAY<STRING>");
    assert_eq!(ty(p, "estado"), "VARCHAR(20)");
    assert_eq!(p.foreign_keys.len(), 1);
    assert_eq!(p.foreign_keys[0].on_delete, None);
    // Identity columns are BIGINT in Delta.
    let r = conv(&[table("t", vec![nn(auto(c("id", "integer")))], &["id"])], "postgres", "databricks");
    assert_eq!(ty(&r.tables[0], "id"), "BIGINT");

    for id in ["hive", "spark", "kyuubi", "cloudera", "impala"] {
        let r = conv(&pg_tables(), "postgres", id);
        let p = &r.tables[1];
        assert!(p.primary_key.is_none(), "{id}");
        assert!(p.foreign_keys.is_empty() && p.indexes.is_empty(), "{id}");
        assert!(p.columns.iter().all(|c| c.default_value.is_none() && c.nullable && !c.auto_increment), "{id}");
    }
    let r = conv(&pg_tables(), "postgres", "hive");
    assert_eq!(ty(&r.tables[1], "creado"), "TIMESTAMP");
    assert!(has(&r, IssueCode::TimeZoneLoss, "creado"));
    assert_eq!(ty(&r.tables[1], "etiquetas"), "ARRAY<STRING>");
    let r = conv(&pg_tables(), "postgres", "impala");
    assert_eq!(ty(&r.tables[1], "etiquetas"), "STRING");
    assert!(r.issues.iter().any(|i| i.code == IssueCode::PrimaryKeyDropped && i.message.contains("Kudu")));
    let r = conv(&pg_tables(), "postgres", "spark");
    assert_eq!(ty(&r.tables[1], "local"), "TIMESTAMP_NTZ");

    let t = table("h", vec![c("a", "array<int>"), c("m", "map<string,bigint>"), c("s", "struct<x:int>"), c("ts", "timestamp"), c("d", "decimal(10,2)")], &[]);
    let r = conv(&[t], "hive", "postgres");
    let h = &r.tables[0];
    assert_eq!(ty(h, "a"), "integer[]");
    assert_eq!(ty(h, "m"), "jsonb");
    assert_eq!(ty(h, "s"), "jsonb");
    assert_eq!(ty(h, "ts"), "timestamp(6)");
    assert!(has(&r, IssueCode::PrecisionLoss, "ts"));
    assert_eq!(ty(h, "d"), "numeric(10, 2)");
}

#[test]
fn postgres_to_athena() {
    let r = conv(&pg_tables(), "postgres", "athena");
    let p = &r.tables[1];
    assert_eq!(ty(p, "id"), "BIGINT");
    assert_eq!(ty(p, "estado"), "STRING");
    assert_eq!(ty(p, "local"), "TIMESTAMP");
    assert_eq!(p.options.get("table_type").map(String::as_str), Some("iceberg"));
    assert!(r.issues.iter().any(|i| i.object.as_deref() == Some("location") && i.severity == Severity::Warning));
    let t = table("c", vec![c("n", "smallint")], &[]);
    let r = conv(&[t], "postgres", "athena");
    assert_eq!(ty(&r.tables[0], "n"), "INT");
}

// ---------------------------------------------------------------------------
// Against real servers

/// Read `names` from `src`, convert, create them on `dst` (in `schema`,
/// or the session's default), read them back.
async fn rt(
    src: &mut Box<dyn Session>,
    src_id: &str,
    dst: &mut Box<dyn Session>,
    dst_id: &str,
    names: &[&str],
    schema: Option<&str>,
) -> (Conversion, Vec<String>, Vec<TableSchema>) {
    rt_with(src, src_id, dst, dst_id, names, schema, |_| {}).await
}

/// [`rt`], with a last touch on the conversion before creating the tables
/// (what an emulator lacks).
async fn rt_with(
    src: &mut Box<dyn Session>,
    src_id: &str,
    dst: &mut Box<dyn Session>,
    dst_id: &str,
    names: &[&str],
    schema: Option<&str>,
    fix: fn(&mut Conversion),
) -> (Conversion, Vec<String>, Vec<TableSchema>) {
    let tables = read(src, names).await;
    assert_eq!(tables.len(), names.len(), "no se leyeron las tablas de origen: {:?}", tables.iter().map(|t| &t.name).collect::<Vec<_>>());
    let mut conversion = convert(&tables, src_id, dst_id, &Options::default()).expect("convert");
    for t in &mut conversion.tables {
        t.schema = schema.map(str::to_string);
        for fk in &mut t.foreign_keys {
            fk.ref_schema = schema.map(str::to_string);
        }
    }
    fix(&mut conversion);
    let d = dbine_drivers::find(dst_id).unwrap();
    for t in conversion.tables.iter().rev() {
        let drop = d.table_ddl(t, DdlParts { drop: true, if_exists: true, ..Default::default() }).unwrap_or_default();
        exec_quiet(dst, &[drop.as_str()]).await;
    }
    let ddl = target_ddl(dst_id, &conversion.tables);
    for script in &ddl {
        exec(dst, script).await;
    }
    let target_names: Vec<&str> = conversion.tables.iter().map(|t| t.name.as_str()).collect();
    let mut back = read(dst, &target_names).await;
    if let Some(s) = schema {
        back.retain(|t| t.schema.as_deref().is_none_or(|x| x.eq_ignore_ascii_case(s)));
    }
    assert_eq!(back.len(), target_names.len(), "no se releyeron las tablas creadas:\n{}", ddl.join("\n"));
    (conversion, ddl, back)
}

fn show(c: &Conversion, ddl: &[String], back: &[TableSchema]) {
    for i in &c.issues {
        eprintln!("  {:?} {:?} {}.{}: {}", i.severity, i.code, i.table, i.object.as_deref().unwrap_or(""), i.message);
    }
    eprintln!("{}", ddl.join("\n"));
    for t in back {
        eprintln!("{}: {:?}", t.name, t.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable)).collect::<Vec<_>>());
    }
}

async fn session_cfg(cfg: ConnectionConfig) -> Box<dyn Session> {
    let d = dbine_drivers::find(&cfg.driver).unwrap();
    d.connect(&cfg, None).await.unwrap_or_else(|e| panic!("{}: {e}", cfg.driver))
}

async fn pg() -> Option<Box<dyn Session>> {
    common::session("postgres", "DBINE_TEST_POSTGRES_URL").await
}

/// The PostgreSQL source, rich in types, with one row of edge values.
async fn pg_seed(s: &mut Box<dyn Session>) {
    exec_quiet(s, &["DROP SCHEMA IF EXISTS an_back CASCADE", "DROP TABLE IF EXISTS an_pedidos", "DROP TABLE IF EXISTS an_clientes"]).await;
    exec(
        s,
        "CREATE TABLE an_clientes (id integer PRIMARY KEY, nombre varchar(120) NOT NULL);
         CREATE TABLE an_pedidos (
           id bigserial PRIMARY KEY,
           cliente_id integer NOT NULL REFERENCES an_clientes(id) ON DELETE CASCADE,
           chico smallint,
           total numeric(12,2) DEFAULT 0,
           grande numeric(38,0),
           ratio double precision,
           creado timestamptz DEFAULT now(),
           local_ts timestamp(3),
           dia date,
           activo boolean DEFAULT true,
           codigo uuid DEFAULT gen_random_uuid(),
           datos jsonb,
           etiquetas text[],
           estado varchar(20) DEFAULT 'nuevo',
           notas text,
           bytes bytea
         );
         CREATE INDEX an_ix_creado ON an_pedidos (creado);
         INSERT INTO an_clientes VALUES (1, 'Ñandú 漢字 🎉');
         INSERT INTO an_pedidos (cliente_id, chico, total, grande, ratio, creado, local_ts, dia, activo, datos, etiquetas, estado, notas, bytes)
         VALUES (1, 32767, 9999999999.99, 99999999999999999999999999999999999999, 1.5e300,
                 '2024-02-29 23:59:59.999999+05:30', '1999-12-31 23:59:59.999', '2024-02-29', false,
                 '{\"a\": [1, 2]}', ARRAY['x', 'y''z'], 'ñandú', 'línea\nnueva', '\\x00ff');",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn live_postgres_clickhouse_both_ways() {
    let Some(mut p) = pg().await else { return };
    let Some(mut ch) = common::session("clickhouse", "DBINE_TEST_CLICKHOUSE_URL").await else { return };
    pg_seed(&mut p).await;

    // PostgreSQL → ClickHouse.
    let (conv1, ddl, back) = rt(&mut p, "postgres", &mut ch, "clickhouse", &["an_clientes", "an_pedidos"], None).await;
    show(&conv1, &ddl, &back);
    let t = &back[1];
    assert_eq!(col(t, "id").data_type, "Int64");
    assert!(!col(t, "id").nullable);
    assert_eq!(col(t, "chico").data_type, "Int16");
    assert_eq!(col(t, "total").data_type, "Decimal(12, 2)");
    assert_eq!(col(t, "grande").data_type, "Decimal(38, 0)");
    assert_eq!(col(t, "creado").data_type, "DateTime64(6)");
    assert_eq!(col(t, "local_ts").data_type, "DateTime64(3)");
    assert_eq!(col(t, "dia").data_type, "Date32");
    assert_eq!(col(t, "activo").data_type, "Bool");
    assert_eq!(col(t, "codigo").data_type, "UUID");
    assert_eq!(col(t, "etiquetas").data_type, "Array(String)");
    assert_eq!(t.options.get("engine").map(String::as_str), Some("MergeTree"));
    exec(
        &mut ch,
        "INSERT INTO an_clientes VALUES (1, 'Ñandú 漢字 🎉');
         INSERT INTO an_pedidos (id, cliente_id, chico, total, grande, ratio, creado, local_ts, dia, activo, datos, etiquetas, estado, notas, bytes)
         VALUES (9223372036854775807, 1, 32767, 9999999999.99, 99999999999999999999999999999999999999, 1.5e300,
                 '2024-02-29 18:29:59.999999', '1999-12-31 23:59:59.999', '2024-02-29', false,
                 '{\"a\": [1, 2]}', ['x', 'y\\'z'], 'ñandú', 'línea\\nnueva', unhex('00ff'));",
    )
    .await;

    // …and back to PostgreSQL, in another schema.
    exec(&mut p, "DROP SCHEMA IF EXISTS an_back CASCADE; CREATE SCHEMA an_back").await;
    let (conv2, ddl, back2) = rt(&mut ch, "clickhouse", &mut p, "postgres", &["an_clientes", "an_pedidos"], Some("an_back")).await;
    show(&conv2, &ddl, &back2);
    let t = back2.iter().find(|t| t.name == "an_pedidos" && t.schema.as_deref() == Some("an_back")).unwrap();
    assert_eq!(col(t, "id").data_type, "bigint");
    assert_eq!(col(t, "total").data_type, "numeric(12,2)");
    assert_eq!(col(t, "creado").data_type, "timestamp(6) with time zone");
    assert_eq!(col(t, "activo").data_type, "boolean");
    assert_eq!(col(t, "codigo").data_type, "uuid");
    assert_eq!(col(t, "etiquetas").data_type, "text[]");
    exec(&mut p, "DROP SCHEMA an_back CASCADE").await;
}

async fn my() -> Option<Box<dyn Session>> {
    common::session("mysql", "DBINE_TEST_MYSQL_URL").await
}

/// The MySQL source: unsigned integers, enum / set, year, datetime vs timestamp.
async fn my_seed(s: &mut Box<dyn Session>) {
    exec_quiet(s, &["DROP TABLE IF EXISTS an_items"]).await;
    exec(
        s,
        "CREATE TABLE an_items (
           id int unsigned NOT NULL AUTO_INCREMENT PRIMARY KEY,
           u8 tinyint unsigned,
           big bigint unsigned,
           flag tinyint(1) DEFAULT 1,
           precio decimal(10,2) NOT NULL DEFAULT 0.00,
           tipo enum('alta','baja') DEFAULT 'alta',
           anio year,
           alta datetime(3) DEFAULT CURRENT_TIMESTAMP(3),
           ts timestamp NULL,
           nombre varchar(50) CHARACTER SET utf8mb4,
           doc json,
           raw varbinary(16)
         );
         INSERT INTO an_items (u8, big, flag, precio, tipo, anio, alta, ts, nombre, doc, raw)
         VALUES (255, 18446744073709551615, 0, 99999999.99, 'baja', 2155, '9999-12-31 23:59:59.999',
                 '2038-01-19 03:14:07', 'ñandú 漢字 🎉', '{\"k\": 1}', 0x00ff);",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn live_mysql_to_clickhouse_and_back() {
    let Some(mut m) = my().await else { return };
    let Some(mut ch) = common::session("clickhouse", "DBINE_TEST_CLICKHOUSE_URL").await else { return };
    my_seed(&mut m).await;
    let (conv, ddl, back) = rt(&mut m, "mysql", &mut ch, "clickhouse", &["an_items"], None).await;
    show(&conv, &ddl, &back);
    let t = &back[0];
    assert_eq!(col(t, "id").data_type, "UInt32");
    assert_eq!(col(t, "u8").data_type, "UInt8");
    assert_eq!(col(t, "big").data_type, "UInt64");
    assert_eq!(col(t, "flag").data_type, "Bool");
    assert_eq!(col(t, "tipo").data_type, "Enum8('alta' = 1, 'baja' = 2)");
    assert_eq!(col(t, "anio").data_type, "UInt16");
    assert_eq!(col(t, "alta").data_type, "DateTime64(3)");
    exec(
        &mut ch,
        "INSERT INTO an_items (id, u8, big, flag, precio, tipo, anio, alta, ts, nombre, doc, raw)
         VALUES (4294967295, 255, 18446744073709551615, 0, 99999999.99, 'baja', 2155, '2299-12-31 23:59:59.999',
                 '2038-01-19 03:14:07', 'ñandú 漢字 🎉', '{\"k\": 1}', unhex('00ff'))",
    )
    .await;
    // ClickHouse → MySQL: unsigned and the enum come back.
    exec_quiet(&mut m, &["DROP TABLE IF EXISTS an_items"]).await;
    let (conv, ddl, back) = rt(&mut ch, "clickhouse", &mut m, "mysql", &["an_items"], None).await;
    show(&conv, &ddl, &back);
    let t = &back[0];
    assert_eq!(col(t, "id").data_type, "int unsigned");
    assert_eq!(col(t, "big").data_type, "bigint unsigned");
    assert_eq!(col(t, "tipo").data_type, "enum('alta','baja')");
    assert_eq!(col(t, "alta").data_type, "datetime(3)");
    // JSON is never NULL in ClickHouse, so it comes back NOT NULL.
    assert!(!col(t, "doc").nullable);
    exec(&mut m, "INSERT INTO an_items (id, big, tipo, alta, doc) VALUES (4294967295, 18446744073709551615, 'baja', '2299-12-31 23:59:59.999', '{}')").await;
}

#[tokio::test]
#[ignore]
async fn live_postgres_timeplus_both_ways() {
    let Some(mut p) = pg().await else { return };
    let Some(mut tp) = common::session("timeplus", "DBINE_TEST_TIMEPLUS_URL").await else { return };
    pg_seed(&mut p).await;
    let (conv, ddl, back) = rt(&mut p, "postgres", &mut tp, "timeplus", &["an_clientes", "an_pedidos"], None).await;
    show(&conv, &ddl, &back);
    let t = &back[1];
    assert_eq!(col(t, "id").data_type, "int64");
    assert_eq!(col(t, "total").data_type, "decimal(12, 2)");
    assert_eq!(col(t, "creado").data_type, "datetime64(6)");
    assert_eq!(col(t, "etiquetas").data_type, "array(string)");
    assert_eq!(t.options.get("mode").map(String::as_str), Some("versioned_kv"));
    assert_eq!(t.primary_key.as_ref().map(|k| k.columns.clone()), Some(vec!["id".to_string()]));
    exec(
        &mut tp,
        "INSERT INTO an_pedidos (id, cliente_id, chico, total, grande, ratio, creado, local_ts, dia, activo, etiquetas, estado, notas)
         VALUES (9223372036854775807, 1, 32767, 9999999999.99, 99999999999999999999999999999999999999, 1.5e300,
                 '2024-02-29 18:29:59.999999', '1999-12-31 23:59:59.999', '2024-02-29', false, ['x', 'y\\'z'], 'ñandú', 'línea\\nnueva')",
    )
    .await;
    exec(&mut p, "DROP SCHEMA IF EXISTS an_back CASCADE; CREATE SCHEMA an_back").await;
    let (conv, ddl, back) = rt(&mut tp, "timeplus", &mut p, "postgres", &["an_clientes", "an_pedidos"], Some("an_back")).await;
    show(&conv, &ddl, &back);
    let t = back.iter().find(|t| t.name == "an_pedidos").unwrap();
    assert_eq!(col(t, "id").data_type, "bigint");
    assert_eq!(col(t, "etiquetas").data_type, "text[]");
    assert_eq!(t.primary_key.as_ref().map(|k| k.columns.clone()), Some(vec!["id".to_string()]));
    exec(&mut p, "DROP SCHEMA an_back CASCADE").await;
    exec_quiet(&mut tp, &["DROP STREAM IF EXISTS an_pedidos", "DROP STREAM IF EXISTS an_clientes"]).await;
}

async fn duck(name: &str) -> Box<dyn Session> {
    let path = std::env::temp_dir().join(format!("dbine-schema-{name}-{}.duckdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    session_cfg(ConnectionConfig { driver: "duckdb".into(), host: path.to_string_lossy().into(), ..Default::default() }).await
}

#[tokio::test]
#[ignore]
async fn live_postgres_duckdb_both_ways() {
    let Some(mut p) = pg().await else { return };
    let mut d = duck("pg").await;
    pg_seed(&mut p).await;
    let (conv, ddl, back) = rt(&mut p, "postgres", &mut d, "duckdb", &["an_clientes", "an_pedidos"], Some("main")).await;
    show(&conv, &ddl, &back);
    let t = &back[1];
    assert_eq!(col(t, "id").data_type, "BIGINT");
    assert!(col(t, "id").auto_increment, "sequence default");
    assert_eq!(col(t, "total").data_type, "DECIMAL(12,2)");
    assert_eq!(col(t, "grande").data_type, "DECIMAL(38,0)");
    assert_eq!(col(t, "creado").data_type, "TIMESTAMP WITH TIME ZONE");
    assert_eq!(col(t, "local_ts").data_type, "TIMESTAMP");
    assert_eq!(col(t, "codigo").data_type, "UUID");
    assert_eq!(col(t, "datos").data_type, "JSON");
    assert_eq!(col(t, "etiquetas").data_type, "VARCHAR[]");
    assert_eq!(col(t, "bytes").data_type, "BLOB");
    assert_eq!(t.foreign_keys.len(), 1);
    exec(
        &mut d,
        "INSERT INTO an_clientes VALUES (1, 'Ñandú 漢字 🎉');
         INSERT INTO an_pedidos (cliente_id, chico, total, grande, ratio, creado, local_ts, dia, activo, datos, etiquetas, estado, notas, bytes)
         VALUES (1, 32767, 9999999999.99, 99999999999999999999999999999999999999, 1.5e300,
                 '2024-02-29 23:59:59.999999+05:30', '1999-12-31 23:59:59.999', '2024-02-29', false,
                 '{\"a\": [1, 2]}', ['x', 'y''z'], 'ñandú', 'línea
nueva', '\\x00\\xFF'::BLOB);",
    )
    .await;

    // DuckDB → PostgreSQL.
    exec(&mut p, "DROP SCHEMA IF EXISTS an_back CASCADE; CREATE SCHEMA an_back").await;
    let (conv, ddl, back) = rt(&mut d, "duckdb", &mut p, "postgres", &["an_clientes", "an_pedidos"], Some("an_back")).await;
    show(&conv, &ddl, &back);
    let t = back.iter().find(|t| t.name == "an_pedidos").unwrap();
    assert_eq!(col(t, "id").data_type, "bigint");
    assert!(col(t, "id").auto_increment);
    assert_eq!(col(t, "total").data_type, "numeric(12,2)");
    assert_eq!(col(t, "creado").data_type, "timestamp(6) with time zone");
    assert_eq!(col(t, "codigo").data_type, "uuid");
    assert_eq!(col(t, "etiquetas").data_type, "text[]");
    assert_eq!(col(t, "bytes").data_type, "bytea");
    assert!(col(t, "codigo").default_value.as_deref().is_some_and(|d| d.contains("gen_random_uuid")), "{:?}", col(t, "codigo").default_value);
    assert_eq!(t.foreign_keys.len(), 1);
    exec(&mut p, "DROP SCHEMA an_back CASCADE").await;
}

#[tokio::test]
#[ignore]
async fn live_mysql_to_duckdb() {
    let Some(mut m) = my().await else { return };
    let mut d = duck("my").await;
    my_seed(&mut m).await;
    let (conv, ddl, back) = rt(&mut m, "mysql", &mut d, "duckdb", &["an_items"], Some("main")).await;
    show(&conv, &ddl, &back);
    let t = &back[0];
    assert_eq!(col(t, "id").data_type, "UINTEGER");
    assert_eq!(col(t, "big").data_type, "UBIGINT");
    assert_eq!(col(t, "tipo").data_type, "ENUM('alta', 'baja')");
    assert_eq!(col(t, "ts").data_type, "TIMESTAMP WITH TIME ZONE");
    exec(
        &mut d,
        "INSERT INTO an_items (u8, big, flag, precio, tipo, anio, alta, ts, nombre, doc, raw)
         VALUES (255, 18446744073709551615, false, 99999999.99, 'baja', 2155, '9999-12-31 23:59:59.999',
                 '2038-01-19 03:14:07+00', 'ñandú 漢字 🎉', '{\"k\": 1}', '\\x00\\xFF'::BLOB)",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn live_postgres_trino_both_ways() {
    let Some(mut p) = pg().await else { return };
    let Some(mut tr) = common::session("trino", "DBINE_TEST_TRINO_URL").await else { return };
    pg_seed(&mut p).await;
    exec(&mut tr, "CREATE SCHEMA IF NOT EXISTS memory.an").await;
    let (conv, ddl, back) = rt(&mut p, "postgres", &mut tr, "trino", &["an_clientes", "an_pedidos"], Some("an")).await;
    show(&conv, &ddl, &back);
    let t = &back[1];
    assert_eq!(col(t, "id").data_type, "bigint");
    assert!(!col(t, "id").nullable);
    assert_eq!(col(t, "total").data_type, "decimal(12,2)");
    assert_eq!(col(t, "total").default_value.as_deref(), Some("0"));
    assert_eq!(col(t, "creado").data_type, "timestamp(6) with time zone");
    assert_eq!(col(t, "local_ts").data_type, "timestamp(3)");
    assert_eq!(col(t, "codigo").data_type, "uuid");
    assert_eq!(col(t, "datos").data_type, "json");
    assert_eq!(col(t, "etiquetas").data_type, "array(varchar)");
    assert_eq!(col(t, "estado").data_type, "varchar(20)");
    assert_eq!(col(t, "bytes").data_type, "varbinary");
    exec(
        &mut tr,
        "INSERT INTO memory.an.an_pedidos (id, cliente_id, chico, total, grande, ratio, creado, local_ts, dia, activo, codigo, datos, etiquetas, estado, notas, bytes)
         VALUES (9223372036854775807, 1, SMALLINT '32767', DECIMAL '9999999999.99', DECIMAL '99999999999999999999999999999999999999', 1.5e300,
                 TIMESTAMP '2024-02-29 23:59:59.999999 +05:30', TIMESTAMP '1999-12-31 23:59:59.999', DATE '2024-02-29', false,
                 UUID '12151fd2-7586-11e9-8f9e-2a86e4085a59', JSON '{\"a\": [1, 2]}', ARRAY['x', 'y''z'], 'ñandú', 'línea
nueva', X'00ff')",
    )
    .await;

    exec(&mut p, "DROP SCHEMA IF EXISTS an_back CASCADE; CREATE SCHEMA an_back").await;
    let (conv, ddl, back) = rt(&mut tr, "trino", &mut p, "postgres", &["an_clientes", "an_pedidos"], Some("an_back")).await;
    show(&conv, &ddl, &back);
    let t = back.iter().find(|t| t.name == "an_pedidos").unwrap();
    assert_eq!(col(t, "id").data_type, "bigint");
    assert_eq!(col(t, "creado").data_type, "timestamp(6) with time zone");
    assert_eq!(col(t, "local_ts").data_type, "timestamp(3) without time zone");
    assert_eq!(col(t, "codigo").data_type, "uuid");
    assert_eq!(col(t, "datos").data_type, "json");
    assert_eq!(col(t, "etiquetas").data_type, "text[]");
    assert_eq!(col(t, "estado").data_type, "character varying(20)");
    assert!(t.primary_key.is_none(), "Trino had no key to give back");
    exec(&mut p, "DROP SCHEMA an_back CASCADE").await;
    exec_quiet(&mut tr, &["DROP TABLE memory.an.an_pedidos", "DROP TABLE memory.an.an_clientes"]).await;
}

#[tokio::test]
#[ignore]
async fn live_mysql_to_trino() {
    let Some(mut m) = my().await else { return };
    let Some(mut tr) = common::session("trino", "DBINE_TEST_TRINO_URL").await else { return };
    my_seed(&mut m).await;
    exec(&mut tr, "CREATE SCHEMA IF NOT EXISTS memory.an").await;
    let (conv, ddl, back) = rt(&mut m, "mysql", &mut tr, "trino", &["an_items"], Some("an")).await;
    show(&conv, &ddl, &back);
    let t = &back[0];
    assert_eq!(col(t, "id").data_type, "bigint");
    assert_eq!(col(t, "u8").data_type, "smallint");
    assert_eq!(col(t, "big").data_type, "decimal(20,0)");
    assert_eq!(col(t, "tipo").data_type, "varchar(4)");
    exec(
        &mut tr,
        "INSERT INTO memory.an.an_items (id, u8, big, flag, precio, tipo, anio, alta, ts, nombre, doc, raw)
         VALUES (4294967295, SMALLINT '255', DECIMAL '18446744073709551615', false, DECIMAL '99999999.99', 'baja', SMALLINT '2155',
                 TIMESTAMP '9999-12-31 23:59:59.999', TIMESTAMP '2038-01-19 03:14:07 UTC', 'ñandú 漢字 🎉', JSON '{\"k\": 1}', X'00ff')",
    )
    .await;
    exec_quiet(&mut tr, &["DROP TABLE memory.an.an_items"]).await;
}

async fn bq() -> Option<Box<dyn Session>> {
    let url = std::env::var("DBINE_TEST_BIGQUERY_URL").ok()?;
    let mut c = ConnectionConfig { driver: "bigquery".into(), ..Default::default() };
    c.options.insert("project_id".into(), "test".into());
    c.options.insert("endpoint_url".into(), url);
    let d = dbine_drivers::find("bigquery").unwrap();
    Some(d.connect(&c, Some("ds1")).await.expect("bigquery"))
}

#[tokio::test]
#[ignore]
async fn live_postgres_bigquery_both_ways() {
    let Some(mut p) = pg().await else { return };
    let Some(mut b) = bq().await else { return };
    pg_seed(&mut p).await;
    // The emulator has no foreign keys (BigQuery has them, NOT ENFORCED).
    let (conv, ddl, back) = rt_with(&mut p, "postgres", &mut b, "bigquery", &["an_clientes", "an_pedidos"], Some("ds1"), |c| {
        assert_eq!(c.tables[1].foreign_keys.len(), 1);
        c.tables[1].foreign_keys.clear();
    })
    .await;
    show(&conv, &ddl, &back);
    let t = &back[1];
    assert_eq!(col(t, "id").data_type, "INT64");
    assert!(!col(t, "id").nullable);
    assert!(col(t, "total").data_type.starts_with("NUMERIC"));
    assert_eq!(col(t, "creado").data_type, "TIMESTAMP");
    assert_eq!(col(t, "local_ts").data_type, "DATETIME");
    assert_eq!(col(t, "codigo").data_type, "STRING");
    assert_eq!(col(t, "datos").data_type, "JSON");
    assert_eq!(col(t, "etiquetas").data_type, "ARRAY<STRING>");
    assert_eq!(col(t, "bytes").data_type, "BYTES");
    // The emulator reports FLOAT64 as DOUBLE and drops the parameters of
    // NUMERIC(p, s) / STRING(n); real BigQuery keeps them.
    assert!(["FLOAT64", "DOUBLE"].contains(&col(t, "ratio").data_type.as_str()));
    exec(
        &mut b,
        "INSERT INTO ds1.an_pedidos (id, cliente_id, chico, total, grande, ratio, creado, local_ts, dia, activo, codigo, datos, etiquetas, estado, notas, bytes)
         VALUES (9223372036854775807, 1, 32767, NUMERIC '9999999999.99', BIGNUMERIC '99999999999999999999999999999999999999', 1.5e300,
                 TIMESTAMP '2024-02-29 23:59:59.999999+05:30', DATETIME '1999-12-31 23:59:59.999', DATE '2024-02-29', false,
                 '12151fd2-7586-11e9-8f9e-2a86e4085a59', JSON '{\"a\": [1, 2]}', ['x', 'y\\'z'], 'ñandú', 'línea\\nnueva', b'\\x00\\xff')",
    )
    .await;

    exec(&mut p, "DROP SCHEMA IF EXISTS an_back CASCADE; CREATE SCHEMA an_back").await;
    let (conv, ddl, back) = rt(&mut b, "bigquery", &mut p, "postgres", &["an_clientes", "an_pedidos"], Some("an_back")).await;
    show(&conv, &ddl, &back);
    let t = back.iter().find(|t| t.name == "an_pedidos").unwrap();
    assert_eq!(col(t, "id").data_type, "bigint");
    assert_eq!(col(t, "creado").data_type, "timestamp(6) with time zone");
    assert_eq!(col(t, "local_ts").data_type, "timestamp(6) without time zone");
    assert_eq!(col(t, "datos").data_type, "jsonb");
    assert_eq!(col(t, "etiquetas").data_type, "text[]");
    exec(&mut p, "DROP SCHEMA an_back CASCADE").await;
}

/// A session on the Spanner emulator, creating instance `i1` and database
/// `an` in project `test` if missing.
async fn spanner() -> Option<Box<dyn Session>> {
    let url = std::env::var("DBINE_TEST_SPANNER_URL").ok()?;
    let post = |path: &str, body: &str| {
        let _ = std::process::Command::new("curl")
            .args(["-s", "-X", "POST", &format!("{url}{path}"), "-H", "Content-Type: application/json", "-d", body])
            .output();
    };
    post(
        "/v1/projects/test/instances",
        r#"{"instanceId": "i1", "instance": {"config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1}}"#,
    );
    post("/v1/projects/test/instances/i1/databases", r#"{"createStatement": "CREATE DATABASE `an`"}"#);
    let mut cfg = ConnectionConfig { driver: "spanner".into(), database: "an".into(), ..Default::default() };
    for (k, v) in [("project_id", "test"), ("instance", "i1"), ("endpoint_url", url.as_str())] {
        cfg.options.insert(k.into(), v.into());
    }
    Some(session_cfg(cfg).await)
}

#[tokio::test]
#[ignore]
async fn live_postgres_spanner_both_ways() {
    let Some(mut p) = pg().await else { return };
    let Some(mut sp) = spanner().await else { return };
    pg_seed(&mut p).await;
    exec(&mut p, "DROP TABLE IF EXISTS an_sin_clave; CREATE TABLE an_sin_clave (x integer, y text)").await;
    let (conv, ddl, back) = rt(&mut p, "postgres", &mut sp, "spanner", &["an_clientes", "an_pedidos", "an_sin_clave"], None).await;
    show(&conv, &ddl, &back);
    let t = &back[1];
    assert_eq!(col(t, "id").data_type, "INT64");
    assert!(col(t, "id").auto_increment);
    assert_eq!(col(t, "total").data_type, "NUMERIC");
    assert_eq!(col(t, "creado").data_type, "TIMESTAMP");
    assert_eq!(col(t, "codigo").data_type, "STRING(36)");
    assert_eq!(col(t, "datos").data_type, "JSON");
    assert_eq!(col(t, "etiquetas").data_type, "ARRAY<STRING(MAX)>");
    assert_eq!(col(t, "estado").data_type, "STRING(20)");
    assert_eq!(col(t, "bytes").data_type, "BYTES(MAX)");
    assert_eq!(t.foreign_keys.len(), 1);
    assert_eq!(t.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    assert_eq!(back[2].primary_key.as_ref().map(|k| k.columns.clone()), Some(vec!["row_id".to_string()]));
    exec(
        &mut sp,
        "INSERT INTO an_clientes (id, nombre) VALUES (1, 'Ñandú 漢字 🎉');
         INSERT INTO an_pedidos (cliente_id, chico, total, grande, ratio, creado, local_ts, dia, activo, datos, etiquetas, estado, notas, bytes)
         VALUES (1, 32767, NUMERIC '9999999999.99', NUMERIC '99999999999999999999999999999', 1.5e300,
                 TIMESTAMP '2024-02-29 23:59:59.999999+05:30', TIMESTAMP '1999-12-31 23:59:59.999', DATE '2024-02-29', false,
                 JSON '{\"a\": [1, 2]}', ['x', 'y\\x27z'], 'ñandú', 'línea\\nnueva', b'\\x00\\xff');
         INSERT INTO an_sin_clave (x, y) VALUES (2147483647, 'sin clave')",
    )
    .await;

    // Spanner → PostgreSQL.
    exec(&mut p, "DROP SCHEMA IF EXISTS an_back CASCADE; CREATE SCHEMA an_back").await;
    let (conv, ddl, back) = rt(&mut sp, "spanner", &mut p, "postgres", &["an_clientes", "an_pedidos", "an_sin_clave"], Some("an_back")).await;
    show(&conv, &ddl, &back);
    let t = back.iter().find(|t| t.name == "an_pedidos").unwrap();
    assert_eq!(col(t, "id").data_type, "bigint");
    assert!(col(t, "id").auto_increment);
    assert_eq!(col(t, "total").data_type, "numeric(38,9)");
    assert_eq!(col(t, "creado").data_type, "timestamp(6) with time zone");
    assert_eq!(col(t, "etiquetas").data_type, "text[]");
    assert_eq!(col(t, "estado").data_type, "character varying(20)");
    assert_eq!(t.foreign_keys.len(), 1);
    exec(&mut p, "DROP SCHEMA an_back CASCADE; DROP TABLE an_sin_clave").await;
    for t in ["an_sin_clave", "an_pedidos", "an_clientes"] {
        let d = dbine_drivers::find("spanner").unwrap();
        let ts = back.iter().find(|x| x.name == t).cloned().unwrap_or_else(|| TableSchema { name: t.into(), ..Default::default() });
        let drop = d.table_ddl(&ts, DdlParts { drop: true, if_exists: true, ..Default::default() }).unwrap();
        exec_quiet(&mut sp, &[drop.as_str()]).await;
    }
}

#[tokio::test]
#[ignore]
async fn live_mysql_spanner_both_ways() {
    let Some(mut m) = my().await else { return };
    let Some(mut sp) = spanner().await else { return };
    my_seed(&mut m).await;
    let (conv, ddl, back) = rt(&mut m, "mysql", &mut sp, "spanner", &["an_items"], None).await;
    show(&conv, &ddl, &back);
    let t = &back[0];
    assert_eq!(col(t, "id").data_type, "INT64");
    assert!(col(t, "id").auto_increment);
    assert_eq!(col(t, "big").data_type, "NUMERIC");
    assert_eq!(col(t, "tipo").data_type, "STRING(4)");
    assert_eq!(col(t, "nombre").data_type, "STRING(50)");
    assert_eq!(col(t, "raw").data_type, "BYTES(16)");
    exec(
        &mut sp,
        "INSERT INTO an_items (u8, big, flag, precio, tipo, anio, alta, ts, nombre, doc, raw)
         VALUES (255, NUMERIC '18446744073709551615', false, NUMERIC '99999999.99', 'baja', 2155, TIMESTAMP '9999-12-31 23:59:59.999+00',
                 TIMESTAMP '2038-01-19 03:14:07Z', 'ñandú 漢字 🎉', JSON '{\"k\": 1}', b'\\x00\\xff')",
    )
    .await;
    exec_quiet(&mut m, &["DROP TABLE IF EXISTS an_items"]).await;
    let (conv, ddl, back) = rt(&mut sp, "spanner", &mut m, "mysql", &["an_items"], None).await;
    show(&conv, &ddl, &back);
    let t = &back[0];
    assert_eq!(col(t, "id").data_type, "bigint");
    assert!(col(t, "id").auto_increment);
    assert_eq!(col(t, "nombre").data_type, "varchar(50)");
    assert_eq!(col(t, "raw").data_type, "varbinary(16)");
    assert_eq!(col(t, "doc").data_type, "json");
    exec(&mut m, "INSERT INTO an_items (big, precio, alta, nombre, raw) VALUES (18446744073709551615, 99999999.99, '9999-12-31 23:59:59.999999', 'ñandú 漢字 🎉', 0x00ff)").await;
    let d = dbine_drivers::find("spanner").unwrap();
    let drop = d.table_ddl(&back[0], DdlParts { drop: true, if_exists: true, ..Default::default() }).unwrap();
    exec_quiet(&mut sp, &[drop.as_str()]).await;
}

#[tokio::test]
#[ignore]
async fn live_mysql_to_bigquery() {
    let Some(mut m) = my().await else { return };
    let Some(mut b) = bq().await else { return };
    my_seed(&mut m).await;
    let (conv, ddl, back) = rt(&mut m, "mysql", &mut b, "bigquery", &["an_items"], Some("ds1")).await;
    show(&conv, &ddl, &back);
    let t = &back[0];
    assert_eq!(col(t, "id").data_type, "INT64");
    assert!(col(t, "big").data_type.starts_with("NUMERIC"));
    assert_eq!(col(t, "alta").data_type, "DATETIME");
    assert_eq!(col(t, "ts").data_type, "TIMESTAMP");
    exec(
        &mut b,
        "INSERT INTO ds1.an_items (id, u8, big, flag, precio, tipo, anio, alta, ts, nombre, doc, raw)
         VALUES (4294967295, 255, NUMERIC '18446744073709551615', false, NUMERIC '99999999.99', 'baja', 2155, DATETIME '9999-12-31 23:59:59.999',
                 TIMESTAMP '2038-01-19 03:14:07+00', 'ñandú 漢字 🎉', JSON '{\"k\": 1}', b'\\x00\\xff')",
    )
    .await;
}
