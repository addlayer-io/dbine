//! Schema sync in the HTTP console syntax, on the `_all_docs` pseudo-table
//! (see `ddl.rs`): new Mango indexes are `POST _index` and new design
//! documents `PUT _design/…`. Replacing or deleting a design document
//! needs its current `_rev`, and deleting a Mango index needs the design
//! document it lives in, which the schema doesn't carry: those are
//! warnings. Documents have no schema: field changes are warnings too.

use crate::ddl::{index_body, table_ddl, validator_doc, validator_id, validators};
use crate::seg;
use dbine_driver::{ColumnDef, DdlParts, IndexDef, Result, SyncScript, TableChange, TableSchema};
use serde_json::Value;

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: false };

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

/// Per-field warnings: documents keep what they have.
fn field_warnings(t: &str, old: &[ColumnDef], new: &[ColumnDef], out: &mut Vec<String>) {
    for c in old.iter().filter(|c| !new.iter().any(|n| eq_name(&n.name, &c.name))) {
        out.push(format!("Los documentos no tienen esquema fijo: el campo {t}.{} no se borra de los documentos que ya existen.", c.name));
    }
    for c in new.iter().filter(|c| !old.iter().any(|o| eq_name(&o.name, &c.name))) {
        out.push(format!("Los documentos no tienen esquema fijo: el campo {t}.{} no se agrega a los documentos que ya existen.", c.name));
    }
    for n in new {
        if let Some(o) = old.iter().find(|o| eq_name(&o.name, &n.name)) {
            if squash(&o.data_type) != squash(&n.data_type) || o.nullable != n.nullable {
                out.push(format!("Los documentos no tienen esquema fijo: el campo {t}.{} no se cambia.", n.name));
            }
        }
    }
}

fn ix_same(a: &IndexDef, b: &IndexDef) -> bool {
    match (index_body(a), index_body(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The design documents of the `design_docs` option, by `_id`.
fn design_docs(t: &TableSchema) -> Result<Vec<(String, Value)>> {
    let Some(text) = t.options.get("design_docs").filter(|s| !s.trim().is_empty()) else { return Ok(Vec::new()) };
    let docs: Vec<Value> = serde_json::from_str(text).map_err(|e| dbine_driver::Error::Query(format!("design_docs: JSON inválido ({e})")))?;
    Ok(docs
        .into_iter()
        .map(|mut d| {
            if let Some(m) = d.as_object_mut() {
                m.remove("_rev");
            }
            (d.get("_id").and_then(Value::as_str).unwrap_or_default().to_string(), d)
        })
        .collect())
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut statements, mut warnings) = (Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => statements.push(table_ddl(table, CREATE)?),
            TableChange::Drop { table } => warnings.push(format!(
                "CouchDB no borra {}: los documentos son la base misma; para empezar de cero, borrá la base y creala de nuevo.",
                table.name
            )),
            TableChange::Alter { old, new } => {
                let t = new.name.as_str();
                field_warnings(t, &old.columns, &new.columns, &mut warnings);
                for o in &old.indexes {
                    if !new.indexes.iter().any(|n| eq_name(&n.name, &o.name) && ix_same(o, n)) {
                        warnings.push(format!(
                            "El índice Mango {} no se borra: CouchDB lo borra por su documento de diseño (DELETE _index/<documento>/json/{}); hacelo a mano.",
                            o.name, o.name
                        ));
                    }
                }
                for n in &new.indexes {
                    if !old.indexes.iter().any(|o| eq_name(&o.name, &n.name) && ix_same(o, n)) {
                        statements.push(format!("POST _index\n{}", index_body(n)?));
                    }
                }
                let (od, nd) = (design_docs(old)?, design_docs(new)?);
                for (id, _) in od.iter().filter(|(id, _)| !nd.iter().any(|(n, _)| n == id)) {
                    warnings.push(format!("El documento de diseño {id} no se borra: CouchDB necesita su revisión (_rev); borralo a mano."));
                }
                for (id, doc) in &nd {
                    match od.iter().find(|(o, _)| o == id) {
                        None => {
                            let name = id.strip_prefix("_design/").unwrap_or(id);
                            statements.push(format!("PUT _design/{}\n{doc}", seg(name)));
                        }
                        Some((_, old_doc)) if old_doc != doc => warnings.push(format!(
                            "El documento de diseño {id} cambia: CouchDB necesita su revisión (_rev) para reemplazarlo; actualizalo a mano."
                        )),
                        Some(_) => {}
                    }
                }
                // Validation functions (the CHECKs): a new design document
                // is created; changing or dropping one needs its `_rev`.
                let exists = |id: &str| od.iter().any(|(o, _)| o == id);
                for n in validators(new) {
                    let id = validator_id(n);
                    match validators(old).find(|o| validator_id(o) == id) {
                        None if exists(&id) => warnings.push(format!(
                            "La validación de {id} no se agrega: el documento de diseño ya existe y CouchDB necesita su revisión (_rev); agregala a mano."
                        )),
                        // A design document that comes whole already carries it.
                        None if nd.iter().any(|(d, _)| *d == id) => {}
                        None => statements.push(validator_doc(n)?),
                        Some(o) if o.expression.trim() != n.expression.trim() => warnings.push(format!(
                            "La validación de {id} cambia: CouchDB necesita la revisión (_rev) del documento de diseño; actualizala a mano."
                        )),
                        Some(_) => {}
                    }
                }
                for o in validators(old).filter(|o| !validators(new).any(|n| validator_id(n) == validator_id(o))) {
                    warnings.push(format!(
                        "La validación de {} no se quita: CouchDB necesita la revisión (_rev) del documento de diseño; quitala a mano.",
                        validator_id(o)
                    ));
                }
            }
        }
    }
    let statements = statements.into_iter().filter(|s: &String| !s.trim().is_empty()).collect();
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn ix(name: &str, cols: &[&str]) -> IndexDef {
        IndexDef { name: name.into(), columns: cols.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    fn all_docs(cols: Vec<ColumnDef>, indexes: Vec<IndexDef>, ddocs: &str) -> TableSchema {
        let mut t = TableSchema { kind: "collection".into(), name: "_all_docs".into(), columns: cols, indexes, ..Default::default() };
        if !ddocs.is_empty() {
            t.options.insert("design_docs".into(), ddocs.into());
        }
        t
    }

    #[test]
    fn alter_indexes_design_docs_and_fields() {
        let old = all_docs(
            vec![col("_id", "string"), col("tipo", "string")],
            vec![ix("por_tipo", &["tipo"])],
            r#"[{"_id":"_design/a","_rev":"1-x","views":{"v":{"map":"function(d){}"}}},{"_id":"_design/gone"}]"#,
        );
        let new = all_docs(
            vec![col("_id", "string"), col("tipo", "number"), col("fecha", "string")],
            vec![ix("por_tipo", &["tipo"]), ix("por_fecha", &["fecha:desc"])],
            r#"[{"_id":"_design/a","views":{"v":{"map":"function(d){ emit(1); }"}}},{"_id":"_design/b","language":"javascript"}]"#,
        );
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "POST _index\n{\"index\":{\"fields\":[{\"fecha\":\"desc\"}]},\"name\":\"por_fecha\",\"type\":\"json\"}",
                "PUT _design/b\n{\"_id\":\"_design/b\",\"language\":\"javascript\"}",
            ]
        );
        assert_eq!(
            s.warnings,
            vec![
                "Los documentos no tienen esquema fijo: el campo _all_docs.fecha no se agrega a los documentos que ya existen.",
                "Los documentos no tienen esquema fijo: el campo _all_docs.tipo no se cambia.",
                "El documento de diseño _design/gone no se borra: CouchDB necesita su revisión (_rev); borralo a mano.",
                "El documento de diseño _design/a cambia: CouchDB necesita su revisión (_rev) para reemplazarlo; actualizalo a mano.",
            ]
        );
    }

    #[test]
    fn validation_functions_are_checks() {
        let f = "function (n) { if (!n.tipo) throw({ forbidden: 'tipo' }); }";
        let old = all_docs(vec![], vec![], r#"[{"_id":"_design/v1","validate_doc_update":"function(){}"}]"#);
        let mut new = old.clone();
        new.checks = vec![
            dbine_driver::CheckDef { name: Some("_design/v2".into()), expression: f.into() },
            dbine_driver::CheckDef { name: Some("_design/v1".into()), expression: "function(){ }".into() },
        ];
        let mut old = old;
        old.checks = vec![dbine_driver::CheckDef { name: Some("_design/v1".into()), expression: "function(){}".into() }];
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(s.statements.len(), 1, "{s:?}");
        let v: Value = serde_json::from_str(s.statements[0].strip_prefix("PUT _design/v2\n").unwrap()).unwrap();
        assert_eq!(v["validate_doc_update"], f);
        assert_eq!(s.warnings.len(), 1, "{s:?}");
        assert!(s.warnings[0].contains("_design/v1"));
    }

    #[test]
    fn create_and_drop() {
        let t = all_docs(vec![], vec![ix("por_tipo", &["tipo"])], "");
        let s = sync_script(&[TableChange::Create { table: t.clone() }, TableChange::Drop { table: t }]).unwrap();
        assert_eq!(s.statements, vec!["POST _index\n{\"index\":{\"fields\":[\"tipo\"]},\"name\":\"por_tipo\",\"type\":\"json\"}"]);
        assert_eq!(
            s.warnings,
            vec!["CouchDB no borra _all_docs: los documentos son la base misma; para empezar de cero, borrá la base y creala de nuevo."]
        );
    }
}
