//! End-to-end conversions between the families, with tables spelled the
//! way each driver's `database_schema` reports them.

use dbine_driver::{ColumnDef, ForeignKeyDef, IndexDef, KeyDef, TableSchema};
use dbine_schema::{convert, IssueCode, Options, Severity};

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

fn ty<'a>(t: &'a TableSchema, c: &str) -> &'a str {
    &t.columns.iter().find(|x| x.name == c).unwrap_or_else(|| panic!("no column {c} in {t:?}")).data_type
}

fn column<'a>(t: &'a TableSchema, c: &str) -> &'a ColumnDef {
    t.columns.iter().find(|x| x.name == c).unwrap()
}

/// A PostgreSQL table as `database_schema` reports it.
fn pg_pedidos() -> Vec<TableSchema> {
    let clientes = table("clientes", vec![not_null(col("id", "integer")), col("nombre", "character varying(120)")], &["id"]);
    let mut pedidos = table(
        "pedidos",
        vec![
            not_null(with_default(col("id", "bigint"), "nextval('pedidos_id_seq'::regclass)")),
            not_null(col("cliente_id", "integer")),
            with_default(col("total", "numeric(12,2)"), "0"),
            with_default(col("creado", "timestamp with time zone"), "now()"),
            with_default(col("activo", "boolean"), "true"),
            with_default(col("codigo", "uuid"), "gen_random_uuid()"),
            col("datos", "jsonb"),
            col("etiquetas", "text[]"),
            col("notas", "text"),
            col("ip", "inet"),
            col("estado", "character varying(20)"),
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
    pedidos.indexes.push(IndexDef { name: "ix_pedidos_creado".into(), columns: vec!["creado".into()], kind: Some("btree".into()), ..Default::default() });
    pedidos.indexes.push(IndexDef {
        name: "ux_pedidos_codigo_activo".into(),
        columns: vec!["codigo".into()],
        unique: true,
        filter: Some("activo".into()),
        ..Default::default()
    });
    pedidos.indexes.push(IndexDef { name: "ix_pedidos_datos".into(), columns: vec!["datos".into()], kind: Some("gin".into()), ..Default::default() });
    vec![clientes, pedidos]
}

#[test]
fn postgres_to_mysql() {
    let r = convert(&pg_pedidos(), "postgres", "mysql", &Options::default()).unwrap();
    let p = &r.tables[1];
    assert_eq!(ty(p, "id"), "bigint");
    assert!(column(p, "id").auto_increment, "nextval default becomes auto-increment");
    assert_eq!(column(p, "id").default_value, None);
    assert_eq!(ty(p, "total"), "decimal(12, 2)");
    // timestamptz keeps microseconds; MySQL's default precision is 0.
    assert_eq!(ty(p, "creado"), "datetime(6)");
    assert_eq!(column(p, "creado").default_value.as_deref(), Some("CURRENT_TIMESTAMP(6)"));
    assert_eq!(ty(p, "activo"), "tinyint(1)");
    assert_eq!(column(p, "activo").default_value.as_deref(), Some("1"));
    assert_eq!(ty(p, "codigo"), "char(36)");
    assert_eq!(column(p, "codigo").default_value.as_deref(), Some("(UUID())"));
    assert_eq!(ty(p, "datos"), "json");
    assert_eq!(ty(p, "etiquetas"), "json");
    assert_eq!(ty(p, "notas"), "longtext");
    assert_eq!(ty(p, "estado"), "varchar(20)");
    // Time zone lost, arrays approximated.
    assert!(r.issues.iter().any(|i| i.code == IssueCode::TimeZoneLoss && i.object.as_deref() == Some("creado")));
    assert!(r.issues.iter().any(|i| i.code == IssueCode::TypeApproximated && i.object.as_deref() == Some("etiquetas")));
    // FK kept, ON UPDATE RESTRICT accepted by MySQL.
    assert_eq!(p.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    assert_eq!(p.foreign_keys[0].on_update.as_deref(), Some("RESTRICT"));
    // Unique filtered index dropped (it would get stricter), gin kind reset.
    assert!(!p.indexes.iter().any(|i| i.name == "ux_pedidos_codigo_activo"));
    assert!(r.issues.iter().any(|i| i.code == IssueCode::IndexDropped && i.severity == Severity::Dropped));
    // MySQL can't index a JSON column: the gin index is left out, and said so.
    assert!(!p.indexes.iter().any(|i| i.name == "ix_pedidos_datos"));
    assert!(r.issues.iter().any(|i| i.code == IssueCode::IndexDropped && i.object.as_deref() == Some("ix_pedidos_datos")));
}

#[test]
fn postgres_to_sqlserver() {
    let r = convert(&pg_pedidos(), "postgres", "sqlserver", &Options::default()).unwrap();
    let p = &r.tables[1];
    assert_eq!(ty(p, "creado"), "datetimeoffset");
    assert_eq!(column(p, "creado").default_value.as_deref(), Some("SYSDATETIMEOFFSET()"));
    assert_eq!(ty(p, "activo"), "bit");
    assert_eq!(ty(p, "codigo"), "uniqueidentifier");
    assert_eq!(column(p, "codigo").default_value.as_deref(), Some("NEWID()"));
    assert_eq!(ty(p, "estado"), "nvarchar(20)");
    assert_eq!(ty(p, "notas"), "nvarchar(max)");
    // SQL Server has no RESTRICT: NO ACTION, reported as info.
    assert_eq!(p.foreign_keys[0].on_update.as_deref(), Some("NO ACTION"));
    assert!(r.issues.iter().any(|i| i.code == IssueCode::ForeignKeyActionChanged && i.severity == Severity::Info));
}

#[test]
fn postgres_to_oracle_folds_names_to_upper() {
    let r = convert(&pg_pedidos(), "postgres", "oracle", &Options::default()).unwrap();
    let p = &r.tables[1];
    assert_eq!(p.name, "PEDIDOS");
    assert_eq!(ty(p, "ID"), "NUMBER(19)");
    assert_eq!(ty(p, "TOTAL"), "NUMBER(12, 2)");
    assert_eq!(ty(p, "CREADO"), "TIMESTAMP WITH TIME ZONE");
    assert_eq!(column(p, "CREADO").default_value.as_deref(), Some("SYSTIMESTAMP"));
    assert_eq!(ty(p, "ACTIVO"), "NUMBER(1)");
    assert_eq!(ty(p, "NOTAS"), "NCLOB");
    assert_eq!(ty(p, "CODIGO"), "RAW(16)");
    assert_eq!(column(p, "CODIGO").default_value.as_deref(), Some("SYS_GUID()"));
    // FK follows the renamed table and columns; Oracle has no ON UPDATE.
    let fk = &p.foreign_keys[0];
    assert_eq!((fk.ref_table.as_str(), fk.columns[0].as_str(), fk.ref_columns[0].as_str()), ("CLIENTES", "CLIENTE_ID", "ID"));
    assert_eq!(fk.on_update, None);
    assert!(r.issues.iter().any(|i| i.code == IssueCode::ForeignKeyActionChanged && i.severity == Severity::Warning));
    // Case folding alone isn't reported as a rename.
    assert!(!r.issues.iter().any(|i| i.code == IssueCode::IdentifierRenamed));
}

#[test]
fn oracle_to_postgres() {
    let t = table(
        "EMPLEADOS",
        vec![
            not_null(col("ID", "NUMBER(10)")),
            col("NOMBRE", "VARCHAR2(100 CHAR)"),
            col("SUELDO", "NUMBER(12,2)"),
            col("ALTA", "DATE"),
            with_default(col("CREADO", "TIMESTAMP(6)"), "SYSTIMESTAMP"),
            col("FOTO", "BLOB"),
            col("CV", "CLOB"),
            col("RATIO", "NUMBER"),
            col("FLAG", "NUMBER(1)"),
            col("MiColumna", "VARCHAR2(10)"),
        ],
        &["ID"],
    );
    let r = convert(&[t], "oracle", "postgres", &Options::default()).unwrap();
    let e = &r.tables[0];
    assert_eq!(e.name, "empleados");
    assert_eq!(ty(e, "id"), "bigint");
    assert_eq!(ty(e, "nombre"), "varchar(100)");
    assert_eq!(ty(e, "sueldo"), "numeric(12, 2)");
    // Oracle DATE has a time of day: timestamp(0), not date.
    assert_eq!(ty(e, "alta"), "timestamp(0)");
    assert_eq!(ty(e, "creado"), "timestamp(6)");
    assert_eq!(column(e, "creado").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(ty(e, "foto"), "bytea");
    assert_eq!(ty(e, "cv"), "text");
    assert_eq!(ty(e, "ratio"), "numeric");
    assert_eq!(ty(e, "flag"), "smallint");
    // A mixed-case name was quoted on purpose: kept.
    assert!(e.columns.iter().any(|c| c.name == "MiColumna"));
}

#[test]
fn sqlserver_to_postgres() {
    let t = table(
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
            col("Bytes", "varbinary(max)"),
            col("Chico", "tinyint"),
            col("Version", "rowversion"),
            with_default(col("Alta", "datetime2"), "(getdate())"),
            with_default(col("Activo", "bit"), "((1))"),
        ],
        &["Id"],
    );
    let r = convert(&[t], "sqlserver", "postgres", &Options::default()).unwrap();
    let v = &r.tables[0];
    assert!(column(v, "Id").auto_increment);
    assert_eq!(ty(v, "Codigo"), "uuid");
    assert_eq!(ty(v, "Monto"), "numeric(19, 4)");
    assert_eq!(ty(v, "Fecha"), "timestamp(3)");
    // datetime2(7) has one digit more than PostgreSQL keeps.
    assert_eq!(ty(v, "Exacta"), "timestamp(6)");
    assert!(r.issues.iter().any(|i| i.code == IssueCode::PrecisionLoss && i.object.as_deref() == Some("Exacta")));
    assert_eq!(ty(v, "Zona"), "timestamp(6) with time zone");
    assert_eq!(ty(v, "Nombre"), "varchar(50)");
    assert_eq!(ty(v, "Texto"), "text");
    assert_eq!(ty(v, "Bytes"), "bytea");
    // TINYINT is 0–255: smallint.
    assert_eq!(ty(v, "Chico"), "smallint");
    assert_eq!(column(v, "Alta").default_value.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(column(v, "Activo").default_value.as_deref(), Some("TRUE"));
    assert!(r.issues.iter().any(|i| i.object.as_deref() == Some("Version") && i.code == IssueCode::TypeApproximated));
}

#[test]
fn mysql_to_postgres() {
    let t = table(
        "productos",
        vec![
            not_null(auto(col("id", "int unsigned"))),
            col("activo", "tinyint(1)"),
            col("stock", "mediumint"),
            col("precio", "decimal(10,2)"),
            col("tipo", "enum('a','b','c')"),
            col("desc", "mediumtext"),
            col("alta", "datetime(3)"),
            col("ts", "timestamp"),
            col("anio", "year"),
            col("grande", "bigint unsigned"),
        ],
        &["id"],
    );
    let r = convert(&[t], "mysql", "postgres", &Options::default()).unwrap();
    let p = &r.tables[0];
    // int unsigned needs a wider signed type.
    assert_eq!(ty(p, "id"), "bigint");
    assert_eq!(ty(p, "activo"), "boolean");
    assert_eq!(ty(p, "stock"), "integer");
    assert_eq!(ty(p, "precio"), "numeric(10, 2)");
    assert_eq!(ty(p, "tipo"), "varchar(1)");
    assert!(r.issues.iter().any(|i| i.object.as_deref() == Some("tipo") && i.message.contains("a, b, c")));
    assert_eq!(ty(p, "desc"), "text");
    assert_eq!(ty(p, "alta"), "timestamp(3)");
    assert_eq!(ty(p, "ts"), "timestamp with time zone");
    assert_eq!(ty(p, "grande"), "numeric(39, 0)");
}

#[test]
fn to_sqlite_autoincrement_rules() {
    let t = table("t", vec![not_null(auto(col("id", "bigint"))), auto(col("otro", "integer"))], &["id"]);
    let r = convert(&[t], "postgres", "sqlite", &Options::default()).unwrap();
    assert!(column(&r.tables[0], "id").auto_increment);
    assert!(!column(&r.tables[0], "otro").auto_increment);
    assert!(r.issues.iter().any(|i| i.code == IssueCode::AutoIncrementDropped && i.object.as_deref() == Some("otro")));
}

#[test]
fn identifiers_are_shortened_uniquely() {
    let long = "columna_con_un_nombre_larguisimo_que_no_entra_en_postgres_de_ningun_modo";
    let t = table("t", vec![col(&format!("{long}_1"), "int"), col(&format!("{long}_2"), "int")], &[]);
    let r = convert(&[t], "sqlserver", "postgres", &Options::default()).unwrap();
    let names: Vec<&str> = r.tables[0].columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.iter().all(|n| n.len() <= 63));
    assert_ne!(names[0], names[1]);
    assert_eq!(r.issues.iter().filter(|i| i.code == IssueCode::IdentifierRenamed).count(), 2);
}

#[test]
fn same_family_keeps_native_spelling() {
    let r = convert(&pg_pedidos(), "postgres", "cockroachdb", &Options::default()).unwrap();
    assert_eq!(ty(&r.tables[1], "etiquetas"), "text[]");
    assert_eq!(column(&r.tables[1], "creado").default_value.as_deref(), Some("now()"));
    assert!(r.tables[1].indexes.iter().any(|i| i.kind.as_deref() == Some("gin")));
}

#[test]
fn target_without_foreign_keys() {
    let caps = dbine_schema::dialect::for_driver("postgres").unwrap().caps();
    let opts = Options { target_caps: Some(dbine_schema::dialect::Caps { foreign_keys: false, ..caps }), ..Default::default() };
    let r = convert(&pg_pedidos(), "postgres", "postgres", &opts).unwrap();
    assert!(r.tables[1].foreign_keys.is_empty());
    assert!(r.issues.iter().any(|i| i.code == IssueCode::ForeignKeyDropped));
}

#[test]
fn unknown_engine_and_type() {
    assert!(convert(&[], "nope", "postgres", &Options::default()).is_err());
    let t = table("t", vec![col("x", "tsvector")], &[]);
    let r = convert(&[t], "postgres", "mysql", &Options::default()).unwrap();
    assert_eq!(ty(&r.tables[0], "x"), "tsvector");
    assert!(r.issues.iter().any(|i| i.code == IssueCode::TypeUnknown));
}

#[test]
fn mapping_follows_columns_the_target_adds_or_renames() {
    // GreptimeDB needs a time index: without a timestamp it adds one.
    let t = table("t", vec![not_null(col("id", "int")), col("n", "nvarchar(10)")], &["id"]);
    let r = convert(&[t], "sqlserver", "greptimedb", &Options::default()).unwrap();
    let added: Vec<_> = r.columns.iter().filter(|m| m.column.is_empty()).collect();
    assert_eq!(added.len(), 1, "{:?}", r.columns);
    assert!(r.tables[0].columns.iter().any(|c| c.name == added[0].target_column));
    assert!(r.columns.iter().any(|m| m.column == "id" && m.target_column == "id"));
    // No mark leaks into the target's options.
    assert!(r.tables[0].columns.iter().all(|c| c.options.keys().all(|k| !k.starts_with("__dbine"))));

    // IoTDB's first column is `Time`: an existing timestamp is renamed to it.
    let t = table("m", vec![not_null(col("ts", "timestamp")), col("v", "double precision")], &["ts"]);
    let r = convert(&[t], "postgres", "iotdb", &Options::default()).unwrap();
    let ts = r.columns.iter().find(|m| m.column == "ts").unwrap();
    assert_eq!(r.tables[0].columns[0].name, ts.target_column);
}

#[test]
fn source_only_engines_refuse_as_target() {
    let t = table("t", vec![col("id", "int")], &["id"]);
    for id in ["drill", "avatica", "influxdb", "influxdb1", "influxdb3", "netsuite", "duckdb_files"] {
        let e = convert(&[t.clone()], "postgres", id, &Options::default()).unwrap_err();
        assert!(matches!(e, dbine_schema::Error::SourceOnly { .. }), "{id}: {e}");
        assert!(e.to_string().contains("origen"), "{e}");
    }
    // …but they still work as a source.
    assert!(convert(&[t], "drill", "postgres", &Options::default()).is_ok());
}

#[test]
fn target_schema_places_tables_and_references() {
    let mut src = pg_pedidos();
    for t in &mut src {
        t.schema = Some("public".into());
    }
    src[1].foreign_keys[0].ref_schema = Some("public".into());
    // Across engines the source schema means nothing in the target…
    let r = convert(&src, "postgres", "sqlserver", &Options::default()).unwrap();
    assert!(r.tables.iter().all(|t| t.schema.is_none()));
    assert_eq!(r.tables[1].foreign_keys[0].ref_schema, None);
    // …unless the user picks one; references between the tables follow.
    let opts = Options { target_schema: Some("ventas".into()), ..Default::default() };
    let r = convert(&src, "postgres", "sqlserver", &opts).unwrap();
    assert!(r.tables.iter().all(|t| t.schema.as_deref() == Some("ventas")));
    assert_eq!(r.tables[1].foreign_keys[0].ref_schema.as_deref(), Some("ventas"));
    // Within a family the source schema stays.
    let r = convert(&src, "postgres", "cockroachdb", &Options::default()).unwrap();
    assert!(r.tables.iter().all(|t| t.schema.as_deref() == Some("public")));
}

/// The same table name in two schemas (a DATABASECHANGELOG table in `dbo` and
/// in `Agent`): each gets its own target table, never the same one.
#[test]
fn same_name_in_two_schemas() {
    let mut a = table("DATABASECHANGELOG", vec![not_null(col("ID", "nvarchar(255)"))], &["ID"]);
    a.schema = Some("dbo".into());
    let mut b = table("DATABASECHANGELOG", vec![not_null(col("ID", "nvarchar(255)")), col("EXTRA", "int")], &["ID"]);
    b.schema = Some("Agent".into());
    let mut child = table("Executions", vec![not_null(col("Id", "int")), col("LogId", "nvarchar(255)")], &["Id"]);
    child.schema = Some("Agent".into());
    child.foreign_keys.push(dbine_driver::ForeignKeyDef {
        name: Some("FK_log".into()),
        columns: vec!["LogId".into()],
        ref_schema: Some("Agent".into()),
        ref_table: "DATABASECHANGELOG".into(),
        ref_columns: vec!["ID".into()],
        ..Default::default()
    });
    let tables = [a, b, child];

    // Into one schema: two distinct names, columns not mixed up.
    let c = convert(&tables, "sqlserver", "postgres", &Options::default()).unwrap();
    assert_ne!(c.tables[0].name, c.tables[1].name, "{:?}", c.tables.iter().map(|t| &t.name).collect::<Vec<_>>());
    assert_eq!(c.tables[0].columns.len(), 1);
    assert_eq!(c.tables[1].columns.len(), 2);
    assert_eq!(c.tables[2].foreign_keys[0].ref_table, c.tables[1].name, "the FK points at Agent's table");

    // Keeping schemas: same names, each in its own schema; the FK says which.
    let c = convert(&tables, "sqlserver", "postgres", &Options { keep_schemas: true, ..Options::default() }).unwrap();
    let names: Vec<(Option<&str>, &str)> = c.tables.iter().map(|t| (t.schema.as_deref(), t.name.as_str())).collect();
    // (Mixed-case names are kept as they are; all-caps ones fold.)
    assert_eq!(names, [(Some("dbo"), "databasechangelog"), (Some("Agent"), "databasechangelog"), (Some("Agent"), "Executions")]);
    let fk = &c.tables[2].foreign_keys[0];
    assert_eq!((fk.ref_schema.as_deref(), fk.ref_table.as_str()), (Some("Agent"), "databasechangelog"));
}

/// PostgreSQL takes at most 32 columns in an index: a wider one (SQL Server
/// key + INCLUDE columns) is left out and reported, the table still converts.
#[test]
fn too_wide_index_is_dropped() {
    let cols: Vec<ColumnDef> = (0..40).map(|i| col(&format!("c{i}"), "int")).collect();
    let mut t = table("Audit", cols, &[]);
    t.indexes.push(dbine_driver::IndexDef { name: "ix_wide".into(), columns: (0..40).map(|i| format!("c{i}")).collect(), ..Default::default() });
    t.indexes.push(dbine_driver::IndexDef { name: "ix_ok".into(), columns: vec!["c1".into()], ..Default::default() });
    let c = convert(&[t], "sqlserver", "postgres", &Options::default()).unwrap();
    assert_eq!(c.tables[0].indexes.len(), 1);
    assert!(c.issues.iter().any(|i| i.object.as_deref() == Some("ix_wide") && i.message.contains("32")), "{:?}", c.issues);
}
