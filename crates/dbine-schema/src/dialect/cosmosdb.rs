//! Azure Cosmos DB (NoSQL API). Containers hold schemaless JSON (the type
//! model is in [`super::couchdb`]); every number is an IEEE double.
//!
//! A container needs a partition key, which the conversion takes from the
//! primary key: its columns (up to three, a hierarchical key) as paths.
//! Each row is then its own logical partition, which spreads writes and
//! keeps point reads by key cheap. Cosmos DB identifies a document by its
//! `id` (text, unique within its partition), so a key that isn't `id` is
//! reported: the copy has to fill `id`.
//!
//! Indexes: Cosmos DB indexes every path by itself, so single-field
//! indexes are dropped; non-unique multi-field ones are composite indexes;
//! unique ones become the unique key policy, which is checked only within
//! each logical partition.

use super::couchdb::{json_caps, parse_json_type, render_json_type, schemaless_note};
use super::{Caps, Dialect, Rendered};
use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType as L;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

pub struct CosmosDb;

pub fn lookup(driver_id: &str) -> Option<&'static dyn Dialect> {
    static D: CosmosDb = CosmosDb;
    (driver_id == "cosmosdb").then_some(&D as &dyn Dialect)
}

const PARTITION_KEY: &str = "partition_key";

impl Dialect for CosmosDb {
    fn id(&self) -> &'static str {
        "cosmosdb"
    }

    fn parse_type(&self, t: &TypeSpec) -> L {
        parse_json_type(t)
    }

    fn render_type(&self, t: &L) -> Rendered {
        render_json_type(t, true)
    }

    fn render_default(&self, _d: &DefaultValue, _ty: &L) -> Option<String> {
        None
    }

    fn caps(&self) -> Caps {
        json_caps(255)
    }

    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        if t.options.contains_key(PARTITION_KEY) {
            return;
        }
        let name = t.name.clone();
        t.kind = dbine_driver::kinds::COLLECTION.into();
        schemaless_note(t, report, "Cosmos DB");
        let key: Vec<String> = t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default();
        let paths: Vec<String> = if key.is_empty() { vec!["id".into()] } else { key.iter().take(3).cloned().collect() };
        let pk = paths.iter().map(|p| format!("/{p}")).collect::<Vec<_>>().join(", ");
        t.options.insert(PARTITION_KEY.into(), pk.clone());
        report.push(
            Severity::Info,
            IssueCode::OptionAdded,
            &name,
            Some(PARTITION_KEY),
            format!("Clave de partición {pk}, tomada de la clave primaria: cada fila es su propia partición lógica."),
        );
        if key.len() > 3 {
            report.push(Severity::Warning, IssueCode::OptionAdded, &name, Some(PARTITION_KEY), "La clave de partición jerárquica admite hasta 3 rutas: se usan las 3 primeras columnas de la clave.");
        }
        match key.as_slice() {
            [] => report.push(
                Severity::Warning,
                IssueCode::PrimaryKeyAdded,
                &name,
                None,
                "La tabla no tiene clave primaria: Cosmos DB identifica los documentos por «id», que hay que generar al copiar.",
            ),
            [k] if k == "id" => {}
            _ => report.push(
                Severity::Warning,
                IssueCode::PrimaryKeyDropped,
                &name,
                None,
                format!("Cosmos DB identifica cada documento por «id» (texto): al copiar hay que llenarlo con la clave ({}).", key.join(", ")),
            ),
        }
        let mut kept = Vec::new();
        for mut ix in std::mem::take(&mut t.indexes) {
            if ix.unique {
                report.push(
                    Severity::Warning,
                    IssueCode::IndexChanged,
                    &name,
                    Some(&ix.name),
                    "Clave única de Cosmos DB: solo se controla dentro de cada partición lógica, y no se puede cambiar después de crear el contenedor.",
                );
                kept.push(ix);
            } else if ix.columns.len() < 2 {
                report.push(Severity::Info, IssueCode::IndexDropped, &name, Some(&ix.name), "Cosmos DB ya indexa cada campo: el índice simple no hace falta.");
            } else {
                ix.kind = Some("composite".into());
                kept.push(ix);
            }
        }
        t.indexes = kept;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn types() {
        let d = CosmosDb;
        assert_eq!(d.parse_type(&parse("integer|number")), L::Float { bytes: 8 });
        assert_eq!(d.render_type(&L::int(8)).native, "integer");
        assert!(!d.render_type(&L::int(8)).notes.is_empty());
        assert!(!d.caps().nullability && !d.caps().foreign_keys);
    }
}
