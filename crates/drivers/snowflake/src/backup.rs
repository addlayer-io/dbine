//! The server's own backups (docs/backups.md), two ways:
//!
//! - **Backup sets**: `CREATE BACKUP SET … FOR DATABASE …` once, then
//!   `ALTER BACKUP SET … ADD BACKUP` for each backup (or on the schedule of
//!   a backup policy); `SHOW BACKUPS IN BACKUP SET` lists them, `CREATE
//!   DATABASE … FROM BACKUP SET … IDENTIFIER '<id>'` restores one and
//!   `ALTER BACKUP SET … DELETE BACKUP IDENTIFIER '<id>'` deletes it (only
//!   the oldest of a set).
//! - **Zero-copy clones**: `CREATE DATABASE <db>_BKP_<date> CLONE <db>`,
//!   optionally at a past moment (Time Travel); restoring clones it back.
//!   The history finds them by that name.
//!
//! A restore always creates a database (Snowflake doesn't restore over an
//! existing one); to replace the original, the new one is swapped with it
//! (`ALTER DATABASE … SWAP WITH …`) and the old contents keep the other
//! name.

use crate::ddl::{lit, Row};
use crate::SnowflakeSession;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use std::collections::BTreeMap;

/// What marks a clone made as a backup: `<db>_BKP_<date>`.
const CLONE_MARK: &str = "_BKP_";

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new(
                "method",
                "Método",
                FieldKind::Select(vec![("backup_set", "Backup set de Snowflake"), ("clone", "Clon sin copia (CLONE)")]),
            )
            .default_value("backup_set"),
            Field::new("set_name", "Backup set", FieldKind::Text)
                .placeholder("(<base>_BACKUPS)")
                .help("Si no existe, se crea para esta base; si existe, se le agrega un backup.")
                .when("method", &["backup_set"]),
            Field::new("set_schema", "Guardar el backup set en", FieldKind::Text)
                .placeholder("(<base>.PUBLIC)")
                .help("Base y esquema (base.esquema). Si queda dentro de la misma base, se borra con ella.")
                .when("method", &["backup_set"]),
            Field::new("policy", "Política de backups", FieldKind::Text)
                .help("Opcional, al crear el backup set: una BACKUP POLICY que ya exista (programa y retención).")
                .when("method", &["backup_set"]),
            Field::new("clone_name", "Nombre del clon", FieldKind::Text)
                .placeholder("(<base>_BKP_fecha)")
                .help("El historial encuentra los clones que se llaman <base>_BKP_…")
                .when("method", &["clone"]),
            Field::new("at", "Momento (Time Travel)", FieldKind::Text)
                .placeholder("2026-09-29 15:00:00 -03:00")
                .help("Vacío: ahora. Si no, la base como estaba en ese momento, dentro de su retención de Time Travel.")
                .when("method", &["clone"]),
        ],
        restore: true,
        restore_options: vec![
            Field::new(
                "mode",
                "Cómo",
                FieldKind::Select(vec![
                    ("new", "Crear una base nueva con ese nombre"),
                    ("swap", "Reemplazar la base (la anterior queda como <base>_ANTERIOR_fecha)"),
                ]),
            )
            .default_value("new"),
            Field::new("at", "Momento (Time Travel)", FieldKind::Text)
                .placeholder("2026-09-29 15:00:00 -03:00")
                .help("Solo si el origen es una base (un clon o la misma): la restaura como estaba en ese momento."),
        ],
        delete: true,
        history: true,
        server_wide: false,
        script_database: "",
        note: "Los backups quedan en Snowflake: los backup sets guardan copias inmutables con su propia \
               retención, y los clones son bases nuevas que no ocupan espacio hasta que cambian. Restaurar \
               siempre crea una base; para reemplazar la original se intercambian los nombres. De un backup \
               set solo se puede borrar el backup más viejo.",
    }
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

fn opt<'a>(o: &'a BTreeMap<String, String>, k: &str) -> Option<&'a str> {
    o.get(k).map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// `20260929_154500` (UTC).
fn stamp() -> String {
    chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string()
}

/// `base.esquema` (each part may be "quoted") → `"BASE"."ESQUEMA"`.
/// Unquoted parts are uppercased, as Snowflake resolves them.
fn qualified(path: &str) -> Result<String> {
    let parts = parse_path(path, true).ok_or_else(|| Error::Query(format!("«{path}» no es un nombre válido")))?;
    Ok(parts.iter().map(|p| q(p)).collect::<Vec<_>>().join("."))
}

/// A dotted name: "quoted" parts (with "" for a quote) or plain ones
/// (letters, digits, `_`, `$`; uppercased when `fold`).
fn parse_path(path: &str, fold: bool) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut chars = path.trim().chars().peekable();
    loop {
        let mut part = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next()? {
                    '"' if chars.peek() == Some(&'"') => {
                        chars.next();
                        part.push('"');
                    }
                    '"' => break,
                    c => part.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if !(c.is_alphanumeric() || c == '_' || c == '$') {
                    break;
                }
                part.push(if fold { c.to_ascii_uppercase() } else { c });
                chars.next();
            }
        }
        if part.is_empty() {
            return None;
        }
        parts.push(part);
        match chars.next() {
            None => return Some(parts),
            Some('.') => {}
            Some(_) => return None,
        }
    }
}

/// `AT (TIMESTAMP => '…'::TIMESTAMP_TZ)`, or nothing.
fn at(o: &BTreeMap<String, String>) -> String {
    opt(o, "at").map(|t| format!(" AT (TIMESTAMP => {}::TIMESTAMP_TZ)", lit(t))).unwrap_or_default()
}

/// A history id of a backup set: `<uuid>@"DB"."SCHEMA"."SET"`.
fn parse_set_id(source: &str) -> Option<(String, String)> {
    let (id, set) = source.trim().split_once('@')?;
    let id = id.trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return None;
    }
    let parts = parse_path(set, false)?;
    (parts.len() == 3).then(|| (id.to_string(), parts.iter().map(|p| q(p)).collect::<Vec<_>>().join(".")))
}

fn database(d: &Option<String>) -> Result<&str> {
    d.as_deref().map(str::trim).filter(|d| !d.is_empty()).ok_or_else(|| Error::Query("falta la base".into()))
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database: db, options } => {
            let db = database(db)?;
            if opt(options, "method") == Some("clone") {
                let name = opt(options, "clone_name").map_or_else(|| format!("{db}{CLONE_MARK}{}", stamp()), str::to_string);
                return Ok(format!("CREATE DATABASE {} CLONE {}{};", q(&name), q(db), at(options)));
            }
            let set_name = opt(options, "set_name").map_or_else(|| format!("{db}_BACKUPS"), str::to_string);
            let schema = match opt(options, "set_schema") {
                Some(s) => qualified(s)?,
                None => format!("{}.{}", q(db), q("PUBLIC")),
            };
            let set = format!("{schema}.{}", q(&set_name));
            let policy = match opt(options, "policy") {
                Some(p) => format!(" WITH BACKUP POLICY {}", qualified(p)?),
                None => String::new(),
            };
            Ok(format!(
                "CREATE BACKUP SET IF NOT EXISTS {set} FOR DATABASE {}{policy};\nALTER BACKUP SET {set} ADD BACKUP;",
                q(db)
            ))
        }
        BackupAction::Restore { source, database: db, options } => {
            let target = database(db)?;
            let source = source.trim();
            if source.is_empty() {
                return Err(Error::Query("falta el backup a restaurar".into()));
            }
            let from = match parse_set_id(source) {
                Some((id, set)) => format!("FROM BACKUP SET {set} IDENTIFIER {}", lit(&id)),
                None => {
                    // A database (a clone, or the same one at a past moment).
                    let parts = parse_path(source, false)
                        .filter(|p| p.len() == 1)
                        .ok_or_else(|| Error::Query(format!("«{source}» no es un backup ni una base")))?;
                    format!("CLONE {}{}", q(&parts[0]), at(options))
                }
            };
            if opt(options, "mode") == Some("swap") {
                let old = format!("{target}_ANTERIOR_{}", stamp());
                Ok(format!(
                    "CREATE DATABASE {} {from};\nALTER DATABASE {} SWAP WITH {};",
                    q(&old),
                    q(target),
                    q(&old)
                ))
            } else {
                if source == target {
                    return Err(Error::Query("la base ya existe: elegí otro nombre o reemplazala".into()));
                }
                Ok(format!("CREATE DATABASE {} {from};", q(target)))
            }
        }
        BackupAction::Delete { source } => {
            if let Some((id, set)) = parse_set_id(source) {
                return Ok(format!("ALTER BACKUP SET {set} DELETE BACKUP IDENTIFIER {};", lit(&id)));
            }
            let name = source.trim();
            if !name.contains(CLONE_MARK) || parse_path(name, false).is_none_or(|p| p.len() != 1) {
                return Err(Error::Query(format!("«{name}» no es un backup: DBine solo borra clones llamados <base>{CLONE_MARK}…")));
            }
            Ok(format!("DROP DATABASE {};", q(name)))
        }
    }
}

// -- history -----------------------------------------------------------------

/// A SHOW timestamp (`"<epoch>[ <offset>]"`) as ISO 8601 UTC.
fn when(v: Option<&String>) -> Option<String> {
    let e = v?.split(' ').next()?;
    let (secs, nanos) = crate::epoch(e)?;
    Some(chrono::DateTime::from_timestamp(secs, nanos)?.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn get(r: &Row, k: &str) -> Option<String> {
    r.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// A backup of `SHOW BACKUPS IN BACKUP SET`; `set` is the set's quoted name.
fn set_entry(r: &Row, set: &str, db: &str, policy: Option<&str>) -> BackupEntry {
    let id = get(r, "backup_id").unwrap_or_default();
    let mut details = vec![("Backup set".to_string(), set.to_string()), ("Id".into(), id.clone())];
    if let Some(e) = when(r.get("expire_on")) {
        details.push(("Vence".into(), e));
    }
    if let Some(p) = policy {
        details.push(("Política".into(), p.to_string()));
    }
    if let Some(c) = get(r, "comment") {
        details.push(("Comentario".into(), c));
    }
    BackupEntry {
        id: format!("{id}@{set}"),
        database: Some(db.to_string()),
        kind: Some("Backup set".into()),
        started: when(r.get("created_on")),
        finished: None,
        size: None,
        location: Some(set.to_string()),
        status: None,
        details,
        restorable: !id.is_empty(),
    }
}

/// A database of `SHOW DATABASES` named `<db>_BKP_…`.
fn clone_entry(r: &Row, db: &str) -> BackupEntry {
    let name = get(r, "name").unwrap_or_default();
    let mut details = vec![];
    if let Some(d) = get(r, "retention_time") {
        details.push(("Retención de Time Travel (días)".into(), d));
    }
    if let Some(c) = get(r, "comment") {
        details.push(("Comentario".into(), c));
    }
    BackupEntry {
        id: name.clone(),
        database: Some(db.to_string()),
        kind: Some("Clon".into()),
        started: when(r.get("created_on")),
        finished: None,
        size: None,
        location: Some(name),
        status: None,
        details,
        restorable: true,
    }
}

/// A `SHOW … LIKE` pattern matching `name` literally.
fn like_exact(name: &str) -> String {
    name.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

impl SnowflakeSession {
    /// The backups of every backup set for `db` the role can see, and its
    /// clones named `<db>_BKP_…`; newest first.
    pub(crate) async fn backup_history(&self, db: Option<&str>) -> Result<Vec<BackupEntry>> {
        let db = match db.filter(|d| !d.is_empty()) {
            Some(d) => d.to_string(),
            None => self.database()?,
        };
        let mut out = Vec::new();
        // Accounts without backup sets (or roles that can't see them) still
        // get the clones.
        let sets = self.named_rows("SHOW BACKUP SETS IN ACCOUNT").await.unwrap_or_default();
        for s in sets.iter().filter(|s| {
            get(s, "object_kind").is_some_and(|k| k.eq_ignore_ascii_case("DATABASE"))
                && get(s, "object_name").as_deref() == Some(db.as_str())
        }) {
            let (Some(d), Some(sc), Some(n)) = (get(s, "database_name"), get(s, "schema_name"), get(s, "name")) else {
                continue;
            };
            let set = format!("{}.{}.{}", q(&d), q(&sc), q(&n));
            let policy = get(s, "backup_policy_name");
            // Without OWNERSHIP of the set its backups can't be listed.
            if let Ok(rows) = self.named_rows(&format!("SHOW BACKUPS IN BACKUP SET {set}")).await {
                out.extend(rows.iter().map(|r| set_entry(r, &set, &db, policy.as_deref())));
            }
        }
        let pattern = format!("{}{}%", like_exact(&db), like_exact(CLONE_MARK));
        let clones = self.named_rows(&format!("SHOW DATABASES LIKE {}", lit(&pattern))).await?;
        out.extend(clones.iter().map(|r| clone_entry(r, &db)));
        out.sort_by(|a, b| b.started.cmp(&a.started));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn backup(db: &str, kv: &[(&str, &str)]) -> Result<String> {
        script(&BackupAction::Backup { database: Some(db.into()), options: opts(kv) })
    }

    fn restore(src: &str, db: &str, kv: &[(&str, &str)]) -> Result<String> {
        script(&BackupAction::Restore { source: src.into(), database: Some(db.into()), options: opts(kv) })
    }

    #[test]
    fn backup_scripts() {
        assert_eq!(
            backup("VENTAS", &[]).unwrap(),
            "CREATE BACKUP SET IF NOT EXISTS \"VENTAS\".\"PUBLIC\".\"VENTAS_BACKUPS\" FOR DATABASE \"VENTAS\";\n\
             ALTER BACKUP SET \"VENTAS\".\"PUBLIC\".\"VENTAS_BACKUPS\" ADD BACKUP;"
        );
        assert_eq!(
            backup("Ven\"tas", &[("set_name", "diario"), ("set_schema", "backups.\"Mi Esq\""), ("policy", "ops.pol.hourly")])
                .unwrap(),
            "CREATE BACKUP SET IF NOT EXISTS \"BACKUPS\".\"Mi Esq\".\"diario\" FOR DATABASE \"Ven\"\"tas\" \
             WITH BACKUP POLICY \"OPS\".\"POL\".\"HOURLY\";\n\
             ALTER BACKUP SET \"BACKUPS\".\"Mi Esq\".\"diario\" ADD BACKUP;"
        );
        assert!(backup("V", &[("set_schema", "a; DROP DATABASE V")]).is_err());
        assert_eq!(
            backup("V", &[("method", "clone"), ("clone_name", "V_BKP_1"), ("at", "2026-09-29 10:00:00 -03:00'")]).unwrap(),
            "CREATE DATABASE \"V_BKP_1\" CLONE \"V\" AT (TIMESTAMP => '2026-09-29 10:00:00 -03:00'''::TIMESTAMP_TZ);"
        );
        let auto = backup("V", &[("method", "clone")]).unwrap();
        assert!(auto.starts_with("CREATE DATABASE \"V_BKP_2"), "{auto}");
        assert!(auto.ends_with(" CLONE \"V\";"), "{auto}");
    }

    #[test]
    fn restore_and_delete_scripts() {
        let id = "29c2c1b9-6599-4f0b-87b8-d43377fd7c77@\"V\".\"PUBLIC\".\"V_BACKUPS\"";
        assert_eq!(
            restore(id, "V2", &[]).unwrap(),
            "CREATE DATABASE \"V2\" FROM BACKUP SET \"V\".\"PUBLIC\".\"V_BACKUPS\" IDENTIFIER '29c2c1b9-6599-4f0b-87b8-d43377fd7c77';"
        );
        let swap = restore(id, "V", &[("mode", "swap")]).unwrap();
        assert!(swap.starts_with("CREATE DATABASE \"V_ANTERIOR_2"), "{swap}");
        assert!(swap.contains("\nALTER DATABASE \"V\" SWAP WITH \"V_ANTERIOR_2"), "{swap}");
        assert_eq!(restore("V_BKP_1", "V3", &[]).unwrap(), "CREATE DATABASE \"V3\" CLONE \"V_BKP_1\";");
        assert_eq!(
            restore("V", "V_AYER", &[("at", "2026-09-28 00:00:00 +00:00")]).unwrap(),
            "CREATE DATABASE \"V_AYER\" CLONE \"V\" AT (TIMESTAMP => '2026-09-28 00:00:00 +00:00'::TIMESTAMP_TZ);"
        );
        assert!(restore("V_BKP_1", "V_BKP_1", &[]).is_err());
        assert!(restore("x; DROP DATABASE V", "V2", &[]).is_err());
        assert!(restore("zz@\"a\".\"b\".\"c\"", "V2", &[]).is_err());

        let del = |s: &str| script(&BackupAction::Delete { source: s.into() });
        assert_eq!(
            del(id).unwrap(),
            "ALTER BACKUP SET \"V\".\"PUBLIC\".\"V_BACKUPS\" DELETE BACKUP IDENTIFIER '29c2c1b9-6599-4f0b-87b8-d43377fd7c77';"
        );
        assert_eq!(del("V_BKP_20260929_154500").unwrap(), "DROP DATABASE \"V_BKP_20260929_154500\";");
        assert!(del("VENTAS").is_err());
        assert!(del("V_BKP_1; DROP DATABASE X").is_err());
    }

    #[test]
    fn history_rows() {
        let row = |kv: &[(&str, &str)]| -> Row { kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        let e = set_entry(
            &row(&[("backup_id", "abc-1"), ("created_on", "1790000000.000000000"), ("expire_on", "1790086400.5")]),
            "\"V\".\"PUBLIC\".\"S\"",
            "V",
            Some("P"),
        );
        assert_eq!(e.id, "abc-1@\"V\".\"PUBLIC\".\"S\"");
        assert_eq!(e.started.as_deref(), Some("2026-09-21T14:13:20Z"));
        assert!(e.restorable);
        assert!(parse_set_id(&e.id).is_some());
        let c = clone_entry(&row(&[("name", "V_BKP_1"), ("created_on", "1790000000.0 -0700")]), "V");
        assert_eq!((c.id.as_str(), c.kind.as_deref()), ("V_BKP_1", Some("Clon")));
        assert_eq!(like_exact("A_B%"), "A\\_B\\%");
    }
}
