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
  tool: string;
  summary: string;
  ok: boolean;
  rows: number | null;
  error: string | null;
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
  pendingApprovals: () => invoke<McpApprovalRequest[]>('mcp_pending_approvals'),
  answerApproval: (id: string, decision: McpDecision) => invoke<void>('mcp_answer_approval', { args: { id, decision } }),
  /** Ask again before each write of this client. */
  clearApproveAll: (clientId: string) => invoke<McpStatus>('mcp_clear_approve_all', { args: { client_id: clientId } }),
};

/** The event with the whole list of writes waiting for approval. */
export const MCP_APPROVALS_EVENT = 'mcp-approvals';

/** A write an MCP client wants to run, waiting for the user's answer. */
export interface McpApprovalRequest {
  id: string;
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
