//! "Renombrar…": the `ALTER … RENAME` statements of each variant.
//!
//! PostgreSQL and its distributions keep dependents by OID: views,
//! materialized views, triggers, foreign keys, indexes and `BEGIN ATOMIC`
//! functions follow a rename by themselves (`tracked`). Functions and
//! procedures with a text body (plpgsql, sql) are rewritten by the app and
//! put back with `CREATE OR REPLACE`, all in one transaction.
//!
//! CockroachDB refuses to rename what a view, function or trigger uses:
//! those are dropped before the rename and created after it. Its DDL
//! commits as it goes (`autocommit_before_ddl`), so that isn't atomic. The other engines rename less (see [`spec`]).
//!
//! Databases ([`database_script`]) are renamed with `ALTER DATABASE …
//! RENAME TO`, run from another database ([`database_rename`]):
//! PostgreSQL and the engines that keep its rule (no other session may be
//! connected) end those sessions first with `pg_terminate_backend`;
//! CockroachDB and RisingWave rename with them open. Materialize has no
//! rename for databases, CrateDB and H2 have no databases to rename, and
//! Denodo renames nothing.

use crate::{Variant, SINK, SOURCE};
use dbine_driver::rename::{quote_new, Fold, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::sql::{name_tokens, qualified_name, quote_ident, NameToken, Quote, TokenKind};
use dbine_driver::{kinds, Error, ObjectRef, Result, SyncScript};

/// How much of PostgreSQL's renaming an engine has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    /// PostgreSQL and the distributions that keep its catalog and DDL.
    Full,
    /// CockroachDB: PostgreSQL's statements, but dependents block.
    Cockroach,
    /// Tables, views, columns and schemas (Redshift, Yellowbrick).
    Basic,
    /// Tables, views, materialized views, sources and sinks (Materialize,
    /// RisingWave), which the engine follows by itself.
    Streaming,
    /// Tables, views and columns (CrateDB).
    Crate,
    /// Tables, views, columns, indexes, constraints and schemas (H2).
    H2,
}

fn level(v: Variant) -> Option<Level> {
    Some(match v {
        Variant::Denodo => return None,
        Variant::H2 => Level::H2,
        Variant::Cockroach => Level::Cockroach,
        Variant::Redshift | Variant::Yellowbrick => Level::Basic,
        Variant::Materialize | Variant::RisingWave => Level::Streaming,
        Variant::CrateDb => Level::Crate,
        _ => Level::Full,
    })
}

pub(crate) fn spec(v: Variant) -> Option<RenameSpec> {
    let level = level(v)?;
    let wanted: &[&str] = match level {
        Level::Full | Level::Cockroach => &[
            kinds::TABLE,
            kinds::VIEW,
            kinds::MATERIALIZED_VIEW,
            kinds::SEQUENCE,
            kinds::TYPE,
            kinds::FUNCTION,
            kinds::PROCEDURE,
            kinds::TRIGGER,
        ],
        Level::Basic | Level::Crate | Level::H2 => &[kinds::TABLE, kinds::VIEW],
        Level::Streaming => &[kinds::TABLE, kinds::VIEW, kinds::MATERIALIZED_VIEW, SOURCE, SINK],
    };
    // Only what the variant lists (Yellowbrick has no materialized views…).
    let listed = v.info().object_kinds;
    let kinds: Vec<String> = wanted
        .iter()
        .filter(|k| listed.iter().any(|o| o.id == **k))
        .map(|k| k.to_string())
        .collect();
    let full = matches!(level, Level::Full | Level::Cockroach);
    let tracked: Vec<String> = match level {
        Level::Full => vec![kinds::VIEW.into(), kinds::MATERIALIZED_VIEW.into(), kinds::TRIGGER.into()],
        Level::Streaming => vec![kinds::VIEW.into(), kinds::MATERIALIZED_VIEW.into(), SINK.into()],
        _ => Vec::new(),
    };
    let replace = match level {
        Level::Full | Level::Basic | Level::Crate | Level::H2 => ReplaceStyle::CreateOrReplace,
        Level::Cockroach | Level::Streaming => ReplaceStyle::DropCreate,
    };
    let note = match (v, level) {
        (_, Level::Cockroach) => Some(
            "CockroachDB no renombra lo que usan vistas, funciones o triggers: DBine borra esas dependencias antes del cambio y las vuelve a crear después. Su DDL se confirma sentencia por sentencia: si una falla, las anteriores quedan aplicadas."
                .to_string(),
        ),
        (Variant::Yugabyte, _) => {
            Some("En YugabyteDB el DDL no es transaccional: si una sentencia falla, las anteriores quedan aplicadas.".to_string())
        }
        (_, Level::Streaming) => Some(
            "El motor actualiza por sí solo las vistas, vistas materializadas y sinks que dependen del objeto. No renombra columnas.".to_string(),
        ),
        (_, Level::Crate) => Some("CrateDB renombra columnas desde la versión 5.5.".to_string()),
        _ => None,
    };
    Some(RenameSpec {
        kinds,
        columns: full || matches!(level, Level::Basic | Level::Crate | Level::H2),
        indexes: full || matches!(level, Level::Streaming | Level::H2),
        constraints: full || level == Level::H2,
        schemas: full || matches!(level, Level::Basic | Level::Streaming | Level::H2),
        tracked,
        replace,
        references: dbine_driver::ReferenceStyle::Sql,
        fold: Fold::Lower,
        // YugabyteDB runs each DDL statement on its own, and CockroachDB
        // commits the open transaction before each one
        // (`autocommit_before_ddl`, on by default since 25.1).
        transactional: level == Level::Full && v != Variant::Yugabyte && v.manual_transactions(),
        note,
        databases: database_rename(v).is_some(),
        database_from: database_rename(v).map(|r| r.from.to_string()),
        database_note: database_rename(v).map(|r| database_note(v, r)),
        database_moves: false,
        ..Default::default()
    })
}

/// What an engine does with the other sessions of a database it renames.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sessions {
    /// It refuses while there are any: they're ended first with
    /// `pg_terminate_backend`, reading the backends from `pg_stat_activity`
    /// (the column is the backend's id: `pid`, Redshift's `procpid`).
    Terminate(&'static str),
    /// It renames with them open (CockroachDB, RisingWave).
    Kept,
    /// It may refuse while there are any, and there's no SQL here to end
    /// them (Yellowbrick): the user closes them.
    Closed,
}

#[derive(Clone, Copy, Debug)]
struct DatabaseRename {
    /// The database the script runs in.
    from: &'static str,
    sessions: Sessions,
}

/// How a variant renames a database; `None`: it doesn't.
fn database_rename(v: Variant) -> Option<DatabaseRename> {
    let (from, sessions) = match v {
        // Its database is a namespace; ALTER DATABASE only changes the owner.
        Variant::Materialize => return None,
        // One database per connection (CrateDB's are schemas, H2's a file),
        // and Denodo renames nothing.
        Variant::CrateDb | Variant::H2 | Variant::Denodo => return None,
        // `system` takes CONNECT from admins only; `defaultdb` from everyone.
        Variant::Cockroach => ("defaultdb", Sessions::Kept),
        Variant::RisingWave => ("dev", Sessions::Kept),
        Variant::Redshift => ("dev", Sessions::Terminate("procpid")),
        Variant::Yellowbrick => ("yellowbrick", Sessions::Closed),
        Variant::Yugabyte => ("yugabyte", Sessions::Terminate("pid")),
        Variant::Kingbase => ("kingbase", Sessions::Terminate("pid")),
        // PostgreSQL and the engines that keep its initdb (EDB creates
        // `postgres` next to `edb`).
        _ => ("postgres", Sessions::Terminate("pid")),
    };
    Some(DatabaseRename { from, sessions })
}

/// Databases a variant never renames from DBine, besides the one the
/// script runs in.
fn kept_databases(v: Variant) -> &'static [&'static str] {
    match v {
        Variant::Cockroach => &["system"],
        Variant::Redshift => &["padb_harvest", "template0", "template1"],
        Variant::RisingWave => &[],
        _ => &["template0", "template1"],
    }
}

fn database_note(v: Variant, r: DatabaseRename) -> String {
    let engine = v.info().name;
    let elsewhere = "El código, las aplicaciones y las cadenas de conexión que la nombran en otro lado no se actualizan.";
    let mut note = match r.sessions {
        Sessions::Terminate(_) => format!(
            "{engine} no renombra una base con otras sesiones conectadas: DBine las cierra antes (pg_terminate_backend), así que se cortan sus transacciones en curso. {elsewhere} No es atómico: si el cambio de nombre falla, las sesiones ya quedaron cerradas."
        ),
        Sessions::Kept if v == Variant::Cockroach => format!(
            "CockroachDB renombra la base con las sesiones abiertas y no las cierra, pero las que la tenían como base actual quedan apuntando a un nombre que ya no existe: los nombres sin calificar les fallan hasta que se reconecten. {elsewhere}"
        ),
        Sessions::Kept => format!(
            "{engine} renombra la base con las sesiones abiertas, pero las que estaban conectadas a ella dejan de funcionar y tienen que reconectarse con el nombre nuevo. {elsewhere}"
        ),
        Sessions::Closed => format!(
            "Cerrá antes las demás sesiones conectadas a la base: con alguna abierta, {engine} puede rechazar el cambio. {elsewhere}"
        ),
    };
    note.push(' ');
    note.push_str(match v {
        Variant::Cockroach => "Hace falta ser admin, o el dueño de la base con CREATEDB.",
        Variant::Redshift => "Hace falta ser superusuario, o el dueño de la base con CREATEDB.",
        _ if matches!(r.sessions, Sessions::Terminate(_)) => {
            "Hace falta ser el dueño de la base (con CREATEDB) o superusuario; para cerrar sesiones de otros usuarios, superusuario o pg_signal_backend."
        }
        _ => "Hace falta ser el dueño de la base o superusuario.",
    });
    if v == Variant::Yugabyte {
        note.push_str(" En un clúster de varios nodos, pg_terminate_backend solo alcanza las sesiones del nodo al que está conectado DBine.");
    }
    note.push_str(&format!(" No se renombra «{}»: desde ahí se ejecuta el cambio.", r.from));
    note
}

/// The statements that rename `database` to `new_name`, run in
/// [`DatabaseRename::from`].
pub(crate) fn database_script(v: Variant, database: &str, new_name: &str) -> Result<SyncScript> {
    let engine = v.info().name;
    let Some(r) = database_rename(v) else {
        return Err(Error::Unsupported(match v {
            Variant::Materialize => "Materialize no renombra bases de datos: ALTER DATABASE solo cambia el dueño".into(),
            _ => format!("{engine} no renombra bases de datos desde DBine"),
        }));
    };
    if database.is_empty() || new_name.is_empty() {
        return Err(Error::Unsupported("Falta el nombre de la base.".into()));
    }
    if database == r.from {
        return Err(Error::Unsupported(format!(
            "«{database}» no se renombra desde DBine: es la base desde la que {engine} ejecuta el cambio de nombre"
        )));
    }
    if kept_databases(v).contains(&database) {
        return Err(Error::Unsupported(format!("«{database}» es una base del sistema de {engine}: no se renombra")));
    }
    let mut statements = Vec::new();
    let mut warnings = Vec::new();
    match r.sessions {
        Sessions::Terminate(pid) => {
            statements.push(format!(
                "SELECT pg_terminate_backend({pid}) FROM pg_stat_activity WHERE datname = {} AND {pid} <> pg_backend_pid();",
                crate::catalog::lit(v, database)
            ));
            warnings.push(format!("Se cierran las demás sesiones conectadas a «{database}»."));
        }
        Sessions::Kept if v == Variant::Cockroach => {
            warnings.push(format!("Las sesiones conectadas a «{database}» siguen abiertas, pero su base actual deja de existir."))
        }
        Sessions::Kept => warnings.push(format!("Las sesiones conectadas a «{database}» dejan de funcionar: tienen que reconectarse.")),
        Sessions::Closed => warnings.push(format!("Si quedan sesiones conectadas a «{database}», {engine} puede rechazar el cambio.")),
    }
    statements.push(format!("ALTER DATABASE {} RENAME TO {};", q(database), q(new_name)));
    warnings.push(format!("Lo que nombra «{database}» (código, aplicaciones, cadenas de conexión) no se actualiza."));
    Ok(SyncScript { statements, warnings })
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn qn(schema: Option<&str>, name: &str) -> String {
    qualified_name(Quote::Double, schema, name)
}

pub(crate) fn script(v: Variant, req: &RenameRequest) -> Result<SyncScript> {
    let engine = v.info().name;
    let Some(spec) = spec(v) else {
        return Err(Error::Unsupported(match v {
            Variant::Denodo => "Denodo no renombra objetos por SQL: las vistas se definen en Denodo".into(),
            _ => format!("{engine} no renombra objetos desde DBine"),
        }));
    };
    let level = level(v).expect("a spec has a level");
    if !spec.allows(&req.target) {
        let what = match &req.target {
            RenameTarget::Object { object, .. } => format!("objetos de tipo «{}»", object.kind),
            RenameTarget::Column { .. } => "columnas".into(),
            RenameTarget::Index { .. } => "índices".into(),
            RenameTarget::Constraint { .. } => "restricciones".into(),
            RenameTarget::Schema { .. } => "esquemas".into(),
        };
        return Err(Error::Unsupported(format!("{engine} no renombra {what} desde DBine")));
    }
    let new = quote_new(&req.new_name, &crate::script::DIALECT, spec.fold, false);
    let mut warnings = Vec::new();
    let statements = match &req.target {
        RenameTarget::Object { object, parent } => object_statements(v, level, object, parent.as_deref(), req.definition.as_deref(), &new, &mut warnings)?,
        RenameTarget::Column { table, column } => {
            let alter = if table.kind == kinds::MATERIALIZED_VIEW { "ALTER MATERIALIZED VIEW" } else { "ALTER TABLE" };
            vec![format!("{alter} {} RENAME COLUMN {} TO {new};", qn(table.schema(), &table.name), q(column))]
        }
        RenameTarget::Index { table, index } => vec![match level {
            Level::Cockroach => format!("ALTER INDEX {}@{} RENAME TO {new};", qn(table.schema(), &table.name), q(index)),
            _ => format!("ALTER INDEX {} RENAME TO {new};", qn(table.schema(), index)),
        }],
        RenameTarget::Constraint { table, constraint } => {
            vec![format!("ALTER TABLE {} RENAME CONSTRAINT {} TO {new};", qn(table.schema(), &table.name), q(constraint))]
        }
        RenameTarget::Schema { schema, .. } => {
            warnings.push(format!("Los search_path (de usuarios, bases o funciones) que nombren «{schema}» no se actualizan."));
            vec![format!("ALTER SCHEMA {} RENAME TO {new};", q(schema))]
        }
    };
    Ok(SyncScript { statements, warnings })
}

fn object_statements(
    v: Variant,
    level: Level,
    object: &ObjectRef,
    parent: Option<&str>,
    definition: Option<&str>,
    new: &str,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>> {
    let name = qn(object.schema(), &object.name);
    let alter = match object.kind.as_str() {
        kinds::TABLE => {
            if matches!(level, Level::Full | Level::Cockroach | Level::Basic) {
                warnings.push("Los índices, restricciones y secuencias de la tabla conservan sus nombres (por ejemplo, «…_pkey»).".into());
            }
            "TABLE"
        }
        // Redshift and CrateDB rename views with ALTER TABLE.
        kinds::VIEW if matches!(v, Variant::Redshift | Variant::CrateDb) => "TABLE",
        kinds::VIEW => "VIEW",
        kinds::MATERIALIZED_VIEW => "MATERIALIZED VIEW",
        kinds::SEQUENCE => "SEQUENCE",
        // A domain is listed with the types, and only ALTER DOMAIN renames it.
        kinds::TYPE if definition.is_some_and(is_domain) => "DOMAIN",
        kinds::TYPE => "TYPE",
        SOURCE => "SOURCE",
        SINK => "SINK",
        kinds::TRIGGER => {
            let table = parent.filter(|p| !p.is_empty()).ok_or_else(|| Error::Unsupported("falta la tabla del trigger para renombrarlo".into()))?;
            return Ok(vec![format!("ALTER TRIGGER {} ON {} RENAME TO {new};", q(&object.name), qn(object.schema(), table))]);
        }
        kinds::FUNCTION | kinds::PROCEDURE => {
            let word = if object.kind == kinds::PROCEDURE { "PROCEDURE" } else { "FUNCTION" };
            let sigs = definition.map(|d| signatures(d, &object.name)).unwrap_or_default();
            if sigs.is_empty() {
                warnings.push(format!("No se leyó la firma de «{}»: si tiene sobrecargas, el motor pide indicar sus argumentos.", object.name));
                return Ok(vec![format!("ALTER {word} {name} RENAME TO {new};")]);
            }
            if sigs.len() > 1 {
                warnings.push(format!("«{}» tiene {} sobrecargas: se renombran todas.", object.name, sigs.len()));
            }
            return Ok(sigs.into_iter().map(|(kind, args)| format!("ALTER {kind} {name}({args}) RENAME TO {new};")).collect());
        }
        other => return Err(Error::Unsupported(format!("{} no renombra objetos de tipo «{other}» desde DBine", v.info().name))),
    };
    Ok(vec![format!("ALTER {alter} {name} RENAME TO {new};")])
}

/// `CREATE DOMAIN …`: the type's definition is a domain's.
fn is_domain(definition: &str) -> bool {
    let toks = name_tokens(definition, &crate::script::DIALECT);
    toks.first().is_some_and(|t| word(t, "create")) && toks.get(1).is_some_and(|t| word(t, "domain"))
}

/// An unquoted word (a quoted name's text has no quotes, so it's shorter
/// than what it spans).
fn word(t: &NameToken<'_>, w: &str) -> bool {
    t.kind == TokenKind::Name && t.end - t.start == t.text.len() && t.text.eq_ignore_ascii_case(w)
}

/// Each overload's identity in a routine's definition
/// ([`dbine_driver::rename::routine_signatures`]).
pub(crate) fn signatures(definition: &str, name: &str) -> Vec<(&'static str, String)> {
    dbine_driver::rename::routine_signatures(definition, name)
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

    fn stmts(v: Variant, r: &RenameRequest) -> Vec<String> {
        script(v, r).unwrap_or_else(|e| panic!("{v:?}: {e}")).statements
    }

    #[test]
    fn tables_keep_the_new_names_case() {
        let pg = Variant::Postgres;
        assert_eq!(stmts(pg, &object(kinds::TABLE, "Clientes", "nuevos")), ["ALTER TABLE \"app\".\"Clientes\" RENAME TO nuevos;"]);
        // A name that isn't lower case, or needs quotes, is quoted.
        assert_eq!(stmts(pg, &object(kinds::TABLE, "t", "Nuevos")), ["ALTER TABLE \"app\".\"t\" RENAME TO \"Nuevos\";"]);
        assert_eq!(stmts(pg, &object(kinds::TABLE, "t", "mi tabla")), ["ALTER TABLE \"app\".\"t\" RENAME TO \"mi tabla\";"]);
        assert_eq!(stmts(pg, &object(kinds::TABLE, "t", "select")), ["ALTER TABLE \"app\".\"t\" RENAME TO \"select\";"]);
        assert_eq!(stmts(pg, &object(kinds::TABLE, "t", "a\"b")), ["ALTER TABLE \"app\".\"t\" RENAME TO \"a\"\"b\";"]);
        assert!(script(pg, &object(kinds::TABLE, "t", "n")).unwrap().warnings[0].contains("conservan sus nombres"));
    }

    #[test]
    fn views_sequences_types_and_domains() {
        let pg = Variant::Postgres;
        assert_eq!(stmts(pg, &object(kinds::VIEW, "v", "w")), ["ALTER VIEW \"app\".\"v\" RENAME TO w;"]);
        assert_eq!(stmts(pg, &object(kinds::MATERIALIZED_VIEW, "mv", "mw")), ["ALTER MATERIALIZED VIEW \"app\".\"mv\" RENAME TO mw;"]);
        assert_eq!(stmts(pg, &object(kinds::SEQUENCE, "s", "s2")), ["ALTER SEQUENCE \"app\".\"s\" RENAME TO s2;"]);
        assert_eq!(stmts(pg, &object(kinds::TYPE, "mood", "humor")), ["ALTER TYPE \"app\".\"mood\" RENAME TO humor;"]);
        let domain = req(RenameTarget::Object { object: obj(kinds::TYPE, "app", "pos"), parent: None }, "positivo", Some("CREATE DOMAIN \"app\".\"pos\" AS integer"));
        assert_eq!(stmts(pg, &domain), ["ALTER DOMAIN \"app\".\"pos\" RENAME TO positivo;"]);
        // Redshift renames views with ALTER TABLE.
        assert_eq!(stmts(Variant::Redshift, &object(kinds::VIEW, "v", "w")), ["ALTER TABLE \"app\".\"v\" RENAME TO w;"]);
    }

    #[test]
    fn functions_one_statement_per_overload() {
        let def = "CREATE OR REPLACE FUNCTION app.f(a integer, b text DEFAULT 'x,y'::text, OUT c numeric(10,2))\n RETURNS numeric\n LANGUAGE plpgsql\nAS $function$\nBEGIN\n  EXECUTE 'CREATE FUNCTION app.f(z int) RETURNS int AS $$ select 1 $$';\nEND $function$\n\n\n\
                   CREATE OR REPLACE FUNCTION app.f(VARIADIC xs integer[] = ARRAY[1, 2])\n RETURNS integer\n LANGUAGE sql\nAS $function$ select 1 $function$\n\n\n\
                   CREATE OR REPLACE FUNCTION app.f()\n RETURNS integer\n LANGUAGE sql\nBEGIN ATOMIC\n SELECT 1;\nEND\n";
        let r = req(RenameTarget::Object { object: obj(kinds::FUNCTION, "app", "f"), parent: None }, "G", Some(def));
        let s = script(Variant::Postgres, &r).unwrap();
        assert_eq!(
            s.statements,
            [
                "ALTER FUNCTION \"app\".\"f\"(a integer, b text, OUT c numeric(10,2)) RENAME TO \"G\";",
                "ALTER FUNCTION \"app\".\"f\"(VARIADIC xs integer[]) RENAME TO \"G\";",
                "ALTER FUNCTION \"app\".\"f\"() RENAME TO \"G\";",
            ]
        );
        assert!(s.warnings[0].contains("3 sobrecargas"), "{:?}", s.warnings);
        // A procedure, with a quoted mixed-case name.
        let def = "CREATE OR REPLACE PROCEDURE \"App\".\"Carga\"(IN n integer)\n LANGUAGE plpgsql\nAS $procedure$ BEGIN NULL; END $procedure$\n";
        let r = req(RenameTarget::Object { object: obj(kinds::PROCEDURE, "App", "Carga"), parent: None }, "carga2", Some(def));
        assert_eq!(stmts(Variant::Postgres, &r), ["ALTER PROCEDURE \"App\".\"Carga\"(IN n integer) RENAME TO carga2;"]);
        // No definition: the name alone (fine without overloads), with a warning.
        let r = req(RenameTarget::Object { object: obj(kinds::FUNCTION, "app", "f"), parent: None }, "g", None);
        let s = script(Variant::Postgres, &r).unwrap();
        assert_eq!(s.statements, ["ALTER FUNCTION \"app\".\"f\" RENAME TO g;"]);
        assert_eq!(s.warnings.len(), 1);
        // CockroachDB's pg_get_functiondef.
        let def = "CREATE FUNCTION s.ff(\n\ta INT8,\n\tb STRING DEFAULT 'x,y':::STRING\n)\n\tRETURNS INT8\n\tLANGUAGE SQL\n\tAS $$\n\t\tSELECT a;\n\t$$";
        let r = req(RenameTarget::Object { object: obj(kinds::FUNCTION, "s", "ff"), parent: None }, "ff2", Some(def));
        assert_eq!(stmts(Variant::Cockroach, &r), ["ALTER FUNCTION \"s\".\"ff\"(a INT8, b STRING) RENAME TO ff2;"]);
    }

    #[test]
    fn triggers_need_their_table() {
        let r = req(RenameTarget::Object { object: obj(kinds::TRIGGER, "app", "tg"), parent: Some("Pedidos".into()) }, "tg_nuevo", None);
        assert_eq!(stmts(Variant::Postgres, &r), ["ALTER TRIGGER \"tg\" ON \"app\".\"Pedidos\" RENAME TO tg_nuevo;"]);
        let r = req(RenameTarget::Object { object: obj(kinds::TRIGGER, "app", "tg"), parent: None }, "x", None);
        assert!(matches!(script(Variant::Postgres, &r), Err(Error::Unsupported(_))));
    }

    #[test]
    fn columns_indexes_constraints_and_schemas() {
        let t = obj(kinds::TABLE, "app", "T");
        let col = req(RenameTarget::Column { table: t.clone(), column: "Pepe".into() }, "pepe_nuevo", None);
        assert_eq!(stmts(Variant::Postgres, &col), ["ALTER TABLE \"app\".\"T\" RENAME COLUMN \"Pepe\" TO pepe_nuevo;"]);
        let mv = req(RenameTarget::Column { table: obj(kinds::MATERIALIZED_VIEW, "app", "mv"), column: "a".into() }, "b", None);
        assert_eq!(stmts(Variant::Postgres, &mv), ["ALTER MATERIALIZED VIEW \"app\".\"mv\" RENAME COLUMN \"a\" TO b;"]);
        let ix = req(RenameTarget::Index { table: t.clone(), index: "ix_pepe".into() }, "IX_Nuevo", None);
        assert_eq!(stmts(Variant::Postgres, &ix), ["ALTER INDEX \"app\".\"ix_pepe\" RENAME TO \"IX_Nuevo\";"]);
        assert_eq!(stmts(Variant::Cockroach, &ix), ["ALTER INDEX \"app\".\"T\"@\"ix_pepe\" RENAME TO \"IX_Nuevo\";"]);
        let ck = req(RenameTarget::Constraint { table: t, constraint: "ck_pepe".into() }, "ck_nuevo", None);
        assert_eq!(stmts(Variant::Postgres, &ck), ["ALTER TABLE \"app\".\"T\" RENAME CONSTRAINT \"ck_pepe\" TO ck_nuevo;"]);
        let sc = req(RenameTarget::Schema { database: None, schema: "Ventas".into() }, "comercial", None);
        let s = script(Variant::Postgres, &sc).unwrap();
        assert_eq!(s.statements, ["ALTER SCHEMA \"Ventas\" RENAME TO comercial;"]);
        assert!(s.warnings[0].contains("search_path"));
    }

    #[test]
    fn what_each_variant_renames() {
        for v in Variant::ALL {
            let Some(s) = spec(v) else {
                assert_eq!(v, Variant::Denodo);
                assert!(matches!(script(v, &object(kinds::TABLE, "t", "u")), Err(Error::Unsupported(_))));
                continue;
            };
            assert_eq!(s.fold, Fold::Lower);
            assert!(s.kinds.contains(&kinds::TABLE.to_string()), "{v:?}");
            // Every kind it offers scripts.
            for k in &s.kinds {
                let r = req(RenameTarget::Object { object: obj(k, "app", "x"), parent: Some("t".into()) }, "y", None);
                assert!(script(v, &r).is_ok(), "{v:?} {k}");
            }
            // Transactions only where the session has them.
            assert!(!s.transactional || v.manual_transactions(), "{v:?}");
        }
        let pg = spec(Variant::Postgres).unwrap();
        assert_eq!(pg.replace, ReplaceStyle::CreateOrReplace);
        assert!(pg.transactional && pg.columns && pg.indexes && pg.constraints && pg.schemas);
        assert_eq!(pg.tracked, ["view", "materialized_view", "trigger"]);
        assert_eq!(pg.kinds, ["table", "view", "materialized_view", "sequence", "type", "function", "procedure", "trigger"]);
        let crdb = spec(Variant::Cockroach).unwrap();
        assert_eq!(crdb.replace, ReplaceStyle::DropCreate);
        assert!(crdb.tracked.is_empty() && !crdb.transactional && crdb.note.is_some());
        assert!(!spec(Variant::Yugabyte).unwrap().transactional);
        let rs = spec(Variant::Redshift).unwrap();
        assert_eq!(rs.kinds, ["table", "view"]);
        assert!(rs.columns && rs.schemas && !rs.indexes && !rs.constraints && !rs.transactional && rs.tracked.is_empty());
        let ix = req(RenameTarget::Index { table: obj(kinds::TABLE, "app", "t"), index: "i".into() }, "j", None);
        assert!(matches!(script(Variant::Redshift, &ix), Err(Error::Unsupported(_))));
        assert!(matches!(script(Variant::Redshift, &object(kinds::FUNCTION, "f", "g")), Err(Error::Unsupported(_))));
        let mz = spec(Variant::Materialize).unwrap();
        assert_eq!(mz.kinds, ["table", "view", "materialized_view", "source", "sink"]);
        assert!(!mz.columns && mz.schemas && mz.indexes && !mz.constraints && !mz.transactional);
        assert_eq!(mz.tracked, ["view", "materialized_view", "sink"]);
        assert_eq!(stmts(Variant::RisingWave, &object(SINK, "k", "k2")), ["ALTER SINK \"app\".\"k\" RENAME TO k2;"]);
        let cr = spec(Variant::CrateDb).unwrap();
        assert_eq!(cr.kinds, ["table", "view"]);
        assert!(cr.columns && !cr.schemas && !cr.indexes && cr.tracked.is_empty());
        assert_eq!(stmts(Variant::CrateDb, &object(kinds::VIEW, "v", "w")), ["ALTER TABLE \"app\".\"v\" RENAME TO w;"]);
        let h2 = spec(Variant::H2).unwrap();
        assert_eq!(h2.kinds, ["table", "view"]);
        assert!(h2.columns && h2.indexes && h2.constraints && h2.schemas && !h2.transactional && h2.tracked.is_empty());
        assert_eq!(spec(Variant::Yellowbrick).unwrap().kinds, ["table", "view"]);
    }

    fn db(v: Variant, database: &str, new: &str) -> SyncScript {
        database_script(v, database, new).unwrap_or_else(|e| panic!("{v:?}: {e}"))
    }

    #[test]
    fn postgres_database_rename_ends_sessions_then_renames() {
        for v in [Variant::Postgres, Variant::Timescale, Variant::AlloyDb, Variant::CloudSql, Variant::Aurora, Variant::Edb, Variant::Fujitsu, Variant::OpenGauss, Variant::Greenplum, Variant::Cloudberry, Variant::Greengage] {
            let s = spec(v).unwrap();
            assert!(s.databases && !s.database_moves, "{v:?}");
            assert_eq!(s.database_from.as_deref(), Some("postgres"), "{v:?}");
            let note = s.database_note.unwrap();
            for part in ["cierra", "no se actualizan", "No es atómico", "dueño", "superusuario"] {
                assert!(note.contains(part), "{v:?}: {note}");
            }
            assert_eq!(
                db(v, "Ventas", "ventas 2024").statements,
                [
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = 'Ventas' AND pid <> pg_backend_pid();",
                    "ALTER DATABASE \"Ventas\" RENAME TO \"ventas 2024\";",
                ],
                "{v:?}"
            );
        }
        // Quotes in names are escaped, in the literal and in the identifiers.
        assert_eq!(
            db(Variant::Postgres, "o'brien\"x", "n\"y").statements,
            [
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = 'o''brien\"x' AND pid <> pg_backend_pid();",
                "ALTER DATABASE \"o'brien\"\"x\" RENAME TO \"n\"\"y\";",
            ]
        );
        let w = db(Variant::Postgres, "a", "b").warnings;
        assert!(w.iter().any(|w| w.contains("Se cierran")) && w.iter().any(|w| w.contains("no se actualiza")), "{w:?}");
        assert_eq!(spec(Variant::Kingbase).unwrap().database_from.as_deref(), Some("kingbase"));
    }

    #[test]
    fn yugabyte_and_redshift_database_rename() {
        let yb = spec(Variant::Yugabyte).unwrap();
        assert!(yb.databases && yb.database_note.as_deref().unwrap().contains("varios nodos"));
        assert_eq!(yb.database_from.as_deref(), Some("yugabyte"));
        assert_eq!(db(Variant::Yugabyte, "app", "app2").statements.len(), 2);
        // Yugabyte's own `postgres` database can be renamed; it doesn't run from it.
        assert!(database_script(Variant::Yugabyte, "postgres", "pg").is_ok());

        let rs = spec(Variant::Redshift).unwrap();
        assert!(rs.databases && !rs.database_moves);
        assert_eq!(rs.database_from.as_deref(), Some("dev"));
        assert_eq!(
            db(Variant::Redshift, "ventas\\x", "ventas2").statements,
            [
                "SELECT pg_terminate_backend(procpid) FROM pg_stat_activity WHERE datname = 'ventas\\\\x' AND procpid <> pg_backend_pid();",
                "ALTER DATABASE \"ventas\\x\" RENAME TO \"ventas2\";",
            ]
        );
        assert!(database_script(Variant::Redshift, "padb_harvest", "x").is_err());
    }

    #[test]
    fn cockroach_and_risingwave_rename_with_sessions_open() {
        let c = spec(Variant::Cockroach).unwrap();
        assert!(c.databases && !c.database_moves);
        assert_eq!(c.database_from.as_deref(), Some("defaultdb"));
        assert!(c.database_note.as_deref().unwrap().contains("no las cierra"));
        let s = db(Variant::Cockroach, "app", "App");
        assert_eq!(s.statements, ["ALTER DATABASE \"app\" RENAME TO \"App\";"]);
        assert!(s.warnings[0].contains("siguen abiertas"));
        assert!(matches!(database_script(Variant::Cockroach, "system", "x"), Err(Error::Unsupported(_))));
        // Its `postgres` database is an ordinary one.
        assert!(database_script(Variant::Cockroach, "postgres", "pg").is_ok());

        let rw = spec(Variant::RisingWave).unwrap();
        assert_eq!(rw.database_from.as_deref(), Some("dev"));
        assert_eq!(db(Variant::RisingWave, "a", "b").statements, ["ALTER DATABASE \"a\" RENAME TO \"b\";"]);
        assert!(rw.database_note.as_deref().unwrap().contains("reconectarse"));

        let yb = spec(Variant::Yellowbrick).unwrap();
        assert_eq!(yb.database_from.as_deref(), Some("yellowbrick"));
        assert_eq!(db(Variant::Yellowbrick, "a", "b").statements, ["ALTER DATABASE \"a\" RENAME TO \"b\";"]);
        assert!(yb.database_note.as_deref().unwrap().contains("Cerrá antes"));
    }

    #[test]
    fn database_rename_refusals() {
        for v in Variant::ALL {
            let Some(s) = spec(v) else { continue };
            if !s.databases {
                assert!(matches!(database_script(v, "a", "b"), Err(Error::Unsupported(_))), "{v:?}");
                continue;
            }
            let from = s.database_from.clone().expect("a database rename runs from somewhere");
            let e = database_script(v, &from, "x").unwrap_err().to_string();
            assert!(e.contains(&from) && e.contains("desde la que"), "{v:?}: {e}");
            assert!(s.database_note.as_deref().unwrap().contains(&format!("«{from}»")), "{v:?}");
            if v != Variant::Cockroach && v != Variant::RisingWave {
                for t in ["template0", "template1"] {
                    assert!(database_script(v, t, "x").unwrap_err().to_string().contains("sistema"), "{v:?} {t}");
                }
            }
            assert!(database_script(v, "", "x").is_err() && database_script(v, "a", "").is_err());
        }
        for v in [Variant::Materialize, Variant::CrateDb, Variant::H2, Variant::Denodo] {
            assert!(spec(v).is_none_or(|s| !s.databases && s.database_from.is_none()), "{v:?}");
            assert!(matches!(database_script(v, "a", "b"), Err(Error::Unsupported(_))), "{v:?}");
        }
    }
}
