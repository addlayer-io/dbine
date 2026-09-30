//! What "Comparar esquemas" needs beyond columns and keys: CHECK
//! constraints, full-text indexes with their settings, and the sequences,
//! synonyms and table types as runnable DDL.

use crate::{err, format_type, int, quote, read_lob, text, HanaSession};
use dbine_driver::{kinds, CheckDef, DbObject, IndexDef, Result, TableSchema};
use std::collections::{BTreeMap, HashMap};

/// `IndexDef::kind` of a full-text index.
pub const FULLTEXT: &str = "FULLTEXT";

/// Rows of a catalog query as column name → value (NULLs left out), for
/// views whose columns vary between HANA versions.
async fn named_rows(s: &HanaSession, sql: &str, params: &[&str]) -> Result<Vec<BTreeMap<String, String>>> {
    let response = s.conn.prepare_and_execute(sql, &params.to_vec()).await.map_err(err)?;
    let rs = response.into_result_set().map_err(err)?;
    let names: Vec<String> = rs.metadata().iter().map(|f| f.columnname().to_string()).collect();
    let mut out = Vec::new();
    for row in rs.into_rows().await.map_err(err)? {
        let mut m = BTreeMap::new();
        for (name, v) in names.iter().zip(row) {
            if let Some(t) = text(&read_lob(v).await) {
                m.insert(name.clone(), t);
            }
        }
        out.push(m);
    }
    Ok(out)
}

// ------------------------------------------------------------ catalog

const CHECKS: &str = "SELECT TABLE_NAME, CONSTRAINT_NAME, CHECK_CONDITION FROM SYS.CONSTRAINTS
 WHERE SCHEMA_NAME = ? AND CHECK_CONDITION IS NOT NULL
 ORDER BY TABLE_NAME, CONSTRAINT_NAME";

const FULLTEXT_INDEXES: &str = "SELECT * FROM SYS.FULLTEXT_INDEXES WHERE SCHEMA_NAME = ?";

/// How a SYS.FULLTEXT_INDEXES setting is written in `CREATE FULLTEXT INDEX`.
#[derive(Clone, Copy)]
enum Clause {
    /// `KEYWORD "column"`
    Column,
    /// `KEYWORD ('a', 'b')` from `a,b`
    List,
    /// `KEYWORD 'text'`
    Text,
    /// `KEYWORD ON | OFF` from TRUE / FALSE
    OnOff,
    /// `KEYWORD n`
    Number,
}

/// The settings a full-text index is compared and made with, in the order
/// `CREATE FULLTEXT INDEX` takes them. Keys are SYS.FULLTEXT_INDEXES'
/// column names.
const FULLTEXT_SETTINGS: &[(&str, &str, Clause)] = &[
    ("LANGUAGE_COLUMN", "LANGUAGE COLUMN", Clause::Column),
    ("LANGUAGE_DETECTION", "LANGUAGE DETECTION", Clause::List),
    ("MIME_TYPE_COLUMN", "MIME TYPE COLUMN", Clause::Column),
    ("FUZZY_SEARCH_INDEX", "FUZZY SEARCH INDEX", Clause::OnOff),
    ("PHRASE_INDEX_RATIO", "PHRASE INDEX RATIO", Clause::Number),
    ("CONFIGURATION", "CONFIGURATION", Clause::Text),
    ("SEARCH_ONLY", "SEARCH ONLY", Clause::OnOff),
    ("FAST_PREPROCESS", "FAST PREPROCESS", Clause::OnOff),
    ("TEXT_ANALYSIS", "TEXT ANALYSIS", Clause::OnOff),
    ("MIME_TYPE", "MIME TYPE", Clause::Text),
    ("TOKEN_SEPARATORS", "TOKEN SEPARATORS", Clause::Text),
    ("TEXT_MINING", "TEXT MINING", Clause::OnOff),
    ("TEXT_MINING_CONFIGURATION", "TEXT MINING CONFIGURATION", Clause::Text),
    ("TEXT_MINING_CONFIGURATION_OVERLAY", "TEXT MINING CONFIGURATION OVERLAY", Clause::Text),
];
/// Asynchronous indexes: flushed every N minutes and/or after N documents.
const FLUSH_MINUTES: &str = "FLUSH_EVERY_MINUTES";
const FLUSH_DOCUMENTS: &str = "FLUSH_AFTER_DOCUMENTS";
/// SYNC / ASYNC, under whichever name the version reports it.
const SYNC_COLUMNS: &[&str] = &["SYNCHRONIZATION_TYPE", "SYNC_TYPE", "IS_SYNCHRONOUS", "SYNCHRONOUS"];

/// A full-text index's options from its SYS.FULLTEXT_INDEXES row.
pub(crate) fn fulltext_options(row: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut o = BTreeMap::new();
    let keys = FULLTEXT_SETTINGS.iter().map(|(k, _, _)| *k).chain([FLUSH_MINUTES, FLUSH_DOCUMENTS]).chain(SYNC_COLUMNS.iter().copied());
    for k in keys {
        if let Some(v) = row.get(k).map(|v| v.trim()).filter(|v| !v.is_empty()) {
            o.insert(k.to_string(), v.to_string());
        }
    }
    o
}

/// Fills in the CHECK constraints and full-text indexes of `tables`.
pub(crate) async fn complete(s: &HanaSession, tables: &mut [TableSchema]) {
    let schema = s.schema.as_str();
    let at: HashMap<String, usize> = tables.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();

    match s.rows(CHECKS, &[schema]).await {
        Ok(rows) => {
            for r in rows {
                let t = |i: usize| r.get(i).and_then(text);
                let Some(&ti) = t(0).and_then(|n| at.get(&n)) else { continue };
                let Some(expr) = t(2).map(|e| e.trim().to_string()).filter(|e| !e.is_empty()) else { continue };
                // One row per column of the constraint: keep the first.
                let name = t(1).filter(|n| !n.starts_with("_SYS_"));
                let checks = &mut tables[ti].checks;
                if !checks.iter().any(|c| c.name == name && c.expression == expr) {
                    checks.push(CheckDef { name, expression: expr });
                }
            }
        }
        Err(e) => tracing::warn!("hana: CHECK constraints not read: {e}"),
    }

    match named_rows(s, FULLTEXT_INDEXES, &[schema]).await {
        Ok(rows) => {
            for r in rows {
                let Some(&ti) = r.get("TABLE_NAME").and_then(|n| at.get(n)) else { continue };
                let Some(name) = r.get("INDEX_NAME") else { continue };
                let options = fulltext_options(&r);
                let t = &mut tables[ti];
                match t.indexes.iter_mut().find(|i| &i.name == name) {
                    Some(ix) => {
                        ix.kind = Some(FULLTEXT.into());
                        ix.options = options;
                    }
                    None => t.indexes.push(IndexDef {
                        name: name.clone(),
                        columns: r.get("COLUMN_NAME").cloned().into_iter().collect(),
                        kind: Some(FULLTEXT.into()),
                        options,
                        ..Default::default()
                    }),
                }
            }
        }
        Err(e) => tracing::warn!("hana: full-text indexes not read: {e}"),
    }
    for t in tables.iter_mut() {
        for ix in t.indexes.iter_mut().filter(|i| i.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(FULLTEXT))) {
            ix.unique = false;
        }
        t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

// ---------------------------------------------------------------- DDL

pub(crate) fn is_fulltext(ix: &IndexDef) -> bool {
    ix.kind.as_deref().is_some_and(|k| k.trim().eq_ignore_ascii_case(FULLTEXT))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `CREATE FULLTEXT INDEX … ON t (c)` with its settings.
pub(crate) fn fulltext_sql(table: &str, ix: &IndexDef) -> String {
    let cols: Vec<String> = ix.columns.iter().map(|c| quote(c)).collect();
    let mut s = format!("CREATE FULLTEXT INDEX {} ON {table} ({})", quote(&ix.name), cols.join(", "));
    let o = |k: &str| ix.options.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    for (key, keyword, clause) in FULLTEXT_SETTINGS {
        let Some(v) = o(key) else { continue };
        let value = match clause {
            Clause::Column => quote(v),
            Clause::List => format!("({})", v.split(',').map(|x| lit(x.trim())).collect::<Vec<_>>().join(", ")),
            Clause::Text => lit(v),
            Clause::OnOff => (if v.eq_ignore_ascii_case("TRUE") || v.eq_ignore_ascii_case("ON") { "ON" } else { "OFF" }).to_string(),
            Clause::Number => v.to_string(),
        };
        s.push_str(&format!(" {keyword} {value}"));
    }
    let sync = SYNC_COLUMNS.iter().find_map(|k| o(k)).map(|v| v.to_ascii_uppercase());
    let (minutes, docs) = (o(FLUSH_MINUTES).filter(|v| *v != "0"), o(FLUSH_DOCUMENTS).filter(|v| *v != "0"));
    match sync.as_deref() {
        Some("TRUE" | "SYNC" | "SYNCHRONOUS") => s.push_str(" SYNC"),
        _ if minutes.is_some() || docs.is_some() => {
            s.push_str(" ASYNC FLUSH");
            match (minutes, docs) {
                (Some(m), Some(d)) => s.push_str(&format!(" EVERY {m} MINUTES OR AFTER {d} DOCUMENTS")),
                (Some(m), None) => s.push_str(&format!(" EVERY {m} MINUTES")),
                (None, Some(d)) => s.push_str(&format!(" AFTER {d} DOCUMENTS")),
                (None, None) => {}
            }
        }
        Some(_) => s.push_str(" ASYNC"),
        None => {}
    }
    s
}

// ------------------------------------------------------------ objects

/// Synonyms and table types of the schema, and public synonyms of its
/// objects (listed under the PUBLIC schema).
const OBJECTS: &str = "
SELECT 'synonym', CAST(NULL AS NVARCHAR(256)), SYNONYM_NAME FROM SYS.SYNONYMS WHERE SCHEMA_NAME = ?
UNION ALL SELECT 'synonym', 'PUBLIC', SYNONYM_NAME FROM SYS.SYNONYMS WHERE SCHEMA_NAME = 'PUBLIC' AND OBJECT_SCHEMA = ?
UNION ALL SELECT 'type', NULL, TABLE_NAME FROM SYS.TABLES
 WHERE SCHEMA_NAME = ? AND IS_USER_DEFINED_TYPE = 'TRUE' AND TABLE_NAME NOT LIKE '\\_SYS%' ESCAPE '\\'
ORDER BY 3";

pub(crate) async fn list_objects(s: &HanaSession) -> Vec<DbObject> {
    let schema = s.schema.as_str();
    match s.rows(OBJECTS, &[schema, schema, schema]).await {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| Some(DbObject { kind: text(r.first()?)?, schema: r.get(1).and_then(text), name: text(r.get(2)?)?, parent: None }))
            .collect(),
        Err(e) => {
            tracing::warn!("hana: synonyms and table types not listed: {e}");
            Vec::new()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SequenceInfo {
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cycle: bool,
    pub cache: i64,
    pub reset_by: Option<String>,
}

pub(crate) fn sequence_sql(name: &str, s: &SequenceInfo) -> String {
    let mut out = format!(
        "CREATE SEQUENCE {} INCREMENT BY {} START WITH {} MINVALUE {} MAXVALUE {} {}",
        quote(name),
        s.increment,
        s.start,
        s.min,
        s.max,
        if s.cycle { "CYCLE" } else { "NO CYCLE" }
    );
    out.push_str(&if s.cache > 0 { format!(" CACHE {}", s.cache) } else { " NO CACHE".into() });
    if let Some(q) = s.reset_by.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        out.push_str(&format!(" RESET BY {q}"));
    }
    out.push(';');
    out
}

/// `CREATE [PUBLIC] SYNONYM`; the target's schema is left out when it's the
/// session's, so the synonym points at the same object on either side.
pub(crate) fn synonym_sql(public: bool, name: &str, schema: &str, target_schema: Option<&str>, target: &str) -> String {
    let owner = target_schema.filter(|o| *o != schema).map(|o| format!("{}.", quote(o))).unwrap_or_default();
    format!("CREATE {}SYNONYM {} FOR {owner}{};", if public { "PUBLIC " } else { "" }, quote(name), quote(target))
}

pub(crate) struct TypeColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

pub(crate) fn table_type_sql(name: &str, cols: &[TypeColumn]) -> String {
    let body: Vec<String> =
        cols.iter().map(|c| format!("    {} {}{}", quote(&c.name), c.data_type, if c.nullable { "" } else { " NOT NULL" })).collect();
    format!("CREATE TYPE {} AS TABLE (\n{}\n);", quote(name), body.join(",\n"))
}

const SEQUENCE: &str = "SELECT START_NUMBER, INCREMENT_BY, MIN_VALUE, MAX_VALUE, IS_CYCLED, CACHE_SIZE, RESET_BY_QUERY
  FROM SYS.SEQUENCES WHERE SCHEMA_NAME = ? AND SEQUENCE_NAME = ?";

const TYPE_COLUMNS: &str = "SELECT COLUMN_NAME, DATA_TYPE_NAME, LENGTH, SCALE, IS_NULLABLE FROM SYS.TABLE_COLUMNS
 WHERE SCHEMA_NAME = ? AND TABLE_NAME = ? ORDER BY POSITION";

/// The CREATE statement of a sequence, synonym or table type; `None` for
/// other kinds (or when the catalog can't be read, for sequences).
pub(crate) async fn definition(s: &HanaSession, kind: &str, owner: &str, name: &str) -> Result<Option<String>> {
    match kind {
        kinds::SEQUENCE => {
            let rows = match s.rows(SEQUENCE, &[owner, name]).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("hana: sequence {name} not read from SYS.SEQUENCES: {e}");
                    return Ok(None);
                }
            };
            Ok(rows.first().map(|r| {
                let n = |i: usize, d: i64| r.get(i).and_then(int).unwrap_or(d);
                let t = |i: usize| r.get(i).and_then(text);
                let info = SequenceInfo {
                    start: n(0, 1),
                    increment: n(1, 1),
                    min: n(2, 1),
                    max: n(3, 4_611_686_018_427_387_903),
                    cycle: t(4).as_deref() == Some("TRUE"),
                    cache: n(5, 0),
                    reset_by: t(6),
                };
                sequence_sql(name, &info)
            }))
        }
        kinds::SYNONYM => {
            let rows = s.rows("SELECT OBJECT_SCHEMA, OBJECT_NAME FROM SYS.SYNONYMS WHERE SCHEMA_NAME = ? AND SYNONYM_NAME = ?", &[owner, name]).await?;
            Ok(rows.first().and_then(|r| {
                let target = r.get(1).and_then(text)?;
                Some(synonym_sql(owner == "PUBLIC", name, &s.schema, r.first().and_then(text).as_deref(), &target))
            }))
        }
        kinds::TYPE => {
            let rows = s.rows(TYPE_COLUMNS, &[owner, name]).await?;
            let cols: Vec<TypeColumn> = rows
                .iter()
                .map(|r| {
                    let t = |i: usize| r.get(i).and_then(text);
                    TypeColumn {
                        name: t(0).unwrap_or_default(),
                        data_type: format_type(&t(1).unwrap_or_default(), r.get(2).and_then(int), r.get(3).and_then(int)),
                        nullable: t(4).as_deref() != Some("FALSE"),
                    }
                })
                .collect();
            Ok((!cols.is_empty()).then(|| table_type_sql(name, &cols)))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fulltext_settings_round_trip() {
        let row: BTreeMap<String, String> = [
            ("SCHEMA_NAME", "APP"),
            ("TABLE_NAME", "DOCS"),
            ("INDEX_NAME", "FTI_DOCS"),
            ("INDEX_OID", "123"),
            ("LANGUAGE_DETECTION", "EN,DE"),
            ("FUZZY_SEARCH_INDEX", "TRUE"),
            ("SEARCH_ONLY", "FALSE"),
            ("CONFIGURATION", "LINGANALYSIS_FULL"),
            ("PHRASE_INDEX_RATIO", "0.2"),
            ("TOKEN_SEPARATORS", "/;,.'"),
            ("FLUSH_EVERY_MINUTES", "5"),
            ("FLUSH_AFTER_DOCUMENTS", "100"),
            ("LANGUAGE_COLUMN", "IDIOMA"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let options = fulltext_options(&row);
        assert!(!options.contains_key("INDEX_OID") && !options.contains_key("TABLE_NAME"));
        assert_eq!(options.len(), 9);
        let ix = IndexDef { name: "FTI_DOCS".into(), columns: vec!["CUERPO".into()], kind: Some(FULLTEXT.into()), options, ..Default::default() };
        assert_eq!(
            fulltext_sql("\"DOCS\"", &ix),
            "CREATE FULLTEXT INDEX \"FTI_DOCS\" ON \"DOCS\" (\"CUERPO\") LANGUAGE COLUMN \"IDIOMA\" LANGUAGE DETECTION ('EN', 'DE') \
             FUZZY SEARCH INDEX ON PHRASE INDEX RATIO 0.2 CONFIGURATION 'LINGANALYSIS_FULL' SEARCH ONLY OFF \
             TOKEN SEPARATORS '/;,.''' ASYNC FLUSH EVERY 5 MINUTES OR AFTER 100 DOCUMENTS"
        );
        let mut sync = ix.clone();
        sync.options = [("SYNCHRONIZATION_TYPE".to_string(), "SYNCHRONOUS".to_string())].into();
        assert_eq!(fulltext_sql("\"DOCS\"", &sync), "CREATE FULLTEXT INDEX \"FTI_DOCS\" ON \"DOCS\" (\"CUERPO\") SYNC");
    }

    #[test]
    fn objects_ddl() {
        let s = SequenceInfo { start: 100, increment: 5, min: 1, max: 4_611_686_018_427_387_903, cycle: false, cache: 10, reset_by: None };
        assert_eq!(
            sequence_sql("SEQ_FOLIO", &s),
            "CREATE SEQUENCE \"SEQ_FOLIO\" INCREMENT BY 5 START WITH 100 MINVALUE 1 MAXVALUE 4611686018427387903 NO CYCLE CACHE 10;"
        );
        let s = SequenceInfo { cache: 0, cycle: true, reset_by: Some("SELECT MAX(ID) + 1 FROM T".into()), ..s };
        assert!(sequence_sql("S", &s).ends_with(" CYCLE NO CACHE RESET BY SELECT MAX(ID) + 1 FROM T;"));
        assert_eq!(synonym_sql(false, "SYN", "APP", Some("APP"), "DOCS"), "CREATE SYNONYM \"SYN\" FOR \"DOCS\";");
        assert_eq!(synonym_sql(true, "SYN", "APP", Some("HR"), "EMP"), "CREATE PUBLIC SYNONYM \"SYN\" FOR \"HR\".\"EMP\";");
        let cols = [
            TypeColumn { name: "ID".into(), data_type: "INTEGER".into(), nullable: false },
            TypeColumn { name: "NOMBRE".into(), data_type: "NVARCHAR(50)".into(), nullable: true },
        ];
        assert_eq!(table_type_sql("TT_LINEAS", &cols), "CREATE TYPE \"TT_LINEAS\" AS TABLE (\n    \"ID\" INTEGER NOT NULL,\n    \"NOMBRE\" NVARCHAR(50)\n);");
    }
}
