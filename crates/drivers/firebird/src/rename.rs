//! "Renombrar…" on Firebird: only columns, with
//! `ALTER TABLE t ALTER COLUMN a TO b`. Firebird has no RENAME for tables,
//! views, indexes or routines.
//!
//! Firebird refuses the rename while anything uses the column (it records
//! every use in RDB$DEPENDENCIES): a view, a procedure, a function or a
//! trigger ("Column X from table T is referenced in V"), a CHECK ("…
//! referenced in CHECK_n"), and a primary, unique or foreign key ("Cannot
//! update index segment used by an Integrity Constraint"). A plain index
//! follows the new name by itself. So the rewritten code is dropped before
//! the rename and created after it (`DropCreate`); the definitions DBine
//! reads are `CREATE OR ALTER …`.

use dbine_driver::rename::{quote_new, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::sql::{name_tokens, quote_ident, Quote, ScriptDialect, TokenKind};
use dbine_driver::{Error, Result, SyncScript, TableSchema};

pub fn spec() -> RenameSpec {
    RenameSpec {
        columns: true,
        replace: ReplaceStyle::DropCreate,
        references: ReferenceStyle::Sql,
        fold: Fold::Upper,
        // A drop only takes effect at commit, while the rename checks the
        // dependencies at once: in one transaction the rename still sees
        // the dropped view and fails. Each statement commits on its own.
        transactional: false,
        note: Some(
            "Firebird solo renombra columnas, y rechaza el cambio mientras algo use la columna: una vista, un procedimiento, \
             una función, un trigger, un CHECK o una clave primaria, única o foránea. Por eso lo que se reescribe se borra \
             antes y se vuelve a crear después con CREATE OR ALTER, y lo que quede sin reescribir (un trigger que usa NEW.columna, \
             por ejemplo) hace fallar el cambio. El script no corre en una transacción: si el cambio falla, lo ya borrado \
             queda borrado y se crea con el resto del script."
                .into(),
        ),
        ..Default::default()
    }
}

pub fn script(req: &RenameRequest, dialect: &ScriptDialect) -> Result<SyncScript> {
    let RenameTarget::Column { table, column } = &req.target else {
        return Err(Error::Unsupported(
            "Firebird solo renombra columnas: no tiene RENAME para tablas, vistas, índices ni procedimientos.".into(),
        ));
    };
    let mut warnings = Vec::new();
    if let Some(t) = &req.table {
        refuse_constraints(t, column, dialect)?;
        if t.indexes.iter().any(|i| i.unique && i.columns.iter().any(|c| c == column)) {
            warnings.push("Si la columna es parte de una restricción UNIQUE, Firebird rechaza el cambio.".to_string());
        }
    }
    let new = quote_new(&req.new_name, dialect, Fold::Upper, false);
    // No schemas: the bare table name.
    let statement = format!("ALTER TABLE {} ALTER COLUMN {} TO {new}", quote_ident(Quote::Double, &table.name), quote_ident(Quote::Double, column));
    Ok(SyncScript { statements: vec![statement], warnings })
}

/// The keys and CHECKs of the table that use the column: Firebird refuses
/// the rename while they exist.
fn refuse_constraints(t: &TableSchema, column: &str, dialect: &ScriptDialect) -> Result<()> {
    let keyed = t.primary_key.as_ref().is_some_and(|k| k.columns.iter().any(|c| c == column))
        || t.foreign_keys.iter().any(|f| f.columns.iter().any(|c| c == column));
    if keyed {
        return Err(Error::Unsupported(format!(
            "«{column}» es parte de la clave primaria o de una clave foránea: Firebird no renombra una columna que usa una restricción."
        )));
    }
    for check in &t.checks {
        let uses = name_tokens(&check.expression, dialect).iter().any(|tok| {
            let quoted = check.expression[tok.start..].starts_with('"');
            tok.kind == TokenKind::Name && if quoted { tok.text == column } else { tok.text.to_uppercase() == column }
        });
        if uses {
            let name = check.name.as_deref().map(|n| format!(" {n}")).unwrap_or_default();
            return Err(Error::Unsupported(format!(
                "«{column}» se usa en la restricción CHECK{name}: Firebird no renombra una columna que usa una restricción."
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{kinds, CheckDef, ForeignKeyDef, IndexDef, KeyDef, ObjectRef};

    fn table() -> ObjectRef {
        ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "CLIENTES".into() }
    }

    fn run(target: RenameTarget, new_name: &str, schema: Option<TableSchema>) -> Result<SyncScript> {
        script(&RenameRequest { target, new_name: new_name.into(), table: schema, definition: None }, &ScriptDialect::firebird())
    }

    fn column(c: &str) -> RenameTarget {
        RenameTarget::Column { table: table(), column: c.into() }
    }

    #[test]
    fn renames_a_column() {
        let s = run(column("PEPE"), "NOMBRE", None).unwrap();
        assert_eq!(s.statements, [r#"ALTER TABLE "CLIENTES" ALTER COLUMN "PEPE" TO NOMBRE"#]);
        assert!(s.warnings.is_empty());
        // Unquoted names fold to upper case: anything else is quoted.
        let s = run(column("PEPE"), "Nombre", None).unwrap();
        assert_eq!(s.statements, [r#"ALTER TABLE "CLIENTES" ALTER COLUMN "PEPE" TO "Nombre""#]);
        let s = run(RenameTarget::Column { table: ObjectRef { name: "Mixed".into(), ..table() }, column: "a b".into() }, "SELECT", None).unwrap();
        assert_eq!(s.statements, [r#"ALTER TABLE "Mixed" ALTER COLUMN "a b" TO "SELECT""#]);
    }

    #[test]
    fn refuses_columns_under_a_constraint() {
        let mut t = TableSchema { name: "CLIENTES".into(), ..Default::default() };
        t.primary_key = Some(KeyDef { name: None, columns: vec!["ID".into()] });
        t.foreign_keys = vec![ForeignKeyDef { columns: vec!["PAIS".into()], ref_table: "PAISES".into(), ref_columns: vec!["ID".into()], ..Default::default() }];
        t.checks = vec![CheckDef { name: Some("CK_PEPE".into()), expression: "(pepe >= 0 AND \"Otra\" > 0)".into() }];
        t.indexes = vec![IndexDef { name: "UQ_COD".into(), columns: vec!["COD".into()], unique: true, ..Default::default() }];
        for c in ["ID", "PAIS"] {
            assert!(matches!(run(column(c), "X", Some(t.clone())), Err(Error::Unsupported(m)) if m.contains("clave")), "{c}");
        }
        assert!(matches!(run(column("PEPE"), "X", Some(t.clone())), Err(Error::Unsupported(m)) if m.contains("CHECK CK_PEPE")));
        // A quoted name matches only its exact case.
        assert!(matches!(run(column("Otra"), "X", Some(t.clone())), Err(Error::Unsupported(_))));
        assert!(run(column("OTRA"), "X", Some(t.clone())).is_ok());
        // A unique index may be a UNIQUE constraint: the server decides.
        let s = run(column("COD"), "CODIGO", Some(t.clone())).unwrap();
        assert!(s.warnings[0].contains("UNIQUE"), "{:?}", s.warnings);
        assert!(run(column("FECHA"), "ALTA", Some(t)).unwrap().warnings.is_empty());
    }

    #[test]
    fn refuses_everything_but_columns() {
        for target in [
            RenameTarget::Object { object: table(), parent: None },
            RenameTarget::Object { object: ObjectRef { kind: kinds::PROCEDURE.into(), ..table() }, parent: None },
            RenameTarget::Index { table: table(), index: "IX".into() },
            RenameTarget::Constraint { table: table(), constraint: "CK".into() },
        ] {
            assert!(matches!(run(target, "X", None), Err(Error::Unsupported(m)) if m.contains("solo renombra columnas")));
        }
        let s = spec();
        assert!(s.kinds.is_empty() && s.columns && !s.indexes && !s.constraints && !s.schemas);
        assert_eq!((s.fold, s.replace), (Fold::Upper, ReplaceStyle::DropCreate));
    }
}
