//! The table designer and the scripts DBine writes for DynamoDB: the
//! `CREATE TABLE` / `CREATE INDEX` extensions (see `admin`) and PartiQL
//! `INSERT … VALUE` for rows.

use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, IndexDef, Result, RowChange,
    TableSchema,
};
use dbine_driver::filter::{insert_where, sql_condition, ColumnFilter, FilterOp, SqlFilterStyle};
use serde_json::{json, Map, Value as Json};

pub(crate) const KEY_TYPE: &str = "key_type";
pub(crate) const BILLING_MODE: &str = "billing_mode";
pub(crate) const READ_CAPACITY: &str = "read_capacity";
pub(crate) const WRITE_CAPACITY: &str = "write_capacity";
pub(crate) const TTL_ATTRIBUTE: &str = "ttl_attribute";
pub(crate) const STREAM_VIEW_TYPE: &str = "stream_view_type";

/// Capacity units used when the table is PROVISIONED and none are given.
const DEFAULT_CAPACITY: i64 = 5;

pub(crate) fn designer() -> DesignerSpec {
    DesignerSpec {
        kind: kinds::TABLE,
        label: "Nueva tabla",
        data_types: vec!["S", "N", "B"],
        schemas: false,
        // The key comes from each attribute's "Clave" option.
        primary_key: false,
        auto_increment: false,
        defaults: false,
        nullability: false,
        comments: false,
        indexes: true,
        foreign_keys: false,
        column_options: vec![Field::new(
            KEY_TYPE,
            "Clave",
            FieldKind::Select(vec![
                ("none", "Ninguna"),
                ("HASH", "Clave de partición (HASH)"),
                ("RANGE", "Clave de ordenación (RANGE)"),
            ]),
        )
        .default_value("none")
        .help(
            "Solo hace falta declarar los atributos que son clave de la tabla o de un índice; el resto de los \
             atributos de cada ítem es libre.",
        )],
        table_options: vec![
            Field::new(
                BILLING_MODE,
                "Modo de capacidad",
                FieldKind::Select(vec![("PAY_PER_REQUEST", "Bajo demanda"), ("PROVISIONED", "Aprovisionada")]),
            )
            .default_value("PAY_PER_REQUEST"),
            Field::new(READ_CAPACITY, "Unidades de lectura (RCU)", FieldKind::Number)
                .default_value("5")
                .help("Solo con capacidad aprovisionada; también se usa para los índices globales."),
            Field::new(WRITE_CAPACITY, "Unidades de escritura (WCU)", FieldKind::Number)
                .default_value("5")
                .help("Solo con capacidad aprovisionada; también se usa para los índices globales."),
            Field::new(TTL_ATTRIBUTE, "Atributo de TTL", FieldKind::Text)
                .placeholder("expira")
                .help("Atributo numérico con la fecha de vencimiento (segundos Unix). Vacío = sin TTL."),
            Field::new(
                STREAM_VIEW_TYPE,
                "DynamoDB Streams",
                FieldKind::Select(vec![
                    ("none", "Desactivado"),
                    ("KEYS_ONLY", "Solo las claves"),
                    ("NEW_IMAGE", "Imagen nueva"),
                    ("OLD_IMAGE", "Imagen anterior"),
                    ("NEW_AND_OLD_IMAGES", "Imágenes nueva y anterior"),
                ]),
            )
            .default_value("none"),
        ],
        columns_required: true,
    }
}

pub(crate) fn create_templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: kinds::INDEX,
            label: "Nuevo índice secundario global",
            template: "-- Agrega un índice secundario global (GSI) a una tabla existente.\n\
                       CREATE INDEX \"{name}\" ON \"tabla\" {\n  \
                         \"AttributeDefinitions\": [\n    { \"AttributeName\": \"estado\", \"AttributeType\": \"S\" },\n    \
                         { \"AttributeName\": \"fecha\", \"AttributeType\": \"N\" }\n  ],\n  \
                         \"KeySchema\": [\n    { \"AttributeName\": \"estado\", \"KeyType\": \"HASH\" },\n    \
                         { \"AttributeName\": \"fecha\", \"KeyType\": \"RANGE\" }\n  ],\n  \
                         \"Projection\": { \"ProjectionType\": \"ALL\" }\n};\n"
                .into(),
        },
        CreateTemplate {
            kind: kinds::TABLE,
            label: "Nueva tabla (JSON de CreateTable)",
            template: "-- El cuerpo es la entrada de CreateTable de la API de DynamoDB.\n\
                       CREATE TABLE IF NOT EXISTS \"{name}\" {\n  \
                         \"AttributeDefinitions\": [\n    { \"AttributeName\": \"pk\", \"AttributeType\": \"S\" },\n    \
                         { \"AttributeName\": \"sk\", \"AttributeType\": \"S\" }\n  ],\n  \
                         \"KeySchema\": [\n    { \"AttributeName\": \"pk\", \"KeyType\": \"HASH\" },\n    \
                         { \"AttributeName\": \"sk\", \"KeyType\": \"RANGE\" }\n  ],\n  \
                         \"BillingMode\": \"PAY_PER_REQUEST\",\n  \
                         \"TimeToLiveSpecification\": { \"AttributeName\": \"expira\", \"Enabled\": true }\n};\n"
                .into(),
        },
    ]
}

pub(crate) fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn opt<'a>(t: &'a TableSchema, key: &str) -> Option<&'a str> {
    t.options.get(key).map(|v| v.trim()).filter(|v| !v.is_empty() && *v != "none")
}

/// Partition and sort key of the designed table: the "Clave" option of
/// each attribute, else the primary key's columns (partition, sort).
pub(crate) fn table_keys(t: &TableSchema) -> Result<(String, Option<String>)> {
    let marked = |kt: &str| -> Vec<&ColumnDef> {
        t.columns.iter().filter(|c| c.options.get(KEY_TYPE).is_some_and(|v| v.eq_ignore_ascii_case(kt))).collect()
    };
    let (hash, range) = (marked("HASH"), marked("RANGE"));
    if hash.len() > 1 || range.len() > 1 {
        return Err(Error::Query("La tabla admite una sola clave de partición (HASH) y una sola de ordenación (RANGE).".into()));
    }
    if let Some(h) = hash.first() {
        return Ok((h.name.clone(), range.first().map(|r| r.name.clone())));
    }
    if !range.is_empty() {
        return Err(Error::Query("Falta la clave de partición: marcá un atributo como HASH.".into()));
    }
    match t.primary_key.as_ref().map(|k| k.columns.as_slice()) {
        Some([h]) => Ok((h.clone(), None)),
        Some([h, r]) => Ok((h.clone(), Some(r.clone()))),
        _ => Err(Error::Query("Falta la clave de partición: marcá un atributo como HASH.".into())),
    }
}

fn key_schema(hash: &str, range: Option<&str>) -> Json {
    let mut v = vec![json!({ "AttributeName": hash, "KeyType": "HASH" })];
    if let Some(r) = range {
        v.push(json!({ "AttributeName": r, "KeyType": "RANGE" }));
    }
    Json::Array(v)
}

fn capacity(t: &TableSchema, key: &str) -> Result<i64> {
    match opt(t, key) {
        None => Ok(DEFAULT_CAPACITY),
        Some(v) => v
            .parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::Query(format!("La capacidad «{v}» no es un número entero positivo."))),
    }
}

fn provisioned(t: &TableSchema) -> Result<Option<Json>> {
    let mode = opt(t, BILLING_MODE).unwrap_or("PAY_PER_REQUEST");
    if !mode.eq_ignore_ascii_case("PROVISIONED") {
        return Ok(None);
    }
    Ok(Some(json!({ "ReadCapacityUnits": capacity(t, READ_CAPACITY)?, "WriteCapacityUnits": capacity(t, WRITE_CAPACITY)? })))
}

pub(crate) enum IndexKind {
    Global,
    Local,
}

pub(crate) fn index_kind(i: &IndexDef) -> Result<IndexKind> {
    match i.kind.as_deref().map(str::trim).unwrap_or("") {
        "" => Ok(IndexKind::Global),
        k if k.eq_ignore_ascii_case("gsi") || k.eq_ignore_ascii_case("global") => Ok(IndexKind::Global),
        k if k.eq_ignore_ascii_case("lsi") || k.eq_ignore_ascii_case("local") => Ok(IndexKind::Local),
        k => Err(Error::Query(format!("Tipo de índice «{k}» desconocido: usá GSI o LSI."))),
    }
}

/// Key of an index: `[hash, range?]` for a GSI; for an LSI, the table's
/// partition key plus the index's sort key (`[range]` or `[hash, range]`).
fn index_keys(i: &IndexDef, table_hash: &str) -> Result<(IndexKind, String, Option<String>)> {
    let kind = index_kind(i)?;
    let cols = &i.columns;
    match kind {
        IndexKind::Global => match cols.as_slice() {
            [h] => Ok((kind, h.clone(), None)),
            [h, r] => Ok((kind, h.clone(), Some(r.clone()))),
            _ => Err(Error::Query(format!(
                "El índice {} necesita uno o dos atributos: clave de partición y, opcional, de ordenación.",
                i.name
            ))),
        },
        IndexKind::Local => match cols.as_slice() {
            [r] => Ok((kind, table_hash.to_string(), Some(r.clone()))),
            [h, r] if h == table_hash => Ok((kind, h.clone(), Some(r.clone()))),
            _ => Err(Error::Query(format!(
                "El índice local {} lleva la clave de partición de la tabla ({table_hash}) y un atributo de ordenación.",
                i.name
            ))),
        },
    }
}

/// The JSON body (`CreateTable` input, minus the table name) of the
/// designed table; with `indexes`, its GSIs and LSIs inline.
pub(crate) fn create_table_spec(t: &TableSchema, indexes: bool) -> Result<Json> {
    let (hash, range) = table_keys(t)?;
    let throughput = provisioned(t)?;
    let mut key_attrs = vec![hash.clone()];
    key_attrs.extend(range.clone());
    let mut gsis = Vec::new();
    let mut lsis = Vec::new();
    if indexes {
        for i in &t.indexes {
            let (kind, h, r) = index_keys(i, &hash)?;
            let mut o = Map::new();
            o.insert("IndexName".into(), i.name.clone().into());
            o.insert("KeySchema".into(), key_schema(&h, r.as_deref()));
            o.insert("Projection".into(), projection(i));
            match kind {
                IndexKind::Global => {
                    if let Some(p) = &throughput {
                        o.insert("ProvisionedThroughput".into(), p.clone());
                    }
                    gsis.push(Json::Object(o));
                }
                IndexKind::Local => {
                    if range.is_none() {
                        return Err(Error::Query(format!(
                            "El índice local {} requiere que la tabla tenga clave de ordenación (RANGE).",
                            i.name
                        )));
                    }
                    lsis.push(Json::Object(o));
                }
            }
            for a in std::iter::once(h).chain(r) {
                if !key_attrs.contains(&a) {
                    key_attrs.push(a);
                }
            }
        }
    }
    let mut body = Map::new();
    body.insert("AttributeDefinitions".into(), attribute_definitions(t, &key_attrs)?);
    body.insert("KeySchema".into(), key_schema(&hash, range.as_deref()));
    match throughput {
        Some(p) => {
            body.insert("BillingMode".into(), "PROVISIONED".into());
            body.insert("ProvisionedThroughput".into(), p);
        }
        None => {
            body.insert("BillingMode".into(), "PAY_PER_REQUEST".into());
        }
    }
    if !gsis.is_empty() {
        body.insert("GlobalSecondaryIndexes".into(), gsis.into());
    }
    if !lsis.is_empty() {
        body.insert("LocalSecondaryIndexes".into(), lsis.into());
    }
    if let Some(v) = opt(t, STREAM_VIEW_TYPE) {
        body.insert("StreamSpecification".into(), json!({ "StreamEnabled": true, "StreamViewType": v }));
    }
    if let Some(a) = opt(t, TTL_ATTRIBUTE) {
        body.insert("TimeToLiveSpecification".into(), json!({ "AttributeName": a, "Enabled": true }));
    }
    Ok(Json::Object(body))
}

/// `AttributeDefinitions` for the key attributes (DynamoDB refuses
/// definitions of attributes that aren't keys).
fn attribute_definitions(t: &TableSchema, names: &[String]) -> Result<Json> {
    names
        .iter()
        .map(|n| {
            let col = t.columns.iter().find(|c| &c.name == n).ok_or_else(|| {
                Error::Query(format!("El atributo {n} es clave de la tabla o de un índice: agregalo a la lista con su tipo (S, N o B)."))
            })?;
            let ty = col.data_type.trim().to_ascii_uppercase();
            if !matches!(ty.as_str(), "S" | "N" | "B") {
                return Err(Error::Query(format!(
                    "El atributo {n} es clave: su tipo tiene que ser S (texto), N (número) o B (binario), no «{}».",
                    col.data_type
                )));
            }
            Ok(json!({ "AttributeName": n, "AttributeType": ty }))
        })
        .collect::<Result<Vec<_>>>()
        .map(Json::Array)
}

/// Option of an index that projects only its keys (`KEYS_ONLY`); an index
/// with `include` projects those attributes (`INCLUDE`); otherwise `ALL`.
pub(crate) const PROJECTION_TYPE: &str = "ProjectionType";

/// The `Projection` of an index.
pub(crate) fn projection(i: &IndexDef) -> Json {
    let attrs: Vec<&str> = i.include.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).collect();
    if !attrs.is_empty() {
        return json!({ "ProjectionType": "INCLUDE", "NonKeyAttributes": attrs });
    }
    match i.options.get(PROJECTION_TYPE).map(|t| t.trim().to_ascii_uppercase()) {
        Some(t) if t == "KEYS_ONLY" => json!({ "ProjectionType": "KEYS_ONLY" }),
        _ => json!({ "ProjectionType": "ALL" }),
    }
}

/// `CREATE INDEX` bodies for the GSIs (added to an existing table).
pub(crate) fn create_index_statements(t: &TableSchema) -> Result<Vec<String>> {
    let (hash, _) = table_keys(t)?;
    let throughput = provisioned(t)?;
    let mut out = Vec::new();
    for i in &t.indexes {
        let (kind, h, r) = index_keys(i, &hash)?;
        if matches!(kind, IndexKind::Local) {
            out.push(format!(
                "-- El índice local {} solo se puede crear junto con la tabla (CREATE TABLE).",
                i.name.replace('\n', " ")
            ));
            continue;
        }
        let mut names = vec![h.clone()];
        names.extend(r.clone());
        let mut o = Map::new();
        o.insert("AttributeDefinitions".into(), attribute_definitions(t, &names)?);
        o.insert("KeySchema".into(), key_schema(&h, r.as_deref()));
        o.insert("Projection".into(), projection(i));
        if let Some(p) = &throughput {
            o.insert("ProvisionedThroughput".into(), p.clone());
        }
        out.push(format!("CREATE INDEX {} ON {} {};", q(&i.name), q(&t.name), pretty(&Json::Object(o))));
    }
    Ok(out)
}

fn pretty(v: &Json) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

pub(crate) fn table_ddl(t: &TableSchema, parts: DdlParts) -> Result<String> {
    if t.name.trim().is_empty() {
        return Err(Error::Query("Falta el nombre de la tabla.".into()));
    }
    let mut stmts = Vec::new();
    if parts.drop {
        stmts.push(format!("DROP TABLE {}{};", if parts.if_exists { "IF EXISTS " } else { "" }, q(&t.name)));
    }
    if parts.create {
        let spec = create_table_spec(t, parts.indexes)?;
        stmts.push(format!(
            "CREATE TABLE {}{} {};",
            if parts.if_exists { "IF NOT EXISTS " } else { "" },
            q(&t.name),
            pretty(&spec)
        ));
    } else if parts.indexes {
        stmts.extend(create_index_statements(t)?);
    }
    let mut s = stmts.join("\n\n");
    if !s.is_empty() {
        s.push('\n');
    }
    Ok(s)
}

/// A value as a PartiQL literal: strings in single quotes, JSON arrays and
/// objects as lists and maps.
pub(crate) fn literal(v: &Json) -> String {
    match v {
        Json::Null => "NULL".into(),
        Json::Bool(b) => b.to_string(),
        Json::Number(n) => n.to_string(),
        Json::String(s) => format!("'{}'", s.replace('\'', "''")),
        Json::Array(a) => format!("[{}]", a.iter().map(literal).collect::<Vec<_>>().join(", ")),
        Json::Object(o) => format!(
            "{{{}}}",
            o.iter().map(|(k, v)| format!("'{}': {}", k.replace('\'', "''"), literal(v))).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// One `INSERT INTO "t" VALUE {…};` per row. Null cells are left out (in a
/// DynamoDB result a missing attribute shows as null).
pub(crate) fn insert_script(table: &str, columns: &[String], rows: &[Vec<Json>]) -> String {
    let mut out = String::new();
    for row in rows {
        let attrs: Vec<String> = columns
            .iter()
            .zip(row)
            .filter(|(_, v)| !v.is_null())
            .map(|(c, v)| format!("'{}': {}", c.replace('\'', "''"), literal(v)))
            .collect();
        if attrs.is_empty() {
            continue;
        }
        out.push_str(&format!("INSERT INTO {} VALUE {{{}}};\n", q(table), attrs.join(", ")));
    }
    out
}

/// One PartiQL `UPDATE "t" SET … WHERE <key> = …;` per edited item. A null
/// value becomes `REMOVE` (in a DynamoDB result a missing attribute shows
/// as null, and that's how the insert script writes it).
pub(crate) fn update_script(table: &str, changes: &[RowChange]) -> Result<String> {
    let mut out = String::new();
    for ch in changes.iter().filter(|c| !c.set.is_empty()) {
        if ch.key.is_empty() {
            return Err(Error::Unsupported("para actualizar un ítem de DynamoDB hace falta su clave primaria".into()));
        }
        let actions: Vec<String> = ch
            .set
            .iter()
            .map(|(c, v)| if v.is_null() { format!("REMOVE {}", q(c)) } else { format!("SET {} = {}", q(c), literal(v)) })
            .collect();
        let conds: Vec<String> = ch.key.iter().map(|(c, v)| format!("{} = {}", q(c), literal(v))).collect();
        out.push_str(&format!("UPDATE {} {} WHERE {};\n", q(table), actions.join(" "), conds.join(" AND ")));
    }
    Ok(out)
}

/// One PartiQL `DELETE FROM "t" WHERE <key> = …;` per item. DynamoDB's
/// PartiQL DELETE only takes the full primary key, so it removes one item.
pub(crate) fn delete_script(table: &str, keys: &[Vec<(String, Json)>]) -> Result<String> {
    let mut out = String::new();
    for key in keys {
        if key.is_empty() {
            return Err(Error::Unsupported("para borrar un ítem de DynamoDB hace falta su clave primaria".into()));
        }
        let conds: Vec<String> = key.iter().map(|(c, v)| format!("{} = {}", q(c), literal(v))).collect();
        out.push_str(&format!("DELETE FROM {} WHERE {};\n", q(table), conds.join(" AND ")));
    }
    Ok(out)
}

/// The browse query (`SELECT * FROM "t"`, or `"t"."index"`) restricted by
/// the grid's column filters, in DynamoDB's PartiQL: `[…]` lists,
/// `begins_with()` / `contains()` instead of LIKE (there's none, nor an
/// "ends with"), and a missing attribute counts as null.
pub(crate) fn filtered_browse(browse: &str, filters: &[ColumnFilter]) -> Result<String> {
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let style = SqlFilterStyle { quote: Quote::Double, literal: &literal, like: "LIKE", true_literal: "true", false_literal: "false" };
    let mut parts = Vec::new();
    for f in filters {
        let c = q(&f.column);
        let first = || f.values.first().ok_or_else(|| Error::Query(format!("el filtro de «{}» necesita un valor", f.column)));
        let text = || first().map(|v| literal(&Json::String(v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))));
        let list = || {
            if f.values.is_empty() {
                return Err(Error::Query(format!("el filtro de «{}» necesita al menos un valor", f.column)));
            }
            Ok(literal(&Json::Array(f.values.clone())))
        };
        parts.push(match f.op {
            FilterOp::Contains => format!("contains({c}, {})", text()?),
            FilterOp::NotContains => format!("NOT contains({c}, {})", text()?),
            FilterOp::StartsWith => format!("begins_with({c}, {})", text()?),
            FilterOp::EndsWith => return Err(Error::Unsupported("PartiQL de DynamoDB no filtra por «termina con»".into())),
            FilterOp::IsNull => format!("({c} IS NULL OR {c} IS MISSING)"),
            FilterOp::NotNull => format!("({c} IS NOT NULL AND {c} IS NOT MISSING)"),
            FilterOp::NotEmpty => format!("({c} IS NOT MISSING AND {c} <> '')"),
            FilterOp::In => format!("{c} IN {}", list()?),
            FilterOp::NotIn => format!("NOT ({c} IN {})", list()?),
            FilterOp::TrueOrNull => format!("({c} = true OR {c} IS NULL OR {c} IS MISSING)"),
            FilterOp::FalseOrNull => format!("({c} = false OR {c} IS NULL OR {c} IS MISSING)"),
            _ => sql_condition(std::slice::from_ref(f), &style)?,
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

/// Splits a script on `;` outside quotes, comments and the JSON bodies of
/// the extensions (where `\"` escapes a quote).
pub(crate) fn split_script(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    let mut quote: Option<char> = None;
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        if let Some(qc) = quote {
            cur.push(c);
            if c == '\\' && qc == '"' && depth > 0 {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            } else if c == qc {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            '{' | '[' => {
                depth += 1;
                cur.push(c);
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        cur.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                cur.push(' ');
            }
            ';' if depth == 0 => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_script_by_primary_key() {
        let keys = vec![
            vec![("pk".to_string(), Json::from("O'Brien \"Bob\"")), ("sk".to_string(), Json::from(3))],
            vec![("pk".to_string(), Json::from("b"))],
        ];
        assert_eq!(
            delete_script("my\"table", &keys).unwrap(),
            "DELETE FROM \"my\"\"table\" WHERE \"pk\" = 'O''Brien \"Bob\"' AND \"sk\" = 3;\n\
             DELETE FROM \"my\"\"table\" WHERE \"pk\" = 'b';\n"
        );
        assert!(delete_script("t", &[vec![]]).is_err());
    }

    #[test]
    fn filtered_browse_in_partiql() {
        let f = |column: &str, op: FilterOp, values: Vec<Json>| ColumnFilter { column: column.into(), op, values, sql: None };
        assert_eq!(
            filtered_browse(
                "SELECT * FROM \"Music\"",
                &[
                    f("Artist", FilterOp::Eq, vec![json!("O'Brien")]),
                    f("Year", FilterOp::Gt, vec![json!(1999)]),
                    f("Title", FilterOp::StartsWith, vec![json!("The ")]),
                    f("Notes", FilterOp::IsNull, vec![]),
                    f("Id", FilterOp::In, vec![json!(1), json!("a")]),
                ]
            )
            .unwrap(),
            "SELECT * FROM \"Music\"\nWHERE \"Artist\" = 'O''Brien'\n  AND \"Year\" > 1999\n  AND begins_with(\"Title\", 'The ')\n  AND (\"Notes\" IS NULL OR \"Notes\" IS MISSING)\n  AND \"Id\" IN [1, 'a']"
        );
        assert_eq!(
            filtered_browse("SELECT * FROM \"Music\".\"byYear\"", &[f("Year", FilterOp::Le, vec![json!(5)])]).unwrap(),
            "SELECT * FROM \"Music\".\"byYear\"\nWHERE \"Year\" <= 5"
        );
        assert!(matches!(filtered_browse("SELECT * FROM \"t\"", &[f("a", FilterOp::EndsWith, vec![json!("x")])]), Err(Error::Unsupported(_))));
    }
    use crate::admin::{parse_admin, Admin};
    use dbine_driver::KeyDef;
    use std::collections::BTreeMap;

    fn col(name: &str, ty: &str, key: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            options: BTreeMap::from([(KEY_TYPE.to_string(), key.to_string())]),
            ..Default::default()
        }
    }

    fn orders() -> TableSchema {
        TableSchema {
            name: "Orders".into(),
            columns: vec![col("customer", "S", "HASH"), col("date", "N", "RANGE"), col("status", "S", "none"), col("total", "N", "none")],
            indexes: vec![
                IndexDef { name: "byStatus".into(), columns: vec!["status".into(), "date".into()], kind: Some("GSI".into()), ..Default::default() },
                IndexDef { name: "byTotal".into(), columns: vec!["total".into()], kind: Some("LSI".into()), ..Default::default() },
            ],
            options: BTreeMap::from([
                (BILLING_MODE.to_string(), "PROVISIONED".to_string()),
                (READ_CAPACITY.to_string(), "3".to_string()),
                (TTL_ATTRIBUTE.to_string(), "exp".to_string()),
                (STREAM_VIEW_TYPE.to_string(), "NEW_IMAGE".to_string()),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn create_table_parses_back_with_the_drivers_own_syntax() {
        let ddl = table_ddl(&orders(), DdlParts { drop: true, if_exists: true, create: true, indexes: true, ..Default::default() }).unwrap();
        let stmts = split_script(&ddl);
        assert_eq!(stmts.len(), 2, "{ddl}");
        assert_eq!(parse_admin(&stmts[0]).unwrap(), Some(Admin::DropTable { table: "Orders".into(), if_exists: true }));
        let Some(Admin::CreateTable { table, if_not_exists, spec }) = parse_admin(&stmts[1]).unwrap() else { panic!("{ddl}") };
        assert_eq!(table, "Orders");
        assert!(if_not_exists);
        assert_eq!(spec["KeySchema"], json!([{ "AttributeName": "customer", "KeyType": "HASH" }, { "AttributeName": "date", "KeyType": "RANGE" }]));
        assert_eq!(spec["BillingMode"], "PROVISIONED");
        assert_eq!(spec["ProvisionedThroughput"], json!({ "ReadCapacityUnits": 3, "WriteCapacityUnits": 5 }));
        let defs: Vec<&str> = spec["AttributeDefinitions"].as_array().unwrap().iter().map(|d| d["AttributeName"].as_str().unwrap()).collect();
        assert_eq!(defs, ["customer", "date", "status", "total"]);
        assert_eq!(spec["GlobalSecondaryIndexes"][0]["IndexName"], "byStatus");
        assert_eq!(spec["GlobalSecondaryIndexes"][0]["ProvisionedThroughput"]["ReadCapacityUnits"], 3);
        assert_eq!(spec["LocalSecondaryIndexes"][0]["KeySchema"][0]["AttributeName"], "customer");
        assert_eq!(spec["LocalSecondaryIndexes"][0]["KeySchema"][1]["AttributeName"], "total");
        assert_eq!(spec["StreamSpecification"]["StreamViewType"], "NEW_IMAGE");
        assert_eq!(spec["TimeToLiveSpecification"]["AttributeName"], "exp");
        assert_eq!(spec["GlobalSecondaryIndexes"][0]["Projection"], json!({ "ProjectionType": "ALL" }));
        // The body maps onto the SDK input.
        crate::admin::table_spec(&spec).unwrap();

        // INCLUDE (the index's `include`) and KEYS_ONLY projections.
        let mut t = orders();
        t.indexes[0].include = vec!["total".into()];
        t.indexes[1].options.insert(PROJECTION_TYPE.into(), "KEYS_ONLY".into());
        let ddl = table_ddl(&t, DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
        let Some(Admin::CreateTable { spec, .. }) = parse_admin(split_script(&ddl).last().unwrap()).unwrap() else { panic!("{ddl}") };
        assert_eq!(spec["GlobalSecondaryIndexes"][0]["Projection"], json!({ "ProjectionType": "INCLUDE", "NonKeyAttributes": ["total"] }));
        assert_eq!(spec["LocalSecondaryIndexes"][0]["Projection"], json!({ "ProjectionType": "KEYS_ONLY" }));
        crate::admin::table_spec(&spec).unwrap();
        let gsi = create_index_statements(&t).unwrap();
        assert!(gsi[0].contains("\"NonKeyAttributes\""), "{}", gsi[0]);
    }

    #[test]
    fn keys_from_the_primary_key_and_errors() {
        let t = TableSchema {
            name: "t".into(),
            columns: vec![ColumnDef { name: "id".into(), data_type: "s".into(), ..Default::default() }],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            ..Default::default()
        };
        let spec = create_table_spec(&t, true).unwrap();
        assert_eq!(spec["AttributeDefinitions"], json!([{ "AttributeName": "id", "AttributeType": "S" }]));
        assert_eq!(spec["BillingMode"], "PAY_PER_REQUEST");
        assert!(spec.get("StreamSpecification").is_none());

        let mut bad = t.clone();
        bad.primary_key = None;
        assert!(create_table_spec(&bad, true).is_err());
        let mut bad = t.clone();
        bad.columns[0].data_type = "M".into();
        assert!(create_table_spec(&bad, true).unwrap_err().to_string().contains("S (texto)"));
        let mut bad = t;
        bad.indexes.push(IndexDef { name: "l".into(), columns: vec!["x".into()], kind: Some("LSI".into()), ..Default::default() });
        assert!(create_table_spec(&bad, true).is_err());
    }

    #[test]
    fn indexes_alone_become_create_index() {
        let ddl = table_ddl(&orders(), DdlParts { indexes: true, ..Default::default() }).unwrap();
        let stmts = split_script(&ddl);
        assert_eq!(stmts.len(), 1, "{ddl}");
        let Some(Admin::CreateIndex { index, table, spec }) = parse_admin(&stmts[0]).unwrap() else { panic!("{ddl}") };
        assert_eq!((index.as_str(), table.as_str()), ("byStatus", "Orders"));
        assert_eq!(spec["AttributeDefinitions"].as_array().unwrap().len(), 2);
        assert!(ddl.contains("-- El índice local byTotal"));
    }

    #[test]
    fn inserts_are_partiql_literals() {
        let cols = vec!["id".to_string(), "name".into(), "n".into(), "ok".into(), "tags".into(), "gone".into()];
        let rows = vec![vec![json!("a'1"), json!("O'Brien"), json!(1.5), json!(true), json!({ "k": [1, null] }), Json::Null]];
        let s = insert_script("T\"x", &cols, &rows);
        assert_eq!(
            s,
            "INSERT INTO \"T\"\"x\" VALUE {'id': 'a''1', 'name': 'O''Brien', 'n': 1.5, 'ok': true, 'tags': {'k': [1, NULL]}};\n"
        );
        assert_eq!(split_script(&s).len(), 1);
        assert_eq!(parse_admin(&split_script(&s)[0]).unwrap(), None);
    }

    #[test]
    fn updates_are_partiql() {
        let changes = vec![
            RowChange {
                key: vec![("id".into(), json!("a'1")), ("n".into(), json!(2))],
                set: vec![("name".into(), json!("O'Brien \"Bob\"")), ("gone".into(), Json::Null)], ..Default::default()
            },
            RowChange { key: vec![("id".into(), json!("b"))], set: vec![], ..Default::default() },
        ];
        let s = update_script("T\"x", &changes).unwrap();
        assert_eq!(s, "UPDATE \"T\"\"x\" SET \"name\" = 'O''Brien \"Bob\"' REMOVE \"gone\" WHERE \"id\" = 'a''1' AND \"n\" = 2;\n");
        assert_eq!(split_script(&s).len(), 1);
        assert_eq!(parse_admin(&split_script(&s)[0]).unwrap(), None);
    }

    #[test]
    fn splitter_keeps_json_bodies_whole() {
        let s = split_script("CREATE TABLE \"a\" {\n \"x\": \"semi; \\\" quote; it's\",\n \"y\": [1; 2]\n};\nSELECT * FROM \"a\" -- c;\n; DROP TABLE a");
        assert_eq!(s.len(), 3, "{s:?}");
        assert!(s[0].ends_with('}'));
        assert_eq!(s[1], "SELECT * FROM \"a\"");
        assert_eq!(s[2], "DROP TABLE a");
    }

    #[test]
    fn templates_parse() {
        for t in create_templates() {
            let text = t.template.replace("{name}", "x").replace("{schema}", "");
            let stmts = split_script(&text);
            assert_eq!(stmts.len(), 1);
            let a = parse_admin(&stmts[0]).unwrap().expect("an extension statement");
            if let Admin::CreateTable { spec, .. } = a {
                crate::admin::table_spec(&spec).unwrap();
            }
        }
    }
}
