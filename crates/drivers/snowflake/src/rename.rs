//! "Renombrar…" on Snowflake: the rename statement for the target; the app
//! rewrites and puts back what names it (`CREATE OR REPLACE`).
//!
//! - Tables, views, materialized views and sequences:
//!   `ALTER <kind> s.x RENAME TO s.new`.
//! - Functions and procedures: `ALTER FUNCTION|PROCEDURE s.f(<types>) RENAME
//!   TO s.new`, one per overload. The argument types come from the
//!   definition (`list_objects` lists a name once, whatever its overloads).
//! - Columns: `ALTER TABLE s.t RENAME COLUMN c TO new`.
//! - Schemas: `ALTER SCHEMA [db.]s RENAME TO [db.]new`.
//! - Databases: `ALTER DATABASE "old" RENAME TO "new"` ([`database_script`]).
//!
//! The new name is always qualified like the old one: an unqualified one
//! would land in the session's current schema (a rename can move objects).
//! Snowflake doesn't update the views or code that name what was renamed,
//! and every DDL statement commits by itself.

use crate::script;
use dbine_driver::rename::{quote_new, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{kinds, Error, ObjectRef, Result, ScriptDialect, SyncScript};

const FOLD: Fold = Fold::Upper;

const KINDS: [&str; 6] = [kinds::TABLE, kinds::VIEW, kinds::MATERIALIZED_VIEW, kinds::SEQUENCE, kinds::FUNCTION, kinds::PROCEDURE];

pub(crate) fn spec() -> RenameSpec {
    RenameSpec {
        kinds: KINDS.map(String::from).to_vec(),
        columns: true,
        indexes: false,
        constraints: false,
        schemas: true,
        // Foreign keys follow the table by themselves; views and code
        // keep the old name in their text.
        tracked: Vec::new(),
        replace: ReplaceStyle::CreateOrReplace,
        references: ReferenceStyle::Sql,
        fold: FOLD,
        transactional: false,
        note: Some(
            "Snowflake confirma cada sentencia DDL al ejecutarla: si una falla, las anteriores ya quedaron hechas. \
             No actualiza las vistas ni el código que nombran lo renombrado: DBine los vuelve a crear con CREATE OR REPLACE, \
             que no conserva los permisos otorgados sobre ellos (no se agrega COPY GRANTS)."
                .into(),
        ),
        databases: true,
        // Both names are written out: any database can be current.
        database_from: None,
        database_note: Some(
            "Snowflake renombra la base con ALTER DATABASE … RENAME TO: sus esquemas, objetos y datos quedan en ella, \
             y los permisos otorgados siguen al objeto. No se actualiza lo que nombra la base anterior por su nombre: \
             el código de vistas, funciones y procedimientos, las tareas, los stages, los shares ni las aplicaciones \
             que la usen; hay que revisarlos y corregirlos a mano."
                .into(),
        ),
        database_moves: false,
        ..Default::default()
    }
}

/// `ALTER DATABASE "old" RENAME TO "new"`: names exactly as given.
pub(crate) fn database_script(database: &str, new_name: &str) -> Result<SyncScript> {
    if database.is_empty() || new_name.is_empty() {
        return Err(Error::Unsupported("Falta el nombre de la base.".into()));
    }
    Ok(SyncScript {
        statements: vec![format!("ALTER DATABASE {} RENAME TO {};", q(database), q(new_name))],
        warnings: vec![format!(
            "Lo que nombra «{database}» por su nombre (código, tareas, stages, shares y aplicaciones) no se actualiza."
        )],
    })
}

/// The dialect the new name is quoted in: Snowflake's, with `"…"` (its
/// script dialect reads backslash escapes, which `quote_new` would take
/// for backtick identifiers).
fn quoting() -> ScriptDialect {
    ScriptDialect { backslash_escapes: false, ..script::dialect() }
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn qualified(schema: Option<&str>, name: &str) -> String {
    qualified_name(Quote::Double, schema.filter(|s| !s.is_empty()), name)
}

/// The new name in the old one's schema.
fn new_qualified(schema: Option<&str>, to: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{to}", q(s)),
        None => to.to_string(),
    }
}

pub(crate) fn script(req: &RenameRequest) -> Result<SyncScript> {
    let to = quote_new(&req.new_name, &quoting(), FOLD, false);
    let one = |sql: String| Ok(SyncScript { statements: vec![sql], warnings: Vec::new() });
    match &req.target {
        RenameTarget::Object { object, .. } => {
            let word = match object.kind.as_str() {
                kinds::TABLE => "TABLE",
                kinds::VIEW => "VIEW",
                kinds::MATERIALIZED_VIEW => "MATERIALIZED VIEW",
                kinds::SEQUENCE => "SEQUENCE",
                kinds::FUNCTION | kinds::PROCEDURE => return routine(req, object, &to),
                k => return Err(Error::Unsupported(format!("Snowflake no renombra objetos de tipo «{k}» desde DBine."))),
            };
            one(format!("ALTER {word} {} RENAME TO {};", qualified(object.schema(), &object.name), new_qualified(object.schema(), &to)))
        }
        RenameTarget::Column { table, column } => {
            one(format!("ALTER TABLE {} RENAME COLUMN {} TO {to};", qualified(table.schema(), &table.name), q(column)))
        }
        RenameTarget::Schema { database, schema } => {
            let db = database.as_deref().filter(|d| !d.is_empty());
            let new = match db {
                Some(d) => format!("{}.{to}", q(d)),
                None => to,
            };
            one(format!("ALTER SCHEMA {} RENAME TO {new};", qualified(db, schema)))
        }
        RenameTarget::Index { .. } => Err(Error::Unsupported("Snowflake no tiene índices que se puedan renombrar.".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("Snowflake no renombra restricciones.".into())),
    }
}

/// `ALTER FUNCTION|PROCEDURE s.f(<types>) RENAME TO s.new` for each
/// overload in the definition.
fn routine(req: &RenameRequest, object: &ObjectRef, to: &str) -> Result<SyncScript> {
    let word = if object.kind == kinds::PROCEDURE { "PROCEDURE" } else { "FUNCTION" };
    let definition = req.definition.as_deref().filter(|d| !d.trim().is_empty()).ok_or_else(|| {
        Error::Unsupported(format!(
            "No se pudo leer la definición de «{}»: sin los tipos de sus argumentos Snowflake no sabe cuál renombrar.",
            object.name
        ))
    })?;
    let sigs = signatures(definition, word, &object.name);
    if sigs.is_empty() {
        return Err(Error::Unsupported(format!("No se reconocen los argumentos de «{}» en su definición.", object.name)));
    }
    let name = qualified(object.schema(), &object.name);
    let new = new_qualified(object.schema(), to);
    let statements = sigs.iter().map(|types| format!("ALTER {word} {name}({}) RENAME TO {new};", types.join(", "))).collect::<Vec<_>>();
    let warnings = if statements.len() > 1 {
        vec![format!("«{}» tiene {} sobrecargas: se renombran todas.", object.name, statements.len())]
    } else {
        Vec::new()
    };
    Ok(SyncScript { statements, warnings })
}

/// The argument types of each `CREATE OR REPLACE <word> <name>(…)` in the
/// definition (`definition` builds them from INFORMATION_SCHEMA's
/// `argument_signature`, one per overload, the name unquoted).
pub(crate) fn signatures(definition: &str, word: &str, name: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for unit in script::units(definition) {
        let text = unit.text.trim_start();
        let Some(rest) = strip_words(text, &["CREATE", "OR", "REPLACE", word]) else { continue };
        let rest = rest.trim_start();
        let after = if rest.len() >= name.len() && rest.is_char_boundary(name.len()) && rest[..name.len()].eq_ignore_ascii_case(name) {
            &rest[name.len()..]
        } else if let Some(r) = rest.strip_prefix(&q(name)) {
            r
        } else {
            continue;
        };
        let Some(args) = arguments(after.trim_start()) else { continue };
        out.push(args.iter().filter_map(|a| arg_type(a)).collect());
    }
    out
}

/// `text` after the given words, any case, separated by whitespace.
fn strip_words<'a>(text: &'a str, words: &[&str]) -> Option<&'a str> {
    let mut rest = text;
    for w in words {
        rest = rest.trim_start();
        if rest.len() < w.len() || !rest.is_char_boundary(w.len()) || !rest[..w.len()].eq_ignore_ascii_case(w) {
            return None;
        }
        rest = &rest[w.len()..];
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
    }
    Some(rest)
}

/// The arguments between the parentheses `text` starts with, split at the
/// top-level commas (types like `NUMBER(38,0)` stay whole).
fn arguments(text: &str) -> Option<Vec<String>> {
    let body = text.strip_prefix('(')?;
    let (mut depth, mut quoted, mut cur, mut out) = (0usize, false, String::new(), Vec::new());
    for c in body.chars() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted && depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                return Some(out);
            }
            ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    None
}

/// The type of `NAME TYPE [DEFAULT …]` (or of `[NAME TYPE]`, an optional
/// argument as Snowflake lists it).
fn arg_type(arg: &str) -> Option<String> {
    let arg = arg.trim().trim_start_matches('[').trim_end_matches(']').trim();
    let rest = if let Some(r) = arg.strip_prefix('"') {
        &r[r.find('"')? + 1..]
    } else {
        match arg.find(char::is_whitespace) {
            Some(i) => &arg[i..],
            None => return Some(arg.to_string()).filter(|a| !a.is_empty()),
        }
    };
    let rest = rest.trim();
    let upper = rest.to_ascii_uppercase();
    let ty = match upper.find(" DEFAULT ") {
        Some(i) => &rest[..i],
        None => rest,
    };
    Some(ty.trim().to_string()).filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: Some(schema.into()), name: name.into() }
    }

    fn req(target: RenameTarget, new: &str, definition: Option<&str>) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: definition.map(Into::into) }
    }

    fn stmts(r: &RenameRequest) -> Vec<String> {
        script(r).unwrap().statements
    }

    fn object(kind: &str, new: &str) -> RenameRequest {
        req(RenameTarget::Object { object: obj(kind, "APP", "CLIENTES"), parent: None }, new, None)
    }

    #[test]
    fn databases_rename_natively_with_quoted_names() {
        let s = spec();
        assert!(s.databases && !s.database_moves && s.database_from.is_none());
        assert!(s.database_note.as_deref().is_some_and(|n| n.contains("shares")));
        let out = database_script("Sales", "sales_2024").unwrap();
        assert_eq!(out.statements, vec![r#"ALTER DATABASE "Sales" RENAME TO "sales_2024";"#]);
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(database_script("A\"b", "C").unwrap().statements, vec![r#"ALTER DATABASE "A""b" RENAME TO "C";"#]);
        assert!(database_script("", "x").is_err());
    }

    #[test]
    fn spec_lists_what_snowflake_renames() {
        let s = spec();
        assert_eq!(s.kinds, ["table", "view", "materialized_view", "sequence", "function", "procedure"]);
        assert!(s.columns && s.schemas && !s.indexes && !s.constraints && !s.transactional);
        assert_eq!((s.fold, s.replace), (Fold::Upper, ReplaceStyle::CreateOrReplace));
    }

    #[test]
    fn tables_keep_their_schema_and_upper_case_stays_bare() {
        assert_eq!(stmts(&object(kinds::TABLE, "CLIENTES2")), ["ALTER TABLE \"APP\".\"CLIENTES\" RENAME TO \"APP\".CLIENTES2;"]);
        // Lower or mixed case is kept with quotes, with ", not backticks.
        assert_eq!(stmts(&object(kinds::TABLE, "Clientes")), ["ALTER TABLE \"APP\".\"CLIENTES\" RENAME TO \"APP\".\"Clientes\";"]);
        assert_eq!(stmts(&object(kinds::TABLE, "MI \"T\"")), ["ALTER TABLE \"APP\".\"CLIENTES\" RENAME TO \"APP\".\"MI \"\"T\"\"\";"]);
        // A reserved word is quoted.
        assert_eq!(stmts(&object(kinds::TABLE, "TABLE")), ["ALTER TABLE \"APP\".\"CLIENTES\" RENAME TO \"APP\".\"TABLE\";"]);
    }

    #[test]
    fn views_materialized_views_and_sequences() {
        assert_eq!(stmts(&object(kinds::VIEW, "V2")), ["ALTER VIEW \"APP\".\"CLIENTES\" RENAME TO \"APP\".V2;"]);
        assert_eq!(stmts(&object(kinds::MATERIALIZED_VIEW, "MV2")), ["ALTER MATERIALIZED VIEW \"APP\".\"CLIENTES\" RENAME TO \"APP\".MV2;"]);
        assert_eq!(stmts(&object(kinds::SEQUENCE, "S2")), ["ALTER SEQUENCE \"APP\".\"CLIENTES\" RENAME TO \"APP\".S2;"]);
    }

    #[test]
    fn columns() {
        let r = req(RenameTarget::Column { table: obj(kinds::TABLE, "APP", "T"), column: "PEPE".into() }, "PEPA", None);
        assert_eq!(stmts(&r), ["ALTER TABLE \"APP\".\"T\" RENAME COLUMN \"PEPE\" TO PEPA;"]);
        let r = req(RenameTarget::Column { table: obj(kinds::TABLE, "APP", "T"), column: "PEPE".into() }, "pepa", None);
        assert_eq!(stmts(&r), ["ALTER TABLE \"APP\".\"T\" RENAME COLUMN \"PEPE\" TO \"pepa\";"]);
    }

    #[test]
    fn schemas_in_their_database() {
        let r = req(RenameTarget::Schema { database: Some("VENTAS".into()), schema: "APP".into() }, "COMERCIAL", None);
        assert_eq!(stmts(&r), ["ALTER SCHEMA \"VENTAS\".\"APP\" RENAME TO \"VENTAS\".COMERCIAL;"]);
        let r = req(RenameTarget::Schema { database: None, schema: "APP".into() }, "Comercial", None);
        assert_eq!(stmts(&r), ["ALTER SCHEMA \"APP\" RENAME TO \"Comercial\";"]);
    }

    #[test]
    fn functions_rename_every_overload_with_its_types() {
        let def = "CREATE OR REPLACE FUNCTION TOTAL(X NUMBER, Y VARCHAR) RETURNS NUMBER LANGUAGE SQL AS $$ \
                   SELECT 'CREATE OR REPLACE FUNCTION TOTAL(Z FLOAT)' $$;\n\n\
                   CREATE OR REPLACE FUNCTION TOTAL() RETURNS NUMBER LANGUAGE SQL AS $$ 1 $$;\n\n\
                   CREATE OR REPLACE FUNCTION TOTAL(P NUMBER(38,2), \"q r\" TIMESTAMP_NTZ(9)) RETURNS NUMBER LANGUAGE JAVASCRIPT AS $$ return 1; $$;";
        let r = req(RenameTarget::Object { object: obj(kinds::FUNCTION, "APP", "TOTAL"), parent: None }, "SUMA", Some(def));
        let s = script(&r).unwrap();
        assert_eq!(
            s.statements,
            [
                "ALTER FUNCTION \"APP\".\"TOTAL\"(NUMBER, VARCHAR) RENAME TO \"APP\".SUMA;",
                "ALTER FUNCTION \"APP\".\"TOTAL\"() RENAME TO \"APP\".SUMA;",
                "ALTER FUNCTION \"APP\".\"TOTAL\"(NUMBER(38,2), TIMESTAMP_NTZ(9)) RENAME TO \"APP\".SUMA;",
            ]
        );
        assert_eq!(s.warnings.len(), 1);
    }

    #[test]
    fn procedures_and_optional_arguments() {
        let def = "CREATE OR REPLACE PROCEDURE CARGA(N NUMBER, [MODO VARCHAR]) RETURNS VARCHAR LANGUAGE SQL AS $$\nBEGIN\n  RETURN 'ok';\nEND;\n$$;";
        let r = req(RenameTarget::Object { object: obj(kinds::PROCEDURE, "APP", "CARGA"), parent: None }, "Carga2", Some(def));
        assert_eq!(stmts(&r), ["ALTER PROCEDURE \"APP\".\"CARGA\"(NUMBER, VARCHAR) RENAME TO \"APP\".\"Carga2\";"]);
        assert_eq!(arg_type("B NUMBER DEFAULT 5").as_deref(), Some("NUMBER"));
        // Without a definition there are no types to name it by.
        let r = req(RenameTarget::Object { object: obj(kinds::PROCEDURE, "APP", "CARGA"), parent: None }, "X", None);
        assert!(matches!(script(&r), Err(Error::Unsupported(_))));
    }

    #[test]
    fn refuses_what_snowflake_does_not_rename() {
        assert!(matches!(script(&object(kinds::STREAM, "X")), Err(Error::Unsupported(_))));
        let t = obj(kinds::TABLE, "APP", "T");
        assert!(matches!(script(&req(RenameTarget::Index { table: t.clone(), index: "I".into() }, "J", None)), Err(Error::Unsupported(_))));
        assert!(matches!(script(&req(RenameTarget::Constraint { table: t, constraint: "C".into() }, "D", None)), Err(Error::Unsupported(_))));
    }
}
