//! "Renombrar…": rename an object, a column, an index, a constraint or a
//! schema and put back the code that names it, in one reviewed script.
//!
//! `rename_impact` reads what depends on the target ("Ver dependencias")
//! and each dependent's definition, and sorts them: what the engine updates
//! by itself, what DBine rewrites (`dbine_driver::rename::rewrite_references`)
//! and what the user has to look at. `rename_script` is pure, like
//! `schema_sync_script`: the driver's rename in the middle, the rewritten
//! dependents around it (`compare::plan_around`). It runs through
//! `schema_sync_run`, atomically where the engine allows it.

use crate::commands::compare::{plan_around, ObjectChange};
use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::rename::{
    carried_dependents, DatabaseObject, names_in_code, quote_new, rewrite_references, trigger_routine, with_create_style, writes_to_table, Edit, RewriteOptions, Unresolved,
};
use dbine_driver::{
    kinds, Confidence, DependencyReport, DependencyScan, Dependent, Driver, ObjectRef, ReferenceStyle, Relation, RenameRequest, RenameSpec,
    RenameTarget, ReplaceStyle, ScriptDialect, SyncScript, TableSchema,
};
use dbine_schema::compare::CodeObject;
use serde::{Deserialize, Serialize};
use tauri::State;

/// The scan reads every definition one by one, as "Ver dependencias".
const IMPACT_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
pub struct ImpactArgs {
    pub connection_id: String,
    pub database: String,
    pub target: RenameTarget,
    pub new_name: String,
    /// A view's renamed column keeps its output name (`nuevo AS viejo`).
    #[serde(default = "yes")]
    pub keep_view_columns: bool,
}

/// What the rename touches.
#[derive(Serialize)]
pub struct RenameImpact {
    pub items: Vec<ImpactItem>,
    /// Definitions the scan read.
    pub scanned: u32,
    /// Objects whose definition couldn't be read: what depends on the
    /// target may hide there.
    pub unreadable: Vec<String>,
    /// The scan's own note (a missing permission…).
    pub note: Option<String>,
    /// The driver's caveat for this engine ([`RenameSpec::note`]).
    pub spec_note: Option<String>,
    /// Another object (column, index…) already has the new name.
    pub collides: bool,
    /// The new name as the script writes it: quoted when its case or its
    /// characters need it.
    pub quoted_name: String,
    /// The script runs in one transaction (rolled back on an error).
    pub atomic: bool,
    /// What `rename_script` needs back: the target's definition and table.
    pub definition: Option<String>,
    pub table: Option<TableSchema>,
    /// A database rename: the other sessions on it, which the rename ends
    /// (DBine's own there are closed before it runs).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<dbine_driver::monitor::ServerProcess>,
    /// A database rename that moves its contents: what it holds, back to
    /// `rename_script` ([`RenameSpec::database_moves`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<DatabaseObject>,
    /// A database rename: the database the script runs in
    /// ([`RenameSpec::database_from`]; empty: none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_on: Option<String>,
}

#[derive(Serialize)]
pub struct ImpactItem {
    pub dependent: Dependent,
    pub action: Action,
    /// The dependent's definition as it is (rewritten ones).
    pub original: Option<String>,
}

/// What happens to a dependent.
#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// A foreign key, index or check: the engine updates it.
    Engine,
    /// A view, trigger… the engine follows by itself ([`RenameSpec::tracked`]).
    Tracked,
    /// Left to the user.
    Manual {
        reason: ManualReason,
        /// What the rewrite couldn't decide, when it ran.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        unresolved: Vec<Unresolved>,
    },
    /// DBine rewrites it. `default_selected` is false when part of it was
    /// left out (`unresolved`). `carried`: what depends on it, dropped
    /// with it and created again unchanged (a view dropped and created
    /// again that other views read, [`carried_dependents`]).
    Rewrite { object: CodeObject, edits: Vec<Edit>, unresolved: Vec<Unresolved>, schemabound: bool, default_selected: bool, carried: Vec<CodeObject> },
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManualReason {
    /// The name only shows inside strings (dynamic SQL).
    Dynamic,
    /// Its definition couldn't be read.
    Unreadable,
    /// The engine's dependents aren't rewritten (a pipeline's fields…).
    NotRewritten,
    /// The text doesn't name it in a way the rewrite recognizes.
    NoMatch,
    /// A trigger routine that triggers on other tables run too: its `NEW`
    /// and `OLD` aren't only the renamed column's table.
    SharedRoutine,
}

/// What [`classify`] knows besides the report.
#[derive(Default)]
pub(crate) struct Context {
    /// The database, where it is the schema (ClickHouse, MySQL): a name
    /// qualified with it is the target's ([`RewriteOptions::database`]).
    pub database: Option<String>,
    /// Items of the report that are routines run by triggers on the
    /// renamed column's table (PostgreSQL trigger functions).
    pub row_routines: Vec<usize>,
    /// Those that triggers on other tables run too.
    pub shared_routines: Vec<usize>,
}

/// Whether DBine puts back a dependent of `target` at all: an allowlist,
/// not a list of exceptions. The driver says which dependents may be
/// created again through [`RenameSpec::references`]: `None` (Snowflake,
/// where `CREATE OR REPLACE` hands any object to the renaming role, and
/// tasks, streams and materialized views also run, reset or lose their
/// rows) means none, and every dependent is listed for the user;
/// `Pipeline` rewrites a view's pipeline for an object rename only.
pub(crate) fn puts_back(spec: &RenameSpec, target: &RenameTarget) -> bool {
    match spec.references {
        ReferenceStyle::Sql => true,
        ReferenceStyle::Pipeline => !matches!(target, RenameTarget::Column { .. }),
        ReferenceStyle::None => false,
    }
}

/// A trigger routine found through the triggers on the column's table.
struct RowRoutine {
    schema: Option<String>,
    name: String,
    trigger: String,
    shared: bool,
}

/// The rename, its impact on what depends on it, and what can be rewritten.
#[tauri::command(rename_all = "camelCase")]
pub async fn rename_impact(state: State<'_, AppState>, args: ImpactArgs) -> CommandResult<RenameImpact> {
    let driver = driver_of(&state, &args.connection_id)?;
    let spec = spec_for(driver.as_ref(), &args.target)?;
    let new_name = validate(&args.target, &args.new_name)?;
    let dialect = driver.script_dialect();
    let scan = DependencyScan::new(driver.info(), dialect, driver.capabilities().foreign_keys);
    let target = args.target.clone();
    let tracked = spec.tracked_for_target(&args.target).to_vec();
    let wants_table = spec.wants_table;
    // Definitions are read only where they may be put back.
    let rewrites = puts_back(&spec, &args.target);
    // Engines whose database is the schema: the explorer's objects carry no
    // schema, while the catalog and the stored definitions name the database.
    let database = (!driver.info().has_schemas && !args.database.is_empty()).then(|| args.database.clone());
    let db = database.clone();
    let dl = dialect;
    let read = state
        .meta_read(&args.connection_id, &args.database, IMPACT_LIMIT, move |s| {
            Box::pin(async move {
                let mut report = s.dependents(&target.dependency_target(), &scan).await?;
                let objects = s.list_objects().await.unwrap_or_default();
                let routines = match &target {
                    RenameTarget::Column { table, .. } => row_routines(s.as_mut(), &dl, table, &objects).await,
                    _ => Vec::new(),
                };
                // Routines the scan didn't find (they name no table) go in as code.
                let mut found = Vec::new();
                for r in routines {
                    let same = |d: &Dependent| {
                        matches!(d.kind.as_str(), kinds::FUNCTION | kinds::PROCEDURE) && d.name.eq_ignore_ascii_case(&r.name) && (r.schema.is_none() || d.schema == r.schema)
                    };
                    let (at, added) = match report.items.iter().position(same) {
                        Some(at) => (at, false),
                        None => {
                            let kind = objects
                                .iter()
                                .find(|o| o.name.eq_ignore_ascii_case(&r.name) && (r.schema.is_none() || o.schema == r.schema) && o.kind == kinds::PROCEDURE)
                                .map_or(kinds::FUNCTION, |_| kinds::PROCEDURE);
                            report.items.push(Dependent {
                                kind: kind.into(),
                                schema: r.schema.clone(),
                                name: r.name.clone(),
                                parent: None,
                                relation: Relation::Code,
                                confidence: Confidence::Probable,
                                detail: Some(format!("La ejecuta el trigger «{}»", r.trigger)),
                                mentions: Vec::new(),
                            });
                            (report.items.len() - 1, true)
                        }
                    };
                    // Read as code even if the scan judged it dynamic.
                    report.items[at].confidence = Confidence::Probable;
                    found.push((at, r.shared, added));
                }
                // Each dependent to rewrite, read again (the scan doesn't keep them).
                let mut defs = Vec::with_capacity(report.items.len());
                for d in &report.items {
                    let wanted = rewrites && d.relation == Relation::Code && d.confidence != Confidence::Review && !tracked.contains(&d.kind);
                    defs.push(if wanted { s.definition(&ObjectRef { kind: d.kind.clone(), schema: d.schema.clone(), name: d.name.clone() }).await.ok().flatten() } else { None });
                }
                let definition = match &target {
                    RenameTarget::Object { object, .. } if object.kind != kinds::TABLE => s.definition(object).await.ok().flatten(),
                    _ => None,
                };
                // An object's own table only where the driver uses it (OrientDB's indexes).
                let owner = match &target {
                    RenameTarget::Object { object, .. } if wants_table => Some(object),
                    _ => target.table(),
                };
                let table = match owner {
                    Some(t) => s.database_schema().await?.into_iter().find(|x| x.name == t.name && same_schema(x.schema.as_deref(), t.schema(), db.as_deref())),
                    None => None,
                };
                Ok((report, defs, definition, table, objects, found))
            })
        })
        .await?;
    let (report, defs, definition, table, objects, found) = read;
    let collides = collides(&args.target, &new_name, table.as_ref(), &objects);
    let ctx = Context {
        database,
        row_routines: found.iter().map(|(at, _, _)| *at).collect(),
        shared_routines: found.iter().filter(|(_, shared, _)| *shared).map(|(at, _, _)| *at).collect(),
    };
    // A routine added for its trigger that doesn't name the column isn't one.
    let column = match &args.target {
        RenameTarget::Column { column, .. } => column.as_str(),
        _ => "",
    };
    let unrelated: Vec<usize> = found
        .iter()
        .filter(|(at, _, added)| *added && !defs.get(*at).and_then(|d| d.as_deref()).is_some_and(|d| names_in_code(d, &dialect, column)))
        .map(|(at, _, _)| *at)
        .collect();
    let mut items: Vec<ImpactItem> = classify(&dialect, &spec, &args.target, &new_name, args.keep_view_columns, &report, defs, &ctx)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !unrelated.contains(i))
        .map(|(_, item)| item)
        .collect();
    let mut unreadable = report.unreadable;
    // Views dropped and created again: what reads them goes with them.
    let roots: Vec<(usize, ObjectRef)> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches!(it.action, Action::Rewrite { .. }) && carries(&spec, &it.dependent.kind))
        .map(|(i, it)| (i, ObjectRef { kind: it.dependent.kind.clone(), schema: it.dependent.schema.clone(), name: it.dependent.name.clone() }))
        .collect();
    if !roots.is_empty() {
        let mut skip: Vec<ObjectRef> = items.iter().map(|it| ObjectRef { kind: it.dependent.kind.clone(), schema: it.dependent.schema.clone(), name: it.dependent.name.clone() }).collect();
        if let RenameTarget::Object { object, .. } = &args.target {
            skip.push(object.clone());
        }
        let scan = DependencyScan::new(driver.info(), dialect, driver.capabilities().foreign_keys);
        let found = state
            .meta_read(&args.connection_id, &args.database, IMPACT_LIMIT, move |s| {
                Box::pin(async move {
                    let mut out = Vec::with_capacity(roots.len());
                    for (at, root) in roots {
                        out.push((at, carried_dependents(s.as_mut(), &root, &skip, &scan).await?));
                    }
                    Ok(out)
                })
            })
            .await?;
        for (at, (carried, missing)) in found {
            if let Action::Rewrite { carried: c, .. } = &mut items[at].action {
                *c = carried.into_iter().map(|(o, definition)| CodeObject { kind: o.kind, schema: o.schema, name: o.name, definition }).collect();
            }
            for m in missing {
                if !unreadable.contains(&m) {
                    unreadable.push(m);
                }
            }
        }
    }
    Ok(RenameImpact {
        items,
        scanned: report.scanned,
        unreadable,
        note: report.note,
        spec_note: spec.note.clone(),
        collides,
        quoted_name: quote_new(&new_name, &dialect, spec.fold, false),
        atomic: spec.transactional && driver.supports_manual_transactions(),
        definition,
        table,
        sessions: Vec::new(),
        objects: Vec::new(),
        run_on: None,
    })
}

/// Code kinds a database that moves its contents puts back in the new one.
const MOVED_CODE: &[&str] = &[kinds::VIEW, kinds::MATERIALIZED_VIEW, kinds::PROCEDURE, kinds::FUNCTION, kinds::TRIGGER, "event"];

#[derive(Deserialize)]
pub struct DatabaseImpactArgs {
    pub connection_id: String,
    pub database: String,
    pub new_name: String,
}

/// The driver's spec when it renames databases.
fn database_spec(driver: &dyn Driver) -> CommandResult<RenameSpec> {
    driver
        .rename_spec()
        .filter(|s| s.databases)
        .ok_or_else(|| CommandError::BadRequest(format!("{} no renombra bases de datos desde DBine", driver.info().name)))
}

/// The new name of a database: trimmed, not empty, not the old one, and
/// without characters that no engine takes in one (a path, a quote).
fn database_name(old: &str, new_name: &str) -> CommandResult<String> {
    let name = new_name.trim();
    if name.is_empty() {
        return Err(CommandError::BadRequest("el nombre nuevo está vacío".into()));
    }
    if name == old {
        return Err(CommandError::BadRequest("el nombre nuevo es igual al actual".into()));
    }
    if name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | '"' | '\'' | '`' | '[' | ']' | ';')) {
        return Err(CommandError::BadRequest(format!("«{name}» no sirve como nombre de base: tiene caracteres que los motores no aceptan en uno")));
    }
    Ok(name.to_string())
}

/// "Renombrar…" on a database: no code is rewritten (other databases' code
/// that names it is the user's), the sessions it ends, whether the new name
/// is taken, and what it holds where the engine moves it piece by piece.
#[tauri::command(rename_all = "camelCase")]
pub async fn rename_database_impact(state: State<'_, AppState>, args: DatabaseImpactArgs) -> CommandResult<RenameImpact> {
    let driver = driver_of(&state, &args.connection_id)?;
    let spec = database_spec(driver.as_ref())?;
    let new_name = database_name(&args.database, &args.new_name)?;
    let (connection_id, database) = (args.connection_id.as_str(), args.database.as_str());
    let from = spec.database_from.clone().unwrap_or_default();
    let (names, processes) = state
        .meta_read(connection_id, &from, IMPACT_LIMIT, |s| {
            Box::pin(async move {
                let names = s.list_databases().await?;
                let processes = s.processes().await.unwrap_or_default();
                Ok((names, processes))
            })
        })
        .await?;
    if !names.iter().any(|n| n == database) {
        return Err(CommandError::BadRequest(format!("no se encontró la base «{database}»")));
    }
    let collides = names.iter().any(|n| n.eq_ignore_ascii_case(&new_name));
    let sessions = processes.into_iter().filter(|p| !p.system && !p.own && p.database.as_deref() == Some(database)).collect();
    let mut unreadable = Vec::new();
    let objects = if spec.database_moves {
        let (objects, missing) = state
            .meta_read(connection_id, database, IMPACT_LIMIT, |s| {
                Box::pin(async move {
                    let mut out = Vec::new();
                    let mut missing = Vec::new();
                    for o in s.list_objects().await? {
                        let code = MOVED_CODE.contains(&o.kind.as_str());
                        if !code && o.kind != kinds::TABLE && o.kind != kinds::COLLECTION {
                            continue;
                        }
                        let definition = if code {
                            let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
                            match s.definition(&r).await {
                                Ok(Some(d)) => Some(d),
                                _ => {
                                    missing.push(o.name.clone());
                                    None
                                }
                            }
                        } else {
                            None
                        };
                        out.push(DatabaseObject { kind: o.kind, schema: o.schema, name: o.name, definition });
                    }
                    Ok((out, missing))
                })
            })
            .await?;
        unreadable = missing;
        objects
    } else {
        Vec::new()
    };
    Ok(RenameImpact {
        items: Vec::new(),
        scanned: objects.len() as u32,
        unreadable,
        note: None,
        spec_note: spec.database_note.clone(),
        collides,
        quoted_name: quote_new(&new_name, &driver.script_dialect(), spec.fold, false),
        atomic: false,
        definition: None,
        table: None,
        sessions,
        objects,
        run_on: Some(from),
    })
}

/// A table's schema as the catalog reports it is the target's: equal, or,
/// for a target without one, none or the database (engines where the
/// database is the schema report it as such).
fn same_schema(table: Option<&str>, target: Option<&str>, database: Option<&str>) -> bool {
    let table = table.filter(|s| !s.is_empty());
    match target {
        Some(t) => table == Some(t),
        None => table.is_none() || table.is_some_and(|s| database.is_some_and(|db| s == db)),
    }
}

/// The routines the triggers on `table` run (`EXECUTE FUNCTION f()`), and
/// whether a trigger on another table runs them too. Other triggers are
/// read only when one is found.
async fn row_routines(s: &mut dyn dbine_driver::Session, dialect: &ScriptDialect, table: &ObjectRef, objects: &[dbine_driver::DbObject]) -> Vec<RowRoutine> {
    let triggers: Vec<&dbine_driver::DbObject> = objects.iter().filter(|o| o.kind == kinds::TRIGGER).collect();
    let on_table = |o: &dbine_driver::DbObject| o.parent.as_deref() == Some(table.name.as_str()) && (table.schema().is_none() || o.schema.as_deref() == table.schema());
    let mut out: Vec<RowRoutine> = Vec::new();
    let read = |o: &dbine_driver::DbObject| ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
    for o in triggers.iter().filter(|o| on_table(o)) {
        let Ok(Some(def)) = s.definition(&read(o)).await else { continue };
        // Same-named triggers on several tables come in one definition.
        for part in def.split(";\n\n") {
            let Some((schema, name)) = trigger_routine(part, dialect) else { continue };
            if !dbine_driver::rename::trigger_on(part, dialect, &table.name) {
                continue;
            }
            let schema = schema.or_else(|| o.schema.clone());
            if !out.iter().any(|r| r.name == name && r.schema == schema) {
                out.push(RowRoutine { schema, name, trigger: o.name.clone(), shared: false });
            }
        }
    }
    if out.is_empty() {
        return out;
    }
    for o in &triggers {
        let Ok(Some(def)) = s.definition(&read(o)).await else { continue };
        for part in def.split(";\n\n") {
            if dbine_driver::rename::trigger_on(part, dialect, &table.name) {
                continue;
            }
            if let Some((schema, name)) = trigger_routine(part, dialect) {
                let schema = schema.or_else(|| o.schema.clone());
                for r in out.iter_mut().filter(|r| r.name == name && r.schema == schema) {
                    r.shared = true;
                }
            }
        }
    }
    out
}

/// The driver's spec, if it renames that target.
fn spec_for(driver: &dyn Driver, target: &RenameTarget) -> CommandResult<RenameSpec> {
    let spec = driver.rename_spec().ok_or_else(|| CommandError::BadRequest(format!("{} no renombra objetos desde DBine", driver.info().name)))?;
    if !spec.allows(target) {
        return Err(CommandError::BadRequest(format!("{} no renombra eso desde DBine", driver.info().name)));
    }
    Ok(spec)
}

/// The new name, trimmed: not empty and not the old one.
fn validate(target: &RenameTarget, new_name: &str) -> CommandResult<String> {
    let name = new_name.trim();
    if name.is_empty() {
        return Err(CommandError::BadRequest("el nombre nuevo está vacío".into()));
    }
    if name == target.old_name() {
        return Err(CommandError::BadRequest("el nombre nuevo es igual al actual".into()));
    }
    Ok(name.to_string())
}

/// Another object of the same family, column or index already has the name.
fn collides(target: &RenameTarget, new_name: &str, table: Option<&TableSchema>, objects: &[dbine_driver::DbObject]) -> bool {
    let same = |a: &str| a.eq_ignore_ascii_case(new_name);
    match target {
        RenameTarget::Object { object, .. } => objects.iter().any(|o| same(&o.name) && o.schema.as_deref().filter(|s| !s.is_empty()) == object.schema() && o.kind != kinds::INDEX),
        RenameTarget::Column { .. } => table.is_some_and(|t| t.columns.iter().any(|c| same(&c.name))),
        RenameTarget::Index { .. } => table.is_some_and(|t| t.indexes.iter().any(|i| same(&i.name))),
        RenameTarget::Constraint { .. } => table.is_some_and(|t| {
            t.foreign_keys.iter().filter_map(|f| f.name.as_deref()).chain(t.checks.iter().filter_map(|c| c.name.as_deref())).chain(t.primary_key.as_ref().and_then(|k| k.name.as_deref())).any(same)
        }),
        RenameTarget::Schema { .. } => objects.iter().any(|o| o.schema.as_deref().is_some_and(same)),
    }
}

/// Where each dependent goes. `defs` holds, per item of `report`, the
/// definition read for rewriting (`None`: not read, or unreadable).
#[allow(clippy::too_many_arguments)]
pub(crate) fn classify(
    dialect: &ScriptDialect,
    spec: &RenameSpec,
    target: &RenameTarget,
    new_name: &str,
    keep_view_columns: bool,
    report: &DependencyReport,
    defs: Vec<Option<String>>,
    ctx: &Context,
) -> Vec<ImpactItem> {
    let rewrite_target = target.rewrite_target();
    let rewrites = puts_back(spec, target);
    report
        .items
        .iter()
        .cloned()
        .zip(defs.into_iter().chain(std::iter::repeat(None)))
        .enumerate()
        .map(|(at, (d, def))| {
            let manual = |reason| Action::Manual { reason, unresolved: Vec::new() };
            let action = if d.relation != Relation::Code {
                Action::Engine
            } else if ctx.shared_routines.contains(&at) {
                manual(ManualReason::SharedRoutine)
            } else if spec.tracked_for_target(target).contains(&d.kind) {
                Action::Tracked
            } else if d.confidence == Confidence::Review {
                manual(ManualReason::Dynamic)
            } else if !rewrites {
                manual(ManualReason::NotRewritten)
            } else if let Some(body) = def.as_deref() {
                let opts = RewriteOptions {
                    dependent_schema: d.schema.clone(),
                    keep_view_columns: keep_view_columns && (d.kind == kinds::VIEW || d.kind == kinds::MATERIALIZED_VIEW),
                    database: ctx.database.clone(),
                    row_table: ctx.row_routines.contains(&at),
                };
                let r = rewrite_references(body, dialect, &rewrite_target, new_name, spec, &opts);
                if r.edits.is_empty() {
                    Action::Manual { reason: ManualReason::NoMatch, unresolved: r.unresolved }
                } else if definers(body, dialect) != definers(&r.text, dialect) {
                    // A MySQL definer called like the renamed object: put
                    // back, the view or routine would run as another user.
                    manual(ManualReason::NotRewritten)
                } else {
                    // Put back, a view holding its own rows loses them.
                    let loses_rows = spec.holds_rows.contains(&d.kind) && !writes_to_table(body, dialect);
                    let clean = r.unresolved.is_empty() && !loses_rows;
                    Action::Rewrite {
                        object: CodeObject { kind: d.kind.clone(), schema: d.schema.clone(), name: d.name.clone(), definition: r.text },
                        edits: r.edits,
                        unresolved: r.unresolved,
                        schemabound: dialect.bracket_idents && schemabound(body),
                        default_selected: clean,
                        carried: Vec::new(),
                    }
                }
            } else {
                manual(ManualReason::Unreadable)
            };
            let original = matches!(action, Action::Rewrite { .. }).then(|| def.unwrap_or_default());
            ImpactItem { dependent: d, action, original }
        })
        .collect()
}

/// The users a definition runs as (`DEFINER = user@host`, MySQL and
/// MariaDB), as written: a rewrite must leave them as they were.
fn definers(body: &str, dialect: &ScriptDialect) -> Vec<String> {
    use dbine_driver::sql::TokenKind;
    let toks = dbine_driver::sql::name_tokens(body, dialect);
    (0..toks.len())
        .filter(|&i| toks[i].kind == TokenKind::Name && toks[i].text.eq_ignore_ascii_case("definer") && toks.get(i + 1).is_some_and(|t| t.text == "="))
        .map(|i| toks[i + 2..(i + 5).min(toks.len())].iter().map(|t| &body[t.start..t.end]).collect())
        .collect()
}

/// A rewritten dependent of that kind is dropped and created again, and
/// what reads it has to go with it: views put back with `DropCreate`.
fn carries(spec: &RenameSpec, kind: &str) -> bool {
    (kind == kinds::VIEW || kind == kinds::MATERIALIZED_VIEW) && spec.replace_for(kind) == ReplaceStyle::DropCreate
}

/// A T-SQL module bound to its schema (`WITH SCHEMABINDING`): `sp_rename`
/// refuses what it uses, so it's dropped first and created after.
fn schemabound(body: &str) -> bool {
    dbine_driver::sql::name_tokens(body, &ScriptDialect::tsql()).iter().any(|t| t.kind == dbine_driver::sql::TokenKind::Name && t.text.eq_ignore_ascii_case("schemabinding"))
}

/// A rewritten dependent the user kept.
#[derive(Deserialize, Clone)]
pub struct RewriteChoice {
    pub object: CodeObject,
    #[serde(default)]
    pub schemabound: bool,
    /// [`Action::Rewrite`]'s `carried`, as the impact gave it.
    #[serde(default)]
    pub carried: Vec<CodeObject>,
}

#[derive(Deserialize)]
pub struct ScriptArgs {
    pub connection_id: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub database: String,
    pub request: RenameRequest,
    #[serde(default)]
    pub rewrites: Vec<RewriteChoice>,
}

/// The script: the rewritten dependents that go first, the rename, the
/// rest of them. Pure (no server), so the preview follows each checkbox.
#[tauri::command(rename_all = "camelCase")]
pub async fn rename_script(state: State<'_, AppState>, args: ScriptArgs) -> CommandResult<SyncScript> {
    let driver = driver_of(&state, &args.connection_id)?;
    let spec = spec_for(driver.as_ref(), &args.request.target)?;
    let mut request = args.request;
    request.new_name = validate(&request.target, &request.new_name)?;
    build_script(driver.as_ref(), &spec, &request, &args.rewrites)
}

#[derive(Deserialize)]
pub struct DatabaseScriptArgs {
    pub connection_id: String,
    pub database: String,
    pub new_name: String,
    /// `RenameImpact::objects` (engines that move what the database holds).
    #[serde(default)]
    pub objects: Vec<DatabaseObject>,
}

/// A database rename's script: the driver's, whole (nothing is rewritten
/// around it). Pure, like `rename_script`.
#[tauri::command(rename_all = "camelCase")]
pub async fn rename_database_script(state: State<'_, AppState>, args: DatabaseScriptArgs) -> CommandResult<SyncScript> {
    let driver = driver_of(&state, &args.connection_id)?;
    database_spec(driver.as_ref())?;
    let new_name = database_name(&args.database, &args.new_name)?;
    Ok(driver.rename_database_script(&args.database, &new_name, &args.objects)?)
}

#[derive(Deserialize)]
pub struct FollowArgs {
    pub connection_id: String,
    pub database: String,
    pub new_name: String,
}

/// What of DBine's own followed a renamed database.
#[derive(Serialize, Default)]
pub struct Followed {
    pub connections: u32,
    pub queries: u32,
    pub migrations: u32,
    pub projects: u32,
    pub tasks: u32,
    /// Tasks with steps that change data: their approval named the old
    /// database, so they wait for the user's approval again.
    pub tasks_to_approve: u32,
}

/// After a database rename ran: the connection's default database, saved
/// queries, migrations, project targets and scheduled task steps that named
/// it on that connection now name the new one.
#[tauri::command(rename_all = "camelCase")]
pub async fn rename_database_follow(state: State<'_, AppState>, args: FollowArgs) -> CommandResult<Followed> {
    let store = &state.store;
    let new_name = database_name(&args.database, &args.new_name)?;
    let (cid, old, new) = (args.connection_id.as_str(), args.database.as_str(), new_name.as_str());
    let mut out = Followed::default();
    if let Some(mut c) = store.get_connection(cid)? {
        if c.config.database == old {
            c.config.database = new.to_string();
            store.save_connection(&c)?;
            // As a saved connection pointed at another database: its cached
            // tree and the sessions opened on its default database are stale.
            state.cache_forget(cid);
            state.sessions.retain(|_, e| !(e.connection_id == cid && (e.database.is_empty() || e.database == old)));
            out.connections += 1;
        }
    }
    for mut q in store.list_queries(cid, old)? {
        q.database = new.to_string();
        store.save_query(&q)?;
        out.queries += 1;
    }
    for mut m in store.list_migrations(cid, old)? {
        m.database = new.to_string();
        store.save_migration(&m)?;
        out.migrations += 1;
    }
    for p in store.list_projects()? {
        let mut b = p.binding.clone();
        let mut hit = false;
        for t in b.direct.iter_mut().chain(b.environments.values_mut()) {
            if t.connection_id == cid && t.database == old {
                t.database = new.to_string();
                hit = true;
            }
        }
        if hit {
            store.set_project_binding(&p.id, &b)?;
            out.projects += 1;
        }
    }
    for mut t in store.list_tasks()? {
        let mut hit = false;
        for step in &mut t.steps {
            hit |= follow_json(&mut step.config, cid, old, new);
        }
        if hit {
            if t.approved_writes.is_some() {
                out.tasks_to_approve += 1;
            }
            store.save_task(&t)?;
            out.tasks += 1;
        }
    }
    Ok(out)
}

/// Every object in `v` that names `connection_id` and `database` = `old`
/// gets `new` (a task step's source, target…).
fn follow_json(v: &mut serde_json::Value, cid: &str, old: &str, new: &str) -> bool {
    let mut hit = false;
    match v {
        serde_json::Value::Object(map) => {
            let names_it = map.get("connection_id").and_then(|c| c.as_str()) == Some(cid) && map.get("database").and_then(|d| d.as_str()) == Some(old);
            if names_it {
                map.insert("database".into(), serde_json::Value::String(new.to_string()));
                hit = true;
            }
            for x in map.values_mut() {
                hit |= follow_json(x, cid, old, new);
            }
        }
        serde_json::Value::Array(items) => {
            for x in items {
                hit |= follow_json(x, cid, old, new);
            }
        }
        _ => {}
    }
    hit
}

pub(crate) fn build_script(driver: &dyn Driver, spec: &RenameSpec, request: &RenameRequest, rewrites: &[RewriteChoice]) -> CommandResult<SyncScript> {
    let target = request.target.old_name();
    let middle = driver.rename_script(request)?;
    // `schema_sync_run` runs each statement through `execute`, which cuts
    // it again with the driver's splitter: each one must be a single
    // statement, or what runs isn't what the dialog showed (a name or a
    // stored definition the driver copies into its own statements, with a
    // `;` or a `GO` line the quoting didn't hold).
    for sql in &middle.statements {
        one_unit(driver, sql, &format!("la sentencia que renombra «{target}»"), "renombralo a mano")?;
    }
    let dialect = driver.script_dialect();
    if !rewrites.is_empty() && !puts_back(spec, &request.target) {
        let first = &rewrites[0].object.name;
        return Err(CommandError::BadRequest(format!(
            "{} no vuelve a crear «{first}» desde DBine: lo que nombra lo renombrado solo se lista, porque recrearlo le cambiaría el dueño, los permisos o los datos; corregilo a mano",
            driver.info().name
        )));
    }
    let mut lost = Vec::new();
    let mut objects: Vec<ObjectChange> = Vec::with_capacity(rewrites.len());
    for r in rewrites {
        let style = spec.replace_for(&r.object.kind);
        let object = CodeObject { definition: with_create_style(&r.object.definition, &dialect, style), ..r.object.clone() };
        // A dependent put back must be one unit as the driver cuts scripts:
        // a `GO` line stored in a T-SQL module, or a body whose quoting
        // closes early (a `$$` in a body wrapped in `$$`), would run what
        // follows as statements of their own. Checked as it goes out.
        one_unit(driver, &object.definition, &format!("la definición de «{}»", object.name), "quitala de la lista o recreala a mano")?;
        objects.push(if style == ReplaceStyle::DropCreate || r.schemabound {
            lost.push(format!("«{}»", r.object.name));
            ObjectChange::Replace { object }
        } else {
            ObjectChange::Create { object }
        });
    }
    // What reads a dropped one goes and comes back as it is, once.
    let same = |a: &CodeObject, b: &CodeObject| a.kind == b.kind && a.schema == b.schema && a.name.eq_ignore_ascii_case(&b.name);
    let mut carried: Vec<&CodeObject> = Vec::new();
    for r in rewrites.iter().filter(|r| spec.replace_for(&r.object.kind) == ReplaceStyle::DropCreate || r.schemabound) {
        for c in &r.carried {
            if !rewrites.iter().any(|x| same(&x.object, c)) && !carried.iter().any(|x| same(x, c)) {
                carried.push(c);
            }
        }
    }
    for c in carried {
        one_unit(driver, &c.definition, &format!("la definición de «{}»", c.name), "quitá de la lista lo que la arrastra o recreala a mano")?;
        lost.push(format!("«{}»", c.name));
        objects.push(ObjectChange::Replace { object: c.clone() });
    }
    let mut script = plan_around(driver, middle, &objects);
    if let Some(epilogue) = spec.epilogue_for(&request.target) {
        one_unit(driver, &epilogue, "la sentencia final del renombrado", "renombralo a mano")?;
        script.statements.push(epilogue);
    }
    if !lost.is_empty() && spec.grants_on_objects {
        script.warnings.push(format!("Se borran y se vuelven a crear {}: se pierden los permisos otorgados sobre ellos.", lost.join(", ")));
    }
    Ok(script)
}

/// How many statements `text` runs as: cut by the driver's own splitter,
/// the one its `execute` uses, without client commands (`DELIMITER //`),
/// which the splitter applies and the app never sends.
fn statement_count(driver: &dyn Driver, text: &str) -> usize {
    driver.split_script(text).into_iter().filter(|u| u.kind != dbine_driver::StatementKind::ClientCommand).count()
}

/// `text` (`what`, as the error names it) runs as exactly one statement on
/// `driver`, or the rename refuses the script; `fix` tells the user what
/// to do instead.
fn one_unit(driver: &dyn Driver, text: &str, what: &str, fix: &str) -> CommandResult<()> {
    let n = statement_count(driver, text);
    if n == 1 {
        return Ok(());
    }
    Err(CommandError::BadRequest(if n == 0 {
        format!("{what} no tiene ninguna sentencia que ejecutar: DBine no arma el renombrado así; {fix}")
    } else if driver.script_dialect().batch == dbine_driver::sql::BatchLine::Go {
        format!("{what} tiene una línea que dice solo GO: al ejecutarla se partiría en {n} lotes; {fix}")
    } else {
        format!("{what} se parte en {n} sentencias al ejecutarla: DBine no la ejecuta así; {fix}")
    }))
}

#[cfg(test)]
mod follow_tests {
    use super::follow_json;

    #[test]
    fn task_steps_follow_the_database_on_that_connection_only() {
        let mut v = serde_json::json!({
            "source": { "connection_id": "c1", "database": "ventas" },
            "target": { "connection_id": "c2", "database": "ventas" },
            "list": [{ "connection_id": "c1", "database": "otra" }, { "connection_id": "c1", "database": "ventas", "sql": "x" }]
        });
        assert!(follow_json(&mut v, "c1", "ventas", "ventas2"));
        assert_eq!(v["source"]["database"], "ventas2");
        assert_eq!(v["target"]["database"], "ventas");
        assert_eq!(v["list"][0]["database"], "otra");
        assert_eq!(v["list"][1]["database"], "ventas2");
        assert!(!follow_json(&mut v, "c9", "ventas", "x"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::rename::UnresolvedReason;
    use dbine_driver::{async_trait, ConnectionConfig, DriverInfo, Error, Family, Language, Session};

    /// A driver that renames with `sp_rename`, like SQL Server will.
    struct Fake {
        info: DriverInfo,
        spec: RenameSpec,
    }

    #[async_trait]
    impl Driver for Fake {
        fn info(&self) -> &DriverInfo {
            &self.info
        }
        async fn connect(&self, _: &ConnectionConfig, _: Option<&str>) -> dbine_driver::Result<Box<dyn Session>> {
            Err(Error::Unsupported("test".into()))
        }
        fn rename_spec(&self) -> Option<RenameSpec> {
            Some(self.spec.clone())
        }
        fn rename_script(&self, req: &RenameRequest) -> dbine_driver::Result<SyncScript> {
            let old = match &req.target {
                RenameTarget::Object { object, .. } => format!("{}.{}", object.schema.as_deref().unwrap_or("dbo"), object.name),
                RenameTarget::Column { table, column } => format!("{}.{}.{column}", table.schema.as_deref().unwrap_or("dbo"), table.name),
                _ => return Err(Error::Unsupported("test".into())),
            };
            Ok(SyncScript { statements: vec![format!("EXEC sp_rename N'{old}', N'{}';", req.new_name)], warnings: vec![] })
        }
    }

    fn fake(replace: ReplaceStyle) -> Fake {
        Fake {
            info: DriverInfo {
                id: "fake",
                name: "Fake",
                family: Family::Relational,
                language: Language::Sql,
                dialect: "mssql",
                default_port: 0,
                fields: vec![],
                databases_label: "",
                has_schemas: true,
                object_kinds: vec![],
            },
            spec: RenameSpec {
                kinds: vec!["table".into(), "view".into()],
                columns: true,
                tracked: vec![],
                replace,
                transactional: true,
                ..Default::default()
            },
        }
    }

    fn table_target() -> RenameTarget {
        RenameTarget::Object { object: ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "Clientes".into() }, parent: None }
    }

    fn dependent(kind: &str, name: &str, relation: Relation, confidence: Confidence) -> Dependent {
        Dependent { kind: kind.into(), schema: Some("dbo".into()), name: name.into(), parent: None, relation, confidence, detail: None, mentions: vec![] }
    }

    fn report(items: Vec<Dependent>) -> DependencyReport {
        DependencyReport { items, scanned: 3, unreadable: vec![], note: None }
    }

    #[test]
    fn dependents_go_where_they_belong() {
        let spec = RenameSpec { tracked: vec!["trigger".into()], ..fake(ReplaceStyle::CreateOrAlter).spec };
        let r = report(vec![
            dependent("table", "Pedidos", Relation::ForeignKey, Confidence::Confirmed),
            dependent("trigger", "tg", Relation::Code, Confidence::Probable),
            dependent("procedure", "p_dyn", Relation::Code, Confidence::Review),
            dependent("view", "v_ok", Relation::Code, Confidence::Probable),
            dependent("view", "v_half", Relation::Code, Confidence::Probable),
            dependent("view", "v_bound", Relation::Code, Confidence::Probable),
            dependent("procedure", "p_gone", Relation::Code, Confidence::Probable),
            dependent("procedure", "p_none", Relation::Code, Confidence::Probable),
        ]);
        let defs = vec![
            None,
            None,
            None,
            Some("CREATE VIEW dbo.v_ok AS SELECT id FROM dbo.Clientes".into()),
            Some("CREATE VIEW dbo.v_half AS SELECT id FROM dbo.Clientes WHERE x = 'Clientes'".into()),
            Some("CREATE VIEW dbo.v_bound WITH SCHEMABINDING AS SELECT id FROM dbo.Clientes".into()),
            None,
            Some("CREATE PROCEDURE dbo.p_none AS EXEC('SELECT 1 FROM Clientes')".into()),
        ];
        let items = classify(&ScriptDialect::tsql(), &spec, &table_target(), "Nuevo", true, &r, defs, &Context::default());
        let kinds: Vec<String> = items
            .iter()
            .map(|i| match &i.action {
                Action::Engine => "engine".into(),
                Action::Tracked => "tracked".into(),
                Action::Manual { reason, .. } => format!("manual:{reason:?}"),
                Action::Rewrite { default_selected, schemabound, .. } => format!("rewrite:{default_selected}:{schemabound}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "engine",
                "tracked",
                "manual:Dynamic",
                "rewrite:true:false",
                "rewrite:false:false",
                "rewrite:true:true",
                "manual:Unreadable",
                "manual:NoMatch",
            ]
        );
        let Action::Rewrite { object, unresolved, .. } = &items[4].action else { panic!() };
        assert_eq!(object.definition, "CREATE VIEW dbo.v_half AS SELECT id FROM dbo.Nuevo WHERE x = 'Clientes'");
        assert_eq!(unresolved[0].reason, UnresolvedReason::InString);
        assert_eq!(items[3].original.as_deref(), Some("CREATE VIEW dbo.v_ok AS SELECT id FROM dbo.Clientes"));
        let Action::Manual { unresolved, .. } = &items[7].action else { panic!() };
        assert_eq!(unresolved.len(), 1);
    }

    #[test]
    fn snowflake_lists_every_dependent_and_never_puts_one_back() {
        let d = dbine_drivers::find("snowflake").unwrap();
        let spec = d.rename_spec().unwrap();
        assert!(!puts_back(&spec, &table_target()));
        let code = ["view", "materialized_view", "function", "procedure", "task", "stream", "dynamic_table"];
        let r = report(code.iter().map(|k| dependent(k, "X", Relation::Code, Confidence::Probable)).collect());
        let defs = code.iter().map(|k| Some(format!("CREATE OR REPLACE {} dbo.X AS SELECT id FROM dbo.Clientes", k.to_uppercase().replace('_', " ")))).collect();
        let items = classify(&d.script_dialect(), &spec, &table_target(), "NUEVO", true, &r, defs, &Context::default());
        for it in &items {
            assert_eq!(it.action, Action::Manual { reason: ManualReason::NotRewritten, unresolved: vec![] }, "{}", it.dependent.kind);
        }
        // Sent back anyway (an older UI, a crafted call): refused, whatever its kind.
        for kind in code {
            let o = CodeObject { kind: kind.into(), schema: Some("dbo".into()), name: "K_RUNS_AS_OWNER".into(), definition: "CREATE OR REPLACE TASK dbo.K AS SELECT 1".into() };
            let r = RewriteChoice { object: o, schemabound: false, carried: vec![] };
            assert!(matches!(build_script(d.as_ref(), &spec, &request("NUEVO"), &[r]), Err(CommandError::BadRequest(m)) if m.contains("K_RUNS_AS_OWNER")), "{kind}");
        }
        // The rename alone still goes.
        let s = build_script(d.as_ref(), &spec, &request("NUEVO"), &[]).unwrap();
        assert_eq!(s.statements, vec![r#"ALTER TABLE "dbo"."Clientes" RENAME TO "dbo".NUEVO;"#]);
        // Engines that rewrite keep putting their routines back.
        let other = fake(ReplaceStyle::CreateOrAlter);
        let mut p = choice("p", "CREATE PROCEDURE dbo.p AS SELECT 1 FROM dbo.Nuevo", false);
        p.object.kind = "procedure".into();
        assert!(build_script(&other, &other.spec, &request("Nuevo"), &[p]).is_ok());
    }

    #[test]
    fn engines_that_only_list_or_pipelines_on_columns() {
        let r = report(vec![dependent("view", "v", Relation::Code, Confidence::Probable)]);
        let defs = vec![Some("SELECT 1 FROM Clientes".into())];
        let none = RenameSpec { references: ReferenceStyle::None, ..fake(ReplaceStyle::DropCreate).spec };
        let items = classify(&ScriptDialect::tsql(), &none, &table_target(), "Nuevo", true, &r, defs.clone(), &Context::default());
        assert_eq!(items[0].action, Action::Manual { reason: ManualReason::NotRewritten, unresolved: vec![] });
        let pipe = RenameSpec { references: ReferenceStyle::Pipeline, ..fake(ReplaceStyle::DropCreate).spec };
        let col = RenameTarget::Column { table: ObjectRef { kind: "collection".into(), schema: None, name: "c".into() }, column: "a".into() };
        let items = classify(&ScriptDialect::generic(), &pipe, &col, "b", true, &r, defs, &Context::default());
        assert_eq!(items[0].action, Action::Manual { reason: ManualReason::NotRewritten, unresolved: vec![] });
    }

    fn choice(name: &str, def: &str, schemabound: bool) -> RewriteChoice {
        RewriteChoice { object: CodeObject { kind: "view".into(), schema: Some("dbo".into()), name: name.into(), definition: def.into() }, schemabound, carried: vec![] }
    }

    fn request(new: &str) -> RenameRequest {
        RenameRequest { target: table_target(), new_name: new.into(), table: None, definition: None }
    }

    #[test]
    fn create_or_alter_around_the_rename_and_schemabound_first() {
        let d = fake(ReplaceStyle::CreateOrAlter);
        let rewrites = vec![
            // v2 reads v1: created after it.
            choice("v2", "CREATE VIEW dbo.v2 AS SELECT a FROM dbo.v1", false),
            choice("v1", "CREATE VIEW dbo.v1 AS SELECT a FROM dbo.Nuevo", false),
            choice("vb", "CREATE VIEW dbo.vb WITH SCHEMABINDING AS SELECT a FROM dbo.Nuevo", true),
        ];
        let s = build_script(&d, &d.spec, &request("Nuevo"), &rewrites).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP VIEW IF EXISTS [dbo].[vb];",
                "EXEC sp_rename N'dbo.Clientes', N'Nuevo';",
                "CREATE OR ALTER VIEW dbo.v1 AS SELECT a FROM dbo.Nuevo",
                "CREATE OR ALTER VIEW dbo.v2 AS SELECT a FROM dbo.v1",
                "CREATE OR ALTER VIEW dbo.vb WITH SCHEMABINDING AS SELECT a FROM dbo.Nuevo",
            ]
        );
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("«vb»") && !s.warnings[0].contains("«v1»"), "{:?}", s.warnings);
    }

    #[test]
    fn drop_create_drops_all_first() {
        let d = fake(ReplaceStyle::DropCreate);
        let rewrites = vec![choice("v1", "CREATE VIEW dbo.v1 AS SELECT a FROM dbo.Nuevo", false), choice("v2", "CREATE VIEW dbo.v2 AS SELECT a FROM dbo.v1", false)];
        let s = build_script(&d, &d.spec, &request("Nuevo"), &rewrites).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP VIEW IF EXISTS [dbo].[v2];",
                "DROP VIEW IF EXISTS [dbo].[v1];",
                "EXEC sp_rename N'dbo.Clientes', N'Nuevo';",
                "CREATE VIEW dbo.v1 AS SELECT a FROM dbo.Nuevo",
                "CREATE VIEW dbo.v2 AS SELECT a FROM dbo.v1",
            ]
        );
        assert!(s.warnings[0].contains("«v1»") && s.warnings[0].contains("«v2»"));
        // Nothing to rewrite: just the rename.
        let s = build_script(&d, &d.spec, &request("Nuevo"), &[]).unwrap();
        assert_eq!(s.statements, vec!["EXEC sp_rename N'dbo.Clientes', N'Nuevo';"]);
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn dropped_views_carry_what_reads_them() {
        let d = fake(ReplaceStyle::DropCreate);
        let view = |name: &str, def: &str| CodeObject { kind: "view".into(), schema: Some("dbo".into()), name: name.into(), definition: def.into() };
        let mut v1 = choice("v1", "CREATE VIEW dbo.v1 AS SELECT a FROM dbo.Nuevo", false);
        v1.carried = vec![view("v2", "CREATE VIEW dbo.v2 AS SELECT a FROM dbo.v1"), view("v3", "CREATE VIEW dbo.v3 AS SELECT a FROM dbo.v2")];
        // Another rewrite carrying v2 too: once.
        let mut w = choice("w", "CREATE VIEW dbo.w AS SELECT a FROM dbo.Nuevo", false);
        w.carried = vec![view("v2", "CREATE VIEW dbo.v2 AS SELECT a FROM dbo.v1 JOIN dbo.w ON 1 = 1")];
        let s = build_script(&d, &d.spec, &request("Nuevo"), &[v1.clone(), w]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP VIEW IF EXISTS [dbo].[w];",
                "DROP VIEW IF EXISTS [dbo].[v3];",
                "DROP VIEW IF EXISTS [dbo].[v2];",
                "DROP VIEW IF EXISTS [dbo].[v1];",
                "EXEC sp_rename N'dbo.Clientes', N'Nuevo';",
                "CREATE VIEW dbo.v1 AS SELECT a FROM dbo.Nuevo",
                "CREATE VIEW dbo.w AS SELECT a FROM dbo.Nuevo",
                "CREATE VIEW dbo.v2 AS SELECT a FROM dbo.v1",
                "CREATE VIEW dbo.v3 AS SELECT a FROM dbo.v2",
            ]
        );
        assert!(s.warnings[0].contains("«v2»") && s.warnings[0].contains("«v3»"), "{:?}", s.warnings);
        // Not carried where the dependent is created in place.
        let d = fake(ReplaceStyle::CreateOrAlter);
        let s = build_script(&d, &d.spec, &request("Nuevo"), &[v1]).unwrap();
        assert_eq!(s.statements.len(), 2, "{:?}", s.statements);
        assert!(carries(&fake(ReplaceStyle::DropCreate).spec, "view") && !carries(&fake(ReplaceStyle::DropCreate).spec, "procedure"));
    }

    #[test]
    fn tracked_per_target_and_no_grants() {
        // SQLite: a table's views follow it, a view created again doesn't.
        let spec = RenameSpec { tracked: vec!["view".into()], tracked_for: [("view".to_string(), vec![])].into(), ..fake(ReplaceStyle::DropCreate).spec };
        let r = report(vec![dependent("view", "v2", Relation::Code, Confidence::Probable)]);
        let defs = vec![Some("CREATE VIEW dbo.v2 AS SELECT a FROM dbo.v".into())];
        let items = classify(&ScriptDialect::tsql(), &spec, &table_target(), "Nuevo", true, &r, defs.clone(), &Context::default());
        assert_eq!(items[0].action, Action::Tracked);
        let view = RenameTarget::Object { object: ObjectRef { kind: "view".into(), schema: Some("dbo".into()), name: "v".into() }, parent: None };
        let items = classify(&ScriptDialect::tsql(), &spec, &view, "w", true, &r, defs, &Context::default());
        assert!(matches!(&items[0].action, Action::Rewrite { object, .. } if object.definition.ends_with("FROM dbo.w")), "{:?}", items[0].action);
        // No grants to lose: no warning about them.
        let mut d = fake(ReplaceStyle::DropCreate);
        d.spec.grants_on_objects = false;
        let s = build_script(&d, &d.spec, &request("Nuevo"), &[choice("v1", "CREATE VIEW dbo.v1 AS SELECT a FROM dbo.Nuevo", false)]).unwrap();
        assert_eq!(s.statements.len(), 3);
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
    }

    #[test]
    fn a_dependent_put_back_must_be_one_unit_on_every_engine() {
        // T-SQL: a GO line splits a batch.
        let d = fake(ReplaceStyle::CreateOrAlter);
        let go = choice("v1", "CREATE VIEW dbo.v1 AS SELECT a FROM dbo.Nuevo\nGO\nDROP TABLE dbo.t", false);
        assert!(matches!(build_script(&d, &d.spec, &request("Nuevo"), &[go]), Err(CommandError::BadRequest(m)) if m.contains("GO")));
        // Engines that split at `;`: a definition whose quoting closes early.
        let mut d = fake(ReplaceStyle::CreateOrReplace);
        d.info.dialect = "postgres";
        let two = choice("f", "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql; DROP TABLE t", false);
        assert!(matches!(build_script(&d, &d.spec, &request("Nuevo"), std::slice::from_ref(&two)), Err(CommandError::BadRequest(m)) if m.contains("2 sentencias")));
        // Carried ones too (they go and come back where views are dropped).
        let mut d = fake(ReplaceStyle::DropCreate);
        d.info.dialect = "postgres";
        let mut v = choice("v1", "CREATE VIEW v1 AS SELECT a FROM Nuevo", false);
        v.carried = vec![two.object.clone()];
        assert!(build_script(&d, &d.spec, &request("Nuevo"), &[v]).is_err());
        // One unit (a `;` inside the body, a trailing `;`) is fine.
        let one = choice("f", "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql;", false);
        assert!(build_script(&d, &d.spec, &request("Nuevo"), &[one]).is_ok());
        // Nothing to run is refused too.
        let empty = choice("v0", "-- nada", false);
        assert!(matches!(build_script(&d, &d.spec, &request("Nuevo"), &[empty]), Err(CommandError::BadRequest(m)) if m.contains("«v0»")));
    }

    /// The fake writes the new name as given, unescaped, like a driver that
    /// got its quoting wrong would.
    #[test]
    fn the_drivers_own_statements_are_one_unit_each() {
        // `;` engines: a name that closes the literal early.
        let mut d = fake(ReplaceStyle::CreateOrReplace);
        d.info.dialect = "postgres";
        let r = build_script(&d, &d.spec, &request("x'; DROP TABLE t; --"), &[]);
        assert!(matches!(&r, Err(CommandError::BadRequest(m)) if m.contains("«Clientes»") && m.contains("sentencias")), "{:?}", r.as_ref().err());
        // T-SQL: a GO line in what the driver wrote.
        let d = fake(ReplaceStyle::CreateOrAlter);
        let r = build_script(&d, &d.spec, &request("x'\nGO\nDROP TABLE t\n--"), &[]);
        assert!(matches!(&r, Err(CommandError::BadRequest(m)) if m.contains("«Clientes»") && m.contains("GO")), "{:?}", r.as_ref().err());
        // The epilogue too.
        let mut d = fake(ReplaceStyle::CreateOrReplace);
        d.info.dialect = "postgres";
        d.spec.epilogue = Some("CALL recompile({schema}); DROP TABLE t;".into());
        let r = build_script(&d, &d.spec, &request("Nuevo"), &[]);
        assert!(matches!(&r, Err(CommandError::BadRequest(m)) if m.contains("sentencia final")), "{:?}", r.as_ref().err());
        d.spec.epilogue = Some("CALL recompile({schema});".into());
        assert!(build_script(&d, &d.spec, &request("Nuevo"), &[]).is_ok());
    }

    /// A one-unit refusal (not another error of the driver's).
    fn split_refusal(r: &CommandResult<SyncScript>) -> Option<String> {
        match r {
            Err(CommandError::BadRequest(m)) if m.contains("al ejecutarla") || m.contains("ninguna sentencia") || m.contains("lotes") => Some(m.clone()),
            _ => None,
        }
    }

    /// Every shipped driver's own rename statements, for each target it
    /// takes, pass the check: none emits a statement its splitter cuts.
    #[test]
    fn no_driver_rename_is_refused_as_several_statements() {
        let obj = |kind: &str| ObjectRef { kind: kind.into(), schema: Some("ventas".into()), name: "Clientes".into() };
        let mut checked = 0;
        for d in dbine_drivers::all() {
            let Some(spec) = d.rename_spec() else { continue };
            let mut targets: Vec<RenameTarget> = spec.kinds.iter().map(|k| RenameTarget::Object { object: obj(k), parent: None }).collect();
            targets.push(RenameTarget::Column { table: obj("table"), column: "nombre".into() });
            targets.push(RenameTarget::Schema { database: None, schema: "ventas".into() });
            for target in targets.into_iter().filter(|t| spec.allows(t)) {
                for new_name in ["Nuevo", "nuevo nombre; con punto y coma", "it's"] {
                    let req = RenameRequest { target: target.clone(), new_name: new_name.into(), table: None, definition: None };
                    let r = build_script(d.as_ref(), &spec, &req, &[]);
                    assert!(split_refusal(&r).is_none(), "{} {target:?} {new_name}: {:?}", d.info().id, split_refusal(&r));
                    checked += r.is_ok() as usize;
                }
            }
        }
        assert!(checked > 50, "{checked}");
    }

    /// Typical bodies on each engine stay one statement: PostgreSQL `$$`,
    /// MySQL `BEGIN … END`, T-SQL modules, Oracle PL/SQL, ClickHouse views.
    #[test]
    fn typical_dependents_are_not_refused() {
        let cases: &[(&str, &str, &str)] = &[
            ("postgres", "function", "CREATE OR REPLACE FUNCTION public.total(p int)\n RETURNS integer\n LANGUAGE plpgsql\nAS $function$\nDECLARE n int;\nBEGIN\n  SELECT count(*) INTO n FROM public.\"Nuevo\" WHERE id = p;\n  RETURN n;\nEND;\n$function$\n"),
            ("postgres", "procedure", "CREATE OR REPLACE PROCEDURE public.limpiar()\n LANGUAGE sql\nBEGIN ATOMIC\n DELETE FROM public.\"Nuevo\" WHERE id < 0;\n UPDATE public.\"Nuevo\" SET id = 1;\nEND"),
            ("postgres", "view", " SELECT id\n   FROM public.\"Nuevo\";"),
            ("mysql", "procedure", "CREATE DEFINER=`root`@`%` PROCEDURE `limpiar`(IN p INT)\nBEGIN\n  DECLARE n INT;\n  SELECT COUNT(*) INTO n FROM `Nuevo` WHERE id = p;\n  IF n > 0 THEN\n    DELETE FROM `Nuevo` WHERE id = p;\n  END IF;\nEND"),
            ("mysql", "trigger", "CREATE DEFINER=`root`@`%` TRIGGER `tg` BEFORE INSERT ON `Nuevo` FOR EACH ROW BEGIN\n  SET NEW.a = 1;\n  SET NEW.b = 2;\nEND"),
            ("mysql", "view", "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select `Nuevo`.`id` AS `id` from `Nuevo`"),
            ("sqlserver", "procedure", "CREATE PROCEDURE dbo.limpiar @p int\nAS\nBEGIN\n  SET NOCOUNT ON;\n  DELETE FROM dbo.Nuevo WHERE id = @p;\n  SELECT 1;\nEND"),
            ("sqlserver", "function", "CREATE FUNCTION dbo.total() RETURNS int AS BEGIN DECLARE @n int; SELECT @n = COUNT(*) FROM dbo.Nuevo; RETURN @n; END"),
            ("sqlserver", "trigger", "CREATE TRIGGER dbo.tg ON dbo.Nuevo AFTER INSERT AS\n  UPDATE dbo.Nuevo SET a = 1;\n  DELETE FROM dbo.Log;"),
            ("oracle", "procedure", "CREATE OR REPLACE EDITIONABLE PROCEDURE \"VENTAS\".\"LIMPIAR\" (p IN NUMBER) AS\n  n NUMBER;\nBEGIN\n  SELECT COUNT(*) INTO n FROM ventas.nuevo WHERE id = p;\n  DELETE FROM ventas.nuevo WHERE id = p;\nEND;"),
            ("oracle", "package_body", "CREATE OR REPLACE PACKAGE BODY ventas.pk AS\n  PROCEDURE a IS BEGIN DELETE FROM ventas.nuevo; END;\n  FUNCTION b RETURN NUMBER IS BEGIN RETURN 1; END;\nEND pk;"),
            ("oracle", "trigger", "CREATE OR REPLACE TRIGGER ventas.tg BEFORE INSERT ON ventas.nuevo FOR EACH ROW\nBEGIN\n  :NEW.a := 1;\n  :NEW.b := 2;\nEND;"),
            ("oracle", "view", "CREATE OR REPLACE FORCE VIEW ventas.v AS SELECT id FROM ventas.nuevo"),
            ("clickhouse", "view", "CREATE VIEW ventas.v (`id` UInt64) AS SELECT id FROM ventas.Nuevo WHERE note != 'a;b'"),
            ("clickhouse", "materialized_view", "CREATE MATERIALIZED VIEW ventas.mv TO ventas.d (`id` UInt64) AS SELECT id FROM ventas.Nuevo"),
        ];
        for (id, kind, def) in cases {
            let d = dbine_drivers::find(id).unwrap_or_else(|| panic!("{id}"));
            let spec = d.rename_spec().unwrap();
            let target = RenameTarget::Object { object: ObjectRef { kind: "table".into(), schema: Some("ventas".into()), name: "Clientes".into() }, parent: None };
            let req = RenameRequest { target, new_name: "Nuevo".into(), table: None, definition: None };
            let o = CodeObject { kind: (*kind).into(), schema: Some("ventas".into()), name: "dep".into(), definition: (*def).into() };
            let r = build_script(d.as_ref(), &spec, &req, &[RewriteChoice { object: o, schemabound: false, carried: vec![] }]);
            assert!(r.is_ok(), "{id} {kind}: {:?}", r.err().map(|e| e.to_string()));
            // And a second statement hidden after it isn't.
            // (Oracle: a PL/SQL unit runs to its `/` line, `;` included.)
            let sep = match d.script_dialect().batch {
                dbine_driver::sql::BatchLine::Go => "\nGO\n",
                dbine_driver::sql::BatchLine::Slash => "\n/\n",
                dbine_driver::sql::BatchLine::None => ";\n",
            };
            let o = CodeObject { kind: (*kind).into(), schema: Some("ventas".into()), name: "dep".into(), definition: format!("{}{sep}DROP TABLE ventas.x", def.trim_end().trim_end_matches(';')) };
            let r = build_script(d.as_ref(), &spec, &req, &[RewriteChoice { object: o, schemabound: false, carried: vec![] }]);
            assert!(split_refusal(&r).is_some(), "{id} {kind}: {:?}", r.map(|s| s.statements));
        }
    }

    #[test]
    fn mysql_keeps_the_definer_it_had() {
        // Re-created with the DEFINER it was stored with, not the renamer's,
        // even when the definer is called like the renamed table.
        let d = dbine_drivers::find("mysql").unwrap();
        let spec = d.rename_spec().unwrap();
        let target = RenameTarget::Object { object: ObjectRef { kind: "table".into(), schema: None, name: "admin".into() }, parent: None };
        let r = report(vec![dependent("view", "v", Relation::Code, Confidence::Probable), dependent("procedure", "p", Relation::Code, Confidence::Probable)]);
        let defs = vec![
            Some("CREATE ALGORITHM=UNDEFINED DEFINER=`admin`@`%` SQL SECURITY DEFINER VIEW `v` AS select `admin`.`id` AS `id` from `admin`".into()),
            Some("CREATE DEFINER=`admin`@`%` PROCEDURE `p`()\nBEGIN\n  SELECT COUNT(*) FROM admin;\nEND".into()),
        ];
        let ctx = Context { database: Some("ventas".into()), ..Default::default() };
        // The definer called like the table stays; the table's name changes.
        let items = classify(&d.script_dialect(), &spec, &target, "cuentas", true, &r, defs, &ctx);
        for it in &items {
            let Action::Rewrite { object, .. } = &it.action else { panic!("{:?}", it.action) };
            assert!(object.definition.contains("DEFINER=`admin`@`%`") && object.definition.contains("cuentas"), "{}", object.definition);
        }
        // And if a rewrite ever changed it, the dependent would be left to the user.
        let dl = d.script_dialect();
        assert_eq!(definers("CREATE DEFINER=`admin`@`%` VIEW v AS SELECT 1", &dl), ["`admin`@`%`"]);
        assert_ne!(definers("CREATE DEFINER=`admin`@`%` VIEW v", &dl), definers("CREATE DEFINER=`cuentas`@`%` VIEW v", &dl));
        assert!(definers("CREATE VIEW v AS SELECT 1", &dl).is_empty());
        // Any other definer stays as stored, through the rewrite and the script.
        let defs = vec![
            Some("CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select `admin`.`id` AS `id` from `admin`".into()),
            Some("CREATE DEFINER=`app`@`localhost` PROCEDURE `p`()\nBEGIN\n  SELECT COUNT(*) FROM admin;\nEND".into()),
        ];
        let items = classify(&d.script_dialect(), &spec, &target, "cuentas", true, &r, defs, &ctx);
        let mut rewrites = Vec::new();
        for it in &items {
            let Action::Rewrite { object, .. } = &it.action else { panic!("{:?}", it.action) };
            assert!(object.definition.contains("DEFINER=`root`@`%`") || object.definition.contains("DEFINER=`app`@`localhost`"), "{}", object.definition);
            assert!(object.definition.contains("cuentas"), "{}", object.definition);
            rewrites.push(RewriteChoice { object: object.clone(), schemabound: false, carried: vec![] });
        }
        let req = RenameRequest { target, new_name: "cuentas".into(), table: None, definition: None };
        let s = build_script(d.as_ref(), &spec, &req, &rewrites).unwrap();
        let creates: Vec<&String> = s.statements.iter().filter(|x| x.starts_with("CREATE")).collect();
        assert_eq!(creates.len(), 2, "{:?}", s.statements);
        assert!(creates.iter().any(|c| c.contains("DEFINER=`root`@`%`")) && creates.iter().any(|c| c.contains("DEFINER=`app`@`localhost`")), "{creates:?}");
    }

    #[test]
    fn per_kind_styles_and_the_epilogue() {
        let mut d = fake(ReplaceStyle::DropCreate);
        d.spec.replace_kinds = [("view".to_string(), ReplaceStyle::CreateOrReplace)].into();
        d.spec.epilogue = Some("CALL recompile({schema});".into());
        let mut p = choice("p", "CREATE PROCEDURE dbo.p AS SELECT a FROM dbo.Nuevo", false);
        p.object.kind = "procedure".into();
        let rewrites = vec![choice("v1", "CREATE      VIEW dbo.v1 AS SELECT a FROM dbo.Nuevo", false), p];
        let s = build_script(&d, &d.spec, &request("Nuevo"), &rewrites).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP PROCEDURE IF EXISTS [dbo].[p];",
                "EXEC sp_rename N'dbo.Clientes', N'Nuevo';",
                "CREATE OR REPLACE VIEW dbo.v1 AS SELECT a FROM dbo.Nuevo",
                "CREATE PROCEDURE dbo.p AS SELECT a FROM dbo.Nuevo",
                "CALL recompile('dbo');",
            ]
        );
        assert!(s.warnings[0].contains("«p»") && !s.warnings[0].contains("«v1»"), "{:?}", s.warnings);
    }

    #[test]
    fn views_holding_rows_and_trigger_routines() {
        let spec = RenameSpec { holds_rows: vec!["materialized_view".into()], ..fake(ReplaceStyle::CreateOrReplace).spec };
        let r = report(vec![
            dependent("materialized_view", "mv_to", Relation::Code, Confidence::Probable),
            dependent("materialized_view", "mv_own", Relation::Code, Confidence::Probable),
            dependent("function", "f_rows", Relation::Code, Confidence::Probable),
            dependent("function", "f_shared", Relation::Code, Confidence::Probable),
        ]);
        let col = RenameTarget::Column { table: ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "t".into() }, column: "pepe".into() };
        let defs = vec![
            Some("CREATE MATERIALIZED VIEW dbo.mv_to TO dbo.d AS SELECT pepe FROM dbo.t".into()),
            Some("CREATE MATERIALIZED VIEW dbo.mv_own ENGINE = Memory AS SELECT pepe FROM dbo.t".into()),
            Some("CREATE FUNCTION dbo.f_rows() RETURNS trigger AS BEGIN NEW.pepe = 1; END".into()),
            Some("CREATE FUNCTION dbo.f_shared() RETURNS trigger AS BEGIN NEW.pepe = 1; END".into()),
        ];
        let ctx = Context { row_routines: vec![2, 3], shared_routines: vec![3], ..Default::default() };
        let items = classify(&ScriptDialect::generic(), &spec, &col, "nuevo", true, &r, defs, &ctx);
        let selected: Vec<Option<bool>> = items.iter().map(|i| if let Action::Rewrite { default_selected, .. } = i.action { Some(default_selected) } else { None }).collect();
        assert_eq!(selected, [Some(true), Some(false), Some(true), None]);
        assert_eq!(items[3].action, Action::Manual { reason: ManualReason::SharedRoutine, unresolved: vec![] });
        let Action::Rewrite { object, .. } = &items[2].action else { panic!() };
        assert!(object.definition.contains("NEW.nuevo = 1"), "{}", object.definition);
    }

    #[test]
    fn a_table_without_schema_is_the_databases() {
        assert!(same_schema(Some("db"), None, Some("db")));
        assert!(same_schema(None, None, Some("db")) && same_schema(Some(""), None, None));
        assert!(!same_schema(Some("otra"), None, Some("db")));
        assert!(!same_schema(Some("db"), None, None));
        assert!(same_schema(Some("dbo"), Some("dbo"), None) && !same_schema(None, Some("dbo"), None));
    }

    #[test]
    fn validation_and_specs() {
        assert!(validate(&table_target(), "  ").is_err());
        assert!(validate(&table_target(), "Clientes").is_err());
        assert_eq!(validate(&table_target(), " clientes ").unwrap(), "clientes");
        let d = fake(ReplaceStyle::DropCreate);
        assert!(spec_for(&d, &table_target()).is_ok());
        assert!(spec_for(&d, &RenameTarget::Schema { database: None, schema: "dbo".into() }).is_err());
        // A driver with no rename at all.
        let bare = dbine_drivers::find("postgres").unwrap();
        if bare.rename_spec().is_none() {
            assert!(spec_for(bare.as_ref(), &table_target()).is_err());
        }
    }

    #[test]
    fn collisions() {
        let objects = vec![dbine_driver::DbObject { kind: "view".into(), schema: Some("dbo".into()), name: "Nuevo".into(), parent: None }];
        assert!(collides(&table_target(), "nuevo", None, &objects));
        assert!(!collides(&table_target(), "otro", None, &objects));
        let t = TableSchema { name: "Clientes".into(), columns: vec![dbine_driver::ColumnDef { name: "b".into(), ..Default::default() }], ..Default::default() };
        let col = RenameTarget::Column { table: ObjectRef { kind: "table".into(), schema: None, name: "Clientes".into() }, column: "a".into() };
        assert!(collides(&col, "B", Some(&t), &[]));
    }
}

/// The app's own path (`classify`, `build_script` with its one-unit checks,
/// then each statement through `execute` as `schema_sync_run` does) on real
/// servers, with typical bodies: nothing is refused that should run.
/// `DBINE_TEST_POSTGRES_URL`, `DBINE_TEST_MYSQL_URL`,
/// `DBINE_TEST_SQLSERVER_URL` as in the drivers' tests; `cargo test -p dbine
/// --lib rename::live -- --ignored --test-threads=1`.
#[cfg(test)]
mod live {
    use super::*;
    use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

    fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
        let url = std::env::var(env).ok()?;
        let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
        let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
        let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
        let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
        let (host, port) = hostport.rsplit_once(':')?;
        Some(ConnectionConfig {
            driver: driver.into(),
            host: host.into(),
            port: port.parse().ok()?,
            database: db.into(),
            username: (!user.is_empty()).then(|| user.into()),
            password: pass.map(Into::into),
            ..Default::default()
        })
    }

    async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
        let mut out = QueryOutcome::default();
        s.execute(sql, 100, &mut out).await.map_err(|e| e.to_string())?;
        match out.error.take() {
            Some(e) => Err(format!("{e:?}")),
            None => Ok(out),
        }
    }

    async fn ok(s: &mut Box<dyn Session>, sql: &str) {
        run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// Renames `table` to `new_name` the way the dialog does and runs it;
    /// returns the dependents DBine put back.
    async fn rename(driver: &dyn Driver, s: &mut Box<dyn Session>, table: ObjectRef, new_name: &str, database: Option<String>) -> Vec<String> {
        let spec = driver.rename_spec().unwrap();
        let dialect = driver.script_dialect();
        let target = RenameTarget::Object { object: table, parent: None };
        let scan = DependencyScan::new(driver.info(), dialect, driver.capabilities().foreign_keys);
        let report = s.dependents(&target.dependency_target(), &scan).await.unwrap();
        let mut defs = Vec::new();
        for d in &report.items {
            let wanted = d.relation == Relation::Code && d.confidence != Confidence::Review && !spec.tracked_for_target(&target).contains(&d.kind);
            defs.push(if wanted { s.definition(&ObjectRef { kind: d.kind.clone(), schema: d.schema.clone(), name: d.name.clone() }).await.unwrap() } else { None });
        }
        let ctx = Context { database, ..Default::default() };
        let items = classify(&dialect, &spec, &target, new_name, true, &report, defs, &ctx);
        eprintln!("{}: {:?}", driver.info().id, items.iter().map(|i| (&i.dependent.name, &i.action)).map(|(n, a)| format!("{n}: {}", serde_json::to_string(a).unwrap_or_default().chars().take(60).collect::<String>())).collect::<Vec<_>>());
        let rewrites: Vec<RewriteChoice> = items
            .iter()
            .filter_map(|i| match &i.action {
                Action::Rewrite { object, schemabound, carried, .. } => Some(RewriteChoice { object: object.clone(), schemabound: *schemabound, carried: carried.clone() }),
                _ => None,
            })
            .collect();
        let req = RenameRequest { target, new_name: new_name.into(), table: None, definition: None };
        let script = build_script(driver, &spec, &req, &rewrites).unwrap_or_else(|e| panic!("{}: refused: {e}", driver.info().id));
        for st in &script.statements {
            eprintln!("> {st}");
            ok(s, st).await;
        }
        rewrites.into_iter().map(|r| r.object.name).collect()
    }

    #[tokio::test]
    #[ignore]
    async fn postgres_typical_bodies_pass() {
        let Some(c) = cfg("postgres", "DBINE_TEST_POSTGRES_URL") else { return eprintln!("skipping") };
        let d = dbine_drivers::find("postgres").unwrap();
        let mut s = d.connect(&c, None).await.unwrap();
        for sql in [
            "DROP SCHEMA IF EXISTS rn_live CASCADE",
            "CREATE SCHEMA rn_live",
            "CREATE TABLE rn_live.clientes (id int, nombre text)",
            "INSERT INTO rn_live.clientes VALUES (1, 'a'), (2, 'b;c')",
            "CREATE VIEW rn_live.v AS SELECT id, nombre FROM rn_live.clientes",
            "CREATE FUNCTION rn_live.total(p int) RETURNS int LANGUAGE plpgsql AS $fn$\nDECLARE n int;\nBEGIN\n  SELECT count(*) INTO n FROM rn_live.clientes WHERE id >= p; -- ; in a comment\n  RETURN n;\nEND;\n$fn$",
            "CREATE FUNCTION rn_live.nombres() RETURNS text LANGUAGE sql AS $$ SELECT string_agg(nombre, ';') FROM rn_live.clientes $$",
            "CREATE PROCEDURE rn_live.limpiar() LANGUAGE plpgsql AS $$ BEGIN DELETE FROM rn_live.clientes WHERE id < 0; UPDATE rn_live.clientes SET id = id WHERE false; END $$",
        ] {
            ok(&mut s, sql).await;
        }
        let put_back = rename(d.as_ref(), &mut s, ObjectRef { kind: "table".into(), schema: Some("rn_live".into()), name: "clientes".into() }, "cuentas", None).await;
        assert!(put_back.len() >= 3, "{put_back:?}");
        assert_eq!(run(&mut s, "SELECT rn_live.total(1)").await.unwrap().results[0].rows[0][0].to_string().trim_matches('"'), "2");
        ok(&mut s, "SELECT rn_live.nombres()").await;
        ok(&mut s, "CALL rn_live.limpiar()").await;
        ok(&mut s, "DROP SCHEMA rn_live CASCADE").await;
    }

    #[tokio::test]
    #[ignore]
    async fn mysql_typical_bodies_pass() {
        let Some(c) = cfg("mysql", "DBINE_TEST_MYSQL_URL") else { return eprintln!("skipping") };
        let d = dbine_drivers::find("mysql").unwrap();
        let mut root = d.connect(&c, None).await.unwrap();
        ok(&mut root, "DROP DATABASE IF EXISTS rn_live").await;
        ok(&mut root, "CREATE DATABASE rn_live").await;
        let mut s = d.connect(&c, Some("rn_live")).await.unwrap();
        for sql in [
            "CREATE TABLE clientes (id int, nombre varchar(20))",
            "CREATE TABLE log (id int)",
            "INSERT INTO clientes VALUES (1, 'a'), (2, 'b;c')",
            "CREATE VIEW v AS SELECT id, nombre FROM clientes",
            "CREATE PROCEDURE limpiar(IN p INT)\nBEGIN\n  DECLARE n INT;\n  SELECT COUNT(*) INTO n FROM clientes WHERE id = p;\n  IF n > 0 THEN\n    DELETE FROM clientes WHERE id = p AND nombre = 'x;y';\n  END IF;\nEND",
            "CREATE FUNCTION total() RETURNS INT READS SQL DATA\nBEGIN\n  DECLARE n INT;\n  SELECT COUNT(*) INTO n FROM clientes;\n  RETURN n;\nEND",
            "CREATE TRIGGER tg AFTER INSERT ON log FOR EACH ROW\nBEGIN\n  INSERT INTO clientes VALUES (NEW.id, 't');\n  UPDATE clientes SET nombre = 'u' WHERE id = NEW.id;\nEND",
        ] {
            ok(&mut s, sql).await;
        }
        let put_back = rename(d.as_ref(), &mut s, ObjectRef { kind: "table".into(), schema: None, name: "clientes".into() }, "cuentas", Some("rn_live".into())).await;
        assert!(put_back.len() >= 4, "{put_back:?}");
        assert_eq!(run(&mut s, "SELECT total()").await.unwrap().results[0].rows[0][0].to_string().trim_matches('"'), "2");
        ok(&mut s, "CALL limpiar(1)").await;
        ok(&mut s, "INSERT INTO log VALUES (9)").await;
        ok(&mut s, "SELECT * FROM v").await;
        ok(&mut root, "DROP DATABASE rn_live").await;
    }

    #[tokio::test]
    #[ignore]
    async fn sqlserver_typical_bodies_pass() {
        let Some(c) = cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL") else { return eprintln!("skipping") };
        let d = dbine_drivers::find("sqlserver").unwrap();
        let mut root = d.connect(&c, None).await.unwrap();
        ok(&mut root, "IF DB_ID('rn_live') IS NOT NULL BEGIN ALTER DATABASE rn_live SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE rn_live; END").await;
        ok(&mut root, "CREATE DATABASE rn_live").await;
        let mut s = d.connect(&c, Some("rn_live")).await.unwrap();
        for sql in [
            "CREATE TABLE dbo.clientes (id int, nombre nvarchar(20))",
            "CREATE TABLE dbo.log (id int)",
            "INSERT INTO dbo.clientes VALUES (1, N'a'), (2, N'b;c')",
            "CREATE VIEW dbo.v AS SELECT id, nombre FROM dbo.clientes",
            "CREATE PROCEDURE dbo.limpiar @p int\nAS\nBEGIN\n  SET NOCOUNT ON;\n  DELETE FROM dbo.clientes WHERE id = @p AND nombre = N'x;y';\n  SELECT COUNT(*) FROM dbo.clientes;\nEND",
            "CREATE FUNCTION dbo.total() RETURNS int AS BEGIN DECLARE @n int; SELECT @n = COUNT(*) FROM dbo.clientes; RETURN @n; END",
            "CREATE TRIGGER dbo.tg ON dbo.log AFTER INSERT AS\n  INSERT INTO dbo.clientes SELECT id, N't' FROM inserted;\n  UPDATE dbo.clientes SET nombre = N'u' WHERE id IN (SELECT id FROM inserted);",
        ] {
            ok(&mut s, sql).await;
        }
        let put_back = rename(d.as_ref(), &mut s, ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "clientes".into() }, "cuentas", None).await;
        assert!(put_back.len() >= 4, "{put_back:?}");
        assert_eq!(run(&mut s, "SELECT dbo.total()").await.unwrap().results[0].rows[0][0].to_string().trim_matches('"'), "2");
        ok(&mut s, "EXEC dbo.limpiar 1").await;
        ok(&mut s, "INSERT INTO dbo.log VALUES (9)").await;
        ok(&mut s, "SELECT * FROM dbo.v").await;
        drop(s);
        ok(&mut root, "ALTER DATABASE rn_live SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE rn_live").await;
    }
}
