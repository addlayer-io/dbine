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
use dbine_driver::rename::{names_in_code, quote_new, rewrite_references, trigger_routine, with_create_style, writes_to_table, Edit, RewriteOptions, Unresolved};
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
    /// left out (`unresolved`).
    Rewrite { object: CodeObject, edits: Vec<Edit>, unresolved: Vec<Unresolved>, schemabound: bool, default_selected: bool },
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
    let tracked = spec.tracked.clone();
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
                    let wanted = d.relation == Relation::Code && d.confidence != Confidence::Review && !tracked.contains(&d.kind);
                    defs.push(if wanted { s.definition(&ObjectRef { kind: d.kind.clone(), schema: d.schema.clone(), name: d.name.clone() }).await.ok().flatten() } else { None });
                }
                let definition = match &target {
                    RenameTarget::Object { object, .. } if object.kind != kinds::TABLE => s.definition(object).await.ok().flatten(),
                    _ => None,
                };
                let table = match target.table() {
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
    let items = classify(&dialect, &spec, &args.target, &new_name, args.keep_view_columns, &report, defs, &ctx)
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !unrelated.contains(i))
        .map(|(_, item)| item)
        .collect();
    Ok(RenameImpact {
        items,
        scanned: report.scanned,
        unreadable: report.unreadable,
        note: report.note,
        spec_note: spec.note.clone(),
        collides,
        quoted_name: quote_new(&new_name, &dialect, spec.fold, false),
        atomic: spec.transactional && driver.supports_manual_transactions(),
        definition,
        table,
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
    let column = matches!(target, RenameTarget::Column { .. });
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
            } else if spec.tracked.contains(&d.kind) {
                Action::Tracked
            } else if d.confidence == Confidence::Review {
                manual(ManualReason::Dynamic)
            } else if spec.references == ReferenceStyle::None || (column && spec.references == ReferenceStyle::Pipeline) {
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

pub(crate) fn build_script(driver: &dyn Driver, spec: &RenameSpec, request: &RenameRequest, rewrites: &[RewriteChoice]) -> CommandResult<SyncScript> {
    let middle = driver.rename_script(request)?;
    let dialect = driver.script_dialect();
    let mut lost = Vec::new();
    let objects: Vec<ObjectChange> = rewrites
        .iter()
        .map(|r| {
            let style = spec.replace_for(&r.object.kind);
            let object = CodeObject { definition: with_create_style(&r.object.definition, &dialect, style), ..r.object.clone() };
            if style == ReplaceStyle::DropCreate || r.schemabound {
                lost.push(format!("«{}»", r.object.name));
                ObjectChange::Replace { object }
            } else {
                ObjectChange::Create { object }
            }
        })
        .collect();
    let mut script = plan_around(driver, middle, &objects);
    script.statements.extend(spec.epilogue_for(&request.target));
    if !lost.is_empty() {
        script.warnings.push(format!("Se borran y se vuelven a crear {}: se pierden los permisos otorgados sobre ellos.", lost.join(", ")));
    }
    Ok(script)
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
        RewriteChoice { object: CodeObject { kind: "view".into(), schema: Some("dbo".into()), name: name.into(), definition: def.into() }, schemabound }
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
