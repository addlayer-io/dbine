//! "Renombrar…" on Drill: views only, created again with the new name and
//! the old one dropped (Drill has no `RENAME`; a view is a `.view.drill`
//! file in a writable workspace). Tables are files or folders of the
//! storage plugin: Drill doesn't rename them, and its workspaces aren't
//! renamed from SQL.
//!
//! Views keep their SQL text (Drill stores it with every name in
//! backticks) and bind names when queried: the app rewrites the ones that
//! name the view and puts them back with `CREATE OR REPLACE VIEW`.

use dbine_driver::rename::{rename_header, Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle, ReplaceStyle};
use dbine_driver::sql::split_script;
use dbine_driver::{kinds, Error, Result, SyncScript};

pub const NOTE: &str = "Drill solo renombra vistas: la crea con el nombre nuevo en el mismo espacio de trabajo y borra la anterior. Las vistas que la usan no se actualizan solas: DBine las reescribe y las repone con CREATE OR REPLACE VIEW. Las sentencias no son transaccionales.";

pub fn spec() -> Option<RenameSpec> {
    Some(RenameSpec {
        kinds: vec![kinds::VIEW.into()],
        columns: false,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        replace: ReplaceStyle::CreateOrReplace,
        references: ReferenceStyle::Sql,
        fold: Fold::None,
        transactional: false,
        note: Some(NOTE.into()),
        ..Default::default()
    })
}

pub fn script(req: &RenameRequest) -> Result<SyncScript> {
    match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::VIEW => {
            let def = req.definition.as_deref().ok_or_else(|| Error::Query(format!("no se pudo leer la definición de la vista «{}»", object.name)))?;
            let renamed = rename_header(def, &crate::dialect(), Fold::None, &req.new_name)
                .ok_or_else(|| Error::Query(format!("no se reconoce el encabezado de la definición de la vista «{}»", object.name)))?;
            let create = format!("{};", plain_create(&renamed).trim().trim_end_matches(';').trim_end());
            let old = match object.schema() {
                Some(s) => format!("{}.{}", crate::quote(s), crate::quote(&object.name)),
                None => crate::quote(&object.name),
            };
            let statements = vec![create, format!("DROP VIEW {old};")];
            // `execute` cuts what it gets with the same dialect: a stored
            // text whose `;` sits outside what it reads as a string or a
            // comment would run what follows as a statement of its own.
            if statements.iter().any(|s| split_script(s, &crate::dialect()).len() > 1) {
                return Err(Error::Unsupported(format!(
                    "la definición de la vista «{}» se partiría en varias sentencias al ejecutar el cambio de nombre, así que DBine no la renombra: hacelo a mano",
                    object.name
                )));
            }
            Ok(SyncScript { statements, warnings: vec![] })
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE => {
            Err(Error::Unsupported("Drill no renombra tablas: son archivos o carpetas del origen, que se renombran fuera de Drill".into()))
        }
        RenameTarget::Object { .. } => Err(Error::Unsupported("Drill solo renombra vistas".into())),
        RenameTarget::Column { .. } => Err(Error::Unsupported("Drill no renombra columnas: salen de los archivos que lee".into())),
        RenameTarget::Index { .. } => Err(Error::Unsupported("Drill no tiene índices".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("Drill no tiene restricciones".into())),
        RenameTarget::Schema { .. } => Err(Error::Unsupported("Drill no renombra espacios de trabajo: se configuran en el plugin de almacenamiento".into())),
    }
}

/// `CREATE OR REPLACE VIEW` → `CREATE VIEW`: the new name must not
/// overwrite a view that already has it.
fn plain_create(def: &str) -> String {
    let lead = def.len() - def.trim_start().len();
    let words: Vec<(usize, &str)> = def[lead..].split_whitespace().take(3).map(|w| (w.as_ptr() as usize - def.as_ptr() as usize, w)).collect();
    match words.as_slice() {
        [(start, c), (_, o), (r_at, r)] if c.eq_ignore_ascii_case("create") && o.eq_ignore_ascii_case("or") && r.eq_ignore_ascii_case("replace") => {
            format!("{}CREATE{}", &def[..*start], &def[r_at + r.len()..])
        }
        _ => def.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: Some("dfs.tmp".into()), name: name.into() }
    }

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    #[test]
    fn spec_is_views_only() {
        let s = spec().unwrap();
        assert_eq!(s.kinds, ["view"]);
        assert!(!s.columns && !s.indexes && !s.constraints && !s.schemas && !s.transactional);
        assert_eq!(s.replace, ReplaceStyle::CreateOrReplace);
    }

    #[test]
    fn views_are_created_again() {
        let mut r = req(RenameTarget::Object { object: obj("view", "v"), parent: None }, "Ventas Netas");
        r.definition = Some("CREATE OR REPLACE VIEW `dfs.tmp`.`v` AS\nSELECT `id`\nFROM `dfs`.`tmp`.`t`;".into());
        let s = script(&r).unwrap();
        assert_eq!(s.statements, ["CREATE VIEW `dfs.tmp`.`Ventas Netas` AS\nSELECT `id`\nFROM `dfs`.`tmp`.`t`;", "DROP VIEW `dfs.tmp`.`v`;"]);
        r.definition = None;
        assert!(matches!(script(&r), Err(Error::Query(_))));
    }

    #[test]
    fn stored_text_that_would_split_is_refused() {
        let mut r = req(RenameTarget::Object { object: obj("view", "v"), parent: None }, "w");
        r.definition = Some("CREATE VIEW `dfs.tmp`.`v` AS SELECT 1 AS `x`; DROP TABLE `dfs`.`tmp`.`secreta`".into());
        assert!(matches!(script(&r), Err(Error::Unsupported(m)) if m.contains("«v»")));
        // Drill reads `//` as a comment: the `'` in it hides nothing.
        r.definition = Some("CREATE VIEW `dfs.tmp`.`v` AS SELECT 1 AS `x` // it's\n; DROP TABLE `dfs`.`tmp`.`secreta`".into());
        assert!(matches!(script(&r), Err(Error::Unsupported(_))));
        // A `;` inside a string or a comment stays in the view.
        r.definition = Some("CREATE VIEW `dfs.tmp`.`v` AS\nSELECT 'a;b' AS `x` -- c;d\n// e;f\nFROM `dfs`.`tmp`.`t`".into());
        let s = script(&r).unwrap();
        assert_eq!(s.statements, ["CREATE VIEW `dfs.tmp`.`w` AS\nSELECT 'a;b' AS `x` -- c;d\n// e;f\nFROM `dfs`.`tmp`.`t`;", "DROP VIEW `dfs.tmp`.`v`;"]);
        assert!(crate::dialect().slash_comments);
    }

    #[test]
    fn refused() {
        for t in [
            RenameTarget::Object { object: obj("table", "t"), parent: None },
            RenameTarget::Column { table: obj("table", "t"), column: "c".into() },
            RenameTarget::Index { table: obj("table", "t"), index: "r".into() },
            RenameTarget::Constraint { table: obj("table", "t"), constraint: "c".into() },
            RenameTarget::Schema { database: None, schema: "dfs.tmp".into() },
        ] {
            assert!(matches!(script(&req(t, "x")), Err(Error::Unsupported(_))));
        }
    }
}
