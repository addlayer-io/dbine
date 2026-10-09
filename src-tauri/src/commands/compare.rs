//! "Comparar esquemas": load two databases' schemas, compare them, and
//! apply the changes the user carried from one side to the other.
//!
//! Comparing and planning are pure (the UI keeps both schemas and edits its
//! copies); only loading and running touch the servers.

use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::{kinds, Driver, ObjectRef, QueryOutcome, SyncScript, TableChange, TableSchema};
use dbine_schema::compare::{CodeObject, CompareOptions, CompareResult, DbModel};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// Kinds compared by their source.
const CODE_KINDS: &[&str] = &[
    kinds::VIEW,
    kinds::MATERIALIZED_VIEW,
    kinds::PROCEDURE,
    kinds::FUNCTION,
    kinds::TRIGGER,
    kinds::SEQUENCE,
    kinds::SYNONYM,
    kinds::TYPE,
    DOMAIN,
    FULLTEXT_CATALOG,
    FULLTEXT_STOPLIST,
    // SQLite's FTS / R*Tree tables; ClickHouse's dictionaries.
    "virtual_table",
    "dictionary",
];
/// Domains, where the engine keeps them apart from types (H2).
const DOMAIN: &str = "domain";
/// SQL Server's full-text catalogs and stoplists.
const FULLTEXT_CATALOG: &str = "fulltext_catalog";
const FULLTEXT_STOPLIST: &str = "fulltext_stoplist";
/// What tables use (column types, defaults, full-text indexes): created
/// before the tables, dropped after them.
const TABLE_PREREQS: &[&str] = &[kinds::TYPE, DOMAIN, kinds::SEQUENCE, FULLTEXT_CATALOG, FULLTEXT_STOPLIST];
/// Their definition makes them as they should be whether they exist or not
/// (the server won't drop one an index still uses).
const IN_PLACE: &[&str] = &[FULLTEXT_CATALOG, FULLTEXT_STOPLIST];

#[derive(Deserialize)]
pub struct LoadArgs {
    pub connection_id: String,
    pub database: String,
    /// Only these schemas (all when empty).
    #[serde(default)]
    pub schemas: Vec<String>,
}

#[derive(Serialize)]
pub struct Loaded {
    #[serde(flatten)]
    pub model: DbModel,
    /// Objects whose source couldn't be read.
    pub warnings: Vec<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn schema_compare_load(state: State<'_, AppState>, args: LoadArgs) -> CommandResult<Loaded> {
    load_model(&state, args).await
}

/// A database's tables and code objects, as the comparison reads them.
pub(crate) async fn load_model(state: &AppState, args: LoadArgs) -> CommandResult<Loaded> {
    let driver = driver_of(state, &args.connection_id)?;
    let schemas = args.schemas.clone();
    let wanted = move |s: &Option<String>| schemas.is_empty() || s.as_deref().is_some_and(|s| schemas.iter().any(|w| w == s));
    let (tables, objects, warnings) = state
        .meta_read(&args.connection_id, &args.database, crate::commands::explorer::SCHEMA_LIMIT, move |s| {
            Box::pin(async move {
                let tables: Vec<TableSchema> = s.database_schema().await?.into_iter().filter(|t| wanted(&t.schema)).collect();
                let mut objects = Vec::new();
                let mut warnings = Vec::new();
                for o in s.list_objects().await? {
                    if !CODE_KINDS.contains(&o.kind.as_str()) || !wanted(&o.schema) {
                        continue;
                    }
                    let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
                    match s.definition(&r).await {
                        Ok(Some(definition)) => objects.push(CodeObject { kind: o.kind, schema: o.schema, name: o.name, definition }),
                        Ok(None) => {}
                        Err(e) => warnings.push(format!("{}: {e}", o.name)),
                    }
                }
                Ok((tables, objects, warnings))
            })
        })
        .await?;
    Ok(Loaded { model: DbModel { driver: driver.info().id.to_string(), tables, objects }, warnings })
}

#[derive(Deserialize)]
pub struct CompareArgs {
    pub left: DbModel,
    pub right: DbModel,
    #[serde(default)]
    pub options: CompareOptions,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn schema_compare(args: CompareArgs) -> CommandResult<CompareResult> {
    Ok(dbine_schema::compare::compare(&args.left, &args.right, &args.options))
}

#[derive(Deserialize)]
pub struct ConvertArgs {
    pub from_driver: String,
    pub to_driver: String,
    pub tables: Vec<TableSchema>,
    /// The schema they go to on the other side.
    pub target_schema: Option<String>,
}

#[derive(Serialize)]
pub struct Converted {
    pub tables: Vec<TableSchema>,
    /// What didn't carry over exactly.
    pub warnings: Vec<String>,
}

/// Tables in another engine's terms, to carry them across engines.
#[tauri::command(rename_all = "camelCase")]
pub async fn schema_compare_convert(args: ConvertArgs) -> CommandResult<Converted> {
    if args.from_driver == args.to_driver {
        let tables = args.tables.into_iter().map(|t| TableSchema { schema: args.target_schema.clone().or(t.schema.clone()), ..t }).collect();
        return Ok(Converted { tables, warnings: Vec::new() });
    }
    let opts = dbine_schema::Options { target_schema: args.target_schema, ..Default::default() };
    let c = dbine_schema::convert(&args.tables, &args.from_driver, &args.to_driver, &opts).map_err(|e| CommandError::BadRequest(format!("{e:?}")))?;
    let warnings = c.issues.iter().filter(|i| i.severity != dbine_schema::Severity::Info).map(|i| i.message.clone()).collect();
    Ok(Converted { tables: c.tables, warnings })
}

/// A view, procedure… to create, drop or replace.
#[derive(Deserialize, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ObjectChange {
    Create { object: CodeObject },
    Drop { object: CodeObject },
    Replace { object: CodeObject },
}

#[derive(Deserialize)]
pub struct ScriptArgs {
    pub connection_id: String,
    #[serde(default)]
    pub tables: Vec<TableChange>,
    #[serde(default)]
    pub objects: Vec<ObjectChange>,
    /// The target's views as they'll be: the ones over a table whose columns
    /// change are dropped first and made again after (most engines refuse to
    /// change a column a view uses).
    #[serde(default)]
    pub views: Vec<CodeObject>,
}

/// The target's script for the changes: code objects that go first, the
/// tables, then the code objects that come (views over the new columns).
#[tauri::command(rename_all = "camelCase")]
pub async fn schema_sync_script(state: State<'_, AppState>, args: ScriptArgs) -> CommandResult<SyncScript> {
    sync_script(&state, args)
}

pub(crate) fn sync_script(state: &AppState, args: ScriptArgs) -> CommandResult<SyncScript> {
    let driver = driver_of(state, &args.connection_id)?;
    let mut objects = args.objects;
    let extra = dependent_views(&args.tables, &objects, &args.views);
    let note = (!extra.is_empty()).then(|| {
        format!(
            "Se borran y se vuelven a crear las vistas que usan las tablas modificadas: {}.",
            extra.iter().map(|o| match o { ObjectChange::Replace { object } | ObjectChange::Drop { object } | ObjectChange::Create { object } => object.name.clone() }).collect::<Vec<_>>().join(", ")
        )
    });
    let unmade = views_on_dropped_columns(&args.tables, &extra);
    objects.extend(extra);
    let mut script = plan(driver.as_ref(), &args.tables, &objects)?;
    script.warnings.extend(note);
    script.warnings.extend(unmade);
    let triggers = rebuilt_triggers(&args.tables, &script.statements, &objects, &args.views);
    if !triggers.is_empty() {
        script.warnings.push(format!(
            "Se vuelven a crear los triggers de las tablas que se reconstruyen: {}.",
            triggers.iter().map(|o| o.name.as_str()).collect::<Vec<_>>().join(", ")
        ));
        script.statements.extend(triggers.iter().map(|o| o.definition.trim().to_string()));
    }
    Ok(script)
}

/// The views made again around the ALTERs that name a column the change
/// drops: if they use it, creating them again fails.
fn views_on_dropped_columns(tables: &[TableChange], extra: &[ObjectChange]) -> Vec<String> {
    let mut out = Vec::new();
    for ch in extra {
        let ObjectChange::Replace { object: v } = ch else { continue };
        for t in tables {
            let TableChange::Alter { old, new } = t else { continue };
            if !mentions(&v.definition, &new.name) {
                continue;
            }
            for c in old.columns.iter().filter(|o| !new.columns.iter().any(|n| n.name.eq_ignore_ascii_case(&o.name))) {
                if mentions(&v.definition, &c.name) {
                    out.push(format!(
                        "La vista «{}» nombra la columna {}.{}, que se borra: si la usa, no se va a poder volver a crear. Borrala o cambiala antes de sincronizar.",
                        v.name, new.name, c.name
                    ));
                }
            }
        }
    }
    out
}

/// The side's triggers on tables the script rebuilds (new table, copy,
/// drop, rename: SQLite and the like), which go with the dropped original
/// and are made again after it. Not the ones the changes already drop or
/// make.
fn rebuilt_triggers<'a>(tables: &[TableChange], statements: &[String], objects: &[ObjectChange], side: &'a [CodeObject]) -> Vec<&'a CodeObject> {
    let rebuilt: Vec<&TableSchema> = tables
        .iter()
        .filter_map(|c| match c {
            TableChange::Alter { new, .. } if statements.iter().any(|s| mentions(s, &format!("{}__dbine_new", new.name))) => Some(new),
            _ => None,
        })
        .collect();
    if rebuilt.is_empty() {
        return Vec::new();
    }
    let handled = |t: &CodeObject| {
        objects.iter().any(|o| match o {
            ObjectChange::Create { object } | ObjectChange::Drop { object } | ObjectChange::Replace { object } => {
                object.kind == t.kind && object.name.eq_ignore_ascii_case(&t.name) && object.schema == t.schema
            }
        })
    };
    let bare = |n: &str| n.rsplit('.').next().unwrap_or(n).trim_matches(|c| matches!(c, '"' | '`' | '[' | ']')).to_lowercase();
    side.iter()
        .filter(|o| o.kind == kinds::TRIGGER && !handled(o))
        .filter(|o| {
            pg::trigger_tables(&o.definition)
                .iter()
                .any(|(t, _)| rebuilt.iter().any(|r| bare(t) == r.name.to_lowercase() && (o.schema.is_none() || r.schema.is_none() || o.schema == r.schema)))
        })
        .collect()
}

/// Definitions ordered so that one naming another comes after it (a domain
/// over an enum, a composite using a domain…); a cycle keeps its order.
fn by_dependency(items: Vec<(String, String)>) -> Vec<String> {
    let order = dependency_order(&items.iter().map(|(n, t)| (n.as_str(), t.as_str())).collect::<Vec<_>>(), false);
    let mut items: Vec<Option<String>> = items.into_iter().map(|(_, t)| Some(t)).collect();
    order.into_iter().filter_map(|i| items[i].take()).collect()
}

/// The order of `(name, text)` items so that one whose text names another
/// comes after it, or before it with `dependents_first` (to drop them).
/// Items nothing orders keep their place; a cycle is broken at its first
/// item.
fn dependency_order(items: &[(&str, &str)], dependents_first: bool) -> Vec<usize> {
    use std::collections::{BTreeSet, HashSet};
    let n = items.len();
    // Each text's words, once: thousands of routines would be too many
    // substring searches.
    let words: Vec<HashSet<String>> = items
        .iter()
        .map(|(_, t)| t.to_lowercase().split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).filter(|w| !w.is_empty()).map(str::to_string).collect())
        .collect();
    let names: Vec<String> = items.iter().map(|(name, _)| name.to_lowercase()).collect();
    let names_in = |i: usize, j: usize| {
        let name = &names[j];
        if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$') {
            words[i].contains(name)
        } else {
            mentions(items[i].1, items[j].0)
        }
    };
    // `waits[a]`: how many items must go before `a`; `unblocks[b]`: the ones waiting on `b`.
    let mut waits = vec![0usize; n];
    let mut unblocks: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        // Same-named objects (a trigger on two tables) each name the other.
        for j in (0..n).filter(|&j| names[j] != names[i] && names_in(i, j)) {
            let (first, then) = if dependents_first { (i, j) } else { (j, i) };
            waits[then] += 1;
            unblocks[first].push(then);
        }
    }
    let mut ready: BTreeSet<usize> = (0..n).filter(|&i| waits[i] == 0).collect();
    let mut left: BTreeSet<usize> = (0..n).collect();
    let mut out = Vec::with_capacity(n);
    while let Some(&next) = ready.iter().next().or_else(|| left.iter().next()) {
        ready.remove(&next);
        left.remove(&next);
        out.push(next);
        for &k in &unblocks[next] {
            waits[k] = waits[k].saturating_sub(1);
            if waits[k] == 0 && left.contains(&k) {
                ready.insert(k);
            }
        }
    }
    out
}

/// Whether `text` names `name` as a whole word (any case).
fn mentions(text: &str, name: &str) -> bool {
    let (t, n) = (text.to_lowercase(), name.to_lowercase());
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
    let mut from = 0;
    while let Some(i) = t[from..].find(&n) {
        let at = from + i;
        if !word(t[..at].chars().last()) && !word(t[at + n.len()..].chars().next()) {
            return true;
        }
        from = at + n.len();
    }
    false
}

/// Views over tables whose columns are dropped or change type, not already
/// being changed: made again around the ALTERs. Views over dropped tables
/// only go.
fn dependent_views(tables: &[TableChange], objects: &[ObjectChange], views: &[CodeObject]) -> Vec<ObjectChange> {
    let touched: Vec<(&str, bool)> = tables
        .iter()
        .filter_map(|c| match c {
            TableChange::Drop { table } => Some((table.name.as_str(), true)),
            TableChange::Alter { old, new } => {
                let reshaped = old.columns.iter().any(|o| {
                    new.columns.iter().find(|n| n.name.eq_ignore_ascii_case(&o.name)).is_none_or(|n| n.data_type.to_lowercase().split_whitespace().collect::<String>() != o.data_type.to_lowercase().split_whitespace().collect::<String>())
                });
                reshaped.then_some((new.name.as_str(), false))
            }
            TableChange::Create { .. } => None,
        })
        .collect();
    let handled = |v: &CodeObject| {
        objects.iter().any(|o| match o {
            ObjectChange::Create { object } | ObjectChange::Drop { object } | ObjectChange::Replace { object } => {
                object.kind == v.kind && object.name.eq_ignore_ascii_case(&v.name) && object.schema == v.schema
            }
        })
    };
    views
        .iter()
        .filter(|v| (v.kind == kinds::VIEW || v.kind == kinds::MATERIALIZED_VIEW) && !handled(v))
        .filter_map(|v| {
            let hit: Vec<bool> = touched.iter().filter(|(t, _)| mentions(&v.definition, t)).map(|(_, dropped)| *dropped).collect();
            if hit.is_empty() {
                None
            } else if hit.iter().any(|d| *d) {
                Some(ObjectChange::Drop { object: v.clone() })
            } else {
                Some(ObjectChange::Replace { object: v.clone() })
            }
        })
        .collect()
}

fn plan(driver: &dyn Driver, tables: &[TableChange], objects: &[ObjectChange]) -> CommandResult<SyncScript> {
    let tables_part = if tables.is_empty() {
        SyncScript::default()
    } else if driver.supports_schema_sync() {
        driver.sync_script(tables)?
    } else {
        return Err(CommandError::BadRequest(format!("{} no aplica cambios de esquema desde DBine", driver.info().name)));
    };
    Ok(plan_around(driver, tables_part, objects))
}

/// The code objects' drops and creates around `middle` (the tables' changes,
/// a rename…): drops first (dependents before what they use), then the
/// prerequisites, `middle`, the creates (what's used first), and the
/// prerequisites only dropped. `middle`'s warnings are kept.
pub(crate) fn plan_around(driver: &dyn Driver, middle: SyncScript, objects: &[ObjectChange]) -> SyncScript {
    let mut script = middle;
    // Drops as (object, statements): ordered once all are known.
    let mut before: Vec<(&CodeObject, Vec<String>)> = Vec::new();
    let mut early = Vec::new();
    let mut after: Vec<&CodeObject> = Vec::new();
    let mut late: Vec<(&CodeObject, Vec<String>)> = Vec::new();
    let mut dropped: Vec<&CodeObject> = Vec::new();
    for ch in objects {
        let (o, drop, create) = match ch {
            ObjectChange::Create { object } => (object, false, true),
            ObjectChange::Drop { object } => (object, true, false),
            ObjectChange::Replace { object } => (object, true, true),
        };
        let prereq = TABLE_PREREQS.contains(&o.kind.as_str());
        if drop && !(create && IN_PLACE.contains(&o.kind.as_str())) {
            match drop_statements(driver, o) {
                // Only dropped: after the tables that may still use it.
                Some(s) if prereq && !create => late.push((o, s)),
                Some(s) => before.push((o, s)),
                None => script.warnings.push(format!("{}: este motor no permite borrarlo desde DBine.", o.name)),
            }
        }
        if drop && !create {
            dropped.push(o);
        }
        if create {
            let d = o.definition.trim().to_string();
            if prereq {
                early.push((o.name.clone(), d));
            } else {
                after.push(o);
            }
        }
    }
    if !dropped.is_empty() {
        script.warnings.push(dropped_warning(&dropped));
    }
    let tables_part = std::mem::take(&mut script.statements);
    let early = by_dependency(early);
    // A trigger before its function, a view before the view it reads.
    let drops = |list: Vec<(&CodeObject, Vec<String>)>| {
        let order = dependency_order(&list.iter().map(|(o, _)| (o.name.as_str(), o.definition.as_str())).collect::<Vec<_>>(), true);
        let mut list: Vec<Option<Vec<String>>> = list.into_iter().map(|(_, s)| Some(s)).collect();
        let mut seen = std::collections::HashSet::new();
        // Same-named triggers on several tables come as one object per table.
        order.into_iter().filter_map(|i| list[i].take()).flatten().filter(|s| seen.insert(s.clone())).collect::<Vec<_>>()
    };
    let creates = dependency_order(&after.iter().map(|o| (o.name.as_str(), o.definition.as_str())).collect::<Vec<_>>(), false)
        .into_iter()
        .map(|i| after[i].definition.trim().to_string());
    script.statements = drops(before).into_iter().chain(early).chain(tables_part).chain(creates).chain(drops(late)).collect();
    script
}

/// What goes in Spanish, for the warning.
fn kind_label(kind: &str) -> &str {
    match kind {
        kinds::VIEW => "la vista",
        kinds::MATERIALIZED_VIEW => "la vista materializada",
        kinds::PROCEDURE => "el procedimiento",
        kinds::FUNCTION => "la función",
        kinds::TRIGGER => "el trigger",
        kinds::SEQUENCE => "la secuencia",
        kinds::SYNONYM => "el sinónimo",
        kinds::TYPE => "el tipo",
        DOMAIN => "el dominio",
        FULLTEXT_CATALOG => "el catálogo de texto completo",
        FULLTEXT_STOPLIST => "la lista de palabras irrelevantes",
        "virtual_table" => "la tabla virtual",
        "dictionary" => "el diccionario",
        _ => "el objeto",
    }
}

/// Names what's dropped for good (not the ones made again).
fn dropped_warning(dropped: &[&CodeObject]) -> String {
    const SHOWN: usize = 10;
    let mut names: Vec<String> = dropped.iter().take(SHOWN).map(|o| format!("{} «{}»", kind_label(&o.kind), o.name)).collect();
    if dropped.len() > SHOWN {
        names.push(format!("y {} más", dropped.len() - SHOWN));
    }
    format!("Se borran {}: revisá que nada más dependa de ellos.", names.join(", "))
}

/// The statements that drop `o`. Most engines name it and that's it;
/// PostgreSQL drops a trigger `ON` its table, and same-named functions (or
/// procedures) one signature at a time, since the bare name is ambiguous.
fn drop_statements(driver: &dyn Driver, o: &CodeObject) -> Option<Vec<String>> {
    let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
    let plain = crate::commands::scripts::drop_other(driver, &r, true)?;
    if driver.info().dialect != "postgres" {
        return Some(vec![plain]);
    }
    let if_exists = if plain.contains(" IF EXISTS ") { "IF EXISTS " } else { "" };
    let quote = |s: &str| dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Double, s);
    match o.kind.as_str() {
        kinds::TRIGGER => {
            let tables = pg::trigger_tables(&o.definition);
            if tables.is_empty() {
                return Some(vec![plain]);
            }
            Some(
                tables
                    .into_iter()
                    .map(|(t, qualified)| {
                        let table = match (&o.schema, qualified) {
                            (Some(s), false) => format!("{}.{t}", quote(s)),
                            _ => t,
                        };
                        format!("DROP TRIGGER {if_exists}{} ON {table};", quote(&o.name))
                    })
                    .collect(),
            )
        }
        // Overloads (all of them in the definition) one by one, whether
        // dropped for good or to be created again (CockroachDB refuses a
        // bare name that has several).
        kinds::FUNCTION | kinds::PROCEDURE => {
            let sigs = pg::signatures(&o.definition);
            if sigs.len() < 2 {
                return Some(vec![plain]);
            }
            Some(sigs.into_iter().map(|(keyword, sig)| format!("DROP {keyword} {if_exists}{sig};")).collect())
        }
        _ => Some(vec![plain]),
    }
}

/// Reading PostgreSQL's own DDL (`pg_get_triggerdef`, `pg_get_functiondef`)
/// at the top level: strings, quoted names, dollar-quoted bodies and
/// comments are skipped whole.
mod pg {
    #[derive(Clone, Copy, PartialEq)]
    enum Tok {
        Word,
        Quoted,
        Literal,
        Punct(char),
    }

    struct Token {
        tok: Tok,
        start: usize,
        end: usize,
    }

    fn tokens(text: &str) -> Vec<Token> {
        let b = text.as_bytes();
        let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            let start = i;
            let tok = if c.is_ascii_whitespace() {
                i += 1;
                continue;
            } else if text[i..].starts_with("--") {
                i = text[i..].find('\n').map_or(b.len(), |n| i + n);
                continue;
            } else if text[i..].starts_with("/*") {
                i = text[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
                continue;
            } else if c == b'\'' || c == b'"' {
                // Doubled quotes stay inside.
                i += 1;
                loop {
                    match b.get(i) {
                        None => break,
                        Some(&q) if q == c && b.get(i + 1) == Some(&c) => i += 2,
                        Some(&q) if q == c => {
                            i += 1;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
                if c == b'"' { Tok::Quoted } else { Tok::Literal }
            } else if c == b'$' && dollar_tag(&text[i..]).is_some() {
                let tag = dollar_tag(&text[i..]).unwrap_or_default();
                i += tag.len();
                i = text[i..].find(tag).map_or(b.len(), |n| i + n + tag.len());
                Tok::Literal
            } else if word(c) {
                while i < b.len() && word(b[i]) {
                    i += 1;
                }
                Tok::Word
            } else {
                let ch = text[i..].chars().next().unwrap_or(' ');
                i += ch.len_utf8();
                Tok::Punct(ch)
            };
            out.push(Token { tok, start, end: i });
        }
        out
    }

    /// `$tag$` (or `$$`) at the start of `s`.
    fn dollar_tag(s: &str) -> Option<&str> {
        let rest = &s[1..];
        let end = rest.find('$')?;
        let tag = &rest[..end];
        (tag.is_empty() || (!tag.starts_with(|c: char| c.is_ascii_digit()) && tag.chars().all(|c| c.is_alphanumeric() || c == '_'))).then(|| &s[..end + 2])
    }

    fn is(text: &str, t: &Token, w: &str) -> bool {
        t.tok == Tok::Word && text[t.start..t.end].eq_ignore_ascii_case(w)
    }

    /// A dotted name from `i`: its text, whether it's qualified, and where it ends.
    fn name_at(text: &str, toks: &[Token], i: usize) -> Option<(String, bool, usize)> {
        let part = |k: usize| toks.get(k).filter(|t| matches!(t.tok, Tok::Word | Tok::Quoted));
        let first = part(i)?;
        let mut end = i;
        while toks.get(end + 1).is_some_and(|t| t.tok == Tok::Punct('.')) && part(end + 2).is_some() {
            end += 2;
        }
        Some((text[first.start..toks[end].end].to_string(), end > i, end + 1))
    }

    /// The tables of each `CREATE … TRIGGER … ON <table>` (several when
    /// same-named triggers were read together), as written, and whether
    /// the name carries its schema.
    pub fn trigger_tables(text: &str) -> Vec<(String, bool)> {
        let toks = tokens(text);
        let mut out = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            if !is(text, &toks[i], "TRIGGER") || !toks[..i].iter().rev().take(3).any(|t| is(text, t, "CREATE")) {
                i += 1;
                continue;
            }
            // ON is reserved: no bare event column is called that.
            let Some(on) = (i + 1..toks.len()).find(|&k| is(text, &toks[k], "ON")) else { break };
            match name_at(text, &toks, on + 1) {
                Some((table, qualified, end)) => {
                    out.push((table, qualified));
                    i = end;
                }
                None => i = on + 1,
            }
        }
        out
    }

    /// Each `CREATE [OR REPLACE] FUNCTION|PROCEDURE name(args)` header as
    /// (keyword, `name(args)` without the defaults), ready for a DROP.
    pub fn signatures(text: &str) -> Vec<(&'static str, String)> {
        let toks = tokens(text);
        let mut out = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            let keyword = if is(text, t, "FUNCTION") {
                "FUNCTION"
            } else if is(text, t, "PROCEDURE") {
                "PROCEDURE"
            } else {
                continue;
            };
            let created = (i >= 1 && is(text, &toks[i - 1], "CREATE")) || (i >= 3 && is(text, &toks[i - 1], "REPLACE") && is(text, &toks[i - 3], "CREATE"));
            if !created {
                continue;
            }
            let Some((name, _, open)) = name_at(text, &toks, i + 1) else { continue };
            if toks.get(open).is_none_or(|t| t.tok != Tok::Punct('(')) {
                continue;
            }
            let mut args = Vec::new();
            let mut arg: Option<(usize, usize)> = None;
            let mut defaulted = false;
            let mut depth = 0;
            let mut closed = false;
            for t in &toks[open + 1..] {
                match t.tok {
                    Tok::Punct('(') => depth += 1,
                    Tok::Punct(')') if depth == 0 => {
                        closed = true;
                        break;
                    }
                    Tok::Punct(')') => depth -= 1,
                    Tok::Punct(',') if depth == 0 => {
                        args.extend(arg.take());
                        defaulted = false;
                        continue;
                    }
                    _ => {}
                }
                if depth == 0 && (is(text, t, "DEFAULT") || t.tok == Tok::Punct('=')) {
                    defaulted = true;
                }
                if !defaulted {
                    arg = Some((arg.map_or(t.start, |a| a.0), t.end));
                }
            }
            if !closed {
                continue;
            }
            args.extend(arg);
            let args: Vec<&str> = args.iter().map(|&(a, b)| text[a..b].trim()).collect();
            out.push((keyword, format!("{name}({})", args.join(", "))));
        }
        out
    }
}

#[derive(Deserialize)]
pub struct RunArgs {
    pub connection_id: String,
    pub database: String,
    pub statements: Vec<String>,
    /// For `cancel_query` (`sync:<run_id>`).
    pub run_id: String,
    /// All or nothing: in one transaction, rolled back on an error or a
    /// cancel, where the engine runs DDL inside one ("Renombrar…": manual
    /// transactions and `RenameSpec::transactional`). Otherwise statement
    /// by statement, as always.
    #[serde(default)]
    pub atomic: bool,
    /// A database rename: DBine's own sessions on that database (tabs,
    /// explorer, metadata) are closed first, or they hold it open (and the
    /// engine refuses or ends them anyway).
    #[serde(default)]
    pub close_database: Option<String>,
}

#[derive(Serialize)]
pub struct RunResult {
    /// How many statements ran.
    pub done: usize,
    /// The one that failed, and why (the rest didn't run).
    pub failed: Option<(usize, String)>,
    /// An atomic run that failed was rolled back: nothing changed.
    pub rolled_back: bool,
}

/// `schema-sync-progress`: statements run so far in `run_id`. The first one
/// (`done: 0`) also says the run's session is registered, so a cancel sent
/// from then on reaches it.
#[derive(Serialize, Clone)]
struct SyncProgress<'a> {
    run_id: &'a str,
    done: usize,
    total: usize,
}

/// Minimum gap between two progress events of one run (the last one always goes).
const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_millis(150);

/// Run the script on the target, statement by statement; stops at the first
/// error. Refused on read-only connections.
#[tauri::command(rename_all = "camelCase")]
pub async fn schema_sync_run(app: AppHandle, state: State<'_, AppState>, args: RunArgs) -> CommandResult<RunResult> {
    let conn = state.store.get_connection(&args.connection_id)?.ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?;
    if conn.config.read_only {
        return Err(CommandError::BadRequest(format!("«{}» es de solo lectura: no se pueden aplicar cambios", conn.name)));
    }
    let atomic = args.atomic && {
        let driver = crate::commands::schema::driver_of(&state, &args.connection_id)?;
        driver.supports_manual_transactions() && driver.rename_spec().is_some_and(|s| s.transactional)
    };
    if let Some(db) = args.close_database.as_deref().filter(|d| !d.is_empty()) {
        state.sessions.retain(|_, e| !(e.connection_id == args.connection_id && e.database == db));
    }
    let key = format!("sync:{}", args.run_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, false).await?;
    if atomic {
        if let Err(e) = entry.session.lock().await.set_autocommit(false).await {
            state.sessions.remove(&key);
            return Err(e.into());
        }
    }
    let total = args.statements.len();
    let emit = |done| {
        let _ = app.emit("schema-sync-progress", SyncProgress { run_id: &args.run_id, done, total });
    };
    emit(0);
    let mut last = std::time::Instant::now();
    let mut done = 0;
    let mut failed = None;
    for (i, sql) in args.statements.iter().enumerate() {
        if last.elapsed() >= PROGRESS_EVERY {
            emit(done);
            last = std::time::Instant::now();
        }
        if entry.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            failed = Some((i, "cancelado".to_string()));
            break;
        }
        let mut s = entry.session.lock().await;
        let mut out = QueryOutcome::default();
        let r = s.execute(sql, 0, &mut out).await.map_err(|e| e.to_string()).and_then(|_| match out.error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        });
        match r {
            Ok(()) => done += 1,
            Err(e) => {
                failed = Some((i, e));
                break;
            }
        }
    }
    let mut rolled_back = false;
    if atomic {
        let mut s = entry.session.lock().await;
        if failed.is_none() {
            if let Err(e) = s.commit().await {
                failed = Some((total, e.to_string()));
            }
        }
        if failed.is_some() {
            rolled_back = s.rollback().await.is_ok();
        }
    }
    state.sessions.remove(&key);
    emit(done);
    Ok(RunResult { done, failed, rolled_back })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_come_after_the_ones_they_use() {
        let d = |n: &str, t: &str| (n.to_string(), t.to_string());
        let out = by_dependency(vec![
            d("t_comp", "CREATE TYPE t_comp AS (a d_pos, b estado)"),
            d("d_pos", "CREATE DOMAIN d_pos AS int CHECK (VALUE > 0)"),
            d("estado", "CREATE TYPE estado AS ENUM ('a')"),
        ]);
        assert_eq!(out.last().unwrap(), "CREATE TYPE t_comp AS (a d_pos, b estado)");
    }

    #[test]
    fn finds_dependent_views() {
        assert!(mentions("SELECT id FROM clientes;", "clientes"));
        assert!(mentions("from \"public\".\"Clientes\" c", "clientes"));
        assert!(!mentions("FROM clientes_viejos", "clientes"));
        let t = |cols: &[(&str, &str)]| TableSchema {
            name: "clientes".into(),
            columns: cols.iter().map(|(n, ty)| dbine_driver::ColumnDef { name: (*n).into(), data_type: (*ty).into(), ..Default::default() }).collect(),
            ..Default::default()
        };
        let view = |name: &str, def: &str| CodeObject { kind: "view".into(), schema: None, name: name.into(), definition: def.into() };
        let views = vec![view("v1", "SELECT nombre FROM clientes"), view("v2", "SELECT 1 FROM pedidos")];
        let alter = vec![TableChange::Alter { old: t(&[("nombre", "varchar(5)")]), new: t(&[("nombre", "varchar(10)")]) }];
        let got = dependent_views(&alter, &[], &views);
        assert!(matches!(got.as_slice(), [ObjectChange::Replace { object }] if object.name == "v1"));
        // Adding a column doesn't touch the views.
        let add = vec![TableChange::Alter { old: t(&[("nombre", "text")]), new: t(&[("nombre", "text"), ("x", "int")]) }];
        assert!(dependent_views(&add, &[], &views).is_empty());
        let drop = vec![TableChange::Drop { table: t(&[]) }];
        assert!(matches!(dependent_views(&drop, &[], &views).as_slice(), [ObjectChange::Drop { .. }]));
    }

    #[test]
    fn warns_about_views_on_dropped_columns() {
        let t = |cols: &[&str]| TableSchema {
            name: "clientes".into(),
            columns: cols.iter().map(|n| dbine_driver::ColumnDef { name: (*n).into(), data_type: "int".into(), ..Default::default() }).collect(),
            ..Default::default()
        };
        let alter = vec![TableChange::Alter { old: t(&["id", "saldo"]), new: t(&["id"]) }];
        let views = vec![
            CodeObject { kind: "view".into(), schema: None, name: "v_saldo".into(), definition: "SELECT id, saldo FROM clientes".into() },
            CodeObject { kind: "view".into(), schema: None, name: "v_id".into(), definition: "SELECT id FROM clientes".into() },
        ];
        let extra = dependent_views(&alter, &[], &views);
        assert_eq!(extra.len(), 2);
        let w = views_on_dropped_columns(&alter, &extra);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("«v_saldo»") && w[0].contains("clientes.saldo"), "{w:?}");
    }

    #[test]
    fn rebuilt_tables_get_their_triggers_back() {
        let t = |cols: &[&str]| TableSchema {
            name: "hijo".into(),
            columns: cols.iter().map(|n| dbine_driver::ColumnDef { name: (*n).into(), data_type: "int".into(), ..Default::default() }).collect(),
            ..Default::default()
        };
        let alter = vec![TableChange::Alter { old: t(&["id", "x"]), new: t(&["id"]) }];
        let rebuild = vec!["CREATE TABLE \"hijo__dbine_new\" (id int);\nDROP TABLE \"hijo\";\nALTER TABLE \"hijo__dbine_new\" RENAME TO \"hijo\";".to_string()];
        let trig = |name: &str, table: &str| CodeObject { kind: "trigger".into(), schema: None, name: name.into(), definition: format!("CREATE TRIGGER {name} AFTER INSERT ON \"{table}\" BEGIN SELECT 1; END") };
        let side = vec![trig("tg_hijo", "hijo"), trig("tg_otro", "otro"), trig("tg_ido", "hijo")];
        // The one the user drops stays dropped.
        let objects = vec![ObjectChange::Drop { object: side[2].clone() }];
        let got = rebuilt_triggers(&alter, &rebuild, &objects, &side);
        assert_eq!(got.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), ["tg_hijo"]);
        // An ALTER in place drops no trigger.
        assert!(rebuilt_triggers(&alter, &["ALTER TABLE \"hijo\" DROP COLUMN \"x\";".to_string()], &[], &side).is_empty());
        // Nor does a table whose name only ends like it.
        let mut other = t(&["id"]);
        other.name = "ijo".into();
        let alter = vec![TableChange::Alter { old: t(&["id", "x"]), new: other }];
        assert!(rebuilt_triggers(&alter, &rebuild, &[], &side).is_empty());
    }

    fn obj(kind: &str, name: &str, def: &str) -> CodeObject {
        CodeObject { kind: kind.into(), schema: Some("public".into()), name: name.into(), definition: def.into() }
    }

    fn drop(o: CodeObject) -> ObjectChange {
        ObjectChange::Drop { object: o }
    }

    #[test]
    fn reads_trigger_tables() {
        assert_eq!(pg::trigger_tables("CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()"), vec![("t".to_string(), false)]);
        // UPDATE OF columns, a quoted and qualified table, a constraint trigger.
        let def = "CREATE CONSTRAINT TRIGGER \"Tg\" AFTER UPDATE OF a, \"on\" ON \"Sch\".\"My T\" FROM other DEFERRABLE FOR EACH ROW EXECUTE FUNCTION f()\n\nCREATE TRIGGER \"Tg\" INSTEAD OF INSERT ON v2 FOR EACH ROW EXECUTE FUNCTION g()";
        assert_eq!(pg::trigger_tables(def), vec![("\"Sch\".\"My T\"".to_string(), true), ("v2".to_string(), false)]);
        assert!(pg::trigger_tables("CREATE VIEW v AS SELECT 1").is_empty());
    }

    #[test]
    fn reads_function_signatures() {
        let def = "CREATE OR REPLACE FUNCTION public.f(a integer, b numeric(10,2) DEFAULT 1.5, c text DEFAULT 'x, (y'::text)\n RETURNS integer\n LANGUAGE sql\nAS $function$ CREATE FUNCTION nope(x int) $function$\n\n\
                   CREATE OR REPLACE FUNCTION public.f()\n RETURNS integer\n LANGUAGE plpgsql\nAS $$ begin return 1; end $$\n\n\
                   CREATE OR REPLACE PROCEDURE public.\"F x\"(INOUT n integer = 0)\n LANGUAGE sql\nAS $p$ select 1 $p$";
        assert_eq!(
            pg::signatures(def),
            vec![
                ("FUNCTION", "public.f(a integer, b numeric(10,2), c text)".to_string()),
                ("FUNCTION", "public.f()".to_string()),
                ("PROCEDURE", "public.\"F x\"(INOUT n integer)".to_string()),
            ]
        );
    }

    #[test]
    fn postgres_drops_triggers_on_their_table() {
        let pg = dbine_drivers::find("postgres").unwrap().as_ref();
        let tg = obj("trigger", "tg", "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION audit()");
        let f = obj("function", "audit", "CREATE OR REPLACE FUNCTION public.audit()\n RETURNS trigger\nAS $$ begin return new; end $$");
        // The function first in the list: the trigger that calls it still goes first.
        let s = plan(pg, &[], &[drop(f), drop(tg.clone()), drop(tg)]).unwrap();
        assert_eq!(s.statements, vec!["DROP TRIGGER IF EXISTS \"tg\" ON \"public\".t;", "DROP FUNCTION IF EXISTS \"public\".\"audit\";"]);
        assert!(s.warnings.iter().any(|w| w.contains("el trigger «tg»") && w.contains("la función «audit»")), "{:?}", s.warnings);
        // Other engines: by name.
        let my = dbine_drivers::find("mysql").unwrap().as_ref();
        let s = plan(my, &[], &[drop(obj("trigger", "tg", "CREATE TRIGGER tg BEFORE INSERT ON t FOR EACH ROW SET NEW.a = 1"))]).unwrap();
        assert_eq!(s.statements, vec!["DROP TRIGGER IF EXISTS `public`.`tg`;"]);
    }

    #[test]
    fn postgres_drops_overloads_one_by_one() {
        let pg = dbine_drivers::find("postgres").unwrap().as_ref();
        let def = "CREATE OR REPLACE FUNCTION public.f(a integer)\n RETURNS int\nAS $$ select 1 $$\n\nCREATE OR REPLACE FUNCTION public.f(a text)\n RETURNS int\nAS $$ select 2 $$";
        let s = plan(pg, &[], &[drop(obj("function", "f", def))]).unwrap();
        assert_eq!(s.statements, vec!["DROP FUNCTION IF EXISTS public.f(a integer);", "DROP FUNCTION IF EXISTS public.f(a text);"]);
        // A single one (or a replace) keeps the plain DROP.
        let one = "CREATE OR REPLACE FUNCTION public.g(a integer)\n RETURNS int\nAS $$ select 1 $$";
        let s = plan(pg, &[], &[drop(obj("function", "g", one))]).unwrap();
        assert_eq!(s.statements, vec!["DROP FUNCTION IF EXISTS \"public\".\"g\";"]);
        // Replaced (a rename's rewritten dependent): one by one too.
        let s = plan(pg, &[], &[ObjectChange::Replace { object: obj("function", "f", def) }]).unwrap();
        assert_eq!(&s.statements[..2], ["DROP FUNCTION IF EXISTS public.f(a integer);", "DROP FUNCTION IF EXISTS public.f(a text);"]);
        assert_eq!(s.statements.len(), 3, "{:?}", s.statements);
    }

    #[test]
    fn drop_order_and_placement() {
        let pg = dbine_drivers::find("postgres").unwrap().as_ref();
        let v1 = obj("view", "v1", "SELECT 1 AS a");
        let v2 = obj("view", "v2", "SELECT a FROM v1");
        let seq = obj("sequence", "s1", "CREATE SEQUENCE s1");
        let t = TableSchema { schema: Some("public".into()), name: "t".into(), ..Default::default() };
        let s = plan(pg, &[TableChange::Drop { table: t }], &[drop(seq), drop(v1), drop(v2)]).unwrap();
        let at = |p: &str| s.statements.iter().position(|x| x.contains(p)).unwrap_or_else(|| panic!("{p}: {:?}", s.statements));
        // The view over the other goes first; a sequence only dropped goes after the tables.
        assert!(at("\"v2\"") < at("\"v1\""));
        assert!(at("\"v1\"") < at("DROP TABLE"));
        assert!(at("DROP TABLE") < at("SEQUENCE"));
        // Creates the other way round.
        let s = plan(pg, &[], &[ObjectChange::Create { object: obj("view", "v2", "CREATE VIEW v2 AS SELECT a FROM v1") }, ObjectChange::Create { object: obj("view", "v1", "CREATE VIEW v1 AS SELECT 1 AS a") }]).unwrap();
        assert_eq!(s.statements, vec!["CREATE VIEW v1 AS SELECT 1 AS a", "CREATE VIEW v2 AS SELECT a FROM v1"]);
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn dependency_order_keeps_unrelated_and_breaks_cycles() {
        let items = [("a", "x"), ("b", "uses c"), ("c", "y"), ("d", "uses e"), ("e", "uses d")];
        assert_eq!(dependency_order(&items, false), vec![0, 2, 1, 3, 4]);
        assert_eq!(dependency_order(&items, true), vec![0, 1, 2, 3, 4]);
        // Names that aren't one word still count.
        assert_eq!(dependency_order(&[("x", "FROM \"my v\""), ("my v", "")], false), vec![1, 0]);
        let many: Vec<(String, String)> = (0..3000).map(|i| (format!("o{i}"), format!("select * from o{}", i + 1))).collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(n, d)| (n.as_str(), d.as_str())).collect();
        assert_eq!(dependency_order(&refs, false)[0], 2999);
    }
}
