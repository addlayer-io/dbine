//! "Propiedades" of a database ([`dbine_driver::Session::database_properties`]):
//! the connected file (or another of the same server, opened for the call).
//!
//! Facts from `MON$DATABASE`: file, owner, created, ODS, dialect, page size
//! and pages, buffers, sweep interval, forced writes, read-only, reserved
//! space, shutdown, backup, encryption and replica state, transactions.
//!
//! What `ALTER DATABASE` (and `COMMENT ON DATABASE`) changes, per version:
//! the default character set and the comment; Firebird 3+ the linger time;
//! Firebird 4+ the default SQL SECURITY and the replication publication
//! (enabled, and whether it includes every table). Read-only, forced
//! writes and the sweep interval go through the services API (gfix), which
//! the client used here doesn't have: they're shown, not offered.
//! Encryption needs a crypt plugin and its key holder configured on the
//! server, which a connection can't see: not offered either.
//!
//! Each change is one statement in its own transaction, committed.

use crate::{err, int, join_err, text, Conn, FirebirdSession};
use dbine_driver::{DatabaseProperties, Error, Field, FieldChoices, FieldKind, PropertyInfo, Result};
use rsfbclient_core::{Column, Dialect, FirebirdClientSqlOps, TrOp};
use std::collections::BTreeMap;

fn yes(v: &str) -> bool {
    matches!(v.trim(), "true" | "1" | "ON" | "on")
}

fn flag(on: bool) -> String {
    if on { "true".into() } else { String::new() }
}

/// The statements for `changes`, in a fixed order.
pub(crate) fn alter(changes: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (key, value) in changes {
        let value = value.trim();
        out.push(match key.as_str() {
            "charset" => {
                if value.is_empty() || !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err(Error::Query(format!("juego de caracteres: «{value}» no es un valor válido")));
                }
                format!("ALTER DATABASE SET DEFAULT CHARACTER SET {}", value.to_ascii_uppercase())
            }
            "comment" if value.is_empty() => "COMMENT ON DATABASE IS NULL".into(),
            "comment" => format!("COMMENT ON DATABASE IS '{}'", value.replace('\'', "''")),
            "linger" if value.is_empty() || value == "0" => "ALTER DATABASE DROP LINGER".into(),
            "linger" => {
                if value.len() > 9 || !value.chars().all(|c| c.is_ascii_digit()) {
                    return Err(Error::Query(format!("demora al cerrar: «{value}» no es un número de segundos")));
                }
                format!("ALTER DATABASE SET LINGER TO {value}")
            }
            "sql_security" => {
                if !matches!(value, "DEFINER" | "INVOKER") {
                    return Err(Error::Query(format!("SQL SECURITY: «{value}» no es DEFINER ni INVOKER")));
                }
                format!("ALTER DATABASE SET DEFAULT SQL SECURITY {value}")
            }
            "publication" => format!("ALTER DATABASE {} PUBLICATION", if yes(value) { "ENABLE" } else { "DISABLE" }),
            "publication_all" if yes(value) => "ALTER DATABASE INCLUDE ALL TO PUBLICATION".into(),
            "publication_all" => "ALTER DATABASE EXCLUDE ALL FROM PUBLICATION".into(),
            k => return Err(Error::Query(format!("propiedad desconocida: {k}"))),
        });
    }
    Ok(out)
}

pub(crate) fn script(changes: &BTreeMap<String, String>) -> Result<String> {
    Ok(alter(changes)?.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n"))
}

/// `1.5 GB`.
fn human(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn label(v: Option<i64>, names: &[&str]) -> String {
    match v {
        Some(n) if n >= 0 && (n as usize) < names.len() => names[n as usize].to_string(),
        Some(n) => n.to_string(),
        None => String::new(),
    }
}

const MON: &str = "SELECT MON$DATABASE_NAME, MON$PAGE_SIZE, MON$ODS_MAJOR, MON$ODS_MINOR, MON$SQL_DIALECT, MON$PAGES,
       MON$PAGE_BUFFERS, MON$SWEEP_INTERVAL, MON$FORCED_WRITES, MON$READ_ONLY, MON$RESERVE_SPACE,
       CAST(MON$CREATION_DATE AS VARCHAR(60)), MON$SHUTDOWN_MODE, MON$BACKUP_STATE,
       MON$OLDEST_TRANSACTION, MON$OLDEST_ACTIVE, MON$NEXT_TRANSACTION
  FROM MON$DATABASE";

/// What one attachment reads (see [`FirebirdSession::properties`]).
struct Read {
    mon: Vec<Column>,
    /// Firebird 3+: owner, encryption state, security database.
    mon3: Option<Vec<Column>>,
    /// Firebird 4+: replica mode.
    replica: Option<i64>,
    /// Character set, comment.
    db: Vec<Column>,
    /// Firebird 3+.
    linger: Option<Option<i64>>,
    /// Firebird 4+.
    sql_security: Option<String>,
    /// Firebird 4+: RDB$DEFAULT's active and auto-enable flags.
    publication: Option<(bool, bool)>,
    relations: Vec<Column>,
    charsets: Vec<String>,
}

fn read(c: &mut Conn) -> Result<Read> {
    let first = |rows: Result<Vec<Vec<Column>>>| rows.ok().and_then(|r| r.into_iter().next());
    let mon = first(c.rows(MON, vec![])).ok_or_else(|| Error::Query("MON$DATABASE no devolvió filas".into()))?;
    let mon3 = first(c.rows("SELECT MON$OWNER, MON$CRYPT_STATE, MON$SEC_DATABASE FROM MON$DATABASE", vec![]));
    let replica = first(c.rows("SELECT MON$REPLICA_MODE FROM MON$DATABASE", vec![])).and_then(|r| r.first().and_then(int));
    let db = first(c.rows("SELECT TRIM(RDB$CHARACTER_SET_NAME), RDB$DESCRIPTION FROM RDB$DATABASE", vec![])).unwrap_or_default();
    let linger = first(c.rows("SELECT RDB$LINGER FROM RDB$DATABASE", vec![])).map(|r| r.first().and_then(int));
    let sql_security = first(c.rows("SELECT CASE WHEN RDB$SQL_SECURITY THEN 'DEFINER' ELSE 'INVOKER' END FROM RDB$DATABASE", vec![]))
        .and_then(|r| r.first().and_then(text));
    let publication = first(c.rows(
        "SELECT RDB$ACTIVE_FLAG, RDB$AUTO_ENABLE FROM RDB$PUBLICATIONS WHERE RDB$PUBLICATION_NAME = 'RDB$DEFAULT'",
        vec![],
    ))
    .map(|r| (r.first().and_then(int) == Some(1), r.get(1).and_then(int) == Some(1)));
    let relations = first(c.rows(
        "SELECT COUNT(CASE WHEN RDB$VIEW_BLR IS NULL THEN 1 END), COUNT(RDB$VIEW_BLR)
           FROM RDB$RELATIONS WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0",
        vec![],
    ))
    .unwrap_or_default();
    let charsets = c
        .rows("SELECT TRIM(RDB$CHARACTER_SET_NAME) FROM RDB$CHARACTER_SETS ORDER BY 1", vec![])
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.first().and_then(text))
        .collect();
    Ok(Read { mon, mon3, replica, db, linger, sql_security, publication, relations, charsets })
}

impl FirebirdSession {
    /// Run `f` on a fresh attachment to `database` (the session's when
    /// empty), apart from the session's transaction.
    async fn on_database<T, F>(&self, database: &str, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Conn) -> Result<T> + Send + 'static,
    {
        let mut t = self.target.clone();
        let database = database.trim();
        t.attach.db_name = if database.is_empty() { self.database.clone() } else { database.to_string() };
        tokio::task::spawn_blocking(move || {
            let mut c = Conn::open(&t)?;
            f(&mut c)
        })
        .await
        .map_err(join_err)?
    }

    pub(crate) async fn properties(&mut self, database: &str) -> Result<DatabaseProperties> {
        let r = self.on_database(database, read).await?;
        let m = |i: usize| r.mon.get(i);
        let n = |i: usize| m(i).and_then(int);
        let t = |i: usize| m(i).and_then(text).map(|s| s.trim().to_string()).unwrap_or_default();
        let yes_no = |i: usize| if n(i) == Some(1) { "Sí".to_string() } else { "No".to_string() };
        let fact = |group: &str, label: &str, value: String| PropertyInfo { group: group.into(), label: label.into(), value };

        let mut info = vec![fact("", "Archivo", t(0))];
        if let Some(m3) = &r.mon3 {
            info.push(fact("", "Dueño", m3.first().and_then(text).map(|s| s.trim().to_string()).unwrap_or_default()));
        }
        info.push(fact("", "Creada", t(11)));
        info.push(fact("", "ODS", format!("{}.{}", t(2), t(3))));
        info.push(fact("", "Dialecto SQL", t(4)));
        info.push(fact("", "Solo lectura", yes_no(9)));
        info.push(fact("", "Apagado (shutdown)", label(n(12), &["En línea", "Multiusuario", "Un solo usuario", "Completo"])));
        info.push(fact("", "Copia (nbackup)", label(n(13), &["Normal", "Bloqueada (BEGIN BACKUP)", "Fusionando"])));
        if let Some(m3) = &r.mon3 {
            info.push(fact(
                "",
                "Cifrado",
                label(m3.get(1).and_then(int), &["Sin cifrar", "Cifrada", "Descifrándose", "Cifrándose"]),
            ));
            info.push(fact("", "Base de seguridad", m3.get(2).and_then(text).map(|s| s.trim().to_string()).unwrap_or_default()));
        }
        if let Some(rm) = r.replica {
            info.push(fact("Replicación", "Modo de réplica", label(Some(rm), &["No es réplica", "Réplica de solo lectura", "Réplica de lectura y escritura"])));
        }
        if !r.relations.is_empty() {
            info.push(fact("", "Tablas", r.relations.first().and_then(int).unwrap_or(0).to_string()));
            info.push(fact("", "Vistas", r.relations.get(1).and_then(int).unwrap_or(0).to_string()));
        }
        let (page, pages) = (n(1).unwrap_or(0), n(5).unwrap_or(0));
        info.push(fact("Almacenamiento", "Tamaño", human(page * pages)));
        info.push(fact("Almacenamiento", "Tamaño de página", t(1)));
        info.push(fact("Almacenamiento", "Páginas", t(5)));
        info.push(fact("Almacenamiento", "Páginas en caché", t(6)));
        info.push(fact("Almacenamiento", "Escrituras forzadas (forced writes)", yes_no(8)));
        info.push(fact("Almacenamiento", "Espacio reservado", yes_no(10)));
        info.push(fact("Mantenimiento", "Intervalo de barrido (sweep)", t(7)));
        info.push(fact("Mantenimiento", "Transacción más vieja (OIT)", t(14)));
        info.push(fact("Mantenimiento", "Activa más vieja (OAT)", t(15)));
        info.push(fact("Mantenimiento", "Próxima transacción", t(16)));
        info.push(fact(
            "Mantenimiento",
            "Cómo cambiarlos",
            "Solo lectura, escrituras forzadas y el intervalo de barrido se cambian con gfix (-mode, -write, -housekeeping) o la API de servicios.".into(),
        ));

        let mut values = BTreeMap::new();
        let mut fields = vec![
            Field::new("charset", "Juego de caracteres por defecto", FieldKind::Text)
                .help("El de las columnas y dominios nuevos que no indiquen otro."),
            Field::new("comment", "Comentario", FieldKind::Textarea),
        ];
        values.insert("charset".to_string(), r.db.first().and_then(text).map(|s| s.trim().to_string()).unwrap_or_default());
        values.insert("comment".to_string(), r.db.get(1).and_then(text).unwrap_or_default());
        let mut warnings = BTreeMap::new();
        warnings.insert(
            "charset".to_string(),
            "Solo cambia el juego de caracteres de las columnas y dominios nuevos: los existentes conservan el suyo.".to_string(),
        );
        if let Some(l) = r.linger {
            values.insert("linger".into(), l.unwrap_or(0).to_string());
            fields.push(
                Field::new("linger", "Demora al cerrar (LINGER, s)", FieldKind::Number)
                    .help("Cuánto queda abierta la base después de la última desconexión (SuperServer). 0: se cierra enseguida.")
                    .group("Conexiones"),
            );
        }
        if let Some(s) = r.sql_security {
            values.insert("sql_security".into(), s);
            fields.push(
                Field::new(
                    "sql_security",
                    "SQL SECURITY por defecto",
                    FieldKind::Select(vec![("INVOKER", "Quien lo llama (INVOKER)"), ("DEFINER", "Su dueño (DEFINER)")]),
                )
                .help("Con qué permisos corren los procedimientos, funciones y triggers nuevos que no indiquen otro.")
                .group("Seguridad"),
            );
            warnings.insert(
                "sql_security".into(),
                "Con DEFINER, el código nuevo corre con los permisos de su dueño, aunque quien lo llame tenga menos: es un riesgo de seguridad.".into(),
            );
        }
        if let Some((active, all)) = r.publication {
            values.insert("publication".into(), flag(active));
            values.insert("publication_all".into(), flag(all));
            fields.push(
                Field::new("publication", "Publicación activa (ENABLE PUBLICATION)", FieldKind::Bool)
                    .help("Envía los cambios a las réplicas; requiere la replicación configurada en replication.conf.")
                    .group("Replicación"),
            );
            fields.push(
                Field::new("publication_all", "Publicar todas las tablas (INCLUDE ALL)", FieldKind::Bool)
                    .help("También las que se creen después.")
                    .group("Replicación"),
            );
            warnings.insert("publication".into(), "Desactivarla deja de enviar los cambios a las réplicas.".into());
            warnings.insert(
                "publication_all".into(),
                "Activarlo agrega a la publicación todas las tablas (también las nuevas); desactivarlo las quita a todas, también las agregadas una por una.".into(),
            );
        }
        let choices = vec![FieldChoices { key: "charset".into(), default: None, values: r.charsets }];
        Ok(DatabaseProperties { fields, values, info, choices, warnings })
    }

    pub(crate) async fn alter_database_impl(&mut self, database: &str, changes: &BTreeMap<String, String>) -> Result<()> {
        let statements = alter(changes)?;
        self.on_database(database, move |c| {
            for (i, sql) in statements.iter().enumerate() {
                let r = c
                    .client
                    .exec_immediate(&mut c.db, &mut c.tr, Dialect::D3, sql)
                    .and_then(|_| c.end_transaction(TrOp::Commit))
                    .map_err(err);
                if let Err(e) = r {
                    return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
                }
            }
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn each_change_is_one_statement() {
        assert_eq!(
            script(&c(&[
                ("charset", "utf8"),
                ("comment", "it's"),
                ("linger", "60"),
                ("sql_security", "DEFINER"),
                ("publication", "true"),
                ("publication_all", ""),
            ]))
            .unwrap(),
            "ALTER DATABASE SET DEFAULT CHARACTER SET UTF8;
COMMENT ON DATABASE IS 'it''s';
ALTER DATABASE SET LINGER TO 60;
ALTER DATABASE ENABLE PUBLICATION;
ALTER DATABASE EXCLUDE ALL FROM PUBLICATION;
ALTER DATABASE SET DEFAULT SQL SECURITY DEFINER;"
        );
        assert_eq!(
            script(&c(&[("comment", " "), ("linger", "0"), ("publication", ""), ("publication_all", "true")])).unwrap(),
            "COMMENT ON DATABASE IS NULL;\nALTER DATABASE DROP LINGER;\nALTER DATABASE DISABLE PUBLICATION;\nALTER DATABASE INCLUDE ALL TO PUBLICATION;"
        );
    }

    #[test]
    fn values_are_checked() {
        for bad in [("charset", "UTF8; DROP"), ("charset", ""), ("linger", "-1"), ("linger", "1e3"), ("sql_security", "OWNER"), ("nope", "1")] {
            assert!(script(&c(&[bad])).is_err(), "{bad:?}");
        }
    }
}
