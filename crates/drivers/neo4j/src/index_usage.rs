//! A label's (or relationship type's) indexes and how they're used
//! (`Session::index_usage`).
//!
//! - Neo4j 5: `SHOW INDEXES YIELD *` lists every index with `readCount`
//!   (queries that read it), `lastRead` and `trackedSince` (when the
//!   counting started: the database's start or the index's creation).
//!   One "index used N times" number, with no difference between a seek and
//!   a scan: it goes in `seeks` and `seek_scan_split` is false. Neo4j
//!   counts no writes per index (`writes_counted` false), so no index is
//!   marked "sin uso" (an index never read shows 0 %). The indexes that
//!   back a constraint (`owningConstraint`) are listed under the
//!   constraint's name (what the schema sync drops); a KEY constraint is
//!   the label's primary key, a UNIQUENESS one is unique. The token LOOKUP
//!   indexes cover every label, not one: left out. No sizes.
//!   Neo4j 4 has no `readCount`: the indexes are listed without counters.
//!   `SHOW INDEXES` refused (Enterprise RBAC without `SHOW INDEX`): the
//!   indexes that back a constraint (`SHOW CONSTRAINTS` `ownedIndex`) are
//!   listed without counters and the note names the privilege.
//! - Memgraph: `SHOW INDEX INFO` and `SHOW CONSTRAINT INFO` (the unique
//!   constraints) give the indexes, with no usage counters.
//! - Neptune: no user-defined indexes (`supports_index_usage` is false).
//! - Graphs have no foreign keys: relationships are the links.

use crate::{as_text, strs, CatalogEntry, Flavor, GraphSession, CONSTRAINT, RELATIONSHIP, LABEL};
use dbine_driver::{kinds, IndexUsage, IndexUsageReport, ObjectRef, Result};
use serde_json::{Map, Value};

type Row = Map<String, Value>;

/// A Neo4j date-time (`2024-01-01T01:00:00.123+01:00`) as `yyyy-mm-dd hh:mm:ss`.
fn stamp(v: Option<&Value>) -> Option<String> {
    let s = v.and_then(Value::as_str)?;
    (s.len() >= 19).then(|| s[..19].replace('T', " "))
}

fn count(v: Option<&Value>) -> Option<u64> {
    v.and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
}

/// `obj`'s entity: `Some(false)` a label, `Some(true)` a relationship type,
/// `None` either (the schema compare asks for a "table").
fn wanted(obj: &ObjectRef) -> Option<bool> {
    match obj.kind.as_str() {
        LABEL => Some(false),
        RELATIONSHIP => Some(true),
        _ => None,
    }
}

/// `SHOW INDEXES YIELD *` and `SHOW CONSTRAINTS YIELD *` rows as the report
/// for `name` (a label or relationship type; `rel` as in [`wanted`]).
pub(crate) fn neo4j_report(name: &str, rel: Option<bool>, indexes: &[Row], constraints: &[Row]) -> IndexUsageReport {
    let counted = indexes.iter().any(|r| r.contains_key("readCount"));
    let mut since: Option<String> = None;
    let mut out = Vec::new();
    for r in indexes {
        let t = r.get("type").map(as_text).unwrap_or_default();
        if t == "LOOKUP" || !strs(r.get("labelsOrTypes")).iter().any(|l| l == name) {
            continue;
        }
        let is_rel = r.get("entityType").map(as_text).as_deref() == Some("RELATIONSHIP");
        if rel.is_some_and(|w| w != is_rel) {
            continue;
        }
        let owner = r.get("owningConstraint").filter(|v| !v.is_null()).map(as_text);
        let ctype = owner
            .as_ref()
            .and_then(|o| constraints.iter().find(|c| c.get("name").map(as_text).as_ref() == Some(o)))
            .and_then(|c| c.get("type"))
            .map(as_text)
            .unwrap_or_default();
        let primary_key = ctype.ends_with("_KEY");
        let unique = primary_key || ctype.contains("UNIQUENESS");
        let kind = match (primary_key, unique) {
            (true, _) => format!("{t} (KEY)"),
            (_, true) => format!("{t} (UNIQUE)"),
            _ => t,
        };
        let reads = count(r.get("readCount"));
        if reads.is_some() {
            if let Some(s) = stamp(r.get("trackedSince")) {
                if since.as_ref().is_none_or(|x| s < *x) {
                    since = Some(s);
                }
            }
        }
        out.push(IndexUsage {
            name: owner.unwrap_or_else(|| r.get("name").map(as_text).unwrap_or_default()),
            kind,
            unique,
            primary_key,
            key_columns: strs(r.get("properties")),
            seeks: reads.unwrap_or(0),
            last_read: stamp(r.get("lastRead")),
            ..Default::default()
        });
    }
    IndexUsageReport {
        since,
        stats_available: counted,
        note: Some(if counted {
            "Neo4j cuenta las lecturas de cada índice pero no las escrituras: un índice sin lecturas queda en 0 % y no se marca «sin uso».".into()
        } else {
            "Esta versión de Neo4j no cuenta el uso de los índices (readCount llega en Neo4j 5): se listan sin contadores.".into()
        }),
        indexes: out,
        foreign_keys: Vec::new(),
        seek_scan_split: false,
        writes_counted: false,
    }
}

/// `SHOW INDEXES` was refused (or failed): the indexes that back a
/// constraint, from `SHOW CONSTRAINTS` (`ownedIndex`), without counters,
/// and a note that names the missing privilege.
pub(crate) fn refused_report(name: &str, rel: Option<bool>, constraints: &[Row], error: &str) -> IndexUsageReport {
    let rows: Vec<Row> = constraints
        .iter()
        .filter(|c| c.get("ownedIndex").is_some_and(|v| !v.is_null()))
        .map(|c| {
            let mut r = c.clone();
            r.insert("owningConstraint".into(), c.get("name").cloned().unwrap_or(Value::Null));
            r.insert("type".into(), Value::from("CONSTRAINT"));
            r.remove("readCount");
            r
        })
        .collect();
    let mut r = neo4j_report(name, rel, &rows, constraints);
    r.since = None;
    r.stats_available = false;
    let l = error.to_ascii_lowercase();
    r.note = Some(if l.contains("security") || l.contains("forbidden") || l.contains("permission") || l.contains("denied") || l.contains("not allowed") {
        "El usuario no tiene el privilegio SHOW INDEX (GRANT SHOW INDEX ON DATABASE …): se listan solo los índices de las restricciones, sin contadores.".into()
    } else {
        format!("No se pudieron leer los índices (SHOW INDEXES): {error}. Se listan solo los de las restricciones, sin contadores.")
    });
    r
}

/// Memgraph's catalog entries (indexes and constraints) as the report for `name`.
pub(crate) fn memgraph_report(name: &str, rel: Option<bool>, cat: &[CatalogEntry]) -> IndexUsageReport {
    let indexes = cat
        .iter()
        .filter(|e| e.target.split(',').any(|t| t == name) && rel.is_none_or(|w| w == e.relationship))
        .filter(|e| e.kind == kinds::INDEX || (e.kind == CONSTRAINT && e.spec_kind == "UNIQUE"))
        .map(|e| IndexUsage {
            name: e.name.clone(),
            kind: e.spec_kind.clone(),
            unique: e.spec_kind == "UNIQUE",
            key_columns: e.properties.clone(),
            ..Default::default()
        })
        .collect();
    IndexUsageReport {
        stats_available: false,
        note: Some("Memgraph no lleva la cuenta del uso de sus índices: se listan sin contadores.".into()),
        indexes,
        seek_scan_split: false,
        writes_counted: false,
        ..Default::default()
    }
}

impl GraphSession {
    pub(crate) async fn index_usage_report(&mut self, obj: &ObjectRef) -> Result<Option<IndexUsageReport>> {
        let rel = wanted(obj);
        match self.flavor {
            Flavor::Neo4j => {
                let ix = self.records("SHOW INDEXES YIELD *").await;
                let cs = self.records("SHOW CONSTRAINTS YIELD *").await.unwrap_or_default();
                Ok(Some(match ix {
                    Ok(ix) => neo4j_report(&obj.name, rel, &ix, &cs),
                    Err(e) => refused_report(&obj.name, rel, &cs, &e.to_string()),
                }))
            }
            Flavor::Memgraph => {
                let cat = self.catalog().await?;
                Ok(Some(memgraph_report(&obj.name, rel, &cat)))
            }
            Flavor::Neptune => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: Value) -> Row {
        v.as_object().unwrap().clone()
    }

    fn indexes() -> Vec<Row> {
        vec![
            row(json!({"name": "index_lookup", "type": "LOOKUP", "entityType": "NODE", "labelsOrTypes": null, "properties": null, "readCount": 99, "trackedSince": "2024-01-01T00:00:00Z"})),
            row(json!({"name": "pk_persona", "type": "RANGE", "entityType": "NODE", "labelsOrTypes": ["Persona"], "properties": ["id"], "owningConstraint": "pk_persona", "readCount": 4, "lastRead": "2024-05-01T10:00:00.5Z", "trackedSince": "2024-01-02T00:00:00Z"})),
            row(json!({"name": "ix_nombre", "type": "RANGE", "entityType": "NODE", "labelsOrTypes": ["Persona"], "properties": ["nombre", "apellido"], "owningConstraint": null, "readCount": 6, "lastRead": "2024-05-02T10:00:00Z", "trackedSince": "2024-01-01T12:00:00+01:00"})),
            row(json!({"name": "ix_bio", "type": "TEXT", "entityType": "NODE", "labelsOrTypes": ["Persona"], "properties": ["bio"], "owningConstraint": null, "readCount": 0, "lastRead": null, "trackedSince": "2024-01-03T00:00:00Z"})),
            row(json!({"name": "u_mail", "type": "RANGE", "entityType": "NODE", "labelsOrTypes": ["Persona"], "properties": ["mail"], "owningConstraint": "u_mail", "readCount": 0, "trackedSince": "2024-01-03T00:00:00Z"})),
            row(json!({"name": "ix_desde", "type": "RANGE", "entityType": "RELATIONSHIP", "labelsOrTypes": ["Persona"], "properties": ["desde"], "readCount": 1, "trackedSince": "2023-01-01T00:00:00Z"})),
        ]
    }

    #[test]
    fn show_indexes_refused() {
        let cs = vec![
            row(json!({"name": "pk_persona", "type": "NODE_KEY", "entityType": "NODE", "labelsOrTypes": ["Persona"], "properties": ["id"], "ownedIndex": "pk_persona"})),
            row(json!({"name": "n_nombre", "type": "NODE_PROPERTY_EXISTENCE", "entityType": "NODE", "labelsOrTypes": ["Persona"], "properties": ["nombre"], "ownedIndex": null})),
            row(json!({"name": "u_otro", "type": "UNIQUENESS", "entityType": "NODE", "labelsOrTypes": ["Otro"], "properties": ["x"], "ownedIndex": "u_otro"})),
        ];
        let r = refused_report("Persona", Some(false), &cs, "Neo.ClientError.Security.Forbidden: Permission denied").derived();
        assert!(!r.stats_available && r.since.is_none());
        assert!(r.note.as_deref().unwrap().contains("SHOW INDEX"));
        assert_eq!(r.indexes.len(), 1);
        assert!(r.indexes[0].primary_key && r.indexes[0].name == "pk_persona");
        assert_eq!(r.indexes[0].key_columns, ["id"]);
        let r = refused_report("Persona", None, &[], "connection reset");
        assert!(r.indexes.is_empty() && r.note.unwrap().contains("connection reset"));
    }

    fn constraints() -> Vec<Row> {
        vec![row(json!({"name": "pk_persona", "type": "NODE_KEY"})), row(json!({"name": "u_mail", "type": "UNIQUENESS"}))]
    }

    #[test]
    fn neo4j_rows() {
        let r = neo4j_report("Persona", Some(false), &indexes(), &constraints()).derived();
        assert!(r.stats_available && !r.seek_scan_split);
        let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["pk_persona", "ix_nombre", "ix_bio", "u_mail"]);
        let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap();
        assert!(get("pk_persona").primary_key && get("pk_persona").unique);
        assert_eq!(get("pk_persona").kind, "RANGE (KEY)");
        assert!(get("u_mail").unique && !get("u_mail").primary_key);
        assert_eq!(get("ix_nombre").key_columns, ["nombre", "apellido"]);
        assert_eq!(get("ix_nombre").seeks, 6);
        assert_eq!(get("pk_persona").last_read.as_deref(), Some("2024-05-01 10:00:00"));
        assert_eq!(r.since.as_deref(), Some("2024-01-01 12:00:00"));
        assert_eq!(get("ix_nombre").read_share, Some(0.6));
        assert_eq!(get("ix_bio").read_share, Some(0.0));
        assert!(r.indexes.iter().all(|i| !i.unused && i.seek_health.is_none()));
        // The relationship type of the same name, and either for a "table".
        let r = neo4j_report("Persona", Some(true), &indexes(), &constraints());
        assert_eq!(r.indexes.len(), 1);
        assert_eq!(neo4j_report("Persona", None, &indexes(), &constraints()).indexes.len(), 5);
    }

    #[test]
    fn neo4j_4_has_no_counters() {
        let ix = vec![row(json!({"name": "ix", "type": "BTREE", "entityType": "NODE", "labelsOrTypes": ["A"], "properties": ["x"]}))];
        let r = neo4j_report("A", None, &ix, &[]).derived();
        assert!(!r.stats_available);
        assert_eq!(r.indexes.len(), 1);
        assert!(r.note.unwrap().contains("Neo4j 5"));
    }
}
