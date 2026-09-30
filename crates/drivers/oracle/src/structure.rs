//! What "Comparar esquemas" needs beyond columns and keys: CHECK
//! constraints, index kinds and options (bitmap, reverse, compression,
//! visibility, partitioning, domain indexes such as Oracle Text), and the
//! sequences, synonyms and object / collection types as runnable DDL.

use crate::{err, quote};
use dbine_driver::{CheckDef, IndexDef, Result, TableSchema};
use oracledb::{Connection, Row};
use std::collections::HashMap;

/// Oracle Text (`CTXSYS.*`) domain indexes.
pub const FULLTEXT: &str = "FULLTEXT";
/// Oracle Spatial (`MDSYS.*`) domain indexes.
pub const SPATIAL: &str = "SPATIAL";
/// Any other domain index (user-defined index types).
pub const DOMAIN: &str = "DOMAIN";

fn text(r: &Row, i: usize) -> Result<Option<String>> {
    r.get::<Option<String>>(i).map_err(err)
}

// ------------------------------------------------------------ catalog

/// Index attributes that make two indexes different. PARAMETERS (a
/// VARCHAR2) and the partitioning come along.
const INDEX_ATTRS: &str = "SELECT i.index_name, i.index_type, i.compression, i.prefix_length, i.visibility,
        i.ityp_owner, i.ityp_name, i.parameters, p.locality, p.partitioning_type, p.partition_count
   FROM all_indexes i
   LEFT JOIN all_part_indexes p ON p.owner = i.owner AND p.index_name = i.index_name
  WHERE i.table_owner = :1 AND i.generated = 'N'";

/// Partition key columns of partitioned indexes.
const INDEX_PART_KEYS: &str = "SELECT name, column_name FROM all_part_key_columns
  WHERE owner = :1 AND object_type LIKE 'INDEX%' ORDER BY name, column_position";

/// Partitions of global range-partitioned indexes (HIGH_VALUE is a LONG: last).
const INDEX_PARTITIONS: &str = "SELECT index_name, partition_name, high_value FROM all_ind_partitions
  WHERE index_owner = :1 ORDER BY index_name, partition_position";

/// CHECK constraints (SEARCH_CONDITION is a LONG: last). NOT NULL ones are
/// told apart afterwards: they're the columns' nullability.
const CHECKS: &str = "SELECT table_name, constraint_name, generated, search_condition FROM all_constraints
  WHERE owner = :1 AND constraint_type = 'C' AND table_name NOT LIKE 'BIN$%'
  ORDER BY table_name, constraint_name";

#[derive(Debug, Default, Clone)]
pub(crate) struct IndexAttrs {
    pub index_type: String,
    pub compression: Option<String>,
    pub prefix_length: Option<i64>,
    pub visibility: Option<String>,
    pub indextype: Option<String>,
    pub parameters: Option<String>,
    pub locality: Option<String>,
    pub partitioning: Option<String>,
    pub partition_count: Option<i64>,
    pub part_keys: Vec<String>,
    /// (partition, HIGH_VALUE) of a range-partitioned global index.
    pub partitions: Vec<(String, String)>,
}

/// The kind and options an index gets from its attributes.
pub(crate) fn apply_attrs(ix: &mut IndexDef, a: &IndexAttrs) {
    if a.index_type.ends_with("DOMAIN") {
        let it = a.indextype.clone().unwrap_or_default();
        let owner = it.split('.').next().unwrap_or("").to_ascii_uppercase();
        ix.kind = Some(
            match owner.as_str() {
                "CTXSYS" => FULLTEXT,
                "MDSYS" => SPATIAL,
                _ => DOMAIN,
            }
            .into(),
        );
        ix.options.insert("INDEXTYPE".into(), it);
        if let Some(p) = a.parameters.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            ix.options.insert("PARAMETERS".into(), p.to_string());
        }
    }
    if a.index_type.ends_with("/REV") {
        ix.options.insert("REVERSE".into(), "YES".into());
    }
    match a.compression.as_deref() {
        Some("ENABLED") => {
            ix.options.insert("COMPRESS".into(), a.prefix_length.map(|n| n.to_string()).unwrap_or_default());
        }
        Some(c) if c.starts_with("ADVANCED") => {
            ix.options.insert("COMPRESS".into(), c.to_string());
        }
        _ => {}
    }
    if a.visibility.as_deref() == Some("INVISIBLE") {
        ix.options.insert("VISIBILITY".into(), "INVISIBLE".into());
    }
    if let Some(l) = partition_clause(a) {
        ix.options.insert("LOCALITY".into(), l);
    }
}

/// `LOCAL`, or the `GLOBAL PARTITION BY …` clause of a partitioned index.
fn partition_clause(a: &IndexAttrs) -> Option<String> {
    let locality = a.locality.as_deref()?;
    if locality == "LOCAL" {
        return Some("LOCAL".into());
    }
    let keys = a.part_keys.iter().map(|k| quote(k)).collect::<Vec<_>>().join(", ");
    match a.partitioning.as_deref() {
        Some("HASH") => Some(format!("GLOBAL PARTITION BY HASH ({keys}) PARTITIONS {}", a.partition_count.unwrap_or(1))),
        Some("RANGE") => {
            let parts: Vec<String> =
                a.partitions.iter().map(|(n, hv)| format!("PARTITION {} VALUES LESS THAN ({})", quote(n), hv.trim())).collect();
            Some(format!("GLOBAL PARTITION BY RANGE ({keys}) ({})", parts.join(", ")))
        }
        _ => Some("GLOBAL".into()),
    }
}

/// `"COL" IS NOT NULL`: the NOT NULL constraint of a column.
fn not_null_of(expr: &str) -> Option<&str> {
    let col = expr.trim().strip_suffix("IS NOT NULL")?.trim_end();
    let col = col.strip_prefix('"')?.strip_suffix('"')?;
    (!col.contains('"')).then_some(col)
}

/// Fills in the CHECK constraints and index attributes of `tables`.
pub(crate) fn complete(c: &Connection, owner: &str, tables: &mut [TableSchema]) -> Result<()> {
    let at: HashMap<String, usize> = tables.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();

    for row in c.query(CHECKS, &[&owner]).map_err(err)? {
        let row = row.map_err(err)?;
        let table: String = row.get(0).map_err(err)?;
        let Some(&i) = at.get(&table) else { continue };
        let name: String = row.get(1).map_err(err)?;
        let generated = text(&row, 2)?.as_deref() == Some("GENERATED NAME");
        let Some(expr) = text(&row, 3)?.map(|e| e.trim().to_string()).filter(|e| !e.is_empty()) else { continue };
        let t = &mut tables[i];
        if let Some(col) = not_null_of(&expr) {
            if t.columns.iter().any(|c| c.name == col && !c.nullable) {
                continue;
            }
        }
        // System names (SYS_C…) are left for the target database to pick.
        t.checks.push(CheckDef { name: (!generated).then_some(name), expression: expr });
    }

    let mut attrs: HashMap<String, IndexAttrs> = HashMap::new();
    for row in c.query(INDEX_ATTRS, &[&owner]).map_err(err)? {
        let row = row.map_err(err)?;
        let name: String = row.get(0).map_err(err)?;
        let indextype = match (text(&row, 5)?, text(&row, 6)?) {
            (Some(o), Some(n)) => Some(format!("{o}.{n}")),
            _ => None,
        };
        attrs.insert(
            name,
            IndexAttrs {
                index_type: text(&row, 1)?.unwrap_or_default(),
                compression: text(&row, 2)?,
                prefix_length: row.get(3).map_err(err)?,
                visibility: text(&row, 4)?,
                indextype,
                parameters: text(&row, 7)?,
                locality: text(&row, 8)?,
                partitioning: text(&row, 9)?,
                partition_count: row.get(10).map_err(err)?,
                ..Default::default()
            },
        );
    }
    if attrs.values().any(|a| a.locality.as_deref() == Some("GLOBAL")) {
        for row in c.query(INDEX_PART_KEYS, &[&owner]).map_err(err)? {
            let row = row.map_err(err)?;
            let name: String = row.get(0).map_err(err)?;
            if let Some(a) = attrs.get_mut(&name) {
                a.part_keys.push(row.get(1).map_err(err)?);
            }
        }
        for row in c.query(INDEX_PARTITIONS, &[&owner]).map_err(err)? {
            let row = row.map_err(err)?;
            let name: String = row.get(0).map_err(err)?;
            if let Some(a) = attrs.get_mut(&name).filter(|a| a.locality.as_deref() == Some("GLOBAL") && a.partitioning.as_deref() == Some("RANGE")) {
                a.partitions.push((row.get(1).map_err(err)?, text(&row, 2)?.unwrap_or_default()));
            }
        }
    }
    for t in tables.iter_mut() {
        for ix in t.indexes.iter_mut().filter(|ix| ix.kind.as_deref() != Some(crate::ddl::UNIQUE_CONSTRAINT)) {
            if let Some(a) = attrs.get(&ix.name) {
                apply_attrs(ix, a);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- DDL

/// `CREATE … INDEX` for a plain, bitmap, function-based or domain index,
/// with its attributes.
pub(crate) fn index_sql(table: &str, ix: &IndexDef, cols: &[String]) -> String {
    let o = |k: &str| ix.options.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let domain = matches!(ix.kind.as_deref(), Some(FULLTEXT | SPATIAL | DOMAIN)) || o("INDEXTYPE").is_some();
    let prefix = match (ix.unique, ix.kind.as_deref()) {
        (true, _) => "UNIQUE ",
        (false, Some(k)) if k.eq_ignore_ascii_case("BITMAP") => "BITMAP ",
        _ => "",
    };
    let mut s = format!("CREATE {prefix}INDEX {} ON {table} ({})", quote(&ix.name), cols.join(", "));
    if domain {
        if let Some(it) = o("INDEXTYPE") {
            s.push_str(&format!(" INDEXTYPE IS {it}"));
        }
        if o("LOCALITY") == Some("LOCAL") {
            s.push_str(" LOCAL");
        }
        if let Some(p) = o("PARAMETERS") {
            s.push_str(&format!(" PARAMETERS ('{}')", p.replace('\'', "''")));
        }
        return s;
    }
    if o("REVERSE").is_some() {
        s.push_str(" REVERSE");
    }
    match o("COMPRESS") {
        Some(n) => s.push_str(&format!(" COMPRESS {n}")),
        None if ix.options.contains_key("COMPRESS") => s.push_str(" COMPRESS"),
        None => {}
    }
    if let Some(l) = o("LOCALITY") {
        s.push_str(&format!(" {l}"));
    }
    if o("VISIBILITY") == Some("INVISIBLE") {
        s.push_str(" INVISIBLE");
    }
    s
}

// ------------------------------------------------------------ objects

const SEQUENCE: &str = "SELECT TO_CHAR(min_value), TO_CHAR(max_value), TO_CHAR(increment_by), cycle_flag, order_flag,
        cache_size, TO_CHAR(last_number)
   FROM all_sequences WHERE sequence_owner = :1 AND sequence_name = :2";

/// 12c+ (18c for SCALE / EXTEND): errors are ignored on older servers.
const SEQUENCE_EXTRA: &str = "SELECT scale_flag, extend_flag, session_flag, keep_value
   FROM all_sequences WHERE sequence_owner = :1 AND sequence_name = :2";

#[derive(Debug, Clone, Default)]
pub(crate) struct SequenceInfo {
    pub min: String,
    pub max: String,
    pub increment: String,
    pub cycle: bool,
    pub order: bool,
    pub cache: i64,
    pub start: String,
    pub scale: bool,
    pub extend: bool,
    pub session: bool,
    pub keep: bool,
}

pub(crate) fn sequence_sql(name: &str, s: &SequenceInfo) -> String {
    let mut out = format!(
        "CREATE SEQUENCE {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {}",
        quote(name),
        s.increment,
        s.min,
        s.max,
        s.start
    );
    out.push_str(&if s.cache > 0 { format!(" CACHE {}", s.cache) } else { " NOCACHE".into() });
    out.push_str(if s.order { " ORDER" } else { " NOORDER" });
    out.push_str(if s.cycle { " CYCLE" } else { " NOCYCLE" });
    if s.keep {
        out.push_str(" KEEP");
    }
    if s.scale {
        out.push_str(if s.extend { " SCALE EXTEND" } else { " SCALE" });
    }
    if s.session {
        out.push_str(" SESSION");
    }
    out.push(';');
    out
}

fn yes(v: Option<String>) -> bool {
    v.as_deref() == Some("Y")
}

pub(crate) fn sequence(c: &Connection, owner: &str, name: &str) -> Result<Option<String>> {
    let Some(r) = c.query(SEQUENCE, &[&owner, &name]).map_err(err)?.next() else { return Ok(None) };
    let r = r.map_err(err)?;
    let mut s = SequenceInfo {
        min: text(&r, 0)?.unwrap_or_default(),
        max: text(&r, 1)?.unwrap_or_default(),
        increment: text(&r, 2)?.unwrap_or_default(),
        cycle: yes(text(&r, 3)?),
        order: yes(text(&r, 4)?),
        cache: r.get::<Option<i64>>(5).map_err(err)?.unwrap_or(0),
        start: text(&r, 6)?.unwrap_or_default(),
        ..Default::default()
    };
    if let Ok(Some(Ok(r))) = c.query(SEQUENCE_EXTRA, &[&owner, &name]).map(|mut q| q.next()) {
        s.scale = yes(text(&r, 0).unwrap_or(None));
        s.extend = yes(text(&r, 1).unwrap_or(None));
        s.session = yes(text(&r, 2).unwrap_or(None));
        s.keep = yes(text(&r, 3).unwrap_or(None));
    }
    Ok(Some(sequence_sql(name, &s)))
}

/// `CREATE [PUBLIC] SYNONYM`. The target's owner is left out when it's
/// the session's schema, so the synonym points at the same object on the
/// side it's run on.
pub(crate) fn synonym_sql(public: bool, name: &str, schema: &str, target_owner: Option<&str>, target: &str, link: Option<&str>) -> String {
    let owner = target_owner.filter(|o| *o != schema).map(|o| format!("{}.", quote(o))).unwrap_or_default();
    let link = link.filter(|l| !l.is_empty()).map(|l| format!("@{l}")).unwrap_or_default();
    format!(
        "CREATE OR REPLACE {}SYNONYM {} FOR {owner}{}{link};",
        if public { "PUBLIC " } else { "" },
        quote(name),
        quote(target)
    )
}

pub(crate) fn synonym(c: &Connection, owner: &str, name: &str, schema: &str) -> Result<Option<String>> {
    let Some(r) = c
        .query("SELECT table_owner, table_name, db_link FROM all_synonyms WHERE owner = :1 AND synonym_name = :2", &[&owner, &name])
        .map_err(err)?
        .next()
    else {
        return Ok(None);
    };
    let r = r.map_err(err)?;
    let target = text(&r, 1)?.unwrap_or_default();
    Ok(Some(synonym_sql(owner == "PUBLIC", name, schema, text(&r, 0)?.as_deref(), &target, text(&r, 2)?.as_deref())))
}

/// Public synonyms for this schema's objects (they belong to PUBLIC).
pub(crate) const PUBLIC_SYNONYMS: &str =
    "SELECT synonym_name FROM all_synonyms WHERE owner = 'PUBLIC' AND table_owner = :1 ORDER BY synonym_name";

#[cfg(test)]
mod tests {
    use super::*;

    fn ix(name: &str, cols: &[&str]) -> IndexDef {
        IndexDef { name: name.into(), columns: cols.iter().map(|c| c.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn attributes_become_kind_and_options() {
        let mut i = ix("IX_TXT", &["CUERPO"]);
        apply_attrs(
            &mut i,
            &IndexAttrs {
                index_type: "DOMAIN".into(),
                indextype: Some("CTXSYS.CONTEXT".into()),
                parameters: Some("SYNC (ON COMMIT)".into()),
                ..Default::default()
            },
        );
        assert_eq!(i.kind.as_deref(), Some(FULLTEXT));
        assert_eq!(i.options.get("INDEXTYPE").map(String::as_str), Some("CTXSYS.CONTEXT"));
        assert_eq!(index_sql("\"DOCS\"", &i, &["\"CUERPO\"".into()]), "CREATE INDEX \"IX_TXT\" ON \"DOCS\" (\"CUERPO\") INDEXTYPE IS CTXSYS.CONTEXT PARAMETERS ('SYNC (ON COMMIT)')");

        let mut g = ix("IX_GEO", &["G"]);
        apply_attrs(&mut g, &IndexAttrs { index_type: "DOMAIN".into(), indextype: Some("MDSYS.SPATIAL_INDEX_V2".into()), ..Default::default() });
        assert_eq!(g.kind.as_deref(), Some(SPATIAL));
        assert!(!g.options.contains_key("PARAMETERS"));

        let mut r = ix("IX_R", &["A", "B"]);
        apply_attrs(
            &mut r,
            &IndexAttrs {
                index_type: "NORMAL/REV".into(),
                compression: Some("ENABLED".into()),
                prefix_length: Some(1),
                visibility: Some("INVISIBLE".into()),
                ..Default::default()
            },
        );
        assert_eq!(r.options.len(), 3);
        assert_eq!(index_sql("\"T\"", &r, &["\"A\"".into(), "\"B\"".into()]), "CREATE INDEX \"IX_R\" ON \"T\" (\"A\", \"B\") REVERSE COMPRESS 1 INVISIBLE");

        // Defaults leave no options.
        let mut n = ix("IX_N", &["A"]);
        apply_attrs(&mut n, &IndexAttrs { index_type: "NORMAL".into(), compression: Some("DISABLED".into()), visibility: Some("VISIBLE".into()), ..Default::default() });
        assert!(n.options.is_empty() && n.kind.is_none());
    }

    #[test]
    fn partitioned_indexes() {
        let local = IndexAttrs { index_type: "NORMAL".into(), locality: Some("LOCAL".into()), partitioning: Some("RANGE".into()), ..Default::default() };
        assert_eq!(partition_clause(&local).as_deref(), Some("LOCAL"));
        let hash = IndexAttrs {
            locality: Some("GLOBAL".into()),
            partitioning: Some("HASH".into()),
            partition_count: Some(4),
            part_keys: vec!["ID".into()],
            ..Default::default()
        };
        assert_eq!(partition_clause(&hash).as_deref(), Some("GLOBAL PARTITION BY HASH (\"ID\") PARTITIONS 4"));
        let range = IndexAttrs {
            locality: Some("GLOBAL".into()),
            partitioning: Some("RANGE".into()),
            part_keys: vec!["ID".into()],
            partitions: vec![("P1".into(), "100".into()), ("PMAX".into(), "MAXVALUE".into())],
            ..Default::default()
        };
        let mut i = ix("IX_P", &["ID"]);
        apply_attrs(&mut i, &range);
        assert_eq!(
            index_sql("\"T\"", &i, &["\"ID\"".into()]),
            "CREATE INDEX \"IX_P\" ON \"T\" (\"ID\") GLOBAL PARTITION BY RANGE (\"ID\") (PARTITION \"P1\" VALUES LESS THAN (100), PARTITION \"PMAX\" VALUES LESS THAN (MAXVALUE))"
        );
        let mut b = ix("IX_B", &["A"]);
        b.kind = Some("BITMAP".into());
        b.options.insert("LOCALITY".into(), "LOCAL".into());
        assert_eq!(index_sql("\"T\"", &b, &["\"A\"".into()]), "CREATE BITMAP INDEX \"IX_B\" ON \"T\" (\"A\") LOCAL");
    }

    #[test]
    fn not_null_checks_are_told_apart() {
        assert_eq!(not_null_of("\"NOMBRE\" IS NOT NULL"), Some("NOMBRE"));
        assert_eq!(not_null_of("nombre IS NOT NULL"), None);
        assert_eq!(not_null_of("\"A\" IS NOT NULL OR \"B\" IS NOT NULL"), None);
        assert_eq!(not_null_of("precio > 0"), None);
    }

    #[test]
    fn sequences_and_synonyms() {
        let s = SequenceInfo {
            min: "1".into(),
            max: "9999999999999999999999999999".into(),
            increment: "5".into(),
            cache: 10,
            start: "100".into(),
            ..Default::default()
        };
        assert_eq!(
            sequence_sql("SEQ_FOLIO", &s),
            "CREATE SEQUENCE \"SEQ_FOLIO\" INCREMENT BY 5 MINVALUE 1 MAXVALUE 9999999999999999999999999999 START WITH 100 CACHE 10 NOORDER NOCYCLE;"
        );
        let s = SequenceInfo { cache: 0, cycle: true, order: true, keep: true, scale: true, extend: true, session: true, ..s };
        assert!(sequence_sql("S", &s).ends_with(" NOCACHE ORDER CYCLE KEEP SCALE EXTEND SESSION;"));
        assert_eq!(synonym_sql(false, "SYN", "APP", Some("APP"), "DOCS", None), "CREATE OR REPLACE SYNONYM \"SYN\" FOR \"DOCS\";");
        assert_eq!(synonym_sql(true, "SYN", "APP", Some("HR"), "EMP", Some("REMOTO")), "CREATE OR REPLACE PUBLIC SYNONYM \"SYN\" FOR \"HR\".\"EMP\"@REMOTO;");
    }
}
