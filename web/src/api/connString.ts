import { invoke } from '@tauri-apps/api/core';

// "Copiar cadena de conexión…" (docs/connection-strings.md). Mirrors
// src-tauri/src/commands/connstring.rs.

export interface ConnStringNote {
  /** `connString:note.<code>` */
  code: string;
  args: Record<string, string>;
}

export interface ConnStringFormat {
  /** `uri`, `libpq`, `adonet`, `jdbc`, `sqlalchemy`, `odbc`, `node`, `go`… */
  id: string;
  /** The format and its library; empty: `connString:format.<id>`. */
  label: string;
}

export interface ConnStringInfo {
  formats: ConnStringFormat[];
  database: string;
  notes: ConnStringNote[];
  has_secrets: boolean;
  /** Why secrets can't be included (`vault_db`, `iam`), else null. */
  blocked: string | null;
}

export interface ConnStringBuilt {
  text: string;
  notes: ConnStringNote[];
  /** A resolved secret is in `text`. */
  secret: boolean;
}

export interface ConnStringBuildArgs {
  connection_id: string;
  database: string;
  format: string;
  include_secrets: boolean;
  /** With include_secrets: resolve them (else the mask). */
  reveal: boolean;
  /** Typed now for a connection that doesn't keep its password. */
  password?: string | null;
}

export const connStringApi = {
  info: (connectionId: string, database: string) =>
    invoke<ConnStringInfo>('connection_string_info', { args: { connection_id: connectionId, database } }),
  build: (args: ConnStringBuildArgs) => invoke<ConnStringBuilt>('connection_string_build', { args }),
};
