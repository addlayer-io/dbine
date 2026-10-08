//! "Renombrar…" on Aurora DSQL: PostgreSQL's `ALTER … RENAME`, cut down to
//! DSQL's DDL subset. It renames tables, views, sequences and functions
//! (`ALTER TABLE|VIEW|SEQUENCE|FUNCTION … RENAME TO`), columns of tables and
//! views, and table constraints. There is no `ALTER INDEX`, `ALTER SCHEMA`
//! nor `ALTER DOMAIN`, so indexes, schemas and domains aren't offered.
//!
//! Views follow a rename by OID, as in PostgreSQL (`tracked`); SQL
//! functions with a text body are rewritten by the app and put back with
//! `CREATE OR REPLACE`. DSQL runs each DDL statement in its own
//! transaction, so the script isn't atomic.

use dbine_driver::rename::{quote_new, Fold, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::sql::{qualified_name, quote_ident, Quote, ScriptDialect};
use dbine_driver::{kinds, Error, ObjectRef, ReferenceStyle, Result, SyncScript};

const ENGINE: &str = "Aurora DSQL";

pub(crate) fn spec() -> RenameSpec {
    RenameSpec {
        kinds: [kinds::TABLE, kinds::VIEW, kinds::SEQUENCE, kinds::FUNCTION].map(String::from).to_vec(),
        columns: true,
        indexes: false,
        constraints: true,
        schemas: false,
        tracked: vec![kinds::VIEW.into()],
        replace: ReplaceStyle::CreateOrReplace,
        references: ReferenceStyle::Sql,
        fold: Fold::Lower,
        // One DDL statement per transaction, and never with DML.
        transactional: false,
        note: Some(
            "Aurora DSQL confirma cada sentencia DDL en su propia transacción: si una falla, las anteriores quedan aplicadas. Las vistas que usan el objeto se actualizan solas."
                .into(),
        ),
        ..Default::default()
    }
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn qn(schema: Option<&str>, name: &str) -> String {
    qualified_name(Quote::Double, schema, name)
}

pub(crate) fn script(req: &RenameRequest) -> Result<SyncScript> {
    let spec = spec();
    if !spec.allows(&req.target) {
        let what = match &req.target {
            RenameTarget::Object { object, .. } => format!("objetos de tipo «{}»", object.kind),
            RenameTarget::Column { .. } => "columnas".into(),
            RenameTarget::Index { .. } => "índices (no tiene ALTER INDEX)".into(),
            RenameTarget::Constraint { .. } => "restricciones".into(),
            RenameTarget::Schema { .. } => "esquemas (no tiene ALTER SCHEMA)".into(),
        };
        return Err(Error::Unsupported(format!("{ENGINE} no renombra {what} desde DBine")));
    }
    let new = quote_new(&req.new_name, &ScriptDialect::postgres(), spec.fold, false);
    let mut warnings = Vec::new();
    let statements = match &req.target {
        RenameTarget::Object { object, .. } => object_statements(object, req.definition.as_deref(), &new, &mut warnings)?,
        RenameTarget::Column { table, column } => {
            let alter = if table.kind == kinds::VIEW { "ALTER VIEW" } else { "ALTER TABLE" };
            vec![format!("{alter} {} RENAME COLUMN {} TO {new};", qn(table.schema(), &table.name), q(column))]
        }
        RenameTarget::Constraint { table, constraint } => {
            warnings.push("Si la restricción tiene un índice propio (clave primaria o única), el índice también cambia de nombre.".into());
            vec![format!("ALTER TABLE {} RENAME CONSTRAINT {} TO {new};", qn(table.schema(), &table.name), q(constraint))]
        }
        RenameTarget::Index { .. } | RenameTarget::Schema { .. } => unreachable!("refused above"),
    };
    Ok(SyncScript { statements, warnings })
}

fn object_statements(object: &ObjectRef, definition: Option<&str>, new: &str, warnings: &mut Vec<String>) -> Result<Vec<String>> {
    let name = qn(object.schema(), &object.name);
    let alter = match object.kind.as_str() {
        kinds::TABLE => {
            warnings.push("Los índices, restricciones y secuencias de la tabla conservan sus nombres (por ejemplo, «…_pkey»).".into());
            "TABLE"
        }
        kinds::VIEW => "VIEW",
        kinds::SEQUENCE => "SEQUENCE",
        kinds::FUNCTION => {
            let sigs = definition.map(|d| signatures(d, &object.name)).unwrap_or_default();
            if sigs.is_empty() {
                warnings.push(format!("No se leyó la firma de «{}»: si tiene sobrecargas, el motor pide indicar sus argumentos.", object.name));
                return Ok(vec![format!("ALTER FUNCTION {name} RENAME TO {new};")]);
            }
            if sigs.len() > 1 {
                warnings.push(format!("«{}» tiene {} sobrecargas: se renombran todas.", object.name, sigs.len()));
            }
            return Ok(sigs.into_iter().map(|args| format!("ALTER FUNCTION {name}({args}) RENAME TO {new};")).collect());
        }
        other => return Err(Error::Unsupported(format!("{ENGINE} no renombra objetos de tipo «{other}» desde DBine"))),
    };
    Ok(vec![format!("ALTER {alter} {name} RENAME TO {new};")])
}

/// Each overload's arguments in the function's definition
/// ([`dbine_driver::rename::routine_signatures`], functions only: DSQL has
/// no procedures).
fn signatures(definition: &str, name: &str) -> Vec<String> {
    dbine_driver::rename::routine_signatures(definition, name).into_iter().filter(|(kind, _)| *kind == "FUNCTION").map(|(_, args)| args).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: (!schema.is_empty()).then(|| schema.into()), name: name.into() }
    }

    fn req(target: RenameTarget, new: &str, definition: Option<&str>) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: definition.map(Into::into) }
    }

    fn object(kind: &str, name: &str, new: &str) -> RenameRequest {
        req(RenameTarget::Object { object: obj(kind, "app", name), parent: None }, new, None)
    }

    fn stmts(r: &RenameRequest) -> Vec<String> {
        script(r).unwrap_or_else(|e| panic!("{e}")).statements
    }

    #[test]
    fn tables_quote_what_isnt_lower_case() {
        assert_eq!(stmts(&object(kinds::TABLE, "Clientes", "nuevos")), ["ALTER TABLE \"app\".\"Clientes\" RENAME TO nuevos;"]);
        assert_eq!(stmts(&object(kinds::TABLE, "t", "Nuevos")), ["ALTER TABLE \"app\".\"t\" RENAME TO \"Nuevos\";"]);
        assert_eq!(stmts(&object(kinds::TABLE, "t", "mi tabla")), ["ALTER TABLE \"app\".\"t\" RENAME TO \"mi tabla\";"]);
        assert_eq!(stmts(&object(kinds::TABLE, "t", "select")), ["ALTER TABLE \"app\".\"t\" RENAME TO \"select\";"]);
        assert!(script(&object(kinds::TABLE, "t", "n")).unwrap().warnings[0].contains("conservan sus nombres"));
    }

    #[test]
    fn views_and_sequences() {
        assert_eq!(stmts(&object(kinds::VIEW, "v", "W")), ["ALTER VIEW \"app\".\"v\" RENAME TO \"W\";"]);
        assert_eq!(stmts(&object(kinds::SEQUENCE, "s", "s2")), ["ALTER SEQUENCE \"app\".\"s\" RENAME TO s2;"]);
    }

    #[test]
    fn functions_one_statement_per_overload() {
        // As the driver's `definition` joins them.
        let def = "CREATE OR REPLACE FUNCTION app.f(a integer, b text DEFAULT 'x,y'::text)\n RETURNS integer\n LANGUAGE sql\nAS $function$ SELECT 'CREATE FUNCTION app.f(z int)'::text, a $function$;\n\n\
                   CREATE OR REPLACE FUNCTION app.f()\n RETURNS integer\n LANGUAGE sql\nBEGIN ATOMIC\n SELECT 1;\nEND;";
        let r = req(RenameTarget::Object { object: obj(kinds::FUNCTION, "app", "f"), parent: None }, "G", Some(def));
        let s = script(&r).unwrap();
        assert_eq!(s.statements, ["ALTER FUNCTION \"app\".\"f\"(a integer, b text) RENAME TO \"G\";", "ALTER FUNCTION \"app\".\"f\"() RENAME TO \"G\";"]);
        assert!(s.warnings[0].contains("2 sobrecargas"), "{:?}", s.warnings);
        let r = req(RenameTarget::Object { object: obj(kinds::FUNCTION, "app", "f"), parent: None }, "g", None);
        let s = script(&r).unwrap();
        assert_eq!(s.statements, ["ALTER FUNCTION \"app\".\"f\" RENAME TO g;"]);
        assert_eq!(s.warnings.len(), 1);
    }

    #[test]
    fn columns_and_constraints() {
        let t = obj(kinds::TABLE, "app", "T");
        let col = req(RenameTarget::Column { table: t.clone(), column: "Pepe".into() }, "pepe_nuevo", None);
        assert_eq!(stmts(&col), ["ALTER TABLE \"app\".\"T\" RENAME COLUMN \"Pepe\" TO pepe_nuevo;"]);
        let vcol = req(RenameTarget::Column { table: obj(kinds::VIEW, "app", "v"), column: "a".into() }, "B", None);
        assert_eq!(stmts(&vcol), ["ALTER VIEW \"app\".\"v\" RENAME COLUMN \"a\" TO \"B\";"]);
        let ck = req(RenameTarget::Constraint { table: t, constraint: "t_pkey".into() }, "pk_t", None);
        let s = script(&ck).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE \"app\".\"T\" RENAME CONSTRAINT \"t_pkey\" TO pk_t;"]);
        assert!(s.warnings[0].contains("índice"));
    }

    #[test]
    fn refuses_what_dsql_cant_rename() {
        let t = obj(kinds::TABLE, "app", "t");
        let ix = req(RenameTarget::Index { table: t, index: "i".into() }, "j", None);
        let sc = req(RenameTarget::Schema { database: None, schema: "app".into() }, "b", None);
        for r in [ix, sc, object(kinds::TYPE, "d", "e")] {
            assert!(matches!(script(&r), Err(Error::Unsupported(_))));
        }
        let s = spec();
        assert_eq!(s.kinds, ["table", "view", "sequence", "function"]);
        assert!(s.columns && s.constraints && !s.indexes && !s.schemas && !s.transactional);
        assert_eq!(s.tracked, ["view"]);
        assert_eq!(s.fold, Fold::Lower);
        assert_eq!(s.replace, ReplaceStyle::CreateOrReplace);
    }
}
