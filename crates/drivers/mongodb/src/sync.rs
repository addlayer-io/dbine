//! Schema sync in the driver's shell language: collections and views are
//! created and dropped, indexes are dropped and made again, and the
//! validator (with its level and action), a view's definition and the TTL
//! of clustered / time series collections change with `collMod`. Documents
//! have no fixed schema: field changes that the validator doesn't carry are
//! warnings.

use crate::ddl::{collection_options, index_parts, opt, q, relaxed, table_ddl};
use dbine_driver::{kinds, ColumnDef, DdlParts, IndexDef, Result, SyncScript, TableChange, TableSchema};
use serde_json::json;

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: false };
const INDEXES: DdlParts = DdlParts { drop: false, if_exists: false, create: false, indexes: true, foreign_keys: false };
const DROP: DdlParts = DdlParts { drop: true, if_exists: false, create: false, indexes: false, foreign_keys: false };

/// Options fixed at creation: `collMod` can't change them.
const CREATION_ONLY: &[&str] =
    &["capped", "size", "max", "timeField", "metaField", "granularity", "clustered", crate::ddl::CLUSTERED_NAME, "collation"];

#[derive(Default)]
struct Plan {
    drops: Vec<String>,
    pre: Vec<String>,
    post: Vec<String>,
    creates: Vec<String>,
    warnings: Vec<String>,
}

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

fn required(c: &ColumnDef) -> bool {
    matches!(c.options.get("required").map(|s| s.trim()), Some("true" | "1"))
}

fn ix_same(a: &IndexDef, b: &IndexDef) -> bool {
    match (index_parts(a), index_parts(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let mut p = Plan::default();
    for ch in changes {
        match ch {
            TableChange::Create { table } => p.creates.push(table_ddl(table, CREATE)?),
            TableChange::Drop { table } => {
                p.warnings.push(if table.kind == kinds::VIEW {
                    format!("Se borra la vista {}.", table.name)
                } else {
                    format!("Se borra la colección {} con todos sus documentos.", table.name)
                });
                p.drops.push(table_ddl(table, DROP)?);
            }
            TableChange::Alter { old, new } => alter(old, new, &mut p)?,
        }
    }
    let statements = [p.drops, p.pre, p.post, p.creates].into_iter().flatten().filter(|s| !s.trim().is_empty()).collect();
    Ok(SyncScript { statements, warnings: p.warnings })
}

/// Per-field warnings: documents keep what they have.
pub(crate) fn field_warnings(t: &str, old: &[ColumnDef], new: &[ColumnDef], out: &mut Vec<String>) {
    for c in old.iter().filter(|c| !new.iter().any(|n| eq_name(&n.name, &c.name))) {
        out.push(format!("Los documentos no tienen esquema fijo: el campo {t}.{} no se borra de los documentos que ya existen.", c.name));
    }
    for c in new.iter().filter(|c| !old.iter().any(|o| eq_name(&o.name, &c.name))) {
        out.push(format!("Los documentos no tienen esquema fijo: el campo {t}.{} no se agrega a los documentos que ya existen.", c.name));
    }
    for n in new {
        if let Some(o) = old.iter().find(|o| eq_name(&o.name, &n.name)) {
            if squash(&o.data_type) != squash(&n.data_type) || o.nullable != n.nullable || required(o) != required(n) {
                out.push(format!("Los documentos no tienen esquema fijo: el campo {t}.{} no se cambia.", n.name));
            }
        }
    }
}

fn alter(old: &TableSchema, new: &TableSchema, p: &mut Plan) -> Result<()> {
    let tname = new.name.as_str();
    let coll = format!("db.getCollection({})", q(tname));

    if new.kind == kinds::VIEW || old.kind == kinds::VIEW {
        if old.kind != new.kind {
            p.warnings.push(format!("{tname} pasa de colección a vista o al revés: borrala y creala a mano."));
            return Ok(());
        }
        let pipeline = |t: &TableSchema| opt(t, "pipeline").map(|s| relaxed(s, "pipeline de la vista")).transpose();
        let (op, np) = (pipeline(old)?.unwrap_or(json!([])), pipeline(new)?.unwrap_or(json!([])));
        if opt(old, "viewOn") != opt(new, "viewOn") || op != np {
            let source = opt(new, "viewOn").unwrap_or_default();
            p.post.push(format!("db.runCommand({{ \"collMod\": {}, \"viewOn\": {}, \"pipeline\": {np} }})", q(tname), q(source)));
        }
        return Ok(());
    }

    // collMod: validator (with level and action) and TTL.
    let (oo, no) = (collection_options(old)?, collection_options(new)?);
    let mut modify: Vec<String> = Vec::new();
    let validator_changed = oo.get("validator") != no.get("validator");
    if validator_changed {
        modify.push(format!("\"validator\": {}", no.get("validator").cloned().unwrap_or(json!({}))));
        p.warnings.push(format!(
            "El validador de {tname} cambia: vale para los documentos que se inserten o modifiquen; los que ya existen no se revisan."
        ));
    }
    for k in ["validationLevel", "validationAction", "expireAfterSeconds"] {
        if oo.get(k) != no.get(k) {
            match no.get(k) {
                Some(v) => modify.push(format!("{}: {v}", q(k))),
                None if k == "expireAfterSeconds" => modify.push("\"expireAfterSeconds\": \"off\"".into()),
                None => modify.push(format!("{}: {}", q(k), q(if k == "validationLevel" { "strict" } else { "error" }))),
            }
        }
    }
    if !modify.is_empty() {
        p.post.push(format!("db.runCommand({{ \"collMod\": {}, {} }})", q(tname), modify.join(", ")));
    }
    if CREATION_ONLY.iter().any(|k| opt(old, k) != opt(new, k)) {
        p.warnings.push(format!(
            "{tname}: capped, serie temporal y colección agrupada se fijan al crearla y no se cambian; se deja como está."
        ));
    }
    if !validator_changed {
        field_warnings(tname, &old.columns, &new.columns, &mut p.warnings);
    }

    // Indexes, by name.
    for o in &old.indexes {
        if new.indexes.iter().find(|n| eq_name(&n.name, &o.name)).is_none_or(|n| !ix_same(o, n)) {
            p.pre.push(format!("{coll}.dropIndex({})", q(&o.name)));
        }
    }
    let add: Vec<IndexDef> = new
        .indexes
        .iter()
        .filter(|n| n.name.trim().is_empty() || old.indexes.iter().find(|o| eq_name(&o.name, &n.name)).is_none_or(|o| !ix_same(o, n)))
        .cloned()
        .collect();
    if !add.is_empty() {
        p.post.push(table_ddl(&TableSchema { indexes: add, ..new.clone() }, INDEXES)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn coll(cols: Vec<ColumnDef>, indexes: Vec<IndexDef>) -> TableSchema {
        let mut t = TableSchema { kind: kinds::COLLECTION.into(), name: "users".into(), columns: cols, indexes, ..Default::default() };
        t.options.insert("validate_fields".into(), "false".into());
        t
    }

    fn ix(name: &str, cols: &[&str], unique: bool) -> IndexDef {
        IndexDef { name: name.into(), columns: cols.iter().map(|s| s.to_string()).collect(), unique, ..Default::default() }
    }

    #[test]
    fn alter_fields_and_indexes() {
        let old = coll(vec![col("_id", "objectId"), col("name", "string"), col("age", "int"), col("legacy", "string")], vec![ix("name_1", &["name"], false), ix("age_1", &["age"], false)]);
        let new = coll(vec![col("_id", "objectId"), col("name", "string"), col("age", "long"), col("email", "string")], vec![ix("name_1", &["name"], true), ix("age_1", &["age"], false), ix("email_1", &["email"], false)]);
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "db.getCollection(\"users\").dropIndex(\"name_1\")",
                "db.getCollection(\"users\").createIndex({\"name\":1}, {\"name\":\"name_1\",\"unique\":true})\ndb.getCollection(\"users\").createIndex({\"email\":1}, {\"name\":\"email_1\"})",
            ]
        );
        assert_eq!(
            s.warnings,
            vec![
                "Los documentos no tienen esquema fijo: el campo users.legacy no se borra de los documentos que ya existen.",
                "Los documentos no tienen esquema fijo: el campo users.email no se agrega a los documentos que ya existen.",
                "Los documentos no tienen esquema fijo: el campo users.age no se cambia.",
            ]
        );
    }

    #[test]
    fn validator_goes_by_coll_mod() {
        let old = coll(vec![col("name", "string")], vec![]);
        let mut new = old.clone();
        new.options.insert("validator".into(), "{\"$jsonSchema\": {\"required\": [\"name\"]}}".into());
        new.options.insert("validationAction".into(), "warn".into());
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec!["db.runCommand({ \"collMod\": \"users\", \"validator\": {\"$jsonSchema\":{\"required\":[\"name\"]}}, \"validationAction\": \"warn\" })"]
        );
        assert_eq!(s.warnings.len(), 1);
    }

    #[test]
    fn create_and_drop() {
        let t = coll(vec![col("name", "string")], vec![ix("name_1", &["name"], false)]);
        let gone = TableSchema { name: "old_logs".into(), ..coll(vec![], vec![]) };
        let s = sync_script(&[TableChange::Create { table: t }, TableChange::Drop { table: gone }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "db.getCollection(\"old_logs\").drop()",
                "db.createCollection(\"users\")\ndb.getCollection(\"users\").createIndex({\"name\":1}, {\"name\":\"name_1\"})",
            ]
        );
        assert_eq!(s.warnings, vec!["Se borra la colección old_logs con todos sus documentos."]);
    }
}
