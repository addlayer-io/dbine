//! "Renombrar…" on Oracle: the rename statement for the target; the app
//! rewrites and puts back what names it (`CREATE OR REPLACE`).
//!
//! - Tables: `ALTER TABLE s.t RENAME TO`, which works in any schema.
//! - Views, sequences and private synonyms: `RENAME`, which Oracle only
//!   runs on objects of the session's own user while the current schema is
//!   that user. The statement is a block that refuses any other user with a
//!   clear message, and sets the current schema to the owner around it.
//! - Triggers and indexes: `ALTER TRIGGER|INDEX s.x RENAME TO`.
//! - Columns and constraints: `ALTER TABLE s.t RENAME COLUMN|CONSTRAINT`.
//! - Procedures, functions and packages have no rename: the unit is created
//!   under the new name (header, `END` label and calls to itself renamed)
//!   and the old one is dropped.
//!
//! Every DDL statement commits on its own, so nothing here is atomic.

use crate::{quote, PACKAGE};
use dbine_driver::rename::{
    quote_new, rename_header, rewrite_references, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle,
    RewriteOptions, RewriteTarget,
};
use dbine_driver::sql::{name_tokens, split_script, NameToken, TokenKind};
use dbine_driver::{kinds, Error, ObjectRef, Result, ScriptDialect, SyncScript};

const FOLD: Fold = Fold::Upper;

pub(crate) fn spec() -> RenameSpec {
    RenameSpec {
        kinds: [kinds::TABLE, kinds::VIEW, kinds::SEQUENCE, kinds::SYNONYM, kinds::TRIGGER, kinds::PROCEDURE, kinds::FUNCTION, PACKAGE]
            .map(String::from)
            .to_vec(),
        columns: true,
        indexes: true,
        constraints: true,
        schemas: false,
        // Foreign keys, indexes and checks follow by themselves (they're
        // relations, not code); views, code and synonyms go INVALID.
        tracked: Vec::new(),
        replace: ReplaceStyle::CreateOrReplace,
        references: ReferenceStyle::Sql,
        fold: FOLD,
        transactional: false,
        note: Some(
            "Oracle confirma cada sentencia DDL al ejecutarla: si una falla, las anteriores ya quedaron hechas. Las vistas y el código que nombran el objeto quedan inválidos hasta que se reponen; al final, el script vuelve a compilar el esquema para que lo que depende de lo repuesto quede válido. Los hints (/*+ … */) son comentarios y no se modifican."
                .into(),
        ),
        // What depends on the rewritten dependents is compiled again, so it
        // doesn't stay INVALID until it's used.
        epilogue: Some("BEGIN DBMS_UTILITY.COMPILE_SCHEMA({schema}, FALSE); END;".into()),
        ..Default::default()
    }
}

pub(crate) fn script(req: &RenameRequest) -> Result<SyncScript> {
    let d = ScriptDialect::oracle();
    let to = quote_new(&req.new_name, &d, FOLD, false);
    let one = |sql: String| Ok(SyncScript { statements: vec![sql], warnings: Vec::new() });
    match &req.target {
        RenameTarget::Object { object, .. } => match object.kind.as_str() {
            kinds::TABLE => one(format!("ALTER TABLE {} RENAME TO {to}", qualified(object.schema(), &object.name))),
            kinds::TRIGGER => one(format!("ALTER TRIGGER {} RENAME TO {to}", qualified(object.schema(), &object.name))),
            kinds::SYNONYM if object.schema().is_some_and(|s| s.eq_ignore_ascii_case("PUBLIC")) => {
                Err(Error::Unsupported("Oracle no renombra sinónimos públicos: hay que borrarlo y crearlo con el nombre nuevo.".into()))
            }
            kinds::VIEW | kinds::SEQUENCE | kinds::SYNONYM => Ok(SyncScript {
                statements: vec![own_schema_rename(object, &to)],
                warnings: vec![format!(
                    "Oracle renombra vistas, secuencias y sinónimos solo en el esquema del usuario conectado: el script se detiene si la conexión no es la de {}.",
                    object.schema().map_or_else(|| "su dueño".to_string(), |s| format!("«{s}»"))
                )],
            }),
            kinds::PROCEDURE | kinds::FUNCTION | PACKAGE => recreate(req, object, &d),
            _ => Err(Error::Unsupported(format!("Oracle no renombra objetos de tipo «{}».", object.kind))),
        },
        RenameTarget::Column { table, column } => {
            one(format!("ALTER TABLE {} RENAME COLUMN {} TO {to}", qualified(table.schema(), &table.name), quote(column)))
        }
        RenameTarget::Index { table, index } => Ok(SyncScript {
            statements: vec![format!("ALTER INDEX {} RENAME TO {to}", qualified(table.schema(), index))],
            warnings: vec!["Los hints que nombran el índice (/*+ INDEX(…) */) son comentarios y no se actualizan: hay que revisarlos a mano.".into()],
        }),
        RenameTarget::Constraint { table, constraint } => {
            one(format!("ALTER TABLE {} RENAME CONSTRAINT {} TO {to}", qualified(table.schema(), &table.name), quote(constraint)))
        }
        RenameTarget::Schema { .. } => Err(Error::Unsupported("Oracle no renombra usuarios (esquemas).".into())),
    }
}

fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{}.{}", quote(s), quote(name)),
        None => quote(name),
    }
}

/// `'text'` as a SQL string literal.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `RENAME old TO new`, which only runs on the session user's own objects
/// with the current schema set to that user: refuses any other user, and
/// puts the current schema back afterwards (failed or not).
fn own_schema_rename(object: &ObjectRef, to: &str) -> String {
    let rename = literal(&format!("RENAME {} TO {to}", quote(&object.name)));
    let Some(owner) = object.schema() else {
        let refuse = literal(&format!(
            "RENAME solo renombra objetos del propio esquema: para renombrar «{}» el esquema actual tiene que ser el del usuario conectado.",
            object.name
        ));
        return format!(
            "BEGIN\n  IF SYS_CONTEXT('USERENV', 'SESSION_USER') <> SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') THEN\n    RAISE_APPLICATION_ERROR(-20001, {refuse});\n  END IF;\n  EXECUTE IMMEDIATE {rename};\nEND;"
        );
    };
    let refuse = literal(&format!(
        "RENAME solo renombra objetos del propio esquema: para renombrar «{}» hay que conectarse como el usuario {owner}.",
        object.name
    ));
    let set_owner = literal(&format!("ALTER SESSION SET CURRENT_SCHEMA = {}", quote(owner)));
    let back = "EXECUTE IMMEDIATE 'ALTER SESSION SET CURRENT_SCHEMA = ' || DBMS_ASSERT.ENQUOTE_NAME(prev, FALSE);";
    format!(
        "DECLARE\n  prev VARCHAR2(128) := SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA');\nBEGIN\n  IF SYS_CONTEXT('USERENV', 'SESSION_USER') <> {owner_lit} THEN\n    RAISE_APPLICATION_ERROR(-20001, {refuse});\n  END IF;\n  EXECUTE IMMEDIATE {set_owner};\n  BEGIN\n    EXECUTE IMMEDIATE {rename};\n  EXCEPTION WHEN OTHERS THEN\n    {back}\n    RAISE;\n  END;\n  {back}\nEND;",
        owner_lit = literal(owner)
    )
}

/// A procedure, function or package created under the new name from its
/// definition (each unit's header, its `END` label and calls to itself
/// renamed; plain `CREATE`, so an existing object of that name is never
/// replaced), then the old one dropped.
fn recreate(req: &RenameRequest, object: &ObjectRef, d: &ScriptDialect) -> Result<SyncScript> {
    let definition = req
        .definition
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| Error::Unsupported(format!("No se pudo leer la definición de «{}»: sin ella no se puede crear con el nombre nuevo.", object.name)))?;
    let spec = spec();
    let target = RewriteTarget::Object { object: object.clone() };
    let opts = RewriteOptions { dependent_schema: object.schema.clone(), keep_view_columns: false, ..Default::default() };
    let mut statements = Vec::new();
    for unit in units(definition, object, d)? {
        let body = rewrite_references(&unit, d, &target, &req.new_name, &spec, &opts).text;
        let renamed = rename_header(&body, d, FOLD, &req.new_name)
            .ok_or_else(|| Error::Unsupported(format!("No se reconoce el encabezado CREATE de «{}».", object.name)))?;
        statements.push(plain_create(&renamed));
    }
    if statements.is_empty() {
        return Err(Error::Unsupported(format!("La definición de «{}» no tiene un CREATE.", object.name)));
    }
    statements.push(format!("DROP {} {}", object.kind.to_uppercase(), qualified(object.schema(), &object.name)));
    Ok(SyncScript {
        statements,
        warnings: vec![format!(
            "Oracle no renombra procedimientos, funciones ni paquetes: se crea «{}» con el nombre nuevo y se borra «{}». Se pierden los permisos otorgados sobre el objeto viejo.",
            req.new_name, object.name
        )],
    })
}

/// The `CREATE` units of a definition, cut as SQL*Plus does: at `/` lines
/// outside comments and literals (q-quotes included), a package being its
/// spec and its body. `ALTER …` statements (`ALTER … ENABLE`) are left out.
///
/// The definition comes from the server, and its owner may not be the user
/// who renames it: the units have to be exactly the object's (one, or a
/// spec and its body), each naming it, or nothing is run. Anything else
/// would run a stranger's DDL with the renaming user's privileges.
fn units(definition: &str, object: &ObjectRef, d: &ScriptDialect) -> Result<Vec<String>> {
    let refuse = |why: String| Error::Unsupported(format!("La definición de «{}» no es solo la de ese objeto ({why}): no se renombra.", object.name));
    let mut out: Vec<(Header, String)> = Vec::new();
    for st in split_script(definition, d) {
        let toks = name_tokens(&st.text, d);
        let lead = toks.first().filter(|t| unquoted(&st.text, t)).map(|t| t.text.to_ascii_lowercase());
        match lead.as_deref() {
            Some("create") => {
                let h = header(&st.text, &toks).ok_or_else(|| refuse(format!("un CREATE en la línea {} sin encabezado reconocible", st.line)))?;
                out.push((h, st.text));
            }
            Some("alter") => {}
            _ => return Err(refuse(format!("una sentencia que no es CREATE en la línea {}", st.line))),
        }
    }
    let kind = object.kind.to_ascii_lowercase();
    let with_body = matches!(kind.as_str(), PACKAGE | kinds::TYPE);
    let max = if with_body { 2 } else { 1 };
    if out.len() > max {
        return Err(refuse(format!("tiene {} unidades CREATE y se esperaba {}", out.len(), if with_body { "la especificación y el cuerpo" } else { "una sola" })));
    }
    for (i, (h, _)) in out.iter().enumerate() {
        // The spec first, then (packages and types) its body.
        let body = i == 1;
        let label = format!("{}{}", h.kind.to_uppercase(), if h.body { " BODY" } else { "" });
        if h.kind != kind || h.body != body {
            return Err(refuse(format!("un CREATE {label} donde se esperaba {}{}", kind.to_uppercase(), if body { " BODY" } else { "" })));
        }
        let schema_ok = match (&h.schema, object.schema()) {
            (Some(s), Some(o)) => s == o,
            _ => true,
        };
        if h.name != object.name || !schema_ok {
            let named = h.schema.as_ref().map_or_else(|| h.name.clone(), |s| format!("{s}.{}", h.name));
            return Err(refuse(format!("un CREATE {label} de «{named}»")));
        }
    }
    Ok(out.into_iter().map(|(_, text)| text).collect())
}

/// What a `CREATE` unit's header creates: the kind (`package`, with `body`
/// for PACKAGE BODY), and its schema and name as Oracle stores them.
#[derive(Debug, PartialEq)]
struct Header {
    kind: String,
    body: bool,
    schema: Option<String>,
    name: String,
}

fn unquoted(text: &str, t: &NameToken<'_>) -> bool {
    t.kind == TokenKind::Name && text.as_bytes().get(t.start) != Some(&b'"')
}

/// `CREATE [OR REPLACE] [EDITIONABLE | NONEDITIONABLE | EDITIONING]
/// [[NO] FORCE] kind [BODY] [IF NOT EXISTS] [schema.]name`.
fn header(text: &str, toks: &[NameToken<'_>]) -> Option<Header> {
    const KINDS: &[&str] = &["procedure", "function", "package", "type", "trigger", "view"];
    const SKIP: &[&str] = &["or", "replace", "editionable", "noneditionable", "editioning", "no", "force"];
    let word = |k: usize| toks.get(k).filter(|t| unquoted(text, t)).map(|t| t.text.to_ascii_lowercase());
    let mut i = 1;
    while word(i).is_some_and(|w| SKIP.contains(&w.as_str())) {
        i += 1;
    }
    let kind = word(i).filter(|w| KINDS.contains(&w.as_str()))?;
    i += 1;
    let body = matches!(kind.as_str(), "package" | "type") && word(i).as_deref() == Some("body");
    if body {
        i += 1;
    }
    if word(i).as_deref() == Some("if") && word(i + 1).as_deref() == Some("not") && word(i + 2).as_deref() == Some("exists") {
        i += 3;
    }
    // A name as Oracle stores it: quoted as written, unquoted in upper case.
    let ident = |k: usize| {
        toks.get(k).filter(|t| t.kind == TokenKind::Name).map(|t| if unquoted(text, t) { t.text.to_uppercase() } else { t.text.replace("\"\"", "\"") })
    };
    let first = ident(i)?;
    let dot = toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".");
    let (schema, name) = if dot { (Some(first), ident(i + 2)?) } else { (None, first) };
    Some(Header { kind, body, schema, name })
}

/// `CREATE OR REPLACE …` as `CREATE …`: the new name mustn't replace an
/// object that already has it.
fn plain_create(unit: &str) -> String {
    let words: Vec<&str> = unit.splitn(4, char::is_whitespace).collect();
    if words.len() == 4 && words[1].eq_ignore_ascii_case("or") && words[2].eq_ignore_ascii_case("replace") {
        format!("{} {}", words[0], words[3].trim_start())
    } else {
        unit.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() }
    }

    #[test]
    fn the_schema_is_compiled_at_the_end() {
        let t = RenameTarget::Object { object: obj(kinds::TABLE, Some("APP"), "T"), parent: None };
        assert_eq!(spec().epilogue_for(&t).as_deref(), Some("BEGIN DBMS_UTILITY.COMPILE_SCHEMA('APP', FALSE); END;"));
    }

    fn req(target: RenameTarget, new: &str, definition: Option<&str>) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: definition.map(Into::into) }
    }

    fn object(kind: &str, name: &str, new: &str) -> SyncScript {
        script(&req(RenameTarget::Object { object: obj(kind, Some("APP"), name), parent: None }, new, None)).unwrap()
    }

    #[test]
    fn spec_matches_oracle() {
        let s = spec();
        assert_eq!(s.fold, Fold::Upper);
        assert!(!s.transactional && !s.schemas && s.columns && s.indexes && s.constraints);
        assert_eq!(s.replace, ReplaceStyle::CreateOrReplace);
        for k in ["table", "view", "sequence", "synonym", "trigger", "procedure", "function", "package"] {
            assert!(s.kinds.contains(&k.to_string()), "{k}");
        }
        assert!(!s.kinds.contains(&"materialized_view".to_string()));
        assert!(!s.kinds.contains(&"type".to_string()));
    }

    #[test]
    fn table_rename_quotes_and_folds() {
        assert_eq!(object("table", "CLIENTES", "CLIENTES_V2").statements, ["ALTER TABLE \"APP\".\"CLIENTES\" RENAME TO CLIENTES_V2"]);
        // Lower case is stored as is, so it needs quotes; a reserved word too.
        assert_eq!(object("table", "CLIENTES", "Clientes").statements, ["ALTER TABLE \"APP\".\"CLIENTES\" RENAME TO \"Clientes\""]);
        assert_eq!(object("table", "CLIENTES", "ORDER").statements, ["ALTER TABLE \"APP\".\"CLIENTES\" RENAME TO \"ORDER\""]);
        assert_eq!(object("table", "Mixed\"Q", "NUEVA").statements, ["ALTER TABLE \"APP\".\"Mixed\"\"Q\" RENAME TO NUEVA"]);
    }

    #[test]
    fn trigger_rename() {
        assert_eq!(object("trigger", "TRG_T", "TRG_NUEVO").statements, ["ALTER TRIGGER \"APP\".\"TRG_T\" RENAME TO TRG_NUEVO"]);
    }

    #[test]
    fn view_sequence_synonym_use_guarded_rename() {
        for kind in ["view", "sequence", "synonym"] {
            let s = object(kind, "V_CLI", "v nueva");
            assert_eq!(s.statements.len(), 1);
            let b = &s.statements[0];
            assert!(b.contains("IF SYS_CONTEXT('USERENV', 'SESSION_USER') <> 'APP' THEN"), "{b}");
            assert!(b.contains("EXECUTE IMMEDIATE 'ALTER SESSION SET CURRENT_SCHEMA = \"APP\"';"), "{b}");
            assert!(b.contains("EXECUTE IMMEDIATE 'RENAME \"V_CLI\" TO \"v nueva\"';"), "{b}");
            assert!(b.contains("RAISE_APPLICATION_ERROR(-20001, 'RENAME solo renombra objetos del propio esquema"), "{b}");
            assert_eq!(b.matches("DBMS_ASSERT.ENQUOTE_NAME(prev, FALSE)").count(), 2, "{b}");
            assert_eq!(s.warnings.len(), 1);
        }
        // Quotes in a name are doubled inside the dynamic SQL literal.
        let s = object("view", "O'V", "NUEVA");
        assert!(s.statements[0].contains("'RENAME \"O''V\" TO NUEVA'"), "{}", s.statements[0]);
        // No schema: the session's own one.
        let s = script(&req(RenameTarget::Object { object: obj("view", None, "V"), parent: None }, "W", None)).unwrap();
        assert!(s.statements[0].contains("<> SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')"), "{}", s.statements[0]);
        assert!(!s.statements[0].contains("ALTER SESSION"));
    }

    #[test]
    fn public_synonyms_and_other_kinds_are_refused() {
        let r = script(&req(RenameTarget::Object { object: obj("synonym", Some("PUBLIC"), "S"), parent: None }, "S2", None));
        assert!(matches!(r, Err(Error::Unsupported(_))));
        for kind in ["materialized_view", "type"] {
            let r = script(&req(RenameTarget::Object { object: obj(kind, Some("APP"), "X"), parent: None }, "Y", None));
            assert!(matches!(r, Err(Error::Unsupported(_))), "{kind}");
        }
        let r = script(&req(RenameTarget::Schema { database: None, schema: "APP".into() }, "APP2", None));
        assert!(matches!(r, Err(Error::Unsupported(_))));
    }

    #[test]
    fn column_rename() {
        let t = obj("table", Some("APP"), "T");
        let s = script(&req(RenameTarget::Column { table: t.clone(), column: "PEPE".into() }, "PEPA", None)).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE \"APP\".\"T\" RENAME COLUMN \"PEPE\" TO PEPA"]);
        let s = script(&req(RenameTarget::Column { table: t, column: "pepe".into() }, "Pepa", None)).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE \"APP\".\"T\" RENAME COLUMN \"pepe\" TO \"Pepa\""]);
    }

    #[test]
    fn index_rename_warns_about_hints() {
        let t = obj("table", Some("APP"), "T");
        let s = script(&req(RenameTarget::Index { table: t, index: "IX_T_PEPE".into() }, "IX_T_PEPA", None)).unwrap();
        assert_eq!(s.statements, ["ALTER INDEX \"APP\".\"IX_T_PEPE\" RENAME TO IX_T_PEPA"]);
        assert!(s.warnings[0].contains("hints"));
    }

    #[test]
    fn constraint_rename() {
        let t = obj("table", Some("APP"), "T");
        let s = script(&req(RenameTarget::Constraint { table: t, constraint: "SYS_C0013178".into() }, "CK_T_PEPE", None)).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE \"APP\".\"T\" RENAME CONSTRAINT \"SYS_C0013178\" TO CK_T_PEPE"]);
    }

    #[test]
    fn procedure_is_recreated() {
        let def = "  CREATE OR REPLACE EDITIONABLE PROCEDURE \"APP\".\"SUBIR\" (n NUMBER) AS\nBEGIN\n  IF n > 0 THEN subir(n - 1); END IF;\n  UPDATE t SET pepe = pepe + 1;\nEND subir;\n/";
        let r = req(RenameTarget::Object { object: obj("procedure", Some("APP"), "SUBIR"), parent: None }, "INCREMENTAR", Some(def));
        let s = script(&r).unwrap();
        assert_eq!(
            s.statements,
            [
                "CREATE EDITIONABLE PROCEDURE \"APP\".\"INCREMENTAR\" (n NUMBER) AS\nBEGIN\n  IF n > 0 THEN INCREMENTAR(n - 1); END IF;\n  UPDATE t SET pepe = pepe + 1;\nEND INCREMENTAR;",
                "DROP PROCEDURE \"APP\".\"SUBIR\"",
            ]
        );
        assert!(s.warnings[0].contains("permisos"));
    }

    #[test]
    fn function_is_recreated_with_quoted_new_name() {
        let def = "CREATE OR REPLACE FUNCTION \"APP\".\"F\" RETURN NUMBER AS BEGIN RETURN 1; END f;\n/";
        let r = req(RenameTarget::Object { object: obj("function", Some("APP"), "F"), parent: None }, "Doble", Some(def));
        let s = script(&r).unwrap();
        assert_eq!(s.statements, ["CREATE FUNCTION \"APP\".\"Doble\" RETURN NUMBER AS BEGIN RETURN 1; END \"Doble\";", "DROP FUNCTION \"APP\".\"F\""]);
    }

    #[test]
    fn package_spec_and_body_are_recreated() {
        let def = "  CREATE OR REPLACE EDITIONABLE PACKAGE \"APP\".\"PK\" AS\n  PROCEDURE x;\nEND pk;\n/\nCREATE OR REPLACE EDITIONABLE PACKAGE BODY \"APP\".\"PK\" AS\n  PROCEDURE x IS BEGIN pk.y; END x;\n  PROCEDURE y IS BEGIN NULL; END y;\nEND pk;\n/";
        let r = req(RenameTarget::Object { object: obj("package", Some("APP"), "PK"), parent: None }, "PK_NUEVO", Some(def));
        let s = script(&r).unwrap();
        assert_eq!(
            s.statements,
            [
                "CREATE EDITIONABLE PACKAGE \"APP\".\"PK_NUEVO\" AS\n  PROCEDURE x;\nEND PK_NUEVO;",
                "CREATE EDITIONABLE PACKAGE BODY \"APP\".\"PK_NUEVO\" AS\n  PROCEDURE x IS BEGIN PK_NUEVO.y; END x;\n  PROCEDURE y IS BEGIN NULL; END y;\nEND PK_NUEVO;",
                "DROP PACKAGE \"APP\".\"PK\"",
            ]
        );
    }

    fn rename_with(kind: &str, name: &str, def: &str) -> Result<SyncScript> {
        script(&req(RenameTarget::Object { object: obj(kind, Some("APP"), name), parent: None }, "NUEVO", Some(def)))
    }

    #[test]
    fn slash_lines_inside_comments_and_literals_do_not_split() {
        let cases = [
            // Block comment.
            "CREATE OR REPLACE PROCEDURE \"APP\".\"P\" AS\nBEGIN\n  /* uno\n/\nCREATE OR REPLACE PROCEDURE APP.X AS BEGIN NULL; END;\n*/\n  NULL;\nEND p;\n/",
            // Line comment (the `/` line is its own line, still in the body).
            "CREATE OR REPLACE PROCEDURE \"APP\".\"P\" AS\nBEGIN\n  NULL; -- a\n  -- /\n  NULL;\nEND p;\n/",
            // Normal string.
            "CREATE OR REPLACE PROCEDURE \"APP\".\"P\" AS\n  s VARCHAR2(200) := 'a\n/\nCREATE OR REPLACE PROCEDURE APP.X AS BEGIN NULL; END;\n';\nBEGIN\n  NULL;\nEND p;\n/",
            // q-quote, with a quote inside that would end a normal string.
            "CREATE OR REPLACE PROCEDURE \"APP\".\"P\" AS\n  s VARCHAR2(200) := q'[it's\n/\nCREATE OR REPLACE PROCEDURE APP.X AS BEGIN NULL; END;\n]';\nBEGIN\n  NULL;\nEND p;\n/",
        ];
        for def in cases {
            let s = rename_with("procedure", "P", def).unwrap_or_else(|e| panic!("{e}: {def}"));
            assert_eq!(s.statements.len(), 2, "{def}");
            assert!(s.statements[0].starts_with("CREATE PROCEDURE \"APP\".\"NUEVO\" AS"), "{}", s.statements[0]);
            assert!(s.statements[0].ends_with("END NUEVO;"), "{}", s.statements[0]);
            assert_eq!(s.statements[1], "DROP PROCEDURE \"APP\".\"P\"");
        }
    }

    #[test]
    fn a_definition_with_another_unit_is_refused() {
        let refused = |kind: &str, name: &str, def: &str| match rename_with(kind, name, def) {
            Err(Error::Unsupported(m)) => assert!(m.contains(&format!("«{name}»")) && m.contains("no se renombra"), "{m}"),
            other => panic!("{other:?}: {def}"),
        };
        // A second unit after a real `/` line (the comment before it closed).
        refused("procedure", "P", "CREATE OR REPLACE PROCEDURE \"APP\".\"P\" AS\nBEGIN\n  NULL; /* x */\nEND p;\n/\nCREATE OR REPLACE PROCEDURE \"APP\".\"P2\" AS BEGIN EXECUTE IMMEDIATE 'GRANT DBA TO pepe'; END;\n/");
        // Even another unit of the same name: a procedure is one unit.
        refused("procedure", "P", "CREATE PROCEDURE APP.P AS BEGIN NULL; END;\n/\nCREATE PROCEDURE APP.P AS BEGIN NULL; END;\n/");
        // A unit of another kind, or of the same name in another schema.
        refused("function", "F", "CREATE OR REPLACE PROCEDURE \"APP\".\"F\" AS BEGIN NULL; END;\n/");
        refused("procedure", "P", "CREATE OR REPLACE PROCEDURE \"SYS\".\"P\" AS BEGIN NULL; END;\n/");
        refused("procedure", "P", "CREATE OR REPLACE PROCEDURE \"APP\".\"p\" AS BEGIN NULL; END;\n/");
        // Something that isn't a CREATE after the unit.
        refused("procedure", "P", "CREATE OR REPLACE PROCEDURE \"APP\".\"P\" AS BEGIN NULL; END;\n/\nGRANT DBA TO pepe;");
        // A package: its spec and body only, in that order.
        let spec = "CREATE OR REPLACE PACKAGE \"APP\".\"PK\" AS PROCEDURE x; END pk;\n/\n";
        let body = "CREATE OR REPLACE PACKAGE BODY \"APP\".\"PK\" AS PROCEDURE x IS BEGIN NULL; END x; END pk;\n/\n";
        refused("package", "PK", &format!("{spec}{body}CREATE OR REPLACE PROCEDURE APP.X AS BEGIN NULL; END;\n/"));
        refused("package", "PK", &format!("{spec}{spec}"));
        refused("package", "PK", body);
        refused("package", "PK", &format!("{spec}CREATE OR REPLACE PACKAGE BODY \"APP\".\"OTRO\" AS END;\n/"));
    }

    #[test]
    fn package_spec_alone_and_unquoted_source_still_work() {
        let s = rename_with("package", "PK", "CREATE OR REPLACE PACKAGE \"APP\".\"PK\" AS\n  PROCEDURE x;\nEND pk;\n/").unwrap();
        assert_eq!(s.statements, ["CREATE PACKAGE \"APP\".\"NUEVO\" AS\n  PROCEDURE x;\nEND NUEVO;", "DROP PACKAGE \"APP\".\"PK\""]);
        // ALL_SOURCE's text (the fallback): the name as the user wrote it.
        let s = rename_with("procedure", "P", "CREATE OR REPLACE procedure p as\nbegin\n  null;\nend;\n/").unwrap();
        assert_eq!(s.statements[0], "CREATE procedure NUEVO as\nbegin\n  null;\nend;");
        // A trailing ALTER (as GET_DDL writes for some kinds) is left out.
        let s = rename_with("procedure", "P", "CREATE OR REPLACE PROCEDURE \"APP\".\"P\" AS BEGIN NULL; END;\n/\nALTER PROCEDURE \"APP\".\"P\" COMPILE;").unwrap();
        assert_eq!(s.statements.len(), 2);
    }

    #[test]
    fn routine_without_definition_is_refused() {
        let r = req(RenameTarget::Object { object: obj("procedure", Some("APP"), "P"), parent: None }, "Q", None);
        assert!(matches!(script(&r), Err(Error::Unsupported(_))));
    }
}
