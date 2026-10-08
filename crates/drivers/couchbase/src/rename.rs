//! "Renombrar…": fields only.
//!
//! Couchbase can't rename buckets, scopes or collections (the cluster
//! manager has no such call: a collection is moved by copying its
//! documents). A field is renamed in every document that has it with one
//! SQL++ `UPDATE … SET new = old UNSET old`. The GSI indexes that name it
//! are dropped after the update (it uses them to find the documents) and
//! created again with the new field. SQL++ functions that name the field
//! are listed, not rewritten (`ReferenceStyle::None`).

use crate::ddl::{index_ddl, path, q};
use dbine_driver::rename::{Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle, ReplaceStyle};
use dbine_driver::{Error, IndexDef, ObjectRef, Result, SyncScript, TableSchema};

pub(crate) const NOTE: &str = "Couchbase no renombra buckets, scopes ni colecciones: desde DBine se renombran campos, con un UPDATE que reescribe cada documento que tiene el campo. Hace falta un índice que sirva para encontrarlos (uno sobre el campo o el primario). Las funciones SQL++ que nombran el campo se muestran y no se reescriben.";

pub(crate) fn spec() -> RenameSpec {
    RenameSpec {
        kinds: Vec::new(),
        columns: true,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        replace: ReplaceStyle::DropCreate,
        references: ReferenceStyle::None,
        fold: Fold::None,
        transactional: false,
        note: Some(NOTE.into()),
        ..Default::default()
    }
}

pub(crate) fn script(req: &RenameRequest) -> Result<SyncScript> {
    match &req.target {
        RenameTarget::Column { table, column } => field(table, column, &req.new_name, req.table.as_ref()),
        RenameTarget::Object { .. } => Err(Error::Unsupported("Couchbase no renombra buckets, scopes ni colecciones".into())),
        _ => Err(Error::Unsupported("desde DBine Couchbase solo renombra campos".into())),
    }
}

/// `text` (an index key or condition as `system:indexes` writes it, every
/// name in backticks) with the top-level field `` `old` `` named `new`: not
/// after a `.` (a field of another object).
fn rename_in(text: &str, old: &str, new: &str) -> Option<String> {
    let from = q(old);
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text;
    while let Some(p) = rest.find(&from) {
        let prev = rest[..p].chars().next_back().or_else(|| out.chars().next_back());
        out.push_str(&rest[..p]);
        if prev == Some('.') || prev == Some('`') {
            out.push_str(&from);
        } else {
            out.push_str(&q(new));
            changed = true;
        }
        rest = &rest[p + from.len()..];
    }
    out.push_str(rest);
    changed.then_some(out)
}

fn renamed_index(ix: &IndexDef, old: &str, new: &str) -> Option<IndexDef> {
    let mut out = ix.clone();
    let mut changed = false;
    for c in &mut out.columns {
        if let Some(n) = rename_in(c, old, new) {
            *c = n;
            changed = true;
        }
    }
    if let Some(n) = out.filter.as_deref().and_then(|f| rename_in(f, old, new)) {
        out.filter = Some(n);
        changed = true;
    }
    changed.then_some(out)
}

fn field(table: &ObjectRef, old: &str, new: &str, schema: Option<&TableSchema>) -> Result<SyncScript> {
    if matches!(old, "_id" | "meta_id") {
        return Err(Error::Unsupported("la clave del documento (META().id) no es un campo y no se puede renombrar".into()));
    }
    let ks = path(table.schema(), &table.name);
    let name = &table.name;
    let mut statements = vec![format!("UPDATE {ks} SET {} = {} UNSET {} WHERE {} IS NOT MISSING;", q(new), q(old), q(old), q(old))];
    let mut warnings = vec![format!(
        "Renombrar un campo reescribe cada documento de «{name}» que lo tiene: en una colección grande tarda. No es atómico: si se corta, volver a correrlo termina el trabajo."
    )];
    match schema {
        Some(t) => {
            let mut names = Vec::new();
            for ix in &t.indexes {
                if let Some(r) = renamed_index(ix, old, new) {
                    statements.push(format!("DROP INDEX {} ON {ks};", q(&ix.name)));
                    statements.push(index_ddl(&ks, &r, false)?);
                    names.push(format!("«{}»", ix.name));
                }
            }
            if !names.is_empty() {
                warnings.push(format!("Los índices que usan el campo se borran después del UPDATE y se vuelven a crear con el campo nuevo: {}.", names.join(", ")));
            }
        }
        None => warnings.push(format!("No se pudo leer «{name}»: no se revisaron sus índices.")),
    }
    warnings.push(format!("Las aplicaciones que sigan escribiendo «{old}» lo vuelven a crear en los documentos nuevos."));
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::kinds;

    fn coll() -> ObjectRef {
        ObjectRef { kind: kinds::COLLECTION.into(), schema: Some("app.ventas".into()), name: "clientes".into() }
    }

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    #[test]
    fn only_fields() {
        let s = spec();
        assert!(s.kinds.is_empty() && s.columns && !s.indexes && !s.schemas && !s.transactional);
        assert_eq!(s.references, ReferenceStyle::None);
        assert!(matches!(script(&req(RenameTarget::Object { object: coll(), parent: None }, "x")), Err(Error::Unsupported(_))));
        assert!(matches!(script(&req(RenameTarget::Schema { database: None, schema: "app.ventas".into() }, "x")), Err(Error::Unsupported(_))));
        assert!(matches!(script(&req(RenameTarget::Index { table: coll(), index: "i".into() }, "x")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn field_with_update_and_indexes() {
        let mut t = TableSchema { kind: kinds::COLLECTION.into(), schema: Some("app.ventas".into()), name: "clientes".into(), ..Default::default() };
        t.indexes = vec![
            IndexDef { name: "ix_pepe".into(), columns: vec!["`pepe`".into(), "`fecha` DESC".into()], ..Default::default() },
            IndexDef { name: "ix_otro".into(), columns: vec!["`otro`".into()], filter: Some("(`pepe` = \"x\")".into()), ..Default::default() },
            IndexDef { name: "ix_dir".into(), columns: vec!["(`dir`.`pepe`)".into()], ..Default::default() },
            IndexDef { name: "#primary".into(), kind: Some("primary".into()), ..Default::default() },
        ];
        let mut r = req(RenameTarget::Column { table: coll(), column: "pepe".into() }, "Pe`pe");
        r.table = Some(t);
        let s = script(&r).unwrap();
        assert_eq!(
            s.statements,
            [
                "UPDATE `app`.`ventas`.`clientes` SET `Pe``pe` = `pepe` UNSET `pepe` WHERE `pepe` IS NOT MISSING;",
                "DROP INDEX `ix_pepe` ON `app`.`ventas`.`clientes`;",
                "CREATE INDEX `ix_pepe` ON `app`.`ventas`.`clientes`(`Pe``pe`, `fecha` DESC);",
                "DROP INDEX `ix_otro` ON `app`.`ventas`.`clientes`;",
                "CREATE INDEX `ix_otro` ON `app`.`ventas`.`clientes`(`otro`) WHERE (`Pe``pe` = \"x\");",
            ]
        );
        assert!(s.warnings.iter().any(|w| w.contains("«ix_pepe», «ix_otro»")));
        assert!(matches!(script(&req(RenameTarget::Column { table: coll(), column: "_id".into() }, "id")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn nested_and_longer_names_are_not_the_field() {
        assert_eq!(rename_in("(`dir`.`pepe`)", "pepe", "x"), None);
        assert_eq!(rename_in("`pepes`", "pepe", "x"), None);
        assert_eq!(rename_in("`pepe`.`calle`", "pepe", "x").as_deref(), Some("`x`.`calle`"));
    }
}
