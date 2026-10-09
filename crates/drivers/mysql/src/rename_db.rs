//! "Renombrar…" on a database. MySQL has no `RENAME DATABASE` (it existed
//! briefly in 5.1 and was removed), so the rename moves what the database
//! holds into a new one:
//!
//! 0. A guard: the old database holds exactly what the app read (tables,
//!    views, routines, triggers and events, counted in information_schema),
//!    or the first statement fails before anything changes. Events (the
//!    explorer doesn't list them), MariaDB sequences, what the login can't
//!    read and what changed since the app read it would be left half-moved.
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
//!    database as the default one (bare names resolve there). The
//!    `DEFINER` is kept, as in an object rename.
//! 5. The views, routines and events left in the old one are dropped, and
//!    the old database last, only if nothing is left in it (a prepared
//!    statement that otherwise fails on purpose).
//!
//! Every statement names its database, so it runs from any connection; the
//! code is created after a `USE` of the new one in the same request.
//! MySQL (Aurora, Cloud SQL) and MariaDB. The others don't offer it:
//! TiDB moves the tables, but their foreign keys keep naming the old
//! database (`REFERENCES old.t`, checked on 8.5), so they'd break when it's
//! dropped; SingleStore, StarRocks, Doris, Databend and GreptimeDB can't
//! move a table to another database with a rename; Manticore has one
//! namespace; OceanBase hasn't been checked.

use crate::rename::spec;
use crate::Variant;
use dbine_driver::rename::{names_in_code, rewrite_references, DatabaseObject, RewriteOptions, RewriteTarget, UnresolvedReason};
use dbine_driver::sql::{code_tokens, quote_ident, NameToken, Quote, TokenKind};
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
const LAST_DROP: &str = "La última sentencia borra la base vieja solo si quedó vacía; si apareció algo mientras corría el script, falla a propósito con «Table '…DBine: la base no quedó vacía y no se borra' doesn't exist» y la base queda para revisarla.";

/// The table the guards select from to fail: its name is the message
/// (at most 64 characters, MySQL's limit for a name).
const NOT_READ: &str = "DBine: hay eventos, secuencias u objetos sin leer; nada cambió";
const NOT_EMPTY: &str = "DBine: la base no quedó vacía y no se borra";

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

    // 1. The new database, with the old one's defaults.
    statements.push(prepared(&format!(
        "COALESCE((SELECT CONCAT({}, DEFAULT_CHARACTER_SET_NAME, ' COLLATE ', DEFAULT_COLLATION_NAME) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {}), {})",
        lit(&format!("CREATE DATABASE {new} CHARACTER SET ")),
        lit(database),
        lit(&format!("CREATE DATABASE {new}")),
    )));

    // 2. Triggers: RENAME TABLE won't move a table that has them.
    statements.extend(of(kinds::TRIGGER).map(|t| format!("DROP TRIGGER {old}.{};", q(&t.name))));

    // 3. The tables, all at once.
    let moves: Vec<String> = of(kinds::TABLE).map(|t| format!("{old}.{n} TO {new}.{n}", n = q(&t.name))).collect();
    if !moves.is_empty() {
        statements.push(format!("RENAME TABLE {};", moves.join(",\n  ")));
    }

    // 4. The code, in the new database.
    let recreate = |o: &DatabaseObject, statements: &mut Vec<String>, warnings: &mut Vec<String>| {
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
        // `USE` in the same request: SHOW CREATE VIEW leaves out the database
        // of the tables in the session's own (the app reads the definitions
        // from a session on the old database), and routine and trigger
        // bodies name tables bare.
        statements.push(format!("USE {new};\n{};", text.trim_end().trim_end_matches(';').trim_end()));
    };
    let routines: Vec<&DatabaseObject> = objects.iter().filter(|o| matches!(o.kind.as_str(), kinds::FUNCTION | kinds::PROCEDURE)).collect();
    for o in &routines {
        recreate(o, &mut statements, &mut warnings);
    }
    let views = view_order(of(kinds::VIEW).collect(), &dialect);
    for o in &views {
        recreate(o, &mut statements, &mut warnings);
    }
    for o in of(kinds::TRIGGER) {
        recreate(o, &mut statements, &mut warnings);
    }
    let events: Vec<&DatabaseObject> = of(EVENT).collect();
    for o in &events {
        recreate(o, &mut statements, &mut warnings);
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
        assert!(st[1].starts_with("SET @dbine_sql = COALESCE((SELECT CONCAT('CREATE DATABASE `negocio` CHARACTER SET ', DEFAULT_CHARACTER_SET_NAME"), "{}", st[1]);
        assert!(st[1].contains("WHERE SCHEMA_NAME = 'tienda'), 'CREATE DATABASE `negocio`');\nPREPARE dbine_rename FROM @dbine_sql;\nEXECUTE dbine_rename;"));
        assert_eq!(st[2], "DROP TRIGGER `tienda`.`tr_pedidos`;");
        assert_eq!(st[3], "RENAME TABLE `tienda`.`clientes` TO `negocio`.`clientes`,\n  `tienda`.`pedidos` TO `negocio`.`pedidos`;");
        // Routines first (views check the functions they call), qualified.
        assert_eq!(st[4], "USE `negocio`;\nCREATE DEFINER=`root`@`%` PROCEDURE `negocio`.`p_total`()\nBEGIN\n  SELECT COUNT(*) FROM negocio.pedidos;\n  SELECT 'tienda.pedidos';\nEND;");
        assert!(st[5].starts_with("USE `negocio`;\nCREATE DEFINER=`root`@`%` FUNCTION `negocio`.`f_doble`(x INT)"));
        // v_cli before v_top, which reads it.
        assert_eq!(st[6], "USE `negocio`;\nCREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `negocio`.`v_cli` AS select `negocio`.`clientes`.`id` AS `id` from `negocio`.`clientes`;");
        assert!(st[7].contains("VIEW `negocio`.`v_top` AS select `v_cli`.`id` AS `id` from `negocio`.`v_cli`;"));
        assert_eq!(st[8], "USE `negocio`;\nCREATE DEFINER=`root`@`%` TRIGGER `negocio`.tr_pedidos BEFORE INSERT ON `negocio`.pedidos FOR EACH ROW SET NEW.total = f_doble(NEW.total);");
        assert_eq!(st[9], "USE `negocio`;\nCREATE DEFINER=`root`@`%` EVENT `negocio`.`ev_limpia` ON SCHEDULE EVERY 1 DAY DO DELETE FROM `negocio`.`pedidos` WHERE total < 0;");
        assert_eq!(&st[10..14], ["DROP VIEW `tienda`.`v_top`;", "DROP VIEW `tienda`.`v_cli`;", "DROP PROCEDURE `tienda`.`p_total`;", "DROP FUNCTION `tienda`.`f_doble`;"]);
        assert_eq!(st[14], "DROP EVENT `tienda`.`ev_limpia`;");
        let last = st.last().unwrap();
        assert!(last.contains("information_schema.EVENTS WHERE EVENT_SCHEMA = 'tienda') = 0, 'DROP DATABASE `tienda`', 'SELECT * FROM `tienda`.`DBine: la base no quedó vacía y no se borra`')"), "{last}");
        assert_eq!(st.len(), 16);
        // The dynamic SQL in p_total is left to the user.
        assert!(s.warnings.iter().any(|w| w.contains("p_total") && w.contains("línea 4") && w.contains("SQL dinámico")), "{:?}", s.warnings);
        assert!(s.warnings[0].contains("No es atómico"));
        assert!(s.warnings[1].contains("GRANT … ON `tienda`.*") && s.warnings[1].contains("`negocio`"));
        assert!(s.warnings[2].contains("otras bases"));
        assert!(s.warnings.iter().any(|w| w.contains("SET_USER_ID")));
        assert!(s.warnings.iter().any(|w| w.contains("quedó vacía")));
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
        assert!(s.statements.contains(&"USE `new`;\nCREATE VIEW `new`.`v` AS SELECT 1;".to_string()), "{:?}", s.statements);
        assert!(s.statements.contains(&"USE `new`;\nCREATE TRIGGER new.t AFTER UPDATE ON new.x FOR EACH ROW SET @a = 1;".to_string()), "{:?}", s.statements);
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
