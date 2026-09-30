//! Schema sync in the console syntax: collections (cores) are created and
//! dropped with DBine's `PUT` / `DELETE /solr/<name>`, and fields change
//! through the Schema API in one `POST /solr/<name>/schema` with
//! `delete-field`, `add-field` and `replace-field` lists. The uniqueKey
//! comes from the configset and can't change.

use crate::ddl::{check_name, collection_ddl, field_def, BUILTIN_FIELDS};
use dbine_driver::{ColumnDef, DdlParts, Result, SyncScript, TableChange, TableSchema};
use dbine_driver_elasticsearch::json::J;

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: false, foreign_keys: false };
const DROP: DdlParts = DdlParts { drop: true, if_exists: false, create: false, indexes: false, foreign_keys: false };

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Fields the configset owns (the uniqueKey and Solr's own).
fn managed(t: &TableSchema, c: &ColumnDef) -> bool {
    let key = t.primary_key.as_ref().and_then(|k| k.columns.first()).map(String::as_str).unwrap_or("id");
    BUILTIN_FIELDS.contains(&c.name.as_str()) || c.name == key || c.name.trim().is_empty()
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut alters, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(collection_ddl(table, CREATE)?),
            TableChange::Drop { table } => {
                warnings.push(format!("Se borra la colección {} con todos sus documentos.", table.name));
                drops.push(collection_ddl(table, DROP)?);
            }
            TableChange::Alter { old, new } => {
                let name = new.name.trim();
                check_name(name)?;
                let key = |t: &TableSchema| t.primary_key.as_ref().and_then(|k| k.columns.first().cloned());
                if key(old) != key(new) {
                    warnings.push(format!("La clave única (uniqueKey) de {name} viene del configset y no se cambia con la Schema API; se deja como está."));
                }
                let (oc, nc): (Vec<&ColumnDef>, Vec<&ColumnDef>) =
                    (old.columns.iter().filter(|c| !managed(old, c)).collect(), new.columns.iter().filter(|c| !managed(new, c)).collect());
                let mut delete = Vec::new();
                let mut add = Vec::new();
                let mut replace = Vec::new();
                for c in oc.iter().filter(|c| !nc.iter().any(|n| eq_name(&n.name, &c.name))) {
                    warnings.push(format!("Se borra el campo {name}.{} del esquema: sus valores ya indexados dejan de poder buscarse.", c.name));
                    delete.push(J::Obj(vec![("name".into(), J::Str(c.name.clone()))]));
                }
                for c in nc.iter().filter(|c| !oc.iter().any(|o| eq_name(&o.name, &c.name))) {
                    add.push(J::Obj(field_def(c)?));
                }
                for n in &nc {
                    let Some(o) = oc.iter().find(|o| eq_name(&o.name, &n.name)) else { continue };
                    let nd = field_def(n)?;
                    if J::Obj(field_def(o)?).compact() != J::Obj(nd.clone()).compact() {
                        warnings.push(format!(
                            "{name}.{}: cambia su definición ({} → {}). Los documentos ya indexados no cambian: hay que reindexarlos.",
                            n.name, o.data_type, n.data_type
                        ));
                        replace.push(J::Obj(nd));
                    }
                }
                if let Some(o) = ["configSet", "numShards", "replicationFactor"].iter().find(|k| old.options.get(**k) != new.options.get(**k)) {
                    warnings.push(format!("{name}: {o} se fija al crear la colección; se deja como está."));
                }
                let mut cmds = Vec::new();
                for (cmd, list) in [("delete-field", delete), ("add-field", add), ("replace-field", replace)] {
                    if !list.is_empty() {
                        let items: Vec<String> = list.iter().map(|f| format!("    {}", f.compact())).collect();
                        cmds.push(format!("  \"{cmd}\": [\n{}\n  ]", items.join(",\n")));
                    }
                }
                if !cmds.is_empty() {
                    alters.push(format!("POST /solr/{name}/schema\n{{\n{}\n}}", cmds.join(",\n")));
                }
            }
        }
    }
    let statements = [drops, alters, creates].into_iter().flatten().filter(|s: &String| !s.trim().is_empty()).collect();
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::KeyDef;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn coll(name: &str, cols: Vec<ColumnDef>) -> TableSchema {
        TableSchema {
            kind: "collection".into(),
            name: name.into(),
            columns: cols,
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            ..Default::default()
        }
    }

    #[test]
    fn alter_fields() {
        let old = coll("films", vec![col("id", "string"), col("_version_", "plong"), col("title", "text_general"), col("year", "pint"), col("old", "string")]);
        let mut year = col("year", "plong");
        year.options.insert("docValues".into(), "true".into());
        let new = coll("films", vec![col("id", "string"), col("title", "text_general"), year, col("genre", "string[]")]);
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "POST /solr/films/schema\n{\n  \"delete-field\": [\n    {\"name\":\"old\"}\n  ],\n  \"add-field\": [\n    {\"name\":\"genre\",\"type\":\"string\",\"multiValued\":true}\n  ],\n  \"replace-field\": [\n    {\"name\":\"year\",\"type\":\"plong\",\"docValues\":true}\n  ]\n}"
            ]
        );
        assert_eq!(
            s.warnings,
            vec![
                "Se borra el campo films.old del esquema: sus valores ya indexados dejan de poder buscarse.",
                "films.year: cambia su definición (pint → plong). Los documentos ya indexados no cambian: hay que reindexarlos.",
            ]
        );
    }

    #[test]
    fn create_and_drop() {
        let s = sync_script(&[
            TableChange::Create { table: coll("films", vec![col("id", "string"), col("title", "text_general")]) },
            TableChange::Drop { table: coll("old", vec![]) },
        ])
        .unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DELETE /solr/old",
                "PUT /solr/films\n\n# Ya definidos por el configset: id.\nPOST /solr/films/schema\n{\n  \"add-field\": [\n    {\"name\":\"title\",\"type\":\"text_general\"}\n  ]\n}",
            ]
        );
        assert_eq!(s.warnings, vec!["Se borra la colección old con todos sus documentos."]);
    }
}
