//! The step kinds. Each reads its config (JSON), does its work with the
//! same code the app uses for it, and returns what it did. Nothing here
//! records rows or secrets: summaries carry counts, files and messages.

use super::{Ctx, StepDone, Target, SCRIPT_MAX_ROWS};
use crate::commands::compare::{self, LoadArgs, ObjectChange};
use crate::error::{CommandError, CommandResult};
use dbine_core::export::{ExportOptions, Exporter, Format};
use dbine_core::tasks::{expand, kinds, Step};
use dbine_driver::{BackupAction, ObjectRef, QueryOutcome, RowSinkRef, TableChange};
use dbine_schema::compare::{CompareOptions, Status};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Vars = BTreeMap<String, String>;

pub(super) async fn run(ctx: &Ctx<'_>, step: &Step, vars: &Vars) -> CommandResult<StepDone> {
    match step.kind.as_str() {
        kinds::RUN_SCRIPT => run_script(ctx, step, vars).await,
        kinds::EXPORT => export(ctx, step, vars).await,
        kinds::COMPARE_SCHEMAS => compare_schemas(ctx, step, vars).await,
        kinds::BACKUP => backup(ctx, step, vars).await,
        kinds::DOCUMENT => document(ctx, step, vars).await,
        kinds::SEND_MAIL => super::mail::step(ctx.state, step, vars).await,
        other => Err(CommandError::BadRequest(format!("esta versión de DBine no sabe ejecutar pasos «{other}»"))),
    }
}

fn config<T: for<'de> Deserialize<'de>>(step: &Step) -> CommandResult<T> {
    serde_json::from_value(step.config.clone()).map_err(|e| CommandError::BadRequest(format!("la configuración del paso no es válida: {e}")))
}

fn need_connection(t: &Target) -> CommandResult<()> {
    if t.connection_id.is_empty() {
        return Err(CommandError::BadRequest("el paso no tiene conexión".into()));
    }
    Ok(())
}

/// `folder/name` with the variables expanded, the folder created and the
/// name made safe for every OS.
fn out_path(folder: &str, name: &str, ext: &str, vars: &Vars) -> CommandResult<PathBuf> {
    let folder = expand(folder.trim(), vars);
    if folder.is_empty() {
        return Err(CommandError::BadRequest("el paso no tiene carpeta de destino".into()));
    }
    let dir = PathBuf::from(folder);
    std::fs::create_dir_all(&dir).map_err(|e| CommandError::BadRequest(format!("no se pudo crear la carpeta {}: {e}", dir.display())))?;
    let name = expand(if name.trim().is_empty() { "{task}-{datetime}" } else { name.trim() }, vars);
    let mut safe: String = name
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let dotted = format!(".{ext}");
    if !safe.to_lowercase().ends_with(&dotted) {
        safe.push_str(&dotted);
    }
    Ok(dir.join(safe))
}

fn shown(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// -- run a script ------------------------------------------------------------

#[derive(Deserialize)]
struct ScriptConfig {
    #[serde(flatten)]
    target: Target,
    sql: String,
    #[serde(default)]
    continue_on_error: Option<bool>,
}

async fn run_script(ctx: &Ctx<'_>, step: &Step, vars: &Vars) -> CommandResult<StepDone> {
    let c: ScriptConfig = config(step)?;
    need_connection(&c.target)?;
    let sql = expand(&c.sql, vars);
    if sql.trim().is_empty() {
        return Err(CommandError::BadRequest("el script está vacío".into()));
    }
    let key = format!("{}:{}", ctx.run_key, step.id);
    let out = crate::commands::query::run_unattended(ctx.state, &key, &c.target.connection_id, &c.target.database, &sql, c.continue_on_error, SCRIPT_MAX_ROWS).await?;
    let affected: u64 = out.results.iter().filter_map(|r| r.rows_affected).sum();
    let statements = out.results.len() as u64;
    let mut done = StepDone::default();
    done.outputs.insert("statements".into(), statements.to_string());
    done.outputs.insert("affected".into(), affected.to_string());
    done.outputs.insert("errors".into(), out.errors.len().to_string());
    // Server messages (PRINT, notices) and errors: no result rows.
    done.messages = out.log.iter().map(|m| m.text.clone()).chain(out.errors.iter().map(error_line)).take(200).collect();
    if let Some(first) = out.errors.first() {
        return Err(CommandError::Sql(format!("{} ({} en total)", error_line(first), plural(out.errors.len() as u64, "error", "errores"))));
    }
    done.summary = format!("{}; {} afectadas.", plural(statements, "resultado", "resultados"), plural(affected, "fila", "filas"));
    Ok(done)
}

fn error_line(e: &dbine_driver::ScriptError) -> String {
    match e.line {
        Some(l) => format!("línea {l}: {}", e.message),
        None => e.message.clone(),
    }
}

// -- export to a file --------------------------------------------------------

#[derive(Deserialize)]
struct ExportConfig {
    #[serde(flatten)]
    target: Target,
    sql: String,
    folder: String,
    #[serde(default)]
    file_name: String,
    #[serde(default)]
    result_index: usize,
    /// `format`, `header`, `delimiter`… as the export dialog has them.
    #[serde(default)]
    options: Value,
}

fn extension(f: Format) -> &'static str {
    match f {
        Format::Json => "json",
        Format::JsonLines => "jsonl",
        Format::Sql => "sql",
        Format::Csv | Format::CsvSemicolon | Format::CsvExcel => "csv",
        Format::Tsv => "tsv",
        Format::Xlsx => "xlsx",
        Format::Xml => "xml",
    }
}

async fn export(ctx: &Ctx<'_>, step: &Step, vars: &Vars) -> CommandResult<StepDone> {
    let c: ExportConfig = config(step)?;
    need_connection(&c.target)?;
    let sql = expand(&c.sql, vars);
    if sql.trim().is_empty() {
        return Err(CommandError::BadRequest("la consulta está vacía".into()));
    }
    let mut options: ExportOptions = if c.options.is_null() {
        ExportOptions::default()
    } else {
        serde_json::from_value(c.options.clone()).map_err(|e| CommandError::BadRequest(format!("opciones de exportación no válidas: {e}")))?
    };
    // String literals of an SQL export follow the source engine's escaping.
    options.backslash_escapes =
        crate::commands::schema::driver_of(ctx.state, &c.target.connection_id).is_ok_and(|d| d.script_dialect().backslash_escapes);
    let path = out_path(&c.folder, &c.file_name, extension(options.format), vars)?;
    let key = format!("{}:{}", ctx.run_key, step.id);
    // Reading only, whatever the connection says.
    let entry = ctx.state.dedicated_session(&key, &c.target.connection_id, &c.target.database, true).await?;
    let exporter = Arc::new(Mutex::new(Exporter::new(&path, c.result_index, options)));
    let mut out = QueryOutcome { sink: Some(RowSinkRef(exporter.clone())), ..Default::default() };
    let finished = {
        let mut s = entry.session.lock().await;
        tokio::select! {
            r = s.execute(&sql, usize::MAX, &mut out) => Some(r),
            _ = entry.cancel.notified() => None,
        }
    };
    ctx.state.sessions.remove(&key);
    out.sink = None;
    let failure = match finished {
        None => Some("Exportación cancelada.".to_string()),
        Some(Err(e)) => Some(e.to_string()),
        Some(Ok(())) => out
            .sink_error
            .clone()
            .map(|e| format!("no se pudo escribir el archivo: {e}"))
            .or_else(|| (out.results.len() <= c.result_index).then(|| "la consulta no devolvió ese resultado".to_string()))
            .or_else(|| out.results[c.result_index].columns.is_empty().then(|| "ese resultado no tiene filas para exportar".to_string())),
    };
    let rows = exporter.lock().map_err(|_| CommandError::Internal("exportador".into()))?.finish();
    if let Some(msg) = failure {
        let _ = std::fs::remove_file(&path);
        return Err(CommandError::Sql(msg));
    }
    let rows = rows.map_err(|e| CommandError::Internal(format!("no se pudo cerrar el archivo: {e}")))?;
    let mut done = StepDone { summary: format!("{} exportadas a {}", plural(rows, "fila", "filas"), shown(&path)), ..Default::default() };
    done.outputs.insert("file".into(), shown(&path));
    done.outputs.insert("rows".into(), rows.to_string());
    Ok(done)
}

// -- compare schemas ---------------------------------------------------------

#[derive(Deserialize)]
struct Side {
    connection_id: String,
    #[serde(default)]
    database: String,
    #[serde(default)]
    schemas: Vec<String>,
}

#[derive(Deserialize)]
struct CompareConfig {
    /// The reference: the script makes `target` like it.
    source: Side,
    target: Side,
    #[serde(default)]
    options: CompareOptions,
    /// The script also drops what only the target has.
    #[serde(default)]
    include_drops: bool,
    folder: String,
    #[serde(default)]
    file_name: String,
}

async fn compare_schemas(ctx: &Ctx<'_>, step: &Step, vars: &Vars) -> CommandResult<StepDone> {
    let c: CompareConfig = config(step)?;
    if c.source.connection_id.is_empty() || c.target.connection_id.is_empty() {
        return Err(CommandError::BadRequest("el paso necesita las dos bases".into()));
    }
    let load = |s: &Side| LoadArgs { connection_id: s.connection_id.clone(), database: s.database.clone(), schemas: s.schemas.clone() };
    let left = compare::load_model(ctx.state, load(&c.source)).await?;
    let right = compare::load_model(ctx.state, load(&c.target)).await?;
    let result = dbine_schema::compare::compare(&left.model, &right.model, &c.options);

    // The target's schema for what's carried over when pairing ignores it.
    let target_schema = right.model.tables.iter().find_map(|t| t.schema.clone()).or_else(|| c.target.schemas.first().cloned());
    let retarget = |mut t: dbine_driver::TableSchema, like: Option<&dbine_driver::TableSchema>| {
        if c.options.ignore_schema {
            t.schema = like.map_or(target_schema.clone(), |l| l.schema.clone());
        }
        t
    };
    let mut tables = Vec::new();
    let mut only_target = 0u64;
    for d in &result.tables {
        let l = d.left.and_then(|i| left.model.tables.get(i));
        let r = d.right.and_then(|i| right.model.tables.get(i));
        match (d.status, l, r) {
            (Status::Changed, Some(l), Some(r)) => tables.push(TableChange::Alter { old: r.clone(), new: retarget(l.clone(), Some(r)) }),
            (Status::OnlyLeft, Some(l), _) => tables.push(TableChange::Create { table: retarget(l.clone(), None) }),
            (Status::OnlyRight, _, Some(r)) => {
                only_target += 1;
                if c.include_drops {
                    tables.push(TableChange::Drop { table: r.clone() });
                }
            }
            _ => {}
        }
    }
    let mut objects = Vec::new();
    for d in &result.objects {
        let l = d.left.and_then(|i| left.model.objects.get(i));
        let r = d.right.and_then(|i| right.model.objects.get(i));
        let carried = |o: &dbine_schema::compare::CodeObject, like: Option<&dbine_schema::compare::CodeObject>| {
            let mut o = o.clone();
            if c.options.ignore_schema {
                o.schema = like.map_or(target_schema.clone(), |l| l.schema.clone());
            }
            o
        };
        match (d.status, l, r) {
            (Status::Changed, Some(l), Some(r)) => objects.push(ObjectChange::Replace { object: carried(l, Some(r)) }),
            (Status::OnlyLeft, Some(l), _) => objects.push(ObjectChange::Create { object: carried(l, None) }),
            (Status::OnlyRight, _, Some(r)) => {
                only_target += 1;
                if c.include_drops {
                    objects.push(ObjectChange::Drop { object: r.clone() });
                }
            }
            _ => {}
        }
    }
    let differences = result.tables.iter().filter(|t| t.status != Status::Equal).count() + result.objects.iter().filter(|o| o.status != Status::Equal).count();
    let mut done = StepDone::default();
    done.outputs.insert("differences".into(), differences.to_string());
    done.messages = left.warnings.iter().chain(&right.warnings).cloned().collect();
    let names = |s: &Side| if s.database.is_empty() { s.connection_id.clone() } else { s.database.clone() };
    if differences == 0 {
        done.summary = "Las bases tienen el mismo esquema.".into();
        return Ok(done);
    }
    let views = right.model.objects.iter().filter(|o| o.kind == dbine_driver::kinds::VIEW).cloned().collect();
    let script = compare::sync_script(
        ctx.state,
        compare::ScriptArgs { connection_id: c.target.connection_id.clone(), tables, objects, views },
    )?;
    let driver = crate::commands::schema::driver_of(ctx.state, &c.target.connection_id)?;
    let sep = driver.script_separator();
    let mut body = String::new();
    for w in &script.warnings {
        body.push_str(&format!("-- {w}\n"));
    }
    if !c.include_drops && only_target > 0 {
        body.push_str(&format!("-- {} solo en el destino: no se borran (opción «Incluir borrados»).\n", plural(only_target, "objeto está", "objetos están")));
    }
    body.push('\n');
    for st in &script.statements {
        let t = st.trim_end();
        if sep.is_empty() {
            body.push_str(t);
            body.push_str(if t.ends_with(';') { "\n\n" } else { ";\n\n" });
        } else {
            body.push_str(&format!("{t}\n{sep}\n\n"));
        }
    }
    let path = out_path(&c.folder, &c.file_name, "sql", vars)?;
    std::fs::write(&path, body).map_err(|e| CommandError::Internal(format!("no se pudo escribir {}: {e}", path.display())))?;
    let alert = format!("«{}» y «{}» difieren en {}.", names(&c.source), names(&c.target), plural(differences as u64, "objeto", "objetos"));
    done.summary = format!("{alert} Script de sincronización: {}", shown(&path));
    done.alert = Some(alert);
    done.outputs.insert("file".into(), shown(&path));
    Ok(done)
}

// -- backup ------------------------------------------------------------------

#[derive(Deserialize)]
struct BackupConfig {
    #[serde(flatten)]
    target: Target,
    /// "native": the engine's own backup; "copy": DBine's copy (a script
    /// with the structure and, with `data`, the rows).
    #[serde(default = "native")]
    mode: String,
    #[serde(default)]
    options: BTreeMap<String, String>,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    file_name: String,
    #[serde(default = "yes")]
    data: bool,
}

fn native() -> String {
    "native".into()
}

fn yes() -> bool {
    true
}

async fn backup(ctx: &Ctx<'_>, step: &Step, vars: &Vars) -> CommandResult<StepDone> {
    let mut c: BackupConfig = config(step)?;
    need_connection(&c.target)?;
    let driver = crate::commands::schema::driver_of(ctx.state, &c.target.connection_id)?;
    let spec = driver.backup();
    if c.mode == "native" {
        let Some(spec) = spec else {
            return Err(CommandError::BadRequest("este motor no tiene backups propios: elegí «Copia de DBine» en el paso".into()));
        };
        // Secret options (an encryption password) come from the vault.
        if let Ok(Some(json)) = dbine_core::secrets::get_raw(&super::step_secret_name(&ctx.task.id, &step.id)) {
            if let Ok(secret) = serde_json::from_str::<BTreeMap<String, String>>(&json) {
                c.options.extend(secret);
            }
        }
        for v in c.options.values_mut() {
            *v = expand(v, vars);
        }
        let database = (!spec.server_wide).then(|| c.target.database.clone()).filter(|d| !d.is_empty());
        let script = driver.backup_script(&BackupAction::Backup { database, options: c.options })?;
        let run_on = if spec.script_database.is_empty() { c.target.database.clone() } else { spec.script_database.to_string() };
        let key = format!("{}:{}", ctx.run_key, step.id);
        let out = crate::commands::query::run_unattended(ctx.state, &key, &c.target.connection_id, &run_on, &script, Some(false), 10).await?;
        if let Some(e) = out.errors.first() {
            return Err(CommandError::Sql(error_line(e)));
        }
        let mut done = StepDone { summary: "Backup del motor terminado.".into(), ..Default::default() };
        done.messages = out.log.iter().map(|m| m.text.clone()).take(50).collect();
        return Ok(done);
    }
    // DBine's copy: every object of the database.
    let ext = match driver.info().language {
        dbine_driver::Language::Sql => "sql",
        dbine_driver::Language::Cql => "cql",
        dbine_driver::Language::Json => "js",
        _ => "txt",
    };
    let path = out_path(&c.folder, &c.file_name, ext, vars)?;
    let objects: Vec<ObjectRef> = ctx
        .state
        .meta_read(&c.target.connection_id, &c.target.database, crate::commands::explorer::META_LIMIT, |s| {
            Box::pin(async move { s.list_objects().await })
        })
        .await?
        .into_iter()
        .map(|o| ObjectRef { kind: o.kind, schema: o.schema, name: o.name })
        .collect();
    if objects.is_empty() {
        return Err(CommandError::BadRequest("la base no tiene objetos para copiar".into()));
    }
    let copy = crate::commands::backup::make_copy(
        ctx.state,
        ctx.app,
        crate::commands::backup::CopyArgs {
            backup_id: format!("{}-{}", ctx.run_key.replace(':', "-"), step.id),
            connection_id: c.target.connection_id.clone(),
            database: c.target.database.clone(),
            objects,
            data: c.data,
            path: shown(&path),
        },
    )
    .await?;
    let mut done = StepDone {
        summary: format!("Copia de {} y {} en {}", plural(copy.objects, "objeto", "objetos"), plural(copy.rows, "fila", "filas"), copy.path),
        ..Default::default()
    };
    done.outputs.insert("file".into(), copy.path);
    done.outputs.insert("rows".into(), copy.rows.to_string());
    Ok(done)
}

// -- document the database ---------------------------------------------------

#[derive(Deserialize)]
struct DocumentConfig {
    #[serde(flatten)]
    target: Target,
    folder: String,
    #[serde(default)]
    file_name: String,
    /// Format, schemas and parts, as the "Documentar la base" dialog has them.
    #[serde(default)]
    options: crate::dbdocs::DocOptions,
}

async fn document(ctx: &Ctx<'_>, step: &Step, vars: &Vars) -> CommandResult<StepDone> {
    let c: DocumentConfig = config(step)?;
    need_connection(&c.target)?;
    let path = out_path(&c.folder, &c.file_name, c.options.format.extension(), vars)?;
    let key = format!("{}:{}", ctx.run_key, step.id);
    let built = crate::commands::dbdocs::document(ctx.state, &key, &c.target.connection_id, &c.target.database, &c.options, &|_, _, _| {}).await?;
    std::fs::write(&path, &built.text).map_err(|e| CommandError::Internal(format!("no se pudo escribir {}: {e}", path.display())))?;
    let mut done = StepDone {
        summary: format!("{} y {} documentados en {}", plural(built.tables as u64, "tabla", "tablas"), plural(built.objects as u64, "objeto", "objetos"), shown(&path)),
        messages: built.notes,
        ..Default::default()
    };
    done.outputs.insert("file".into(), shown(&path));
    done.outputs.insert("tables".into(), built.tables.to_string());
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_paths() {
        let dir = std::env::temp_dir().join(format!("dbine-task-paths-{}", std::process::id()));
        let mut v = Vars::new();
        v.insert("task".into(), "ventas/día".into());
        v.insert("date".into(), "2026-10-08".into());
        let p = out_path(&dir.to_string_lossy(), "{task}_{date}", "csv", &v).unwrap();
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "ventas_día_2026-10-08.csv");
        let p = out_path(&dir.to_string_lossy(), "reporte.CSV", "csv", &v).unwrap();
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "reporte.CSV");
        assert!(out_path("", "x", "csv", &v).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
