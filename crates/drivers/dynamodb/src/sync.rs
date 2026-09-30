//! Schema sync with the driver's admin statements: tables are created and
//! dropped, global secondary indexes are dropped and added (UpdateTable,
//! one at a time: each statement waits for the table to be ACTIVE). The key
//! schema and the local indexes only exist with their table, and billing,
//! streams and TTL aren't statements: those are warnings. Items have no
//! schema besides the keys: other attribute changes are warnings too.

use crate::ddl::{
    create_index_statements, index_kind, q, table_keys, table_ddl, IndexKind, BILLING_MODE, READ_CAPACITY, STREAM_VIEW_TYPE,
    TTL_ATTRIBUTE, WRITE_CAPACITY,
};
use dbine_driver::{ColumnDef, DdlParts, IndexDef, Result, SyncScript, TableChange, TableSchema};

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: false };
const DROP: DdlParts = DdlParts { drop: true, if_exists: false, create: false, indexes: false, foreign_keys: false };

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

fn opt<'a>(t: &'a TableSchema, k: &str) -> Option<&'a str> {
    t.options.get(k).map(|v| v.trim()).filter(|v| !v.is_empty() && *v != "none")
}

fn ix_same(a: &IndexDef, b: &IndexDef) -> bool {
    let kind = |i: &IndexDef| matches!(index_kind(i), Ok(IndexKind::Local));
    a.columns == b.columns && kind(a) == kind(b) && crate::ddl::projection(a) == crate::ddl::projection(b)
}

fn is_local(i: &IndexDef) -> bool {
    matches!(index_kind(i), Ok(IndexKind::Local))
}

/// The key attributes with their types.
fn key_shape(t: &TableSchema) -> Result<Vec<(String, String)>> {
    let (h, r) = table_keys(t)?;
    let ty = |n: &str| t.columns.iter().find(|c| c.name == n).map(|c| squash(&c.data_type)).unwrap_or_default();
    Ok(std::iter::once(h).chain(r).map(|n| {
        let ty = ty(&n);
        (n, ty)
    }).collect())
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut pre, mut post, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(table_ddl(table, CREATE)?),
            TableChange::Drop { table } => {
                warnings.push(format!("Se borra la tabla {} con todos sus ítems.", table.name));
                drops.push(table_ddl(table, DROP)?);
            }
            TableChange::Alter { old, new } => {
                let t = new.name.as_str();
                let (ok, nk) = (key_shape(old)?, key_shape(new)?);
                if ok != nk {
                    warnings.push(format!(
                        "La clave primaria de {t} cambia y DynamoDB no la modifica: hay que recrear la tabla (borrarla y crearla de nuevo, con sus ítems). Se deja como está."
                    ));
                }
                let keys: Vec<&str> = ok.iter().chain(&nk).map(|(n, _)| n.as_str()).collect();
                let index_attrs: Vec<&str> = old.indexes.iter().chain(&new.indexes).flat_map(|i| i.columns.iter().map(String::as_str)).collect();
                let is_key = |c: &ColumnDef| keys.contains(&c.name.as_str()) || index_attrs.contains(&c.name.as_str());
                let (oc, nc): (Vec<ColumnDef>, Vec<ColumnDef>) =
                    (old.columns.iter().filter(|c| !is_key(c)).cloned().collect(), new.columns.iter().filter(|c| !is_key(c)).cloned().collect());
                for c in oc.iter().filter(|c| !nc.iter().any(|n| eq_name(&n.name, &c.name))) {
                    warnings.push(format!("Los ítems no tienen esquema fijo: el atributo {t}.{} no se borra de los ítems que ya existen.", c.name));
                }
                for c in nc.iter().filter(|c| !oc.iter().any(|o| eq_name(&o.name, &c.name))) {
                    warnings.push(format!("Los ítems no tienen esquema fijo: el atributo {t}.{} no se agrega a los ítems que ya existen.", c.name));
                }
                for n in &nc {
                    if let Some(o) = oc.iter().find(|o| eq_name(&o.name, &n.name)) {
                        if squash(&o.data_type) != squash(&n.data_type) {
                            warnings.push(format!("Los ítems no tienen esquema fijo: el atributo {t}.{} no se cambia.", n.name));
                        }
                    }
                }

                // Indexes, by name: GSIs go and come back; LSIs can't.
                for o in &old.indexes {
                    if new.indexes.iter().find(|n| n.name == o.name).is_none_or(|n| !ix_same(o, n)) {
                        if is_local(o) {
                            warnings.push(format!("El índice local {t}.{} solo existe con su tabla: no se borra ni se cambia sin recrearla.", o.name));
                        } else {
                            pre.push(format!("DROP INDEX {} ON {};", q(&o.name), q(t)));
                        }
                    }
                }
                let add: Vec<IndexDef> = new
                    .indexes
                    .iter()
                    .filter(|n| old.indexes.iter().find(|o| o.name == n.name).is_none_or(|o| !ix_same(o, n)))
                    .filter(|n| {
                        if is_local(n) && !old.indexes.iter().any(|o| o.name == n.name) {
                            warnings.push(format!("El índice local {t}.{} solo se crea junto con la tabla: no se agrega sin recrearla.", n.name));
                        }
                        !is_local(n)
                    })
                    .cloned()
                    .collect();
                if !add.is_empty() {
                    post.extend(create_index_statements(&TableSchema { indexes: add, ..new.clone() })?);
                }

                let mut settings = Vec::new();
                if [BILLING_MODE, READ_CAPACITY, WRITE_CAPACITY].iter().any(|k| opt(old, k) != opt(new, k)) {
                    settings.push("el modo de facturación y la capacidad");
                }
                if opt(old, STREAM_VIEW_TYPE) != opt(new, STREAM_VIEW_TYPE) {
                    settings.push("el stream");
                }
                if opt(old, TTL_ATTRIBUTE) != opt(new, TTL_ATTRIBUTE) {
                    settings.push("el TTL");
                }
                if !settings.is_empty() {
                    warnings.push(format!("{t}: {} no se cambia desde un script; hacelo desde la consola de AWS o la CLI.", settings.join(", ")));
                }
            }
        }
    }
    let statements = [drops, pre, post, creates].into_iter().flatten().map(|s| s.trim_end().to_string()).filter(|s| !s.is_empty()).collect();
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::KEY_TYPE;

    fn attr(name: &str, ty: &str, key: &str) -> ColumnDef {
        let mut c = ColumnDef { name: name.into(), data_type: ty.into(), nullable: key == "none", ..Default::default() };
        c.options.insert(KEY_TYPE.into(), key.into());
        c
    }

    fn table(name: &str, cols: Vec<ColumnDef>, indexes: Vec<IndexDef>) -> TableSchema {
        let mut t = TableSchema { name: name.into(), columns: cols, indexes, ..Default::default() };
        t.options.insert(BILLING_MODE.into(), "PAY_PER_REQUEST".into());
        t
    }

    fn gsi(name: &str, cols: &[&str]) -> IndexDef {
        IndexDef { name: name.into(), columns: cols.iter().map(|s| s.to_string()).collect(), kind: Some("GSI".into()), ..Default::default() }
    }

    #[test]
    fn alter_indexes_and_attributes() {
        let old = table("orders", vec![attr("pk", "S", "HASH"), attr("status", "S", "none"), attr("total", "N", "none")], vec![gsi("by_status", &["status"])]);
        let new = table(
            "orders",
            vec![attr("pk", "S", "HASH"), attr("customer", "S", "none"), attr("total", "S", "none"), attr("note", "S", "none")],
            vec![gsi("by_customer", &["customer"])],
        );
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX \"by_status\" ON \"orders\";",
                "CREATE INDEX \"by_customer\" ON \"orders\" {\n  \"AttributeDefinitions\": [\n    {\n      \"AttributeName\": \"customer\",\n      \"AttributeType\": \"S\"\n    }\n  ],\n  \"KeySchema\": [\n    {\n      \"AttributeName\": \"customer\",\n      \"KeyType\": \"HASH\"\n    }\n  ],\n  \"Projection\": {\n    \"ProjectionType\": \"ALL\"\n  }\n};",
            ]
        );
        assert_eq!(
            s.warnings,
            vec![
                "Los ítems no tienen esquema fijo: el atributo orders.note no se agrega a los ítems que ya existen.",
                "Los ítems no tienen esquema fijo: el atributo orders.total no se cambia.",
            ]
        );
    }

    #[test]
    fn key_change_is_a_warning() {
        let old = table("t", vec![attr("pk", "S", "HASH")], vec![]);
        let new = table("t", vec![attr("pk", "N", "HASH")], vec![]);
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert!(s.statements.is_empty());
        assert_eq!(
            s.warnings,
            vec!["La clave primaria de t cambia y DynamoDB no la modifica: hay que recrear la tabla (borrarla y crearla de nuevo, con sus ítems). Se deja como está."]
        );
    }

    #[test]
    fn create_and_drop() {
        let s = sync_script(&[
            TableChange::Create { table: table("t", vec![attr("pk", "S", "HASH")], vec![]) },
            TableChange::Drop { table: table("old", vec![attr("pk", "S", "HASH")], vec![]) },
        ])
        .unwrap();
        assert_eq!(s.statements.len(), 2);
        assert_eq!(s.statements[0], "DROP TABLE \"old\";");
        assert!(s.statements[1].starts_with("CREATE TABLE \"t\" {"), "{}", s.statements[1]);
        assert_eq!(s.warnings, vec!["Se borra la tabla old con todos sus ítems."]);
    }
}
