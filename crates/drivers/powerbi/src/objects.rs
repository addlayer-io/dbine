//! The explorer's objects, their columns and definitions, the ER diagram's
//! schema, row estimates and comments (spec §1 object kinds, §4.6). Owned
//! by part C. Everything comes from one metadata read ([`crate::meta`]).
//!
//! - `list_objects`: tables (no auto date/time, no calculation groups),
//!   calculation groups, measures (`parent` = home table), hierarchies
//!   (`'Table'[Hierarchy]`), relationships (`'Sales'[CustomerKey] →
//!   'Customer'[CustomerKey]`), UDFs, roles, perspectives, expressions.
//!   Hidden objects included. Kinds the tier can't read come back empty.
//! - `columns`: `data_type` = the model type ([`crate::meta::DataType::name`])
//!   plus " (calculada)" for calculated columns; `nullable` from IsNullable;
//!   `primary_key` = IsKey, else the one side of a relationship; a
//!   hierarchy's columns are its levels. No Min/Max anywhere (S4).
//! - `definition`: table → the calculated table's DAX or the partition
//!   source (embedded data replaced); measure → `MEASURE 'T'[M] = …`;
//!   calculation group → items and expressions; relationship → cardinality,
//!   direction, active, security filtering; role → filters and column
//!   permissions; `None` where the tier has no text.
//! - `database_schema`: tables with columns, descriptions as comments,
//!   primary keys as `columns`, foreign keys from relationships (many →
//!   one; inactive ones too, with a warning), no indexes or checks.
//! - `row_estimates`: `INFO.STORAGETABLES` or the `RowNumber` cardinality.
//! - `object_comments`: descriptions of measures, hierarchies, calculation
//!   groups, roles, perspectives and UDFs.

use crate::dax::quote::{column_ref, quote_column, quote_table};
use crate::meta::service::{session_model, Service};
use crate::meta::{
    hierarchy_name, is_guid_like, is_one, relationship_name, same, ModelMeta, Relationship,
    TableKind,
};
use crate::{
    Ctx, CALCULATION_GROUP, EXPRESSION, FUNCTION, HIERARCHY, MEASURE, PERSPECTIVE, RELATIONSHIP,
    ROLE, TABLE,
};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{
    ColumnDef, ColumnInfo, DbObject, ForeignKeyDef, KeyDef, ObjectRef, Result, TableSchema,
};
use std::sync::Arc;

async fn meta<S: Service>(s: &mut S) -> Result<Arc<ModelMeta>> {
    let m = session_model(s)?;
    crate::meta::model_for(s, &m).await
}

pub async fn list_objects(cx: &mut Ctx) -> Result<Vec<DbObject>> {
    Ok(objects_of(&*meta(cx).await?))
}

pub async fn columns(cx: &mut Ctx, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
    Ok(columns_of(&*meta(cx).await?, obj))
}

pub async fn definition(cx: &mut Ctx, obj: &ObjectRef) -> Result<Option<String>> {
    Ok(definition_of(&*meta(cx).await?, obj))
}

pub async fn database_schema(cx: &mut Ctx) -> Result<Vec<TableSchema>> {
    Ok(schema_of(&*meta(cx).await?))
}

pub async fn row_estimates(cx: &mut Ctx) -> Result<Vec<RowEstimate>> {
    Ok(estimates_of(&*meta(cx).await?))
}

pub async fn object_comments(cx: &mut Ctx) -> Result<Vec<ObjectComment>> {
    Ok(comments_of(&*meta(cx).await?))
}

fn obj(kind: &str, name: &str, parent: Option<&str>) -> DbObject {
    DbObject {
        kind: kind.into(),
        schema: None,
        name: name.into(),
        parent: parent.map(str::to_string),
    }
}

fn oref(kind: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind: kind.into(),
        schema: None,
        name: name.into(),
    }
}

/// The explorer's list, in its kinds' order.
pub(crate) fn objects_of(meta: &ModelMeta) -> Vec<DbObject> {
    let mut out = Vec::new();
    out.extend(meta.plain_tables().map(|t| obj(TABLE, &t.name, None)));
    out.extend(
        meta.calculation_groups
            .iter()
            .map(|g| obj(CALCULATION_GROUP, &g.table, None)),
    );
    out.extend(
        meta.measures
            .iter()
            .map(|m| obj(MEASURE, &m.name, Some(&m.table))),
    );
    out.extend(
        meta.hierarchies
            .iter()
            .map(|h| obj(HIERARCHY, &hierarchy_name(h), None)),
    );
    out.extend(
        meta.model_relationships()
            .map(|r| obj(RELATIONSHIP, &relationship_name(r), None)),
    );
    out.extend(meta.functions.iter().map(|f| obj(FUNCTION, &f.name, None)));
    out.extend(meta.roles.iter().map(|r| obj(ROLE, &r.name, None)));
    out.extend(
        meta.perspectives
            .iter()
            .map(|p| obj(PERSPECTIVE, &p.name, None)),
    );
    out.extend(
        meta.expressions
            .iter()
            .map(|e| obj(EXPRESSION, &e.name, None)),
    );
    out
}

/// A hierarchy by its explorer name (`'Table'[Hierarchy]`).
pub(crate) fn hierarchy<'a>(meta: &'a ModelMeta, name: &str) -> Option<&'a crate::meta::Hierarchy> {
    meta.hierarchies
        .iter()
        .find(|h| hierarchy_name(h) == name || same(&h.name, name))
}

pub(crate) fn relationship<'a>(meta: &'a ModelMeta, name: &str) -> Option<&'a Relationship> {
    meta.model_relationships()
        .find(|r| relationship_name(r) == name || r.name.as_deref() == Some(name))
}

/// A column's type as `ColumnInfo` carries it.
fn type_name(c: &crate::meta::Column) -> String {
    let t = c.data_type.name();
    if c.expression.is_some() {
        format!("{t} (calculada)")
    } else {
        t.to_string()
    }
}

pub(crate) fn columns_of(meta: &ModelMeta, o: &ObjectRef) -> Vec<ColumnInfo> {
    match o.kind.as_str() {
        TABLE | CALCULATION_GROUP => {
            let Some(t) = meta.table(&o.name) else {
                return Vec::new();
            };
            let keys = meta.key_columns(&t.name);
            t.columns
                .iter()
                .map(|c| ColumnInfo {
                    name: c.name.clone(),
                    data_type: type_name(c),
                    nullable: c.nullable,
                    primary_key: keys.iter().any(|k| same(k, &c.name)),
                    auto_increment: false,
                    default_value: None,
                })
                .collect()
        }
        HIERARCHY => {
            let Some(h) = hierarchy(meta, &o.name) else {
                return Vec::new();
            };
            let t = meta.table(&h.table);
            h.levels
                .iter()
                .map(|l| {
                    let col = t.and_then(|t| t.columns.iter().find(|c| same(&c.name, &l.column)));
                    ColumnInfo {
                        name: l.name.clone(),
                        data_type: col.map(type_name).unwrap_or_else(|| "Unknown".into()),
                        nullable: col.is_none_or(|c| c.nullable),
                        primary_key: false,
                        auto_increment: false,
                        default_value: None,
                    }
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Comment lines of a description.
fn described(out: &mut String, description: Option<&str>) {
    if let Some(d) = description {
        for l in d.lines() {
            out.push_str(&format!("// {l}\n"));
        }
    }
}

/// `= expr` on the same line when it's one line, else on the next ones,
/// indented.
fn assigned(head: &str, expr: &str) -> String {
    let expr = expr.trim_matches('\n');
    if expr.contains('\n') {
        let body: Vec<String> = expr.lines().map(|l| format!("    {l}")).collect();
        format!("{head} =\n{}", body.join("\n"))
    } else {
        format!("{head} = {}", expr.trim())
    }
}

fn spanish_mode(mode: &str) -> String {
    match mode {
        "import" => "importación".into(),
        "directQuery" => "DirectQuery".into(),
        "directLake" => "Direct Lake".into(),
        "dual" => "dual".into(),
        "push" => "inserción".into(),
        other => other.into(),
    }
}

pub(crate) fn cardinality_text(r: &Relationship) -> &'static str {
    match (
        is_one(r.from_cardinality.as_deref(), false),
        is_one(r.to_cardinality.as_deref(), true),
    ) {
        (false, true) => "muchos a uno",
        (true, true) => "uno a uno",
        (false, false) => "muchos a muchos",
        (true, false) => "uno a muchos",
    }
}

pub(crate) fn both_directions(f: Option<&str>) -> bool {
    f.is_some_and(|f| f.eq_ignore_ascii_case("bothDirections"))
}

fn direction_text(f: Option<&str>) -> &'static str {
    match f {
        Some(f) if f.eq_ignore_ascii_case("bothDirections") => "ambas direcciones",
        Some(f) if f.eq_ignore_ascii_case("automatic") => "automático",
        _ => "una dirección",
    }
}

fn permission_text(p: &str) -> String {
    match p {
        "read" => "lectura".into(),
        "readRefresh" => "lectura y actualización".into(),
        "refresh" => "actualización".into(),
        "administrator" => "administrador".into(),
        "none" => "ninguno".into(),
        other => other.into(),
    }
}

/// An object's source, as the definition tab and the code search read it.
pub(crate) fn definition_of(meta: &ModelMeta, o: &ObjectRef) -> Option<String> {
    let mut out = String::new();
    match o.kind.as_str() {
        TABLE => {
            let t = meta.table(&o.name)?;
            if let Some(e) = &t.expression {
                out.push_str(&assigned(&format!("TABLE {}", quote_table(&t.name)), e));
                out.push('\n');
            }
            for c in &t.columns {
                if let Some(e) = &c.expression {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    described(&mut out, c.description.as_deref());
                    out.push_str(&assigned(
                        &format!("COLUMN {}", column_ref(&t.name, &c.name)),
                        e,
                    ));
                    out.push('\n');
                }
            }
            if t.kind != TableKind::Calculated {
                for p in t.partitions.iter().filter(|p| p.expression.is_some()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    let kind = match p.source_kind.as_deref() {
                        Some("m") | None => "M".to_string(),
                        Some(k) => k.to_string(),
                    };
                    let mode = p
                        .mode
                        .as_deref()
                        .map(|m| format!(" · {}", spanish_mode(m)))
                        .unwrap_or_default();
                    out.push_str(&format!("// Partición «{}» ({kind}{mode})\n", p.name));
                    out.push_str(p.expression.as_deref().unwrap_or(""));
                    out.push('\n');
                }
            }
            if !out.is_empty() && t.description.is_some() {
                let mut head = String::new();
                described(&mut head, t.description.as_deref());
                out.insert_str(0, &head);
            }
        }
        CALCULATION_GROUP => {
            let g = meta
                .calculation_groups
                .iter()
                .find(|g| same(&g.table, &o.name))?;
            described(&mut out, g.description.as_deref());
            let precedence = g
                .precedence
                .map(|p| format!(" · precedencia {p}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "// Grupo de cálculo {}{precedence}\n",
                quote_table(&g.table)
            ));
            for i in &g.items {
                let ordinal = i
                    .ordinal
                    .map(|o| format!(" (orden {o})"))
                    .unwrap_or_default();
                out.push_str(&format!("\n// {}{ordinal}\n", quote_column(&i.name)));
                match &i.expression {
                    Some(e) => out.push_str(e.trim_matches('\n')),
                    None => out.push_str("// (sin permiso para ver la expresión)"),
                }
                out.push('\n');
                if let Some(f) = &i.format_string_expression {
                    out.push_str(&format!("// Formato: {f}\n"));
                }
            }
        }
        MEASURE => {
            let m = meta.measure(&o.name)?;
            let e = m.expression.as_deref()?;
            described(&mut out, m.description.as_deref());
            out.push_str(&assigned(
                &format!("MEASURE {}", column_ref(&m.table, &m.name)),
                e,
            ));
            out.push('\n');
        }
        HIERARCHY => {
            let h = hierarchy(meta, &o.name)?;
            described(&mut out, h.description.as_deref());
            out.push_str(&format!("// Jerarquía {}\n", hierarchy_name(h)));
            for (i, l) in h.levels.iter().enumerate() {
                out.push_str(&format!(
                    "// {}. {} → {}\n",
                    i + 1,
                    l.name,
                    column_ref(&h.table, &l.column)
                ));
            }
        }
        RELATIONSHIP => {
            let r = relationship(meta, &o.name)?;
            out.push_str(&format!("// Relación {}\n", relationship_name(r)));
            out.push_str(&format!("// Cardinalidad: {}\n", cardinality_text(r)));
            out.push_str(&format!(
                "// Filtro cruzado: {}\n",
                direction_text(r.cross_filtering.as_deref())
            ));
            out.push_str(&format!(
                "// Activa: {}\n",
                if r.active { "sí" } else { "no" }
            ));
            if let Some(sf) = &r.security_filtering {
                out.push_str(&format!(
                    "// Filtro de seguridad: {}\n",
                    direction_text(Some(sf))
                ));
            }
            if !r.active {
                out.push_str(&format!(
                    "// Para usarla en un cálculo:\n// USERELATIONSHIP({}, {})\n",
                    column_ref(&r.from_table, &r.from_column),
                    column_ref(&r.to_table, &r.to_column)
                ));
            }
        }
        FUNCTION => {
            let f = meta.functions.iter().find(|f| same(&f.name, &o.name))?;
            let e = f.expression.as_deref()?;
            described(&mut out, f.description.as_deref());
            out.push_str(&assigned(&format!("FUNCTION {}", f.name), e));
            out.push('\n');
        }
        ROLE => {
            let r = meta.roles.iter().find(|r| same(&r.name, &o.name))?;
            described(&mut out, r.description.as_deref());
            let perm = r
                .model_permission
                .as_deref()
                .map(|p| format!(" · permiso: {}", permission_text(p)))
                .unwrap_or_default();
            out.push_str(&format!("// Rol «{}»{perm}\n", r.name));
            for (t, f) in &r.table_filters {
                out.push_str(&format!(
                    "\n// Filtro de {}\n{}\n",
                    quote_table(t),
                    f.trim()
                ));
            }
            for (t, c, p) in &r.column_permissions {
                let what = if p.eq_ignore_ascii_case("none") {
                    "oculta".to_string()
                } else {
                    permission_text(p)
                };
                out.push_str(&format!("// Columna {}: {what}\n", column_ref(t, c)));
            }
            if !r.members.is_empty() {
                out.push_str(&format!("// Miembros: {}\n", r.members.len()));
            }
        }
        PERSPECTIVE => {
            let p = meta.perspectives.iter().find(|p| same(&p.name, &o.name))?;
            described(&mut out, p.description.as_deref());
            out.push_str(&format!("// Perspectiva «{}»\n", p.name));
            for t in &p.tables {
                out.push_str(&format!("// Tabla {}\n", quote_table(t)));
                for (_, c) in p.columns.iter().filter(|(pt, _)| same(pt, t)) {
                    out.push_str(&format!("//   columna {}\n", column_ref(t, c)));
                }
                for (_, m) in p.measures.iter().filter(|(pt, _)| same(pt, t)) {
                    out.push_str(&format!("//   medida {}\n", quote_column(m)));
                }
            }
        }
        EXPRESSION => {
            let e = meta.expressions.iter().find(|e| same(&e.name, &o.name))?;
            let x = e.expression.as_deref()?;
            described(&mut out, e.description.as_deref());
            let kind = if e.kind.as_deref() == Some("parameter") {
                "Parámetro"
            } else {
                "Expresión M"
            };
            out.push_str(&format!(
                "// {kind} «{}»\n{}\n",
                e.name,
                x.trim_matches('\n')
            ));
        }
        _ => return None,
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// The ER diagram's and the compare's view of the model.
pub(crate) fn schema_of(meta: &ModelMeta) -> Vec<TableSchema> {
    meta.tables
        .iter()
        .filter(|t| t.kind != TableKind::AutoDate)
        .map(|t| {
            let keys = meta.key_columns(&t.name);
            let mut warnings = Vec::new();
            let foreign_keys = meta
                .model_relationships()
                .filter(|r| same(&r.from_table, &t.name))
                .map(|r| {
                    let mut notes = Vec::new();
                    if !r.active {
                        notes.push("está inactiva: solo filtra con USERELATIONSHIP");
                    }
                    if !is_one(r.to_cardinality.as_deref(), true) {
                        notes.push("es de muchos a muchos");
                    }
                    if both_directions(r.cross_filtering.as_deref()) {
                        notes.push("filtra en ambas direcciones");
                    }
                    if !notes.is_empty() {
                        warnings.push(format!(
                            "La relación {} {}.",
                            relationship_name(r),
                            notes.join(" y ")
                        ));
                    }
                    ForeignKeyDef {
                        name: r.name.clone().filter(|n| !is_guid_like(n)),
                        columns: vec![r.from_column.clone()],
                        ref_schema: None,
                        ref_table: r.to_table.clone(),
                        ref_columns: vec![r.to_column.clone()],
                        on_delete: None,
                        on_update: None,
                    }
                })
                .collect();
            TableSchema {
                kind: TABLE.into(),
                schema: None,
                name: t.name.clone(),
                columns: t
                    .columns
                    .iter()
                    .map(|c| ColumnDef {
                        name: c.name.clone(),
                        data_type: type_name(c),
                        nullable: c.nullable,
                        default_value: None,
                        auto_increment: false,
                        comment: c.description.clone(),
                        options: Default::default(),
                    })
                    .collect(),
                primary_key: (!keys.is_empty()).then_some(KeyDef {
                    name: None,
                    columns: keys,
                }),
                foreign_keys,
                indexes: Vec::new(),
                checks: Vec::new(),
                comment: t.description.clone(),
                options: Default::default(),
                warnings,
            }
        })
        .collect()
}

pub(crate) fn estimates_of(meta: &ModelMeta) -> Vec<RowEstimate> {
    meta.plain_tables()
        .filter_map(|t| {
            Some(RowEstimate {
                object: oref(TABLE, &t.name),
                rows: t.rows?,
            })
        })
        .collect()
}

pub(crate) fn comments_of(meta: &ModelMeta) -> Vec<ObjectComment> {
    let mut out = Vec::new();
    let mut add = |kind: &str, name: String, d: Option<&String>| {
        if let Some(d) = d.filter(|d| !d.trim().is_empty()) {
            out.push(ObjectComment {
                object: oref(kind, &name),
                comment: d.clone(),
            });
        }
    };
    for t in meta.plain_tables() {
        add(TABLE, t.name.clone(), t.description.as_ref());
    }
    for m in &meta.measures {
        add(MEASURE, m.name.clone(), m.description.as_ref());
    }
    for h in &meta.hierarchies {
        add(HIERARCHY, hierarchy_name(h), h.description.as_ref());
    }
    for g in &meta.calculation_groups {
        add(CALCULATION_GROUP, g.table.clone(), g.description.as_ref());
    }
    for r in &meta.roles {
        add(ROLE, r.name.clone(), r.description.as_ref());
    }
    for p in &meta.perspectives {
        add(PERSPECTIVE, p.name.clone(), p.description.as_ref());
    }
    for f in &meta.functions {
        add(FUNCTION, f.name.clone(), f.description.as_ref());
    }
    for e in &meta.expressions {
        add(EXPRESSION, e.name.clone(), e.description.as_ref());
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::definition::tmsl;
    use crate::meta::fake::Fake;

    pub(crate) fn sample() -> ModelMeta {
        tmsl::parse(&serde_json::from_str(tmsl::tests::SAMPLE).unwrap()).unwrap()
    }

    fn r(kind: &str, name: &str) -> ObjectRef {
        oref(kind, name)
    }

    #[test]
    fn the_explorer_lists_every_kind() {
        let objects = objects_of(&sample());
        let names: Vec<(&str, &str, Option<&str>)> = objects
            .iter()
            .map(|o| (o.kind.as_str(), o.name.as_str(), o.parent.as_deref()))
            .collect();
        assert_eq!(
            names,
            [
                (TABLE, "Sales", None),
                (TABLE, "Customer", None),
                (TABLE, "Date", None),
                (CALCULATION_GROUP, "Time Intelligence", None),
                (MEASURE, "Total Sales", Some("Sales")),
                (MEASURE, "Sales YoY", Some("Sales")),
                (HIERARCHY, "'Customer'[Geography]", None),
                (
                    RELATIONSHIP,
                    "'Sales'[CustomerKey] → 'Customer'[CustomerKey]",
                    None
                ),
                (RELATIONSHIP, "'Sales'[Order Date] → 'Date'[Date]", None),
                (FUNCTION, "AddTax", None),
                (ROLE, "West", None),
                (PERSPECTIVE, "Comercial", None),
                (EXPRESSION, "Servidor", None),
                (EXPRESSION, "Origen web", None),
            ]
        );
    }

    #[test]
    fn columns_carry_types_and_keys() {
        let meta = sample();
        let c = columns_of(&meta, &r(TABLE, "Sales"));
        assert_eq!(c[2].data_type, "Decimal");
        assert!(!c[2].nullable);
        assert_eq!(c[3].data_type, "Double (calculada)");
        // Customer: IsKey.
        let c = columns_of(&meta, &r(TABLE, "Customer"));
        assert!(c[0].primary_key && !c[1].primary_key);
        // Date: no IsKey, but relationships point at [Date].
        let c = columns_of(&meta, &r(TABLE, "Date"));
        assert!(c[0].primary_key);
        // A hierarchy's columns are its levels.
        let h = columns_of(&meta, &r(HIERARCHY, "'Customer'[Geography]"));
        assert_eq!(
            h.iter()
                .map(|c| (c.name.as_str(), c.data_type.as_str()))
                .collect::<Vec<_>>(),
            [("Cliente", "Int64"), ("Ciudad", "String")]
        );
        assert!(columns_of(&meta, &r(MEASURE, "Total Sales")).is_empty());
    }

    #[test]
    fn definitions_paste_into_a_define() {
        let meta = sample();
        assert_eq!(
            definition_of(&meta, &r(MEASURE, "Total Sales")).unwrap(),
            "// Suma de importes\nMEASURE 'Sales'[Total Sales] = SUM ( Sales[Amount] )\n"
        );
        let yoy = definition_of(&meta, &r(MEASURE, "Sales YoY")).unwrap();
        assert!(
            yoy.starts_with("MEASURE 'Sales'[Sales YoY] =\n    VAR prev"),
            "{yoy}"
        );
        let date = definition_of(&meta, &r(TABLE, "Date")).unwrap();
        assert_eq!(
            date,
            "TABLE 'Date' = CALENDAR ( DATE ( 2020, 1, 1 ), DATE ( 2026, 12, 31 ) )\n"
        );
        let sales = definition_of(&meta, &r(TABLE, "Sales")).unwrap();
        assert!(sales.contains("COLUMN 'Sales'[Margin] = [Amount] - RELATED('Customer'[Cost])"));
        assert!(sales.contains("// Partición «Sales-8a1f» (M · importación)\nlet"));
        let customer = definition_of(&meta, &r(TABLE, "Customer")).unwrap();
        assert!(!customer.contains("i45W"));
        let g = definition_of(&meta, &r(CALCULATION_GROUP, "Time Intelligence")).unwrap();
        assert!(g.contains(
            "// [YTD] (orden 1)\nCALCULATE ( SELECTEDMEASURE (), DATESYTD ( 'Date'[Date] ) )"
        ));
        let rel = definition_of(
            &meta,
            &r(RELATIONSHIP, "'Sales'[Order Date] → 'Date'[Date]"),
        )
        .unwrap();
        assert!(
            rel.contains("Activa: no")
                && rel.contains("USERELATIONSHIP('Sales'[Order Date], 'Date'[Date])")
        );
        assert!(rel.contains("ambas direcciones"));
        let role = definition_of(&meta, &r(ROLE, "West")).unwrap();
        assert!(role.contains("// Filtro de 'Customer'\n[City] = \"Seattle\""));
        assert!(role.contains("Columna 'Customer'[Cost]: oculta"));
        // Members are counted, not listed (the definition is cached).
        assert!(!role.contains("ana@contoso.com"));
        assert!(definition_of(&meta, &r(FUNCTION, "AddTax"))
            .unwrap()
            .contains("FUNCTION AddTax = ( amount : NUMERIC ) => amount * 1.21"));
        assert!(definition_of(&meta, &r(EXPRESSION, "Servidor"))
            .unwrap()
            .starts_with("// Parámetro «Servidor»"));
        assert!(definition_of(&meta, &r(PERSPECTIVE, "Comercial"))
            .unwrap()
            .contains("medida [Total Sales]"));
        assert!(definition_of(&meta, &r(TABLE, "Nope")).is_none());
    }

    #[test]
    fn the_schema_has_relationships_as_foreign_keys() {
        let meta = sample();
        // The sample has Desktop's auto date/time relationship too: hidden
        // like its table, everywhere.
        assert!(meta
            .relationships
            .iter()
            .any(|r| r.to_table.starts_with("LocalDateTable")));
        assert!(relationship(
            &meta,
            "'Sales'[Order Date] → 'LocalDateTable_0c0a3e44-8a7b-4c5b-9f43-3c0d9b8e2f11'[Date]"
        )
        .is_none());
        let schema = schema_of(&meta);
        assert!(schema.iter().all(|t| !t.name.starts_with("LocalDateTable")));
        assert!(schema
            .iter()
            .flat_map(|t| &t.foreign_keys)
            .all(|fk| !fk.ref_table.starts_with("LocalDateTable")));
        let sales = schema.iter().find(|t| t.name == "Sales").unwrap();
        assert_eq!(sales.comment.as_deref(), Some("Ventas por línea"));
        assert_eq!(sales.foreign_keys.len(), 2);
        assert_eq!(sales.foreign_keys[0].ref_table, "Customer");
        assert!(
            sales.foreign_keys[0].name.is_none(),
            "GUID names are left out"
        );
        assert!(sales.warnings[0].contains("inactiva"));
        let customer = schema.iter().find(|t| t.name == "Customer").unwrap();
        assert_eq!(
            customer.primary_key.as_ref().unwrap().columns,
            ["CustomerKey"]
        );
        assert_eq!(customer.columns[1].data_type, "String");
    }

    #[test]
    fn comments_and_estimates() {
        let mut meta = sample();
        meta.tables[0].rows = Some(60398);
        let e = estimates_of(&meta);
        assert_eq!((e[0].object.name.as_str(), e[0].rows), ("Sales", 60398));
        assert_eq!(e.len(), 1);
        let c = comments_of(&meta);
        let names: Vec<(&str, &str)> = c
            .iter()
            .map(|c| (c.object.kind.as_str(), c.object.name.as_str()))
            .collect();
        assert!(names.contains(&(MEASURE, "Total Sales")));
        assert!(names.contains(&(HIERARCHY, "'Customer'[Geography]")));
        assert!(names.contains(&(ROLE, "West")));
        assert!(names.contains(&(FUNCTION, "AddTax")));
    }

    #[tokio::test]
    async fn a_session_without_a_model_says_to_pick_one() {
        let mut f = Fake::new();
        f.model = None;
        let e = meta(&mut f).await.unwrap_err();
        assert!(e.to_string().contains("Elegí un modelo"));
    }
}
