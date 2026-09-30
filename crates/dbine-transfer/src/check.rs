//! The check before copying: never load into a different structure.

use dbine_driver::{ColumnInfo, TransferColumn};
use std::collections::HashSet;

/// Every difference between the columns a copy expects and those the
/// target table has (empty: they match). Names are compared ignoring case;
/// types only when `compare_types` (source and target are the same engine).
/// `load` are the columns the copy writes. A target column the copy doesn't
/// write counts only when it can't be left empty (not nullable, no default,
/// not generated).
pub fn column_differences(expected: &[TransferColumn], load: &[String], actual: &[ColumnInfo], compare_types: bool) -> Vec<String> {
    let find = |name: &str| actual.iter().find(|c| c.name.eq_ignore_ascii_case(name));
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for e in expected {
        seen.insert(e.name.to_lowercase());
        match find(&e.name) {
            None => out.push(format!("falta la columna «{}» en el destino", e.name)),
            Some(a) if compare_types && norm(&a.data_type) != norm(&e.type_name) => {
                out.push(format!("«{}» es {} en el origen y {} en el destino", e.name, e.type_name, a.data_type))
            }
            Some(_) => {}
        }
    }
    for name in load {
        if seen.insert(name.to_lowercase()) && find(name).is_none() {
            out.push(format!("falta la columna «{name}» en el destino"));
        }
    }
    for a in actual {
        if !seen.contains(&a.name.to_lowercase()) && !a.nullable && a.default_value.is_none() && !a.auto_increment {
            out.push(format!("«{}» está solo en el destino y no admite nulos", a.name));
        }
    }
    out
}

/// A type name without case or spaces (`NVARCHAR (50)` = `nvarchar(50)`).
fn norm(t: &str) -> String {
    t.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, t: &str, nullable: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: t.into(), nullable, primary_key: false, auto_increment: false, default_value: None }
    }
    fn tc(name: &str, t: &str) -> TransferColumn {
        TransferColumn { name: name.into(), type_name: t.into(), nullable: true }
    }

    #[test]
    fn reports_every_difference() {
        let expected = [tc("Id", "int"), tc("name", "NVARCHAR (50)"), tc("price", "decimal(10,2)"), tc("gone", "int")];
        let actual = [col("id", "INT", false), col("NAME", "nvarchar(50)", true), col("price", "float", true), col("extra", "int", false), col("opt", "int", true)];
        let names: Vec<String> = expected.iter().map(|c| c.name.clone()).collect();
        let d = column_differences(&expected, &names, &actual, true);
        assert_eq!(d.len(), 3, "{d:?}");
        assert!(d[0].contains("price"));
        assert!(d[1].contains("gone"));
        assert!(d[2].contains("extra"));
        // Between engines, only names count.
        assert_eq!(column_differences(&expected, &names, &actual, false).len(), 2);
    }
}
