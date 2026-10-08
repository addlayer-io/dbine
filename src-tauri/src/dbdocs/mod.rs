//! "Documentar la base" (docs/documentar-la-base.md): a data dictionary of
//! a database in one file, HTML (inline CSS, index, search, light/dark,
//! printable) or Markdown. [`collect`] reads what the engine reports through
//! the driver contract (`database_schema`, `list_objects`, `columns`,
//! `definition`) into a [`Doc`]; `html` and `markdown` render it, escaping
//! every name, comment and source text; `diagram` draws the ER diagram.
//!
//! The document's own texts come from the UI's `dbDocs:doc.*` keys
//! ([`Labels`]): the dialog sends them in the user's language, and the
//! Spanish file is the fallback (scheduled runs, older steps).

pub mod diagram;
pub mod html;
pub mod markdown;

use dbine_driver::dependencies::{find_mentions, schema_dependents};
use dbine_driver::{kinds, ColumnDef, Confidence, DependencyTarget, Driver, KeyDef, ObjectRef, Relation, Session, TableSchema};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

/// Tables per diagram at most: past it, one diagram per schema, or none.
pub const DIAGRAM_MAX: usize = 150;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocFormat {
    #[default]
    Html,
    Markdown,
}

impl DocFormat {
    pub fn extension(self) -> &'static str {
        match self {
            DocFormat::Html => "html",
            DocFormat::Markdown => "md",
        }
    }
}

/// What the dialog (or a scheduled step) asks for.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DocOptions {
    pub format: DocFormat,
    /// Empty: every schema.
    pub schemas: Vec<String>,
    pub tables: bool,
    pub views: bool,
    pub routines: bool,
    pub triggers: bool,
    /// Sequences, types, synonyms… (whatever else the engine lists).
    pub others: bool,
    /// The code of views, routines and triggers.
    pub source: bool,
    pub indexes: bool,
    pub foreign_keys: bool,
    /// "Usada por" on each table.
    pub dependencies: bool,
    /// The ER diagram (HTML only).
    pub diagram: bool,
    /// `dbDocs:doc.*` in the user's language, flattened (`kinds.view`).
    pub labels: BTreeMap<String, String>,
}

impl Default for DocOptions {
    fn default() -> Self {
        DocOptions {
            format: DocFormat::Html,
            schemas: Vec::new(),
            tables: true,
            views: true,
            routines: true,
            triggers: true,
            others: true,
            source: true,
            indexes: true,
            foreign_keys: true,
            dependencies: true,
            diagram: true,
            labels: BTreeMap::new(),
        }
    }
}

// -- labels ------------------------------------------------------------------

/// The document's texts: the caller's, over the Spanish ones.
pub struct Labels(BTreeMap<String, String>);

fn spanish() -> &'static BTreeMap<String, String> {
    static ES: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    ES.get_or_init(|| {
        let all: serde_json::Value = serde_json::from_str(include_str!("../../../web/src/locales/es/dbDocs.json")).unwrap_or_default();
        let mut out = BTreeMap::new();
        flatten(&all["doc"], "", &mut out);
        out
    })
}

fn flatten(v: &serde_json::Value, prefix: &str, out: &mut BTreeMap<String, String>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(v, &key, out);
            }
        }
        serde_json::Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        _ => {}
    }
}

impl Labels {
    pub fn new(given: &BTreeMap<String, String>) -> Self {
        let mut all = spanish().clone();
        all.extend(given.iter().filter(|(_, v)| !v.trim().is_empty()).map(|(k, v)| (k.clone(), v.clone())));
        Labels(all)
    }

    pub fn get<'a>(&'a self, key: &'a str) -> &'a str {
        self.0.get(key).map(String::as_str).unwrap_or(key)
    }

    /// With `{{name}}` placeholders filled.
    pub fn fill(&self, key: &str, args: &[(&str, String)]) -> String {
        let mut s = self.get(key).to_string();
        for (k, v) in args {
            s = s.replace(&format!("{{{{{k}}}}}"), v);
        }
        s
    }

    /// A kind's plural title: ours when we have it, else the driver's.
    pub fn kind(&self, id: &str, driver_label: &str) -> String {
        self.0.get(&format!("kinds.{id}")).cloned().unwrap_or_else(|| driver_label.to_string())
    }
}

// -- the document ------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Doc {
    pub database: String,
    pub connection: String,
    pub engine: String,
    pub version: Option<String>,
    pub generated_at: String,
    pub schemas: Vec<SchemaDoc>,
    /// What couldn't be read, for the reader.
    pub notes: Vec<String>,
    pub indexes: bool,
    pub foreign_keys: bool,
    pub source: bool,
    pub dependencies: bool,
    pub diagram: bool,
}

impl Doc {
    pub fn table_count(&self) -> usize {
        self.schemas.iter().map(|s| s.tables.len()).sum()
    }
    pub fn object_count(&self) -> usize {
        self.schemas.iter().flat_map(|s| &s.groups).map(|g| g.items.len()).sum()
    }
}

#[derive(Debug, Clone, Default)]
pub struct SchemaDoc {
    /// `None` on engines without schemas.
    pub name: Option<String>,
    pub tables: Vec<TableDoc>,
    /// Views, routines, triggers… one group per kind.
    pub groups: Vec<ObjectGroup>,
}

#[derive(Debug, Clone, Default)]
pub struct TableDoc {
    pub table: TableSchema,
    /// The kind's singular-less title ("Tablas", "Colecciones").
    pub kind_label: String,
    pub used_by: Vec<UsedBy>,
    /// Its triggers' names.
    pub triggers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsedBy {
    pub kind_label: String,
    pub schema: Option<String>,
    pub name: String,
    /// The foreign key (`fk_x (a) → t (id)`), or how the code names it.
    pub how: String,
    /// The table is a foreign key's holder, linkable in the document.
    pub table: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ObjectGroup {
    pub kind: String,
    pub label: String,
    pub items: Vec<ObjectDoc>,
}

#[derive(Debug, Clone, Default)]
pub struct ObjectDoc {
    pub schema: Option<String>,
    pub name: String,
    /// A trigger's table.
    pub parent: Option<String>,
    pub columns: Vec<ColumnDef>,
    pub source: Option<String>,
}

// -- reading -----------------------------------------------------------------

/// Where a kind goes in the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Table,
    View,
    Routine,
    Trigger,
    Other,
}

fn section_of(kind: &str, has_columns: bool) -> Section {
    match kind {
        kinds::TABLE | kinds::COLLECTION => Section::Table,
        kinds::VIEW | kinds::MATERIALIZED_VIEW => Section::View,
        kinds::PROCEDURE | kinds::FUNCTION | "package" | "package_body" | "aggregate" => Section::Routine,
        kinds::TRIGGER => Section::Trigger,
        _ if has_columns => Section::Table,
        _ => Section::Other,
    }
}

fn wanted(section: Section, kind: &str, o: &DocOptions) -> bool {
    match section {
        Section::Table => o.tables,
        Section::View => o.views,
        Section::Routine => o.routines,
        Section::Trigger => o.triggers,
        Section::Other => o.others && (kind != kinds::INDEX || o.indexes),
    }
}

/// Who's asking: the header's facts and how to report progress.
pub struct Context<'a> {
    pub database: &'a str,
    pub connection: &'a str,
    /// (done, total, phase): `phase` is a `dbDocs:phase.*` key.
    pub progress: &'a (dyn Fn(usize, usize, &'static str) + Send + Sync),
    pub cancelled: &'a (dyn Fn() -> bool + Send + Sync),
}

fn key(schema: Option<&str>, name: &str) -> (String, String) {
    (schema.unwrap_or("").to_lowercase(), name.to_lowercase())
}

fn shown(schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// Read everything the options ask for. Parts the engine doesn't have, or
/// that fail, become notes; only the object list is required.
pub async fn collect(s: &mut dyn Session, driver: &dyn Driver, opts: &DocOptions, labels: &Labels, cx: &Context<'_>) -> crate::error::CommandResult<Doc> {
    let info = driver.info();
    let mut doc = Doc {
        database: cx.database.to_string(),
        connection: cx.connection.to_string(),
        engine: info.name.to_string(),
        generated_at: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        indexes: opts.indexes,
        foreign_keys: opts.foreign_keys,
        source: opts.source,
        dependencies: opts.dependencies && driver.supports_dependencies(),
        diagram: opts.diagram && opts.format == DocFormat::Html,
        ..Default::default()
    };
    let cancelled = || -> crate::error::CommandResult<()> { if (cx.cancelled)() { Err(crate::error::CommandError::Cancelled) } else { Ok(()) } };
    (cx.progress)(0, 0, "objects");
    doc.version = s.server_version().await.ok().filter(|v| !v.trim().is_empty());
    let kind_info: HashMap<&str, &dbine_driver::ObjectKindInfo> = info.object_kinds.iter().map(|k| (k.id, k)).collect();
    let has_columns = |kind: &str| kind_info.get(kind).is_some_and(|k| k.has_columns);
    let has_source = |kind: &str| kind_info.get(kind).is_some_and(|k| k.has_definition);
    let driver_label = |kind: &str| kind_info.get(kind).map(|k| k.label.to_string()).unwrap_or_else(|| kind.to_string());
    let in_scope = |schema: Option<&str>| opts.schemas.is_empty() || schema.is_none_or(|sc| opts.schemas.iter().any(|w| w.eq_ignore_ascii_case(sc)));

    let objects: Vec<_> = s.list_objects().await?.into_iter().filter(|o| in_scope(o.schema.as_deref())).collect();
    cancelled()?;

    // Tables, keys and indexes from the catalog, when it has them.
    let need_schema = opts.tables || opts.views || doc.diagram || doc.dependencies;
    let mut tables: Vec<TableSchema> = Vec::new();
    if need_schema {
        (cx.progress)(0, 0, "schema");
        match s.database_schema().await {
            Ok(list) => tables = list.into_iter().filter(|t| in_scope(t.schema.as_deref())).collect(),
            Err(dbine_driver::Error::Unsupported(_)) => {}
            Err(e) => doc.notes.push(format!("{}: {e}", labels.get("tables"))),
        }
    }
    cancelled()?;
    let known: HashSet<(String, String)> = tables.iter().map(|t| key(t.schema.as_deref(), &t.name)).collect();

    // Columns of what the catalog read left out (views, measurements…).
    let missing: Vec<_> = objects
        .iter()
        .filter(|o| has_columns(&o.kind) && !known.contains(&key(o.schema.as_deref(), &o.name)))
        .filter(|o| {
            let sec = section_of(&o.kind, true);
            matches!(sec, Section::Table | Section::View) && wanted(sec, &o.kind, opts)
        })
        .collect();
    // Source: what's documented with its code, and every code object when
    // "Usada por" needs to search it.
    let code_kinds = dbine_driver::dependencies::code_kinds(info);
    let read_source: Vec<_> = objects
        .iter()
        .filter(|o| has_source(&o.kind) && section_of(&o.kind, has_columns(&o.kind)) != Section::Table)
        .filter(|o| {
            let sec = section_of(&o.kind, has_columns(&o.kind));
            (opts.source && wanted(sec, &o.kind, opts)) || (doc.dependencies && code_kinds.contains(&o.kind))
        })
        .collect();
    let total = missing.len() + read_source.len();
    let mut done = 0;
    let mut columns: HashMap<(String, String), (Vec<ColumnDef>, Vec<String>)> = HashMap::new();
    let mut unreadable = Vec::new();
    for o in &missing {
        cancelled()?;
        (cx.progress)(done, total, "columns");
        let obj = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        match s.columns(&obj).await {
            Ok(cols) => {
                let pk = cols.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
                let defs = cols
                    .into_iter()
                    .map(|c| ColumnDef {
                        name: c.name,
                        data_type: c.data_type,
                        nullable: c.nullable,
                        default_value: c.default_value,
                        auto_increment: c.auto_increment,
                        ..Default::default()
                    })
                    .collect();
                columns.insert(key(o.schema.as_deref(), &o.name), (defs, pk));
            }
            Err(_) => unreadable.push(shown(o.schema.as_deref(), &o.name)),
        }
        done += 1;
    }
    let mut sources: HashMap<(String, String, String), String> = HashMap::new();
    for o in &read_source {
        cancelled()?;
        (cx.progress)(done, total, "source");
        let obj = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        match s.definition(&obj).await {
            Ok(Some(text)) => {
                let (sc, n) = key(o.schema.as_deref(), &o.name);
                sources.insert((o.kind.clone(), sc, n), text);
            }
            Ok(None) => {}
            Err(_) => unreadable.push(shown(o.schema.as_deref(), &o.name)),
        }
        done += 1;
    }
    (cx.progress)(total, total, "writing");
    if !unreadable.is_empty() {
        unreadable.sort();
        unreadable.dedup();
        let list = if unreadable.len() > 20 {
            format!("{}, {}", unreadable[..20].join(", "), labels.fill("more", &[("count", (unreadable.len() - 20).to_string())]))
        } else {
            unreadable.join(", ")
        };
        doc.notes.push(format!("{}: {list}", labels.get("noSource")));
    }

    // Tables: the catalog's, then the ones read column by column.
    let mut table_docs: Vec<TableDoc> = Vec::new();
    let mut view_columns: HashMap<(String, String), Vec<ColumnDef>> = HashMap::new();
    for t in &tables {
        if section_of(&t.kind, true) == Section::View {
            view_columns.insert(key(t.schema.as_deref(), &t.name), t.columns.clone());
            continue;
        }
        table_docs.push(TableDoc { kind_label: labels.kind(&t.kind, &driver_label(&t.kind)), table: t.clone(), ..Default::default() });
    }
    for o in &missing {
        let k = key(o.schema.as_deref(), &o.name);
        let (cols, pk) = columns.remove(&k).unwrap_or_default();
        if section_of(&o.kind, true) == Section::View {
            view_columns.insert(k, cols);
            continue;
        }
        table_docs.push(TableDoc {
            kind_label: labels.kind(&o.kind, &driver_label(&o.kind)),
            table: TableSchema {
                kind: o.kind.clone(),
                schema: o.schema.clone(),
                name: o.name.clone(),
                columns: cols,
                primary_key: (!pk.is_empty()).then(|| KeyDef { name: None, columns: pk }),
                ..Default::default()
            },
            ..Default::default()
        });
    }
    // Tables the object list has but nobody could read (no columns).
    let have: HashSet<(String, String)> = table_docs.iter().map(|t| key(t.table.schema.as_deref(), &t.table.name)).collect();
    for o in objects.iter().filter(|o| section_of(&o.kind, has_columns(&o.kind)) == Section::Table && !has_columns(&o.kind)) {
        if opts.tables && !have.contains(&key(o.schema.as_deref(), &o.name)) {
            table_docs.push(TableDoc {
                kind_label: labels.kind(&o.kind, &driver_label(&o.kind)),
                table: TableSchema { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone(), ..Default::default() },
                ..Default::default()
            });
        }
    }

    // "Usada por": foreign keys from the catalog, code that names the table.
    if doc.dependencies {
        let dialect = driver.script_dialect();
        let bodies: Vec<_> = objects
            .iter()
            .filter_map(|o| {
                let (sc, n) = key(o.schema.as_deref(), &o.name);
                sources.get(&(o.kind.clone(), sc, n)).map(|b| (o, b, b.to_lowercase()))
            })
            .collect();
        for t in &mut table_docs {
            let target = DependencyTarget {
                object: ObjectRef { kind: t.table.kind.clone(), schema: t.table.schema.clone(), name: t.table.name.clone() },
                column: None,
            };
            for d in schema_dependents(&tables, &target).into_iter().filter(|d| d.relation == Relation::ForeignKey) {
                t.used_by.push(UsedBy {
                    kind_label: labels.kind(&d.kind, &driver_label(&d.kind)),
                    schema: d.schema,
                    name: d.name,
                    how: d.detail.unwrap_or_default(),
                    table: true,
                });
            }
            let needle = t.table.name.to_lowercase();
            for (o, body, lower) in &bodies {
                if !lower.contains(&needle) {
                    continue;
                }
                if let Some((confidence, _)) = find_mentions(body, &dialect, &target) {
                    t.used_by.push(UsedBy {
                        kind_label: labels.kind(&o.kind, &driver_label(&o.kind)),
                        schema: o.schema.clone(),
                        name: o.name.clone(),
                        how: labels.get(if confidence == Confidence::Review { "usedByReview" } else { "usedByCode" }).to_string(),
                        table: false,
                    });
                }
            }
        }
    }

    // Triggers listed on their table.
    for o in objects.iter().filter(|o| opts.triggers && o.kind == kinds::TRIGGER) {
        let Some(parent) = o.parent.as_deref() else { continue };
        if let Some(t) = table_docs.iter_mut().find(|t| t.table.name.eq_ignore_ascii_case(parent) && t.table.schema.as_deref().unwrap_or("").eq_ignore_ascii_case(o.schema.as_deref().unwrap_or(""))) {
            t.triggers.push(o.name.clone());
        }
    }
    if !opts.tables {
        table_docs.clear();
    }

    // Everything else, grouped by kind in the driver's order.
    let order: HashMap<&str, usize> = info.object_kinds.iter().enumerate().map(|(i, k)| (k.id, i)).collect();
    let mut by_schema: BTreeMap<String, SchemaDoc> = BTreeMap::new();
    let schema_of = |m: &mut BTreeMap<String, SchemaDoc>, sc: Option<&str>| -> String {
        let k = sc.unwrap_or("").to_string();
        m.entry(k.clone()).or_insert_with(|| SchemaDoc { name: sc.filter(|s| !s.is_empty()).map(str::to_string), ..Default::default() });
        k
    };
    for t in table_docs {
        let k = schema_of(&mut by_schema, t.table.schema.as_deref());
        by_schema.get_mut(&k).unwrap().tables.push(t);
    }
    for o in &objects {
        let sec = section_of(&o.kind, has_columns(&o.kind));
        if sec == Section::Table || !wanted(sec, &o.kind, opts) {
            continue;
        }
        let k = schema_of(&mut by_schema, o.schema.as_deref());
        let (sc, n) = key(o.schema.as_deref(), &o.name);
        let item = ObjectDoc {
            schema: o.schema.clone(),
            name: o.name.clone(),
            parent: o.parent.clone(),
            columns: view_columns.get(&(sc.clone(), n.clone())).cloned().unwrap_or_default(),
            source: if opts.source { sources.get(&(o.kind.clone(), sc, n)).cloned() } else { None },
        };
        let groups = &mut by_schema.get_mut(&k).unwrap().groups;
        match groups.iter_mut().find(|g| g.kind == o.kind) {
            Some(g) => g.items.push(item),
            None => groups.push(ObjectGroup { kind: o.kind.clone(), label: labels.kind(&o.kind, &driver_label(&o.kind)), items: vec![item] }),
        }
    }
    for sd in by_schema.values_mut() {
        sd.tables.sort_by(|a, b| {
            let rank = |t: &TableDoc| order.get(t.table.kind.as_str()).copied().unwrap_or(usize::MAX);
            rank(a).cmp(&rank(b)).then_with(|| a.table.name.to_lowercase().cmp(&b.table.name.to_lowercase()))
        });
        sd.groups.sort_by_key(|g| (section_rank(&g.kind, has_columns(&g.kind)), order.get(g.kind.as_str()).copied().unwrap_or(usize::MAX)));
        for g in &mut sd.groups {
            g.items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        }
    }
    doc.schemas = by_schema.into_values().filter(|s| !s.tables.is_empty() || !s.groups.is_empty()).collect();
    Ok(doc)
}

fn section_rank(kind: &str, has_columns: bool) -> u8 {
    match section_of(kind, has_columns) {
        Section::Table => 0,
        Section::View => 1,
        Section::Routine => 2,
        Section::Trigger => 3,
        Section::Other => 4,
    }
}

/// Render `doc` in the format asked for.
pub fn render(doc: &Doc, format: DocFormat, labels: &Labels) -> String {
    match format {
        DocFormat::Html => html::render(doc, labels),
        DocFormat::Markdown => markdown::render(doc, labels),
    }
}

// -- shared by the renderers -------------------------------------------------

/// Stable anchors: `t1`, `t2`… for tables, `o1`… for the other objects,
/// looked up by schema and name (case-insensitive) for links.
pub struct Anchors {
    tables: HashMap<(String, String), String>,
}

impl Anchors {
    pub fn new(doc: &Doc) -> Self {
        let mut tables = HashMap::new();
        let mut n = 0;
        for s in &doc.schemas {
            for t in &s.tables {
                n += 1;
                tables.entry(key(t.table.schema.as_deref(), &t.table.name)).or_insert_with(|| format!("t{n}"));
            }
        }
        Anchors { tables }
    }

    /// A table's anchor; `schema` `None` falls back to `default_schema`.
    pub fn table(&self, schema: Option<&str>, default_schema: Option<&str>, name: &str) -> Option<&str> {
        let sc = schema.or(default_schema);
        self.tables
            .get(&key(sc, name))
            .or_else(|| if schema.is_none() { self.tables.get(&key(None, name)) } else { None })
            .map(String::as_str)
    }
}

/// `schema.name`, or the name alone.
pub fn qualified(schema: Option<&str>, name: &str) -> String {
    shown(schema, name)
}

/// What a column is in its table's keys: `PK`, `FK`, `PK, FK`.
pub fn key_marks(t: &TableSchema, column: &str) -> String {
    let pk = t.primary_key.as_ref().is_some_and(|k| k.columns.iter().any(|c| c.eq_ignore_ascii_case(column)));
    let fk = t.foreign_keys.iter().any(|f| f.columns.iter().any(|c| c.eq_ignore_ascii_case(column)));
    match (pk, fk) {
        (true, true) => "PK, FK".into(),
        (true, false) => "PK".into(),
        (false, true) => "FK".into(),
        _ => String::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small document with what needs escaping in every place.
    pub fn sample() -> Doc {
        let users = TableSchema {
            kind: kinds::TABLE.into(),
            schema: Some("app".into()),
            name: "users<script>".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "int".into(), nullable: false, auto_increment: true, ..Default::default() },
                ColumnDef { name: "name|x".into(), data_type: "varchar(50)".into(), comment: Some("<b>bold</b> & \"q\"\nline".into()), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: Some("pk_users".into()), columns: vec!["id".into()] }),
            comment: Some("Users <img src=x onerror=alert(1)>".into()),
            ..Default::default()
        };
        let orders = TableSchema {
            kind: kinds::TABLE.into(),
            schema: Some("app".into()),
            name: "orders".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "int".into(), nullable: false, ..Default::default() },
                ColumnDef { name: "user_id".into(), data_type: "int".into(), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            foreign_keys: vec![dbine_driver::ForeignKeyDef {
                name: Some("fk_orders_users".into()),
                columns: vec!["user_id".into()],
                ref_schema: Some("app".into()),
                ref_table: "users<script>".into(),
                ref_columns: vec!["id".into()],
                on_delete: Some("CASCADE".into()),
                on_update: None,
            }],
            indexes: vec![dbine_driver::IndexDef { name: "ix_orders_user".into(), columns: vec!["user_id".into()], ..Default::default() }],
            checks: vec![dbine_driver::CheckDef { name: Some("ck".into()), expression: "id > 0 AND id < 10".into() }],
            ..Default::default()
        };
        Doc {
            database: "shop</title>".into(),
            connection: "local".into(),
            engine: "PostgreSQL".into(),
            version: Some("16.2".into()),
            generated_at: "2026-10-08 10:00".into(),
            schemas: vec![SchemaDoc {
                name: Some("app".into()),
                tables: vec![
                    TableDoc { table: orders, kind_label: "Tablas".into(), ..Default::default() },
                    TableDoc {
                        table: users,
                        kind_label: "Tablas".into(),
                        used_by: vec![UsedBy { kind_label: "Tablas".into(), schema: Some("app".into()), name: "orders".into(), how: "fk_orders_users (user_id) → app.users<script> (id)".into(), table: true }],
                        triggers: vec![],
                    },
                ],
                groups: vec![ObjectGroup {
                    kind: kinds::VIEW.into(),
                    label: "Vistas".into(),
                    items: vec![ObjectDoc { schema: Some("app".into()), name: "v_orders".into(), source: Some("SELECT * FROM orders WHERE x < 1 -- ``` </pre>".into()), ..Default::default() }],
                }],
            }],
            notes: vec!["nota <i>".into()],
            indexes: true,
            foreign_keys: true,
            source: true,
            dependencies: true,
            diagram: true,
        }
    }

    #[test]
    fn labels_fall_back_to_spanish() {
        let mut given = BTreeMap::new();
        given.insert("tables".to_string(), "Tables".to_string());
        given.insert("columns".to_string(), " ".to_string());
        let l = Labels::new(&given);
        assert_eq!(l.get("tables"), "Tables");
        assert_eq!(l.get("columns"), "Columnas");
        assert_eq!(l.kind("view", "x"), "Vistas");
        assert_eq!(l.kind("measurement", "Mediciones"), "Mediciones");
        assert!(l.fill("diagramSkipped", &[("count", "200".into()), ("max", "150".into())]).contains("200"));
    }

    #[test]
    fn sections() {
        assert_eq!(section_of("table", true), Section::Table);
        assert_eq!(section_of("measurement", true), Section::Table);
        assert_eq!(section_of("materialized_view", true), Section::View);
        assert_eq!(section_of("sequence", false), Section::Other);
        let o = DocOptions { indexes: false, ..Default::default() };
        assert!(!wanted(Section::Other, "index", &o));
        assert!(wanted(Section::Other, "sequence", &o));
    }

    #[test]
    fn anchors_resolve_case_insensitively() {
        let doc = sample();
        let a = Anchors::new(&doc);
        assert_eq!(a.table(Some("APP"), None, "ORDERS"), Some("t1"));
        assert_eq!(a.table(None, Some("app"), "users<script>"), Some("t2"));
        assert_eq!(a.table(Some("other"), None, "orders"), None);
    }
}
