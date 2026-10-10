//! "Renombrar…" on Snowflake: the rename statement for the target. What
//! names it (views, materialized views, functions, procedures, tasks,
//! streams, dynamic tables…) is only listed for the user to fix
//! (`ReferenceStyle::None`): no kind of dependent is safe to create again.
//! `CREATE OR REPLACE` hands any object to the role running the rename, so
//! a view would read, a routine or a task run (on its schedule), with that
//! role's privileges; a stream would start over from a new offset, a
//! materialized or dynamic table lose its rows; and the catalog text of a
//! routine has no `EXECUTE AS`, `SECURE`, `HANDLER`… The renamed object
//! itself goes through `ALTER … RENAME TO`, which keeps its owner, rights
//! and grants.
//!
//! - Tables, views, materialized views and sequences:
//!   `ALTER <kind> s.x RENAME TO s.new`.
//! - Functions and procedures: `ALTER FUNCTION|PROCEDURE s.f(<types>) RENAME
//!   TO s.new`, one per overload. The argument types come from the
//!   definition (`list_objects` lists a name once, whatever its overloads),
//!   which is built from the catalog's `argument_signature`; a definition
//!   whose `$$` structure is ambiguous, or a type that isn't a plain
//!   Snowflake type, is refused ([`signatures`]).
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
        // Listed, never created again: no dependent kind is safe to put
        // back (see the module's doc). An allowlist that holds nothing.
        references: ReferenceStyle::None,
        fold: FOLD,
        transactional: false,
        note: Some(
            "Snowflake confirma cada sentencia DDL al ejecutarla: si una falla, las anteriores ya quedaron hechas. \
             No actualiza las vistas ni el código que nombran lo renombrado. Todo lo que lo nombra (vistas, vistas materializadas, \
             funciones, procedimientos, tareas, streams, tablas dinámicas) solo se lista para corregirlo a mano: recrearlo lo pasaría \
             al rol que renombra (leería y correría con sus privilegios), las rutinas perderían EXECUTE AS, SECURE y sus opciones, \
             las tareas volverían a programarse, los streams perderían su posición y las vistas materializadas y tablas dinámicas, sus filas."
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
    // DBine splits Snowflake scripts reading backslash escapes, which a
    // quoted identifier doesn't have: a name with one would be cut apart.
    if let Some(n) = [database, new_name].into_iter().find(|n| n.contains('\\')) {
        return Err(Error::Unsupported(format!("«{n}» tiene una barra invertida: DBine no renombra esa base (hacelo desde Snowsight o SnowSQL).")));
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
    let sigs = signatures(definition, word, &object.name)?;
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

/// The argument types of each `CREATE OR REPLACE <word> <name>(…) RETURNS
/// … AS $$<body>$$` in the definition (`definition` builds them from
/// INFORMATION_SCHEMA's `argument_signature`, one per overload, the name
/// unquoted).
///
/// The body is wrapped in `$$` without escaping, so a body that holds `$$`
/// (one written with `AS '…'`) can fake units of its own. Every unit has to
/// be one of these CREATEs with exactly its two `$$`, and every type has to
/// be a plain Snowflake type ([`valid_type`]): anything else is refused, so
/// no text from a body reaches the `ALTER`.
pub(crate) fn signatures(definition: &str, word: &str, name: &str) -> Result<Vec<Vec<String>>> {
    let ambiguous = || {
        Error::Unsupported(format!(
            "La definición de «{name}» no se puede leer sin ambigüedad (su cuerpo tiene $$ o texto fuera de un CREATE): \
             DBine no la renombra (hacelo desde Snowsight o SnowSQL)."
        ))
    };
    let mut out = Vec::new();
    for unit in script::units(definition) {
        let text = unit.text.trim().trim_end_matches(';').trim_end();
        if text.is_empty() {
            continue;
        }
        let rest = strip_words(text, &["CREATE", "OR", "REPLACE", word]).ok_or_else(ambiguous)?.trim_start();
        let after = if rest.len() >= name.len() && rest.is_char_boundary(name.len()) && rest[..name.len()].eq_ignore_ascii_case(name) {
            &rest[name.len()..]
        } else if let Some(r) = rest.strip_prefix(&q(name)) {
            r
        } else {
            return Err(ambiguous());
        };
        let (args, tail) = arguments(after.trim_start()).ok_or_else(ambiguous)?;
        // `RETURNS … AS $$<body>$$`, with no other `$$` in the unit, or
        // `RETURNS … AS '<body>'` (a body that holds `$$`), the literal
        // running to the end of the unit.
        let tail = tail.trim_start();
        let returns = tail.len() >= 8 && tail.is_char_boundary(8) && tail[..8].eq_ignore_ascii_case("RETURNS ");
        let dollar = tail.find("$$");
        let quote = tail.find('\'').filter(|q| dollar.is_none_or(|d| *q < d));
        let open = quote.or(dollar).ok_or_else(ambiguous)?;
        let header = tail[..open].trim_end();
        let as_kw = header.len() >= 3 && header.is_char_boundary(header.len() - 3) && header[header.len() - 3..].eq_ignore_ascii_case(" AS");
        let body_ok = match quote {
            Some(q) => literal_end(tail, q) == Some(tail.len()),
            None => tail.matches("$$").count() == 2 && tail.ends_with("$$") && tail.len() >= open + 4,
        };
        if !returns || !as_kw || !body_ok {
            return Err(ambiguous());
        }
        let mut types = Vec::with_capacity(args.len());
        for a in &args {
            let ty = arg_type(a).ok_or_else(ambiguous)?;
            if !valid_type(&ty) {
                return Err(Error::Unsupported(format!(
                    "«{name}» tiene un argumento de un tipo que DBine no reconoce («{}»): no se renombra (hacelo desde Snowsight o SnowSQL).",
                    ty.chars().take(60).collect::<String>()
                )));
            }
            types.push(ty);
        }
        out.push(types);
    }
    Ok(out)
}

/// Where the '…' literal opening at `open` ends (after its closing quote),
/// as Snowflake reads it: `\\` escapes the next character, `''` is a quote.
fn literal_end(text: &str, open: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = open + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'\'' if b.get(i + 1) == Some(&b'\'') => i += 2,
            b'\'' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Snowflake's type names (and their synonyms), as words separated by one
/// space.
const TYPE_NAMES: &[&str] = &[
    "NUMBER", "DECIMAL", "DEC", "NUMERIC", "INT", "INTEGER", "BIGINT", "SMALLINT", "TINYINT", "BYTEINT",
    "FLOAT", "FLOAT4", "FLOAT8", "DOUBLE", "DOUBLE PRECISION", "REAL",
    "VARCHAR", "CHAR", "CHARACTER", "CHAR VARYING", "CHARACTER VARYING", "NCHAR", "NCHAR VARYING", "NVARCHAR", "NVARCHAR2",
    "STRING", "TEXT", "BINARY", "VARBINARY", "BOOLEAN",
    "DATE", "DATETIME", "TIME", "TIMESTAMP", "TIMESTAMP_LTZ", "TIMESTAMP_NTZ", "TIMESTAMP_TZ",
    "TIMESTAMPLTZ", "TIMESTAMPNTZ", "TIMESTAMPTZ",
    "TIMESTAMP WITH LOCAL TIME ZONE", "TIMESTAMP WITH TIME ZONE", "TIMESTAMP WITHOUT TIME ZONE",
    "VARIANT", "OBJECT", "ARRAY", "MAP", "VECTOR", "GEOGRAPHY", "GEOMETRY", "FILE",
];

#[derive(Debug, PartialEq)]
enum Tok<'a> {
    Word(&'a str),
    Num,
    Open,
    Close,
    Comma,
}

/// `ty` is a Snowflake type and nothing else: a known type name, with an
/// optional `(…)` of precisions (`NUMBER(38,0)`), element types
/// (`ARRAY(NUMBER)`, `MAP(VARCHAR, NUMBER)`, `VECTOR(INT, 3)`) or fields
/// (`OBJECT(a NUMBER, b VARCHAR)`). Only ASCII letters, digits, `_`, single
/// spaces, parentheses and commas: no quotes, `;`, `$`, comments or line
/// breaks.
fn valid_type(ty: &str) -> bool {
    if ty.is_empty() || !ty.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ' ' | '(' | ')' | ',')) {
        return false;
    }
    let mut toks = Vec::new();
    let b = ty.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' => i += 1,
            b'(' | b')' | b',' => {
                toks.push(match b[i] {
                    b'(' => Tok::Open,
                    b')' => Tok::Close,
                    _ => Tok::Comma,
                });
                i += 1;
            }
            c if c.is_ascii_digit() => {
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
                    return false;
                }
                toks.push(Tok::Num);
            }
            _ => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                toks.push(Tok::Word(&ty[s..i]));
            }
        }
    }
    let mut pos = 0;
    parse_type(&toks, &mut pos, 0) && pos == toks.len()
}

/// `type := NAME [ '(' item (',' item)* ')' ]`, `item := NUM | type | WORD type`.
fn parse_type(toks: &[Tok], pos: &mut usize, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    // The longest known name made of the next words.
    let mut words = Vec::new();
    let mut best = None;
    while let Some(Tok::Word(w)) = toks.get(*pos + words.len()) {
        words.push(w.to_ascii_uppercase());
        if TYPE_NAMES.contains(&words.join(" ").as_str()) {
            best = Some(words.len());
        }
    }
    let Some(n) = best else { return false };
    *pos += n;
    if toks.get(*pos) != Some(&Tok::Open) {
        return true;
    }
    *pos += 1;
    loop {
        match toks.get(*pos) {
            Some(Tok::Num) => *pos += 1,
            Some(Tok::Word(_)) => {
                let save = *pos;
                if !parse_type(toks, pos, depth + 1) {
                    // An OBJECT field: its name, then its type.
                    *pos = save + 1;
                    if !parse_type(toks, pos, depth + 1) {
                        return false;
                    }
                } else if matches!(toks.get(*pos), Some(Tok::Word(_))) {
                    // A field whose name is also a type name (`date DATE`).
                    *pos = save + 1;
                    if !parse_type(toks, pos, depth + 1) {
                        return false;
                    }
                }
            }
            _ => return false,
        }
        match toks.get(*pos) {
            Some(Tok::Comma) => *pos += 1,
            Some(Tok::Close) => {
                *pos += 1;
                return true;
            }
            _ => return false,
        }
    }
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
/// top-level commas (types like `NUMBER(38,0)` stay whole), and the text
/// after the closing parenthesis.
fn arguments(text: &str) -> Option<(Vec<String>, &str)> {
    let body = text.strip_prefix('(')?;
    let (mut depth, mut quoted, mut cur, mut out) = (0usize, false, String::new(), Vec::new());
    for (i, c) in body.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted && depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                } else if !out.is_empty() {
                    // `(A NUMBER, )`: an empty argument.
                    return None;
                }
                return Some((out, &body[i + 1..]));
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
        assert!(s.note.as_deref().is_some_and(|n| n.contains("procedimientos") && n.contains("EXECUTE AS")));
    }

    #[test]
    fn no_dependent_is_ever_created_again() {
        // Tasks, streams, materialized views, views and routines alike: the
        // app lists every dependent for the user and refuses to put one back.
        let s = spec();
        assert_eq!(s.references, ReferenceStyle::None);
        let target = dbine_driver::rename::RewriteTarget::Object { object: obj(kinds::TABLE, "S", "T") };
        for def in [
            "CREATE OR REPLACE TASK S.K WAREHOUSE = W SCHEDULE = '1 minute' AS INSERT INTO S.LOG SELECT * FROM S.T",
            "CREATE OR REPLACE STREAM S.ST ON TABLE S.T",
            "CREATE OR REPLACE MATERIALIZED VIEW S.MV AS SELECT A FROM S.T",
            "CREATE OR REPLACE VIEW S.V AS SELECT A FROM S.T",
        ] {
            let r = dbine_driver::rename::rewrite_references(def, &script::dialect(), &target, "U", &s, &Default::default());
            assert!(r.edits.is_empty() && r.text == def, "{def}");
        }
    }

    #[test]
    fn a_renamed_routine_keeps_its_rights_through_alter() {
        // `ALTER … RENAME TO` keeps owner, EXECUTE AS, SECURE and grants;
        // nothing is created again.
        let def = "CREATE OR REPLACE PROCEDURE TOTAL(X NUMBER) RETURNS NUMBER LANGUAGE SQL AS $$ 1 $$;";
        for kind in [kinds::PROCEDURE, kinds::FUNCTION] {
            let def = if kind == kinds::FUNCTION { def.replace("PROCEDURE", "FUNCTION") } else { def.to_string() };
            let s = stmts(&routine_req(kind, &def));
            assert_eq!(s.len(), 1);
            assert!(s[0].starts_with("ALTER ") && s[0].contains(" RENAME TO ") && !s[0].contains("CREATE"), "{s:?}");
        }
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

    fn routine_req(kind: &str, def: &str) -> RenameRequest {
        req(RenameTarget::Object { object: obj(kind, "APP", "TOTAL"), parent: None }, "SUMA", Some(def))
    }

    #[test]
    fn a_body_with_dollar_quotes_faking_another_unit_is_refused() {
        // A body written with AS '…' holds `$$`: wrapped in `$$…$$` it closes
        // early and the rest reads as a second CREATE carrying SQL in its types.
        let fake = "CREATE OR REPLACE FUNCTION TOTAL(X NUMBER) RETURNS NUMBER LANGUAGE SQL AS $$ 1 $$;\n\
                    CREATE OR REPLACE FUNCTION TOTAL(A NUMBER) RENAME TO APP.X; DROP TABLE APP.T; ALTER FUNCTION APP.Y(NUMBER) RETURNS NUMBER LANGUAGE SQL AS $$ 2 $$;";
        assert!(matches!(script(&routine_req(kinds::FUNCTION, fake)), Err(Error::Unsupported(_))));
        // A stray `$$` left in the body.
        let odd = "CREATE OR REPLACE FUNCTION TOTAL(X NUMBER) RETURNS NUMBER LANGUAGE SQL AS $$ select '$$' $$;";
        assert!(script(&routine_req(kinds::FUNCTION, odd)).is_err());
        // Text that isn't one of the routine's CREATEs.
        let extra = "CREATE OR REPLACE PROCEDURE TOTAL(X NUMBER) RETURNS NUMBER LANGUAGE SQL AS $$ 1 $$;\nDROP TABLE APP.T;";
        assert!(script(&routine_req(kinds::PROCEDURE, extra)).is_err());
        // A unit of another routine's name.
        let other = "CREATE OR REPLACE FUNCTION TOTAL(X NUMBER) RETURNS NUMBER LANGUAGE SQL AS $$ 1 $$;\n\
                     CREATE OR REPLACE FUNCTION OTRA(X NUMBER) RETURNS NUMBER LANGUAGE SQL AS $$ 1 $$;";
        assert!(script(&routine_req(kinds::FUNCTION, other)).is_err());
    }

    #[test]
    fn a_body_holding_dollar_quotes_comes_as_one_quoted_literal() {
        // What `definition` builds (search::routine_source) for a body that
        // holds `$$`: a '…' literal with `\` and `'` escaped.
        let body = "select '$$'; drop table app.t; -- \\' $$";
        let lit = format!("'{}'", body.replace('\\', "\\\\").replace('\'', "''"));
        let def = format!("CREATE OR REPLACE FUNCTION TOTAL(X NUMBER) RETURNS NUMBER LANGUAGE SQL AS {lit};");
        assert_eq!(script::units(&def).len(), 1, "{def}");
        assert_eq!(stmts(&routine_req(kinds::FUNCTION, &def)), ["ALTER FUNCTION \"APP\".\"TOTAL\"(NUMBER) RENAME TO \"APP\".SUMA;"]);
        // A literal that ends before the unit does is refused.
        let early = "CREATE OR REPLACE FUNCTION TOTAL(X NUMBER) RETURNS NUMBER LANGUAGE SQL AS '1' || 'x';";
        assert!(script(&routine_req(kinds::FUNCTION, early)).is_err());
        assert_eq!(literal_end("'a\\'b''c' x", 0), Some(9));
        assert_eq!(literal_end("'open", 0), None);
    }

    #[test]
    fn types_carrying_sql_are_refused() {
        for args in [
            "(X NUMBER); DROP TABLE APP.T; --)",
            "(X NUMBER(38,0)) RENAME TO APP.Z; DROP TABLE T; SELECT (1)",
            "(X VARCHAR 'a')",
            "(X \"VARCHAR\")",
            "(X NUMBER /* c */)",
            "(X NUMBER -- c\n)",
            "(X NUMBER$)",
            "(X NOTATYPE)",
            "(X NUMBER(38,0) RENAME)",
            "(X ARRAY(NUMBER) RENAME TO Y)",
            "(X NUMBER, )",
        ] {
            let def = format!("CREATE OR REPLACE FUNCTION TOTAL{args} RETURNS NUMBER LANGUAGE SQL AS $$ 1 $$;");
            assert!(script(&routine_req(kinds::FUNCTION, &def)).is_err(), "{args}");
        }
        assert!(!valid_type("NUMBER;"));
        assert!(!valid_type("VARCHAR'"));
        assert!(!valid_type("NUMBER(38,0"));
        assert!(!valid_type("NUMBER)"));
        assert!(!valid_type("NUMBER\n"));
    }

    #[test]
    fn plain_types_still_rename() {
        let def = "CREATE OR REPLACE FUNCTION TOTAL(A NUMBER(38,0), B VARCHAR, C ARRAY, D OBJECT, E VARIANT, F TIMESTAMP_NTZ(9)) \
                   RETURNS NUMBER LANGUAGE SQL AS $$ 1 $$;\n\n\
                   CREATE OR REPLACE FUNCTION TOTAL(G VECTOR(FLOAT, 256), H ARRAY(NUMBER), I MAP(VARCHAR, NUMBER), J OBJECT(a NUMBER, date DATE), K DOUBLE PRECISION) \
                   RETURNS NUMBER LANGUAGE SQL AS $$ 2 $$;";
        assert_eq!(
            stmts(&routine_req(kinds::FUNCTION, def)),
            [
                "ALTER FUNCTION \"APP\".\"TOTAL\"(NUMBER(38,0), VARCHAR, ARRAY, OBJECT, VARIANT, TIMESTAMP_NTZ(9)) RENAME TO \"APP\".SUMA;",
                "ALTER FUNCTION \"APP\".\"TOTAL\"(VECTOR(FLOAT, 256), ARRAY(NUMBER), MAP(VARCHAR, NUMBER), OBJECT(a NUMBER, date DATE), DOUBLE PRECISION) RENAME TO \"APP\".SUMA;",
            ]
        );
        let def = "CREATE OR REPLACE PROCEDURE TOTAL(N NUMBER(38,0), [M TIMESTAMP_TZ(9)]) RETURNS VARCHAR LANGUAGE JAVASCRIPT AS $$ return 'a;b'; $$;";
        assert_eq!(stmts(&routine_req(kinds::PROCEDURE, def)), ["ALTER PROCEDURE \"APP\".\"TOTAL\"(NUMBER(38,0), TIMESTAMP_TZ(9)) RENAME TO \"APP\".SUMA;"]);
    }

    #[test]
    fn refuses_what_snowflake_does_not_rename() {
        assert!(matches!(script(&object(kinds::STREAM, "X")), Err(Error::Unsupported(_))));
        let t = obj(kinds::TABLE, "APP", "T");
        assert!(matches!(script(&req(RenameTarget::Index { table: t.clone(), index: "I".into() }, "J", None)), Err(Error::Unsupported(_))));
        assert!(matches!(script(&req(RenameTarget::Constraint { table: t, constraint: "C".into() }, "D", None)), Err(Error::Unsupported(_))));
    }

    #[test]
    fn names_with_a_backslash_are_refused() {
        assert!(database_script("a\\\"b", "c").is_err());
        assert!(database_script("a", "c\\d").is_err());
        assert!(database_script("Sales", "sales_2024").is_ok());
    }
}
