//! "Renombrar…" on SAP HANA: `RENAME TABLE`, `RENAME COLUMN` and
//! `RENAME INDEX`. HANA keeps the text of views, procedures and functions
//! as written: what names the old name stays invalid until DBine puts it
//! back with `CREATE OR REPLACE`.

use dbine_driver::rename::{quote_new, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::sql::{qualified_name, quote_ident, Quote, ScriptDialect};
use dbine_driver::{kinds, Error, Result, SyncScript};

pub fn spec() -> RenameSpec {
    RenameSpec {
        kinds: vec![kinds::TABLE.into()],
        columns: true,
        indexes: true,
        replace: ReplaceStyle::CreateOrReplace,
        references: ReferenceStyle::Sql,
        fold: Fold::Upper,
        // DDL commits by itself unless the session turns `AUTOCOMMIT DDL` off.
        transactional: false,
        note: Some(
            "SAP HANA no actualiza las vistas, procedimientos y funciones que usan el nombre viejo: quedan inválidos hasta \
             que se reponen con CREATE OR REPLACE. En una versión de HANA sin CREATE OR REPLACE ese paso falla y hay que \
             borrarlos y crearlos a mano."
                .into(),
        ),
        ..Default::default()
    }
}

pub fn script(req: &RenameRequest, dialect: &ScriptDialect) -> Result<SyncScript> {
    let new = quote_new(&req.new_name, dialect, Fold::Upper, false);
    let q = |s: &str| quote_ident(Quote::Double, s);
    let statement = match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE => {
            format!("RENAME TABLE {} TO {new}", qualified_name(Quote::Double, object.schema(), &object.name))
        }
        RenameTarget::Column { table, column } => {
            format!("RENAME COLUMN {}.{} TO {new}", qualified_name(Quote::Double, table.schema(), &table.name), q(column))
        }
        // An index lives in its table's schema.
        RenameTarget::Index { table, index } => {
            format!("RENAME INDEX {} TO {new}", qualified_name(Quote::Double, table.schema(), index))
        }
        RenameTarget::Object { .. } => {
            return Err(Error::Unsupported(
                "SAP HANA solo renombra tablas (RENAME TABLE): las vistas, procedimientos y funciones no se renombran.".into(),
            ))
        }
        RenameTarget::Constraint { .. } => return Err(Error::Unsupported("SAP HANA no renombra restricciones.".into())),
        RenameTarget::Schema { .. } => return Err(Error::Unsupported("SAP HANA no renombra esquemas.".into())),
    };
    Ok(SyncScript { statements: vec![statement], warnings: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;

    fn dialect() -> ScriptDialect {
        ScriptDialect { backtick_idents: false, ..ScriptDialect::generic() }
    }

    fn table() -> ObjectRef {
        ObjectRef { kind: kinds::TABLE.into(), schema: Some("App".into()), name: "Clientes".into() }
    }

    fn run(target: RenameTarget, new_name: &str) -> Result<SyncScript> {
        script(&RenameRequest { target, new_name: new_name.into(), table: None, definition: None }, &dialect())
    }

    #[test]
    fn renames_a_table() {
        let s = run(RenameTarget::Object { object: table(), parent: None }, "CLIENTES2").unwrap();
        assert_eq!(s.statements, [r#"RENAME TABLE "App"."Clientes" TO CLIENTES2"#]);
        // Mixed case would fold to upper case: it's quoted.
        let s = run(RenameTarget::Object { object: table(), parent: None }, "Socios").unwrap();
        assert_eq!(s.statements, [r#"RENAME TABLE "App"."Clientes" TO "Socios""#]);
    }

    #[test]
    fn renames_a_column() {
        let s = run(RenameTarget::Column { table: table(), column: "Pepe".into() }, "NOMBRE").unwrap();
        assert_eq!(s.statements, [r#"RENAME COLUMN "App"."Clientes"."Pepe" TO NOMBRE"#]);
        let s = run(RenameTarget::Column { table: table(), column: "PEPE".into() }, "mi columna").unwrap();
        assert_eq!(s.statements, [r#"RENAME COLUMN "App"."Clientes"."PEPE" TO "mi columna""#]);
    }

    #[test]
    fn renames_an_index() {
        let s = run(RenameTarget::Index { table: table(), index: "IX_Pepe".into() }, "IX_NOMBRE").unwrap();
        assert_eq!(s.statements, [r#"RENAME INDEX "App"."IX_Pepe" TO IX_NOMBRE"#]);
    }

    #[test]
    fn refuses_what_hana_does_not_rename() {
        let view = ObjectRef { kind: kinds::VIEW.into(), ..table() };
        assert!(matches!(run(RenameTarget::Object { object: view, parent: None }, "V2"), Err(Error::Unsupported(m)) if m.contains("solo renombra tablas")));
        assert!(matches!(run(RenameTarget::Constraint { table: table(), constraint: "CK".into() }, "CK2"), Err(Error::Unsupported(_))));
        assert!(matches!(run(RenameTarget::Schema { database: None, schema: "APP".into() }, "APP2"), Err(Error::Unsupported(_))));
        let s = spec();
        assert!(s.kinds == [kinds::TABLE] && s.columns && s.indexes && !s.constraints && !s.schemas && !s.transactional);
        assert_eq!(s.fold, Fold::Upper);
        assert_eq!(s.replace, ReplaceStyle::CreateOrReplace);
    }
}
