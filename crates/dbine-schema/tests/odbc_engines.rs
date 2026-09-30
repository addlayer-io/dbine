//! End-to-end conversions for the engines served through ODBC presets
//! (Ingres, Zen, Altibase, CUBRID, Dameng, HeavyDB, IRIS/Caché, Machbase,
//! Access, dBase, Mimer, MonetDB, NuoDB, Ocient, Virtuoso, NetSuite,
//! OpenEdge, MaxDB, SQream, Ignite 2 and 3) and the generic `odbc` preset.
//!
//! Tables are spelled the way the ODBC driver's `database_schema` reports
//! them (SQLColumns TYPE_NAME plus `(size)` / `(p,s)`), and every converted
//! table goes through the real ODBC driver's `table_ddl`, so the DDL each
//! preset would run is checked too. No server is needed for those.
//!
//! Live (only the generic preset has a driver and a server here): SQL Server
//! through Microsoft's ODBC driver, against PostgreSQL.
//!
//! ```text
//! docker start dbine-test-postgres dbine-test-sqlserver
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_ODBC_CONN='DRIVER={ODBC Driver 18 for SQL Server};SERVER=127.0.0.1,25013;UID=sa;PWD={Pw_12345!};TrustServerCertificate=yes;DATABASE=master' \
//!   cargo test -p dbine-schema --test odbc_engines -- --ignored --test-threads=1
//! ```

mod common;

use dbine_driver::{ColumnDef, ConnectionConfig, DdlParts, ForeignKeyDef, IndexDef, KeyDef, QueryOutcome, Session, TableSchema};
use dbine_schema::convert::logical_of;
use dbine_schema::dialect::for_driver;
use dbine_schema::parse::parse;
use dbine_schema::{convert, Conversion, IssueCode, LogicalType as L, Options, Severity};

const ENGINES: &[&str] = &[
    "odbc", "netsuite", "ingres", "zen", "altibase", "cubrid", "dameng", "heavydb", "iris", "cache", "machbase", "access", "dbase",
    "mimer", "monetdb", "nuodb", "ocient", "virtuoso", "openedge", "maxdb", "sqream", "ignite", "ignite3",
];

// ------------------------------------------------------------------ helpers

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

fn column<'a>(t: &'a TableSchema, c: &str) -> &'a ColumnDef {
    t.columns.iter().find(|x| x.name.eq_ignore_ascii_case(c)).unwrap_or_else(|| panic!("no column {c} in {}: {:?}", t.name, t.columns))
}

fn ty<'a>(t: &'a TableSchema, c: &str) -> &'a str {
    &column(t, c).data_type
}

fn has(r: &Conversion, code: IssueCode, object: &str) -> bool {
    r.issues.iter().any(|i| i.code == code && i.object.as_deref().is_some_and(|o| o.eq_ignore_ascii_case(object)))
}

fn run(tables: &[TableSchema], from: &str, to: &str) -> Conversion {
    convert(tables, from, to, &Options::default()).unwrap_or_else(|e| panic!("{from} → {to}: {e}"))
}

/// The DDL the real driver writes for the converted tables.
fn ddl(driver: &str, tables: &[TableSchema]) -> String {
    let d = dbine_drivers::find(driver).unwrap_or_else(|| panic!("no driver {driver}"));
    let parts = DdlParts { create: true, indexes: true, foreign_keys: true, ..Default::default() };
    tables.iter().map(|t| d.table_ddl(t, parts).unwrap_or_else(|e| panic!("{driver} table_ddl {}: {e}", t.name))).collect::<Vec<_>>().join("\n")
}

/// The converted tables are consistent with the target: every type is one
/// its dialect reads, nothing the caps exclude survives, and the driver's
/// DDL writes every column with the chosen type.
fn check_target(r: &Conversion, to: &str) {
    let d = for_driver(to).unwrap();
    let caps = d.caps();
    let designer = dbine_drivers::find(to).and_then(|x| x.designer());
    let script = ddl(to, &r.tables);
    for t in &r.tables {
        for c in &t.columns {
            let l = logical_of(d, &parse(&c.data_type));
            assert!(!matches!(l, L::Other { .. }), "{to}: {}.{} → {} no se lee de vuelta", t.name, c.name, c.data_type);
            assert!(c.name.len() <= caps.max_identifier, "{to}: {} supera {}", c.name, caps.max_identifier);
            assert!(caps.defaults || c.default_value.is_none(), "{to}: default en {}", c.name);
            assert!(caps.nullability || c.nullable, "{to}: NOT NULL en {}", c.name);
            assert!(caps.auto_increment || !c.auto_increment, "{to}: autoincremental en {}", c.name);
            if to != "access" && to != "iris" && to != "cache" && to != "zen" {
                // Those three replace an identity column's type in their DDL.
                assert!(script.contains(&c.data_type), "{to}: el DDL no tiene «{}»:\n{script}", c.data_type);
            }
        }
        assert!(caps.foreign_keys || t.foreign_keys.is_empty(), "{to}: claves foráneas en {}", t.name);
        assert!(caps.indexes || t.indexes.is_empty(), "{to}: índices en {}", t.name);
        if let Some(spec) = &designer {
            assert!(spec.primary_key || t.primary_key.is_none(), "{to}: clave primaria en {}", t.name);
        }
        for fk in &t.foreign_keys {
            assert!(fk.on_delete.as_deref().is_none_or(|a| caps.on_delete.contains(&a)), "{to}: ON DELETE {:?}", fk.on_delete);
            assert!(fk.on_update.as_deref().is_none_or(|a| caps.on_update.contains(&a)), "{to}: ON UPDATE {:?}", fk.on_update);
        }
    }
    assert!(!r.issues.iter().any(|i| i.code == IssueCode::TypeUnknown), "{to}: tipos desconocidos: {:?}", r.issues);
}

// ------------------------------------------------------------------ sources

/// A PostgreSQL table as `database_schema` reports it.
fn pg_tables() -> Vec<TableSchema> {
    let clientes = table("clientes", vec![not_null(col("id", "integer")), col("nombre", "character varying(120)")], &["id"]);
    let mut pedidos = table(
        "pedidos",
        vec![
            not_null(with_default(col("id", "bigint"), "nextval('pedidos_id_seq'::regclass)")),
            not_null(col("cliente_id", "integer")),
            col("cantidad", "smallint"),
            with_default(col("total", "numeric(12,2)"), "0"),
            col("grande", "numeric(40,5)"),
            col("libre", "numeric"),
            col("ratio", "real"),
            col("medida", "double precision"),
            with_default(col("creado", "timestamp(6) with time zone"), "now()"),
            with_default(col("alta", "timestamp without time zone"), "CURRENT_TIMESTAMP"),
            with_default(col("dia", "date"), "CURRENT_DATE"),
            col("hora", "time(3) without time zone"),
            with_default(col("activo", "boolean"), "true"),
            with_default(col("codigo", "uuid"), "gen_random_uuid()"),
            col("datos", "jsonb"),
            col("etiquetas", "text[]"),
            col("notas", "text"),
            col("foto", "bytea"),
            col("ip", "inet"),
            with_default(col("estado", "character varying(20)"), "'nuevo'::character varying"),
            col("sigla", "character(3)"),
        ],
        &["id"],
    );
    pedidos.foreign_keys.push(ForeignKeyDef {
        name: Some("fk_pedidos_clientes".into()),
        columns: vec!["cliente_id".into()],
        ref_table: "clientes".into(),
        ref_columns: vec!["id".into()],
        on_delete: Some("CASCADE".into()),
        on_update: Some("RESTRICT".into()),
        ..Default::default()
    });
    pedidos.indexes.push(IndexDef { name: "ix_pedidos_creado".into(), columns: vec!["creado".into()], ..Default::default() });
    pedidos.indexes.push(IndexDef { name: "ux_pedidos_codigo".into(), columns: vec!["codigo".into()], unique: true, ..Default::default() });
    vec![clientes, pedidos]
}

fn mysql_tables() -> Vec<TableSchema> {
    vec![table(
        "productos",
        vec![
            not_null(auto(col("id", "int unsigned"))),
            col("activo", "tinyint(1)"),
            col("chico", "tinyint unsigned"),
            col("stock", "mediumint"),
            col("grande", "bigint unsigned"),
            col("precio", "decimal(10,2)"),
            col("tipo", "enum('a','b','c')"),
            col("desc", "mediumtext"),
            with_default(col("alta", "datetime(3)"), "CURRENT_TIMESTAMP(3)"),
            col("ts", "timestamp"),
            col("anio", "year"),
            col("bits", "bit(12)"),
            col("hash", "binary(16)"),
            col("doc", "json"),
            col("nombre", "varchar(100)"),
        ],
        &["id"],
    )]
}

fn sqlserver_tables() -> Vec<TableSchema> {
    vec![table(
        "Ventas",
        vec![
            not_null(auto(col("Id", "int"))),
            col("Codigo", "uniqueidentifier"),
            col("Monto", "money"),
            col("Fecha", "datetime"),
            col("Exacta", "datetime2(7)"),
            col("Zona", "datetimeoffset(7)"),
            col("Nombre", "nvarchar(50)"),
            col("Texto", "nvarchar(max)"),
            col("Ascii", "varchar(8000)"),
            col("Bytes", "varbinary(max)"),
            col("Chico", "tinyint"),
            col("Version", "rowversion"),
            with_default(col("Creado", "datetime2"), "(getdate())"),
            with_default(col("Activo", "bit"), "((1))"),
            col("Xml", "xml"),
        ],
        &["Id"],
    )]
}

/// Each engine's own table, as its ODBC driver reports it.
fn engine_table(id: &str) -> TableSchema {
    let cols: Vec<ColumnDef> = match id {
        "odbc" | "netsuite" => vec![
            not_null(auto(col("id", "int identity"))),
            col("nombre", "nvarchar(50)"),
            col("texto", "nvarchar"),
            col("fecha", "datetime2"),
            col("monto", "money"),
            col("activo", "bit"),
            col("codigo", "uniqueidentifier"),
            col("cant", "decimal(12,2)"),
            col("chico", "tinyint"),
        ],
        "ingres" => vec![
            not_null(auto(col("id", "INTEGER"))),
            col("chico", "INTEGER1"),
            col("grande", "BIGINT"),
            col("monto", "DECIMAL(12,2)"),
            col("real4", "FLOAT4"),
            col("real8", "FLOAT"),
            col("dinero", "MONEY"),
            col("nombre", "NVARCHAR(40)"),
            col("ascii", "VARCHAR(100)"),
            col("largo", "LONG NVARCHAR"),
            col("bytes", "VARBYTE(16)"),
            with_default(col("dia", "ANSIDATE"), "CURRENT_DATE"),
            col("viejo", "INGRESDATE"),
            col("hora", "TIME WITHOUT TIME ZONE"),
            with_default(col("creado", "TIMESTAMP WITH TIME ZONE"), "CURRENT_TIMESTAMP"),
            col("activo", "BOOLEAN"),
            col("lapso", "INTERVAL DAY TO SECOND"),
        ],
        "zen" => vec![
            not_null(auto(col("id", "IDENTITY"))),
            col("sinsigno", "UINTEGER"),
            col("enorme", "UBIGINT"),
            col("monto", "DECIMAL(12,2)"),
            col("dinero", "CURRENCY"),
            col("m2", "MONEY"),
            col("nombre", "NVARCHAR(40)"),
            col("largo", "LONGVARCHAR"),
            col("bin", "BINARY(16)"),
            col("activo", "BIT"),
            col("dia", "DATE"),
            col("hora", "TIME"),
            col("creado", "TIMESTAMP"),
            col("dt", "DATETIME"),
            col("codigo", "UNIQUEIDENTIFIER"),
        ],
        "altibase" => vec![
            not_null(col("ID", "INTEGER")),
            col("MONTO", "NUMERIC(12,2)"),
            col("LIBRE", "NUMBER"),
            col("REAL8", "DOUBLE"),
            col("NOMBRE", "VARCHAR(100)"),
            col("UNI", "NVARCHAR(50)"),
            col("TEXTO", "CLOB"),
            col("BYTES", "VARBYTE(16)"),
            with_default(col("CREADO", "DATE"), "SYSDATE"),
            col("BLOQUE", "BLOB"),
        ],
        "cubrid" => vec![
            not_null(auto(col("id", "INTEGER"))),
            col("corto", "SHORT"),
            col("monto", "NUMERIC(12,2)"),
            col("dinero", "MONETARY"),
            col("nombre", "VARCHAR(100)"),
            col("texto", "STRING"),
            col("bytes", "BIT VARYING(128)"),
            col("hash", "BIT(128)"),
            col("dia", "DATE"),
            col("hora", "TIME"),
            col("ts", "TIMESTAMP"),
            with_default(col("dt", "DATETIME"), "CURRENT_DATETIME"),
            col("tipo", "ENUM('a','b')"),
            col("doc", "JSON"),
        ],
        "dameng" => vec![
            not_null(auto(col("ID", "INT"))),
            col("CHICO", "TINYINT"),
            col("MONTO", "NUMBER(12,2)"),
            col("NOMBRE", "VARCHAR2(100 CHAR)"),
            col("TEXTO", "TEXT"),
            col("BYTES", "VARBINARY(16)"),
            col("ACTIVO", "BIT"),
            col("DIA", "DATE"),
            col("HORA", "TIME(3)"),
            with_default(col("CREADO", "DATETIME(6)"), "SYSDATE"),
            col("ZONA", "TIMESTAMP(6) WITH TIME ZONE"),
        ],
        "heavydb" => vec![
            not_null(col("id", "INTEGER")),
            col("chico", "TINYINT"),
            col("monto", "DECIMAL(12,2)"),
            col("nombre", "TEXT ENCODING DICT(32)"),
            col("libre", "TEXT ENCODING NONE"),
            col("activo", "BOOLEAN"),
            col("dia", "DATE ENCODING DAYS(32)"),
            col("creado", "TIMESTAMP(3)"),
            col("lista", "INTEGER[]"),
            col("lugar", "POINT"),
        ],
        "iris" | "cache" => vec![
            not_null(auto(col("ID", "BIGINT"))),
            col("Chico", "TINYINT"),
            col("Monto", "NUMERIC(12,2)"),
            col("Dinero", "MONEY"),
            col("Nombre", "VARCHAR(100)"),
            col("Texto", "LONGVARCHAR"),
            col("Bytes", "VARBINARY(16)"),
            col("Activo", "BIT"),
            col("Dia", "DATE"),
            col("Hora", "TIME"),
            with_default(col("Creado", "TIMESTAMP"), "CURRENT_TIMESTAMP"),
            col("Posix", "POSIXTIME"),
        ],
        "machbase" => vec![
            not_null(col("ID", "LONG")),
            col("SENSOR", "USHORT"),
            col("VALOR", "DOUBLE"),
            col("NOMBRE", "VARCHAR(100)"),
            col("TEXTO", "TEXT"),
            col("DATOS", "BINARY"),
            col("CREADO", "DATETIME"),
            col("ORIGEN", "IPV4"),
            col("DOC", "JSON"),
        ],
        "access" => vec![
            not_null(auto(col("Id", "COUNTER"))),
            col("Chico", "BYTE"),
            col("Entero", "INTEGER"),
            col("Corto", "SMALLINT"),
            col("Monto", "DECIMAL(12,2)"),
            col("Dinero", "CURRENCY"),
            col("Nombre", "VARCHAR(50)"),
            col("Notas", "LONGCHAR"),
            col("Activo", "BIT"),
            with_default(col("Creado", "DATETIME"), "Now()"),
            col("Codigo", "GUID"),
            col("Objeto", "LONGBINARY"),
        ],
        "dbase" => vec![
            col("CODIGO", "CHAR(10)"),
            col("MONTO", "NUMERIC(10,2)"),
            col("MEDIDA", "FLOAT"),
            col("ACTIVO", "LOGICAL"),
            col("DIA", "DATE"),
            col("NOTAS", "MEMO"),
        ],
        "mimer" => vec![
            not_null(col("ID", "INTEGER")),
            col("DIGITOS", "INTEGER(5)"),
            col("MONTO", "DECIMAL(12,2)"),
            col("NOMBRE", "NATIONAL CHARACTER VARYING(40)"),
            col("ASCII", "CHARACTER VARYING(40)"),
            col("TEXTO", "NCLOB"),
            col("BYTES", "BINARY VARYING(16)"),
            col("ACTIVO", "BOOLEAN"),
            col("DIA", "DATE"),
            col("HORA", "TIME(3)"),
            with_default(col("CREADO", "TIMESTAMP(6)"), "LOCALTIMESTAMP"),
            col("LAPSO", "INTERVAL DAY TO SECOND"),
        ],
        "monetdb" => vec![
            not_null(with_default(col("id", "int"), "next value for \"sys\".\"seq_7788\"")),
            col("enorme", "hugeint"),
            col("monto", "decimal(12,2)"),
            col("nombre", "varchar(100)"),
            col("texto", "clob"),
            col("bytes", "blob"),
            col("activo", "boolean"),
            col("dia", "date"),
            col("hora", "time"),
            with_default(col("creado", "timestamptz"), "CURRENT_TIMESTAMP"),
            col("codigo", "uuid"),
            col("doc", "json"),
            col("ip", "inet"),
            col("lapso", "sec_interval"),
        ],
        "nuodb" => vec![
            not_null(auto(col("ID", "BIGINT"))),
            col("MONTO", "DECIMAL(12,2)"),
            col("LIBRE", "NUMBER"),
            col("NOMBRE", "VARCHAR(100)"),
            col("TEXTO", "STRING"),
            col("BYTES", "VARBINARY(16)"),
            col("ACTIVO", "BOOLEAN"),
            col("CREADO", "TIMESTAMP"),
            col("TIPO", "ENUM('a','b')"),
        ],
        "ocient" => vec![
            not_null(col("id", "BIGINT")),
            col("chico", "TINYINT"),
            col("monto", "DECIMAL(12,2)"),
            col("nombre", "VARCHAR(100)"),
            col("bytes", "VARBINARY(16)"),
            col("activo", "BOOLEAN"),
            col("creado", "TIMESTAMP"),
            col("codigo", "UUID"),
            col("ip", "IP"),
        ],
        "virtuoso" => vec![
            not_null(auto(col("ID", "INTEGER"))),
            col("MONTO", "DECIMAL(12,2)"),
            col("NOMBRE", "NVARCHAR(100)"),
            col("TEXTO", "LONG NVARCHAR"),
            col("BYTES", "VARBINARY(16)"),
            col("CREADO", "DATETIME"),
            col("DOC", "LONG XML"),
        ],
        "openedge" => vec![
            not_null(col("id", "INTEGER")),
            col("monto", "NUMERIC(12,2)"),
            col("nombre", "VARCHAR(100)"),
            col("texto", "LVARCHAR"),
            col("activo", "BIT"),
            with_default(col("creado", "TIMESTAMP"), "SYSTIMESTAMP"),
            col("zona", "TIMESTAMP WITH TIME ZONE"),
        ],
        "maxdb" => vec![
            not_null(auto(col("ID", "INTEGER"))),
            col("MONTO", "FIXED(12,2)"),
            col("MEDIDA", "FLOAT(38)"),
            col("NOMBRE", "VARCHAR(100) UNICODE"),
            col("ASCII", "CHAR(10) ASCII"),
            col("HASH", "CHAR(16) BYTE"),
            col("TEXTO", "LONG UNICODE"),
            col("ACTIVO", "BOOLEAN"),
            with_default(col("CREADO", "TIMESTAMP"), "TIMESTAMP"),
        ],
        "sqream" => vec![
            not_null(col("id", "BIGINT")),
            col("chico", "TINYINT"),
            col("monto", "NUMERIC(12,2)"),
            col("nombre", "TEXT(100)"),
            col("activo", "BOOL"),
            col("creado", "DATETIME"),
            col("fino", "DATETIME2"),
        ],
        "ignite" | "ignite3" => vec![
            not_null(col("ID", "BIGINT")),
            col("MONTO", "DECIMAL(12,2)"),
            col("NOMBRE", "VARCHAR(100)"),
            col("ACTIVO", "BOOLEAN"),
            col("CREADO", "TIMESTAMP"),
            col("CODIGO", "UUID"),
        ],
        other => panic!("sin tabla para {other}"),
    };
    let pk_col = cols[0].name.clone();
    let mut t = table("muestra", cols, &[]);
    if id != "dbase" {
        t.primary_key = Some(KeyDef { name: None, columns: vec![pk_col] });
    }
    t
}

// ------------------------------------------------------------------ into the ODBC engines

#[test]
fn from_the_core_families_into_every_engine() {
    for to in ENGINES.iter().filter(|id| **id != "netsuite") {
        for (from, tables) in [("postgres", pg_tables()), ("mysql", mysql_tables()), ("sqlserver", sqlserver_tables())] {
            let r = run(&tables, from, to);
            check_target(&r, to);
        }
    }
}

#[test]
fn postgres_to_ingres() {
    let r = run(&pg_tables(), "postgres", "ingres");
    let p = &r.tables[1];
    assert!(column(p, "id").auto_increment);
    assert_eq!(ty(p, "id"), "BIGINT");
    assert_eq!(ty(p, "total"), "DECIMAL(12, 2)");
    assert_eq!(ty(p, "grande"), "DECIMAL(39, 5)");
    assert!(has(&r, IssueCode::PrecisionLoss, "grande"));
    assert_eq!(ty(p, "creado"), "TIMESTAMP(6) WITH TIME ZONE");
    assert_eq!(column(p, "creado").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(column(p, "alta").default_value.as_deref(), Some("LOCAL_TIMESTAMP"));
    assert_eq!(ty(p, "dia"), "ANSIDATE");
    assert_eq!(ty(p, "hora"), "TIME(3)");
    assert_eq!(ty(p, "activo"), "BOOLEAN");
    assert_eq!(column(p, "activo").default_value.as_deref(), Some("TRUE"));
    // No UUID default in Ingres: reported.
    assert_eq!(ty(p, "codigo"), "CHAR(36)");
    assert!(has(&r, IssueCode::DefaultDropped, "codigo"));
    assert_eq!(ty(p, "notas"), "LONG NVARCHAR");
    assert_eq!(ty(p, "foto"), "LONG BYTE");
    assert_eq!(ty(p, "estado"), "NVARCHAR(20)");
    assert_eq!(column(p, "estado").default_value.as_deref(), Some("'nuevo'"));
    assert_eq!(p.foreign_keys[0].on_update.as_deref(), Some("RESTRICT"));
    let script = ddl("ingres", &r.tables);
    assert!(script.contains("\"id\" BIGINT GENERATED BY DEFAULT AS IDENTITY NOT NULL"), "{script}");
    assert!(script.contains("COMMENT ON") || !script.contains("COMMENT"), "{script}");
}

#[test]
fn postgres_to_cubrid_and_dameng() {
    let r = run(&pg_tables(), "postgres", "cubrid");
    let p = &r.tables[1];
    assert_eq!(ty(p, "activo"), "SMALLINT");
    assert_eq!(column(p, "activo").default_value.as_deref(), Some("1"));
    assert_eq!(ty(p, "creado"), "DATETIMETZ");
    assert!(has(&r, IssueCode::PrecisionLoss, "creado"));
    assert_eq!(column(p, "creado").default_value.as_deref(), Some("CURRENT_DATETIME"));
    assert_eq!(ty(p, "notas"), "STRING");
    assert_eq!(ty(p, "datos"), "JSON");
    // ON UPDATE RESTRICT exists in CUBRID; ON DELETE CASCADE too.
    assert_eq!(p.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    assert!(ddl("cubrid", &r.tables).contains("\"id\" BIGINT AUTO_INCREMENT NOT NULL"));

    let r = run(&pg_tables(), "postgres", "dameng");
    let p = &r.tables[1];
    assert_eq!(p.name, "PEDIDOS");
    assert_eq!(ty(p, "ID"), "BIGINT");
    assert_eq!(ty(p, "ACTIVO"), "BIT");
    assert_eq!(ty(p, "CREADO"), "TIMESTAMP(6) WITH TIME ZONE");
    assert_eq!(column(p, "CREADO").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(column(p, "ALTA").default_value.as_deref(), Some("SYSDATE"));
    assert_eq!(ty(p, "LIBRE"), "NUMBER");
    assert_eq!(ty(p, "NOTAS"), "TEXT");
    assert!(ddl("dameng", &r.tables).contains("\"ID\" BIGINT IDENTITY(1,1) NOT NULL"));
}

#[test]
fn into_engines_without_keys() {
    for to in ["heavydb", "sqream", "machbase", "ocient"] {
        let r = run(&pg_tables(), "postgres", to);
        assert!(r.tables.iter().all(|t| t.primary_key.is_none()), "{to}");
        assert!(r.issues.iter().any(|i| i.code == IssueCode::PrimaryKeyDropped && i.severity == Severity::Dropped), "{to}");
        assert!(r.tables.iter().all(|t| t.foreign_keys.is_empty()), "{to}");
        assert!(!ddl(to, &r.tables).contains("PRIMARY KEY"), "{to}");
    }
    let r = run(&pg_tables(), "postgres", "heavydb");
    let p = &r.tables[1];
    assert_eq!(ty(p, "etiquetas"), "TEXT[]");
    assert_eq!(ty(p, "foto"), "TEXT ENCODING NONE");
    assert!(has(&r, IssueCode::TypeApproximated, "foto"));
    assert!(has(&r, IssueCode::DefaultDropped, "creado"));
    assert_eq!(ty(p, "grande"), "DECIMAL(18, 5)");
    // dBase keeps the key as a unique index with a 10-character name.
    let r = run(&pg_tables(), "postgres", "dbase");
    let c = &r.tables[0];
    assert!(c.primary_key.is_none());
    assert!(c.indexes.iter().any(|i| i.unique && i.columns == ["ID"] && i.name.len() <= 10), "{:?}", c.indexes);
    assert!(r.tables.iter().flat_map(|t| &t.columns).all(|c| c.name.len() <= 10 && c.default_value.is_none() && c.nullable));
}

#[test]
fn mysql_to_zen_monetdb_and_access() {
    let r = run(&mysql_tables(), "mysql", "zen");
    let p = &r.tables[0];
    // Zen keeps unsigned integers as they are.
    assert_eq!(ty(p, "id"), "UINTEGER");
    assert_eq!(ty(p, "chico"), "UTINYINT");
    assert_eq!(ty(p, "grande"), "UBIGINT");
    assert_eq!(ty(p, "activo"), "BIT");
    assert_eq!(column(p, "alta").default_value.as_deref(), Some("NOW()"));
    assert!(ddl("zen", &r.tables).contains("IDENTITY"));

    let r = run(&mysql_tables(), "mysql", "monetdb");
    let p = &r.tables[0];
    assert_eq!(ty(p, "id"), "BIGINT");
    assert_eq!(ty(p, "grande"), "HUGEINT");
    assert_eq!(ty(p, "ts"), "TIMESTAMP(6) WITH TIME ZONE");
    assert_eq!(ty(p, "tipo"), "VARCHAR(1)");
    assert!(r.issues.iter().any(|i| i.object.as_deref() == Some("tipo") && i.message.contains("a, b, c")));
    assert!(ddl("monetdb", &r.tables).contains("\"id\" BIGINT AUTO_INCREMENT NOT NULL"));

    let r = run(&mysql_tables(), "mysql", "access");
    let p = &r.tables[0];
    assert_eq!(ty(p, "chico"), "BYTE");
    assert_eq!(ty(p, "activo"), "YESNO");
    assert_eq!(ty(p, "desc"), "LONGTEXT");
    assert_eq!(ty(p, "grande"), "DECIMAL(20, 0)");
    assert_eq!(ty(p, "alta"), "DATETIME");
    assert!(has(&r, IssueCode::PrecisionLoss, "alta"));
    assert_eq!(column(p, "alta").default_value.as_deref(), Some("Now()"));
    // The Access driver writes COUNTER for the auto-increment column.
    assert!(ddl("access", &r.tables).contains("[id] COUNTER NOT NULL"), "{}", ddl("access", &r.tables));
}

#[test]
fn sqlserver_to_iris_altibase_and_maxdb() {
    let r = run(&sqlserver_tables(), "sqlserver", "iris");
    let v = &r.tables[0];
    assert_eq!(ty(v, "Codigo"), "UNIQUEIDENTIFIER");
    assert_eq!(ty(v, "Monto"), "MONEY");
    assert_eq!(ty(v, "Texto"), "LONGVARCHAR");
    assert_eq!(ty(v, "Version"), "ROWVERSION");
    assert_eq!(ty(v, "Activo"), "BIT");
    assert_eq!(column(v, "Activo").default_value.as_deref(), Some("1"));
    assert!(ddl("iris", &r.tables).contains("\"Id\" IDENTITY NOT NULL"));

    let r = run(&sqlserver_tables(), "sqlserver", "altibase");
    let v = &r.tables[0];
    // A mixed-case name was quoted on purpose: kept.
    assert_eq!(v.name, "Ventas");
    assert_eq!(ty(v, "Exacta"), "DATE");
    assert!(has(&r, IssueCode::PrecisionLoss, "Exacta"));
    assert!(has(&r, IssueCode::TimeZoneLoss, "Zona"));
    assert_eq!(ty(v, "NOMBRE"), "NVARCHAR(50)");
    assert_eq!(column(v, "CREADO").default_value.as_deref(), Some("SYSDATE"));
    // Altibase has no identity the driver can write.
    assert!(!column(v, "ID").auto_increment);
    assert!(has(&r, IssueCode::AutoIncrementDropped, "Id"));

    let r = run(&sqlserver_tables(), "sqlserver", "maxdb");
    let v = &r.tables[0];
    assert_eq!(ty(v, "NOMBRE"), "VARCHAR(50) UNICODE");
    assert_eq!(ty(v, "ASCII"), "VARCHAR(8000) ASCII");
    assert_eq!(ty(v, "BYTES"), "LONG BYTE");
    assert_eq!(column(v, "CREADO").default_value.as_deref(), Some("TIMESTAMP"));
    assert!(ddl("maxdb", &r.tables).contains("DEFAULT SERIAL"));
}

#[test]
fn ignite_takes_a_unique_index_as_key() {
    let mut t = table("t", vec![not_null(col("codigo", "uuid")), col("valor", "integer")], &[]);
    t.indexes.push(IndexDef { name: "ux_codigo".into(), columns: vec!["codigo".into()], unique: true, ..Default::default() });
    for to in ["ignite", "ignite3"] {
        let r = run(&[t.clone()], "postgres", to);
        assert_eq!(r.tables[0].primary_key.as_ref().map(|k| k.columns.clone()), Some(vec!["CODIGO".to_string()]), "{to}");
        assert!(r.tables[0].indexes.is_empty());
        assert!(r.issues.iter().any(|i| i.code == IssueCode::PrimaryKeyAdded));
        assert!(ddl(to, &r.tables).contains("PRIMARY KEY (\"CODIGO\")"));
    }
}

#[test]
fn generic_target_and_netsuite() {
    let r = run(&pg_tables(), "postgres", "odbc");
    let p = &r.tables[1];
    assert_eq!(ty(p, "activo"), "SMALLINT");
    assert_eq!(ty(p, "notas"), "CLOB");
    assert_eq!(ty(p, "creado"), "TIMESTAMP(6)");
    assert!(has(&r, IssueCode::TimeZoneLoss, "creado"));
    // The generic flavor writes no identity: reported, not silently lost.
    assert!(has(&r, IssueCode::AutoIncrementDropped, "id"));
    assert!(r.issues.iter().any(|i| i.object.is_none() && i.message.contains("ODBC genérico")));
    // ON UPDATE isn't common to every engine.
    assert_eq!(p.foreign_keys[0].on_update, None);
    let e = convert(&pg_tables(), "postgres", "netsuite", &Options::default()).unwrap_err();
    assert!(e.to_string().contains("solo lectura"), "{e}");
}

// ------------------------------------------------------------------ out of the ODBC engines

#[test]
fn every_engine_into_the_core_families() {
    for from in ENGINES {
        let src = vec![engine_table(from)];
        for to in ["postgres", "mysql", "sqlserver", "oracle"] {
            let r = run(&src, from, to);
            assert!(!r.issues.iter().any(|i| i.code == IssueCode::TypeUnknown), "{from} → {to}: {:?}", r.issues);
            let d = for_driver(to).unwrap();
            for c in &r.tables[0].columns {
                assert!(!matches!(logical_of(d, &parse(&c.data_type)), L::Other { .. }), "{from} → {to}: {} {}", c.name, c.data_type);
            }
        }
    }
}

#[test]
fn engines_to_postgres() {
    let pg = |id: &str| run(&[engine_table(id)], id, "postgres");

    let r = pg("ingres");
    let t = &r.tables[0];
    assert!(column(t, "id").auto_increment);
    assert_eq!(ty(t, "chico"), "smallint");
    assert_eq!(ty(t, "dinero"), "numeric(19, 4)");
    assert_eq!(ty(t, "viejo"), "timestamp(0)");
    assert_eq!(ty(t, "hora"), "time(0)");
    assert_eq!(ty(t, "creado"), "timestamp(6) with time zone");
    assert_eq!(column(t, "creado").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(ty(t, "lapso"), "interval");

    let r = pg("zen");
    let t = &r.tables[0];
    assert_eq!(ty(t, "sinsigno"), "bigint");
    assert_eq!(ty(t, "enorme"), "numeric(39, 0)");
    assert_eq!(ty(t, "creado"), "timestamp(6)");
    assert!(has(&r, IssueCode::PrecisionLoss, "creado"));
    assert_eq!(ty(t, "codigo"), "uuid");

    let r = pg("altibase");
    let t = &r.tables[0];
    assert_eq!(t.name, "muestra");
    assert_eq!(ty(t, "creado"), "timestamp(6)");
    assert_eq!(column(t, "creado").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(ty(t, "libre"), "numeric");

    let r = pg("cubrid");
    let t = &r.tables[0];
    assert_eq!(ty(t, "hash"), "bytea");
    assert_eq!(ty(t, "ts"), "timestamp(0) with time zone");
    // CUBRID's CURRENT_DATETIME is "now".
    assert_eq!(column(t, "dt").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));

    let r = pg("access");
    let t = &r.tables[0];
    assert!(column(t, "Id").auto_increment, "COUNTER is an auto-increment");
    assert_eq!(ty(t, "Chico"), "smallint");
    assert_eq!(ty(t, "Codigo"), "uuid");
    assert_eq!(column(t, "Creado").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));

    let r = pg("monetdb");
    let t = &r.tables[0];
    assert!(column(t, "id").auto_increment, "a sequence default is an auto-increment");
    assert_eq!(ty(t, "enorme"), "numeric(39, 0)");
    assert_eq!(ty(t, "creado"), "timestamp(6) with time zone");

    let r = pg("maxdb");
    let t = &r.tables[0];
    // MaxDB's bare TIMESTAMP default is "now".
    assert_eq!(column(t, "creado").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(ty(t, "hash"), "bytea");
    assert_eq!(ty(t, "ascii"), "char(10)");
    assert_eq!(ty(t, "medida"), "numeric");

    let r = pg("dbase");
    assert!(r.tables[0].primary_key.is_none());
    assert_eq!(ty(&r.tables[0], "notas"), "text");
}

/// `b` is `a`, or a wider integer or a Unicode text that holds all of its values.
fn holds(a: &L, b: &L) -> bool {
    match (a, b) {
        (L::Int { bytes: x, unsigned: ux }, L::Int { bytes: y, unsigned: uy }) => {
            (ux == uy && y >= x) || (!uy && *y >= L::signed_bytes_for(*x, *ux))
        }
        // Unicode holds any character set.
        (L::Char { len: x, unicode: ux }, L::Char { len: y, unicode: uy }) | (L::Varchar { len: x, unicode: ux }, L::Varchar { len: y, unicode: uy }) => {
            x == y && (*uy || !*ux)
        }
        (L::Text { unicode: ux }, L::Text { unicode: uy }) => *uy || !*ux,
        // More fractional digits hold fewer.
        (L::Timestamp { precision: x, tz: tx }, L::Timestamp { precision: y, tz: ty }) | (L::Time { precision: x, tz: tx }, L::Time { precision: y, tz: ty }) => {
            tx == ty && (x == y || x.zip(*y).is_some_and(|(x, y)| y >= x))
        }
        _ => a == b,
    }
}

/// A → PostgreSQL → A keeps every logical type the report didn't mention.
#[test]
fn round_trip_through_postgres() {
    // NetSuite is read-only: there's no way back.
    for id in ENGINES.iter().filter(|id| **id != "netsuite") {
        let src = vec![engine_table(id)];
        let there = run(&src, id, "postgres");
        let back = convert(&there.tables, "postgres", id, &Options::default()).unwrap();
        let d = for_driver(id).unwrap();
        // Any note on the column (even an Info like "booleano como SMALLINT") explains a change.
        let flagged = |c: &str| there.issues.iter().chain(&back.issues).any(|i| i.object.as_deref().is_some_and(|o| o.eq_ignore_ascii_case(c)));
        for (a, b) in src[0].columns.iter().zip(&back.tables[0].columns) {
            if flagged(&a.name) {
                continue;
            }
            // Money is a decimal(19, 4) wherever the target has no money type.
            let norm = |l: L| if l == L::Money { L::Decimal { precision: Some(19), scale: Some(4) } } else { l };
            let la = norm(logical_of(d, &parse(&a.data_type)));
            let lb = norm(logical_of(d, &parse(&b.data_type)));
            assert!(holds(&la, &lb), "{id}: {} {} → {} → {} ({la:?} → {lb:?})", a.name, a.data_type, ty(&there.tables[0], &a.name), b.data_type);
        }
    }
}

#[test]
fn between_odbc_engines() {
    let r = run(&[engine_table("altibase")], "altibase", "dameng");
    let t = &r.tables[0];
    // Altibase's DATE carries a time: Dameng needs a TIMESTAMP.
    assert_eq!(ty(t, "CREADO"), "TIMESTAMP(6)");
    assert_eq!(column(t, "CREADO").default_value.as_deref(), Some("SYSDATE"));
    assert_eq!(ty(t, "LIBRE"), "NUMBER");
    check_target(&r, "dameng");

    let r = run(&[engine_table("access")], "access", "mimer");
    let t = &r.tables[0];
    assert_eq!(ty(t, "CHICO"), "SMALLINT");
    assert_eq!(ty(t, "DINERO"), "DECIMAL(19, 4)");
    assert_eq!(ty(t, "NOMBRE"), "NVARCHAR(50)");
    assert!(has(&r, IssueCode::AutoIncrementDropped, "Id"));
    check_target(&r, "mimer");

    let r = run(&[engine_table("iris")], "iris", "cache");
    // Same family: native spellings stay.
    assert_eq!(ty(&r.tables[0], "Posix"), "POSIXTIME");

    for from in ENGINES {
        for to in ["ingres", "cubrid", "monetdb", "heavydb", "access", "openedge"] {
            let r = run(&[engine_table(from)], from, to);
            check_target(&r, to);
        }
    }
}

/// Every type the preset's designer suggests is one the dialect reads.
#[test]
fn designer_types_are_understood() {
    let opaque = [("virtuoso", "ANY")];
    for id in ENGINES {
        // NetSuite is read-only: no designer.
        let Some(spec) = dbine_drivers::find(id).and_then(|d| d.designer()) else {
            assert_eq!(*id, "netsuite");
            continue;
        };
        let d = for_driver(id).unwrap();
        for t in &spec.data_types {
            if opaque.contains(&(id, t)) {
                continue;
            }
            assert!(!matches!(logical_of(d, &parse(t)), L::Other { .. }), "{id}: «{t}» no se reconoce");
        }
        // The family's caps are never wider than the designer.
        let caps = d.caps();
        assert_eq!(caps.clone().narrow(&spec), caps, "{id}");
    }
}

// ------------------------------------------------------------------ live

/// A session on the generic ODBC preset from `DBINE_TEST_ODBC_CONN`.
async fn odbc_session() -> Option<Box<dyn Session>> {
    let Ok(conn) = std::env::var("DBINE_TEST_ODBC_CONN") else {
        eprintln!("DBINE_TEST_ODBC_CONN no está definida: se saltea");
        return None;
    };
    let cfg = ConnectionConfig { driver: "odbc".into(), options: [("connection_string".to_string(), conn)].into(), ..Default::default() };
    let d = dbine_drivers::find("odbc").unwrap();
    Some(d.connect(&cfg, None).await.unwrap_or_else(|e| panic!("odbc: {e}")))
}

async fn rows(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<serde_json::Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
    out.results.into_iter().rev().find(|r| !r.columns.is_empty()).map(|r| r.rows).unwrap_or_default()
}

/// `common::round_trip` without the source schema (`dbo`, `public`), which
/// doesn't exist on the other side: the tables go to the session's default.
async fn trip(src: &mut Box<dyn Session>, from: &str, dst: &mut Box<dyn Session>, to: &str, names: &[&str]) -> common::RoundTrip {
    let mut tables = common::read(src, names).await;
    assert_eq!(tables.len(), names.len(), "{from}: {:?}", tables.iter().map(|t| &t.name).collect::<Vec<_>>());
    for t in &mut tables {
        t.schema = None;
        for fk in &mut t.foreign_keys {
            fk.ref_schema = None;
        }
    }
    let conversion = run(&tables, from, to);
    let ddl = common::target_ddl(to, &conversion.tables);
    for script in &ddl {
        common::exec(dst, script).await;
    }
    let target: Vec<&str> = conversion.tables.iter().map(|t| t.name.as_str()).collect();
    let back = common::read(dst, &target).await;
    common::RoundTrip { conversion, ddl, back }
}

/// SQL Server read through the generic ODBC preset (the engine unknown to
/// the dialect), converted to PostgreSQL, created there and read back; a
/// boundary row goes into both. Then a PostgreSQL table with the portable
/// types goes the other way.
#[tokio::test]
#[ignore]
async fn live_generic_odbc_sqlserver_and_postgres() {
    let (Some(mut ms), Some(mut pg)) = (odbc_session().await, common::session("postgres", "DBINE_TEST_POSTGRES_URL").await) else {
        return;
    };

    // --- SQL Server (through ODBC) → PostgreSQL.
    common::exec_quiet(&mut ms, &["DROP TABLE IF EXISTS oe_tipos", "DROP TABLE IF EXISTS oe_padre"]).await;
    common::exec_quiet(&mut pg, &["DROP TABLE IF EXISTS oe_tipos CASCADE", "DROP TABLE IF EXISTS oe_padre CASCADE"]).await;
    common::exec(
        &mut ms,
        "CREATE TABLE oe_padre (id int NOT NULL PRIMARY KEY, nombre nvarchar(50) NOT NULL);
         CREATE TABLE oe_tipos (
            id int IDENTITY(1,1) NOT NULL PRIMARY KEY,
            padre_id int NULL REFERENCES oe_padre (id) ON DELETE CASCADE,
            chico tinyint, corto smallint, grande bigint,
            dec_p decimal(38,10), dinero money, f4 real, f8 float,
            ch nchar(10), nv nvarchar(200), nmax nvarchar(max), va varchar(100),
            bin varbinary(max), fijo binary(16),
            d date, t time(7), dt datetime, dt2 datetime2(7), dto datetimeoffset(7),
            u uniqueidentifier, b bit, x xml,
            creado datetime2 DEFAULT (sysdatetime()),
            activo bit DEFAULT ((1)),
            estado nvarchar(20) DEFAULT (N'nuevo')
         );
         CREATE UNIQUE INDEX ux_oe_tipos_u ON oe_tipos (u);",
    )
    .await;
    let rt = trip(&mut ms, "odbc", &mut pg, "postgres", &["oe_padre", "oe_tipos"]).await;
    println!("{}", rt.ddl.join("\n"));
    for i in &rt.conversion.issues {
        println!("{:?} {:?} {}.{}: {}", i.severity, i.code, i.table, i.object.as_deref().unwrap_or(""), i.message);
    }
    let t = &rt.back[1];
    let expect = [
        ("id", "integer"),
        ("chico", "smallint"),
        ("grande", "bigint"),
        ("dec_p", "numeric(38,10)"),
        ("dinero", "numeric(19,4)"),
        ("f4", "real"),
        ("f8", "double precision"),
        ("ch", "character(10)"),
        ("nv", "character varying(200)"),
        ("nmax", "text"),
        ("bin", "bytea"),
        ("d", "date"),
        // The ODBC catalog gives no precision for TIME and DATETIME and the
        // generic reader can't know the engine's default: PostgreSQL's.
        ("t", "time without time zone"),
        ("dt", "timestamp without time zone"),
        ("dt2", "timestamp(6) without time zone"),
        ("dto", "timestamp(6) with time zone"),
        ("u", "uuid"),
        ("b", "boolean"),
        ("x", "xml"),
    ];
    for (c, want) in expect {
        assert_eq!(common::col(t, c).data_type, want, "{c}");
    }
    assert!(common::col(t, "id").auto_increment, "identity survives");
    assert_eq!(t.foreign_keys.len(), 1);
    assert!(t.indexes.iter().any(|i| i.unique), "{:?}", t.indexes);
    assert!(rt.conversion.issues.iter().any(|i| i.code == IssueCode::PrecisionLoss && i.object.as_deref() == Some("dt2")));

    // Boundary row on both sides (7 → 6 fractional digits where the report says so).
    common::exec(&mut ms, "INSERT INTO oe_padre VALUES (1, N'ñandú')").await;
    common::exec(
        &mut ms,
        "INSERT INTO oe_tipos (padre_id, chico, corto, grande, dec_p, dinero, f4, f8, ch, nv, nmax, va, bin, fijo, d, t, dt, dt2, dto, u, b, x)
         VALUES (1, 255, -32768, 9223372036854775807, 9999999999999999999999999999.9999999999, 922337203685477.5807, 3.4e38, 1.7e308,
                 N'ñandú', N'año 😀', REPLICATE(CAST(N'x' AS nvarchar(max)), 5000), 'abc', 0xDEADBEEF, 0x00112233445566778899AABBCCDDEEFF,
                 '9999-12-31', '23:59:59.9999999', '2024-02-29 12:34:56.997', '9999-12-31 23:59:59.9999999',
                 '2024-02-29 12:34:56.1234567 -03:00', '6F9619FF-8B86-D011-B42D-00C04FC964FF', 1, '<a>1</a>')",
    )
    .await;
    common::exec(&mut pg, "INSERT INTO oe_padre VALUES (1, 'ñandú')").await;
    common::exec(
        &mut pg,
        "INSERT INTO oe_tipos (padre_id, chico, corto, grande, dec_p, dinero, f4, f8, ch, nv, nmax, va, bin, fijo, d, t, dt, dt2, dto, u, b, x)
         VALUES (1, 255, -32768, 9223372036854775807, 9999999999999999999999999999.9999999999, 922337203685477.5807, 3.4e38, 1.7e308,
                 'ñandú', 'año 😀', repeat('x', 5000), 'abc', '\\xDEADBEEF', '\\x00112233445566778899AABBCCDDEEFF',
                 '9999-12-31', '23:59:59.999999', '2024-02-29 12:34:56.997', '9999-12-31 23:59:59.999999',
                 '2024-02-29 12:34:56.123456-03:00', '6F9619FF-8B86-D011-B42D-00C04FC964FF', true, '<a>1</a>')",
    )
    .await;
    let a = rows(&mut ms, "SELECT grande, CAST(dec_p AS varchar(60)), nv, LEN(nmax), chico FROM oe_tipos").await;
    let b = rows(&mut pg, "SELECT grande::text, dec_p::text, nv, length(nmax), chico FROM oe_tipos").await;
    println!("sqlserver: {a:?}\npostgres: {b:?}");
    let s = |v: &serde_json::Value| v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
    assert_eq!(s(&a[0][0]), s(&b[0][0]));
    assert_eq!(s(&a[0][1]), s(&b[0][1]));
    assert_eq!(s(&a[0][2]), s(&b[0][2]));
    assert_eq!(s(&a[0][3]), s(&b[0][3]));
    assert_eq!(s(&a[0][4]), s(&b[0][4]));

    // --- PostgreSQL → SQL Server through the generic preset: the portable types.
    common::exec_quiet(&mut ms, &["DROP TABLE IF EXISTS oe_vuelta"]).await;
    common::exec_quiet(&mut pg, &["DROP TABLE IF EXISTS oe_vuelta"]).await;
    common::exec(
        &mut pg,
        "CREATE TABLE oe_vuelta (
            id integer PRIMARY KEY, i2 smallint, i8 bigint, n numeric(12,2), r real, dp double precision,
            c char(5), v varchar(300), d date, b boolean DEFAULT true, u uuid, t time(3), m money
         )",
    )
    .await;
    let rt = trip(&mut pg, "postgres", &mut ms, "odbc", &["oe_vuelta"]).await;
    println!("{}", rt.ddl.join("\n"));
    let t = &rt.back[0];
    println!("{:?}", t.columns.iter().map(|c| (&c.name, &c.data_type)).collect::<Vec<_>>());
    // As SQL Server's ODBC driver reports them.
    let expect = [
        ("id", "int"),
        ("i2", "smallint"),
        ("i8", "bigint"),
        ("n", "decimal(12,2)"),
        ("r", "real"),
        ("dp", "float"),
        ("c", "char(5)"),
        ("v", "varchar(300)"),
        ("d", "date"),
        ("b", "smallint"),
        ("u", "char(36)"),
        ("t", "time"),
        ("m", "decimal(19,2)"),
    ];
    for (c, want) in expect {
        assert_eq!(common::col(t, c).data_type, want, "{c}");
    }
    assert!(rt.conversion.issues.iter().any(|i| i.message.contains("ODBC genérico")));
    // And back to PostgreSQL through the generic reader.
    let again = run(&rt.back, "odbc", "postgres");
    let orig = common::read(&mut pg, &["oe_vuelta"]).await;
    let pgd = for_driver("postgres").unwrap();
    for c in &orig[0].columns {
        // SMALLINT and CHAR(36) on the way (reported); TIME(3) comes back
        // without its precision (the ODBC catalog doesn't give it).
        if matches!(c.name.as_str(), "b" | "u" | "t") {
            continue;
        }
        let a = logical_of(pgd, &parse(&c.data_type));
        let b = logical_of(pgd, &parse(ty(&again.tables[0], &c.name)));
        assert_eq!(a, b, "{}: {} → {}", c.name, c.data_type, ty(&again.tables[0], &c.name));
    }
    common::exec(&mut ms, "INSERT INTO oe_vuelta (id, i2, i8, n, r, dp, c, v, d, b, u, t, m) VALUES (1, -32768, 9223372036854775807, 9999999999.99, 1.5, 2.5, 'abc', N'ñandú', '2024-02-29', 1, '6F9619FF-8B86-D011-B42D-00C04FC964FF', '12:34:56.789', 92233720368547758.07)").await;
    let r = rows(&mut ms, "SELECT b, n, t FROM oe_vuelta").await;
    println!("{r:?}");
    assert_eq!(r.len(), 1);

    common::exec_quiet(&mut ms, &["DROP TABLE IF EXISTS oe_vuelta", "DROP TABLE IF EXISTS oe_tipos", "DROP TABLE IF EXISTS oe_padre"]).await;
    common::exec_quiet(&mut pg, &["DROP TABLE IF EXISTS oe_vuelta", "DROP TABLE IF EXISTS oe_tipos CASCADE", "DROP TABLE IF EXISTS oe_padre CASCADE"]).await;
}
