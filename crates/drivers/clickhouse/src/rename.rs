//! "Renombrar…" for ClickHouse and Timeplus Proton.
//!
//! ClickHouse renames tables, views, materialized views (`RENAME TABLE`),
//! dictionaries (`RENAME DICTIONARY`), databases on the Atomic engine
//! (`RENAME DATABASE`) and columns (`ALTER TABLE … RENAME COLUMN`). Proton
//! spells both `STREAM` and has no database rename.
//!
//! Neither updates the code that names what was renamed. ClickHouse puts
//! views back with `CREATE OR REPLACE`, which also replaces a materialized
//! view (a new one: the data it kept in its own inner table goes). Proton
//! refuses to rename a stream, or a column of it, while views depend on it,
//! so its views are dropped before the rename and created after. A column in a key
//! can't be renamed (the server refuses it), and one a materialized view
//! reads is refused too: the view would stop loading rows. Both are checked
//! on the server by a guard that stops the script before the rename, since
//! `rename_script` has no connection.

use crate::schema::{q, string_literal};
use crate::{dialect, Flavor, DICTIONARY};
use dbine_driver::rename::{quote_new, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::{kinds, Error, ObjectRef, Result, SyncScript, TableSchema};

pub fn spec(flavor: Flavor) -> RenameSpec {
    let (kinds, schemas, replace, note) = match flavor {
        Flavor::ClickHouse => (
            vec![kinds::TABLE, kinds::VIEW, kinds::MATERIALIZED_VIEW, DICTIONARY],
            true,
            ReplaceStyle::CreateOrReplace,
            "ClickHouse no actualiza las vistas que usan lo renombrado: DBine las vuelve a crear con CREATE OR REPLACE. \
             Una vista materializada se reemplaza por una nueva: si no tiene TO y guarda sus propios datos, se pierden. \
             No se renombran columnas de la clave de ordenamiento, de la primaria, de la partición o de muestreo, ni las que \
             lee una vista materializada. Las tablas Distributed que apuntan a la tabla no se detectan: hay que revisarlas. \
             Las bases de datos solo se renombran con el motor Atomic.",
        ),
        Flavor::Timeplus => (
            vec![kinds::STREAM, kinds::VIEW, kinds::MATERIALIZED_VIEW],
            false,
            ReplaceStyle::DropCreate,
            "Timeplus Proton no renombra un stream ni sus columnas mientras haya vistas que lo usan: DBine las borra antes \
             y las vuelve a crear después. Una vista materializada creada de nuevo pierde lo que guardaba y vuelve a leer \
             desde el punto en que se crea. No se renombran columnas de la clave del stream.",
        ),
    };
    RenameSpec {
        kinds: kinds.into_iter().map(str::to_string).collect(),
        columns: true,
        indexes: false,
        constraints: false,
        schemas,
        tracked: Vec::new(),
        replace,
        references: ReferenceStyle::Sql,
        fold: Fold::None,
        transactional: false,
        note: Some(note.to_string()),
        // A materialized view without `TO` keeps its rows in an inner
        // table that a replace starts empty.
        holds_rows: vec![kinds::MATERIALIZED_VIEW.to_string()],
        ..Default::default()
    }
}

/// `db.name`, or `name` in the session's database.
fn qualified(o: &ObjectRef) -> String {
    match o.schema() {
        Some(s) => format!("{}.{}", q(s), q(&o.name)),
        None => q(&o.name),
    }
}

/// `expr` names `column` (as a whole name, bare or between backticks).
fn mentions(expr: &str, column: &str) -> bool {
    if expr.contains(&q(column)) {
        return true;
    }
    expr.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w == column)
}

fn key_message(column: &str) -> String {
    format!("No se puede renombrar la columna «{column}»: es parte de una clave de la tabla (de ordenamiento, primaria, de partición o de muestreo).")
}

fn view_message(column: &str) -> String {
    format!("No se puede renombrar la columna «{column}»: la lee una vista materializada, que dejaría de cargar filas. Hay que cambiar o borrar la vista antes.")
}

/// The key columns `database_schema` reports: the primary key and the
/// expressions of the sorting, partition and sampling keys.
fn in_key(t: &TableSchema, column: &str) -> bool {
    t.primary_key.as_ref().is_some_and(|k| k.columns.iter().any(|c| c == column || mentions(c, column)))
        || ["order_by", "partition_by", "sample_by"].iter().any(|k| t.options.get(*k).is_some_and(|e| mentions(e, column)))
}

/// A regular expression for `column` as a whole name in a query.
fn word_regex(column: &str) -> String {
    let mut esc = String::new();
    for c in column.chars() {
        if !(c.is_alphanumeric() || c == '_') {
            esc.push('\\');
        }
        esc.push(c);
    }
    format!("(^|[^A-Za-z0-9_]){esc}([^A-Za-z0-9_]|$)")
}

/// Stops the script, before the rename, when the column is in a key or a
/// materialized view reads it (`SELECT *` included).
fn guard(table: &ObjectRef, column: &str) -> String {
    let db = table.schema().map(string_literal).unwrap_or_else(|| "currentDatabase()".into());
    let (t, c) = (string_literal(&table.name), string_literal(column));
    format!(
        "SELECT\n  throwIf((SELECT count() FROM system.columns WHERE database = {db} AND table = {t} AND name = {c}\n    \
         AND (is_in_partition_key OR is_in_sorting_key OR is_in_primary_key OR is_in_sampling_key)) > 0,\n    {}),\n  \
         throwIf((SELECT count() FROM system.tables WHERE engine = 'MaterializedView'\n    \
         AND (database, name) IN (SELECT arrayJoin(arrayZip(dependencies_database, dependencies_table)) FROM system.tables WHERE database = {db} AND name = {t})\n    \
         AND (match(as_select, {}) OR position(as_select, '*') > 0)) > 0,\n    {})\nFORMAT Null;",
        string_literal(&key_message(column)),
        string_literal(&word_regex(column)),
        string_literal(&view_message(column)),
    )
}

pub fn script(flavor: Flavor, req: &RenameRequest) -> Result<SyncScript> {
    let spec = spec(flavor);
    if !spec.allows(&req.target) {
        return Err(Error::Unsupported(match flavor {
            Flavor::ClickHouse => "ClickHouse solo renombra tablas, vistas, vistas materializadas, diccionarios, columnas y bases de datos".into(),
            Flavor::Timeplus => "Timeplus Proton solo renombra streams, vistas, vistas materializadas y columnas".into(),
        }));
    }
    // Quoted, the new name goes through `q`, which escapes backslashes too.
    let new = match quote_new(&req.new_name, &dialect(), Fold::None, false) {
        bare if bare == req.new_name => bare,
        _ => q(&req.new_name),
    };
    let stream = flavor == Flavor::Timeplus;
    let mut warnings = Vec::new();
    let statements = match &req.target {
        RenameTarget::Object { object, .. } => {
            let what = if stream {
                "STREAM"
            } else if object.kind == DICTIONARY {
                "DICTIONARY"
            } else {
                "TABLE"
            };
            let to = match object.schema() {
                Some(s) => format!("{}.{new}", q(s)),
                None => new,
            };
            if object.kind == kinds::TABLE || object.kind == kinds::STREAM {
                warnings.push(match flavor {
                    Flavor::ClickHouse => "Las tablas Distributed que apuntan a esta tabla no se detectan: hay que revisarlas. Si un diccionario la lee, ClickHouse rechaza el cambio de nombre.".into(),
                    Flavor::Timeplus => "Los streams externos y los diccionarios que leen este stream por nombre no se detectan: hay que revisarlos.".into(),
                });
            }
            vec![format!("RENAME {what} {} TO {to};", qualified(object))]
        }
        RenameTarget::Column { table, column } => {
            if req.table.as_ref().is_some_and(|t| in_key(t, column)) {
                return Err(Error::Unsupported(key_message(column)));
            }
            let alter = if stream { "STREAM" } else { "TABLE" };
            warnings.push("Los diccionarios y las tablas Distributed que leen la columna no se detectan: hay que revisarlos.".into());
            vec![guard(table, column), format!("ALTER {alter} {} RENAME COLUMN {} TO {new};", qualified(table), q(column))]
        }
        RenameTarget::Schema { schema, .. } => {
            warnings.push("Solo las bases de datos con el motor Atomic se pueden renombrar; con otro motor, ClickHouse lo rechaza. También lo rechaza si un diccionario lee una de sus tablas.".into());
            // Requests name the session's database, which is the one renamed:
            // the session moves away first and then into the new name.
            vec!["USE system;".into(), format!("RENAME DATABASE {} TO {new};", q(schema)), format!("USE {new};")]
        }
        RenameTarget::Index { .. } | RenameTarget::Constraint { .. } => unreachable!("refused by the spec"),
    };
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::KeyDef;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: schema.map(str::to_string), name: name.into() }
    }

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    fn object(kind: &str, schema: Option<&str>, name: &str, new: &str) -> RenameRequest {
        req(RenameTarget::Object { object: obj(kind, schema, name), parent: None }, new)
    }

    #[test]
    fn tables_views_and_materialized_views() {
        let s = script(Flavor::ClickHouse, &object(kinds::TABLE, Some("ventas"), "T", "Nueva")).unwrap();
        assert_eq!(s.statements, ["RENAME TABLE `ventas`.`T` TO `ventas`.Nueva;"]);
        assert_eq!(s.warnings.len(), 1);
        let s = script(Flavor::ClickHouse, &object(kinds::VIEW, None, "v", "mi vista")).unwrap();
        assert_eq!(s.statements, ["RENAME TABLE `v` TO `mi vista`;"]);
        assert!(s.warnings.is_empty());
        let s = script(Flavor::ClickHouse, &object(kinds::MATERIALIZED_VIEW, Some("db"), "mv", "select")).unwrap();
        assert_eq!(s.statements, ["RENAME TABLE `db`.`mv` TO `db`.`select`;"]);
    }

    #[test]
    fn a_backslash_cannot_end_the_quoted_names() {
        let s = script(Flavor::ClickHouse, &object(kinds::TABLE, Some("d\\"), "t\\", "x\\`; DROP TABLE y; --")).unwrap();
        assert_eq!(s.statements, ["RENAME TABLE `d\\\\`.`t\\\\` TO `d\\\\`.`x\\\\\\`; DROP TABLE y; --`;"]);
    }

    #[test]
    fn dictionaries() {
        let s = script(Flavor::ClickHouse, &object(DICTIONARY, Some("db"), "d", "d2")).unwrap();
        assert_eq!(s.statements, ["RENAME DICTIONARY `db`.`d` TO `db`.d2;"]);
    }

    #[test]
    fn databases_move_the_session_away() {
        let s = script(Flavor::ClickHouse, &req(RenameTarget::Schema { database: Some("old".into()), schema: "old".into() }, "Nueva-Base")).unwrap();
        assert_eq!(s.statements, ["USE system;", "RENAME DATABASE `old` TO `Nueva-Base`;", "USE `Nueva-Base`;"]);
        assert!(s.warnings[0].contains("Atomic"));
    }

    #[test]
    fn columns_keep_case_and_are_guarded() {
        let s = script(Flavor::ClickHouse, &req(RenameTarget::Column { table: obj(kinds::TABLE, Some("db"), "T"), column: "pepe".into() }, "Juan")).unwrap();
        assert_eq!(s.statements.len(), 2);
        assert_eq!(s.statements[1], "ALTER TABLE `db`.`T` RENAME COLUMN `pepe` TO Juan;");
        let g = &s.statements[0];
        assert!(g.contains("database = 'db' AND table = 'T' AND name = 'pepe'"), "{g}");
        assert!(g.contains("is_in_sorting_key") && g.contains("dependencies_table") && g.contains("FORMAT Null"), "{g}");
        // Unqualified: the session's database.
        let s = script(Flavor::ClickHouse, &req(RenameTarget::Column { table: obj(kinds::TABLE, None, "T"), column: "a.b".into() }, "c")).unwrap();
        assert!(s.statements[0].contains("database = currentDatabase()"));
        assert!(s.statements[0].contains(r"(^|[^A-Za-z0-9_])a\\.b([^A-Za-z0-9_]|$)"), "{}", s.statements[0]);
        assert_eq!(s.statements[1], "ALTER TABLE `T` RENAME COLUMN `a.b` TO c;");
    }

    #[test]
    fn key_columns_are_refused() {
        let mut t = TableSchema { name: "T".into(), primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }), ..Default::default() };
        t.options.insert("partition_by".into(), "toYYYYMM(fecha)".into());
        t.options.insert("order_by".into(), "(id, `cliente id`)".into());
        let col = |c: &str| RenameRequest { table: Some(t.clone()), ..req(RenameTarget::Column { table: obj(kinds::TABLE, None, "T"), column: c.into() }, "x") };
        for c in ["id", "fecha", "cliente id"] {
            let e = script(Flavor::ClickHouse, &col(c)).unwrap_err();
            assert!(matches!(&e, Error::Unsupported(m) if m.contains(&format!("«{c}»")) && m.contains("clave")), "{e:?}");
        }
        assert!(script(Flavor::ClickHouse, &col("fechas")).is_ok());
        assert!(script(Flavor::ClickHouse, &col("pepe")).is_ok());
    }

    #[test]
    fn timeplus_says_stream() {
        let s = script(Flavor::Timeplus, &object(kinds::STREAM, None, "s", "s2")).unwrap();
        assert_eq!(s.statements, ["RENAME STREAM `s` TO s2;"]);
        let s = script(Flavor::Timeplus, &object(kinds::VIEW, None, "v", "v2")).unwrap();
        assert_eq!(s.statements, ["RENAME STREAM `v` TO v2;"]);
        let s = script(Flavor::Timeplus, &req(RenameTarget::Column { table: obj(kinds::STREAM, None, "s"), column: "pepe".into() }, "juan")).unwrap();
        assert_eq!(s.statements[1], "ALTER STREAM `s` RENAME COLUMN `pepe` TO juan;");
        assert_eq!(spec(Flavor::Timeplus).replace, ReplaceStyle::DropCreate);
        let db = req(RenameTarget::Schema { database: None, schema: "default".into() }, "x");
        assert!(matches!(script(Flavor::Timeplus, &db), Err(Error::Unsupported(_))));
    }

    #[test]
    fn what_is_not_renamed() {
        let ix = req(RenameTarget::Index { table: obj(kinds::TABLE, None, "T"), index: "ix".into() }, "iy");
        assert!(matches!(script(Flavor::ClickHouse, &ix), Err(Error::Unsupported(_))));
        assert!(matches!(script(Flavor::ClickHouse, &object(kinds::FUNCTION, None, "f", "g")), Err(Error::Unsupported(_))));
        let s = spec(Flavor::ClickHouse);
        assert_eq!(s.replace, ReplaceStyle::CreateOrReplace);
        assert!(s.columns && s.schemas && !s.indexes && !s.transactional);
        assert_eq!(s.holds_rows, [kinds::MATERIALIZED_VIEW]);
    }
}
