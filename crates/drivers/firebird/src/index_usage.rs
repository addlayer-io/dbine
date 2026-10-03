//! A table's indexes (`Session::index_usage`).
//!
//! - The indexes, the primary key and the foreign keys come from the same
//!   catalog read as the schema compare (`RDB$INDICES`,
//!   `RDB$INDEX_SEGMENTS`, `RDB$RELATION_CONSTRAINTS`), so an index has the
//!   same name here as in the drop script "Eliminar índice" generates.
//!   UNIQUE constraints are listed as unique indexes.
//! - The kind: `ASC` or `DESC`, `COMPUTED` for an expression index and
//!   `INACTIVE` for one switched off (`ALTER INDEX … INACTIVE`).
//! - Firebird counts nothing per index: the monitoring tables
//!   (`MON$RECORD_STATS.MON$RECORD_IDX_READS`) count indexed reads per
//!   table, and the index's size is only in `gstat`. So `stats_available`
//!   is false and the note says so.
//! - "Deshabilitar / Habilitar índice" ([`toggle_script`]): `ALTER INDEX …
//!   INACTIVE` / `ACTIVE`; `disabled` is `RDB$INDEX_INACTIVE = 1`.

use crate::schema::INACTIVE;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{Error, IndexUsage, IndexUsageReport, Result, SyncScript, TableSchema};

pub const NOTE: &str = "Firebird no registra cuántas veces se usa cada índice (las tablas MON$ cuentan las lecturas por índice de toda la tabla) ni su tamaño (solo gstat lo informa): se listan los índices con sus columnas, sin contadores. Para saber si una consulta usa un índice, mirá su plan de ejecución.";

/// The report for `t` (`None`: the table isn't there): the primary key
/// first, then the indexes in the schema's order.
pub fn report(t: Option<&TableSchema>) -> IndexUsageReport {
    let Some(t) = t else { return IndexUsageReport { note: Some(NOTE.into()), writes_counted: false, ..Default::default() } };
    let mut indexes = Vec::new();
    if let Some(pk) = &t.primary_key {
        indexes.push(IndexUsage {
            name: pk.name.clone().unwrap_or_else(|| "PRIMARY KEY".into()),
            kind: "ASC".into(),
            unique: true,
            primary_key: true,
            key_columns: pk.columns.clone(),
            ..Default::default()
        });
    }
    for ix in &t.indexes {
        let mut kind: Vec<&str> = Vec::new();
        let flags = ix.kind.as_deref().unwrap_or("");
        kind.push(if flags.contains("DESC") { "DESC" } else { "ASC" });
        if flags.contains("COMPUTED") {
            kind.push("COMPUTED");
        }
        let inactive = ix.options.get(INACTIVE).is_some_and(|v| v == "true");
        if inactive {
            kind.push("INACTIVE");
        }
        indexes.push(IndexUsage {
            name: ix.name.clone(),
            kind: kind.join(" "),
            unique: ix.unique,
            disabled: inactive,
            key_columns: ix.columns.clone(),
            filter: ix.filter.clone(),
            ..Default::default()
        });
    }
    IndexUsageReport { note: Some(NOTE.into()), indexes, foreign_keys: t.foreign_keys.clone(), writes_counted: false, ..Default::default() }.derived()
}

/// "Deshabilitar / Habilitar índice": `ALTER INDEX … INACTIVE` and
/// `ALTER INDEX … ACTIVE` (activating rebuilds the index whole).
///
/// An index that backs a PRIMARY KEY, FOREIGN KEY or UNIQUE constraint
/// can't be deactivated. The report lists the primary key (`primary_key`)
/// and UNIQUE constraints (as unique indexes named after the constraint);
/// foreign key indexes aren't listed. Refused here: the primary key, and
/// names Firebird made up for constraints (`RDB$PRIMARYn`, `RDB$FOREIGNn`,
/// `RDB$n`, `INTEG_n`). A named UNIQUE constraint can't be told from a
/// `CREATE UNIQUE INDEX` with what the report carries, so for unique
/// indexes the server decides and the script warns.
pub fn toggle_script(index: &IndexUsage, enable: bool) -> Result<SyncScript> {
    let generated = index.name.starts_with("RDB$") || (index.unique && index.name.starts_with("INTEG_"));
    if index.primary_key || generated {
        return Err(Error::Unsupported(format!(
            "{} respalda una restricción (PRIMARY KEY, FOREIGN KEY o UNIQUE): Firebird no deja desactivar esos índices.",
            index.name
        )));
    }
    let name = quote_ident(Quote::Double, &index.name);
    let mut warnings = Vec::new();
    if enable {
        warnings.push("Activar un índice lo reconstruye entero: en una tabla grande tarda.".to_string());
    } else {
        warnings.push("Firebird deja de mantenerlo; al activarlo lo reconstruye entero.".to_string());
        if index.unique {
            warnings.push("Si este índice respalda una restricción UNIQUE, Firebird no lo deja desactivar y el script falla.".to_string());
        }
    }
    Ok(SyncScript { statements: vec![format!("ALTER INDEX {name} {}", if enable { "ACTIVE" } else { "INACTIVE" })], warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ForeignKeyDef, IndexDef, KeyDef};
    use std::collections::BTreeMap;

    #[test]
    fn key_first_then_indexes_with_their_kind() {
        let t = TableSchema {
            name: "T".into(),
            primary_key: Some(KeyDef { name: Some("PK_T".into()), columns: vec!["ID".into()] }),
            indexes: vec![
                IndexDef { name: "UQ_CODE".into(), columns: vec!["CODE".into()], unique: true, ..Default::default() },
                IndexDef { name: "IX_N".into(), columns: vec!["N".into()], kind: Some("DESC".into()), ..Default::default() },
                IndexDef {
                    name: "IX_UP".into(),
                    columns: vec!["(UPPER(T))".into()],
                    kind: Some("COMPUTED".into()),
                    filter: Some("T IS NOT NULL".into()),
                    options: BTreeMap::from([(INACTIVE.to_string(), "true".to_string())]),
                    ..Default::default()
                },
            ],
            foreign_keys: vec![ForeignKeyDef { columns: vec!["P_ID".into()], ref_table: "P".into(), ref_columns: vec!["ID".into()], ..Default::default() }],
            ..Default::default()
        };
        let r = report(Some(&t));
        assert!(!r.stats_available && r.note.is_some());
        let got: Vec<(&str, &str)> = r.indexes.iter().map(|i| (i.name.as_str(), i.kind.as_str())).collect();
        assert_eq!(got, [("PK_T", "ASC"), ("UQ_CODE", "ASC"), ("IX_N", "DESC"), ("IX_UP", "ASC COMPUTED INACTIVE")]);
        assert!(r.indexes[0].primary_key && r.indexes[1].unique && !r.indexes[2].unique);
        assert_eq!(r.indexes.iter().map(|i| i.disabled).collect::<Vec<_>>(), [false, false, false, true]);
        assert_eq!(r.indexes[3].filter.as_deref(), Some("T IS NOT NULL"));
        assert_eq!(r.foreign_keys, t.foreign_keys);
        assert!(r.indexes.iter().all(|i| !i.unused));
        // An unnamed key (RDB$PRIMARYn).
        let t = TableSchema { primary_key: Some(KeyDef { name: None, columns: vec!["ID".into()] }), ..Default::default() };
        assert_eq!(report(Some(&t)).indexes[0].name, "PRIMARY KEY");
        assert!(report(None).indexes.is_empty());
    }

    #[test]
    fn toggle_scripts() {
        let ix = IndexUsage { name: "IX_Fecha".into(), kind: "ASC".into(), ..Default::default() };
        let off = toggle_script(&ix, false).unwrap();
        assert_eq!(off.statements, ["ALTER INDEX \"IX_Fecha\" INACTIVE"]);
        assert_eq!(off.warnings, ["Firebird deja de mantenerlo; al activarlo lo reconstruye entero."]);
        let on = toggle_script(&ix, true).unwrap();
        assert_eq!(on.statements, ["ALTER INDEX \"IX_Fecha\" ACTIVE"]);
        assert!(on.warnings[0].contains("reconstruye"));
        // A unique index may be a UNIQUE constraint: the server decides, with a warning.
        let uq = IndexUsage { name: "UQ_CODE".into(), unique: true, ..Default::default() };
        let w = toggle_script(&uq, false).unwrap().warnings;
        assert!(w.len() == 2 && w[1].contains("UNIQUE"), "{w:?}");
        // Constraint indexes are refused.
        let pk = IndexUsage { name: "PK_T".into(), primary_key: true, unique: true, ..Default::default() };
        for ix in [pk, IndexUsage { name: "RDB$FOREIGN12".into(), ..Default::default() }, IndexUsage { name: "INTEG_7".into(), unique: true, ..Default::default() }] {
            for enable in [false, true] {
                assert!(matches!(toggle_script(&ix, enable), Err(Error::Unsupported(m)) if m.contains("restricción")), "{}", ix.name);
            }
        }
    }
}
