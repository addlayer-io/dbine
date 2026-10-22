use crate::error::{CommandError, CommandResult};
use crate::state::{apply_secrets, driver_info, extract_secrets, meta_key, AppState};
use dbine_core::secret_sources as source_defs;
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

/// A connection as the explorer lists it.
#[derive(Serialize)]
pub struct ListedConnection {
    #[serde(flatten)]
    pub connection: SavedConnection,
    /// A file opened from the OS, not saved (src/open_files.rs).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub temporary: bool,
}

/// The saved connections, after the files opened in this run (temporary).
#[tauri::command]
pub async fn list_connections(state: State<'_, AppState>) -> CommandResult<Vec<ListedConnection>> {
    let temporary = state
        .store
        .list_temporary_connections()?
        .into_iter()
        .map(|connection| ListedConnection {
            connection,
            temporary: true,
        });
    let saved = state
        .store
        .list_connections()?
        .into_iter()
        .map(|connection| ListedConnection {
            connection,
            temporary: false,
        });
    Ok(temporary.chain(saved).collect())
}

#[derive(Deserialize)]
pub struct SaveConnectionArgs {
    /// Secret fields may come filled in: they're moved to the keychain.
    /// A secret left empty keeps the stored value.
    pub connection: SavedConnection,
    /// The connection a new form was copied from ("Duplicar conexión"):
    /// while its definition for a field is unconfirmed, the copy's isn't
    /// confirmed by saving it either.
    #[serde(default)]
    pub duplicate_of: Option<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn save_connection(
    state: State<'_, AppState>,
    args: SaveConnectionArgs,
) -> CommandResult<SavedConnection> {
    let mut conn = args.connection;
    if conn.id.is_empty() {
        conn.id = uuid::Uuid::new_v4().to_string();
    }
    if conn.name.trim().is_empty() {
        return Err(CommandError::BadRequest(
            "la conexión necesita un nombre".into(),
        ));
    }
    crate::mcp::check_override(conn.mcp_level.as_deref())?;
    let info = driver_info(&conn.config.driver)?;
    // Where secret fields come from (docs/secret-sources.md): only fields
    // that are secret here, each definition complete.
    let mut sources =
        source_defs::read(&conn.config).map_err(|e| CommandError::BadRequest(e.to_string()))?;
    sources.retain(|k, _| {
        info.fields.iter().any(|f| f.secret && f.key == k)
            || crate::tunnels::SECRETS.contains(&k.as_str())
    });
    for (field, src) in &sources {
        src.validate().map_err(|e| {
            CommandError::BadRequest(format!(
                "el origen de «{}» no es válido: {e}",
                crate::secret_sources::field_label(Some(info), field)
            ))
        })?;
    }
    source_defs::write(&mut conn.config, &sources);
    let typed = extract_secrets(&mut conn.config, info);
    // What the keychain keeps: the driver's secret fields (the password
    // only with `save_password`), the SSH tunnel's (there's no prompt for
    // them) and the Vault tokens of fields that use Vault. Never a field
    // whose value comes from a source.
    let keep = |k: &str| {
        if sources.contains_key(k) {
            return false;
        }
        if let Some((field, slot)) = source_defs::token_parts(k) {
            return sources
                .get(field)
                .is_some_and(|s| s.token_slots().contains(&slot));
        }
        crate::tunnels::SECRETS.contains(&k)
            || info
                .fields
                .iter()
                .any(|f| f.secret && f.key == k && (k != "password" || conn.save_password))
    };
    let typed: secrets::Secrets = typed.into_iter().filter(|(k, _)| keep(k)).collect();
    let has_vault = sources.values().any(|s| !s.token_slots().is_empty());
    // The keychain is only touched when there's something to keep.
    if conn.save_password
        || !typed.is_empty()
        || crate::tunnels::enabled(&conn.config)
        || has_vault
        || info.fields.iter().any(|f| f.secret && f.key != "password")
    {
        let mut stored = secrets::get(&conn.id).unwrap_or_default();
        // Drops what the driver no longer has (driver changed) too.
        stored.retain(|k, _| keep(k));
        stored.extend(typed);
        secrets::set(&conn.id, &stored)?;
    } else {
        secrets::delete(&conn.id)?;
    }
    // Typed in this form on this computer: confirmed. A definition that
    // came from outside (an import, a restored backup) and was left as it
    // was, or copied, isn't, and neither is anything saved over a field
    // whose stored definition is still unconfirmed: it's confirmed when it
    // first runs.
    confirm_typed(
        &state.store.list_connections()?,
        &conn.id,
        args.duplicate_of.as_deref(),
        &sources,
    )?;
    // An opened file saved as it is ("Guardar conexión") keeps its sessions.
    let mut keep_sessions = false;
    // Pointed at another server, database or login: its cached tree is
    // someone else's.
    if let Some(old) = state.store.get_connection(&conn.id)? {
        keep_sessions = state.store.is_temporary_connection(&conn.id)?
            && serde_json::to_value(&old.config).ok() == serde_json::to_value(&conn.config).ok();
        let (a, b) = (&old.config, &conn.config);
        if a.driver != b.driver
            || a.host != b.host
            || a.port != b.port
            || a.database != b.database
            || a.username != b.username
            || a.options != b.options
        {
            state.cache_forget(&conn.id);
        }
    }
    // Settings changed: open sessions would keep using the old ones.
    if !keep_sessions {
        state.close_connection_sessions(&conn.id);
    }
    state.typed_secrets.remove(&conn.id);
    state.keychain_cache.remove(&conn.id);
    Ok(state.store.save_connection(&conn)?)
}

/// Confirm the definitions saved for connection `id` that were typed in its
/// form here (`saved` is the state before the save; `duplicate_of`, the
/// connection the form was copied from).
fn confirm_typed(
    saved: &[SavedConnection],
    id: &str,
    duplicate_of: Option<&str>,
    sources: &source_defs::SecretSources,
) -> CommandResult<()> {
    for (field, src) in sources {
        let digest = source_defs::digest(id, field, src);
        if !source_defs::is_approved(&digest)
            && !source_defs::needs_confirmation(saved, Some(id), duplicate_of, field, src)
        {
            source_defs::approve(&digest)?;
        }
    }
    Ok(())
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
pub async fn trust_ssh_host(
    state: State<'_, AppState>,
    args: TrustSshHostArgs,
) -> CommandResult<()> {
    let mut conn = state
        .store
        .get_connection(&args.connection_id)?
        .ok_or_else(|| {
            CommandError::NotFound(format!("no existe la conexión '{}'", args.connection_id))
        })?;
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
    // Its confirmations go too.
    if let Some(old) = state.store.get_connection(&args.id)? {
        let digests: Vec<String> = source_defs::read(&old.config)
            .unwrap_or_default()
            .iter()
            .map(|(f, s)| source_defs::digest(&args.id, f, s))
            .collect();
        let _ = source_defs::forget(&digests);
    }
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
    /// The connection a new form was copied from ("Duplicar conexión").
    #[serde(default)]
    pub duplicate_of: Option<String>,
}

#[derive(Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub message: String,
}

/// Never fails: the outcome is in the result, for the dialog to show.
#[tauri::command(rename_all = "camelCase")]
pub async fn test_connection(
    state: State<'_, AppState>,
    args: TestConnectionArgs,
) -> CommandResult<TestResult> {
    // Vault database credentials the test asks for are revoked as soon as
    // its session is closed (not held like a saved connection's).
    crate::secret_sources::with_form_leases(test_connection_with(&state, args)).await
}

async fn test_connection_with(
    state: &AppState,
    args: TestConnectionArgs,
) -> CommandResult<TestResult> {
    let mut cfg = args.config;
    // The form's sources (resolved below) and Vault tokens it typed.
    let sources = match source_defs::take(&mut cfg) {
        Ok(s) => s,
        Err(e) => {
            return Ok(TestResult {
                ok: false,
                message: e.to_string(),
            })
        }
    };
    let mut tokens = crate::secret_sources::take_tokens(&mut cfg);
    let id = args.connection_id.as_deref().filter(|i| !i.is_empty());
    let mut name = String::new();
    if let Some(id) = id {
        let mut known = secrets::get(id).unwrap_or_default();
        if let Some(t) = state.typed_secrets.get(id) {
            known.extend(t.clone());
        }
        for (k, v) in crate::secret_sources::tokens_of(&known) {
            tokens.entry(k).or_insert(v);
        }
        // Only fill what the form left empty, and nothing a source gives.
        known.retain(|k, _| !k.starts_with(source_defs::TOKEN_PREFIX) && !sources.contains_key(k));
        known.retain(|k, _| {
            if k == "password" {
                cfg.password.as_deref().unwrap_or("").is_empty()
            } else {
                cfg.option(k).is_none()
            }
        });
        apply_secrets(&mut cfg, &known);
        name = state
            .store
            .get_connection(id)?
            .map(|c| c.name)
            .unwrap_or_default();
    }
    // A definition that came from outside asks first, and so does a master
    // password asked when needed (errors the UI answers); one that fails is
    // a failed test.
    match crate::secret_sources::fill(
        &state.store,
        &mut cfg,
        &sources,
        &tokens,
        crate::secret_sources::Origin::Form {
            id,
            duplicate_of: args.duplicate_of.as_deref().filter(|i| !i.is_empty()),
            name: &name,
        },
    )
    .await
    {
        Ok(()) => {}
        Err(e @ (CommandError::SecretSourceApproval(_) | CommandError::SecretSourceUnlock(_))) => {
            return Err(e)
        }
        Err(e) => {
            return Ok(TestResult {
                ok: false,
                message: e.to_string(),
            })
        }
    }
    // The tunnel lives until the test ends. An unknown SSH server is an
    // error (the UI asks to trust it), not a failed test.
    let tunnel = match state.tunnels.route(None, &mut cfg).await {
        Ok(t) => t,
        Err(e @ CommandError::SshUnknownHost { .. }) => return Err(e),
        Err(e) => {
            return Ok(TestResult {
                ok: false,
                message: e.to_string(),
            })
        }
    };
    // A cloud sign-in (AWS IAM, Google Cloud IAM, Entra ID): the result
    // says who signed in. Only here: never in the explorer, the feedback
    // report or the documentation, which also show the version.
    if matches!(
        cfg.option("auth"),
        Some("aws_iam" | "gcp_iam" | "azure_entra")
    ) {
        cfg.options
            .insert("describe_identity".into(), "true".into());
    }
    let run = async {
        let mut s = dbine_drivers::open_session(&cfg, None).await?;
        s.server_version().await
    };
    let result = run.await;
    drop(tunnel);
    Ok(match result {
        Ok(v) => TestResult {
            ok: true,
            message: v,
        },
        Err(e) => TestResult {
            ok: false,
            message: e.to_string(),
        },
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
pub async fn connect(
    state: State<'_, AppState>,
    args: ConnectArgs,
) -> CommandResult<ConnectResult> {
    if let Some(p) = args.password {
        // Next to a master password asked for a field's source, if any.
        state
            .typed_secrets
            .entry(args.connection_id.clone())
            .or_default()
            .insert("password".to_string(), p);
    }
    let result = async {
        let cfg = state.resolve_config(&args.connection_id)?;
        let key = meta_key(&args.connection_id, "");
        let entry = state.session(&key, &args.connection_id, "").await?;
        let mut s = entry.session.lock().await;
        let read = async {
            Ok::<_, dbine_driver::Error>((s.server_version().await?, s.list_databases().await?))
        }
        .await;
        let (server_version, databases) = match read {
            Ok(r) => r,
            Err(e) => {
                // A session that broke since it opened (the server went
                // away): the next connect opens a new one instead of failing
                // on it forever.
                if e.is_connect() || matches!(e, dbine_driver::Error::Io(_)) {
                    drop(s);
                    state
                        .sessions
                        .remove_if(&key, |_, held| std::sync::Arc::ptr_eq(held, &entry));
                }
                return Err(e.into());
            }
        };
        state.cache_put(
            &args.connection_id,
            "",
            dbine_core::cache::kinds::DATABASES,
            "",
            &databases,
        );
        Ok::<_, CommandError>(ConnectResult {
            server_version,
            databases,
            default_database: cfg.database,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use source_defs::SecretSource;

    fn cmd(c: &str) -> SecretSource {
        SecretSource::Command {
            command: c.into(),
            json_key: String::new(),
        }
    }

    fn saved_with(id: &str, json: &str) -> SavedConnection {
        let mut config = ConnectionConfig {
            driver: "postgres".into(),
            ..Default::default()
        };
        config
            .options
            .insert(source_defs::OPTION.into(), json.into());
        SavedConnection {
            id: id.into(),
            name: id.into(),
            color: None,
            config,
            save_password: true,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        }
    }

    /// Saving the form over an imported definition (padded, next to an
    /// entry that doesn't parse, or replaced by anything else) confirms
    /// nothing: it asks when it first runs. A definition typed here, on a
    /// new connection or on a confirmed one, is confirmed by the save.
    #[test]
    fn save_confirms_only_what_was_typed_here() {
        let uid = uuid::Uuid::new_v4();
        let padded = format!("imp-pad-{uid}");
        let malformed = format!("imp-bad-{uid}");
        let mine = format!("mine-{uid}");
        let saved = vec![
            saved_with(
                &padded,
                r#"{"password":{"kind":"command","command":"touch /tmp/never-run "}}"#,
            ),
            saved_with(
                &malformed,
                r#"{"password":{"kind":"command","command":"touch /tmp/never-run-2"},"zz":{"kind":"nope"}}"#,
            ),
            saved_with(
                &mine,
                r#"{"password":{"kind":"command","command":"pass db"}}"#,
            ),
        ];
        source_defs::approve(&source_defs::digest(&mine, "password", &cmd("pass db"))).unwrap();
        let one = |s: SecretSource| -> source_defs::SecretSources {
            [("password".to_string(), s)].into()
        };
        let cases = [
            (padded.as_str(), cmd("touch /tmp/never-run")),
            (padded.as_str(), cmd("echo replaced")),
            (malformed.as_str(), cmd("touch /tmp/never-run-2")),
            (malformed.as_str(), cmd("echo replaced")),
        ];
        for (id, src) in &cases {
            confirm_typed(&saved, id, None, &one(src.clone())).unwrap();
            assert!(
                !source_defs::is_approved(&source_defs::digest(id, "password", src)),
                "{id}: {src:?}"
            );
        }
        // A copy of the imported one in a new connection: not confirmed either.
        let new_id = format!("new-{uid}");
        confirm_typed(&saved, &new_id, None, &one(cmd("touch /tmp/never-run"))).unwrap();
        assert!(!source_defs::is_approved(&source_defs::digest(
            &new_id,
            "password",
            &cmd("touch /tmp/never-run")
        )));
        // A duplicate of the imported one, whatever the form made of its
        // text (a BOM the form trimmed, or edited): not confirmed.
        let copy_id = format!("copy-{uid}");
        for src in [cmd("touch /tmp/never-run"), cmd("echo edited in the copy")] {
            confirm_typed(&saved, &copy_id, Some(&padded), &one(src.clone())).unwrap();
            assert!(
                !source_defs::is_approved(&source_defs::digest(&copy_id, "password", &src)),
                "{src:?}"
            );
        }
        // Typed here: confirmed by the save.
        for id in [&new_id, &mine] {
            confirm_typed(&saved, id, None, &one(cmd("pass typed-here"))).unwrap();
            assert!(
                source_defs::is_approved(&source_defs::digest(
                    id,
                    "password",
                    &cmd("pass typed-here")
                )),
                "{id}"
            );
        }
        let all: Vec<String> = [&new_id, &mine]
            .iter()
            .map(|id| source_defs::digest(id, "password", &cmd("pass typed-here")))
            .chain([source_defs::digest(&mine, "password", &cmd("pass db"))])
            .collect();
        source_defs::forget(&all).unwrap();
    }
}
