//! "Renombrar…" for classes (`ALTER CLASS … NAME`) and properties
//! (`ALTER PROPERTY … NAME`), with what OrientDB leaves behind fixed in the
//! same script:
//!
//! - Indexes keep the old class and field names (a class with indexes isn't
//!   renamed at all without `UNSAFE`): the target's are dropped first and
//!   created again on the new names, under their own names.
//! - Renaming a property copies each record's value to the new field but
//!   keeps the old one: an `UPDATE … REMOVE` moves it.
//! - Renaming an edge class needs `UNSAFE`, and vertices keep their
//!   `out_<old>` / `in_<old>` fields, so traversals stop finding the edges:
//!   an `UPDATE V` moves them to `out_<new>` / `in_<new>`.
//!
//! OrientDB runs each statement on its own (no transactional DDL).

use crate::ddl::{ident_index, index_tail, opt, truthy};
use crate::{ident, EDGE, VERTEX};
use dbine_driver::rename::{needs_quotes, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget};
use dbine_driver::{kinds, Error, IndexDef, Result, SyncScript};

/// Characters OrientDB refuses in a property name.
const BAD_FIELD_CHARS: &str = ":,; %@=.";

pub fn spec() -> RenameSpec {
    RenameSpec {
        kinds: vec![VERTEX.into(), EDGE.into(), kinds::TABLE.into()],
        columns: true,
        references: ReferenceStyle::None,
        note: Some(
            "OrientDB renombra la clase con ALTER CLASS … NAME y la propiedad con ALTER PROPERTY … NAME. Sus índices se borran antes y se vuelven \
             a crear después, porque el motor no los actualiza. Cada sentencia corre por separado: si una falla, lo anterior queda hecho. Lo que \
             nombre la clase o la propiedad en funciones o en la aplicación hay que cambiarlo a mano."
                .into(),
        ),
        ..Default::default()
    }
}

/// A name as the script writes it: between backticks when it isn't a
/// plain identifier or is a reserved word (`select`).
fn q(name: &str) -> String {
    if needs_quotes(name, Fold::None) {
        format!("`{}`", name.replace('`', "\\`"))
    } else {
        name.to_string()
    }
}

/// An index to drop before the rename and create after it.
struct Index {
    /// Its name as `DROP INDEX` takes it.
    name: String,
    create: String,
}

/// `CREATE INDEX` for `ix` on `class`, with `old` among its fields
/// written as `new`.
fn create_index(ix: &IndexDef, class: &str, rename: Option<(&str, &str)>) -> String {
    let ty = ix.kind.clone().filter(|k| !k.is_empty()).unwrap_or_else(|| if ix.unique { "UNIQUE".into() } else { "NOTUNIQUE".into() });
    let fields: Vec<String> = ix
        .columns
        .iter()
        .map(|c| {
            let (f, collate) = split_collate(c);
            let f = match rename {
                Some((old, new)) if f == old => new,
                _ => f,
            };
            format!("{}{}", q(f), collate.map(|c| format!(" COLLATE {c}")).unwrap_or_default())
        })
        .collect();
    format!("CREATE INDEX {} ON {} ({}) {ty}{};", ident_index(&ix.name), q(class), fields.join(", "), index_tail(ix))
}

/// `name COLLATE ci` → (`name`, `ci`).
fn split_collate(c: &str) -> (&str, Option<&str>) {
    match c.split_once(" COLLATE ") {
        Some((f, collate)) => (f.trim(), Some(collate.trim())),
        None => (c.trim(), None),
    }
}

/// The `CREATE INDEX` lines of a class definition (as `definition` writes
/// it: `CREATE INDEX name ON Class (…) TYPE;`), moved to the new class.
fn indexes_in(def: &str, old: &str, new: &str) -> Vec<Index> {
    let on = format!(" ON {} (", ident(old));
    def.lines()
        .map(str::trim)
        .filter(|l| l.starts_with("CREATE INDEX "))
        .filter_map(|l| {
            let rest = &l["CREATE INDEX ".len()..];
            let name = if rest.starts_with('`') {
                let mut end = None;
                let mut escaped = false;
                for (i, c) in rest.char_indices().skip(1) {
                    match c {
                        '\\' if !escaped => escaped = true,
                        '`' if !escaped => {
                            end = Some(i);
                            break;
                        }
                        _ => escaped = false,
                    }
                }
                &rest[..=end?]
            } else {
                rest.split_whitespace().next()?
            };
            let at = l.find(&on)?;
            let create = format!("{} ON {} ({}", &l[..at], q(new), &l[at + on.len()..]);
            let create = if create.ends_with(';') { create } else { format!("{create};") };
            Some(Index { name: name.to_string(), create })
        })
        .collect()
}

pub fn script(req: &RenameRequest) -> Result<SyncScript> {
    match &req.target {
        RenameTarget::Object { object, .. } if [VERTEX, EDGE, kinds::TABLE].contains(&object.kind.as_str()) => class_script(&object.name, object.kind == EDGE, req),
        RenameTarget::Column { column, .. } => property_script(column, req),
        _ => Err(Error::Unsupported("en OrientDB solo se renombran clases y propiedades".into())),
    }
}

fn class_script(old: &str, edge: bool, req: &RenameRequest) -> Result<SyncScript> {
    let new = req.new_name.as_str();
    if old == "V" || old == "E" {
        return Err(Error::Unsupported("las clases base V y E no se renombran".into()));
    }
    let indexes: Option<Vec<Index>> = match (req.table.as_ref().filter(|t| t.name == old), req.definition.as_deref()) {
        (Some(t), _) => Some(t.indexes.iter().map(|ix| Index { name: ident_index(&ix.name), create: create_index(ix, new, None) }).collect()),
        (None, Some(def)) => Some(indexes_in(def, old, new)),
        (None, None) => None,
    };
    // `UNSAFE` skips the engine's check for indexes: without knowing them,
    // they'd stay on the old name.
    if edge && indexes.is_none() {
        return Err(Error::Unsupported("para renombrar una clase de aristas hace falta leer su definición (sus índices)".into()));
    }
    let mut warnings = Vec::new();
    let mut statements: Vec<String> = Vec::new();
    let ixs = indexes.as_deref().unwrap_or_default();
    statements.extend(ixs.iter().map(|ix| format!("DROP INDEX {};", ix.name)));
    statements.push(format!("ALTER CLASS {} NAME {}{};", q(old), q(new), if edge { " UNSAFE" } else { "" }));
    if edge {
        for dir in ["out", "in"] {
            let (o, n) = (q(&format!("{dir}_{old}")), q(&format!("{dir}_{new}")));
            statements.push(format!("UPDATE V SET {n} = {o} REMOVE {o} WHERE {o} IS DEFINED;"));
        }
        warnings.push(format!(
            "Se reescriben los vértices unidos por aristas de «{old}»: sus campos out_{old} / in_{old} pasan a out_{new} / in_{new}, para que los recorridos sigan encontrándolas."
        ));
    }
    statements.extend(ixs.iter().map(|ix| ix.create.clone()));
    if !ixs.is_empty() {
        warnings.push(index_warning(ixs));
    }
    if indexes.is_none() {
        warnings.push("Si la clase tiene índices, OrientDB rechaza el cambio: borralos antes y volvé a crearlos después.".into());
    }
    Ok(SyncScript { statements, warnings })
}

fn index_warning(ixs: &[Index]) -> String {
    let names: Vec<String> = ixs.iter().map(|ix| format!("«{}»", ix.name.trim_matches('`'))).collect();
    format!("Se borran y se vuelven a crear los índices {}: OrientDB no los actualiza solo y se reconstruyen con todos los registros.", names.join(", "))
}

fn property_script(column: &str, req: &RenameRequest) -> Result<SyncScript> {
    let new = req.new_name.as_str();
    let t = req.table.as_ref().ok_or_else(|| Error::Unsupported("para renombrar una propiedad hace falta leer la estructura de la clase".into()))?;
    if column.starts_with('@') {
        return Err(Error::Unsupported("los atributos de registro (@rid, @class…) no se renombran".into()));
    }
    if t.kind == EDGE && (column == "out" || column == "in") {
        return Err(Error::Unsupported("«out» e «in» de una arista no se renombran: son sus extremos".into()));
    }
    if let Some(c) = new.chars().find(|c| BAD_FIELD_CHARS.contains(*c)) {
        return Err(Error::Unsupported(format!("OrientDB no admite «{c}» en el nombre de una propiedad")));
    }
    let col = t.columns.iter().find(|c| c.name == column).ok_or_else(|| Error::Query(format!("la clase {} no tiene la propiedad {column}", t.name)))?;
    let class = q(&t.name);
    let (o, n) = (q(column), q(new));
    let move_values = format!("UPDATE {class} SET {n} = {o} REMOVE {o} WHERE {o} IS DEFINED;");
    if truthy(opt(col, "inferred")) {
        return Ok(SyncScript {
            statements: vec![move_values],
            warnings: vec![format!("«{column}» no está declarada en el esquema: se mueve el valor en cada registro de {} (y de sus subclases) que la tiene.", t.name)],
        });
    }
    let ixs: Vec<Index> = t
        .indexes
        .iter()
        .filter(|ix| ix.columns.iter().any(|c| split_collate(c).0 == column))
        .map(|ix| Index { name: ident_index(&ix.name), create: create_index(ix, &t.name, Some((column, new))) })
        .collect();
    let mut statements: Vec<String> = ixs.iter().map(|ix| format!("DROP INDEX {};", ix.name)).collect();
    statements.push(format!("ALTER PROPERTY {class}.{o} NAME {n};"));
    statements.push(move_values);
    statements.extend(ixs.iter().map(|ix| ix.create.clone()));
    let mut warnings = vec![format!("Se reescriben los registros de {} (y de sus subclases): el valor pasa de «{column}» a «{new}».", t.name)];
    if !ixs.is_empty() {
        warnings.push(index_warning(&ixs));
    }
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, DdlParts, ObjectRef, TableSchema};

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: None, name: name.into() }
    }

    fn class_req(kind: &str, old: &str, new: &str, table: Option<TableSchema>, definition: Option<String>) -> RenameRequest {
        RenameRequest { target: RenameTarget::Object { object: obj(kind, old), parent: None }, new_name: new.into(), table, definition }
    }

    fn col_req(t: &TableSchema, column: &str, new: &str) -> RenameRequest {
        RenameRequest {
            target: RenameTarget::Column { table: obj(&t.kind, &t.name), column: column.into() },
            new_name: new.into(),
            table: Some(t.clone()),
            definition: None,
        }
    }

    fn person() -> TableSchema {
        let mut inferred = ColumnDef { name: "apodo".into(), data_type: "STRING".into(), ..Default::default() };
        inferred.options.insert("inferred".into(), "true".into());
        TableSchema {
            kind: VERTEX.into(),
            name: "T".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "INTEGER".into(), ..Default::default() },
                ColumnDef { name: "pepe".into(), data_type: "STRING".into(), ..Default::default() },
                inferred,
            ],
            indexes: vec![
                IndexDef { name: "T.id".into(), columns: vec!["id".into()], unique: true, kind: Some("UNIQUE".into()), ..Default::default() },
                IndexDef { name: "T.pepe".into(), columns: vec!["pepe COLLATE ci".into(), "id".into()], kind: Some("NOTUNIQUE".into()), ..Default::default() },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn vertex_class_moves_its_indexes() {
        let s = script(&class_req(VERTEX, "T", "Persona", Some(person()), None)).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX T.id;",
                "DROP INDEX T.pepe;",
                "ALTER CLASS T NAME Persona;",
                "CREATE INDEX T.id ON Persona (id) UNIQUE;",
                "CREATE INDEX T.pepe ON Persona (pepe COLLATE ci, id) NOTUNIQUE;",
            ]
        );
        assert_eq!(s.warnings.len(), 1);
    }

    #[test]
    fn class_indexes_from_the_definition() {
        // What `definition` returns for the class.
        let def = crate::ddl::table_ddl(&person(), DdlParts { create: true, indexes: true, ..Default::default() }).unwrap();
        let from_def = script(&class_req(VERTEX, "T", "Mi clase", None, Some(def))).unwrap();
        assert_eq!(
            from_def.statements,
            vec![
                "DROP INDEX T.id;",
                "DROP INDEX T.pepe;",
                "ALTER CLASS T NAME `Mi clase`;",
                "CREATE INDEX T.id ON `Mi clase` (id) UNIQUE;",
                "CREATE INDEX T.pepe ON `Mi clase` (pepe COLLATE ci, id) NOTUNIQUE;",
            ]
        );
        let quoted = indexes_in("CREATE INDEX `ix a` ON `my class` (a) UNIQUE;", "my class", "Otra");
        assert_eq!(quoted[0].name, "`ix a`");
        assert_eq!(quoted[0].create, "CREATE INDEX `ix a` ON Otra (a) UNIQUE;");
    }

    #[test]
    fn edge_class_is_unsafe_and_moves_vertex_fields() {
        let s = script(&class_req(EDGE, "Knows", "Conoce", None, Some("CREATE CLASS Knows EXTENDS E;".into()))).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER CLASS Knows NAME Conoce UNSAFE;",
                "UPDATE V SET out_Conoce = out_Knows REMOVE out_Knows WHERE out_Knows IS DEFINED;",
                "UPDATE V SET in_Conoce = in_Knows REMOVE in_Knows WHERE in_Knows IS DEFINED;",
            ]
        );
        let spaced = script(&class_req(EDGE, "K", "a b", None, Some(String::new()))).unwrap();
        assert_eq!(spaced.statements[1], "UPDATE V SET `out_a b` = out_K REMOVE out_K WHERE out_K IS DEFINED;");
        // Without its indexes, UNSAFE would leave them behind.
        assert!(matches!(script(&class_req(EDGE, "K", "L", None, None)), Err(Error::Unsupported(_))));
    }

    #[test]
    fn document_class_without_index_info_warns() {
        let s = script(&class_req(kinds::TABLE, "Doc", "select", None, None)).unwrap();
        assert_eq!(s.statements, vec!["ALTER CLASS Doc NAME `select`;"]);
        assert!(s.warnings[0].contains("rechaza"));
        assert!(matches!(script(&class_req(VERTEX, "V", "W", None, None)), Err(Error::Unsupported(_))));
    }

    #[test]
    fn declared_property_with_its_indexes() {
        let s = script(&col_req(&person(), "pepe", "Juan")).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX T.pepe;",
                "ALTER PROPERTY T.pepe NAME Juan;",
                "UPDATE T SET Juan = pepe REMOVE pepe WHERE pepe IS DEFINED;",
                "CREATE INDEX T.pepe ON T (Juan COLLATE ci, id) NOTUNIQUE;",
            ]
        );
        assert_eq!(s.warnings.len(), 2);
        let reserved = script(&col_req(&person(), "id", "select")).unwrap();
        assert_eq!(reserved.statements[2], "ALTER PROPERTY T.id NAME `select`;");
        assert_eq!(reserved.statements[5], "CREATE INDEX T.pepe ON T (pepe COLLATE ci, `select`) NOTUNIQUE;");
    }

    #[test]
    fn inferred_field_is_moved() {
        let s = script(&col_req(&person(), "apodo", "alias")).unwrap();
        assert_eq!(s.statements, vec!["UPDATE T SET alias = apodo REMOVE apodo WHERE apodo IS DEFINED;"]);
    }

    #[test]
    fn refused() {
        let p = person();
        assert!(matches!(script(&col_req(&p, "pepe", "a b")), Err(Error::Unsupported(_))));
        assert!(matches!(script(&col_req(&p, "@rid", "x")), Err(Error::Unsupported(_))));
        assert!(matches!(script(&col_req(&p, "nada", "x")), Err(Error::Query(_))));
        let edge = TableSchema { kind: EDGE.into(), name: "K".into(), columns: vec![ColumnDef { name: "out".into(), ..Default::default() }], ..Default::default() };
        assert!(matches!(script(&col_req(&edge, "out", "desde")), Err(Error::Unsupported(_))));
        let ix = RenameRequest {
            target: RenameTarget::Index { table: obj(VERTEX, "T"), index: "T.id".into() },
            new_name: "x".into(),
            table: Some(p.clone()),
            definition: None,
        };
        assert!(matches!(script(&ix), Err(Error::Unsupported(_))));
        assert!(matches!(script(&class_req(kinds::FUNCTION, "f", "g", None, None)), Err(Error::Unsupported(_))));
        let s = spec();
        assert!(s.columns && !s.indexes && !s.constraints && !s.schemas && !s.transactional);
    }
}
