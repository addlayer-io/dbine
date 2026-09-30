//! Native backups (docs/backups.md), for the variants whose own mechanism
//! is reachable through SQL:
//!
//! - CockroachDB: `BACKUP … INTO '<collection>'` (full, or incremental
//!   `INTO LATEST IN`), history from the cluster's `BACKUP` jobs and
//!   `RESTORE DATABASE … FROM '<subdir>' IN '<collection>'`. A backup can't
//!   be deleted through SQL (it's a folder in the storage).
//! - CrateDB: snapshots in a repository (`CREATE SNAPSHOT`,
//!   `sys.snapshots`, `RESTORE SNAPSHOT`, `DROP SNAPSHOT`). They cover the
//!   cluster, not a database.
//! - H2: `SCRIPT TO` / `BACKUP TO` a file on the server, `RUNSCRIPT FROM`
//!   to restore the SQL one. H2 keeps no history.
//!
//! PostgreSQL itself and the other variants back up with client tools
//! (pg_dump, pg_basebackup, gpbackup, yb-admin…) or the cloud provider's
//! API, which no statement reaches: only DBine's copies there.
//!
//! Every script is a single statement: the session sends a script as one
//! simple query, an implicit transaction, and CockroachDB refuses BACKUP
//! and RESTORE inside one.

use crate::catalog::{cell, lit};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

pub fn spec(v: Variant) -> Option<BackupSpec> {
    match v {
        Variant::Cockroach => Some(cockroach_spec()),
        Variant::CrateDb => Some(crate_spec()),
        Variant::H2 => Some(h2_spec()),
        _ => None,
    }
}

pub fn script(v: Variant, action: &BackupAction) -> Result<String> {
    match v {
        Variant::Cockroach => cockroach_script(action),
        Variant::CrateDb => crate_script(action),
        Variant::H2 => h2_script(action),
        _ => Err(Error::Unsupported(unsupported_reason(v).into())),
    }
}

pub async fn history(s: &PgSession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    match s.variant {
        Variant::Cockroach => cockroach_history(s, database).await,
        Variant::CrateDb => crate_history(s).await,
        Variant::H2 => Err(Error::Unsupported("H2 no guarda un historial de sus backups".into())),
        v => Err(Error::Unsupported(unsupported_reason(v).into())),
    }
}

/// Why a variant has no native backups in DBine.
fn unsupported_reason(v: Variant) -> &'static str {
    match v {
        Variant::Redshift => "los snapshots de Redshift se hacen con la API o la consola de AWS, no con SQL",
        Variant::Aurora => "los snapshots de Aurora se hacen con la API o la consola de AWS, no con SQL",
        Variant::CloudSql | Variant::AlloyDb => "los backups de este servicio se hacen con la API o la consola de Google Cloud, no con SQL",
        Variant::Yugabyte => "YugabyteDB hace sus backups con yb-admin o YugabyteDB Anywhere, no con SQL",
        Variant::Greenplum | Variant::Cloudberry | Variant::Greengage => {
            "este motor hace sus backups con gpbackup / gprestore, herramientas de línea de comandos"
        }
        Variant::Yellowbrick => "Yellowbrick hace sus backups con ybbackup / ybrestore, herramientas de línea de comandos",
        Variant::RisingWave => "RisingWave hace los backups de su metadata con risectl, no con SQL",
        Variant::Materialize => "Materialize no tiene backups propios: su estado se reconstruye desde las fuentes",
        Variant::Denodo => "Denodo exporta sus metadatos con su herramienta de exportación, no con un backup del servidor",
        _ => "este motor hace sus backups con herramientas de línea de comandos (pg_dump, pg_basebackup…), no con SQL",
    }
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

/// A string literal (none of these engines treats backslashes as escapes).
fn s(text: &str) -> String {
    lit(Variant::Postgres, text)
}

fn opt<'a>(options: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    options.get(key).map(|v| v.trim()).unwrap_or("")
}

fn flag(options: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    match opt(options, key) {
        "" => default,
        v => v == "true",
    }
}

/// A server timestamp (`2026-09-29 12:43:46.56+00`) as ISO 8601.
fn iso(t: &str) -> String {
    let mut t = t.trim().replacen(' ', "T", 1);
    let b = t.as_bytes();
    if b.len() > 3 && matches!(b[b.len() - 3], b'+' | b'-') && b[b.len() - 2..].iter().all(u8::is_ascii_digit) {
        t.push_str(":00");
    }
    t
}

// -- CockroachDB -----------------------------------------------------------

fn cockroach_spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new(
                "scope",
                "Alcance",
                FieldKind::Select(vec![("database", "Esta base de datos"), ("cluster", "Todo el clúster")]),
            )
            .default_value("database")
            .help("El backup del clúster incluye todas las bases, los usuarios y la configuración; necesita el rol admin."),
            Field::new("collection", "Colección (destino)", FieldKind::Text)
                .required()
                .placeholder("nodelocal://1/backups")
                .help("La URI donde el clúster guarda los backups: nodelocal://1/…, s3://bucket/ruta?AUTH=implicit, gs://…, azure-blob://…"),
            Field::new(
                "type",
                "Tipo",
                FieldKind::Select(vec![("full", "Completo"), ("incremental", "Incremental (sobre el último completo)")]),
            )
            .default_value("full"),
            Field::new("as_of", "AS OF SYSTEM TIME", FieldKind::Text)
                .placeholder("-10s")
                .help("Opcional: el momento a copiar, por ejemplo -10s para no competir con las escrituras en curso."),
            Field::new("revision_history", "Guardar el historial de revisiones", FieldKind::Bool)
                .default_value("false")
                .help("Permite restaurar a cualquier momento entre este backup y el siguiente."),
            Field::new("detached", "En segundo plano (DETACHED)", FieldKind::Bool)
                .default_value("false")
                .help("Devuelve el id del job sin esperar a que termine; el avance se ve en el historial."),
        ],
        restore: true,
        restore_options: vec![
            Field::new("from_database", "Base dentro del backup", FieldKind::Text)
                .placeholder("(la misma que la de destino)")
                .help("El nombre que tenía la base al hacer el backup. Si es distinto del destino, se restaura con new_db_name."),
            Field::new("detached", "En segundo plano (DETACHED)", FieldKind::Bool).default_value("false"),
        ],
        delete: false,
        history: true,
        server_wide: false,
        script_database: "",
        note: "El backup queda en la colección (nodelocal://, s3://, gs://, azure-blob://…), que el clúster tiene que poder escribir. \
               Cada backup completo crea una subcarpeta con la fecha y los incrementales se suman al último completo. \
               RESTORE crea la base: si ya existe, restaurá con otro nombre o borrala antes. \
               Los backups no se borran con SQL: hay que borrar la subcarpeta en el almacenamiento.",
    }
}

fn cockroach_script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database, options } => {
            let collection = opt(options, "collection");
            if collection.is_empty() {
                return Err(Error::Query("falta la colección donde guardar el backup (por ejemplo nodelocal://1/backups)".into()));
            }
            let what = match opt(options, "scope") {
                "cluster" => String::new(),
                "database" | "" => {
                    let db = database.as_deref().filter(|d| !d.is_empty()).ok_or_else(|| Error::Query("elegí la base de datos".into()))?;
                    format!(" DATABASE {}", q(db))
                }
                other => return Err(Error::Query(format!("'{other}' no es un alcance de backup"))),
            };
            let into = match opt(options, "type") {
                "incremental" => "INTO LATEST IN ",
                "full" | "" => "INTO ",
                other => return Err(Error::Query(format!("'{other}' no es un tipo de backup"))),
            };
            let mut sql = format!("BACKUP{what} {into}{}", s(collection));
            let as_of = opt(options, "as_of");
            if !as_of.is_empty() {
                sql.push_str(&format!(" AS OF SYSTEM TIME {}", s(as_of)));
            }
            let with: Vec<&str> = [("revision_history", "revision_history"), ("detached", "detached")]
                .into_iter()
                .filter(|(k, _)| flag(options, k, false))
                .map(|(_, w)| w)
                .collect();
            if !with.is_empty() {
                sql.push_str(&format!(" WITH {}", with.join(", ")));
            }
            Ok(sql)
        }
        BackupAction::Restore { source, database, options } => {
            let (collection, subdir) = split_source(source.trim());
            if collection.is_empty() {
                return Err(Error::Query("falta el backup a restaurar (la colección, o la colección con la subcarpeta)".into()));
            }
            let target = database.as_deref().map(str::trim).filter(|d| !d.is_empty());
            let from = Some(opt(options, "from_database")).filter(|d| !d.is_empty()).or(target);
            let Some(from) = from else {
                return Err(Error::Query("falta la base a restaurar".into()));
            };
            let from_where = match &subdir {
                Some(sub) => s(sub),
                None => "LATEST".into(),
            };
            let mut sql = format!("RESTORE DATABASE {} FROM {from_where} IN {}", q(from), s(&collection));
            let mut with = Vec::new();
            if let Some(t) = target.filter(|t| *t != from) {
                with.push(format!("new_db_name = {}", s(t)));
            }
            if flag(options, "detached", false) {
                with.push("detached".into());
            }
            if !with.is_empty() {
                sql.push_str(&format!(" WITH {}", with.join(", ")));
            }
            Ok(sql)
        }
        BackupAction::Delete { .. } => Err(Error::Unsupported(
            "CockroachDB no borra backups con SQL: hay que borrar la subcarpeta del backup en el almacenamiento".into(),
        )),
    }
}

/// `/2026/09/29-124346.56`: the subfolder a full backup gets.
fn is_subdir(segments: &[&str]) -> bool {
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    match segments {
        [y, m, d] => {
            digits(y, 4)
                && digits(m, 2)
                && d.split_once('-').is_some_and(|(day, time)| digits(day, 2) && time.split_once('.').is_some_and(|(hms, _)| digits(hms, 6)))
        }
        _ => false,
    }
}

/// A backup's id: the collection with its subfolder appended to the path
/// (`nodelocal://1/backups/2026/09/29-124346.56`), query string kept last.
fn backup_id(collection: &str, subdir: &str) -> String {
    let (base, query) = collection.split_once('?').map_or((collection, None), |(b, q)| (b, Some(q)));
    let mut id = format!("{}/{}", base.trim_end_matches('/'), subdir.trim_start_matches('/'));
    if let Some(q) = query {
        id.push('?');
        id.push_str(q);
    }
    id
}

/// An id (or a location the user typed) as collection + subfolder; a bare
/// collection restores its latest backup.
fn split_source(source: &str) -> (String, Option<String>) {
    let (base, query) = source.split_once('?').map_or((source, None), |(b, q)| (b, Some(q)));
    let base = base.trim_end_matches('/');
    let segments: Vec<&str> = base.rsplitn(4, '/').collect();
    if segments.len() == 4 {
        let tail = [segments[2], segments[1], segments[0]];
        if is_subdir(&tail) {
            let mut collection = segments[3].to_string();
            if let Some(q) = query {
                collection.push('?');
                collection.push_str(q);
            }
            return (collection, Some(format!("/{}", tail.join("/"))));
        }
    }
    (source.to_string(), None)
}

/// What a `BACKUP` job's description says.
#[derive(Debug, PartialEq)]
struct BackupJob {
    /// `None`: the whole cluster; empty: tables.
    databases: Option<Vec<String>>,
    /// `LATEST` for an incremental one.
    subdir: String,
    collection: String,
}

/// A tiny reader over a job description (`BACKUP DATABASE a, "B" INTO
/// '/2026/…' IN 'nodelocal://1/x' WITH OPTIONS (…)`).
struct Reader<'a>(&'a str);

impl Reader<'_> {
    fn skip_ws(&mut self) {
        self.0 = self.0.trim_start();
    }

    fn keyword(&mut self, kw: &str) -> bool {
        self.skip_ws();
        let ok = self.0.len() >= kw.len()
            && self.0[..kw.len()].eq_ignore_ascii_case(kw)
            && !self.0[kw.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_');
        if ok {
            self.0 = &self.0[kw.len()..];
        }
        ok
    }

    fn punct(&mut self, c: char) -> bool {
        self.skip_ws();
        match self.0.strip_prefix(c) {
            Some(rest) => {
                self.0 = rest;
                true
            }
            None => false,
        }
    }

    /// A quoted run with doubled quotes as escapes.
    fn quoted(&mut self, quote: char) -> Option<String> {
        self.skip_ws();
        let rest = self.0.strip_prefix(quote)?;
        let mut out = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == quote {
                if chars.peek().is_some_and(|(_, n)| *n == quote) {
                    out.push(quote);
                    chars.next();
                } else {
                    self.0 = &rest[i + c.len_utf8()..];
                    return Some(out);
                }
            } else {
                out.push(c);
            }
        }
        None
    }

    /// A possibly dotted name; quoted parts keep their case.
    fn name(&mut self) -> Option<String> {
        let mut parts = Vec::new();
        loop {
            self.skip_ws();
            let part = if self.0.starts_with('"') {
                self.quoted('"')?
            } else {
                let end = self.0.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(self.0.len());
                if end == 0 {
                    return None;
                }
                let (p, rest) = self.0.split_at(end);
                self.0 = rest;
                p.to_string()
            };
            parts.push(part);
            if !self.0.starts_with('.') {
                return Some(parts.join("."));
            }
            self.0 = &self.0[1..];
        }
    }

    fn names(&mut self) -> Option<Vec<String>> {
        let mut out = vec![self.name()?];
        while self.punct(',') {
            out.push(self.name()?);
        }
        Some(out)
    }
}

fn parse_job(description: &str) -> Option<BackupJob> {
    let mut r = Reader(description);
    if !r.keyword("BACKUP") {
        return None;
    }
    let databases = if r.keyword("DATABASE") {
        Some(r.names()?)
    } else if r.keyword("TABLE") {
        r.names()?;
        Some(Vec::new())
    } else {
        None
    };
    if !r.keyword("INTO") {
        return None;
    }
    let subdir = if r.keyword("LATEST") { "LATEST".to_string() } else { r.quoted('\'')? };
    if !r.keyword("IN") {
        return None;
    }
    let collection = r.quoted('\'')?;
    Some(BackupJob { databases, subdir, collection })
}

fn job_status(status: &str) -> String {
    match status {
        "succeeded" => "completado",
        "failed" => "falló",
        "running" => "en curso",
        "pending" => "pendiente",
        "paused" => "en pausa",
        "pause-requested" => "pausándose",
        "canceled" => "cancelado",
        "cancel-requested" => "cancelándose",
        "reverting" => "revirtiendo",
        other => other,
    }
    .to_string()
}

/// How many backups get their size read (one `SHOW BACKUP` each).
const SIZED: usize = 20;

async fn cockroach_history(s: &PgSession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    // `SHOW JOBS` cuts long descriptions; `SHOW JOBS <ids>` doesn't.
    let rows = s
        .text(
            "SELECT job_id::STRING AS id, description, status, created::STRING AS created_at, \
             finished::STRING AS finished_at, fraction_completed::STRING AS fraction, error, user_name \
             FROM [SHOW JOBS (SELECT job_id FROM [SHOW JOBS] WHERE job_type = 'BACKUP')] ORDER BY created DESC, job_id DESC",
        )
        .await?;
    // An incremental backup names the subfolder of the full one it adds
    // to (or `LATEST` until it resolves it): the oldest job of a subfolder
    // is the full backup, the later ones are incrementals.
    let mut fulls = HashSet::new();
    let mut incrementals = HashSet::new();
    for r in rows.iter().rev() {
        let Some(job) = cell(r, "description").as_deref().and_then(parse_job) else { continue };
        if job.subdir == "LATEST" || !fulls.insert((job.collection, job.subdir)) {
            incrementals.insert(cell(r, "id").unwrap_or_default());
        }
    }
    let mut out = Vec::new();
    for r in &rows {
        let description = cell(r, "description").unwrap_or_default();
        let job = parse_job(&description);
        let databases = job.as_ref().and_then(|j| j.databases.clone());
        // Filtering by database keeps its backups and the cluster's.
        if let (Some(db), Some(dbs)) = (database, &databases) {
            if !dbs.iter().any(|d| d == db) {
                continue;
            }
        }
        let status = cell(r, "status").unwrap_or_default();
        let id = cell(r, "id").unwrap_or_default();
        let incremental = incrementals.contains(&id);
        let mut details = vec![("Job".to_string(), id.clone())];
        details.push((
            "Alcance".into(),
            match &databases {
                None => "Todo el clúster".into(),
                Some(d) if d.is_empty() => "Tablas".into(),
                Some(d) => d.join(", "),
            },
        ));
        if let Some(u) = cell(r, "user_name").filter(|u| !u.is_empty()) {
            details.push(("Usuario".into(), u));
        }
        if status == "running" {
            if let Some(f) = cell(r, "fraction").and_then(|f| f.parse::<f64>().ok()) {
                details.push(("Avance".into(), format!("{:.0} %", f * 100.0)));
            }
        }
        if let Some(e) = cell(r, "error").filter(|e| !e.is_empty()) {
            details.push(("Error".into(), e));
        }
        if incremental {
            details.push(("Restauración".into(), "Se restaura desde su backup completo, que incluye los incrementales".into()));
        }
        details.push(("Sentencia".into(), description.clone()));
        let restorable = status == "succeeded" && !incremental && job.is_some();
        out.push(BackupEntry {
            id: match &job {
                Some(j) if !incremental => backup_id(&j.collection, &j.subdir),
                _ => id,
            },
            database: databases.as_ref().filter(|d| d.len() == 1).map(|d| d[0].clone()),
            kind: Some(if incremental { "Incremental" } else { "Completo" }.into()),
            started: cell(r, "created_at").map(|t| iso(&t)),
            finished: cell(r, "finished_at").filter(|t| !t.is_empty()).map(|t| iso(&t)),
            size: None,
            location: job.as_ref().map(|j| j.collection.clone()),
            status: Some(job_status(&status)),
            details,
            restorable,
        });
    }
    // Sizes of the newest completed full backups (the storage may be slow
    // or gone: a failure leaves the size empty).
    for e in out.iter_mut().filter(|e| e.restorable).take(SIZED) {
        let (collection, Some(subdir)) = split_source(&e.id) else { continue };
        let sql = format!(
            "SELECT sum(size_bytes)::STRING AS size FROM [SHOW BACKUP FROM {} IN {}]",
            lit(Variant::Postgres, &subdir),
            lit(Variant::Postgres, &collection)
        );
        if let Ok(rows) = s.text_within(&sql, Duration::from_secs(10)).await {
            e.size = rows.first().and_then(|r| cell(r, "size")).and_then(|v| v.parse().ok());
        }
    }
    Ok(out)
}

// -- CrateDB ---------------------------------------------------------------

fn crate_spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("repository", "Repositorio", FieldKind::Text)
                .required()
                .placeholder("backups")
                .help("El repositorio donde se guarda el snapshot (sys.repositories)."),
            Field::new("create_repository", "Crear el repositorio (tipo fs)", FieldKind::Bool)
                .default_value("false")
                .help("Crea el repositorio antes del snapshot. La carpeta tiene que estar en path.repo de cada nodo."),
            Field::new("location", "Carpeta del repositorio", FieldKind::Text)
                .placeholder("/data/backups")
                .when("create_repository", &["true"]),
            Field::new("snapshot", "Nombre del snapshot", FieldKind::Text)
                .placeholder("(dbine_ y la fecha)")
                .help("En minúsculas, sin espacios."),
            Field::new("tables", "Tablas", FieldKind::Text)
                .placeholder("(todas)")
                .help("Opcional: esquema.tabla separadas por comas. Vacío: todo el clúster, con usuarios y configuración."),
            Field::new("wait", "Esperar a que termine", FieldKind::Bool).default_value("true"),
        ],
        restore: true,
        restore_options: vec![
            Field::new("tables", "Tablas", FieldKind::Text)
                .placeholder("(todo el snapshot)")
                .help("Opcional: esquema.tabla separadas por comas. Las tablas no pueden existir: borralas antes."),
            Field::new("wait", "Esperar a que termine", FieldKind::Bool).default_value("true"),
        ],
        delete: true,
        history: true,
        server_wide: true,
        script_database: "",
        note: "Los snapshots se guardan en un repositorio (CREATE REPOSITORY): el tipo fs necesita que la carpeta esté en path.repo \
               del crate.yml de cada nodo; también hay s3, azure, gcs y url. Abarcan todo el clúster o las tablas elegidas. \
               Para restaurar, las tablas no pueden existir: borralas antes.",
    }
}

/// `repo.snap`, or with quoted parts when a name has a dot.
fn snapshot_id(repo: &str, snap: &str) -> String {
    let part = |p: &str| if p.contains('.') || p.contains('"') { q(p) } else { p.to_string() };
    format!("{}.{}", part(repo), part(snap))
}

/// A snapshot id as `"repo"."snap"`.
fn snapshot_ref(id: &str) -> Result<String> {
    let bad = || Error::Query(format!("'{id}' no es un snapshot (repositorio.snapshot)"));
    let mut r = Reader(id.trim());
    let mut parts = Vec::new();
    loop {
        r.skip_ws();
        let part = if r.0.starts_with('"') {
            r.quoted('"').ok_or_else(bad)?
        } else {
            let end = r.0.find(['.', '"']).unwrap_or(r.0.len());
            let (p, rest) = r.0.split_at(end);
            r.0 = rest;
            p.trim().to_string()
        };
        parts.push(part);
        if !r.punct('.') {
            break;
        }
    }
    r.skip_ws();
    match parts.as_slice() {
        [repo, snap] if r.0.is_empty() && !repo.is_empty() && !snap.is_empty() => Ok(format!("{}.{}", q(repo), q(snap))),
        _ => Err(bad()),
    }
}

/// `doc.t1, t2` as quoted names.
fn table_list(text: &str) -> Result<Vec<String>> {
    text.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| {
            let parts: Vec<&str> = t.splitn(2, '.').map(|p| p.trim().trim_matches('"')).collect();
            if parts.iter().any(|p| p.is_empty()) {
                return Err(Error::Query(format!("'{t}' no es un nombre de tabla")));
            }
            Ok(parts.iter().map(|p| q(p)).collect::<Vec<_>>().join("."))
        })
        .collect()
}

/// Days since 1970-01-01 as (year, month, day), proleptic Gregorian.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `dbine_20260929_124500` (UTC).
fn default_snapshot_name(unix: i64) -> String {
    let (y, m, d) = civil(unix.div_euclid(86_400));
    let secs = unix.rem_euclid(86_400);
    format!("dbine_{y:04}{m:02}{d:02}_{:02}{:02}{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

fn crate_script(action: &BackupAction) -> Result<String> {
    let wait = |options: &BTreeMap<String, String>| format!(" WITH (wait_for_completion = {})", flag(options, "wait", true));
    match action {
        BackupAction::Backup { options, .. } => {
            let repo = opt(options, "repository");
            if repo.is_empty() {
                return Err(Error::Query("falta el repositorio del snapshot".into()));
            }
            let snap = match opt(options, "snapshot") {
                "" => {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
                    default_snapshot_name(now)
                }
                name => name.to_string(),
            };
            let mut sql = String::new();
            if flag(options, "create_repository", false) {
                let location = opt(options, "location");
                if location.is_empty() {
                    return Err(Error::Query("falta la carpeta del repositorio a crear".into()));
                }
                sql.push_str(&format!("CREATE REPOSITORY {} TYPE fs WITH (location = {});\n", q(repo), s(location)));
            }
            let tables = table_list(opt(options, "tables"))?;
            let what = if tables.is_empty() { "ALL".to_string() } else { format!("TABLE {}", tables.join(", ")) };
            sql.push_str(&format!("CREATE SNAPSHOT {}.{} {what}{}", q(repo), q(&snap), wait(options)));
            Ok(sql)
        }
        BackupAction::Restore { source, options, .. } => {
            let snap = snapshot_ref(source)?;
            let tables = table_list(opt(options, "tables"))?;
            let what = if tables.is_empty() { "ALL".to_string() } else { format!("TABLE {}", tables.join(", ")) };
            Ok(format!("RESTORE SNAPSHOT {snap} {what}{}", wait(options)))
        }
        BackupAction::Delete { source } => Ok(format!("DROP SNAPSHOT {}", snapshot_ref(source)?)),
    }
}

fn crate_state(state: &str) -> String {
    match state {
        "SUCCESS" => "completado",
        "FAILED" => "falló",
        "IN_PROGRESS" => "en curso",
        "PARTIAL" => "parcial",
        "INCOMPATIBLE" => "incompatible",
        other => other,
    }
    .to_string()
}

async fn crate_history(s: &PgSession) -> Result<Vec<BackupEntry>> {
    let full = "SELECT s.repository, s.name, s.state, s.started, s.finished, s.version, \
                array_to_string(s.tables, ', ') AS tables, array_to_string(s.failures, '; ') AS failures, s.reason, \
                s.total_shards::TEXT AS shards, r.type AS repo_type, \
                coalesce(r.settings['location'], r.settings['bucket'], r.settings['container'], r.settings['url']) AS location, \
                r.settings['base_path'] AS base_path \
                FROM sys.snapshots s LEFT JOIN sys.repositories r ON r.name = s.repository ORDER BY s.started DESC";
    // Older servers: fewer columns.
    let short = "SELECT repository, name, state, started, finished, version \
                 FROM sys.snapshots ORDER BY started DESC";
    let rows = match s.text(full).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("cratedb: full snapshot query failed: {e}");
            s.text(short).await?
        }
    };
    Ok(rows
        .iter()
        .map(|r| {
            let repo = cell(r, "repository").unwrap_or_default();
            let name = cell(r, "name").unwrap_or_default();
            let state = cell(r, "state").unwrap_or_default();
            let mut details = vec![("Repositorio".to_string(), repo.clone())];
            for (col, label) in [
                ("repo_type", "Tipo de repositorio"),
                ("tables", "Tablas"),
                ("version", "Versión"),
                ("shards", "Shards"),
                ("failures", "Fallas"),
                ("reason", "Motivo"),
            ] {
                if let Some(v) = cell(r, col).filter(|v| !v.is_empty()) {
                    details.push((label.into(), v));
                }
            }
            let location = match (cell(r, "location"), cell(r, "base_path").filter(|p| !p.is_empty())) {
                (Some(l), Some(p)) => Some(format!("{l}/{p}")),
                (l, _) => l,
            };
            BackupEntry {
                id: snapshot_id(&repo, &name),
                database: None,
                kind: Some("Snapshot".into()),
                started: cell(r, "started").map(|t| iso(&t)),
                finished: cell(r, "finished").filter(|t| !t.is_empty()).map(|t| iso(&t)),
                size: None,
                location: location.or(Some(repo)),
                status: Some(crate_state(&state)),
                details,
                restorable: matches!(state.as_str(), "SUCCESS" | "PARTIAL"),
            }
        })
        .collect())
}

// -- H2 --------------------------------------------------------------------

fn h2_spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new(
                "format",
                "Formato",
                FieldKind::Select(vec![
                    ("script", "SCRIPT: SQL con estructura y datos (se restaura desde DBine)"),
                    ("zip", "BACKUP: copia del archivo de la base en un .zip"),
                ]),
            )
            .default_value("script"),
            Field::new("file", "Archivo en el servidor", FieldKind::Text)
                .required()
                .placeholder("/ruta/backup.sql")
                .help("Una ruta del servidor H2; si es relativa, es relativa a su carpeta de trabajo."),
            Field::new("drop", "Incluir DROP de cada tabla", FieldKind::Bool)
                .default_value("true")
                .help("Así el script reemplaza las tablas al restaurarlo.")
                .when("format", &["script"]),
        ],
        restore: true,
        restore_options: Vec::new(),
        delete: false,
        history: false,
        server_wide: true,
        script_database: "",
        note: "El archivo queda en el disco del servidor H2. SCRIPT escribe SQL que se restaura con RUNSCRIPT (indicá la ruta del archivo); \
               BACKUP copia el archivo de la base en un .zip que solo se restaura con el servidor detenido (org.h2.tools.Restore). \
               H2 no guarda un historial de backups ni los borra.",
    }
}

fn h2_script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { options, .. } => {
            let file = opt(options, "file");
            if file.is_empty() {
                return Err(Error::Query("falta el archivo donde guardar el backup".into()));
            }
            match opt(options, "format") {
                "zip" => Ok(format!("BACKUP TO {}", s(file))),
                "script" | "" => {
                    let drop = if flag(options, "drop", true) { " DROP" } else { "" };
                    Ok(format!("SCRIPT{drop} TO {}", s(file)))
                }
                other => Err(Error::Query(format!("'{other}' no es un formato de backup de H2"))),
            }
        }
        BackupAction::Restore { source, .. } => {
            let file = source.trim();
            if file.is_empty() {
                return Err(Error::Query("falta el archivo del backup (un script hecho con SCRIPT)".into()));
            }
            Ok(format!("RUNSCRIPT FROM {}", s(file)))
        }
        BackupAction::Delete { .. } => {
            Err(Error::Unsupported("H2 no borra archivos con SQL: el backup está en el disco del servidor".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn backup(v: Variant, db: Option<&str>, pairs: &[(&str, &str)]) -> Result<String> {
        script(v, &BackupAction::Backup { database: db.map(Into::into), options: o(pairs) })
    }

    fn restore(v: Variant, source: &str, db: Option<&str>, pairs: &[(&str, &str)]) -> Result<String> {
        script(v, &BackupAction::Restore { source: source.into(), database: db.map(Into::into), options: o(pairs) })
    }

    #[test]
    fn only_variants_with_sql_backups_offer_them() {
        for v in Variant::ALL {
            let has = spec(v).is_some();
            assert_eq!(has, matches!(v, Variant::Cockroach | Variant::CrateDb | Variant::H2), "{v:?}");
            if !has {
                let r = backup(v, Some("x"), &[]);
                assert!(matches!(r, Err(Error::Unsupported(_))), "{v:?}");
            }
        }
    }

    #[test]
    fn cockroach_backups_quote_and_combine_options() {
        let c = Variant::Cockroach;
        assert_eq!(
            backup(c, Some("ven\"tas"), &[("collection", "nodelocal://1/b'k")]).unwrap(),
            "BACKUP DATABASE \"ven\"\"tas\" INTO 'nodelocal://1/b''k'"
        );
        assert_eq!(
            backup(
                c,
                Some("app"),
                &[
                    ("collection", "s3://bk/x?AUTH=implicit"),
                    ("type", "incremental"),
                    ("as_of", "-10s"),
                    ("revision_history", "true"),
                    ("detached", "true"),
                ]
            )
            .unwrap(),
            "BACKUP DATABASE \"app\" INTO LATEST IN 's3://bk/x?AUTH=implicit' AS OF SYSTEM TIME '-10s' WITH revision_history, detached"
        );
        assert_eq!(
            backup(c, Some("app"), &[("collection", "nodelocal://1/b"), ("scope", "cluster")]).unwrap(),
            "BACKUP INTO 'nodelocal://1/b'"
        );
        assert!(backup(c, Some("app"), &[]).is_err());
        assert!(backup(c, None, &[("collection", "nodelocal://1/b")]).is_err());
        assert!(backup(c, Some("app"), &[("collection", "x"), ("type", "diff")]).is_err());
    }

    #[test]
    fn cockroach_restores_from_a_subfolder_or_the_latest() {
        let c = Variant::Cockroach;
        assert_eq!(
            restore(c, "nodelocal://1/b/2026/09/29-124346.56", Some("app"), &[]).unwrap(),
            "RESTORE DATABASE \"app\" FROM '/2026/09/29-124346.56' IN 'nodelocal://1/b'"
        );
        assert_eq!(
            restore(c, "s3://bk/x/2026/09/29-124346.56?AUTH=implicit", Some("app_copia"), &[("from_database", "app"), ("detached", "true")])
                .unwrap(),
            "RESTORE DATABASE \"app\" FROM '/2026/09/29-124346.56' IN 's3://bk/x?AUTH=implicit' WITH new_db_name = 'app_copia', detached"
        );
        assert_eq!(
            restore(c, "nodelocal://1/b", None, &[("from_database", "app")]).unwrap(),
            "RESTORE DATABASE \"app\" FROM LATEST IN 'nodelocal://1/b'"
        );
        assert!(restore(c, "nodelocal://1/b", None, &[]).is_err());
        assert!(matches!(script(c, &BackupAction::Delete { source: "x".into() }), Err(Error::Unsupported(_))));
    }

    #[test]
    fn backup_ids_round_trip() {
        for (collection, subdir) in [
            ("nodelocal://1/b", "/2026/09/29-124346.56"),
            ("nodelocal://1/b/", "/2026/09/29-124346.56"),
            ("s3://bk/x?AUTH=implicit&X=1", "/2026/01/02-030405.00"),
        ] {
            let id = backup_id(collection, subdir);
            let (c, s) = split_source(&id);
            assert_eq!(c, collection.replace("b/", "b"));
            assert_eq!(s.as_deref(), Some(subdir));
        }
        assert_eq!(split_source("nodelocal://1/backups"), ("nodelocal://1/backups".into(), None));
        assert_eq!(split_source("nodelocal://1/a/b/c"), ("nodelocal://1/a/b/c".into(), None));
    }

    #[test]
    fn job_descriptions_parse() {
        assert_eq!(
            parse_job("BACKUP DATABASE bk_probe INTO '/2026/09/29-124346.56' IN 'nodelocal://1/dbine-probe' WITH OPTIONS (revision_history = true)"),
            Some(BackupJob {
                databases: Some(vec!["bk_probe".into()]),
                subdir: "/2026/09/29-124346.56".into(),
                collection: "nodelocal://1/dbine-probe".into(),
            })
        );
        assert_eq!(
            parse_job("BACKUP DATABASE a, \"B c\" INTO 'LATEST' IN 's3://x?AUTH=implicit'").unwrap().databases,
            Some(vec!["a".into(), "B c".into()])
        );
        let cluster = parse_job("BACKUP INTO '/2026/09/29-124432.88' IN 'nodelocal://1/o''k' AS OF SYSTEM TIME '-1s'").unwrap();
        assert_eq!(cluster.databases, None);
        assert_eq!(cluster.collection, "nodelocal://1/o'k");
        assert_eq!(parse_job("BACKUP TABLE db.public.t INTO '/x' IN 'y'").unwrap().databases, Some(Vec::new()));
        assert_eq!(parse_job("BACKUP DATABASE x TO 'nodelocal://1/old'"), None);
        assert_eq!(parse_job("RESTORE DATABASE x FROM LATEST IN 'y'"), None);
    }

    #[test]
    fn timestamps_become_iso() {
        assert_eq!(iso("2026-09-29 12:43:46+00"), "2026-09-29T12:43:46+00:00");
        assert_eq!(iso("2026-09-29 12:46:00.179-03"), "2026-09-29T12:46:00.179-03:00");
        assert_eq!(iso("2026-09-29T12:43:46+00:00"), "2026-09-29T12:43:46+00:00");
    }

    #[test]
    fn crate_snapshots() {
        let c = Variant::CrateDb;
        assert_eq!(
            backup(c, None, &[("repository", "bk"), ("snapshot", "s1")]).unwrap(),
            "CREATE SNAPSHOT \"bk\".\"s1\" ALL WITH (wait_for_completion = true)"
        );
        assert_eq!(
            backup(
                c,
                None,
                &[
                    ("repository", "my repo"),
                    ("create_repository", "true"),
                    ("location", "/data/o'k"),
                    ("snapshot", "s1"),
                    ("tables", "doc.t1, t\"2"),
                    ("wait", "false"),
                ]
            )
            .unwrap(),
            "CREATE REPOSITORY \"my repo\" TYPE fs WITH (location = '/data/o''k');\n\
             CREATE SNAPSHOT \"my repo\".\"s1\" TABLE \"doc\".\"t1\", \"t\"\"2\" WITH (wait_for_completion = false)"
        );
        let auto = backup(c, None, &[("repository", "bk")]).unwrap();
        assert!(auto.starts_with("CREATE SNAPSHOT \"bk\".\"dbine_"), "{auto}");
        assert!(backup(c, None, &[]).is_err());
        assert!(backup(c, None, &[("repository", "bk"), ("create_repository", "true")]).is_err());

        assert_eq!(restore(c, "bk.s1", None, &[]).unwrap(), "RESTORE SNAPSHOT \"bk\".\"s1\" ALL WITH (wait_for_completion = true)");
        assert_eq!(
            restore(c, "bk.s1", None, &[("tables", "doc.t1")]).unwrap(),
            "RESTORE SNAPSHOT \"bk\".\"s1\" TABLE \"doc\".\"t1\" WITH (wait_for_completion = true)"
        );
        assert_eq!(script(c, &BackupAction::Delete { source: "bk.s1".into() }).unwrap(), "DROP SNAPSHOT \"bk\".\"s1\"");
        let dotted = snapshot_id("my.repo", "s1");
        assert_eq!(dotted, "\"my.repo\".s1");
        assert_eq!(script(c, &BackupAction::Delete { source: dotted }).unwrap(), "DROP SNAPSHOT \"my.repo\".\"s1\"");
        assert!(script(c, &BackupAction::Delete { source: "solo".into() }).is_err());
        assert_eq!(
            script(c, &BackupAction::Delete { source: "a.b; DROP TABLE x".into() }).unwrap(),
            "DROP SNAPSHOT \"a\".\"b; DROP TABLE x\""
        );
        assert!(script(c, &BackupAction::Delete { source: "a.b.c".into() }).is_err());
    }

    #[test]
    fn snapshot_names_carry_the_utc_date() {
        assert_eq!(default_snapshot_name(0), "dbine_19700101_000000");
        assert_eq!(default_snapshot_name(1_790_685_938), "dbine_20260929_124538");
        assert_eq!(civil(-1), (1969, 12, 31));
    }

    #[test]
    fn h2_scripts() {
        let h = Variant::H2;
        assert_eq!(backup(h, None, &[("file", "/tmp/o'k.sql")]).unwrap(), "SCRIPT DROP TO '/tmp/o''k.sql'");
        assert_eq!(backup(h, None, &[("file", "/tmp/a.sql"), ("drop", "false")]).unwrap(), "SCRIPT TO '/tmp/a.sql'");
        assert_eq!(backup(h, None, &[("file", "/tmp/a.zip"), ("format", "zip")]).unwrap(), "BACKUP TO '/tmp/a.zip'");
        assert!(backup(h, None, &[]).is_err());
        assert_eq!(restore(h, "/tmp/a.sql", None, &[]).unwrap(), "RUNSCRIPT FROM '/tmp/a.sql'");
        assert!(matches!(script(h, &BackupAction::Delete { source: "x".into() }), Err(Error::Unsupported(_))));
    }
}
