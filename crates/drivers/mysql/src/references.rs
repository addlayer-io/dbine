//! What an object uses (`Session::references`).
//!
//! - A view: MySQL 8.0.13+ records the tables, views and functions a view
//!   reads in `information_schema.VIEW_TABLE_USAGE` and
//!   `VIEW_ROUTINE_USAGE` (confirmed). Where those don't exist (older
//!   MySQL, MariaDB, TiDB, the analytical engines) the query fails and the
//!   view's code is read, as for every other kind.
//! - A trigger: its table, from `information_schema.TRIGGERS` (confirmed),
//!   plus what its body names.
//! - A procedure or function: its body (the engines don't track what a
//!   routine uses).
//! - A table: the tables its foreign keys point at
//!   (`information_schema.KEY_COLUMN_USAGE`, confirmed), and the remote
//!   source of a FEDERATED or CONNECT table, or of a StarRocks / Doris
//!   external table.
//!
//! The code is read as the contract's default does (`used_names`, then the
//! names resolved against the database's objects: probable), with two
//! differences. MySQL and MariaDB store a view as
//! ``from (`a` join `b` on(…))``, and the first table after `from (` is
//! taken too. And in MySQL a schema is a database, which is what a session
//! opens: a name qualified with another database (`otra.t`), or with
//! another catalog on StarRocks / Doris (`hive.db.t`), is no object of
//! this one's tree, so it goes in `external` with its database (and no
//! server) instead of being matched with a local object of the same name.

use crate::index_usage::{foreign_keys, foreign_keys_sql};
use crate::session::{at, lit, named, MySqlSession};
use crate::Variant;
use dbine_driver::dependencies::{
    find_mentions, foreign_key_parents, references_scan, used_names, used_objects, Confidence,
    DependencyReport, DependencyScan, DependencyTarget, Dependent, ExternalRef, Mention, Relation,
    UsedName, LINE_CHARS, LOOKUP_CAP, MAX_MENTIONS,
};
use dbine_driver::sql::{code_tokens, ScriptDialect, TokenKind};
use dbine_driver::{
    kinds, DbObject, Error, ForeignKeyDef, ObjectQuery, ObjectRef, Result, Session,
};
use mysql_async::prelude::Queryable;
use mysql_async::Row;

/// The object's code couldn't be read (a routine's body is NULL without
/// the privilege; SHOW CREATE VIEW needs SHOW VIEW).
pub(crate) const NOTE_CODE_HIDDEN: &str = "El servidor no le muestra a este usuario el código del objeto (hace falta ser su DEFINER o tener un privilegio como SHOW_ROUTINE, SHOW VIEW o SELECT global): la lista tiene solo lo que registra el catálogo.";

/// A catalog view refused for lack of a privilege.
pub(crate) fn note_catalog_denied(view: &str) -> String {
    format!(
        "Sin permiso para leer information_schema.{view}: lo que usa se buscó por otra vía y puede estar incompleto o sin confirmar."
    )
}

/// Kinds whose code is searched and whose unreadable code gets a note.
const CODE: &[&str] = &[
    kinds::VIEW,
    kinds::MATERIALIZED_VIEW,
    kinds::PROCEDURE,
    kinds::FUNCTION,
    kinds::TRIGGER,
];

pub(crate) async fn references(
    s: &mut MySqlSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let t = &target.object;
    let db = match t.schema() {
        Some(d) => Some(d.to_string()),
        None if !s.variant.single_namespace() => s.current_database().await,
        None => None,
    };
    let Some(db) = db else {
        return references_scan(s, target, scan).await;
    };
    let mut report = DependencyReport::default();
    // What the catalog records, before the code adds its mentions.
    let mut confirmed: Vec<Dependent> = Vec::new();
    match t.kind.as_str() {
        kinds::TABLE => table(s, target, scan, &db, &mut report).await?,
        kinds::VIEW => {
            let sql = view_uses_sql(&db, &t.name);
            match catalog_rows(s, &sql).await {
                Ok(rows) => {
                    let (items, external) = view_uses(&rows, &db);
                    confirmed.extend(items);
                    for x in external {
                        add_external(&mut report.external, x);
                    }
                }
                Err(denied) => {
                    if denied {
                        report.note = Some(note_catalog_denied("VIEW_TABLE_USAGE"));
                    }
                }
            }
        }
        kinds::TRIGGER if s.variant.has_triggers() => {
            match catalog_rows(s, &trigger_sql(&db, &t.name)).await {
                Ok(rows) => {
                    let (items, external) = trigger_table(&rows, &db);
                    confirmed.extend(items);
                    for x in external {
                        add_external(&mut report.external, x);
                    }
                }
                Err(denied) => {
                    if denied {
                        report.note = Some(note_catalog_denied("TRIGGERS"));
                    }
                }
            }
        }
        _ => {}
    }
    let body = if scan.written_kinds.contains(&t.kind) {
        match s.definition(t).await {
            Ok(Some(b)) => Some(b),
            Ok(None) => {
                hidden(t, &mut report);
                None
            }
            Err(e) => {
                report.unreadable.push(t.name.clone());
                if denied_text(&e) && CODE.contains(&t.kind.as_str()) {
                    report.note.get_or_insert_with(|| NOTE_CODE_HIDDEN.into());
                }
                None
            }
        }
    } else {
        None
    };
    match &body {
        Some(body) => {
            report.scanned = 1;
            let uses = split_uses(
                code_names(body, &scan.dialect),
                s.variant,
                &db,
                t.kind == kinds::TRIGGER,
            );
            let objects = resolve(s, &uses.local).await?;
            let found = used_objects(body, &uses.local, &objects, t);
            report
                .items
                .extend(merge(confirmed, found, Some(body), &scan.dialect));
            let types = if uses.other.iter().any(|o| o.catalog.is_some()) {
                catalog_types(s).await
            } else {
                Vec::new()
            };
            for x in uses.external(body, &types) {
                add_external(&mut report.external, x);
            }
            for x in report.external.iter_mut().filter(|x| x.mentions.is_empty()) {
                x.mentions = external_mentions(body, &scan.dialect, x);
            }
        }
        None => report.items.extend(confirmed),
    }
    sort(&mut report.items);
    Ok(report)
}

/// The target's code came back empty (a routine's body without the privilege).
fn hidden(t: &ObjectRef, report: &mut DependencyReport) {
    if CODE.contains(&t.kind.as_str()) {
        report.unreadable.push(t.name.clone());
        report.note.get_or_insert_with(|| NOTE_CODE_HIDDEN.into());
    }
}

/// A table: its foreign keys' parents, and the remote table it reads.
async fn table(
    s: &mut MySqlSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
    db: &str,
    report: &mut DependencyReport,
) -> Result<()> {
    let t = &target.object;
    let here = ObjectRef {
        kind: t.kind.clone(),
        schema: Some(db.to_string()),
        name: t.name.clone(),
    };
    // TiDB's KEY_COLUMN_USAGE gives a foreign key to another database the
    // table's own schema (7.5): its SHOW CREATE TABLE has the right one.
    let create = if matches!(
        s.variant,
        Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::StarRocks | Variant::Doris
    ) {
        s.show_create("TABLE", &here, "Create Table")
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    if s.variant.has_foreign_keys() {
        match catalog_rows(s, &foreign_keys_sql(&here)).await {
            Ok(rows) => {
                let mut fks = fk_defs(&rows);
                if let (Variant::TiDb, Some(create)) = (s.variant, &create) {
                    fix_ref_schemas(&mut fks, create, &scan.dialect, db);
                }
                let (items, external) = foreign_key_uses(fks, t);
                report.items.extend(items);
                for x in external {
                    add_external(&mut report.external, x);
                }
            }
            Err(denied) => {
                // The default reads them from SHOW INDEX / the schema.
                report.items = references_scan(s, target, scan).await?.items;
                if denied {
                    report.note = Some(note_catalog_denied("KEY_COLUMN_USAGE"));
                }
            }
        }
    } else if scan.foreign_keys {
        report.items = references_scan(s, target, scan).await?.items;
    }
    let Some(create) = create else {
        return Ok(());
    };
    match remote_source(s.variant, &create, &scan.dialect, &t.name) {
        Some(Remote::External { mut external, link }) => {
            if let Some(link) = link {
                server_link(s, &link, &mut external).await;
            }
            add_external(&mut report.external, external);
        }
        Some(Remote::Local { database, table }) => match database {
            Some(d) if !same(&d, db) => add_external(
                &mut report.external,
                ExternalRef {
                    system: Some("CONNECT".into()),
                    database: Some(d),
                    name: Some(table),
                    ..Default::default()
                },
            ),
            _ => report.items.push(Dependent {
                kind: kinds::TABLE.into(),
                schema: None,
                name: table,
                parent: None,
                relation: Relation::Code,
                confidence: Confidence::Confirmed,
                detail: Some("CONNECT".into()),
                mentions: Vec::new(),
            }),
        },
        None => {}
    }
    Ok(())
}

/// A catalog query's rows; `Err(true)` when a privilege is missing,
/// `Err(false)` when the server lacks it (an older version, another engine).
pub(crate) async fn catalog_rows(
    s: &mut MySqlSession,
    sql: &str,
) -> std::result::Result<Vec<Row>, bool> {
    match s.conn.query::<Row, _>(sql).await {
        Ok(rows) => Ok(rows),
        Err(e) => {
            tracing::debug!("{:?}: {sql}: {e}", s.variant);
            Err(denied(&e))
        }
    }
}

/// Access denied: to a table (1142), a column (1143), a database (1044),
/// for lack of a privilege (1227), or to a routine (1370).
fn denied(e: &mysql_async::Error) -> bool {
    matches!(e, mysql_async::Error::Server(s) if matches!(s.code, 1044 | 1142 | 1143 | 1227 | 1370))
}

fn denied_text(e: &Error) -> bool {
    e.to_string().to_lowercase().contains("denied")
}

// -- the catalog --------------------------------------------------------------------------

/// The tables, views (VIEW_TABLE_USAGE) and functions (VIEW_ROUTINE_USAGE)
/// a view uses: schema, name and its type (`TABLES.TABLE_TYPE`, or
/// `FUNCTION`). MySQL 8.0.13 and later. VIEW_ROUTINE_USAGE names the view
/// in TABLE_SCHEMA / TABLE_NAME (the SQL standard's columns).
pub(crate) fn view_uses_sql(db: &str, view: &str) -> String {
    let (db, view) = (lit(db), lit(view));
    format!(
        "SELECT u.TABLE_SCHEMA, u.TABLE_NAME, t.TABLE_TYPE
  FROM information_schema.VIEW_TABLE_USAGE u
  LEFT JOIN information_schema.TABLES t
    ON t.TABLE_SCHEMA = u.TABLE_SCHEMA AND t.TABLE_NAME = u.TABLE_NAME
 WHERE u.VIEW_SCHEMA = {db} AND u.VIEW_NAME = {view}
UNION ALL
SELECT r.SPECIFIC_SCHEMA, r.SPECIFIC_NAME, 'FUNCTION'
  FROM information_schema.VIEW_ROUTINE_USAGE r
 WHERE r.TABLE_SCHEMA = {db} AND r.TABLE_NAME = {view}"
    )
}

/// The table a trigger is on, with when and what fires it.
pub(crate) fn trigger_sql(db: &str, trigger: &str) -> String {
    format!(
        "SELECT EVENT_OBJECT_SCHEMA, EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION
  FROM information_schema.TRIGGERS
 WHERE TRIGGER_SCHEMA = {} AND TRIGGER_NAME = {}",
        lit(db),
        lit(trigger)
    )
}

/// A catalog row's cells, for the parsers (and their tests).
trait Cells {
    fn cell(&self, i: usize) -> Option<String>;
}

impl Cells for Row {
    fn cell(&self, i: usize) -> Option<String> {
        at(self, i)
    }
}

/// `view_uses_sql`'s rows: this database's objects (confirmed) and
/// another database's (external).
fn view_uses<R: Cells>(rows: &[R], db: &str) -> (Vec<Dependent>, Vec<ExternalRef>) {
    let mut items: Vec<Dependent> = Vec::new();
    let mut external = Vec::new();
    for r in rows {
        let (Some(schema), Some(name)) = (r.cell(0), r.cell(1)) else {
            continue;
        };
        if !same(&schema, db) {
            external.push(ExternalRef {
                database: Some(schema),
                name: Some(name),
                ..Default::default()
            });
            continue;
        }
        let kind = used_kind(r.cell(2).as_deref());
        if items.iter().any(|d| d.kind == kind && same(&d.name, &name)) {
            continue;
        }
        items.push(catalog_item(kind, name, None));
    }
    (items, external)
}

/// The kind of what a view uses, from `TABLES.TABLE_TYPE` (NULL: a table
/// the login can't see, or one dropped since).
pub(crate) fn used_kind(table_type: Option<&str>) -> &'static str {
    match table_type.map(str::to_ascii_uppercase).as_deref() {
        Some("FUNCTION") => kinds::FUNCTION,
        Some("PROCEDURE") => kinds::PROCEDURE,
        Some(t) if t.contains("VIEW") => kinds::VIEW,
        _ => kinds::TABLE,
    }
}

/// `trigger_sql`'s row: the trigger's table (`BEFORE INSERT`…).
fn trigger_table<R: Cells>(rows: &[R], db: &str) -> (Vec<Dependent>, Vec<ExternalRef>) {
    let mut items = Vec::new();
    let mut external = Vec::new();
    for r in rows {
        let (Some(schema), Some(name)) = (r.cell(0), r.cell(1)) else {
            continue;
        };
        let when = [r.cell(2), r.cell(3)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let detail = (!when.is_empty()).then_some(when);
        if same(&schema, db) {
            items.push(catalog_item(kinds::TABLE, name, detail));
        } else {
            external.push(ExternalRef {
                database: Some(schema),
                name: Some(name),
                detail,
                ..Default::default()
            });
        }
    }
    (items, external)
}

/// `foreign_keys_sql`'s rows, one key per constraint.
fn fk_defs<R: Cells>(rows: &[R]) -> Vec<ForeignKeyDef> {
    let rows: Vec<_> = rows
        .iter()
        .map(|r| {
            (
                r.cell(0).unwrap_or_default(),
                r.cell(1).unwrap_or_default(),
                r.cell(2),
                r.cell(3).unwrap_or_default(),
                r.cell(4).unwrap_or_default(),
            )
        })
        .collect();
    foreign_keys(&rows)
}

/// The database each key's `REFERENCES` names in `create` (`None`: this
/// one), for an engine whose catalog gets it wrong.
fn fix_ref_schemas(fks: &mut [ForeignKeyDef], create: &str, dialect: &ScriptDialect, db: &str) {
    for (constraint, schema, table) in create_references(create, dialect) {
        if let Some(fk) = fks.iter_mut().find(|fk| {
            fk.name.as_deref().is_some_and(|n| same(n, &constraint)) && same(&fk.ref_table, &table)
        }) {
            fk.ref_schema = schema.filter(|s| !same(s, db));
        }
    }
}

/// `CONSTRAINT name FOREIGN KEY (…) REFERENCES [db.]table` in a `CREATE
/// TABLE`: the constraint, the database written (if any) and the table.
fn create_references(
    create: &str,
    dialect: &ScriptDialect,
) -> Vec<(String, Option<String>, String)> {
    let toks = code_tokens(create, dialect);
    let word = |k: usize, w: &str| {
        toks.get(k).is_some_and(|t| {
            t.kind == TokenKind::Name
                && t.text.eq_ignore_ascii_case(w)
                && !matches!(create.as_bytes().get(t.start), Some(b'`' | b'"'))
        })
    };
    let name = |k: usize| {
        toks.get(k)
            .filter(|t| t.kind == TokenKind::Name)
            .map(|t| t.text.to_string())
    };
    let mut out = Vec::new();
    let mut constraint = None;
    for i in 0..toks.len() {
        if word(i, "CONSTRAINT") {
            constraint = name(i + 1);
        } else if word(i, "REFERENCES") {
            let Some(first) = name(i + 1) else { continue };
            let dotted = toks
                .get(i + 2)
                .is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".");
            let (schema, table) = match (dotted, name(i + 3)) {
                (true, Some(t)) => (Some(first), t),
                _ => (None, first),
            };
            if let Some(c) = constraint.take() {
                out.push((c, schema, table));
            }
        }
    }
    out
}

/// The keys' parents in this database (confirmed), and those in another
/// one (external, with the key).
fn foreign_key_uses(fks: Vec<ForeignKeyDef>, t: &ObjectRef) -> (Vec<Dependent>, Vec<ExternalRef>) {
    let (here, other): (Vec<_>, Vec<_>) = fks.into_iter().partition(|fk| fk.ref_schema.is_none());
    let local = ObjectRef {
        schema: None,
        ..t.clone()
    };
    let external = other
        .into_iter()
        .map(|fk| {
            let database = fk.ref_schema.clone().unwrap_or_default();
            let name = fk
                .name
                .as_deref()
                .map(|n| format!("{n} "))
                .unwrap_or_default();
            ExternalRef {
                detail: Some(format!(
                    "{name}({}) → {database}.{} ({})",
                    fk.columns.join(", "),
                    fk.ref_table,
                    fk.ref_columns.join(", ")
                )),
                database: Some(database),
                name: Some(fk.ref_table),
                ..Default::default()
            }
        })
        .collect();
    (foreign_key_parents(&here, &local), external)
}

fn catalog_item(kind: &str, name: String, detail: Option<String>) -> Dependent {
    Dependent {
        kind: kind.into(),
        schema: None,
        name,
        parent: None,
        relation: Relation::Code,
        confidence: Confidence::Confirmed,
        detail,
        mentions: Vec::new(),
    }
}

/// StarRocks' and Doris' catalogs: name and type (`Hive`, `Iceberg`, `Jdbc`…).
async fn catalog_types(s: &mut MySqlSession) -> Vec<(String, String)> {
    if !matches!(s.variant, Variant::StarRocks | Variant::Doris) {
        return Vec::new();
    }
    s.optional_rows("SHOW CATALOGS")
        .await
        .iter()
        .filter_map(|r| {
            Some((
                named(r, &["Catalog", "CatalogName", "name"])?,
                named(r, &["Type"])?,
            ))
        })
        .collect()
}

/// A FEDERATED table through `CREATE SERVER`: its host, port and database
/// from `mysql.servers`, when the login may read it.
async fn server_link(s: &mut MySqlSession, link: &str, x: &mut ExternalRef) {
    let rows = s
        .optional_rows(&format!(
            "SELECT Host, Port, Db FROM mysql.servers WHERE Server_name = {}",
            lit(link)
        ))
        .await;
    let Some(r) = rows.first() else { return };
    x.server = host_port(at(r, 0), at(r, 1).filter(|p| p != "0"));
    if x.database.is_none() {
        x.database = at(r, 2).filter(|d| !d.is_empty());
    }
}

// -- the code -----------------------------------------------------------------------------

/// The names `body` uses: the contract's `used_names` plus the first table
/// of a nested join (``from (`a` join `b` …)``, as MySQL and MariaDB
/// store views), which it doesn't take.
fn code_names(body: &str, dialect: &ScriptDialect) -> Vec<UsedName> {
    let mut used = used_names(body, dialect);
    for n in nested_join_names(body, dialect) {
        if !used.iter().any(|u| u.at == n.at) {
            used.push(n);
        }
    }
    used.sort_by_key(|u| u.at);
    used
}

/// Names right after `FROM (` or `JOIN (` (one or more parentheses) that
/// aren't a subquery or a call.
fn nested_join_names(body: &str, dialect: &ScriptDialect) -> Vec<UsedName> {
    let toks = code_tokens(body, dialect);
    let punct = |k: usize, p: &str| {
        toks.get(k)
            .is_some_and(|t| t.kind == TokenKind::Punct && t.text == p)
    };
    let quoted = |start: usize| matches!(body.as_bytes().get(start), Some(b'`' | b'"'));
    let word = |k: usize, words: &[&str]| {
        toks.get(k).is_some_and(|t| {
            t.kind == TokenKind::Name
                && !quoted(t.start)
                && words.iter().any(|w| t.text.eq_ignore_ascii_case(w))
        })
    };
    let mut out = Vec::new();
    for i in 0..toks.len() {
        if toks[i].kind != TokenKind::Name || i == 0 || !punct(i - 1, "(") {
            continue;
        }
        let mut k = i - 1;
        while k > 0 && punct(k - 1, "(") {
            k -= 1;
        }
        if k == 0 || !word(k - 1, &["from", "join"]) {
            continue;
        }
        if word(i, &["select", "with", "values", "table", "lateral"]) {
            continue;
        }
        let mut parts = vec![toks[i].text];
        let mut j = i;
        while punct(j + 1, ".") && toks.get(j + 2).is_some_and(|n| n.kind == TokenKind::Name) {
            parts.push(toks[j + 2].text);
            j += 2;
        }
        if punct(j + 1, "(") {
            continue;
        }
        let name = parts.pop().unwrap_or_default().to_string();
        out.push(UsedName {
            quals: parts.iter().rev().map(|p| p.to_string()).collect(),
            name,
            at: toks[j].start,
            call: false,
        });
    }
    out
}

/// A name of another database (or of another catalog's database).
#[derive(Debug, Clone, PartialEq)]
struct OtherName {
    catalog: Option<String>,
    database: String,
    used: UsedName,
}

/// The names a body uses: this database's, to resolve against its
/// objects, and the rest.
#[derive(Debug, Default, PartialEq)]
struct Uses {
    local: Vec<UsedName>,
    other: Vec<OtherName>,
}

/// The catalog a name means when it's written without one.
fn own_catalog(v: Variant) -> &'static str {
    match v {
        Variant::StarRocks => "default_catalog",
        Variant::Doris => "internal",
        Variant::Databend => "default",
        Variant::GreptimeDb => "greptime",
        _ => "def",
    }
}

/// `used` split by where each name lives. In a trigger, `NEW.x` and
/// `OLD.x` are the row's columns.
fn split_uses(used: Vec<UsedName>, v: Variant, db: &str, trigger: bool) -> Uses {
    let mut out = Uses::default();
    for mut u in used {
        let (catalog, database) = match u.quals.as_slice() {
            [] => (None, None),
            [d] => (None, Some(d.clone())),
            [d, c, ..] => (Some(c.clone()), Some(d.clone())),
        };
        if trigger
            && catalog.is_none()
            && database
                .as_deref()
                .is_some_and(|d| same(d, "new") || same(d, "old"))
        {
            continue;
        }
        let catalog = catalog.filter(|c| !same(c, own_catalog(v)));
        match database {
            Some(d) if catalog.is_some() || !same(&d, db) => out.other.push(OtherName {
                catalog,
                database: d,
                used: u,
            }),
            _ => {
                u.quals.clear();
                out.local.push(u);
            }
        }
    }
    out
}

impl Uses {
    /// The other databases' objects, each once with the lines that name it;
    /// another catalog's with its type (`types`: `catalog_types`).
    fn external(&self, body: &str, types: &[(String, String)]) -> Vec<ExternalRef> {
        let mut out: Vec<(ExternalRef, Vec<usize>)> = Vec::new();
        for o in &self.other {
            let hit = out.iter_mut().find(|(x, _)| {
                x.detail.as_deref().map(str::to_lowercase)
                    == o.catalog.as_deref().map(str::to_lowercase)
                    && x.database.as_deref().is_some_and(|d| same(d, &o.database))
                    && x.name.as_deref().is_some_and(|n| same(n, &o.used.name))
            });
            match hit {
                Some((_, at)) => at.push(o.used.at),
                None => out.push((
                    ExternalRef {
                        system: o.catalog.as_deref().and_then(|c| {
                            types
                                .iter()
                                .find(|(n, _)| same(n, c))
                                .map(|(_, t)| t.clone())
                        }),
                        database: Some(o.database.clone()),
                        name: Some(o.used.name.clone()),
                        detail: o.catalog.clone(),
                        ..Default::default()
                    },
                    vec![o.used.at],
                )),
            }
        }
        out.into_iter()
            .map(|(mut x, mut at)| {
                at.sort();
                x.mentions = mentions(body, &at);
                x
            })
            .collect()
    }
}

/// This database's objects named like `used`: one lookup each, or the whole
/// list once when there are many (as the contract's default).
async fn resolve(s: &mut MySqlSession, used: &[UsedName]) -> Result<Vec<DbObject>> {
    let mut names: Vec<&str> = Vec::new();
    for u in used {
        if !names.iter().any(|n| same(n, &u.name)) {
            names.push(&u.name);
        }
    }
    if names.is_empty() {
        return Ok(Vec::new());
    }
    if names.len() > LOOKUP_CAP {
        return s.list_objects().await;
    }
    let mut found = Vec::new();
    for name in names {
        let q = ObjectQuery {
            name: Some(name.to_string()),
            limit: 50,
            ..Default::default()
        };
        match s.list_objects_page(&q).await {
            Ok(page) => found.extend(page.objects),
            Err(Error::Unsupported(_)) => return s.list_objects().await,
            Err(e) => return Err(e),
        }
    }
    Ok(found)
}

/// The catalog's rows with the lines of the code that name them, then what
/// only the code names (probable).
fn merge(
    confirmed: Vec<Dependent>,
    mut found: Vec<Dependent>,
    body: Option<&str>,
    dialect: &ScriptDialect,
) -> Vec<Dependent> {
    let relation = |k: &str| matches!(k, kinds::TABLE | kinds::VIEW);
    let mut out = Vec::new();
    for mut c in confirmed {
        let hit = found.iter().position(|f| {
            same(&f.name, &c.name) && (f.kind == c.kind || (relation(&f.kind) && relation(&c.kind)))
        });
        match (hit, body) {
            (Some(i), _) => {
                let f = found.remove(i);
                if c.mentions.is_empty() {
                    c.mentions = f.mentions;
                }
            }
            (None, Some(body)) if c.mentions.is_empty() => {
                let object = ObjectRef {
                    kind: c.kind.clone(),
                    schema: None,
                    name: c.name.clone(),
                };
                c.mentions = named_lines(body, dialect, object);
            }
            _ => {}
        }
        out.push(c);
    }
    out.extend(found);
    out
}

/// The lines of `body` that name `object`.
fn named_lines(body: &str, dialect: &ScriptDialect, object: ObjectRef) -> Vec<Mention> {
    let target = DependencyTarget {
        object,
        column: None,
    };
    find_mentions(body, dialect, &target)
        .map(|(_, m)| m)
        .unwrap_or_default()
}

/// The lines naming an external object the catalog gave (another database's).
fn external_mentions(body: &str, dialect: &ScriptDialect, x: &ExternalRef) -> Vec<Mention> {
    match (&x.system, &x.server, &x.database, &x.name) {
        (None, None, Some(d), Some(n)) => named_lines(
            body,
            dialect,
            ObjectRef {
                kind: kinds::TABLE.into(),
                schema: Some(d.clone()),
                name: n.clone(),
            },
        ),
        _ => Vec::new(),
    }
}

/// One mention per line of `body` at the byte offsets `at` (sorted), at
/// most `MAX_MENTIONS`, as the contract writes them.
fn mentions(body: &str, at: &[usize]) -> Vec<Mention> {
    let mut out: Vec<Mention> = Vec::new();
    let mut line = 1u32;
    let mut from = 0usize;
    for &start in at {
        line += body[from..start].bytes().filter(|&b| b == b'\n').count() as u32;
        from = start;
        if out.last().is_some_and(|m| m.line == line) {
            continue;
        }
        if out.len() == MAX_MENTIONS {
            break;
        }
        let a = body[..start].rfind('\n').map_or(0, |p| p + 1);
        let b = body[start..].find('\n').map_or(body.len(), |p| start + p);
        out.push(Mention {
            line,
            text: body[a..b].trim().chars().take(LINE_CHARS).collect(),
            dynamic: false,
        });
    }
    out
}

/// One external entry per source; the code's mentions join the catalog's row.
fn add_external(list: &mut Vec<ExternalRef>, x: ExternalRef) {
    let opt = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(a), Some(b)) => same(a, b),
        (None, None) => true,
        _ => false,
    };
    let hit = list.iter_mut().find(|y| {
        opt(&y.system, &x.system)
            && opt(&y.server, &x.server)
            && opt(&y.database, &x.database)
            && opt(&y.name, &x.name)
            && (y.detail.is_none() || x.detail.is_none() || y.detail == x.detail)
    });
    match hit {
        Some(y) => {
            if y.mentions.is_empty() {
                y.mentions = x.mentions;
            }
            if y.detail.is_none() {
                y.detail = x.detail;
            }
        }
        None => list.push(x),
    }
}

/// Confirmed first, then by relation and name (as the contract sorts).
fn sort(items: &mut [Dependent]) {
    items.sort_by(|a, b| {
        (
            a.confidence,
            a.relation,
            a.schema.as_deref().unwrap_or(""),
            a.name.to_lowercase(),
        )
            .cmp(&(
                b.confidence,
                b.relation,
                b.schema.as_deref().unwrap_or(""),
                b.name.to_lowercase(),
            ))
    });
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

// -- remote tables ------------------------------------------------------------------------

/// Where a table's rows come from, when they aren't its own.
#[derive(Debug, PartialEq)]
enum Remote {
    /// Another server, a file or a service. `link`: a `CREATE SERVER`
    /// name whose host `mysql.servers` has.
    External {
        external: ExternalRef,
        link: Option<String>,
    },
    /// A CONNECT table over a table of this server (PROXY, XCOL, PIVOT…).
    Local {
        database: Option<String>,
        table: String,
    },
}

/// The remote source in a `SHOW CREATE TABLE`: FEDERATED's `CONNECTION`,
/// CONNECT's options, a StarRocks / Doris external table's `PROPERTIES`.
/// User names and passwords in them are never copied.
fn remote_source(v: Variant, create: &str, dialect: &ScriptDialect, table: &str) -> Option<Remote> {
    let opts = table_options(create, dialect);
    let get = |k: &str| {
        opts.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let engine = get("ENGINE")?.to_ascii_uppercase();
    match engine.as_str() {
        "FEDERATED" | "FEDERATEDX" => {
            let conn = get("CONNECTION").or_else(|| get("COMMENT"))?;
            Some(federated(&conn, table))
        }
        "CONNECT" => connect(&get, table),
        "OLAP" => None,
        _ if matches!(v, Variant::StarRocks | Variant::Doris) => {
            let props = properties(create, dialect);
            let get = |k: &str| {
                props
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(k))
                    .map(|(_, v)| v.clone())
                    .filter(|v| !v.is_empty())
            };
            let server = host_port(get("host"), get("port"))
                .or_else(|| get("hosts"))
                .or_else(|| get("hive.metastore.uris"))
                .map(|s| without_credentials(&s));
            Some(Remote::External {
                external: ExternalRef {
                    system: Some(engine),
                    server,
                    database: get("database"),
                    name: get("table").or_else(|| get("index")),
                    detail: get("resource"),
                    ..Default::default()
                },
                link: None,
            })
        }
        _ => None,
    }
}

/// FEDERATED's `CONNECTION`: `scheme://user:pass@host:port/db/table`, or a
/// `CREATE SERVER` name (`server` or `server/table`).
fn federated(conn: &str, table: &str) -> Remote {
    if conn.contains("://") {
        let (server, database, name) = url_parts(conn, false);
        return Remote::External {
            external: ExternalRef {
                system: Some("FEDERATED".into()),
                server,
                database,
                name: name.or_else(|| Some(table.to_string())),
                ..Default::default()
            },
            link: None,
        };
    }
    let (link, name) = conn.split_once('/').unwrap_or((conn, table));
    Remote::External {
        external: ExternalRef {
            system: Some("FEDERATED".into()),
            name: Some(if name.is_empty() { table } else { name }.to_string()),
            detail: Some(link.to_string()),
            ..Default::default()
        },
        link: Some(link.to_string()),
    }
}

/// A CONNECT table's source by its `TABLE_TYPE`: a server (MYSQL, ODBC,
/// JDBC, MONGO), a file (`FILE_NAME`) or a table of this server.
fn connect(get: &dyn Fn(&str) -> Option<String>, table: &str) -> Option<Remote> {
    let kind = get("TABLE_TYPE")
        .unwrap_or_else(|| "DOS".into())
        .to_ascii_uppercase();
    let system = Some(format!("CONNECT {kind}"));
    let tabname = get("TABNAME");
    let dbname = get("DBNAME");
    let external = |server, database, name, detail| Remote::External {
        external: ExternalRef {
            system: system.clone(),
            server,
            database,
            name,
            detail,
            ..Default::default()
        },
        link: None,
    };
    match kind.as_str() {
        "MYSQL" => match get("CONNECTION") {
            Some(c) if c.contains("://") => {
                let (server, database, name) = url_parts(&c, false);
                Some(external(
                    server,
                    database.or(dbname),
                    name.or(tabname).or_else(|| Some(table.to_string())),
                    None,
                ))
            }
            _ => {
                let opts = option_list(get("OPTION_LIST").as_deref().unwrap_or(""));
                let opt = |k: &str| {
                    opts.iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(k))
                        .map(|(_, v)| v.clone())
                };
                let server = host_port(opt("host"), opt("port"));
                if server.is_none() {
                    // No host: this server's own table, through MYSQL.
                    return Some(Remote::Local {
                        database: dbname,
                        table: tabname.unwrap_or_else(|| table.to_string()),
                    });
                }
                Some(external(
                    server,
                    dbname,
                    tabname.or_else(|| Some(table.to_string())),
                    None,
                ))
            }
        },
        "ODBC" => {
            let pairs = odbc_pairs(&get("CONNECTION").unwrap_or_default());
            let pick = |keys: &[&str]| {
                pairs
                    .iter()
                    .find(|(k, _)| keys.iter().any(|w| k.eq_ignore_ascii_case(w)))
                    .map(|(_, v)| v.clone())
            };
            let server = host_port(pick(&["SERVER", "HOST", "HOSTNAME"]), pick(&["PORT"]));
            Some(external(
                server,
                pick(&["DATABASE", "DB"]).or(dbname),
                tabname.or_else(|| Some(table.to_string())),
                pick(&["DSN"]),
            ))
        }
        "JDBC" | "MONGO" => {
            let (server, database, _) = get("CONNECTION")
                .map(|c| url_parts(&c, true))
                .unwrap_or_default();
            Some(external(
                server,
                database.or(dbname),
                tabname.or_else(|| Some(table.to_string())),
                None,
            ))
        }
        _ => match (get("FILE_NAME"), tabname) {
            (Some(file), _) => Some(external(None, None, Some(file), None)),
            (None, Some(t)) => Some(Remote::Local {
                database: dbname,
                table: t,
            }),
            (None, None) => None,
        },
    }
}

/// `scheme://user:pass@host:port/db/table?query` into its server, database
/// and table, without the user, the password or the query. `query_first`:
/// a JDBC URL, whose query may hold an `@`; FEDERATED's password may hold
/// a `?` instead.
fn url_parts(url: &str, query_first: bool) -> (Option<String>, Option<String>, Option<String>) {
    let rest = url.strip_prefix("jdbc:").unwrap_or(url);
    let rest = rest.split_once("://").map_or(rest, |(_, r)| r);
    let cut_query = |s: &str| s.split_once('?').map_or(s, |(a, _)| a).to_string();
    let rest = if query_first {
        cut_query(rest)
    } else {
        rest.to_string()
    };
    let hostpath = rest.rfind('@').map_or(rest.as_str(), |i| &rest[i + 1..]);
    let hostpath = cut_query(hostpath);
    let mut parts = hostpath.splitn(3, '/');
    let nonempty = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let server = nonempty(parts.next());
    let database = nonempty(parts.next());
    let table = nonempty(parts.next());
    (server, database, table)
}

/// `host:port` (`port` left out when there's none).
fn host_port(host: Option<String>, port: Option<String>) -> Option<String> {
    let host = host.filter(|h| !h.is_empty())?;
    Some(match port.filter(|p| !p.is_empty()) {
        Some(p) => format!("{host}:{p}"),
        None => host,
    })
}

/// A server address without `user:pass@` in front.
fn without_credentials(s: &str) -> String {
    match s.split_once("://") {
        Some((scheme, rest)) => {
            let rest = rest.rfind('@').map_or(rest, |i| &rest[i + 1..]);
            format!("{scheme}://{rest}")
        }
        None => s.rfind('@').map_or(s, |i| &s[i + 1..]).to_string(),
    }
}

/// CONNECT's `OPTION_LIST`: `name=value` pairs separated by commas.
fn option_list(s: &str) -> Vec<(String, String)> {
    s.split(',')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// An ODBC connection string's `KEY=value;` pairs (`Driver={…}` included).
fn odbc_pairs(s: &str) -> Vec<(String, String)> {
    s.split(';')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let v = v
                .strip_prefix('{')
                .and_then(|v| v.strip_suffix('}'))
                .unwrap_or(v);
            (k.trim().to_string(), v.to_string())
        })
        .collect()
}

/// A `CREATE TABLE`'s table options (`ENGINE=…`, `CONNECTION='…'`,
/// CONNECT's `` `TABLE_TYPE`=… ``): `name = value` outside the column list.
fn table_options(create: &str, dialect: &ScriptDialect) -> Vec<(String, String)> {
    let toks = code_tokens(create, dialect);
    let mut depth = 0i32;
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match (t.kind, t.text) {
            (TokenKind::Punct, "(") => depth += 1,
            (TokenKind::Punct, ")") => depth -= 1,
            (TokenKind::Name, key) if depth == 0 => {
                let eq = toks
                    .get(i + 1)
                    .is_some_and(|n| n.kind == TokenKind::Punct && n.text == "=");
                if let (true, Some(v)) = (eq, toks.get(i + 2)) {
                    if matches!(v.kind, TokenKind::Name | TokenKind::String) {
                        out.push((key.to_string(), value(v.kind, v.text)));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// StarRocks' and Doris' `PROPERTIES ("key" = "value", …)`.
fn properties(create: &str, dialect: &ScriptDialect) -> Vec<(String, String)> {
    let toks = code_tokens(create, dialect);
    let Some(start) = toks
        .iter()
        .position(|t| t.kind == TokenKind::Name && t.text.eq_ignore_ascii_case("PROPERTIES"))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut i = start + 2;
    while i + 2 < toks.len() {
        let (k, e, v) = (&toks[i], &toks[i + 1], &toks[i + 2]);
        if k.kind == TokenKind::Punct && k.text == ")" {
            break;
        }
        if e.kind == TokenKind::Punct && e.text == "=" && k.kind != TokenKind::Punct {
            out.push((value(k.kind, k.text), value(v.kind, v.text)));
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// A token's value: a string without its quotes and escapes.
fn value(kind: TokenKind, text: &str) -> String {
    if kind != TokenKind::String || text.len() < 2 {
        return text.to_string();
    }
    let q = text.chars().next().unwrap_or('\'');
    let inner = &text[1..text.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else if c == q && chars.peek() == Some(&q) {
            chars.next();
            out.push(q);
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Cells for Vec<Option<&str>> {
        fn cell(&self, i: usize) -> Option<String> {
            self.get(i).copied().flatten().map(str::to_string)
        }
    }

    fn d() -> ScriptDialect {
        crate::script_dialect(Variant::MySql)
    }

    fn row(cells: &[Option<&'static str>]) -> Vec<Option<&'static str>> {
        cells.to_vec()
    }

    fn names(used: &[UsedName]) -> Vec<String> {
        used.iter()
            .map(|u| {
                let mut q: Vec<&str> = u.quals.iter().rev().map(String::as_str).collect();
                q.push(&u.name);
                q.join(".")
            })
            .collect()
    }

    #[test]
    fn view_uses_sql_reads_both_usage_views() {
        let sql = view_uses_sql("ventas", "v_o'k");
        assert!(sql.contains("information_schema.VIEW_TABLE_USAGE u"));
        assert!(sql.contains("information_schema.VIEW_ROUTINE_USAGE r"));
        assert!(sql.contains("u.VIEW_SCHEMA = 'ventas' AND u.VIEW_NAME = 'v_o''k'"));
        assert!(sql.contains("r.TABLE_SCHEMA = 'ventas' AND r.TABLE_NAME = 'v_o''k'"));
        assert!(sql.contains("LEFT JOIN information_schema.TABLES t"));
    }

    #[test]
    fn trigger_sql_reads_its_table() {
        let sql = trigger_sql("ventas", "trg");
        assert!(sql.contains("EVENT_OBJECT_SCHEMA, EVENT_OBJECT_TABLE"));
        assert!(sql.contains("TRIGGER_SCHEMA = 'ventas' AND TRIGGER_NAME = 'trg'"));
    }

    #[test]
    fn view_rows_are_confirmed_here_and_external_elsewhere() {
        let rows = vec![
            row(&[Some("ventas"), Some("clientes"), Some("BASE TABLE")]),
            row(&[Some("ventas"), Some("v_base"), Some("VIEW")]),
            row(&[Some("ventas"), Some("f_total"), Some("FUNCTION")]),
            row(&[Some("Ventas"), Some("perdida"), None]),
            row(&[Some("stock"), Some("items"), Some("BASE TABLE")]),
            row(&[Some("ventas"), Some("clientes"), Some("BASE TABLE")]),
        ];
        let (items, external) = view_uses(&rows, "ventas");
        let got: Vec<(&str, &str)> = items
            .iter()
            .map(|d| (d.kind.as_str(), d.name.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("table", "clientes"),
                ("view", "v_base"),
                ("function", "f_total"),
                ("table", "perdida"),
            ]
        );
        assert!(items
            .iter()
            .all(|d| d.confidence == Confidence::Confirmed && d.schema.is_none()));
        assert_eq!(
            external,
            [ExternalRef {
                database: Some("stock".into()),
                name: Some("items".into()),
                ..Default::default()
            }]
        );
    }

    #[test]
    fn trigger_row_is_its_table() {
        let rows = vec![row(&[
            Some("ventas"),
            Some("pedidos"),
            Some("BEFORE"),
            Some("INSERT"),
        ])];
        let (items, external) = trigger_table(&rows, "ventas");
        assert!(external.is_empty());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "pedidos");
        assert_eq!(items[0].kind, kinds::TABLE);
        assert_eq!(items[0].detail.as_deref(), Some("BEFORE INSERT"));
        assert_eq!(items[0].confidence, Confidence::Confirmed);
    }

    #[test]
    fn foreign_keys_split_by_database() {
        let t = ObjectRef {
            kind: kinds::TABLE.into(),
            schema: None,
            name: "pedidos".into(),
        };
        let rows = vec![
            row(&[
                Some("fk_cli"),
                Some("cliente_id"),
                None,
                Some("clientes"),
                Some("id"),
            ]),
            row(&[
                Some("fk_item"),
                Some("item_id"),
                Some("stock"),
                Some("items"),
                Some("id"),
            ]),
            row(&[
                Some("fk_item"),
                Some("item_var"),
                Some("stock"),
                Some("items"),
                Some("var"),
            ]),
        ];
        let (items, external) = foreign_key_uses(fk_defs(&rows), &t);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "clientes");
        assert_eq!(items[0].schema, None);
        assert_eq!(items[0].relation, Relation::ForeignKey);
        assert_eq!(items[0].confidence, Confidence::Confirmed);
        assert_eq!(
            items[0].detail.as_deref(),
            Some("fk_cli (cliente_id) → clientes (id)")
        );
        assert_eq!(external.len(), 1);
        assert_eq!(external[0].database.as_deref(), Some("stock"));
        assert_eq!(external[0].name.as_deref(), Some("items"));
        assert_eq!(external[0].server, None);
        assert_eq!(
            external[0].detail.as_deref(),
            Some("fk_item (item_id, item_var) → stock.items (id, var)")
        );
    }

    #[test]
    fn tidb_cross_database_key_from_the_create() {
        // TiDB 7.5's KEY_COLUMN_USAGE says `ventas` for a key to `stock.items`.
        let rows = vec![
            row(&[
                Some("fk_l"),
                Some("item_id"),
                None,
                Some("items"),
                Some("id"),
            ]),
            row(&[
                Some("fk_c"),
                Some("cli_id"),
                None,
                Some("clientes"),
                Some("id"),
            ]),
        ];
        let mut fks = fk_defs(&rows);
        let create = "CREATE TABLE `lineas` (\n  `id` int(11) NOT NULL,\n  `item_id` int(11) DEFAULT NULL,\n  `cli_id` int(11) DEFAULT NULL,\n  PRIMARY KEY (`id`) /*T![clustered_index] CLUSTERED */,\n  CONSTRAINT `fk_l` FOREIGN KEY (`item_id`) REFERENCES `stock`.`items` (`id`),\n  CONSTRAINT `fk_c` FOREIGN KEY (`cli_id`) REFERENCES `clientes` (`id`)\n) ENGINE=InnoDB";
        fix_ref_schemas(&mut fks, create, &d(), "ventas");
        assert_eq!(fks[0].ref_schema.as_deref(), Some("stock"));
        assert_eq!(fks[1].ref_schema, None);
        let t = ObjectRef {
            kind: kinds::TABLE.into(),
            schema: None,
            name: "lineas".into(),
        };
        let (items, external) = foreign_key_uses(fks, &t);
        assert_eq!(items.len(), 1);
        assert_eq!(external[0].database.as_deref(), Some("stock"));
    }

    #[test]
    fn nested_join_takes_the_first_table() {
        // How MySQL stores a view.
        let body = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select `db`.`a`.`id` AS `id`,`db`.`f`(`db`.`b`.`x`) AS `y` from (`db`.`a` join `db`.`b` on((`db`.`a`.`id` = `db`.`b`.`id`)))";
        let used = code_names(body, &d());
        assert_eq!(names(&used), ["db.f", "db.a", "db.b"]);
        // MariaDB's, and a subquery that isn't a table.
        let body = "select `t1`.`id` AS `id` from ((`t1` join `t2` on(`t1`.`id` = `t2`.`id`)) join (select 1 AS `x`) `s`)";
        assert_eq!(names(&code_names(body, &d())), ["t1", "t2"]);
    }

    #[test]
    fn names_split_by_database_and_catalog() {
        let body = "CREATE PROCEDURE p() BEGIN\n  SELECT * FROM ventas.clientes c JOIN otra.items i ON i.id = c.id;\n  INSERT INTO bitacora VALUES (1);\n  CALL otra.registrar(1);\nEND";
        let uses = split_uses(code_names(body, &d()), Variant::MySql, "ventas", false);
        assert_eq!(names(&uses.local), ["p", "clientes", "bitacora"]);
        let other: Vec<_> = uses
            .other
            .iter()
            .map(|o| (o.database.as_str(), o.used.name.as_str()))
            .collect();
        assert_eq!(other, [("otra", "items"), ("otra", "registrar")]);
        let external = uses.external(body, &[]);
        assert_eq!(external.len(), 2);
        assert_eq!(external[0].database.as_deref(), Some("otra"));
        assert_eq!(external[0].name.as_deref(), Some("items"));
        assert_eq!(external[0].server, None);
        assert_eq!(external[0].mentions.len(), 1);
        assert_eq!(external[0].mentions[0].line, 2);
        assert_eq!(external[1].mentions[0].line, 4);

        // StarRocks: the default catalog is this one; another is external.
        let body = "select * from default_catalog.ventas.a join hive_prod.logs.eventos e on true";
        let uses = split_uses(code_names(body, &d()), Variant::StarRocks, "ventas", false);
        assert_eq!(names(&uses.local), ["a"]);
        let types = [("hive_prod".to_string(), "hive".to_string())];
        let external = uses.external(body, &types);
        assert_eq!(external.len(), 1);
        assert_eq!(external[0].system.as_deref(), Some("hive"));
        assert_eq!(external[0].database.as_deref(), Some("logs"));
        assert_eq!(external[0].name.as_deref(), Some("eventos"));
        assert_eq!(external[0].detail.as_deref(), Some("hive_prod"));
    }

    #[test]
    fn trigger_rows_new_and_old_are_columns() {
        let body = "CREATE TRIGGER trg BEFORE INSERT ON pedidos FOR EACH ROW\nBEGIN\n  SELECT COUNT(*) INTO NEW.n FROM items;\n  SELECT 1 INTO OLD.m FROM otra.t;\nEND";
        let uses = split_uses(code_names(body, &d()), Variant::MySql, "ventas", true);
        assert_eq!(names(&uses.local), ["items"]);
        assert_eq!(uses.other.len(), 1);
        assert_eq!(uses.other[0].database, "otra");
    }

    #[test]
    fn catalog_rows_take_the_code_mentions() {
        let body = "select `db`.`a`.`id` AS `id`\nfrom (`db`.`a` join `db`.`b` on(true))";
        let confirmed = vec![
            catalog_item(kinds::TABLE, "a".into(), None),
            catalog_item(kinds::VIEW, "b".into(), None),
        ];
        let found = vec![Dependent {
            kind: kinds::TABLE.into(),
            schema: None,
            name: "b".into(),
            parent: None,
            relation: Relation::Code,
            confidence: Confidence::Probable,
            detail: None,
            mentions: vec![Mention {
                line: 2,
                text: "x".into(),
                dynamic: false,
            }],
        }];
        let out = merge(confirmed, found, Some(body), &d());
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|d| d.confidence == Confidence::Confirmed));
        // `b`: the code's (a table resolved, a view in the catalog).
        assert_eq!(out[1].mentions[0].text, "x");
        // `a`: searched in the body.
        let lines: Vec<u32> = out[0].mentions.iter().map(|m| m.line).collect();
        assert_eq!(lines, [1, 2]);
    }

    #[test]
    fn code_only_names_stay_probable() {
        let found = vec![Dependent {
            kind: kinds::FUNCTION.into(),
            schema: None,
            name: "f".into(),
            parent: None,
            relation: Relation::Code,
            confidence: Confidence::Probable,
            detail: None,
            mentions: Vec::new(),
        }];
        let out = merge(Vec::new(), found, None, &d());
        assert_eq!(out[0].confidence, Confidence::Probable);
    }

    #[test]
    fn external_entries_merge() {
        let mut list = vec![ExternalRef {
            database: Some("stock".into()),
            name: Some("items".into()),
            ..Default::default()
        }];
        add_external(
            &mut list,
            ExternalRef {
                database: Some("STOCK".into()),
                name: Some("items".into()),
                mentions: vec![Mention {
                    line: 3,
                    text: "from stock.items".into(),
                    dynamic: false,
                }],
                ..Default::default()
            },
        );
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].mentions[0].line, 3);
        // A foreign key to the same table is its own entry.
        add_external(
            &mut list,
            ExternalRef {
                database: Some("stock".into()),
                name: Some("items".into()),
                detail: Some("fk (a) → stock.items (id)".into()),
                ..Default::default()
            },
        );
        assert_eq!(list.len(), 1);
        add_external(
            &mut list,
            ExternalRef {
                database: Some("stock".into()),
                name: Some("items".into()),
                detail: Some("fk2 (b) → stock.items (id)".into()),
                ..Default::default()
            },
        );
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn federated_url_without_credentials() {
        let create = "CREATE TABLE `remota` (\n  `id` int NOT NULL,\n  `connection` varchar(10) COMMENT 'CONNECTION=x'\n) ENGINE=FEDERATED DEFAULT CHARSET=utf8mb4 CONNECTION='mysql://fed:s3cr@t/x@db.example.com:3307/ventas/clientes'";
        let r = remote_source(Variant::MySql, create, &d(), "remota").unwrap();
        let Remote::External { external, link } = r else {
            panic!("{r:?}")
        };
        assert_eq!(link, None);
        assert_eq!(external.system.as_deref(), Some("FEDERATED"));
        assert_eq!(external.server.as_deref(), Some("db.example.com:3307"));
        assert_eq!(external.database.as_deref(), Some("ventas"));
        assert_eq!(external.name.as_deref(), Some("clientes"));
        let all = format!("{external:?}");
        assert!(!all.contains("s3cr") && !all.contains("fed:"), "{all}");
    }

    #[test]
    fn federated_through_a_server() {
        let create = "CREATE TABLE `r` (`id` int) ENGINE=FEDERATED CONNECTION='enlace/clientes'";
        let r = remote_source(Variant::MariaDb, create, &d(), "r").unwrap();
        let Remote::External { external, link } = r else {
            panic!("{r:?}")
        };
        assert_eq!(link.as_deref(), Some("enlace"));
        assert_eq!(external.name.as_deref(), Some("clientes"));
        assert_eq!(external.detail.as_deref(), Some("enlace"));
        // Without a table, the remote one has the local name.
        let create = "CREATE TABLE `r` (`id` int) ENGINE=FEDERATED CONNECTION='enlace'";
        let Some(Remote::External { external, .. }) =
            remote_source(Variant::MySql, create, &d(), "r")
        else {
            panic!()
        };
        assert_eq!(external.name.as_deref(), Some("r"));
    }

    #[test]
    fn plain_tables_have_no_remote_source() {
        let create = "CREATE TABLE `t` (`id` int, `engine` varchar(10) DEFAULT 'FEDERATED') ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";
        assert_eq!(remote_source(Variant::MySql, create, &d(), "t"), None);
        let create = "CREATE TABLE `t` (`id` int) ENGINE=OLAP DUPLICATE KEY(`id`) PROPERTIES (\"replication_num\" = \"1\")";
        assert_eq!(remote_source(Variant::StarRocks, create, &d(), "t"), None);
    }

    #[test]
    fn connect_tables() {
        let mysql = "CREATE TABLE `c` (`id` int) ENGINE=CONNECT DEFAULT CHARSET=latin1 `TABLE_TYPE`=MYSQL `CONNECTION`='mysql://u:pw@10.0.0.5/ventas/clientes'";
        let Some(Remote::External { external, .. }) =
            remote_source(Variant::MariaDb, mysql, &d(), "c")
        else {
            panic!()
        };
        assert_eq!(external.system.as_deref(), Some("CONNECT MYSQL"));
        assert_eq!(external.server.as_deref(), Some("10.0.0.5"));
        assert_eq!(external.database.as_deref(), Some("ventas"));
        assert_eq!(external.name.as_deref(), Some("clientes"));

        let odbc = "CREATE TABLE `o` (`id` int) ENGINE=CONNECT `TABLE_TYPE`='ODBC' `TABNAME`='dbo.Clientes' `CONNECTION`='Driver={ODBC Driver 18};Server=sql.local;Port=1433;Database=crm;UID=sa;PWD=secreto'";
        let Some(Remote::External { external, .. }) =
            remote_source(Variant::MariaDb, odbc, &d(), "o")
        else {
            panic!()
        };
        assert_eq!(external.system.as_deref(), Some("CONNECT ODBC"));
        assert_eq!(external.server.as_deref(), Some("sql.local:1433"));
        assert_eq!(external.database.as_deref(), Some("crm"));
        assert_eq!(external.name.as_deref(), Some("dbo.Clientes"));
        assert!(!format!("{external:?}").contains("secreto"));

        let jdbc = "CREATE TABLE `j` (`id` int) ENGINE=CONNECT `TABLE_TYPE`=JDBC `TABNAME`='pedidos' `CONNECTION`='jdbc:postgresql://pg.local:5432/erp?user=a&password=b@c'";
        let Some(Remote::External { external, .. }) =
            remote_source(Variant::MariaDb, jdbc, &d(), "j")
        else {
            panic!()
        };
        assert_eq!(external.server.as_deref(), Some("pg.local:5432"));
        assert_eq!(external.database.as_deref(), Some("erp"));
        assert!(!format!("{external:?}").contains("password"));

        let csv = "CREATE TABLE `f` (`id` int) ENGINE=CONNECT `TABLE_TYPE`=CSV `FILE_NAME`='/data/clientes.csv' `HEADER`=1";
        let Some(Remote::External { external, .. }) =
            remote_source(Variant::MariaDb, csv, &d(), "f")
        else {
            panic!()
        };
        assert_eq!(external.system.as_deref(), Some("CONNECT CSV"));
        assert_eq!(external.name.as_deref(), Some("/data/clientes.csv"));

        let proxy = "CREATE TABLE `p` (`id` int) ENGINE=CONNECT `TABLE_TYPE`=PROXY `TABNAME`='clientes' `DBNAME`='otra'";
        assert_eq!(
            remote_source(Variant::MariaDb, proxy, &d(), "p"),
            Some(Remote::Local {
                database: Some("otra".into()),
                table: "clientes".into()
            })
        );
    }

    #[test]
    fn starrocks_external_table() {
        let create = "CREATE EXTERNAL TABLE `m` (\n  `id` int(11) NULL\n) ENGINE=MYSQL\nPROPERTIES (\n\"host\" = \"10.0.0.9\",\n\"port\" = \"3306\",\n\"user\" = \"root\",\n\"password\" = \"clave\",\n\"database\" = \"ventas\",\n\"table\" = \"clientes\"\n);";
        let Some(Remote::External { external, .. }) =
            remote_source(Variant::StarRocks, create, &d(), "m")
        else {
            panic!()
        };
        assert_eq!(external.system.as_deref(), Some("MYSQL"));
        assert_eq!(external.server.as_deref(), Some("10.0.0.9:3306"));
        assert_eq!(external.database.as_deref(), Some("ventas"));
        assert_eq!(external.name.as_deref(), Some("clientes"));
        assert!(!format!("{external:?}").contains("clave"));

        let es = "CREATE EXTERNAL TABLE `e` (`id` int) ENGINE=ELASTICSEARCH PROPERTIES (\"hosts\" = \"http://u:p@es.local:9200\", \"index\" = \"logs\")";
        let Some(Remote::External { external, .. }) = remote_source(Variant::Doris, es, &d(), "e")
        else {
            panic!()
        };
        assert_eq!(external.server.as_deref(), Some("http://es.local:9200"));
        assert_eq!(external.name.as_deref(), Some("logs"));
    }

    #[test]
    fn url_parts_drop_credentials_and_query() {
        assert_eq!(
            url_parts("mysql://a:b@h:1/d/t", false),
            (Some("h:1".into()), Some("d".into()), Some("t".into()))
        );
        assert_eq!(
            url_parts("mysql://h/d/t", false),
            (Some("h".into()), Some("d".into()), Some("t".into()))
        );
        assert_eq!(
            url_parts("jdbc:mysql://h:3306/d?user=x&password=y", true),
            (Some("h:3306".into()), Some("d".into()), None)
        );
    }

    #[test]
    fn string_values_unquote() {
        assert_eq!(value(TokenKind::String, "'a''b\\\\c'"), "a'b\\c");
        assert_eq!(value(TokenKind::String, "\"x\""), "x");
        assert_eq!(value(TokenKind::Name, "MYSQL"), "MYSQL");
    }

    #[test]
    fn mentions_one_per_line() {
        let body = "a\nfrom x join x\nb x";
        let at: Vec<usize> = body.match_indices('x').map(|(i, _)| i).collect();
        let m = mentions(body, &at);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].line, m[0].text.as_str()), (2, "from x join x"));
        assert_eq!((m[1].line, m[1].text.as_str()), (3, "b x"));
    }
}
