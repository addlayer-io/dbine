//! Schema sync in SQL++: collections are created and dropped, indexes
//! (and the primary index) are dropped and made again. Documents have no
//! schema: field changes are warnings.

use crate::ddl::{path, q, table_ddl};
use dbine_driver::{ColumnDef, DdlParts, IndexDef, Result, SyncScript, TableChange, TableSchema};

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: false };
const INDEXES: DdlParts = DdlParts { drop: false, if_exists: false, create: false, indexes: true, foreign_keys: false };
const DROP: DdlParts = DdlParts { drop: true, if_exists: false, create: false, indexes: false, foreign_keys: false };

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn opt<'a>(t: &'a TableSchema, k: &str) -> Option<&'a str> {
    t.options.get(k).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn ix_same(a: &IndexDef, b: &IndexDef) -> bool {
    let cols = |x: &[String]| x.iter().map(|c| c.trim().to_lowercase()).collect::<Vec<_>>();
    let w = |x: &Option<String>| x.as_deref().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let kind = |x: &IndexDef| x.kind.as_deref().unwrap_or("").to_uppercase();
    cols(&a.columns) == cols(&b.columns) && w(&a.filter) == w(&b.filter) && kind(a) == kind(b) && a.options == b.options
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

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut pre, mut post, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(table_ddl(table, CREATE)?),
            TableChange::Drop { table } => {
                warnings.push(format!("Se borra la colección {} con todos sus documentos.", display(table)));
                drops.push(table_ddl(table, DROP)?);
            }
            TableChange::Alter { old, new } => {
                let tname = display(new);
                let ks = path(new.schema.as_deref(), &new.name);
                let primary = |t: &TableSchema| opt(t, "primary_index") == Some("true");
                if primary(old) && !primary(new) {
                    pre.push(format!("DROP PRIMARY INDEX ON {ks};"));
                } else if !primary(old) && primary(new) {
                    post.push(format!("CREATE PRIMARY INDEX ON {ks};"));
                }
                if opt(old, "max_ttl") != opt(new, "max_ttl") {
                    warnings.push(format!("{tname}: el TTL máximo de una colección no se cambia con SQL++; se deja como está."));
                }
                field_warnings(&tname, &old.columns, &new.columns, &mut warnings);
                for o in &old.indexes {
                    if new.indexes.iter().find(|n| eq_name(&n.name, &o.name)).is_none_or(|n| !ix_same(o, n)) {
                        pre.push(format!("DROP INDEX {} ON {ks};", q(&o.name)));
                    }
                }
                let add: Vec<IndexDef> = new
                    .indexes
                    .iter()
                    .filter(|n| old.indexes.iter().find(|o| eq_name(&o.name, &n.name)).is_none_or(|o| !ix_same(o, n)))
                    .cloned()
                    .collect();
                if !add.is_empty() {
                    post.push(table_ddl(&TableSchema { indexes: add, ..new.clone() }, INDEXES)?);
                }
            }
        }
    }
    let statements = [drops, pre, post, creates].into_iter().flatten().filter(|s: &String| !s.trim().is_empty()).collect();
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn coll(name: &str, cols: Vec<ColumnDef>, indexes: Vec<IndexDef>) -> TableSchema {
        TableSchema { kind: "collection".into(), schema: Some("app.inv".into()), name: name.into(), columns: cols, indexes, ..Default::default() }
    }

    fn ix(name: &str, cols: &[&str]) -> IndexDef {
        IndexDef { name: name.into(), columns: cols.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn alter_indexes_and_fields() {
        let old = coll("items", vec![col("sku", "string"), col("qty", "number")], vec![ix("ix_sku", &["sku"]), ix("ix_old", &["qty"])]);
        let mut new = coll("items", vec![col("sku", "string"), col("qty", "string"), col("tag", "string")], vec![ix("ix_sku", &["sku", "tag"])]);
        new.options.insert("primary_index".into(), "true".into());
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX `ix_sku` ON `app`.`inv`.`items`;",
                "DROP INDEX `ix_old` ON `app`.`inv`.`items`;",
                "CREATE PRIMARY INDEX ON `app`.`inv`.`items`;",
                "CREATE INDEX `ix_sku` ON `app`.`inv`.`items`(`sku`, `tag`);",
            ]
        );
        assert_eq!(
            s.warnings,
            vec![
                "Los documentos no tienen esquema fijo: el campo app.inv.items.tag no se agrega a los documentos que ya existen.",
                "Los documentos no tienen esquema fijo: el campo app.inv.items.qty no se cambia.",
            ]
        );
    }

    #[test]
    fn create_and_drop() {
        let s = sync_script(&[
            TableChange::Create { table: coll("items", vec![], vec![ix("ix_sku", &["sku"])]) },
            TableChange::Drop { table: coll("old", vec![], vec![]) },
        ])
        .unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP COLLECTION `app`.`inv`.`old`;",
                "CREATE COLLECTION `app`.`inv`.`items`;\nCREATE INDEX `ix_sku` ON `app`.`inv`.`items`(`sku`);",
            ]
        );
        assert_eq!(s.warnings, vec!["Se borra la colección app.inv.old con todos sus documentos."]);
    }
}
