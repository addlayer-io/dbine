//! Power BI and Microsoft Fabric semantic models, through the public REST
//! APIs only (no XMLA, no TOM): browse a model's tables, measures,
//! relationships and the rest, run DAX queries read-only, and manage the
//! model's refresh (history, refresh now, schedules), its users'
//! permissions, its name and description, and delete it, each on
//! connections that aren't read-only. The design is
//! `.plan/spec-powerbi.md`; the modules' contracts are
//! `.plan/powerbi-interfaces.md`.
//!
//! Each database is a model: `list_databases` names them `<workspace> ›
//! <model>` (or just `<model>` with the connection's workspace). Queries go
//! to `executeQueries` (JSON, any license) or `executeDaxQueries` (Arrow,
//! capacity), whichever the model takes ([`query`]). Metadata comes from
//! the richest tier the user can read ([`meta`]): `INFO` functions, the
//! Fabric definition API ([`definition`]) or `COLUMNSTATISTICS()`.
//!
//! This file only declares the driver and hands each contract method to
//! the module that owns it; the modules hold the work.

use dbine_driver::async_trait;
use dbine_driver::sql::{ScriptDialect, ScriptMode, ScriptStatement};
use dbine_driver::{
    kinds, Capabilities, ColumnInfo, ConnectionConfig, DbObject, Driver, DriverInfo, Family, Field,
    FieldKind, Language, ObjectKindInfo, ObjectRef, QueryOutcome, Result, Session, TlsSupport,
};
use std::collections::BTreeMap;
use std::sync::Arc;

pub mod admin;
pub mod auth;
pub mod catalog;
pub mod config;
pub mod dax;
pub mod definition;
pub mod dependencies;
pub mod functions;
pub mod health;
pub mod http;
pub mod meta;
pub mod monitor;
pub mod objects;
pub mod properties;
pub mod query;
pub mod refresh;
pub mod search;
pub mod security;
pub mod transfer;

/// Imported, DirectQuery, Direct Lake and calculated tables (not the
/// calculation groups' tables nor the auto date/time ones).
pub const TABLE: &str = kinds::TABLE;
/// A calculation group: its table lists its items.
pub const CALCULATION_GROUP: &str = "calculation_group";
/// A measure; its `parent` is its home table.
pub const MEASURE: &str = "measure";
/// A hierarchy, named `'Table'[Hierarchy]`; its columns are its levels.
pub const HIERARCHY: &str = "hierarchy";
/// A relationship, named `'Sales'[CustomerKey] → 'Customer'[CustomerKey]`.
pub const RELATIONSHIP: &str = "relationship";
/// A DAX user-defined function of the model.
pub const FUNCTION: &str = kinds::FUNCTION;
/// A row-level security role.
pub const ROLE: &str = "role";
pub const PERSPECTIVE: &str = "perspective";
/// A shared M expression or parameter.
pub const EXPRESSION: &str = "expression";

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(PowerBiDriver { info: info() })]
}

/// The connection form (spec §2). Keys other than `username`, `database`
/// and `read_only` go to `cfg.options` ([`config::Settings`] reads them).
fn fields() -> Vec<Field> {
    let auth_modes = &[
        "entra_mfa",
        "entra_sp",
        "entra_sp_cert",
        "entra_default",
        "entra_msi",
    ];
    vec![
        Field::new(
            "auth_mode",
            "Autenticación",
            FieldKind::Select(vec![
                ("entra_mfa", "Microsoft Entra ID (navegador, con MFA)"),
                ("entra_sp", "Entidad de servicio (Client ID y secreto)"),
                ("entra_sp_cert", "Entidad de servicio (Client ID y certificado)"),
                (
                    "entra_default",
                    "Credencial predeterminada (variables de entorno, identidad administrada, Azure CLI)",
                ),
                ("entra_msi", "Identidad administrada de Azure"),
                ("token", "Token de acceso"),
            ]),
        )
        .default_value("entra_mfa"),
        Field::new("tenant", "Inquilino (tenant)", FieldKind::Text)
            .placeholder("contoso.onmicrosoft.com o GUID")
            .help("Obligatorio con una entidad de servicio. Vacío: el inquilino de la cuenta con la que inicies sesión.")
            .when("auth_mode", auth_modes),
        Field::username()
            .placeholder("ana@contoso.com")
            .help("Opcional: la cuenta con la que se inicia sesión. Las conexiones sin usuario de la misma nube, inquilino y aplicación comparten una sola cuenta: para usar otra, escribila acá.")
            .when("auth_mode", &["entra_mfa"]),
        Field::new("client_id", "Client ID", FieldKind::Text)
            .placeholder("00000000-0000-0000-0000-000000000000")
            .help("El Client ID de la entidad de servicio, o el de una identidad administrada asignada por el usuario (vacío: la del sistema).")
            .when("auth_mode", &["entra_sp", "entra_sp_cert", "entra_msi"]),
        Field::new("client_secret", "Secreto del cliente", FieldKind::Password)
            .secret()
            .when("auth_mode", &["entra_sp"]),
        Field::new("client_certificate", "Certificado", FieldKind::File)
            .placeholder("certificado.pem o .pfx")
            .help("Un PEM con el certificado y su clave privada (PKCS#8, con o sin contraseña), o un PFX / PKCS#12. La clave tiene que ser RSA. El certificado sin la clave se sube en Certificados y secretos del registro de la aplicación.")
            .when("auth_mode", &["entra_sp_cert"]),
        Field::new(
            "client_certificate_password",
            "Contraseña del certificado",
            FieldKind::Password,
        )
        .secret()
        .help("La del PFX o la de la clave privada cifrada. Vacía si no tiene.")
        .when("auth_mode", &["entra_sp_cert"]),
        Field::new("access_token", "Token de acceso", FieldKind::Textarea)
            .secret()
            .help("Un token para Power BI de la nube elegida. DBine no lo renueva: cuando vence hay que pegar otro.")
            .when("auth_mode", &["token"]),
        Field::new("workspace", "Área de trabajo", FieldKind::Text)
            .placeholder("(todas)")
            .help("Opcional: nombre o ID. Con muchas áreas de trabajo conviene indicar una: listar todas pide una llamada por área."),
        Field {
            label: "Modelo semántico",
            placeholder: "(opcional) nombre o ID",
            help: "El modelo que se abre al conectar: su nombre, o su ID para un modelo compartido con vos sin acceso a su área de trabajo.",
            ..Field::database()
        },
        Field::read_only()
            .default_value("true")
            .help("Las consultas DAX nunca cambian el modelo. Desmarcado permite actualizar el modelo, cambiar sus programaciones y sus permisos, renombrarlo y borrarlo."),
        Field::new(
            "cloud",
            "Nube",
            FieldKind::Select(vec![
                ("public", "Pública (api.powerbi.com)"),
                ("gcc", "Gobierno de EE. UU. (GCC)"),
                ("gcc_high", "Gobierno de EE. UU. (GCC High)"),
                ("dod", "Gobierno de EE. UU. (DoD)"),
                ("china", "China (21Vianet)"),
            ]),
        )
        .default_value("public")
        .help("GCC High, DoD y China necesitan una aplicación propia registrada en esa nube (Aplicación para iniciar sesión: Propia).")
        .advanced(),
        Field::new(
            "client_app",
            "Aplicación para iniciar sesión",
            FieldKind::Select(vec![
                ("dbine", "DBine (predeterminada)"),
                ("azure_cli", "Cliente de Azure CLI"),
                ("custom", "Propia (Client ID)"),
            ]),
        )
        .default_value("dbine")
        .help("Si tu inquilino todavía no aprobó la aplicación de DBine, el cliente de Azure CLI suele estar permitido.")
        .when("auth_mode", &["entra_mfa"])
        .advanced(),
        Field::new("app_client_id", "Client ID de la aplicación", FieldKind::Text)
            .placeholder("00000000-0000-0000-0000-000000000000")
            .help("Una aplicación cliente pública con permisos delegados de Power BI y la URI de redirección http://localhost.")
            .when("client_app", &["custom"])
            .advanced(),
        Field::new(
            "query_api",
            "API de consultas",
            FieldKind::Select(vec![
                ("auto", "Automática"),
                ("json", "executeQueries (JSON, cualquier licencia)"),
                ("arrow", "executeDaxQueries (Arrow, capacidad Premium o Fabric)"),
            ]),
        )
        .default_value("auto")
        .help("Automática usa executeDaxQueries donde el modelo lo admite y si no executeQueries, que devuelve como máximo 100.000 filas por consulta.")
        .advanced(),
        Field::new("query_timeout", "Tiempo máximo de consulta (s)", FieldKind::Number)
            .default_value("600")
            .placeholder("600")
            .advanced(),
        Field::new("rls_user", "Probar como usuario (RLS)", FieldKind::Text)
            .placeholder("ana@contoso.com")
            .help("Las consultas se filtran con la seguridad por filas de ese usuario. Hace falta permiso de escritura en el modelo (executeQueries) o ser administrador del área de trabajo (executeDaxQueries).")
            .advanced(),
        Field::new("rls_roles", "Probar con roles (RLS)", FieldKind::Text)
            .placeholder("Ventas Norte, Ventas Sur")
            .help("Roles separados por comas (solo executeDaxQueries).")
            .advanced(),
        Field::new("custom_data", "Valor de CUSTOMDATA()", FieldKind::Text)
            .help("Solo executeDaxQueries.")
            .advanced(),
        Field::new(
            "definition_api",
            "Leer la definición completa del modelo (API de Fabric)",
            FieldKind::Bool,
        )
        .default_value("true")
        .help("Con permiso de escritura en el modelo trae medidas, relaciones y jerarquías también en áreas de trabajo Pro. Pide el permiso SemanticModel.ReadWrite.All y solo lo usa para leer.")
        .advanced(),
    ]
}

fn info() -> DriverInfo {
    DriverInfo {
        // HTTPS to fixed Microsoft hosts, with the system's roots.
        tls: TlsSupport::Forced,
        tls_port: None,
        id: "powerbi",
        name: "Power BI",
        family: Family::Analytical,
        language: Language::Dax,
        // Room for an Analysis Services (XMLA) driver: same language, another dialect.
        dialect: "powerbi",
        default_port: 443,
        fields: fields(),
        databases_label: "Modelos semánticos",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::new(TABLE, "Tablas", true, true, true),
            ObjectKindInfo::new(CALCULATION_GROUP, "Grupos de cálculo", true, true, true),
            ObjectKindInfo::new(MEASURE, "Medidas", false, true, true),
            ObjectKindInfo::new(HIERARCHY, "Jerarquías", true, true, true),
            ObjectKindInfo::new(RELATIONSHIP, "Relaciones", false, false, true),
            ObjectKindInfo::new(FUNCTION, "Funciones", false, false, true),
            ObjectKindInfo::new(ROLE, "Roles (RLS)", false, false, true),
            ObjectKindInfo::new(PERSPECTIVE, "Perspectivas", false, false, true),
            ObjectKindInfo::new(EXPRESSION, "Expresiones", false, false, true),
        ],
    }
}

const HELP: &str =
    "Consultas DAX de solo lectura. Cada consulta empieza con EVALUATE <expresión de tabla>\n\
(una tabla, SUMMARIZECOLUMNS, FILTER, TOPN…; un valor va entre llaves: EVALUATE { [Ventas] }).\n\
DEFINE antes del primer EVALUATE declara MEASURE, VAR, TABLE, COLUMN o FUNCTION que valen\n\
para todos los EVALUATE que siguen. ORDER BY y START AT ordenan y paginan el resultado.\n\
Comentarios: //, -- y /* */. Las tablas van entre comillas simples ('Ventas') y las columnas\n\
y medidas entre corchetes ('Ventas'[Importe], [Total ventas]).";

/// How the app's own readers see a DAX script (spec, Critique G8): `"…"`
/// strings and `'…'` names double their quote, `[…]` is a name, `//`, `--`
/// and `/* */` are comments. `semicolons` stays on so the editor's
/// statement split goes to [`Driver::split_script`] (the DAX splitter).
pub fn script_dialect() -> ScriptDialect {
    ScriptDialect {
        slash_comments: true,
        bracket_idents: true,
        backtick_idents: false,
        ..ScriptDialect::generic()
    }
}

pub struct PowerBiDriver {
    info: DriverInfo,
}

#[async_trait]
impl Driver for PowerBiDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn query_help(&self) -> &'static str {
        HELP
    }

    /// Relationships where the tier reads them; measures, calculated
    /// columns and the rest through `INFO.CALCDEPENDENCY` or the definition
    /// ([`dependencies`]).
    fn supports_dependencies(&self) -> bool {
        true
    }

    /// Models aren't created from DBine (they're published); they're
    /// deleted on connections that aren't read-only (decision D4).
    /// Relationships are the foreign keys.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            create_database: false,
            drop_database: true,
            foreign_keys: true,
            monitor: true,
            blocking: false,
            kill_session: false,
            processes: false,
            cancel_query: false,
            database_properties: true,
            jobs: true,
            scheduler: true,
        }
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    /// The driver's own commands for the model's permissions (D4): see
    /// [`admin`].
    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        admin::security_script(action)
    }

    /// Renaming the model itself (D4); nothing inside a model is renamed.
    fn rename_spec(&self) -> Option<dbine_driver::RenameSpec> {
        Some(admin::rename_spec())
    }

    fn rename_database_script(
        &self,
        database: &str,
        new_name: &str,
        _objects: &[dbine_driver::rename::DatabaseObject],
    ) -> Result<dbine_driver::SyncScript> {
        admin::rename_database_script(database, new_name)
    }

    /// The model's description, the one property it changes (D4).
    fn alter_database_script(
        &self,
        database: &str,
        changes: &BTreeMap<String, String>,
    ) -> Result<String> {
        admin::alter_database_script(database, changes)
    }

    fn filtered_browse(
        &self,
        browse: &str,
        filters: &[dbine_driver::ColumnFilter],
    ) -> Result<String> {
        dax::browse::filtered_browse(browse, filters)
    }

    fn sorted_browse(&self, query: &str, sort: &[dbine_driver::SortKey]) -> Result<String> {
        dax::browse::sorted_browse(query, sort)
    }

    /// A `GROUPBY` around the user's query (spec §4.5).
    fn aggregate_query(
        &self,
        query: &str,
        spec: &dbine_driver::aggregate::AggregateSpec,
    ) -> Result<String> {
        dax::aggregate::aggregate_query(query, spec)
    }

    fn script_dialect(&self) -> ScriptDialect {
        script_dialect()
    }

    /// `DEFINE` / `EVALUATE` units, and the driver's own commands one per
    /// line ([`dax::split`]).
    fn split_script(&self, text: &str) -> Vec<ScriptStatement> {
        dax::split::split(text)
    }

    /// One unit per call; a `DEFINE` unit may answer several results.
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        database: Option<&str>,
    ) -> Result<Box<dyn Session>> {
        Ok(Box::new(PowerBiSession {
            cx: catalog::connect(cfg, database).await?,
        }))
    }
}

/// What a session holds. Each part's type belongs to the module that
/// defines it, which adds to it as it needs (spec §10: B owns `settings`,
/// `api`, `model` and `query`; C owns `meta`).
pub struct Ctx {
    /// The connection's options, read once.
    pub settings: config::Settings,
    /// Tokens, the HTTP client, the rate window and cancellation.
    pub api: http::Api,
    /// The session's model; `None` when the connection names none (the
    /// explorer's listing session).
    pub model: Option<catalog::Model>,
    /// The query API in use and what it remembers per model.
    pub query: query::QueryState,
    /// The model's metadata and its tier, cached.
    pub meta: meta::MetaState,
}

pub struct PowerBiSession {
    pub cx: Ctx,
}

#[async_trait]
impl Session for PowerBiSession {
    async fn server_version(&mut self) -> Result<String> {
        catalog::server_version(&mut self.cx).await
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        catalog::list_databases(&mut self.cx).await
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        objects::list_objects(&mut self.cx).await
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        objects::columns(&mut self.cx, obj).await
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        objects::definition(&mut self.cx, obj).await
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        dax::browse::browse_query(&self.cx, obj, limit)
    }

    /// A unit of the script: the driver's own commands ([`admin`]) or a
    /// DAX query ([`query`]).
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if admin::is_command(text) {
            return admin::execute(&mut self.cx, text, out).await;
        }
        query::execute(&mut self.cx, text, max_rows, out).await
    }

    async fn run_read_only(
        &mut self,
        statement: &str,
        max_rows: usize,
        out: &mut QueryOutcome,
    ) -> Result<()> {
        query::run_read_only(&mut self.cx, statement, max_rows, out).await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        http::interrupter(&self.cx)
    }

    async fn database_schema(&mut self) -> Result<Vec<dbine_driver::TableSchema>> {
        objects::database_schema(&mut self.cx).await
    }

    async fn database_properties(
        &mut self,
        database: &str,
    ) -> Result<dbine_driver::DatabaseProperties> {
        properties::database_properties(&mut self.cx, database).await
    }

    async fn alter_database(
        &mut self,
        database: &str,
        changes: &BTreeMap<String, String>,
    ) -> Result<()> {
        admin::alter_database(&mut self.cx, database, changes).await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        admin::drop_database(&mut self.cx, name).await
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        monitor::monitor(&mut self.cx).await
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(&mut self.cx).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(&mut self.cx, principal).await
    }

    async fn privileges(&mut self) -> Result<Vec<dbine_driver::Privilege>> {
        security::privileges(&mut self.cx).await
    }

    async fn jobs(&mut self) -> Result<dbine_driver::JobList> {
        refresh::jobs::jobs(&mut self.cx).await
    }

    async fn job_detail(&mut self, id: &str) -> Result<dbine_driver::JobDetail> {
        refresh::jobs::job_detail(&mut self.cx, id).await
    }

    async fn job_runs(&mut self, id: &str, limit: u32) -> Result<Vec<dbine_driver::JobRun>> {
        refresh::jobs::job_runs(&mut self.cx, id, limit).await
    }

    async fn run_job(&mut self, id: &str) -> Result<()> {
        refresh::jobs::run_job(&mut self.cx, id).await
    }

    async fn stop_job(&mut self, id: &str) -> Result<()> {
        refresh::jobs::stop_job(&mut self.cx, id).await
    }

    async fn set_job_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        refresh::jobs::set_job_enabled(&mut self.cx, id, enabled).await
    }

    async fn job_script(&mut self, id: &str) -> Result<String> {
        refresh::jobs::job_script(&mut self.cx, id).await
    }

    async fn function_catalog(&mut self) -> Result<Vec<dbine_driver::FunctionInfo>> {
        functions::function_catalog(&mut self.cx).await
    }

    async fn scheduler_info(&mut self) -> Result<dbine_driver::SchedulerInfo> {
        refresh::scheduler::scheduler_info(&mut self.cx).await
    }

    async fn scheduler_items(&mut self, kind: &str) -> Result<dbine_driver::SchedulerList> {
        refresh::scheduler::scheduler_items(&mut self.cx, kind).await
    }

    async fn scheduler_form(
        &mut self,
        kind: &str,
        id: Option<&str>,
    ) -> Result<dbine_driver::ObjectForm> {
        refresh::scheduler::scheduler_form(&mut self.cx, kind, id).await
    }

    async fn scheduler_script(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &dbine_driver::FormValues,
    ) -> Result<String> {
        refresh::scheduler::scheduler_script(&mut self.cx, kind, id, values).await
    }

    async fn scheduler_save(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &dbine_driver::FormValues,
    ) -> Result<Option<String>> {
        refresh::scheduler::scheduler_save(&mut self.cx, kind, id, values).await
    }

    async fn scheduler_action_form(
        &mut self,
        kind: &str,
        id: &str,
        action: &str,
    ) -> Result<dbine_driver::ObjectForm> {
        refresh::scheduler::scheduler_action_form(&mut self.cx, kind, id, action).await
    }

    async fn scheduler_action_script(
        &mut self,
        kind: &str,
        id: &str,
        action: &str,
        values: &dbine_driver::FormValues,
    ) -> Result<String> {
        refresh::scheduler::scheduler_action_script(&mut self.cx, kind, id, action, values).await
    }

    async fn scheduler_action(
        &mut self,
        kind: &str,
        id: &str,
        action: &str,
        values: &dbine_driver::FormValues,
    ) -> Result<()> {
        refresh::scheduler::scheduler_action(&mut self.cx, kind, id, action, values).await
    }

    async fn read_batches(
        &mut self,
        spec: &dbine_driver::ReadSpec,
        sink: dbine_driver::BatchSinkRef,
    ) -> Result<u64> {
        transfer::read_batches(&mut self.cx, spec, sink).await
    }

    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        security::permissions(&mut self.cx, database).await
    }

    async fn dependents(
        &mut self,
        target: &dbine_driver::DependencyTarget,
        scan: &dbine_driver::DependencyScan,
    ) -> Result<dbine_driver::DependencyReport> {
        dependencies::dependents(&mut self.cx, target, scan).await
    }

    async fn health_checks(
        &mut self,
        database: &str,
    ) -> Result<Vec<dbine_driver::health::HealthCheck>> {
        health::health_checks(&mut self.cx, database).await
    }

    async fn row_estimates(&mut self) -> Result<Vec<dbine_driver::stats::RowEstimate>> {
        objects::row_estimates(&mut self.cx).await
    }

    async fn object_comments(&mut self) -> Result<Vec<dbine_driver::stats::ObjectComment>> {
        objects::object_comments(&mut self.cx).await
    }

    async fn search_code(
        &mut self,
        query: &dbine_driver::search::CodeSearch,
    ) -> Result<Option<dbine_driver::search::CodeSearchReport>> {
        search::search_code(&mut self.cx, query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_itself() {
        let d = &drivers()[0];
        let i = d.info();
        assert_eq!(
            (i.id, i.name, i.dialect),
            ("powerbi", "Power BI", "powerbi")
        );
        assert_eq!(i.language, Language::Dax);
        assert_eq!(i.tls, TlsSupport::Forced);
        assert!(!i.has_schemas);
        assert_eq!(d.script_mode(), ScriptMode::PerStatement);
        assert!(d.supports_dependencies() && !d.supports_explain() && !d.supports_profiler());
        assert!(!d.cross_database() && d.key_search().is_none());
        // Secrets go to the keychain.
        for k in [
            "client_secret",
            "client_certificate_password",
            "access_token",
        ] {
            assert!(i.fields.iter().any(|f| f.key == k && f.secret), "{k}");
        }
        // The certificate is a file picked in the form (a path, no secret).
        let cert = i
            .fields
            .iter()
            .find(|f| f.key == "client_certificate")
            .unwrap();
        assert!(matches!(cert.kind, FieldKind::File) && !cert.secret);
        // Every option key the settings read is a field of the form.
        for k in config::OPTION_KEYS {
            assert!(i.fields.iter().any(|f| f.key == *k), "{k}");
        }
        // Read-only by default: only refresh and the model's administration write.
        let ro = i.fields.iter().find(|f| f.key == "read_only").unwrap();
        assert_eq!(ro.default, "true");
        let kinds: Vec<&str> = i.object_kinds.iter().map(|k| k.id).collect();
        assert_eq!(
            kinds,
            [
                TABLE,
                CALCULATION_GROUP,
                MEASURE,
                HIERARCHY,
                RELATIONSHIP,
                FUNCTION,
                ROLE,
                PERSPECTIVE,
                EXPRESSION
            ]
        );
        let c = d.capabilities();
        assert!(c.foreign_keys && c.monitor && c.database_properties && c.jobs && c.scheduler);
        assert!(c.drop_database && !c.create_database);
        assert!(!c.blocking && !c.kill_session && !c.processes && !c.cancel_query);
        assert!(d.security().is_some() && d.rename_spec().is_some_and(|r| r.databases));
    }

    #[test]
    fn dax_parameters_are_dax_literals() {
        let d = &drivers()[0];
        assert_eq!(
            d.param_literal(&dbine_driver::ParamValue::Text("a\"b".into()))
                .unwrap(),
            "\"a\"\"b\""
        );
        assert_eq!(
            d.param_literal(&dbine_driver::ParamValue::Null).unwrap(),
            "BLANK()"
        );
    }

    #[test]
    fn the_scripts_dialect_reads_dax_quotes_and_comments() {
        let d = script_dialect();
        assert!(d.semicolons && d.slash_comments && d.bracket_idents && !d.backtick_idents);
        // A `//` comment with a quote doesn't open a string for the app's
        // readers (export, charts, lint…).
        let units = dbine_driver::sql::split_script("EVALUATE {1} // don't\n", &d);
        assert_eq!(units.len(), 1);
    }
}
