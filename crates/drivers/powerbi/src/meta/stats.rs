//! Tier C: `EVALUATE COLUMNSTATISTICS()` (spec §4.3), which `executeQueries`
//! runs for anyone with Build. Owned by part C.
//!
//! Tables and columns, types inferred from Min/Max (dropped right after,
//! S4), row counts from the `RowNumber-…` column's cardinality;
//! `RowNumber-*`, `LocalDateTable_*` and `DateTableTemplate_*` left out.
//! No measures, relationships, hierarchies, roles or definitions: the
//! notes say so.
//!
//! Statistics can't tell a whole-number column from a decimal one (prices
//! from 1 to 100 with 49.99 inside have whole Min and Max), so every
//! numeric column is reported as `Double`, never `Int64`: a transfer or a
//! schema conversion then makes a column that keeps fractions, and the
//! notes say the types are inferred. What `COLUMNSTATISTICS` costs on
//! DirectQuery tables (it may query the source) is still a live check
//! (spec §4.3, §8.3); until it's known, those tables are read like the
//! rest.

use super::rows::Rows;
use super::service::{session_model, Service};
use super::{is_auto_date, Column, DataType, ModelMeta, Table, TableKind, Tier};
use crate::catalog::Model;
use crate::query::{QueryOpts, Target};
use crate::Ctx;
use dbine_driver::Result;
use serde_json::Value;

pub async fn read(cx: &mut Ctx) -> Result<ModelMeta> {
    let m = session_model(cx)?;
    read_model(cx, &m).await
}

/// What Tier C can't read, for properties and the health check.
pub const NOTE: &str = "Con los permisos de esta conexión solo se leen tablas y columnas (COLUMNSTATISTICS): las medidas, relaciones, jerarquías, roles y definiciones necesitan executeDaxQueries (capacidad Premium o Fabric) o permiso de escritura en el modelo (API de definición de Fabric).";

/// Why the types are approximate in this tier.
pub const TYPES_NOTE: &str = "Los tipos de las columnas se deducen de sus valores mínimo y máximo: las numéricas figuran como Double, porque así no se distingue un número entero de uno decimal.";

pub(crate) async fn read_model<S: Service>(s: &mut S, m: &Model) -> Result<ModelMeta> {
    let sets = s
        .dax(
            &Target::of(m),
            "EVALUATE COLUMNSTATISTICS()",
            &QueryOpts::metadata(),
        )
        .await?;
    let mut meta = ModelMeta {
        tier: Tier::Stats,
        notes: vec![NOTE.into(), TYPES_NOTE.into()],
        ..Default::default()
    };
    let Some(set) = sets.first() else {
        return Ok(meta);
    };
    let rows = Rows::new(set);
    for r in rows.iter() {
        let (Some(table), Some(column)) = (r.str("Table Name"), r.str("Column Name")) else {
            continue;
        };
        let cardinality = r.u64("Cardinality");
        let i = match meta.tables.iter().position(|t| t.name == table) {
            Some(i) => i,
            None => {
                meta.tables.push(Table {
                    kind: if is_auto_date(&table) {
                        TableKind::AutoDate
                    } else {
                        TableKind::Data
                    },
                    name: table,
                    ..Default::default()
                });
                meta.tables.len() - 1
            }
        };
        let t = &mut meta.tables[i];
        if column.starts_with("RowNumber-") {
            // The engine's row-number column: one value per row.
            t.rows = cardinality;
            continue;
        }
        // Min and Max are the table's values: read for the type, then gone.
        let data_type = infer(
            r.value("Min"),
            r.value("Max"),
            r.value("Max Length").is_some(),
        );
        t.columns.push(Column {
            name: column,
            data_type,
            nullable: true,
            cardinality,
            ..Default::default()
        });
    }
    Ok(meta)
}

/// A column's type from its statistics: text when it has a longest length,
/// else what Min and Max look like. Both blank: a binary column, or an
/// empty table (unknown). A number is always `Double`: whole Min and Max
/// don't make the column whole numbers.
fn infer(min: Option<&Value>, max: Option<&Value>, max_length: bool) -> DataType {
    if max_length {
        return DataType::String;
    }
    let kinds: Vec<DataType> = [min, max].into_iter().flatten().map(kind_of).collect();
    match kinds.as_slice() {
        [] => DataType::Unknown,
        ks if ks.contains(&DataType::String) => DataType::String,
        ks if ks.contains(&DataType::DateTime) => DataType::DateTime,
        ks if ks.contains(&DataType::Boolean) => DataType::Boolean,
        _ => DataType::Double,
    }
}

fn kind_of(v: &Value) -> DataType {
    match v {
        Value::Bool(_) => DataType::Boolean,
        Value::Number(_) => DataType::Double,
        Value::String(s) => {
            let t = s.trim();
            if t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false") {
                DataType::Boolean
            } else if looks_like_datetime(t) {
                DataType::DateTime
            } else if t.parse::<f64>().is_ok() {
                DataType::Double
            } else {
                DataType::String
            }
        }
        _ => DataType::Unknown,
    }
}

/// `2024-01-31T00:00:00`, `2024-01-31 00:00:00`, `2024-01-31`.
fn looks_like_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
        && (b.len() == 10 || matches!(b[10], b'T' | b' '))
}

#[cfg(test)]
mod tests {
    use super::super::fake::{rowset, Fake};
    use super::*;
    use serde_json::json;

    /// Learn's COLUMNSTATISTICS sample (Adventure Works DW 2020), as
    /// `executeQueries` returns it, plus the row-number and auto date/time
    /// rows the engine adds.
    fn answers(f: &mut Fake) {
        f.dax_answer(
            "COLUMNSTATISTICS()",
            vec![rowset(
                &[
                    "[Table Name]",
                    "[Column Name]",
                    "[Min]",
                    "[Max]",
                    "[Cardinality]",
                    "[Max Length]",
                ],
                vec![
                    json!([
                        "Customer",
                        "RowNumber-2662979B-1795-4F74-8F37-6A1BA8059B61",
                        null,
                        null,
                        18485,
                        null
                    ]),
                    json!(["Customer", "CustomerKey", -1, 29483, 18485, null]),
                    json!([
                        "Customer",
                        "Customer ID",
                        "[Not Applicable]",
                        "AW00029483",
                        18485,
                        16
                    ]),
                    json!(["Customer", "City", "[Not Applicable]", "York", 270, 21]),
                    json!([
                        "Sales",
                        "Order Date",
                        "2017-07-01T00:00:00",
                        "2020-06-15T00:00:00",
                        1124,
                        null
                    ]),
                    json!(["Sales", "Unit Price", 0.5, 2171.29, 233, null]),
                    json!(["Sales", "Is Return", false, true, 2, null]),
                    json!(["Sales", "Photo", null, null, 10, null]),
                    json!([
                        "LocalDateTable_0c0a",
                        "Date",
                        "2017-01-01T00:00:00",
                        "2020-12-31T00:00:00",
                        1461,
                        null
                    ]),
                ],
            )],
        );
    }

    #[test]
    fn whole_min_and_max_never_claim_int64() {
        // A price column from 1 to 100 with 49.99 inside.
        assert_eq!(
            infer(Some(&json!(1)), Some(&json!(100)), false),
            DataType::Double
        );
        assert_eq!(
            infer(Some(&json!("1")), Some(&json!("100")), false),
            DataType::Double
        );
        assert_eq!(
            infer(Some(&json!(0.5)), Some(&json!(2)), false),
            DataType::Double
        );
        assert_eq!(infer(None, None, false), DataType::Unknown);
    }

    #[tokio::test]
    async fn tables_columns_and_rows_without_the_values() {
        let mut f = Fake::new();
        answers(&mut f);
        let m = f.model.clone().unwrap();
        let meta = read_model(&mut f, &m).await.unwrap();
        assert_eq!(meta.tier, Tier::Stats);
        let c = meta.table("Customer").unwrap();
        assert_eq!(c.rows, Some(18485));
        let types: Vec<(&str, DataType)> = c
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type))
            .collect();
        assert_eq!(
            types,
            [
                // Whole Min and Max: still Double (49.99 may be inside).
                ("CustomerKey", DataType::Double),
                ("Customer ID", DataType::String),
                ("City", DataType::String)
            ]
        );
        let s = meta.table("Sales").unwrap();
        let types: Vec<DataType> = s.columns.iter().map(|c| c.data_type).collect();
        assert_eq!(
            types,
            [
                DataType::DateTime,
                DataType::Double,
                DataType::Boolean,
                DataType::Unknown
            ]
        );
        assert_eq!(
            meta.table("LocalDateTable_0c0a").unwrap().kind,
            TableKind::AutoDate
        );
        assert!(meta.measures.is_empty() && meta.relationships.is_empty());
        assert_eq!(meta.notes, [NOTE, TYPES_NOTE]);
        // S4: no Min or Max value anywhere in what's kept.
        let dump = format!("{meta:?}");
        for v in ["29483", "AW00029483", "York", "2171.29", "2020-06-15"] {
            assert!(!dump.contains(v), "{v} leaked");
        }
    }
}
