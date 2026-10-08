//! "Renombrar…" per preset, where the engine's syntax is documented:
//!
//! - Db2 (LUW, z/OS): `RENAME TABLE|INDEX`, `ALTER TABLE … RENAME COLUMN`.
//!   Db2 for i: `RENAME TABLE|INDEX` (a view goes through `RENAME TABLE`),
//!   no column rename.
//! - Sybase ASE: `sp_rename` (tables, views, `t.col`, `t.ix`).
//! - Informix: `RENAME TABLE|COLUMN|INDEX`; it updates the views itself.
//! - Teradata: `RENAME TABLE|VIEW`.
//! - Vertica: `ALTER TABLE|VIEW|SCHEMA … RENAME TO`, `RENAME COLUMN`.
//! - Exasol: `RENAME TABLE|VIEW|SCHEMA`, `ALTER TABLE … RENAME COLUMN`.
//!
//! The rest of the presets don't offer it (see [`spec`]).

use crate::design;
use crate::presets::Preset;
use dbine_driver::rename::{quote_new, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::sql::{qualified_name, quote_ident, ScriptDialect};
use dbine_driver::{kinds, Error, Result, SyncScript};

/// What the preset renames, and how its dependents come back.
struct Engine {
    kinds: &'static [&'static str],
    columns: bool,
    indexes: bool,
    schemas: bool,
    tracked: &'static [&'static str],
    replace: ReplaceStyle,
    fold: Fold,
    transactional: bool,
    note: &'static str,
}

fn engine(p: &Preset) -> Option<Engine> {
    let base = Engine {
        kinds: &[kinds::TABLE],
        columns: false,
        indexes: false,
        schemas: false,
        tracked: &[],
        replace: ReplaceStyle::DropCreate,
        fold: Fold::None,
        transactional: false,
        note: "",
    };
    Some(match p.id {
        // RENAME TABLE refuses a table with triggers or in a foreign key;
        // views and routines are invalidated, and CREATE OR REPLACE
        // revalidates them. DDL is transactional.
        "db2" => Engine {
            columns: true,
            indexes: true,
            replace: ReplaceStyle::CreateOrReplace,
            fold: Fold::Upper,
            transactional: true,
            note: "Db2 rechaza renombrar una tabla que tiene triggers o que está en una clave foránea (como padre o como hija): \
                   hay que quitarlos antes. Las vistas y rutinas que usan el nombre viejo quedan inválidas hasta que se reponen \
                   con CREATE OR REPLACE.",
            ..base
        },
        // RENAME TABLE refuses a table that a view reads: views go first.
        "db2zos" => Engine {
            columns: true,
            indexes: true,
            fold: Fold::Upper,
            note: "Db2 for z/OS rechaza renombrar una tabla que tiene triggers o que lee una vista: las vistas que se reescriben \
                   se borran antes y se vuelven a crear después; las demás hacen fallar el cambio.",
            ..base
        },
        "db2i" => Engine {
            kinds: &[kinds::TABLE, kinds::VIEW],
            indexes: true,
            fold: Fold::Upper,
            note: "Db2 for i renombra tablas, vistas e índices, no columnas. Lo que usa el nombre viejo se borra y se vuelve a \
                   crear.",
            ..base
        },
        "sybase" => Engine {
            kinds: &[kinds::TABLE, kinds::VIEW],
            columns: true,
            indexes: true,
            note: "Se renombra con sp_rename, que solo renombra objetos del usuario conectado. El texto guardado de las vistas \
                   y procedimientos sigue con el nombre viejo: los que se reescriben se borran y se vuelven a crear.",
            ..base
        },
        // Informix rewrites the views that read a renamed table (sysviews);
        // a trigger gets its header changed, not its actions.
        "informix" => Engine {
            columns: true,
            indexes: true,
            tracked: &[kinds::VIEW],
            fold: Fold::Lower,
            note: "Informix actualiza solo las vistas que usan el nombre viejo. En los triggers cambia el encabezado pero no las \
                   acciones, y los procedimientos SPL quedan como estaban: los que se reescriben se borran y se vuelven a crear.",
            ..base
        },
        // REPLACE VIEW / REPLACE PROCEDURE, not CREATE OR REPLACE.
        "teradata" => Engine {
            kinds: &[kinds::TABLE, kinds::VIEW],
            note: "Teradata no actualiza las vistas, macros ni procedimientos que usan el nombre viejo: los que se reescriben se \
                   borran y se vuelven a crear.",
            ..base
        },
        "vertica" => Engine {
            kinds: &[kinds::TABLE, kinds::VIEW],
            columns: true,
            schemas: true,
            replace: ReplaceStyle::CreateOrReplace,
            note: "Vertica no actualiza las vistas ni los procedimientos que usan el nombre viejo: se reponen con CREATE OR \
                   REPLACE. Cada cambio se confirma solo.",
            ..base
        },
        "exasol" => Engine {
            kinds: &[kinds::TABLE, kinds::VIEW],
            columns: true,
            schemas: true,
            replace: ReplaceStyle::CreateOrReplace,
            fold: Fold::Upper,
            note: "Exasol no actualiza las vistas ni las funciones que usan el nombre viejo: se reponen con CREATE OR REPLACE. \
                   Al renombrar una vista, su texto guardado sigue con el nombre viejo en el encabezado.",
            ..base
        },
        _ => return None,
    })
}

pub fn spec(p: &Preset) -> Option<RenameSpec> {
    let e = engine(p)?;
    Some(RenameSpec {
        kinds: e.kinds.iter().map(|k| k.to_string()).collect(),
        columns: e.columns,
        indexes: e.indexes,
        constraints: false,
        schemas: e.schemas,
        tracked: e.tracked.iter().map(|k| k.to_string()).collect(),
        replace: e.replace,
        references: ReferenceStyle::Sql,
        fold: e.fold,
        transactional: e.transactional,
        note: Some(e.note.into()),
    })
}

pub fn script(p: &Preset, dialect: &ScriptDialect, req: &RenameRequest) -> Result<SyncScript> {
    let Some(e) = engine(p) else {
        return Err(Error::Unsupported(format!("{} no renombra objetos desde DBine", p.name)));
    };
    let refused = |what: &str| Err(Error::Unsupported(format!("{} no renombra {what} desde DBine", p.name)));
    let allowed = match &req.target {
        RenameTarget::Object { object, .. } => e.kinds.contains(&object.kind.as_str()),
        RenameTarget::Column { .. } => e.columns,
        RenameTarget::Index { .. } => e.indexes,
        RenameTarget::Constraint { .. } => false,
        RenameTarget::Schema { .. } => e.schemas,
    };
    if !allowed {
        return refused(match &req.target {
            RenameTarget::Object { .. } => "esa clase de objeto",
            RenameTarget::Column { .. } => "columnas",
            RenameTarget::Index { .. } => "índices",
            RenameTarget::Constraint { .. } => "restricciones",
            RenameTarget::Schema { .. } => "esquemas",
        });
    }
    let q = design::quote(p);
    let qn = |schema: Option<&str>, name: &str| qualified_name(q, schema, name);
    let new = quote_new(&req.new_name, dialect, e.fold, false);
    let statement = match (p.id, &req.target) {
        // sp_rename takes names as strings: the new one exactly as stored.
        ("sybase", target) => {
            let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
            let to = lit(&req.new_name);
            match target {
                RenameTarget::Object { object, .. } => format!("EXEC sp_rename {}, {to}", lit(&object.name)),
                RenameTarget::Column { table, column } => format!("EXEC sp_rename {}, {to}, 'column'", lit(&format!("{}.{column}", table.name))),
                RenameTarget::Index { table, index } => format!("EXEC sp_rename {}, {to}, 'index'", lit(&format!("{}.{index}", table.name))),
                _ => unreachable!("checked above"),
            }
        }
        // The new name is unqualified: it stays in the old one's schema.
        ("db2" | "db2zos" | "db2i", RenameTarget::Object { object, .. }) => format!("RENAME TABLE {} TO {new}", qn(object.schema(), &object.name)),
        ("db2" | "db2zos" | "db2i" | "informix", RenameTarget::Index { table, index }) => format!("RENAME INDEX {} TO {new}", qn(table.schema(), index)),
        ("db2" | "db2zos" | "vertica" | "exasol", RenameTarget::Column { table, column }) => {
            format!("ALTER TABLE {} RENAME COLUMN {} TO {new}", qn(table.schema(), &table.name), quote_ident(q, column))
        }
        ("informix", RenameTarget::Object { object, .. }) => format!("RENAME TABLE {} TO {new}", qn(object.schema(), &object.name)),
        ("informix", RenameTarget::Column { table, column }) => format!("RENAME COLUMN {}.{} TO {new}", qn(table.schema(), &table.name), quote_ident(q, column)),
        // Unqualified, the new name would land in the default database.
        ("teradata", RenameTarget::Object { object, .. }) => {
            let what = if object.kind == kinds::VIEW { "VIEW" } else { "TABLE" };
            let to = match object.schema() {
                Some(db) => format!("{}.{new}", quote_ident(q, db)),
                None => new.clone(),
            };
            format!("RENAME {what} {} TO {to}", qn(object.schema(), &object.name))
        }
        ("vertica", RenameTarget::Object { object, .. }) => {
            let what = if object.kind == kinds::VIEW { "VIEW" } else { "TABLE" };
            format!("ALTER {what} {} RENAME TO {new}", qn(object.schema(), &object.name))
        }
        ("vertica", RenameTarget::Schema { schema, .. }) => format!("ALTER SCHEMA {} RENAME TO {new}", quote_ident(q, schema)),
        ("exasol", RenameTarget::Object { object, .. }) => {
            let what = if object.kind == kinds::VIEW { "VIEW" } else { "TABLE" };
            format!("RENAME {what} {} TO {new}", qn(object.schema(), &object.name))
        }
        ("exasol", RenameTarget::Schema { schema, .. }) => format!("RENAME SCHEMA {} TO {new}", quote_ident(q, schema)),
        _ => return refused("eso"),
    };
    Ok(SyncScript { statements: vec![statement], warnings: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PRESETS;
    use dbine_driver::ObjectRef;

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: Some(schema.into()), name: name.into() }
    }

    fn table() -> ObjectRef {
        obj(kinds::TABLE, "App", "Clientes")
    }

    fn run(id: &str, target: RenameTarget, new_name: &str) -> Result<String> {
        let p = preset(id);
        let req = RenameRequest { target, new_name: new_name.into(), table: None, definition: None };
        script(p, &crate::script_dialect(p), &req).map(|s| {
            assert_eq!(s.statements.len(), 1);
            s.statements[0].clone()
        })
    }

    fn object(o: ObjectRef) -> RenameTarget {
        RenameTarget::Object { object: o, parent: None }
    }

    fn column(c: &str) -> RenameTarget {
        RenameTarget::Column { table: table(), column: c.into() }
    }

    fn index(i: &str) -> RenameTarget {
        RenameTarget::Index { table: table(), index: i.into() }
    }

    fn schema(s: &str) -> RenameTarget {
        RenameTarget::Schema { database: None, schema: s.into() }
    }

    fn unsupported(r: Result<String>) -> bool {
        matches!(r, Err(Error::Unsupported(_)))
    }

    #[test]
    fn db2() {
        for id in ["db2", "db2zos"] {
            assert_eq!(run(id, object(table()), "SOCIOS").unwrap(), r#"RENAME TABLE "App"."Clientes" TO SOCIOS"#);
            // Upper case is the folded case: anything else is quoted.
            assert_eq!(run(id, object(table()), "Socios").unwrap(), r#"RENAME TABLE "App"."Clientes" TO "Socios""#);
            assert_eq!(run(id, column("PEPE"), "NOMBRE").unwrap(), r#"ALTER TABLE "App"."Clientes" RENAME COLUMN "PEPE" TO NOMBRE"#);
            assert_eq!(run(id, index("IX_PEPE"), "IX_NOMBRE").unwrap(), r#"RENAME INDEX "App"."IX_PEPE" TO IX_NOMBRE"#);
            assert!(unsupported(run(id, object(obj(kinds::VIEW, "App", "V")), "W")));
            assert!(unsupported(run(id, schema("App"), "X")));
            assert!(unsupported(run(id, RenameTarget::Constraint { table: table(), constraint: "CK".into() }, "X")));
        }
        assert_eq!(run("db2i", object(obj(kinds::VIEW, "APP", "V")), "W").unwrap(), r#"RENAME TABLE "APP"."V" TO W"#);
        assert_eq!(run("db2i", index("IX"), "IX2").unwrap(), r#"RENAME INDEX "App"."IX" TO IX2"#);
        assert!(unsupported(run("db2i", column("PEPE"), "NOMBRE")));
        let s = spec(preset("db2")).unwrap();
        assert!(s.transactional && s.replace == ReplaceStyle::CreateOrReplace && s.fold == Fold::Upper);
        let z = spec(preset("db2zos")).unwrap();
        assert!(!z.transactional && z.replace == ReplaceStyle::DropCreate);
    }

    #[test]
    fn sybase_uses_sp_rename() {
        assert_eq!(run("sybase", object(table()), "Socios").unwrap(), "EXEC sp_rename 'Clientes', 'Socios'");
        assert_eq!(run("sybase", object(obj(kinds::VIEW, "dbo", "v_x")), "v y").unwrap(), "EXEC sp_rename 'v_x', 'v y'");
        assert_eq!(run("sybase", column("pepe"), "O'Brien").unwrap(), "EXEC sp_rename 'Clientes.pepe', 'O''Brien', 'column'");
        assert_eq!(run("sybase", index("ix_pepe"), "ix_nombre").unwrap(), "EXEC sp_rename 'Clientes.ix_pepe', 'ix_nombre', 'index'");
        assert!(unsupported(run("sybase", object(obj(kinds::PROCEDURE, "dbo", "p")), "q")));
        assert_eq!(spec(preset("sybase")).unwrap().fold, Fold::None);
    }

    #[test]
    fn informix() {
        assert_eq!(run("informix", object(obj(kinds::TABLE, "informix", "clientes")), "socios").unwrap(), r#"RENAME TABLE "informix"."clientes" TO socios"#);
        // Lower case is the folded case.
        assert_eq!(run("informix", object(table()), "Socios").unwrap(), r#"RENAME TABLE "App"."Clientes" TO "Socios""#);
        assert_eq!(run("informix", column("pepe"), "nombre").unwrap(), r#"RENAME COLUMN "App"."Clientes"."pepe" TO nombre"#);
        assert_eq!(run("informix", index("ix_pepe"), "ix_nombre").unwrap(), r#"RENAME INDEX "App"."ix_pepe" TO ix_nombre"#);
        let s = spec(preset("informix")).unwrap();
        assert_eq!(s.tracked, [kinds::VIEW]);
        assert_eq!(s.fold, Fold::Lower);
    }

    #[test]
    fn teradata() {
        assert_eq!(run("teradata", object(obj(kinds::TABLE, "ventas", "Clientes")), "Socios").unwrap(), r#"RENAME TABLE "ventas"."Clientes" TO "ventas".Socios"#);
        assert_eq!(run("teradata", object(obj(kinds::VIEW, "ventas", "v")), "w").unwrap(), r#"RENAME VIEW "ventas"."v" TO "ventas".w"#);
        assert!(unsupported(run("teradata", column("pepe"), "nombre")));
        assert!(unsupported(run("teradata", index("ix"), "ix2")));
    }

    #[test]
    fn vertica() {
        assert_eq!(run("vertica", object(table()), "Socios").unwrap(), r#"ALTER TABLE "App"."Clientes" RENAME TO Socios"#);
        assert_eq!(run("vertica", object(obj(kinds::VIEW, "App", "v")), "mi vista").unwrap(), r#"ALTER VIEW "App"."v" RENAME TO "mi vista""#);
        assert_eq!(run("vertica", column("pepe"), "nombre").unwrap(), r#"ALTER TABLE "App"."Clientes" RENAME COLUMN "pepe" TO nombre"#);
        assert_eq!(run("vertica", schema("App"), "ventas").unwrap(), r#"ALTER SCHEMA "App" RENAME TO ventas"#);
        assert!(unsupported(run("vertica", index("ix"), "ix2")));
        assert_eq!(spec(preset("vertica")).unwrap().replace, ReplaceStyle::CreateOrReplace);
    }

    #[test]
    fn exasol() {
        assert_eq!(run("exasol", object(table()), "SOCIOS").unwrap(), r#"RENAME TABLE "App"."Clientes" TO SOCIOS"#);
        assert_eq!(run("exasol", object(obj(kinds::VIEW, "APP", "V")), "w").unwrap(), r#"RENAME VIEW "APP"."V" TO "w""#);
        assert_eq!(run("exasol", column("PEPE"), "NOMBRE").unwrap(), r#"ALTER TABLE "App"."Clientes" RENAME COLUMN "PEPE" TO NOMBRE"#);
        assert_eq!(run("exasol", schema("APP"), "VENTAS").unwrap(), r#"RENAME SCHEMA "APP" TO VENTAS"#);
        assert!(unsupported(run("exasol", index("ix"), "ix2")));
        assert_eq!(spec(preset("exasol")).unwrap().fold, Fold::Upper);
    }

    #[test]
    fn other_presets_do_not_rename() {
        for p in PRESETS.iter().filter(|p| !["db2", "db2zos", "db2i", "sybase", "informix", "teradata", "vertica", "exasol"].contains(&p.id)) {
            assert!(spec(p).is_none(), "{}", p.id);
            assert!(unsupported(run(p.id, object(table()), "X")), "{}", p.id);
        }
    }
}
