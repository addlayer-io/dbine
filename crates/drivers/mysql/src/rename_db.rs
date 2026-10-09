//! "Renombrar…" on a database. MySQL has no `RENAME DATABASE` (it existed
//! briefly in 5.1 and was removed), so the rename moves what the database
//! holds into a new one:
//!
//! 0. A guard: the old database holds exactly what the app read (tables,
//!    views, routines, triggers and events, counted in information_schema),
//!    or the first statement fails before anything changes. Events (the
//!    explorer doesn't list them), MariaDB sequences, what the login can't
//!    read and what changed since the app read it would be left half-moved.
//!    Right after it, each routine's, trigger's and event's own `sql_mode`
//!    and `collation_connection` (information_schema) go into user
//!    variables, before anything is dropped.
//! 1. `CREATE DATABASE` with the old one's charset and collation (read from
//!    `information_schema.SCHEMATA` and run as a prepared statement).
//! 2. The triggers are dropped: `RENAME TABLE` refuses to move a table that
//!    has any ("Trigger in wrong schema", 1435).
//! 3. Every table moves in one `RENAME TABLE a.t TO b.t, …` (atomic, and
//!    the foreign keys between them follow). Views can't move across
//!    databases (1450).
//! 4. Routines, views (after the routines: `CREATE VIEW` checks the
//!    functions it calls, and in dependency order), triggers and events are
//!    created in the new database from their definitions, with `old.`
//!    qualifiers rewritten to `new.`, the name qualified with it and the new
//!    database as the default one (a `USE` of its own: bare names resolve
//!    there). The `DEFINER` is kept, as in an object rename. Like
//!    mysqldump, each routine, trigger and event is created under the
//!    `sql_mode` and `collation_connection` it was created with, and the
//!    session's are put back after it: its body was parsed under that mode
//!    (`NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`…), and under another one the
//!    same text could end early and run what follows as statements of
//!    their own, with the privileges of whoever renames. Every `CREATE`
//!    goes in a request of its own, and a definition that isn't exactly one
//!    statement (with backslash escapes and without) is refused.
//! 5. The views, routines and events left in the old one are dropped, and
//!    the old database last, only if nothing is left in it (a prepared
//!    statement that otherwise fails on purpose).
//!
//! Every statement names its database, so it runs from any connection. The
//! statements share session state (the captured modes, the `USE`): they run
//! one after the other on one session, as the app does; on another, the
//! mode variables are NULL and `SET sql_mode = NULL` fails before the
//! `CREATE`.
//! MySQL (Aurora, Cloud SQL) and MariaDB. The others don't offer it:
//! TiDB moves the tables, but their foreign keys keep naming the old
//! database (`REFERENCES old.t`, checked on 8.5), so they'd break when it's
//! dropped; SingleStore, StarRocks, Doris, Databend and GreptimeDB can't
//! move a table to another database with a rename; Manticore has one
//! namespace; OceanBase hasn't been checked.

use crate::rename::spec;
use crate::Variant;
use dbine_driver::rename::{names_in_code, rewrite_references, DatabaseObject, RewriteOptions, RewriteTarget, UnresolvedReason};
use dbine_driver::sql::{code_tokens, quote_ident, split_script, NameToken, Quote, ScriptDialect, TokenKind};
use dbine_driver::{kinds, Error, Result, SyncScript};

const EVENT: &str = "event";

/// Databases the server owns.
const SYSTEM: &[&str] = &["mysql", "sys", "information_schema", "performance_schema", "metrics_schema"];

/// It renames databases.
pub(crate) fn supported(v: Variant) -> bool {
    matches!(v.base(), Variant::MySql | Variant::MariaDb)
}

pub(crate) const NOTE: &str = "MySQL no tiene RENAME DATABASE: DBine crea la base nueva con el mismo juego de caracteres e intercalación, borra los triggers de la vieja, mueve todas las tablas con un solo RENAME TABLE (con sus filas, índices y claves foráneas), vuelve a crear en la nueva las rutinas, vistas, triggers y eventos (cambiando la base vieja por la nueva donde su código la nombra) y al final borra la vieja, solo si quedó vacía. No es atómico: si algo falla a mitad de camino, las tablas ya movidas quedan en la base nueva y hay que terminar a mano. Los permisos (GRANT) sobre la base vieja y sus objetos no se copian. El código de otras bases y las aplicaciones que nombran la base vieja no se cambian.";

const NOT_ATOMIC: &str = "No es atómico: cada sentencia se confirma sola. Si una falla, lo anterior queda hecho: las tablas ya movidas quedan en la base nueva y los objetos borrados de la vieja ya no están; el script se puede seguir a mano desde la sentencia que falló.";
const GRANTS: &str = "Los permisos no se copian: los GRANT … ON {old}.* y los otorgados sobre sus tablas, vistas y rutinas siguen nombrando la base vieja y no valen para la nueva. Revisalos (SHOW GRANTS FOR cada usuario, o information_schema.SCHEMA_PRIVILEGES y TABLE_PRIVILEGES) y otorgalos de nuevo sobre {new}.";
const OUTSIDE: &str = "No se cambia lo que nombra la base desde afuera: vistas, rutinas y triggers de otras bases, eventos, jobs y las aplicaciones (cadenas de conexión, consultas con {old}.tabla) siguen apuntando a la base vieja.";
const DEFINER: &str = "Las rutinas, vistas, triggers y eventos se crean con su DEFINER original: si no es tu usuario hace falta el privilegio SET_USER_ID (SET_ANY_DEFINER desde MySQL 8.2) o SUPER (SET USER en MariaDB).";
const GUARD: &str = "La primera sentencia comprueba que la base tenga exactamente las tablas, vistas, rutinas y triggers que DBine leyó y ningún evento ni secuencia: si tiene algo más (eventos, secuencias de MariaDB, objetos que tu usuario no puede leer o creados después de leerla), falla a propósito con «Table '…DBine: hay eventos, secuencias u objetos sin leer; nada cambió' doesn't exist» antes de cambiar nada. Esas bases no se renombran desde DBine.";
const MODES: &str = "Las rutinas, triggers y eventos se vuelven a crear con el sql_mode con que se crearon (como mysqldump). Si alguno ya no está con ese nombre o usa ORACLE o MSSQL (MariaDB), modos que cambian cómo se lee su código, la sentencia que lo comprueba falla con «Table '…DBine: un objeto usa un sql_mode no admitido; nada cambió' doesn't exist» antes de crear la base nueva.";
const LAST_DROP: &str = "La última sentencia borra la base vieja solo si quedó vacía; si apareció algo mientras corría el script, falla a propósito con «Table '…DBine: la base no quedó vacía y no se borra' doesn't exist» y la base queda para revisarla.";

/// The table the guards select from to fail: its name is the message
/// (at most 64 characters, MySQL's limit for a name).
const NOT_READ: &str = "DBine: hay eventos, secuencias u objetos sin leer; nada cambió";
const NOT_EMPTY: &str = "DBine: la base no quedó vacía y no se borra";
const BAD_MODE: &str = "DBine: un objeto usa un sql_mode no admitido; nada cambió";

/// `sql_mode` flags that change how the server reads a body beyond what
/// [`one_statement`] models (MariaDB's ORACLE and MSSQL): an object
/// carrying one isn't created again.
const UNMODELED: &[&str] = &["ORACLE", "MSSQL"];

/// SQL true when the mode in `var` (a user variable) is NULL (the object
/// isn't there by that name) or has a flag in [`UNMODELED`].
fn bad_mode(var: &str) -> String {
    let flags: Vec<String> = UNMODELED.iter().map(|f| format!("FIND_IN_SET('{f}', {var}) > 0")).collect();
    format!("{var} IS NULL OR {}", flags.join(" OR "))
}

fn q(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

/// A string literal with backslash escapes (the mysql dialect's).
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// A prepared statement from `sql`, an expression: one chunk, run in one
/// request so the user variable stays its own.
fn prepared(sql: &str) -> String {
    format!("SET @dbine_sql = {sql};\nPREPARE dbine_rename FROM @dbine_sql;\nEXECUTE dbine_rename;\nDEALLOCATE PREPARE dbine_rename;")
}

fn drop_word(kind: &str) -> &'static str {
    match kind {
        kinds::VIEW => "VIEW",
        kinds::PROCEDURE => "PROCEDURE",
        kinds::FUNCTION => "FUNCTION",
        kinds::TRIGGER => "TRIGGER",
        _ => "EVENT",
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        kinds::VIEW => "la vista",
        kinds::PROCEDURE => "el procedimiento",
        kinds::FUNCTION => "la función",
        kinds::TRIGGER => "el trigger",
        _ => "el evento",
    }
}

/// The statements that rename `database` to `new_name` on `v`.
pub(crate) fn script(v: Variant, database: &str, new_name: &str, objects: &[DatabaseObject]) -> Result<SyncScript> {
    if !supported(v) {
        return Err(Error::Unsupported("este motor no renombra bases de datos".into()));
    }
    for name in [database, new_name] {
        if SYSTEM.iter().any(|s| s.eq_ignore_ascii_case(name)) {
            return Err(Error::Unsupported(format!("«{name}» es una base del sistema: no se renombra ni se reemplaza")));
        }
    }
    if new_name.trim().is_empty() || new_name.eq_ignore_ascii_case(database) {
        return Err(Error::Query("el nombre nuevo tiene que ser distinto del actual".into()));
    }
    let spec = spec(v).expect("the variants that rename databases rename objects");
    let dialect = crate::script_dialect(v);
    let (old, new) = (q(database), q(new_name));
    let mut statements = Vec::new();
    let mut warnings = vec![NOT_ATOMIC.to_string(), GRANTS.replace("{old}", &old).replace("{new}", &new), OUTSIDE.replace("{old}", &old)];

    let of = |kind: &'static str| objects.iter().filter(move |o| o.kind == kind);
    let code_kinds = [kinds::PROCEDURE, kinds::FUNCTION, kinds::VIEW, kinds::TRIGGER, EVENT];
    fn readable(o: &&DatabaseObject) -> bool {
        o.definition.as_deref().is_some_and(|d| !d.trim().is_empty())
    }
    // Nothing is left behind: code that can't be created again, or a kind
    // this doesn't move, refuses the whole rename.
    if let Some(o) = objects.iter().find(|o| code_kinds.contains(&o.kind.as_str()) && !readable(o)) {
        return Err(Error::Query(format!(
            "no se pudo leer la definición de {} «{}»: no se podría volver a crear en la base nueva, así que la base no se renombra (revisá los privilegios SHOW VIEW, TRIGGER o los de las rutinas)",
            kind_label(&o.kind),
            o.name
        )));
    }
    if let Some(o) = objects.iter().find(|o| o.kind != kinds::TABLE && !code_kinds.contains(&o.kind.as_str())) {
        return Err(Error::Unsupported(format!("la base tiene «{}» ({}), que DBine no mueve a otra base: no se renombra", o.name, o.kind)));
    }

    // The code as it's created again, checked before anything is written.
    let mut texts: Vec<(&DatabaseObject, String)> = Vec::new();
    for o in objects.iter().filter(|o| code_kinds.contains(&o.kind.as_str())) {
        let def = o.definition.as_deref().unwrap_or_default();
        let rw = rewrite_references(def, &dialect, &RewriteTarget::Schema { schema: database.to_string() }, new_name, &spec, &RewriteOptions::default());
        for u in &rw.unresolved {
            warnings.push(format!(
                "{} «{}», línea {}: «{}» nombra la base vieja {} y no se cambió; revisalo en el script antes de ejecutarlo.",
                kind_label(&o.kind),
                o.name,
                u.line,
                u.text,
                reason(u.reason)
            ));
        }
        let text = qualify(&rw.text, &dialect, &o.kind, &new);
        let text = text.trim_end().trim_end_matches(';').trim_end().to_string();
        if !one_statement(def, &dialect) || !one_statement(&text, &dialect) {
            return Err(Error::Query(format!(
                "la definición de {} «{}» no se lee como una sola sentencia (con y sin escapes de barra invertida): no se vuelve a crear, así que la base no se renombra; revisala a mano",
                kind_label(&o.kind),
                o.name
            )));
        }
        texts.push((o, text));
    }
    let text_of = |o: &DatabaseObject| texts.iter().find(|(x, _)| std::ptr::eq(*x, o)).map(|(_, t)| t.clone()).unwrap_or_default();

    // 0. The guard: exactly what was read is there, or nothing changes.
    let l = lit(database);
    let n = |kind: &'static str| of(kind).count();
    let count = |from: &str, column: &str, extra: &str| format!("(SELECT COUNT(*) FROM information_schema.{from} WHERE {column} = {l}{extra})");
    statements.push(prepared(&format!(
        "IF({} = {} AND {} = {} AND {} = {} AND {} = {} AND {} = {}, 'DO 0', {})",
        count("TABLES", "TABLE_SCHEMA", " AND TABLE_TYPE <> 'VIEW'"),
        n(kinds::TABLE),
        count("TABLES", "TABLE_SCHEMA", " AND TABLE_TYPE = 'VIEW'"),
        n(kinds::VIEW),
        count("ROUTINES", "ROUTINE_SCHEMA", ""),
        n(kinds::PROCEDURE) + n(kinds::FUNCTION),
        count("TRIGGERS", "TRIGGER_SCHEMA", ""),
        n(kinds::TRIGGER),
        count("EVENTS", "EVENT_SCHEMA", ""),
        n(EVENT),
        lit(&format!("SELECT * FROM {old}.{}", q(NOT_READ))),
    )));
    warnings.push(GUARD.to_string());

    // Each body's own mode, read while the old objects are still there.
    let routines: Vec<&DatabaseObject> = objects.iter().filter(|o| matches!(o.kind.as_str(), kinds::FUNCTION | kinds::PROCEDURE)).collect();
    let triggers: Vec<&DatabaseObject> = of(kinds::TRIGGER).collect();
    let events: Vec<&DatabaseObject> = of(EVENT).collect();
    let moded: Vec<&DatabaseObject> = routines.iter().chain(&triggers).chain(&events).copied().collect();
    for (i, o) in moded.iter().enumerate() {
        let name = lit(&o.name);
        let (from, filter) = match o.kind.as_str() {
            kinds::TRIGGER => ("TRIGGERS", format!("TRIGGER_SCHEMA = {l} AND TRIGGER_NAME = {name}")),
            EVENT => ("EVENTS", format!("EVENT_SCHEMA = {l} AND EVENT_NAME = {name}")),
            k => ("ROUTINES", format!("ROUTINE_SCHEMA = {l} AND ROUTINE_NAME = {name} AND ROUTINE_TYPE = '{}'", if k == kinds::PROCEDURE { "PROCEDURE" } else { "FUNCTION" })),
        };
        let n = i + 1;
        statements.push(format!(
            "SET @dbine_m{n} = (SELECT SQL_MODE FROM information_schema.{from} WHERE {filter}), @dbine_c{n} = (SELECT COLLATION_CONNECTION FROM information_schema.{from} WHERE {filter});"
        ));
    }

    // Before anything changes: every object is there by name, under a mode
    // its text was checked for.
    if !moded.is_empty() {
        let bad: Vec<String> = (1..=moded.len()).map(|n| format!("({})", bad_mode(&format!("@dbine_m{n}")))).collect();
        statements.push(prepared(&format!("IF({}, {}, 'DO 0')", bad.join(" OR "), lit(&format!("SELECT * FROM {old}.{}", q(BAD_MODE))))));
        warnings.push(MODES.to_string());
    }

    // 1. The new database, with the old one's defaults.
    statements.push(prepared(&format!(
        "COALESCE((SELECT CONCAT({}, DEFAULT_CHARACTER_SET_NAME, ' COLLATE ', DEFAULT_COLLATION_NAME) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {}), {})",
        lit(&format!("CREATE DATABASE {new} CHARACTER SET ")),
        lit(database),
        lit(&format!("CREATE DATABASE {new}")),
    )));

    // 2. Triggers: RENAME TABLE won't move a table that has them.
    statements.extend(triggers.iter().map(|t| format!("DROP TRIGGER {old}.{};", q(&t.name))));

    // 3. The tables, all at once.
    let moves: Vec<String> = of(kinds::TABLE).map(|t| format!("{old}.{n} TO {new}.{n}", n = q(&t.name))).collect();
    if !moves.is_empty() {
        statements.push(format!("RENAME TABLE {};", moves.join(",\n  ")));
    }

    // 4. The code, in the new database: SHOW CREATE VIEW leaves out the
    // database of the tables in the session's own (the app reads the
    // definitions from a session on the old one), and routine and trigger
    // bodies name tables bare.
    let views = view_order(of(kinds::VIEW).collect(), &dialect);
    if !texts.is_empty() {
        statements.push(format!("USE {new};"));
    }
    if !moded.is_empty() {
        statements.push("SET @dbine_saved_m = @@SESSION.sql_mode, @dbine_saved_c = @@SESSION.collation_connection;".to_string());
    }
    // Under its own mode, then the session's back. The client character set
    // stays the session's: the text is sent in it (utf8mb4), and declaring
    // another one (GBK, SJIS…) would make the server read these bytes as
    // that charset, where a 0x5C inside a character is a backslash.
    let under_own_mode = |o: &DatabaseObject, statements: &mut Vec<String>| {
        let n = moded.iter().position(|x| std::ptr::eq(*x, o)).expect("moded") + 1;
        // NULL (an error) again if the mode isn't one the text was checked for.
        let var = format!("@dbine_m{n}");
        statements.push(format!("SET SESSION sql_mode = IF({}, NULL, {var}), SESSION collation_connection = @dbine_c{n};", bad_mode(&var)));
        statements.push(format!("{};", text_of(o)));
        statements.push("SET SESSION sql_mode = @dbine_saved_m, SESSION collation_connection = @dbine_saved_c;".to_string());
    };
    for o in &routines {
        under_own_mode(o, &mut statements);
    }
    for o in &views {
        statements.push(format!("{};", text_of(o)));
    }
    for o in triggers.iter().chain(&events) {
        under_own_mode(o, &mut statements);
    }

    // 5. What's left of the old one, then the database if it's empty.
    for o in views.iter().rev().chain(&routines).chain(&events) {
        statements.push(format!("DROP {} {old}.{};", drop_word(&o.kind), q(&o.name)));
    }
    statements.push(prepared(&format!(
        "IF((SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = {l}) + (SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = {l}) + (SELECT COUNT(*) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = {l}) = 0, {}, {})",
        lit(&format!("DROP DATABASE {old}")),
        lit(&format!("SELECT * FROM {old}.{}", q(NOT_EMPTY))),
    )));
    warnings.push(LAST_DROP.to_string());
    if objects.iter().any(|o| code_kinds.contains(&o.kind.as_str())) {
        warnings.push(DEFINER.to_string());
    }
    Ok(SyncScript { statements, warnings })
}

/// The server reads `text` as one statement under every mode that changes
/// how it's tokenized and that a routine may carry: with backslash escapes
/// or `NO_BACKSLASH_ESCAPES`, and with `"…"` as a string or as an
/// identifier (`ANSI_QUOTES`). The modes are read at run time, after the
/// text: whichever the object has by then, its text is one statement.
/// Modes that change more (MariaDB's ORACLE, MSSQL) are refused by the
/// script itself ([`UNMODELED`]). The client's `DELIMITER` means nothing to
/// the server.
fn one_statement(text: &str, d: &ScriptDialect) -> bool {
    let d = ScriptDialect { delimiter_command: false, ..*d };
    [(true, false), (true, true), (false, false), (false, true)]
        .into_iter()
        .all(|(backslash_escapes, ansi_quotes)| split_script(&lexed(text, backslash_escapes, ansi_quotes), &d).iter().filter(|s| !s.text.trim().is_empty()).count() == 1)
}

/// `text` as the server's lexer reads it under one mode, with nothing left
/// for the splitter to read differently: each string becomes `'s'`, each
/// quoted identifier `` `i` ``, comments go (an executable `/*! … */` or
/// MariaDB `/*M! … */` keeps its contents as code, as the server runs
/// them). An unterminated literal swallows the rest, as it does on the
/// server (where the statement then fails).
fn lexed(text: &str, backslash_escapes: bool, ansi_quotes: bool) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut executable = false;
    // The end of a literal closed by `q`, doubled `q` escaping it.
    let end = |mut i: usize, q: u8, backslash: bool| -> Option<usize> {
        while i < b.len() {
            if backslash && b[i] == b'\\' {
                i += 2;
            } else if b[i] == q {
                if b.get(i + 1) == Some(&q) {
                    i += 2;
                } else {
                    return Some(i + 1);
                }
            } else {
                i += 1;
            }
        }
        None
    };
    while i < b.len() {
        let c = b[i];
        let literal = match c {
            b'\'' => Some((b'\'', backslash_escapes, "'s'")),
            b'"' if ansi_quotes => Some((b'"', false, "`i`")),
            b'"' => Some((b'"', backslash_escapes, "'s'")),
            b'`' => Some((b'`', false, "`i`")),
            _ => None,
        };
        if let Some((q, backslash, placeholder)) = literal {
            out.push(' ');
            out.push_str(placeholder);
            out.push(' ');
            match end(i + 1, q, backslash) {
                Some(e) => i = e,
                None => return out,
            }
        } else if c == b'#' || (c == b'-' && b.get(i + 1) == Some(&b'-') && b.get(i + 2).is_none_or(|n| n.is_ascii_whitespace() || n.is_ascii_control())) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let rest = &text[i + 2..];
            let opener = if rest.starts_with('!') { Some(1) } else if rest.starts_with("M!") { Some(2) } else { None };
            if let (Some(n), false) = (opener, executable) {
                executable = true;
                i += 2 + n;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                out.push(' ');
            } else {
                match text[i + 2..].find("*/") {
                    Some(e) => {
                        i += 2 + e + 2;
                        out.push(' ');
                    }
                    None => return out,
                }
            }
        } else if executable && c == b'*' && b.get(i + 1) == Some(&b'/') {
            executable = false;
            i += 2;
            out.push(' ');
        } else {
            let n = text[i..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&text[i..i + n]);
            i += n;
        }
    }
    out
}

fn reason(r: UnresolvedReason) -> &'static str {
    match r {
        UnresolvedReason::InString => "dentro de un texto (SQL dinámico)",
        UnresolvedReason::AliasNamedLikeSchema => "y un alias se llama igual que la base",
        UnresolvedReason::Case => "con otras mayúsculas",
        _ => "de una forma que no se puede cambiar con seguridad",
    }
}

fn unquoted_word(body: &str, t: &NameToken<'_>, words: &[&str]) -> bool {
    t.kind == TokenKind::Name && !body[t.start..t.end].starts_with('`') && words.iter().any(|w| t.text.eq_ignore_ascii_case(w))
}

fn dotted(toks: &[NameToken<'_>], i: usize) -> bool {
    toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".")
}

/// `definition` with the object's name, and a trigger's table, qualified
/// with `db` where they aren't: SHOW CREATE writes them bare.
fn qualify(definition: &str, dialect: &dbine_driver::ScriptDialect, kind: &str, db: &str) -> String {
    let toks = code_tokens(definition, dialect);
    let Some(create) = toks.iter().position(|t| unquoted_word(definition, t, &["create"])) else {
        return definition.to_string();
    };
    let word = drop_word(kind);
    let Some(k) = toks.iter().skip(create + 1).position(|t| unquoted_word(definition, t, &[word])).map(|p| p + create + 1) else {
        return definition.to_string();
    };
    let mut name = k + 1;
    while toks.get(name).is_some_and(|t| unquoted_word(definition, t, &["if", "not", "exists"])) {
        name += 1;
    }
    let mut at = Vec::new();
    if toks.get(name).is_some_and(|t| t.kind == TokenKind::Name) && !dotted(&toks, name) {
        at.push(toks[name].start);
    }
    if kind == kinds::TRIGGER {
        // `… BEFORE INSERT ON t FOR EACH ROW`: the first ON after the name.
        let after = if dotted(&toks, name) { name + 2 } else { name };
        if let Some(on) = toks.iter().skip(after + 1).position(|t| unquoted_word(definition, t, &["on"])).map(|p| p + after + 1) {
            if toks.get(on + 1).is_some_and(|t| t.kind == TokenKind::Name) && !dotted(&toks, on + 1) {
                at.push(toks[on + 1].start);
            }
        }
    }
    let mut out = definition.to_string();
    for p in at.into_iter().rev() {
        out.insert_str(p, &format!("{db}."));
    }
    out
}

/// Views after the views they read; a cycle (or a name the check mistakes)
/// keeps the catalog's order for the rest.
fn view_order<'a>(views: Vec<&'a DatabaseObject>, dialect: &dbine_driver::ScriptDialect) -> Vec<&'a DatabaseObject> {
    let reads = |a: &DatabaseObject, b: &DatabaseObject| !std::ptr::eq(a, b) && names_in_code(a.definition.as_deref().unwrap_or_default(), dialect, &b.name);
    let mut pending = views;
    let mut out = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let next = pending.iter().position(|a| !pending.iter().any(|b| reads(a, b))).unwrap_or(0);
        out.push(pending.remove(next));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, name: &str, definition: Option<&str>) -> DatabaseObject {
        DatabaseObject { kind: kind.into(), schema: None, name: name.into(), definition: definition.map(Into::into) }
    }

    fn sample() -> Vec<DatabaseObject> {
        vec![
            obj("table", "clientes", None),
            obj("table", "pedidos", None),
            // As SHOW CREATE VIEW writes them: the body fully qualified.
            obj(
                "view",
                "v_top",
                Some("CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v_top` AS select `v_cli`.`id` AS `id` from `tienda`.`v_cli`"),
            ),
            obj(
                "view",
                "v_cli",
                Some("CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v_cli` AS select `tienda`.`clientes`.`id` AS `id` from `tienda`.`clientes`"),
            ),
            obj(
                "procedure",
                "p_total",
                Some("CREATE DEFINER=`root`@`%` PROCEDURE `p_total`()\nBEGIN\n  SELECT COUNT(*) FROM tienda.pedidos;\n  SELECT 'tienda.pedidos';\nEND"),
            ),
            obj("function", "f_doble", Some("CREATE DEFINER=`root`@`%` FUNCTION `f_doble`(x INT) RETURNS int\n    DETERMINISTIC\nRETURN x * 2")),
            obj("trigger", "tr_pedidos", Some("CREATE DEFINER=`root`@`%` TRIGGER tr_pedidos BEFORE INSERT ON pedidos FOR EACH ROW SET NEW.total = f_doble(NEW.total)")),
            obj("event", "ev_limpia", Some("CREATE DEFINER=`root`@`%` EVENT `ev_limpia` ON SCHEDULE EVERY 1 DAY DO DELETE FROM `tienda`.`pedidos` WHERE total < 0")),
        ]
    }

    #[test]
    fn full_script() {
        let s = script(Variant::MySql, "tienda", "negocio", &sample()).unwrap();
        let st = &s.statements;
        // The guard first: 2 tables, 2 views, 2 routines, 1 trigger, 1 event.
        assert!(st[0].contains("TABLE_TYPE <> 'VIEW') = 2 AND (SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'tienda' AND TABLE_TYPE = 'VIEW') = 2 AND (SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = 'tienda') = 2 AND (SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = 'tienda') = 1 AND (SELECT COUNT(*) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = 'tienda') = 1, 'DO 0'"), "{}", st[0]);
        // Each body's own mode, before anything is dropped.
        assert_eq!(st[1], "SET @dbine_m1 = (SELECT SQL_MODE FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = 'tienda' AND ROUTINE_NAME = 'p_total' AND ROUTINE_TYPE = 'PROCEDURE'), @dbine_c1 = (SELECT COLLATION_CONNECTION FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = 'tienda' AND ROUTINE_NAME = 'p_total' AND ROUTINE_TYPE = 'PROCEDURE');");
        assert!(st[2].contains("ROUTINE_NAME = 'f_doble' AND ROUTINE_TYPE = 'FUNCTION'") && st[2].starts_with("SET @dbine_m2 = "));
        assert_eq!(st[3], "SET @dbine_m3 = (SELECT SQL_MODE FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = 'tienda' AND TRIGGER_NAME = 'tr_pedidos'), @dbine_c3 = (SELECT COLLATION_CONNECTION FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = 'tienda' AND TRIGGER_NAME = 'tr_pedidos');");
        assert!(st[4].starts_with("SET @dbine_m4 = (SELECT SQL_MODE FROM information_schema.EVENTS WHERE EVENT_SCHEMA = 'tienda' AND EVENT_NAME = 'ev_limpia')"));
        // Then every one of them is there under a mode its text was checked for.
        assert!(st[5].starts_with("SET @dbine_sql = IF((@dbine_m1 IS NULL OR FIND_IN_SET('ORACLE', @dbine_m1) > 0 OR FIND_IN_SET('MSSQL', @dbine_m1) > 0) OR ("), "{}", st[5]);
        assert!(st[5].contains("(@dbine_m4 IS NULL OR FIND_IN_SET('ORACLE', @dbine_m4) > 0 OR FIND_IN_SET('MSSQL', @dbine_m4) > 0), 'SELECT * FROM `tienda`.`DBine: un objeto usa un sql_mode no admitido; nada cambió`', 'DO 0');"));
        assert!(st[6].starts_with("SET @dbine_sql = COALESCE((SELECT CONCAT('CREATE DATABASE `negocio` CHARACTER SET ', DEFAULT_CHARACTER_SET_NAME"), "{}", st[6]);
        assert!(st[6].contains("WHERE SCHEMA_NAME = 'tienda'), 'CREATE DATABASE `negocio`');\nPREPARE dbine_rename FROM @dbine_sql;\nEXECUTE dbine_rename;"));
        assert_eq!(st[7], "DROP TRIGGER `tienda`.`tr_pedidos`;");
        assert_eq!(st[8], "RENAME TABLE `tienda`.`clientes` TO `negocio`.`clientes`,\n  `tienda`.`pedidos` TO `negocio`.`pedidos`;");
        assert_eq!(st[9], "USE `negocio`;");
        assert_eq!(st[10], "SET @dbine_saved_m = @@SESSION.sql_mode, @dbine_saved_c = @@SESSION.collation_connection;");
        // Routines first (views check the functions they call), each one
        // under its own mode, in a request of its own.
        let own = |n: u32| format!("SET SESSION sql_mode = IF(@dbine_m{n} IS NULL OR FIND_IN_SET('ORACLE', @dbine_m{n}) > 0 OR FIND_IN_SET('MSSQL', @dbine_m{n}) > 0, NULL, @dbine_m{n}), SESSION collation_connection = @dbine_c{n};");
        let back = "SET SESSION sql_mode = @dbine_saved_m, SESSION collation_connection = @dbine_saved_c;";
        assert_eq!(st[11], own(1));
        assert_eq!(st[12], "CREATE DEFINER=`root`@`%` PROCEDURE `negocio`.`p_total`()\nBEGIN\n  SELECT COUNT(*) FROM negocio.pedidos;\n  SELECT 'tienda.pedidos';\nEND;");
        assert_eq!(st[13], back);
        assert_eq!(st[14], own(2));
        assert!(st[15].starts_with("CREATE DEFINER=`root`@`%` FUNCTION `negocio`.`f_doble`(x INT)"));
        assert_eq!(st[16], back);
        // Views under the session's mode; v_cli before v_top, which reads it.
        assert_eq!(st[17], "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `negocio`.`v_cli` AS select `negocio`.`clientes`.`id` AS `id` from `negocio`.`clientes`;");
        assert!(st[18].contains("VIEW `negocio`.`v_top` AS select `v_cli`.`id` AS `id` from `negocio`.`v_cli`;"));
        assert_eq!(st[19..22], [own(3), "CREATE DEFINER=`root`@`%` TRIGGER `negocio`.tr_pedidos BEFORE INSERT ON `negocio`.pedidos FOR EACH ROW SET NEW.total = f_doble(NEW.total);".into(), back.into()]);
        assert_eq!(st[22..25], [own(4), "CREATE DEFINER=`root`@`%` EVENT `negocio`.`ev_limpia` ON SCHEDULE EVERY 1 DAY DO DELETE FROM `negocio`.`pedidos` WHERE total < 0;".into(), back.into()]);
        assert_eq!(&st[25..29], ["DROP VIEW `tienda`.`v_top`;", "DROP VIEW `tienda`.`v_cli`;", "DROP PROCEDURE `tienda`.`p_total`;", "DROP FUNCTION `tienda`.`f_doble`;"]);
        assert_eq!(st[29], "DROP EVENT `tienda`.`ev_limpia`;");
        let last = st.last().unwrap();
        assert!(last.contains("information_schema.EVENTS WHERE EVENT_SCHEMA = 'tienda') = 0, 'DROP DATABASE `tienda`', 'SELECT * FROM `tienda`.`DBine: la base no quedó vacía y no se borra`')"), "{last}");
        assert_eq!(st.len(), 31);
        // No request holds a CREATE and anything else.
        for x in st.iter().filter(|x| x.contains("CREATE DEFINER") || x.contains("CREATE ALGORITHM")) {
            assert!(!x.contains("USE ") && !x.contains("SET SESSION"), "{x}");
        }
        // The dynamic SQL in p_total is left to the user.
        assert!(s.warnings.iter().any(|w| w.contains("p_total") && w.contains("línea 4") && w.contains("SQL dinámico")), "{:?}", s.warnings);
        assert!(s.warnings[0].contains("No es atómico"));
        assert!(s.warnings[1].contains("GRANT … ON `tienda`.*") && s.warnings[1].contains("`negocio`"));
        assert!(s.warnings[2].contains("otras bases"));
        assert!(s.warnings.iter().any(|w| w.contains("SET_USER_ID")));
        assert!(s.warnings.iter().any(|w| w.contains("quedó vacía")));
    }

    /// A body stored under NO_BACKSLASH_ESCAPES that ends early with
    /// backslash escapes (or the other way round) and runs what follows.
    #[test]
    fn bodies_that_split_are_refused() {
        let attack = r"CREATE DEFINER=`u`@`%` PROCEDURE `p`() BEGIN SELECT 'a\'; SELECT '; END; DROP DATABASE victima; SELECT '; END";
        let o = [obj("table", "t", None), obj("procedure", "p", Some(attack))];
        match script(Variant::MySql, "a", "b", &o) {
            Err(Error::Query(m)) => assert!(m.contains("«p»") && m.contains("una sola sentencia"), "{m}"),
            other => panic!("{other:?}"),
        }
        // The same the other way round: one statement only with escapes.
        let reverse = r"CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET @a = 'x\'; DROP DATABASE victima; -- '";
        assert!(matches!(script(Variant::MariaDb, "a", "b", &[obj("table", "x", None), obj("trigger", "t", Some(reverse))]), Err(Error::Query(_))));
        // Two statements outright.
        let two = "CREATE FUNCTION f() RETURNS INT RETURN 1; DROP DATABASE victima";
        assert!(matches!(script(Variant::MySql, "a", "b", &[obj("function", "f", Some(two))]), Err(Error::Query(_))));
    }

    /// `"` read as a string (and backslash escapes) hides a `;` that ends
    /// the statement when ANSI_QUOTES makes it an identifier quote, and the
    /// other way round: every combination of the two modes is checked.
    #[test]
    fn ansi_quotes_and_backslashes_mixed() {
        let d = crate::script_dialect(Variant::MySql);
        // ANSI_QUOTES + backslash escapes: `"a\"` is an identifier ending at
        // the second quote, so `; DROP …` is a statement of its own; with
        // `"` as a string, `\"` escapes and it's all one string.
        let ansi = r#"CREATE PROCEDURE p() SELECT "a\"; DROP DATABASE victima; SELECT ""#;
        assert_eq!(split_script(&lexed(ansi, true, false), &d).len(), 1);
        assert!(split_script(&lexed(ansi, true, true), &d).len() > 1);
        assert!(!one_statement(ansi, &d));
        // Without backslash escapes and without ANSI_QUOTES `"…"` is a plain
        // string; ANSI_QUOTES alone doesn't split it either.
        let mixed = r#"CREATE PROCEDURE p() SELECT 'x\' AS a, "y;z" AS b"#;
        for (bs, aq) in [(true, false), (true, true), (false, false), (false, true)] {
            assert_eq!(split_script(&lexed(mixed, bs, aq), &d).len(), 1, "{bs} {aq}");
        }
        assert!(one_statement(mixed, &d));
        let o = [obj("table", "t", None), obj("procedure", "p", Some(ansi))];
        assert!(matches!(script(Variant::MariaDb, "a", "b", &o), Err(Error::Query(m)) if m.contains("«p»")));
        // Executable comments run as code: their `;` counts.
        assert!(!one_statement("CREATE PROCEDURE p() SELECT 1 /*!50000 ; DROP DATABASE victima */", &d));
        assert!(one_statement("CREATE PROCEDURE p() SELECT 1 /* ; DROP DATABASE victima */ -- ;\n", &d));
    }

    /// Backslashes and quotes that read as one statement either way are fine.
    #[test]
    fn backslashes_in_one_statement() {
        let nbe = r"CREATE DEFINER=`root`@`%` PROCEDURE `p_nbe`()
SELECT 'C:\tmp\' AS ruta, 'it''s' AS cita";
        let s = script(Variant::MySql, "a", "b", &[obj("procedure", "p_nbe", Some(nbe))]).unwrap();
        assert!(s.statements.contains(&"CREATE DEFINER=`root`@`%` PROCEDURE `b`.`p_nbe`()\nSELECT 'C:\\tmp\\' AS ruta, 'it''s' AS cita;".to_string()), "{:?}", s.statements);
        assert!(one_statement("CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END IF; SELECT 'x;y'; END", &crate::script_dialect(Variant::MySql)));
    }

    #[test]
    fn tables_only_and_odd_names() {
        let s = script(Variant::MariaDb, "it's", "nuevo db", &[obj("table", "a`b", None)]).unwrap();
        assert!(s.statements[1].contains("'CREATE DATABASE `nuevo db` CHARACTER SET '") && s.statements[1].contains("SCHEMA_NAME = 'it''s'"), "{}", s.statements[1]);
        assert_eq!(s.statements[2], "RENAME TABLE `it's`.`a``b` TO `nuevo db`.`a``b`;");
        assert!(s.statements[3].contains("'DROP DATABASE `it''s`'"));
        assert_eq!(s.statements.len(), 4);
        assert!(!s.warnings.iter().any(|w| w.contains("SET_USER_ID")));
        // An empty database: checked, created and dropped.
        assert_eq!(script(Variant::MySql, "a", "b", &[]).unwrap().statements.len(), 3);
    }

    #[test]
    fn guard_comes_first() {
        let o = [obj("table", "t", None), obj("table", "u", None), obj("view", "v", Some("CREATE VIEW v AS SELECT 1"))];
        let s = script(Variant::MariaDb, "it's", "b", &o).unwrap();
        assert_eq!(
            s.statements[0],
            "SET @dbine_sql = IF(\
(SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'it''s' AND TABLE_TYPE <> 'VIEW') = 2 AND \
(SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'it''s' AND TABLE_TYPE = 'VIEW') = 1 AND \
(SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = 'it''s') = 0 AND \
(SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = 'it''s') = 0 AND \
(SELECT COUNT(*) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = 'it''s') = 0, \
'DO 0', 'SELECT * FROM `it''s`.`DBine: hay eventos, secuencias u objetos sin leer; nada cambió`');
PREPARE dbine_rename FROM @dbine_sql;
EXECUTE dbine_rename;
DEALLOCATE PREPARE dbine_rename;"
        );
        assert!(s.statements[1].contains("CREATE DATABASE"));
        assert!(s.warnings.iter().any(|w| w.contains("Esas bases no se renombran")), "{:?}", s.warnings);
        // MySQL takes names of 64 characters at most.
        assert!(NOT_READ.chars().count() <= 64 && NOT_EMPTY.chars().count() <= 64);
    }

    #[test]
    fn qualified_headers_stay() {
        let o = [obj("view", "v", Some("CREATE VIEW `old`.`v` AS SELECT 1")), obj("trigger", "t", Some("CREATE TRIGGER old.t AFTER UPDATE ON old.x FOR EACH ROW SET @a = 1"))];
        let s = script(Variant::MySql, "old", "new", &o).unwrap();
        assert!(s.statements.contains(&"CREATE VIEW `new`.`v` AS SELECT 1;".to_string()), "{:?}", s.statements);
        assert!(s.statements.contains(&"CREATE TRIGGER new.t AFTER UPDATE ON new.x FOR EACH ROW SET @a = 1;".to_string()), "{:?}", s.statements);
    }

    #[test]
    fn nothing_is_left_behind() {
        // Code without a definition, or a kind this doesn't move: refused.
        for o in [obj("procedure", "p", None), obj("view", "v", Some("  ")), obj("trigger", "tr", None), obj("event", "e", None)] {
            assert!(matches!(script(Variant::MySql, "a", "b", &[obj("table", "t", None), o.clone()]), Err(Error::Query(_))), "{o:?}");
        }
        assert!(matches!(script(Variant::MariaDb, "a", "b", &[obj("table", "t", None), obj("sequence", "s", None)]), Err(Error::Unsupported(_))));
    }

    #[test]
    fn refusals() {
        for db in ["mysql", "SYS", "information_schema", "performance_schema"] {
            assert!(matches!(script(Variant::MySql, db, "x", &[]), Err(Error::Unsupported(_))), "{db}");
            assert!(matches!(script(Variant::MySql, "x", db, &[]), Err(Error::Unsupported(_))), "{db}");
        }
        assert!(matches!(script(Variant::MySql, "a", "A", &[]), Err(Error::Query(_))));
        for v in [Variant::TiDb, Variant::OceanBase, Variant::SingleStore, Variant::StarRocks, Variant::Doris, Variant::VeloDb, Variant::Databend, Variant::GreptimeDb, Variant::Manticore] {
            assert!(matches!(script(v, "a", "b", &[]), Err(Error::Unsupported(_))), "{v:?}");
        }
    }
}
