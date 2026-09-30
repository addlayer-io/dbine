//! Conversions to and from the non-relational engines: document stores
//! (MongoDB family, Cosmos DB, CouchDB, Couchbase), wide-column (CQL),
//! search (Elasticsearch family, Solr), key-value with keys (DynamoDB) and
//! graphs (Neo4j family, OrientDB).
//!
//! The first half works offline, with tables spelled the way each driver's
//! `database_schema` reports them. The second half (`#[ignore]`) runs
//! against real servers, reading `DBINE_TEST_<ENGINE>_URL` (a test whose
//! servers aren't set skips):
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011            # uses database nsq_schema
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin
//! DBINE_TEST_FERRETDB_URL=mongodb://root:secret@localhost:25203/
//! DBINE_TEST_CASSANDRA_URL=localhost:25402  DBINE_TEST_SCYLLADB_URL=localhost:<port>
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520  DBINE_TEST_OPENSEARCH_URL=http://localhost:25521
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893  DBINE_TEST_COUCHBASE_MGMT_PORT=25891  (bucket `nsq`)
//! DBINE_TEST_DYNAMODB_URL=http://localhost:25300
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687
//! cargo test -p dbine-schema --test nosql -- --ignored --test-threads=1
//! ```

mod common;

use dbine_driver::{ColumnDef, ConnectionConfig, DdlParts, ForeignKeyDef, IndexDef, KeyDef, ObjectRef, Session, TableSchema};
use dbine_schema::{convert, Conversion, IssueCode, Options, Severity};
use serde_json::{json, Value};

// ---- offline -----------------------------------------------------------------

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
        schema: Some("public".into()),
        name: name.into(),
        columns: cols,
        primary_key: (!pk.is_empty()).then(|| KeyDef { name: None, columns: pk.iter().map(|s| s.to_string()).collect() }),
        ..Default::default()
    }
}

fn column<'a>(t: &'a TableSchema, c: &str) -> &'a ColumnDef {
    t.columns.iter().find(|x| x.name == c).unwrap_or_else(|| panic!("no column {c} in {}", t.name))
}

fn ty<'a>(t: &'a TableSchema, c: &str) -> &'a str {
    &column(t, c).data_type
}

fn opt<'a>(c: &'a ColumnDef, k: &str) -> Option<&'a str> {
    c.options.get(k).map(String::as_str)
}

fn has(r: &Conversion, code: IssueCode, object: Option<&str>) -> bool {
    r.issues.iter().any(|i| i.code == code && (object.is_none() || i.object.as_deref() == object))
}

/// The DDL the target driver writes (panics if it refuses the tables).
fn ddl(driver: &str, tables: &[TableSchema]) -> String {
    common::target_ddl(driver, tables).join("\n")
}

/// Orders with a composite key, as PostgreSQL reports them.
fn pg_orders() -> Vec<TableSchema> {
    let clientes = table("clientes", vec![not_null(col("id", "integer")), not_null(col("nombre", "character varying(80)"))], &["id"]);
    let mut pedidos = table(
        "pedidos",
        vec![
            not_null(col("id", "bigint")),
            not_null(col("linea", "smallint")),
            not_null(col("cliente_id", "integer")),
            not_null(with_default(col("total", "numeric(12,2)"), "0")),
            col("ratio", "double precision"),
            col("codigo", "uuid"),
            col("estado", "character varying(20)"),
            col("notas", "text"),
            with_default(col("activo", "boolean"), "true"),
            with_default(col("creado", "timestamp with time zone"), "now()"),
            col("local_ts", "timestamp(6) without time zone"),
            col("dia", "date"),
            col("datos", "jsonb"),
            col("etiquetas", "text[]"),
            col("bytes", "bytea"),
        ],
        &["id", "linea"],
    );
    pedidos.foreign_keys.push(ForeignKeyDef {
        name: Some("fk_pedidos_clientes".into()),
        columns: vec!["cliente_id".into()],
        ref_table: "clientes".into(),
        ref_columns: vec!["id".into()],
        on_delete: Some("CASCADE".into()),
        ..Default::default()
    });
    pedidos.indexes.push(IndexDef { name: "ux_pedidos_codigo".into(), columns: vec!["codigo".into()], unique: true, ..Default::default() });
    pedidos.indexes.push(IndexDef { name: "ix_pedidos_estado".into(), columns: vec!["estado".into()], ..Default::default() });
    pedidos.indexes.push(IndexDef { name: "ix_pedidos_cli_dia".into(), columns: vec!["cliente_id".into(), "dia".into()], ..Default::default() });
    pedidos.indexes.push(IndexDef {
        name: "ux_pedidos_activos".into(),
        columns: vec!["estado".into()],
        unique: true,
        filter: Some("activo".into()),
        ..Default::default()
    });
    vec![clientes, pedidos]
}

/// A collection as the MongoDB driver reports it (sampled fields).
fn mongo_people() -> TableSchema {
    let mut t = TableSchema {
        kind: "collection".into(),
        name: "personas".into(),
        columns: vec![
            not_null(col("_id", "objectId")),
            not_null(col("nombre", "string")),
            col("edad", "int|long"),
            col("saldo", "decimal"),
            col("alta", "date"),
            col("tags", "array"),
            col("direccion", "object"),
            col("mixto", "string|int"),
            col("foto", "binData"),
            col("activo", "bool"),
            col("vacio", "null"),
        ],
        primary_key: Some(KeyDef { name: None, columns: vec!["_id".into()] }),
        ..Default::default()
    };
    t.indexes.push(IndexDef { name: "nombre_1".into(), columns: vec!["nombre".into()], unique: true, ..Default::default() });
    t.indexes.push(IndexDef { name: "alta_-1".into(), columns: vec!["alta:-1".into()], ..Default::default() });
    t.options.insert("validate_fields".into(), "false".into());
    t
}

#[test]
fn postgres_to_mongodb() {
    let r = convert(&pg_orders(), "postgres", "mongodb", &Options::default()).unwrap();
    let p = &r.tables[1];
    assert_eq!(p.kind, "collection");
    assert_eq!(ty(p, "id"), "long");
    assert_eq!(ty(p, "linea"), "int");
    assert_eq!(ty(p, "total"), "decimal");
    assert_eq!(ty(p, "ratio"), "double");
    assert_eq!(ty(p, "codigo"), "string");
    assert_eq!(ty(p, "activo"), "bool");
    assert_eq!(ty(p, "creado"), "date");
    assert_eq!(ty(p, "dia"), "date");
    assert_eq!(ty(p, "datos"), "object|array");
    assert_eq!(ty(p, "etiquetas"), "array");
    assert_eq!(ty(p, "bytes"), "binData");
    // NOT NULL → required; nullable fields keep null in the validator.
    assert_eq!(opt(column(p, "total"), "required"), Some("true"));
    assert_eq!(opt(column(p, "notas"), "required"), None);
    // No defaults, no foreign keys: reported.
    assert!(column(p, "creado").default_value.is_none());
    assert!(has(&r, IssueCode::DefaultDropped, Some("creado")));
    assert!(p.foreign_keys.is_empty() && has(&r, IssueCode::ForeignKeyDropped, None));
    // The key is a unique index, MongoDB adds `_id`.
    assert!(p.primary_key.is_none());
    let pk = p.indexes.iter().find(|i| i.name == "pedidos_pk").unwrap();
    assert!(pk.unique && pk.columns == ["id", "linea"]);
    assert!(has(&r, IssueCode::PrimaryKeyDropped, Some("pedidos_pk")));
    // A filtered unique index would be stricter without its filter.
    assert!(!p.indexes.iter().any(|i| i.name == "ux_pedidos_activos"));
    // Microseconds and zone-less timestamps are reported.
    assert!(has(&r, IssueCode::PrecisionLoss, Some("local_ts")));
    assert!(has(&r, IssueCode::TimeZoneLoss, Some("local_ts")));
    let script = ddl("mongodb", &r.tables);
    assert!(script.contains("\"required\":[\"id\",\"linea\",\"cliente_id\",\"total\"]"), "{script}");
    assert!(script.contains("\"bsonType\":[\"string\",\"null\"]"), "{script}");
    assert!(script.contains("createIndex({\"id\":1,\"linea\":1}, {\"name\":\"pedidos_pk\",\"unique\":true})"), "{script}");
    // FerretDB: no validators.
    let f = convert(&pg_orders(), "postgres", "ferretdb", &Options::default()).unwrap();
    assert_eq!(f.tables[1].options.get("validate_fields").map(String::as_str), Some("false"));
    assert!(!ddl("ferretdb", &f.tables).contains("$jsonSchema"));
}

#[test]
fn mongodb_to_sql() {
    let src = [mongo_people()];
    let pg = convert(&src, "mongodb", "postgres", &Options::default()).unwrap();
    let p = &pg.tables[0];
    assert_eq!(ty(p, "_id"), "char(24)");
    assert_eq!(p.primary_key.as_ref().unwrap().columns, ["_id"]);
    assert_eq!(ty(p, "nombre"), "text");
    assert!(!column(p, "nombre").nullable);
    assert_eq!(ty(p, "edad"), "bigint");
    assert_eq!(ty(p, "saldo"), "numeric");
    assert_eq!(ty(p, "alta"), "timestamp(3) with time zone");
    assert_eq!(ty(p, "tags"), "jsonb");
    assert_eq!(ty(p, "direccion"), "jsonb");
    assert_eq!(ty(p, "mixto"), "jsonb");
    assert_eq!(ty(p, "foto"), "bytea");
    assert_eq!(ty(p, "activo"), "boolean");
    assert_eq!(ty(p, "vacio"), "text");
    // Index keys with a direction lose it (`alta:-1` is a Mongo spelling).
    assert!(p.indexes.iter().any(|i| i.name == "nombre_1" && i.unique));
    // Mongo's own options don't travel.
    assert!(has(&pg, IssueCode::OptionDropped, Some("validate_fields")));

    let my = convert(&src, "mongodb", "mysql", &Options::default()).unwrap();
    let m = &my.tables[0];
    assert_eq!(ty(m, "_id"), "char(24)");
    assert_eq!(ty(m, "alta"), "datetime(3)");
    assert_eq!(ty(m, "direccion"), "json");
    assert_eq!(ty(m, "saldo"), "decimal(65, 30)");
    assert_eq!(ty(m, "foto"), "longblob");

    let ms = convert(&src, "mongodb", "sqlserver", &Options::default()).unwrap();
    let s = &ms.tables[0];
    assert_eq!(ty(s, "_id"), "char(24)");
    assert_eq!(ty(s, "alta"), "datetimeoffset(3)");

    // And back: `_id` stays the key (and an ObjectId again).
    let back = convert(&pg.tables, "postgres", "mongodb", &Options::default()).unwrap();
    let b = &back.tables[0];
    assert_eq!(b.primary_key.as_ref().unwrap().columns, ["_id"]);
    assert_eq!(ty(b, "_id"), "objectId|string");
    assert_eq!(ty(b, "edad"), "long");
    assert_eq!(ty(b, "alta"), "date");
    assert!(!b.indexes.iter().any(|i| i.name.ends_with("_pk")));
}

#[test]
fn postgres_to_cassandra() {
    let r = convert(&pg_orders(), "postgres", "cassandra", &Options::default()).unwrap();
    let p = &r.tables[1];
    // First key column partitions, the rest cluster (ascending).
    assert_eq!(opt(column(p, "id"), "partition_key"), Some("true"));
    assert_eq!(opt(column(p, "linea"), "clustering_key"), Some("true"));
    assert_eq!(opt(column(p, "linea"), "clustering_order"), Some("ASC"));
    assert_eq!(opt(column(r.tables.first().unwrap(), "id"), "partition_key"), Some("true"));
    assert!(has(&r, IssueCode::OptionAdded, Some("partition_key")));
    assert_eq!(ty(p, "id"), "bigint");
    assert_eq!(ty(p, "linea"), "smallint");
    assert_eq!(ty(p, "total"), "decimal");
    assert_eq!(ty(p, "codigo"), "uuid");
    assert_eq!(ty(p, "estado"), "text");
    assert_eq!(ty(p, "creado"), "timestamp");
    assert_eq!(ty(p, "datos"), "text");
    assert_eq!(ty(p, "etiquetas"), "list<text>");
    assert_eq!(ty(p, "bytes"), "blob");
    // No NOT NULL, no FK, no defaults; unique and multi-column indexes out.
    assert!(column(p, "total").nullable && has(&r, IssueCode::NullabilityChanged, Some("total")));
    assert!(p.foreign_keys.is_empty());
    assert!(column(p, "activo").default_value.is_none());
    let names: Vec<&str> = p.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["ix_pedidos_estado"]);
    assert!(r.issues.iter().any(|i| i.object.as_deref() == Some("ux_pedidos_codigo") && i.severity == Severity::Dropped));
    assert!(r.issues.iter().any(|i| i.object.as_deref() == Some("ix_pedidos_cli_dia") && i.code == IssueCode::IndexDropped));
    let mut tables = r.tables.clone();
    tables.iter_mut().for_each(|t| t.schema = None);
    let script = ddl("cassandra", &tables);
    assert!(script.contains("PRIMARY KEY (\"id\", \"linea\")") || script.contains("PRIMARY KEY (id, linea)"), "{script}");
    // Keyspaces: no secondary indexes.
    let k = convert(&pg_orders(), "postgres", "keyspaces", &Options::default()).unwrap();
    assert!(k.tables[1].indexes.is_empty());
}

#[test]
fn cassandra_to_sql_without_primary_key_source() {
    let mut t = table(
        "eventos",
        vec![
            not_null(col("sensor", "text")),
            not_null(col("ts", "timestamp")),
            col("valor", "double"),
            col("tags", "set<text>"),
            col("attrs", "map<text, int>"),
            col("pos", "frozen<tuple<double, double>>"),
            col("dur", "duration"),
            col("uid", "timeuuid"),
            col("big", "varint"),
            col("ip", "inet"),
            col("t", "time"),
            col("dir", "direccion"),
        ],
        &["sensor", "ts"],
    );
    t.schema = Some("telemetria".into());
    t.columns[0].options.insert("partition_key".into(), "true".into());
    t.columns[1].options.insert("clustering_key".into(), "true".into());
    let pg = convert(std::slice::from_ref(&t), "cassandra", "postgres", &Options::default()).unwrap();
    let p = &pg.tables[0];
    assert_eq!(ty(p, "ts"), "timestamp(3) with time zone");
    assert_eq!(ty(p, "tags"), "text[]");
    assert_eq!(ty(p, "attrs"), "jsonb");
    assert_eq!(ty(p, "pos"), "jsonb");
    assert_eq!(ty(p, "dur"), "interval");
    assert_eq!(ty(p, "uid"), "uuid");
    assert_eq!(ty(p, "big"), "numeric");
    assert_eq!(ty(p, "ip"), "inet");
    assert_eq!(ty(p, "t"), "time(6)");
    assert!(has(&pg, IssueCode::PrecisionLoss, Some("t")));
    assert_eq!(ty(p, "dir"), "jsonb");
    assert_eq!(p.primary_key.as_ref().unwrap().columns, ["sensor", "ts"]);
    let my = convert(&[t], "cassandra", "mysql", &Options::default()).unwrap();
    // A text key column: MySQL needs a bounded type to index it.
    assert!(matches!(ty(&my.tables[0], "sensor"), "longtext") || ty(&my.tables[0], "sensor").starts_with("varchar"));
    assert_eq!(ty(&my.tables[0], "tags"), "json");

    // SQL without a primary key: the first unique index is the key.
    let mut nokey = table("t", vec![col("a", "integer"), col("b", "text"), col("c", "integer[]")], &[]);
    nokey.indexes.push(IndexDef { name: "ux".into(), columns: vec!["b".into()], unique: true, ..Default::default() });
    let r = convert(&[nokey.clone()], "postgres", "scylladb", &Options::default()).unwrap();
    assert_eq!(opt(column(&r.tables[0], "b"), "partition_key"), Some("true"));
    nokey.indexes.clear();
    let r = convert(&[nokey], "postgres", "cassandra", &Options::default()).unwrap();
    assert_eq!(r.tables[0].primary_key.as_ref().unwrap().columns, ["a", "b"]);
    assert!(has(&r, IssueCode::PrimaryKeyAdded, None));
}

#[test]
fn postgres_to_elasticsearch() {
    let r = convert(&pg_orders(), "postgres", "elasticsearch", &Options::default()).unwrap();
    let p = &r.tables[1];
    assert_eq!(p.kind, "index");
    assert!(p.primary_key.is_none() && has(&r, IssueCode::PrimaryKeyDropped, None));
    assert_eq!(ty(p, "id"), "long");
    assert_eq!(ty(p, "linea"), "short");
    assert_eq!(ty(p, "total"), "scaled_float");
    assert_eq!(opt(column(p, "total"), "scaling_factor"), Some("100"));
    assert_eq!(ty(p, "estado"), "keyword");
    assert_eq!(ty(p, "notas"), "text");
    assert_eq!(ty(p, "codigo"), "keyword");
    assert_eq!(ty(p, "creado"), "date");
    assert_eq!(ty(p, "local_ts"), "date_nanos");
    assert!(opt(column(p, "creado"), "format").unwrap().starts_with("strict_date_optional_time||"));
    assert_eq!(ty(p, "datos"), "flattened");
    assert_eq!(ty(p, "etiquetas"), "text");
    assert_eq!(ty(p, "bytes"), "binary");
    assert!(p.indexes.is_empty());
    let script = ddl("elasticsearch", &r.tables);
    assert!(script.contains("\"scaling_factor\": 100"), "{script}");
    let os = convert(&pg_orders(), "postgres", "opensearch", &Options::default()).unwrap();
    assert_eq!(ty(&os.tables[1], "datos"), "flat_object");

    // Mixed-case names become valid index names.
    let t = table("Ventas Año", vec![col("Id", "int")], &["Id"]);
    let r = convert(&[t], "sqlserver", "elasticsearch", &Options::default()).unwrap();
    assert_eq!(r.tables[0].name, "ventas_año");
    ddl("elasticsearch", &r.tables);

    // Back to SQL.
    let back = convert(&os.tables, "opensearch", "postgres", &Options::default()).unwrap();
    let b = &back.tables[1];
    assert_eq!(ty(b, "id"), "bigint");
    assert_eq!(ty(b, "total"), "numeric");
    assert_eq!(ty(b, "estado"), "text");
    assert_eq!(ty(b, "creado"), "timestamp(3) with time zone");
    assert_eq!(ty(b, "datos"), "jsonb");
}

#[test]
fn sqlserver_to_dynamodb() {
    let mut t = table(
        "Ventas",
        vec![
            not_null(col("Tienda", "int")),
            not_null(col("Fecha", "datetime2(7)")),
            not_null(col("Nro", "int")),
            col("Monto", "decimal(18,2)"),
            col("Cliente", "nvarchar(50)"),
            col("Activo", "bit"),
            col("Datos", "nvarchar(max)"),
        ],
        &["Tienda", "Fecha", "Nro"],
    );
    t.indexes.push(IndexDef { name: "IX_Cliente".into(), columns: vec!["Cliente".into()], unique: true, ..Default::default() });
    t.indexes.push(IndexDef { name: "IX_Activo".into(), columns: vec!["Activo".into()], ..Default::default() });
    let r = convert(&[t], "sqlserver", "dynamodb", &Options::default()).unwrap();
    let v = &r.tables[0];
    assert_eq!(opt(column(v, "Tienda"), "key_type"), Some("HASH"));
    assert_eq!(opt(column(v, "Fecha"), "key_type"), Some("RANGE"));
    assert_eq!(opt(column(v, "Nro"), "key_type"), Some("none"));
    assert!(r.issues.iter().any(|i| i.code == IssueCode::PrimaryKeyDropped && i.severity == Severity::Loss));
    assert_eq!(ty(v, "Tienda"), "N");
    assert_eq!(ty(v, "Fecha"), "S");
    assert_eq!(ty(v, "Monto"), "N");
    assert_eq!(ty(v, "Activo"), "BOOL");
    // A BOOL can't be an index key; a unique index becomes a plain GSI.
    assert_eq!(v.indexes.len(), 1);
    assert_eq!((v.indexes[0].name.as_str(), v.indexes[0].unique, v.indexes[0].kind.as_deref()), ("IX_Cliente", false, Some("GSI")));
    let script = ddl("dynamodb", &r.tables);
    assert!(script.contains("\"AttributeName\": \"Cliente\""), "{script}");
    assert!(!script.contains("\"AttributeName\": \"Monto\""), "{script}");
    // Back: only key attributes (and sampled ones) are reported.
    let back = convert(&r.tables, "dynamodb", "postgres", &Options::default()).unwrap();
    assert_eq!(ty(&back.tables[0], "Tienda"), "numeric");
    assert_eq!(ty(&back.tables[0], "Activo"), "boolean");
}

#[test]
fn postgres_to_neo4j() {
    let r = convert(&pg_orders(), "postgres", "neo4j", &Options::default()).unwrap();
    let p = &r.tables[1];
    assert_eq!(p.kind, "label");
    assert_eq!(ty(p, "id"), "INTEGER");
    assert_eq!(ty(p, "total"), "FLOAT");
    assert_eq!(ty(p, "creado"), "ZONED DATETIME");
    assert_eq!(ty(p, "local_ts"), "LOCAL DATETIME");
    assert_eq!(ty(p, "etiquetas"), "LIST<STRING>");
    assert_eq!(ty(p, "datos"), "STRING");
    // FKs aren't relationships (yet): reported with the reason.
    assert!(p.foreign_keys.is_empty());
    assert!(r.issues.iter().any(|i| i.code == IssueCode::ForeignKeyDropped && i.message.contains("relación")));
    let pk = &p.indexes[0];
    assert_eq!((pk.name.as_str(), pk.kind.as_deref(), pk.columns.len()), ("pedidos_pk", Some("UNIQUE"), 2));
    let script = ddl("neo4j", &r.tables);
    assert!(script.contains("REQUIRE (e.`id`, e.`linea`) IS UNIQUE") || script.contains("REQUIRE (e.id, e.linea) IS UNIQUE"), "{script}");
    // Neptune: nothing to create but the report.
    let n = convert(&pg_orders(), "postgres", "neptune", &Options::default()).unwrap();
    assert!(n.tables[1].indexes.is_empty());
    // Back from a sampled label.
    let label = TableSchema {
        kind: "label".into(),
        name: "Persona".into(),
        columns: vec![col("nombre", "STRING"), col("edad", "INTEGER"), col("puntos", "INTEGER|FLOAT"), col("gustos", "LIST"), col("pos", "POINT")],
        ..Default::default()
    };
    let back = convert(&[label], "neo4j", "mysql", &Options::default()).unwrap();
    let b = &back.tables[0];
    assert_eq!(ty(b, "edad"), "bigint");
    assert_eq!(ty(b, "puntos"), "double");
    assert_eq!(ty(b, "gustos"), "json");
    assert_eq!(ty(b, "pos"), "point");
}

#[test]
fn to_cosmos_couchbase_couchdb_solr_orientdb() {
    let src = pg_orders();
    let c = convert(&src, "postgres", "cosmosdb", &Options::default()).unwrap();
    let p = &c.tables[1];
    assert_eq!(p.options.get("partition_key").map(String::as_str), Some("/id, /linea"));
    assert_eq!(ty(p, "total"), "number");
    assert_eq!(ty(p, "creado"), "string");
    // Single-field indexes are implicit; multi-field ones composite; unique = unique keys.
    assert!(p.indexes.iter().any(|i| i.name == "ix_pedidos_cli_dia" && i.kind.as_deref() == Some("composite")));
    assert!(!p.indexes.iter().any(|i| i.name == "ix_pedidos_estado"));
    assert!(p.indexes.iter().any(|i| i.name == "ux_pedidos_codigo" && i.unique));
    let script = ddl("cosmosdb", &c.tables);
    assert!(script.contains("\"kind\": \"MultiHash\""), "{script}");

    let cb = convert(&src, "postgres", "couchbase", &Options::default()).unwrap();
    let p = &cb.tables[1];
    assert!(p.schema.is_none() && cb.issues.iter().any(|i| i.code == IssueCode::OptionDropped && i.message.contains("bucket.scope")));
    assert_eq!(p.options.get("primary_index").map(String::as_str), Some("true"));
    assert!(p.indexes.iter().all(|i| !i.unique));
    assert!(p.indexes.iter().any(|i| i.name == "pedidos_pk"));
    let mut tables = cb.tables.clone();
    tables.iter_mut().for_each(|t| t.schema = Some("nsq._default".into()));
    let script = ddl("couchbase", &tables);
    assert!(script.contains("CREATE PRIMARY INDEX ON `nsq`.`_default`.`pedidos`"), "{script}");

    let cd = convert(&src, "postgres", "couchdb", &Options::default()).unwrap();
    assert!(cd.issues.iter().any(|i| i.code == IssueCode::TableChanged && i.message.contains("CouchDB no tiene tablas")));
    let script = ddl("couchdb", &cd.tables);
    assert!(script.contains("POST _index"), "{script}");

    let so = convert(&src, "postgres", "solr", &Options::default()).unwrap();
    let p = &so.tables[1];
    assert_eq!(ty(p, "id"), "string");
    assert!(has(&so, IssueCode::TypeChanged, Some("id")));
    assert_eq!(ty(p, "estado"), "string");
    assert_eq!(ty(p, "notas"), "text_general");
    assert_eq!(ty(p, "etiquetas"), "text_general[]");
    assert_eq!(column(p, "activo").default_value.as_deref(), Some("true"));
    assert_eq!(column(p, "creado").default_value.as_deref(), Some("NOW"));
    let script = ddl("solr", &so.tables);
    assert!(script.contains("\"name\":\"etiquetas\",\"type\":\"text_general\",\"multiValued\":true"), "{script}");

    let o = convert(&src, "postgres", "orientdb", &Options::default()).unwrap();
    let p = &o.tables[1];
    assert_eq!(ty(p, "id"), "LONG");
    assert_eq!(ty(p, "total"), "DECIMAL");
    assert_eq!(ty(p, "creado"), "DATETIME");
    assert_eq!(column(p, "creado").default_value.as_deref(), Some("sysdate()"));
    let script = ddl("orientdb", &o.tables);
    assert!(script.contains("CREATE INDEX pedidos.pk ON pedidos (id, linea) UNIQUE"), "{script}");

    // Documents back to SQL: the JSON types.
    let couch = TableSchema {
        kind: "collection".into(),
        name: "_all_docs".into(),
        columns: vec![not_null(col("_id", "string")), col("_rev", "string"), col("n", "integer|number"), col("o", "object"), col("x", "string|integer")],
        primary_key: Some(KeyDef { name: None, columns: vec!["_id".into()] }),
        ..Default::default()
    };
    let back = convert(&[couch], "couchdb", "postgres", &Options::default()).unwrap();
    assert_eq!(ty(&back.tables[0], "n"), "double precision");
    assert_eq!(ty(&back.tables[0], "o"), "jsonb");
    assert_eq!(ty(&back.tables[0], "x"), "jsonb");
}

// ---- against real servers ---------------------------------------------------

type S = Box<dyn Session>;

fn url(env: &str) -> Option<String> {
    match std::env::var(env) {
        Ok(u) => Some(u),
        Err(_) => {
            eprintln!("{env} no está definida: se saltea");
            None
        }
    }
}

async fn open(driver: &str, cfg: ConnectionConfig) -> S {
    let d = dbine_drivers::find(driver).unwrap_or_else(|| panic!("no driver {driver}"));
    d.connect(&cfg, None).await.unwrap_or_else(|e| panic!("{driver}: {e}"))
}

async fn postgres() -> Option<S> {
    Some(open("postgres", common::config("postgres", &url("DBINE_TEST_POSTGRES_URL")?)).await)
}

/// MySQL on its own `nsq_schema` database.
async fn mysql() -> Option<S> {
    let base = url("DBINE_TEST_MYSQL_URL")?;
    let mut cfg = common::config("mysql", &base);
    cfg.database = String::new();
    let mut s = open("mysql", cfg.clone()).await;
    common::exec(&mut s, "CREATE DATABASE IF NOT EXISTS nsq_schema").await;
    cfg.database = "nsq_schema".into();
    Some(open("mysql", cfg).await)
}

async fn mongo(driver: &str, env: &str) -> Option<S> {
    let mut c = ConnectionConfig { driver: driver.into(), database: "nsq_schema".into(), ..Default::default() };
    c.options.insert("connection_string".into(), url(env)?);
    Some(open(driver, c).await)
}

async fn cql(driver: &str, env: &str) -> Option<S> {
    let u = url(env)?;
    let (host, port) = u.rsplit_once(':').unwrap();
    let mut c = ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), ..Default::default() };
    let mut s = open(driver, c.clone()).await;
    let _ = s.create_database("nsq_schema").await;
    c.database = "nsq_schema".into();
    Some(open(driver, c).await)
}

async fn search(driver: &str, env: &str) -> Option<S> {
    Some(open(driver, ConnectionConfig { driver: driver.into(), host: url(env)?, ..Default::default() }).await)
}

async fn couchdb() -> Option<S> {
    let mut c = common::config("couchdb", &url("DBINE_TEST_COUCHDB_URL")?);
    let mut s = open("couchdb", c.clone()).await;
    let _ = s.create_database("nsq_schema").await;
    c.database = "nsq_schema".into();
    Some(open("couchdb", c).await)
}

async fn couchbase() -> Option<S> {
    let u = url("DBINE_TEST_COUCHBASE_URL")?;
    let mut c = common::config("couchbase", &u);
    c.username = Some("Administrator".into());
    c.password = Some("secreto1".into());
    c.database = "nsq".into();
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(open("couchbase", c).await)
}

async fn dynamodb() -> Option<S> {
    let mut c = ConnectionConfig { driver: "dynamodb".into(), ..Default::default() };
    for (k, v) in [("region", "us-east-1"), ("auth_mode", "keys"), ("access_key_id", "dummy"), ("secret_access_key", "dummy")] {
        c.options.insert(k.into(), v.into());
    }
    c.options.insert("endpoint_url".into(), url("DBINE_TEST_DYNAMODB_URL")?);
    Some(open("dynamodb", c).await)
}

async fn neo4j() -> Option<S> {
    let u = url("DBINE_TEST_NEO4J_URL")?;
    let (auth, hp) = u.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    let c = ConnectionConfig {
        driver: "neo4j".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    Some(open("neo4j", c).await)
}

/// Run a generated script unless it's only comments (a label's DDL).
async fn run_script(s: &mut S, script: &str) {
    let real = script.lines().any(|l| {
        let l = l.trim();
        !l.is_empty() && !l.starts_with("//") && !l.starts_with('#') && !l.starts_with("--")
    });
    if real {
        common::exec(s, script).await;
    }
}

/// Read `names` from `src`, convert, adjust (`fix`), drop and create them
/// on `dst` with the target driver's DDL.
async fn create_converted(src: &mut S, from: &str, names: &[&str], dst: &mut S, to: &str, fix: impl Fn(&mut TableSchema)) -> Conversion {
    let tables = common::read(src, names).await;
    assert_eq!(tables.len(), names.len(), "{from}: no se leyeron todas las tablas: {:?}", tables.iter().map(|t| &t.name).collect::<Vec<_>>());
    let mut conv = convert(&tables, from, to, &Options::default()).expect("convert");
    conv.tables.iter_mut().for_each(fix);
    let d = dbine_drivers::find(to).unwrap();
    for t in conv.tables.iter().rev() {
        if let Ok(drop) = d.table_ddl(t, DdlParts { drop: true, if_exists: true, ..Default::default() }) {
            if !drop.trim().is_empty() {
                common::exec_quiet(dst, &[drop.as_str()]).await;
            }
        }
    }
    for script in common::target_ddl(to, &conv.tables) {
        run_script(dst, &script).await;
    }
    conv
}

/// Insert `rows` with the target driver's own insert script (the data
/// copy's path), by position over the converted table's columns.
async fn insert(s: &mut S, driver: &str, t: &TableSchema, rows: &[Vec<Value>]) {
    let d = dbine_drivers::find(driver).unwrap();
    let cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
    let obj = ObjectRef { kind: t.kind.clone(), schema: t.schema.clone(), name: t.name.clone() };
    let script = d.insert_script(&obj, &cols, rows).unwrap_or_else(|e| panic!("insert_script {driver}: {e}"));
    common::exec(s, &script).await;
}

fn no_schema(t: &mut TableSchema) {
    t.schema = None;
}

/// The way back (A → B → A): `<name>_rt`, with its indexes renamed too
/// (PostgreSQL index names share the schema's namespace).
fn rt(t: &mut TableSchema) {
    t.name = format!("{}_rt", t.name.trim_start_matches('_'));
    t.schema = None;
    for ix in &mut t.indexes {
        ix.name = format!("{}_{}", t.name, ix.name);
    }
    if let Some(k) = &mut t.primary_key {
        k.name = None;
    }
}

const PG_SOURCE: &str = r#"
DROP TABLE IF EXISTS nsq_pedidos;
DROP TABLE IF EXISTS nsq_clientes;
CREATE TABLE nsq_clientes (id integer PRIMARY KEY, nombre varchar(80) NOT NULL);
CREATE TABLE nsq_pedidos (
    id bigint NOT NULL,
    linea smallint NOT NULL,
    cliente_id integer NOT NULL REFERENCES nsq_clientes (id) ON DELETE CASCADE,
    total numeric(12,2) NOT NULL DEFAULT 0,
    ratio double precision,
    peso real,
    codigo uuid,
    estado varchar(20),
    notas text,
    activo boolean DEFAULT true,
    creado timestamptz DEFAULT now(),
    local_ts timestamp(6),
    dia date,
    datos jsonb,
    etiquetas text[],
    PRIMARY KEY (id, linea)
);
CREATE UNIQUE INDEX nsq_pedidos_codigo ON nsq_pedidos (codigo);
CREATE INDEX nsq_pedidos_estado ON nsq_pedidos (estado);
INSERT INTO nsq_clientes VALUES (2147483647, 'Ñandú 漢字 🎉');
INSERT INTO nsq_pedidos VALUES (9223372036854775807, 32767, 2147483647, 9999999999.99, 1.7976931348623157e308, 3.4e38,
    'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', 'pendiente', 'ñ 漢字 🎉 "comillas"', true, '2024-02-29 23:59:59.999+00',
    '1999-12-31 23:59:59.999999', '2000-01-01', '{"a": [1, 2, {"b": null}]}', ARRAY['x', 'ñ']);
"#;

const MY_SOURCE: &str = r#"
DROP TABLE IF EXISTS nsq_productos;
CREATE TABLE nsq_productos (
    id int unsigned NOT NULL AUTO_INCREMENT PRIMARY KEY,
    sku varchar(40) NOT NULL,
    activo tinyint(1) NOT NULL DEFAULT 1,
    precio decimal(10,2) NOT NULL,
    stock mediumint,
    tipo enum('a','b','c'),
    descripcion mediumtext,
    alta datetime(3) DEFAULT CURRENT_TIMESTAMP(3),
    anio year,
    grande bigint unsigned,
    extra json,
    UNIQUE KEY nsq_productos_sku (sku)
);
INSERT INTO nsq_productos VALUES (4294967295, 'SKU-ñ', 1, 99999999.99, 8388607, 'c', 'texto 漢字 🎉', '2024-02-29 23:59:59.999', 2155, 18446744073709551615, '{"k": [1, 2]}');
"#;

/// The PostgreSQL row, as JSON cells in column order (clientes, pedidos).
fn pg_rows(iso_dates: bool) -> (Vec<Value>, Vec<Value>) {
    let ts = |iso: &str, sql: &str| if iso_dates { json!(iso) } else { json!(sql) };
    (
        vec![json!(2147483647), json!("Ñandú 漢字 🎉")],
        vec![
            json!(9223372036854775807i64),
            json!(32767),
            json!(2147483647),
            json!(9999999999.99),
            json!(1.7976931348623157e308),
            json!(3.4e38),
            json!("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
            json!("pendiente"),
            json!("ñ 漢字 🎉 \"comillas\""),
            json!(true),
            ts("2024-02-29T23:59:59.999Z", "2024-02-29 23:59:59.999+0000"),
            ts("1999-12-31T23:59:59.999Z", "1999-12-31 23:59:59.999+0000"),
            json!("2000-01-01"),
            json!({"a": [1, 2, {"b": null}]}),
            json!(["x", "ñ"]),
        ],
    )
}

fn back<'a>(tables: &'a [TableSchema], name: &str) -> &'a TableSchema {
    tables.iter().find(|t| t.name.eq_ignore_ascii_case(name)).unwrap_or_else(|| panic!("no table {name}"))
}

fn back_ty(t: &TableSchema, c: &str) -> String {
    common::col(t, c).data_type.to_ascii_lowercase()
}

/// MySQL → `to` (one way): the conversion is accepted by the target DDL.
async fn mysql_to(dst: &mut S, to: &str, fix: impl Fn(&mut TableSchema)) -> Option<Conversion> {
    let mut my = mysql().await?;
    common::exec(&mut my, MY_SOURCE).await;
    Some(create_converted(&mut my, "mysql", &["nsq_productos"], dst, to, fix).await)
}

// -- MongoDB family --

async fn mongo_family(driver: &str, env: &str) {
    let Some(mut m) = mongo(driver, env).await else { return };
    let validators = driver != "ferretdb";
    if let Some(mut pg) = postgres().await {
        common::exec(&mut pg, PG_SOURCE).await;
        // PostgreSQL → Mongo: validator and unique key index.
        let conv = create_converted(&mut pg, "postgres", &["nsq_clientes", "nsq_pedidos"], &mut m, driver, no_schema).await;
        common::exec(
            &mut m,
            r#"db.getCollection("nsq_clientes").insertOne({id: 2147483647, nombre: "Ñandú 漢字 🎉"})
db.getCollection("nsq_pedidos").insertOne({id: NumberLong("9223372036854775807"), linea: 32767, cliente_id: 2147483647,
  total: NumberDecimal("9999999999.99"), ratio: 1.7976931348623157e308, peso: 3.4e38, codigo: "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
  estado: "pendiente", notas: "ñ 漢字 🎉", activo: true, creado: ISODate("2024-02-29T23:59:59.999Z"),
  local_ts: ISODate("1999-12-31T23:59:59.999Z"), dia: ISODate("2000-01-01T00:00:00Z"), datos: {a: [1, 2, {b: null}]}, etiquetas: ["x", "ñ"]})"#,
        )
        .await;
        if validators {
            // The validator rejects a document without a required field or with a wrong type.
            let mut out = dbine_driver::QueryOutcome::default();
            let bad = m.execute(r#"db.getCollection("nsq_pedidos").insertOne({id: "x", linea: 1})"#, 10, &mut out).await;
            assert!(bad.is_err(), "{driver}: el validador tenía que rechazar el documento");
        }
        // The unique index on the former primary key holds.
        let mut out = dbine_driver::QueryOutcome::default();
        let dup = m
            .execute(r#"db.getCollection("nsq_clientes").insertOne({id: 2147483647, nombre: "otro"})"#, 10, &mut out)
            .await;
        assert!(dup.is_err(), "{driver}: la clave única tenía que rechazar el duplicado");
        let read = common::read(&mut m, &["nsq_clientes", "nsq_pedidos"]).await;
        let p = back(&read, "nsq_pedidos");
        assert_eq!(back_ty(p, "id"), "long");
        assert_eq!(back_ty(p, "total"), "decimal");
        assert_eq!(back_ty(p, "creado"), "date");
        assert_eq!(back_ty(p, "datos"), "object");
        assert!(p.indexes.iter().any(|i| i.name == "nsq_pedidos_pk" && i.unique));
        assert!(conv.issues.iter().any(|i| i.code == IssueCode::ForeignKeyDropped));

        // … and back to PostgreSQL (A → B → A).
        let back_conv = create_converted(&mut m, driver, &["nsq_pedidos"], &mut pg, "postgres", rt)
        .await;
        let rt = common::read(&mut pg, &["nsq_pedidos_rt"]).await;
        let t = &rt[0];
        assert_eq!(back_ty(t, "id"), "bigint");
        assert_eq!(back_ty(t, "linea"), "integer");
        assert_eq!(back_ty(t, "total"), "numeric");
        assert_eq!(back_ty(t, "creado"), "timestamp(3) with time zone");
        assert_eq!(back_ty(t, "datos"), "jsonb");
        assert_eq!(back_ty(t, "_id"), "character(24)");
        let (_, row) = pg_rows(false);
        let mut full = vec![json!("65a1b2c3d4e5f60718293a4b")];
        full.extend(row);
        let full = &back_conv.tables[0].columns.iter().map(|c| {
            let src = ["_id", "id", "linea", "cliente_id", "total", "ratio", "peso", "codigo", "estado", "notas", "activo", "creado", "local_ts", "dia", "datos", "etiquetas"];
            src.iter().position(|s| *s == c.name).map_or(Value::Null, |i| full[i].clone())
        }).collect::<Vec<_>>();
        insert(&mut pg, "postgres", &back_conv.tables[0], std::slice::from_ref(full)).await;
    }

    // Mongo → SQL: a collection with every BSON kind.
    common::exec_quiet(&mut m, &[r#"db.getCollection("nsq_personas").drop()"#]).await;
    common::exec(
        &mut m,
        r#"db.getCollection("nsq_personas").insertMany([
  {_id: ObjectId("65a1b2c3d4e5f60718293a4b"), nombre: "Ñandú 🎉", edad: 30, saldo: NumberDecimal("12345678901234567890.1234"),
   alta: ISODate("2024-02-29T23:59:59.999Z"), tags: ["a", "b"], dir: {calle: "x", n: 1}, mixto: "texto", activo: true, grande: NumberLong("9223372036854775807")},
  {_id: ObjectId("65a1b2c3d4e5f60718293a4c"), nombre: "Otro", edad: 31, mixto: 5, activo: false}
])
db.getCollection("nsq_personas").createIndex({nombre: 1}, {name: "nombre_1", unique: true})"#,
    )
    .await;
    for (sql, env_ok) in [("postgres", std::env::var("DBINE_TEST_POSTGRES_URL").is_ok()), ("mysql", std::env::var("DBINE_TEST_MYSQL_URL").is_ok())] {
        if !env_ok {
            continue;
        }
        let mut dst = if sql == "postgres" { postgres().await.unwrap() } else { mysql().await.unwrap() };
        let conv = create_converted(&mut m, driver, &["nsq_personas"], &mut dst, sql, no_schema).await;
        let t = &common::read(&mut dst, &["nsq_personas"]).await[0];
        assert!(back_ty(t, "_id").starts_with(if sql == "postgres" { "character(24)" } else { "char(24)" }), "{:?}", common::col(t, "_id"));
        assert_eq!(t.primary_key.as_ref().unwrap().columns, ["_id"]);
        let json_ty = if sql == "postgres" { "jsonb" } else { "json" };
        assert_eq!(back_ty(t, "tags"), json_ty);
        assert_eq!(back_ty(t, "dir"), json_ty);
        assert_eq!(back_ty(t, "mixto"), json_ty);
        assert_eq!(back_ty(t, "grande"), "bigint");
        assert!(t.indexes.iter().any(|i| i.unique));
        let c = &conv.tables[0];
        let row: Vec<Value> = c
            .columns
            .iter()
            .map(|col| match col.name.as_str() {
                "_id" => json!("65a1b2c3d4e5f60718293a4b"),
                "nombre" => json!("Ñandú 🎉"),
                "edad" => json!(30),
                "saldo" => json!("12345678901234567890.1234"),
                "alta" => json!("2024-02-29 23:59:59.999"),
                "tags" => json!(["a", "b"]),
                "dir" => json!({"calle": "x", "n": 1}),
                "mixto" => json!("\"texto\""),
                "activo" => json!(true),
                "grande" => json!(9223372036854775807i64),
                _ => Value::Null,
            })
            .collect();
        insert(&mut dst, sql, c, &[row]).await;
    }

    if let Some(conv) = mysql_to(&mut m, driver, no_schema).await {
        let t = &conv.tables[0];
        assert_eq!(t.columns.iter().find(|c| c.name == "grande").unwrap().data_type, "decimal");
        common::exec(
            &mut m,
            r#"db.getCollection("nsq_productos").insertOne({id: NumberLong("4294967295"), sku: "SKU-ñ", activo: true, precio: NumberDecimal("99999999.99"),
  stock: 8388607, tipo: "c", descripcion: "texto 漢字 🎉", alta: ISODate("2024-02-29T23:59:59.999Z"), anio: 2155,
  grande: NumberDecimal("18446744073709551615"), extra: {k: [1, 2]}})"#,
        )
        .await;
    }
}

#[tokio::test]
#[ignore]
async fn live_mongodb() {
    mongo_family("mongodb", "DBINE_TEST_MONGODB_URL").await;
}

#[tokio::test]
#[ignore]
async fn live_ferretdb() {
    mongo_family("ferretdb", "DBINE_TEST_FERRETDB_URL").await;
}

// -- CQL --

async fn cql_family(driver: &str, env: &str) {
    let Some(mut c) = cql(driver, env).await else { return };
    if let Some(mut pg) = postgres().await {
        common::exec(&mut pg, PG_SOURCE).await;
        let conv = create_converted(&mut pg, "postgres", &["nsq_clientes", "nsq_pedidos"], &mut c, driver, no_schema).await;
        let (cli, mut ped) = pg_rows(false);
        // JSON became text, but the driver's `INSERT … JSON` turns text that
        // looks like JSON back into an object, which a text column refuses:
        // that value goes with a plain UPDATE.
        ped[13] = Value::Null;
        insert(&mut c, driver, &conv.tables[0], &[cli]).await;
        insert(&mut c, driver, &conv.tables[1], &[ped]).await;
        common::exec(&mut c, r#"UPDATE nsq_pedidos SET datos = '{"a": [1, 2, {"b": null}]}' WHERE id = 9223372036854775807 AND linea = 32767"#).await;
        let read = common::read(&mut c, &["nsq_clientes", "nsq_pedidos"]).await;
        let p = back(&read, "nsq_pedidos");
        assert_eq!(back_ty(p, "id"), "bigint");
        assert_eq!(back_ty(p, "linea"), "smallint");
        assert_eq!(back_ty(p, "total"), "decimal");
        assert_eq!(back_ty(p, "peso"), "float");
        assert_eq!(back_ty(p, "codigo"), "uuid");
        assert_eq!(back_ty(p, "creado"), "timestamp");
        assert_eq!(back_ty(p, "dia"), "date");
        assert_eq!(back_ty(p, "etiquetas"), "list<text>");
        assert_eq!(p.primary_key.as_ref().unwrap().columns, ["id", "linea"]);
        assert_eq!(common::col(p, "linea").options.get("clustering_key").map(String::as_str), Some("true"));

        // … and back to PostgreSQL.
        let rt = create_converted(&mut c, driver, &["nsq_pedidos"], &mut pg, "postgres", rt)
        .await;
        let t = &common::read(&mut pg, &["nsq_pedidos_rt"]).await[0];
        assert_eq!(back_ty(t, "id"), "bigint");
        assert_eq!(back_ty(t, "linea"), "smallint");
        assert_eq!(back_ty(t, "total"), "numeric");
        assert_eq!(back_ty(t, "peso"), "real");
        assert_eq!(back_ty(t, "codigo"), "uuid");
        assert_eq!(back_ty(t, "creado"), "timestamp(3) with time zone");
        assert_eq!(back_ty(t, "etiquetas"), "text[]");
        assert_eq!(t.primary_key.as_ref().unwrap().columns, ["id", "linea"]);
        let (_, row) = pg_rows(false);
        let names = ["id", "linea", "cliente_id", "total", "ratio", "peso", "codigo", "estado", "notas", "activo", "creado", "local_ts", "dia", "datos", "etiquetas"];
        let cells: Vec<Value> = rt.tables[0]
            .columns
            .iter()
            .map(|col| {
                let v = names.iter().position(|n| *n == col.name).map_or(Value::Null, |i| row[i].clone());
                // A JSON column that became text holds the JSON's text.
                if col.name == "datos" { json!(v.to_string()) } else if col.name == "etiquetas" { json!("{x,ñ}") } else { v }
            })
            .collect();
        insert(&mut pg, "postgres", &rt.tables[0], &[cells]).await;
    }

    // CQL → SQL: a table with collections, a UDT-free tuple and a clustering key.
    common::exec_quiet(&mut c, &["DROP TABLE IF EXISTS nsq_eventos"]).await;
    common::exec(
        &mut c,
        "CREATE TABLE nsq_eventos (sensor text, ts timestamp, valor double, tags set<text>, attrs map<text, int>, pos frozen<tuple<double, double>>, \
         uid timeuuid, big varint, ip inet, t time, PRIMARY KEY (sensor, ts)) WITH CLUSTERING ORDER BY (ts DESC)",
    )
    .await;
    for (sql, ok) in [("postgres", std::env::var("DBINE_TEST_POSTGRES_URL").is_ok()), ("mysql", std::env::var("DBINE_TEST_MYSQL_URL").is_ok())] {
        if !ok {
            continue;
        }
        let mut dst = if sql == "postgres" { postgres().await.unwrap() } else { mysql().await.unwrap() };
        let conv = create_converted(&mut c, driver, &["nsq_eventos"], &mut dst, sql, no_schema).await;
        let t = &common::read(&mut dst, &["nsq_eventos"]).await[0];
        assert_eq!(t.primary_key.as_ref().unwrap().columns, ["sensor", "ts"]);
        assert_eq!(back_ty(t, "big"), if sql == "postgres" { "numeric" } else { "decimal(65,30)" });
        let row: Vec<Value> = conv.tables[0]
            .columns
            .iter()
            .map(|col| match col.name.as_str() {
                "sensor" => json!("s-ñ"),
                "ts" => json!("2024-02-29 23:59:59.999"),
                "valor" => json!(1.5),
                "tags" => if sql == "postgres" { json!("{a,b}") } else { json!(["a", "b"]) },
                "attrs" => json!({"k": 1}),
                "pos" => json!([1.5, 2.5]),
                "uid" => json!("d2177dd0-eaa2-11de-a572-001b779c76e3"),
                "big" => json!("123456789012345678901234567890"),
                "ip" => json!("10.0.0.1"),
                "t" => json!("23:59:59.123456"),
                _ => Value::Null,
            })
            .collect();
        insert(&mut dst, sql, &conv.tables[0], &[row]).await;
    }

    if let Some(conv) = mysql_to(&mut c, driver, no_schema).await {
        insert(
            &mut c,
            driver,
            &conv.tables[0],
            &[vec![
                json!(4294967295u64),
                json!("SKU-ñ"),
                json!(true),
                json!(99999999.99),
                json!(8388607),
                json!("c"),
                json!("texto 漢字 🎉"),
                json!("2024-02-29 23:59:59.999+0000"),
                json!(2155),
                json!(18446744073709551615u64),
                Value::Null,
            ]],
        )
        .await;
        common::exec(&mut c, r#"UPDATE nsq_productos SET extra = '{"k": [1, 2]}' WHERE id = 4294967295"#).await;
    }
}

#[tokio::test]
#[ignore]
async fn live_cassandra() {
    cql_family("cassandra", "DBINE_TEST_CASSANDRA_URL").await;
}

#[tokio::test]
#[ignore]
async fn live_scylladb() {
    cql_family("scylladb", "DBINE_TEST_SCYLLADB_URL").await;
}

// -- Elasticsearch family --

async fn search_family(driver: &str, env: &str) {
    let Some(mut es) = search(driver, env).await else { return };
    if let Some(mut pg) = postgres().await {
        common::exec(&mut pg, PG_SOURCE).await;
        let conv = create_converted(&mut pg, "postgres", &["nsq_clientes", "nsq_pedidos"], &mut es, driver, no_schema).await;
        let (_, ped) = pg_rows(true);
        insert(&mut es, driver, &conv.tables[1], &[ped]).await;
        // SQL drivers print timestamps with a space: the format takes them too.
        let (_, mut ped2) = pg_rows(false);
        ped2[1] = json!(1);
        insert(&mut es, driver, &conv.tables[1], &[ped2]).await;
        let read = common::read(&mut es, &["nsq_pedidos"]).await;
        let p = &read[0];
        assert_eq!(back_ty(p, "id"), "long");
        assert_eq!(back_ty(p, "total"), "scaled_float");
        let factor = common::col(p, "total").options.get("scaling_factor").and_then(|f| f.parse::<f64>().ok());
        assert_eq!(factor, Some(100.0));
        assert_eq!(back_ty(p, "estado"), "keyword");
        assert_eq!(back_ty(p, "notas"), "text");
        assert_eq!(back_ty(p, "local_ts"), "date_nanos");
        assert_eq!(back_ty(p, "datos"), if driver == "opensearch" { "flat_object" } else { "flattened" });

        // … and back to PostgreSQL.
        let rt = create_converted(&mut es, driver, &["nsq_pedidos"], &mut pg, "postgres", rt)
        .await;
        let t = &common::read(&mut pg, &["nsq_pedidos_rt"]).await[0];
        assert_eq!(back_ty(t, "id"), "bigint");
        assert_eq!(back_ty(t, "total"), "numeric");
        assert_eq!(back_ty(t, "creado"), "timestamp(3) with time zone");
        assert_eq!(back_ty(t, "local_ts"), "timestamp(6) with time zone");
        assert!(rt.issues.iter().any(|i| i.object.as_deref() == Some("local_ts") && i.code == IssueCode::PrecisionLoss));
    }

    // Search index → SQL.
    common::exec_quiet(&mut es, &["DELETE /nsq_docs"]).await;
    common::exec(
        &mut es,
        r#"PUT /nsq_docs
{"mappings": {"properties": {"titulo": {"type": "text"}, "cat": {"type": "keyword"}, "n": {"type": "integer"},
 "precio": {"type": "scaled_float", "scaling_factor": 100}, "fecha": {"type": "date"}, "ok": {"type": "boolean"},
 "ip": {"type": "ip"}, "autor": {"properties": {"nombre": {"type": "keyword"}}}}}}"#,
    )
    .await;
    for (sql, ok) in [("postgres", std::env::var("DBINE_TEST_POSTGRES_URL").is_ok()), ("mysql", std::env::var("DBINE_TEST_MYSQL_URL").is_ok())] {
        if !ok {
            continue;
        }
        let mut dst = if sql == "postgres" { postgres().await.unwrap() } else { mysql().await.unwrap() };
        let conv = create_converted(&mut es, driver, &["nsq_docs"], &mut dst, sql, no_schema).await;
        let t = &common::read(&mut dst, &["nsq_docs"]).await[0];
        assert_eq!(back_ty(t, "n"), if sql == "postgres" { "integer" } else { "int" });
        assert!(t.columns.iter().any(|c| c.name == "autor.nombre"));
        let row: Vec<Value> = conv.tables[0]
            .columns
            .iter()
            .map(|c| match c.name.as_str() {
                "titulo" | "cat" | "autor.nombre" => json!("ñandú 🎉"),
                "n" => json!(2147483647),
                "precio" => json!("12.34"),
                "fecha" => json!("2024-02-29 23:59:59.999"),
                "ok" => json!(true),
                "ip" => json!("10.0.0.1"),
                "autor" => json!({"nombre": "x"}),
                _ => Value::Null,
            })
            .collect();
        insert(&mut dst, sql, &conv.tables[0], &[row]).await;
    }

    if let Some(conv) = mysql_to(&mut es, driver, no_schema).await {
        let t = &conv.tables[0];
        assert_eq!(t.columns.iter().find(|c| c.name == "grande").unwrap().data_type, "unsigned_long");
        insert(
            &mut es,
            driver,
            t,
            &[vec![
                json!(4294967295u64),
                json!("SKU-ñ"),
                json!(true),
                json!(99999999.99),
                json!(8388607),
                json!("c"),
                json!("texto 漢字 🎉"),
                json!("2024-02-29 23:59:59.999"),
                json!(2155),
                json!(18446744073709551615u64),
                json!({"k": [1, 2]}),
            ]],
        )
        .await;
    }
}

#[tokio::test]
#[ignore]
async fn live_elasticsearch() {
    search_family("elasticsearch", "DBINE_TEST_ELASTICSEARCH_URL").await;
}

#[tokio::test]
#[ignore]
async fn live_opensearch() {
    search_family("opensearch", "DBINE_TEST_OPENSEARCH_URL").await;
}

// -- JSON documents, key-value and graph --

/// SQL tables into a schemaless target, the rows through its insert
/// script, then the sampled fields back into PostgreSQL / MySQL.
async fn documents(driver: &str, dst: &mut S, fix: impl Fn(&mut TableSchema) + Copy, sampled: &[&str]) {
    let Some(mut pg) = postgres().await else { return };
    common::exec(&mut pg, PG_SOURCE).await;
    let conv = create_converted(&mut pg, "postgres", &["nsq_clientes", "nsq_pedidos"], dst, driver, fix).await;
    let (cli, mut ped) = pg_rows(true);
    if driver == "couchbase" {
        // The Couchbase driver can't read back a document holding a double
        // next to f64::MAX (Couchbase prints it as a 309-digit integer and
        // serde_json's fast float parsing overflows): reported, not this
        // crate's business.
        ped[4] = json!(1.0e300);
    }
    if driver == "dynamodb" {
        // DynamoDB numbers stop at 9.99E+125 (reported as a range loss).
        ped[4] = json!(9.99e125);
    }
    insert(dst, driver, &conv.tables[0], &[cli]).await;
    insert(dst, driver, &conv.tables[1], &[ped]).await;
    let names: Vec<&str> = sampled.to_vec();
    let read = common::read(dst, &names).await;
    assert_eq!(read.len(), names.len(), "{driver}: {:?}", read.iter().map(|t| &t.name).collect::<Vec<_>>());
    for sql in ["postgres", "mysql"] {
        let mut target = if sql == "postgres" { postgres().await.unwrap() } else {
            match mysql().await {
                Some(m) => m,
                None => continue,
            }
        };
        let back_conv = create_converted(dst, driver, &names, &mut target, sql, rt)
        .await;
        let t = back_conv.tables.last().unwrap();
        let rt = &common::read(&mut target, &[t.name.as_str()]).await[0];
        let find = |c: &str| rt.columns.iter().find(|x| x.name.eq_ignore_ascii_case(c));
        if let Some(c) = find("datos") {
            assert!(matches!(c.data_type.as_str(), "jsonb" | "json" | "text" | "longtext"), "{sql} datos: {}", c.data_type);
        }
        if let Some(c) = find("activo") {
            assert!(matches!(c.data_type.as_str(), "boolean" | "tinyint(1)"), "{sql} activo: {}", c.data_type);
        }
        let row: Vec<Value> = t
            .columns
            .iter()
            .map(|c| {
                // A value of the column's type (the sample decided it).
                let t = c.data_type.to_ascii_lowercase();
                if t.contains("bool") || t == "tinyint(1)" {
                    json!(true)
                } else if t.contains("json") {
                    json!("{\"a\": [1, \"ñ\"]}")
                } else if ["int", "numeric", "decimal", "double", "real", "float"].iter().any(|n| t.contains(n)) {
                    json!(32767)
                } else if t.contains("text") || t.contains("char") {
                    json!("ñ 漢字 🎉")
                } else {
                    Value::Null
                }
            })
            .collect();
        insert(&mut target, sql, t, &[row]).await;
    }
}

#[tokio::test]
#[ignore]
async fn live_couchdb() {
    let Some(mut c) = couchdb().await else { return };
    // Mango indexes of every converted table land in the session's database.
    documents("couchdb", &mut c, no_schema, &["_all_docs"]).await;
}

#[tokio::test]
#[ignore]
async fn live_couchbase() {
    let Some(mut c) = couchbase().await else { return };
    common::exec_quiet(&mut c, &["CREATE SCOPE `nsq`.`s1` IF NOT EXISTS"]).await;
    documents("couchbase", &mut c, |t| t.schema = Some("nsq.s1".into()), &["nsq_clientes", "nsq_pedidos"]).await;
}

#[tokio::test]
#[ignore]
async fn live_dynamodb() {
    let Some(mut d) = dynamodb().await else { return };
    documents("dynamodb", &mut d, no_schema, &["nsq_clientes", "nsq_pedidos"]).await;
    let read = common::read(&mut d, &["nsq_pedidos"]).await;
    let p = &read[0];
    assert_eq!(common::col(p, "id").options.get("key_type").map(String::as_str), Some("HASH"));
    assert_eq!(common::col(p, "linea").options.get("key_type").map(String::as_str), Some("RANGE"));
    assert_eq!(back_ty(p, "id"), "n");
    assert!(p.indexes.iter().any(|i| i.name == "nsq_pedidos_codigo"));
    if let Some(conv) = mysql_to(&mut d, "dynamodb", no_schema).await {
        assert_eq!(conv.tables[0].primary_key.as_ref().unwrap().columns, ["id"]);
    }
}

#[tokio::test]
#[ignore]
async fn live_neo4j() {
    let Some(mut n) = neo4j().await else { return };
    common::exec_quiet(&mut n, &["MATCH (x:nsq_clientes) DETACH DELETE x", "MATCH (x:nsq_pedidos) DETACH DELETE x"]).await;
    documents("neo4j", &mut n, no_schema, &["nsq_clientes", "nsq_pedidos"]).await;
    // The primary key is a uniqueness constraint.
    let mut out = dbine_driver::QueryOutcome::default();
    let dup = n.execute("CREATE (:nsq_clientes {id: 2147483647, nombre: 'otro'})", 10, &mut out).await;
    assert!(dup.is_err(), "la restricción de unicidad tenía que rechazar el duplicado");
    let read = common::read(&mut n, &["nsq_pedidos"]).await;
    assert!(read[0].indexes.iter().any(|i| i.unique && i.columns == ["id", "linea"]), "{:?}", read[0].indexes);
    if let Some(conv) = mysql_to(&mut n, "neo4j", no_schema).await {
        assert!(conv.tables[0].indexes.iter().any(|i| i.kind.as_deref() == Some("UNIQUE")));
    }
}
