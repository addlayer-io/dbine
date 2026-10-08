//! "Renombrar…" for Solr: a standalone server renames a core for real
//! (CoreAdmin `RENAME`; `core.properties` keeps the new name, the folder
//! keeps the old one). SolrCloud has no rename: the Collections API's
//! `RENAME` only adds an alias with the new name, and the collection keeps
//! its own (it's still listed, and answers, by the old one), so the
//! CoreAdmin call is sent as is and the server refuses it there
//! ([`CLOUD`] explains why).

use dbine_driver::rename::{Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle};
use dbine_driver::{kinds, Error, Result, SyncScript};

pub(crate) const NOTE: &str = "Solo en modo standalone: el core se renombra con CoreAdmin RENAME y su carpeta en el disco conserva el nombre anterior. \
En SolrCloud las colecciones no se renombran.";
pub(crate) const CLOUD: &str = "En SolrCloud las colecciones no se renombran: RENAME de la Collections API solo agrega un alias con el nombre nuevo \
y la colección conserva el suyo. Si alcanza con otro nombre para consultarla, creá un alias (action=CREATEALIAS).";

pub(crate) fn spec() -> RenameSpec {
    RenameSpec {
        kinds: vec![kinds::COLLECTION.into()],
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

/// Solr's rule for core and collection names.
fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty() && !name.starts_with('-') && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(Error::Query(format!("El nombre «{name}» no es válido: solo letras, números, punto, guion y guion bajo, y no puede empezar con guion.")))
    }
}

/// The request that renames the target.
pub(crate) fn script(req: &RenameRequest) -> Result<SyncScript> {
    match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::COLLECTION => {
            let new = req.new_name.trim();
            check_name(new)?;
            Ok(SyncScript { statements: vec![format!("GET /solr/admin/cores?action=RENAME&core={}&other={new}", object.name)], warnings: Vec::new() })
        }
        RenameTarget::Column { .. } => Err(Error::Unsupported("Solr no renombra campos: hay que agregar el campo nuevo al esquema y reindexar los documentos.".into())),
        _ => Err(Error::Unsupported("Solr solo renombra cores (en modo standalone).".into())),
    }
}

/// The request is CoreAdmin's `RENAME` (sent to SolrCloud, it's refused).
pub(crate) fn is_core_rename(req: &dbine_driver_elasticsearch::console::Request) -> bool {
    matches!(req.segments().as_slice(), ["solr", "admin", "cores", ..]) && req.query_param("action").is_some_and(|a| a.eq_ignore_ascii_case("RENAME"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    fn collection(name: &str) -> RenameTarget {
        RenameTarget::Object { object: ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: name.into() }, parent: None }
    }

    #[test]
    fn core_rename() {
        let s = script(&req(collection("libros"), "Libros_v2.1")).unwrap();
        assert_eq!(s.statements, vec!["GET /solr/admin/cores?action=RENAME&core=libros&other=Libros_v2.1"]);
        assert!(script(&req(collection("libros"), "mis libros")).unwrap_err().to_string().contains("no es válido"));
        assert!(script(&req(collection("libros"), "-x")).is_err());
        let console = dbine_driver_elasticsearch::console::parse(&s.statements[0]).unwrap();
        let dbine_driver_elasticsearch::console::Command::Http(r) = &console[0] else { panic!() };
        assert!(is_core_rename(r));
    }

    #[test]
    fn fields_are_refused() {
        let t = ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: "c".into() };
        assert!(script(&req(RenameTarget::Column { table: t, column: "a".into() }, "b")).unwrap_err().to_string().contains("reindexar"));
        assert!(script(&req(RenameTarget::Schema { database: None, schema: "s".into() }, "b")).is_err());
    }
}
