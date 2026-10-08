//! "Renombrar…" for SQLite and libSQL (which reuses it).
//!
//! Tables (virtual ones too, where the module allows it) are renamed with
//! `ALTER TABLE … RENAME TO` and columns with `RENAME COLUMN` (3.25+).
//! With `legacy_alter_table` off, SQLite rewrites by itself the views,
//! triggers, indexes, checks and foreign keys that name them (`tracked`).
//! Triggers and indexes have no `RENAME`: they are created again under the
//! new name and the old one dropped. DDL is transactional, so the whole
//! script runs atomically.
//!
//! Views are not offered: renaming one means creating it again, and SQLite
//! doesn't follow that in the views and triggers that use it, while the
//! spec's `tracked` (views and triggers, right for tables and columns)
//! would show them as followed.
//!
//! libSQL's server refuses `PRAGMA legacy_alter_table`; SQLite's script
//! turns it off before renaming a table, in case the connection had it on.

use crate::properties::Flavor;
use crate::schema::{table_ddl, VIRTUAL_TABLE};
use dbine_driver::rename::{quote_new, rename_header, Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle, ReplaceStyle};
use dbine_driver::sql::{qualified_name, quote_ident, Quote, ScriptDialect};
use dbine_driver::{kinds, DdlParts, Error, ObjectRef, Result, SyncScript, TableSchema};

pub const NOTE: &str = "SQLite actualiza por sí solo las vistas, los triggers, los índices, los CHECK y las claves foráneas que usan la tabla o la columna. Si alguna vista de la base ya está rota (usa una tabla o una columna que no existe), SQLite rechaza el cambio: corregila o borrala antes. Los triggers y los índices se renombran creándolos con el nombre nuevo y borrando los anteriores.";

const LEGACY_OFF: &str = "PRAGMA legacy_alter_table = OFF;";

/// SQLite's script dialect (`[name]` too, trigger bodies kept whole).
pub fn dialect() -> ScriptDialect {
    ScriptDialect { bracket_idents: true, ..ScriptDialect::generic() }
}

fn engine(f: Flavor) -> &'static str {
    match f {
        Flavor::Sqlite => "SQLite",
        Flavor::Libsql => "libSQL",
    }
}

pub fn spec(_f: Flavor) -> RenameSpec {
    RenameSpec {
        kinds: vec![kinds::TABLE.into(), VIRTUAL_TABLE.into(), kinds::TRIGGER.into()],
        columns: true,
        indexes: true,
        constraints: false,
        schemas: false,
        tracked: vec![kinds::VIEW.into(), kinds::TRIGGER.into()],
        // No CREATE OR REPLACE in SQLite.
        replace: ReplaceStyle::DropCreate,
        references: ReferenceStyle::Sql,
        fold: Fold::None,
        transactional: true,
        note: Some(NOTE.into()),
        ..Default::default()
    }
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn qn(o: &ObjectRef) -> String {
    qualified_name(Quote::Double, o.schema(), &o.name)
}

fn written(name: &str) -> String {
    quote_new(name, &dialect(), Fold::None, false)
}

/// The statements that rename the target.
pub fn script(f: Flavor, req: &RenameRequest) -> Result<SyncScript> {
    let engine = engine(f);
    let new = written(&req.new_name);
    let statements = match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE || object.kind == VIRTUAL_TABLE => {
            let rename = format!("ALTER TABLE {} RENAME TO {new};", qn(object));
            match f {
                Flavor::Sqlite => vec![LEGACY_OFF.to_string(), rename],
                Flavor::Libsql => vec![rename],
            }
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::TRIGGER => {
            let def = req
                .definition
                .as_deref()
                .ok_or_else(|| Error::Query(format!("no se pudo leer la definición del trigger «{}» para renombrarlo", object.name)))?;
            let created = rename_header(def, &dialect(), Fold::None, &req.new_name)
                .ok_or_else(|| Error::Query(format!("no se pudo leer el encabezado CREATE TRIGGER de «{}»", object.name)))?;
            vec![terminated(&created), format!("DROP TRIGGER {};", qn(object))]
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::VIEW => {
            return Err(Error::Unsupported(format!(
                "{engine} no renombra vistas desde DBine: creá la vista con el nombre nuevo, corregí lo que la usa y borrá la anterior"
            )))
        }
        RenameTarget::Object { object, .. } => return Err(Error::Unsupported(format!("{engine} no renombra objetos de tipo «{}»", object.kind))),
        RenameTarget::Column { table, column } => vec![format!("ALTER TABLE {} RENAME COLUMN {} TO {new};", qn(table), q(column))],
        RenameTarget::Index { table, index } => index_script(engine, table, index, &req.new_name, req.table.as_ref())?,
        RenameTarget::Constraint { .. } => {
            return Err(Error::Unsupported(format!("{engine} no renombra restricciones: hay que reconstruir la tabla con el nombre nuevo")))
        }
        RenameTarget::Schema { .. } => return Err(Error::Unsupported(format!("{engine} no renombra bases adjuntas"))),
    };
    Ok(SyncScript { statements, warnings: vec![] })
}

/// The index created again with the new name (keys, order, collation and
/// `WHERE` as the catalog reads them), then the old one dropped.
fn index_script(engine: &str, table: &ObjectRef, index: &str, new_name: &str, schema: Option<&TableSchema>) -> Result<Vec<String>> {
    if index.starts_with("sqlite_autoindex_") {
        return Err(Error::Unsupported(format!(
            "ese índice es el de una restricción PRIMARY KEY o UNIQUE: {engine} le pone el nombre y no se renombra"
        )));
    }
    let t = schema.ok_or_else(|| Error::Query(format!("no se pudo leer la definición de la tabla «{}» para renombrar el índice", table.name)))?;
    let ix = t
        .indexes
        .iter()
        .find(|i| i.name == index)
        .or_else(|| t.indexes.iter().find(|i| i.name.eq_ignore_ascii_case(index)))
        .ok_or_else(|| Error::Query(format!("la tabla «{}» no tiene el índice «{index}»", table.name)))?;
    let one = TableSchema {
        name: t.name.clone(),
        indexes: vec![dbine_driver::IndexDef { name: new_name.to_string(), ..ix.clone() }],
        ..Default::default()
    };
    let ddl = table_ddl(&one, DdlParts { indexes: true, ..Default::default() });
    // The DDL quotes the name in double quotes; the new one goes as the
    // rewrite of the dependents writes it.
    let created = ddl.trim().replacen(&format!("INDEX {}", q(new_name)), &format!("INDEX {}", written(new_name)), 1);
    let created = match table.schema() {
        Some(s) if !s.is_empty() => created.replacen(&format!("INDEX {}", written(new_name)), &format!("INDEX {}.{}", q(s), written(new_name)), 1),
        _ => created,
    };
    let old = ObjectRef { kind: kinds::INDEX.into(), schema: table.schema.clone(), name: ix.name.clone() };
    Ok(vec![terminated(&created), format!("DROP INDEX {};", qn(&old))])
}

fn terminated(sql: &str) -> String {
    let t = sql.trim_end();
    if t.ends_with(';') {
        t.to_string()
    } else {
        format!("{t};")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::IndexDef;

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    fn obj(kind: &str, name: &str) -> RenameTarget {
        RenameTarget::Object { object: ObjectRef { kind: kind.into(), schema: None, name: name.into() }, parent: None }
    }

    fn tref(name: &str) -> ObjectRef {
        ObjectRef { kind: kinds::TABLE.into(), schema: None, name: name.into() }
    }

    #[test]
    fn spec_lists_what_sqlite_renames() {
        for f in [Flavor::Sqlite, Flavor::Libsql] {
            let s = spec(f);
            assert_eq!(s.kinds, ["table", "virtual_table", "trigger"]);
            assert!(s.columns && s.indexes && !s.constraints && !s.schemas && s.transactional);
            assert_eq!(s.tracked, ["view", "trigger"]);
            assert_eq!(s.replace, ReplaceStyle::DropCreate);
            assert_eq!(s.fold, Fold::None);
            assert!(s.note.as_deref().unwrap().contains("rota"));
        }
    }

    #[test]
    fn tables() {
        let s = script(Flavor::Sqlite, &req(obj("table", "clientes"), "Clientes Viejos")).unwrap();
        assert_eq!(s.statements, ["PRAGMA legacy_alter_table = OFF;", "ALTER TABLE \"clientes\" RENAME TO [Clientes Viejos];"]);
        // libSQL's server refuses the PRAGMA.
        let s = script(Flavor::Libsql, &req(obj("table", "a\"b"), "Nueva")).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE \"a\"\"b\" RENAME TO Nueva;"]);
        assert!(s.statements.iter().all(|x| !x.contains("legacy_alter_table")));
        let s = script(Flavor::Libsql, &req(obj("virtual_table", "docs"), "select")).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE \"docs\" RENAME TO [select];"]);
        let other = RenameTarget::Object { object: ObjectRef { kind: "table".into(), schema: Some("aux".into()), name: "t".into() }, parent: None };
        assert_eq!(script(Flavor::Sqlite, &req(other, "u")).unwrap().statements[1], "ALTER TABLE \"aux\".\"t\" RENAME TO u;");
    }

    #[test]
    fn columns() {
        for f in [Flavor::Sqlite, Flavor::Libsql] {
            let s = script(f, &req(RenameTarget::Column { table: tref("t"), column: "pepe".into() }, "Nuevo Pepe")).unwrap();
            assert_eq!(s.statements, ["ALTER TABLE \"t\" RENAME COLUMN \"pepe\" TO [Nuevo Pepe];"]);
            let s = script(f, &req(RenameTarget::Column { table: tref("t"), column: "pepe".into() }, "pepa")).unwrap();
            assert_eq!(s.statements, ["ALTER TABLE \"t\" RENAME COLUMN \"pepe\" TO pepa;"]);
        }
    }

    #[test]
    fn triggers_are_created_again() {
        let mut r = req(obj("trigger", "tr_log"), "Tr Log");
        r.definition = Some("CREATE TRIGGER tr_log AFTER INSERT ON t BEGIN INSERT INTO log VALUES (NEW.id); END".into());
        let s = script(Flavor::Libsql, &r).unwrap();
        assert_eq!(
            s.statements,
            ["CREATE TRIGGER [Tr Log] AFTER INSERT ON t BEGIN INSERT INTO log VALUES (NEW.id); END;", "DROP TRIGGER \"tr_log\";"]
        );
        let mut r = req(obj("trigger", "tr_log"), "tr2");
        r.definition = Some("CREATE TRIGGER IF NOT EXISTS \"tr_log\" BEFORE DELETE ON t BEGIN SELECT 1; END;".into());
        let s = script(Flavor::Sqlite, &r).unwrap();
        assert_eq!(s.statements[0], "CREATE TRIGGER IF NOT EXISTS \"tr2\" BEFORE DELETE ON t BEGIN SELECT 1; END;");
        assert!(matches!(script(Flavor::Sqlite, &req(obj("trigger", "x"), "y")), Err(Error::Query(_))));
    }

    #[test]
    fn indexes_are_created_again() {
        let t = TableSchema {
            name: "t".into(),
            indexes: vec![IndexDef {
                name: "ix_pepe".into(),
                columns: vec!["pepe".into(), "(lower(x))".into()],
                unique: true,
                filter: Some("pepe IS NOT NULL".into()),
                options: [("desc".to_string(), "pepe".to_string()), ("collate:pepe".to_string(), "NOCASE".to_string())].into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut r = req(RenameTarget::Index { table: tref("t"), index: "IX_PEPE".into() }, "Ix Nuevo");
        r.table = Some(t.clone());
        let s = script(Flavor::Sqlite, &r).unwrap();
        assert_eq!(
            s.statements,
            ["CREATE UNIQUE INDEX [Ix Nuevo] ON \"t\" (\"pepe\" COLLATE NOCASE DESC, (lower(x))) WHERE pepe IS NOT NULL;", "DROP INDEX \"ix_pepe\";"]
        );
        let mut r = req(RenameTarget::Index { table: tref("t"), index: "nada".into() }, "x");
        r.table = Some(t);
        assert!(matches!(script(Flavor::Libsql, &r), Err(Error::Query(_))));
        let r = req(RenameTarget::Index { table: tref("t"), index: "sqlite_autoindex_t_1".into() }, "x");
        assert!(matches!(script(Flavor::Sqlite, &r), Err(Error::Unsupported(_))));
    }

    #[test]
    fn refused() {
        assert!(matches!(script(Flavor::Sqlite, &req(obj("view", "v"), "w")), Err(Error::Unsupported(_))));
        let c = RenameTarget::Constraint { table: tref("t"), constraint: "ck".into() };
        assert!(matches!(script(Flavor::Sqlite, &req(c, "ck2")), Err(Error::Unsupported(_))));
        let d = RenameTarget::Schema { database: None, schema: "aux".into() };
        assert!(matches!(script(Flavor::Libsql, &req(d, "aux2")), Err(Error::Unsupported(_))));
    }
}
