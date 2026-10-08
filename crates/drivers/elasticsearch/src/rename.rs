//! "Renombrar…" in console syntax. An index has no rename: it's
//! write-blocked, cloned under the new name (`_clone`, the block taken off
//! the copy), waited on, and the old one goes in one `_aliases` call that
//! also moves its aliases (`remove_index` is atomic with the adds). An
//! alias is removed and added under the new name in one `_aliases` call,
//! on every index it points to, with its filter, routing and write flag.
//! Fields can't be renamed without a reindex; data streams aren't renamed.

use crate::ddl::check_index_name;
use crate::json::{Obj, J};
use crate::KIND_ALIAS;
use dbine_driver::rename::{Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle};
use dbine_driver::{kinds, Error, Result, SyncScript};

pub(crate) const NOTE: &str = "Un índice no se renombra en el lugar: se bloquea su escritura, se clona con el nombre nuevo (_clone), \
se espera a que la copia esté activa y se borra el original, pasándole sus alias a la copia en el mismo paso. \
Un alias se quita y se vuelve a crear con el nombre nuevo en un solo paso atómico. Los campos no se renombran: hace falta reindexar en un índice nuevo.";

pub(crate) fn spec() -> RenameSpec {
    RenameSpec {
        kinds: vec![kinds::INDEX.into(), KIND_ALIAS.into()],
        columns: false,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        references: ReferenceStyle::None,
        fold: Fold::None,
        transactional: false,
        note: Some(NOTE.into()),
        ..Default::default()
    }
}

fn obj(pairs: Vec<(&str, J)>) -> J {
    J::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn s(v: &str) -> J {
    J::Str(v.to_string())
}

/// `{ "<index>": { "aliases": {…}, … }, … }` (a `GET /<name>` reply) as
/// (index, its aliases) pairs.
fn indices(definition: &str) -> Result<Vec<(String, Obj)>> {
    let j = J::parse(definition).map_err(|e| Error::Query(format!("La definición leída del servidor no es JSON válido: {e}")))?;
    let all = j.as_obj().ok_or_else(|| Error::Query("La definición leída del servidor no tiene el formato esperado.".into()))?;
    Ok(all.iter().map(|(index, v)| (index.clone(), v.get("aliases").and_then(J::as_obj).cloned().unwrap_or_default())).collect())
}

/// An `add` action for `alias` on `index`, with the alias's own settings.
fn add(index: &str, alias: &str, props: &J) -> J {
    let mut a: Obj = vec![("index".into(), s(index)), ("alias".into(), s(alias))];
    a.extend(props.as_obj().into_iter().flatten().cloned());
    obj(vec![("add", J::Obj(a))])
}

fn aliases_request(actions: Vec<J>) -> String {
    format!("POST /_aliases\n{}", obj(vec![("actions", J::Arr(actions))]).pretty())
}

fn index_script(old: &str, new: &str, definition: Option<&str>) -> Result<SyncScript> {
    let mut warnings = vec![
        format!(
            "El clon copia el índice «{old}»: según su tamaño tarda y ocupa espacio en disco (en el mismo nodo suele usar enlaces duros y es rápido). \
Mientras dura, «{old}» no acepta escrituras."
        ),
        format!(
            "Si un paso falla, «{old}» queda bloqueado para escritura; se desbloquea con PUT /{old}/_settings y el cuerpo {{\"index.blocks.write\": false}}."
        ),
    ];
    let aliases = match definition {
        Some(d) => indices(d)?.into_iter().find(|(i, _)| i == old).map(|(_, a)| a).unwrap_or_default(),
        None => {
            warnings.push(format!("No se pudo leer la definición de «{old}»: si tiene alias, se pierden al borrarlo y hay que volver a crearlos sobre «{new}»."));
            Vec::new()
        }
    };
    let mut statements = vec![
        format!("PUT /{old}/_block/write"),
        // The copy inherits the block: it's taken off in the same call.
        format!("POST /{old}/_clone/{new}\n{}", obj(vec![("settings", obj(vec![("index.blocks.write", J::Null)]))]).pretty()),
        // The clone may answer before its primaries are active; the old
        // index isn't deleted until they are (a timeout stops the script).
        format!("GET /_cluster/health/{new}?wait_for_status=yellow&timeout=120s"),
    ];
    if aliases.is_empty() {
        statements.push(format!("DELETE /{old}"));
    } else {
        let names: Vec<String> = aliases.iter().map(|(a, _)| format!("«{a}»")).collect();
        warnings.push(format!("Los alias de «{old}» ({}) pasan a «{new}» en el mismo paso que borra el original.", names.join(", ")));
        let mut actions: Vec<J> = aliases.iter().map(|(a, props)| add(new, a, props)).collect();
        actions.push(obj(vec![("remove_index", obj(vec![("index", s(old))]))]));
        statements.push(aliases_request(actions));
    }
    Ok(SyncScript { statements, warnings })
}

fn alias_script(old: &str, new: &str, definition: Option<&str>) -> Result<SyncScript> {
    let definition = definition.ok_or_else(|| Error::Query(format!("No se pudo leer a qué índices apunta el alias «{old}».")))?;
    let mut actions = Vec::new();
    for (index, aliases) in indices(definition)? {
        let Some((_, props)) = aliases.iter().find(|(a, _)| a == old) else { continue };
        actions.push(obj(vec![("remove", obj(vec![("index", s(&index)), ("alias", s(old))]))]));
        actions.push(add(&index, new, props));
    }
    if actions.is_empty() {
        return Err(Error::Query(format!("El alias «{old}» no apunta a ningún índice.")));
    }
    Ok(SyncScript { statements: vec![aliases_request(actions)], warnings: Vec::new() })
}

/// The requests that rename the target.
pub(crate) fn script(req: &RenameRequest) -> Result<SyncScript> {
    let new = req.new_name.trim();
    match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::INDEX => {
            check_index_name(new)?;
            index_script(&object.name, new, req.definition.as_deref())
        }
        RenameTarget::Object { object, .. } if object.kind == KIND_ALIAS => {
            check_index_name(new).map_err(|_| {
                Error::Query(format!("El nombre «{new}» no sirve para un alias: va en minúsculas, no puede empezar con -, _ o + ni tener espacios, comas ni \\ / * ? \" < > | # :"))
            })?;
            alias_script(&object.name, new, req.definition.as_deref())
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::STREAM => {
            Err(Error::Unsupported("Un data stream no se renombra: hay que crear uno nuevo y reindexar sus documentos.".into()))
        }
        RenameTarget::Object { .. } => Err(Error::Unsupported("Solo se renombran índices y alias.".into())),
        RenameTarget::Column { .. } => Err(Error::Unsupported(
            "Los campos no se renombran: hay que reindexar en un índice nuevo con el campo renombrado (_reindex con un script) o agregar un alias de campo (type: alias).".into(),
        )),
        RenameTarget::Index { .. } | RenameTarget::Constraint { .. } => Err(Error::Unsupported("Solo se renombran índices y alias.".into())),
        RenameTarget::Schema { .. } => Err(Error::Unsupported("Un clúster no tiene bases de datos que renombrar.".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;

    fn req(kind: &str, name: &str, new: &str, definition: Option<&str>) -> RenameRequest {
        RenameRequest {
            target: RenameTarget::Object { object: ObjectRef { kind: kind.into(), schema: None, name: name.into() }, parent: None },
            new_name: new.into(),
            table: None,
            definition: definition.map(str::to_string),
        }
    }

    const DEF: &str = r#"{"clientes": {"aliases": {"activos": {"filter": {"term": {"x": "1"}}, "index_routing": "1", "search_routing": "1"}, "escritura": {"is_write_index": true}}, "mappings": {}, "settings": {}}}"#;

    #[test]
    fn index_is_cloned_and_its_aliases_moved() {
        let s = script(&req(kinds::INDEX, "clientes", "clientes_v2", Some(DEF))).unwrap();
        assert_eq!(s.statements[0], "PUT /clientes/_block/write");
        assert!(s.statements[1].starts_with("POST /clientes/_clone/clientes_v2\n"), "{}", s.statements[1]);
        let body = J::parse(s.statements[1].split_once('\n').unwrap().1).unwrap();
        assert_eq!(body.at(&["settings", "index.blocks.write"]), Some(&J::Null));
        assert_eq!(s.statements[2], "GET /_cluster/health/clientes_v2?wait_for_status=yellow&timeout=120s");
        let (head, body) = s.statements[3].split_once('\n').unwrap();
        assert_eq!(head, "POST /_aliases");
        let actions = J::parse(body).unwrap();
        let actions = actions.get("actions").and_then(J::as_arr).unwrap();
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].at(&["add", "index"]).and_then(J::as_str), Some("clientes_v2"));
        assert_eq!(actions[0].at(&["add", "alias"]).and_then(J::as_str), Some("activos"));
        assert_eq!(actions[0].at(&["add", "index_routing"]).and_then(J::as_str), Some("1"));
        assert!(actions[0].at(&["add", "filter", "term"]).is_some());
        assert_eq!(actions[1].at(&["add", "is_write_index"]).and_then(J::as_bool), Some(true));
        assert_eq!(actions[2].at(&["remove_index", "index"]).and_then(J::as_str), Some("clientes"));
        assert!(s.warnings.iter().any(|w| w.contains("«activos», «escritura»")));
    }

    #[test]
    fn index_without_aliases_is_deleted() {
        let s = script(&req(kinds::INDEX, "logs", "logs2", Some(r#"{"logs": {"aliases": {}}}"#))).unwrap();
        assert_eq!(s.statements.len(), 4);
        assert_eq!(s.statements[3], "DELETE /logs");
        let s = script(&req(kinds::INDEX, "logs", "logs2", None)).unwrap();
        assert_eq!(s.statements[3], "DELETE /logs");
        assert!(s.warnings.iter().any(|w| w.contains("se pierden")));
    }

    #[test]
    fn index_names_are_checked() {
        assert!(script(&req(kinds::INDEX, "logs", "Logs", None)).unwrap_err().to_string().contains("minúsculas"));
        assert!(script(&req(KIND_ALIAS, "a", "b c", Some(DEF))).unwrap_err().to_string().contains("alias"));
    }

    #[test]
    fn alias_is_moved_atomically_on_every_index() {
        let def = r#"{"i1": {"aliases": {"todos": {"is_write_index": true}, "otro": {}}}, "i2": {"aliases": {"todos": {}}}}"#;
        let s = script(&req(KIND_ALIAS, "todos", "todo", Some(def))).unwrap();
        assert_eq!(s.statements.len(), 1);
        let actions = J::parse(s.statements[0].split_once('\n').unwrap().1).unwrap();
        let a = actions.get("actions").and_then(J::as_arr).unwrap();
        assert_eq!(a.len(), 4);
        assert_eq!(a[0].at(&["remove", "alias"]).and_then(J::as_str), Some("todos"));
        assert_eq!(a[1].at(&["add", "alias"]).and_then(J::as_str), Some("todo"));
        assert_eq!(a[1].at(&["add", "is_write_index"]).and_then(J::as_bool), Some(true));
        assert_eq!(a[3].at(&["add", "index"]).and_then(J::as_str), Some("i2"));
        assert!(script(&req(KIND_ALIAS, "todos", "todo", None)).is_err());
    }

    #[test]
    fn fields_and_streams_are_refused() {
        let t = ObjectRef { kind: kinds::INDEX.into(), schema: None, name: "i".into() };
        let col = RenameRequest { target: RenameTarget::Column { table: t, column: "a".into() }, new_name: "b".into(), table: None, definition: None };
        assert!(script(&col).unwrap_err().to_string().contains("reindexar"));
        assert!(script(&req(kinds::STREAM, "s", "s2", None)).unwrap_err().to_string().contains("data stream"));
        let sp = spec();
        assert_eq!(sp.kinds, vec![kinds::INDEX.to_string(), KIND_ALIAS.to_string()]);
        assert!(!sp.columns);
    }
}
