//! Database scripts: generate one from the database (structure, code and
//! data, into the editor or a file) and run a script file (restore a dump).
//! See docs/api-comandos.md.

use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::{AppState, SessionEntry};
use dbine_driver::{kinds, DdlParts, Driver, Language, ObjectRef, QueryOutcome, ResultColumn, RowSink, RowSinkRef, TableSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};

/// Text for the editor stops here (bigger scripts go to a file).
const EDITOR_LIMIT: usize = 20 * 1024 * 1024;
/// Rows per INSERT statement block in data scripts.
const DATA_BATCH: usize = 500;

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(default)]
pub struct ScriptOptions {
    pub drop: bool,
    pub if_exists: bool,
    pub create: bool,
    pub indexes: bool,
    pub foreign_keys: bool,
    /// Views, routines, triggers… (objects that aren't tables).
    pub definitions: bool,
    pub data: bool,
    pub data_limit: Option<u64>,
}

#[derive(Deserialize)]
pub struct GenerateArgs {
    pub script_id: String,
    pub connection_id: String,
    pub database: String,
    pub objects: Vec<ObjectRef>,
    pub options: ScriptOptions,
    /// Write to this file; `None` returns the text.
    pub path: Option<String>,
    /// Write it for another engine (tables converted with `dbine-schema`);
    /// `None` or the source's own = the same engine.
    #[serde(default)]
    pub target_driver: Option<String>,
}

#[derive(Serialize)]
pub struct GenerateResult {
    pub script: Option<String>,
    pub objects: usize,
    pub rows: u64,
}

#[derive(Clone, Serialize)]
struct ScriptProgress {
    id: String,
    done: usize,
    total: usize,
    current: String,
}

/// Where the script goes: a file, or text for the editor (capped).
enum Out {
    File(std::io::BufWriter<std::fs::File>),
    Mem(String),
}

impl Out {
    fn write(&mut self, s: &str) -> std::io::Result<()> {
        match self {
            Out::File(f) => f.write_all(s.as_bytes()),
            Out::Mem(m) => {
                if m.len() + s.len() > EDITOR_LIMIT {
                    return Err(std::io::Error::other(
                        "el script supera los 20 MB: generalo a un archivo (\"Guardar en archivo…\")",
                    ));
                }
                m.push_str(s);
                Ok(())
            }
        }
    }
}

/// Rows of one object written as the driver's insert script, in batches.
struct InsertSink {
    driver: &'static Arc<dyn Driver>,
    target: ObjectRef,
    /// Source column → target column (another engine may rename them).
    rename: HashMap<String, String>,
    columns: Vec<String>,
    buf: Vec<Vec<Value>>,
    out: Arc<Mutex<Out>>,
    rows: u64,
}

impl InsertSink {
    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.buf);
        let text = self
            .driver
            .insert_script(&self.target, &self.columns, &rows)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut out = self.out.lock().expect("script output");
        out.write(&text)?;
        out.write("\n")
    }
}

impl RowSink for InsertSink {
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> std::io::Result<()> {
        if index == 0 {
            self.columns = columns.iter().map(|c| self.rename.get(&c.name).cloned().unwrap_or_else(|| c.name.clone())).collect();
        }
        Ok(())
    }
    fn row(&mut self, index: usize, row: &[Value]) -> std::io::Result<()> {
        if index != 0 {
            return Ok(());
        }
        self.buf.push(row.to_vec());
        self.rows += 1;
        if self.buf.len() >= DATA_BATCH {
            self.flush()?;
        }
        Ok(())
    }
}

fn is_table(kind: &str) -> bool {
    kind == kinds::TABLE || kind == kinds::COLLECTION
}

/// `DROP VIEW …` and friends for objects that aren't tables (SQL and CQL
/// engines, MongoDB views).
pub(crate) fn drop_other(driver: &dyn Driver, obj: &ObjectRef, if_exists: bool) -> Option<String> {
    let id = driver.info().id;
    // A MongoDB view is a collection: dropping it doesn't touch its source.
    if matches!(id, "mongodb" | "ferretdb" | "documentdb") && obj.kind == kinds::VIEW {
        return Some(format!("db.getCollection({}).drop()", serde_json::to_string(&obj.name).unwrap_or_default()));
    }
    // OrientDB functions are records of OFunction.
    if id == "orientdb" && obj.kind == kinds::FUNCTION {
        return Some(format!("DELETE FROM OFunction WHERE name = '{}'", obj.name.replace('\\', "\\\\").replace('\'', "\\'")));
    }
    // CQL drops the same way (keyspace-qualified, double quotes).
    if !matches!(driver.info().language, Language::Sql | Language::Cql) {
        return None;
    }
    // SQL Server's full-text catalogs and stoplists: no schema, and no
    // `IF EXISTS` in their DROP.
    if driver.info().dialect == "mssql" && matches!(obj.kind.as_str(), "fulltext_catalog" | "fulltext_stoplist") {
        let (view, what) = if obj.kind == "fulltext_catalog" { ("sys.fulltext_catalogs", "FULLTEXT CATALOG") } else { ("sys.fulltext_stoplists", "FULLTEXT STOPLIST") };
        let drop = format!("DROP {what} {};", dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Bracket, &obj.name));
        return Some(if if_exists { format!("IF EXISTS (SELECT 1 FROM {view} WHERE name = N'{}')\n    {drop}", obj.name.replace('\'', "''")) } else { drop });
    }
    let keyword = match obj.kind.as_str() {
        kinds::VIEW => "VIEW",
        kinds::MATERIALIZED_VIEW => "MATERIALIZED VIEW",
        kinds::PROCEDURE => "PROCEDURE",
        kinds::FUNCTION => "FUNCTION",
        kinds::TRIGGER => "TRIGGER",
        // CUBRID's sequences are serials; Mimer's types are domains.
        kinds::SEQUENCE if driver.info().id == "cubrid" => "SERIAL",
        kinds::SEQUENCE => "SEQUENCE",
        kinds::SYNONYM => "SYNONYM",
        kinds::TYPE if matches!(driver.info().id, "mimer" | "firebird") => "DOMAIN",
        kinds::TYPE => "TYPE",
        "domain" => "DOMAIN",
        "virtual_table" => "TABLE",
        "dictionary" => "DICTIONARY",
        _ => return None,
    };
    let dialect = driver.info().dialect;
    let quote = match dialect {
        "mssql" | "sybase" => dbine_driver::sql::Quote::Bracket,
        "mysql" | "bigquery" | "spanner" | "hive" | "clickhouse" | "sparksql" | "databricks" | "orientdb" => dbine_driver::sql::Quote::Backtick,
        _ => dbine_driver::sql::Quote::Double,
    };
    // These don't take (or may not take) `IF EXISTS` in DROP (Oracle only
    // from 23ai); the object is there anyway when a compare drops it.
    let no_if_exists = matches!(dialect, "db2" | "teradata" | "oracle" | "sybase")
        || matches!(driver.info().id, "hana" | "netezza" | "dameng" | "altibase" | "cubrid" | "ingres" | "mimer" | "edb" | "firebird");
    let if_exists = if_exists && !no_if_exists;
    // Oracle and HANA list public synonyms under the schema PUBLIC.
    if obj.kind == kinds::SYNONYM && obj.schema().is_some_and(|s| s.eq_ignore_ascii_case("PUBLIC")) && matches!(driver.info().id, "oracle" | "oracle_adb" | "hana") {
        return Some(format!("DROP PUBLIC SYNONYM {};", dbine_driver::sql::quote_ident(quote, &obj.name)));
    }
    let name = dbine_driver::sql::qualified_name(quote, obj.schema(), &obj.name);
    // An Oracle type something depends on is only dropped with FORCE.
    let force = if obj.kind == kinds::TYPE && dialect == "oracle" && !matches!(driver.info().id, "dameng") { " FORCE" } else { "" };
    Some(format!("DROP {keyword} {}{name}{force};", if if_exists { "IF EXISTS " } else { "" }))
}

#[tauri::command(rename_all = "camelCase")]
pub async fn generate_script(app: AppHandle, state: State<'_, AppState>, args: GenerateArgs) -> CommandResult<GenerateResult> {
    let driver = driver_of(&state, &args.connection_id)?;
    let key = format!("script:{}", args.script_id);
    // Reading only: the script is written, never run here.
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, true).await?;
    let res = generate(Some(&app), driver, &entry, &args).await;
    state.sessions.remove(&key);
    if res.is_err() {
        if let Some(p) = &args.path {
            let _ = std::fs::remove_file(p);
        }
    }
    res
}

pub(crate) async fn generate(
    // `None`: no window to tell the progress to (a scheduled task).
    app: Option<&AppHandle>,
    driver: &'static Arc<dyn Driver>,
    entry: &Arc<SessionEntry>,
    args: &GenerateArgs,
) -> CommandResult<GenerateResult> {
    let o = args.options;
    let io = |e: std::io::Error| CommandError::Internal(e.to_string());
    // Another engine: the tables are converted and its driver writes.
    let src_id = driver.info().id;
    let other_engine = args.target_driver.as_deref().filter(|t| !t.is_empty() && *t != src_id);
    let w: &'static Arc<dyn Driver> = match other_engine {
        Some(t) => dbine_drivers::find(t).ok_or_else(|| CommandError::BadRequest(format!("esta versión no incluye el driver '{t}'")))?,
        None => driver,
    };
    let out = Arc::new(Mutex::new(match &args.path {
        Some(p) => Out::File(std::io::BufWriter::new(std::fs::File::create(p).map_err(io)?)),
        None => Out::Mem(String::new()),
    }));
    let write = |s: &str| out.lock().expect("script output").write(s).map_err(io);
    let sep = w.script_separator();
    let end_block = |text: &str| -> String {
        let t = text.trim_end();
        match (sep.is_empty(), w.info().language == Language::Sql && !t.ends_with(';')) {
            (false, _) => format!("{t}\n{sep}\n\n"),
            (true, true) => format!("{t};\n\n"),
            (true, false) => format!("{t}\n\n"),
        }
    };

    let mut s = entry.session.lock().await;
    let schemas: HashMap<(Option<String>, String), TableSchema> = if o.create || o.indexes || o.foreign_keys || o.drop || o.data {
        s.database_schema()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|t| ((t.schema.clone(), t.name.clone()), t))
            .collect()
    } else {
        HashMap::new()
    };
    // Converted for the other engine, keyed by the source's name.
    type Key = (Option<String>, String);
    let mut renames: HashMap<Key, HashMap<String, String>> = HashMap::new();
    let mut notes: Vec<String> = Vec::new();
    let schemas: HashMap<Key, TableSchema> = match other_engine {
        None => schemas,
        Some(t) => {
            let sel: Vec<TableSchema> = args
                .objects
                .iter()
                .filter(|o| is_table(&o.kind))
                .filter_map(|o| schemas.get(&(o.schema.clone(), o.name.clone())).cloned())
                .collect();
            // Each table stays in its schema (they're created first), so
            // same-named tables in two schemas don't collide.
            let opts = dbine_schema::Options {
                keep_schemas: w.info().has_schemas,
                rename_schemas: crate::commands::migration::default_schema_rename(driver.info().dialect, w.info().dialect),
                ..dbine_schema::Options::default()
            };
            let conv = dbine_schema::convert(&sel, src_id, t, &opts)
                .map_err(|e| CommandError::BadRequest(e.to_string()))?;
            for i in &conv.issues {
                if matches!(i.severity, dbine_schema::Severity::Loss | dbine_schema::Severity::Dropped) {
                    notes.push(format!("{}{}: {}", i.table, i.object.as_deref().map(|o| format!(" · {o}")).unwrap_or_default(), i.message));
                }
            }
            let mut m = HashMap::new();
            for (src, dst) in sel.iter().zip(conv.tables.into_iter()) {
                let key = (src.schema.clone(), src.name.clone());
                let qualified = src.schema.as_deref().map_or(src.name.clone(), |sc| format!("{sc}.{}", src.name));
                let r: HashMap<String, String> = conv
                    .columns
                    .iter()
                    .filter(|c| (c.table == src.name || c.table == qualified) && !c.column.is_empty())
                    .map(|c| (c.column.clone(), c.target_column.clone()))
                    .collect();
                renames.insert(key.clone(), r);
                m.insert(key, dst);
            }
            m
        }
    };
    let table_schema = |obj: &ObjectRef| schemas.get(&(obj.schema.clone(), obj.name.clone())).cloned();

    let tables: Vec<&ObjectRef> = args.objects.iter().filter(|o| is_table(&o.kind)).collect();
    let others: Vec<&ObjectRef> = args.objects.iter().filter(|o| !is_table(&o.kind)).collect();
    let (triggers, code): (Vec<&&ObjectRef>, Vec<&&ObjectRef>) = others.iter().partition(|o| o.kind == kinds::TRIGGER);
    // Code (views, routines, triggers) only goes out on the same engine.
    let same_engine = other_engine.is_none();
    // Exactly the steps the loops below take, so `done` ends at `total`.
    let total = [
        (o.drop, tables.len() + if same_engine { others.len() } else { 0 }),
        (o.create || o.indexes, tables.len()),
        (o.definitions && same_engine, code.len()),
        (o.data, tables.len()),
        (o.foreign_keys, tables.len()),
        (o.definitions && same_engine, triggers.len()),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, n)| n)
    .sum::<usize>();
    let mut done = 0usize;
    let cancelled = || entry.cancelled.load(Ordering::SeqCst);
    // Fast steps are throttled; `force` is for the first and last event and
    // for steps that may take long (a table's rows).
    let last_emit = Mutex::new(None::<std::time::Instant>);
    let progress = |done: usize, current: &str, force: bool| {
        let now = std::time::Instant::now();
        let mut last = last_emit.lock().expect("script progress");
        if !force && last.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(150)) {
            return;
        }
        *last = Some(now);
        if let Some(app) = app {
            let _ = app.emit("script-progress", ScriptProgress { id: args.script_id.clone(), done, total, current: current.to_string() });
        }
    };
    progress(0, "", true);
    let label = |obj: &ObjectRef| obj.schema().map_or(obj.name.clone(), |sc| format!("{sc}.{}", obj.name));

    if matches!(driver.info().language, Language::Sql | Language::Cql) {
        write(&format!(
            "-- DBine · script de {} · {}\n\n",
            if args.database.is_empty() { "la base" } else { &args.database },
            chrono::Local::now().format("%Y-%m-%d %H:%M")
        ))?;
    }

    if let Some(t) = other_engine {
        let c = if w.info().language == Language::Sql { "--" } else { "//" };
        let mut head = format!("{c} Convertido de {} a {}: tipos, valores por defecto, claves e índices (dbine-schema).\n", driver.info().name, w.info().name);
        if o.definitions && !others.is_empty() {
            head.push_str(&format!("{c} Vistas, rutinas y triggers no se convierten a otro motor: se omitieron {} objeto(s).\n", others.len()));
        }
        if !notes.is_empty() {
            head.push_str(&format!("{c} Observaciones de la conversión:\n"));
            for n in &notes {
                head.push_str(&format!("{c}   - {n}\n"));
            }
        }
        let _ = t;
        write(&format!("{head}\n"))?;
        // The schemas the converted tables go to.
        if (o.create || o.drop) && w.info().has_schemas {
            let mut seen: Vec<String> = Vec::new();
            for t in schemas.values() {
                if let Some(sc) = t.schema.as_deref().filter(|s| !s.is_empty()) {
                    if !seen.iter().any(|x| x == sc) {
                        seen.push(sc.to_string());
                    }
                }
            }
            seen.sort();
            for sc in seen {
                if let Some(stmt) = crate::commands::migration::create_schema(w.info().dialect, &sc) {
                    write(&end_block(&stmt))?;
                }
            }
        }
    }

    // 1. DROP: code first (it may depend on tables), then tables.
    if o.drop {
        for obj in others.iter().rev().filter(|_| same_engine) {
            progress(done, &label(obj), false);
            if let Some(d) = drop_other(driver.as_ref(), obj, o.if_exists) {
                write(&end_block(&d))?;
            }
            done += 1;
        }
        for obj in tables.iter().rev() {
            progress(done, &label(obj), false);
            if let Some(t) = table_schema(obj) {
                let ddl = w.table_ddl(&t, DdlParts { drop: true, if_exists: o.if_exists, ..Default::default() })?;
                write(&end_block(&ddl))?;
            }
            done += 1;
        }
    }
    // 2. Tables and their indexes.
    if o.create || o.indexes {
        for obj in &tables {
            if cancelled() {
                return Err(CommandError::Cancelled);
            }
            progress(done, &label(obj), false);
            match table_schema(obj) {
                Some(t) => {
                    let parts = DdlParts { create: o.create, indexes: o.indexes, if_exists: o.if_exists && !o.drop, ..Default::default() };
                    write(&end_block(&w.table_ddl(&t, parts)?))?;
                }
                // Engines that report no structure: their own source, if any.
                None => {
                    if let Some(def) = s.definition(obj).await? {
                        write(&end_block(&def))?;
                    }
                }
            }
            done += 1;
        }
    }
    // 4. Views, routines… (triggers wait until the data is in, so a restore
    //    doesn't fire them on every INSERT).
    if o.definitions && same_engine {
        for obj in &code {
            if cancelled() {
                return Err(CommandError::Cancelled);
            }
            progress(done, &label(obj), false);
            if let Some(def) = s.definition(obj).await? {
                write(&end_block(&def))?;
            }
            done += 1;
        }
    }
    // 5. Data, streamed through the driver's insert script.
    let mut rows = 0u64;
    if o.data {
        for obj in &tables {
            if cancelled() {
                return Err(CommandError::Cancelled);
            }
            progress(done, &label(obj), true);
            let (before, after) = table_schema(obj).map(|t| w.data_load_wrap(&t)).unwrap_or_default();
            if !before.is_empty() {
                write(&format!("{before}\n"))?;
            }
            let limit = o.data_limit.map_or(1_000_000_000, |l| l.min(u32::MAX as u64) as u32);
            let query = s.browse_query(obj, limit);
            let key = (obj.schema.clone(), obj.name.clone());
            let target = match (other_engine, table_schema(obj)) {
                (Some(_), Some(t)) => ObjectRef { kind: kinds::TABLE.into(), schema: t.schema.clone(), name: t.name.clone() },
                _ => (*obj).clone(),
            };
            let sink = Arc::new(Mutex::new(InsertSink {
                driver: w,
                target,
                rename: renames.get(&key).cloned().unwrap_or_default(),
                columns: Vec::new(),
                buf: Vec::new(),
                out: out.clone(),
                rows: 0,
            }));
            let mut result = QueryOutcome { sink: Some(RowSinkRef(sink.clone())), ..Default::default() };
            let run = tokio::select! {
                r = s.execute(&query, usize::MAX, &mut result) => Some(r),
                _ = entry.cancel.notified() => None,
            };
            match run {
                None => return Err(CommandError::Cancelled),
                Some(Err(e)) => return Err(CommandError::Sql(format!("datos de {}: {e}", label(obj)))),
                Some(Ok(())) => {}
            }
            if let Some(e) = result.sink_error.take() {
                return Err(CommandError::Internal(e));
            }
            result.sink = None;
            let mut sink = sink.lock().expect("insert sink");
            sink.flush().map_err(io)?;
            rows += sink.rows;
            drop(sink);
            if !after.is_empty() {
                write(&format!("{after}\n"))?;
            }
            write(&if sep.is_empty() { "\n".to_string() } else { format!("{sep}\n\n") })?;
            done += 1;
        }
    }
    // 5b. Foreign keys, once the tables exist and the rows are in (a data
    //     load then can't trip over a missing parent row).
    if o.foreign_keys {
        for obj in &tables {
            progress(done, &label(obj), false);
            if let Some(t) = table_schema(obj).filter(|t| !t.foreign_keys.is_empty()) {
                let fk = w.table_ddl(&t, DdlParts { foreign_keys: true, ..Default::default() })?;
                if !fk.trim().is_empty() {
                    write(&end_block(&fk))?;
                }
            }
            done += 1;
        }
    }
    // 6. Triggers.
    if o.definitions && same_engine {
        for obj in &triggers {
            if cancelled() {
                return Err(CommandError::Cancelled);
            }
            progress(done, &label(obj), false);
            if let Some(def) = s.definition(obj).await? {
                write(&end_block(&def))?;
            }
            done += 1;
        }
    }
    progress(total, "", true);

    let out = Arc::try_unwrap(out).map_err(|_| CommandError::Internal("script en uso".into()))?.into_inner().expect("script output");
    let script = match out {
        Out::File(mut f) => {
            f.flush().map_err(io)?;
            None
        }
        Out::Mem(m) => Some(m),
    };
    Ok(GenerateResult { script, objects: args.objects.len(), rows })
}

// -- run a script file ------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RunFileArgs {
    pub run_id: String,
    pub connection_id: String,
    pub database: String,
    pub path: String,
    #[serde(default)]
    pub continue_on_error: bool,
}

#[derive(Serialize)]
pub struct RunFileResult {
    pub statements: u64,
    pub errors: Vec<String>,
    pub elapsed_ms: u64,
}

#[derive(Clone, Serialize)]
struct RunProgress {
    id: String,
    statements: u64,
    bytes: u64,
    total_bytes: u64,
}

/// Chunks sent to the server at once (at statement boundaries).
const RUN_CHUNK: usize = 256 * 1024;

/// Run a script file in chunks: GO-batched engines split at `GO` lines,
/// Oracle at `/` lines, the others at lines ending a statement (`;`).
/// Each chunk goes to the driver's `execute`, which splits it further.
#[tauri::command(rename_all = "camelCase")]
pub async fn run_script_file(app: AppHandle, state: State<'_, AppState>, args: RunFileArgs) -> CommandResult<RunFileResult> {
    let started = std::time::Instant::now();
    let driver = driver_of(&state, &args.connection_id)?;
    let go_batches = matches!(driver.info().dialect, "mssql" | "sybase");
    let key = format!("run:{}", args.run_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, false).await?;
    let file = std::fs::File::open(&args.path).map_err(|e| CommandError::BadRequest(format!("no se pudo abrir {}: {e}", args.path)))?;
    let total_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut reader = BufReader::new(file);

    let mut errors = Vec::new();
    let mut statements = 0u64;
    let mut bytes = 0u64;
    let mut chunk = String::new();
    let mut line = String::new();
    let mut result: CommandResult<()> = Ok(());

    loop {
        line.clear();
        let n = tokio::task::block_in_place(|| reader.read_line(&mut line)).map_err(|e| CommandError::Internal(e.to_string()))?;
        let eof = n == 0;
        bytes += n as u64;
        if !eof {
            chunk.push_str(&line);
        }
        let t = line.trim();
        let boundary = eof
            || (go_batches && t.eq_ignore_ascii_case("go"))
            || t == "/"
            || (!go_batches && t.ends_with(';') && chunk.len() >= RUN_CHUNK);
        if boundary && !chunk.trim().is_empty() {
            if entry.cancelled.load(Ordering::SeqCst) {
                result = Err(CommandError::Cancelled);
                break;
            }
            let text = std::mem::take(&mut chunk);
            let count = if go_batches { 1 } else { dbine_driver::sql::split_statements(&text).len().max(1) as u64 };
            let mut out = QueryOutcome::default();
            let run = {
                let mut s = entry.session.lock().await;
                tokio::select! {
                    r = s.execute(&text, 1, &mut out) => Some(r),
                    _ = entry.cancel.notified() => None,
                }
            };
            match run {
                None => {
                    result = Err(CommandError::Cancelled);
                    break;
                }
                Some(Err(e)) => {
                    errors.push(format!("{} — {e}", text.trim().lines().next().unwrap_or("").chars().take(120).collect::<String>()));
                    if !args.continue_on_error {
                        break;
                    }
                }
                Some(Ok(())) => statements += count,
            }
            let _ = app.emit("script-run-progress", RunProgress { id: args.run_id.clone(), statements, bytes, total_bytes });
        }
        if eof {
            break;
        }
    }
    state.sessions.remove(&key);
    result?;
    Ok(RunFileResult { statements, errors, elapsed_ms: started.elapsed().as_millis() as u64 })
}
