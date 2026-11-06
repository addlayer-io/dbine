import { invoke } from '@tauri-apps/api/core';

// Signed in to DBine Enterprise (src-tauri/src/enterprise): the company's
// catalog, access requests and extensions. Types copied from the Rust side.

/** What a profile allows now: databases (null: every one) and until when (null: no end). */
export interface EnterpriseAccess {
  databases: string[] | null;
  until: string | null;
}

export interface EnterpriseProfile {
  id: string;
  name: string;
  description: string;
  read_only: boolean;
  access: EnterpriseAccess | null;
  pending_request: { id: string; created_at: string; extension: boolean } | null;
}

export interface EnterpriseConnection {
  id: string;
  name: string;
  driver: string;
  package: string;
  /** When it may be used (null: at any time). */
  schedule: unknown | null;
  within_hours: boolean;
  profiles: EnterpriseProfile[];
}

export interface EnterpriseSettings {
  warn_minutes: number[];
  end: { policy: 'strict' | 'finish' | 'finish_if_extension'; finish_minutes: number };
  allow_ai: boolean;
  allow_mcp: boolean;
  explorer_cache: boolean;
}

export interface EnterpriseCatalog {
  organization: string;
  settings: EnterpriseSettings;
  connections: EnterpriseConnection[];
  last_event: number;
}

export interface EnterpriseStatus {
  signed_in: boolean;
  server: string;
  email: string;
  name: string;
  organization: string;
  online: boolean;
  error: string | null;
  catalog: EnterpriseCatalog | null;
}

/** A notification from DBine Enterprise (`/desktop/events`). */
export interface EnterpriseEvent {
  id?: number;
  kind: string;
  data?: Record<string, unknown>;
  at?: string;
}

/** How long access is asked for: some hours, until a date, or (neither) no expiry. */
export interface EnterpriseDuration {
  hours: number | null;
  until: string | null;
}

export interface RunningStatement {
  statement: string;
  seconds: number;
}

/** The managed connection's options (src-tauri/src/enterprise/mod.rs). */
export const PROFILE_OPTION = 'dbine.gateway.profile';
export const CONNECTION_OPTION = 'dbine.enterprise.connection';
export const PROFILE_NAME_OPTION = 'dbine.enterprise.profile';

export const enterpriseApi = {
  status: () => invoke<EnterpriseStatus>('enterprise_status'),
  signIn: (server: string, donePage: string) => invoke<EnterpriseStatus>('enterprise_sign_in', { args: { server, done_page: donePage } }),
  cancelSignIn: () => invoke<void>('enterprise_cancel_sign_in'),
  signOut: () => invoke<void>('enterprise_sign_out'),
  sync: () => invoke<EnterpriseStatus>('enterprise_sync'),
  request: (profileId: string, databases: string[] | null, d: EnterpriseDuration, reason: string, outsideHours = false) =>
    invoke<string>('enterprise_request', { args: { profile_id: profileId, databases, outside_hours: outsideHours, hours: d.hours, until: d.until, reason } }),
  extend: (profileId: string, d: EnterpriseDuration, reason: string, running: RunningStatement[]) =>
    invoke<string>('enterprise_extend', { args: { profile_id: profileId, hours: d.hours, until: d.until, reason, running } }),
  cancelRequest: (requestId: string) => invoke<void>('enterprise_cancel_request', { args: { request_id: requestId } }),
  notify: (title: string, body: string) => invoke<void>('enterprise_notify', { args: { title, body } }),
};
