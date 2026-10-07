//! "Nueva base de datos" with options
//! ([`dbine_driver::Driver::create_database_fields`]): the storage type,
//! the segment of `POST /database/{name}/{storage}/graph`. OrientDB 3
//! gives graph and document databases the same classes (`V`, `E`), so the
//! database type isn't asked; it stays `graph`, as before.

use crate::{seg, OrientSession};
use dbine_driver::{Error, Field, FieldKind, Result};
use reqwest::Method;
use std::collections::BTreeMap;

pub(crate) fn fields() -> Vec<Field> {
    vec![Field::new(
        "storage",
        "Almacenamiento",
        FieldKind::Select(vec![("plocal", "En disco (plocal)"), ("memory", "En memoria (memory)")]),
    )
    .help("Vacío: en disco. En memoria se pierde al reiniciar el servidor.")]
}

/// The path of the create.
pub(crate) fn path(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Query("falta el nombre de la base".into()));
    }
    let storage = match o.get("storage").map(|v| v.trim()).filter(|v| !v.is_empty()) {
        None => "plocal",
        Some(s @ ("plocal" | "memory")) => s,
        Some(v) => return Err(Error::Query(format!("almacenamiento: «{v}» no es un valor válido"))),
    };
    Ok(format!("/database/{}/{storage}/graph", seg(name)))
}

/// What "Ver script" shows: the request.
pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    Ok(format!("POST {}", path(name, o)?))
}

impl OrientSession {
    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se puede crear una base.".into()));
        }
        let p = path(name, o)?;
        self.call(Method::POST, &p, None).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn plain_name_is_the_old_create() {
        assert_eq!(script(" ventas ", &o(&[("storage", "")])).unwrap(), "POST /database/ventas/plocal/graph");
        assert_eq!(script("a b", &o(&[])).unwrap(), "POST /database/a%20b/plocal/graph");
    }

    #[test]
    fn storage() {
        assert_eq!(script("v", &o(&[("storage", "memory")])).unwrap(), "POST /database/v/memory/graph");
        assert!(script("v", &o(&[("storage", "remote")])).is_err());
        assert!(script("v", &o(&[("storage", "memory/../x")])).is_err());
        assert!(script(" ", &o(&[])).is_err());
    }
}
