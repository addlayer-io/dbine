use crate::error::{CommandError, CommandResult};
use crate::state::{apply_secrets, driver_info, extract_secrets, meta_key, AppState};
use dbine_core::secrets;
use dbine_core::SavedConnection;
use dbine_driver::{ConnectionConfig, DriverInfo};
use serde::{Deserialize, Serialize};
use tauri::State;

/// A driver as the UI sees it: its info plus the query-syntax help.
#[derive(Serialize)]
pub struct DriverDescriptor {
    #[serde(flatten)]
    info: DriverInfo,
    query_help: &'static str,
    supports_explain: bool,
    /// "Comparar esquemas" can apply changes to it.
    supports_schema_sync: bool,
    supports_profiler: bool,
    /// A table's indexes with their usage (`Session::index_usage`).
    supports_index_usage: bool,
    /// "Ver dependencias…" (`Session::dependents`).
    supports_dependencies: bool,
    /// "Deshabilitar / Habilitar índice" (`Driver::index_toggle_script`).
    supports_index_toggle: bool,
    /// "Renombrar…" (`Driver::rename_spec`); `None`: not offered.
    rename: Option<dbine_driver::RenameSpec>,
    /// "Asignar login…" in Users and permissions (`Driver::supports_map_login`).
    supports_map_login: bool,
    /// Databases of keys, searched on the server (Redis, etcd).
    key_search: Option<dbine_driver::KeySearch>,
    capabilities: dbine_driver::Capabilities,
    designer: Option<dbine_driver::DesignerSpec>,
    create_templates: Vec<dbine_driver::CreateTemplate>,
    /// Users and permissions (docs/users-and-permissions.md).
    security: Option<dbine_driver::SecuritySpec>,
    /// The engine's own backups (docs/backups.md).
    backup: Option<dbine_driver::BackupSpec>,
    /// "Nuevo esquema…" / "Borrar esquema…"; `None`: not offered.
    schema_spec: Option<dbine_driver::SchemaSpec>,
    /// "Nueva base de datos"'s advanced options; empty: just the name.
    create_database_fields: Vec<dbine_driver::Field>,
    script_separator: &'static str,
    /// How the editor runs a script ("Seguir si hay un error" only matters
    /// when it isn't `whole`) and the engine's defaults for it.
    script_mode: dbine_driver::ScriptMode,
    script_defaults: dbine_driver::ScriptDefaults,
    /// The editor offers the Auto/Manual transactions toggle.
    supports_manual_transactions: bool,
}

#[tauri::command]
pub async fn list_drivers() -> CommandResult<Vec<DriverDescriptor>> {
    Ok(dbine_drivers::all()
        .iter()
        .map(|d| DriverDescriptor {
            info: d.info().clone(),
            query_help: d.query_help(),
            supports_explain: d.supports_explain(),
            supports_schema_sync: d.supports_schema_sync(),
            supports_profiler: d.supports_profiler(),
            supports_index_usage: d.supports_index_usage(),
            supports_dependencies: d.supports_dependencies(),
            supports_index_toggle: d.supports_index_toggle(),
            rename: d.rename_spec(),
            supports_map_login: d.supports_map_login(),
            key_search: d.key_search(),
            capabilities: d.capabilities(),
            designer: d.designer(),
            create_templates: d.create_templates(),
            security: d.security(),
            backup: d.backup(),
            schema_spec: d.schema_spec(),
            create_database_fields: d.create_database_fields(),
            script_separator: d.script_separator(),
            script_mode: d.script_mode(),
            script_defaults: d.script_defaults(),
            supports_manual_transactions: d.supports_manual_transactions(),
        })
        .collect())
}

#[tauri::command]
pub async fn list_connections(state: State<'_, AppState>) -> CommandResult<Vec<SavedConnection>> {
    Ok(state.store.list_connections()?)
}

#[derive(Deserialize)]
pub struct SaveConnectionArgs {
    /// Secret fields may come filled in: they're moved to the keychain.
    /// A secret left empty keeps the stored value.
    pub connection: SavedConnection,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn save_connection(state: State<'_, AppState>, args: SaveConnectionArgs) -> CommandResult<SavedConnection> {
    let mut conn = args.connection;
    if conn.id.is_empty() {
        conn.id = uuid::Uuid::new_v4().to_string();
    }
    if conn.name.trim().is_empty() {
        return Err(CommandError::BadRequest("la conexión necesita un nombre".into()));
    }
    crate::mcp::check_override(conn.mcp_level.as_deref())?;
    let info = driver_info(&conn.config.driver)?;
    let typed = extract_secrets(&mut conn.config, info);
    if conn.save_password {
        let mut stored = secrets::get(&conn.id).unwrap_or_default();
        // Drop secrets of fields the driver no longer has (driver changed).
        stored.retain(|k, _| info.fields.iter().any(|f| f.secret && f.key == k) || crate::tunnels::SECRETS.contains(&k.as_str()));
        stored.extend(typed);
        secrets::set(&conn.id, &stored)?;
    } else {
        // Not keeping the database password; the SSH tunnel's secrets are
        // kept all the same (there's no prompt for them). The keychain is
        // only touched when there's a tunnel secret to keep.
        let ssh_typed: secrets::Secrets = typed.into_iter().filter(|(k, _)| crate::tunnels::SECRETS.contains(&k.as_str())).collect();
        if ssh_typed.is_empty() && !crate::tunnels::enabled(&conn.config) {
            secrets::delete(&conn.id)?;
        } else {
            let mut ssh: secrets::Secrets = secrets::get(&conn.id).unwrap_or_default();
            ssh.retain(|k, _| crate::tunnels::SECRETS.contains(&k.as_str()));
            ssh.extend(ssh_typed);
            secrets::set(&conn.id, &ssh)?;
        }
    }
    // Pointed at another server, database or login: its cached tree is
    // someone else's.
    if let Some(old) = state.store.get_connection(&conn.id)? {
        let (a, b) = (&old.config, &conn.config);
        if a.driver != b.driver || a.host != b.host || a.port != b.port || a.database != b.database || a.username != b.username || a.options != b.options {
            state.cache_forget(&conn.id);
        }
    }
    // Settings changed: open sessions would keep using the old ones.
    state.close_connection_sessions(&conn.id);
    state.typed_secrets.remove(&conn.id);
    state.keychain_cache.remove(&conn.id);
    Ok(state.store.save_connection(&conn)?)
}

#[derive(Deserialize)]
pub struct TrustSshHostArgs {
    pub connection_id: String,
    /// The key bound to the server it was accepted for, `[host]:port SHA256:…`,
    /// with the host, port and fingerprint the `ssh_unknown_host` error gave
    /// (web/src/composables/sshTrust.ts). A bare fingerprint is refused: it
    /// would say nothing about which server it is for.
    pub fingerprint: String,
}

/// Trust an SSH server's key for a saved connection's tunnel (the user
/// checked the fingerprint): it's added to `ssh.trusted`, for that server
/// only, which has to be one of the tunnel's and have no other key accepted
/// (a changed key is forgotten in the connection's form first, never
/// replaced from the first-connection prompt).
#[tauri::command(rename_all = "camelCase")]
pub async fn trust_ssh_host(state: State<'_, AppState>, args: TrustSshHostArgs) -> CommandResult<()> {
    let mut conn = state
        .store
        .get_connection(&args.connection_id)?
        .ok_or_else(|| CommandError::NotFound(format!("no existe la conexión '{}'", args.connection_id)))?;
    crate::tunnels::trust(&mut conn.config, &args.fingerprint)?;
    state.store.save_connection(&conn)?;
    Ok(())
}

#[derive(Deserialize)]
pub struct IdArgs {
    pub id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn delete_connection(state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    state.close_connection_sessions(&args.id);
    state.typed_secrets.remove(&args.id);
    state.keychain_cache.remove(&args.id);
    secrets::delete(&args.id)?;
    state.cache_forget(&args.id);
    Ok(state.store.delete_connection(&args.id)?)
}

#[derive(Deserialize)]
pub struct TestConnectionArgs {
    pub config: ConnectionConfig,
    /// When editing a saved connection, secret fields left empty take the
    /// stored values.
    pub connection_id: Option<String>,
}

#[derive(Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub message: String,
}

/// Never fails: the outcome is in the result, for the dialog to show.
#[tauri::command(rename_all = "camelCase")]
pub async fn test_connection(state: State<'_, AppState>, args: TestConnectionArgs) -> CommandResult<TestResult> {
    let mut cfg = args.config;
    if let Some(id) = &args.connection_id {
        let mut known = secrets::get(id).unwrap_or_default();
        if let Some(t) = state.typed_secrets.get(id) {
            known.extend(t.clone());
        }
        // Only fill what the form left empty.
        known.retain(|k, _| if k == "password" { cfg.password.as_deref().unwrap_or("").is_empty() } else { cfg.option(k).is_none() });
        apply_secrets(&mut cfg, &known);
    }
    // The tunnel lives until the test ends. An unknown SSH server is an
    // error (the UI asks to trust it), not a failed test.
    let tunnel = match state.tunnels.route(None, &mut cfg).await {
        Ok(t) => t,
        Err(e @ CommandError::SshUnknownHost { .. }) => return Err(e),
        Err(e) => return Ok(TestResult { ok: false, message: e.to_string() }),
    };
    let run = async {
        let mut s = dbine_drivers::open_session(&cfg, None).await?;
        s.server_version().await
    };
    let result = run.await;
    drop(tunnel);
    Ok(match result {
        Ok(v) => TestResult { ok: true, message: v },
        Err(e) => TestResult { ok: false, message: e.to_string() },
    })
}

#[derive(Deserialize)]
pub struct ConnectArgs {
    pub connection_id: String,
    /// Typed when the connection doesn't keep its password.
    pub password: Option<String>,
}

#[derive(Serialize)]
pub struct ConnectResult {
    pub server_version: String,
    pub databases: Vec<String>,
    /// The database the login lands on, to expand first.
    pub default_database: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn connect(state: State<'_, AppState>, args: ConnectArgs) -> CommandResult<ConnectResult> {
    if let Some(p) = args.password {
        state.typed_secrets.insert(args.connection_id.clone(), [("password".to_string(), p)].into());
    }
    let result = async {
        let cfg = state.resolve_config(&args.connection_id)?;
        let entry = state.session(&meta_key(&args.connection_id, ""), &args.connection_id, "").await?;
        let mut s = entry.session.lock().await;
        let server_version = s.server_version().await?;
        let databases = s.list_databases().await?;
        state.cache_put(&args.connection_id, "", dbine_core::cache::kinds::DATABASES, "", &databases);
        Ok::<_, CommandError>(ConnectResult { server_version, databases, default_database: cfg.database })
    }
    .await;
    if result.is_err() {
        // A wrong typed password shouldn't stick for the next try.
        state.typed_secrets.remove(&args.connection_id);
    }
    result
}

#[derive(Deserialize)]
pub struct ConnectionIdArgs {
    pub connection_id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn disconnect(state: State<'_, AppState>, args: ConnectionIdArgs) -> CommandResult<()> {
    state.close_connection_sessions(&args.connection_id);
    Ok(())
}
