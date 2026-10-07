//! Engines reached through ODBC: one generic session and a preset per
//! engine (DB2, Sybase, Informix, Teradata, Hive…) that fills in the
//! connection string, dialect details and a few catalog queries.
//!
//! The ODBC driver manager is loaded at runtime (see [`ffi`]), so the app
//! starts without unixODBC installed; only opening an ODBC connection
//! needs it.

mod backup;
mod blocking;
mod connstr;
mod create_db;
mod design;
mod explain;
mod ffi;
mod index_usage;
mod monitor;
mod odbc;
mod permissions;
mod plan;
mod presets;
mod processes;
mod schemas;
mod security;
mod steps;
mod structure;
mod sync;
mod transfer;

use dbine_driver::sql::{
    leading_keyword, qualified_name, select_top, split_script, split_statements, strip_comments, Limit, Quote, ScriptDefaults,
    ScriptDialect, ScriptMode, StatementKind,
};
use dbine_driver::{
    async_trait, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts,
    DesignerSpec, Driver, DriverInfo, Error, Field, FieldKind, ForeignKeyDef, IndexDef, KeyDef, Language, ObjectKindInfo,
    MonitorSnapshot, ObjectRef, QueryOutcome, ResultColumn, Result, Session, TableSchema, TxState,
};
use odbc::{kind_of, Conn, ConnectOptions, StmtSlot};
use presets::{Batch, Def, LimitStyle, Preset, P, PRESETS, V};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    PRESETS.iter().map(|p| Arc::new(OdbcDriver { preset: p, info: info(p) }) as Arc<dyn Driver>).collect()
}

/// Names of the ODBC drivers installed on this machine (odbcinst.ini, or
/// the registry on Windows), for a driver picker. Fails with a readable
/// message when there's no ODBC driver manager.
pub fn installed_odbc_drivers() -> Result<Vec<String>> {
    let api = ffi::api(None).map_err(Error::Connect)?;
    Ok(odbc::Env::new(api)?.drivers())
}

struct OdbcDriver {
    preset: &'static Preset,
    info: DriverInfo,
}

const BATCH_OPTIONS: &[(&str, &str)] = &[
    ("statements", "Una sentencia por vez (separadas por ;)"),
    ("go", "Bloques separados por GO"),
    ("script", "Todo el texto de una vez"),
];

fn batch_str(b: Batch) -> &'static str {
    match b {
        Batch::Statements => "statements",
        Batch::Go => "go",
        Batch::Script => "script",
    }
}

fn common_tail(p: &Preset) -> Vec<Field> {
    vec![
        Field::new("batch_mode", "Envío de scripts", FieldKind::Select(BATCH_OPTIONS.to_vec()))
            .default_value(batch_str(p.batch))
            .help("Cómo se manda un script al servidor. Si el motor acepta varias sentencias por pedido, «Todo el texto de una vez» respeta bloques como BEGIN … END.")
            .advanced(),
        Field::new("odbc_library", "Biblioteca ODBC", FieldKind::File)
            .placeholder("(se detecta sola)")
            .help("Ruta del administrador de controladores ODBC (libodbc.2.dylib, libodbc.so.2 u odbc32.dll), solo si no se encuentra en las rutas habituales.")
            .advanced(),
        Field::read_only(),
    ]
}

fn info(p: &'static Preset) -> DriverInfo {
    let mut fields = Vec::new();
    if p.is_generic() {
        fields.push(
            Field::new("dsn", "DSN", FieldKind::Text)
                .placeholder("MiOrigenDeDatos")
                .help("Un origen de datos definido en odbc.ini (o en el Administrador de orígenes de datos ODBC de Windows). Dejalo vacío si usás una cadena de conexión."),
        );
        fields.push(
            Field::new("connection_string", "Cadena de conexión", FieldKind::Textarea)
                .secret()
                .placeholder("DRIVER={ODBC Driver 18 for SQL Server};SERVER=host,1433;UID=usuario;PWD=…")
                .help("Se usa tal cual y tiene prioridad sobre el DSN. Se guarda en el llavero porque suele llevar la contraseña."),
        );
        fields.push(Field::username().help("Si se completa, se agrega como UID."));
        fields.push(Field::password().help("Si se completa, se agrega como PWD."));
        fields.push(
            Field::new("database", p.database_label, FieldKind::Text)
                .placeholder("(el predeterminado)")
                .help("Catálogo al que se cambia después de conectar."),
        );
    } else {
        if p.uses_part(|v| matches!(v, V::Host | V::HostPort), "{host}") {
            fields.push(Field::host());
        }
        if p.uses_part(|v| matches!(v, V::Port | V::HostPort), "{port}") {
            fields.push(if p.default_port == 0 {
                Field::port().required()
            } else {
                let port: &'static str = Box::leak(p.default_port.to_string().into_boxed_str());
                Field::port().placeholder(port)
            });
        }
        for (key, label, placeholder, required, help) in p.extra_options {
            let mut f = Field::new(key, label, FieldKind::Text).placeholder(placeholder).help(help);
            if *required {
                f = f.required();
            }
            fields.push(f);
        }
        if p.uses_part(|v| matches!(v, V::Database), "{database}") {
            let kind = if p.database_file() { FieldKind::File } else { FieldKind::Text };
            let mut f = Field::new("database", p.database_label, kind);
            f = if p.database_required { f.required() } else { f.placeholder("(la predeterminada)") };
            fields.push(f);
        }
        if p.uses_part(|v| matches!(v, V::User), "{user}") {
            fields.push(Field::username());
        }
        if p.uses(|v| matches!(v, V::Password)) {
            fields.push(Field::password());
        }
        fields.push(
            Field::new("odbc_driver", "Driver ODBC", FieldKind::Text)
                .placeholder(p.driver_hint)
                .default_value(p.driver_hint)
                .help(p.driver_help),
        );
        fields.push(
            Field::new("extra", "Atributos adicionales", FieldKind::Text)
                .placeholder("Clave=Valor;OtraClave=Valor")
                .help(
                    "Se agregan al final de la cadena de conexión ODBC y reemplazan a los del mismo nombre (TLS, timeouts, \
                     juego de caracteres, Kerberos o seguridad integrada de Windows…).",
                )
                .advanced(),
        );
    }
    fields.extend(common_tail(p));

    let mut object_kinds = vec![ObjectKindInfo::tables(), ObjectKindInfo::views()];
    if p.procedures {
        object_kinds.push(ObjectKindInfo::procedures());
    }
    if p.functions {
        object_kinds.push(ObjectKindInfo::functions());
    }
    object_kinds.extend(structure::object_kinds(design::eng(p)));
    DriverInfo {
        id: p.id,
        name: p.name,
        family: p.family,
        language: Language::Sql,
        dialect: p.dialect,
        default_port: p.default_port,
        fields,
        databases_label: p.databases_label,
        has_schemas: p.has_schemas,
        object_kinds,
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[async_trait]
impl Driver for OdbcDriver {
    /// Sybase ASE, like SQL Server, loads explicit identity values only
    /// with IDENTITY_INSERT on.
    fn data_load_wrap(&self, table: &dbine_driver::TableSchema) -> (String, String) {
        if self.info.id != "sybase" || !table.columns.iter().any(|c| c.auto_increment) {
            return (String::new(), String::new());
        }
        let name = dbine_driver::sql::qualified_name(
            dbine_driver::sql::Quote::Bracket,
            table.schema.as_deref().filter(|s| !s.is_empty()),
            &table.name,
        );
        (format!("SET IDENTITY_INSERT {name} ON"), format!("SET IDENTITY_INSERT {name} OFF"))
    }

    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        security::dialect(self.preset).map(security::spec)
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        match security::dialect(self.preset) {
            Some(d) => security::script(d, action),
            None => Err(Error::Unsupported(security::unsupported(self.preset).into())),
        }
    }

    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        schemas::spec(self.preset)
    }

    fn create_schema_script(&self, _database: Option<&str>, name: &str, owner: Option<&str>) -> Result<String> {
        schemas::create(self.preset, name, owner)
    }

    fn schema_owner_script(&self, _database: Option<&str>, name: &str, owner: &str) -> Result<Option<String>> {
        schemas::owner(self.preset, name, owner)
    }

    fn drop_schema_script(&self, _database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        schemas::drop(self.preset, name, cascade).ok_or_else(|| Error::Unsupported("este motor no borra esquemas desde DBine".into()))
    }

    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        backup::spec(self.preset)
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(self.preset, action)
    }

    fn supports_bulk_load(&self) -> bool {
        transfer::supports_bulk_load(self.preset)
    }

    fn supports_explain(&self) -> bool {
        explain::preset_supports_explain(self.preset.id)
    }

    fn script_dialect(&self) -> ScriptDialect {
        script_dialect(self.preset)
    }

    fn script_mode(&self) -> ScriptMode {
        script_mode(self.preset)
    }

    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: continues_on_error(self.preset), confirm_unsafe_dml: true }
    }

    fn supports_manual_transactions(&self) -> bool {
        has_transactions(self.preset)
    }

    fn capabilities(&self) -> Capabilities {
        design::capabilities(self.preset)
    }

    /// "Nueva base de datos"'s options (see [`create_db`]).
    fn create_database_fields(&self) -> Vec<dbine_driver::Field> {
        create_db::fields(self.preset)
    }

    fn create_database_script(&self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<String> {
        create_db::script(self.preset, name, options)
    }

    fn designer(&self) -> Option<DesignerSpec> {
        // SuiteAnalytics Connect is read-only.
        (design::eng(self.preset) != design::Eng::NetSuite).then(|| design::designer(self.preset))
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        design::create_templates(self.preset)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(design::table_ddl(self.preset, table, parts))
    }

    fn supports_schema_sync(&self) -> bool {
        design::eng(self.preset) != design::Eng::NetSuite
    }

    /// The indexes and keys of the ODBC catalog, with the engine's counters
    /// where it keeps them per index (see [`index_usage`]).
    fn supports_index_usage(&self) -> bool {
        index_usage::supported(self.preset)
    }

    /// Informix, GBase 8s and SAP MaxDB (see [`index_usage`]).
    fn supports_index_toggle(&self) -> bool {
        index_usage::toggle_supported(self.preset)
    }

    fn index_toggle_script(&self, table: &ObjectRef, index: &dbine_driver::IndexUsage, enable: bool) -> Result<dbine_driver::SyncScript> {
        index_usage::toggle_script(self.preset, table, index, enable)
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(self.preset, changes)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        Ok(design::insert_script(self.preset, target.schema(), &target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        design::update_script(self.preset, target.schema(), &target.name, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        design::delete_script(self.preset, target.schema(), &target.name, keys)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        design::filtered_browse(self.preset, browse, filters)
    }

    fn script_separator(&self) -> &'static str {
        design::script_separator(self.preset)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let preset = self.preset;
        // Single-namespace engines always use the configured database; the
        // others (and the generic catalog) take the one the explorer asks for.
        let db = if preset.databases_label.is_empty() {
            cfg.database.trim().to_string()
        } else {
            database.filter(|d| !d.is_empty()).unwrap_or(&cfg.database).trim().to_string()
        };
        // dBase: the folder of the .dbf files, also when a file was picked.
        let db = if preset.id == "dbase" && std::path::Path::new(&db).is_file() {
            std::path::Path::new(&db).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or(db)
        } else {
            db
        };
        let conn_str = connstr::build(preset, cfg, &db)?;
        let file = preset.database_file().then(|| cfg.database.trim().to_string()).filter(|f| !f.is_empty());
        let catalog = (preset.is_generic() && !db.is_empty()).then(|| db.clone());
        let library = cfg.option("odbc_library").map(str::to_string);
        let batch = cfg.option("batch_mode").and_then(Batch::from_option).unwrap_or(preset.batch);
        let read_only = cfg.read_only;

        let task = tokio::task::spawn_blocking(move || -> Result<_> {
            let api = ffi::api(library.as_deref()).map_err(Error::Connect)?;
            let conn = Conn::open(
                api,
                &ConnectOptions {
                    conn_str: &conn_str,
                    login_timeout_secs: 15,
                    read_only,
                    catalog: catalog.as_deref(),
                },
            )?;
            let dbms = conn.info_string(ffi::SQL_DBMS_NAME);
            let version = conn.info_string(ffi::SQL_DBMS_VER);
            let quote = conn.info_string(ffi::SQL_IDENTIFIER_QUOTE_CHAR);
            let current = conn.info_string(ffi::SQL_DATABASE_NAME);
            let escape = conn.info_string(ffi::SQL_SEARCH_PATTERN_ESCAPE);
            Ok((api, conn, dbms, version, quote, current, escape))
        });
        let (api, conn, dbms, version, quote_char, current, escape) = tokio::time::timeout(CONNECT_TIMEOUT, task)
            .await
            .map_err(|_| Error::Connect(format!("No hubo respuesta del servidor en {} s.", CONNECT_TIMEOUT.as_secs())))?
            .map_err(|e| Error::State(e.to_string()))??;

        let quote = preset.quote.unwrap_or(match quote_char.trim() {
            "`" => Quote::Backtick,
            "[" => Quote::Bracket,
            _ => Quote::Double,
        });
        let limit = preset.limit.unwrap_or_else(|| guess_limit(&dbms));
        let database = [db, current].into_iter().find(|d| !d.is_empty()).unwrap_or_else(|| "default".into());
        Ok(Box::new(OdbcSession {
            preset,
            api,
            inner: Arc::new(Inner { conn: Mutex::new(conn), slot: StmtSlot::default() }),
            quote,
            limit,
            batch,
            database,
            version: format!("{dbms} {version}").trim().to_string(),
            dbms,
            escape,
            file,
            manual: false,
            dirty: false,
        }))
    }
}

/// How the engine's own tool reads a script.
fn script_dialect(p: &Preset) -> ScriptDialect {
    let generic = ScriptDialect::generic();
    match p.id {
        // CLP's `--#SET TERMINATOR`; SQL PL bodies (`BEGIN … END`) whole.
        "db2" | "db2i" | "db2zos" => ScriptDialect::db2(),
        // isql: batches separated by `go`.
        "sybase" | "sqlanywhere" => ScriptDialect::tsql(),
        // disql: PL blocks ended by `/`, as in SQL*Plus.
        "dameng" => ScriptDialect::oracle(),
        // beeline / impala-shell / spark-sql: backslash escapes in strings.
        "hive" | "impala" | "spark" | "kyuubi" | "cloudera" => ScriptDialect { backslash_escapes: true, compound_blocks: false, ..generic },
        // PLvSQL and Python UDF bodies go between `$$`.
        "vertica" | "sqream" => ScriptDialect { dollar_quotes: true, ..generic },
        "access" | "dbase" => ScriptDialect { bracket_idents: true, backtick_idents: false, ..generic },
        _ => ScriptDialect::for_hint(p.dialect),
    }
}

/// Presets whose block syntax the shared lexer doesn't know yet (Informix
/// SPL's `END PROCEDURE`, Exasol's scripts ended by `/`, Netezza's
/// `BEGIN_PROC`, NuoDB's `END_PROCEDURE`, ObjectScript and Virtuoso `{ }`
/// bodies, declarations before `BEGIN` in Ingres, MaxDB and Altibase), and
/// the generic preset (any engine): the driver gets the whole script and
/// splits it as the connection's "Envío de scripts" says. Teradata too: its
/// macros hold `;` between parentheses (`CREATE MACRO m AS (…; …;)`) and
/// `REPLACE PROCEDURE` heads a body; [`teradata_statements`] keeps both
/// whole, and "Todo el texto de una vez" still sends the script as is.
const WHOLE_SCRIPT: &[&str] = &["odbc", "teradata", "informix", "gbase8s", "exasol", "netezza", "nuodb", "virtuoso", "iris", "cache", "maxdb", "ingres", "altibase"];

fn script_mode(p: &Preset) -> ScriptMode {
    if WHOLE_SCRIPT.contains(&p.id) {
        ScriptMode::Whole
    } else if p.batch == Batch::Go {
        ScriptMode::Batches
    } else {
        ScriptMode::PerStatement
    }
}

/// The engine's tool goes on after an error: db2 CLP, isql, BTEQ, vsql,
/// EXAplus, nzsql, mclient, disql. beeline, impala-shell and the rest stop.
fn continues_on_error(p: &Preset) -> bool {
    matches!(
        p.id,
        "db2" | "db2i" | "db2zos" | "sybase" | "sqlanywhere" | "teradata" | "vertica" | "exasol" | "netezza" | "monetdb" | "dameng" | "altibase"
    )
}

/// Engines without transactions (or whose ODBC drivers refuse autocommit
/// off): no Auto/Manual switch.
fn has_transactions(p: &Preset) -> bool {
    !matches!(p.id, "hive" | "impala" | "spark" | "kyuubi" | "cloudera" | "netsuite" | "heavydb" | "machbase" | "dbase" | "ocient" | "sqream")
}

/// The statements `execute` runs one by one: on presets the app runs
/// statement by statement, split with the preset's dialect (so a unit the
/// app sends stays whole); a text that only splits into pieces of a block
/// (one of them starts with `END`: a `BEGIN … END` cut at its `;`, sent
/// under a switched terminator) goes whole.
fn unit_statements(text: &str, d: &ScriptDialect) -> Vec<String> {
    let pieces: Vec<String> = split_script(text, d)
        .into_iter()
        .filter(|s| s.kind != StatementKind::ClientCommand)
        .map(|s| strip_comments(&s.text, d, true).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if pieces.len() > 1 && pieces.iter().any(|p| leading_keyword(p, d).as_deref() == Some("end")) {
        return vec![text.trim().to_string()];
    }
    pieces
}

/// BTEQ-like statements of a Teradata script: the shared lexer's, with a
/// `REPLACE PROCEDURE|FUNCTION|TRIGGER…` read as the `CREATE` it stands for
/// (so its `BEGIN … END` body stays whole) and the pieces of a statement
/// whose parentheses are still open joined back (a macro's body).
fn teradata_statements(text: &str, d: &ScriptDialect) -> Vec<String> {
    teradata_units(text, d).into_iter().map(|(s, _)| s).collect()
}

/// [`teradata_statements`], each with its byte offset in `text`.
fn teradata_units(text: &str, d: &ScriptDialect) -> Vec<(String, usize)> {
    // REPLACE and "CREATE " have the same length: offsets carry over.
    let mut lex = text.to_string();
    for u in split_script(text, d) {
        let head = &text[u.start..];
        if head.len() > 7 && head[..7].eq_ignore_ascii_case("replace") && head.as_bytes()[7].is_ascii_whitespace() {
            lex.replace_range(u.start..u.start + 7, "CREATE ");
        }
    }
    let mut out = Vec::new();
    let mut open: Option<(usize, i64)> = None;
    for u in split_script(&lex, d) {
        if u.kind == StatementKind::ClientCommand {
            continue;
        }
        let depth = paren_depth(&strip_comments(&text[u.start..u.end], d, false));
        let (start, total) = match open.take() {
            Some((start, before)) => (start, before + depth),
            None => (u.start, depth),
        };
        if total > 0 {
            open = Some((start, total));
        } else {
            out.push((text[start..u.end].trim().to_string(), start));
        }
    }
    if let Some((start, _)) = open {
        out.push((text[start..].trim().trim_end_matches(';').trim_end().to_string(), start));
    }
    out.retain(|(s, _)| !s.is_empty());
    out
}

/// `(` minus `)` outside '…' and "…" (comments already removed).
fn paren_depth(s: &str) -> i64 {
    let mut depth = 0;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// What `execute` / `explain` run on this preset.
fn preset_statements(p: &Preset, batch: Batch, text: &str) -> Result<Vec<String>> {
    match (p.id, batch) {
        ("teradata", Batch::Statements) => Ok(teradata_statements(text, &script_dialect(p))),
        _ => split_checked(batch, text),
    }
}

/// [`preset_statements`], each with its byte offset in `text` (where its
/// first token is), so a failure in a later statement of a script the
/// driver splits gets its own line.
fn placed_statements(p: &Preset, batch: Batch, text: &str) -> Result<Vec<(String, usize)>> {
    match (p.id, batch) {
        ("teradata", Batch::Statements) => Ok(teradata_units(text, &script_dialect(p))),
        // As `split_statements`, keeping where each one starts.
        (_, Batch::Statements) => {
            let d = ScriptDialect::generic();
            Ok(split_script(text, &d)
                .into_iter()
                .filter(|s| s.kind != StatementKind::ClientCommand)
                .map(|s| (strip_comments(&s.text, &d, false).trim().to_string(), s.start))
                .filter(|(s, _)| !s.is_empty())
                .collect())
        }
        _ => Ok(locate(text, split_checked(batch, text)?)),
    }
}

/// Offsets of `pieces` (cut from `text` in order) in `text`; a piece not
/// found as written (comments taken out) gets the previous one's end.
fn locate(text: &str, pieces: Vec<String>) -> Vec<(String, usize)> {
    let mut from = 0;
    pieces
        .into_iter()
        .map(|s| {
            match text.get(from..).and_then(|rest| rest.find(s.as_str())) {
                Some(i) => {
                    let at = from + i;
                    from = at + s.len();
                    (s, at)
                }
                None => (s, from),
            }
        })
        .collect()
}

/// Row-limit syntax for the generic preset, from the DBMS name.
fn guess_limit(dbms: &str) -> LimitStyle {
    let d = dbms.to_ascii_lowercase();
    if ["sql server", "adaptive server", "sql anywhere", "teradata", "sybase"].iter().any(|n| d.contains(n)) {
        LimitStyle::Top
    } else if ["db2", "oracle", "derby"].iter().any(|n| d.contains(n)) {
        LimitStyle::FetchFirst
    } else if d.contains("informix") || d.contains("gbase") {
        LimitStyle::First
    } else {
        LimitStyle::Limit
    }
}

fn browse(quote: Quote, limit: LimitStyle, schema: Option<&str>, name: &str, n: u32) -> String {
    match limit {
        LimitStyle::Limit => select_top(quote, Limit::Limit, schema, name, n),
        LimitStyle::FetchFirst => select_top(quote, Limit::FetchFirst, schema, name, n),
        LimitStyle::Top => format!("SELECT TOP {n} *\nFROM {}", qualified_name(quote, schema, name)),
        LimitStyle::First => format!("SELECT FIRST {n} *\nFROM {}", qualified_name(quote, schema, name)),
    }
}

/// Batches separated by `GO [N]` lines, cut by the shared T-SQL lexer (a
/// `GO` inside a comment or string doesn't split; `GO 3` runs its batch
/// three times; `GO -- note` is a separator). An invalid count runs
/// nothing, as in isql / sqlcmd.
fn split_go(sql: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for unit in dbine_driver::sql::split_script(sql, &dbine_driver::ScriptDialect::tsql()) {
        if let Some(e) = unit.error {
            return Err(dbine_driver::ScriptError::new(e).at_line(unit.line).fatal().into());
        }
        for _ in 0..unit.repeat.max(1) {
            out.push(unit.text.clone());
        }
    }
    Ok(out)
}

/// What `execute` / `explain` run: an invalid `GO` count fails the script.
fn split_checked(batch: Batch, text: &str) -> Result<Vec<String>> {
    Ok(match batch {
        Batch::Statements => split_statements(text),
        Batch::Go => split_go(text)?,
        Batch::Script if text.trim().is_empty() => Vec::new(),
        Batch::Script => vec![text.to_string()],
    })
}

/// [`split_checked`] for generated scripts (always valid).
#[cfg(test)]
fn split(batch: Batch, text: &str) -> Vec<String> {
    split_checked(batch, text).unwrap_or_default()
}

/// `nvarchar(50)`, `decimal(18,2)`… from SQLColumns.
fn format_type(name: &str, sql_type: i16, size: Option<u64>, digits: Option<u64>) -> String {
    use ffi::*;
    if name.contains('(') {
        return name.to_string();
    }
    match sql_type {
        SQL_CHAR | SQL_VARCHAR | SQL_WCHAR | SQL_WVARCHAR | SQL_BINARY | SQL_VARBINARY => match size {
            Some(s) if s > 0 && s < (1 << 30) => format!("{name}({s})"),
            _ => name.to_string(),
        },
        SQL_NUMERIC | SQL_DECIMAL => match (size, digits) {
            (Some(p), Some(s)) if p > 0 => format!("{name}({p},{s})"),
            (Some(p), None) if p > 0 => format!("{name}({p})"),
            _ => name.to_string(),
        },
        _ => name.to_string(),
    }
}

/// SQL Server reports procedures as `name;1`.
fn strip_version(name: &str) -> &str {
    match name.rsplit_once(';') {
        Some((n, v)) if v.chars().all(|c| c.is_ascii_digit()) => n,
        _ => name,
    }
}

struct Inner {
    conn: Mutex<Conn>,
    slot: StmtSlot,
}

pub struct OdbcSession {
    preset: &'static Preset,
    api: &'static ffi::Api,
    inner: Arc<Inner>,
    quote: Quote,
    limit: LimitStyle,
    batch: Batch,
    database: String,
    version: String,
    /// SQL_DBMS_NAME, for the plan dialect of the generic preset.
    dbms: String,
    /// SQL_SEARCH_PATTERN_ESCAPE.
    escape: String,
    /// The database file or folder of file-based presets (Access, dBase).
    file: Option<String>,
    /// Autocommit off (manual transactions).
    manual: bool,
    /// In manual mode: something changed since the last commit or rollback.
    dirty: bool,
}

type Rows = Vec<Vec<Option<String>>>;

fn col(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten()
}

fn num(r: &[Option<String>], i: usize) -> Option<u64> {
    col(r, i).and_then(|s| s.trim().parse::<i64>().ok()).and_then(|v| u64::try_from(v).ok())
}

impl OdbcSession {
    /// Runs `f` on a blocking thread with the connection locked.
    async fn run<T: Send + 'static>(&self, f: impl FnOnce(&Conn, &StmtSlot) -> Result<T> + Send + 'static) -> Result<T> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let conn = inner.conn.lock().unwrap_or_else(|e| e.into_inner());
            inner.slot.cancelled.store(false, std::sync::atomic::Ordering::SeqCst);
            f(&conn, &inner.slot)
        })
        .await
        .map_err(|e| Error::State(e.to_string()))?
    }

    /// [`Self::query`] with the result's column names.
    async fn query_named(&self, sql: String, params: Vec<String>) -> Result<(Vec<String>, Rows)> {
        self.run(move |c, slot| {
            let st = c.stmt(slot)?;
            let p: Vec<&str> = params.iter().map(String::as_str).collect();
            st.exec_params(&sql, &p)?;
            let n = st.num_cols()?;
            let cols = if n > 0 { st.describe(n)?.into_iter().map(|c| c.name).collect() } else { Vec::new() };
            Ok((cols, st.text_rows()?))
        })
        .await
    }

    /// Rows of a query with text parameters, every column as text.
    async fn query(&self, sql: String, params: Vec<String>) -> Result<Rows> {
        self.run(move |c, slot| {
            let st = c.stmt(slot)?;
            let p: Vec<&str> = params.iter().map(String::as_str).collect();
            if p.is_empty() {
                st.exec(&sql)?;
            } else {
                st.exec_params(&sql, &p)?;
            }
            st.text_rows()
        })
        .await
    }
}

fn run_one(c: &Conn, slot: &StmtSlot, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    let st = c.stmt(slot)?;
    let rc = st.exec(sql)?;
    for d in st.diag_messages() {
        out.message(d.to_message());
    }
    if rc == ffi::SQL_NO_DATA {
        // A searched UPDATE/DELETE that matched nothing.
        out.push_affected(0);
        return Ok(());
    }
    let before = out.results.len();
    loop {
        let n = st.num_cols()?;
        if n > 0 {
            let cols = st.describe(n)?;
            let kinds: Vec<_> = cols.iter().map(|c| kind_of(c.sql_type)).collect();
            out.begin_result(cols.into_iter().map(|c| ResultColumn { name: c.name, type_name: c.type_name }).collect());
            let mut seen = 0usize;
            while st.fetch()? {
                if seen < max_rows {
                    let mut row = Vec::with_capacity(kinds.len());
                    for (i, k) in kinds.iter().enumerate() {
                        row.push(st.cell(i as u16 + 1, *k)?);
                    }
                    out.push_row(row, max_rows);
                } else {
                    // Past the limit: count without reading the cells.
                    out.push_row(Vec::new(), max_rows);
                }
                seen += 1;
            }
        } else {
            if let Some(n) = st.row_count() {
                out.push_affected(n);
            }
        }
        let more = st.more_results();
        for d in st.diag_messages() {
            out.message(d.to_message());
        }
        if !more? {
            break;
        }
    }
    if out.results.len() == before {
        // DDL, PRINT…: no rows and no count, but it ran.
        out.results.push(Default::default());
    }
    Ok(())
}

#[async_trait]
impl Session for OdbcSession {
    async fn server_version(&mut self) -> Result<String> {
        Ok(if self.version.is_empty() { self.preset.name.to_string() } else { self.version.clone() })
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        if let Some(sql) = self.preset.databases_sql {
            let rows = self.query(sql.to_string(), Vec::new()).await?;
            return Ok(rows.iter().filter_map(|r| col(r, 0)).map(|s| s.trim().to_string()).collect());
        }
        if !self.preset.databases_label.is_empty() {
            // SQL_ALL_CATALOGS: catalog "%", schema and table "".
            let rows = self
                .run(|c, slot| {
                    let st = c.stmt(slot)?;
                    st.tables(Some("%"), Some(""), Some(""), None)?;
                    st.text_rows()
                })
                .await
                .unwrap_or_default();
            let mut v: Vec<String> = rows.iter().filter_map(|r| col(r, 0)).filter(|s| !s.is_empty()).collect();
            v.dedup();
            if !v.is_empty() {
                return Ok(v);
            }
        }
        Ok(vec![self.database.clone()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let preset = self.preset;
        let dbms = self.version.to_ascii_lowercase();
        let typed_routines = !(dbms.contains("sql server") || dbms.contains("adaptive server"));
        let (tables, procs) = self
            .run(move |c, slot| {
                let tables = {
                    let st = c.stmt(slot)?;
                    st.tables(None, Some("%"), Some("%"), None)?;
                    st.text_rows()?
                };
                let procs = if preset.procedures {
                    let r = c.stmt(slot).and_then(|st| {
                        st.procedures()?;
                        st.text_rows()
                    });
                    r.unwrap_or_else(|e| {
                        tracing::debug!("odbc: SQLProcedures failed: {e}");
                        Vec::new()
                    })
                } else {
                    Vec::new()
                };
                Ok((tables, procs))
            })
            .await?;

        let schema_of = |s: Option<String>| if preset.has_schemas { s.filter(|s| !s.is_empty()) } else { None };
        let mut out = Vec::new();
        for r in &tables {
            let schema = col(r, 1);
            if schema.as_deref().is_some_and(|s| preset.is_system_schema(s)) {
                continue;
            }
            let kind = match col(r, 3).unwrap_or_default().to_ascii_uppercase().as_str() {
                "TABLE" | "BASE TABLE" => kinds::TABLE,
                "VIEW" => kinds::VIEW,
                _ => continue,
            };
            let Some(name) = col(r, 2) else { continue };
            out.push(DbObject { kind: kind.into(), schema: schema_of(schema), name, parent: None });
        }
        let mut routines = Vec::new();
        for r in &procs {
            let schema = col(r, 1);
            if schema.as_deref().is_some_and(|s| preset.is_system_schema(s)) {
                continue;
            }
            let Some(name) = col(r, 2) else { continue };
            // PROCEDURE_TYPE: 2 = returns a value. SQL Server and ASE say 2
            // for every procedure (the return code), so there it means nothing.
            let kind = if col(r, 7).as_deref().map(str::trim) == Some("2") && preset.functions && typed_routines {
                kinds::FUNCTION
            } else {
                kinds::PROCEDURE
            };
            routines.push(DbObject {
                kind: kind.into(),
                schema: schema_of(schema),
                name: strip_version(&name).to_string(),
                parent: None,
            });
        }
        routines.sort_by(|a, b| (&a.kind, &a.schema, &a.name).cmp(&(&b.kind, &b.schema, &b.name)));
        routines.dedup_by(|a, b| a.kind == b.kind && a.schema == b.schema && a.name == b.name);
        out.extend(routines);
        // Sequences, aliases / synonyms and types, from the engine's catalog.
        for sql in structure::objects_sql(design::eng(preset)) {
            match self.query(sql.to_string(), Vec::new()).await {
                Ok(rows) => out.extend(rows.iter().filter_map(|r| {
                    let (kind, name) = (col(r, 0)?, col(r, 2)?.trim_end().to_string());
                    let schema = col(r, 1).map(|s| s.trim_end().to_string());
                    let kind = [kinds::SEQUENCE, kinds::SYNONYM, kinds::TYPE].into_iter().find(|k| *k == kind.trim())?;
                    Some(DbObject { kind: kind.into(), schema: schema_of(schema), name, parent: None })
                })),
                Err(e) => tracing::debug!("odbc: object list failed: {e}"),
            }
        }
        out.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
        Ok(out)
    }

    /// Presets with "Nuevo esquema…" (see `schemas::list_sql`): their
    /// catalog, else SQLTables' SQL_ALL_SCHEMAS; `None` when both fail.
    async fn list_schemas(&mut self) -> Result<Option<Vec<dbine_driver::SchemaInfo>>> {
        let Some(sql) = schemas::list_sql(self.preset) else { return Ok(None) };
        if let Some(sql) = sql {
            match self.query(sql.to_string(), Vec::new()).await {
                Ok(rows) => return Ok(Some(schemas::infos(self.preset, &rows, 0, schemas::flag_col(self.preset)))),
                Err(e) => tracing::debug!("odbc: schema list failed, trying SQLTables: {e}"),
            }
        }
        let rows = self
            .run(|c, slot| {
                let st = c.stmt(slot)?;
                st.tables(Some(""), Some("%"), Some(""), None)?;
                st.text_rows()
            })
            .await;
        match rows {
            Ok(rows) => Ok(Some(schemas::infos(self.preset, &rows, 1, None))),
            Err(e) => {
                tracing::debug!("odbc: SQLTables(SQL_ALL_SCHEMAS) failed: {e}");
                Ok(None)
            }
        }
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let schema = obj.schema().map(str::to_string);
        let name = obj.name.clone();
        let esc = self.escape.clone();
        let (cols, pks) = self
            .run(move |c, slot| {
                let cols = {
                    let st = c.stmt(slot)?;
                    let sp = schema.as_deref().map(|s| connstr::escape_pattern(s, &esc));
                    st.columns(sp.as_deref(), &connstr::escape_pattern(&name, &esc))?;
                    st.text_rows()?
                };
                let pks = c
                    .stmt(slot)
                    .and_then(|st| {
                        st.primary_keys(schema.as_deref(), &name)?;
                        st.text_rows()
                    })
                    .unwrap_or_default();
                // Without a working escape, `_` may have matched more tables.
                let cols: Rows = cols
                    .into_iter()
                    .filter(|r| {
                        col(r, 2).as_deref() == Some(name.as_str())
                            && (schema.is_none() || col(r, 1).is_none() || col(r, 1) == schema)
                    })
                    .collect();
                Ok((cols, pks))
            })
            .await?;
        let pk_names: Vec<String> = pks.iter().filter_map(|r| col(r, 3)).collect();
        let mut out: Vec<(u64, ColumnInfo)> = cols
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let type_name = col(r, 5).unwrap_or_default();
                let sql_type = col(r, 4).and_then(|s| s.trim().parse::<i16>().ok()).unwrap_or(0);
                let name = col(r, 3).unwrap_or_default();
                let lower = type_name.to_ascii_lowercase();
                let info = ColumnInfo {
                    data_type: format_type(&type_name, sql_type, num(r, 6), num(r, 8)),
                    nullable: col(r, 10).as_deref().map(str::trim) != Some("0"),
                    primary_key: pk_names.contains(&name),
                    auto_increment: lower.contains("identity") || lower.contains("serial") || lower.contains("auto_increment"),
                    default_value: col(r, 12).filter(|d| !d.is_empty()),
                    name,
                };
                (num(r, 16).unwrap_or(i as u64), info)
            })
            .collect();
        out.sort_by_key(|(ord, _)| *ord);
        Ok(out.into_iter().map(|(_, c)| c).collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let defs = &self.preset.defs;
        let e = design::eng(self.preset);
        let own = structure::definition_sql(e, &obj.kind);
        if !own.is_empty() {
            let mut sets = Vec::new();
            for sql in own {
                let mut params = vec![obj.name.clone()];
                if structure::definition_takes_schema(e) {
                    params.insert(0, obj.schema().unwrap_or("").to_string());
                }
                sets.push(self.query(sql.to_string(), params).await?);
            }
            return Ok(structure::build(e, &obj.kind, self.quote, obj.schema(), &obj.name, &sets));
        }
        if let Some(sql) = structure::named_definition_sql(e, &obj.kind) {
            let params = vec![obj.schema().unwrap_or("").to_string(), obj.name.clone()];
            let (cols, rows) = self.query_named(sql.to_string(), params).await?;
            return Ok(rows.first().and_then(|r| structure::build_named(e, &obj.kind, self.quote, obj.schema(), &obj.name, &cols, r)));
        }
        let def = match obj.kind.as_str() {
            kinds::TABLE => defs.table,
            kinds::VIEW => defs.view,
            kinds::PROCEDURE => defs.procedure,
            kinds::FUNCTION => defs.function,
            _ => Def::None,
        };
        let res = match def {
            Def::None => return Ok(None),
            Def::Sql(sql, params, join) => {
                let values = params
                    .iter()
                    .map(|p| match p {
                        P::Schema => obj.schema().unwrap_or("").to_string(),
                        P::Name => obj.name.clone(),
                        P::Catalog => self.database.clone(),
                        P::Qualified => match obj.schema() {
                            Some(s) if !s.is_empty() => format!("{s}.{}", obj.name),
                            _ => obj.name.clone(),
                        },
                    })
                    .collect();
                self.query(sql.to_string(), values).await.map(|rows| {
                    rows.iter().filter_map(|r| col(r, 0)).collect::<Vec<_>>().join(join)
                })
            }
            Def::Show(prefix) => {
                let sql = format!("{prefix} {}", qualified_name(self.quote, obj.schema(), &obj.name));
                self.query(sql, Vec::new()).await.map(|rows| {
                    rows.iter()
                        .filter_map(|r| col(r, 0))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .replace("\r\n", "\n")
                        .replace('\r', "\n")
                })
            }
        };
        match res {
            Ok(text) if text.trim().is_empty() => Ok(None),
            Ok(text) => Ok(Some(text)),
            // The generic preset guesses INFORMATION_SCHEMA: not every engine has it.
            Err(e) if self.preset.is_generic() => {
                tracing::debug!("odbc: no definition for {}: {e}", obj.name);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        browse(self.quote, self.limit, obj.schema(), &obj.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let dialect = script_dialect(self.preset);
        let stmts = match (script_mode(self.preset), self.batch) {
            (ScriptMode::PerStatement, Batch::Statements) => locate(text, unit_statements(text, &dialect)),
            _ => placed_statements(self.preset, self.batch, text)?,
        };
        if stmts.is_empty() {
            return Ok(());
        }
        let stmts: Vec<(String, usize, u32)> = stmts.into_iter().map(|(s, at)| (s, at, steps::line_at(text, at))).collect();
        // `Whole` presets split the script here: as the app would, editor
        // runs go on after errors (`out.continue_on_error`) and report each
        // statement live (`out.progress_sink`).
        let own = out.current_statement.is_none();
        // `USE db` / `DATABASE db` switch the database the explorer lists.
        let tracks_database = !self.preset.databases_label.is_empty();
        let database = self.database.clone();
        let fork = out.fork();
        let (local, err, changed) = self
            .run(move |c, slot| {
                let mut local = fork;
                // Whether the last statement that ran changed something
                // (`None`: it ended the transaction itself).
                let mut changed = Some(false);
                let mut database = database;
                for (i, (s, at, line)) in stmts.iter().enumerate() {
                    let step = steps::Step::start(&mut local, own, i, *at, *line);
                    let before = local.results.len();
                    let r = run_one(c, slot, s, max_rows, &mut local);
                    if r.is_ok() {
                        match leading_keyword(s, &dialect).as_deref() {
                            Some("commit" | "rollback") => changed = None,
                            Some("use" | "database") if tracks_database => {
                                let now = c.info_string(ffi::SQL_DATABASE_NAME).trim().to_string();
                                if !now.is_empty() && now != database {
                                    database = now.clone();
                                    local.database = Some(now);
                                }
                            }
                            _ if local.results[before..].iter().any(|r| r.columns.is_empty()) => changed = Some(true),
                            _ => {}
                        }
                    }
                    if let Err(e) = step.end(&mut local, r) {
                        return Ok((local, Some(e), changed));
                    }
                }
                Ok((local, None, changed))
            })
            .await?;
        match changed {
            None => self.dirty = false,
            Some(true) if self.manual => self.dirty = true,
            _ => {}
        }
        if let Some(db) = &local.database {
            self.database = db.clone();
        }
        out.merge(local);
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        if !has_transactions(self.preset) {
            return Ok(None);
        }
        Ok(Some(if self.manual && self.dirty { TxState::Open } else { TxState::Idle }))
    }

    /// `SQL_ATTR_AUTOCOMMIT`: off, the driver keeps a transaction open
    /// until Commit / Rollback (`SQLEndTran`).
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if !on && !has_transactions(self.preset) {
            return Err(Error::Unsupported(format!("{} no tiene transacciones", self.preset.name)));
        }
        self.run(move |c, _| c.set_autocommit(on)).await?;
        self.manual = !on;
        if on {
            // Turning autocommit on commits what was pending.
            self.dirty = false;
        }
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        if self.manual {
            self.run(|c, _| c.end_tran(true)).await?;
        }
        self.dirty = false;
        Ok(())
    }

    async fn rollback(&mut self) -> Result<()> {
        if self.manual {
            self.run(|c, _| c.end_tran(false)).await?;
        }
        self.dirty = false;
        Ok(())
    }

    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let dialect = explain::dialect(self.preset.id, &self.dbms);
        if let explain::Dialect::Unsupported(why) = dialect {
            return Err(Error::Unsupported(why.into()));
        }
        let stmts = preset_statements(self.preset, self.batch, text)?;
        if stmts.is_empty() {
            return Ok(());
        }
        let fork = out.fork();
        let (local, err) = self
            .run(move |c, slot| {
                let mut local = fork;
                let r = explain::explain_all(dialect, c, slot, &stmts, analyze, max_rows, &mut local);
                Ok((local, r.err()))
            })
            .await?;
        out.merge(local);
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let inner = self.inner.clone();
        let api = self.api;
        Some(Arc::new(move || inner.slot.cancel(api)))
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let preset = self.preset;
        let esc = self.escape.clone();
        let raw = self.run(move |c, slot| catalog_schema(c, slot, preset, &esc)).await?;
        let mut tables = build_schema(preset, raw);
        // CHECKs and INCLUDE columns from the engine's own catalog.
        let e = design::eng(preset);
        for sql in structure::checks_sql(e) {
            match self.query(sql.to_string(), Vec::new()).await {
                Ok(rows) => structure::attach_checks(&mut tables, &rows, preset.has_schemas),
                Err(e) => tracing::debug!("odbc: CHECK constraints not read: {e}"),
            }
        }
        if let Some(sql) = structure::include_sql(e) {
            match self.query(sql.to_string(), Vec::new()).await {
                Ok(rows) => structure::attach_includes(&mut tables, &rows),
                Err(e) => tracing::debug!("odbc: INCLUDE columns not read: {e}"),
            }
        }
        Ok(tables)
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        let preset = self.preset;
        if !index_usage::supported(preset) {
            return Ok(None);
        }
        let esc = self.escape.clone();
        let (schema, name) = (table.schema().map(str::to_string), table.name.clone());
        let raw = self.run(move |c, slot| catalog_table(c, slot, preset, &esc, schema, name)).await?;
        let mut tables = build_schema(preset, raw);
        let e = design::eng(preset);
        if let Some(sql) = structure::include_sql(e) {
            match self.query(sql.to_string(), Vec::new()).await {
                Ok(rows) => structure::attach_includes(&mut tables, &rows),
                Err(e) => tracing::debug!("odbc: INCLUDE columns not read: {e}"),
            }
        }
        let usage = match index_usage::counters_sql(e) {
            Some(sql) => match self.query(sql.to_string(), index_usage::counters_params(e, table.schema(), &table.name)).await {
                Ok(rows) => Some(index_usage::parse_counters(&rows)),
                Err(err) => {
                    tracing::debug!("odbc: index counters not read: {err}");
                    None
                }
            },
            None => None,
        };
        let since = match (usage.is_some(), index_usage::since_sql(e)) {
            (true, Some(sql)) => self.query(sql.to_string(), Vec::new()).await.ok().and_then(|r| r.into_iter().next()).and_then(|r| col(&r, 0)),
            _ => None,
        };
        let report = index_usage::assemble(preset, tables.first(), usage.as_deref(), since);
        let Some((sql, params)) = index_usage::disabled_sql(e, table.schema(), &table.name) else {
            return Ok(Some(report));
        };
        match self.query(sql.to_string(), params).await {
            Ok(rows) => Ok(Some(index_usage::mark_disabled(report, &rows))),
            Err(err) => {
                tracing::debug!("odbc: disabled indexes not read: {err}");
                Ok(Some(report))
            }
        }
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.database_ddl("CREATE", name).await
    }

    async fn create_database_choices(&mut self) -> Result<Vec<dbine_driver::FieldChoices>> {
        self.create_database_choices_impl().await
    }

    /// The generic preset keeps the plain create (its quoting comes from
    /// the driver); Sybase ASE and Netezza take their options.
    async fn create_database_with(&mut self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        if !matches!(design::eng(self.preset), design::Eng::Ase | design::Eng::Netezza) {
            if options.values().all(|v| v.trim().is_empty()) {
                return self.create_database(name).await;
            }
            return Err(Error::Unsupported(format!("{} no admite opciones al crear una base", self.preset.name)));
        }
        self.create_database_with_impl(name, options).await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        self.database_ddl("DROP", name).await
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        if let Some(why) = monitor::unsupported_reason(self.preset) {
            return Err(Error::Unsupported(why.into()));
        }
        let preset = self.preset;
        let ctx = monitor::Ctx {
            dbms: self.dbms.clone(),
            version: self.version.clone(),
            database: self.database.clone(),
            file: self.file.clone(),
        };
        self.run(move |c, slot| Ok(monitor::collect(preset, &ctx, &mut ConnSource { c, slot }))).await
    }

    async fn blocking(&mut self) -> Result<Vec<dbine_driver::BlockedSession>> {
        let preset = self.preset;
        if !blocking::supports_blocking(preset) {
            return Err(Error::Unsupported("este motor no informa bloqueos entre sesiones".into()));
        }
        self.run(move |c, slot| blocking::collect(preset, &mut ConnSource { c, slot })).await
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        match security::dialect(self.preset) {
            Some(d) => security::principals(self, d).await,
            None => Err(Error::Unsupported(security::unsupported(self.preset).into())),
        }
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        match security::dialect(self.preset) {
            Some(d) => security::grants(self, d, principal).await,
            None => Err(Error::Unsupported(security::unsupported(self.preset).into())),
        }
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        backup::history(self, self.preset, database).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        if !blocking::supports_kill(self.preset) {
            return Err(Error::Unsupported("este motor no permite terminar sesiones desde DBine".into()));
        }
        let sql = blocking::kill_sql(self.preset, id)?;
        self.run(move |c, slot| c.stmt(slot)?.exec(&sql).map(|_| ())).await
    }

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        let preset = self.preset;
        if let Some(why) = processes::unsupported_reason(preset) {
            return Err(Error::Unsupported(why.into()));
        }
        self.run(move |c, slot| processes::collect(preset, &mut ConnSource { c, slot })).await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        let (preset, id) = (self.preset, id.to_string());
        self.run(move |c, slot| processes::cancel(preset, &mut ConnSource { c, slot }, &id)).await
    }

    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        let sql = transfer::select_sql(self.quote, spec);
        let sizes = transfer::sizes_reliable(self.preset);
        let wanted = spec.columns.as_ref().map(Vec::len);
        self.run(move |c, slot| transfer::read(c, slot, &sql, sizes, wanted, sink)).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        _columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::bulk_load(self, spec, source, progress).await
    }

    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }
}

/// The monitor's queries on the session's connection: the first result
/// set of each, every column as text.
struct ConnSource<'a> {
    c: &'a Conn,
    slot: &'a StmtSlot,
}

impl monitor::Source for ConnSource<'_> {
    fn query(&mut self, sql: &str) -> Result<monitor::Set> {
        let st = self.c.stmt(self.slot)?;
        st.exec(sql)?;
        loop {
            let n = st.num_cols()?;
            if n > 0 {
                let cols = st.describe(n)?.into_iter().map(|c| c.name).collect();
                return Ok(monitor::Set { cols, rows: st.text_rows()? });
            }
            if !st.more_results()? {
                return Ok(monitor::Set::default());
            }
        }
    }
}

impl OdbcSession {
    /// CREATE / DROP DATABASE where the preset offers it. Sybase ASE runs
    /// them from `master` and then goes back to the session's database.
    async fn database_ddl(&mut self, verb: &'static str, name: &str) -> Result<()> {
        let caps = design::capabilities(self.preset);
        if !(if verb == "CREATE" { caps.create_database } else { caps.drop_database }) {
            return Err(Error::Unsupported(format!("{} no crea ni borra bases desde DBine", self.preset.name)));
        }
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Query("Falta el nombre de la base".into()));
        }
        let q = |n: &str| dbine_driver::sql::quote_ident(self.quote, n);
        let stmt = format!("{verb} DATABASE {}", q(name));
        let stmts = if design::eng(self.preset) == design::Eng::Ase {
            vec!["USE master".to_string(), stmt, format!("USE {}", q(&self.database))]
        } else {
            vec![stmt]
        };
        self.run(move |c, slot| {
            let mut err = None;
            for s in &stmts {
                let r = c.stmt(slot).and_then(|st| st.exec(s).map(|_| ()));
                // Keep going to the final USE even when the DDL failed.
                if let Err(e) = r {
                    err.get_or_insert(e);
                }
            }
            err.map_or(Ok(()), Err)
        })
        .await
    }
}

/// What the ODBC catalog functions return for the tables of a database.
struct RawSchema {
    /// SQLTables rows of base tables.
    tables: Rows,
    /// SQLColumns rows, all tables.
    columns: Rows,
    /// Per table (index into `tables`): SQLPrimaryKeys, SQLForeignKeys, SQLStatistics.
    per_table: Vec<(Rows, Rows, Rows)>,
}

/// SQLTables, then SQLColumns once per schema (a pattern for every table)
/// and SQLPrimaryKeys / SQLForeignKeys / SQLStatistics per table (they take
/// no patterns). Keys and indexes are best effort: a driver that fails
/// them leaves the table without.
fn catalog_schema(c: &Conn, slot: &StmtSlot, preset: &'static Preset, esc: &str) -> Result<RawSchema> {
    let all = {
        let st = c.stmt(slot)?;
        st.tables(None, Some("%"), Some("%"), None)?;
        st.text_rows()?
    };
    let tables: Rows = all
        .into_iter()
        .filter(|r| matches!(col(r, 3).unwrap_or_default().to_ascii_uppercase().as_str(), "TABLE" | "BASE TABLE"))
        .filter(|r| !col(r, 1).is_some_and(|s| preset.is_system_schema(&s)))
        .filter(|r| col(r, 2).is_some())
        .collect();
    let mut schemas: Vec<Option<String>> = tables.iter().map(|r| col(r, 1)).collect();
    schemas.sort();
    schemas.dedup();
    let mut columns: Rows = Vec::new();
    for sc in &schemas {
        let batch = c.stmt(slot).and_then(|st| {
            let sp = sc.as_deref().map(|s| connstr::escape_pattern(s, esc));
            st.columns(sp.as_deref(), "%")?;
            st.text_rows()
        });
        match batch {
            Ok(rows) => columns.extend(rows),
            Err(e) => {
                // Some drivers refuse a table pattern: one call per table.
                tracing::debug!("odbc: SQLColumns per schema failed, going per table: {e}");
                for t in tables.iter().filter(|r| col(r, 1) == *sc) {
                    let name = col(t, 2).unwrap_or_default();
                    let st = c.stmt(slot)?;
                    let sp = sc.as_deref().map(|s| connstr::escape_pattern(s, esc));
                    st.columns(sp.as_deref(), &connstr::escape_pattern(&name, esc))?;
                    columns.extend(st.text_rows()?);
                }
            }
        }
        if slot.is_cancelled() {
            return Err(Error::Query("Cancelado".into()));
        }
    }
    let mut per_table = Vec::with_capacity(tables.len());
    for t in &tables {
        per_table.push(table_keys(c, slot, preset, col(t, 1).as_deref(), &col(t, 2).unwrap_or_default()));
        if slot.is_cancelled() {
            return Err(Error::Query("Cancelado".into()));
        }
    }
    Ok(RawSchema { tables, columns, per_table })
}

/// A table's SQLPrimaryKeys, SQLForeignKeys and SQLStatistics rows (empty
/// where the preset has none or the driver refuses the call).
fn table_keys(c: &Conn, slot: &StmtSlot, preset: &'static Preset, schema: Option<&str>, name: &str) -> (Rows, Rows, Rows) {
    let catalog = |what: &str, f: &dyn Fn(&odbc::Stmt) -> Result<()>| -> Rows {
        c.stmt(slot)
            .and_then(|st| {
                f(&st)?;
                st.text_rows()
            })
            .unwrap_or_else(|e| {
                tracing::debug!("odbc: {what} failed for {name}: {e}");
                Vec::new()
            })
    };
    let pk = catalog("SQLPrimaryKeys", &|st| st.primary_keys(schema, name));
    let fk = if design::reports_foreign_keys(preset) { catalog("SQLForeignKeys", &|st| st.foreign_keys(schema, name)) } else { Vec::new() };
    let ix = if design::has_indexes(preset) { catalog("SQLStatistics", &|st| st.statistics(schema, name)) } else { Vec::new() };
    (pk, fk, ix)
}

/// [`catalog_schema`] for one table: its columns and keys.
fn catalog_table(c: &Conn, slot: &StmtSlot, preset: &'static Preset, esc: &str, schema: Option<String>, name: String) -> Result<RawSchema> {
    let columns = {
        let st = c.stmt(slot)?;
        let sp = schema.as_deref().map(|s| connstr::escape_pattern(s, esc));
        st.columns(sp.as_deref(), &connstr::escape_pattern(&name, esc))?;
        st.text_rows()?
    };
    let keys = table_keys(c, slot, preset, schema.as_deref(), &name);
    // An SQLTables row: catalog, schema, name, type, remarks.
    let tables = vec![vec![None, schema, Some(name), Some("TABLE".to_string()), None]];
    Ok(RawSchema { tables, columns, per_table: vec![keys] })
}

/// SQLForeignKeys UPDATE_RULE / DELETE_RULE.
fn fk_rule(v: Option<String>) -> Option<String> {
    match v.as_deref().map(str::trim) {
        Some("0") => Some("CASCADE".into()),
        Some("2") => Some("SET NULL".into()),
        Some("4") => Some("SET DEFAULT".into()),
        // 1 RESTRICT, 3 NO ACTION: the default.
        _ => None,
    }
}

fn int(r: &[Option<String>], i: usize) -> i64 {
    col(r, i).and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn build_schema(preset: &'static Preset, raw: RawSchema) -> Vec<TableSchema> {
    let strip = design::strips_identity_suffix(preset);
    let schema_of = |s: Option<String>| if preset.has_schemas { s.filter(|s| !s.is_empty()) } else { None };
    let mut out = Vec::with_capacity(raw.tables.len());
    for (t, (pk, fk, ix)) in raw.tables.iter().zip(raw.per_table) {
        let (schema, name) = (col(t, 1), col(t, 2).unwrap_or_default());
        let mut cols: Vec<(i64, usize, ColumnDef)> = raw
            .columns
            .iter()
            .enumerate()
            .filter(|(_, r)| col(r, 2).as_deref() == Some(name.as_str()) && (col(r, 1) == schema || col(r, 1).is_none()))
            .map(|(i, r)| {
                let type_name = col(r, 5).unwrap_or_default();
                let sql_type = col(r, 4).and_then(|s| s.trim().parse::<i16>().ok()).unwrap_or(0);
                let lower = type_name.to_ascii_lowercase();
                let auto = lower.contains("identity") || lower.contains("serial") || lower.contains("auto_increment");
                let mut base = type_name.clone();
                if strip && auto {
                    for suffix in [" identity", " auto_increment"] {
                        if lower.ends_with(suffix) {
                            base.truncate(base.len() - suffix.len());
                        }
                    }
                }
                let def = ColumnDef {
                    name: col(r, 3).unwrap_or_default(),
                    data_type: format_type(&base, sql_type, num(r, 6), num(r, 8)),
                    nullable: col(r, 10).as_deref().map(str::trim) != Some("0"),
                    default_value: col(r, 12).filter(|d| !d.trim().is_empty()),
                    auto_increment: auto,
                    comment: col(r, 11).filter(|d| !d.trim().is_empty()),
                    ..Default::default()
                };
                (int(r, 16), i, def)
            })
            .collect();
        cols.sort_by_key(|(ord, i, _)| (*ord, *i));
        // Several catalogs may report the same table (no catalog filter).
        let mut seen = std::collections::HashSet::new();
        let columns: Vec<ColumnDef> = cols.into_iter().map(|(_, _, c)| c).filter(|c| seen.insert(c.name.clone())).collect();

        let mut pk_rows: Vec<(i64, String, Option<String>)> =
            pk.iter().filter_map(|r| Some((int(r, 4), col(r, 3)?, col(r, 5)))).collect();
        pk_rows.sort();
        pk_rows.dedup_by(|a, b| a.1 == b.1);
        let primary_key = (!pk_rows.is_empty()).then(|| KeyDef {
            name: pk_rows[0].2.clone().filter(|n| !n.is_empty()),
            columns: pk_rows.iter().map(|r| r.1.clone()).collect(),
        });

        // Foreign keys: grouped by name, else by referenced table and a
        // KEY_SEQ that restarts at 1.
        let mut foreign_keys: Vec<ForeignKeyDef> = Vec::new();
        for r in &fk {
            let (Some(ref_table), Some(pcol), Some(fcol)) = (col(r, 2), col(r, 3), col(r, 7)) else { continue };
            let fk_name = col(r, 11).filter(|n| !n.is_empty());
            let ref_schema = schema_of(col(r, 1));
            let seq = int(r, 8);
            let existing = foreign_keys.iter_mut().rev().find(|f| match (&fk_name, &f.name) {
                (Some(a), Some(b)) => a == b,
                (None, None) => f.ref_table == ref_table && f.ref_schema == ref_schema && seq > 1,
                _ => false,
            });
            match existing {
                Some(f) => {
                    f.columns.push(fcol);
                    f.ref_columns.push(pcol);
                }
                None => foreign_keys.push(ForeignKeyDef {
                    name: fk_name,
                    columns: vec![fcol],
                    ref_schema,
                    ref_table,
                    ref_columns: vec![pcol],
                    on_update: fk_rule(col(r, 9)),
                    on_delete: fk_rule(col(r, 10)),
                }),
            }
        }

        // Indexes: TYPE 0 rows are table statistics; the primary key's own
        // index is left out.
        let mut ix_rows: Vec<(String, i64, String, bool, i64, Option<String>)> = ix
            .iter()
            .filter(|r| int(r, 6) != 0)
            .filter_map(|r| {
                Some((col(r, 5)?, int(r, 7), col(r, 8)?, col(r, 3).as_deref().map(str::trim) == Some("0"), int(r, 6), col(r, 12)))
            })
            .collect();
        ix_rows.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        let mut indexes: Vec<IndexDef> = Vec::new();
        for (iname, _, cname, unique, ty, filter) in ix_rows {
            match indexes.last_mut() {
                Some(last) if last.name == iname => last.columns.push(cname),
                _ => indexes.push(IndexDef {
                    name: iname,
                    columns: vec![cname],
                    unique,
                    kind: (ty == 1).then(|| "CLUSTERED".to_string()),
                    filter: filter.filter(|f| !f.trim().is_empty()),
                    ..Default::default()
                }),
            }
        }
        if let Some(k) = &primary_key {
            indexes.retain(|i| k.name.as_deref() != Some(i.name.as_str()) && !(i.unique && i.columns == k.columns));
        }

        out.push(TableSchema {
            kind: kinds::TABLE.into(),
            schema: schema_of(schema),
            name,
            columns,
            primary_key,
            foreign_keys,
            indexes,
            comment: col(t, 4).filter(|d| !d.trim().is_empty()),
            ..Default::default()
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Con opción de otorgar" is offered on the new schema's grants
    /// exactly where the engine writes them (`SchemaSpec::grant_option`).
    #[test]
    fn schema_grant_option_matches_the_script() {
        for d in crate::drivers() {
            let Some(spec) = d.schema_spec() else { continue };
            let Some(p) = spec.privileges.first() else { continue };
            let grant = |grantable| d.schema_grant_script(None, "VENTAS", &[p.to_string()], "ana", grantable);
            assert!(grant(false).is_ok(), "{}", d.info().id);
            assert_eq!(grant(true).is_ok(), spec.grant_option, "{}: {:?}", d.info().id, grant(true));
        }
    }

    #[test]
    fn every_preset_describes_itself() {
        let ds = drivers();
        assert!(ds.len() >= 18);
        for d in &ds {
            let i = d.info();
            assert_eq!(i.language, Language::Sql);
            assert!(i.fields.iter().any(|f| f.key == "read_only"));
            if i.id != "odbc" {
                assert!(i.fields.iter().any(|f| f.key == "odbc_driver"), "{}", i.id);
            }
        }
    }

    #[test]
    fn browse_per_limit_style() {
        assert_eq!(browse(Quote::Double, LimitStyle::FetchFirst, Some("S"), "T", 5), "SELECT *\nFROM \"S\".\"T\"\nFETCH FIRST 5 ROWS ONLY");
        assert_eq!(browse(Quote::Bracket, LimitStyle::Top, Some("dbo"), "t", 5), "SELECT TOP 5 *\nFROM [dbo].[t]");
        assert_eq!(browse(Quote::Double, LimitStyle::First, None, "t", 5), "SELECT FIRST 5 *\nFROM \"t\"");
        assert_eq!(browse(Quote::Backtick, LimitStyle::Limit, Some("db"), "t", 5), "SELECT *\nFROM `db`.`t`\nLIMIT 5");
    }

    #[test]
    fn limit_is_guessed_from_the_dbms() {
        assert_eq!(guess_limit("Microsoft SQL Server"), LimitStyle::Top);
        assert_eq!(guess_limit("DB2/LINUXX8664"), LimitStyle::FetchFirst);
        assert_eq!(guess_limit("PostgreSQL"), LimitStyle::Limit);
    }

    #[test]
    fn types_carry_their_size() {
        assert_eq!(format_type("nvarchar", ffi::SQL_WVARCHAR, Some(50), None), "nvarchar(50)");
        assert_eq!(format_type("varchar", ffi::SQL_VARCHAR, Some(0), None), "varchar");
        assert_eq!(format_type("decimal", ffi::SQL_DECIMAL, Some(18), Some(2)), "decimal(18,2)");
        assert_eq!(format_type("int", ffi::SQL_INTEGER, Some(10), Some(0)), "int");
    }

    #[test]
    fn teradata_keeps_macros_and_replaced_procedures_whole() {
        let p = preset("teradata");
        assert_eq!(script_mode(p), ScriptMode::Whole);
        let script = "CREATE MACRO m1 AS (\n SELECT 1; SELECT 2; );\n\
                      REPLACE PROCEDURE p1 ()\nBEGIN\n DECLARE x INTEGER;\n SET x = 1;\n SELECT 'a;)' ;\nEND;\n\
                      -- ( not counted\nSELECT (1);\nREPLACE VIEW v AS SELECT 1 AS a;";
        let st = preset_statements(p, Batch::Statements, script).unwrap();
        assert_eq!(st.len(), 4, "{st:#?}");
        assert!(st[0].starts_with("CREATE MACRO") && st[0].ends_with(')') && st[0].contains("SELECT 2"));
        assert!(st[1].starts_with("REPLACE PROCEDURE") && st[1].ends_with("END"));
        assert!(st[2].ends_with("SELECT (1)"));
        assert_eq!(st[3], "REPLACE VIEW v AS SELECT 1 AS a");
        // Other presets and "Todo el texto de una vez" are unchanged.
        assert_eq!(preset_statements(p, Batch::Script, script).unwrap().len(), 1);
    }

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    #[test]
    fn scripts_follow_each_engine_tool() {
        assert_eq!(script_mode(preset("db2")), ScriptMode::PerStatement);
        assert_eq!(script_mode(preset("sybase")), ScriptMode::Batches);
        assert_eq!(script_mode(preset("informix")), ScriptMode::Whole);
        assert_eq!(script_mode(preset("odbc")), ScriptMode::Whole);
        assert!(continues_on_error(preset("db2")) && !continues_on_error(preset("hive")));
        assert!(has_transactions(preset("teradata")) && !has_transactions(preset("hive")));
        // Every id named in the lists is a real preset.
        for id in WHOLE_SCRIPT {
            preset(id);
        }

        // DB2: the terminator directive keeps a compound statement whole,
        // and the unit the app sends isn't cut again by the driver.
        let d = script_dialect(preset("db2"));
        let script = "--#SET TERMINATOR @\nBEGIN ATOMIC\n  DECLARE x INT;\n  SET x = 1;\nEND@\n--#SET TERMINATOR ;\nVALUES 1;";
        let units: Vec<_> = split_script(script, &d).into_iter().filter(|u| u.kind != StatementKind::ClientCommand).collect();
        assert_eq!(units.len(), 2, "{units:?}");
        assert_eq!(unit_statements(&units[0].text, &d), vec![units[0].text.clone()]);
        assert_eq!(unit_statements("values 1; values 2", &d).len(), 2);
        let proc = "CREATE PROCEDURE p() LANGUAGE SQL BEGIN DECLARE x INT; SET x = 1; END";
        assert_eq!(unit_statements(&format!("{proc};\nVALUES 1"), &d), vec![proc.to_string(), "VALUES 1".to_string()]);

        // Hive: backslash escapes.
        let d = script_dialect(preset("hive"));
        assert_eq!(unit_statements("select 'it\\'s;' from t; select 2", &d).len(), 2);
        // Vertica: dollar-quoted bodies.
        let d = script_dialect(preset("vertica"));
        assert_eq!(unit_statements("CREATE PROCEDURE p() AS $$ BEGIN PERFORM 1; END $$; SELECT 1", &d).len(), 2);
    }

    #[test]
    fn diagnostics_become_errors_and_messages() {
        use odbc::Diag;
        let d = |state: &str, native: i32, m: &str| Diag { state: state.into(), native, message: m.into() };
        let e = odbc::query_error(&[d("01000", 0, "printed"), d("42S02", -204, "undefined name")]).to_script_error();
        assert_eq!((e.sqlstate.as_deref(), e.code.as_deref(), e.fatal), (Some("42S02"), Some("-204"), false));
        assert!(odbc::query_error(&[d("08S01", 0, "link down")]).ends_script());
        assert!(matches!(odbc::query_error(&[d("HY008", 0, "cancelled")]), Error::Cancelled));
        assert_eq!(d("01000", 0, "x").to_message().level, dbine_driver::MessageLevel::Info);
        let w = d("01003", 8153, "NULL eliminated").to_message();
        assert_eq!((w.level, w.code.as_deref()), (dbine_driver::MessageLevel::Warning, Some("8153")));
    }

    #[test]
    fn go_and_versions() {
        assert_eq!(split_go("select 1\nGO\n go \nselect 2").unwrap(), vec!["select 1", "select 2"]);
        // The shared lexer: GO N, GO with a comment, GO inside comments and strings.
        assert_eq!(split_go("select 1\nGO 3\nselect 2\ngo -- end").unwrap(), vec!["select 1", "select 1", "select 1", "select 2"]);
        assert_eq!(split_go("/*\nGO\n*/ select 1\nGO").unwrap().len(), 1);
        assert_eq!(split_go("select 'a\nGO\nb'").unwrap().len(), 1);
        assert!(split_go("select 1\nGO 0").is_err());
        assert_eq!(strip_version("p;1"), "p");
        assert_eq!(strip_version("a;b"), "a;b");
    }

    /// A `Whole` preset's statements know where they start, so a failure
    /// in a later one gets its own line.
    #[test]
    fn whole_script_statements_are_placed() {
        let script = "-- head\nselect 1;\n\n/* c */ select 2;\nselect 3";
        let got = placed_statements(preset("informix"), Batch::Statements, script).unwrap();
        let lines: Vec<u32> = got.iter().map(|(_, at)| steps::line_at(script, *at)).collect();
        assert_eq!(got.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), ["select 1", "select 2", "select 3"]);
        assert_eq!(lines, [2, 4, 5]);
        let td = "CREATE MACRO m AS (SELECT 1; SELECT 2;);\nSELECT 3;";
        let got = placed_statements(preset("teradata"), Batch::Statements, td).unwrap();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(&td[got[1].1..], "SELECT 3;");
        assert_eq!(placed_statements(preset("odbc"), Batch::Script, " x ").unwrap(), vec![(" x ".to_string(), 0)]);
        assert_eq!(locate("a; b; a", vec!["a".into(), "b".into(), "a".into(), "zz".into()]), vec![
            ("a".to_string(), 0),
            ("b".to_string(), 3),
            ("a".to_string(), 6),
            ("zz".to_string(), 7)
        ]);
    }
}
