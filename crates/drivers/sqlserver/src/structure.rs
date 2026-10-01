//! What "Comparar esquemas" needs beyond columns and keys: every index with
//! its included columns and settings (columnstore, XML, spatial, hash and
//! full-text ones), CHECK constraints, and the objects tables depend on
//! (sequences, synonyms, user types, full-text catalogs and stoplists), read
//! from the catalog and written back as T-SQL.
//!
//! Index settings go into [`IndexDef::options`] named as T-SQL's `WITH`
//! options (`FILLFACTOR` → `80`, `DATA_COMPRESSION` → `PAGE`); the
//! lowercase keys in [`STRUCTURAL`] carry what isn't a `WITH` option
//! (descending keys, columnstore order, the primary XML index…).

use crate::variant::Variant;
use crate::{format_type, text, SqlServerSession};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{kinds, CheckDef, DbObject, IndexDef, KeyDef, ObjectKindInfo, ObjectRef, Result, TableChange, TableSchema};
use std::collections::{BTreeMap, HashMap, HashSet};
use tiberius::Row;

pub const FULLTEXT_CATALOG: &str = "fulltext_catalog";
pub const FULLTEXT_STOPLIST: &str = "fulltext_stoplist";

/// The object kinds whose source this module writes.
pub const KINDS: &[&str] = &[kinds::SEQUENCE, kinds::SYNONYM, kinds::TYPE, FULLTEXT_CATALOG, FULLTEXT_STOPLIST];

/// The single full-text index a table may have goes by this name.
pub const FULLTEXT_NAME: &str = "fulltext";

pub const fn fulltext_catalogs() -> ObjectKindInfo {
    ObjectKindInfo::new(FULLTEXT_CATALOG, "Catálogos de texto completo", false, false, true)
}

pub const fn fulltext_stoplists() -> ObjectKindInfo {
    ObjectKindInfo::new(FULLTEXT_STOPLIST, "Listas de palabras irrelevantes", false, false, true)
}

/// Index options that aren't written in `WITH (…)`.
const STRUCTURAL: &[&str] = &["desc", "order", "primary_xml_index", "unique_constraint", "using", "rebuild"];

/// Engines whose catalog has everything read here.
fn rich(v: Variant) -> bool {
    matches!(v, Variant::SqlServer | Variant::AzureSql)
}

/// The kinds each engine has.
pub fn object_kinds(v: Variant) -> Vec<ObjectKindInfo> {
    match v {
        Variant::SqlServer | Variant::AzureSql => {
            vec![ObjectKindInfo::sequences(), ObjectKindInfo::synonyms(), ObjectKindInfo::types(), fulltext_catalogs(), fulltext_stoplists()]
        }
        // Babelfish has user types; its sys.sequences is empty (the
        // sequences can't be described) and it has no synonyms nor
        // full-text catalogs.
        Variant::Babelfish => vec![ObjectKindInfo::types()],
        // A Fabric warehouse has none of them.
        Variant::Fabric => Vec::new(),
    }
}

fn q(s: &str) -> String {
    quote_ident(Quote::Bracket, s)
}

fn qn(schema: Option<&str>, name: &str) -> String {
    qualified_name(Quote::Bracket, schema.filter(|s| !s.is_empty()), name)
}

fn nlit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

fn st(r: &Row, i: usize) -> String {
    text(r, i).unwrap_or_default()
}

fn b(r: &Row, i: usize) -> bool {
    r.try_get::<bool, _>(i).ok().flatten().unwrap_or(false)
}

fn n(r: &Row, i: usize) -> i32 {
    r.try_get::<i32, _>(i).ok().flatten().unwrap_or(0)
}

fn ni(r: &Row, i: usize) -> Option<i32> {
    r.try_get::<i32, _>(i).ok().flatten()
}

fn f(r: &Row, i: usize) -> Option<f64> {
    r.try_get::<f64, _>(i).ok().flatten()
}

// --- Indexes ----------------------------------------------------------------

/// One index as the catalog describes it.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawIndex {
    pub name: String,
    /// sys.indexes.type: 1 clustered, 2 nonclustered, 3 XML, 4 spatial,
    /// 5 clustered columnstore, 6 nonclustered columnstore, 7 hash.
    pub ty: i32,
    pub type_desc: String,
    pub primary: bool,
    pub unique: bool,
    pub unique_constraint: bool,
    pub filter: Option<String>,
    pub fill_factor: i32,
    pub padded: bool,
    pub ignore_dup_key: bool,
    pub row_locks: bool,
    pub page_locks: bool,
    pub no_recompute: bool,
    pub sequential_key: bool,
    pub compression_delay: i32,
    /// Secondary XML index: its primary and `PATH` / `VALUE` / `PROPERTY`.
    pub xml_primary: Option<String>,
    pub xml_secondary: Option<String>,
    pub tessellation: Option<String>,
    pub bounding_box: Option<[f64; 4]>,
    pub grids: Option<[String; 4]>,
    pub cells_per_object: Option<i32>,
    pub bucket_count: Option<i64>,
    /// Key columns in key order (name, descending).
    pub keys: Vec<(String, bool)>,
    pub include: Vec<String>,
    /// A nonclustered columnstore index's columns.
    pub columns: Vec<String>,
    /// Columnstore `ORDER` columns with their ordinal.
    pub order: Vec<(i32, String)>,
    /// Each partition's compression.
    pub compression: Vec<(i32, String)>,
}

/// `PAGE`, `NONE`… when every partition has it, otherwise
/// `PAGE ON PARTITIONS (1, 2); NONE ON PARTITIONS (3)`.
fn compression_of(parts: &[(i32, String)]) -> Option<String> {
    let first = &parts.first()?.1;
    if parts.iter().all(|(_, c)| c.eq_ignore_ascii_case(first)) {
        return Some(first.to_ascii_uppercase());
    }
    let mut groups: Vec<(String, Vec<i32>)> = Vec::new();
    for (p, c) in parts {
        let c = c.to_ascii_uppercase();
        match groups.iter_mut().find(|(g, _)| *g == c) {
            Some((_, ps)) => ps.push(*p),
            None => groups.push((c, vec![*p])),
        }
    }
    Some(
        groups
            .into_iter()
            .map(|(c, ps)| format!("{c} ON PARTITIONS ({})", ps.iter().map(i32::to_string).collect::<Vec<_>>().join(", ")))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

fn number(x: f64) -> String {
    format!("{x}")
}

impl RawIndex {
    pub(crate) fn to_index(&self) -> IndexDef {
        let mut o = BTreeMap::new();
        let on = |o: &mut BTreeMap<String, String>, k: &str, v: &str| {
            o.insert(k.to_string(), v.to_string());
        };
        let rowstore = matches!(self.ty, 1 | 2);
        let columnstore = matches!(self.ty, 5 | 6);
        let kind = match (self.ty, &self.xml_secondary) {
            (3, Some(t)) => format!("XML {}", t.to_ascii_uppercase()),
            (3, None) => "XML".to_string(),
            _ => self.type_desc.to_ascii_uppercase(),
        };
        let columns = match self.ty {
            5 => Vec::new(),
            6 => self.columns.clone(),
            _ => self.keys.iter().map(|(c, _)| c.clone()).collect(),
        };
        let desc: Vec<&str> = self.keys.iter().filter(|(_, d)| *d).map(|(c, _)| c.as_str()).collect();
        if rowstore && !desc.is_empty() {
            on(&mut o, "desc", &desc.join(", "));
        }
        if !self.order.is_empty() {
            let mut ord = self.order.clone();
            ord.sort();
            on(&mut o, "order", &ord.into_iter().map(|(_, c)| c).collect::<Vec<_>>().join(", "));
        }
        if let (3, Some(p)) = (self.ty, &self.xml_primary) {
            on(&mut o, "primary_xml_index", p);
        }
        if self.unique_constraint {
            on(&mut o, "unique_constraint", "ON");
        }
        if matches!(self.ty, 1..=4) {
            if (1..100).contains(&self.fill_factor) {
                on(&mut o, "FILLFACTOR", &self.fill_factor.to_string());
            }
            if self.padded {
                on(&mut o, "PAD_INDEX", "ON");
            }
        }
        if rowstore && self.ignore_dup_key {
            on(&mut o, "IGNORE_DUP_KEY", "ON");
        }
        if self.no_recompute && self.ty != 7 {
            on(&mut o, "STATISTICS_NORECOMPUTE", "ON");
        }
        // Columnstore indexes report locks off and take no lock options.
        if matches!(self.ty, 1..=4) {
            if !self.row_locks {
                on(&mut o, "ALLOW_ROW_LOCKS", "OFF");
            }
            if !self.page_locks {
                on(&mut o, "ALLOW_PAGE_LOCKS", "OFF");
            }
        }
        if rowstore && self.sequential_key {
            on(&mut o, "OPTIMIZE_FOR_SEQUENTIAL_KEY", "ON");
        }
        if matches!(self.ty, 1 | 2 | 4 | 5 | 6) {
            let default = if columnstore { "COLUMNSTORE" } else { "NONE" };
            if let Some(c) = compression_of(&self.compression).filter(|c| c != default) {
                on(&mut o, "DATA_COMPRESSION", &c);
            }
        }
        if columnstore && self.compression_delay > 0 {
            on(&mut o, "COMPRESSION_DELAY", &self.compression_delay.to_string());
        }
        if self.ty == 4 {
            let scheme = self.tessellation.clone().unwrap_or_default().to_ascii_uppercase();
            if scheme.starts_with("GEOMETRY") {
                if let Some([x0, y0, x1, y1]) = self.bounding_box {
                    on(&mut o, "BOUNDING_BOX", &format!("({}, {}, {}, {})", number(x0), number(y0), number(x1), number(y1)));
                }
            }
            if !scheme.contains("AUTO") {
                if let Some([l1, l2, l3, l4]) = &self.grids {
                    on(&mut o, "GRIDS", &format!("(LEVEL_1 = {l1}, LEVEL_2 = {l2}, LEVEL_3 = {l3}, LEVEL_4 = {l4})"));
                }
            }
            if let Some(c) = self.cells_per_object {
                on(&mut o, "CELLS_PER_OBJECT", &c.to_string());
            }
            if !scheme.is_empty() {
                on(&mut o, "using", &scheme);
            }
        }
        if let (7, Some(bc)) = (self.ty, self.bucket_count) {
            on(&mut o, "BUCKET_COUNT", &bc.to_string());
        }
        IndexDef {
            name: self.name.clone(),
            columns,
            unique: self.unique,
            kind: Some(kind),
            filter: self.filter.as_deref().map(|f| crate::schema::strip_parens(f).to_string()).filter(|f| !f.is_empty()),
            include: if rowstore { self.include.clone() } else { Vec::new() },
            options: o,
        }
    }
}

/// A table's full-text index as the catalog describes it.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawFullText {
    pub key_index: String,
    pub catalog: Option<String>,
    /// `AUTO`, `MANUAL`, `OFF`.
    pub change_tracking: String,
    /// `None`: no stoplist (`OFF`); `Some(0)`: the system one.
    pub stoplist_id: Option<i32>,
    pub stoplist: Option<String>,
    pub property_list: Option<String>,
    /// Column, language (LCID), type column, statistical semantics.
    pub columns: Vec<(String, i32, Option<String>, bool)>,
}

impl RawFullText {
    /// The index, with the settings that differ from `CREATE FULLTEXT
    /// INDEX`'s defaults (the key index and the catalog always).
    pub(crate) fn to_index(&self, default_language: i32) -> IndexDef {
        let mut o = BTreeMap::new();
        o.insert("KEY INDEX".to_string(), self.key_index.clone());
        if let Some(c) = &self.catalog {
            o.insert("CATALOG".into(), c.clone());
        }
        let tracking = self.change_tracking.trim().to_ascii_uppercase();
        if !tracking.is_empty() && tracking != "AUTO" {
            o.insert("CHANGE_TRACKING".into(), tracking);
        }
        match (self.stoplist_id, &self.stoplist) {
            (None, _) => {
                o.insert("STOPLIST".into(), "OFF".into());
            }
            (Some(0), _) => {}
            (Some(_), Some(s)) => {
                o.insert("STOPLIST".into(), s.clone());
            }
            (Some(id), None) => {
                o.insert("STOPLIST".into(), id.to_string());
            }
        }
        if let Some(p) = &self.property_list {
            o.insert("SEARCH PROPERTY LIST".into(), p.clone());
        }
        for (c, lang, ty, semantics) in &self.columns {
            if *lang != default_language {
                o.insert(format!("LANGUAGE {c}"), lang.to_string());
            }
            if let Some(t) = ty {
                o.insert(format!("TYPE COLUMN {c}"), t.clone());
            }
            if *semantics {
                o.insert(format!("STATISTICAL_SEMANTICS {c}"), "ON".into());
            }
        }
        IndexDef {
            name: FULLTEXT_NAME.into(),
            columns: self.columns.iter().map(|(c, ..)| c.clone()).collect(),
            unique: false,
            kind: Some("FULLTEXT".into()),
            filter: None,
            include: Vec::new(),
            options: o,
        }
    }
}

fn kind_of(ix: &IndexDef) -> String {
    ix.kind.as_deref().unwrap_or("").trim().to_ascii_uppercase()
}

fn is_fulltext(ix: &IndexDef) -> bool {
    kind_of(ix) == "FULLTEXT"
}

/// Order in which the server lets them go: the full-text index, secondary
/// XML indexes before their primary, spatial, the rest, the clustered one
/// last (dropping it first would rebuild every other index).
fn drop_rank(ix: &IndexDef) -> u8 {
    match kind_of(ix).as_str() {
        "FULLTEXT" => 0,
        k if k.starts_with("XML ") => 1,
        "XML" => 2,
        "SPATIAL" => 3,
        "CLUSTERED" | "CLUSTERED COLUMNSTORE" => 5,
        _ => 4,
    }
}

/// Order in which they can be made: the clustered one first, primary XML
/// indexes before their secondary ones, the full-text index after its key.
fn create_rank(ix: &IndexDef) -> u8 {
    match kind_of(ix).as_str() {
        "CLUSTERED" | "CLUSTERED COLUMNSTORE" => 0,
        "XML" => 2,
        k if k.starts_with("XML ") => 3,
        "FULLTEXT" => 4,
        _ => 1,
    }
}

const CAPS_SQL: &str = "SELECT
        CAST(CASE WHEN COL_LENGTH('sys.indexes', 'optimize_for_sequential_key') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN COL_LENGTH('sys.indexes', 'compression_delay') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN COL_LENGTH('sys.index_columns', 'column_store_order_ordinal') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(ISNULL((SELECT CAST(value_in_use AS int) FROM sys.configurations WHERE name = 'default full-text language'), 1033) AS int)";

fn indexes_sql(seqkey: bool, delay: bool) -> String {
    format!(
        "SELECT i.object_id, i.index_id, s.name, t.name, i.name, CAST(i.type AS int), i.type_desc,
                i.is_primary_key, i.is_unique, i.is_unique_constraint, i.filter_definition,
                CAST(i.fill_factor AS int), i.is_padded, i.ignore_dup_key, i.allow_row_locks, i.allow_page_locks,
                CAST(ISNULL(sx.no_recompute, 0) AS bit), {}, {},
                pxi.name, xi.secondary_type_desc,
                tes.tessellation_scheme, CAST(tes.bounding_box_xmin AS float), CAST(tes.bounding_box_ymin AS float),
                CAST(tes.bounding_box_xmax AS float), CAST(tes.bounding_box_ymax AS float),
                tes.level_1_grid_desc, tes.level_2_grid_desc, tes.level_3_grid_desc, tes.level_4_grid_desc,
                CAST(tes.cells_per_object AS int), CAST(hx.bucket_count AS bigint)
           FROM sys.indexes i
           JOIN sys.tables t ON t.object_id = i.object_id
           JOIN sys.schemas s ON s.schema_id = t.schema_id
           LEFT JOIN sys.stats sx ON sx.object_id = i.object_id AND sx.stats_id = i.index_id
           LEFT JOIN sys.xml_indexes xi ON xi.object_id = i.object_id AND xi.index_id = i.index_id
           LEFT JOIN sys.indexes pxi ON pxi.object_id = xi.object_id AND pxi.index_id = xi.using_xml_index_id
           LEFT JOIN sys.spatial_index_tessellations tes ON tes.object_id = i.object_id AND tes.index_id = i.index_id
           LEFT JOIN sys.hash_indexes hx ON hx.object_id = i.object_id AND hx.index_id = i.index_id
          WHERE t.is_ms_shipped = 0 AND i.type > 0 AND i.is_hypothetical = 0
          ORDER BY s.name, t.name, i.index_id",
        if seqkey { "i.optimize_for_sequential_key" } else { "CAST(0 AS bit)" },
        if delay { "CAST(ISNULL(i.compression_delay, 0) AS int)" } else { "0" },
    )
}

fn index_columns_sql(order: bool) -> String {
    format!(
        "SELECT ic.object_id, ic.index_id, c.name, CAST(ic.key_ordinal AS int), ic.is_descending_key,
                ic.is_included_column, CAST(ic.partition_ordinal AS int), {}
           FROM sys.index_columns ic
           JOIN sys.tables t ON t.object_id = ic.object_id
           JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
          WHERE t.is_ms_shipped = 0
          ORDER BY ic.object_id, ic.index_id, ic.key_ordinal, ic.index_column_id",
        if order { "CAST(ISNULL(ic.column_store_order_ordinal, 0) AS int)" } else { "0" },
    )
}

const PARTITIONS_SQL: &str = "
SELECT p.object_id, p.index_id, p.partition_number, p.data_compression_desc
  FROM sys.partitions p
  JOIN sys.tables t ON t.object_id = p.object_id
 WHERE t.is_ms_shipped = 0 AND p.index_id > 0
 ORDER BY p.object_id, p.index_id, p.partition_number";

const FULLTEXT_SQL: &str = "
SELECT s.name, t.name, ki.name, fc.name, fi.change_tracking_state_desc, CAST(fi.stoplist_id AS int), sl.name, pl.name
  FROM sys.fulltext_indexes fi
  JOIN sys.tables t ON t.object_id = fi.object_id
  JOIN sys.schemas s ON s.schema_id = t.schema_id
  JOIN sys.indexes ki ON ki.object_id = fi.object_id AND ki.index_id = fi.unique_index_id
  LEFT JOIN sys.fulltext_catalogs fc ON fc.fulltext_catalog_id = fi.fulltext_catalog_id
  LEFT JOIN sys.fulltext_stoplists sl ON sl.stoplist_id = fi.stoplist_id
  LEFT JOIN sys.registered_search_property_lists pl ON pl.property_list_id = fi.property_list_id";

const FULLTEXT_COLUMNS_SQL: &str = "
SELECT s.name, t.name, c.name, CAST(fic.language_id AS int), tc.name, CAST(fic.statistical_semantics AS bit)
  FROM sys.fulltext_index_columns fic
  JOIN sys.tables t ON t.object_id = fic.object_id
  JOIN sys.schemas s ON s.schema_id = t.schema_id
  JOIN sys.columns c ON c.object_id = fic.object_id AND c.column_id = fic.column_id
  LEFT JOIN sys.columns tc ON tc.object_id = fic.object_id AND tc.column_id = fic.type_column_id
 ORDER BY s.name, t.name, fic.column_id";

const CHECKS_SQL: &str = "
SELECT s.name, t.name, cc.name, cc.definition
  FROM sys.check_constraints cc
  JOIN sys.tables t ON t.object_id = cc.parent_object_id
  JOIN sys.schemas s ON s.schema_id = t.schema_id
 WHERE t.is_ms_shipped = 0
 ORDER BY s.name, t.name, cc.name";

type Key = (String, String);

/// Primary key and indexes per table, from the catalog.
async fn read_indexes(s: &mut SqlServerSession) -> Result<HashMap<Key, (Option<KeyDef>, Vec<IndexDef>)>> {
    let caps = s.rows(CAPS_SQL, &[]).await?;
    let caps = caps.first();
    let (seqkey, delay, order) = caps.map(|r| (b(r, 0), b(r, 1), b(r, 2))).unwrap_or_default();
    let default_language = caps.and_then(|r| ni(r, 3)).unwrap_or(1033);

    let mut raw: BTreeMap<(i32, i32), (Key, RawIndex)> = BTreeMap::new();
    for r in s.rows(&indexes_sql(seqkey, delay), &[]).await? {
        let grids = match (text(&r, 26), text(&r, 27), text(&r, 28), text(&r, 29)) {
            (Some(a), Some(b), Some(c), Some(d)) => Some([a, b, c, d]),
            _ => None,
        };
        let bounding_box = match (f(&r, 22), f(&r, 23), f(&r, 24), f(&r, 25)) {
            (Some(a), Some(b), Some(c), Some(d)) => Some([a, b, c, d]),
            _ => None,
        };
        let ix = RawIndex {
            name: st(&r, 4),
            ty: n(&r, 5),
            type_desc: st(&r, 6),
            primary: b(&r, 7),
            unique: b(&r, 8),
            unique_constraint: b(&r, 9),
            filter: text(&r, 10),
            fill_factor: n(&r, 11),
            padded: b(&r, 12),
            ignore_dup_key: b(&r, 13),
            row_locks: b(&r, 14),
            page_locks: b(&r, 15),
            no_recompute: b(&r, 16),
            sequential_key: b(&r, 17),
            compression_delay: n(&r, 18),
            xml_primary: text(&r, 19),
            xml_secondary: text(&r, 20),
            tessellation: text(&r, 21),
            bounding_box,
            grids,
            cells_per_object: ni(&r, 30),
            bucket_count: r.try_get::<i64, _>(31).ok().flatten(),
            ..Default::default()
        };
        raw.insert((n(&r, 0), n(&r, 1)), ((st(&r, 2), st(&r, 3)), ix));
    }
    for r in s.rows(&index_columns_sql(order), &[]).await? {
        let Some((_, i)) = raw.get_mut(&(n(&r, 0), n(&r, 1))) else { continue };
        let (col, key_ordinal, desc, included, part, ord) = (st(&r, 2), n(&r, 3), b(&r, 4), b(&r, 5), n(&r, 6), n(&r, 7));
        if ord > 0 {
            i.order.push((ord, col.clone()));
        }
        match i.ty {
            5 => {}
            6 => {
                if key_ordinal > 0 || included || part == 0 {
                    i.columns.push(col);
                }
            }
            3 | 4 => {
                if key_ordinal > 0 || part == 0 {
                    i.keys.push((col, false));
                }
            }
            _ => {
                if key_ordinal > 0 {
                    i.keys.push((col, desc));
                } else if included {
                    i.include.push(col);
                }
            }
        }
    }
    for r in s.rows(PARTITIONS_SQL, &[]).await? {
        if let Some((_, i)) = raw.get_mut(&(n(&r, 0), n(&r, 1))) {
            i.compression.push((n(&r, 2), st(&r, 3)));
        }
    }

    let mut out: HashMap<Key, (Option<KeyDef>, Vec<IndexDef>)> = HashMap::new();
    for (_, (key, ix)) in raw {
        let e = out.entry(key).or_default();
        if ix.primary {
            e.0 = Some(KeyDef { name: Some(ix.name.clone()), columns: ix.keys.iter().map(|(c, _)| c.clone()).collect() });
        } else {
            e.1.push(ix.to_index());
        }
    }

    let mut ft: BTreeMap<Key, RawFullText> = BTreeMap::new();
    for r in s.rows(FULLTEXT_SQL, &[]).await? {
        ft.insert(
            (st(&r, 0), st(&r, 1)),
            RawFullText {
                key_index: st(&r, 2),
                catalog: text(&r, 3),
                change_tracking: st(&r, 4),
                stoplist_id: ni(&r, 5),
                stoplist: text(&r, 6),
                property_list: text(&r, 7),
                columns: Vec::new(),
            },
        );
    }
    if !ft.is_empty() {
        for r in s.rows(FULLTEXT_COLUMNS_SQL, &[]).await? {
            if let Some(x) = ft.get_mut(&(st(&r, 0), st(&r, 1))) {
                x.columns.push((st(&r, 2), n(&r, 3), text(&r, 4), b(&r, 5)));
            }
        }
    }
    for (key, x) in ft {
        out.entry(key).or_default().1.push(x.to_index(default_language));
    }
    // By name within each rank: the same on both sides of a compare, however
    // the indexes were made.
    for (_, ixs) in out.values_mut() {
        ixs.sort_by(|a, b| drop_rank(a).cmp(&drop_rank(b)).then_with(|| a.name.cmp(&b.name)));
    }
    Ok(out)
}

/// Everything `database_schema` reads beyond the basic query: indexes with
/// their settings and the full-text index (SQL Server, Azure SQL), and
/// CHECK constraints. A part the server refuses leaves what was there.
pub(crate) async fn complete(s: &mut SqlServerSession, tables: &mut [TableSchema]) {
    if s.variant == Variant::Fabric {
        return;
    }
    let at: HashMap<Key, usize> =
        tables.iter().enumerate().map(|(i, t)| ((t.schema.clone().unwrap_or_default(), t.name.clone()), i)).collect();
    if rich(s.variant) {
        match read_indexes(s).await {
            Ok(mut by_table) => {
                for (key, &i) in &at {
                    let (pk, ixs) = by_table.remove(key).unwrap_or_default();
                    tables[i].primary_key = pk;
                    tables[i].indexes = ixs;
                }
            }
            Err(e) => tracing::warn!("sqlserver: index details not read: {e}"),
        }
    }
    match s.rows(CHECKS_SQL, &[]).await {
        Ok(rows) => {
            for r in rows {
                if let Some(&i) = at.get(&(st(&r, 0), st(&r, 1))) {
                    tables[i].checks.push(CheckDef { name: text(&r, 2), expression: st(&r, 3) });
                }
            }
        }
        Err(e) => tracing::warn!("sqlserver: CHECK constraints not read: {e}"),
    }
}

// --- Index DDL --------------------------------------------------------------

fn col_list(cols: &[String]) -> String {
    cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
}

fn split_list(v: Option<&String>) -> Vec<String> {
    v.map(|s| s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()).unwrap_or_default()
}

/// ` WITH (…)` from the options that are `WITH` options.
fn with_clause(ix: &IndexDef) -> String {
    let mut w = Vec::new();
    for (k, v) in &ix.options {
        if STRUCTURAL.contains(&k.as_str()) {
            continue;
        }
        if k == "DATA_COMPRESSION" {
            w.extend(v.split("; ").map(|p| format!("DATA_COMPRESSION = {p}")));
        } else {
            w.push(format!("{k} = {v}"));
        }
    }
    if w.is_empty() {
        String::new()
    } else {
        format!(" WITH ({})", w.join(", "))
    }
}

fn where_clause(ix: &IndexDef) -> String {
    ix.filter.as_deref().map(str::trim).filter(|w| !w.is_empty()).map(|w| format!(" WHERE {w}")).unwrap_or_default()
}

/// Key columns, with `DESC` on the descending ones.
fn key_list(ix: &IndexDef) -> String {
    let desc: HashSet<String> = split_list(ix.options.get("desc")).into_iter().map(|c| c.to_lowercase()).collect();
    ix.columns.iter().map(|c| if desc.contains(&c.to_lowercase()) { format!("{} DESC", q(c)) } else { q(c) }).collect::<Vec<_>>().join(", ")
}

fn fulltext_sql(owner: &str, ix: &IndexDef) -> String {
    let o = &ix.options;
    let cols: Vec<String> = ix
        .columns
        .iter()
        .map(|c| {
            let mut s = q(c);
            if let Some(t) = o.get(&format!("TYPE COLUMN {c}")) {
                s.push_str(&format!(" TYPE COLUMN {}", q(t)));
            }
            if let Some(l) = o.get(&format!("LANGUAGE {c}")) {
                // An LCID as a number, a language name as a string.
                if l.chars().all(|ch| ch.is_ascii_digit()) {
                    s.push_str(&format!(" LANGUAGE {l}"));
                } else {
                    s.push_str(&format!(" LANGUAGE {}", nlit(l)));
                }
            }
            if o.contains_key(&format!("STATISTICAL_SEMANTICS {c}")) {
                s.push_str(" STATISTICAL_SEMANTICS");
            }
            s
        })
        .collect();
    let mut s = format!("CREATE FULLTEXT INDEX ON {owner} ({})", cols.join(", "));
    if let Some(k) = o.get("KEY INDEX") {
        s.push_str(&format!(" KEY INDEX {}", q(k)));
    }
    if let Some(c) = o.get("CATALOG") {
        s.push_str(&format!(" ON {}", q(c)));
    }
    let mut w = Vec::new();
    if let Some(t) = o.get("CHANGE_TRACKING") {
        w.push(format!("CHANGE_TRACKING = {t}"));
    }
    if let Some(sl) = o.get("STOPLIST") {
        let v = if sl.eq_ignore_ascii_case("OFF") || sl.eq_ignore_ascii_case("SYSTEM") { sl.to_ascii_uppercase() } else { q(sl) };
        w.push(format!("STOPLIST = {v}"));
    }
    if let Some(p) = o.get("SEARCH PROPERTY LIST") {
        w.push(format!("SEARCH PROPERTY LIST = {}", q(p)));
    }
    if !w.is_empty() {
        s.push_str(&format!(" WITH ({})", w.join(", ")));
    }
    s.push(';');
    s
}

/// One index's statement, or `None` for a kind this engine doesn't know.
fn index_sql(owner: &str, ix: &IndexDef) -> Option<String> {
    let name = q(&ix.name);
    let kind = kind_of(ix);
    let first = ix.columns.first().map(|c| q(c)).unwrap_or_default();
    let order = split_list(ix.options.get("order"));
    let order = if order.is_empty() { String::new() } else { format!(" ORDER ({})", col_list(&order)) };
    let s = match kind.as_str() {
        "FULLTEXT" => return Some(fulltext_sql(owner, ix)),
        "XML" => format!("CREATE PRIMARY XML INDEX {name} ON {owner} ({first}){}", with_clause(ix)),
        k if k.starts_with("XML ") => {
            let primary = ix.options.get("primary_xml_index").map(|p| q(p)).unwrap_or_default();
            format!("CREATE XML INDEX {name} ON {owner} ({first}) USING XML INDEX {primary} FOR {}{}", &k[4..], with_clause(ix))
        }
        "SPATIAL" => {
            let using = ix.options.get("using").map(|u| format!(" USING {u}")).unwrap_or_default();
            format!("CREATE SPATIAL INDEX {name} ON {owner} ({first}){using}{}", with_clause(ix))
        }
        "CLUSTERED COLUMNSTORE" => format!("CREATE CLUSTERED COLUMNSTORE INDEX {name} ON {owner}{order}{}", with_clause(ix)),
        "NONCLUSTERED COLUMNSTORE" => {
            format!("CREATE NONCLUSTERED COLUMNSTORE INDEX {name} ON {owner} ({}){order}{}{}", col_list(&ix.columns), where_clause(ix), with_clause(ix))
        }
        // Memory-optimized tables take indexes only through ALTER TABLE.
        "NONCLUSTERED HASH" => format!("ALTER TABLE {owner} ADD INDEX {name} NONCLUSTERED HASH ({}){}", col_list(&ix.columns), with_clause(ix)),
        "" | "CLUSTERED" | "NONCLUSTERED" if ix.options.contains_key("unique_constraint") => {
            let clustering = if kind.is_empty() { String::new() } else { format!(" {kind}") };
            format!("ALTER TABLE {owner} ADD CONSTRAINT {name} UNIQUE{clustering} ({}){}", key_list(ix), with_clause(ix))
        }
        "" | "CLUSTERED" | "NONCLUSTERED" => {
            let include = if ix.include.is_empty() { String::new() } else { format!(" INCLUDE ({})", col_list(&ix.include)) };
            format!(
                "CREATE {}{}INDEX {name} ON {owner} ({}){include}{}{}",
                if ix.unique { "UNIQUE " } else { "" },
                if kind.is_empty() { String::new() } else { format!("{kind} ") },
                key_list(ix),
                where_clause(ix),
                with_clause(ix),
            )
        }
        _ => return None,
    };
    Some(format!("{s};"))
}

/// `CREATE … INDEX` for each index, in an order the server accepts.
pub(crate) fn index_statements(owner: &str, t: &TableSchema, if_exists: bool) -> Vec<String> {
    let mut ixs: Vec<&IndexDef> = t.indexes.iter().collect();
    ixs.sort_by_key(|i| create_rank(i));
    ixs.into_iter()
        .map(|ix| match index_sql(owner, ix) {
            None => format!("-- Índice {} ({}) omitido: se crea a mano.", ix.name, ix.kind.as_deref().unwrap_or("")),
            Some(s) if !if_exists => s,
            Some(s) if is_fulltext(ix) => {
                format!("IF NOT EXISTS (SELECT 1 FROM sys.fulltext_indexes WHERE object_id = OBJECT_ID({}))\n    {s}", nlit(owner))
            }
            Some(s) => format!(
                "IF NOT EXISTS (SELECT 1 FROM sys.indexes WHERE object_id = OBJECT_ID({}) AND name = {})\n    {s}",
                nlit(owner),
                nlit(&ix.name)
            ),
        })
        .collect()
}

// --- Schema sync ------------------------------------------------------------

fn same_index(a: &IndexDef, b: &IndexDef) -> bool {
    let cols = |x: &[String]| x.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>();
    let inc = |x: &[String]| {
        let mut v = cols(x);
        v.sort();
        v
    };
    let w = |x: &Option<String>| x.as_deref().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    cols(&a.columns) == cols(&b.columns)
        && a.unique == b.unique
        && kind_of(a) == kind_of(b)
        && w(&a.filter) == w(&b.filter)
        && inc(&a.include) == inc(&b.include)
        && a.options == b.options
}

/// Changes as the generic planner should see them: an index that depends
/// on one that is dropped and made again (a secondary XML index on its
/// primary; XML, spatial and full-text indexes on the primary key; the
/// full-text index on its key index) goes too, and comes back after.
pub(crate) fn prepare_changes(changes: &[TableChange]) -> Vec<TableChange> {
    changes
        .iter()
        .map(|ch| match ch {
            TableChange::Alter { old, new } => {
                let find = |t: &TableSchema, name: &str| t.indexes.iter().find(|i| i.name.eq_ignore_ascii_case(name)).cloned();
                let changed = |name: &str| find(old, name).is_some_and(|o| find(new, name).is_none_or(|n| !same_index(&o, &n)));
                let pk_cols = |t: &TableSchema| t.primary_key.as_ref().map(|k| k.columns.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>()).unwrap_or_default();
                let pk_changed = pk_cols(old) != pk_cols(new);
                let pk_name = old.primary_key.as_ref().and_then(|k| k.name.clone()).unwrap_or_default();
                let mut old = old.clone();
                // Dependents go first (the generic planner drops in list order).
                old.indexes.sort_by_key(drop_rank);
                for ix in &mut old.indexes {
                    let kind = kind_of(ix);
                    let force = match kind.as_str() {
                        "FULLTEXT" => ix.options.get("KEY INDEX").is_some_and(|k| changed(k) || (pk_changed && k.eq_ignore_ascii_case(&pk_name))),
                        "XML" | "SPATIAL" => pk_changed,
                        k if k.starts_with("XML ") => pk_changed || ix.options.get("primary_xml_index").is_some_and(|p| changed(p)),
                        _ => false,
                    };
                    if force && find(new, &ix.name).is_some() {
                        ix.options.insert("rebuild".into(), "1".into());
                    }
                }
                TableChange::Alter { old, new: new.clone() }
            }
            other => other.clone(),
        })
        .collect()
}

/// UNIQUE constraints and the indexes of memory-optimized tables aren't
/// dropped with `DROP INDEX … ON`.
pub(crate) fn fix_drops(statements: &mut [String], changes: &[TableChange]) {
    for ch in changes {
        let TableChange::Alter { old, new } = ch else { continue };
        let owner = qn(new.schema.as_deref(), &new.name);
        for ix in &old.indexes {
            let replacement = if ix.options.contains_key("unique_constraint") {
                format!("ALTER TABLE {owner} DROP CONSTRAINT {};", q(&ix.name))
            } else if kind_of(ix) == "NONCLUSTERED HASH" {
                format!("ALTER TABLE {owner} DROP INDEX {};", q(&ix.name))
            } else {
                continue;
            };
            let plain = format!("DROP INDEX {} ON {owner};", q(&ix.name));
            for s in statements.iter_mut().filter(|s| **s == plain) {
                *s = replacement.clone();
            }
        }
    }
}

// --- Other objects ----------------------------------------------------------

const LIST_SQL: &str = "
SELECT N'SO', SCHEMA_NAME(schema_id), name FROM sys.sequences WHERE is_ms_shipped = 0
UNION ALL SELECT N'SN', SCHEMA_NAME(schema_id), name FROM sys.synonyms WHERE is_ms_shipped = 0
UNION ALL SELECT N'TY', SCHEMA_NAME(schema_id), name FROM sys.types WHERE is_user_defined = 1 AND is_assembly_type = 0
UNION ALL SELECT N'FC', NULL, name FROM sys.fulltext_catalogs
UNION ALL SELECT N'FS', NULL, name FROM sys.fulltext_stoplists
ORDER BY 2, 3";

const LIST_BABELFISH_SQL: &str = "
SELECT N'TY', SCHEMA_NAME(schema_id), name FROM sys.types WHERE is_user_defined = 1
ORDER BY 2, 3";

/// Sequences, synonyms, user types, full-text catalogs and stoplists. A
/// catalog that can't be read leaves them out (the explorer still works).
pub(crate) async fn list_objects(s: &mut SqlServerSession) -> Vec<DbObject> {
    let sql = match s.variant {
        Variant::SqlServer | Variant::AzureSql => LIST_SQL,
        Variant::Babelfish => LIST_BABELFISH_SQL,
        Variant::Fabric => return Vec::new(),
    };
    let rows = match s.rows(sql, &[]).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("sqlserver: sequences, synonyms and types not listed: {e}");
            return Vec::new();
        }
    };
    rows.iter()
        .filter_map(|r| {
            let kind = match text(r, 0)?.as_str() {
                "SO" => kinds::SEQUENCE,
                "SN" => kinds::SYNONYM,
                "TY" => kinds::TYPE,
                "FC" => FULLTEXT_CATALOG,
                _ => FULLTEXT_STOPLIST,
            };
            Some(DbObject { kind: kind.into(), schema: text(r, 1), name: text(r, 2)?, parent: None })
        })
        .collect()
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SequenceInfo {
    pub type_name: String,
    /// Set for an alias type.
    pub type_schema: Option<String>,
    pub precision: i32,
    pub start: String,
    pub increment: String,
    pub min: String,
    pub max: String,
    pub cycle: bool,
    pub cached: bool,
    pub cache_size: Option<i32>,
}

pub(crate) fn sequence_sql(schema: Option<&str>, name: &str, s: &SequenceInfo) -> String {
    let ty = match (&s.type_schema, s.type_name.to_ascii_lowercase().as_str()) {
        (Some(ts), _) => qn(Some(ts), &s.type_name),
        (None, t @ ("decimal" | "numeric")) => format!("{t}({}, 0)", s.precision),
        (None, t) => t.to_string(),
    };
    let cache = match (s.cached, s.cache_size) {
        (false, _) => "NO CACHE".to_string(),
        (true, Some(n)) => format!("CACHE {n}"),
        (true, None) => "CACHE".into(),
    };
    format!(
        "CREATE SEQUENCE {}\n    AS {ty}\n    START WITH {}\n    INCREMENT BY {}\n    MINVALUE {}\n    MAXVALUE {}\n    {}\n    {cache};",
        qn(schema, name),
        s.start,
        s.increment,
        s.min,
        s.max,
        if s.cycle { "CYCLE" } else { "NO CYCLE" },
    )
}

pub(crate) fn synonym_sql(schema: Option<&str>, name: &str, base: &str) -> String {
    format!("CREATE SYNONYM {} FOR {base};", qn(schema, name))
}

/// A column of a table type.
#[derive(Debug, Clone, Default)]
pub(crate) struct TypeColumn {
    pub name: String,
    /// As written: `nvarchar(50)`, `[dbo].[Email]`.
    pub data_type: String,
    pub nullable: bool,
    /// Seed, increment.
    pub identity: Option<(String, String)>,
    /// Expression, persisted.
    pub computed: Option<(String, bool)>,
    pub default: Option<String>,
}

/// An index or key of a table type.
#[derive(Debug, Clone, Default)]
pub(crate) struct TypeIndex {
    pub name: String,
    pub primary: bool,
    pub unique_constraint: bool,
    pub unique: bool,
    pub clustered: bool,
    pub bucket_count: Option<i64>,
    pub keys: Vec<(String, bool)>,
}

pub(crate) fn alias_type_sql(schema: Option<&str>, name: &str, base: &str, nullable: bool) -> String {
    format!("CREATE TYPE {} FROM {base} {};", qn(schema, name), if nullable { "NULL" } else { "NOT NULL" })
}

pub(crate) fn table_type_sql(schema: Option<&str>, name: &str, cols: &[TypeColumn], ixs: &[TypeIndex], checks: &[String], memory_optimized: bool) -> String {
    let mut parts: Vec<String> = cols
        .iter()
        .map(|c| {
            if let Some((expr, persisted)) = &c.computed {
                return format!("{} AS {expr}{}", q(&c.name), if *persisted { " PERSISTED" } else { "" });
            }
            let mut s = format!("{} {}", q(&c.name), c.data_type);
            if let Some((seed, inc)) = &c.identity {
                s.push_str(&format!(" IDENTITY({seed}, {inc})"));
            }
            if let Some(d) = &c.default {
                s.push_str(&format!(" DEFAULT {d}"));
            }
            s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
            s
        })
        .collect();
    for i in ixs {
        let kind = match (i.bucket_count, i.clustered) {
            (Some(_), _) => "NONCLUSTERED HASH",
            (None, true) => "CLUSTERED",
            (None, false) => "NONCLUSTERED",
        };
        let keys = i
            .keys
            .iter()
            .map(|(c, d)| if *d && i.bucket_count.is_none() { format!("{} DESC", q(c)) } else { q(c) })
            .collect::<Vec<_>>()
            .join(", ");
        let bucket = i.bucket_count.map(|n| format!(" WITH (BUCKET_COUNT = {n})")).unwrap_or_default();
        // Table types take their keys unnamed.
        parts.push(if i.primary {
            format!("PRIMARY KEY {kind} ({keys}){bucket}")
        } else if i.unique_constraint {
            format!("UNIQUE {kind} ({keys}){bucket}")
        } else {
            format!("INDEX {} {}{kind} ({keys}){bucket}", q(&i.name), if i.unique { "UNIQUE " } else { "" })
        });
    }
    for c in checks {
        parts.push(format!("CHECK {c}"));
    }
    format!(
        "CREATE TYPE {} AS TABLE (\n    {}\n){};",
        qn(schema, name),
        parts.join(",\n    "),
        if memory_optimized { " WITH (MEMORY_OPTIMIZED = ON)" } else { "" }
    )
}

/// A full-text catalog, made or changed in place: one that full-text
/// indexes use can't be dropped, so the sync applies this as it is.
pub(crate) fn catalog_sql(name: &str, accent_sensitive: bool, default: bool) -> String {
    let accent = if accent_sensitive { "ON" } else { "OFF" };
    let mut s = format!(
        "IF NOT EXISTS (SELECT 1 FROM sys.fulltext_catalogs WHERE name = {l})\n    CREATE FULLTEXT CATALOG {c} WITH ACCENT_SENSITIVITY = {accent};\n\
         ELSE IF (SELECT is_accent_sensitivity_on FROM sys.fulltext_catalogs WHERE name = {l}) <> {}\n    ALTER FULLTEXT CATALOG {c} REBUILD WITH ACCENT_SENSITIVITY = {accent};",
        u8::from(accent_sensitive),
        l = nlit(name),
        c = q(name),
    );
    if default {
        s.push_str(&format!("\nALTER FULLTEXT CATALOG {} AS DEFAULT;", q(name)));
    }
    s
}

/// A full-text stoplist, made or brought in place to its words: the
/// system's (when it was made from them) plus and minus the listed ones.
/// One that a full-text index uses can't be dropped, so the sync applies
/// this as it is.
pub(crate) fn stoplist_sql(name: &str, from_system: bool, added: &[(String, i32)], removed: &[(String, i32)]) -> String {
    let (l, sl) = (nlit(name), q(name));
    let mut s = format!(
        "IF NOT EXISTS (SELECT 1 FROM sys.fulltext_stoplists WHERE name = {l})\n    CREATE FULLTEXT STOPLIST {sl}{};\n\
         DECLARE @words TABLE (word nvarchar(64) NOT NULL, lang int NOT NULL);",
        if from_system { " FROM SYSTEM STOPLIST" } else { "" }
    );
    if from_system {
        s.push_str("\nINSERT INTO @words SELECT stopword, language_id FROM sys.fulltext_system_stopwords;");
        for chunk in removed.chunks(100) {
            let cond: Vec<String> = chunk.iter().map(|(w, lang)| format!("(lang = {lang} AND word = {})", nlit(w))).collect();
            s.push_str(&format!("\nDELETE FROM @words WHERE {};", cond.join("\n    OR ")));
        }
    }
    for chunk in added.chunks(1000) {
        let vals: Vec<String> = chunk.iter().map(|(w, lang)| format!("({}, {lang})", nlit(w))).collect();
        s.push_str(&format!("\nINSERT INTO @words VALUES {};", vals.join(",\n    ")));
    }
    let listed = format!(
        "sys.fulltext_stopwords w JOIN sys.fulltext_stoplists l ON l.stoplist_id = w.stoplist_id AND l.name = {l}"
    );
    let stmt = |verb: &str, col: &str, lang: &str| {
        format!("N'ALTER FULLTEXT STOPLIST {} {verb} N' + QUOTENAME({col}, '''') + N' LANGUAGE ' + CAST({lang} AS nvarchar(10)) + N';'", sl.replace('\'', "''"))
    };
    s.push_str(&format!(
        "\nDECLARE @sql nvarchar(max) = N'';\n\
         SELECT @sql += {} FROM {listed}\n WHERE NOT EXISTS (SELECT 1 FROM @words x WHERE x.word = w.stopword COLLATE DATABASE_DEFAULT AND x.lang = w.language_id);\n\
         SELECT @sql += {} FROM @words x\n WHERE NOT EXISTS (SELECT 1 FROM {listed} WHERE w.stopword COLLATE DATABASE_DEFAULT = x.word AND w.language_id = x.lang);\n\
         EXEC (@sql);",
        stmt("DROP", "w.stopword", "w.language_id"),
        stmt("ADD", "x.word", "x.lang"),
    ));
    s
}

const SEQUENCE_SQL: &str = "
SELECT ty.name, CASE WHEN ty.is_user_defined = 1 THEN SCHEMA_NAME(ty.schema_id) END, CAST(seq.precision AS int),
       CAST(seq.start_value AS nvarchar(40)), CAST(seq.increment AS nvarchar(40)),
       CAST(seq.minimum_value AS nvarchar(40)), CAST(seq.maximum_value AS nvarchar(40)),
       seq.is_cycling, seq.is_cached, CAST(seq.cache_size AS int)
  FROM sys.sequences seq
  JOIN sys.types ty ON ty.user_type_id = seq.user_type_id
 WHERE seq.object_id = OBJECT_ID(@P1)";

const TYPE_SQL: &str = "
SELECT ty.is_table_type, TYPE_NAME(ty.system_type_id), CAST(ty.max_length AS int), CAST(ty.precision AS int),
       CAST(ty.scale AS int), ty.is_nullable, CAST(tt.type_table_object_id AS int), {mo}
  FROM sys.types ty
  LEFT JOIN sys.table_types tt ON tt.user_type_id = ty.user_type_id
 WHERE ty.user_type_id = TYPE_ID(@P1) AND ty.is_user_defined = 1";

const TYPE_COLUMNS_SQL: &str = "
SELECT c.name, TYPE_NAME(c.user_type_id), ty.is_user_defined, SCHEMA_NAME(ty.schema_id), CAST(c.max_length AS int),
       CAST(c.precision AS int), CAST(c.scale AS int), c.is_nullable,
       CAST(idc.seed_value AS nvarchar(40)), CAST(idc.increment_value AS nvarchar(40)),
       cc.definition, CAST(ISNULL(cc.is_persisted, 0) AS bit), dc.definition
  FROM sys.columns c
  JOIN sys.types ty ON ty.user_type_id = c.user_type_id
  LEFT JOIN sys.identity_columns idc ON idc.object_id = c.object_id AND idc.column_id = c.column_id
  LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id
  LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id
 WHERE c.object_id = CAST(@P1 AS int)
 ORDER BY c.column_id";

const TYPE_INDEXES_SQL: &str = "
SELECT i.index_id, i.name, i.is_primary_key, i.is_unique_constraint, i.is_unique, CAST(i.type AS int), {bucket},
       c.name, ic.is_descending_key
  FROM sys.indexes i
  JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.key_ordinal > 0
  JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
  {hash}
 WHERE i.object_id = CAST(@P1 AS int) AND i.type > 0
 ORDER BY i.index_id, ic.key_ordinal";

const TYPE_CHECKS_SQL: &str = "SELECT definition FROM sys.check_constraints WHERE parent_object_id = CAST(@P1 AS int) ORDER BY name";

const STOPLIST_SQL: &str = "
DECLARE @id int = (SELECT stoplist_id FROM sys.fulltext_stoplists WHERE name = @P1);
DECLARE @total int = (SELECT COUNT(*) FROM sys.fulltext_system_stopwords);
DECLARE @shared int = (SELECT COUNT(*) FROM sys.fulltext_stopwords w
                        WHERE w.stoplist_id = @id AND EXISTS (SELECT 1 FROM sys.fulltext_system_stopwords s
                              WHERE s.stopword COLLATE DATABASE_DEFAULT = w.stopword COLLATE DATABASE_DEFAULT AND s.language_id = w.language_id));
DECLARE @sys bit = CASE WHEN @total > 0 AND @shared * 2 > @total THEN 1 ELSE 0 END;
SELECT N'=', CAST(NULL AS nvarchar(64)), CAST(@sys AS int) WHERE @id IS NOT NULL
UNION ALL
SELECT N'+', w.stopword COLLATE DATABASE_DEFAULT, CAST(w.language_id AS int) FROM sys.fulltext_stopwords w
 WHERE w.stoplist_id = @id AND (@sys = 0 OR NOT EXISTS (SELECT 1 FROM sys.fulltext_system_stopwords s
       WHERE s.stopword COLLATE DATABASE_DEFAULT = w.stopword COLLATE DATABASE_DEFAULT AND s.language_id = w.language_id))
UNION ALL
SELECT N'-', s.stopword COLLATE DATABASE_DEFAULT, CAST(s.language_id AS int) FROM sys.fulltext_system_stopwords s
 WHERE @sys = 1 AND NOT EXISTS (SELECT 1 FROM sys.fulltext_stopwords w
       WHERE w.stoplist_id = @id AND s.stopword COLLATE DATABASE_DEFAULT = w.stopword COLLATE DATABASE_DEFAULT AND s.language_id = w.language_id)
ORDER BY 1, 3, 2";

/// The source of a sequence, synonym, user type, full-text catalog or
/// stoplist, rebuilt from the catalog (`None` when it isn't there).
pub(crate) async fn definition(s: &mut SqlServerSession, obj: &ObjectRef) -> Result<Option<String>> {
    let schema = obj.schema();
    let full = qn(schema, &obj.name);
    match obj.kind.as_str() {
        kinds::SEQUENCE => {
            let rows = s.rows(SEQUENCE_SQL, &[&full]).await?;
            Ok(rows.first().map(|r| {
                let info = SequenceInfo {
                    type_name: st(r, 0),
                    type_schema: text(r, 1),
                    precision: n(r, 2),
                    start: st(r, 3),
                    increment: st(r, 4),
                    min: st(r, 5),
                    max: st(r, 6),
                    cycle: b(r, 7),
                    cached: b(r, 8),
                    cache_size: ni(r, 9),
                };
                sequence_sql(schema, &obj.name, &info)
            }))
        }
        kinds::SYNONYM => {
            let rows = s.rows("SELECT base_object_name FROM sys.synonyms WHERE object_id = OBJECT_ID(@P1)", &[&full]).await?;
            Ok(rows.first().and_then(|r| text(r, 0)).map(|base| synonym_sql(schema, &obj.name, &base)))
        }
        kinds::TYPE => type_definition(s, schema, &obj.name).await,
        FULLTEXT_CATALOG => {
            let rows = s.rows("SELECT is_accent_sensitivity_on, is_default FROM sys.fulltext_catalogs WHERE name = @P1", &[&obj.name]).await?;
            Ok(rows.first().map(|r| catalog_sql(&obj.name, b(r, 0), b(r, 1))))
        }
        FULLTEXT_STOPLIST => {
            let rows = s.rows(STOPLIST_SQL, &[&obj.name]).await?;
            let Some(head) = rows.iter().find(|r| text(r, 0).as_deref() == Some("=")) else { return Ok(None) };
            let from_system = n(head, 2) == 1;
            let words = |mark: &str| -> Vec<(String, i32)> {
                rows.iter().filter(|r| text(r, 0).as_deref() == Some(mark)).map(|r| (st(r, 1), n(r, 2))).collect()
            };
            Ok(Some(stoplist_sql(&obj.name, from_system, &words("+"), &words("-"))))
        }
        _ => Ok(None),
    }
}

async fn type_definition(s: &mut SqlServerSession, schema: Option<&str>, name: &str) -> Result<Option<String>> {
    let full = qn(schema, name);
    let babelfish = s.variant == Variant::Babelfish;
    let mo = if babelfish { "CAST(0 AS bit)" } else { "CAST(ISNULL(tt.is_memory_optimized, 0) AS bit)" };
    let rows = s.rows(&TYPE_SQL.replace("{mo}", mo), &[&full]).await?;
    let Some(r) = rows.first() else { return Ok(None) };
    if !b(r, 0) {
        let base = format_type(&st(r, 1), n(r, 2), n(r, 3), n(r, 4));
        return Ok(Some(alias_type_sql(schema, name, &base, b(r, 5))));
    }
    let tt = n(r, 6).to_string();
    let memory_optimized = b(r, 7);
    let cols = s
        .rows(TYPE_COLUMNS_SQL, &[&tt])
        .await?
        .iter()
        .map(|r| {
            let ty = st(r, 1);
            let data_type = if b(r, 2) { qn(text(r, 3).as_deref(), &ty) } else { format_type(&ty, n(r, 4), n(r, 5), n(r, 6)) };
            TypeColumn {
                name: st(r, 0),
                data_type,
                nullable: b(r, 7),
                identity: text(r, 8).map(|seed| (seed, text(r, 9).unwrap_or_else(|| "1".into()))),
                computed: text(r, 10).map(|e| (e, b(r, 11))),
                default: text(r, 12),
            }
        })
        .collect::<Vec<_>>();
    let (bucket, hash) = if babelfish {
        ("CAST(NULL AS bigint)", "")
    } else {
        ("CAST(hx.bucket_count AS bigint)", "LEFT JOIN sys.hash_indexes hx ON hx.object_id = i.object_id AND hx.index_id = i.index_id")
    };
    let mut ixs: Vec<(i32, TypeIndex)> = Vec::new();
    for r in s.rows(&TYPE_INDEXES_SQL.replace("{bucket}", bucket).replace("{hash}", hash), &[&tt]).await? {
        let id = n(&r, 0);
        if ixs.last().is_none_or(|(i, _)| *i != id) {
            ixs.push((
                id,
                TypeIndex {
                    name: st(&r, 1),
                    primary: b(&r, 2),
                    unique_constraint: b(&r, 3),
                    unique: b(&r, 4),
                    clustered: n(&r, 5) == 1,
                    bucket_count: r.try_get::<i64, _>(6).ok().flatten(),
                    keys: Vec::new(),
                },
            ));
        }
        if let Some((_, ix)) = ixs.last_mut() {
            ix.keys.push((st(&r, 7), b(&r, 8)));
        }
    }
    let ixs: Vec<TypeIndex> = ixs.into_iter().map(|(_, i)| i).collect();
    let checks: Vec<String> = s.rows(TYPE_CHECKS_SQL, &[&tt]).await?.iter().filter_map(|r| text(r, 0)).collect();
    Ok(Some(table_type_sql(schema, name, &cols, &ixs, &checks, memory_optimized)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, DdlParts};

    fn raw(name: &str, ty: i32, desc: &str) -> RawIndex {
        RawIndex { name: name.into(), ty, type_desc: desc.into(), row_locks: true, page_locks: true, ..Default::default() }
    }

    #[test]
    fn rowstore_index_options_include_and_desc() {
        let mut r = raw("IX_a", 2, "NONCLUSTERED");
        r.keys = vec![("a".into(), false), ("b".into(), true)];
        r.include = vec!["c".into(), "d".into()];
        r.filter = Some("([a] IS NOT NULL)".into());
        r.fill_factor = 80;
        r.padded = true;
        r.ignore_dup_key = true;
        r.page_locks = false;
        r.no_recompute = true;
        r.sequential_key = true;
        r.compression = vec![(1, "PAGE".into())];
        let ix = r.to_index();
        assert_eq!(ix.kind.as_deref(), Some("NONCLUSTERED"));
        assert_eq!(ix.columns, vec!["a", "b"]);
        assert_eq!(ix.include, vec!["c", "d"]);
        assert_eq!(ix.filter.as_deref(), Some("[a] IS NOT NULL"));
        let o: Vec<(&str, &str)> = ix.options.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            o,
            vec![
                ("ALLOW_PAGE_LOCKS", "OFF"),
                ("DATA_COMPRESSION", "PAGE"),
                ("FILLFACTOR", "80"),
                ("IGNORE_DUP_KEY", "ON"),
                ("OPTIMIZE_FOR_SEQUENTIAL_KEY", "ON"),
                ("PAD_INDEX", "ON"),
                ("STATISTICS_NORECOMPUTE", "ON"),
                ("desc", "b"),
            ]
        );
        let t = TableSchema { schema: Some("dbo".into()), name: "t".into(), indexes: vec![ix], ..Default::default() };
        assert_eq!(
            index_statements("[dbo].[t]", &t, false),
            vec!["CREATE NONCLUSTERED INDEX [IX_a] ON [dbo].[t] ([a], [b] DESC) INCLUDE ([c], [d]) WHERE [a] IS NOT NULL WITH (ALLOW_PAGE_LOCKS = OFF, DATA_COMPRESSION = PAGE, FILLFACTOR = 80, IGNORE_DUP_KEY = ON, OPTIMIZE_FOR_SEQUENTIAL_KEY = ON, PAD_INDEX = ON, STATISTICS_NORECOMPUTE = ON);"]
        );
    }

    #[test]
    fn defaults_leave_no_options() {
        let mut r = raw("IX", 2, "NONCLUSTERED");
        r.keys = vec![("a".into(), false)];
        r.fill_factor = 0;
        r.compression = vec![(1, "NONE".into())];
        assert!(r.to_index().options.is_empty());
        r.fill_factor = 100;
        assert!(r.to_index().options.is_empty());
        let mut cs = raw("CCI", 5, "CLUSTERED COLUMNSTORE");
        cs.compression = vec![(1, "COLUMNSTORE".into())];
        // The catalog says locks off for columnstore; CREATE refuses the option.
        cs.row_locks = false;
        cs.page_locks = false;
        let ix = cs.to_index();
        assert!(ix.options.is_empty() && ix.columns.is_empty(), "{ix:?}");
    }

    #[test]
    fn mixed_partition_compression() {
        assert_eq!(compression_of(&[(1, "PAGE".into()), (2, "PAGE".into()), (3, "NONE".into())]).as_deref(), Some("PAGE ON PARTITIONS (1, 2); NONE ON PARTITIONS (3)"));
        let mut r = raw("IX", 2, "NONCLUSTERED");
        r.keys = vec![("a".into(), false)];
        r.compression = vec![(1, "ROW".into()), (2, "PAGE".into())];
        let t = TableSchema { name: "t".into(), indexes: vec![r.to_index()], ..Default::default() };
        assert_eq!(
            index_statements("[t]", &t, false)[0],
            "CREATE NONCLUSTERED INDEX [IX] ON [t] ([a]) WITH (DATA_COMPRESSION = ROW ON PARTITIONS (1), DATA_COMPRESSION = PAGE ON PARTITIONS (2));"
        );
    }

    #[test]
    fn columnstore_indexes() {
        let mut cci = raw("CCI", 5, "CLUSTERED COLUMNSTORE");
        cci.compression = vec![(1, "COLUMNSTORE_ARCHIVE".into())];
        cci.order = vec![(2, "b".into()), (1, "a".into())];
        cci.compression_delay = 10;
        let mut nc = raw("NCCI", 6, "NONCLUSTERED COLUMNSTORE");
        nc.columns = vec!["x".into(), "y".into()];
        nc.filter = Some("([x]>(0))".into());
        nc.compression = vec![(1, "COLUMNSTORE".into())];
        let t = TableSchema { name: "t".into(), indexes: vec![nc.to_index(), cci.to_index()], ..Default::default() };
        let s = index_statements("[dbo].[t]", &t, false);
        assert_eq!(s[0], "CREATE CLUSTERED COLUMNSTORE INDEX [CCI] ON [dbo].[t] ORDER ([a], [b]) WITH (COMPRESSION_DELAY = 10, DATA_COMPRESSION = COLUMNSTORE_ARCHIVE);");
        assert_eq!(s[1], "CREATE NONCLUSTERED COLUMNSTORE INDEX [NCCI] ON [dbo].[t] ([x], [y]) WHERE [x]>(0);");
    }

    #[test]
    fn xml_spatial_and_hash_indexes() {
        let mut px = raw("PX", 3, "XML");
        px.keys = vec![("doc".into(), false)];
        let mut sx = raw("SX", 3, "XML");
        sx.keys = vec![("doc".into(), false)];
        sx.xml_primary = Some("PX".into());
        sx.xml_secondary = Some("PATH".into());
        sx.fill_factor = 90;
        let sx = sx.to_index();
        assert_eq!(sx.kind.as_deref(), Some("XML PATH"));
        assert_eq!(sx.options.get("primary_xml_index").map(String::as_str), Some("PX"));
        let mut sp = raw("SP", 4, "SPATIAL");
        sp.keys = vec![("geo".into(), false)];
        sp.tessellation = Some("GEOMETRY_GRID".into());
        sp.bounding_box = Some([0.0, 0.0, 100.5, 100.0]);
        sp.grids = Some(["MEDIUM".into(), "MEDIUM".into(), "LOW".into(), "HIGH".into()]);
        sp.cells_per_object = Some(16);
        let mut auto = raw("SPA", 4, "SPATIAL");
        auto.keys = vec![("geo".into(), false)];
        auto.tessellation = Some("GEOGRAPHY_AUTO_GRID".into());
        auto.grids = Some(["MEDIUM".into(), "MEDIUM".into(), "MEDIUM".into(), "MEDIUM".into()]);
        auto.cells_per_object = Some(12);
        let mut hash = raw("HX", 7, "NONCLUSTERED HASH");
        hash.keys = vec![("id".into(), false)];
        hash.bucket_count = Some(1024);
        let t = TableSchema { name: "t".into(), indexes: vec![sx, hash.to_index(), px.to_index(), sp.to_index(), auto.to_index()], ..Default::default() };
        let s = index_statements("[dbo].[t]", &t, false);
        assert_eq!(
            s,
            vec![
                "ALTER TABLE [dbo].[t] ADD INDEX [HX] NONCLUSTERED HASH ([id]) WITH (BUCKET_COUNT = 1024);",
                "CREATE SPATIAL INDEX [SP] ON [dbo].[t] ([geo]) USING GEOMETRY_GRID WITH (BOUNDING_BOX = (0, 0, 100.5, 100), CELLS_PER_OBJECT = 16, GRIDS = (LEVEL_1 = MEDIUM, LEVEL_2 = MEDIUM, LEVEL_3 = LOW, LEVEL_4 = HIGH));",
                "CREATE SPATIAL INDEX [SPA] ON [dbo].[t] ([geo]) USING GEOGRAPHY_AUTO_GRID WITH (CELLS_PER_OBJECT = 12);",
                "CREATE PRIMARY XML INDEX [PX] ON [dbo].[t] ([doc]);",
                "CREATE XML INDEX [SX] ON [dbo].[t] ([doc]) USING XML INDEX [PX] FOR PATH WITH (FILLFACTOR = 90);",
            ]
        );
    }

    #[test]
    fn unique_constraints_are_constraints() {
        let mut r = raw("UQ_email", 2, "NONCLUSTERED");
        r.unique = true;
        r.unique_constraint = true;
        r.keys = vec![("email".into(), false)];
        let t = TableSchema { name: "t".into(), indexes: vec![r.to_index()], ..Default::default() };
        assert_eq!(index_statements("[dbo].[t]", &t, false), vec!["ALTER TABLE [dbo].[t] ADD CONSTRAINT [UQ_email] UNIQUE NONCLUSTERED ([email]);"]);
        let g = index_statements("[dbo].[t]", &t, true);
        assert!(g[0].starts_with("IF NOT EXISTS (SELECT 1 FROM sys.indexes WHERE object_id = OBJECT_ID(N'[dbo].[t]') AND name = N'UQ_email')\n    ALTER TABLE"), "{g:?}");
    }

    fn fulltext() -> RawFullText {
        RawFullText {
            key_index: "PK_docs".into(),
            catalog: Some("cat".into()),
            change_tracking: "MANUAL".into(),
            stoplist_id: Some(5),
            stoplist: Some("palabras".into()),
            property_list: None,
            columns: vec![("titulo".into(), 1033, None, false), ("cuerpo".into(), 3082, Some("ext".into()), false)],
        }
    }

    #[test]
    fn fulltext_index_from_the_catalog() {
        let ix = fulltext().to_index(1033);
        assert_eq!(ix.name, "fulltext");
        assert_eq!(ix.kind.as_deref(), Some("FULLTEXT"));
        assert_eq!(ix.columns, vec!["titulo", "cuerpo"]);
        let o: Vec<(&str, &str)> = ix.options.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            o,
            vec![
                ("CATALOG", "cat"),
                ("CHANGE_TRACKING", "MANUAL"),
                ("KEY INDEX", "PK_docs"),
                ("LANGUAGE cuerpo", "3082"),
                ("STOPLIST", "palabras"),
                ("TYPE COLUMN cuerpo", "ext"),
            ]
        );
        // Defaults: AUTO tracking, the system stoplist, the server's language.
        let d = RawFullText { change_tracking: "AUTO".into(), stoplist_id: Some(0), stoplist: None, ..fulltext() }.to_index(3082);
        let keys: Vec<&str> = d.options.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["CATALOG", "KEY INDEX", "LANGUAGE titulo", "TYPE COLUMN cuerpo"]);
        let off = RawFullText { stoplist_id: None, stoplist: None, ..fulltext() }.to_index(1033);
        assert_eq!(off.options.get("STOPLIST").map(String::as_str), Some("OFF"));
    }

    #[test]
    fn fulltext_ddl_comes_after_its_key_index() {
        let ft = fulltext().to_index(1033);
        let mut key = raw("UX_docs", 2, "NONCLUSTERED");
        key.unique = true;
        key.keys = vec![("id".into(), false)];
        let t = TableSchema { schema: Some("dbo".into()), name: "docs".into(), indexes: vec![ft, key.to_index()], ..Default::default() };
        let s = index_statements("[dbo].[docs]", &t, false);
        assert_eq!(s[0], "CREATE UNIQUE NONCLUSTERED INDEX [UX_docs] ON [dbo].[docs] ([id]);");
        assert_eq!(
            s[1],
            "CREATE FULLTEXT INDEX ON [dbo].[docs] ([titulo], [cuerpo] TYPE COLUMN [ext] LANGUAGE 3082) KEY INDEX [PK_docs] ON [cat] WITH (CHANGE_TRACKING = MANUAL, STOPLIST = [palabras]);"
        );
        let g = index_statements("[dbo].[docs]", &t, true);
        assert!(g[1].starts_with("IF NOT EXISTS (SELECT 1 FROM sys.fulltext_indexes WHERE object_id = OBJECT_ID(N'[dbo].[docs]'))\n    CREATE FULLTEXT"), "{g:?}");
        let sys = IndexDef { options: [("KEY INDEX".to_string(), "PK".to_string()), ("STOPLIST".into(), "OFF".into())].into(), ..t.indexes[0].clone() };
        assert_eq!(fulltext_sql("[t]", &sys), "CREATE FULLTEXT INDEX ON [t] ([titulo], [cuerpo]) KEY INDEX [PK] WITH (STOPLIST = OFF);");
    }

    #[test]
    fn drops_follow_dependencies() {
        let mut v = [
            IndexDef { name: "c".into(), kind: Some("CLUSTERED".into()), ..Default::default() },
            IndexDef { name: "px".into(), kind: Some("XML".into()), ..Default::default() },
            IndexDef { name: "n".into(), kind: Some("NONCLUSTERED".into()), ..Default::default() },
            IndexDef { name: "sx".into(), kind: Some("XML PATH".into()), ..Default::default() },
            IndexDef { name: "fulltext".into(), kind: Some("FULLTEXT".into()), ..Default::default() },
        ];
        v.sort_by_key(drop_rank);
        let names: Vec<&str> = v.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["fulltext", "sx", "px", "n", "c"]);
    }

    fn table(indexes: Vec<IndexDef>) -> TableSchema {
        TableSchema {
            schema: Some("dbo".into()),
            name: "docs".into(),
            columns: vec![ColumnDef { name: "id".into(), data_type: "int".into(), nullable: false, ..Default::default() }],
            primary_key: Some(KeyDef { name: Some("PK_docs".into()), columns: vec!["id".into()] }),
            indexes,
            ..Default::default()
        }
    }

    fn sync(changes: &[TableChange]) -> Vec<String> {
        use dbine_driver::alter::{AlterStyle, ColumnAlter, DropIndex};
        let cd = |t: &TableSchema, c: &ColumnDef| dbine_driver::ddl::column_def(&crate::schema::FLAVOR, t, c);
        let dd = |t: &TableSchema, p: DdlParts| Ok(crate::schema::table_ddl(t, p));
        let mut st = AlterStyle::from_flavor(&crate::schema::FLAVOR, ColumnAlter::SqlServer, &cd, &dd);
        st.add_column = "ADD";
        st.drop_index = DropIndex::OnTable;
        let prepared = prepare_changes(changes);
        let comments = |t: &TableSchema, c: Option<&ColumnDef>, v: Option<&str>| Some(crate::schema::comment_change(t, c, v));
        let mut s = dbine_driver::alter::sync_script_with_comments(&st, Some(&comments), &prepared).unwrap();
        fix_drops(&mut s.statements, &prepared);
        s.statements
    }

    #[test]
    fn comments_sync_as_extended_properties() {
        let mut old = table(vec![]);
        old.columns.push(ColumnDef { name: "nombre".into(), data_type: "nvarchar(50)".into(), nullable: true, comment: Some("viejo".into()), ..Default::default() });
        old.columns.push(ColumnDef { name: "baja".into(), data_type: "date".into(), nullable: true, comment: Some("se va".into()), ..Default::default() });
        let mut new = old.clone();
        new.comment = Some("Documentos".into());
        new.columns[1].comment = Some("it's new".into());
        new.columns[2].comment = None;
        new.columns.push(ColumnDef { name: "email".into(), data_type: "nvarchar(100)".into(), nullable: true, comment: Some("correo".into()), ..Default::default() });
        let s = sync(&[TableChange::Alter { old: old.clone(), new: new.clone() }]);
        assert_eq!(s.len(), 5, "{s:#?}");
        assert_eq!(s[0], "ALTER TABLE [dbo].[docs] ADD [email] nvarchar(100) NULL;");
        let upsert = |s: &str, levels: &str| {
            s.contains(&format!("IF NOT EXISTS (SELECT 1 FROM sys.fn_listextendedproperty(N'MS_Description', {levels}))"))
                && s.contains("EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = @v")
                && s.contains("ELSE\n    EXEC sys.sp_updateextendedproperty @name = N'MS_Description', @value = @v")
        };
        assert!(upsert(&s[1], "N'SCHEMA', N'dbo', N'TABLE', N'docs', N'COLUMN', N'email'") && s[1].starts_with("DECLARE @v sql_variant = N'correo';"), "{}", s[1]);
        assert!(upsert(&s[2], "N'SCHEMA', N'dbo', N'TABLE', N'docs', N'COLUMN', N'nombre'") && s[2].starts_with("DECLARE @v sql_variant = N'it''s new';"), "{}", s[2]);
        assert_eq!(
            s[3],
            "IF EXISTS (SELECT 1 FROM sys.fn_listextendedproperty(N'MS_Description', N'SCHEMA', N'dbo', N'TABLE', N'docs', N'COLUMN', N'baja'))\n    EXEC sys.sp_dropextendedproperty @name = N'MS_Description', @level0type = N'SCHEMA', @level0name = N'dbo', @level1type = N'TABLE', @level1name = N'docs', @level2type = N'COLUMN', @level2name = N'baja';"
        );
        assert!(upsert(&s[4], "N'SCHEMA', N'dbo', N'TABLE', N'docs', NULL, NULL") && s[4].starts_with("DECLARE @v sql_variant = N'Documentos';"), "{}", s[4]);
        // And back: the table's comment goes.
        let s = sync(&[TableChange::Alter { old: new, new: old }]);
        assert!(s.last().unwrap().starts_with("IF EXISTS (SELECT 1 FROM sys.fn_listextendedproperty(N'MS_Description', N'SCHEMA', N'dbo', N'TABLE', N'docs', NULL, NULL))\n    EXEC sys.sp_dropextendedproperty"), "{s:#?}");
        assert!(!s.last().unwrap().contains("@level2type"), "{s:#?}");
    }

    #[test]
    fn key_index_change_takes_the_fulltext_index_along() {
        let ft = fulltext().to_index(1033);
        let ft = IndexDef { options: [("KEY INDEX".to_string(), "UX".to_string()), ("CATALOG".into(), "cat".into())].into(), ..ft };
        let ux = IndexDef { name: "UX".into(), columns: vec!["id".into()], unique: true, kind: Some("NONCLUSTERED".into()), ..Default::default() };
        let mut ux2 = ux.clone();
        ux2.options.insert("FILLFACTOR".into(), "80".into());
        let old = table(vec![ft.clone(), ux]);
        let new = table(vec![ft, ux2]);
        let s = sync(&[TableChange::Alter { old, new }]);
        let pos = |p: &str| s.iter().position(|x| x.contains(p)).unwrap_or_else(|| panic!("{p} in {s:#?}"));
        assert!(pos("DROP FULLTEXT INDEX ON [dbo].[docs]") < pos("DROP INDEX [UX] ON [dbo].[docs]"), "{s:#?}");
        let create = &s[pos("CREATE FULLTEXT INDEX")];
        assert!(create.find("CREATE UNIQUE NONCLUSTERED INDEX [UX]").unwrap() < create.find("CREATE FULLTEXT INDEX").unwrap(), "{create}");
    }

    #[test]
    fn primary_xml_change_takes_the_secondary_along() {
        let px = IndexDef { name: "PX".into(), columns: vec!["doc".into()], kind: Some("XML".into()), ..Default::default() };
        let sx = IndexDef {
            name: "SX".into(),
            columns: vec!["doc".into()],
            kind: Some("XML PATH".into()),
            options: [("primary_xml_index".to_string(), "PX".to_string())].into(),
            ..Default::default()
        };
        let mut px2 = px.clone();
        px2.options.insert("FILLFACTOR".into(), "70".into());
        let s = sync(&[TableChange::Alter { old: table(vec![sx.clone(), px]), new: table(vec![sx, px2]) }]);
        let joined = s.join("\n");
        assert!(joined.find("DROP INDEX [SX]").unwrap() < joined.find("DROP INDEX [PX]").unwrap(), "{joined}");
        assert!(joined.find("CREATE PRIMARY XML INDEX [PX]").unwrap() < joined.find("CREATE XML INDEX [SX]").unwrap(), "{joined}");
    }

    #[test]
    fn unique_constraints_and_hash_indexes_drop_their_own_way() {
        let uq = IndexDef {
            name: "UQ".into(),
            columns: vec!["id".into()],
            unique: true,
            kind: Some("NONCLUSTERED".into()),
            options: [("unique_constraint".to_string(), "ON".to_string())].into(),
            ..Default::default()
        };
        let hx = IndexDef { name: "HX".into(), columns: vec!["id".into()], kind: Some("NONCLUSTERED HASH".into()), ..Default::default() };
        let s = sync(&[TableChange::Alter { old: table(vec![uq, hx]), new: table(vec![]) }]);
        assert!(s.contains(&"ALTER TABLE [dbo].[docs] DROP CONSTRAINT [UQ];".to_string()), "{s:#?}");
        assert!(s.contains(&"ALTER TABLE [dbo].[docs] DROP INDEX [HX];".to_string()), "{s:#?}");
    }

    #[test]
    fn checks_go_in_create_table_and_through_alter() {
        let mut t = table(vec![]);
        t.checks = vec![CheckDef { name: Some("CK_id".into()), expression: "([id]>(0))".into() }];
        let s = crate::schema::table_ddl(&t, DdlParts { create: true, ..Default::default() });
        assert!(s.contains("CONSTRAINT [CK_id] CHECK ([id]>(0))"), "{s}");
        let new = TableSchema { checks: vec![CheckDef { name: Some("CK_id".into()), expression: "([id]>(1))".into() }], ..t.clone() };
        let st = sync(&[TableChange::Alter { old: t, new }]);
        assert_eq!(st, vec!["ALTER TABLE [dbo].[docs] DROP CONSTRAINT [CK_id];", "ALTER TABLE [dbo].[docs] ADD CONSTRAINT [CK_id] CHECK ([id]>(1));"]);
    }

    #[test]
    fn sequence_synonym_and_types() {
        let s = SequenceInfo {
            type_name: "decimal".into(),
            precision: 12,
            start: "10".into(),
            increment: "5".into(),
            min: "1".into(),
            max: "999999".into(),
            cycle: true,
            cached: true,
            cache_size: Some(20),
            ..Default::default()
        };
        assert_eq!(
            sequence_sql(Some("ventas"), "folio", &s),
            "CREATE SEQUENCE [ventas].[folio]\n    AS decimal(12, 0)\n    START WITH 10\n    INCREMENT BY 5\n    MINVALUE 1\n    MAXVALUE 999999\n    CYCLE\n    CACHE 20;"
        );
        let plain = SequenceInfo { type_name: "bigint".into(), cached: false, ..s.clone() };
        assert!(sequence_sql(None, "n", &plain).contains("AS bigint\n") && sequence_sql(None, "n", &plain).ends_with("NO CACHE;"));
        let alias = SequenceInfo { type_name: "Folio".into(), type_schema: Some("dbo".into()), cached: true, cache_size: None, cycle: false, ..s };
        let a = sequence_sql(Some("dbo"), "n", &alias);
        assert!(a.contains("AS [dbo].[Folio]\n") && a.contains("NO CYCLE\n    CACHE;"), "{a}");
        assert_eq!(synonym_sql(Some("dbo"), "cli", "[otra].[dbo].[clientes]"), "CREATE SYNONYM [dbo].[cli] FOR [otra].[dbo].[clientes];");
        assert_eq!(alias_type_sql(Some("dbo"), "Email", "nvarchar(120)", false), "CREATE TYPE [dbo].[Email] FROM nvarchar(120) NOT NULL;");
        let cols = vec![
            TypeColumn { name: "id".into(), data_type: "int".into(), identity: Some(("1".into(), "1".into())), ..Default::default() },
            TypeColumn { name: "mail".into(), data_type: "[dbo].[Email]".into(), nullable: true, default: Some("(N'x')".into()), ..Default::default() },
            TypeColumn { name: "doble".into(), computed: Some(("([id]*(2))".into(), false)), ..Default::default() },
        ];
        let ixs = vec![
            TypeIndex { name: "PK__x".into(), primary: true, unique: true, clustered: true, keys: vec![("id".into(), false)], ..Default::default() },
            TypeIndex { name: "IX_mail".into(), keys: vec![("mail".into(), true)], ..Default::default() },
        ];
        assert_eq!(
            table_type_sql(Some("dbo"), "Lineas", &cols, &ixs, &["([id]>(0))".into()], false),
            "CREATE TYPE [dbo].[Lineas] AS TABLE (\n    [id] int IDENTITY(1, 1) NOT NULL,\n    [mail] [dbo].[Email] DEFAULT (N'x') NULL,\n    [doble] AS ([id]*(2)),\n    PRIMARY KEY CLUSTERED ([id]),\n    INDEX [IX_mail] NONCLUSTERED ([mail] DESC),\n    CHECK ([id]>(0))\n);"
        );
        let mo = table_type_sql(None, "M", &cols[..1], &[TypeIndex { primary: true, bucket_count: Some(64), keys: vec![("id".into(), false)], ..Default::default() }], &[], true);
        assert!(mo.contains("PRIMARY KEY NONCLUSTERED HASH ([id]) WITH (BUCKET_COUNT = 64)") && mo.ends_with(") WITH (MEMORY_OPTIMIZED = ON);"), "{mo}");
    }

    #[test]
    fn fulltext_catalog_and_stoplist_apply_in_place() {
        let c = catalog_sql("cat", false, true);
        assert_eq!(
            c,
            "IF NOT EXISTS (SELECT 1 FROM sys.fulltext_catalogs WHERE name = N'cat')\n    CREATE FULLTEXT CATALOG [cat] WITH ACCENT_SENSITIVITY = OFF;\n\
             ELSE IF (SELECT is_accent_sensitivity_on FROM sys.fulltext_catalogs WHERE name = N'cat') <> 0\n    ALTER FULLTEXT CATALOG [cat] REBUILD WITH ACCENT_SENSITIVITY = OFF;\n\
             ALTER FULLTEXT CATALOG [cat] AS DEFAULT;"
        );
        assert!(!catalog_sql("cat", true, false).contains("AS DEFAULT") && catalog_sql("cat", true, false).contains("<> 1"));
        let s = stoplist_sql("sl", true, &[("dbine".into(), 1033), ("o'clock".into(), 3082)], &[("a".into(), 1033)]);
        assert!(s.starts_with("IF NOT EXISTS (SELECT 1 FROM sys.fulltext_stoplists WHERE name = N'sl')\n    CREATE FULLTEXT STOPLIST [sl] FROM SYSTEM STOPLIST;"), "{s}");
        assert!(s.contains("INSERT INTO @words SELECT stopword, language_id FROM sys.fulltext_system_stopwords;"), "{s}");
        assert!(s.contains("DELETE FROM @words WHERE (lang = 1033 AND word = N'a');"), "{s}");
        assert!(s.contains("INSERT INTO @words VALUES (N'dbine', 1033),\n    (N'o''clock', 3082);"), "{s}");
        assert!(s.contains("N'ALTER FULLTEXT STOPLIST [sl] ADD N' + QUOTENAME(x.word, '''')"), "{s}");
        assert!(s.ends_with("EXEC (@sql);"), "{s}");
        let empty = stoplist_sql("sl", false, &[], &[]);
        assert!(!empty.contains("FROM SYSTEM STOPLIST") && !empty.contains("fulltext_system_stopwords;"), "{empty}");
    }

    #[test]
    fn kinds_per_engine() {
        let ids = |v| object_kinds(v).iter().map(|k| k.id).collect::<Vec<_>>();
        assert_eq!(ids(Variant::SqlServer), vec!["sequence", "synonym", "type", "fulltext_catalog", "fulltext_stoplist"]);
        assert_eq!(ids(Variant::AzureSql), ids(Variant::SqlServer));
        assert_eq!(ids(Variant::Babelfish), vec!["type"]);
        assert!(ids(Variant::Fabric).is_empty());
    }
}
