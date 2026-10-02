//! Schema sync ("Comparar esquemas") for indices, in console syntax.
//!
//! A mapping only grows: new fields (and field descriptions, the index's
//! `_meta` and `dynamic`) go in one `PUT /<index>/_mapping`. Changing a
//! field's type or dropping a field needs a reindex into a new index, so
//! those only warn. Dynamic settings (replicas, refresh interval) go in
//! `PUT /<index>/_settings`, aliases in `POST /_aliases`.

use crate::ddl::{analysis_opt, check_index_name, field_mapping, index_ddl, json_obj_opt, opt, put_field, MAPPINGS_EXTRA, META_FIELDS};
use crate::json::{Obj, J};
use dbine_driver::{ColumnDef, DdlParts, Result, SyncScript, TableChange, TableSchema};
use std::collections::BTreeSet;

pub fn sync_script(changes: &[TableChange], opensearch: bool) -> Result<SyncScript> {
    let (mut drops, mut alters, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(index_ddl(table, DdlParts { create: true, ..Default::default() }, opensearch)?),
            TableChange::Drop { table } => {
                warnings.push(format!("Se borra el índice {} con todos sus documentos.", table.name));
                drops.push(index_ddl(table, DdlParts { drop: true, ..Default::default() }, opensearch)?);
            }
            TableChange::Alter { old, new } => alter(old, new, opensearch, &mut alters, &mut warnings)?,
        }
    }
    Ok(SyncScript { statements: [drops, alters, creates].concat(), warnings })
}

fn squash(s: &str) -> String {
    s.to_lowercase().split_whitespace().collect()
}

fn field(c: &ColumnDef) -> bool {
    let n = c.name.trim();
    !n.is_empty() && !META_FIELDS.contains(&n)
}

/// A field's mapping without its description (`meta`), to compare.
fn params(c: &ColumnDef, opensearch: bool) -> Result<String> {
    let m: Obj = field_mapping(c, opensearch)?.into_iter().filter(|(k, _)| k != "meta").collect();
    Ok(J::Obj(m).compact())
}

fn alter(old: &TableSchema, new: &TableSchema, os: bool, out: &mut Vec<String>, warnings: &mut Vec<String>) -> Result<()> {
    let name = new.name.trim();
    check_index_name(name)?;
    // Analysis (analyzers, tokenizers, filters…): only on a closed index,
    // and before the fields that use it.
    let (oa, na) = (analysis_opt(&old.options)?, analysis_opt(&new.options)?);
    if oa.as_ref().map(J::compact) != na.as_ref().map(J::compact) {
        match na {
            Some(a) => {
                warnings.push(format!(
                    "{name}: el análisis (analizadores, filtros…) solo cambia con el índice cerrado: se cierra, se cambia y se vuelve a abrir. Los documentos ya indexados no se reanalizan."
                ));
                out.push(format!("POST /{name}/_close"));
                out.push(format!("PUT /{name}/_settings\n{}", J::Obj(vec![("analysis".into(), a)]).pretty()));
                out.push(format!("POST /{name}/_open"));
            }
            None => warnings.push(format!("{name}: el análisis propio del índice no se quita sin reindexar en uno nuevo; se deja como está.")),
        }
    }
    let find = |t: &TableSchema, n: &str| t.columns.iter().find(|c| c.name.trim() == n.trim()).cloned();
    let ty = |c: &ColumnDef| squash(if c.data_type.trim().is_empty() { "object" } else { &c.data_type });

    // Fields that go in the mapping update, by path.
    let mut put: Vec<ColumnDef> = Vec::new();
    for n in new.columns.iter().filter(|c| field(c)) {
        match find(old, &n.name) {
            None => {
                // A nested parent has to say so, or the update reads it as an object.
                let parts: Vec<&str> = n.name.trim().split('.').collect();
                for i in 1..parts.len() {
                    let parent = parts[..i].join(".");
                    if let Some(p) = find(new, &parent).filter(|p| ty(p) == "nested") {
                        if !put.iter().any(|x| x.name.trim() == parent) {
                            put.push(ColumnDef { name: parent, data_type: p.data_type.clone(), ..Default::default() });
                        }
                    }
                }
                put.push(n.clone());
            }
            Some(o) if ty(&o) != ty(n) => warnings.push(format!(
                "{name}.{}: {} → {}. El tipo de un campo no se cambia en un índice existente: hay que reindexar en uno nuevo (_reindex); se deja como está.",
                n.name, o.data_type, n.data_type
            )),
            Some(o) if params(&o, os)? != params(n, os)? => warnings.push(format!(
                "{name}.{}: la mayoría de los parámetros de un campo (analizador, formato, index…) no se cambian sin reindexar; se deja como está.",
                n.name
            )),
            Some(o) if o.comment.as_deref().unwrap_or("") != n.comment.as_deref().unwrap_or("") => put.push(n.clone()),
            Some(_) => {}
        }
    }
    for o in old.columns.iter().filter(|c| field(c) && find(new, &c.name).is_none()) {
        warnings.push(format!(
            "{name}.{}: un campo no se puede quitar del mapping; queda (sin valores en los documentos nuevos). Para quitarlo, reindexá en otro índice.",
            o.name
        ));
    }

    let mut body: Obj = Vec::new();
    if opt(&new.options, "dynamic") != opt(&old.options, "dynamic") {
        if let Some(d) = opt(&new.options, "dynamic") {
            body.push(("dynamic".into(), if d == "true" || d == "false" { J::Bool(d == "true") } else { J::Str(d.into()) }));
        }
    }
    if old.comment.as_deref().unwrap_or("") != new.comment.as_deref().unwrap_or("") {
        // `_meta` is replaced whole: its other keys go along.
        let d = new.comment.as_deref().unwrap_or("").trim().to_string();
        let mut meta: Obj = json_obj_opt(&new.options, MAPPINGS_EXTRA)?
            .unwrap_or_default()
            .into_iter()
            .find(|(k, _)| k == "_meta")
            .and_then(|(_, v)| v.as_obj().cloned())
            .unwrap_or_default();
        meta.retain(|(k, _)| k != "description");
        // An empty description reads as none (an empty `_meta` might not replace the old one).
        if !d.is_empty() || meta.is_empty() {
            meta.insert(0, ("description".into(), J::Str(d)));
        }
        body.push(("_meta".into(), J::Obj(meta)));
    }
    if !put.is_empty() {
        put.sort_by_key(|c| c.name.matches('.').count());
        let mut props: Obj = Vec::new();
        for c in &put {
            let path: Vec<&str> = c.name.trim().split('.').collect();
            put_field(&mut props, &path, field_mapping(c, os)?, c.name.trim())?;
        }
        body.push(("properties".into(), J::Obj(props)));
    }
    if !body.is_empty() {
        out.push(format!("PUT /{name}/_mapping\n{}", J::Obj(body).pretty()));
    }

    // Settings: the dynamic ones change in place.
    let mut index: Obj = Vec::new();
    for key in ["number_of_replicas", "refresh_interval"] {
        if let Some(v) = opt(&new.options, key).filter(|v| Some(*v) != opt(&old.options, key)) {
            let j = v.parse::<serde_json::Number>().map(J::Num).unwrap_or_else(|_| J::Str(v.into()));
            index.push((key.into(), j));
        }
    }
    if !index.is_empty() {
        out.push(format!("PUT /{name}/_settings\n{}", J::Obj(vec![("index".into(), J::Obj(index))]).pretty()));
    }
    for (key, what) in [("number_of_shards", "la cantidad de shards"), ("knn", "k-NN")] {
        if opt(&new.options, key).is_some_and(|v| Some(v) != opt(&old.options, key)) {
            warnings.push(format!("{name}: {what} no se cambia en un índice existente; se deja como está."));
        }
    }

    // Aliases.
    let aliases = |t: &TableSchema| -> BTreeSet<String> {
        opt(&t.options, "aliases").unwrap_or("").split(',').map(str::trim).filter(|a| !a.is_empty()).map(str::to_string).collect()
    };
    let (oa, na) = (aliases(old), aliases(new));
    let action = |verb: &str, a: &str| J::Obj(vec![(verb.into(), J::Obj(vec![("index".into(), J::Str(name.into())), ("alias".into(), J::Str(a.into()))]))]);
    let actions: Vec<J> = oa.difference(&na).map(|a| action("remove", a)).chain(na.difference(&oa).map(|a| action("add", a))).collect();
    if !actions.is_empty() {
        out.push(format!("POST /_aliases\n{}", J::Obj(vec![("actions".into(), J::Arr(actions))]).pretty()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn idx() -> TableSchema {
        TableSchema {
            kind: "index".into(),
            name: "clientes".into(),
            columns: vec![col("nombre", "text"), col("edad", "integer"), col("baja", "date"), col("dir", "nested"), col("dir.calle", "keyword")],
            options: [("number_of_shards".to_string(), "1".to_string()), ("aliases".to_string(), "a1".to_string())].into(),
            ..Default::default()
        }
    }

    #[test]
    fn mapping_grows_and_the_rest_warns() {
        let mut new = idx();
        new.columns[1].data_type = "long".into();
        new.columns[0].comment = Some("Nombre".into());
        new.columns.remove(2);
        new.columns.push(col("email", "keyword"));
        new.columns.push(col("dir.cp", "keyword"));
        new.options.insert("number_of_replicas".into(), "0".into());
        new.options.insert("aliases".into(), "a2".into());
        new.comment = Some("Clientes".into());
        let s = sync_script(&[TableChange::Alter { old: idx(), new }], false).unwrap();
        assert_eq!(s.statements.len(), 3, "{:?}", s.statements);
        let m: serde_json::Value = serde_json::from_str(s.statements[0].strip_prefix("PUT /clientes/_mapping\n").unwrap()).unwrap();
        assert_eq!(
            m,
            serde_json::json!({
                "_meta": {"description": "Clientes"},
                "properties": {
                    "nombre": {"type": "text", "meta": {"description": "Nombre"}},
                    "email": {"type": "keyword"},
                    "dir": {"type": "nested", "properties": {"cp": {"type": "keyword"}}}
                }
            })
        );
        assert_eq!(s.statements[1], "PUT /clientes/_settings\n{\n  \"index\": {\n    \"number_of_replicas\": 0\n  }\n}");
        let a: serde_json::Value = serde_json::from_str(s.statements[2].strip_prefix("POST /_aliases\n").unwrap()).unwrap();
        assert_eq!(a, serde_json::json!({"actions": [{"remove": {"index": "clientes", "alias": "a1"}}, {"add": {"index": "clientes", "alias": "a2"}}]}));
        let w = s.warnings.join("\n");
        assert!(w.contains("reindexar") && w.contains("edad") && w.contains("baja"), "{w}");
        assert_eq!(s.warnings.len(), 2);
    }

    #[test]
    fn analysis_changes_on_a_closed_index() {
        let mut new = idx();
        new.options.insert("analysis".into(), r#"{"analyzer":{"es":{"type":"standard","stopwords":"_spanish_"}}}"#.into());
        let s = sync_script(&[TableChange::Alter { old: idx(), new: new.clone() }], false).unwrap();
        assert_eq!(s.statements.len(), 3, "{:?}", s.statements);
        assert_eq!(s.statements[0], "POST /clientes/_close");
        assert!(s.statements[1].starts_with("PUT /clientes/_settings\n{\n  \"analysis\""), "{}", s.statements[1]);
        assert_eq!(s.statements[2], "POST /clientes/_open");
        // Removing it only warns.
        let s = sync_script(&[TableChange::Alter { old: new, new: idx() }], false).unwrap();
        assert!(s.statements.is_empty() && s.warnings.len() == 1, "{s:?}");
    }

    #[test]
    fn the_description_keeps_the_rest_of_meta() {
        let mut old = idx();
        old.comment = Some("Clientes".into());
        old.options.insert(MAPPINGS_EXTRA.into(), r#"{"_meta":{"owner":"x"}}"#.into());
        let mut new = old.clone();
        new.comment = None;
        let s = sync_script(&[TableChange::Alter { old: old.clone(), new }], false).unwrap();
        let m: serde_json::Value = serde_json::from_str(s.statements[0].strip_prefix("PUT /clientes/_mapping\n").unwrap()).unwrap();
        assert_eq!(m, serde_json::json!({"_meta": {"owner": "x"}}));
        // Without other keys, an empty description takes it off.
        old.options.remove(MAPPINGS_EXTRA);
        let mut new = old.clone();
        new.comment = None;
        let s = sync_script(&[TableChange::Alter { old, new }], false).unwrap();
        let m: serde_json::Value = serde_json::from_str(s.statements[0].strip_prefix("PUT /clientes/_mapping\n").unwrap()).unwrap();
        assert_eq!(m, serde_json::json!({"_meta": {"description": ""}}));
    }

    #[test]
    fn create_and_drop() {
        let mut old = idx();
        old.name = "viejo".into();
        let s = sync_script(&[TableChange::Create { table: idx() }, TableChange::Drop { table: old }], true).unwrap();
        assert_eq!(s.statements[0], "DELETE /viejo");
        assert!(s.statements[1].starts_with("PUT /clientes\n{"), "{}", s.statements[1]);
    }
}
