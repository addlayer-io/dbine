import { invoke } from '@tauri-apps/api/core';
import type { Language } from './types';

// DBine's local MCP server (docs/mcp.md): Settings › MCP.

/** What MCP clients may do with a connection. `write` asks the user before each write. */
export type McpLevel = 'disabled' | 'schema' | 'read' | 'write';
export const MCP_LEVELS: McpLevel[] = ['disabled', 'schema', 'read', 'write'];

export interface McpClient {
  id: string;
  name: string;
  created_at: string;
  last_used_at: string | null;
  /** "Aprobar todo" is on: its writes run without asking until DBine closes. */
  approve_all: boolean;
  /** The same for reads the engine can't enforce as read-only on the server. */
  approve_all_reads: boolean;
}

export interface McpStatus {
  enabled: boolean;
  port: number;
  default_level: McpLevel;
  /** Listening now. */
  running: boolean;
  /** Why it isn't running although it's on (the port is busy…). */
  error: string | null;
  url: string;
  clients: McpClient[];
}

export interface McpActivity {
  id: number;
  at: string;
  client: string;
  connection: string;
  /** The database the call named ("" when none). */
  database: string;
  tool: string;
  summary: string;
  /** The query or code, whole (run_query, explain, execute); null for the
   *  other tools, older calls or past 64 KB. */
  code: string | null;
  ok: boolean;
  rows: number | null;
  error: string | null;
}

/** A client's calls with no pause longer than 30 minutes (src-tauri mcp::activity::Session). */
export interface McpSession {
  client: string;
  first_id: number;
  last_id: number;
  start: string;
  end: string;
  connections: string[];
  calls: number;
  reads: number;
  writes: number;
  rejected: number;
  errors: number;
}

export const mcpApi = {
  status: () => invoke<McpStatus>('mcp_status'),
  configure: (enabled: boolean, port: number, defaultLevel: McpLevel) =>
    invoke<McpStatus>('mcp_configure', { args: { enabled, port, default_level: defaultLevel } }),
  /** The token comes back only here: it's shown once. */
  createClient: (name: string) => invoke<{ client: McpClient; token: string }>('mcp_create_client', { args: { name } }),
  revokeClient: (id: string) => invoke<void>('mcp_revoke_client', { args: { id } }),
  activity: (client: string | null, connection: string | null, limit = 200) =>
    invoke<McpActivity[]>('mcp_activity', { args: { client, connection, limit } }),
  sessions: (client: string | null, connection: string | null, limit = 50) =>
    invoke<McpSession[]>('mcp_sessions', { args: { client, connection, limit } }),
  /** A session's calls, oldest first. */
  sessionCalls: (s: McpSession) =>
    invoke<McpActivity[]>('mcp_session_calls', { args: { client: s.client, firstId: s.first_id, lastId: s.last_id } }),
  pendingApprovals: () => invoke<McpApprovalRequest[]>('mcp_pending_approvals'),
  answerApproval: (id: string, decision: McpDecision) => invoke<void>('mcp_answer_approval', { args: { id, decision } }),
  /** Ask again before each write (or read) of this client; without `kind`, both. */
  clearApproveAll: (clientId: string, kind?: McpApprovalKind) =>
    invoke<McpStatus>('mcp_clear_approve_all', { args: { client_id: clientId, kind: kind ?? null } }),
};

/** The event with the whole list of writes waiting for approval. */
export const MCP_APPROVALS_EVENT = 'mcp-approvals';

/** `write`: `execute`. `read`: `run_query` / `explain` on an engine that
 *  can't enforce a read-only transaction on the server. */
export type McpApprovalKind = 'read' | 'write';

/** A write (or an unenforced read) an MCP client wants to run, waiting for the user's answer. */
export interface McpApprovalRequest {
  id: string;
  kind: McpApprovalKind;
  client_id: string;
  client: string;
  connection: string;
  database: string;
  engine: string;
  language: Language;
  dialect: string;
  code: string;
  /** Rejected if unanswered by then (RFC 3339). */
  expires_at: string;
  timeout_secs: number;
}

export type McpDecision = 'approve' | 'reject' | 'approve_all';

/** Ready-to-paste configuration for each kind of client. */
export function mcpSnippets(url: string, token: string) {
  return {
    claude: `claude mcp add --scope user --transport http dbine ${url} --header "Authorization: Bearer ${token}"`,
    codex: `# ~/.codex/config.toml\n[mcp_servers.dbine]\nurl = "${url}"\nhttp_headers = { "Authorization" = "Bearer ${token}" }`,
    json: JSON.stringify({ mcpServers: { dbine: { type: 'http', url, headers: { Authorization: `Bearer ${token}` } } } }, null, 2),
    cursor: JSON.stringify({ mcpServers: { dbine: { url, headers: { Authorization: `Bearer ${token}` } } } }, null, 2),
    // Claude Desktop only starts local servers by command: mcp-remote bridges to HTTP.
    claudeDesktop: JSON.stringify({
      mcpServers: {
        dbine: {
          command: 'npx',
          args: ['-y', 'mcp-remote', url, '--header', 'Authorization:${DBINE_AUTH}'],
          env: { DBINE_AUTH: `Bearer ${token}` },
        },
      },
    }, null, 2),
    vscode: `code --add-mcp '${JSON.stringify({ name: 'dbine', type: 'http', url, headers: { Authorization: `Bearer ${token}` } })}'`,
    windsurf: JSON.stringify({ mcpServers: { dbine: { serverUrl: url, headers: { Authorization: `Bearer ${token}` } } } }, null, 2),
  };
}
