//! Enterprise engines: Db2 (LUW, i, z/OS), Firebird, SAP HANA, Informix /
//! GBase 8s, SAP ASE and SQL Anywhere, Teradata, Vertica, Exasol and
//! Netezza. End-to-end conversions with tables spelled the way each driver
//! reports them (most of these come through ODBC), the DDL the target
//! driver writes for them, and round trips against real servers.
//!
//! Live tests (`cargo test -p dbine-schema --test enterprise -- --ignored --test-threads=1`)
//! read the `DBINE_TEST_<ENGINE>_URL` variables and skip without them:
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013/master'
//! ```
//!
//! Only Firebird has a local server: HANA's image doesn't run on ARM and
//! the other engines have neither an image nor an installed ODBC driver,
//! so for them only the conversions and the generated DDL are checked.

mod common;

use common::{col, exec, exec_quiet, read, round_trip, RoundTrip};
use dbine_driver::{ColumnDef, DdlParts, ForeignKeyDef, IndexDef, KeyDef, TableSchema};
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

fn dflt<'a>(t: &'a TableSchema, c: &str) -> Option<&'a str> {
    column(t, c).default_value.as_deref()
}

fn has(r: &Conversion, code: IssueCode, object: &str) -> bool {
    r.issues.iter().any(|i| i.code == code && i.object.as_deref() == Some(object))
}

fn conv(tables: &[TableSchema], from: &str, to: &str) -> Conversion {
    convert(tables, from, to, &Options::default()).unwrap()
}

/// The whole script the target driver writes (tables, indexes, keys).
fn ddl(driver: &str, tables: &[TableSchema]) -> String {
    let d = dbine_drivers::find(driver).unwrap_or_else(|| panic!("no driver {driver}"));
    let mut out = Vec::new();
    for t in tables {
        out.push(d.table_ddl(t, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap());
    }
    for t in tables {
        out.push(d.table_ddl(t, DdlParts { foreign_keys: true, ..Default::default() }).unwrap());
    }
    out.join("\n")
}

/// PostgreSQL tables as its `database_schema` reports them.
fn pg_tables() -> Vec<TableSchema> {
    let clientes = table("clientes", vec![nn(c("id", "integer")), c("nombre", "character varying(120)")], &["id"]);
    let mut pedidos = table(
        "pedidos",
        vec![
            nn(auto(c("id", "bigint"))),
            nn(c("cliente_id", "integer")),
            c("chico", "smallint"),
            def(c("total", "numeric(12,2)"), "0"),
            c("ratio", "numeric"),
            c("doble", "double precision"),
            def(c("activo", "boolean"), "true"),
            c("letra", "character(1)"),
            def(c("estado", "character varying(20)"), "'nuevo'::character varying"),
            c("notas", "text"),
            c("bytes", "bytea"),
            def(c("dia", "date"), "CURRENT_DATE"),
            c("hora", "time without time zone"),
            def(c("creado", "timestamp with time zone"), "now()"),
            c("local_ts", "timestamp without time zone"),
            def(c("codigo", "uuid"), "gen_random_uuid()"),
            c("datos", "jsonb"),
            c("etiquetas", "text[]"),
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
    pedidos.indexes.push(IndexDef { name: "ix_pedidos_dia".into(), columns: vec!["dia".into()], ..Default::default() });
    pedidos.indexes.push(IndexDef {
        name: "ux_pedidos_codigo".into(),
        columns: vec!["codigo".into()],
        unique: true,
        filter: Some("activo".into()),
        ..Default::default()
    });
    vec![clientes, pedidos]
}

/// MySQL, as its `database_schema` reports it.
fn mysql_table() -> Vec<TableSchema> {
    vec![table(
        "productos",
        vec![
            nn(auto(c("id", "int unsigned"))),
            c("activo", "tinyint(1)"),
            c("stock", "mediumint"),
            c("precio", "decimal(10,2)"),
            c("tipo", "enum('chico','grande')"),
            c("alta", "datetime(3)"),
            def(c("ts", "timestamp"), "CURRENT_TIMESTAMP"),
            c("anio", "year"),
            c("grande", "bigint unsigned"),
            c("nombre", "varchar(50)"),
            c("foto", "longblob"),
            c("extra", "json"),
        ],
        &["id"],
    )]
}

/// SQL Server, as its `database_schema` reports it.
fn mssql_table() -> Vec<TableSchema> {
    vec![table(
        "Ventas",
        vec![
            nn(auto(c("Id", "int"))),
            def(c("Codigo", "uniqueidentifier"), "(newid())"),
            c("Monto", "money"),
            c("Fecha", "datetime"),
            c("Exacta", "datetime2(7)"),
            c("Zona", "datetimeoffset(7)"),
            c("Nombre", "nvarchar(50)"),
            c("Texto", "nvarchar(max)"),
            c("Bytes", "varbinary(max)"),
            c("Chico", "tinyint"),
            c("Version", "rowversion"),
            def(c("Activo", "bit"), "((1))"),
            c("Doc", "xml"),
        ],
        &["Id"],
    )]
}

// ---------------------------------------------------------------------------
// Registry

#[test]
fn every_enterprise_id_has_its_dialect() {
    for (id, family) in [
        ("db2", "db2"),
        ("db2i", "db2"),
        ("db2zos", "db2"),
        ("firebird", "firebird"),
        ("hana", "hana"),
        ("informix", "informix"),
        ("gbase8s", "informix"),
        ("sybase", "sybase"),
        ("sqlanywhere", "sqlanywhere"),
        ("teradata", "teradata"),
        ("vertica", "vertica"),
        ("exasol", "exasol"),
        ("netezza", "netezza"),
    ] {
        let d = dbine_schema::dialect::for_driver(id).unwrap_or_else(|| panic!("sin dialecto: {id}"));
        assert_eq!(d.id(), family, "{id}");
        assert!(dbine_drivers::find(id).is_some(), "no hay driver {id}");
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL → each engine

#[test]
fn postgres_to_db2_luw() {
    let r = conv(&pg_tables(), "postgres", "db2");
    let p = &r.tables[1];
    assert_eq!(p.name, "PEDIDOS");
    assert_eq!(ty(p, "ID"), "BIGINT");
    assert!(column(p, "ID").auto_increment);
    assert_eq!(ty(p, "CHICO"), "SMALLINT");
    assert_eq!(ty(p, "TOTAL"), "DECIMAL(12, 2)");
    assert_eq!(ty(p, "RATIO"), "DECFLOAT(34)");
    assert!(has(&r, IssueCode::PrecisionLoss, "ratio"));
    assert_eq!(ty(p, "DOBLE"), "DOUBLE");
    assert_eq!(ty(p, "ACTIVO"), "BOOLEAN");
    assert_eq!(dflt(p, "ACTIVO"), Some("TRUE"));
    assert_eq!(ty(p, "LETRA"), "CHAR(1 CODEUNITS32)");
    assert_eq!(ty(p, "ESTADO"), "VARCHAR(20 CODEUNITS32)");
    assert_eq!(dflt(p, "ESTADO"), Some("'nuevo'"));
    assert_eq!(ty(p, "NOTAS"), "CLOB(1G)");
    assert_eq!(ty(p, "BYTES"), "BLOB(1G)");
    assert_eq!(dflt(p, "DIA"), Some("CURRENT DATE"));
    assert_eq!(ty(p, "HORA"), "TIME");
    assert_eq!(ty(p, "CREADO"), "TIMESTAMP");
    assert_eq!(dflt(p, "CREADO"), Some("CURRENT TIMESTAMP"));
    assert!(has(&r, IssueCode::TimeZoneLoss, "creado"));
    assert_eq!(ty(p, "CODIGO"), "CHAR(36)");
    // No UUID function allowed in a Db2 DEFAULT.
    assert_eq!(dflt(p, "CODIGO"), None);
    assert!(has(&r, IssueCode::DefaultDropped, "codigo"));
    assert_eq!(ty(p, "DATOS"), "CLOB(1G)");
    assert!(has(&r, IssueCode::TypeApproximated, "etiquetas"));
    // Db2 has ON UPDATE RESTRICT; the filtered unique index can't go.
    let fk = &p.foreign_keys[0];
    assert_eq!((fk.on_delete.as_deref(), fk.on_update.as_deref()), (Some("CASCADE"), Some("RESTRICT")));
    assert!(!p.indexes.iter().any(|i| i.name == "UX_PEDIDOS_CODIGO"));

    let s = ddl("db2", &r.tables);
    assert!(s.contains("\"ID\" BIGINT GENERATED BY DEFAULT AS IDENTITY NOT NULL"), "{s}");
    assert!(s.contains("\"ESTADO\" VARCHAR(20 CODEUNITS32) DEFAULT 'nuevo'"), "{s}");
    assert!(s.contains("ON DELETE CASCADE ON UPDATE RESTRICT"), "{s}");
}

#[test]
fn postgres_to_db2_zos_and_i() {
    let z = conv(&pg_tables(), "postgres", "db2zos");
    let p = &z.tables[1];
    assert_eq!(ty(p, "ACTIVO"), "SMALLINT");
    assert_eq!(dflt(p, "ACTIVO"), Some("1"));
    assert_eq!(ty(p, "CREADO"), "TIMESTAMP WITH TIME ZONE");
    assert!(!has(&z, IssueCode::TimeZoneLoss, "creado"));
    assert_eq!(ty(p, "ESTADO"), "VARGRAPHIC(20)");
    assert_eq!(ty(p, "NOTAS"), "DBCLOB(512M)");
    // z/OS column names: 30 bytes.
    assert!(p.columns.iter().all(|c| c.name.len() <= 30));

    let i = conv(&pg_tables(), "postgres", "db2i");
    let p = &i.tables[1];
    assert_eq!(ty(p, "ACTIVO"), "BOOLEAN");
    assert_eq!(ty(p, "ESTADO"), "NVARCHAR(20)");
    assert_eq!(ty(p, "NOTAS"), "NCLOB(512M)");
    assert_eq!(ty(p, "TOTAL"), "DECIMAL(12, 2)");
    let s = ddl("db2i", &i.tables);
    assert!(s.contains("\"ESTADO\" NVARCHAR(20)"), "{s}");
}

#[test]
fn postgres_to_firebird() {
    let r = conv(&pg_tables(), "postgres", "firebird");
    let p = &r.tables[1];
    assert_eq!(p.name, "PEDIDOS");
    assert_eq!(ty(p, "ID"), "BIGINT");
    assert_eq!(ty(p, "TOTAL"), "NUMERIC(12, 2)");
    assert_eq!(ty(p, "RATIO"), "DECFLOAT(34)");
    assert_eq!(ty(p, "DOBLE"), "DOUBLE PRECISION");
    assert_eq!(ty(p, "ACTIVO"), "BOOLEAN");
    assert_eq!(dflt(p, "ACTIVO"), Some("TRUE"));
    assert_eq!(ty(p, "ESTADO"), "VARCHAR(20)");
    assert_eq!(ty(p, "NOTAS"), "BLOB SUB_TYPE TEXT");
    assert_eq!(ty(p, "BYTES"), "BLOB SUB_TYPE BINARY");
    assert_eq!(dflt(p, "DIA"), Some("CURRENT_DATE"));
    assert_eq!(ty(p, "CREADO"), "TIMESTAMP WITH TIME ZONE");
    assert_eq!(dflt(p, "CREADO"), Some("CURRENT_TIMESTAMP"));
    assert_eq!(ty(p, "LOCAL_TS"), "TIMESTAMP");
    assert_eq!(ty(p, "CODIGO"), "BINARY(16)");
    assert_eq!(dflt(p, "CODIGO"), None);
    assert_eq!(ty(p, "DATOS"), "BLOB SUB_TYPE TEXT");
    // No RESTRICT keyword in Firebird: NO ACTION.
    assert_eq!(p.foreign_keys[0].on_update.as_deref(), Some("NO ACTION"));
    let s = ddl("firebird", &r.tables);
    assert!(s.contains("\"ID\" BIGINT GENERATED BY DEFAULT AS IDENTITY NOT NULL"), "{s}");
    assert!(s.contains("\"CREADO\" TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP"), "{s}");
}

#[test]
fn postgres_to_hana() {
    let r = conv(&pg_tables(), "postgres", "hana");
    let p = &r.tables[1];
    assert_eq!(ty(p, "CHICO"), "SMALLINT");
    assert_eq!(ty(p, "RATIO"), "DECIMAL");
    assert_eq!(ty(p, "ESTADO"), "NVARCHAR(20)");
    assert_eq!(ty(p, "LETRA"), "NCHAR(1)");
    assert_eq!(ty(p, "NOTAS"), "NCLOB");
    assert_eq!(ty(p, "BYTES"), "BLOB");
    assert_eq!(ty(p, "HORA"), "TIME");
    assert_eq!(ty(p, "CREADO"), "TIMESTAMP");
    assert!(has(&r, IssueCode::TimeZoneLoss, "creado"));
    assert_eq!(dflt(p, "CREADO"), Some("CURRENT_TIMESTAMP"));
    assert_eq!(ty(p, "CODIGO"), "VARBINARY(16)");
    // HANA has RESTRICT but no NO ACTION.
    assert_eq!(p.foreign_keys[0].on_update.as_deref(), Some("RESTRICT"));
    let s = ddl("hana", &r.tables);
    assert!(s.contains("CREATE COLUMN TABLE \"PEDIDOS\""), "{s}");
    assert!(s.contains("\"ID\" BIGINT GENERATED BY DEFAULT AS IDENTITY NOT NULL"), "{s}");
}

#[test]
fn postgres_to_informix_and_gbase() {
    for id in ["informix", "gbase8s"] {
        let r = conv(&pg_tables(), "postgres", id);
        let p = &r.tables[1];
        // Informix folds to lower case like PostgreSQL.
        assert_eq!(p.name, "pedidos");
        assert_eq!(ty(p, "id"), "BIGINT");
        assert_eq!(ty(p, "ratio"), "DECIMAL(32)");
        assert_eq!(ty(p, "doble"), "FLOAT");
        assert_eq!(dflt(p, "activo"), Some("'t'"));
        assert_eq!(ty(p, "estado"), "NVARCHAR(20)");
        assert_eq!(ty(p, "notas"), "TEXT");
        assert_eq!(ty(p, "bytes"), "BYTE");
        assert_eq!(dflt(p, "dia"), Some("TODAY"));
        assert_eq!(ty(p, "hora"), "DATETIME HOUR TO SECOND");
        assert_eq!(ty(p, "creado"), "DATETIME YEAR TO FRACTION(5)");
        assert_eq!(dflt(p, "creado"), Some("CURRENT YEAR TO FRACTION(5)"));
        assert!(has(&r, IssueCode::TimeZoneLoss, "creado"));
        assert_eq!(ty(p, "codigo"), "CHAR(36)");
        assert!(!r.issues.iter().any(|i| i.code == IssueCode::ForeignKeyActionChanged));
        let s = ddl(id, &r.tables);
        assert!(s.contains("\"id\" BIGSERIAL NOT NULL"), "{s}");
        assert!(s.contains("ON DELETE CASCADE CONSTRAINT \"fk_pedidos_clientes\""), "{s}");
        assert!(!s.contains("COMMENT"));
    }
}

#[test]
fn postgres_to_sybase_ase() {
    let r = conv(&pg_tables(), "postgres", "sybase");
    let p = &r.tables[1];
    assert_eq!(ty(p, "id"), "bigint");
    assert_eq!(ty(p, "total"), "numeric(12, 2)");
    // Nullable booleans can't be bit in ASE.
    assert_eq!(ty(p, "activo"), "tinyint");
    assert_eq!(dflt(p, "activo"), Some("1"));
    assert!(has(&r, IssueCode::TypeChanged, "activo"));
    assert_eq!(ty(p, "estado"), "univarchar(20)");
    assert_eq!(ty(p, "notas"), "unitext");
    assert_eq!(ty(p, "bytes"), "image");
    assert_eq!(ty(p, "creado"), "bigdatetime");
    assert_eq!(dflt(p, "creado"), Some("current_bigdatetime()"));
    assert_eq!(dflt(p, "dia"), Some("current_date()"));
    assert_eq!(ty(p, "codigo"), "char(36)");
    assert_eq!(dflt(p, "codigo"), Some("newid(1)"));
    // ASE has no referential actions.
    let fk = &p.foreign_keys[0];
    assert_eq!((fk.on_delete.as_deref(), fk.on_update.as_deref()), (None, None));
    assert!(r.issues.iter().any(|i| i.code == IssueCode::ForeignKeyActionChanged && i.severity == Severity::Warning));
    let s = ddl("sybase", &r.tables);
    assert!(s.contains("[id] bigint IDENTITY,"), "{s}");
    assert!(s.contains("[activo] tinyint DEFAULT 1 NULL"), "{s}");
}

#[test]
fn postgres_to_sql_anywhere() {
    let r = conv(&pg_tables(), "postgres", "sqlanywhere");
    let p = &r.tables[1];
    assert_eq!(ty(p, "activo"), "bit");
    assert_eq!(dflt(p, "activo"), Some("1"));
    assert_eq!(ty(p, "ratio"), "numeric(127, 30)");
    assert_eq!(ty(p, "estado"), "nvarchar(20)");
    assert_eq!(ty(p, "notas"), "long nvarchar");
    assert_eq!(ty(p, "bytes"), "long binary");
    assert_eq!(ty(p, "creado"), "timestamp with time zone");
    assert_eq!(dflt(p, "creado"), Some("CURRENT TIMESTAMP"));
    assert_eq!(ty(p, "codigo"), "uniqueidentifier");
    assert_eq!(dflt(p, "codigo"), Some("NEWID()"));
    assert_eq!(p.foreign_keys[0].on_update.as_deref(), Some("RESTRICT"));
    let s = ddl("sqlanywhere", &r.tables);
    assert!(s.contains("\"id\" bigint DEFAULT AUTOINCREMENT NOT NULL"), "{s}");
}

#[test]
fn postgres_to_teradata() {
    let r = conv(&pg_tables(), "postgres", "teradata");
    let p = &r.tables[1];
    assert_eq!(ty(p, "activo"), "BYTEINT");
    assert_eq!(dflt(p, "activo"), Some("1"));
    assert_eq!(ty(p, "ratio"), "NUMBER");
    assert_eq!(ty(p, "doble"), "FLOAT");
    assert_eq!(ty(p, "estado"), "VARCHAR(20) CHARACTER SET UNICODE");
    assert_eq!(ty(p, "notas"), "CLOB CHARACTER SET UNICODE");
    assert_eq!(ty(p, "bytes"), "BLOB");
    assert_eq!(ty(p, "creado"), "TIMESTAMP WITH TIME ZONE");
    assert_eq!(dflt(p, "creado"), Some("CURRENT_TIMESTAMP"));
    assert_eq!(ty(p, "datos"), "JSON(8388096) CHARACTER SET UNICODE");
    let fk = &p.foreign_keys[0];
    assert_eq!((fk.on_delete.as_deref(), fk.on_update.as_deref()), (None, None));
    let s = ddl("teradata", &r.tables);
    assert!(s.contains("CREATE MULTISET TABLE \"pedidos\""), "{s}");
    assert!(s.contains("CREATE INDEX \"ix_pedidos_dia\" (\"dia\") ON \"pedidos\";"), "{s}");
}

#[test]
fn postgres_to_vertica() {
    let r = conv(&pg_tables(), "postgres", "vertica");
    let p = &r.tables[1];
    assert_eq!(ty(p, "chico"), "INTEGER");
    assert_eq!(ty(p, "ratio"), "NUMERIC(37, 15)");
    // Vertica counts bytes: 4 per character.
    assert_eq!(ty(p, "estado"), "VARCHAR(80)");
    assert_eq!(ty(p, "letra"), "CHAR(4)");
    assert_eq!(ty(p, "notas"), "LONG VARCHAR(32000000)");
    assert_eq!(ty(p, "creado"), "TIMESTAMPTZ");
    assert_eq!(dflt(p, "creado"), Some("CURRENT_TIMESTAMP"));
    assert_eq!(ty(p, "local_ts"), "TIMESTAMP");
    assert_eq!(ty(p, "codigo"), "UUID");
    assert_eq!(dflt(p, "codigo"), Some("UUID_GENERATE()"));
    // An array of unbounded text can't be a Vertica ARRAY: JSON in text.
    assert_eq!(ty(p, "etiquetas"), "LONG VARCHAR(32000000)");
    // No user indexes in Vertica.
    assert!(p.indexes.is_empty());
    assert!(has(&r, IssueCode::IndexDropped, "ix_pedidos_dia"));
    let s = ddl("vertica", &r.tables);
    assert!(s.contains("\"id\" IDENTITY NOT NULL"), "{s}");
}

#[test]
fn postgres_to_exasol() {
    let r = conv(&pg_tables(), "postgres", "exasol");
    let p = &r.tables[1];
    assert_eq!(p.name, "PEDIDOS");
    assert_eq!(ty(p, "ID"), "DECIMAL(19, 0)");
    assert_eq!(ty(p, "CHICO"), "DECIMAL(5, 0)");
    assert_eq!(ty(p, "ACTIVO"), "BOOLEAN");
    assert_eq!(ty(p, "ESTADO"), "VARCHAR(20) UTF8");
    assert_eq!(ty(p, "NOTAS"), "VARCHAR(2000000) UTF8");
    assert_eq!(ty(p, "BYTES"), "VARCHAR(2000000) ASCII");
    assert!(has(&r, IssueCode::TypeApproximated, "bytes"));
    assert_eq!(ty(p, "HORA"), "TIMESTAMP");
    assert_eq!(ty(p, "CREADO"), "TIMESTAMP WITH LOCAL TIME ZONE");
    assert_eq!(dflt(p, "CREADO"), Some("CURRENT_TIMESTAMP"));
    assert_eq!(dflt(p, "DIA"), Some("CURRENT_DATE"));
    assert_eq!(ty(p, "CODIGO"), "HASHTYPE(16 BYTE)");
    assert!(p.indexes.is_empty());
    let s = ddl("exasol", &r.tables);
    assert!(s.contains("\"ID\" DECIMAL(19, 0) IDENTITY NOT NULL"), "{s}");
}

#[test]
fn postgres_to_netezza() {
    let r = conv(&pg_tables(), "postgres", "netezza");
    let p = &r.tables[1];
    assert_eq!(ty(p, "ID"), "BIGINT");
    // No identity columns: reported.
    assert!(!column(p, "ID").auto_increment);
    assert!(has(&r, IssueCode::AutoIncrementDropped, "id"));
    assert_eq!(ty(p, "ESTADO"), "NVARCHAR(20)");
    assert_eq!(ty(p, "NOTAS"), "NVARCHAR(16000)");
    assert!(has(&r, IssueCode::LengthLoss, "notas"));
    assert_eq!(ty(p, "BYTES"), "VARBINARY(64000)");
    assert_eq!(ty(p, "CREADO"), "TIMESTAMP");
    assert_eq!(ty(p, "DATOS"), "NVARCHAR(16000)");
    assert!(p.indexes.is_empty());
    // Distributed on the primary key.
    assert_eq!(p.options.get("distribute_on").map(String::as_str), Some("ID"));
    assert!(r.issues.iter().any(|i| i.code == IssueCode::OptionAdded && i.object.as_deref() == Some("distribute_on")));
    let s = ddl("netezza", &r.tables);
    assert!(s.contains(") DISTRIBUTE ON (\"ID\");"), "{s}");
}

// ---------------------------------------------------------------------------
// MySQL and SQL Server → the engines

#[test]
fn mysql_to_enterprise() {
    let t = mysql_table();
    let fb = conv(&t, "mysql", "firebird");
    let p = &fb.tables[0];
    // int unsigned needs a wider signed type; bigint unsigned needs INT128.
    assert_eq!(ty(p, "ID"), "BIGINT");
    assert_eq!(ty(p, "GRANDE"), "INT128");
    assert_eq!(ty(p, "ACTIVO"), "BOOLEAN");
    assert_eq!(ty(p, "STOCK"), "INTEGER");
    assert_eq!(ty(p, "TIPO"), "VARCHAR(6)");
    assert!(fb.issues.iter().any(|i| i.object.as_deref() == Some("tipo") && i.message.contains("chico, grande")));
    assert_eq!(ty(p, "ALTA"), "TIMESTAMP");
    assert_eq!(ty(p, "TS"), "TIMESTAMP WITH TIME ZONE");
    assert_eq!(ty(p, "FOTO"), "BLOB SUB_TYPE BINARY");

    let ase = conv(&t, "mysql", "sybase");
    let p = &ase.tables[0];
    // ASE has the unsigned types.
    assert_eq!(ty(p, "id"), "unsigned int");
    assert_eq!(ty(p, "grande"), "unsigned bigint");
    assert_eq!(ty(p, "anio"), "smallint");
    assert_eq!(ty(p, "alta"), "datetime");
    assert_eq!(ty(p, "foto"), "image");

    let hana = conv(&t, "mysql", "hana");
    assert_eq!(ty(&hana.tables[0], "GRANDE"), "DECIMAL(20, 0)");
    assert_eq!(ty(&hana.tables[0], "EXTRA"), "NCLOB");

    let td = conv(&t, "mysql", "teradata");
    assert_eq!(ty(&td.tables[0], "alta"), "TIMESTAMP(3)");
    assert_eq!(ty(&td.tables[0], "extra"), "JSON(8388096) CHARACTER SET UNICODE");

    let v = conv(&t, "mysql", "vertica");
    assert_eq!(ty(&v.tables[0], "grande"), "NUMERIC(20, 0)");
    assert_eq!(ty(&v.tables[0], "nombre"), "VARCHAR(200)");

    let d = conv(&t, "mysql", "db2");
    assert_eq!(ty(&d.tables[0], "GRANDE"), "DECIMAL(20, 0)");
    assert_eq!(ty(&d.tables[0], "ALTA"), "TIMESTAMP(3)");
    for id in ["db2", "firebird", "hana", "informix", "sybase", "sqlanywhere", "teradata", "vertica", "exasol", "netezza"] {
        let r = conv(&t, "mysql", id);
        ddl(id, &r.tables);
    }
}

#[test]
fn sqlserver_to_enterprise() {
    let t = mssql_table();
    let ase = conv(&t, "sqlserver", "sybase");
    let v = &ase.tables[0];
    assert_eq!(ty(v, "Codigo"), "char(36)");
    assert_eq!(dflt(v, "Codigo"), Some("newid(1)"));
    assert_eq!(ty(v, "Monto"), "money");
    assert_eq!(ty(v, "Fecha"), "datetime");
    assert_eq!(ty(v, "Exacta"), "bigdatetime");
    assert!(has(&ase, IssueCode::PrecisionLoss, "Exacta"));
    assert_eq!(ty(v, "Nombre"), "univarchar(50)");
    assert_eq!(ty(v, "Chico"), "tinyint");
    assert_eq!(ty(v, "Version"), "timestamp");
    assert_eq!(ty(v, "Activo"), "tinyint");
    assert_eq!(dflt(v, "Activo"), Some("1"));

    let sqla = conv(&t, "sqlserver", "sqlanywhere");
    let v = &sqla.tables[0];
    assert_eq!(ty(v, "Codigo"), "uniqueidentifier");
    assert_eq!(dflt(v, "Codigo"), Some("NEWID()"));
    assert_eq!(ty(v, "Zona"), "timestamp with time zone");
    assert_eq!(ty(v, "Doc"), "xml");

    let ifx = conv(&t, "sqlserver", "informix");
    let v = &ifx.tables[0];
    assert_eq!(ty(v, "Exacta"), "DATETIME YEAR TO FRACTION(5)");
    assert!(has(&ifx, IssueCode::PrecisionLoss, "Exacta"));
    assert_eq!(ty(v, "Fecha"), "DATETIME YEAR TO FRACTION(3)");
    assert_eq!(ty(v, "Monto"), "MONEY(19, 4)");
    assert_eq!(ty(v, "Chico"), "SMALLINT");

    let db2 = conv(&t, "sqlserver", "db2");
    let v = &db2.tables[0];
    // Mixed-case names were quoted on purpose: kept.
    assert_eq!(v.name, "Ventas");
    assert_eq!(ty(v, "Exacta"), "TIMESTAMP(7)");
    assert_eq!(ty(v, "Doc"), "XML");
    assert_eq!(ty(v, "Texto"), "CLOB(1G)");
    assert_eq!(ty(v, "Bytes"), "BLOB(1G)");
    assert_eq!(ty(v, "Activo"), "BOOLEAN");
    assert_eq!(dflt(v, "Activo"), Some("TRUE"));

    let td = conv(&t, "sqlserver", "teradata");
    assert_eq!(ty(&td.tables[0], "Exacta"), "TIMESTAMP(6)");
    assert_eq!(ty(&td.tables[0], "Doc"), "XML");

    let ex = conv(&t, "sqlserver", "exasol");
    assert_eq!(ty(&ex.tables[0], "Codigo"), "HASHTYPE(16 BYTE)");
    assert_eq!(ty(&ex.tables[0], "Exacta"), "TIMESTAMP(7)");

    let nz = conv(&t, "sqlserver", "netezza");
    assert_eq!(ty(&nz.tables[0], "Texto"), "NVARCHAR(16000)");
    assert_eq!(nz.tables[0].options.get("distribute_on").map(String::as_str), Some("Id"));
}

// ---------------------------------------------------------------------------
// The engines → PostgreSQL, MySQL, SQL Server

#[test]
fn db2_to_postgres_and_sqlserver() {
    let t = table(
        "EMPLEADOS",
        vec![
            nn(auto(c("ID", "INTEGER"))),
            c("NOMBRE", "VARCHAR(40)"),
            c("SUELDO", "DECIMAL(12,2)"),
            c("RATIO", "DECFLOAT"),
            def(c("ALTA", "TIMESTAMP"), "'2024-01-01-00.00.00'"),
            c("HUELLA", "CHAR () FOR BIT DATA"),
            c("CV", "CLOB"),
            c("FOTO", "BLOB"),
            c("PERFIL", "XML"),
            c("ACTIVO", "BOOLEAN"),
            c("SIGLA", "GRAPHIC(5)"),
            c("DIA", "DATE"),
            c("HORA", "TIME"),
            def(c("SALDO", "DOUBLE"), "0"),
        ],
        &["ID"],
    );
    let r = conv(std::slice::from_ref(&t), "db2", "postgres");
    let e = &r.tables[0];
    assert_eq!(e.name, "empleados");
    assert_eq!(ty(e, "id"), "integer");
    assert!(column(e, "id").auto_increment);
    assert_eq!(ty(e, "nombre"), "varchar(40)");
    assert_eq!(ty(e, "sueldo"), "numeric(12, 2)");
    assert_eq!(ty(e, "ratio"), "numeric");
    assert_eq!(ty(e, "alta"), "timestamp(6)");
    assert_eq!(ty(e, "huella"), "bytea");
    assert_eq!(ty(e, "cv"), "text");
    assert_eq!(ty(e, "perfil"), "xml");
    assert_eq!(ty(e, "activo"), "boolean");
    assert_eq!(ty(e, "sigla"), "char(5)");
    assert_eq!(ty(e, "hora"), "time(0)");
    assert_eq!(dflt(e, "saldo"), Some("0"));

    let r = conv(&[t], "db2", "sqlserver");
    let e = &r.tables[0];
    assert_eq!(ty(e, "NOMBRE"), "nvarchar(40)");
    assert_eq!(ty(e, "RATIO"), "decimal(38, 10)");
    assert_eq!(ty(e, "HUELLA"), "varbinary(max)");
    assert_eq!(ty(e, "CV"), "nvarchar(max)");
    assert_eq!(ty(e, "ALTA"), "datetime2(6)");
}

/// Firebird as its driver reports it.
fn fb_table() -> Vec<TableSchema> {
    vec![table(
        "MOVIMIENTOS",
        vec![
            nn(auto(c("ID", "BIGINT"))),
            c("ENORME", "INT128"),
            def(c("IMPORTE", "NUMERIC(18,4)"), "0"),
            c("DECF", "DECFLOAT(34)"),
            c("REAL4", "FLOAT"),
            def(c("ACTIVO", "BOOLEAN"), "TRUE"),
            c("NOMBRE", "VARCHAR(50)"),
            c("NOTAS", "BLOB SUB_TYPE TEXT"),
            c("DATOS", "BLOB SUB_TYPE BINARY"),
            def(c("CREADO", "TIMESTAMP"), "LOCALTIMESTAMP"),
            c("ZONA", "TIMESTAMP WITH TIME ZONE"),
            c("HORA", "TIME"),
            c("DOBLE", "COMPUTED BY (IMPORTE * 2)"),
        ],
        &["ID"],
    )]
}

#[test]
fn firebird_to_mysql_and_sqlserver() {
    let r = conv(&fb_table(), "firebird", "mysql");
    let m = &r.tables[0];
    // MySQL keeps the case it's given.
    assert_eq!(m.name, "MOVIMIENTOS");
    assert_eq!(ty(m, "ID"), "bigint");
    assert_eq!(ty(m, "IMPORTE"), "decimal(18, 4)");
    assert_eq!(ty(m, "ACTIVO"), "tinyint(1)");
    assert_eq!(ty(m, "NOTAS"), "longtext");
    assert_eq!(ty(m, "DATOS"), "longblob");
    assert_eq!(dflt(m, "CREADO"), Some("CURRENT_TIMESTAMP(4)"));
    // A computed column has no type to convert: kept and reported.
    assert!(has(&r, IssueCode::TypeUnknown, "DOBLE"));

    let r = conv(&fb_table(), "firebird", "sqlserver");
    let m = &r.tables[0];
    assert_eq!(ty(m, "ENORME"), "decimal(38, 0)");
    assert!(has(&r, IssueCode::RangeLoss, "ENORME"));
    assert_eq!(ty(m, "REAL4"), "real");
    assert_eq!(ty(m, "NOMBRE"), "nvarchar(50)");
    assert_eq!(ty(m, "CREADO"), "datetime2(4)");
    assert_eq!(ty(m, "ZONA"), "datetimeoffset(4)");
    assert_eq!(ty(m, "HORA"), "time(4)");
    assert_eq!(dflt(m, "ACTIVO"), Some("1"));
}

#[test]
fn hana_to_sqlserver_and_postgres() {
    let t = table(
        "PRODUCTOS",
        vec![
            nn(auto(c("ID", "INTEGER"))),
            c("STOCK", "TINYINT"),
            c("NOMBRE", "NVARCHAR(100)"),
            c("CODIGO", "VARCHAR(10)"),
            c("PRECIO", "DECIMAL(12,2)"),
            c("FLOTANTE", "DECIMAL"),
            c("ALTA", "SECONDDATE"),
            def(c("CREADO", "TIMESTAMP"), "CURRENT_TIMESTAMP"),
            c("UBICACION", "ST_POINT(4326)"),
            c("DESCRIPCION", "NCLOB"),
            c("HUELLA", "VARBINARY(16)"),
        ],
        &["ID"],
    );
    let r = conv(std::slice::from_ref(&t), "hana", "sqlserver");
    let m = &r.tables[0];
    assert_eq!(ty(m, "STOCK"), "tinyint");
    assert_eq!(ty(m, "NOMBRE"), "nvarchar(100)");
    assert_eq!(ty(m, "CODIGO"), "varchar(10)");
    assert_eq!(ty(m, "ALTA"), "datetime2(0)");
    assert_eq!(ty(m, "CREADO"), "datetime2(7)");
    assert_eq!(dflt(m, "CREADO"), Some("SYSDATETIME()"));
    assert_eq!(ty(m, "UBICACION"), "geometry");
    assert_eq!(ty(m, "DESCRIPCION"), "nvarchar(max)");

    let r = conv(&[t], "hana", "postgres");
    let m = &r.tables[0];
    assert_eq!(ty(m, "stock"), "smallint");
    assert_eq!(ty(m, "flotante"), "numeric");
    assert_eq!(ty(m, "creado"), "timestamp(6)");
    assert!(has(&r, IssueCode::PrecisionLoss, "CREADO"));
    assert_eq!(ty(m, "ubicacion"), "geometry(point, 4326)");
}

#[test]
fn informix_to_postgres() {
    let t = table(
        "facturas",
        vec![
            nn(c("id", "serial")),
            c("numero", "int8"),
            c("importe", "money(16,2)"),
            c("detalle", "lvarchar(2048)"),
            c("alta", "datetime year to fraction(3)"),
            c("cierre", "datetime year to second"),
            c("plazo", "interval day(3) to second"),
            def(c("pagada", "boolean"), "'f'"),
            c("pdf", "byte"),
            c("texto", "text"),
            c("hora", "datetime hour to minute"),
            c("ratio", "decimal(16)"),
        ],
        &["id"],
    );
    let r = conv(&[t], "informix", "postgres");
    let f = &r.tables[0];
    // SERIAL is an auto-increment by its type.
    assert!(column(f, "id").auto_increment);
    assert_eq!(ty(f, "id"), "integer");
    assert_eq!(ty(f, "numero"), "bigint");
    assert_eq!(ty(f, "importe"), "numeric(19, 4)");
    assert_eq!(ty(f, "detalle"), "varchar(2048)");
    assert_eq!(ty(f, "alta"), "timestamp(3)");
    assert_eq!(ty(f, "cierre"), "timestamp(0)");
    assert_eq!(ty(f, "plazo"), "interval");
    assert_eq!(ty(f, "pdf"), "bytea");
    assert_eq!(ty(f, "texto"), "text");
    assert_eq!(ty(f, "hora"), "time(0)");
    assert_eq!(ty(f, "ratio"), "numeric");
}

#[test]
fn sybase_engines_to_mysql_and_postgres() {
    let ase = table(
        "cuentas",
        vec![
            nn(auto(c("id", "numeric(10,0)"))),
            c("saldo", "money"),
            c("visitas", "unsigned int"),
            c("nombre", "univarchar(20)"),
            c("notas", "unitext"),
            c("alta", "bigdatetime"),
            c("vieja", "smalldatetime"),
            nn(c("activa", "bit")),
            c("version", "timestamp"),
            c("foto", "image"),
        ],
        &["id"],
    );
    let r = conv(&[ase], "sybase", "mysql");
    let m = &r.tables[0];
    // numeric(10,0) identity: MySQL's AUTO_INCREMENT needs an integer.
    assert_eq!(ty(m, "id"), "bigint");
    assert_eq!(ty(m, "visitas"), "int unsigned");
    assert_eq!(ty(m, "nombre"), "varchar(20)");
    assert_eq!(ty(m, "notas"), "longtext");
    assert_eq!(ty(m, "alta"), "datetime(6)");
    assert_eq!(ty(m, "vieja"), "datetime");
    assert_eq!(ty(m, "activa"), "tinyint(1)");
    assert!(has(&r, IssueCode::TypeApproximated, "version"));

    let sqla = table(
        "eventos",
        vec![
            nn(auto(c("id", "unsigned bigint"))),
            c("clave", "uniqueidentifier"),
            c("cuando", "timestamp with time zone"),
            c("texto", "long nvarchar"),
            c("mascara", "varbit(12)"),
            c("importe", "numeric(40,2)"),
            c("forma", "st_geometry"),
        ],
        &["id"],
    );
    let r = conv(&[sqla], "sqlanywhere", "postgres");
    let p = &r.tables[0];
    // An identity column has to stay an integer in PostgreSQL.
    assert_eq!(ty(p, "id"), "bigint");
    assert_eq!(ty(p, "clave"), "uuid");
    assert_eq!(ty(p, "cuando"), "timestamp(6) with time zone");
    assert_eq!(ty(p, "texto"), "text");
    assert_eq!(ty(p, "mascara"), "bit(12)");
    assert_eq!(ty(p, "importe"), "numeric(40, 2)");
    assert_eq!(ty(p, "forma"), "geometry");
}

#[test]
fn teradata_vertica_exasol_netezza_to_the_big_three() {
    let td = table(
        "ventas",
        vec![
            c("id", "INTEGER"),
            c("flag", "BYTEINT"),
            c("nombre", "VARCHAR(100) CHARACTER SET UNICODE"),
            c("codigo", "CHAR(3)"),
            c("cuando", "TIMESTAMP(0)"),
            c("importe", "NUMBER"),
            c("vigencia", "PERIOD(DATE)"),
            c("doc", "JSON"),
            c("bytes", "VARBYTE(200)"),
        ],
        &["id"],
    );
    let r = conv(&[td], "teradata", "sqlserver");
    let m = &r.tables[0];
    assert_eq!(ty(m, "flag"), "smallint");
    assert_eq!(ty(m, "nombre"), "nvarchar(100)");
    assert_eq!(ty(m, "codigo"), "char(3)");
    assert_eq!(ty(m, "cuando"), "datetime2(0)");
    assert_eq!(ty(m, "importe"), "decimal(38, 10)");
    assert_eq!(ty(m, "vigencia"), "PERIOD(DATE)");
    assert!(has(&r, IssueCode::TypeUnknown, "vigencia"));
    assert_eq!(ty(m, "bytes"), "varbinary(200)");

    let v = table(
        "clicks",
        vec![
            c("id", "int"),
            c("url", "varchar(80)"),
            c("cuando", "timestamptz"),
            c("visitante", "uuid"),
            c("tags", "Array[int]"),
            c("crudo", "long varbinary(1048576)"),
            c("ratio", "numeric(37,15)"),
        ],
        &["id"],
    );
    let r = conv(&[v], "vertica", "postgres");
    let p = &r.tables[0];
    assert_eq!(ty(p, "id"), "bigint");
    assert_eq!(ty(p, "url"), "varchar(80)");
    assert_eq!(ty(p, "cuando"), "timestamp(6) with time zone");
    assert_eq!(ty(p, "visitante"), "uuid");
    assert_eq!(ty(p, "tags"), "bigint[]");
    assert_eq!(ty(p, "crudo"), "bytea");
    assert_eq!(ty(p, "ratio"), "numeric(37, 15)");

    let e = table(
        "LECTURAS",
        vec![
            c("ID", "DECIMAL(18,0)"),
            c("TOTAL", "DECIMAL(36,0)"),
            c("NOMBRE", "VARCHAR(100) UTF8"),
            c("CUANDO", "TIMESTAMP WITH LOCAL TIME ZONE"),
            c("HUELLA", "HASHTYPE(16 BYTE)"),
            c("LUGAR", "GEOMETRY(4326)"),
            c("OK", "BOOLEAN"),
        ],
        &["ID"],
    );
    let r = conv(&[e], "exasol", "postgres");
    let p = &r.tables[0];
    assert_eq!(ty(p, "id"), "bigint");
    assert_eq!(ty(p, "total"), "numeric(36, 0)");
    assert_eq!(ty(p, "nombre"), "varchar(100)");
    assert_eq!(ty(p, "cuando"), "timestamp(3) with time zone");
    assert_eq!(ty(p, "huella"), "uuid");
    assert_eq!(ty(p, "lugar"), "geometry");
    assert_eq!(ty(p, "ok"), "boolean");

    let n = table(
        "PAGOS",
        vec![
            c("ID", "BIGINT"),
            c("FLAG", "BYTEINT"),
            c("NOMBRE", "NATIONAL CHARACTER VARYING(100)"),
            c("NOTA", "CHARACTER VARYING(500)"),
            c("PLAZO", "INTERVAL"),
            c("CUANDO", "TIMESTAMP"),
        ],
        &["ID"],
    );
    let r = conv(&[n], "netezza", "mysql");
    let m = &r.tables[0];
    assert_eq!(ty(m, "ID"), "bigint");
    assert_eq!(ty(m, "FLAG"), "tinyint");
    assert_eq!(ty(m, "NOMBRE"), "varchar(100)");
    assert_eq!(ty(m, "CUANDO"), "datetime(6)");
}

#[test]
fn between_enterprise_engines() {
    // Db2 → Informix: names fold from upper to lower, DECFLOAT has no room.
    let t = table("T", vec![nn(auto(c("ID", "BIGINT"))), c("D", "DECFLOAT"), c("TS", "TIMESTAMP(12)")], &["ID"]);
    let r = conv(&[t], "db2", "informix");
    let x = &r.tables[0];
    assert_eq!(x.name, "t");
    assert_eq!(ty(x, "ts"), "DATETIME YEAR TO FRACTION(5)");
    assert!(has(&r, IssueCode::PrecisionLoss, "TS"));
    // Teradata → Exasol: UNICODE text, no indexes, upper-case names.
    let mut t = table("t", vec![c("a", "VARCHAR(10) CHARACTER SET UNICODE")], &[]);
    t.indexes.push(IndexDef { name: "ix".into(), columns: vec!["a".into()], ..Default::default() });
    let r = conv(&[t], "teradata", "exasol");
    assert_eq!(r.tables[0].name, "T");
    assert_eq!(ty(&r.tables[0], "A"), "VARCHAR(10) UTF8");
    assert!(r.tables[0].indexes.is_empty());
    // Same family keeps the native spelling (db2 → db2zos).
    let t = table("T", vec![c("A", "VARGRAPHIC(10)")], &[]);
    let r = conv(&[t], "db2", "db2zos");
    assert_eq!(ty(&r.tables[0], "A"), "VARGRAPHIC(10)");
}

// ---------------------------------------------------------------------------
// Against real servers

async fn fb() -> Option<Box<dyn dbine_driver::Session>> {
    common::session("firebird", "DBINE_TEST_FIREBIRD_URL").await
}

async fn pg() -> Option<Box<dyn dbine_driver::Session>> {
    common::session("postgres", "DBINE_TEST_POSTGRES_URL").await
}

async fn mssql() -> Option<Box<dyn dbine_driver::Session>> {
    common::session("sqlserver", "DBINE_TEST_SQLSERVER_URL").await
}

fn show(rt: &RoundTrip) {
    for i in &rt.conversion.issues {
        eprintln!("  {:?} {:?} {}.{}: {}", i.severity, i.code, i.table, i.object.as_deref().unwrap_or(""), i.message);
    }
    eprintln!("{}", rt.ddl.join("\n"));
    for t in &rt.back {
        eprintln!("{}: {:?}", t.name, t.columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str(), c.nullable)).collect::<Vec<_>>());
    }
}

/// Assert the types read back, by column (case-insensitive).
fn expect(t: &TableSchema, cols: &[(&str, &str)]) {
    for (c, want) in cols {
        assert_eq!(col(t, c).data_type.to_ascii_lowercase(), want.to_ascii_lowercase(), "{}.{c}", t.name);
    }
}

async fn count(s: &mut Box<dyn dbine_driver::Session>, sql: &str) -> String {
    let mut out = dbine_driver::QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
    let v = serde_json::to_value(&out).unwrap();
    let found = v.pointer("/results/0/rows/0/0").cloned().unwrap_or_default();
    match found {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

#[tokio::test]
#[ignore]
async fn live_postgres_firebird_both_ways() {
    let (Some(mut p), Some(mut f)) = (pg().await, fb().await) else { return };
    exec_quiet(&mut p, &["DROP TABLE IF EXISTS fbx_tipos", "DROP TABLE IF EXISTS fbx_clientes"]).await;
    exec(
        &mut p,
        "CREATE TABLE fbx_clientes (id integer PRIMARY KEY, nombre varchar(80) NOT NULL);
         CREATE TABLE fbx_tipos (
           id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
           cliente_id integer REFERENCES fbx_clientes(id) ON DELETE CASCADE,
           chico smallint, entero integer, grande bigint,
           precio numeric(12,2) DEFAULT 0, enorme numeric(38,10), real4 real, doble double precision,
           activo boolean DEFAULT true, codigo char(5), nombre varchar(100) DEFAULT 'sin nombre', notas text,
           datos bytea, dia date DEFAULT CURRENT_DATE, hora time(3), creado timestamp(3) DEFAULT CURRENT_TIMESTAMP,
           zona timestamptz, uid uuid, doc jsonb
         );
         CREATE INDEX fbx_tipos_dia ON fbx_tipos (dia);",
    )
    .await;
    let original = read(&mut p, &["fbx_clientes", "fbx_tipos"]).await;

    // PostgreSQL → Firebird.
    let rt = round_trip(&mut p, "postgres", &mut f, "firebird", &["fbx_clientes", "fbx_tipos"]).await;
    show(&rt);
    let t = &rt.back[1];
    expect(
        t,
        &[
            ("ID", "BIGINT"),
            ("CHICO", "SMALLINT"),
            ("GRANDE", "BIGINT"),
            ("PRECIO", "NUMERIC(12,2)"),
            ("ENORME", "NUMERIC(38,10)"),
            ("REAL4", "FLOAT"),
            ("DOBLE", "DOUBLE PRECISION"),
            ("ACTIVO", "BOOLEAN"),
            ("CODIGO", "CHAR(5)"),
            ("NOMBRE", "VARCHAR(100)"),
            ("NOTAS", "BLOB SUB_TYPE TEXT"),
            ("DATOS", "BLOB SUB_TYPE BINARY"),
            ("DIA", "DATE"),
            ("HORA", "TIME"),
            ("CREADO", "TIMESTAMP"),
            ("ZONA", "TIMESTAMP WITH TIME ZONE"),
            // BINARY(16) is CHAR(16) CHARACTER SET OCTETS; the driver
            // doesn't report the character set.
            ("UID", "CHAR(16)"),
            ("DOC", "BLOB SUB_TYPE TEXT"),
        ],
    );
    assert!(col(t, "ID").auto_increment);
    assert!(!col(t, "ID").nullable);
    assert_eq!(col(t, "ACTIVO").default_value.as_deref(), Some("TRUE"));
    assert_eq!(col(t, "DIA").default_value.as_deref(), Some("CURRENT_DATE"));
    assert_eq!(col(t, "CREADO").default_value.as_deref(), Some("LOCALTIMESTAMP"));
    assert_eq!(t.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    assert!(t.indexes.iter().any(|i| i.columns == ["DIA"]));

    // The same row of edge values on both sides.
    exec(
        &mut p,
        "INSERT INTO fbx_clientes VALUES (1, 'Ñandú 漢字 🎉');
         INSERT INTO fbx_tipos (cliente_id, chico, entero, grande, precio, enorme, real4, doble, activo, codigo, nombre, notas, datos, dia, hora, creado, zona, uid, doc)
         VALUES (1, -32768, 2147483647, 9223372036854775807, 9999999999.99, 9999999999999999999999999999.9999999999, 3.4e38,
                 1.7976931348623157e308, false, 'abcde', 'ñandú ☃ 😀', 'línea 1', '\\xdeadbeef', '9999-12-31', '23:59:59.999',
                 '2024-02-29 12:34:56.789', '2024-06-01 10:00:00+03', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '{\"a\": 1}');",
    )
    .await;
    exec(
        &mut f,
        "INSERT INTO FBX_CLIENTES (ID, NOMBRE) VALUES (1, 'Ñandú 漢字 🎉');
         INSERT INTO FBX_TIPOS (CLIENTE_ID, CHICO, ENTERO, GRANDE, PRECIO, ENORME, REAL4, DOBLE, ACTIVO, CODIGO, NOMBRE, NOTAS, DATOS, DIA, HORA, CREADO, ZONA, UID, DOC)
         VALUES (1, -32768, 2147483647, 9223372036854775807, 9999999999.99, 9999999999999999999999999999.9999999999, 3.4e38,
                 1.7976931348623157e308, FALSE, 'abcde', 'ñandú ☃ 😀', 'línea 1', x'DEADBEEF', DATE '9999-12-31', TIME '23:59:59.999',
                 TIMESTAMP '2024-02-29 12:34:56.789', TIMESTAMP '2024-06-01 10:00:00 +03:00', x'A0EEBC999C0B4EF8BB6D6BB9BD380A11', '{\"a\": 1}');",
    )
    .await;
    assert_eq!(count(&mut f, "SELECT COUNT(*) FROM FBX_TIPOS WHERE NOMBRE = 'ñandú ☃ 😀' AND GRANDE = 9223372036854775807").await, "1");

    // …and back: Firebird → PostgreSQL, compared with the original.
    let back = round_trip(&mut f, "firebird", &mut p, "postgres", &["FBX_CLIENTES", "FBX_TIPOS"]).await;
    show(&back);
    let (o, b) = (&original[1], &back.back[1]);
    for c in ["id", "cliente_id", "chico", "entero", "grande", "precio", "enorme", "real4", "doble", "activo", "codigo", "nombre", "notas", "datos", "dia"] {
        assert_eq!(col(b, c).data_type, col(o, c).data_type, "{c}");
    }
    // Firebird keeps 1/10000 s; its BLOB text has no JSON; UUID bytes come back as CHAR(16).
    expect(
        b,
        &[
            ("hora", "time(4) without time zone"),
            ("creado", "timestamp(4) without time zone"),
            ("zona", "timestamp(4) with time zone"),
            ("uid", "character(16)"),
            ("doc", "text"),
        ],
    );
    assert!(col(b, "id").auto_increment);
    assert_eq!(b.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    exec_quiet(&mut p, &["DROP TABLE IF EXISTS fbx_tipos", "DROP TABLE IF EXISTS fbx_clientes"]).await;
    exec_quiet(&mut f, &["DROP TABLE FBX_TIPOS", "DROP TABLE FBX_CLIENTES"]).await;
}

/// A Firebird table with its own types (INT128, DECFLOAT, time zones).
async fn fb_seed(f: &mut Box<dyn dbine_driver::Session>, prefix: &str) {
    exec_quiet(f, &[&format!("DROP TABLE {prefix}_TIPOS"), &format!("DROP TABLE {prefix}_CLIENTES")]).await;
    exec(
        f,
        &format!(
            "CREATE TABLE {prefix}_CLIENTES (ID INTEGER NOT NULL PRIMARY KEY, NOMBRE VARCHAR(80) NOT NULL);
             CREATE TABLE {prefix}_TIPOS (
               ID BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
               CLIENTE_ID INTEGER REFERENCES {prefix}_CLIENTES (ID) ON DELETE CASCADE,
               CHICO SMALLINT, ENTERO INTEGER, GRANDE BIGINT, ENORME INT128,
               PRECIO NUMERIC(18,4) DEFAULT 0, AMPLIO NUMERIC(38,6), DECF DECFLOAT(34), REAL4 FLOAT, DOBLE DOUBLE PRECISION,
               ACTIVO BOOLEAN DEFAULT TRUE, CODIGO CHAR(5), NOMBRE VARCHAR(8191) DEFAULT 'sin nombre',
               NOTAS BLOB SUB_TYPE TEXT, DATOS BLOB SUB_TYPE BINARY,
               DIA DATE DEFAULT CURRENT_DATE, HORA TIME, HORA_Z TIME WITH TIME ZONE,
               CREADO TIMESTAMP DEFAULT LOCALTIMESTAMP, ZONA TIMESTAMP WITH TIME ZONE
             );
             CREATE INDEX {prefix}_TIPOS_DIA ON {prefix}_TIPOS (DIA);"
        ),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn live_firebird_postgres_both_ways() {
    let (Some(mut p), Some(mut f)) = (pg().await, fb().await) else { return };
    fb_seed(&mut f, "FBP").await;
    let original = read(&mut f, &["FBP_CLIENTES", "FBP_TIPOS"]).await;

    // Firebird → PostgreSQL.
    let rt = round_trip(&mut f, "firebird", &mut p, "postgres", &["FBP_CLIENTES", "FBP_TIPOS"]).await;
    show(&rt);
    let t = &rt.back[1];
    assert_eq!(t.name, "fbp_tipos");
    expect(
        t,
        &[
            ("id", "bigint"),
            ("enorme", "numeric(39,0)"),
            ("precio", "numeric(18,4)"),
            ("amplio", "numeric(38,6)"),
            ("decf", "numeric"),
            ("real4", "real"),
            ("activo", "boolean"),
            ("codigo", "character(5)"),
            ("nombre", "character varying(8191)"),
            ("notas", "text"),
            ("datos", "bytea"),
            ("hora", "time(4) without time zone"),
            ("hora_z", "time(4) with time zone"),
            ("creado", "timestamp(4) without time zone"),
            ("zona", "timestamp(4) with time zone"),
        ],
    );
    assert!(col(t, "id").auto_increment);
    assert!(col(t, "creado").default_value.as_deref().is_some_and(|d| d.to_ascii_uppercase().contains("CURRENT_TIMESTAMP")));

    exec(
        &mut f,
        "INSERT INTO FBP_CLIENTES VALUES (1, 'Ñandú 漢字 🎉');
         INSERT INTO FBP_TIPOS (CLIENTE_ID, CHICO, ENTERO, GRANDE, ENORME, PRECIO, AMPLIO, DECF, REAL4, DOBLE, ACTIVO, CODIGO, NOMBRE, NOTAS, DATOS, DIA, HORA, HORA_Z, CREADO, ZONA)
         VALUES (1, -32768, -2147483648, -9223372036854775807, 170141183460469231731687303715884105727, 99999999999999.9999,
                 99999999999999999999999999999999.999999, 1.234567890123456789012345678901234E+100, 3.4e38, 1.7976931348623157e308,
                 FALSE, 'abcde', 'ñandú ☃ 😀', 'texto', x'DEADBEEF', DATE '0001-01-01', TIME '23:59:59.9999',
                 TIME '23:59:59.9999 +03:00', TIMESTAMP '9999-12-31 23:59:59.9999', TIMESTAMP '2024-06-01 10:00:00 -03:00');",
    )
    .await;
    exec(
        &mut p,
        "INSERT INTO fbp_clientes VALUES (1, 'Ñandú 漢字 🎉');
         INSERT INTO fbp_tipos (cliente_id, chico, entero, grande, enorme, precio, amplio, decf, real4, doble, activo, codigo, nombre, notas, datos, dia, hora, hora_z, creado, zona)
         VALUES (1, -32768, -2147483648, -9223372036854775807, 170141183460469231731687303715884105727, 99999999999999.9999,
                 99999999999999999999999999999999.999999, 1.234567890123456789012345678901234E+100, 3.4e38, 1.7976931348623157e308,
                 false, 'abcde', 'ñandú ☃ 😀', 'texto', '\\xdeadbeef', '0001-01-01', '23:59:59.9999',
                 '23:59:59.9999+03', '9999-12-31 23:59:59.9999', '2024-06-01 10:00:00-03');",
    )
    .await;
    assert_eq!(count(&mut p, "SELECT COUNT(*) FROM fbp_tipos WHERE enorme = 170141183460469231731687303715884105727").await, "1");

    // …and back: PostgreSQL → Firebird, compared with the original.
    let back = round_trip(&mut p, "postgres", &mut f, "firebird", &["fbp_clientes", "fbp_tipos"]).await;
    show(&back);
    let (o, b) = (&original[1], &back.back[1]);
    for c in ["ID", "CHICO", "ENTERO", "GRANDE", "PRECIO", "AMPLIO", "DECF", "REAL4", "DOBLE", "ACTIVO", "CODIGO", "NOMBRE", "NOTAS", "DATOS", "DIA", "HORA", "HORA_Z", "CREADO", "ZONA"] {
        assert_eq!(col(b, c).data_type, col(o, c).data_type, "{c}");
    }
    // numeric(39,0) doesn't fit INT128 exactly: DECFLOAT(34).
    expect(b, &[("ENORME", "DECFLOAT(34)")]);
    assert!(col(b, "ID").auto_increment);
    assert_eq!(col(b, "ACTIVO").default_value.as_deref(), Some("TRUE"));
    exec_quiet(&mut p, &["DROP TABLE IF EXISTS fbp_tipos", "DROP TABLE IF EXISTS fbp_clientes"]).await;
    exec_quiet(&mut f, &["DROP TABLE FBP_TIPOS", "DROP TABLE FBP_CLIENTES"]).await;
}

#[tokio::test]
#[ignore]
async fn live_firebird_sqlserver_both_ways() {
    let (Some(mut m), Some(mut f)) = (mssql().await, fb().await) else { return };
    fb_seed(&mut f, "FBM").await;
    let original = read(&mut f, &["FBM_CLIENTES", "FBM_TIPOS"]).await;

    // Firebird → SQL Server.
    let rt = round_trip(&mut f, "firebird", &mut m, "sqlserver", &["FBM_CLIENTES", "FBM_TIPOS"]).await;
    show(&rt);
    let t = &rt.back[1];
    expect(
        t,
        &[
            ("ID", "bigint"),
            ("ENORME", "decimal(38,0)"),
            ("AMPLIO", "decimal(38,6)"),
            ("DECF", "decimal(38,10)"),
            ("REAL4", "real"),
            ("DOBLE", "float"),
            ("ACTIVO", "bit"),
            ("CODIGO", "nchar(5)"),
            ("NOMBRE", "nvarchar(max)"),
            ("NOTAS", "nvarchar(max)"),
            ("DATOS", "varbinary(max)"),
            ("HORA", "time(4)"),
            ("HORA_Z", "time(4)"),
            ("CREADO", "datetime2(4)"),
            ("ZONA", "datetimeoffset(4)"),
        ],
    );
    assert!(rt.conversion.issues.iter().any(|i| i.code == IssueCode::RangeLoss && i.object.as_deref() == Some("ENORME")));
    assert!(rt.conversion.issues.iter().any(|i| i.code == IssueCode::TimeZoneLoss && i.object.as_deref() == Some("HORA_Z")));
    assert!(col(t, "ID").auto_increment);

    // 38 digits: what decimal(38, 0) holds.
    exec(
        &mut f,
        "INSERT INTO FBM_CLIENTES VALUES (1, 'Ñandú 漢字 🎉');
         INSERT INTO FBM_TIPOS (CLIENTE_ID, CHICO, ENTERO, GRANDE, ENORME, PRECIO, AMPLIO, DECF, REAL4, DOBLE, ACTIVO, CODIGO, NOMBRE, NOTAS, DATOS, DIA, HORA, HORA_Z, CREADO, ZONA)
         VALUES (1, -32768, -2147483648, -9223372036854775807, 99999999999999999999999999999999999999, 99999999999999.9999,
                 99999999999999999999999999999999.999999, 1.5, 3.4e38, 1.7976931348623157e308,
                 FALSE, 'abcde', 'ñandú ☃ 😀', 'texto', x'DEADBEEF', DATE '0001-01-01', TIME '23:59:59.9999',
                 TIME '23:59:59.9999 +03:00', TIMESTAMP '9999-12-31 23:59:59.9999', TIMESTAMP '2024-06-01 10:00:00 -03:00');",
    )
    .await;
    exec(
        &mut m,
        "INSERT INTO FBM_CLIENTES (ID, NOMBRE) VALUES (1, N'Ñandú 漢字 🎉');
         INSERT INTO FBM_TIPOS (CLIENTE_ID, CHICO, ENTERO, GRANDE, ENORME, PRECIO, AMPLIO, DECF, REAL4, DOBLE, ACTIVO, CODIGO, NOMBRE, NOTAS, DATOS, DIA, HORA, HORA_Z, CREADO, ZONA)
         VALUES (1, -32768, -2147483648, -9223372036854775807, 99999999999999999999999999999999999999, 99999999999999.9999,
                 99999999999999999999999999999999.999999, 1.5, 3.4e38, 1.7976931348623157e308,
                 0, N'abcde', N'ñandú ☃ 😀', N'texto', 0xDEADBEEF, '0001-01-01', '23:59:59.9999',
                 '23:59:59.9999', '9999-12-31 23:59:59.9999', '2024-06-01 10:00:00 -03:00');",
    )
    .await;
    assert_eq!(count(&mut m, "SELECT COUNT(*) FROM FBM_TIPOS WHERE NOMBRE = N'ñandú ☃ 😀'").await, "1");

    // …and back: SQL Server → Firebird.
    let back = round_trip(&mut m, "sqlserver", &mut f, "firebird", &["FBM_CLIENTES", "FBM_TIPOS"]).await;
    show(&back);
    let (o, b) = (&original[1], &back.back[1]);
    for c in ["ID", "CHICO", "ENTERO", "GRANDE", "PRECIO", "AMPLIO", "REAL4", "DOBLE", "ACTIVO", "CODIGO", "NOTAS", "DATOS", "DIA", "HORA", "CREADO", "ZONA"] {
        assert_eq!(col(b, c).data_type, col(o, c).data_type, "{c}");
    }
    // What SQL Server narrowed stays narrowed.
    expect(b, &[("ENORME", "NUMERIC(38,0)"), ("DECF", "NUMERIC(38,10)"), ("NOMBRE", "BLOB SUB_TYPE TEXT"), ("HORA_Z", "TIME")]);
    exec_quiet(&mut m, &["DROP TABLE FBM_TIPOS", "DROP TABLE FBM_CLIENTES"]).await;
    exec_quiet(&mut f, &["DROP TABLE FBM_TIPOS", "DROP TABLE FBM_CLIENTES"]).await;
}

#[tokio::test]
#[ignore]
async fn live_sqlserver_firebird_both_ways() {
    let (Some(mut m), Some(mut f)) = (mssql().await, fb().await) else { return };
    exec_quiet(&mut m, &["DROP TABLE fbz_tipos", "DROP TABLE fbz_clientes"]).await;
    exec(
        &mut m,
        "CREATE TABLE fbz_clientes (id int NOT NULL PRIMARY KEY, nombre nvarchar(80) NOT NULL);
         CREATE TABLE fbz_tipos (
           id int IDENTITY(1,1) NOT NULL PRIMARY KEY,
           cliente_id int NULL REFERENCES fbz_clientes(id) ON DELETE CASCADE,
           chico tinyint, monto money, fecha datetime, exacta datetime2(7), zona datetimeoffset(7),
           nombre nvarchar(50), texto nvarchar(max), bytes varbinary(max), codigo uniqueidentifier DEFAULT NEWID(),
           activo bit DEFAULT 1, doc xml, precio decimal(19,4), dia date, hora time(7), letras char(3), simple varchar(20)
         );",
    )
    .await;
    let original = read(&mut m, &["fbz_clientes", "fbz_tipos"]).await;

    // SQL Server → Firebird.
    let rt = round_trip(&mut m, "sqlserver", &mut f, "firebird", &["fbz_clientes", "fbz_tipos"]).await;
    show(&rt);
    let t = &rt.back[1];
    assert_eq!(t.name, "FBZ_TIPOS");
    expect(
        t,
        &[
            ("ID", "INTEGER"),
            ("CHICO", "SMALLINT"),
            ("MONTO", "NUMERIC(19,4)"),
            ("FECHA", "TIMESTAMP"),
            ("EXACTA", "TIMESTAMP"),
            ("ZONA", "TIMESTAMP WITH TIME ZONE"),
            ("NOMBRE", "VARCHAR(50)"),
            ("TEXTO", "BLOB SUB_TYPE TEXT"),
            ("BYTES", "BLOB SUB_TYPE BINARY"),
            ("CODIGO", "CHAR(16)"),
            ("ACTIVO", "BOOLEAN"),
            ("DOC", "BLOB SUB_TYPE TEXT"),
            ("PRECIO", "NUMERIC(19,4)"),
            ("DIA", "DATE"),
            ("HORA", "TIME"),
            ("LETRAS", "CHAR(3)"),
            ("SIMPLE", "VARCHAR(20)"),
        ],
    );
    assert!(col(t, "ID").auto_increment);
    assert_eq!(col(t, "ACTIVO").default_value.as_deref(), Some("TRUE"));
    assert!(rt.conversion.issues.iter().any(|i| i.code == IssueCode::PrecisionLoss && i.object.as_deref() == Some("exacta")));
    assert!(rt.conversion.issues.iter().any(|i| i.code == IssueCode::DefaultDropped && i.object.as_deref() == Some("codigo")));

    exec(
        &mut m,
        "INSERT INTO fbz_clientes VALUES (1, N'Ñandú 漢字 🎉');
         INSERT INTO fbz_tipos (cliente_id, chico, monto, fecha, exacta, zona, nombre, texto, bytes, activo, doc, precio, dia, hora, letras, simple)
         VALUES (1, 255, 922337203685477.5807, '9999-12-31 23:59:59.997', '9999-12-31 23:59:59.9999999', '2024-06-01 10:00:00.1234567 +03:00',
                 N'ñandú ☃ 😀', N'texto', 0xDEADBEEF, 1, N'<a>ñ</a>', 999999999999999.9999, '0001-01-01', '23:59:59.9999999', 'abc', 'simple');",
    )
    .await;
    exec(
        &mut f,
        "INSERT INTO FBZ_CLIENTES (ID, NOMBRE) VALUES (1, 'Ñandú 漢字 🎉');
         INSERT INTO FBZ_TIPOS (CLIENTE_ID, CHICO, MONTO, FECHA, EXACTA, ZONA, NOMBRE, TEXTO, BYTES, CODIGO, ACTIVO, DOC, PRECIO, DIA, HORA, LETRAS, SIMPLE)
         VALUES (1, 255, 922337203685477.5807, TIMESTAMP '9999-12-31 23:59:59.997', TIMESTAMP '9999-12-31 23:59:59.9999',
                 TIMESTAMP '2024-06-01 10:00:00.1234 +03:00', 'ñandú ☃ 😀', 'texto', x'DEADBEEF', x'00112233445566778899AABBCCDDEEFF',
                 TRUE, '<a>ñ</a>', 999999999999999.9999, DATE '0001-01-01', TIME '23:59:59.9999', 'abc', 'simple');",
    )
    .await;
    assert_eq!(count(&mut f, "SELECT COUNT(*) FROM FBZ_TIPOS WHERE MONTO = 922337203685477.5807").await, "1");

    // …and back: Firebird → SQL Server.
    let back = round_trip(&mut f, "firebird", &mut m, "sqlserver", &["FBZ_CLIENTES", "FBZ_TIPOS"]).await;
    show(&back);
    let (o, b) = (&original[1], &back.back[1]);
    for c in ["id", "cliente_id", "nombre", "texto", "bytes", "activo", "precio", "dia"] {
        assert_eq!(col(b, c).data_type, col(o, c).data_type, "{c}");
    }
    expect(
        b,
        &[
            // tinyint went through SMALLINT; money through NUMERIC(19,4).
            ("chico", "smallint"),
            ("monto", "decimal(19,4)"),
            ("fecha", "datetime2(4)"),
            ("exacta", "datetime2(4)"),
            ("zona", "datetimeoffset(4)"),
            ("doc", "nvarchar(max)"),
            ("hora", "time(4)"),
            ("letras", "nchar(3)"),
            ("simple", "nvarchar(20)"),
        ],
    );
    assert!(col(b, "id").auto_increment);
    exec_quiet(&mut m, &["DROP TABLE fbz_tipos", "DROP TABLE fbz_clientes"]).await;
    exec_quiet(&mut f, &["DROP TABLE FBZ_TIPOS", "DROP TABLE FBZ_CLIENTES"]).await;
}
