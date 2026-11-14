//! `model.bim` (TMSL JSON) into [`ModelMeta`]. Owned by part C.
//!
//! Tables, columns (`dataType`, `isHidden`, `isKey`, `isNullable`,
//! `expression`, `description`), measures, hierarchies and levels,
//! partitions (M source, embedded data replaced), calculation groups and
//! items, relationships, roles (`tablePermissions`, `columnPermissions`,
//! `members`), perspectives, expressions, functions. An `expression` may be
//! a string or an array of lines. Never panics on any input (fuzzed, S7).

use crate::meta::{
    is_auto_date, normalize_relationship, redact_m, CalculationGroup, CalculationItem, Column,
    DataType, Function, Hierarchy, Level, Measure, ModelMeta, NamedExpression, Partition,
    Perspective, Relationship, Role, Table, TableKind, Tier,
};
use dbine_driver::{Error, Result};
use serde_json::Value;

/// A TMSL text property: a string, or an array of lines.
fn text(v: Option<&Value>) -> Option<String> {
    let s = match v? {
        Value::String(s) => s.clone(),
        Value::Array(lines) => lines
            .iter()
            .map(|l| l.as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
}

fn b(v: &Value, key: &str, default: bool) -> bool {
    match v.get(key) {
        Some(Value::Bool(x)) => *x,
        Some(Value::String(x)) => x.eq_ignore_ascii_case("true"),
        _ => default,
    }
}

fn list<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter(|x| x.is_object())
}

/// The trimmed DAX of an expression that TMSL writes with a leading empty
/// line (`["", "VAR …", …]`).
fn dax(v: Option<&Value>) -> Option<String> {
    text(v).map(|t| t.trim_matches('\n').to_string())
}

pub fn parse(model_bim: &serde_json::Value) -> Result<ModelMeta> {
    let model = match model_bim.get("model") {
        Some(m) if m.is_object() => m,
        _ if model_bim.get("tables").is_some() => model_bim,
        _ => {
            return Err(Error::Query(
                "La definición no es un modelo TMSL (falta «model»).".into(),
            ))
        }
    };
    let mut meta = ModelMeta {
        tier: Tier::Definition,
        compatibility_level: model_bim.get("compatibilityLevel").and_then(Value::as_i64),
        culture: s(model, "culture"),
        ..Default::default()
    };
    let default_mode = s(model, "defaultMode");
    for t in list(model, "tables") {
        let Some(name) = s(t, "name") else {
            continue;
        };
        let partitions: Vec<Partition> = list(t, "partitions")
            .map(|p| {
                let source = p.get("source").unwrap_or(&Value::Null);
                let kind = s(source, "type").unwrap_or_else(|| "m".into());
                let expression = text(source.get("expression")).map(|e| {
                    if kind == "calculated" {
                        e
                    } else {
                        redact_m(&e)
                    }
                });
                Partition {
                    name: s(p, "name").unwrap_or_default(),
                    source_kind: Some(kind),
                    expression,
                    mode: s(p, "mode").or_else(|| default_mode.clone()),
                }
            })
            .collect();
        let group = t.get("calculationGroup").filter(|g| g.is_object());
        let calculated = partitions
            .iter()
            .find(|p| p.source_kind.as_deref() == Some("calculated"));
        let kind = if is_auto_date(&name) {
            TableKind::AutoDate
        } else if group.is_some() {
            TableKind::CalculationGroup
        } else if calculated.is_some() {
            TableKind::Calculated
        } else {
            TableKind::Data
        };
        let expression = calculated.and_then(|p| p.expression.clone());
        let columns = list(t, "columns")
            .filter(|c| !s(c, "type").is_some_and(|k| k.eq_ignore_ascii_case("rowNumber")))
            .filter_map(|c| {
                let calc = s(c, "type").is_some_and(|k| k == "calculated");
                Some(Column {
                    name: s(c, "name")?,
                    data_type: s(c, "dataType")
                        .map(|d| DataType::parse(&d))
                        .unwrap_or_default(),
                    expression: if calc { dax(c.get("expression")) } else { None },
                    description: text(c.get("description")),
                    hidden: b(c, "isHidden", false),
                    key: b(c, "isKey", false),
                    unique: b(c, "isUnique", false),
                    nullable: b(c, "isNullable", true),
                    cardinality: None,
                    format_string: s(c, "formatString"),
                })
            })
            .collect();
        for m in list(t, "measures") {
            if let Some(mname) = s(m, "name") {
                meta.measures.push(Measure {
                    table: name.clone(),
                    name: mname,
                    expression: dax(m.get("expression")),
                    description: text(m.get("description")),
                    data_type: s(m, "dataType")
                        .map(|d| DataType::parse(&d))
                        .filter(|t| *t != DataType::Unknown),
                    format_string: s(m, "formatString"),
                    display_folder: s(m, "displayFolder"),
                    hidden: b(m, "isHidden", false),
                    state: None,
                });
            }
        }
        for h in list(t, "hierarchies") {
            let Some(hname) = s(h, "name") else {
                continue;
            };
            let mut levels: Vec<(i64, Level)> = list(h, "levels")
                .filter_map(|l| {
                    let column = s(l, "column")?;
                    Some((
                        l.get("ordinal").and_then(Value::as_i64).unwrap_or(0),
                        Level {
                            name: s(l, "name").unwrap_or_else(|| column.clone()),
                            column,
                        },
                    ))
                })
                .collect();
            levels.sort_by_key(|(o, _)| *o);
            meta.hierarchies.push(Hierarchy {
                table: name.clone(),
                name: hname,
                levels: levels.into_iter().map(|(_, l)| l).collect(),
                description: text(h.get("description")),
                hidden: b(h, "isHidden", false),
            });
        }
        if let Some(g) = group {
            let mut items: Vec<CalculationItem> = list(g, "calculationItems")
                .filter_map(|i| {
                    Some(CalculationItem {
                        name: s(i, "name")?,
                        expression: dax(i.get("expression")),
                        ordinal: i.get("ordinal").and_then(Value::as_i64),
                        format_string_expression: i
                            .get("formatStringDefinition")
                            .and_then(|f| dax(f.get("expression"))),
                    })
                })
                .collect();
            items.sort_by_key(|i| i.ordinal.unwrap_or(i64::MAX));
            meta.calculation_groups.push(CalculationGroup {
                table: name.clone(),
                precedence: g.get("precedence").and_then(Value::as_i64),
                items,
                description: text(t.get("description")).or_else(|| text(g.get("description"))),
            });
        }
        let storage_mode = partitions
            .iter()
            .find_map(|p| p.mode.clone())
            .or_else(|| default_mode.clone());
        meta.tables.push(Table {
            name,
            kind,
            description: text(t.get("description")),
            hidden: b(t, "isHidden", false),
            expression,
            columns,
            partitions,
            rows: None,
            storage_mode,
        });
    }
    for r in list(model, "relationships") {
        let (Some(ft), Some(fc), Some(tt), Some(tc)) = (
            s(r, "fromTable"),
            s(r, "fromColumn"),
            s(r, "toTable"),
            s(r, "toColumn"),
        ) else {
            continue;
        };
        meta.relationships
            .push(normalize_relationship(Relationship {
                name: s(r, "name"),
                from_table: ft,
                from_column: fc,
                to_table: tt,
                to_column: tc,
                from_cardinality: Some(s(r, "fromCardinality").unwrap_or_else(|| "many".into())),
                to_cardinality: Some(s(r, "toCardinality").unwrap_or_else(|| "one".into())),
                cross_filtering: Some(
                    s(r, "crossFilteringBehavior").unwrap_or_else(|| "oneDirection".into()),
                ),
                active: b(r, "isActive", true),
                security_filtering: Some(
                    s(r, "securityFilteringBehavior").unwrap_or_else(|| "oneDirection".into()),
                ),
            }));
    }
    for r in list(model, "roles") {
        let Some(name) = s(r, "name") else {
            continue;
        };
        let mut role = Role {
            name,
            description: text(r.get("description")),
            model_permission: s(r, "modelPermission"),
            members: list(r, "members")
                .filter_map(|m| s(m, "memberName"))
                .collect(),
            ..Default::default()
        };
        for tp in list(r, "tablePermissions") {
            let Some(table) = s(tp, "name") else {
                continue;
            };
            if let Some(f) = dax(tp.get("filterExpression")) {
                role.table_filters.push((table.clone(), f));
            }
            for cp in list(tp, "columnPermissions") {
                if let Some(col) = s(cp, "name") {
                    role.column_permissions.push((
                        table.clone(),
                        col,
                        s(cp, "metadataPermission").unwrap_or_else(|| "default".into()),
                    ));
                }
            }
        }
        meta.roles.push(role);
    }
    for p in list(model, "perspectives") {
        let Some(name) = s(p, "name") else {
            continue;
        };
        let mut out = Perspective {
            name,
            description: text(p.get("description")),
            ..Default::default()
        };
        for t in list(p, "tables") {
            let Some(table) = s(t, "name") else {
                continue;
            };
            out.columns
                .extend(list(t, "columns").filter_map(|c| Some((table.clone(), s(c, "name")?))));
            out.measures
                .extend(list(t, "measures").filter_map(|c| Some((table.clone(), s(c, "name")?))));
            out.tables.push(table);
        }
        meta.perspectives.push(out);
    }
    for e in list(model, "expressions") {
        let Some(name) = s(e, "name") else {
            continue;
        };
        let expression = text(e.get("expression"));
        let parameter = expression
            .as_deref()
            .is_some_and(|x| x.replace(' ', "").contains("IsParameterQuery=true"));
        meta.expressions.push(NamedExpression {
            name,
            kind: Some(if parameter {
                "parameter".into()
            } else {
                s(e, "kind").unwrap_or_else(|| "m".into())
            }),
            expression: expression.map(|x| redact_m(&x)),
            description: text(e.get("description")),
        });
    }
    for f in list(model, "functions") {
        if let Some(name) = s(f, "name") {
            meta.functions.push(Function {
                name,
                expression: dax(f.get("expression")),
                description: text(f.get("description")),
            });
        }
    }
    Ok(meta)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A `model.bim` shaped as the TMSL reference documents it.
    pub(crate) const SAMPLE: &str = include_str!("../../tests/fixtures/tmsl_model.json");

    #[test]
    fn reads_every_part_of_the_model() {
        let v: Value = serde_json::from_str(SAMPLE).unwrap();
        let meta = parse(&v).unwrap();
        assert_eq!(meta.tier, Tier::Definition);
        assert_eq!(meta.compatibility_level, Some(1601));
        assert_eq!(meta.culture.as_deref(), Some("es-AR"));
        let sales = meta.table("Sales").unwrap();
        assert_eq!(sales.kind, TableKind::Data);
        assert_eq!(sales.columns[2].data_type, DataType::Decimal);
        assert!(!sales.columns[2].nullable);
        assert_eq!(
            sales.columns[3].expression.as_deref(),
            Some("[Amount] - RELATED('Customer'[Cost])")
        );
        // An expression as an array of lines.
        let src = sales.partitions[0].expression.as_deref().unwrap();
        assert!(src.starts_with("let\n    Source = Sql.Database("), "{src}");
        assert_eq!(sales.storage_mode.as_deref(), Some("import"));
        let yoy = meta.measure("Sales YoY").unwrap();
        assert!(yoy.hidden);
        assert!(yoy.expression.as_deref().unwrap().starts_with("VAR prev"));
        // Embedded data is gone.
        let customer = meta.table("Customer").unwrap();
        assert!(!customer.partitions[0]
            .expression
            .as_deref()
            .unwrap()
            .contains("i45W"));
        assert!(customer.columns[0].key);
        assert_eq!(meta.table("Date").unwrap().kind, TableKind::Calculated);
        assert_eq!(
            meta.table("Date").unwrap().expression.as_deref(),
            Some("CALENDAR ( DATE ( 2020, 1, 1 ), DATE ( 2026, 12, 31 ) )")
        );
        assert_eq!(
            meta.tables[4].kind,
            TableKind::AutoDate,
            "{}",
            meta.tables[4].name
        );
        let g = &meta.calculation_groups[0];
        assert_eq!(
            (g.table.as_str(), g.precedence),
            ("Time Intelligence", Some(1))
        );
        assert_eq!(g.items[0].name, "Actual");
        assert_eq!(
            g.items[0].format_string_expression.as_deref(),
            Some("SELECTEDMEASUREFORMATSTRING ()")
        );
        let h = &meta.hierarchies[0];
        assert_eq!(h.levels[0].column, "CustomerKey");
        // Defaults: many → one, one direction, active.
        let r = &meta.relationships[0];
        assert_eq!(r.from_cardinality.as_deref(), Some("many"));
        assert!(r.active);
        let inactive = &meta.relationships[1];
        assert!(!inactive.active);
        assert_eq!(inactive.cross_filtering.as_deref(), Some("bothDirections"));
        let role = &meta.roles[0];
        assert_eq!(role.members, ["ana@contoso.com"]);
        assert_eq!(
            role.column_permissions,
            [("Customer".into(), "Cost".into(), "none".into())]
        );
        assert_eq!(
            meta.perspectives[0].measures,
            [("Sales".into(), "Total Sales".into())]
        );
        assert_eq!(meta.expressions[0].kind.as_deref(), Some("parameter"));
        let web = meta.expressions[1].expression.as_deref().unwrap();
        assert!(
            !web.contains("abc123") && web.contains("region=oeste"),
            "{web}"
        );
        assert_eq!(meta.functions[0].name, "AddTax");
    }

    #[test]
    fn any_shape_is_refused_or_read_without_panicking() {
        for v in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({"model": 3}),
            serde_json::json!("model"),
        ] {
            assert!(parse(&v).is_err(), "{v}");
        }
        for v in [
            serde_json::json!({"model": {}}),
            serde_json::json!({"model": {"tables": 3, "relationships": {}}}),
            serde_json::json!({"model": {"tables": [1, null, {"name": 3}, {"name": "T", "columns": [[]], "partitions": [{"source": 1}]}]}}),
            serde_json::json!({"model": {"roles": [{"name": "r", "tablePermissions": [{"columnPermissions": 1}]}]}}),
            serde_json::json!({"tables": [{"name": "T", "measures": [{"name": "m", "expression": [1, 2]}]}]}),
        ] {
            assert!(parse(&v).is_ok(), "{v}");
        }
    }
}
