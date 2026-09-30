//! Native backups (docs/backups.md). SolrCloud backs up a collection with
//! the Collections API (BACKUP / LISTBACKUP / RESTORE / DELETEBACKUP): a
//! backup is a name inside a location (a folder of the server, or a
//! repository such as S3 or GCS) holding incremental points, each with a
//! backupId. A standalone server backs up a core with its replication
//! handler (`/replication?command=backup|restore|deletebackup`), which
//! writes a `snapshot.<name>` folder.
//!
//! Collections and cores live under the server (the only database is
//! "default"), so the tab opens from the connection. The scripts are
//! console requests; the mode can't be known without a session, so the
//! backup form asks for it and an entry's id carries it:
//! `cloud:<name>/<backupId>` or `core:<core>/<snapshot>`, plus
//! `@<location>` when the backup isn't in the default folder.

use crate::SolrSession;
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use dbine_driver_elasticsearch::json::J;
use std::collections::BTreeMap;

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("mode", "Modo del servidor", FieldKind::Select(vec![
                ("cloud", "SolrCloud (Collections API)"),
                ("standalone", "Standalone (replication handler)"),
            ]))
            .default_value("cloud"),
            Field::new("collection", "Colección o core", FieldKind::Text).required(),
            Field::new("location", "Carpeta en el servidor", FieldKind::Text)
                .placeholder("/var/solr/backups (vacío = la de por defecto)")
                .help("Tiene que existir y estar en solr.allowPaths (o dentro de SOLR_HOME). En SolrCloud, vacío usa la propiedad location del cluster o la del repositorio; en standalone, la carpeta de datos del core."),
            Field::new("set_default", "Guardarla como carpeta por defecto del cluster", FieldKind::Bool)
                .default_value("true")
                .help("Fija la propiedad location del cluster (CLUSTERPROP). El historial busca los backups ahí.")
                .when("mode", &["cloud"]),
            Field::new("name", "Nombre del backup", FieldKind::Text)
                .placeholder("vacío = el de la colección")
                .help("Los backups con el mismo nombre son puntos incrementales del mismo backup. El historial lista los que se llaman como la colección.")
                .when("mode", &["cloud"]),
            Field::new("repository", "Repositorio", FieldKind::Text)
                .placeholder("vacío = el de por defecto")
                .help("Repositorio de backups definido en solr.xml (S3, GCS, HDFS…).")
                .when("mode", &["cloud"]),
            Field::new("max_points", "Puntos a conservar", FieldKind::Number)
                .placeholder("vacío = todos")
                .help("Después del backup borra los puntos más viejos de ese nombre (maxNumBackupPoints).")
                .when("mode", &["cloud"]),
            Field::new("snapshot", "Nombre del snapshot", FieldKind::Text)
                .placeholder("vacío = la fecha y hora")
                .help("Solr lo guarda en la carpeta snapshot.<nombre>.")
                .when("mode", &["standalone"]),
        ],
        restore: true,
        restore_options: Vec::new(),
        delete: true,
        history: true,
        server_wide: true,
        script_database: "",
        note: "En SolrCloud cada backup es de una colección y se restaura en una colección nueva; en standalone, de un core, y se restaura en ese core o en uno nuevo. La carpeta tiene que existir en el servidor y estar en solr.allowPaths (o dentro de SOLR_HOME). En standalone Solr solo informa el último backup de cada core.",
    }
}

fn opt<'a>(options: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    options.get(key).map(|v| v.trim()).unwrap_or_default()
}

/// A query-string value, percent-encoded (`/` and `:` kept, for paths and URIs).
fn enc(v: &str) -> String {
    let mut out = String::new();
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A core or collection name as a path segment.
fn segment(name: &str) -> Result<String> {
    if name.is_empty() || name.contains(['/', '?', '#', '&', ' ']) {
        return Err(Error::Query(format!("'{name}' no es un nombre de colección o core válido")));
    }
    Ok(enc(name))
}

fn get(path: &str, params: &[(&str, &str)]) -> String {
    let qs: Vec<String> = params.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}={}", enc(v))).collect();
    format!("GET {path}?{}\n", qs.join("&"))
}

const COLLECTIONS: &str = "/solr/admin/collections";

/// A parsed entry id.
#[derive(Debug, PartialEq)]
struct Source<'a> {
    cloud: bool,
    /// The backup name (SolrCloud) or the core (standalone).
    name: &'a str,
    /// The backupId (SolrCloud) or the snapshot name (standalone).
    point: Option<&'a str>,
    location: Option<&'a str>,
}

/// `cloud:<name>/<backupId>@<location>` or `core:<core>/<snapshot>@<location>`;
/// the prefix, the point and the location are optional (no prefix = SolrCloud).
fn parse_source(source: &str) -> Result<Source<'_>> {
    let s = source.trim();
    let (cloud, rest) = if let Some(r) = s.strip_prefix("core:") {
        (false, r)
    } else {
        (true, s.strip_prefix("cloud:").unwrap_or(s))
    };
    let (head, location) = match rest.split_once('@') {
        Some((h, l)) => (h, Some(l.trim()).filter(|l| !l.is_empty())),
        None => (rest, None),
    };
    let (name, point) = match head.split_once('/') {
        Some((n, p)) => (n.trim(), Some(p.trim()).filter(|p| !p.is_empty())),
        None => (head.trim(), None),
    };
    if name.is_empty() {
        return Err(Error::Query(format!(
            "'{source}' no es un backup: tiene que ser cloud:<nombre>/<backupId> o core:<core>/<snapshot>, con @<carpeta> opcional"
        )));
    }
    if !cloud && point.is_none() {
        return Err(Error::Query(format!("'{source}' no dice el snapshot: core:<core>/<snapshot>")));
    }
    Ok(Source { cloud, name, point, location })
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { options, .. } => {
            let coll = opt(options, "collection");
            if coll.is_empty() {
                return Err(Error::Query("Falta la colección o el core".into()));
            }
            let location = opt(options, "location");
            if opt(options, "mode") == "standalone" {
                let path = format!("/solr/{}/replication", segment(coll)?);
                return Ok(get(&path, &[("command", "backup"), ("name", opt(options, "snapshot")), ("location", location)]));
            }
            let mut out = String::new();
            if !location.is_empty() && opt(options, "set_default") != "false" {
                out.push_str(&get(COLLECTIONS, &[("action", "CLUSTERPROP"), ("name", "location"), ("val", location)]));
                out.push('\n');
            }
            let name = match opt(options, "name") {
                "" => coll,
                n => n,
            };
            let max = opt(options, "max_points");
            if !max.is_empty() && max.parse::<u32>().is_err() {
                return Err(Error::Query(format!("'{max}' no es una cantidad de puntos válida")));
            }
            out.push_str(&get(
                COLLECTIONS,
                &[
                    ("action", "BACKUP"),
                    ("name", name),
                    ("collection", coll),
                    ("location", location),
                    ("repository", opt(options, "repository")),
                    ("maxNumBackupPoints", max),
                ],
            ));
            Ok(out)
        }
        BackupAction::Restore { source, database, .. } => {
            let src = parse_source(source)?;
            let target = database.as_deref().map(str::trim).filter(|d| !d.is_empty() && *d != "default");
            let location = src.location.unwrap_or_default();
            if src.cloud {
                let target = target.ok_or_else(|| Error::Query("Falta la colección nueva donde restaurar".into()))?;
                return Ok(get(
                    COLLECTIONS,
                    &[
                        ("action", "RESTORE"),
                        ("name", src.name),
                        ("collection", target),
                        ("backupId", src.point.unwrap_or_default()),
                        ("location", location),
                    ],
                ));
            }
            let core = target.unwrap_or(src.name);
            let mut out = String::new();
            if core != src.name {
                // DBine's own request: creates the core (with _default) unless it exists.
                out.push_str(&format!("PUT /solr/{}?if_not_exists=true\n\n", segment(core)?));
            }
            let path = format!("/solr/{}/replication", segment(core)?);
            out.push_str(&get(&path, &[("command", "restore"), ("name", src.point.unwrap_or_default()), ("location", location)]));
            out.push('\n');
            out.push_str(&get(&path, &[("command", "restorestatus")]));
            Ok(out)
        }
        BackupAction::Delete { source } => {
            let src = parse_source(source)?;
            let location = src.location.unwrap_or_default();
            if src.cloud {
                let id = src.point.ok_or_else(|| Error::Query(format!("'{source}' no dice el backupId: cloud:<nombre>/<backupId>")))?;
                return Ok(get(COLLECTIONS, &[("action", "DELETEBACKUP"), ("name", src.name), ("backupId", id), ("location", location)]));
            }
            let path = format!("/solr/{}/replication", segment(src.name)?);
            Ok(get(&path, &[("command", "deletebackup"), ("name", src.point.unwrap_or_default()), ("location", location)]))
        }
    }
}

// -- history ---------------------------------------------------------------

fn text(j: &J, key: &str) -> Option<String> {
    j.get(key).map(J::text).filter(|s| !s.is_empty())
}

/// The points of a SolrCloud LISTBACKUP reply, newest first.
pub fn cloud_entries(name: &str, location: Option<&str>, resp: &J) -> Vec<BackupEntry> {
    let coll = text(resp, "collection");
    let mut out: Vec<BackupEntry> = resp
        .get("backups")
        .and_then(J::as_arr)
        .unwrap_or_default()
        .iter()
        .map(|b| {
            let id = b.get("backupId").map(J::text).unwrap_or_default();
            let mut details = vec![("Nombre del backup".to_string(), name.to_string())];
            for (k, label) in [("indexFileCount", "Archivos del índice"), ("indexVersion", "Versión del índice"), ("collection.configName", "Configset")] {
                if let Some(v) = text(b, k) {
                    details.push((label.into(), v));
                }
            }
            BackupEntry {
                id: match location {
                    Some(l) => format!("cloud:{name}/{id}@{l}"),
                    None => format!("cloud:{name}/{id}"),
                },
                database: text(b, "collectionAlias").or_else(|| coll.clone()),
                kind: Some(if id == "0" { "Completo".into() } else { "Incremental".into() }),
                started: text(b, "startTime"),
                finished: text(b, "endTime"),
                size: b.get("indexSizeMB").and_then(|v| v.text().parse::<f64>().ok()).map(|mb| (mb * 1024.0 * 1024.0) as u64),
                location: location.map(|l| format!("{}/{name}", l.trim_end_matches('/'))),
                status: Some("completo".into()),
                details,
                restorable: true,
            }
        })
        .collect();
    out.reverse();
    out
}

/// A core's last backup, from the replication handler's `details`.
pub fn core_entry(core: &str, data_dir: Option<&str>, details: &J) -> Option<BackupEntry> {
    let b = details.at(&["details", "backup"])?;
    let snap = text(b, "snapshotName")?;
    let mut info = Vec::new();
    if let Some(v) = text(b, "fileCount") {
        info.push(("Archivos".to_string(), v));
    }
    if let Some(v) = text(b, "exception") {
        info.push(("Error".to_string(), v));
    }
    info.push(("Carpeta".to_string(), "Solr no informa la carpeta: si no fue la de datos del core, cambiala después de la @ del origen.".into()));
    let dir = text(b, "directoryName").unwrap_or_else(|| format!("snapshot.{snap}"));
    Some(BackupEntry {
        id: match data_dir {
            Some(d) => format!("core:{core}/{snap}@{d}"),
            None => format!("core:{core}/{snap}"),
        },
        database: Some(core.to_string()),
        kind: Some("Snapshot".into()),
        started: text(b, "startTime"),
        finished: text(b, "snapshotCompletedAt").or_else(|| text(b, "endTime")),
        size: None,
        location: Some(match data_dir {
            Some(d) => format!("{}/{dir}", d.trim_end_matches('/')),
            None => dir,
        }),
        status: text(b, "status"),
        details: info,
        restorable: true,
    })
}

pub async fn history(s: &SolrSession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    let only = database.filter(|d| !d.is_empty() && *d != "default");
    let keep = |n: &str| !n.starts_with('.') && only.is_none_or(|o| o == n);
    let mut out = Vec::new();
    if s.cloud {
        let status = s.get_json("/solr/admin/collections?action=CLUSTERSTATUS").await?;
        let location = status.at(&["cluster", "properties", "location"]).map(J::text).filter(|l| !l.is_empty());
        let list = s.get_json("/solr/admin/collections?action=LIST").await?;
        for c in list.get("collections").and_then(J::as_arr).unwrap_or_default() {
            let Some(name) = c.as_str().filter(|n| keep(n)) else { continue };
            match s.get_json(&format!("{COLLECTIONS}?action=LISTBACKUP&name={}", enc(name))).await {
                Ok(r) => out.extend(cloud_entries(name, location.as_deref(), &r)),
                Err(e) if e.to_string().contains("'location' is not specified") => {
                    return Err(Error::Query(
                        "El cluster no tiene carpeta de backups por defecto: hacé un backup con una carpeta y \"Guardarla como carpeta por defecto\", o fijá la propiedad location (CLUSTERPROP).".into(),
                    ))
                }
                // No backup with that name.
                Err(_) => {}
            }
        }
    } else {
        let status = s.get_json("/solr/admin/cores?action=STATUS&indexInfo=false").await?;
        for (core, info) in status.get("status").and_then(J::as_obj).into_iter().flatten() {
            if !keep(core) {
                continue;
            }
            let data_dir = info.get("dataDir").map(J::text).map(|d| d.trim_end_matches('/').to_string());
            if let Ok(d) = s.get_json(&format!("/solr/{}/replication?command=details", enc(core))).await {
                out.extend(core_entry(core, data_dir.as_deref(), &d));
            }
        }
    }
    out.sort_by(|a, b| b.started.cmp(&a.started));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver_elasticsearch::console::{self, Command};

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn paths(script: &str) -> Vec<String> {
        console::parse(script)
            .unwrap()
            .into_iter()
            .map(|c| match c {
                Command::Http(r) => format!("{} {}", r.method, r.path),
                Command::Sql(s) => panic!("SQL {s}"),
            })
            .collect()
    }

    #[test]
    fn cloud_backup() {
        let s = script(&BackupAction::Backup {
            database: None,
            options: opts(&[("mode", "cloud"), ("collection", "films"), ("location", "/var/solr/data/backups"), ("set_default", "true"), ("max_points", "3")]),
        })
        .unwrap();
        assert_eq!(
            paths(&s),
            [
                "GET /solr/admin/collections?action=CLUSTERPROP&name=location&val=/var/solr/data/backups",
                "GET /solr/admin/collections?action=BACKUP&name=films&collection=films&location=/var/solr/data/backups&maxNumBackupPoints=3",
            ]
        );
        let s = script(&BackupAction::Backup { database: None, options: opts(&[("collection", "a b&c"), ("set_default", "false"), ("name", "x")]) }).unwrap();
        assert_eq!(paths(&s), ["GET /solr/admin/collections?action=BACKUP&name=x&collection=a%20b%26c"]);
        assert!(script(&BackupAction::Backup { database: None, options: opts(&[("mode", "cloud")]) }).is_err());
        assert!(script(&BackupAction::Backup { database: None, options: opts(&[("collection", "a"), ("max_points", "x")]) }).is_err());
    }

    #[test]
    fn standalone_backup() {
        let s = script(&BackupAction::Backup { database: None, options: opts(&[("mode", "standalone"), ("collection", "core1"), ("snapshot", "s1")]) })
            .unwrap();
        assert_eq!(paths(&s), ["GET /solr/core1/replication?command=backup&name=s1"]);
        assert!(script(&BackupAction::Backup { database: None, options: opts(&[("mode", "standalone"), ("collection", "a/b")]) }).is_err());
    }

    #[test]
    fn sources() {
        assert_eq!(
            parse_source("cloud:films/2@/var/b").unwrap(),
            Source { cloud: true, name: "films", point: Some("2"), location: Some("/var/b") }
        );
        assert_eq!(parse_source("films").unwrap(), Source { cloud: true, name: "films", point: None, location: None });
        assert_eq!(
            parse_source("core:c1/snap").unwrap(),
            Source { cloud: false, name: "c1", point: Some("snap"), location: None }
        );
        assert!(parse_source("core:c1").is_err());
        assert!(parse_source("").is_err());
    }

    #[test]
    fn restore_and_delete() {
        let r = |source: &str, db: Option<&str>| script(&BackupAction::Restore { source: source.into(), database: db.map(Into::into), options: BTreeMap::new() });
        assert_eq!(
            paths(&r("cloud:films/2@/var/b", Some("films2")).unwrap()),
            ["GET /solr/admin/collections?action=RESTORE&name=films&collection=films2&backupId=2&location=/var/b"]
        );
        assert!(r("cloud:films/2", Some("default")).is_err());
        assert_eq!(
            paths(&r("core:c1/s1@/var/solr/data/c1/data", Some("c2")).unwrap()),
            [
                "PUT /solr/c2?if_not_exists=true",
                "GET /solr/c2/replication?command=restore&name=s1&location=/var/solr/data/c1/data",
                "GET /solr/c2/replication?command=restorestatus",
            ]
        );
        assert_eq!(
            paths(&r("core:c1/s1", None).unwrap()),
            ["GET /solr/c1/replication?command=restore&name=s1", "GET /solr/c1/replication?command=restorestatus"]
        );
        let d = |source: &str| script(&BackupAction::Delete { source: source.into() });
        assert_eq!(paths(&d("cloud:films/0").unwrap()), ["GET /solr/admin/collections?action=DELETEBACKUP&name=films&backupId=0"]);
        assert!(d("cloud:films").is_err());
        assert_eq!(paths(&d("core:c1/s1@/x").unwrap()), ["GET /solr/c1/replication?command=deletebackup&name=s1&location=/x"]);
    }

    #[test]
    fn history_parsing() {
        let r = J::parse(r#"{"collection":"films","backups":[
            {"backupId":0,"indexVersion":"9.12.3","startTime":"2026-09-29T14:05:42Z","endTime":"2026-09-29T14:05:43Z","indexFileCount":13,"indexSizeMB":1.5,"collectionAlias":"films"},
            {"backupId":1,"startTime":"2026-09-29T15:00:00Z","indexSizeMB":2}]}"#)
        .unwrap();
        let e = cloud_entries("films", Some("/var/b"), &r);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].id, "cloud:films/1@/var/b");
        assert_eq!(e[0].kind.as_deref(), Some("Incremental"));
        assert_eq!(e[1].size, Some(1572864));
        assert_eq!(e[1].location.as_deref(), Some("/var/b/films"));
        assert_eq!(e[1].database.as_deref(), Some("films"));
        let d = J::parse(r#"{"details":{"backup":{"startTime":"2026-09-29T14:06:01Z","fileCount":16,"status":"success","snapshotCompletedAt":"2026-09-29T14:06:02Z","snapshotName":"b2","directoryName":"snapshot.b2"}}}"#).unwrap();
        let e = core_entry("c1", Some("/var/solr/data/c1/data"), &d).unwrap();
        assert_eq!(e.id, "core:c1/b2@/var/solr/data/c1/data");
        assert_eq!(e.location.as_deref(), Some("/var/solr/data/c1/data/snapshot.b2"));
        assert_eq!(e.status.as_deref(), Some("success"));
        assert!(core_entry("c1", None, &J::parse(r#"{"details":{}}"#).unwrap()).is_none());
    }
}
