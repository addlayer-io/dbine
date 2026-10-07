//! "Nueva base de datos" with options ([`dbine_driver::Driver::create_database_fields`]).
//!
//! A Firebird database is a file the server creates (op_create, what isql
//! sends for `CREATE DATABASE`). The options are the folder it goes in
//! (with the name, the file's path), its page size (a create parameter)
//! and its default character set, which the create can't take over this
//! protocol: it's an `ALTER DATABASE SET DEFAULT CHARACTER SET` on the new
//! database right after (Firebird 3+).
//!
//! The script shows that `CREATE DATABASE` and the `ALTER`, separated by
//! `;`. Every value is checked before it's used.

use crate::{err, join_err, text, FirebirdSession};
use dbine_driver::{Error, Field, FieldChoices, FieldKind, Result};
use rsfbclient_core::{Dialect, FirebirdClientDbOps, FirebirdClientSqlOps, TrDataAccessMode, TrIsolationLevel, TrLockResolution, TrOp, TransactionConfiguration};
use rsfbclient_rust::RustFbClient;
use std::collections::BTreeMap;

const PAGE_SIZES: [(&str, &str); 4] = [("4096", "4096"), ("8192", "8192"), ("16384", "16384"), ("32768", "32768 (Firebird 4+)")];

pub(crate) fn fields() -> Vec<Field> {
    vec![
        Field::new("folder", "Carpeta del archivo", FieldKind::Text).help(
            "En el servidor. Con carpeta, el nombre es el del archivo (se le agrega .fdb si no tiene extensión). \
             Vacía: el nombre es la ruta completa o un alias de databases.conf.",
        ),
        Field::new("page_size", "Tamaño de página (PAGE_SIZE)", FieldKind::Select(PAGE_SIZES.to_vec()))
            .help("Vacío: el del servidor (8192 desde Firebird 3)."),
        Field::new("charset", "Juego de caracteres por defecto", FieldKind::Text)
            .help("Vacío: NONE. Es el de las columnas de texto que no indiquen otro."),
    ]
}

fn opt<'a>(o: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    o.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// What the options resolve to: the file's path, the page size and the
/// `ALTER` that sets the default character set.
pub(crate) struct Plan {
    pub path: String,
    pub page_size: Option<u32>,
    pub alter: Option<String>,
}

pub(crate) fn plan(name: &str, o: &BTreeMap<String, String>) -> Result<Plan> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Query("Falta la ruta o el alias de la base de datos.".into()));
    }
    let path = match opt(o, "folder") {
        Some(dir) => {
            if name.contains(['/', '\\']) {
                return Err(Error::Query("con una carpeta, el nombre es solo el del archivo (sin carpetas)".into()));
            }
            let sep = if dir.contains('\\') && !dir.contains('/') { '\\' } else { '/' };
            let file = if name.contains('.') { name.to_string() } else { format!("{name}.fdb") };
            format!("{}{sep}{file}", dir.trim_end_matches(['/', '\\']))
        }
        None => name.to_string(),
    };
    if path.chars().any(char::is_control) {
        return Err(Error::Query("la ruta tiene caracteres de control".into()));
    }
    let page_size = match opt(o, "page_size") {
        Some(p) => match PAGE_SIZES.iter().find(|(v, _)| *v == p) {
            Some(_) => Some(p.parse().unwrap_or(8192)),
            None => return Err(Error::Query(format!("tamaño de página: «{p}» no es 4096, 8192, 16384 ni 32768"))),
        },
        None => None,
    };
    let alter = match opt(o, "charset") {
        Some(c) => {
            if !c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
                return Err(Error::Query(format!("juego de caracteres: «{c}» no es un valor válido")));
            }
            Some(format!("ALTER DATABASE SET DEFAULT CHARACTER SET {}", c.to_ascii_uppercase()))
        }
        None => None,
    };
    Ok(Plan { path, page_size, alter })
}

pub(crate) fn script(name: &str, o: &BTreeMap<String, String>) -> Result<String> {
    let p = plan(name, o)?;
    let mut create = format!("CREATE DATABASE '{}'", p.path.replace('\'', "''"));
    if let Some(ps) = p.page_size {
        create.push_str(&format!("\nPAGE_SIZE {ps}"));
    }
    Ok(match p.alter {
        Some(a) => format!("{create};\n{a};"),
        None => create,
    })
}

impl FirebirdSession {
    /// The server's character sets (default NONE) and the current
    /// database's folder.
    pub(crate) async fn create_database_choices_impl(&mut self) -> Result<Vec<FieldChoices>> {
        let charsets = self.rows("SELECT TRIM(RDB$CHARACTER_SET_NAME) FROM RDB$CHARACTER_SETS ORDER BY 1", vec![]).await.unwrap_or_default();
        let file = self.rows("SELECT MON$DATABASE_NAME FROM MON$DATABASE", vec![]).await.ok().and_then(|r| r.first().and_then(|r| r.first()).and_then(text));
        let folder = file.and_then(|f| f.rfind(['/', '\\']).map(|i| f[..i].to_string())).filter(|d| !d.is_empty());
        Ok(vec![
            FieldChoices { key: "folder".into(), default: None, values: folder.into_iter().collect() },
            FieldChoices { key: "charset".into(), default: Some("NONE".into()), values: charsets.iter().filter_map(|r| r.first().and_then(text)).collect() },
        ])
    }

    /// Create the file, then set its default character set on it.
    pub(crate) async fn create_database_with_impl(&mut self, name: &str, o: &BTreeMap<String, String>) -> Result<()> {
        let p = plan(name, o)?;
        let mut t = self.target.clone();
        t.attach.db_name = p.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut client = RustFbClient::new(t.charset.clone());
            // rsfbclient-rust writes the page size big-endian, but DPB
            // integers are little-endian (16384 arrived as 4194304 and the
            // server took its maximum): swapped here, it arrives right.
            let page_size = p.page_size.map(u32::swap_bytes);
            let mut db = client.create_database(&t.attach, page_size, Dialect::D3).map_err(err)?;
            let mut step = || -> Result<()> {
                let Some(alter) = &p.alter else { return Ok(()) };
                let conf = TransactionConfiguration {
                    data_access: TrDataAccessMode::ReadWrite,
                    isolation: TrIsolationLevel::Concurrency,
                    lock_resolution: TrLockResolution::Wait(None),
                };
                let mut tr = client.begin_transaction(&mut db, conf).map_err(err)?;
                client.exec_immediate(&mut db, &mut tr, Dialect::D3, alter).map_err(err)?;
                client.transaction_operation(&mut tr, TrOp::Commit).map_err(err)
            };
            let r = step().map_err(|e| {
                Error::Query(format!("la base «{}» se creó, pero falló este paso: {}\n{e}", p.path, p.alter.as_deref().unwrap_or_default()))
            });
            let detached = client.detach_database(&mut db).map_err(err);
            r.and(detached)
        })
        .await
        .map_err(join_err)?
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
        assert_eq!(script(" /data/v.fdb ", &o(&[("charset", "")])).unwrap(), "CREATE DATABASE '/data/v.fdb'");
        assert_eq!(script("it's", &o(&[])).unwrap(), "CREATE DATABASE 'it''s'");
        let p = plan("ventas", &o(&[])).unwrap();
        assert_eq!((p.path.as_str(), p.page_size, p.alter), ("ventas", None, None));
    }

    #[test]
    fn folder_page_size_and_charset() {
        assert_eq!(
            script("ventas", &o(&[("folder", "/var/lib/firebird/data/"), ("page_size", "16384"), ("charset", "utf8")])).unwrap(),
            "CREATE DATABASE '/var/lib/firebird/data/ventas.fdb'\nPAGE_SIZE 16384;\nALTER DATABASE SET DEFAULT CHARACTER SET UTF8;"
        );
        assert_eq!(plan("v.gdb", &o(&[("folder", "C:\\Datos")])).unwrap().path, "C:\\Datos\\v.gdb");
    }

    #[test]
    fn values_are_checked() {
        for bad in [("page_size", "1000"), ("charset", "UTF8; DROP"), ("folder", "/x\u{1}y")] {
            assert!(script("v", &o(&[bad])).is_err(), "{bad:?}");
        }
        assert!(script("a/b", &o(&[("folder", "/data")])).is_err());
        assert!(script(" ", &o(&[])).is_err());
    }
}
