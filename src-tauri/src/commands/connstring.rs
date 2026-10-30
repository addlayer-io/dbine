//! "Copiar cadena de conexión…" (docs/connection-strings.md): the formats
//! of a saved connection and each one's text (crate::connstring).
//!
//! The preview is built from the saved, non-secret fields: placeholders, or
//! a mask where a secret goes. Secrets are resolved only when the user asks
//! for them ("Incluir contraseña" and "Mostrar" or "Copiar"), through the
//! same path a connect takes (keychain, secret sources with their
//! confirmation, a password typed now), and the text goes back to the UI
//! for the clipboard only. Nothing here is logged, kept or reachable from
//! MCP, the assistant or the phone.

use crate::connstring::{self, note, Conn, Fill, Note};
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::secret_sources::{self as source_defs, SecretSource};
use dbine_driver::DriverInfo;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tauri::State;

#[derive(Deserialize)]
pub struct ConnStringArgs {
    pub connection_id: String,
    /// Empty: the connection's own.
    #[serde(default)]
    pub database: String,
}

#[derive(Serialize)]
pub struct ConnStringFormat {
    pub id: &'static str,
    pub label: String,
}

#[derive(Serialize)]
pub struct ConnStringInfo {
    pub formats: Vec<ConnStringFormat>,
    /// The connection's database.
    pub database: String,
    /// About the connection (tunnel, IAM, secret manager…), for every format.
    pub notes: Vec<Note>,
    /// Some format can carry a secret ("Incluir contraseña" applies).
    pub has_secrets: bool,
    /// Why secrets can't be included: `vault_db`, `iam`; `None` = they can.
    pub blocked: Option<&'static str>,
}

#[derive(Deserialize)]
pub struct ConnStringBuildArgs {
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
    pub format: String,
    /// "Incluir contraseña".
    #[serde(default)]
    pub include_secrets: bool,
    /// With `include_secrets`: resolve them ("Mostrar", "Copiar"); else the mask.
    #[serde(default)]
    pub reveal: bool,
    /// Typed now, for a connection that doesn't keep its password. Used
    /// for this string only.
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Serialize)]
pub struct ConnStringBuilt {
    pub text: String,
    pub notes: Vec<Note>,
    /// A resolved secret is in `text`.
    pub secret: bool,
}

struct Loaded {
    conn: Conn,
    info: Option<&'static DriverInfo>,
    notes: Vec<Note>,
    blocked: Option<&'static str>,
}

fn load(state: &AppState, id: &str, database: &str) -> CommandResult<Loaded> {
    let saved = state
        .store
        .get_connection(id)?
        .ok_or_else(|| CommandError::NotFound(format!("no existe la conexión '{id}'")))?;
    let info = dbine_drivers::find(&saved.config.driver).map(|d| d.info());
    let sources = source_defs::read(&saved.config).unwrap_or_default();
    let conn = Conn::new(&saved.name, &saved.config, info, database);
    let mut notes = Vec::new();
    if crate::tunnels::enabled(&saved.config) {
        // The SSH server may be its tunnel's profile's.
        let mut tunnel = saved.config.clone();
        let _ = state.store.resolve_ssh_profile(&mut tunnel);
        notes.push(note(
            "tunnel",
            &[("ssh", tunnel.option("ssh.host").unwrap_or(""))],
        ));
    }
    let vault_db = sources
        .values()
        .any(|s| matches!(s, SecretSource::VaultDb { .. }));
    if conn.iam {
        notes.push(note("iam", &[]));
    }
    if vault_db {
        notes.push(note("vault_db", &[]));
    } else if !sources.is_empty() {
        notes.push(note("secret_manager", &[]));
    }
    if !saved.save_password
        && !sources.contains_key("password")
        && !conn.iam
        && !conn.user.is_empty()
    {
        notes.push(note("password_not_saved", &[]));
    }
    let blocked = if vault_db {
        Some("vault_db")
    } else if conn.iam && !has_other_secrets(&conn) {
        Some("iam")
    } else {
        None
    };
    Ok(Loaded {
        conn,
        info,
        notes,
        blocked,
    })
}

/// A format of the engine carries a secret other than the IAM token.
fn has_other_secrets(conn: &Conn) -> bool {
    let none = BTreeMap::new();
    let mut probe = BTreeMap::new();
    for k in [
        "token",
        "client_secret",
        "account_key",
        "api_key",
        "auth_token",
        "secret_access_key",
        "wallet_password",
        "connection_string",
    ] {
        probe.insert(k.to_string(), "\u{1}".to_string());
    }
    let a = connstring::build_all(conn, &Fill::Values(&none));
    let b = connstring::build_all(conn, &Fill::Values(&probe));
    a.iter().zip(b.iter()).any(|(x, y)| x.text != y.text)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn connection_string_info(
    state: State<'_, AppState>,
    args: ConnStringArgs,
) -> CommandResult<ConnStringInfo> {
    let l = load(&state, &args.connection_id, &args.database)?;
    let placeholder = connstring::build_all(&l.conn, &Fill::Placeholder);
    let none = BTreeMap::new();
    let bare = connstring::build_all(&l.conn, &Fill::Values(&none));
    let has_secrets = placeholder
        .iter()
        .zip(bare.iter())
        .any(|(p, b)| p.text != b.text);
    Ok(ConnStringInfo {
        formats: placeholder
            .into_iter()
            .map(|b| ConnStringFormat {
                id: b.id,
                label: b.label,
            })
            .collect(),
        database: l.conn.database.clone(),
        notes: l.notes,
        has_secrets,
        blocked: l.blocked,
    })
}

#[tauri::command(rename_all = "camelCase")]
pub async fn connection_string_build(
    state: State<'_, AppState>,
    args: ConnStringBuildArgs,
) -> CommandResult<ConnStringBuilt> {
    let l = load(&state, &args.connection_id, &args.database)?;
    let unknown = || CommandError::BadRequest(format!("formato desconocido: {}", args.format));
    if !args.include_secrets {
        let b = connstring::build(&l.conn, &args.format, &Fill::Placeholder).ok_or_else(unknown)?;
        return Ok(ConnStringBuilt {
            text: b.text,
            notes: b.notes,
            secret: false,
        });
    }
    if !args.reveal {
        let b = connstring::build(&l.conn, &args.format, &Fill::Masked).ok_or_else(unknown)?;
        return Ok(ConnStringBuilt {
            text: b.text,
            notes: b.notes,
            secret: false,
        });
    }
    match l.blocked {
        // Leased credentials DBine revokes when it disconnects: a copy would
        // stop working, and asking for one would keep a lease alive.
        Some("vault_db") => return Err(CommandError::BadRequest("esta conexión usa credenciales temporales de HashiCorp Vault: no se copian en la cadena".into())),
        Some(_) => return Err(CommandError::BadRequest("la contraseña de esta conexión es un token de IAM temporal: no se copia en la cadena".into())),
        None => {}
    }
    let values = resolve(&state, &args.connection_id, args.password, l.info).await?;
    let b = connstring::build(&l.conn, &args.format, &Fill::Values(&values)).ok_or_else(unknown)?;
    Ok(ConnStringBuilt {
        text: b.text,
        notes: b.notes,
        secret: b.secret,
    })
}

/// The connection's secrets, as a connect resolves them (with its
/// confirmations): `password` and the driver's secret fields by key.
async fn resolve(
    state: &AppState,
    id: &str,
    typed: Option<String>,
    info: Option<&DriverInfo>,
) -> CommandResult<BTreeMap<String, String>> {
    // A password typed for this copy is used once and not kept: the
    // connection's typed secrets are left as they were.
    let before = typed.as_ref().map(|_| {
        state
            .typed_secrets
            .get(id)
            .and_then(|t| t.get("password").cloned())
    });
    if let Some(p) = typed {
        state
            .typed_secrets
            .entry(id.to_string())
            .or_default()
            .insert("password".to_string(), p);
    }
    let resolved = state.connect_config(id).await;
    if let Some(prev) = before {
        match prev {
            Some(p) => {
                state
                    .typed_secrets
                    .entry(id.to_string())
                    .or_default()
                    .insert("password".to_string(), p);
            }
            None => {
                if let Some(mut t) = state.typed_secrets.get_mut(id) {
                    t.remove("password");
                }
                state.typed_secrets.remove_if(id, |_, t| t.is_empty());
            }
        }
    }
    let cfg = resolved?;
    let mut out = BTreeMap::new();
    if let Some(p) = cfg.password.as_deref().filter(|p| !p.is_empty()) {
        out.insert("password".to_string(), p.to_string());
    }
    for f in info
        .map(|i| {
            i.fields
                .iter()
                .filter(|f| f.secret && f.key != "password")
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    {
        if let Some(v) = cfg.option(f.key) {
            out.insert(f.key.to_string(), v.to_string());
        }
    }
    Ok(out)
}
