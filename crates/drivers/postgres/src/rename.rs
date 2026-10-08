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
        ..Default::default()
    })
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
}
