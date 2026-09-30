//! Settings › MCP: the server switch, its port and default level, the
//! clients and their tokens, the activity log, and the answers to the
//! writes waiting for approval.

use super::activity::ActivityEntry;
use super::approvals::{ApprovalRequest, Decision};
use super::{check_level, load_clients, load_config, save_config, McpLevel, McpRuntime};
use crate::error::{CommandError, CommandResult};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Serialize)]
pub struct ClientView {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    /// "Approve all" is on: its writes run without asking until DBine
    /// closes (or the user removes it).
    pub approve_all: bool,
}

#[derive(Serialize)]
pub struct McpStatus {
    pub enabled: bool,
    pub port: u16,
    pub default_level: McpLevel,
    /// Listening now.
    pub running: bool,
    /// Why it isn't running although it's on (the port is busy…).
    pub error: Option<String>,
    /// The endpoint clients use.
    pub url: String,
    pub clients: Vec<ClientView>,
}

fn status(rt: &McpRuntime) -> McpStatus {
    let state = &rt.inner.state;
    let cfg = load_config(state);
    let approve_all = rt.inner.approvals.approve_all_clients();
    McpStatus {
        enabled: cfg.enabled,
        port: cfg.port,
        default_level: cfg.default_level,
        running: rt.running_port().is_some(),
        error: rt.last_error(),
        url: format!("http://127.0.0.1:{}/mcp", cfg.port),
        clients: load_clients(state)
            .into_iter()
            .map(|c| ClientView { approve_all: approve_all.contains(&c.id), id: c.id, name: c.name, created_at: c.created_at, last_used_at: c.last_used_at })
            .collect(),
    }
}

#[tauri::command]
pub async fn mcp_status(rt: State<'_, McpRuntime>) -> CommandResult<McpStatus> {
    Ok(status(&rt))
}

#[derive(Deserialize)]
pub struct ConfigureArgs {
    pub enabled: bool,
    pub port: u16,
    pub default_level: McpLevel,
}

/// Save the settings and start, stop or move the server to match. A busy
/// port doesn't fail the call: the status says so.
#[tauri::command(rename_all = "camelCase")]
pub async fn mcp_configure(rt: State<'_, McpRuntime>, args: ConfigureArgs) -> CommandResult<McpStatus> {
    check_level(args.default_level)?;
    if args.port < 1024 {
        return Err(CommandError::BadRequest("el puerto tiene que estar entre 1024 y 65535".into()));
    }
    let cfg = super::McpConfig { enabled: args.enabled, port: args.port, default_level: args.default_level };
    save_config(&rt.inner.state, &cfg)?;
    rt.apply();
    Ok(status(&rt))
}

#[derive(Deserialize)]
pub struct CreateClientArgs {
    pub name: String,
}

#[derive(Serialize)]
pub struct CreatedClient {
    pub client: ClientView,
    /// Shown once: only its hash is kept.
    pub token: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn mcp_create_client(rt: State<'_, McpRuntime>, args: CreateClientArgs) -> CommandResult<CreatedClient> {
    let (c, token) = rt.inner.create_client(&args.name)?;
    Ok(CreatedClient { client: ClientView { id: c.id, name: c.name, created_at: c.created_at, last_used_at: c.last_used_at, approve_all: false }, token })
}

#[derive(Deserialize)]
pub struct RevokeClientArgs {
    pub id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn mcp_revoke_client(rt: State<'_, McpRuntime>, args: RevokeClientArgs) -> CommandResult<()> {
    rt.inner.revoke_client(&args.id)
}

#[derive(Deserialize)]
pub struct ActivityArgs {
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub connection: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn mcp_activity(rt: State<'_, McpRuntime>, args: ActivityArgs) -> CommandResult<Vec<ActivityEntry>> {
    let client = args.client.filter(|c| !c.is_empty());
    let connection = args.connection.filter(|c| !c.is_empty());
    rt.inner
        .activity
        .list(client.as_deref(), connection.as_deref(), args.limit.unwrap_or(200))
        .map_err(|e| CommandError::State(e.to_string()))
}

/// The writes waiting for the user's answer (the UI also gets them as the
/// `mcp-approvals` event).
#[tauri::command]
pub async fn mcp_pending_approvals(rt: State<'_, McpRuntime>) -> CommandResult<Vec<ApprovalRequest>> {
    Ok(rt.inner.approvals.pending())
}

#[derive(Deserialize)]
pub struct AnswerArgs {
    pub id: String,
    pub decision: Decision,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn mcp_answer_approval(rt: State<'_, McpRuntime>, args: AnswerArgs) -> CommandResult<()> {
    rt.inner.approvals.answer(&args.id, args.decision).map_err(CommandError::BadRequest)
}

#[derive(Deserialize)]
pub struct ClearApproveAllArgs {
    pub client_id: String,
}

/// Ask again before each write of this client.
#[tauri::command(rename_all = "camelCase")]
pub async fn mcp_clear_approve_all(rt: State<'_, McpRuntime>, args: ClearApproveAllArgs) -> CommandResult<McpStatus> {
    rt.inner.approvals.clear_approve_all(&args.client_id);
    Ok(status(&rt))
}
