//! Schema sync in the console syntax: collections (cores) are created and
//! dropped with DBine's `PUT` / `DELETE /solr/<name>`, and fields change
//! through the Schema API in one `POST /solr/<name>/schema` with
//! `delete-field`, `add-field` and `replace-field` lists. The uniqueKey
//! comes from the configset and can't change.
//!
//! A vector column's field type goes first: `add-field-type` for one the
//! schema doesn't have yet, `replace-field-type` (and a reindex warning)
//! when its vector index changes. A field whose vector index goes is
//! replaced with `indexed: false` and keeps its field type, which other
//! fields may share.

use crate::ddl::{
    check_name, collection_ddl, field_def, vector_field_type, vector_field_types, BUILTIN_FIELDS,
};
use dbine_driver::{ColumnDef, DdlParts, Result, SyncScript, TableChange, TableSchema};
use dbine_driver_elasticsearch::json::J;

const CREATE: DdlParts = DdlParts {
    drop: false,
    if_exists: false,
    create: true,
    indexes: false,
    foreign_keys: false,
};
const DROP: DdlParts = DdlParts {
    drop: true,
    if_exists: false,
    create: false,
    indexes: false,
    foreign_keys: false,
};

/// A column's field type and vector index as [`field_def`] takes them.
fn as_ref(v: &Option<(String, bool)>) -> Option<(&str, bool)> {
    v.as_ref().map(|(n, i)| (n.as_str(), *i))
}

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Fields the configset owns (the uniqueKey and Solr's own).
fn managed(t: &TableSchema, c: &ColumnDef) -> bool {
    let key = t
        .primary_key
        .as_ref()
        .and_then(|k| k.columns.first())
        .map(String::as_str)
        .unwrap_or("id");
    BUILTIN_FIELDS.contains(&c.name.as_str()) || c.name == key || c.name.trim().is_empty()
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut alters, mut creates, mut warnings) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(collection_ddl(table, CREATE)?),
            TableChange::Drop { table } => {
                warnings.push(format!(
                    "Se borra la colección {} con todos sus documentos.",
                    table.name
                ));
                drops.push(collection_ddl(table, DROP)?);
            }
            TableChange::Alter { old, new } => {
                let name = new.name.trim();
                check_name(name)?;
                let key = |t: &TableSchema| {
                    t.primary_key
                        .as_ref()
                        .and_then(|k| k.columns.first().cloned())
                };
                if key(old) != key(new) {
                    warnings.push(format!("La clave única (uniqueKey) de {name} viene del configset y no se cambia con la Schema API; se deja como está."));
                }
                let (oc, nc): (Vec<&ColumnDef>, Vec<&ColumnDef>) = (
                    old.columns.iter().filter(|c| !managed(old, c)).collect(),
                    new.columns.iter().filter(|c| !managed(new, c)).collect(),
                );
                let mut delete = Vec::new();
                let mut add = Vec::new();
                let mut replace = Vec::new();
                // Vector field types: the new ones, and the ones that change.
                let old_types = vector_field_types(old)?;
                let mut add_types = Vec::new();
                let mut replace_types = Vec::new();
                for (ty, def, indexed) in vector_field_types(new)? {
                    match old_types.iter().find(|(o, _, _)| *o == ty) {
                        None => add_types.push(J::Obj(def)),
                        // Only fields without a vector index use it: its
                        // distance and parameters don't matter to them.
                        Some(_) if !indexed => {}
                        Some((_, d, _)) if *d != def => {
                            warnings.push(format!(
                                "{name}: cambia el tipo de campo vectorial «{ty}» (distancia, parámetros del índice). Lo usan todos sus campos y los vectores ya indexados no cambian: hay que reindexarlos."
                            ));
                            replace_types.push(J::Obj(def));
                        }
                        Some(_) => {}
                    }
                }
                let vector_of =
                    |t: &TableSchema, c: &ColumnDef| -> Result<Option<(String, bool)>> {
                        Ok(vector_field_type(t, c)?.map(|(n, _, indexed)| (n, indexed)))
                    };

                for c in oc
                    .iter()
                    .filter(|c| !nc.iter().any(|n| eq_name(&n.name, &c.name)))
                {
                    warnings.push(format!("Se borra el campo {name}.{} del esquema: sus valores ya indexados dejan de poder buscarse.", c.name));
                    delete.push(J::Obj(vec![("name".into(), J::Str(c.name.clone()))]));
                }
                for c in nc
                    .iter()
                    .filter(|c| !oc.iter().any(|o| eq_name(&o.name, &c.name)))
                {
                    add.push(J::Obj(field_def(c, as_ref(&vector_of(new, c)?))?));
                }
                for n in &nc {
                    let Some(o) = oc.iter().find(|o| eq_name(&o.name, &n.name)) else {
                        continue;
                    };
                    let (nv, ov) = (vector_of(new, n)?, vector_of(old, o)?);
                    let nd = field_def(n, as_ref(&nv))?;
                    let od = field_def(o, as_ref(&ov))?;
                    if J::Obj(od).compact() != J::Obj(nd.clone()).compact() {
                        let index_only = o.data_type == n.data_type
                            && matches!((&ov, &nv), (Some((_, a)), Some((_, b))) if a != b);
                        warnings.push(if index_only {
                            format!(
                                "{name}.{}: cambia su índice vectorial. Los documentos ya indexados no cambian: hay que reindexarlos.",
                                n.name
                            )
                        } else {
                            format!(
                                "{name}.{}: cambia su definición ({} → {}). Los documentos ya indexados no cambian: hay que reindexarlos.",
                                n.name, o.data_type, n.data_type
                            )
                        });
                        replace.push(J::Obj(nd));
                    }
                }
                if let Some(o) = ["configSet", "numShards", "replicationFactor"]
                    .iter()
                    .find(|k| old.options.get(**k) != new.options.get(**k))
                {
                    warnings.push(format!(
                        "{name}: {o} se fija al crear la colección; se deja como está."
                    ));
                }
                let mut cmds = Vec::new();
                for (cmd, list) in [
                    ("delete-field", delete),
                    ("add-field-type", add_types),
                    ("replace-field-type", replace_types),
                    ("add-field", add),
                    ("replace-field", replace),
                ] {
                    if !list.is_empty() {
                        let items: Vec<String> = list
                            .iter()
                            .map(|f| format!("    {}", f.compact()))
                            .collect();
                        cmds.push(format!("  \"{cmd}\": [\n{}\n  ]", items.join(",\n")));
                    }
                }
                if !cmds.is_empty() {
                    alters.push(format!(
                        "POST /solr/{name}/schema\n{{\n{}\n}}",
                        cmds.join(",\n")
                    ));
                }
            }
        }
    }
    let statements = [drops, alters, creates]
        .into_iter()
        .flatten()
        .filter(|s: &String| !s.trim().is_empty())
        .collect();
    Ok(SyncScript {
        statements,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{IndexDef, KeyDef, VectorIndex};

    #[test]
    fn vector_field_types_go_first() {
        let old = coll("docs", vec![col("id", "string")]);
        let mut new = old.clone();
        new.columns.push(col("emb", "knn_vector(3)"));
        new.indexes.push(IndexDef {
            name: "emb".into(),
            columns: vec!["emb".into()],
            vector: Some(VectorIndex {
                metric: Some("cosine".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let s = sync_script(&[TableChange::Alter {
            old: old.clone(),
            new: new.clone(),
        }])
        .unwrap();
        assert_eq!(
            s.statements,
            ["POST /solr/docs/schema\n{\n  \"add-field-type\": [\n    {\"name\":\"knn_vector_3_cosine\",\"class\":\"solr.DenseVectorField\",\"vectorDimension\":3,\"similarityFunction\":\"cosine\"}\n  ],\n  \"add-field\": [\n    {\"name\":\"emb\",\"type\":\"knn_vector_3_cosine\"}\n  ]\n}"]
        );
        // Its distance changes: a field type of its own, and a reindex.
        let mut l2 = new.clone();
        l2.indexes[0].vector.as_mut().unwrap().metric = Some("l2".into());
        let s = sync_script(&[TableChange::Alter {
            old: new.clone(),
            new: l2,
        }])
        .unwrap();
        assert!(
            s.statements[0].contains("\"add-field-type\"")
                && s.statements[0].contains("knn_vector_3_euclidean"),
            "{:?}",
            s.statements
        );
        assert!(s.statements[0].contains("\"replace-field\""));
        assert!(!s.warnings.is_empty());
        // Read back with its field type's name: that type is replaced.
        let mut named = new.clone();
        named.columns[1]
            .options
            .insert("fieldType".into(), "knn".into());
        let mut changed = named.clone();
        changed.indexes[0].vector.as_mut().unwrap().metric = Some("ip".into());
        let s = sync_script(&[TableChange::Alter {
            old: named,
            new: changed,
        }])
        .unwrap();
        assert!(
            s.statements[0].contains("\"replace-field-type\"")
                && s.statements[0].contains("dot_product"),
            "{:?}",
            s.statements
        );
        assert!(!s.statements[0].contains("\"replace-field\""));
        assert!(s.warnings[0].contains("reindex"), "{:?}", s.warnings);
    }

    #[test]
    fn a_vector_index_dropped_leaves_the_field_unindexed() {
        // As describe reads it: the field type's name in fieldType.
        let mut old = coll("docs", vec![col("id", "string")]);
        let mut emb = col("emb", "knn_vector(3)");
        emb.options
            .insert("fieldType".into(), "knn_vector_3_cosine".into());
        old.columns.push(emb.clone());
        let mut other = emb.clone();
        other.name = "emb2".into();
        old.columns.push(other);
        for f in ["emb", "emb2"] {
            old.indexes.push(IndexDef {
                name: f.into(),
                columns: vec![f.into()],
                vector: Some(VectorIndex {
                    metric: Some("cosine".into()),
                    ..Default::default()
                }),
                options: [("similarityFunction".to_string(), "cosine".to_string())].into(),
                ..Default::default()
            });
        }
        let mut new = old.clone();
        new.indexes.remove(0);
        let s = sync_script(&[TableChange::Alter {
            old: old.clone(),
            new: new.clone(),
        }])
        .unwrap();
        // The shared field type stays (emb2 still searches by cosine), and
        // emb stops being indexed.
        assert_eq!(
            s.statements,
            ["POST /solr/docs/schema\n{\n  \"replace-field\": [\n    {\"name\":\"emb\",\"type\":\"knn_vector_3_cosine\",\"indexed\":false}\n  ]\n}"]
        );
        assert_eq!(
            s.warnings,
            ["docs.emb: cambia su índice vectorial. Los documentos ya indexados no cambian: hay que reindexarlos."]
        );
        // And back: the field is indexed again with its type.
        let s = sync_script(&[TableChange::Alter { old: new, new: old }]).unwrap();
        assert!(
            s.statements[0].contains(r#"{"name":"emb","type":"knn_vector_3_cosine"}"#),
            "{:?}",
            s.statements
        );
        assert!(!s.statements[0].contains("field-type"));
        // A designer model's field (no fieldType) losing its index: a
        // field type that only holds the vector.
        let designed = |ix: bool| {
            let mut t = coll(
                "docs",
                vec![col("id", "string"), col("emb", "knn_vector(3)")],
            );
            if ix {
                t.indexes.push(IndexDef {
                    name: "emb".into(),
                    columns: vec!["emb".into()],
                    vector: Some(VectorIndex {
                        metric: Some("cosine".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
            t
        };
        let s = sync_script(&[TableChange::Alter {
            old: designed(true),
            new: designed(false),
        }])
        .unwrap();
        assert!(
            s.statements[0].contains(
                r#"{"name":"knn_vector_3","class":"solr.DenseVectorField","vectorDimension":3}"#
            ) && s.statements[0]
                .contains(r#"{"name":"emb","type":"knn_vector_3","indexed":false}"#),
            "{:?}",
            s.statements
        );
    }

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            nullable: true,
            ..Default::default()
        }
    }

    fn coll(name: &str, cols: Vec<ColumnDef>) -> TableSchema {
        TableSchema {
            kind: "collection".into(),
            name: name.into(),
            columns: cols,
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn alter_fields() {
        let old = coll(
            "films",
            vec![
                col("id", "string"),
                col("_version_", "plong"),
                col("title", "text_general"),
                col("year", "pint"),
                col("old", "string"),
            ],
        );
        let mut year = col("year", "plong");
        year.options.insert("docValues".into(), "true".into());
        let new = coll(
            "films",
            vec![
                col("id", "string"),
                col("title", "text_general"),
                year,
                col("genre", "string[]"),
            ],
        );
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
            TableChange::Create {
                table: coll(
                    "films",
                    vec![col("id", "string"), col("title", "text_general")],
                ),
            },
            TableChange::Drop {
                table: coll("old", vec![]),
            },
        ])
        .unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DELETE /solr/old",
                "PUT /solr/films\n\n# Ya definidos por el configset: id.\nPOST /solr/films/schema\n{\n  \"add-field\": [\n    {\"name\":\"title\",\"type\":\"text_general\"}\n  ]\n}",
            ]
        );
        assert_eq!(
            s.warnings,
            vec!["Se borra la colección old con todos sus documentos."]
        );
    }
}
