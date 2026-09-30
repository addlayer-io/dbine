//! What a clone does differently on document and search engines
//! (MongoDB, CouchDB, Couchbase, Cosmos DB, Elasticsearch, OpenSearch,
//! Solr), where a "table" holds documents rather than rows of fixed
//! columns:
//!
//! - The copy moves whole documents ([`whole_documents`]): the read asks
//!   for no columns, so the driver hands over every field the documents
//!   have (and its metadata: Elasticsearch's `_id` and `_routing`), not the
//!   fields a sample of the structure happened to see, and the load takes
//!   the read's columns. MongoDB to MongoDB moves the raw BSON.
//! - Memberships the clone must not join are left out, with a note
//!   ([`prepare`]): the original's aliases (the clone would answer the
//!   alias's searches twice and leave it without a single write index) and
//!   its lifecycle policy (ILM / ISM would roll it over or delete it as if
//!   it were the original).
//! - Elasticsearch / OpenSearch settings that act on writes (ingest
//!   pipelines, write blocks) are set after the documents are in: the
//!   documents were already processed once, and a block would refuse them.
//! - MongoDB's validator is set after the documents too (`collMod`, which
//!   doesn't check the documents already in): the original may hold
//!   documents from before its validator.
//! - MongoDB's `create` succeeds when the collection already exists with
//!   the same options, so two clones picking the same name at once would
//!   share one collection (and one's cleanup would drop the other's). The
//!   clone is built under a name of its own and renamed at the end;
//!   `renameCollection` fails if the name was taken meanwhile, and then
//!   only the clone's own collection is dropped. Time series collections
//!   can't be renamed: they're built in place.

use super::exec;
use dbine_driver::{Driver, Error, Family, QueryOutcome, Result, Session};
use serde_json::{Map, Value};

use super::ClonePlan;

/// Engines whose copy moves whole documents (see the module docs).
pub(super) fn whole_documents(driver: &dyn Driver) -> bool {
    matches!(driver.info().family, Family::Document | Family::Search)
}

/// What [`prepare`] left for [`finish`].
#[derive(Debug, Default)]
pub(super) struct Prepared {
    /// The name the clone gets at the end (it's built under
    /// `plan.table.name`).
    pub rename_to: Option<String>,
    /// Statements run after the documents and the indexes.
    pub after: Vec<String>,
}

/// Table options that make the object a member of something shared: never
/// carried to the clone.
const MEMBERSHIPS: &[(&str, &str)] = &[
    ("aliases", "el clon no se suma a los alias del original ({}): con los dos detrás del alias, sus búsquedas devolverían cada documento dos veces y las escrituras no tendrían un único índice de destino"),
    ("lifecycle", "el clon no queda bajo la política de ciclo de vida del original ({}): la política lo trataría como al original (rollover, borrado); asignásela a mano si corresponde"),
];

/// Elasticsearch / OpenSearch settings deferred until the documents are in.
fn deferred_setting(key: &str) -> bool {
    matches!(key, "index.default_pipeline" | "index.final_pipeline") || key.starts_with("index.blocks.")
}

/// A unique suffix for the staging name.
fn unique_tag() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    format!("{:08x}", super::fnv(&format!("{}-{nanos}-{n}", std::process::id())))
}

/// The plan adjusted for this engine (see the module docs), with notes.
pub(super) fn prepare(driver: &dyn Driver, plan: &mut ClonePlan, notes: &mut Vec<String>) -> Result<Prepared> {
    let info = driver.info();
    let mut out = Prepared::default();
    let t = &mut plan.table;
    for (key, note) in MEMBERSHIPS {
        if let Some(v) = t.options.remove(*key) {
            let v = v.trim();
            if !v.is_empty() {
                let shown = serde_json::from_str::<Map<String, Value>>(v).map(|m| m.values().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join(", ")).unwrap_or_else(|_| v.to_string());
                notes.push(note.replace("{}", &shown));
            }
        }
    }
    if info.family == Family::Search {
        if let Some(text) = t.options.get("settings_extra").cloned() {
            let mut all: Map<String, Value> =
                serde_json::from_str(&text).map_err(|_| Error::State("la configuración del índice no es un objeto JSON".into()))?;
            let later: Map<String, Value> = all.iter().filter(|(k, _)| deferred_setting(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
            if !later.is_empty() {
                all.retain(|k, _| !deferred_setting(k));
                if all.is_empty() {
                    t.options.remove("settings_extra");
                } else {
                    t.options.insert("settings_extra".into(), Value::Object(all).to_string());
                }
                let body = serde_json::to_string_pretty(&Value::Object(later)).unwrap_or_default();
                out.after.push(format!("PUT /{}/_settings\n{body}", t.name.trim()));
            }
        }
    }
    if matches!(info.id, "mongodb" | "ferretdb" | "documentdb") && !t.options.contains_key("timeField") {
        let final_name = t.name.clone();
        t.name = format!("{final_name}__dbine_tmp_{}", unique_tag());
        out.rename_to = Some(final_name);
    }
    if matches!(info.id, "mongodb" | "ferretdb" | "documentdb") {
        // The validator after the documents: the original may hold
        // documents from before it (or kept under `moderate`), and `collMod`
        // doesn't check the ones already in.
        let at: Vec<usize> = t.checks.iter().enumerate().filter(|(_, c)| c.expression.trim_start().starts_with('{')).map(|(i, _)| i).collect();
        if let [i] = at[..] {
            let c = t.checks.remove(i);
            let w: Map<String, Value> =
                serde_json::from_str(&c.expression).map_err(|_| Error::State("el validador de la colección no es un objeto JSON".into()))?;
            let mut parts = vec![format!("\"collMod\": {}", Value::String(t.name.clone()))];
            for k in ["validator", "validationLevel", "validationAction"] {
                if let Some(v) = w.get(k) {
                    parts.push(format!("{}: {v}", Value::String(k.into())));
                }
            }
            out.after.push(format!("db.runCommand({{ {} }})", parts.join(", ")));
        }
    }
    Ok(out)
}

/// Elasticsearch / OpenSearch: an index whose `_source` leaves fields out
/// (`_source.includes` / `excludes`) can't be read whole, so a clone with
/// its data would lose those fields. Refused, with the fields.
pub(super) fn check_whole_source(driver: &dyn Driver, t: &dbine_driver::TableSchema) -> Result<()> {
    if driver.info().family != Family::Search {
        return Ok(());
    }
    let Some(m) = t.options.get("mappings_extra").and_then(|s| serde_json::from_str::<Value>(s).ok()) else { return Ok(()) };
    let Some(src) = m.get("_source") else { return Ok(()) };
    let mut listed = Vec::new();
    for k in ["includes", "excludes"] {
        let items: Vec<String> = match src.get(k) {
            Some(Value::Array(a)) => a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect(),
            Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
            _ => Vec::new(),
        };
        if !items.is_empty() {
            listed.push(format!("_source.{k}: {}", items.join(", ")));
        }
    }
    if listed.is_empty() {
        return Ok(());
    }
    Err(Error::Unsupported(format!(
        "el índice «{}» no guarda en _source los documentos completos ({}): los campos que quedan afuera no se pueden leer, y el clon los perdería; no se clona con datos",
        t.name,
        listed.join("; ")
    )))
}

/// The database a MongoDB session works on (`dbStats`' `db`).
async fn mongo_database(s: &mut dyn Session) -> Result<String> {
    let mut out = QueryOutcome::default();
    s.execute(r#"db.runCommand({"dbStats": 1, "scale": 1})"#, 1, &mut out).await?;
    if let Some(e) = out.error {
        return Err(Error::Query(e));
    }
    out.results
        .iter()
        .find_map(|r| {
            let i = r.columns.iter().position(|c| c.name == "db")?;
            r.rows.first()?.get(i)?.as_str().map(str::to_string)
        })
        .filter(|d| !d.is_empty())
        .ok_or_else(|| Error::State("no se pudo saber en qué base está la colección".into()))
}

/// After the documents and the indexes: the deferred settings, then the
/// clone under its final name. An error leaves the clone under its
/// building name (the caller drops it).
pub(super) async fn finish(s: &mut dyn Session, plan: &ClonePlan, p: &Prepared) -> Result<()> {
    for sql in &p.after {
        exec(s, sql).await.map_err(|e| Error::Query(format!("configuración aplicada después de los documentos: {e}")))?;
    }
    if let Some(to) = &p.rename_to {
        let db = mongo_database(s).await?;
        let cmd = serde_json::json!({ "renameCollection": format!("{db}.{}", plan.table.name), "to": format!("{db}.{to}") });
        if let Err(e) = exec(s, &format!("db.adminCommand({cmd})")).await {
            let m = e.to_string().to_lowercase();
            // MongoDB: "target namespace exists"; FerretDB: a duplicate key
            // in its catalog.
            if m.contains("exist") || m.contains("duplicate key") {
                return Err(Error::State(format!("ya existe un objeto llamado «{to}» (se creó mientras se clonaba); elegí otro nombre")));
            }
            return Err(Error::Query(format!("no se pudo dar el nombre final a la colección: {e}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{async_trait, ConnectionConfig, DriverInfo, Language, ObjectKindInfo, TableSchema};

    struct D(DriverInfo);

    #[async_trait]
    impl Driver for D {
        fn info(&self) -> &DriverInfo {
            &self.0
        }
        async fn connect(&self, _: &ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
            Err(Error::Unsupported("test".into()))
        }
    }

    fn driver(id: &'static str, family: Family) -> D {
        D(DriverInfo {
            id,
            name: id,
            family,
            language: Language::Json,
            dialect: "",
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: false,
            object_kinds: vec![ObjectKindInfo::tables()],
        })
    }

    fn plan(name: &str, options: &[(&str, &str)]) -> ClonePlan {
        let table = TableSchema {
            kind: "index".into(),
            name: name.into(),
            options: options.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        };
        ClonePlan { table, renames: vec![], notes: vec![] }
    }

    /// CouchDB's documents are the whole database: refused with the reason
    /// (not "can't drop a half-made clone").
    #[test]
    fn couchdb_is_refused_with_its_reason() {
        let e = super::super::check_cloneable(&driver("couchdb", Family::Document), "table").unwrap_err().to_string();
        assert!(e.contains("la base entera"), "{e}");
        assert!(super::super::check_cloneable(&driver("mongodb", Family::Document), "table").is_ok());
    }

    #[test]
    fn documents_move_whole() {
        assert!(whole_documents(&driver("mongodb", Family::Document)));
        assert!(whole_documents(&driver("opensearch", Family::Search)));
        assert!(!whole_documents(&driver("postgres", Family::Relational)));
    }

    /// The clone never joins the original's aliases or lifecycle policy,
    /// and says so; pipelines and write blocks come after the documents.
    #[test]
    fn search_indexes_leave_memberships_and_defer_write_settings() {
        let os = driver("opensearch", Family::Search);
        let mut p = plan(
            "copia",
            &[
                ("aliases", "al1,al2"),
                ("lifecycle", r#"{"index.lifecycle.name":"pol"}"#),
                ("settings_extra", r#"{"index.blocks.write":"true","index.default_pipeline":"p","index.max_result_window":"50000"}"#),
            ],
        );
        let mut notes = Vec::new();
        let r = prepare(&os, &mut p, &mut notes).unwrap();
        assert!(!p.table.options.contains_key("aliases") && !p.table.options.contains_key("lifecycle"));
        assert!(notes.iter().any(|n| n.contains("al1,al2")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("pol")), "{notes:?}");
        assert_eq!(p.table.options.get("settings_extra").map(String::as_str), Some(r#"{"index.max_result_window":"50000"}"#));
        assert_eq!(r.after.len(), 1);
        assert!(r.after[0].starts_with("PUT /copia/_settings\n"), "{}", r.after[0]);
        let body: Value = serde_json::from_str(r.after[0].split_once('\n').unwrap().1).unwrap();
        assert_eq!(body, serde_json::json!({"index.blocks.write": "true", "index.default_pipeline": "p"}));
        assert!(r.rename_to.is_none());
        assert_eq!(p.table.name, "copia");
    }

    /// MongoDB builds the clone under a name of its own and renames it at
    /// the end (time series collections, which can't be renamed, in place).
    #[test]
    fn mongodb_builds_under_its_own_name() {
        let m = driver("mongodb", Family::Document);
        let mut p = plan("c_20260930_070509", &[]);
        let r = prepare(&m, &mut p, &mut Vec::new()).unwrap();
        assert_eq!(r.rename_to.as_deref(), Some("c_20260930_070509"));
        assert!(p.table.name.starts_with("c_20260930_070509__dbine_tmp_"), "{}", p.table.name);
        let mut q = plan("c_20260930_070509", &[]);
        prepare(&m, &mut q, &mut Vec::new()).unwrap();
        assert_ne!(p.table.name, q.table.name, "two clones never share a building name");
        let mut ts = plan("t2", &[("timeField", "at")]);
        let r = prepare(&m, &mut ts, &mut Vec::new()).unwrap();
        assert!(r.rename_to.is_none());
        assert_eq!(ts.table.name, "t2");
    }

    /// A validator added after the data: the original keeps documents that
    /// don't satisfy it, so the clone gets it after its documents.
    #[test]
    fn mongodb_validator_comes_after_the_documents() {
        let m = driver("mongodb", Family::Document);
        let mut p = plan("v", &[]);
        p.table.checks.push(dbine_driver::CheckDef {
            name: Some("validator".into()),
            expression: r#"{"validator":{"$jsonSchema":{"required":["n"]}},"validationLevel":"moderate"}"#.into(),
        });
        let r = prepare(&m, &mut p, &mut Vec::new()).unwrap();
        assert!(p.table.checks.is_empty(), "the CREATE carries no validator");
        assert_eq!(r.after.len(), 1);
        let cmd = &r.after[0];
        assert!(cmd.starts_with(&format!("db.runCommand({{ \"collMod\": \"{}\", ", p.table.name)), "{cmd}");
        assert!(cmd.contains(r#""validator": {"$jsonSchema":{"required":["n"]}}"#) && cmd.contains(r#""validationLevel": "moderate""#), "{cmd}");
        assert!(!cmd.contains("validationAction"), "{cmd}");
    }

    /// `_source.excludes` / `includes`: the left-out fields can't be read,
    /// so a clone with data is refused (never a clone that lost them).
    #[test]
    fn filtered_source_is_refused() {
        let os = driver("opensearch", Family::Search);
        let p = plan("i", &[("mappings_extra", r#"{"_source":{"excludes":["secret"]}}"#)]);
        let e = check_whole_source(&os, &p.table).unwrap_err().to_string();
        assert!(e.contains("secret") && e.contains("no se clona"), "{e}");
        let p = plan("i", &[("mappings_extra", r#"{"_source":{"includes":["a.*"]}}"#)]);
        assert!(check_whole_source(&os, &p.table).unwrap_err().to_string().contains("a.*"));
        for ok in [r#"{"_source":{"excludes":[]}}"#, r#"{"_routing":{"required":true}}"#] {
            assert!(check_whole_source(&os, &plan("i", &[("mappings_extra", ok)]).table).is_ok(), "{ok}");
        }
        assert!(check_whole_source(&os, &plan("i", &[]).table).is_ok());
    }
}
