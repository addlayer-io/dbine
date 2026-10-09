//! "Renombrar…": collections, views and fields.
//!
//! - A collection is renamed with `renameCollection` (documents, indexes
//!   and options go with it). MongoDB, FerretDB and Amazon DocumentDB all
//!   have it.
//! - A view can't be renamed (`cannot rename view`): it is dropped and
//!   created again with the new name and the same pipeline. It keeps no
//!   documents, so nothing is lost. Only MongoDB has views: FerretDB
//!   accepts `createView` and makes a plain collection, DocumentDB has none.
//! - A field is renamed with `updateMany` and `$rename` on every document
//!   that has it. The indexes that use it are dropped first (a unique one
//!   would refuse the documents left without the old field) and created
//!   again with the new field; a validator that names it is changed first
//!   with `collMod`, so the renamed documents pass it.
//!
//! Views that read a renamed collection or view name it in their pipeline:
//! the app rewrites them (`ReferenceStyle::Pipeline`) and creates them
//! again around this script. Nothing here is transactional.
//!
//! A database has no rename ([`database_script`]): each collection moves
//! with `renameCollection` to the new database (as an admin command, which
//! takes namespaces and copies across databases, indexes included), each
//! view is created again there and dropped in the old one, and the old
//! database's empty `system.views` goes last, so nothing keeps it alive.

use crate::ddl::{index_parts, q, validator_check};
use crate::shell::{parse_units, Item};
use crate::Flavor;
use dbine_driver::rename::{DatabaseObject, Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle, ReplaceStyle};
use dbine_driver::{kinds, Error, IndexDef, ObjectRef, Result, SyncScript, TableSchema};
use mongodb::bson::{Bson, Document};
use serde_json::{Map, Value};

pub(crate) const NOTE_MONGO: &str = "MongoDB no tiene DDL transaccional: cada paso se confirma solo. Las colecciones se renombran con renameCollection y conservan sus documentos e índices; las vistas no se pueden renombrar, así que se borran y se crean con el nombre nuevo y el mismo pipeline (no guardan datos). Las vistas que las usan se reescriben (viewOn, $lookup, $graphLookup, $unionWith, $out, $merge) y se vuelven a crear. Los roles con privilegios sobre el nombre viejo no se actualizan, y las colecciones de series temporales no se pueden renombrar.";
pub(crate) const NOTE_NO_VIEWS: &str = "Este motor no tiene DDL transaccional: cada paso se confirma solo. Las colecciones se renombran con renameCollection y conservan sus documentos e índices. Los roles con privilegios sobre el nombre viejo no se actualizan.";

/// What the variant renames.
pub(crate) fn spec(flavor: Flavor) -> RenameSpec {
    let views = flavor == Flavor::Mongo;
    let mut kinds = vec![kinds::COLLECTION.to_string()];
    if views {
        kinds.push(kinds::VIEW.to_string());
    }
    RenameSpec {
        kinds,
        columns: true,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        // A view can't be created over an existing one: dropped and created.
        replace: ReplaceStyle::DropCreate,
        references: ReferenceStyle::Pipeline,
        fold: Fold::None,
        transactional: false,
        note: Some(if views { NOTE_MONGO } else { NOTE_NO_VIEWS }.to_string()),
        // Roles grant on a collection's name, not on the object: a view
        // created again under the same name keeps them.
        grants_on_objects: false,
        // Collections move across databases with `renameCollection`, which
        // FerretDB and DocumentDB don't do (see `database_script`).
        databases: views,
        database_from: views.then(|| "admin".to_string()),
        database_note: views.then(|| DATABASE_NOTE.to_string()),
        database_moves: views,
        ..Default::default()
    }
}

pub(crate) const DATABASE_NOTE: &str = "MongoDB no puede renombrar una base de datos: DBine mueve cada colección a la base nueva con renameCollection (documentos, índices y opciones), crea las vistas en la base nueva y las borra de la anterior. No es atómico: si se detiene a mitad de camino, las colecciones ya movidas quedan en la base nueva y el resto en la anterior. Mover una colección a otra base copia y reescribe sus datos, así que en colecciones grandes puede tardar bastante. Los usuarios y roles definidos en la base anterior no se mueven, y los privilegios que nombran la base anterior no se actualizan.";

const RESERVED_DATABASES: [&str; 3] = ["admin", "local", "config"];

fn check_database_name(name: &str) -> Result<()> {
    if RESERVED_DATABASES.iter().any(|r| r.eq_ignore_ascii_case(name)) {
        return Err(Error::Unsupported(format!("«{name}» es una base del sistema: DBine no la renombra ni la usa como destino")));
    }
    if name.is_empty() || name.len() >= 64 || name.contains(['/', '\\', '.', ' ', '"', '$', '*', '<', '>', ':', '|', '?', '\0']) {
        return Err(Error::Query(format!("«{name}» no es un nombre de base válido en MongoDB")));
    }
    Ok(())
}

/// The statements that move `database`'s collections and views to
/// `new_name` (run from `admin`, one by one):
///
/// ```text
/// db.adminCommand({"renameCollection": "old.c", "to": "new.c"})
/// use new
/// db.createView("v", "c", [...])
/// use old
/// db.getCollection("v").drop()
/// db.getCollection("system.views").drop()
/// ```
pub(crate) fn database_script(flavor: Flavor, database: &str, new_name: &str, objects: &[DatabaseObject]) -> Result<SyncScript> {
    if flavor != Flavor::Mongo {
        return Err(Error::Unsupported("este motor no mueve colecciones entre bases (renameCollection entre bases), así que no renombra bases de datos".into()));
    }
    check_database_name(database)?;
    check_database_name(new_name)?;
    if database == new_name {
        return Err(Error::Query("el nombre nuevo es igual al actual".into()));
    }
    let mut statements = Vec::new();
    let mut views = Vec::new();
    let mut skipped = Vec::new();
    for o in objects {
        if o.name.starts_with("system.") {
            skipped.push(o.name.clone());
            continue;
        }
        match o.kind.as_str() {
            kinds::COLLECTION => {
                let mut cmd = Document::new();
                cmd.insert("renameCollection", format!("{database}.{}", o.name));
                cmd.insert("to", format!("{new_name}.{}", o.name));
                statements.push(format!("db.adminCommand({})", Bson::Document(cmd).into_relaxed_extjson()));
            }
            kinds::VIEW => {
                let obj = ObjectRef { kind: o.kind.clone(), schema: None, name: o.name.clone() };
                views.push((o.name.as_str(), view_options(&obj, o.definition.as_deref())?));
            }
            k => return Err(Error::Unsupported(format!("«{}» es de tipo «{k}»: MongoDB solo mueve colecciones y vistas", o.name))),
        }
    }
    if !views.is_empty() {
        statements.push(format!("use {new_name}"));
        statements.extend(views.iter().map(|(name, options)| crate::ddl::view_definition(name, Some(options))));
    }
    statements.push(format!("use {database}"));
    statements.extend(views.iter().map(|(name, _)| format!("{}.drop()", coll(name))));
    // What's left of the views' catalog: without it the old database is gone.
    statements.push(format!("{}.drop()", coll("system.views")));
    let mut warnings = vec![
        format!("No es atómico: si se detiene a mitad de camino, las colecciones ya movidas quedan en «{new_name}» y el resto en «{database}»."),
        "Mover una colección a otra base copia y reescribe sus datos: en colecciones grandes puede tardar bastante.".to_string(),
        format!("Los usuarios y roles definidos en «{database}» no se mueven."),
    ];
    if !skipped.is_empty() {
        warnings.push(format!("Las colecciones del sistema no se mueven y quedan en «{database}»: {}.", skipped.join(", ")));
    }
    Ok(SyncScript { statements, warnings })
}

/// The statements that rename the target.
pub(crate) fn script(flavor: Flavor, req: &RenameRequest) -> Result<SyncScript> {
    let new = req.new_name.as_str();
    match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::COLLECTION => {
            check_collection_name(new)?;
            Ok(SyncScript { statements: vec![format!("{}.renameCollection({})", coll(&object.name), q(new))], warnings: Vec::new() })
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::VIEW && flavor == Flavor::Mongo => {
            check_collection_name(new)?;
            view(object, req.definition.as_deref(), new)
        }
        RenameTarget::Column { table, column } => field(table, column, new, req.table.as_ref()),
        RenameTarget::Object { .. } => Err(Error::Unsupported("desde DBine este motor solo renombra colecciones, vistas y campos".into())),
        RenameTarget::Index { .. } => Err(Error::Unsupported("MongoDB no renombra índices: hay que borrarlo y crearlo con el nombre nuevo".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("MongoDB no tiene restricciones con nombre".into())),
        RenameTarget::Schema { .. } => Err(Error::Unsupported("MongoDB no renombra bases de datos".into())),
    }
}

fn coll(name: &str) -> String {
    format!("db.getCollection({})", q(name))
}

fn check_collection_name(name: &str) -> Result<()> {
    if name.contains(['$', '\0']) || name.starts_with("system.") {
        return Err(Error::Query(format!("«{name}» no es un nombre de colección válido: no puede tener $ ni empezar con «system.»")));
    }
    Ok(())
}

/// A view: dropped and created with the new name, from its definition
/// (`db.createView(name, source, pipeline[, options])`).
fn view(object: &ObjectRef, definition: Option<&str>, new: &str) -> Result<SyncScript> {
    let options = view_options(object, definition)?;
    Ok(SyncScript {
        statements: vec![format!("{}.drop()", coll(&object.name)), crate::ddl::view_definition(new, Some(&options))],
        warnings: Vec::new(),
    })
}

/// A view's `listCollections` options (source, pipeline, collation) read
/// back from its definition.
fn view_options(object: &ObjectRef, definition: Option<&str>) -> Result<Document> {
    let unreadable = || Error::Query(format!("no se pudo leer la definición de la vista «{}»", object.name));
    let def = definition.ok_or_else(unreadable)?;
    let units = parse_units(def).map_err(|_| unreadable())?;
    let Some(Item::Run(stmt)) = units.into_iter().next().map(|u| u.item) else { return Err(unreadable()) };
    let source = stmt.cmd.get_str("viewOn").map_err(|_| unreadable())?;
    let mut options = Document::new();
    options.insert("viewOn", source);
    options.insert("pipeline", stmt.cmd.get("pipeline").cloned().unwrap_or(Bson::Array(Vec::new())));
    if let Some(c) = stmt.cmd.get("collation") {
        options.insert("collation", c.clone());
    }
    Ok(options)
}

/// `path` with the field `old` (or a path inside it) renamed to `new`.
fn rename_path(path: &str, old: &str, new: &str) -> Option<String> {
    if path == old {
        return Some(new.to_string());
    }
    path.strip_prefix(old).filter(|rest| rest.starts_with('.')).map(|rest| format!("{new}{rest}"))
}

/// A query document (a validator, a partial index filter): field keys at
/// query level (through `$and` / `$or` / `$nor`), `$jsonSchema`'s top-level
/// `properties` and `required`, and `"$field"` paths in expressions.
fn rename_query(v: &mut Value, old: &str, new: &str) -> bool {
    let Value::Object(m) = v else { return false };
    let mut changed = false;
    let mut out = Map::new();
    for (k, mut val) in std::mem::take(m) {
        let key = if k == "$jsonSchema" {
            changed |= rename_schema(&mut val, old, new);
            k
        } else if matches!(k.as_str(), "$and" | "$or" | "$nor") {
            if let Value::Array(items) = &mut val {
                for item in items {
                    changed |= rename_query(item, old, new);
                }
            }
            k
        } else if k.starts_with('$') {
            changed |= rename_expr(&mut val, old, new);
            k
        } else {
            match rename_path(&k, old, new) {
                Some(n) => {
                    changed = true;
                    n
                }
                None => k,
            }
        };
        out.insert(key, val);
    }
    *m = out;
    changed
}

fn rename_schema(v: &mut Value, old: &str, new: &str) -> bool {
    let Value::Object(m) = v else { return false };
    let mut changed = false;
    if let Some(Value::Object(props)) = m.get_mut("properties") {
        changed |= rename_keys(props, old, new);
    }
    if let Some(Value::Array(req)) = m.get_mut("required") {
        for r in req {
            if r.as_str() == Some(old) {
                *r = Value::String(new.to_string());
                changed = true;
            }
        }
    }
    changed
}

/// `"$old"` and `"$old.x"` anywhere in an aggregation expression.
fn rename_expr(v: &mut Value, old: &str, new: &str) -> bool {
    match v {
        Value::String(s) => match s.strip_prefix('$').filter(|p| !p.starts_with('$')).and_then(|p| rename_path(p, old, new)) {
            Some(n) => {
                *s = format!("${n}");
                true
            }
            None => false,
        },
        Value::Array(a) => a.iter_mut().fold(false, |c, x| rename_expr(x, old, new) | c),
        Value::Object(m) => m.values_mut().fold(false, |c, x| rename_expr(x, old, new) | c),
        _ => false,
    }
}

/// The keys of `m` that are the field (or inside it), renamed in place.
fn rename_keys(m: &mut Map<String, Value>, old: &str, new: &str) -> bool {
    let mut changed = false;
    let mut out = Map::new();
    for (k, val) in std::mem::take(m) {
        let key = rename_path(&k, old, new).inspect(|_| changed = true).unwrap_or(k);
        out.insert(key, val);
    }
    *m = out;
    changed
}

/// `ix` with the field renamed: `None` when it doesn't use it.
fn renamed_index(ix: &IndexDef, old: &str, new: &str) -> Option<IndexDef> {
    let mut out = ix.clone();
    let mut changed = false;
    for c in &mut out.columns {
        let (path, dir) = match c.rsplit_once(':') {
            Some((p, d)) => (p, Some(d)),
            None => (c.as_str(), None),
        };
        if let Some(n) = rename_path(path.trim(), old, new) {
            *c = match dir {
                Some(d) => format!("{n}:{d}"),
                None => n,
            };
            changed = true;
        }
    }
    let json = |s: &str| serde_json::from_str::<Value>(s).or_else(|_| crate::ddl::relaxed(s, "índice").map_err(|_| ())).ok();
    if let Some(mut f) = out.filter.as_deref().and_then(json) {
        if rename_query(&mut f, old, new) {
            out.filter = Some(f.to_string());
            changed = true;
        }
    }
    for k in ["weights", "wildcardProjection"] {
        if let Some(Value::Object(mut m)) = out.options.get(k).and_then(|s| json(s)) {
            if rename_keys(&mut m, old, new) {
                out.options.insert(k.to_string(), Value::Object(m).to_string());
                changed = true;
            }
        }
    }
    changed.then_some(out)
}

fn create_index(collection: &str, ix: &IndexDef) -> Result<String> {
    let (key, o) = index_parts(ix)?;
    Ok(format!("{}.createIndex({}, {})", coll(collection), Value::Object(key), Value::Object(o)))
}

/// A field, renamed in every document that has it.
fn field(table: &ObjectRef, old: &str, new: &str, schema: Option<&TableSchema>) -> Result<SyncScript> {
    if table.kind == kinds::VIEW || schema.is_some_and(|t| t.kind == kinds::VIEW) {
        return Err(Error::Unsupported("los campos de una vista salen de su pipeline: para renombrar uno hay que editar la vista".into()));
    }
    if old == "_id" || new == "_id" || old.starts_with("_id.") || new.starts_with("_id.") {
        return Err(Error::Unsupported("el campo _id no se puede renombrar".into()));
    }
    if new.starts_with('$') || new.contains('\0') || new.split('.').any(str::is_empty) {
        return Err(Error::Query(format!("«{new}» no es un nombre de campo válido: no puede empezar con $ ni tener partes vacías")));
    }
    if rename_path(new, old, "").is_some() || rename_path(old, new, "").is_some() {
        return Err(Error::Query("un campo no se puede renombrar a una ruta dentro de sí mismo, ni al revés".into()));
    }
    if let Some(t) = schema {
        if ["timeField", "metaField"].iter().any(|k| t.options.get(*k).is_some_and(|f| rename_path(f, old, "").is_some())) {
            return Err(Error::Unsupported("en una colección de series temporales no se puede renombrar el campo de tiempo ni el de metadatos".into()));
        }
    }
    let name = &table.name;
    let c = coll(name);
    let mut statements = Vec::new();
    let mut warnings = vec![format!(
        "Renombrar un campo reescribe cada documento de «{name}» que lo tiene (updateMany con $rename): en una colección grande tarda y carga el oplog. No es atómico: si se corta, volver a correrlo termina el trabajo."
    )];
    let mut indexes = Vec::new();
    match schema {
        Some(t) => {
            for ix in &t.indexes {
                if let Some(r) = renamed_index(ix, old, new) {
                    statements.push(format!("{c}.dropIndex({})", q(&ix.name)));
                    indexes.push(r);
                }
            }
            if let Some(mut w) = validator_check(t)? {
                if let Some(v) = w.get_mut("validator") {
                    if rename_query(v, old, new) {
                        statements.push(format!("db.runCommand({{ \"collMod\": {}, \"validator\": {v} }})", q(name)));
                        warnings.push(format!("El validador de «{name}» se cambia con collMod para que nombre el campo nuevo."));
                    }
                }
            }
        }
        None => warnings.push(format!("No se pudo leer «{name}»: no se revisaron sus índices ni su validador.")),
    }
    statements.push(format!("{c}.updateMany({{ {}: {{ \"$exists\": true }} }}, {{ \"$rename\": {{ {}: {} }} }})", q(old), q(old), q(new)));
    for ix in &indexes {
        statements.push(create_index(name, ix)?);
    }
    if !indexes.is_empty() {
        let names: Vec<String> = indexes.iter().map(|i| format!("«{}»", i.name)).collect();
        warnings.push(format!(
            "Los índices que usan el campo se borran antes y se vuelven a crear después con el campo nuevo: {}. Mientras tanto las consultas no los usan y los únicos no controlan duplicados.",
            names.join(", ")
        ));
    }
    if old.contains('.') {
        warnings.push("$rename no entra en arrays: donde la ruta del campo pasa por un array, el documento queda como estaba.".into());
    }
    warnings.push(format!("Las aplicaciones que sigan escribiendo «{old}» lo vuelven a crear en los documentos nuevos."));
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{Shape, Stmt};
    use dbine_driver::CheckDef;

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: None, name: name.into() }
    }

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    fn parsed(text: &str) -> Vec<Stmt> {
        parse_units(text)
            .unwrap()
            .into_iter()
            .map(|u| match u.item {
                Item::Run(s) => s,
                _ => panic!("not a statement"),
            })
            .collect()
    }

    #[test]
    fn specs_per_flavor() {
        let m = spec(Flavor::Mongo);
        assert_eq!(m.kinds, ["collection", "view"]);
        assert!(m.columns && !m.indexes && !m.constraints && !m.schemas && !m.transactional);
        assert_eq!(m.references, ReferenceStyle::Pipeline);
        assert_eq!(m.replace, ReplaceStyle::DropCreate);
        assert!(!m.grants_on_objects);
        for f in [Flavor::Ferret, Flavor::DocumentDb] {
            let s = spec(f);
            assert_eq!(s.kinds, ["collection"]);
            assert!(s.columns);
            assert_eq!(s.note.as_deref(), Some(NOTE_NO_VIEWS));
        }
    }

    fn dbo(kind: &str, name: &str, definition: Option<&str>) -> DatabaseObject {
        DatabaseObject { kind: kind.into(), schema: None, name: name.into(), definition: definition.map(String::from) }
    }

    #[test]
    fn database_spec_only_on_mongodb() {
        let m = spec(Flavor::Mongo);
        assert!(m.databases && m.database_moves);
        assert_eq!(m.database_from.as_deref(), Some("admin"));
        assert_eq!(m.database_note.as_deref(), Some(DATABASE_NOTE));
        for f in [Flavor::Ferret, Flavor::DocumentDb] {
            let s = spec(f);
            assert!(!s.databases && !s.database_moves && s.database_from.is_none());
            assert!(database_script(f, "a", "b", &[]).is_err());
        }
    }

    #[test]
    fn database_moves_collections_and_views() {
        let objects = [
            dbo("collection", "clientes", None),
            dbo("collection", "a\"b", None),
            dbo("view", "v_activos", Some(r#"db.createView("v_activos", "clientes", [{"$match":{"activo":true}}], {"collation": {"locale":"es"}})"#)),
            dbo("collection", "system.js", None),
        ];
        let s = database_script(Flavor::Mongo, "ventas", "ventas_2024", &objects).unwrap();
        assert_eq!(
            s.statements,
            [
                r#"db.adminCommand({"renameCollection":"ventas.clientes","to":"ventas_2024.clientes"})"#,
                r#"db.adminCommand({"renameCollection":"ventas.a\"b","to":"ventas_2024.a\"b"})"#,
                "use ventas_2024",
                r#"db.createView("v_activos", "clientes", [{"$match":{"activo":true}}], {"collation": {"locale":"es"}})"#,
                "use ventas",
                r#"db.getCollection("v_activos").drop()"#,
                r#"db.getCollection("system.views").drop()"#,
            ]
        );
        assert_eq!(s.warnings.len(), 4, "{:?}", s.warnings);
        assert!(s.warnings[0].contains("atómico") && s.warnings[2].contains("usuarios y roles") && s.warnings[3].contains("system.js"));
        // Admin commands with whole namespaces, nothing added by the session.
        let st = parsed(&s.statements[0]);
        assert_eq!(st[0].cmd, mongodb::bson::doc! { "renameCollection": "ventas.clientes", "to": "ventas_2024.clientes" });
        assert!(st[0].admin);
        assert_ne!(st[0].shape, Shape::Rename);
        assert!(matches!(&parse_units(&s.statements[2]).unwrap()[0].item, Item::Use(d) if d == "ventas_2024"));
        // Without views: collections, then the old database's leftovers.
        let s = database_script(Flavor::Mongo, "a", "b", &[dbo("collection", "c", None)]).unwrap();
        assert_eq!(s.statements.len(), 3);
        assert_eq!(s.statements[1], "use a");
    }

    #[test]
    fn database_rename_refuses_system_and_bad_names() {
        for (old, new) in [("admin", "x"), ("x", "local"), ("Config", "x"), ("a", "a"), ("a", "b.c"), ("a", "b c"), ("a", "b$"), ("a", "")] {
            assert!(database_script(Flavor::Mongo, old, new, &[]).is_err(), "{old} -> {new}");
        }
        assert!(database_script(Flavor::Mongo, "a", "b", &[dbo("view", "v", None)]).is_err());
        assert!(database_script(Flavor::Mongo, "a", "b", &[dbo("function", "f", None)]).is_err());
    }

    #[test]
    fn collection_with_rename_collection() {
        let s = script(Flavor::Mongo, &req(RenameTarget::Object { object: obj("collection", "Pedidos"), parent: None }, "Órdenes 2024")).unwrap();
        assert_eq!(s.statements, [r#"db.getCollection("Pedidos").renameCollection("Órdenes 2024")"#]);
        let st = parsed(&s.statements[0]);
        assert_eq!(st[0].cmd, mongodb::bson::doc! { "renameCollection": "Pedidos", "to": "Órdenes 2024", "dropTarget": false });
        assert_eq!(st[0].shape, Shape::Rename);
        assert!(st[0].admin);
        // Quotes in names are JSON-escaped.
        let s = script(Flavor::Ferret, &req(RenameTarget::Object { object: obj("collection", "a\"b"), parent: None }, "c")).unwrap();
        assert_eq!(s.statements, [r#"db.getCollection("a\"b").renameCollection("c")"#]);
        assert!(script(Flavor::Mongo, &req(RenameTarget::Object { object: obj("collection", "a"), parent: None }, "x$y")).is_err());
        assert!(script(Flavor::Mongo, &req(RenameTarget::Object { object: obj("collection", "a"), parent: None }, "system.x")).is_err());
    }

    #[test]
    fn view_dropped_and_created() {
        let mut r = req(RenameTarget::Object { object: obj("view", "v_activos"), parent: None }, "VActivos");
        r.definition = Some(r#"db.createView("v_activos", "clientes", [{"$match":{"activo":true}}], {"collation": {"locale":"es"}})"#.into());
        let s = script(Flavor::Mongo, &r).unwrap();
        assert_eq!(
            s.statements,
            [
                r#"db.getCollection("v_activos").drop()"#,
                r#"db.createView("VActivos", "clientes", [{"$match":{"activo":true}}], {"collation": {"locale":"es"}})"#,
            ]
        );
        r.definition = None;
        assert!(matches!(script(Flavor::Mongo, &r), Err(Error::Query(_))));
        // No views on FerretDB / DocumentDB.
        assert!(matches!(script(Flavor::Ferret, &req(RenameTarget::Object { object: obj("view", "v"), parent: None }, "w")), Err(Error::Unsupported(_))));
    }

    fn ix(name: &str, cols: &[&str], unique: bool) -> IndexDef {
        IndexDef { name: name.into(), columns: cols.iter().map(|s| s.to_string()).collect(), unique, ..Default::default() }
    }

    #[test]
    fn field_with_dollar_rename_indexes_and_validator() {
        let mut t = TableSchema { kind: "collection".into(), name: "clientes".into(), ..Default::default() };
        let mut partial = ix("ix_otro", &["otro"], false);
        partial.filter = Some(r#"{"pepe":{"$gt":0}}"#.into());
        t.indexes = vec![ix("ux_pepe", &["pepe", "fecha:-1"], true), ix("ix_fecha", &["fecha"], false), partial];
        t.checks = vec![CheckDef {
            name: Some("validator".into()),
            expression: r#"{"validator":{"$jsonSchema":{"required":["pepe"],"properties":{"pepe":{"bsonType":"int"}}},"$expr":{"$gt":["$pepe",0]}}}"#.into(),
        }];
        let mut r = req(RenameTarget::Column { table: obj("collection", "clientes"), column: "pepe".into() }, "Pepe Nuevo");
        r.table = Some(t);
        let s = script(Flavor::Mongo, &r).unwrap();
        assert_eq!(
            s.statements,
            [
                r#"db.getCollection("clientes").dropIndex("ux_pepe")"#,
                r#"db.getCollection("clientes").dropIndex("ix_otro")"#,
                r#"db.runCommand({ "collMod": "clientes", "validator": {"$jsonSchema":{"required":["Pepe Nuevo"],"properties":{"Pepe Nuevo":{"bsonType":"int"}}},"$expr":{"$gt":["$Pepe Nuevo",0]}} })"#,
                r#"db.getCollection("clientes").updateMany({ "pepe": { "$exists": true } }, { "$rename": { "pepe": "Pepe Nuevo" } })"#,
                r#"db.getCollection("clientes").createIndex({"Pepe Nuevo":1,"fecha":-1}, {"name":"ux_pepe","unique":true})"#,
                r#"db.getCollection("clientes").createIndex({"otro":1}, {"name":"ix_otro","partialFilterExpression":{"Pepe Nuevo":{"$gt":0}}})"#,
            ]
        );
        assert!(s.warnings[0].contains("reescribe cada documento"));
        assert!(s.warnings.iter().any(|w| w.contains("«ux_pepe», «ix_otro»")));
        // Every statement parses in the driver's own language.
        for st in &s.statements {
            parsed(st);
        }
    }

    #[test]
    fn field_without_schema_and_refusals() {
        let col = |c: &str| RenameTarget::Column { table: obj("collection", "c"), column: c.into() };
        let s = script(Flavor::DocumentDb, &req(col("a.b"), "a.c")).unwrap();
        assert_eq!(s.statements, [r#"db.getCollection("c").updateMany({ "a.b": { "$exists": true } }, { "$rename": { "a.b": "a.c" } })"#]);
        assert!(s.warnings.iter().any(|w| w.contains("arrays")));
        assert!(s.warnings.iter().any(|w| w.contains("no se revisaron")));
        assert!(matches!(script(Flavor::Mongo, &req(col("_id"), "id")), Err(Error::Unsupported(_))));
        assert!(script(Flavor::Mongo, &req(col("a"), "$a")).is_err());
        assert!(script(Flavor::Mongo, &req(col("a"), "a.b")).is_err());
        assert!(script(Flavor::Mongo, &req(col("a"), "x..y")).is_err());
        let view_field = RenameTarget::Column { table: obj("view", "v"), column: "a".into() };
        assert!(matches!(script(Flavor::Mongo, &req(view_field, "b")), Err(Error::Unsupported(_))));
        let mut ts = TableSchema { kind: "collection".into(), name: "c".into(), ..Default::default() };
        ts.options.insert("timeField".into(), "t".into());
        let mut r = req(col("t"), "ts");
        r.table = Some(ts);
        assert!(matches!(script(Flavor::Mongo, &r), Err(Error::Unsupported(_))));
    }

    #[test]
    fn other_targets_refused() {
        let t = obj("collection", "c");
        assert!(matches!(script(Flavor::Mongo, &req(RenameTarget::Index { table: t.clone(), index: "i".into() }, "j")), Err(Error::Unsupported(_))));
        assert!(matches!(script(Flavor::Mongo, &req(RenameTarget::Constraint { table: t, constraint: "k".into() }, "j")), Err(Error::Unsupported(_))));
        assert!(matches!(script(Flavor::Mongo, &req(RenameTarget::Schema { database: None, schema: "d".into() }, "e")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn text_index_weights() {
        let mut t = ix("txt", &["pepe", "bio"], false);
        t.kind = Some("FULLTEXT".into());
        t.options.insert("weights".into(), r#"{"bio":1,"pepe":5}"#.into());
        let r = renamed_index(&t, "pepe", "apodo").unwrap();
        assert_eq!(r.columns, ["apodo", "bio"]);
        assert_eq!(r.options["weights"], r#"{"bio":1,"apodo":5}"#);
        assert!(renamed_index(&ix("f", &["fecha"], false), "pepe", "x").is_none());
        // A field named like the start of another isn't it.
        assert!(renamed_index(&ix("p", &["pepes"], false), "pepe", "x").is_none());
    }
}
