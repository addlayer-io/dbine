//! "Renombrar…" in CQL: Cassandra and ScyllaDB rename only primary key
//! columns (`ALTER TABLE ks.t RENAME a TO b`). Regular columns, tables,
//! keyspaces, types and functions can't be renamed. The server also refuses
//! a key column a secondary index or a materialized view uses: the indexes
//! come with the table and are checked here; the views are left to the
//! server. Amazon Keyspaces renames nothing.

use crate::cql::{ident, qualified};
use crate::ddl::keys;
use crate::Flavor;
use dbine_driver::rename::{quote_new, Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle};
use dbine_driver::{kinds, Error, Result, ScriptDialect, SyncScript, TableSchema};

pub(crate) const NOTE: &str = "Cassandra solo renombra columnas de la clave primaria (de partición o de clustering). \
El servidor rechaza el cambio si la columna tiene un índice secundario o la usa una vista materializada.";
pub(crate) const VIEWS: &str = "Si una vista materializada usa la columna, el servidor rechaza el cambio: hay que borrar la vista, renombrar y volver a crearla.";
const NOTHING: &str = "Amazon Keyspaces no renombra columnas, tablas ni keyspaces.";

/// What the flavor renames; `None`: nothing (Amazon Keyspaces).
pub(crate) fn spec(flavor: Flavor) -> Option<RenameSpec> {
    if flavor == Flavor::Keyspaces {
        return None;
    }
    Some(RenameSpec {
        kinds: Vec::new(),
        columns: true,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        references: ReferenceStyle::None,
        fold: Fold::Lower,
        transactional: false,
        note: Some(NOTE.into()),
        ..Default::default()
    })
}

/// The new name as CQL writes it: bare only when it's lower case and not a
/// CQL keyword.
fn written(name: &str, dialect: &ScriptDialect) -> String {
    let cql = ident(name);
    if cql != name {
        cql
    } else {
        quote_new(name, dialect, Fold::Lower, false)
    }
}

/// The column a secondary index's target names: `pepe`, `"Pepe"`,
/// `values(pepe)`, `keys(m)`, `entries(m)`, `full(l)`.
fn index_column(target: &str) -> String {
    let t = target.trim();
    let inner = match (t.find('('), t.ends_with(')')) {
        (Some(open), true) => &t[open + 1..t.len() - 1],
        _ => t,
    };
    let inner = inner.trim();
    match inner.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(q) => q.replace("\"\"", "\""),
        None => inner.to_lowercase(),
    }
}

/// Whether `column` is part of the primary key.
fn in_key(table: &TableSchema, column: &str) -> bool {
    if let Ok((pk, ck)) = keys(table) {
        return pk.iter().chain(ck.iter()).any(|c| c.name == column);
    }
    table.primary_key.as_ref().is_some_and(|k| k.columns.iter().any(|c| c == column))
}

/// The statements that rename the target.
pub(crate) fn script(flavor: Flavor, dialect: &ScriptDialect, req: &RenameRequest) -> Result<SyncScript> {
    if flavor == Flavor::Keyspaces {
        return Err(Error::Unsupported(NOTHING.into()));
    }
    match &req.target {
        RenameTarget::Column { table, column } => {
            if let Some(t) = &req.table {
                if t.columns.iter().any(|c| &c.name == column) && !in_key(t, column) {
                    return Err(Error::Unsupported(format!(
                        "Cassandra solo renombra columnas de la clave primaria; «{column}» es una columna común. Para cambiarle el nombre hay que agregar una columna nueva, copiar los datos y borrar la anterior."
                    )));
                }
                if let Some(ix) = t.indexes.iter().find(|i| i.columns.iter().any(|c| index_column(c) == *column)) {
                    return Err(Error::Unsupported(format!(
                        "La columna «{column}» tiene el índice secundario «{}» y Cassandra no la renombra así: borrá el índice, renombrá la columna y volvé a crearlo.",
                        ix.name
                    )));
                }
            }
            let statement = format!("ALTER TABLE {} RENAME {} TO {};", qualified(table.schema(), &table.name), ident(column), written(&req.new_name, dialect));
            Ok(SyncScript { statements: vec![statement], warnings: vec![VIEWS.into()] })
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE => {
            Err(Error::Unsupported("Cassandra no renombra tablas: hay que crear una tabla con el nombre nuevo y copiar los datos.".into()))
        }
        RenameTarget::Object { .. } => Err(Error::Unsupported("Cassandra no renombra vistas materializadas, tipos ni funciones: hay que crearlos con el nombre nuevo y borrar los anteriores.".into())),
        RenameTarget::Index { .. } => Err(Error::Unsupported("Cassandra no renombra índices: hay que borrarlo y crearlo con el nombre nuevo.".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("Cassandra no tiene restricciones con nombre.".into())),
        RenameTarget::Schema { .. } => Err(Error::Unsupported("Cassandra no renombra keyspaces.".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, IndexDef, KeyDef, ObjectRef};

    fn dialect() -> ScriptDialect {
        ScriptDialect { dollar_quotes: true, backtick_idents: false, compound_blocks: false, ..ScriptDialect::generic() }
    }

    fn table() -> TableSchema {
        let col = |n: &str| ColumnDef { name: n.into(), data_type: "int".into(), ..Default::default() };
        TableSchema {
            schema: Some("ks".into()),
            name: "t".into(),
            columns: vec![col("id"), col("Ck"), col("pepe"), col("tagged")],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into(), "Ck".into(), "tagged".into()] }),
            indexes: vec![IndexDef { name: "t_tagged".into(), columns: vec!["values(tagged)".into()], ..Default::default() }],
            ..Default::default()
        }
    }

    fn column(c: &str, new: &str) -> RenameRequest {
        RenameRequest {
            target: RenameTarget::Column { table: ObjectRef { kind: kinds::TABLE.into(), schema: Some("ks".into()), name: "t".into() }, column: c.into() },
            new_name: new.into(),
            table: Some(table()),
            definition: None,
        }
    }

    fn run(req: &RenameRequest) -> Result<SyncScript> {
        script(Flavor::Cassandra, &dialect(), req)
    }

    #[test]
    fn key_columns_are_renamed_with_quotes_where_needed() {
        assert_eq!(run(&column("id", "user_id")).unwrap().statements, vec!["ALTER TABLE ks.t RENAME id TO user_id;"]);
        // Mixed case, both ways.
        assert_eq!(run(&column("Ck", "Fecha")).unwrap().statements, vec!["ALTER TABLE ks.t RENAME \"Ck\" TO \"Fecha\";"]);
        // A CQL keyword.
        assert_eq!(run(&column("id", "token")).unwrap().statements[0], "ALTER TABLE ks.t RENAME id TO \"token\";");
        assert_eq!(run(&column("id", "nuevo")).unwrap().warnings, vec![VIEWS.to_string()]);
    }

    #[test]
    fn regular_and_indexed_columns_are_refused() {
        let e = run(&column("pepe", "juan")).unwrap_err().to_string();
        assert!(e.contains("solo renombra columnas de la clave primaria") && e.contains("«pepe»"), "{e}");
        let e = run(&column("tagged", "etiquetas")).unwrap_err().to_string();
        assert!(e.contains("t_tagged"), "{e}");
    }

    #[test]
    fn index_targets() {
        assert_eq!(index_column("pepe"), "pepe");
        assert_eq!(index_column("\"Pepe\""), "Pepe");
        assert_eq!(index_column("keys(m)"), "m");
        assert_eq!(index_column("full(\"L\")"), "L");
    }

    #[test]
    fn everything_else_is_refused() {
        let t = ObjectRef { kind: kinds::TABLE.into(), schema: Some("ks".into()), name: "t".into() };
        let req = |target| RenameRequest { target, new_name: "x".into(), table: None, definition: None };
        let msg = |target| run(&req(target)).unwrap_err().to_string();
        assert!(msg(RenameTarget::Object { object: t.clone(), parent: None }).contains("no renombra tablas"));
        assert!(msg(RenameTarget::Object { object: ObjectRef { kind: kinds::MATERIALIZED_VIEW.into(), ..t.clone() }, parent: None }).contains("vistas materializadas"));
        assert!(msg(RenameTarget::Index { table: t.clone(), index: "i".into() }).contains("no renombra índices"));
        assert!(msg(RenameTarget::Constraint { table: t.clone(), constraint: "c".into() }).contains("restricciones"));
        assert!(msg(RenameTarget::Schema { database: None, schema: "ks".into() }).contains("keyspaces"));
        assert!(spec(Flavor::Keyspaces).is_none());
        assert!(script(Flavor::Keyspaces, &dialect(), &column("id", "x")).unwrap_err().to_string().contains("Amazon Keyspaces"));
        let s = spec(Flavor::Scylla).unwrap();
        assert!(s.columns && s.kinds.is_empty() && !s.indexes && !s.schemas);
    }
}
